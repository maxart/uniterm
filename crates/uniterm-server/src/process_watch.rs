//! Native one-shot foreground-process exit notification. Linux uses pidfd;
//! macOS uses kqueue EVFILT_PROC/NOTE_EXIT. Each watch is registered in the
//! existing mio poll set, so exit creates one kernel event and no scan/timer.

use std::os::fd::RawFd;

use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};

/// Resolve the running image, independently of argv[0] and launch symlinks.
/// Called only by event-triggered runtime detection, never by the render loop.
pub(crate) fn executable_path(pid: i32) -> Option<std::path::PathBuf> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut buffer = [0u8; 4096];
        // SAFETY: proc_pidpath writes at most the supplied buffer length and
        // retains no pointer. A failed lookup returns no identity.
        let length =
            unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        if length <= 0 {
            return None;
        }
        let bytes = &buffer[..(length as usize).min(buffer.len())];
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        Some(std::ffi::OsString::from_vec(bytes[..end].to_vec()).into())
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

pub struct ProcessWatch {
    fd: RawFd,
    pub pid: i32,
}

impl ProcessWatch {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn new(pid: i32) -> std::io::Result<ProcessWatch> {
        // SAFETY: pidfd_open takes integer values and returns a new owned fd.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(ProcessWatch { fd, pid })
    }

    #[cfg(target_os = "macos")]
    pub fn new(pid: i32) -> std::io::Result<ProcessWatch> {
        // SAFETY: kqueue creates an owned descriptor; kevent receives a fully
        // initialized event and no output buffer for this registration call.
        unsafe {
            let fd = libc::kqueue();
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut event: libc::kevent = std::mem::zeroed();
            event.ident = pid as libc::uintptr_t;
            event.filter = libc::EVFILT_PROC;
            event.flags = libc::EV_ADD | libc::EV_ONESHOT;
            event.fflags = libc::NOTE_EXIT;
            if libc::kevent(fd, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) < 0 {
                let error = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(error);
            }
            Ok(ProcessWatch { fd, pid })
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    pub fn new(_pid: i32) -> std::io::Result<ProcessWatch> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "native process watches are unsupported on this platform",
        ))
    }

    pub fn register(&mut self, registry: &Registry, token: Token) -> std::io::Result<()> {
        registry.register(&mut SourceFd(&self.fd), token, Interest::READABLE)
    }

    pub fn deregister(&mut self, registry: &Registry) {
        let _ = registry.deregister(&mut SourceFd(&self.fd));
    }

    /// Wait for this exact process to exit during a user-requested stop.
    /// Used only on a runtime blocking worker, never on the mio loop.
    pub(crate) fn wait_exit(&self, timeout: std::time::Duration) -> std::io::Result<bool> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let mut descriptor = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let millis = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
            // SAFETY: the watch owns the fd and descriptor is valid for this call.
            let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
            if ready >= 0 {
                return Ok(ready > 0);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// Signal the captured process-group identity, including surviving members
    /// after the leader exits. Linux 6.9 added PIDFD_SIGNAL_PROCESS_GROUP;
    /// older kernels and other platforms return false to request a safe fallback.
    pub(crate) fn signal_group(&self, signal: i32) -> std::io::Result<bool> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            const PIDFD_SIGNAL_PROCESS_GROUP: libc::c_uint = 1 << 2;
            // SAFETY: this pidfd refers to the captured group leader's struct
            // pid. The kernel resolves its PGID members without a numeric lookup.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.fd,
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    PIDFD_SIGNAL_PROCESS_GROUP,
                )
            };
            if result == 0 {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ESRCH) => Ok(true),
                Some(libc::EINVAL) | Some(libc::ENOSYS) => Ok(false),
                _ => Err(error),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = signal;
            Ok(false)
        }
    }

    /// Signal a captured listener identity, avoiding reused Linux PIDs and
    /// refusing an already-completed native exit watch on other platforms.
    pub(crate) fn signal(&self, signal: i32) -> std::io::Result<()> {
        if self.pid <= 1 {
            return Err(std::io::Error::other("invalid listener PID"));
        }
        if self.wait_exit(std::time::Duration::ZERO)? {
            return Ok(());
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: the pidfd pins one process identity; no siginfo is supplied.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.fd,
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        // SAFETY: the positive PID has a live native exit watch.
        let result = unsafe { libc::kill(self.pid, signal) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

impl Drop for ProcessWatch {
    fn drop(&mut self) {
        // SAFETY: fd is owned by this wrapper and closed exactly once.
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    #[test]
    fn executable_identity_matches_current_image() {
        let actual = super::executable_path(std::process::id() as i32).unwrap();
        assert_eq!(
            std::fs::canonicalize(actual).unwrap(),
            std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap()
        );
    }

    use super::*;
    use mio::{Events, Poll};
    use std::time::Duration;

    #[test]
    fn kernel_notifies_once_when_the_process_exits() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 0.05"])
            .spawn()
            .unwrap();
        let mut watch = ProcessWatch::new(child.id() as i32).unwrap();
        let mut poll = Poll::new().unwrap();
        watch.register(poll.registry(), Token(9)).unwrap();
        child.wait().unwrap();

        let mut events = Events::with_capacity(4);
        poll.poll(&mut events, Some(Duration::from_secs(1)))
            .unwrap();
        assert!(events.iter().any(|event| event.token() == Token(9)));
    }
}
