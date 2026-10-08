//! Exercise Settings theme search, preview, and apply through a real attach PTY.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use uniterm_core::ThemePreset;
use uniterm_proto::{
    encode_frame, ClientMessage, FrameDecoder, ServerMessage, SettingsPatch, SettingsSnapshot,
};
use uniterm_server::Terminal;

mod common;

const TIMEOUT: Duration = Duration::from_secs(5);
const BACKGROUND: &str = "theme-picker fixture background";

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
        let runtime = common::socket_root().join(format!("tp-{}", common::unique_nonce()));
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
                ClientMessage::WorkspaceState => {
                    self.stream
                        .write_all(&encode_frame(&ServerMessage::Workspace {
                            name: "fixture".into(),
                            active_project: uniterm_core::ProjectId(1),
                            projects: Vec::new(),
                        }))
                        .unwrap();
                }
                ClientMessage::Settings => self.settings(false, "uniterm-dark"),
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

    fn settings(&mut self, saved: bool, theme: &str) {
        let mut settings = snapshot();
        settings.theme = theme.into();
        self.stream
            .write_all(&encode_frame(&ServerMessage::Settings {
                settings: Box::new(settings),
                saved,
                error: None,
            }))
            .unwrap();
    }

    fn open_picker(&mut self) {
        self.send(b"\r");
        self.wait_screen("Choose theme");
        self.assert_idle();
    }

    fn expect_apply(&mut self, theme: &str) {
        match self.next_message() {
            ClientMessage::SettingsApply(patch) => assert_eq!(
                patch,
                SettingsPatch {
                    theme: Some(theme.into()),
                    ..SettingsPatch::default()
                }
            ),
            other => panic!("expected theme-only SettingsApply, got {other:?}"),
        }
        self.settings(true, theme);
        self.wait_settings();
        self.assert_idle();
    }

    fn wait_settings(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            let screen = self.screen.dump_text();
            if screen.contains("Settings") && !screen.contains("Choose theme") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Settings did not return: {screen}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    // Read the visible range and coordinates from the actual terminal. This
    // catches mismatches between scrolling, painting, and mouse hit testing.
    fn visible_range(&self) -> (usize, usize, usize, u16, u16) {
        let screen = self.screen.dump_text();
        let (row, line) = screen
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(" matches"))
            .expect("visible match counter");
        let prefix = line.split(" matches").next().unwrap();
        let numbers: Vec<usize> = prefix
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(numbers.len(), 3, "bad range: {line}");
        let x = line.chars().position(|c| c.is_ascii_digit()).unwrap() as u16 + 1;
        (numbers[0], numbers[1], numbers[2], x, row as u16 + 2)
    }

    fn mouse(&mut self, button: u8, x: u16, y: u16) {
        self.send(format!("\x1b[<{button};{x};{y}M").as_bytes());
        if button == 0 {
            self.send(format!("\x1b[<0;{x};{y}m").as_bytes());
        }
        self.assert_idle();
    }
}

fn snapshot() -> SettingsSnapshot {
    SettingsSnapshot {
        theme: "uniterm-dark".into(),
        themes: ThemePreset::ALL
            .iter()
            .map(|preset| preset.name().into())
            .collect(),
        status: true,
        status_top: false,
        sidebar: true,
        sidebar_width: 24,
        file_sidebar: false,
        file_sidebar_width: 32,
        notification_delivery: "uniterm".into(),
        notification_deliveries: vec![
            "off".into(),
            "uniterm".into(),
            "terminal".into(),
            "system".into(),
        ],
        notification_sound: "bell".into(),
        notification_sounds: vec!["off".into(), "bell".into(), "chime".into(), "file".into()],
        notification_sound_file: String::new(),
        notify_completion: false,
        focus_follows_mouse: false,
        freeze_on_select: false,
        copy_on_select: true,
        confirm_close: true,
        confirm_tab_close: true,
        scrollback_limit: 10_000,
        restore: true,
        guardrail_max_active_runs: 8,
        guardrail_max_role_panes: 16,
        guardrail_max_iterations: 3,
        guardrail_max_elapsed_minutes: 120,
        guardrail_allowed_projects: "api; /work/web".into(),
        editor: "vi".into(),
        editor_rules: "md=glow".into(),
    }
}

