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

use uniterm_core::{grid::Color, Theme, ThemePreset};
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
    last_input: Vec<u8>,
    initial_settings: SettingsSnapshot,
    refreshes: usize,
}

impl Fixture {
    fn start(initial_settings: SettingsSnapshot) -> (Self, DirectoryGuard) {
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
            last_input: Vec::new(),
            initial_settings,
            refreshes: 0,
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
                    self.send_settings(self.initial_settings.clone(), false);
                    self.render();
                }
                ClientMessage::Refresh => {
                    self.refreshes += 1;
                    self.render();
                }
                ClientMessage::WorkspaceState => {
                    self.stream
                        .write_all(&encode_frame(&ServerMessage::Workspace {
                            name: "fixture".into(),
                            active_project: uniterm_core::ProjectId(1),
                            projects: Vec::new(),
                        }))
                        .unwrap();
                }
                ClientMessage::Settings => self.send_settings(self.initial_settings.clone(), false),
                ClientMessage::Agents => self.agents(uniterm_proto::ConnectorStatus::NotInstalled),
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
        self.last_input = bytes.to_vec();
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

    #[track_caller]
    fn next_message(&mut self) -> ClientMessage {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if let Some(message) = self.messages.pop_front() {
                return message;
            }
            assert!(
                Instant::now() < deadline,
                "client message never arrived after input {:?}; queued messages: {:?}; screen:\n{}",
                self.last_input,
                self.messages,
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
        self.assert_quiet();
    }

    // Unlike assert_idle, inspect the very first byte after an event rather
    // than draining it. Duplicate palettes must not cause even one repaint.
    fn assert_quiet(&mut self) {
        let refreshes = self.refreshes;
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
        assert_eq!(
            self.refreshes, refreshes,
            "event requested a server Refresh"
        );
    }

    fn settings(&mut self, saved: bool, theme: &str) {
        let mut settings = snapshot();
        settings.theme = theme.into();
        settings.effective_theme = Theme::named(theme);
        self.send_settings(settings, saved);
    }

    fn send_settings(&mut self, settings: SettingsSnapshot, saved: bool) {
        self.stream
            .write_all(&encode_frame(&ServerMessage::Settings {
                settings: Box::new(settings),
                saved,
                error: None,
            }))
            .unwrap();
    }

    fn theme_changed(&mut self, theme: Theme) {
        self.stream
            .write_all(&encode_frame(&ServerMessage::ThemeChanged { theme }))
            .unwrap();
    }

    fn assert_surface(&self, theme: Theme) {
        assert!(
            (0..24).any(|y| (0..80).any(|x| {
                let cell = self.screen.grid().get(x, y);
                cell.bg == theme.surface && cell.fg == theme.foreground
            })),
            "host palette was not rendered: {:?}",
            theme
        );
    }

    fn live_host_theme(&mut self, host: SettingsSnapshot) {
        // The unsolicited attach snapshot must theme a modal before Settings
        // is ever opened, including clients attached to a remote host.
        self.send(b"\x01,");
        self.wait_screen("Rename tab");
        self.assert_idle();
        self.assert_surface(host.effective_theme);
        self.send(b"draft\x1b[D");
        self.wait_screen("draf█t");
        self.drain();
        let before = self.screen.dump_text();
        let refreshes = self.refreshes;
        let mut changed = host.effective_theme;
        changed.surface = Color::Rgb(43, 59, 71);
        changed.foreground = Color::Rgb(193, 209, 221);
        self.theme_changed(changed);
        self.drain();
        self.assert_surface(changed);
        assert_eq!(self.screen.dump_text(), before);
        assert_eq!(self.refreshes, refreshes);
        self.theme_changed(changed);
        self.assert_quiet();
        self.send(b"X\r");
        assert!(matches!(
            self.next_message(),
            ClientMessage::RenameWindow { name } if name == "drafXt"
        ));
        self.wait_screen(BACKGROUND);
        self.assert_idle();

        // With no overlay, a distinct palette updates client state without
        // emitting terminal output or asking the host for a full frame.
        self.theme_changed(host.effective_theme);
        self.assert_quiet();
        self.send(b"\x01g");
        self.wait_settings();
        self.assert_idle();
        self.assert_surface(host.effective_theme);
        assert!(self.screen.dump_text().contains("Follow Omarchy theme"));
        let screen = self.screen.dump_text();
        assert!(screen.find("Ready").unwrap() < screen.find("Follow Omarchy theme").unwrap());
        assert!(screen.lines().all(|line| !line
            .split('│')
            .nth(1)
            .is_some_and(|rail| rail.contains("Follow Omarchy"))));
        if let Some(path) = std::env::var_os("UNITERM_THEME_PICKER_SCREEN_DUMP") {
            std::fs::write(
                std::path::PathBuf::from(path).with_extension("omarchy.txt"),
                &screen,
            )
            .unwrap();
        }
        self.send(b"f");
        assert!(
            matches!(self.next_message(), ClientMessage::SettingsApply(patch)
            if patch == SettingsPatch { follow_omarchy_theme: Some(false), ..Default::default() })
        );
        let mut off = host.clone();
        off.follow_omarchy_theme = false;
        self.send_settings(off, true);
        self.drain();
        let screen = self.screen.dump_text();
        let (row, line) = screen
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("Follow Omarchy theme"))
            .unwrap();
        let column = line.chars().position(|c| c == '○').unwrap();
        let (x, y) = (column + 1, row + 1);
        self.send(format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m").as_bytes());
        assert!(
            matches!(self.next_message(), ClientMessage::SettingsApply(patch)
            if patch == SettingsPatch { follow_omarchy_theme: Some(true), ..Default::default() })
        );
        self.send_settings(host.clone(), true);
        self.drain();
        self.assert_idle();

        // Navigate to the editor through the real rail, leaving an unsaved
        // edit with the cursor inside its text when the OS theme changes.
        self.send(&[b'j'; 26]);
        self.wait_screen("Catch-all command");
        self.send(b"\r");
        self.drain();
        self.send(b"-draft\x1b[D");
        self.wait_screen("vi-draf█t");
        self.drain();
        let before = self.screen.dump_text();
        let refreshes = self.refreshes;
        self.theme_changed(changed);
        self.drain();
        self.assert_surface(changed);
        assert_eq!(self.screen.dump_text(), before);
        assert_eq!(self.refreshes, refreshes);
        self.theme_changed(changed);
        self.assert_quiet();
        self.send(b"X\r");
        assert!(matches!(
            self.next_message(),
            ClientMessage::SettingsApply(patch)
                if patch == SettingsPatch {
                    editor: Some("vi-drafXt".into()),
                    ..Default::default()
                }
        ));
        self.send(b"\x1b");
        self.wait_screen(BACKGROUND);
        self.assert_idle();

        // The remaining existing keyboard/pointer tests run against an
        // ineligible host, preserving their original rail geometry exactly.
        self.initial_settings = snapshot();
        self.settings(false, "uniterm-dark");
        self.assert_idle();
    }

    fn agents(&mut self, connector: uniterm_proto::ConnectorStatus) {
        self.stream
            .write_all(&encode_frame(&ServerMessage::Agents {
                items: uniterm_core::agent::PROVIDERS
                    .iter()
                    .map(|provider| uniterm_proto::AgentInfo {
                        id: provider.id.into(),
                        name: provider.name.into(),
                        command: provider.id.into(),
                        installed: true,
                        connector,
                        running: 0,
                    })
                    .collect(),
                error: None,
                auto_approve: false,
                auto_update_connectors: false,
                auto_install_connectors: false,
            }))
            .unwrap();
    }

    fn approval(&mut self, enabled: bool) {
        self.stream
            .write_all(&encode_frame(&ServerMessage::AutoApproval {
                enabled,
                error: None,
            }))
            .unwrap();
        self.drain();
    }

    fn connector_setting(&mut self, install: bool, settings: bool) {
        let steps = 1 + usize::from(install) + 2 * usize::from(settings);
        self.send(&vec![b'j'; steps]);
        self.wait_screen(if install {
            "auto-install-connectors = true"
        } else {
            "auto-update-connectors = true"
        });
        self.assert_idle();
        let screen = self.screen.dump_text();
        assert!(screen.contains(if install {
            "Auto-install connectors"
        } else {
            "Auto-update connectors"
        }));
        assert!(screen.contains("◉ off"));
        let (row, line) = screen
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("○ on"))
            .unwrap();
        for line in screen.lines().skip(row + 1).take(2) {
            assert!(line.split('│').nth(2).unwrap().trim().is_empty());
        }
        let x = line.chars().position(|c| c == '○').unwrap() as u16 + 1;
        let y = row as u16 + 1;
        self.mouse(0, x + 8, y);
        self.assert_idle();
        self.send(format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m").as_bytes());
        assert!(
            matches!(self.next_message(), ClientMessage::SettingsApply(patch) if (if install { patch.auto_install_connectors } else { patch.auto_update_connectors }) == Some(true))
        );
        let mut updated = snapshot();
        if install {
            updated.auto_install_connectors = true;
        } else {
            updated.auto_update_connectors = true;
        }
        self.stream
            .write_all(&encode_frame(&ServerMessage::Settings {
                settings: Box::new(updated),
                saved: true,
                error: None,
            }))
            .unwrap();
        self.wait_screen("◉ on");
        self.assert_idle();
        self.mouse(0, x, y);
        self.assert_idle();
        self.send(b"\r");
        assert!(
            matches!(self.next_message(), ClientMessage::SettingsApply(patch) if (if install { patch.auto_install_connectors } else { patch.auto_update_connectors }) == Some(false))
        );
        self.settings(true, "uniterm-dark");
        self.wait_screen("◉ off");
        self.send(&vec![b'k'; steps]);
        self.wait_screen("Default off.");
    }

    fn project_trust_setting(&mut self) {
        self.send(b"jj");
        self.wait_screen("Auto-trust Project directories");
        // The navigation label can already be visible before the client has
        // processed the selection. Wait for its control value too.
        self.wait_screen("◉ on");
        self.assert_idle();
        self.send(b" ");
        assert!(
            matches!(self.next_message(), ClientMessage::SettingsApply(patch)
            if patch == SettingsPatch { auto_trust_projects: Some(false), ..Default::default() })
        );
        let mut updated = snapshot();
        updated.auto_trust_projects = false;
        self.stream
            .write_all(&encode_frame(&ServerMessage::Settings {
                settings: Box::new(updated),
                saved: true,
                error: None,
            }))
            .unwrap();
        self.wait_screen("◉ off");
        let screen = self.screen.dump_text();
        let (row, line) = screen
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("○ on"))
            .unwrap();
        let x = line.chars().position(|c| c == '○').unwrap() as u16 + 1;
        let y = row as u16 + 1;
        self.mouse(0, x + 8, y);
        self.assert_idle();
        self.send(format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m").as_bytes());
        assert!(
            matches!(self.next_message(), ClientMessage::SettingsApply(patch)
            if patch == SettingsPatch { auto_trust_projects: Some(true), ..Default::default() })
        );
        self.settings(true, "uniterm-dark");
        self.wait_screen("◉ on");
        self.assert_idle();
        self.send(b"kk");
        self.wait_screen("Default off.");
        self.wait_screen("◉ off");
    }

    fn approval_persistence_setting(&mut self) {
        self.send(b"j");
        self.wait_screen("Keep auto-approve on restart");
        self.assert_idle();
        self.send(b" ");
        assert!(
            matches!(self.next_message(), ClientMessage::SettingsApply(patch)
            if patch == SettingsPatch { persist_auto_approve: Some(true), ..SettingsPatch::default() })
        );
        let mut updated = snapshot();
        updated.persist_auto_approve = true;
        self.stream
            .write_all(&encode_frame(&ServerMessage::Settings {
                settings: Box::new(updated),
                saved: true,
                error: None,
            }))
            .unwrap();
        self.drain();
        self.assert_idle();
        self.send(b" ");
        assert!(
            matches!(self.next_message(), ClientMessage::SettingsApply(patch)
            if patch == SettingsPatch { persist_auto_approve: Some(false), ..SettingsPatch::default() })
        );
        self.settings(true, "uniterm-dark");
        self.send(b"k");
        self.wait_screen("Default off.");
        assert!(self.screen.dump_text().contains("◉ off"));
    }

    fn agent_settings(&mut self) {
        self.send(b"jjjjjjjjj");
        self.wait_screen("Install all connectors");
        self.assert_idle();
        let screen = self.screen.dump_text();
        for provider in uniterm_core::agent::PROVIDERS {
            assert!(
                screen.contains(&format!("{}: missing", provider.name)),
                "{} missing: {screen}",
                provider.name
            );
        }
        self.send(b"\r");
        assert!(matches!(
            self.next_message(),
            ClientMessage::ConnectorsInstallAll
        ));
        self.agents(uniterm_proto::ConnectorStatus::Installed);
        self.wait_screen("Codex: ready");
        self.send(b"jj");
        self.wait_screen("Behavior");
        self.assert_idle();
        let screen = self.screen.dump_text();
        assert!(screen.contains("○ on"));
        assert!(screen.contains("◉ off"));
        assert!(screen.contains("Default off."));
        if let Some(path) = std::env::var_os("UNITERM_AGENT_SETTINGS_SCREEN_DUMP") {
            std::fs::write(path, format!("Workspace Settings (80x24)\n{screen}\n")).unwrap();
        }
        let (row, line) = screen
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("○ on"))
            .unwrap();
        let x = line.chars().position(|c| c == '○').unwrap() as u16 + 1;
        let y = row as u16 + 1;
        // Explanation starts below the setting, separated by two empty detail rows.
        for line in screen.lines().skip(row + 1).take(2) {
            assert!(line.split('│').nth(2).unwrap().trim().is_empty());
        }
        // Explicit Off revokes separate Pane arms even while Workspace mode is off.
        self.revoke_approval(x + 8, y);
        // A single click sends the change, without a confirmation screen.
        self.send(format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m").as_bytes());
        assert!(matches!(
            self.next_message(),
            ClientMessage::AutoApprove {
                enabled: Some(true),
                confirmed: false
            }
        ));
        self.approval(true);
        assert!(self.screen.dump_text().contains("◉ on"));
        self.send(b"\r");
        assert!(matches!(
            self.next_message(),
            ClientMessage::AutoApprove {
                enabled: Some(false),
                confirmed: false
            }
        ));
        self.approval(false);
        assert!(self.screen.dump_text().contains("◉ off"));
        self.approval_persistence_setting();
        self.project_trust_setting();
        self.connector_setting(false, true);
        self.connector_setting(true, true);
        self.send(b"\x1b");
        self.wait_screen(BACKGROUND);
        self.assert_idle();
        self.send(b"\x01a");
        self.wait_screen("Agents Config");
        self.send(&vec![b'j'; uniterm_core::agent::PROVIDERS.len()]);
        self.wait_screen("Behavior");
        self.assert_idle();
        let screen = self.screen.dump_text();
        assert!(screen.contains("◉ off"));
        assert!(screen.contains("Default off."));
        assert!(screen.lines().any(|line| line
            .split('│')
            .nth(1)
            .is_some_and(|rail| rail.contains("Auto-approve"))));
        let (row, line) = screen
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("○ on"))
            .unwrap();
        for line in screen.lines().skip(row + 1).take(2) {
            assert!(line.split('│').nth(2).unwrap().trim().is_empty());
        }
        let x = line.chars().position(|c| c == '○').unwrap() as u16 + 1;
        let y = row as u16 + 1;
        self.revoke_approval(x + 8, y);
        self.send(format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m").as_bytes());
        assert!(matches!(
            self.next_message(),
            ClientMessage::AutoApprove {
                enabled: Some(true),
                confirmed: false,
            }
        ));
        self.approval(true);
        self.send(b"A");
        assert!(matches!(
            self.next_message(),
            ClientMessage::AutoApprove {
                enabled: Some(false),
                confirmed: false,
            }
        ));
        self.approval(false);
        self.assert_idle();
        self.connector_setting(false, false);
        self.connector_setting(true, false);
        if let Some(path) = std::env::var_os("UNITERM_AGENT_SETTINGS_SCREEN_DUMP") {
            let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
            writeln!(file, "\nAgents Config (80x24)\n{screen}").unwrap();
        }
        self.send(b"\x1b");
        self.wait_screen(BACKGROUND);
        self.assert_idle();
        self.send(b"\x01g");
        self.wait_screen("Current: uniterm-dark");
        self.assert_idle();
    }

    fn overwatch_controls(&mut self) {
        self.send(b"\x01O");
        assert!(matches!(
            self.next_message(),
            ClientMessage::Command(uniterm_proto::Command::Overwatch)
        ));
        self.stream
            .write_all(&encode_frame(&ServerMessage::Overwatch {
                state: uniterm_core::overwatch::State::default(),
                providers: vec!["codex".into(), "claude".into()],
                open: true,
            }))
            .unwrap();
        self.wait_screen("Overwatch OFF");
        self.wait_screen("Provider: codex");
        self.assert_idle();
        self.send(b"codex\r");
        assert!(matches!(self.next_message(), ClientMessage::Overwatch {
            operation: uniterm_core::overwatch::Operation::Configure { provider, manage: false, .. }
        } if provider == "codex"));
        self.wait_screen(BACKGROUND);
        self.send(b"\x01g");
        self.wait_settings();
        self.send(b"jjjjjjjjjj");
        self.wait_screen("Open Overwatch");
        let status = uniterm_core::overwatch::State {
            enabled: true,
            provider: "codex".into(),
            phase: uniterm_core::overwatch::Phase::Recovering,
            recoveries: 1,
            ..Default::default()
        };
        self.stream
            .write_all(&encode_frame(&ServerMessage::Overwatch {
                state: status.clone(),
                providers: vec!["codex".into()],
                open: false,
            }))
            .unwrap();
        self.wait_screen("Status: ON - Recovering");
        self.wait_screen("Provider: codex");
        self.assert_idle();
        self.stream
            .write_all(&encode_frame(&ServerMessage::Overwatch {
                state: status,
                providers: vec!["codex".into()],
                open: false,
            }))
            .unwrap();
        self.assert_idle();
        self.send(b"\r");
        assert!(matches!(
            self.next_message(),
            ClientMessage::Command(uniterm_proto::Command::Overwatch)
        ));
        self.stream
            .write_all(&encode_frame(&ServerMessage::Overwatch {
                state: uniterm_core::overwatch::State {
                    enabled: true,
                    provider: "codex".into(),
                    ..Default::default()
                },
                providers: vec!["codex".into()],
                open: true,
            }))
            .unwrap();
        self.wait_screen("Overwatch ON");
        self.wait_screen("interrupt reply");
        self.wait_screen("Model: Provider default");
        self.assert_idle();
        let screen = self.screen.dump_text();
        let rows: Vec<_> = screen.lines().collect();
        let chat = rows
            .iter()
            .position(|row| row.contains("Open chat"))
            .unwrap();
        assert!(rows[chat + 1].contains('├') && rows[chat + 1].contains('┤'));
        assert!(rows[chat + 2].contains("Provider: codex"));
        assert!(rows[chat + 3].contains("Model: Provider default"));
        assert!(rows[chat + 4].contains("Reasoning effort:"));
        assert!(rows[chat + 5].contains("Retry recovery"));
        assert!(rows[chat + 6].contains("Access: Read-only"));
        let x = rows[chat + 1].chars().position(|ch| ch == '├').unwrap() as u16 + 3;
        self.mouse(0, x, chat as u16 + 2);
        assert!(
            self.messages.is_empty(),
            "divider clicks must not dispatch an action"
        );
        let catalog = uniterm_core::agent_models::ModelCatalog {
            provider: "codex".into(),
            notice: "Installed harness catalog".into(),
            models: vec![uniterm_core::agent_models::ModelChoice {
                id: "fixture-model".into(),
                label: "Friendly Fixture".into(),
                description: "Model description from the installed harness".into(),
                aliases: vec!["friendly-alias".into()],
                efforts: vec![
                    uniterm_core::agent_models::EffortChoice {
                        id: "high".into(),
                        label: "High".into(),
                        description: "Careful reasoning".into(),
                    },
                    uniterm_core::agent_models::EffortChoice {
                        id: "xhigh".into(),
                        label: "Extra High".into(),
                        description: "Hardest tasks".into(),
                    },
                ],
                default_effort: Some("high".into()),
                effort_support: uniterm_core::agent_models::EffortSupport::Supported,
            }],
            ..Default::default()
        };
        self.send(b"Model\r");
        let ClientMessage::Overwatch {
            operation: uniterm_core::overwatch::Operation::Models { request },
        } = self.next_message()
        else {
            panic!("expected model catalog request");
        };
        self.stream
            .write_all(&encode_frame(&ServerMessage::OverwatchModels {
                request,
                catalog: catalog.clone(),
            }))
            .unwrap();
        self.wait_screen("Overwatch models");
        self.wait_screen("Friendly Fixture [fixture-model]");
        self.assert_idle();
        self.send(b"friendly-alias\r");
        self.wait_screen("Overwatch effort");
        self.wait_screen("restarts chat");
        self.wait_screen("Extra High [xhigh]");
        self.assert_idle(); // Choosing a model cannot send a restart yet.
        self.stream
            .write_all(&encode_frame(&ServerMessage::Overwatch {
                state: uniterm_core::overwatch::State {
                    enabled: true,
                    provider: "codex".into(),
                    ..Default::default()
                },
                providers: vec!["codex".into()],
                open: false,
            }))
            .unwrap();
        self.assert_idle();
        assert!(
            self.screen.dump_text().contains("Overwatch effort"),
            "live state must preserve the picker"
        );
        self.send(b"Extra High\r");
        assert!(matches!(self.next_message(), ClientMessage::Overwatch {
            operation: uniterm_core::overwatch::Operation::Selection { provider, model: Some(model), effort: Some(effort) }
        } if provider == "codex" && model == "fixture-model" && effort == "xhigh"));
        self.wait_screen(BACKGROUND);
        self.assert_idle();

        // Pointer entry, cancellation between model and effort, and a late reply.
        let selected = uniterm_core::overwatch::State {
            enabled: true,
            provider: "codex".into(),
            model: Some("fixture-model".into()),
            effort: Some("xhigh".into()),
            ..Default::default()
        };
        self.stream
            .write_all(&encode_frame(&ServerMessage::Overwatch {
                state: selected.clone(),
                providers: vec!["codex".into()],
                open: true,
            }))
            .unwrap();
        self.wait_screen("Model: fixture-model");
        let screen = self.screen.dump_text();
        let (row, line) = screen
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("Model: fixture-model"))
            .unwrap();
        let x = line.chars().position(|ch| ch == 'M').unwrap() as u16 + 1;
        self.send(format!("\x1b[<0;{x};{}M\x1b[<0;{x};{}m", row + 1, row + 1).as_bytes());
        let ClientMessage::Overwatch {
            operation: uniterm_core::overwatch::Operation::Models { request },
        } = self.next_message()
        else {
            panic!("expected pointer model catalog request");
        };
        self.stream
            .write_all(&encode_frame(&ServerMessage::OverwatchModels {
                request,
                catalog: catalog.clone(),
            }))
            .unwrap();
        self.wait_screen("Friendly Fixture [fixture-model]");
        self.send(b"friendly-alias\r");
        self.wait_screen("Overwatch effort");
        self.send(b"\x1b");
        self.wait_screen(BACKGROUND);
        self.assert_idle();
        self.stream
            .write_all(&encode_frame(&ServerMessage::OverwatchModels {
                request,
                catalog: catalog.clone(),
            }))
            .unwrap();
        self.assert_idle();
        assert!(!self.screen.dump_text().contains("Overwatch effort"));

        // Defaults are a single explicit choice, using the same pointer rectangles.
        self.stream
            .write_all(&encode_frame(&ServerMessage::Overwatch {
                state: selected,
                providers: vec!["codex".into()],
                open: true,
            }))
            .unwrap();
        self.wait_screen("Model: fixture-model");
        self.send(b"Model\r");
        let ClientMessage::Overwatch {
            operation: uniterm_core::overwatch::Operation::Models { request },
        } = self.next_message()
        else {
            panic!("expected another model catalog request");
        };
        // The default row already exists while the catalog is loading. Its
        // current-model fallback has a wider preview, so those coordinates
        // can fall outside the narrower loaded menu. Observe that intermediate
        // frame deliberately, then require catalog-specific content below.
        self.wait_screen("Loading models...");
        let loading = self.screen.dump_text();
        let loading_default_x = loading
            .lines()
            .find(|line| line.contains("Harness default (model and effort)"))
            .unwrap()
            .chars()
            .position(|ch| ch == 'H')
            .unwrap() as u16
            + 1;
        self.stream
            .write_all(&encode_frame(&ServerMessage::OverwatchModels {
                request,
                catalog,
            }))
            .unwrap();
        self.wait_screen("Friendly Fixture [fixture-model]");
        self.wait_screen("Harness default (model and effort)");
        let screen = self.screen.dump_text();
        assert!(!screen.contains("Loading models..."));
        let (row, line) = screen
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("Harness default (model and effort)"))
            .unwrap();
        let x = line.chars().position(|ch| ch == 'H').unwrap() as u16 + 1;
        let loaded_left = line.chars().position(|ch| ch == '│').unwrap() as u16 + 1;
        assert!(
            loading_default_x < loaded_left,
            "the old default-row click must reproduce the stale-geometry miss"
        );
        self.send(format!("\x1b[<0;{x};{}M\x1b[<0;{x};{}m", row + 1, row + 1).as_bytes());
        assert!(matches!(self.next_message(), ClientMessage::Overwatch {
            operation: uniterm_core::overwatch::Operation::Selection { provider, model: None, effort: None }
        } if provider == "codex"));
        self.wait_screen(BACKGROUND);
        self.assert_idle();

