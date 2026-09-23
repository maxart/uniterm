//! Reconstruct the actual client screen after each operation and after heavy
//! bottom-row output. Initial attach bytes cannot satisfy later assertions.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, Command, FrameDecoder, ServerMessage, SplitAxis};
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

    fn receive_until(&mut self, mut done: impl FnMut(&ServerMessage, &Terminal) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut buffer = [0; 32768];
        while Instant::now() < deadline {
            while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                if let ServerMessage::RenderOps(ops) = &message {
                    self.screen.feed(ops);
                }
                if done(&message, &self.screen) {
                    return;
                }
            }
            match self.stream.read(&mut buffer) {
                Ok(0) => panic!("server disconnected"),
                Ok(read) => self.decoder.push(&buffer[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("read failed: {error}"),
            }
        }
        panic!(
            "expected post-operation output did not arrive: {}",
            self.screen.dump_text()
        );
    }

    fn barrier(&mut self, panes: usize) {
        self.send(ClientMessage::PaneList);
        self.receive_until(|message, _| {
            if let ServerMessage::Panes { panes: actual, .. } = message {
                assert_eq!(actual.len(), panes, "structural operation was not applied");
                true
            } else {
                false
            }
        });
    }

    fn assert_chrome(&self, workspace: &str, vertical: bool, horizontal: bool) {
        let grid = self.screen.grid();
        let status: String = (0..grid.width())
            .map(|x| grid.get(x, grid.height() - 1).ch)
            .collect();
        assert!(
            status.contains(workspace),
            "status row lost Workspace: {status:?}"
        );
        assert!(status.contains('▾'), "Workspace control disappeared");
        if vertical {
            let x = grid.width() / 2;
            for y in 0..grid.height() - 1 {
                assert_eq!(grid.get(x, y).ch, '│', "divider lost at ({x},{y})");
            }
        } else {
            for y in 0..grid.height() - 1 {
                assert_ne!(grid.get(grid.width() / 2, y).ch, '│', "stale divider");
            }
        }
        if horizontal {
            let y = (grid.height() - 2) / 2;
            for x in grid.width() / 2 + 1..grid.width() {
                assert_eq!(
                    grid.get(x, y).ch,
                    '─',
                    "horizontal divider lost at ({x},{y})"
                );
            }
        }
    }

    fn burst(&mut self, step: usize, workspace: &str, vertical: bool, horizontal: bool) {
        self.send(ClientMessage::Input(format!(
            "i=0; while [ $i -lt 2000 ]; do printf 'bottom-output\\n'; i=$((i+1)); done; printf '\\033[999;1H\\033[2K\\104\\117\\116\\105-step{step}'\n"
        ).into_bytes()));
        let marker = format!("DONE-step{step}");
        self.receive_until(|_, screen| screen.dump_text().contains(&marker));
        self.assert_chrome(workspace, vertical, horizontal);
    }
}

#[test]
fn status_line_survives_pane_ops() {
    common::isolate_state();
    let dir = common::socket_root().join(format!("ut-r2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = common::unique_workspace_name();
    let socket = dir.join(format!("{workspace}.sock"));
    let server_socket = socket.clone();
    // Each input causes scrolling and a repaint at the Pane's bottom edge.
    // Octal escapes keep the completion marker out of echoed command text.
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 64, 24).unwrap();
        server.set_config(uniterm_core::Config {
            status_position: uniterm_core::StatusPosition::Bottom,
            ..uniterm_core::Config::default()
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
        screen: Terminal::new(64, 24),
    };
    client.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 64,
        rows: 24,
    });
    client.barrier(1);
    client.assert_chrome(&workspace, false, false);
    client.burst(0, &workspace, false, false);

    let steps = [
        (
            Command::Focus(uniterm_proto::FocusDir::Left),
            1,
            false,
            false,
        ), // no-op
        (Command::Split(SplitAxis::LeftRight), 2, true, false),
        (Command::Split(SplitAxis::TopBottom), 3, true, true),
        (Command::KillPane, 2, true, false),
        (Command::ZoomToggle, 2, false, false),
        (Command::ZoomToggle, 2, true, false),
        (Command::NewWindow, 3, false, false),
        (Command::SelectWindow(0), 3, true, false),
        (
            Command::Focus(uniterm_proto::FocusDir::Left),
            3,
            true,
            false,
        ),
    ];
    for (step, (command, panes, vertical, horizontal)) in steps.into_iter().enumerate() {
        client.send(ClientMessage::Command(command));
        client.barrier(panes);
        client.assert_chrome(&workspace, vertical, horizontal);
        client.burst(step + 1, &workspace, vertical, horizontal);
    }
    client.screen.resize(70, 20);
    client.send(ClientMessage::Resize { cols: 70, rows: 20 });
    client.barrier(3);
    client.assert_chrome(&workspace, true, false);
    client.burst(10, &workspace, true, false);

    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
