//! PTY layer: spawn a child process attached to a pseudo-terminal.
//!
//! Wraps `portable-pty` (WezTerm) and exposes the master's raw fd so the `mio`
//! core loop can read it non-blocking and event-driven, per `docs/03`/`docs/04`
//! (no per-pane blocking reader threads). Unix-only for v1; Windows (ConPTY) is
//! a fast-follow.

use std::os::unix::io::RawFd;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// A spawned child on a PTY. Reads/writes go through the master's raw fd.
pub struct PtyProcess {
    // `master` owns the fd; keep it alive so `fd` stays valid.
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    fd: RawFd,
    process_group: libc::pid_t,
    termination_finished: bool,
    /// The last size handed to the kernel, so a relayout that leaves this
    /// pane's rectangle alone does not send the child a spurious SIGWINCH.
    size: std::cell::Cell<(u16, u16)>,
}

/// Longest a pane stop waits for a killed shell to finish exiting.
const REAP_BOUND: std::time::Duration = std::time::Duration::from_secs(2);

impl PtyProcess {
    /// Spawn `program` with `args` on a fresh PTY of the given size, optionally
    /// in working directory `cwd`, with `extra_env` exported into the child
    /// (e.g. `UNITERM_SOCKET`, so `uniterm workflow submit` inside a pane can
    /// deliver to its own session).
    pub fn spawn(
        program: &str,
        args: &[&str],
        cols: u16,
        rows: u16,
        cwd: Option<&std::path::Path>,
        extra_env: &[(&str, &str)],
    ) -> std::io::Result<PtyProcess> {
        if let Some(dir) = cwd {
            if !dir.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("working directory does not exist: {}", dir.display()),
                ));
            }
        }
        let sys = native_pty_system();
        let pair = sys
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_io)?;

        let mut cmd = CommandBuilder::new(program);
        for a in args {
            cmd.arg(a);
        }
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        cmd.env("TERM", "xterm-256color");
        // The emulator and renderer carry 24-bit colour end to end; advertise
        // it so truecolor-first apps (btop, Claude Code) use their full range.
        cmd.env("COLORTERM", "truecolor");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd).map_err(to_io)?;
        let process_group = child
            .process_id()
            .map(|pid| pid as libc::pid_t)
            .ok_or_else(|| std::io::Error::other("PTY child has no process id"))?;
        // Drop the slave so the master read observes EOF when the child exits.
        drop(pair.slave);

        let fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| std::io::Error::other("PTY master has no raw fd"))?;

        Ok(PtyProcess {
            master: pair.master,
            child,
            fd,
            process_group,
            termination_finished: false,
            size: std::cell::Cell::new((cols, rows)),
        })
    }

    /// The master fd, for registering with `mio` (`SourceFd`).
    pub fn raw_fd(&self) -> RawFd {
        self.fd
    }

    /// Put the master fd into non-blocking mode (required for the mio loop).
    pub fn set_nonblocking(&self) -> std::io::Result<()> {
        // SAFETY: `fd` is owned by `self.master` and valid for its lifetime.
        unsafe {
            let flags = libc::fcntl(self.fd, libc::F_GETFL);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(self.fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Read available output. `Ok(0)` is EOF (child exited). On a non-blocking
    /// fd with no data ready, returns `ErrorKind::WouldBlock`.
    pub fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // SAFETY: valid fd, buffer pointer and length are from a live slice.
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    /// Attempt one non-blocking input write. The mio owner retains unwritten
    /// bytes and requests writable readiness instead of spinning in this call.
    pub fn write_some(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        // SAFETY: valid fd; pointer/len derived from a live slice.
        let n = unsafe { libc::write(self.fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    /// Resize the PTY (on a client resize). An unchanged size is a no-op:
    /// every TIOCSWINSZ raises SIGWINCH in the child, and a full-screen
    /// application answers each one with a complete repaint that the server
    /// then has to parse and render.
    pub fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        if self.size.get() == (cols, rows) {
            return Ok(());
        }
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_io)?;
        self.size.set((cols, rows));
        Ok(())
    }

    /// The size the child currently sees.
    pub fn size(&self) -> (u16, u16) {
        self.size.get()
    }

    /// Non-blocking check whether the child has exited.
    pub fn try_wait_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Wait for the child to exit; returns whether it succeeded. Only for
    /// callers that keep reading the master meanwhile or own no master at all:
    /// the server's termination path uses the draining reap instead.
    pub fn wait(&mut self) -> std::io::Result<bool> {
        Ok(self.child.wait().map_err(to_io)?.success())
    }

    /// Start a process-group shutdown without waiting, so callers terminating
    /// many panes can grant one collective grace period instead of one per pane.
    pub fn request_terminate(&mut self) -> std::io::Result<bool> {
        if self.termination_finished {
            return Ok(false);
        }
        let exited = self.try_wait_exited();
        let group_signaled = self.signal_group(libc::SIGTERM)?;
        if group_signaled || exited {
            Ok(group_signaled)
        } else {
            self.signal_process(libc::SIGTERM)
        }
    }

    /// End a process-group grace period, kill survivors, and reap the pane shell.
    pub fn finish_terminate(&mut self) -> std::io::Result<()> {
        if self.termination_finished {
            return Ok(());
        }
        let exited = self.try_wait_exited();
        let group_signaled = self.signal_group(libc::SIGKILL)?;
        if !group_signaled && !exited {
            let _ = self.signal_process(libc::SIGKILL)?;
        }
        if !exited {
            self.reap_while_draining();
        }
        self.termination_finished = true;
        Ok(())
    }

    /// Reap the killed shell without ever blocking in `waitpid`.
    ///
    /// A process closing its tty on exit waits for the output it has already
    /// written to reach the master. On macOS that wait is unconditional, so a
    /// shell that printed anything after the master was last read (a "Killed:
    /// 9" job notice, or an application's final repaint) sits in the kernel's
    /// exiting state until someone reads the master. The only reader is this
    /// thread, so blocking in `waitpid` here deadlocked the whole Workspace on
    /// every macOS stop after a resize. Draining the master while polling for
    /// the exit lets the close complete; the bytes belong to a pane that is
    /// being destroyed. The bound keeps a stop finite even if the child is
    /// stuck for another reason; the process watch reaps it later.
    fn reap_while_draining(&mut self) {
        let started = std::time::Instant::now();
        let mut scratch = [0u8; 4096];
        loop {
            if self.try_wait_exited() {
                return;
            }
            loop {
                // SAFETY: reading our own non-blocking master fd into a live
                // buffer; a zero or negative result ends the drain.
                let read = unsafe {
                    libc::read(
                        self.fd,
                        scratch.as_mut_ptr() as *mut libc::c_void,
                        scratch.len(),
                    )
                };
                if read <= 0 {
                    break;
                }
            }
            if started.elapsed() >= REAP_BOUND {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Terminate and reap the entire pane process group after a short grace period.
    pub fn kill(&mut self) -> std::io::Result<()> {
        if self.request_terminate()? {
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        self.finish_terminate()
    }

    fn signal_group(&self, signal: libc::c_int) -> std::io::Result<bool> {
        if self.process_group <= 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to signal an unsafe process group",
            ));
        }
        // SAFETY: a negative pid targets the process group created by
        // portable-pty's setsid call. No pointers or shared memory are involved.
        if unsafe { libc::kill(-self.process_group, signal) } == 0 {
            return Ok(true);
        }
        Self::signal_outcome(std::io::Error::last_os_error())
    }

    /// `ESRCH` means the target is gone. `EPERM` for our own child means the
    /// kernel will not accept a signal for a process that is already exiting:
    /// macOS answers that way for a shell stuck draining its tty on the way
    /// out, which the reap resolves by reading the master. Both are "nothing
    /// left to signal", not failures.
    fn signal_outcome(error: std::io::Error) -> std::io::Result<bool> {
        match error.raw_os_error() {
            Some(libc::ESRCH) | Some(libc::EPERM) => Ok(false),
            _ => Err(error),
        }
    }

    fn signal_process(&self, signal: libc::c_int) -> std::io::Result<bool> {
        // If an application moved itself out of the PTY's original process
        // group, still terminate the pane leader without risking pid 0 or 1.
        if self.process_group <= 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to signal an unsafe process id",
            ));
        }
        // SAFETY: a positive pid targets only the child that portable-pty
        // returned and the value was validated above.
        if unsafe { libc::kill(self.process_group, signal) } == 0 {
            return Ok(true);
        }
        Self::signal_outcome(std::io::Error::last_os_error())
    }

    /// The working directory of the pane's foreground process group, read from
    /// `/proc` (Linux). Best-effort - `None` if unavailable.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        let pid = self.master.process_group_leader()?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// Whether the pane's own child (its shell) holds the terminal foreground
    /// right now - i.e. nothing (an editor, a pager, an agent) is running in
    /// front of it. Guards typed launches into the pane: one `tcgetpgrp`
    /// ioctl at use time, never a poll. `false` when it cannot be determined,
    /// so callers fail safe.
    pub fn child_owns_foreground(&self) -> bool {
        match (self.master.process_group_leader(), self.child.process_id()) {
            (Some(fg), Some(child)) => fg == child as libc::pid_t,
            _ => false,
        }
    }

    /// Current foreground process-group leader, sampled only in response to
    /// PTY output. Callers cache changes, so this is event-driven discovery,
    /// not process polling.
    pub fn foreground_process_group(&self) -> Option<i32> {
        self.master.process_group_leader()
    }
}

/// Read the current terminal window size from a tty fd via `TIOCGWINSZ`.
/// Returns `(cols, rows)`, falling back to 80x24 if the ioctl fails.
pub fn tty_size(fd: RawFd) -> (u16, u16) {
    // SAFETY: winsize is POD; we pass a valid pointer and check the return.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

fn to_io(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// Supply the `openpty` ABI that Android's Bionic libc omits.
///
/// `portable-pty` uses the conventional Unix helper, while Android exposes
/// only the underlying POSIX PTY operations. Keeping the compatibility shim
/// here preserves the provider API without adding a Termux shared-library
/// dependency to the release binary.
///
/// # Safety
///
/// The output pointers must be writable, and optional inputs must follow the
/// platform `openpty` ABI. A non-null `name_out` must have room for the PTY
/// path, as required by the C interface.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn openpty(
    master_out: *mut libc::c_int,
    slave_out: *mut libc::c_int,
    name_out: *mut libc::c_char,
    termios: *const libc::termios,
    winsize: *const libc::winsize,
) -> libc::c_int {
    if master_out.is_null() || slave_out.is_null() {
        // SAFETY: Bionic exposes a thread-local errno pointer.
        unsafe { *libc::__errno() = libc::EINVAL };
        return -1;
    }

    // SAFETY: every descriptor and pointer is checked before use, and all
    // descriptors opened on an error path are closed before returning.
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return -1;
        }
        if libc::grantpt(master) != 0 || libc::unlockpt(master) != 0 {
            libc::close(master);
            return -1;
        }

        let mut path = [0 as libc::c_char; 128];
        let name_error = libc::ptsname_r(master, path.as_mut_ptr(), path.len());
        if name_error != 0 {
            libc::close(master);
            *libc::__errno() = name_error;
            return -1;
        }

        let slave = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        if slave < 0 {
            libc::close(master);
            return -1;
        }
        if !termios.is_null() && libc::tcsetattr(slave, libc::TCSANOW, termios) != 0 {
            libc::close(slave);
            libc::close(master);
            return -1;
        }
        if !winsize.is_null() && libc::ioctl(slave, libc::TIOCSWINSZ, winsize) != 0 {
            libc::close(slave);
            libc::close(master);
            return -1;
        }
        if !name_out.is_null() {
            libc::strcpy(name_out, path.as_ptr());
        }

        *master_out = master;
        *slave_out = slave;
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn resize_records_the_size_the_child_sees_and_skips_repeats() {
        let mut pane = PtyProcess::spawn("/bin/sh", &["-c", "sleep 5"], 80, 24, None, &[]).unwrap();
        assert_eq!(pane.size(), (80, 24));
        pane.resize(100, 30).unwrap();
        assert_eq!(pane.size(), (100, 30));
        pane.resize(100, 30).unwrap();
        // SAFETY: TIOCGWINSZ on our own master fd into a zeroed winsize.
        let winsize = unsafe {
            let mut winsize: libc::winsize = std::mem::zeroed();
            libc::ioctl(pane.raw_fd(), libc::TIOCGWINSZ, &mut winsize);
            winsize
        };
        assert_eq!((winsize.ws_col, winsize.ws_row), (100, 30));
        pane.kill().unwrap();
    }

    #[test]
    fn a_shell_with_unread_output_is_killed_and_reaped_promptly() {
        // Twenty KiB the server never reads fills the tty's output queue, so
        // the shell blocks in write and, once killed, must drain on exit.
        let mut pane = PtyProcess::spawn(
            "/bin/sh",
            &[
                "-c",
                "i=0; while [ $i -lt 400 ]; do printf '%050d\n' $i; i=$((i+1)); done; sleep 30",
            ],
            80,
            24,
            None,
            &[],
        )
        .unwrap();
        pane.set_nonblocking().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        pane.kill().unwrap();
        assert!(pane.termination_finished);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "kill took {:?}",
            started.elapsed()
        );
        assert!(pane.try_wait_exited());
    }

    #[test]
    fn termination_escalates_for_the_whole_process_group() {
        let mut pane = PtyProcess::spawn(
            "/bin/sh",
            &["-c", "trap '' TERM; sleep 30 & wait"],
            80,
            24,
            None,
            &[],
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let group = pane.process_group;
        let started = Instant::now();
        pane.kill().unwrap();
        pane.kill().unwrap();
        assert!(pane.termination_finished);
        assert!(started.elapsed() < Duration::from_millis(250));

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            // SAFETY: signal 0 only probes the test process group.
            let gone = unsafe { libc::kill(-group, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if gone {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "pane descendants survived shutdown"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
