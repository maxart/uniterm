//! File opens reuse live terminal editor invocations through the real client path.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use uniterm_core::Config;
use uniterm_proto::{encode_frame, ClientMessage, Command, FrameDecoder, PaneInfo, ServerMessage};
use uniterm_server::Server;

mod common;

struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
}

impl Client {
    fn send(&mut self, message: ClientMessage) {
        self.stream.write_all(&encode_frame(&message)).unwrap();
    }

    fn until(&mut self, predicate: impl Fn(&ServerMessage) -> bool) -> ServerMessage {
        let deadline = Instant::now() + Duration::from_secs(5);
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

    fn wait_panes(&mut self, predicate: impl Fn(&[PaneInfo]) -> bool) -> Vec<PaneInfo> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let panes = self.panes();
            if predicate(&panes) {
                return panes;
            }
            assert!(
                Instant::now() < deadline,
                "Pane state did not converge: {panes:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn rendered(&mut self, text: &str) {
        self.until(|m| matches!(m, ServerMessage::RenderOps(ops) if String::from_utf8_lossy(ops).contains(text)));
    }

    fn open_selected(&mut self) {
        // Hide/reveal the Files dock to give it keyboard focus, retaining selection.
        self.send(ClientMessage::Command(Command::FileSidebarToggle));
        self.send(ClientMessage::Command(Command::FileSidebarToggle));
        self.send(ClientMessage::Input(b"\r".to_vec()));
    }
}

#[test]
fn file_opens_reuse_live_editors_and_reopen_after_exit_or_close() {
    common::isolate_state();
    let root = common::temp_dir("editor-reuse");
    std::fs::write(root.join("a.txt"), "file").unwrap();
    std::os::unix::fs::symlink(root.join("a.txt"), root.join("b.txt")).unwrap();
    std::fs::write(root.join("c.txt"), "another file").unwrap();
    let socket_dir = common::socket_root().join(format!("ut-editor-{}", std::process::id()));
    std::fs::create_dir_all(&socket_dir).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", socket_dir.join("config"));
    std::env::set_var("XDG_RUNTIME_DIR", &socket_dir);
    let socket = socket_dir.join(format!("{}.sock", common::unique_workspace_name()));
    let server_socket = socket.clone();
    let server = std::thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 120, 24).unwrap();
        server.set_config(Config {
            sidebar: false,
            file_sidebar: true,
            confirm_close: false,
            editor: "sh -c 'printf EDITOR_READY; read answer; printf EDITOR_DONE'".into(),
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
        cols: 120,
        rows: 24,
    });
    client.send(ClientMessage::ProjectCreate {
        name: "EditorProject".into(),
        root: root.to_string_lossy().into_owned(),
    });
    client.send(ClientMessage::Command(Command::FileSidebarToggle));
    client.rendered("a.txt");
    let initial = client.panes();
    let shell = initial.iter().find(|pane| pane.active).unwrap().id;

    // First open, including two requests queued before resolution completes.
    client.send(ClientMessage::Input(b"\r".to_vec()));
    client.send(ClientMessage::Input(b"\r".to_vec()));
    client.rendered("EDITOR_READY");
    let opened = client.panes();
    assert_eq!(opened.len(), initial.len() + 1);
    let editor = opened.iter().find(|pane| pane.active).unwrap().id;

    client.send(ClientMessage::PaneFocus { pane: shell });
    client.until(|m| matches!(m, ServerMessage::PaneFocused { .. }));
    client.open_selected();
    // Resolution refocuses the existing editor and repaints its contents.
    client.rendered("EDITOR_READY");
    let reused =
        client.wait_panes(|panes| panes.iter().any(|pane| pane.id == editor && pane.active));
    assert_eq!(reused.len(), opened.len());
    assert_eq!(reused.iter().find(|pane| pane.active).unwrap().id, editor);

    // Reuse targets the editor Pane even when another split is focused/zoomed.
    client.send(ClientMessage::Command(Command::Split(
        uniterm_proto::SplitAxis::LeftRight,
    )));
    let split = client.panes().iter().find(|pane| pane.active).unwrap().id;
    client.send(ClientMessage::Command(Command::ZoomToggle));
    client.open_selected();
    let split_reuse =
        client.wait_panes(|panes| panes.iter().any(|pane| pane.id == editor && pane.active));
    assert_eq!(split_reuse.len(), opened.len() + 1);
    client.send(ClientMessage::PaneFocus { pane: split });
    client.until(|m| matches!(m, ServerMessage::PaneFocused { .. }));
    client.send(ClientMessage::Command(Command::KillPane));
    assert_eq!(client.panes().len(), opened.len());

    // The symlink is the same file; the next file has a distinct identity.
    for (expected_count, expected_same) in [(opened.len(), true), (opened.len() + 1, false)] {
        client.send(ClientMessage::PaneFocus { pane: shell });
        client.until(|m| matches!(m, ServerMessage::PaneFocused { .. }));
        client.send(ClientMessage::Command(Command::FileSidebarToggle));
        client.send(ClientMessage::Command(Command::FileSidebarToggle));
        client.send(ClientMessage::Input(b"j".to_vec()));
        client.send(ClientMessage::Input(b"\r".to_vec()));
        client.rendered("EDITOR_READY");
        let panes = client.wait_panes(|panes| {
            panes.len() == expected_count
                && panes.iter().any(|pane| {
                    pane.active
                        && if expected_same {
                            pane.id == editor
                        } else {
                            pane.id != shell && pane.id != editor
                        }
                })
        });
        assert_eq!(panes.len(), expected_count);
        assert_eq!(
            panes.iter().find(|pane| pane.active).unwrap().id == editor,
            expected_same
        );
    }

    client.send(ClientMessage::Input(b"done\r".to_vec()));
    client.rendered("EDITOR_DONE");
    client.open_selected();
    client.rendered("EDITOR_READY");
    assert_eq!(
        client
            .wait_panes(|panes| panes.len() == opened.len() + 2)
            .len(),
        opened.len() + 2
    );

    client.send(ClientMessage::Command(Command::KillPane));
    let remaining = client.panes().len();
    client.open_selected();
    client.rendered("EDITOR_READY");
    assert_eq!(
        client
            .wait_panes(|panes| panes.len() == remaining + 1)
            .len(),
        remaining + 1
    );

    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(socket_dir).unwrap();
}
