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
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    // Keep a childless recovery terminal open without generating EOF events.
    _recovery_slave: Option<Box<dyn portable_pty::SlavePty + Send>>,
    foreground_shutdown: Option<crate::process_watch::ProcessWatch>,
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
            child: Some(child),
            _recovery_slave: None,
            foreground_shutdown: None,
            fd,
            process_group,
            termination_finished: false,
            size: std::cell::Cell::new((cols, rows)),
        })
    }

    /// Retain a failed restore as a quiet terminal without starting any command
    /// in an unrelated directory. The slave prevents EOF readiness while idle.
    pub(crate) fn unavailable(cols: u16, rows: u16) -> std::io::Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_io)?;
        let fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| std::io::Error::other("PTY master has no raw fd"))?;
        Ok(Self {
            master: pair.master,
            child: None,
            _recovery_slave: Some(pair.slave),
            foreground_shutdown: None,
            fd,
            process_group: 0,
            termination_finished: true,
            size: std::cell::Cell::new((cols, rows)),
        })
    }

    /// The master fd, for registering with `mio` (`SourceFd`).
    pub fn raw_fd(&self) -> RawFd {
        self.fd
    }

    /// Let an orchestration bootstrap read a long launch without the kernel's
    /// canonical-line limit. Its reader restores canonical input and echo
    /// before starting the provider. Call before queueing any launch bytes.
    pub(crate) fn prepare_launch_input(&self) -> std::io::Result<()> {
        // SAFETY: the master is owned by self and termios is initialized by
        // tcgetattr before it is inspected or passed back to tcsetattr.
        unsafe {
            let mut attrs: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(self.fd, &mut attrs) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            attrs.c_lflag &= !(libc::ICANON | libc::ECHO);
            if libc::tcsetattr(self.fd, libc::TCSANOW, &attrs) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
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

    /// Diagnose a terminal read boundary only when explicitly enabled, without
    /// consuming the exit status that process-group teardown still owns.
    pub(crate) fn trace_read_end(&self, error: Option<&std::io::Error>) {
        if !pty_diagnostics_enabled() {
            return;
        }
        let pid = self.child_pid();
        let observed = pid.map(|pid| observe_child_exit(pid).map_err(|error| error.raw_os_error()));
        eprintln!(
            "uniterm: pty-read-end fd={} child_pid={pid:?} fgpid={:?} read_eof={} read_errno={:?} waitid_pid_code_status={observed:?}",
            self.fd,
            self.foreground_process_group(),
            error.is_none(),
            error.and_then(std::io::Error::raw_os_error),
        );
        #[cfg(target_os = "macos")]
        if let Some(pid) = pid {
            let reason = observe_macos_exit_reason(pid).map_err(|error| error.raw_os_error());
            eprintln!(
                "uniterm: pty-exit-reason child_pid={pid} namespace_code_flags_size={reason:?}"
            );
        }
    }

    fn trace_signal(&self, target: i32, signal: i32) {
        if signal == libc::SIGKILL && pty_diagnostics_enabled() {
            eprint!(
                "{}",
                signal_diagnostic(std::process::id(), self.child_pid(), target, signal)
            );
        }
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
        self.child
            .as_mut()
            .is_none_or(|child| matches!(child.try_wait(), Ok(Some(_))))
    }

    /// Wait for the child to exit; returns whether it succeeded. Only for
    /// callers that keep reading the master meanwhile or own no master at all:
    /// the server's termination path uses the draining reap instead.
    pub fn wait(&mut self) -> std::io::Result<bool> {
        self.child
            .as_mut()
            .map_or(Ok(true), |child| Ok(child.wait().map_err(to_io)?.success()))
    }

    /// Start a process-group shutdown without waiting, so callers terminating
    /// many panes can grant one collective grace period instead of one per pane.
    pub fn request_terminate(&mut self) -> std::io::Result<bool> {
        if self.termination_finished {
            return Ok(false);
        }
        self.capture_foreground_shutdown(false)?;
        self.signal_foreground(libc::SIGTERM)?;
        if self.signal_group(libc::SIGTERM)? {
            Ok(true)
        } else {
            self.signal_process(libc::SIGTERM)
        }
    }

    /// End a process-group grace period, kill survivors, and reap the pane shell.
    pub fn finish_terminate(&mut self) -> std::io::Result<()> {
        if self.termination_finished {
            return Ok(());
        }
        self.force_terminate()?;
        self.reap_while_draining();
        self.termination_finished = true;
        Ok(())
    }

    /// Escalate without waiting. Keep the unreaped session leader through both
    /// group signals so no unrelated session can acquire its session id.
    pub(crate) fn force_terminate(&mut self) -> std::io::Result<()> {
        if self.termination_finished {
            return Ok(());
        }
        self.signal_foreground(libc::SIGKILL)?;
        if !self.signal_group(libc::SIGKILL)? {
            let _ = self.signal_process(libc::SIGKILL)?;
        }
        Ok(())
    }

    /// Start asynchronous teardown without reaping the session leader.
    pub(crate) fn begin_terminate(&mut self) -> std::io::Result<()> {
        self.request_terminate().map(|_| ())
    }

    /// The owned shell pid, used only to register a native exit watch.
    pub(crate) fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|child| child.process_id())
    }

    fn capture_foreground_shutdown(&mut self, legacy: bool) -> std::io::Result<()> {
        if self.foreground_shutdown.is_some() {
            return Ok(());
        }
        let Some(group) = self.foreground_process_group() else {
            return Ok(());
        };
        if group <= 1 || group == self.process_group {
            return Ok(());
        }
        let watch = crate::process_watch::ProcessWatch::new(group).ok();
        // SAFETY: getsid only queries the foreground leader selected by our
        // own PTY. The unreaped shell reserves the session id through shutdown.
        if unsafe { libc::getsid(group) } != self.process_group {
            return Ok(());
        }
        if !legacy {
            if let Some(watch) = watch {
                if watch.signal_group(0)? {
                    self.foreground_shutdown = Some(watch);
                    return Ok(());
                }
            }
        }
        // POSIX has no identity-bound group signal. Kill the validated group
        // immediately, while the leader can still establish session ownership;
        // TERM followed by a delayed numeric kill could target a reused PGID or
        // strand resistant descendants after their leader exits. The check and
        // kill are not atomic on these platforms, so recheck the foreground too.
        if self.foreground_process_group() == Some(group)
            && unsafe { libc::getsid(group) } == self.process_group
        {
            self.trace_signal(-group, libc::SIGKILL);
            // SAFETY: the foreground group still belongs to our owned session.
            if unsafe { libc::kill(-group, libc::SIGKILL) } != 0 {
                Self::signal_outcome(std::io::Error::last_os_error())?;
            }
        }
        Ok(())
    }

    fn signal_foreground(&self, signal: i32) -> std::io::Result<()> {
        if let Some(watch) = &self.foreground_shutdown {
            self.trace_signal(-watch.pid, signal);
            watch.signal_group(signal)?;
        }
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
        self.trace_signal(-self.process_group, signal);
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
        self.trace_signal(self.process_group, signal);
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
        match (self.master.process_group_leader(), self.child_pid()) {
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

fn pty_diagnostics_enabled() -> bool {
    std::env::var_os("UNITERM_PTY_DIAGNOSTICS").as_deref() == Some(std::ffi::OsStr::new("1"))
}

fn signal_diagnostic(caller: u32, child: Option<u32>, target: i32, signal: i32) -> String {
    format!(
        "uniterm: pty-signal caller_pid={caller} child_pid={child:?} target={target} signal={signal}\n"
    )
}

// Diagnostic-only private Darwin ABI. Flavor 25 observes an owned zombie's
// termination reason without consuming its wait status or reading a payload.
// Apple defines the packed layout in bsd/sys/proc_info.h, the flavor in
// bsd/sys/proc_info_private.h, and parent/zombie access in bsd/kern/proc_info.c:
// https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/proc_info.h
// https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/proc_info_private.h
// https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/proc_info.c
#[cfg(target_os = "macos")]
#[repr(C, packed)]
struct MacExitReason {
    namespace: u32,
    code: u64,
    flags: u64,
    reason_buf_size: u32,
}

#[cfg(target_os = "macos")]
const _: () = {
    assert!(std::mem::size_of::<MacExitReason>() == 24);
    assert!(std::mem::align_of::<MacExitReason>() == 1);
    assert!(std::mem::offset_of!(MacExitReason, namespace) == 0);
    assert!(std::mem::offset_of!(MacExitReason, code) == 4);
    assert!(std::mem::offset_of!(MacExitReason, flags) == 12);
    assert!(std::mem::offset_of!(MacExitReason, reason_buf_size) == 20);
};

#[cfg(target_os = "macos")]
impl MacExitReason {
    fn fields(self) -> (u32, u64, u64, u32) {
        // Copy packed fields by value; never create unaligned references.
        (self.namespace, self.code, self.flags, self.reason_buf_size)
    }
}

#[cfg(target_os = "macos")]
fn observe_macos_exit_reason(pid: u32) -> std::io::Result<(u32, u64, u64, u32)> {
    const PROC_PIDEXITREASONBASICINFO: libc::c_int = 25;
    let mut reason = MacExitReason {
        namespace: 0,
        code: 0,
        flags: 0,
        reason_buf_size: 0,
    };
    let size = std::mem::size_of::<MacExitReason>() as libc::c_int;
    // SAFETY: the fixed-size buffer matches Apple's packed ABI. The query only
    // observes our owned child; unsupported flavors or absent reasons are errors.
    let count = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDEXITREASONBASICINFO,
            0,
            std::ptr::from_mut(&mut reason).cast(),
            size,
        )
    };
    if count <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    if count != size {
        return Err(std::io::Error::from_raw_os_error(libc::EIO));
    }
    Ok(reason.fields())
}

