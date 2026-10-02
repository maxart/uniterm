//! Drive prefix actions through the real attach client, PTY, and wire protocol.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, Command, FrameDecoder, ServerMessage, SplitAxis};
use uniterm_server::Terminal;

mod common;

const TIMEOUT: Duration = Duration::from_secs(5);
const BACKGROUND: &str = "quick-actions fixture background";

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct DirectoryGuard(PathBuf);

impl Drop for DirectoryGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    child: ChildGuard,
    master: File,
    // Keep a slave open so macOS preserves unread output after child exit.
    _slave: File,
    stream: UnixStream,
    decoder: FrameDecoder,
    messages: VecDeque<ClientMessage>,
    screen: Terminal,
}

impl Fixture {
    fn start(prefix: u8) -> (Self, DirectoryGuard) {
        let state = common::isolate_state();
        let workspace = common::unique_workspace_name();
        let runtime = common::socket_root().join(format!("qa-{}", common::unique_nonce()));
        let directory = DirectoryGuard(runtime.clone());
        std::fs::create_dir_all(runtime.join("uniterm")).unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        if prefix != 1 {
            std::fs::write(runtime.join("uniterm/uniterm.conf"), "prefix = C-b\n").unwrap();
        }
        let listener =
            UnixListener::bind(uniterm_server::server::default_socket_path(&workspace)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut master = 0;
        let mut slave = 0;
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: all output pointers and the initial window size are live.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut size,
                )
            },
            0
        );
        // SAFETY: each new descriptor is transferred to exactly one owner.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        // SAFETY: fcntl only updates flags on the owned master descriptor.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            assert!(flags >= 0);
            assert_eq!(
                libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
                0
            );
        }
        let mut child = ChildGuard(
            ProcessCommand::new(env!("CARGO_BIN_EXE_ut"))
                .args(["attach", &workspace])
                .env("XDG_STATE_HOME", &state)
                .env("XDG_RUNTIME_DIR", &runtime)
                .env("XDG_CONFIG_HOME", &runtime)
                .env("TERM", "xterm-256color")
                .env_remove("UNITERM_SOCKET")
                .env_remove("UNITERM_PANE_ID")
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::from(slave.try_clone().unwrap()))
                .stderr(Stdio::from(slave.try_clone().unwrap()))
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + TIMEOUT;
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("accept: {error}"),
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "attach exited before connecting"
            );
            assert!(Instant::now() < deadline, "attach never connected");
            thread::sleep(Duration::from_millis(5));
        };
        stream.set_nonblocking(true).unwrap();
        let mut fixture = Self {
            child,
            master,
            _slave: slave,
            stream,
            decoder: FrameDecoder::new(),
            messages: VecDeque::new(),
            screen: Terminal::new(80, 24),
        };
        fixture.wait_screen(BACKGROUND);
        (fixture, directory)
    }

    // Serve only deterministic frames, never spawn a shell or a real Workspace.
    // Refresh must repaint the background so the client exercises recomposition.
    fn pump(&mut self) -> usize {
        assert!(
            self.child.0.try_wait().unwrap().is_none(),
            "attach unexpectedly exited"
        );
        let mut buffer = [0; 8192];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => panic!("attach disconnected"),
                Ok(n) => self.decoder.push(&buffer[..n]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("socket read: {error}"),
            }
        }
        while let Some(message) = self.decoder.decode::<ClientMessage>().unwrap() {
            match message {
                ClientMessage::Attach { cols, rows, .. } => {
                    assert_eq!((cols, rows), (80, 24));
                    self.render();
                }
                ClientMessage::Refresh => self.render(),
                // Visibility bookkeeping is not an action or Pane input.
                ClientMessage::OverlayVisible { .. } => {}
                other => self.messages.push_back(other),
            }
        }
        let mut bytes = 0;
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => panic!("PTY closed"),
                Ok(n) => {
                    self.screen.feed(&buffer[..n]);
                    bytes += n;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("PTY read: {error}"),
            }
        }
        bytes
    }

    fn render(&mut self) {
        self.stream
            .write_all(&encode_frame(&ServerMessage::RenderOps(
                format!("\x1b[0m\x1b[2J\x1b[H{BACKGROUND}").into_bytes(),
            )))
            .unwrap();
    }

    fn send(&mut self, bytes: &[u8]) {
        // One write deliberately covers prefix and shortcut in the same chunk.
        assert_eq!(self.master.write(bytes).unwrap(), bytes.len());
    }

    fn wait_screen(&mut self, text: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if self.screen.dump_text().contains(text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing {text:?}: {}",
                self.screen.dump_text()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn next_message(&mut self) -> ClientMessage {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if let Some(message) = self.messages.pop_front() {
                return message;
            }
            assert!(Instant::now() < deadline, "client message never arrived");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn expect_input(&mut self, expected: &[u8]) {
        let mut actual = Vec::new();
        while actual.len() < expected.len() {
            match self.next_message() {
                ClientMessage::Input(bytes) => actual.extend(bytes),
                other => panic!("expected Input, got {other:?}"),
            }
        }
        assert_eq!(actual, expected);
    }

    fn expect_command(&mut self, expected: Command) {
        let message = self.next_message();
        assert!(
            matches!(message, ClientMessage::Command(command) if command == expected),
            "expected {expected:?}, got {message:?}"
        );
    }

    fn drain(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        let mut quiet_since = Instant::now();
        loop {
            if self.pump() > 0 {
                quiet_since = Instant::now();
            }
            if quiet_since.elapsed() >= Duration::from_millis(100) {
                return;
            }
            assert!(Instant::now() < deadline, "output never settled");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_idle(&mut self) {
        self.drain();
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            assert_eq!(self.pump(), 0, "idle overlay emitted terminal bytes");
            assert!(
                self.messages.is_empty(),
                "unexpected messages: {:?}",
                self.messages
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_closed(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            let screen = self.screen.dump_text();
            if screen.contains(BACKGROUND) && !screen.contains("Quick actions") {
                break;
            }
            assert!(Instant::now() < deadline, "overlay did not close: {screen}");
            thread::sleep(Duration::from_millis(5));
        }
        self.drain();
        let screen = self.screen.dump_text();
        assert!(
            screen.contains(BACKGROUND),
            "background not restored: {screen}"
        );
        assert!(
            !screen.contains("Quick actions"),
            "overlay still open: {screen}"
        );
        assert!(
            self.messages.is_empty(),
            "unexpected messages: {:?}",
            self.messages
        );
    }
}

#[test]
fn quick_actions_prefix_search_shortcuts_and_idle_through_real_pty() {
    let mut open_latency = Vec::new();
    // One test owns the process environment; cover both the default and config.
    for prefix in [b'\x01', b'\x02'] {
        let (mut fixture, directory) = Fixture::start(prefix);
        fixture.send(b"plain first attach keys");
        fixture.expect_input(b"plain first attach keys");
        fixture.assert_closed();

        fixture.send(&[prefix]);
        fixture.wait_screen("Quick actions");
        fixture.assert_idle();
        fixture.send(b"c");
        fixture.expect_command(Command::NewWindow);
        fixture.assert_closed();

        fixture.send(&[prefix, b'c']);
        fixture.expect_command(Command::NewWindow);
        fixture.assert_closed();

        let mut batched = vec![prefix, b'c', prefix, b'/'];
        batched.extend_from_slice(b"splt r\rafter action");
        fixture.send(&batched);
        fixture.expect_command(Command::NewWindow);
        fixture.expect_command(Command::Split(SplitAxis::LeftRight));
        fixture.expect_input(b"after action");
        fixture.assert_closed();

        let mut literal_then_search = vec![prefix, prefix, prefix, b'/'];
        literal_then_search.extend_from_slice(b"splt r\r");
        fixture.send(&literal_then_search);
        fixture.expect_input(&[prefix]);
        fixture.expect_command(Command::Split(SplitAxis::LeftRight));
        fixture.assert_closed();

        for search in [b' ', b'/'] {
            fixture.send(&[prefix, search]);
            fixture.wait_screen("Quick actions");
            fixture.send(b"splt r");
            fixture.wait_screen("splt r");
            fixture.send(b"\r");
            fixture.expect_command(Command::Split(SplitAxis::LeftRight));
            fixture.assert_closed();
        }

        let mut same_write = vec![prefix, b'/'];
        same_write.extend_from_slice(b"splt r\r");
        fixture.send(&same_write);
        fixture.expect_command(Command::Split(SplitAxis::LeftRight));
        fixture.assert_closed();

        fixture.send(&[prefix, b'/', b'z', b'z', b'z', b'z']);
        fixture.wait_screen("No matching actions");
        fixture.send(b"\r");
        fixture.assert_idle();
        assert!(fixture.screen.dump_text().contains("No matching actions"));
        fixture.send(b"\x1b");
        fixture.assert_closed();
        fixture.send(b"after escape");
        fixture.expect_input(b"after escape");

        fixture.send(&[prefix, prefix]);
        fixture.expect_input(&[prefix]);
        fixture.assert_closed();
        fixture.send(&[prefix]);
        fixture.wait_screen("Quick actions");
        fixture.send(&[prefix]);
        fixture.expect_input(&[prefix]);
        fixture.assert_closed();
        for _ in 0..10 {
            let started = Instant::now();
            fixture.send(&[prefix]);
            fixture.wait_screen("Quick actions");
            open_latency.push(started.elapsed());
            fixture.send(b"\x1b");
            fixture.assert_closed();
        }
        // Reap the client before removing its isolated socket/config directory.
        drop(fixture);
        drop(directory);
    }
    open_latency.sort();
    eprintln!(
        "prefix-to-visible picker p95 over {} samples: {:?}",
        open_latency.len(),
        open_latency[open_latency.len() * 95 / 100 - 1]
    );
}