        let live = uniterm_core::overwatch::State {
            enabled: true,
            provider: "codex".into(),
            model: Some("fixture-model".into()),
            project: Some(uniterm_core::ProjectId(42)),
            manage: true,
            ..Default::default()
        };
        for (provider, pointer) in [("codex", false), ("claude", true)] {
            self.stream
                .write_all(&encode_frame(&ServerMessage::Overwatch {
                    state: live.clone(),
                    providers: vec!["codex".into(), "claude".into()],
                    open: true,
                }))
                .unwrap();
            self.wait_screen("Overwatch ON");
            self.wait_screen("Provider: claude");
            if pointer {
                let screen = self.screen.dump_text();
                let (row, line) = screen
                    .lines()
                    .enumerate()
                    .find(|(_, line)| line.contains("Provider: claude"))
                    .unwrap();
                let x = line.chars().position(|ch| ch == 'P').unwrap() as u16 + 1;
                self.send(format!("\x1b[<0;{x};{}M", row + 1).as_bytes());
            } else {
                self.send(b"codex\r");
            }
            assert!(matches!(self.next_message(), ClientMessage::Overwatch {
                operation: uniterm_core::overwatch::Operation::Configure {
                    provider: chosen, model, project: Some(uniterm_core::ProjectId(42)), manage: true,
                }
            } if chosen == provider && model == (provider == "codex").then(|| "fixture-model".to_string())));
            self.wait_screen(BACKGROUND);
            self.assert_idle();
        }
    }

    fn workspace_switcher(&mut self) {
        self.send(b"\x01S");
        assert!(matches!(
            self.next_message(),
            ClientMessage::Command(uniterm_proto::Command::Switcher)
        ));
        let entry = uniterm_proto::SwitcherEntry {
            pane: uniterm_proto::PaneInfo {
                id: uniterm_core::PaneId(42),
                project: uniterm_core::ProjectId(7),
                project_name: "API".into(),
                tab: 2,
                tab_name: "Repair auth".into(),
                pane: 1,
                active: false,
                stack_parent: None,
                layout_mode: uniterm_core::layout::PaneLayout::Split,
            },
            cwd: "/work/login".into(),
            branch: "fix/auth".into(),
            agent: "codex".into(),
            status: "working".into(),
            preview: "Tests pass".into(),
        };
        self.stream
            .write_all(&encode_frame(&ServerMessage::Switcher {
                open: true,
                enabled: true,
                entries: vec![entry.clone()],
            }))
            .unwrap();
        self.wait_screen("Find Tabs and Agents");
        assert!(matches!(
            self.next_message(),
            ClientMessage::SwitcherSubscribe { on: true }
        ));
        self.wait_screen("Tests pass");
        self.assert_idle();
        self.send(b"fix/auth");
        self.wait_screen("Repair auth");
        self.assert_idle();
        let mut updated = entry.clone();
        updated.pane.tab = 9;
        updated.status = "question".into();
        self.stream
            .write_all(&encode_frame(&ServerMessage::Switcher {
                open: false,
                enabled: true,
                entries: vec![updated],
            }))
            .unwrap();
        self.wait_screen("question");
        self.send(b"\r");
        assert!(matches!(
            self.next_message(),
            ClientMessage::PaneFocus {
                pane: uniterm_core::PaneId(42)
            }
        ));
        assert!(matches!(
            self.next_message(),
            ClientMessage::SwitcherSubscribe { on: false }
        ));
        self.wait_screen(BACKGROUND);
        self.assert_idle();
        self.stream
            .write_all(&encode_frame(&ServerMessage::Switcher {
                open: false,
                enabled: true,
                entries: vec![entry],
            }))
            .unwrap();
        self.assert_idle();
        assert!(!self.screen.dump_text().contains("Find Tabs and Agents"));
        self.send(b"\x01S");
        assert!(matches!(
            self.next_message(),
            ClientMessage::Command(uniterm_proto::Command::Switcher)
        ));
        self.stream
            .write_all(&encode_frame(&ServerMessage::Switcher {
                open: true,
                enabled: false,
                entries: Vec::new(),
            }))
            .unwrap();
        self.wait_screen("Find Tabs and Agents - OFF");
        assert!(matches!(
            self.next_message(),
            ClientMessage::SwitcherSubscribe { on: true }
        ));
        self.send(b"\x1b");
        assert!(matches!(
            self.next_message(),
            ClientMessage::SwitcherSubscribe { on: false }
        ));
        self.wait_screen(BACKGROUND);
        self.assert_idle();
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

    fn revoke_approval(&mut self, x: u16, y: u16) {
        self.send(format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m").as_bytes());
        assert!(matches!(
            self.next_message(),
            ClientMessage::AutoApprove {
                enabled: Some(false),
                confirmed: false,
            }
        ));
        self.approval(false);
        self.assert_idle();
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
        follow_omarchy_theme: false,
        omarchy_available: false,
        effective_theme: Theme::dark(),
        theme_follow_error: None,
        themes: ThemePreset::ALL
            .iter()
            .map(|preset| preset.name().into())
            .collect(),
        status: true,
        status_top: false,
        tab_label_alignment: "center".into(),
        run_graph: false,
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
        auto_update_connectors: false,
        auto_install_connectors: false,
        persist_auto_approve: false,
        auto_trust_projects: true,
        auto_tab_names: false,
        auto_track_tasks: true,
        switcher_enabled: false,
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
    let mut host = snapshot();
    host.omarchy_available = true;
    host.follow_omarchy_theme = true;
    // A typed host palette deliberately differs from the named fallback and
    // from anything the client's local config could infer from that name.
    host.effective_theme.surface = Color::Rgb(11, 23, 37);
    host.effective_theme.foreground = Color::Rgb(211, 223, 237);
    let (mut fixture, directory) = Fixture::start(host.clone());
    fixture.live_host_theme(host);
    fixture.workspace_switcher();
    fixture.overwatch_controls();
    fixture.send(b"\x01g");
    fixture.wait_settings();
    fixture.assert_idle();
    let settings_screen = fixture.screen.dump_text();
    assert!(!settings_screen.contains("Follow Omarchy theme"));
    assert!(settings_screen.contains("Current: uniterm-dark"));
    assert!(settings_screen.contains("$ uniterm"));
    assert!(settings_screen.contains("Ready"));
    fixture.agent_settings();
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

#[test]
fn nested_omarchy_toggle_fits_below_preview_in_narrow_and_wide_terminal_grids() {
    use uniterm_client::settings::SettingsView;
    for (cols, rows) in [(80, 24), (120, 40)] {
        for name in [
            "uniterm-dark",
            "夜明けのテーマ é 👩‍💻 with a deliberately long fallback name",
        ] {
            let mut data = snapshot();
            data.omarchy_available = true;
            data.theme = name.into();
            let view = SettingsView::new(data, true, None);
            let mut terminal = Terminal::new(cols, rows);
            terminal.feed(&view.render(cols, rows));
            let screen = terminal.dump_text();
            assert!(screen.find("Ready").unwrap() < screen.find("Follow Omarchy theme").unwrap());
            let line = screen
                .lines()
                .find(|line| line.contains("Follow Omarchy theme"))
                .unwrap();
            assert!(line.contains("○ on") && line.contains("◉ off"), "{line}");
            let rect = SettingsView::rect(cols, rows);
            for row in 0..rect.h - 4 {
                assert_eq!(
                    terminal.grid().get(rect.x + rect.w - 2, rect.y + row).ch,
                    '│'
                );
            }
        }
    }
}
