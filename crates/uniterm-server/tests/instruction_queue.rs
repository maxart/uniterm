//! End-to-end contract for queued human direction and cooperative delivery.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use uniterm_core::{AgentStatus, InstructionAuthor, PaneId};
use uniterm_proto::{
    encode_frame, ClientMessage, ControlCommand, ControlFrame, ControlRequest, ControlResult,
    FrameDecoder, ServerMessage, CONTROL_API_VERSION,
};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_socket() -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "uniterm-instruction-{}-{nonce}",
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

fn control_send(writer: &mut UnixStream, workspace: &str, id: u64, command: ControlCommand) {
    serde_json::to_writer(
        &mut *writer,
        &ControlRequest {
            version: CONTROL_API_VERSION,
            id,
            workspace: workspace.into(),
            command,
        },
    )
    .unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();
}

fn control_read(reader: &mut BufReader<UnixStream>) -> ControlFrame {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

struct Wire {
    stream: UnixStream,
    decoder: FrameDecoder,
}

impl Wire {
    fn connect(path: &std::path::Path) -> Self {
        let stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
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
            assert!(Instant::now() < deadline, "timed out waiting for frame");
            match self.stream.read(&mut buf) {
                Ok(0) => panic!("server closed the instruction connection"),
                Ok(read) => self.decoder.push(&buf[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("instruction connection read failed: {error}"),
            }
        }
    }

    fn instructions(&mut self) -> Vec<uniterm_proto::InstructionEntry> {
        self.send(ClientMessage::InstructionList);
        match self.next_until(Duration::from_secs(2), |message| {
            matches!(message, ServerMessage::Instructions { .. })
        }) {
            ServerMessage::Instructions { items } => items,
            message => panic!("unexpected instruction response: {message:?}"),
        }
    }

    fn add(&mut self, text: &str) -> u64 {
        self.send(ClientMessage::InstructionAdd {
            pane: PaneId(1),
            author: InstructionAuthor::Cli,
            text: text.into(),
        });
        match self.next_until(Duration::from_secs(2), |message| {
            matches!(message, ServerMessage::InstructionChanged { .. })
        }) {
            ServerMessage::InstructionChanged {
                id,
                found: true,
                accepted: true,
                ..
            } => id,
            message => panic!("instruction add was rejected: {message:?}"),
        }
    }

    fn pane_output(&mut self) -> String {
        self.send(ClientMessage::PaneRead {
            pane: PaneId(1),
            lines: 100,
        });
        match self.next_until(Duration::from_secs(2), |message| {
            matches!(message, ServerMessage::PaneOutput { .. })
        }) {
            ServerMessage::PaneOutput {
                found: true, text, ..
            } => text,
            message => panic!("unexpected Pane output response: {message:?}"),
        }
    }
}

#[test]
fn instructions_wait_for_cooperative_ready_and_stay_invocation_scoped() {
    isolate_state();
    let socket = temp_socket();
    let workspace = socket.file_stem().unwrap().to_str().unwrap().to_string();
    let server_socket = socket.clone();
    let script = "printf '\\033]777;notify;uniterm://cli-agent;{\"agent\":\"codex\",\"event\":\"session_start\"}\\007'; \
                  while IFS= read -r line; do \
                    case \"$line\" in \
                      READY) printf '\\033]777;notify;uniterm://cli-agent;{\"agent\":\"codex\",\"event\":\"idle\"}\\007' ;; \
                      END) printf '\\033]777;notify;uniterm://cli-agent;{\"agent\":\"codex\",\"event\":\"session_end\"}\\007' ;; \
                      *) printf 'ECHO:%s\\n' \"$line\" ;; \
                    esac; \
                  done";
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", script], 80, 24).unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&socket);
    let control = socket.with_extension("control.sock");
    wait_for(&control);
    let mut wire = Wire::connect(&socket);
    wire.send(ClientMessage::AgentWait {
        pane: PaneId(1),
        status: AgentStatus::Starting,
        timeout_ms: 2_000,
    });
    assert!(matches!(
        wire.next_until(Duration::from_secs(3), |message| matches!(
            message,
            ServerMessage::AgentWaited { .. }
        )),
        ServerMessage::AgentWaited {
            found: true,
            matched: true,
            ..
        }
    ));

    let mut control_writer = UnixStream::connect(&control).unwrap();
    control_writer
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut control_reader = BufReader::new(control_writer.try_clone().unwrap());
    control_send(
        &mut control_writer,
        &workspace,
        1,
        ControlCommand::InstructionAdd {
            pane: PaneId(1),
            text: "from control".into(),
        },
    );
    let control_id = match control_read(&mut control_reader) {
        ControlFrame::Response(response) => match response.result.unwrap() {
            ControlResult::InstructionChanged {
                id,
                found: true,
                accepted: true,
                ref items,
            } if items.len() == 1
                && items[0].author == InstructionAuthor::ControlApi
                && items[0].text == "from control" =>
            {
                id
            }
            result => panic!("unexpected control add result: {result:?}"),
        },
        frame => panic!("unexpected control add frame: {frame:?}"),
    };
    assert_eq!(wire.instructions()[0].id, control_id);
    control_send(
        &mut control_writer,
        &workspace,
        2,
        ControlCommand::InstructionCancel { id: control_id },
    );
    assert!(matches!(
        control_read(&mut control_reader),
        ControlFrame::Response(response)
            if matches!(response.result, Some(ControlResult::InstructionChanged {
                found: true,
                accepted: true,
                ref items,
                ..
            }) if items.is_empty())
    ));

    let first = wire.add("first direction");
    let second = wire.add("second direction");
    assert_ne!(first, second);

    // The startup output settles into heuristic Grid idle after two seconds.
    // That status must never inject queued human direction.
    thread::sleep(Duration::from_millis(2_300));
    assert_eq!(wire.instructions().len(), 2);
    assert!(!wire.pane_output().contains("ECHO:first direction"));

    wire.send(ClientMessage::PaneSend {
        pane: PaneId(1),
        bytes: b"READY\n".to_vec(),
    });
    wire.next_until(Duration::from_secs(2), |message| {
        matches!(message, ServerMessage::PaneSent { accepted: true, .. })
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let output = wire.pane_output();
        if output.contains("ECHO:first direction") {
            assert!(!output.contains("ECHO:second direction"));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ready instruction was not delivered"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let queued = wire.instructions();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].id, second);

    wire.send(ClientMessage::InstructionSendNow { id: second });
    assert!(matches!(
        wire.next_until(Duration::from_secs(2), |message| matches!(
            message,
            ServerMessage::InstructionChanged { .. }
        )),
        ServerMessage::InstructionChanged {
            id,
            found: true,
            accepted: true,
            ref items,
        } if id == second && items.is_empty()
    ));

    let old = wire.add("obsolete direction");
    wire.send(ClientMessage::InstructionReplace {
        id: old,
        author: InstructionAuthor::Cli,
        text: "replacement direction".into(),
    });
    let replacement = match wire.next_until(Duration::from_secs(2), |message| {
        matches!(message, ServerMessage::InstructionChanged { .. })
    }) {
        ServerMessage::InstructionChanged {
            id,
            found: true,
            accepted: true,
            ref items,
        } if items.len() == 1 && items[0].id == id && items[0].text == "replacement direction" => {
            id
        }
        message => panic!("unexpected replacement response: {message:?}"),
    };
    assert_ne!(old, replacement);
    wire.send(ClientMessage::InstructionCancel { id: replacement });
    assert!(matches!(
        wire.next_until(Duration::from_secs(2), |message| matches!(
            message,
            ServerMessage::InstructionChanged { .. }
        )),
        ServerMessage::InstructionChanged {
            found: true,
            accepted: true,
            ref items,
            ..
        } if items.is_empty()
    ));

    wire.add("must not reach another invocation");
    wire.send(ClientMessage::PaneSend {
        pane: PaneId(1),
        bytes: b"END\n".to_vec(),
    });
    wire.next_until(Duration::from_secs(2), |message| {
        matches!(message, ServerMessage::PaneSent { accepted: true, .. })
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if wire.instructions().is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ended invocation retained queued direction"
        );
        thread::sleep(Duration::from_millis(20));
    }

    wire.send(ClientMessage::KillServer);
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(socket.parent().unwrap());
}
