//! Tab drag, keyboard ordering, and durable history share one semantic move.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use uniterm_proto::{
    encode_frame, ClientMessage, Command, FrameDecoder, MouseKind, PaneInfo, ServerMessage,
    TabMoveDirection,
};
use uniterm_server::{Server, Terminal};

mod common;

struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
    screen: Terminal,
}

impl Client {
    fn send(&mut self, msg: ClientMessage) {
        self.stream.write_all(&encode_frame(&msg)).unwrap();
    }

    fn barrier(&mut self) -> Vec<PaneInfo> {
        self.send(ClientMessage::PaneList);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut bytes = [0; 32768];
        while Instant::now() < deadline {
            while let Some(msg) = self.decoder.decode::<ServerMessage>().unwrap() {
                match msg {
                    ServerMessage::RenderOps(ops) => {
                        self.screen.feed(&ops);
                    }
                    ServerMessage::Panes { panes, .. } => return panes,
                    _ => {}
                }
            }
            match self.stream.read(&mut bytes) {
                Ok(0) => panic!("server disconnected"),
                Ok(n) => self.decoder.push(&bytes[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("{e}"),
            }
        }
        panic!("server did not answer");
    }

    fn tab_x(&self, label: &str) -> u16 {
        let row: String = (0..160).map(|x| self.screen.grid().get(x, 23).ch).collect();
        (row.find(label)
            .unwrap_or_else(|| panic!("{label} absent: {row}"))
            + 1) as u16
    }

    fn mouse(&mut self, x: u16, y: u16, kind: MouseKind) {
        self.send(ClientMessage::Mouse { x, y, kind });
    }
}

#[test]
fn drag_and_keyboard_preserve_identity_order_and_durable_projection() {
    common::isolate_state();
    let workspace = common::unique_workspace_name();
    let runtime = common::socket_root().join(format!("tab-reorder-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    let socket = runtime.join(format!("{workspace}.sock"));
    let server_socket = socket.clone();
    let server = std::thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 160, 24).unwrap();
        server.set_config(uniterm_core::Config {
            sidebar: false,
            file_sidebar: false,
            status_position: uniterm_core::StatusPosition::Bottom,
            ..Default::default()
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
        screen: Terminal::new(160, 24),
    };
    client.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 160,
        rows: 24,
    });
    let original = client.barrier()[0].id;
    client.send(ClientMessage::Command(Command::MoveTab(
        TabMoveDirection::Next,
    )));
    assert_eq!(client.barrier()[0].id, original, "one Tab is a no-op");
    for (i, name) in ["alpha", "beta", "gamma"].into_iter().enumerate() {
        if i > 0 {
            client.send(ClientMessage::Command(Command::NewWindow));
        }
        client.send(ClientMessage::RenameWindow { name: name.into() });
    }
    let before = client.barrier();
    let first = client.tab_x("1:alpha");
    let last = client.tab_x("3:gamma");
    client.mouse(first, 24, MouseKind::Click);
    client.mouse(last, 24, MouseKind::Drag);
    client.mouse(last, 24, MouseKind::Release);
    let after = client.barrier();
    for (pane, ordinal) in before.iter().zip([3, 1, 2]) {
        assert_eq!(after.iter().find(|p| p.id == pane.id).unwrap().tab, ordinal);
    }
    assert_eq!(after.iter().find(|p| p.active).unwrap().id, original);
    // Clicking/releasing in place and canceling outside the bar cannot reorder.
    let last = client.tab_x("3:alpha");
    client.mouse(last, 24, MouseKind::Click);
    client.mouse(last, 24, MouseKind::Release);
    client.mouse(last, 24, MouseKind::Click);
    client.mouse(1, 2, MouseKind::Release);
    assert_eq!(
        client
            .barrier()
            .iter()
            .find(|p| p.id == original)
            .unwrap()
            .tab,
        3
    );
    // Wrap is a move, retaining the relative order of the other Tabs.
    client.send(ClientMessage::Command(Command::MoveTab(
        TabMoveDirection::Next,
    )));
    let restored_order = client.barrier();
    for (pane, ordinal) in before.iter().zip([1, 2, 3]) {
        assert_eq!(
            restored_order.iter().find(|p| p.id == pane.id).unwrap().tab,
            ordinal
        );
    }
    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    let mut names = Vec::new();
    uniterm_server::eventlog::visit_after(&workspace, 0, |envelope| {
        if let uniterm_server::eventlog::LogEvent::WorkspaceProjected { state } = envelope.event {
            names = state.windows.into_iter().map(|w| w.name).collect();
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(
        names,
        [
            Some("alpha".into()),
            Some("beta".into()),
            Some("gamma".into())
        ]
    );
}
