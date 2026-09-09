//! Terminal and socket plumbing for the attach client.
//!
//! Raw mode, the alternate screen, resize signals, stdout writes, and the
//! non-blocking server socket all live here so the attach loop above deals in
//! frames rather than in file descriptors. Nothing in this module wakes on a
//! timer; every entry point runs because real work arrived (`docs/04`).

use std::os::unix::io::RawFd;

use mio::net::UnixStream;
use mio::{Interest, Registry};
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, FrameError, ServerMessage};

use crate::SERVER;

/// Write-end of the self-pipe the SIGWINCH handler pokes. A signal handler may
/// only do async-signal-safe things, so it just writes one byte here; the mio
/// loop notices and queries the new size.
static WINCH_PIPE_W: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn on_winch(_sig: libc::c_int) {
    let fd = WINCH_PIPE_W.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        let b = [1u8];
        // SAFETY: write() is async-signal-safe; fd is a valid pipe write-end.
        unsafe {
            libc::write(fd, b.as_ptr() as *const libc::c_void, 1);
        }
    }
}

pub(crate) struct SignalPipe {
    pub(crate) read: RawFd,
    write: RawFd,
}

impl Drop for SignalPipe {
    fn drop(&mut self) {
        let _ = WINCH_PIPE_W.compare_exchange(
            self.write,
            -1,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        );
        // SAFETY: these are the two pipe descriptors this guard owns.
        unsafe {
            libc::close(self.read);
            libc::close(self.write);
        }
    }
}

/// Install the SIGWINCH handler and return an owned self-pipe.
pub(crate) fn install_winch() -> std::io::Result<SignalPipe> {
    let mut fds = [0i32; 2];
    // SAFETY: standard pipe creation, result checked.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (r, w) = (fds[0], fds[1]);
    for fd in [r, w] {
        // SAFETY: set non-blocking on our own pipe fds.
        unsafe {
            let fl = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
    WINCH_PIPE_W.store(w, std::sync::atomic::Ordering::Relaxed);
    // SAFETY: installing a handler with a zeroed sigaction + our fn pointer.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_winch as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut());
        // A resumed attach client cannot trust the physical terminal contents.
        // Reuse the same self-pipe; the resize branch sends an authoritative
        // repaint even when the dimensions did not change.
        libc::sigaction(libc::SIGCONT, &sa, std::ptr::null_mut());
    }
    Ok(SignalPipe { read: r, write: w })
}

/// Write a batch of render ops as one atomic frame, wrapped in DEC 2026
/// synchronized-output guards (terminals that support it render the whole
/// frame at once; others ignore the markers).
pub(crate) fn write_sync_frame(frame: &[u8]) {
    let mut out = Vec::with_capacity(frame.len() + 16);
    out.extend_from_slice(b"\x1b[?2026h");
    out.extend_from_slice(frame);
    out.extend_from_slice(b"\x1b[?2026l");
    write_stdout(&out);
}

/// Drain complete server messages after each socket read.
///
/// `FrameDecoder` bounds one incomplete frame. Letting several valid render
/// frames accumulate before decoding can exceed that bound during a large
/// mouse-triggered application redraw even though every frame is valid.
pub(crate) fn drain_server_messages(
    decoder: &mut FrameDecoder,
    messages: &mut Vec<ServerMessage>,
) -> Result<(), FrameError> {
    while let Some(message) = decoder.decode()? {
        messages.push(message);
    }
    Ok(())
}

/// Load the user's config (prefix, status-line position, ...) - the same file
/// the server reads, so both sides agree on chrome geometry.
pub(crate) fn load_client_config() -> uniterm_core::Config {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| uniterm_core::Config::parse(&t))
        .unwrap_or_default()
}

/// The config file path, matching the server's resolution.
pub(crate) fn config_path() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("uniterm").join("uniterm.conf"));
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("uniterm")
            .join("uniterm.conf"),
    )
}

pub(crate) fn flush_passthrough(passthrough: &mut Vec<u8>, out: &mut Vec<u8>) {
    if !passthrough.is_empty() {
        out.extend(encode_frame(&ClientMessage::Input(std::mem::take(
            passthrough,
        ))));
    }
}

/// Bound and sanitize a server-provided title before it reaches the host OSC
/// channel. The server applies the same policy, but the disposable client is
/// the final trust boundary and must not relay nested controls.
pub(crate) fn sanitize_terminal_title(title: &str) -> String {
    title
        .chars()
        .filter(|character| !character.is_control() && *character != '\u{7f}')
        .take(512)
        .collect()
}

