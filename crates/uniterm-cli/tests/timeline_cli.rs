//! Day history is shared by CLI, the persistent view, and restart recovery.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
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
    fn until(&mut self, expected: impl Fn(&ServerMessage, &Terminal) -> bool) {
        let mut bytes = [0; 32768];
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                if let ServerMessage::RenderOps(ops) = &message {
                    self.screen.feed(ops);
                }
                if expected(&message, &self.screen) {
                    return;
                }
            }
            match self.stream.read(&mut bytes) {
                Ok(0) => panic!("server disconnected"),
                Ok(n) => self.decoder.push(&bytes[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(e) => panic!("{e}"),
            }
        }
        panic!("expected UI not received: {}", self.screen.dump_text());
    }
}

#[test]
fn today_cli_notes_live_view_and_restart_share_one_history() {
    let state = common::isolate_state();
    let runtime = common::socket_root().join(format!("today-cli-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    let workspace = common::unique_workspace_name();
    let socket = uniterm_server::server::default_socket_path(&workspace);
    let start = || {
        let socket = socket.clone();
        std::thread::spawn(move || {
            let (mut server, mut poll) = Server::bind(&socket, "/bin/sh", &[], 140, 30).unwrap();
            server.run(&mut poll).unwrap();
        })
    };
    let server = start();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut client = Client {
        stream,
        decoder: FrameDecoder::new(),
        screen: Terminal::new(140, 30),
    };
    client.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 140,
        rows: 30,
    });
    client.until(|_, screen| screen.dump_text().contains("PROJECTS"));
    let run = |args: &[&str]| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_ut"))
            .args(args)
            .args(["-w", &workspace])
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    run(&["today", "note", "1", "Next: inspect the retry test"]);
    let before: serde_json::Value =
        serde_json::from_slice(&run(&["today", "list", "--json"])).unwrap();
    assert!(before["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["summary"] == "Next: inspect the retry test"));
    // Fill the old Pane, including cells the Today projection leaves blank.
    // A transition must replace that surface rather than diff against an empty Grid.
    client.send(ClientMessage::RenameWindow {
        name: "AGENT_TAB_ONLY".into(),
    });
    client.send(ClientMessage::Input(
        b"i=0; while [ $i -lt 80 ]; do printf 'PANE_ONLY_%s\\n' $i; i=$((i+1)); done\r".to_vec(),
    ));
    client.until(|_, screen| screen.dump_text().contains("PANE_ONLY_79"));
    run(&["today"]);
    client.until(|_, screen| {
        screen.dump_text().contains("TODAY") && screen.dump_text().contains("metadata only")
    });
    assert!(!client.screen.dump_text().contains("PANE_ONLY"));
    assert!(!client.screen.dump_text().contains("AGENT_TAB_ONLY"));
    let rail_row = |y| {
        (0..23)
            .map(|x| client.screen.grid().get(x, y).ch)
            .collect::<String>()
    };
    assert!(rail_row(1).trim().is_empty(), "margin below Workspace");
    assert!(rail_row(2).contains("PROJECTS"));
    assert!(
        (1..30).all(|y| !rail_row(y).contains("Today")),
        "the left rail contains Projects only"
    );
    // Background PTY output is still read, but cannot paint over Today.
    client.send(ClientMessage::PaneSend {
        pane: uniterm_core::PaneId(1),
        bytes: b"printf 'HIDDEN_%s\\n' UPDATE\r".to_vec(),
    });
    client.send(ClientMessage::PaneWaitOutput {
        pane: uniterm_core::PaneId(1),
        needle: "HIDDEN_UPDATE".into(),
        timeout_ms: 3000,
    });
    client.until(|message, _| {
        matches!(
            message,
            ServerMessage::PaneOutputWaited { matched: true, .. }
        )
    });
    client.send(ClientMessage::PaneList);
    client.until(|message, _| matches!(message, ServerMessage::Panes { .. }));
    assert!(!client.screen.dump_text().contains("HIDDEN_UPDATE"));
    // Escape restores the same live Pane and its hidden output; re-entry clears it again.
    client.send(ClientMessage::Input(vec![27]));
    client.until(|_, screen| screen.dump_text().contains("HIDDEN_UPDATE"));
    assert!(client.screen.dump_text().contains("AGENT_TAB_ONLY"));
    client.send(ClientMessage::Command(uniterm_proto::Command::Today));
    client.until(|_, screen| screen.dump_text().contains("TODAY"));
    assert!(!client.screen.dump_text().contains("HIDDEN_UPDATE"));
    assert!(!client.screen.dump_text().contains("AGENT_TAB_ONLY"));
    // Button borders belong to the hit target, not just the label text.
    let title_row = (0..140)
        .map(|x| client.screen.grid().get(x, 2).ch)
        .collect::<String>();
    let main_x = title_row
        .chars()
        .collect::<Vec<_>>()
        .windows(5)
        .position(|chars| chars == ['T', 'O', 'D', 'A', 'Y'])
        .unwrap() as u16;
    let date = before["date"].as_str().unwrap();
    client.send(ClientMessage::Mouse {
        x: main_x + 1,
        y: 4,
        kind: uniterm_proto::MouseKind::Click,
    });
    client.until(|_, screen| !screen.dump_text().contains(date));
    client.send(ClientMessage::Mouse {
        x: main_x + 13,
        y: 4,
        kind: uniterm_proto::MouseKind::Click,
    });
    client.until(|_, screen| screen.dump_text().contains(date));
    // A second client's first attach gets the selected high-level surface.
    let second_stream = UnixStream::connect(&socket).unwrap();
    second_stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut second = Client {
        stream: second_stream,
        decoder: FrameDecoder::new(),
        screen: Terminal::new(140, 30),
    };
    second.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 140,
        rows: 30,
    });
    second.until(|_, screen| screen.dump_text().contains("TODAY"));
    drop(second);
    client.send(ClientMessage::Input(b"n".to_vec()));
    client.send(ClientMessage::Input(b"Remember the second task".to_vec()));
    client.send(ClientMessage::Input(b"\r".to_vec()));
    // Ordered control query is a barrier behind the UI's annotation.
    client.send(ClientMessage::PaneList);
    client.until(|message, _| matches!(message, ServerMessage::Panes { .. }));
    let after: serde_json::Value =
        serde_json::from_slice(&run(&["today", "list", "--json"])).unwrap();
    assert!(after["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["summary"] == "Remember the second task"));
    let note_sequence = after["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["summary"] == "Remember the second task")
        .unwrap()["sequence"]
        .clone();
    // The manager receives a distinct endpoint with enforced read-only scope.
    let manager_socket = socket
        .parent()
        .unwrap()
        .join("manager")
        .join(socket.file_name().unwrap());
    use uniterm_proto::{ControlCommand, ControlFrame, ControlRequest, ControlResult};
    let manager_day = uniterm_client::control_request(
        &manager_socket,
        ControlCommand::TimelineQuery {
            date: "today".into(),
            filter: String::new(),
            before: None,
        },
    )
    .unwrap();
    let Some(ControlResult::Timeline { day }) = manager_day.result else {
        panic!("missing timeline");
    };
    assert!(!day.entries.iter().any(|entry| matches!(
        entry.kind,
        uniterm_core::timeline::TimelineKind::Note | uniterm_core::timeline::TimelineKind::Prompt
    )));
    assert_eq!(day.times.len(), day.entries.len());
    for command in [
        ControlCommand::TimelineNote {
            project: uniterm_core::ProjectId(1),
            pane: None,
            text: "forbidden".into(),
        },
        ControlCommand::PaneRead {
            pane: uniterm_core::PaneId(1),
            lines: 10,
        },
        ControlCommand::TimelineResume { sequence: 1 },
    ] {
        let response = uniterm_client::control_request(&manager_socket, command).unwrap();
        assert_eq!(response.error.unwrap().code, "read_only");
    }
    use std::io::BufRead;
    let mut manager = UnixStream::connect(manager_socket.with_extension("control.sock")).unwrap();
    manager
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(
        manager,
        "{}",
        serde_json::to_string(&ControlRequest {
            version: uniterm_proto::CONTROL_API_VERSION,
            id: 1,
            workspace: workspace.clone(),
            command: ControlCommand::Subscribe { after_sequence: 0 }
        })
        .unwrap()
    )
    .unwrap();
    let mut manager = std::io::BufReader::new(manager);
    let mut line = String::new();
    manager.read_line(&mut line).unwrap();
    let ControlFrame::Response(response) = serde_json::from_str(&line).unwrap() else {
        panic!("missing response");
    };
    let Some(ControlResult::Subscribed {
        current_sequence, ..
    }) = response.result
    else {
        panic!("missing subscription");
    };
    let mut previous = 0;
    while previous < current_sequence {
        line.clear();
        manager.read_line(&mut line).unwrap();
        assert!(!line.contains("Remember the second task") && !line.contains("Next: inspect"));
        let ControlFrame::Event(event) = serde_json::from_str(&line).unwrap() else {
            panic!("missing event: {line}");
        };
        assert_eq!(event.sequence, previous + 1);
        previous = event.sequence;
    }
    drop(manager);
    let stale = uniterm_client::control_request(
        &socket,
        ControlCommand::TimelineFocus {
            pane: uniterm_core::PaneId(1),
            invocation: u64::MAX,
        },
    )
    .unwrap();
    assert!(matches!(
        stale.result,
        Some(ControlResult::Mutation {
            accepted: false,
            ..
        })
    ));

    use std::os::unix::fs::PermissionsExt;
    let provider = runtime.join("fake-manager");
    let capture = runtime.join("manager-endpoint");
    std::fs::write(
        &provider,
        format!(
            "#!/bin/sh\nprintf '%s' \"$UNITERM_SOCKET\" > {}\nexec /bin/cat\n",
            uniterm_server::workflow::shell_quote(&capture.to_string_lossy())
        ),
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    run(&[
        "today",
        "manager",
        provider.to_str().unwrap(),
        "1",
        "--background",
    ]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !capture.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::read_to_string(&capture).unwrap(),
        manager_socket.to_string_lossy()
    );
    client.send(ClientMessage::PaneList);
    client.until(|message, screen| {
        matches!(message, ServerMessage::Panes { .. }) && screen.dump_text().contains("TODAY")
    });

    // Input while browsing cannot reach the remembered shell.
    client.send(ClientMessage::Input(b"q".to_vec()));
    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    let server = start();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let recovered: serde_json::Value =
        serde_json::from_slice(&run(&["today", "list", "--json"])).unwrap();
    assert!(recovered["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["sequence"] == note_sequence && e["summary"] == "Remember the second task"));
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();
}
