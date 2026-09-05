//! Native one-shot foreground-process exit notification. Linux uses pidfd;
//! macOS uses kqueue EVFILT_PROC/NOTE_EXIT. Each watch is registered in the
//! existing mio poll set, so exit creates one kernel event and no scan/timer.

use std::os::fd::RawFd;

use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};

pub struct ProcessWatch {
    fd: RawFd,
    pub pid: i32,
}

impl ProcessWatch {
    #[cfg(target_os = "linux")]
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

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
