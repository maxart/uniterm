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

/// Read argv directly from the macOS kernel on an event-triggered runtime worker.
/// Spawning `ps` here can wait for pipe EOF from a concurrently launched child.
#[cfg(target_os = "macos")]
pub(crate) fn command_line(pid: i32) -> Option<String> {
    use std::sync::OnceLock;
    if pid <= 0 {
        return None;
    }
    static ARGUMENT_CAPACITY: OnceLock<Option<usize>> = OnceLock::new();
    let capacity = (*ARGUMENT_CAPACITY.get_or_init(|| {
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let mut maximum: libc::c_int = 0;
        let mut length = std::mem::size_of_val(&maximum);
        // SAFETY: both arrays are initialized, lengths match the writable
        // buffers, and sysctl retains no pointers. This is a read-only query.
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as _,
                (&mut maximum as *mut libc::c_int).cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        (result == 0
            && length == std::mem::size_of_val(&maximum)
            && maximum > 0
            && maximum <= 1024 * 1024)
            .then_some(maximum as usize)
    }))?;
    let mut bytes = vec![0u8; capacity];
    let mut length = bytes.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: the kernel receives a writable buffer of exactly `length`
    // bytes and a valid three-integer MIB; no pointers outlive this call.
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as _,
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || length > bytes.len() {
        #[cfg(test)]
        eprintln!(
            "native argv pid={pid}: result={result} length={length} error={}",
            std::io::Error::last_os_error()
        );
        return None;
    }
    let command = parse_command_line(&bytes[..length]);
    #[cfg(test)]
    if command.is_none() {
        eprintln!(
            "native argv pid={pid}: invalid layout length={length} argc={:?}",
            bytes.get(..4)
        );
    }
    command
}

// KERN_PROCARGS2: native argc, saved executable path, NUL padding, then
// exactly argc NUL-terminated arguments. Anything after argv is environment.
#[cfg(any(target_os = "macos", test))]
fn parse_command_line(bytes: &[u8]) -> Option<String> {
    let count = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
    if count <= 0 {
        return None;
    }
    let mut rest = &bytes[4..];
    rest = &rest[rest.iter().position(|byte| *byte == 0)? + 1..];
    rest = &rest[rest.iter().position(|byte| *byte != 0)?..];
    if count as usize > rest.len() {
        return None;
    }
    let mut command = String::new();
    for index in 0..count {
        let end = rest.iter().position(|byte| *byte == 0)?;
        if index != 0 {
            command.push(' ');
        }
        command.push_str(&String::from_utf8_lossy(&rest[..end]));
        rest = &rest[end + 1..];
    }
    let command = command.trim();
    (!command.is_empty()).then(|| command.to_owned())
}

#[cfg(test)]
mod command_tests {
    use super::parse_command_line;

    fn buffer(count: i32, strings: &[u8]) -> Vec<u8> {
        let mut bytes = count.to_ne_bytes().to_vec();
        bytes.extend_from_slice(strings);
        bytes
    }

    #[test]
    fn native_command_arguments_preserve_empty_values_and_exclude_environment() {
        let bytes = buffer(
            5,
            b"/saved/path\0\0\0/bin/tool\0two words\0\0K=V\0\xff\0SECRET=agent\0",
        );
        assert_eq!(
            parse_command_line(&bytes).as_deref(),
            Some("/bin/tool two words  K=V \u{fffd}")
        );
        assert_eq!(
            parse_command_line(&buffer(1, b"/saved\0tool\0AGENT=codex\0")).as_deref(),
            Some("tool")
        );
    }

    #[test]
    fn native_command_arguments_reject_incomplete_or_invalid_buffers() {
        for bytes in [
            vec![],
            vec![1, 0, 0],
            buffer(0, b"/saved\0tool\0"),
            buffer(-1, b"/saved\0tool\0"),
            buffer(i32::MAX, b"/saved\0tool\0"),
            buffer(1, b"unterminated"),
            buffer(1, b"/saved\0\0"),
            buffer(1, b"/saved\0tool"),
            buffer(2, b"/saved\0tool\0missing"),
        ] {
            assert_eq!(parse_command_line(&bytes), None, "{bytes:?}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_command_query_reads_live_child_arguments_without_environment() {
        use std::process::{Command, Stdio};
        crate::pty::prepare_process_spawn();
        let mut child = Command::new("/bin/sh")
            .args([
                "-c",
                "kill -STOP $$",
                "uniterm argv probe",
                "two words",
                "",
                "K=V",
            ])
            .env("UNITERM_ARGV_SECRET", "never include environment")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        // Measure only after the child explicitly signals readiness by stopping.
        let mut status = 0;
        let stopped = loop {
            // SAFETY: pid is this test's owned child and status is writable.
            let result = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
            if result != -1
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break result;
            }
        };
        let ready = stopped == pid && libc::WIFSTOPPED(status);
        let mut timings = Vec::new();
        let mut results = Vec::new();
        for _ in 0..200 {
            let start = std::time::Instant::now();
            results.push(super::command_line(pid));
            timings.push(start.elapsed());
        }
        // Reap before assertions, including on a failed query.
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            ready,
            "child did not reach its ready stop: {stopped} {status}"
        );
        for command in results {
            assert_eq!(
                command.as_deref(),
                Some("/bin/sh -c kill -STOP $$ uniterm argv probe two words  K=V")
            );
        }
        assert!(super::command_line(pid).is_none());
        assert!(super::command_line(0).is_none());
        assert!(super::command_line(-1).is_none());
        timings.sort_unstable();
        eprintln!("Native argv query n=200 p95={:?}", timings[189]);
        assert!(timings[189] < std::time::Duration::from_millis(50));
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