fn observe_child_exit(pid: u32) -> std::io::Result<(libc::pid_t, libc::c_int, libc::c_int)> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // SAFETY: waitid targets only this owned child, writes initialized
        // storage, and WNOWAIT preserves the status for the actual reaper.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        #[cfg(target_os = "linux")]
        // SAFETY: waitid initializes the SIGCHLD fields; zero pid means no exit.
        let (pid, status) = unsafe { (info.si_pid(), info.si_status()) };
        #[cfg(target_os = "macos")]
        let (pid, status) = (info.si_pid, info.si_status);
        Ok((pid, info.si_code, status))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
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

    // Exercise the exact vendored child helper, including its portable fallback,
    // without exposing a production API or adding upstream's example dev-deps.
    mod child_fds {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../vendor/portable-pty/src/unix_cloexec.rs"
        ));
    }

    // The exact vendored Linux allocation helper, tested without a fcntl pass.
    #[cfg(target_os = "linux")]
    mod pty_alloc {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../vendor/portable-pty/src/unix_openpty.rs"
        ));
    }

    struct TestPty(PtyProcess);

    impl Drop for TestPty {
        fn drop(&mut self) {
            let _ = self.0.kill();
        }
    }

    #[test]
    fn signal_diagnostics_are_bounded_numeric_records() {
        for target in [i32::MIN, -64, 64, i32::MAX] {
            for child in [None, Some(u32::MAX)] {
                let line = signal_diagnostic(u32::MAX, child, target, libc::SIGKILL);
                assert!(line.len() < 160);
                assert_eq!(line.lines().count(), 1);
                assert!(line.ends_with(" signal=9\n"));
                assert!(line.contains(&format!(" target={target} ")));
                assert!(line.is_ascii());
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_exit_reason_uses_packed_native_fields() {
        let mut bytes = [0u8; 24];
        bytes[..4].copy_from_slice(&3u32.to_ne_bytes());
        bytes[4..12].copy_from_slice(&2u64.to_ne_bytes());
        bytes[12..20].copy_from_slice(&u64::MAX.to_ne_bytes());
        bytes[20..].copy_from_slice(&5120u32.to_ne_bytes());
        // SAFETY: the compile-time assertions fix the 24-byte packed layout;
        // every field is an integer and every bit pattern is valid.
        let reason = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<MacExitReason>()) };
        assert_eq!(reason.fields(), (3, 2, u64::MAX, 5120));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn exit_diagnostics_preserve_live_child_and_waitable_status() {
        let mut pane = TestPty(
            PtyProcess::spawn(
                "/bin/sh",
                &["-c", "read -r line; exit 7"],
                80,
                24,
                None,
                &[],
            )
            .unwrap(),
        );
        pane.0.set_nonblocking().unwrap();
        let pid = pane.0.child_pid().unwrap();
        assert_eq!(observe_child_exit(pid).unwrap().0, 0);
        assert_eq!(observe_child_exit(pid).unwrap().0, 0);
        assert_eq!(pane.0.write_some(b"go\n").unwrap(), 3);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = [0; 128];
        let observed = loop {
            // macOS may keep tty close pending until echoed input is drained.
            loop {
                assert!(Instant::now() < deadline, "fixture child did not exit");
                match pane.0.read(&mut output) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.raw_os_error() == Some(libc::EIO) =>
                    {
                        break;
                    }
                    Err(error) => panic!("fixture PTY read failed: {error}"),
                }
            }
            let observed = observe_child_exit(pid).unwrap();
            if observed.0 != 0 {
                break observed;
            }
            assert!(Instant::now() < deadline, "fixture child did not exit");
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(observed, (pid as libc::pid_t, libc::CLD_EXITED, 7));
        #[cfg(target_os = "macos")]
        // Normal exit may have no kernel reason. Either result must leave the
        // child waitable; this private diagnostic is never required to succeed.
        let _ = observe_macos_exit_reason(pid);
        assert_eq!(observe_child_exit(pid).unwrap(), observed);
        // Only the normal teardown consumes the preserved zombie/status.
        pane.0.finish_terminate().unwrap();
        assert!(!pane.0.wait().unwrap());
        assert_eq!(
            observe_child_exit(pid).unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn executable_with_missing_shebang_interpreter_reports_spawn_error() {
        use std::os::unix::fs::PermissionsExt;

        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("uniterm-pty-exec-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let fixture = Fixture(directory);
        let program = fixture.0.join("executable");
        let interpreter = fixture.0.join("missing-interpreter");
        std::fs::write(&program, format!("#!{}\n", interpreter.display())).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(program.is_file());
        assert!(!interpreter.exists());

        // The file passes the launcher's path/executable checks, but exec must
        // report ENOENT through its private error pipe. Keep even an erroneous
        // successful spawn isolated, including any platform crash artifacts.
        if let Ok(pane) = PtyProcess::spawn(
            program.to_str().unwrap(),
            &[],
            80,
            24,
            Some(&fixture.0),
            &[],
        ) {
            let mut pane = TestPty(pane);
            pane.0.set_nonblocking().unwrap();
            pane.0.kill().unwrap();
            assert!(pane.0.try_wait_exited(), "failed exec child was not reaped");
            panic!("PTY spawn reported success despite a missing shebang interpreter");
        }
    }

    #[test]
    fn inherited_descriptor_is_closed_in_child_without_changing_parent() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        let source = std::fs::File::open("/dev/null").unwrap();
        // A high descriptor avoids the shell's own temporary descriptors.
        // F_DUPFD deliberately produces an inherited descriptor for this test.
        let raw = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD, 64) };
        assert!(raw >= 64, "{}", std::io::Error::last_os_error());
        // SAFETY: F_DUPFD returned a new descriptor owned only by this test.
        let inherited = unsafe { OwnedFd::from_raw_fd(raw) };
        let flags = unsafe { libc::fcntl(inherited.as_raw_fd(), libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0);
        let descriptor = inherited.as_raw_fd().to_string();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let check = "test ! -e /proc/self/fd/\"$1\"";
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let check = "test ! -e /dev/fd/\"$1\"";
        let mut pane = TestPty(
            PtyProcess::spawn(
                "/bin/sh",
                &["-c", check, "fd-check", &descriptor],
                80,
                24,
                None,
                &[],
            )
            .unwrap(),
        );
        pane.0.set_nonblocking().unwrap();
        // The command emits no output, so no master drain is needed to exit.
        let success = pane.0.wait().unwrap();
        pane.0.termination_finished = true;
        assert!(success, "parent descriptor leaked into the executed child");
        assert_eq!(
            unsafe { libc::fcntl(inherited.as_raw_fd(), libc::F_GETFD) },
            flags,
            "child descriptor cleanup changed the parent's flags"
        );
    }

    fn check_descriptor_opened_after_capture(cleanup: fn(RawFd) -> std::io::Result<()>) {
        use std::os::unix::process::CommandExt;

        let bound = child_fds::capture_bound().unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        // SAFETY: the child uses only syscalls, integer checks, and errno values.
        // Its new fd was absent when the parent captured the descriptor bound.
        unsafe {
            command.pre_exec(move || {
                let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let before = libc::fcntl(fd, libc::F_GETFD);
                if before < 0 || before & libc::FD_CLOEXEC != 0 {
                    return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                }
                cleanup(bound)?;
                let after = libc::fcntl(fd, libc::F_GETFD);
                if after < 0 || after & libc::FD_CLOEXEC == 0 {
                    return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                }
                Ok(())
            });
        }
        assert!(command.status().unwrap().success());
    }

    #[test]
    fn descriptor_opened_after_capture_is_marked_close_on_exec() {
        check_descriptor_opened_after_capture(child_fds::mark_cloexec);
    }

    #[test]
    fn fallback_covers_descriptors_opened_after_capture() {
        check_descriptor_opened_after_capture(child_fds::mark_cloexec_fallback);
    }

    #[test]
    fn range_covers_descriptors_opened_after_capture() {
        check_descriptor_opened_after_capture(child_fds::mark_cloexec_range);
    }

    #[test]
    fn fallback_preserves_the_exec_error_pipe() {
        use std::os::unix::process::CommandExt;

        let bound = child_fds::capture_bound().unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        // SAFETY: only the syscall helper and a non-allocating errno value run
        // in the child. Returning this error exercises std's private error pipe.
        unsafe {
            command.pre_exec(move || {
                child_fds::mark_cloexec_fallback(bound)?;
                Err(std::io::Error::from_raw_os_error(libc::EACCES))
            });
        }
        match command.spawn() {
            Err(error) => assert_eq!(error.raw_os_error(), Some(libc::EACCES)),
            Ok(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("fallback discarded the child's launch error");
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn getdents_records_reject_malformed_lengths_names_and_overflow() {
        fn record(name: &[u8]) -> Vec<u8> {
            let length = (19 + name.len() + 1).next_multiple_of(8);
            let mut bytes = vec![0u8; length];
            bytes[16..18].copy_from_slice(&(length as u16).to_ne_bytes());
            bytes[19..19 + name.len()].copy_from_slice(name);
            bytes
        }

        let valid = record(b"2147483647");
        assert_eq!(
            child_fds::next_fd_entry(&valid).unwrap(),
            (valid.len(), Some(i32::MAX))
        );
        for name in [b".".as_slice(), b".."] {
            let entry = record(name);
            assert_eq!(
                child_fds::next_fd_entry(&entry).unwrap(),
                (entry.len(), None)
            );
        }
        for length in 0..valid.len() {
            assert!(child_fds::next_fd_entry(&valid[..length]).is_err());
        }
        for length in [0u16, 19, u16::MAX] {
            let mut malformed = valid.clone();
            malformed[16..18].copy_from_slice(&length.to_ne_bytes());
            assert!(child_fds::next_fd_entry(&malformed).is_err());
        }
        let mut unterminated = valid.clone();
        unterminated[19..].fill(b'1');
        assert!(child_fds::next_fd_entry(&unterminated).is_err());
        for name in [b"".as_slice(), b"-1", b"12x", b"2147483648"] {
            assert!(child_fds::next_fd_entry(&record(name)).is_err());
        }
        let first = record(b"64");
        let mut consecutive = first.clone();
        consecutive.extend(record(b"65"));
        let (length, fd) = child_fds::next_fd_entry(&consecutive).unwrap();
        assert_eq!((length, fd), (first.len(), Some(64)));
        assert_eq!(
            child_fds::next_fd_entry(&consecutive[length..]).unwrap().1,
            Some(65)
        );
    }

    #[test]
    #[ignore = "explicit launch latency measurement; run serially with --nocapture"]
    fn pty_spawn_latency_measurement() {
        let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        eprintln!("PTY launch RLIMIT_NOFILE soft={}", limit.rlim_cur);
        for inherited_count in [0, 64] {
            let _descriptors: Vec<_> = (0..inherited_count)
                .map(|_| std::fs::File::open("/dev/null").unwrap())
                .collect();
            let mut durations = Vec::with_capacity(200);
            let overall = Instant::now();
            for _ in 0..200 {
                let started = Instant::now();
                let mut pane = TestPty(
                    PtyProcess::spawn("/bin/sh", &["-c", "exit 0"], 80, 24, None, &[]).unwrap(),
                );
                durations.push(started.elapsed());
                pane.0.set_nonblocking().unwrap();
                let success = pane.0.wait().unwrap();
                pane.0.termination_finished = true;
                assert!(success);
            }
            durations.sort_unstable();
            eprintln!(
                "PTY spawn: inherited={inherited_count}, n=200, p50={:?}, p95={:?}, total={:?}",
                durations[99],
                durations[189],
                overall.elapsed()
            );
            assert!(
                durations[189] < Duration::from_millis(50),
                "PTY spawn p95 exceeds the 50ms input latency budget"
            );
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use std::os::unix::process::CommandExt;

            // Force the old-kernel descriptor path even on a modern kernel.
            // This measures child setup plus exec, without requiring seccomp or
            // changing the process-wide descriptor limit to simulate a kernel.
            let mut durations = Vec::with_capacity(200);
            let overall = Instant::now();
            for _ in 0..200 {
                let started = Instant::now();
                let bound = child_fds::capture_bound().unwrap();
                let mut command = std::process::Command::new("/bin/sh");
                command.args(["-c", "exit 0"]);
                // SAFETY: the helper performs only syscalls and stack parsing.
                unsafe {
                    command.pre_exec(move || child_fds::mark_cloexec_fallback(bound));
                }
                let mut child = command.spawn().unwrap();
                durations.push(started.elapsed());
                assert!(child.wait().unwrap().success());
            }
            durations.sort_unstable();
            eprintln!(
                "Forced descriptor fallback spawn: n=200, p50={:?}, p95={:?}, total={:?}",
                durations[99],
                durations[189],
                overall.elapsed()
            );
            assert!(
                durations[189] < Duration::from_millis(50),
                "forced descriptor fallback p95 exceeds the 50ms input latency budget"
            );
        }
    }

    fn interactive_foreground_cleanup(leader_ignores_term: bool, legacy: bool) {
        let mut pane = TestPty(PtyProcess::spawn("/bin/sh", &["-i"], 100, 24, None, &[]).unwrap());
        pane.0.set_nonblocking().unwrap();
        let trap = if leader_ignores_term {
            "trap \"\" TERM; "
        } else {
            ""
        };
        let command = format!(
            r#"/bin/sh -c 'trap "" HUP; {trap}/bin/sh -c "trap \"\" HUP TERM; echo __desc:\$\$; exec sleep 60" & echo __job:$$; wait'
"#
        );
        pane.0.write_some(command.as_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = String::new();
        let mut buffer = [0u8; 4096];
        let (job, descendant) = loop {
            match pane.0.read(&mut buffer) {
                Ok(n) => output.push_str(&String::from_utf8_lossy(&buffer[..n])),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                other => panic!("PTY read failed: {other:?}; {output}"),
            }
            let pid = |marker: &str| {
                output.lines().find_map(|line| {
                    line.rsplit_once(marker)
                        .and_then(|(_, value)| value.trim().parse::<i32>().ok())
                })
            };
            if let (Some(job), Some(descendant)) = (pid("__job:"), pid("__desc:")) {
                if pane.0.foreground_process_group() == Some(job) {
                    break (job, descendant);
                }
            }
            assert!(
                Instant::now() < deadline,
                "interactive job failed to start: {output}"
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        let leader_watch = crate::process_watch::ProcessWatch::new(job).unwrap();
        let descendant_watch = crate::process_watch::ProcessWatch::new(descendant).unwrap();
        let shell_watch = crate::process_watch::ProcessWatch::new(pane.0.process_group).unwrap();
        assert_ne!(job, pane.0.process_group);
        if legacy {
            pane.0.capture_foreground_shutdown(true).unwrap();
        }
        pane.0.begin_terminate().unwrap();
        if !leader_ignores_term {
            assert!(leader_watch.wait_exit(Duration::from_secs(2)).unwrap());
        }
        // The shell may exit before escalation; keep it unreaped so its SID
        // remains reserved while the captured foreground group is killed.
        pane.0.signal_process(libc::SIGKILL).unwrap();
        // Like the production reaper, consume final job notices so macOS can
        // finish closing the tty. Do not reap the shell: the next assertion
        // still needs its reserved session identity after the leader exits.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            loop {
                match pane.0.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.raw_os_error() == Some(libc::EIO) =>
                    {
                        break;
                    }
                    Err(error) => panic!("PTY shutdown drain failed: {error}"),
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if shell_watch
                .wait_exit(remaining.min(Duration::from_millis(10)))
                .unwrap()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "shell did not exit after draining"
            );
        }
        pane.0.force_terminate().unwrap();
        assert!(
            descendant_watch.wait_exit(Duration::from_secs(2)).unwrap(),
            "foreground descendant survived"
        );
        assert!(leader_watch.wait_exit(Duration::from_secs(2)).unwrap());
        pane.0.finish_terminate().unwrap();
        // Repeated teardown of already-gone groups is safe and has no work.
        pane.0.kill().unwrap();
    }

    #[test]
    fn interactive_foreground_and_descendants_are_killed() {
        interactive_foreground_cleanup(true, false);
    }

    #[test]
    fn foreground_descendants_are_killed_after_their_leader_exits() {
        interactive_foreground_cleanup(false, false);
    }

    #[test]
    fn legacy_foreground_cleanup_does_not_leave_resistant_descendants() {
        interactive_foreground_cleanup(false, true);
    }

    #[test]
    fn unavailable_pty_stays_quiet_without_a_child() {
        let mut pane = PtyProcess::unavailable(80, 24).unwrap();
        pane.set_nonblocking().unwrap();
        assert_eq!(pane.child_pid(), None);
        assert!(!pane.child_owns_foreground());
        let mut poll = mio::Poll::new().unwrap();
        poll.registry()
            .register(
                &mut mio::unix::SourceFd(&pane.raw_fd()),
                mio::Token(1),
                mio::Interest::READABLE,
            )
            .unwrap();
        let mut events = mio::Events::with_capacity(4);
        poll.poll(&mut events, Some(Duration::from_millis(20)))
            .unwrap();
        assert!(events.is_empty());
        assert_eq!(
            pane.read(&mut [0u8; 1]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        pane.kill().unwrap();
    }

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

    #[cfg(target_os = "linux")]
    fn close_on_exec(fd: &std::os::fd::OwnedFd) -> bool {
        use std::os::fd::AsRawFd;
        // SAFETY: F_GETFD only reads the flags of a live descriptor.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0, "{}", std::io::Error::last_os_error());
        flags & libc::FD_CLOEXEC != 0
    }

    #[cfg(target_os = "linux")]
    fn descriptor_target(fd: &std::os::fd::OwnedFd) -> std::path::PathBuf {
        use std::os::fd::AsRawFd;
        std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap()
    }

    #[cfg(target_os = "linux")]
    fn test_winsize() -> libc::winsize {
        libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_pty_pair_is_close_on_exec_from_allocation() {
        use std::os::fd::AsRawFd;

        // No fcntl pass runs after these opens, so the flag proves that each
        // descriptor was created close-on-exec rather than marked later.
        let (master, slave) = pty_alloc::open_cloexec_pair(&test_winsize()).unwrap();
        assert!(close_on_exec(&master), "PTY master was inheritable");
        assert!(close_on_exec(&slave), "PTY slave was inheritable");
        // SAFETY: isatty only inspects a live descriptor.
        assert_eq!(unsafe { libc::isatty(slave.as_raw_fd()) }, 1);

        let mut name: [libc::c_char; 512] = [0; 512];
        // SAFETY: the buffer is writable for its full length and the master is live.
        let error = unsafe { libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) };
        assert_eq!(error, 0, "{}", std::io::Error::from_raw_os_error(error));
        // SAFETY: ptsname_r succeeded, so the buffer holds a NUL-terminated name.
        let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
        let peer = std::path::PathBuf::from(name.to_str().unwrap());
        assert_eq!(
            descriptor_target(&slave),
            peer,
            "slave is not the master's peer"
        );

        // SAFETY: winsize contains only integers, so all-zero is a valid value.
        let mut seen: libc::winsize = unsafe { std::mem::zeroed() };
        assert_eq!(
            // SAFETY: TIOCGWINSZ writes one winsize into a valid local value.
            unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCGWINSZ, &mut seen) },
            0
        );
        assert_eq!((seen.ws_row, seen.ws_col), (24, 80));

        // Kernels before Linux 4.13 use the devpts path fallback instead.
        let fallback = pty_alloc::open_peer_path(&master).unwrap();
        assert!(
            close_on_exec(&fallback),
            "fallback PTY slave was inheritable"
        );
        assert_eq!(descriptor_target(&fallback), peer);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_pty_allocation_failure_releases_the_master() {
        use std::os::fd::AsRawFd;

        let mut slave = None;
        let error = pty_alloc::open_pair_with(&test_winsize(), |master| {
            assert!(close_on_exec(master), "PTY master was inheritable");
            slave = Some(pty_alloc::open_peer_path(master)?);
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        let slave = slave.unwrap();
        // Only the failed allocation held the master, so closing it hangs up
        // the slave. A concurrently forked test child can delay that until it
        // execs, never indefinitely; a leaked master would never hang up.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ready = libc::pollfd {
            fd: slave.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = libc::c_int::try_from(remaining.as_millis()).unwrap_or(libc::c_int::MAX);
            // SAFETY: `ready` is one valid pollfd for a live descriptor.
            let count = unsafe { libc::poll(&mut ready, 1, timeout) };
            if count < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) && !remaining.is_zero() {
                    continue;
                }
                panic!("poll for PTY hangup failed: {error}");
            }
            assert_eq!(count, 1, "failed PTY allocation kept its master open");
            break;
        }
        assert_ne!(ready.revents & libc::POLLHUP, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pty_slaves_are_not_retained_by_concurrently_spawned_children() {
        use std::collections::HashSet;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Mutex;

        // Stop the allocator on every exit from the spawning side, including a
        // panic, so the scoped join can never wait for it forever.
        struct StopOnDrop<'a>(&'a AtomicBool);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        let stop = AtomicBool::new(false);
        let allocated = Mutex::new(HashSet::new());
        let (started, first_allocation) = std::sync::mpsc::sync_channel(1);
        let inherited = std::thread::scope(|scope| {
            let _stop = StopOnDrop(&stop);
            let (stop, allocated) = (&stop, &allocated);
            scope.spawn(move || {
                let system = native_pty_system();
                let mut started = Some(started);
                while !stop.load(Ordering::Relaxed) {
                    let pair = system.openpty(PtySize::default()).unwrap();
                    let name = pair.master.tty_name().expect("PTY slave has a name");
                    allocated.lock().unwrap().insert(name);
                    if let Some(started) = started.take() {
                        let _ = started.send(());
                    }
                }
            });
            // Spawn only once allocation is underway. If the allocator fails
            // first, its sender is dropped and the test fails here instead.
            first_allocation
                .recv()
                .expect("PTY allocator failed before its first pair");
            let mut inherited = Vec::new();
            for _ in 0..200 {
                // An ordinary std child, as runtime work and other tests spawn
                // concurrently, lists the descriptors it kept across exec. A
                // fixed executable and locale keep the listing deterministic.
                let output = std::process::Command::new("/bin/ls")
                    .args(["-l", "/proc/self/fd"])
                    .env("LC_ALL", "C")
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .unwrap();
                assert!(output.status.success());
                inherited.extend(
                    String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .filter_map(|line| line.split_once(" -> "))
                        .map(|(_, target)| std::path::PathBuf::from(target))
                        .filter(|target| target.starts_with("/dev/pts/")),
                );
            }
            inherited
        });
        let allocated = allocated.into_inner().unwrap();
        assert!(!allocated.is_empty());
        let retained: Vec<_> = inherited
            .iter()
            .filter(|target| allocated.contains(*target))
            .collect();
        assert!(
            retained.is_empty(),
            "children kept PTY slaves across exec: {retained:?}"
        );
    }
}
