//! Server-card menus preserve stable targets and stop only the selected listener.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use uniterm_core::Config;
use uniterm_proto::{
    encode_frame, ClientMessage, Command, DevServerEntry, FrameDecoder, MouseKind, PaneInfo,
    ServerMessage,
};
use uniterm_server::{workflow::shell_quote, Server};

mod common;

// Spawned in its own test process inside a Pane, so killing it cannot end the
// harness. No helper language or installed development server is required.
#[test]
fn listener_fixture() {
    let Some(marker) = std::env::var_os("UNITERM_TEST_LISTENER_FILE") else {
        return;
    };
    if std::env::var_os("UNITERM_TEST_IGNORE_TERM").is_some() {
        // SAFETY: affects only this disposable fixture process.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::fs::write(marker, port.to_string()).unwrap();
    println!("Server listening on http://localhost:{port}");
    std::io::stdout().flush().unwrap();
    for connection in listener.incoming() {
        drop(connection.unwrap());
    }
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
    fn servers(&mut self, count: usize) -> Vec<DevServerEntry> {
        self.send(ClientMessage::Observatory);
        let ServerMessage::DevServers { entries } = self.until(
            |m| matches!(m, ServerMessage::DevServers { entries } if entries.len() == count),
        ) else {
            unreachable!()
        };
        entries
    }
    fn rendered(&mut self, text: &str) -> String {
        let ServerMessage::RenderOps(ops) = self.until(|m| matches!(m, ServerMessage::RenderOps(ops) if String::from_utf8_lossy(ops).contains(text))) else { unreachable!() };
        String::from_utf8_lossy(&ops).into_owned()
    }
    fn mouse(&mut self, x: u16, y: u16, kind: MouseKind) {
        self.send(ClientMessage::Mouse { x, y, kind });
    }
    fn menu(&mut self, index: usize) {
        self.mouse(95, 5 + index as u16 * 3, MouseKind::RightClick);
        let frame = self.rendered("Kill server");
        assert!(frame.contains("Open in browser"));
        assert!(frame.contains("Go to source pane"));
    }
}

#[test]
fn server_menu_opens_focuses_and_kills_only_its_listener() {
    common::isolate_state();
    let root = common::temp_dir("server-menu");
    let socket_dir = common::socket_root().join(format!("ut-server-menu-{}", std::process::id()));
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
    let executable = shell_quote(&std::env::current_exe().unwrap().to_string_lossy());
    for (name, ignore) in [("first", true), ("second", false)] {
        let marker = shell_quote(&root.join(name).to_string_lossy());
        let ignore = if ignore {
            "UNITERM_TEST_IGNORE_TERM=1 "
        } else {
            ""
        };
        client.send(ClientMessage::Input(format!("{ignore}UNITERM_TEST_LISTENER_FILE={marker} {executable} --exact listener_fixture --nocapture &\n").into_bytes()));
    }
    let entries = client.servers(2);
    let first_port: u16 = std::fs::read_to_string(root.join("first"))
        .unwrap()
        .parse()
        .unwrap();
    let second_port: u16 = std::fs::read_to_string(root.join("second"))
        .unwrap()
        .parse()
        .unwrap();
    let first_index = entries
        .iter()
        .position(|entry| entry.port == first_port)
        .unwrap();
    let url = format!("http://localhost:{first_port}");
    client.mouse(95, 1, MouseKind::Click);
    client.rendered("WEB SERVERS 2");

    // A first-attached client can open the browser through either input path.
    client.mouse(95, 5 + first_index as u16 * 3, MouseKind::Click);
    client.until(|m| matches!(m, ServerMessage::OpenUrl { url: opened } if opened == &url));
    client.menu(first_index);
    client.send(ClientMessage::Input(b"\r".to_vec()));
    client.until(|m| matches!(m, ServerMessage::OpenUrl { url: opened } if opened == &url));

    // A card gap must not accidentally open a menu for an adjacent server.
    client.mouse(95, 7, MouseKind::RightClick);
    let frame = client.rendered("WEB SERVERS 2");
    assert!(!frame.contains("Kill server"));

    // Source navigation crosses Projects and returns to the exact split Pane.
    client.send(ClientMessage::Command(Command::Split(
        uniterm_proto::SplitAxis::LeftRight,
    )));
    client.send(ClientMessage::Command(Command::ZoomToggle));
    client.send(ClientMessage::ProjectCreate {
        name: "Other".into(),
        root: root.to_string_lossy().into_owned(),
    });
    let other = client
        .panes()
        .into_iter()
        .find(|pane| pane.active)
        .unwrap()
        .id;
    assert_ne!(other, source);
    client.mouse(95, 3, MouseKind::Click); // workspace scope
    client.rendered("WEB SERVERS 2");
    client.menu(first_index);
    client.send(ClientMessage::Input(b"j\r".to_vec()));
    assert!(client
        .panes()
        .iter()
        .any(|pane| pane.id == source && pane.active));

    // Ignore SIGTERM in the first listener to exercise bounded SIGKILL escalation.
    client.menu(first_index);
    client.send(ClientMessage::Input(b"jj\r".to_vec()));
    let remaining = client.servers(1);
    assert_eq!(remaining[0].port, second_port);
    assert!(TcpStream::connect(("127.0.0.1", first_port)).is_err());
    assert!(TcpStream::connect(("127.0.0.1", second_port)).is_ok());
    let panes = client.panes();
    assert_eq!(panes.len(), 3);
    assert!(panes.iter().any(|pane| pane.id == source));
    client.send(ClientMessage::Input(
        b"printf 'SOURCE_%s\\n' SHELL_SURVIVED\n".to_vec(),
    ));
    client.rendered("SOURCE_SHELL_SURVIVED");

    // A log line for another session's listener must never authorize killing it.
    let outsider = TcpListener::bind("127.0.0.1:0").unwrap();
    let outside_port = outsider.local_addr().unwrap().port();
    client.send(ClientMessage::Input(
        format!("printf 'Server listening on http://localhost:{outside_port}\\n'\n").into_bytes(),
    ));
    let entries = client.servers(2);
    let outside_index = entries
        .iter()
        .position(|entry| entry.port == outside_port)
        .unwrap();
    client.menu(outside_index);
    client.send(ClientMessage::Input(b"jj\r".to_vec()));
    client.rendered("Could not stop server");
    assert!(TcpStream::connect(("127.0.0.1", outside_port)).is_ok());
    assert_eq!(client.servers(2).len(), 2);

    // If the source closes while its menu is open, the old target is a no-op.
    client.menu(0);
    client.send(ClientMessage::Command(Command::KillPane));
    let before = client.panes();
    assert!(!before.iter().any(|pane| pane.id == source));
    client.send(ClientMessage::Input(b"jj\r".to_vec()));
    assert_eq!(client.panes(), before);
    assert!(TcpStream::connect(("127.0.0.1", outside_port)).is_ok());

    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(socket_dir).unwrap();
}
