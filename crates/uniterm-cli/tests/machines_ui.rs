//! Exercise Servers creation, editing, Local return and idle behavior through a real PTY.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Terminal;

mod common;

const TIMEOUT: Duration = Duration::from_secs(5);
const BACKGROUND: &str = "servers fixture background";

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
    fn start() -> (Self, DirectoryGuard) {
        let state = common::isolate_state();
        let workspace = common::unique_workspace_name();
        let runtime = common::socket_root().join(format!("mu-{}", common::unique_nonce()));
        let directory = DirectoryGuard(runtime.clone());
        std::fs::create_dir_all(runtime.join("uniterm")).unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
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
            ProcessCommand::new(
                std::env::var_os("UNITERM_TEST_ATTACH_BINARY")
                    .unwrap_or_else(|| env!("CARGO_BIN_EXE_ut").into()),
            )
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
                ClientMessage::WorkspaceState => {
                    self.stream
                        .write_all(&encode_frame(&ServerMessage::Workspace {
                            name: "fixture".into(),
                            active_project: uniterm_core::ProjectId(1),
                            projects: Vec::new(),
                        }))
                        .unwrap();
                }
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
}

#[test]
fn servers_native_pty_add_edit_disable_local_and_idle() {
    let (mut fixture, _directory) = Fixture::start();
    fixture
        .stream
        .write_all(&encode_frame(&ServerMessage::MachineNavigation {
            local: false,
        }))
        .unwrap();
    fixture.wait_screen("No saved Servers");
    fixture.assert_idle();
    fixture.send(b"aBuild\tbuild\twork\r");
    fixture.wait_screen("Build  build / work");
    fixture.assert_idle();
    fixture.send(b"e");
    fixture.wait_screen("disabled");
    fixture.send(b"n\x15Renamed\r");
    fixture.wait_screen("Renamed  build / work");
    fixture.assert_idle();
    let profiles =
        uniterm_client::machines::list_at(&uniterm_client::machines::catalog_path().unwrap())
            .unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].label, "Renamed");
    assert!(
        !profiles[0].enabled,
        "editing must preserve the explicit disabled state"
    );
    fixture.send(b"l");
    fixture.wait_screen(BACKGROUND);
    fixture.assert_idle();
    assert!(!fixture.screen.dump_text().contains("Renamed"));
    fixture
        .stream
        .write_all(&encode_frame(&ServerMessage::MachineNavigation {
            local: false,
        }))
        .unwrap();
    fixture.wait_screen("Renamed");
    fixture.send(b"dy");
    fixture.wait_screen("No saved Servers");
    fixture.assert_idle();
}
