// Uniterm's local PTY fork-safety patch. See ../UNITERM_PATCH.md.
use std::convert::TryFrom;
use std::io;
use std::os::fd::RawFd;

// Standard-library directory iteration and allocation belong in the parent.
// The soft limit covers descriptors another thread opens after enumeration.
// Existing descriptors above a previously lowered limit extend the bound too.
pub(super) fn capture_bound() -> io::Result<RawFd> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let directory = "/proc/self/fd";
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let directory = "/dev/fd";
    let mut bound = 3;
    for entry in std::fs::read_dir(directory)? {
        let name = entry?.file_name();
        if let Some(fd) = name.to_str().and_then(|name| name.parse::<RawFd>().ok()) {
            bound = bound.max(fd.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "PTY descriptor bound overflow")
            })?);
        }
    }
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if limit.rlim_cur == libc::RLIM_INFINITY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PTY launch requires a finite RLIMIT_NOFILE",
        ));
    }
    let soft = RawFd::try_from(limit.rlim_cur).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "PTY RLIMIT_NOFILE exceeds the descriptor range",
        )
    })?;
    Ok(bound.max(soft))
}

// Mark instead of close: Rust's private exec-error pipe must remain open until
// exec succeeds. No allocation, libc directory iteration, or locks may run here.
pub(super) fn mark_cloexec(bound: RawFd) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    loop {
        // Linux UAPI value, also used on Android where libc lacks the constant.
        // A raw syscall preserves old glibc and Android API compatibility.
        const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
        let result = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                CLOSE_RANGE_CLOEXEC,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            // Old kernels/flags or a syscall filter require the portable path.
            Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EPERM) => break,
            _ => return Err(error),
        }
    }
    mark_cloexec_fallback(bound)
}

pub(super) fn mark_cloexec_fallback(bound: RawFd) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    match mark_proc_fds() {
        Ok(()) => return Ok(()),
        Err(error) => match error.raw_os_error() {
            Some(libc::ENOENT) | Some(libc::ENOTDIR) | Some(libc::EACCES) | Some(libc::EPERM)
            | Some(libc::ENOSYS) | Some(libc::EMFILE) | Some(libc::ENFILE) => {}
            _ => return Err(error),
        },
    }
    mark_cloexec_range(bound)
}

pub(super) fn mark_cloexec_range(bound: RawFd) -> io::Result<()> {
    for fd in 3..bound {
        mark_fd(fd)?;
    }
    Ok(())
}

fn mark_fd(fd: RawFd) -> io::Result<()> {
    fn fcntl(fd: RawFd, command: libc::c_int, argument: libc::c_int) -> io::Result<libc::c_int> {
        loop {
            let result = unsafe { libc::fcntl(fd, command, argument) };
            if result != -1 {
                return Ok(result);
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error);
            }
        }
    }
    let result = fcntl(fd, libc::F_GETFD, 0).and_then(|flags| {
        if flags & libc::FD_CLOEXEC == 0 {
            fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC)?;
        }
        Ok(())
    });
    if let Err(error) = result {
        // Holes in the descriptor table, including parent-snapshot entries
        // closed before fork, are expected. Other errors fail the launch.
        if error.raw_os_error() != Some(libc::EBADF) {
            return Err(error);
        }
    }
    Ok(())
}

// Linux getdents64 ABI: u64 inode, i64 offset, u16 record length, u8 type,
// then a NUL-terminated filename. Decode bytes to avoid unaligned accesses.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn next_fd_entry(bytes: &[u8]) -> io::Result<(usize, Option<RawFd>)> {
    let malformed = || io::Error::from_raw_os_error(libc::EIO);
    if bytes.len() < 20 {
        return Err(malformed());
    }
    let length = u16::from_ne_bytes([bytes[16], bytes[17]]) as usize;
    if length < 20 || length > bytes.len() {
        return Err(malformed());
    }
    let name = &bytes[19..length];
    let end = name
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(malformed)?;
    let name = &name[..end];
    if name == b"." || name == b".." {
        return Ok((length, None));
    }
    if name.is_empty() {
        return Err(malformed());
    }
    let mut fd: RawFd = 0;
    for &digit in name {
        if !digit.is_ascii_digit() {
            return Err(malformed());
        }
        fd = fd
            .checked_mul(10)
            .and_then(|fd| fd.checked_add((digit - b'0') as RawFd))
            .ok_or_else(malformed)?;
    }
    Ok((length, Some(fd)))
}

// Older Linux/Android kernels lack close_range(CLOEXEC). Enumerate the child's
// actual table using raw syscalls and stack storage, never libc readdir or malloc.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[allow(clippy::manual_c_str_literals)] // Upstream crate uses Rust 2018 syntax.
fn mark_proc_fds() -> io::Result<()> {
    struct Directory(RawFd);
    impl Drop for Directory {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }
    let directory = loop {
        let fd = unsafe {
            libc::open(
                b"/proc/self/fd\0".as_ptr().cast(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd >= 0 {
            break Directory(fd);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    };
    let mut buffer = [0u8; 8192];
    loop {
        let count = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory.0,
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        if count == 0 {
            return Ok(());
        }
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        if count as usize > buffer.len() {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        let mut remaining = &buffer[..count as usize];
        while !remaining.is_empty() {
            let (length, fd) = next_fd_entry(remaining)?;
            if let Some(fd) = fd.filter(|fd| *fd > 2) {
                mark_fd(fd)?;
            }
            remaining = &remaining[length..];
        }
    }
}
