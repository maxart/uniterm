//! Exercise Servers creation, editing, Local return and idle behavior through a real PTY.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
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
        common::isolate_state();
        let workspace = common::unique_workspace_name();
        let runtime = common::socket_root().join(format!("mu-{}", common::unique_nonce()));
        let directory = DirectoryGuard(runtime.clone());
        std::fs::create_dir_all(runtime.join("uniterm")).unwrap();
        let state = runtime.join("state");
        let listener =
            UnixListener::bind(runtime.join("uniterm").join(format!("{workspace}.sock"))).unwrap();
        // A regression must be observable without opening a real SSH connection.
        let ssh = runtime.join("ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$XDG_RUNTIME_DIR/ssh-called\"\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(runtime.clone()).chain(std::env::split_paths(&path)),
        )
        .unwrap();
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
            .env("PATH", path)
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
                ClientMessage::Refresh | ClientMessage::Resize { .. } => self.render(),
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

    fn click_text(&mut self, text: &str) {
        self.wait_screen(text);
        let grid = self.screen.grid();
        // All controls located by the fixture have one ASCII cell per glyph.
        assert!(text.is_ascii());
        for y in 0..grid.height() {
            for x in 0..=grid.width().saturating_sub(text.len() as u16) {
                if text
                    .chars()
                    .enumerate()
                    .all(|(offset, ch)| grid.get(x + offset as u16, y).ch == ch)
                {
                    let (col, row) = (x + 1, y + 1);
                    self.send(format!("\x1b[<0;{col};{row}M\x1b[<0;{col};{row}m").as_bytes());
                    return;
                }
            }
        }
        panic!("missing control {text}");
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        self.screen.resize(cols, rows);
        // SAFETY: this master descriptor and child belong to the fixture.
        unsafe {
            assert_eq!(
                libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size),
                0
            );
            assert_eq!(
                libc::kill(self.child.0.id() as libc::pid_t, libc::SIGWINCH),
                0
            );
        }
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
    let (mut fixture, directory) = Fixture::start();
    fixture
        .stream
        .write_all(&encode_frame(&ServerMessage::MachineNavigation {
            local: false,
        }))
        .unwrap();
    fixture.wait_screen("No saved Servers");
    fixture.assert_idle();
    fixture.click_text("a Add");
    fixture.wait_screen("Add Server");
    fixture.assert_idle();
    let wide = fixture.screen.dump_text();
    assert!(
        wide.contains("Server label") && wide.contains("SSH target") && wide.contains("Workspace")
    );
    assert!(!wide.contains("No saved Servers"), "{wide}");
    fixture.click_text("Enter Save");
    fixture.wait_screen("Enter a server label.");
    fixture.send(&[0xc3]);
    fixture.drain();
    fixture.send(&[0xa9]);
    fixture.wait_screen("é█");
    fixture.send(b"\x15Build\r");
    fixture.wait_screen("Enter an SSH target.");
    fixture.resize(36, 13);
    fixture.wait_screen("SSH target  ");
    fixture.assert_idle();
    let narrow = fixture.screen.dump_text();
    assert!(
        narrow.contains("Server label  Build") && narrow.contains("Workspace  default"),
        "{narrow}"
    );
    fixture.send(b"build\twork");
    fixture.wait_screen("work█");
    fixture.click_text("SSH target");
    fixture.wait_screen("build█");
    fixture.resize(80, 24);
    fixture.wait_screen("Enter a hostname, user@host, or an SSH config alias.");
    fixture.drain();
    fixture.click_text("Enter Save");
    fixture.wait_screen("Build  build / work");
    fixture.assert_idle();
    fixture.send(b"aBuild\tother\r");
    fixture.wait_screen("A server already uses this label.");
    fixture.wait_screen("Build█");
    fixture.click_text("Esc Cancel");
    fixture.wait_screen("Build  build / work");
    fixture.send(b"e");
    fixture.wait_screen("disabled");
    fixture.click_text("n Edit");
    fixture.wait_screen("Edit Server");
    fixture.send(b"\x15Discarded");
    fixture.wait_screen("Discarded█");
    fixture.click_text("Esc Cancel");
    fixture.wait_screen("Build  build / work");
    fixture.drain();
    assert!(!fixture.screen.dump_text().contains("Discarded"));
    fixture.send(b"n\x15Renamed\r");
    fixture.wait_screen("Renamed  build / work");
    fixture.assert_idle();
    let profiles =
        uniterm_client::machines::list_at(&directory.0.join("state/uniterm/machines.jsonl"))
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

#[test]
fn refresh_and_connect_detect_a_changed_or_removed_profile_before_starting_ssh() {
    use uniterm_client::machines::update_at;
    use uniterm_core::machine::{MachineEvent, MachineProfile};

    let (mut fixture, directory) = Fixture::start();
    let path = directory.0.join("state/uniterm/machines.jsonl");
    for label in ["Alpha", "Beta", "Gamma"] {
        update_at(&path, |_, id| {
            Ok(MachineEvent::Saved(MachineProfile {
                id,
                label: label.into(),
                target: label.to_lowercase(),
                workspace: "default".into(),
                enabled: true,
            }))
        })
        .unwrap();
    }
    fixture
        .stream
        .write_all(&encode_frame(&ServerMessage::MachineNavigation {
            local: false,
        }))
        .unwrap();
    fixture.wait_screen("> Alpha");
    fixture.send(b"\x1b[B");
    fixture.wait_screen("> Beta");

    // Another writer moves Beta from row 1 to row 0 and changes its target.
    update_at(&path, |_, _| Ok(MachineEvent::Removed(1))).unwrap();
    update_at(&path, |profiles, _| {
        let mut beta = profiles.iter().find(|p| p.id == 2).unwrap().clone();
        beta.target = "moved".into();
        Ok(MachineEvent::Saved(beta))
    })
    .unwrap();
    fixture.send(b"r");
    fixture.wait_screen("> Beta  moved");
    fixture.wait_screen("selected server changed");
    fixture.assert_idle();
    assert!(!directory.0.join("ssh-called").exists());

    // The next row takes Beta's place after a second writer forgets it.
    // The first Enter only refreshes the selection for the user to review.
    update_at(&path, |_, _| Ok(MachineEvent::Removed(2))).unwrap();
    fixture.send(b"\r");
    fixture.wait_screen("> Gamma");
    fixture.wait_screen("selected server changed");
    fixture.assert_idle();
    assert!(!directory.0.join("ssh-called").exists());
}
