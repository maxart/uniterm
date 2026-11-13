// Uniterm's local PTY allocation patch. See ../UNITERM_PATCH.md.
//
// glibc and musl `openpty` create both descriptors without close-on-exec, and
// portable-pty marks them only afterwards. A child that another thread forks
// and execs in between keeps the slave open for its whole life, so the master
// never reports EOF after the pane's own process exits. Linux can create both
// descriptors close-on-exec, which leaves no such window.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

const FLAGS: libc::c_int = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;

/// Allocate a master/slave pair that is close-on-exec from creation, then
/// apply the initial window size. Otherwise this follows glibc `openpty`.
pub(super) fn open_cloexec_pair(size: &libc::winsize) -> io::Result<(OwnedFd, OwnedFd)> {
    open_pair_with(size, open_peer)
}

// The peer step is a parameter only so tests can fail it after the master
// exists and check that the master is released.
pub(super) fn open_pair_with(
    size: &libc::winsize,
    peer: impl FnOnce(&OwnedFd) -> io::Result<OwnedFd>,
) -> io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY: posix_openpt takes only flags and returns a new descriptor or -1.
    let master = owned(unsafe { libc::posix_openpt(FLAGS) })?;
    // SAFETY: grantpt and unlockpt only operate on the live master owned here.
    if unsafe { libc::grantpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::unlockpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = peer(&master)?;
    // glibc and musl `openpty` both ignore this result, and a fresh slave is
    // always a TTY. Keep that behaviour so allocation semantics are unchanged.
    // SAFETY: TIOCSWINSZ reads one winsize through a valid reference, on a
    // live descriptor owned here.
    unsafe {
        libc::ioctl(
            slave.as_raw_fd(),
            libc::TIOCSWINSZ,
            size as *const libc::winsize,
        )
    };
    Ok((master, slave))
}

// Linux 4.13 and later open the slave from the master itself. Like glibc, any
// failure falls back to the devpts path, so older kernels keep working.
fn open_peer(master: &OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: TIOCGPTPEER takes open flags by value on the live master and
    // returns a new descriptor or -1.
    owned(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGPTPEER, FLAGS) })
        .or_else(|_| open_peer_path(master))
}

pub(super) fn open_peer_path(master: &OwnedFd) -> io::Result<OwnedFd> {
    // devpts names are short; ptsname_r reports ERANGE if one does not fit.
    let mut path: [libc::c_char; 512] = [0; 512];
    // SAFETY: the buffer is writable for its full length and the master is live.
    let error = unsafe { libc::ptsname_r(master.as_raw_fd(), path.as_mut_ptr(), path.len()) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    // SAFETY: ptsname_r succeeded, so the buffer holds a NUL-terminated path.
    owned(unsafe { libc::open(path.as_ptr(), FLAGS) })
}

fn owned(fd: RawFd) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a non-negative result is a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
