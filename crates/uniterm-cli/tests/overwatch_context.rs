//! Read-only context paging and independent-client Overwatch controls.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use uniterm_core::overwatch::{Operation, State};
use uniterm_proto::{
    encode_frame, ClientMessage, Command, ControlCommand, ControlResult, FrameDecoder,
    ServerMessage,
};
mod common;

struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
}
impl Client {
    fn connect(socket: &std::path::Path) -> Self {
        let stream = UnixStream::connect(socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut client = Self {
            stream,
            decoder: FrameDecoder::new(),
        };
        client.send(ClientMessage::Attach {
            term: "xterm".into(),
            cols: 100,
            rows: 30,
        });
        client.barrier();
        client
    }
    fn send(&mut self, message: ClientMessage) {
        self.stream.write_all(&encode_frame(&message)).unwrap();
    }
    fn receive(&mut self, mut done: impl FnMut(&ServerMessage) -> bool) -> Vec<ServerMessage> {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut messages = vec![];
        let mut buffer = [0; 32768];
        while Instant::now() < deadline {
            while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                let finished = done(&message);
                messages.push(message);
                if finished {
                    return messages;
                }
            }
            match self.stream.read(&mut buffer) {
                Ok(0) => panic!("server closed"),
                Ok(n) => self.decoder.push(&buffer[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(e) => panic!("read: {e}"),
            }
        }
        panic!("expected message did not arrive");
    }
    fn barrier(&mut self) -> Vec<ServerMessage> {
        self.send(ClientMessage::PaneList);
        self.receive(|message| matches!(message, ServerMessage::Panes { .. }))
    }
}

#[test]
fn context_pages_exclude_private_notes_and_menus_open_only_for_requester() {
    common::isolate_state();
    let root = common::socket_root().join(format!("ow-context-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let socket = root.join(format!("{}.sock", common::unique_workspace_name()));
    let server_socket = socket.clone();
    let server = std::thread::spawn(move || {
        let (mut server, mut poll) =
            uniterm_server::Server::bind(&server_socket, "/bin/sh", &[], 100, 30).unwrap();
        server.set_config(uniterm_core::Config {
            auto_install_connectors: false,
            auto_update_connectors: false,
            ..Default::default()
        });
        server.run(&mut poll).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let call = |command| uniterm_client::control_request(&socket, command).unwrap();
    let mut first = Client::connect(&socket);
    let mut second = Client::connect(&socket);
    second.send(ClientMessage::Settings);
    let initial =
        second.receive(|message| matches!(message, ServerMessage::Overwatch { open: false, .. }));
    assert!(initial
        .iter()
        .any(|message| matches!(message, ServerMessage::Settings { .. })));
    assert!(initial.iter().any(|message| matches!(message, ServerMessage::Overwatch { open: false, state, .. } if state == &State::default())));
    first.send(ClientMessage::Command(Command::Overwatch));
    first.receive(|message| matches!(message, ServerMessage::Overwatch { open: true, .. }));
    assert!(!second
        .barrier()
        .iter()
        .any(|message| matches!(message, ServerMessage::Overwatch { open: true, .. })));
    first.send(ClientMessage::Overwatch {
        operation: Operation::Enable,
    });
    first.receive(|message| matches!(message, ServerMessage::Overwatch { open: true, state, .. } if state.message.contains("Choose a provider")));
    let status = call(ControlCommand::Overwatch {
        operation: Operation::Status,
    });
    assert!(
        matches!(status.result, Some(ControlResult::Overwatch { state }) if state == State::default()),
        "an error notice must not mutate durable state"
    );
    first.send(ClientMessage::Overwatch {
        operation: Operation::Configure {
            provider: "codex".into(),
            model: None,
            project: None,
            manage: false,
        },
    });
    first.receive(|message| matches!(message, ServerMessage::Overwatch { open: true, state, .. } if state.provider == "codex"));
    let updates = second.receive(|message| matches!(message, ServerMessage::Overwatch { open: false, state, .. } if state.provider == "codex"));
    assert!(!updates
        .iter()
        .any(|message| matches!(message, ServerMessage::Overwatch { open: true, .. })));
    for index in 0..70 {
        assert!(call(ControlCommand::TaskCreate {
            title: format!("Task {index}")
        })
        .error
        .is_none());
    }
    assert!(call(ControlCommand::TaskNotes {
        id: 1,
        notes: "PRIVATE-NOTE-NEVER-IN-METADATA".into()
    })
    .error
    .is_none());
    let manager = socket
        .parent()
        .unwrap()
        .join("manager")
        .join(socket.file_name().unwrap());
    let mut offset = 0;
    let mut expected_sequence = None;
    let mut facts = vec![];
    loop {
        let response = uniterm_client::control_request(
            &manager,
            ControlCommand::OverwatchContextPage {
                offset,
                limit: 7,
                expected_sequence,
            },
        )
        .unwrap();
        assert!(response.error.is_none(), "{:?}", response.error);
        let Some(ControlResult::OverwatchContext {
            records,
            sequence,
            next_offset,
            total,
            private_content_excluded,
            ..
        }) = response.result
        else {
            panic!("wrong result")
        };
        assert!(private_content_excluded);
        assert!(records.len() <= 7);
        assert!(total >= 73);
        if let Some(previous) = expected_sequence {
            assert_eq!(sequence, previous);
        }
        expected_sequence = Some(sequence);
        facts.extend(records);
        let Some(next) = next_offset else {
            break;
        };
        assert!(next > offset);
        offset = next;
    }
    let text = serde_json::to_string(&facts).unwrap();
    assert!(!text.contains("PRIVATE-NOTE"));
    assert_eq!(
        facts
            .iter()
            .filter(|fact| matches!(fact, uniterm_proto::overwatch::Fact::Task { .. }))
            .count(),
        70
    );
    assert!(call(ControlCommand::TaskCreate {
        title: "invalidate cursor".into()
    })
    .error
    .is_none());
    let stale = uniterm_client::control_request(
        &manager,
        ControlCommand::OverwatchContextPage {
            offset: 7,
            limit: 7,
            expected_sequence,
        },
    )
    .unwrap();
    assert_eq!(stale.error.unwrap().code, "stale_context");
    uniterm_client::kill_server(&socket).unwrap();
    drop(first);
    drop(second);
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(root);
}