pub(crate) fn flush(stream: &mut UnixStream, out: &mut Vec<u8>) -> std::io::Result<()> {
    use std::io::Write;
    while !out.is_empty() {
        match stream.write(out) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "uniterm server socket stopped accepting output",
                ));
            }
            Ok(n) => {
                out.drain(..n);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Attempt every newly queued frame immediately, then arm writable readiness
/// only for bytes the non-blocking socket could not accept. Waiting for a new
/// writable edge before the first attempt is unreliable across kqueue and SSH
/// proxy sockets, and used to leave typing and click follow-ups buffered until
/// another terminal event arrived.
pub(crate) fn service_server_output(
    reg: &Registry,
    stream: &mut UnixStream,
    out: &mut Vec<u8>,
    write_interest: &mut bool,
) -> std::io::Result<()> {
    flush(stream, out)?;
    let want_write = !out.is_empty();
    if want_write == *write_interest {
        return Ok(());
    }
    reregister_server(reg, stream, want_write)?;
    *write_interest = want_write;
    Ok(())
}

pub(crate) fn reregister_server(
    reg: &Registry,
    stream: &mut UnixStream,
    want_write: bool,
) -> std::io::Result<()> {
    let interest = if want_write {
        Interest::READABLE | Interest::WRITABLE
    } else {
        Interest::READABLE
    };
    reg.reregister(stream, SERVER, interest)
}

pub(crate) fn write_stdout(bytes: &[u8]) {
    let mut off = 0;
    while off < bytes.len() {
        // SAFETY: fd 1 is stdout; pointer/len from a live slice.
        let n = unsafe {
            libc::write(
                libc::STDOUT_FILENO,
                bytes[off..].as_ptr() as *const libc::c_void,
                bytes.len() - off,
            )
        };
        if n <= 0 {
            let e = std::io::Error::last_os_error();
            match e.kind() {
                std::io::ErrorKind::Interrupted => continue,
                // Stdout shares the tty's O_NONBLOCK with raw-mode stdin, and
                // the kernel pty buffer (~8KB) is smaller than a big overlay
                // frame. Dropping the remainder truncated renders mid-frame
                // (a half-drawn modal over a bare shadow slab); wait for the
                // terminal to drain instead - backpressure, not data loss.
                std::io::ErrorKind::WouldBlock => {
                    // SAFETY: polling our own stdout fd for writability.
                    unsafe {
                        let mut pfd = libc::pollfd {
                            fd: libc::STDOUT_FILENO,
                            events: libc::POLLOUT,
                            revents: 0,
                        };
                        libc::poll(&mut pfd, 1, -1);
                    }
                    continue;
                }
                _ => break,
            }
        }
        off += n as usize;
    }
}

pub(crate) fn tty_size(fd: RawFd) -> (u16, u16) {
    // SAFETY: winsize is POD; valid pointer, checked return.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

/// RAII guard that puts the tty into raw mode AND the terminal into a clean,
/// dedicated state for the session - then restores everything on drop, even on
/// panic. This mirrors tmux's `tty_start_tty`/`tty_stop_tty`: enter the
/// alternate screen, reset the scroll region and attributes, and show the
/// cursor on attach; exit the alternate screen (restoring the user's original
/// screen) on detach.
///
/// Doing this is what makes rendering reliable across terminals and shells:
/// without it we draw onto whatever state the terminal was left in (a leftover
/// scroll region, application modes, etc.), which can hide output until a full
/// reset happens - the "prompt is invisible until I press Ctrl-C" symptom.
pub(crate) struct TtyGuard {
    fd: RawFd,
    termios: libc::termios,
    flags: libc::c_int,
}

/// Sent on attach: enter alt screen, reset scroll region, reset attrs, disable
/// autowrap, show cursor, clear, home.
///
/// Autowrap OFF (`?7l`) is essential: we position every cell absolutely, so we
/// never rely on wrapping - but if it were on, painting the last cell of the
/// bottom row would make the terminal scroll the whole screen up, pushing the
/// status line and pane dividers off. This is what tmux does while it paints.
/// The tail enables button events, drag tracking, and SGR extended coordinates
/// (so columns past 223 work), while explicitly disabling any-motion tracking
/// inherited from a prior crashed client. Any-motion is enabled separately
/// only when focus-follows-mouse is configured, so ordinary pointer movement
/// does no work. Native text selection then needs Shift, as in tmux with
/// `mouse on`.
pub(crate) const TERM_SETUP: &[u8] =
    b"\x1b]777;uniterm-input;1\x07\x1b[?1049h\x1b[r\x1b[m\x1b[?7l\x1b[?25h\x1b[2J\x1b[H\x1b[?1000h\x1b[?1002h\x1b[?1003l\x1b[?1004h\x1b[?1006h";
pub(crate) const TERM_ENABLE_MOTION: &[u8] = b"\x1b[?1003h";
/// Sent on detach: disable mouse reporting, reset attrs, re-enable autowrap,
/// show cursor, leave alt screen (restores the user's pre-attach screen).
pub(crate) const TERM_RESTORE: &[u8] =
    b"\x1b]777;uniterm-input;0\x07\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[m\x1b[?7h\x1b[?25h\x1b[?1049l";

impl TtyGuard {
    pub(crate) fn enable(fd: RawFd, focus_follows_mouse: bool) -> std::io::Result<TtyGuard> {
        // SAFETY: termios is POD; all calls are checked.
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut termios) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let orig_termios = termios;
            let flags = libc::fcntl(fd, libc::F_GETFL);

            let mut raw = orig_termios;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);

            write_stdout(TERM_SETUP);
            if focus_follows_mouse {
                write_stdout(TERM_ENABLE_MOTION);
            }

            Ok(TtyGuard {
                fd,
                termios: orig_termios,
                flags,
            })
        }
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        // Restore the terminal state, then the termios/flags. Order matters:
        // leave the alternate screen while still in raw mode, then hand the
        // cooked terminal back to the shell.
        write_stdout(TERM_RESTORE);
        // SAFETY: restoring the saved termios and flags on the same fd.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.termios);
            libc::fcntl(self.fd, libc::F_SETFL, self.flags);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    use mio::Poll;

    #[test]
    fn tty_lifecycle_marks_nested_input_and_keeps_mouse_enabled() {
        assert!(TERM_SETUP.starts_with(b"\x1b]777;uniterm-input;1\x07"));
        assert!(TERM_SETUP.windows(8).any(|bytes| bytes == b"\x1b[?1000h"));
        assert!(TERM_SETUP.windows(8).any(|bytes| bytes == b"\x1b[?1002h"));
        assert!(TERM_SETUP.windows(8).any(|bytes| bytes == b"\x1b[?1006h"));
        assert!(TERM_RESTORE.starts_with(b"\x1b]777;uniterm-input;0\x07"));
    }

    #[test]
    fn default_tty_setup_disables_any_motion_reporting() {
        assert!(TERM_SETUP
            .windows(b"\x1b[?1002h".len())
            .any(|window| window == b"\x1b[?1002h"));
        assert!(TERM_SETUP
            .windows(b"\x1b[?1003l".len())
            .any(|window| window == b"\x1b[?1003l"));
        assert!(!TERM_SETUP
            .windows(TERM_ENABLE_MOTION.len())
            .any(|window| window == TERM_ENABLE_MOTION));
        assert_eq!(TERM_ENABLE_MOTION, b"\x1b[?1003h");
        assert!(TERM_RESTORE
            .windows(b"\x1b[?1002l".len())
            .any(|window| window == b"\x1b[?1002l"));
    }

    #[test]
    fn render_bursts_are_decoded_between_socket_reads() {
        let first = encode_frame(&ServerMessage::RenderOps(vec![b'a'; 64]));
        let second = encode_frame(&ServerMessage::RenderOps(vec![b'b'; 64]));
        let max_frame = u32::try_from(first.len() - 4).unwrap();
        assert!(first.len() + second.len() > max_frame as usize + 4);

        let mut decoder = FrameDecoder::with_max_frame(max_frame);
        let mut messages = Vec::new();
        for read in [&first[..], &second[..]] {
            decoder.push(read);
            drain_server_messages(&mut decoder, &mut messages).unwrap();
        }

        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[0],
            ServerMessage::RenderOps(ops) if ops == &vec![b'a'; 64]
        ));
        assert!(matches!(
            &messages[1],
            ServerMessage::RenderOps(ops) if ops == &vec![b'b'; 64]
        ));
    }

    #[test]
    fn newly_queued_input_is_written_without_waiting_for_another_event() {
        let (client, mut server) = std::os::unix::net::UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        let mut client = UnixStream::from_std(client);
        let poll = Poll::new().unwrap();

        let expected = encode_frame(&ClientMessage::Input(b"typed".to_vec()));
        let mut pending = expected.clone();
        let mut write_interest = false;
        service_server_output(
            poll.registry(),
            &mut client,
            &mut pending,
            &mut write_interest,
        )
        .unwrap();

        assert!(pending.is_empty());
        assert!(!write_interest);
        let mut received = vec![0; expected.len()];
        server.read_exact(&mut received).unwrap();
        assert_eq!(received, expected);
    }

    #[test]
    fn output_write_failure_is_reported_without_discarding_input() {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        let mut client = UnixStream::from_std(client);
        let poll = Poll::new().unwrap();
        poll.registry()
            .register(&mut client, SERVER, Interest::READABLE)
            .unwrap();
        drop(server);

        let expected = encode_frame(&ClientMessage::Input(b"typed".to_vec()));
        let mut pending = expected.clone();
        let mut write_interest = false;
        let error = service_server_output(
            poll.registry(),
            &mut client,
            &mut pending,
            &mut write_interest,
        )
        .unwrap_err();

        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
        ));
        assert_eq!(pending, expected);
    }
}
