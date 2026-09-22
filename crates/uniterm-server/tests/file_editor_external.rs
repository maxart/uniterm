//! External file opens stay outside terminal tabs and bypass slow Git discovery.

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

    fn rendered(&mut self, text: &str) {
        self.until(|m| matches!(m, ServerMessage::RenderOps(ops) if String::from_utf8_lossy(ops).contains(text)));
    }
}

#[test]
fn first_click_opens_external_editor_without_a_tab_or_waiting_for_git() {
    use std::os::unix::fs::PermissionsExt;
    common::isolate_state();
    let base = common::temp_dir("external-editor");
    let root = base.join("project");
    let bin = base.join("bin");
    std::fs::create_dir_all(&root).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(root.join("a.txt"), "first").unwrap();
    let file = root.join("b '$literal.txt");
    std::fs::write(&file, "second").unwrap();
    let git_started = base.join("git-started");
    let git_gate = base.join("git-gate");
    let editor_gate = base.join("editor-gate");
    let calls = base.join("calls");
    for path in [&git_gate, &editor_gate] {
        assert!(std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap()
            .success());
    }
    let mut git_release = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&git_gate)
        .unwrap();
    let mut editor_release = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&editor_gate)
        .unwrap();
    let quote =
        |path: &std::path::Path| uniterm_server::workflow::shell_quote(&path.to_string_lossy());
    std::fs::write(
        bin.join("git"),
        format!(
            "#!/bin/sh\nprintf started > {}; read answer < {}; exit 1\n",
            quote(&git_started),
            quote(&git_gate)
        ),
    )
    .unwrap();
    std::fs::write(
        bin.join("code"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$@\" >> {}; read answer < {}; exit 7\n",
            quote(&calls),
            quote(&editor_gate)
        ),
    )
    .unwrap();
    for name in ["git", "code"] {
        std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut paths = vec![bin.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
    std::env::set_var("XDG_CONFIG_HOME", base.join("config"));
    let socket_dir = common::socket_root().join(format!("ut-ext-{}", std::process::id()));
    std::fs::create_dir_all(&socket_dir).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &socket_dir);
    let socket = socket_dir.join(format!("{}.sock", common::unique_workspace_name()));
    let server_socket = socket.clone();
    let command = format!("{} 'argument with spaces'", quote(&bin.join("code")));
    let server = std::thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 120, 24).unwrap();
        server.set_config(Config {
            sidebar: false,
            file_sidebar: false,
            editor: command,
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
        name: "ExternalEditor".into(),
        root: root.to_string_lossy().into_owned(),
    });
    client.send(ClientMessage::Command(Command::FileSidebarToggle));
    client.rendered("$literal.txt");
    common::wait_for_socket(&git_started);
    let initial = client.panes();
    // The second row is unselected. One click must open it while Git is
    // still blocked, without creating or focusing a different terminal Pane.
    client.send(ClientMessage::Mouse {
        x: 100,
        y: 6,
        kind: uniterm_proto::MouseKind::Click,
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let expected = format!(
        "{}\nargument with spaces\n{}\n",
        root.display(),
        file.display()
    );
    while std::fs::read_to_string(&calls).ok().as_deref() != Some(expected.as_str()) {
        assert!(
            Instant::now() < deadline,
            "file open waited for Git or a second click"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::read_to_string(&calls).unwrap(),
        format!(
            "{}\nargument with spaces\n{}\n",
            root.display(),
            file.display()
        )
    );
    let opened = client.panes();
    assert_eq!(opened.len(), initial.len());
    assert_eq!(
        opened.iter().find(|p| p.active).unwrap().id,
        initial.iter().find(|p| p.active).unwrap().id
    );
    // The editor itself may stay open indefinitely. Its lifetime cannot hold
    // up the shared runtime, and nonzero exit must surface in Files.
    editor_release.write_all(b"exit\n").unwrap();
    client.rendered("status");
    assert_eq!(client.panes().len(), initial.len());
    git_release.write_all(b"continue\n").unwrap();
    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    std::fs::remove_dir_all(base).unwrap();
    std::fs::remove_dir_all(socket_dir).unwrap();
}