#[test]
fn settings_theme_picker_search_scroll_preview_and_apply_through_real_pty() {
    // One test owns the process environment and exercises several independent
    // opens against an isolated socket, without creating a real Workspace.
    let (mut fixture, directory) = Fixture::start();
    fixture.send(b"\x01g");
    fixture.wait_settings();
    fixture.assert_idle();
    let settings_screen = fixture.screen.dump_text();
    assert!(settings_screen.contains("Current: uniterm-dark"));
    assert!(settings_screen.contains("$ uniterm"));
    assert!(settings_screen.contains("Ready"));
    fixture.open_picker();
    assert_eq!(ThemePreset::ALL.len(), 190);
    assert_eq!(fixture.visible_range().2, ThemePreset::ALL.len());
    let initial_screen = fixture.screen.dump_text();

    fixture.send(b"minimal");
    fixture.wait_screen("Minimal");
    fixture.assert_idle();
    assert_eq!(fixture.visible_range().2, 1);
    let filtered = fixture.screen.dump_text();
    // Opt-in visual artifact from the real client's parsed PTY output.
    if let Some(path) = std::env::var_os("UNITERM_THEME_PICKER_SCREEN_DUMP") {
        std::fs::write(
            path,
            format!("Settings theme preview (80x24)\n{settings_screen}\n\nInitial picker (80x24)\n{initial_screen}\n\nFiltered: minimal (80x24)\n{filtered}\n"),
        )
        .unwrap();
    }
    let (_, _, _, x, y) = fixture.visible_range();
    for button in [65, 64, 65] {
        fixture.mouse(button, x, y);
        // A wheel event must neither insert j/k in search nor apply a theme.
        assert_eq!(fixture.screen.dump_text(), filtered);
    }
    fixture.mouse(0, x, y);
    assert!(fixture.screen.dump_text().contains("Preview: Minimal"));
    fixture.send(b"\r");
    let minimal = ThemePreset::ALL
        .iter()
        .find(|preset| preset.name().ends_with("-minimal"))
        .unwrap()
        .name();
    fixture.expect_apply(minimal);

    fixture.open_picker();
    fixture.send(b"\x1b[H");
    fixture.assert_idle();
    let home = fixture.screen.dump_text();
    let (_, _, _, x, y) = fixture.visible_range();
    fixture.mouse(65, x, y);
    assert_ne!(
        fixture.screen.dump_text(),
        home,
        "wheel did not move the preview"
    );
    assert_eq!(fixture.visible_range().2, 190);
    fixture.mouse(64, x, y);
    assert_eq!(fixture.screen.dump_text(), home);
    let (first, mut end, count, _, _) = fixture.visible_range();
    assert_eq!(first, 1);
    assert_eq!(count, 190);
    // Every page must advance without skipping any catalog entries, all the
    // way to the final theme. No navigation may emit Pane input or an apply.
    while end < count {
        fixture.send(b"\x1b[6~");
        fixture.assert_idle();
        let (next_first, next_end, total, _, _) = fixture.visible_range();
        assert_eq!(total, count);
        assert!(next_first <= end + 1, "PageDown skipped themes");
        assert!(next_end > end, "PageDown failed to advance");
        end = next_end;
    }
    fixture.send(b"\x1b[H");
    fixture.assert_idle();
    assert_eq!(fixture.visible_range().0, 1);
    fixture.send(b"\x1b[F");
    fixture.assert_idle();
    let (first, end, total, x, first_y) = fixture.visible_range();
    assert_eq!((end, total), (190, 190));
    assert!(first > 1);
    let last_y = first_y + (end - first) as u16;
    // Select the penultimate rendered row and apply it: the wire must report
    // that row, not the previously selected End item or an off-by-one neighbor.
    fixture.mouse(0, x, last_y - 1);
    fixture.send(b"\r");
    fixture.expect_apply(ThemePreset::ALL[end - 2].name());

    fixture.open_picker();
    fixture.send(b"\x1b[F");
    fixture.assert_idle();
    let (first, end, _, x, first_y) = fixture.visible_range();
    assert_eq!(end, 190);
    // Move off End first, so an ignored last-row click cannot pass by keeping
    // the keyboard selection. Both clicks must remain preview-only.
    fixture.mouse(0, x, first_y);
    let before_last_click = fixture.screen.dump_text();
    fixture.mouse(0, x, first_y + (end - first) as u16);
    assert_ne!(fixture.screen.dump_text(), before_last_click);
    fixture.send(b"\r");
    fixture.expect_apply(ThemePreset::ALL.last().unwrap().name());

    fixture.open_picker();
    fixture.send(b"zzzz-no-such-theme");
    fixture.wait_screen("No matching themes");
    fixture.assert_idle();
    assert_eq!(fixture.visible_range().2, 0);
    fixture.send(b"\r");
    fixture.assert_idle();
    assert!(fixture.screen.dump_text().contains("No matching themes"));
    fixture.send(b"\x15");
    fixture.wait_screen("Search themes...");
    fixture.assert_idle();
    assert_eq!(fixture.visible_range().2, 190);
    fixture.send(b"minimal");
    fixture.wait_screen("Preview: Minimal");
    fixture.send(b"\x1b");
    fixture.wait_settings();
    fixture.assert_idle();
    fixture.open_picker();
    // Cancel discarded the query and retained the last server-applied theme.
    assert_eq!(fixture.visible_range().2, 190);
    assert!(fixture.screen.dump_text().contains("Current theme"));
    fixture.send(b"\x1b");
    fixture.wait_settings();
    fixture.assert_idle();
    drop(fixture);
    drop(directory);
}
