//! Agent-card menus target one invocation across Project, Tab and Pane changes.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use uniterm_core::Config;
use uniterm_proto::{
    encode_frame, ClientMessage, Command, FrameDecoder, MouseKind, PaneInfo, ServerMessage,
};
use uniterm_server::Server;

mod common;

#[test]
fn agent_menu_focuses_copies_and_stops_only_the_selected_invocation() {
    common::isolate_state();
    let root = common::temp_dir("agent-menu");
    let socket_dir = common::socket_root().join(format!("ut-agent-menu-{}", std::process::id()));
    std::fs::create_dir_all(&socket_dir).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", socket_dir.join("config"));
    std::env::set_var("XDG_RUNTIME_DIR", &socket_dir);
    let socket = socket_dir.join(format!("{}.sock", common::unique_workspace_name()));
    let server_socket = socket.clone();
    let server = std::thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 100, 30).unwrap();
        server.set_config(Config {
            sidebar: false,
            confirm_close: false,
            ..Config::default()
        });
        server.run(&mut poll).unwrap();
    });
    common::wait_for_socket(&socket);
    let stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut client = Client {
        stream,
        decoder: FrameDecoder::new(),
    };
    client.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 100,
        rows: 30,
    });
    let source = client.panes()[0].id;
    client.announce(source, "session_start");
    client.rendered("AGENTS 1");

    // The first attach exposes all actions and uses the latest cached OSC 7
    // directory, not the Project root or whichever Pane is active.
    let path = root.join("agent working dir");
    std::fs::create_dir_all(&path).unwrap();
    client.send(ClientMessage::PaneSend {
        pane: source,
        bytes: format!(
            "printf '%b' '\\033]7;file://localhost{}\\007'\n",
            path.to_string_lossy().replace(' ', "%20")
        )
        .into_bytes(),
    });
    // A shell marker provides a PTY-output barrier for the OSC 7 report.
    client.send(ClientMessage::PaneSend {
        pane: source,
        bytes: b"printf 'PATH_%s\\n' UPDATED\n".to_vec(),
    });
    client.rendered("PATH_UPDATED");
    client.menu(5);
    client.send(ClientMessage::Input(b"j\r".to_vec()));
    let clipboard = uniterm_server::copymode::osc52(&path.to_string_lossy());
    client.until(|m| matches!(m, ServerMessage::RenderOps(ops) if ops == &clipboard));

    // Padding and the heading do not select neighboring cards.
    for row in [3, 7] {
        client.mouse(95, row, MouseKind::RightClick);
        let frame = client.rendered("AGENTS 1");
        assert!(!frame.contains("Stop agent"));
    }

    client.send(ClientMessage::Command(Command::Split(
        uniterm_proto::SplitAxis::LeftRight,
    )));
    let sibling = client.panes().into_iter().find(|p| p.active).unwrap().id;
    client.announce(sibling, "session_start");
    client.rendered("AGENTS 2");
    client.send(ClientMessage::Command(Command::ZoomToggle));
    client.send(ClientMessage::ProjectCreate {
        name: "Other".into(),
        root: root.to_string_lossy().into_owned(),
    });
    let other = client.panes().into_iter().find(|p| p.active).unwrap().id;
    assert_ne!(other, source);
    client.mouse(95, 3, MouseKind::Click); // Show the whole Workspace.
    client.rendered("AGENTS 2");
    client.menu(5);
    client.send(ClientMessage::Input(b"\r".to_vec()));
    assert!(client.panes().iter().any(|p| p.id == source && p.active));

    // Rebinding the same Pane while its menu is open invalidates all actions.
    client.menu(5);
    client.announce(source, "session_end");
    client.rendered("AGENTS 1");
    client.announce(source, "session_start");
    client.rendered("AGENTS 2");
    client.send(ClientMessage::Input(b"jj\r".to_vec()));
    assert_eq!(client.panes().len(), 3);

    // The replacement now sorts after its sibling. Click the actual menu row
    // to exercise mouse submission as well as keyboard submission above.
    client.menu(8);
    client.mouse(81, 11, MouseKind::Click);
    let remaining = client.panes();
    assert_eq!(remaining.len(), 2);
    assert!(!remaining.iter().any(|p| p.id == source));
    assert!(remaining.iter().any(|p| p.id == sibling));
    assert!(remaining.iter().any(|p| p.id == other));

    // A closed target is also a no-op, even with another live agent present.
    client.menu(5);
    client.send(ClientMessage::AgentStop { pane: sibling });
    assert_eq!(client.panes().len(), 1);
    client.send(ClientMessage::Input(b"jj\r".to_vec()));
    assert_eq!(client.panes()[0].id, other);
    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(socket_dir).unwrap();
}

struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
}
impl Client {
    fn send(&mut self, message: ClientMessage) {
        self.stream.write_all(&encode_frame(&message)).unwrap();
    }
    fn until(&mut self, predicate: impl Fn(&ServerMessage) -> bool) -> ServerMessage {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut bytes = [0; 32768];
        loop {
            while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                if predicate(&message) {
                    return message;
                }
            }
            assert!(Instant::now() < deadline, "expected server response");
            match self.stream.read(&mut bytes) {
                Ok(0) => panic!("server disconnected"),
                Ok(n) => self.decoder.push(&bytes[..n]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("{error}"),
            }
        }
    }
    fn panes(&mut self) -> Vec<PaneInfo> {
        self.send(ClientMessage::PaneList);
        let ServerMessage::Panes { panes, .. } =
            self.until(|m| matches!(m, ServerMessage::Panes { .. }))
        else {
            unreachable!()
        };
        panes
    }
    fn rendered(&mut self, text: &str) -> String {
        let ServerMessage::RenderOps(ops) = self.until(|m| matches!(m, ServerMessage::RenderOps(ops) if String::from_utf8_lossy(ops).contains(text))) else { unreachable!() };
        String::from_utf8_lossy(&ops).into_owned()
    }
    fn mouse(&mut self, x: u16, y: u16, kind: MouseKind) {
        self.send(ClientMessage::Mouse { x, y, kind });
    }
    fn menu(&mut self, row: u16) {
        self.mouse(95, row, MouseKind::RightClick);
        let frame = self.rendered("Stop agent");
        assert!(frame.contains("Go to agent pane"));
        assert!(frame.contains("Copy working path"));
    }
    fn announce(&mut self, pane: uniterm_core::PaneId, event: &str) {
        self.send(ClientMessage::PaneSend {
            pane,
            bytes: format!("printf '\\033]777;notify;uniterm://cli-agent;{{\"agent\":\"claude\",\"event\":\"{event}\"}}\\007'\n").into_bytes(),
        });
    }
}
