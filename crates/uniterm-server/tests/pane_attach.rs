//! Direct Pane attachment contract: isolated rendering and explicit input ownership.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use uniterm_core::PaneId;
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, PaneAttachRole, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_socket() -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "uniterm-pane-attach-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{}.sock", unique_workspace_name()))
}

fn wait_for(path: &std::path::Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("socket never appeared at {}", path.display());
}

struct Wire {
    stream: UnixStream,
    decoder: FrameDecoder,
}

impl Wire {
    fn connect(path: &std::path::Path) -> Self {
        let stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        Self {
            stream,
            decoder: FrameDecoder::new(),
        }
    }

    fn send(&mut self, message: ClientMessage) {
        self.stream.write_all(&encode_frame(&message)).unwrap();
    }

    fn next_until(
        &mut self,
        timeout: Duration,
        mut matches: impl FnMut(&ServerMessage) -> bool,
    ) -> ServerMessage {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 16 * 1024];
        loop {
            while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                if matches(&message) {
                    return message;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for server frame"
            );
            match self.stream.read(&mut buf) {
                Ok(0) => panic!("server closed the direct attachment"),
                Ok(read) => self.decoder.push(&buf[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("direct attachment read failed: {error}"),
            }
        }
    }
}

fn pane_output(socket: &std::path::Path) -> String {
    let mut query = Wire::connect(socket);
    query.send(ClientMessage::PaneRead {
        pane: PaneId(1),
        lines: 100,
    });
    match query.next_until(Duration::from_secs(2), |message| {
        matches!(message, ServerMessage::PaneOutput { .. })
    }) {
        ServerMessage::PaneOutput {
            found: true, text, ..
        } => text,
        message => panic!("unexpected Pane output response: {message:?}"),
    }
}

#[test]
fn direct_attach_streams_one_pane_and_enforces_controller_takeover() {
    isolate_state();
    let socket = temp_socket();
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(
            &server_socket,
            "/bin/sh",
            &[
                "-c",
                "while IFS= read -r line; do printf 'ECHO:%s\\n' \"$line\"; done",
            ],
            80,
            24,
        )
        .unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&socket);

    let mut observer = Wire::connect(&socket);
    observer.send(ClientMessage::PaneAttach {
        pane: PaneId(1),
        role: PaneAttachRole::Observer,
    });
    let (cols, rows) = match observer.next_until(Duration::from_secs(2), |message| {
        matches!(message, ServerMessage::PaneAttached { .. })
    }) {
        ServerMessage::PaneAttached {
            pane: PaneId(1),
            role: PaneAttachRole::Observer,
            cols,
            rows,
        } => (cols, rows),
        message => panic!("unexpected observer acknowledgement: {message:?}"),
    };
    assert!(cols > 0 && rows > 0);
    assert!(matches!(
        observer.next_until(Duration::from_secs(2), |message| {
            matches!(message, ServerMessage::RenderOps(_))
        }),
        ServerMessage::RenderOps(ops) if ops.starts_with(b"\x1b[2J")
    ));

    let mut controller = Wire::connect(&socket);
    controller.send(ClientMessage::PaneAttach {
        pane: PaneId(1),
        role: PaneAttachRole::Controller,
    });
    assert!(matches!(
        controller.next_until(Duration::from_secs(2), |message| {
            matches!(message, ServerMessage::PaneAttached { .. })
        }),
        ServerMessage::PaneAttached {
            role: PaneAttachRole::Controller,
            ..
        }
    ));

    let mut rival = Wire::connect(&socket);
    rival.send(ClientMessage::PaneAttach {
        pane: PaneId(1),
        role: PaneAttachRole::Controller,
    });
    rival.send(ClientMessage::Input(b"rejected-race\n".to_vec()));
    assert!(matches!(
        rival.next_until(Duration::from_secs(2), |message| {
            matches!(message, ServerMessage::PaneAttachRejected { .. })
        }),
        ServerMessage::PaneAttachRejected { .. }
    ));

    observer.send(ClientMessage::Input(b"observer\n".to_vec()));
    controller.send(ClientMessage::Input(b"controller\n".to_vec()));
    controller.next_until(Duration::from_secs(2), |message| {
        matches!(message, ServerMessage::RenderOps(ops) if ops.windows(b"ECHO:controller".len()).any(|window| window == b"ECHO:controller"))
    });
    let output = pane_output(&socket);
    assert!(output.contains("ECHO:controller"));
    assert!(!output.contains("ECHO:observer"));
    assert!(!output.contains("ECHO:rejected-race"));

    let mut takeover = Wire::connect(&socket);
    takeover.send(ClientMessage::PaneAttach {
        pane: PaneId(1),
        role: PaneAttachRole::Takeover,
    });
    assert!(matches!(
        takeover.next_until(Duration::from_secs(2), |message| {
            matches!(message, ServerMessage::PaneAttached { .. })
        }),
        ServerMessage::PaneAttached {
            role: PaneAttachRole::Takeover,
            ..
        }
    ));
    assert!(matches!(
        controller.next_until(Duration::from_secs(2), |message| {
            matches!(message, ServerMessage::PaneAttachRevoked { .. })
        }),
        ServerMessage::PaneAttachRevoked { .. }
    ));

    controller.send(ClientMessage::Input(b"revoked\n".to_vec()));
    takeover.send(ClientMessage::Resize { cols: 10, rows: 5 });
    takeover.send(ClientMessage::Input(b"takeover\n".to_vec()));
    takeover.next_until(Duration::from_secs(2), |message| {
        matches!(message, ServerMessage::RenderOps(ops) if ops.windows(b"ECHO:takeover".len()).any(|window| window == b"ECHO:takeover"))
    });
    let output = pane_output(&socket);
    assert!(output.contains("ECHO:takeover"));
    assert!(!output.contains("ECHO:revoked"));

    let mut after_resize = Wire::connect(&socket);
    after_resize.send(ClientMessage::PaneAttach {
        pane: PaneId(1),
        role: PaneAttachRole::Observer,
    });
    assert!(matches!(
        after_resize.next_until(Duration::from_secs(2), |message| {
            matches!(message, ServerMessage::PaneAttached { .. })
        }),
        ServerMessage::PaneAttached {
            cols: current_cols,
            rows: current_rows,
            ..
        } if current_cols == cols && current_rows == rows
    ));

    let mut stop = Wire::connect(&socket);
    stop.send(ClientMessage::KillServer);
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(socket.parent().unwrap());
}
