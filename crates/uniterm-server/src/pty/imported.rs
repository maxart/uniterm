//! An upgraded engine owns the master, but the inherited shell is not its
//! child. Use kernel identities for termination and report an unknown status.

use crate::process_watch::ProcessWatch;
use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

pub(super) struct ImportedChild {
    pub(super) shell: ProcessWatch,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    foreground: Option<ProcessWatch>,
    #[cfg(target_os = "macos")]
    shutdown: Vec<darwin::Member>,
}

impl ImportedChild {
    pub(super) fn bind(fd: RawFd, pid: i32) -> io::Result<Self> {
        if pid <= 1 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        super::verify_master(fd)?;
        let shell = ProcessWatch::new(pid)?;
        // The source still reserves the original session during preparation.
        // Recheck the master after capturing the watch to exclude pid reuse.
        if unsafe { libc::tcgetsid(fd) } != pid || shell.wait_exit(Duration::ZERO)? {
            return Err(io::Error::other("Pane session changed during handoff"));
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if !shell.signal_group(0)? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "kernel does not support identity-bound process-group signals",
            ));
        }
        #[cfg(target_os = "macos")]
        {
            darwin::signal_function()?;
            darwin::process_identity(pid)?;
        }
        Ok(Self {
            shell,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            foreground: None,
            #[cfg(target_os = "macos")]
            shutdown: Vec::new(),
        })
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(super) fn terminate(&mut self, fd: RawFd, signal: i32) -> io::Result<()> {
        // The captured group handle remains usable after its leader exits.
        // Capture the foreground while the master still identifies our session.
        let shell = self.shell.pid;
        let fg = unsafe { libc::tcgetpgrp(fd) };
        if fg > 1
            && fg != shell
            && unsafe { libc::tcgetsid(fd) } == shell
            && self.foreground.as_ref().is_none_or(|watch| watch.pid != fg)
        {
            if let Ok(watch) = ProcessWatch::new(fg) {
                if unsafe { libc::getsid(fg) } == shell && unsafe { libc::tcgetpgrp(fd) } == fg {
                    if let Some(old) = self.foreground.replace(watch) {
                        old.signal_group(libc::SIGKILL)?;
                    }
                }
            }
        }
        if let Some(fg) = &self.foreground {
            fg.signal_group(signal)?;
        }
        self.shell.signal_group(signal)?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(super) fn terminate(&mut self, fd: RawFd, signal: i32) -> io::Result<()> {
        darwin::terminate(fd, &self.shell, signal, &mut self.shutdown)
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    pub(super) fn terminate(&mut self, _fd: RawFd, _signal: i32) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(target_os = "macos")]
mod darwin {
    use super::*;
    use std::ffi::c_int;

    type Signal = unsafe extern "C" fn(*const [u32; 8], c_int) -> c_int;

    // Weak lookup keeps older macOS versions able to start Workspaces. A
    // kernel without this ABI declines live import before commit.
    pub(super) fn signal_function() -> io::Result<Signal> {
        // SAFETY: libproc exports this exact signature in libproc.h. dlsym
        // returns a process-lifetime function pointer or null.
        let address =
            unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"proc_signal_with_audittoken".as_ptr()) };
        if address.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "identity-bound Pane shutdown is unavailable",
            ));
        }
        Ok(unsafe { std::mem::transmute::<*mut libc::c_void, Signal>(address) })
    }

    // XNU preserves p_uniqueid across exec, while p_idversion can change.
    // Only possession of the originally captured lifetime permits refreshing
    // the audit token after a foreground process changes its executable.
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub(super) struct Target {
        pid: i32,
        unique: u64,
    }

    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub(super) struct Member {
        group: i32,
        target: Target,
    }

    pub(super) fn process_identity(pid: i32) -> io::Result<(Target, u32)> {
        // PROC_PIDUNIQIDENTIFIERINFO in XNU's bsd/sys/proc_info_private.h.
        // The 56-byte ABI has p_uniqueid at byte 16 and p_idversion at byte 32.
        let mut info = [0u64; 7];
        let size = std::mem::size_of_val(&info) as c_int;
        let n = unsafe { libc::proc_pidinfo(pid, 17, 0, info.as_mut_ptr().cast(), size) };
        if n <= 0 {
            return Err(io::Error::last_os_error());
        }
        if n != size {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        Ok((
            Target {
                pid,
                unique: info[2],
            },
            info[4] as u32,
        ))
    }

    // Capture only the two groups a normal Pane stop owns. Sample the lifetime
    // first, then check the session. Retain these members through KILL even
    // after the shell or foreground group leader has exited.
    fn collect(session: i32, group: i32, targets: &mut Vec<Member>) -> io::Result<()> {
        if group <= 1 {
            return Ok(());
        }
        let mut members = vec![0i32; 16];
        loop {
            let size =
                i32::try_from(std::mem::size_of_val(&members[..])).map_err(io::Error::other)?;
            // SAFETY: the bounded byte count covers exactly the allocated pid array.
            let n = unsafe { libc::proc_listpgrppids(group, members.as_mut_ptr().cast(), size) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if (n as usize) < members.len() {
                members.truncate(n as usize);
                break;
            }
            members.resize(
                members
                    .len()
                    .checked_mul(2)
                    .ok_or_else(|| io::Error::other("process group too large"))?,
                0,
            );
        }
        for member in members {
            if member <= 1 {
                continue;
            }
            let target = match process_identity(member) {
                Ok((target, _)) => target,
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => continue,
                Err(e) => return Err(e),
            };
            // SAFETY: these kernel queries only read current session and group identifiers.
            if unsafe { libc::getpgid(member) } != group
                || unsafe { libc::getsid(member) } != session
            {
                continue;
            }
            targets.push(Member { group, target });
        }
        Ok(())
    }

    impl Member {
        // A live, captured process still in the original group and session
        // reserves both ids even after the imported shell has been reaped.
        fn anchors(self, session: i32) -> bool {
            (unsafe {
                libc::getpgid(self.target.pid) == self.group
                    && libc::getsid(self.target.pid) == session
            }) && process_identity(self.target.pid).is_ok_and(|(current, _)| current == self.target)
        }
    }

    // A TERM handler can fork, and a surviving member can exec. Re-enumerate
    // only groups whose ownership we can still establish across the query.
    fn refresh_members(
        fd: RawFd,
        shell: &ProcessWatch,
        members: &mut Vec<Member>,
    ) -> io::Result<()> {
        let session = shell.pid;
        let live_master = || -> io::Result<bool> {
            Ok(unsafe { libc::tcgetsid(fd) } == session && !shell.wait_exit(Duration::ZERO)?)
        };
        let mut groups: Vec<_> = members.iter().map(|m| m.group).collect();
        let mut observed = Vec::new();
        let foreground = if live_master()? {
            unsafe { libc::tcgetpgrp(fd) }
        } else {
            -1
        };
        if foreground > 1 {
            groups.push(foreground);
        }
        groups.push(session);
        groups.sort_unstable();
        groups.dedup();
        for group in groups {
            let anchor = members
                .iter()
                .copied()
                .find(|m| m.group == group && m.anchors(session));
            let owned_by_master = || -> io::Result<bool> {
                Ok(live_master()? && (group == session || unsafe { libc::tcgetpgrp(fd) } == group))
            };
            if anchor.is_none() && !owned_by_master()? {
                continue;
            }
            observed.clear();
            collect(session, group, &mut observed)?;
            if anchor.is_some_and(|m| m.anchors(session)) || owned_by_master()? {
                members.append(&mut observed);
            }
        }
        members.sort_unstable();
        members.dedup();
        Ok(())
    }

    impl Target {
        fn signal(self, send: Signal, signal: i32) -> io::Result<()> {
            let mut rejected = None;
            for _ in 0..8 {
                let (current, version) = match process_identity(self.pid) {
                    Ok(info) => info,
                    Err(e) if e.raw_os_error() == Some(libc::ESRCH) => return Ok(()),
                    Err(e) => return Err(e),
                };
                if current != self || rejected == Some(version) {
                    return Ok(());
                }
                let mut token = [0u32; 8];
                token[5] = self.pid as u32;
                token[7] = version;
                // SAFETY: the kernel checks pid and version together and
                // independently enforces permission. If an exec races this
                // signal, refresh the token only for the same process lifetime.
                match unsafe { send(&token, signal) } {
                    0 => return Ok(()),
                    libc::ESRCH => rejected = Some(version),
                    error => return Err(io::Error::from_raw_os_error(error)),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Pane process kept changing executable during shutdown",
            ))
        }
    }

    pub(super) fn terminate(
        fd: RawFd,
        shell: &ProcessWatch,
        signal: i32,
        targets: &mut Vec<Member>,
    ) -> io::Result<()> {
        let send = signal_function()?;
        let mut first_error = refresh_members(fd, shell, targets).err();
        for member in targets.iter() {
            if let Err(error) = member.target.signal(send, signal) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};

        #[test]
        fn imported_shutdown_follows_exec_but_rejects_a_different_process_lifetime() {
            let mut child = Command::new("/bin/sh")
                .args(["-c", "echo ready; read -r trigger; exec /bin/cat"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut input = child.stdin.take().unwrap();
            let mut output = BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            assert_eq!(line, "ready\n");
            let (target, _) = process_identity(child.id() as i32).unwrap();
            input.write_all(b"exec\nnew image\n").unwrap();
            line.clear();
            output.read_line(&mut line).unwrap();
            assert_eq!(line, "new image\n");
            let send = signal_function().unwrap();
            let unrelated = Target {
                unique: target.unique.wrapping_add(1),
                ..target
            };
            unrelated.signal(send, libc::SIGKILL).unwrap();
            input.write_all(b"still running\n").unwrap();
            line.clear();
            output.read_line(&mut line).unwrap();
            assert_eq!(line, "still running\n");
            target.signal(send, libc::SIGKILL).unwrap();
            drop(input);
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
        }
    }
}
