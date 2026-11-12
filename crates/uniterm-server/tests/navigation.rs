//! Opt-in naming and switcher projections share live, stable Pane identities.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};
use uniterm_core::{Config, PaneId};
use uniterm_proto::{encode_frame, ClientMessage, Command, FrameDecoder, ServerMessage};
use uniterm_server::{Server, Terminal};
mod common;

struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
    screen: Terminal,
}
impl Client {
    fn send(&mut self, message: ClientMessage) {
        self.stream.write_all(&encode_frame(&message)).unwrap();
    }
    fn until(&mut self, mut done: impl FnMut(&ServerMessage, &Terminal) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut bytes = [0; 32768];
        while Instant::now() < deadline {
            while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                if let ServerMessage::RenderOps(ops) = &message {
                    self.screen.feed(ops);
                }
                if done(&message, &self.screen) {
                    return;
                }
            }
            match self.stream.read(&mut bytes) {
                Ok(0) => panic!("unexpected disconnect"),
                Ok(size) => self.decoder.push(&bytes[..size]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("{error}"),
            }
        }
        panic!("condition was not observed: {}", self.screen.dump_text());
    }
    fn shell(&mut self, script: &str, marker: &str) {
        self.send(ClientMessage::Input(
            format!("{script}; printf '{marker}\\n'\n").into_bytes(),
        ));
        self.until(|_, screen| screen.dump_text().contains(marker));
    }
    fn name(&mut self, expected: &str) -> PaneId {
        self.send(ClientMessage::PaneList);
        let mut id = PaneId(0);
        self.until(|message, _| match message {
            ServerMessage::Panes { panes, .. } => {
                assert_eq!(panes.len(), 1);
                assert_eq!(panes[0].tab_name, expected, "Pane snapshot: {:?}", panes[0]);
                id = panes[0].id;
                true
            }
            _ => false,
        });
        id
    }
}

#[test]
fn navigation_opt_in_summary_manual_names_session_reset_and_idle_subscription() {
    common::isolate_state();
    for enabled in [false, true] {
        let workspace = common::unique_workspace_name();
        let root = std::env::temp_dir().join(format!("navigation-{}", common::unique_nonce()));
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join(format!("{workspace}.sock"));
        let directory = root.file_name().unwrap().to_str().unwrap().to_owned();
        let server_socket = socket.clone();
        let worker = thread::spawn(move || {
            let (mut server, mut poll) = Server::bind(
                &server_socket,
                "/bin/sh",
                &["-c", "stty -echo; printf 'NAV_READY\\n'; exec /bin/sh"],
                100,
                30,
            )
            .unwrap();
            server.set_config(Config {
                auto_tab_names: enabled,
                switcher_enabled: enabled,
                sidebar: false,
                file_sidebar: false,
                ..Config::default()
            });
            server.run(&mut poll).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut client = Client {
            stream,
            decoder: FrameDecoder::new(),
            screen: Terminal::new(100, 30),
        };
        client.send(ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 100,
            rows: 30,
        });
        client.until(|_, screen| screen.dump_text().contains("NAV_READY"));
        let initial_directory = std::env::current_dir()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        client.name(if enabled { &initial_directory } else { "Tab 1" });
        client.shell(
            &format!("printf '\\033]7;file://localhost{}\\007'", root.display()),
            "DIR_READY",
        );
        client.name(if enabled { &directory } else { "Tab 1" });
        if enabled {
            client.send(ClientMessage::Input(b"sleep 1\n".to_vec()));
            client.until(|_, screen| screen.dump_text().contains(": sleep"));
            client.name(&format!("{directory}: sleep"));
            client.shell("true", "PROCESS_FINISHED");
            client.name(&directory);
            assert!(!client
                .screen
                .dump_text()
                .lines()
                .next()
                .unwrap_or("")
                .contains(": sleep"));
        }
        for provider in ["codex", "claude", "grok", "opencode", "pi"] {
            client.shell(
                "printf '\\033]777;notify;uniterm://cli-agent;{\"event\":\"session_end\"}\\007'",
                &format!("END_{provider}"),
            );
            let script = format!("printf '\\033]777;notify;uniterm://cli-agent;{{\"agent\":\"{provider}\",\"event\":\"session_start\",\"session_id\":\"one\",\"summary\":\"Repair auth\"}}\\007'");
            client.shell(&script, &format!("SUMMARY_{provider}"));
            client.name(if enabled { "Repair auth" } else { "Tab 1" });
        }
        let id = client.name(if enabled { "Repair auth" } else { "Tab 1" });
        client.send(ClientMessage::RenameWindow {
            name: "My chosen tab".into(),
        });
        client.name("My chosen tab");
        client.shell("printf '\\033]777;notify;uniterm://cli-agent;{\"agent\":\"pi\",\"event\":\"session_start\",\"session_id\":\"two\"}\\007'", "NEW_SESSION");
        client.name("My chosen tab");
        client.send(ClientMessage::RenameWindow {
            name: String::new(),
        });
        client.name("Tab 1");
        client.shell("printf '\\033]2;Repair rendering\\007'", "TASK_TITLE");
        client.name(if enabled { "Repair rendering" } else { "Tab 1" });
        client.shell("printf '\\033]2;Working\\007'", "ACTIVITY_TITLE");
        client.name(if enabled { "Repair rendering" } else { "Tab 1" });
        client.shell("printf '\\033]777;notify;uniterm://cli-agent;{\"agent\":\"pi\",\"event\":\"session_start\",\"session_id\":\"three\"}\\007'", "TITLE_SESSION_RESET");
        client.name("Tab 1");
        client.send(ClientMessage::Command(Command::Switcher));
        client.until(|message, _| match message {
            ServerMessage::Switcher {
                open,
                enabled: actual,
                entries,
            } => {
                assert!(*open);
                assert_eq!(*actual, enabled);
                assert_eq!(entries.len(), usize::from(enabled));
                if enabled {
                    assert_eq!(entries[0].pane.id, id);
                }
                true
            }
            _ => false,
        });
        if enabled {
            client.send(ClientMessage::SwitcherSubscribe { on: true });
            client
                .until(|message, _| matches!(message, ServerMessage::Switcher { open: false, .. }));
            client.send(ClientMessage::Refresh);
            client.send(ClientMessage::PaneList);
            client.until(|message, _| {
                assert!(
                    !matches!(message, ServerMessage::Switcher { .. }),
                    "unchanged projection must not publish"
                );
                matches!(message, ServerMessage::Panes { .. })
            });
            client.send(ClientMessage::SwitcherSubscribe { on: false });
            client.send(ClientMessage::PaneMetadata {
                pane: id,
                key: "summary".into(),
                value: "Changed after close".into(),
                ttl_seconds: None,
            });
            client.send(ClientMessage::PaneList);
            client.until(|message, _| {
                assert!(
                    !matches!(message, ServerMessage::Switcher { .. }),
                    "closed switcher must not publish"
                );
                matches!(message, ServerMessage::Panes { .. })
            });
        }
        client.send(ClientMessage::KillServer);
        worker.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
