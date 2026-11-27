//! Run Graph opt-in, first attach, surface transitions and idle silence.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};
use uniterm_core::Config;
use uniterm_proto::{
    encode_frame, ClientMessage, Command, FrameDecoder, MouseKind, ServerMessage, SettingsPatch,
};
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
    #[track_caller]
    fn until(&mut self, mut done: impl FnMut(&ServerMessage, &Terminal) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
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
        panic!("expected frame not seen: {}", self.screen.dump_text());
    }
    fn assert_quiet(&mut self) {
        self.barrier();
        let deadline = Instant::now() + Duration::from_millis(300);
        let mut bytes = [0; 32768];
        while Instant::now() < deadline {
            while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                assert!(
                    !matches!(message, ServerMessage::RenderOps(_)),
                    "unchanged Graph emitted a frame"
                );
            }
            match self.stream.read(&mut bytes) {
                Ok(0) => panic!("closed during idle"),
                Ok(n) => self.decoder.push(&bytes[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(e) => panic!("idle read: {e}"),
            }
        }
    }

    fn click_tab(&mut self, label: &str) {
        let text = self.screen.dump_text();
        let (y, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(label))
            .unwrap();
        let x = line.find(label).unwrap();
        self.send(ClientMessage::Mouse {
            x: x as u16 + 1,
            y: y as u16 + 1,
            kind: MouseKind::Click,
        });
    }

    fn barrier(&mut self) {
        self.send(ClientMessage::PaneList);
        self.until(|message, _| matches!(message, ServerMessage::Panes { .. }));
    }
}

#[test]
fn graph_opt_in_first_attach_and_quiet_surface() {
    common::isolate_state();
    let dir = common::socket_root().join(format!("ut-graph-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{}.sock", common::unique_workspace_name()));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 100, 30).unwrap();
        server.set_config(Config {
            sidebar: false,
            file_sidebar: false,
            ..Config::default()
        });
        server.run(&mut poll).unwrap();
    });
    common::wait_for_socket(&socket);
    let stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .unwrap();
    let mut client = Client {
        stream,
        decoder: FrameDecoder::new(),
        screen: Terminal::new(100, 30),
    };
    client.send(ClientMessage::Command(Command::RunGraph));
    client.barrier();
    client.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 100,
        rows: 30,
    });
    client.until(|message, _| matches!(message, ServerMessage::RenderOps(_)));
    assert!(
        !client.screen.dump_text().contains("AGENT GRAPH")
            && !client.screen.dump_text().contains("RUN GRAPH"),
        "off by default"
    );
    client.send(ClientMessage::Command(Command::Today));
    client.until(|_, screen| {
        screen
            .dump_text()
            .contains("projects / agents / observations")
    });
    assert!(!client.screen.dump_text().contains("3 Graph"));
    client.send(ClientMessage::SettingsApply(SettingsPatch {
        run_graph: Some(true),
        // Dismiss the expected default-off toast before testing tab geometry.
        notification_delivery: Some("off".into()),
        ..SettingsPatch::default()
    }));
    client.barrier();
    assert!(
        client.screen.dump_text().contains("3 Graph"),
        "Settings enable must update idle Today immediately"
    );
    client.send(ClientMessage::SettingsApply(SettingsPatch {
        run_graph: Some(false),
        ..SettingsPatch::default()
    }));
    client.barrier();
    assert!(
        !client.screen.dump_text().contains("3 Graph"),
        "Settings disable must update idle Today immediately"
    );
    assert!(client.screen.dump_text().contains("1 Activity"));
    client.send(ClientMessage::SettingsApply(SettingsPatch {
        run_graph: Some(true),
        ..SettingsPatch::default()
    }));
    client.barrier();
    client.send(ClientMessage::Command(Command::RunGraph));
    client.until(|_, screen| screen.dump_text().contains("AGENT GRAPH"));
    client.send(ClientMessage::Input(b"n".to_vec()));
    client.until(|_, screen| screen.dump_text().contains("RUN GRAPH"));
    assert!(client.screen.dump_text().contains("No runs yet"));
    assert!(client.screen.dump_text().contains("TODAY"));
    assert!(client.screen.dump_text().contains("1 Activity"));
    assert!(client.screen.dump_text().contains("2 Flow"));
    assert!(
        client.screen.dump_text().contains("3 Graph"),
        "{}",
        client.screen.dump_text()
    );
    // Shared tabs route through the same semantic input path as their keys.
    client.click_tab("1 Activity");
    client.until(|_, screen| {
        screen
            .dump_text()
            .contains("projects / agents / observations")
    });
    assert!(!client.screen.dump_text().contains("RUN GRAPH"));
    client.click_tab("2 Flow");
    client.until(|_, screen| screen.dump_text().contains("Agent flow"));
    client.send(ClientMessage::Input(b"\t".to_vec()));
    client.until(|_, screen| screen.dump_text().contains("RUN GRAPH"));
    client.send(ClientMessage::Input(b"v".to_vec()));
    client.until(|_, screen| screen.dump_text().contains("No retained runs"));
    client.send(ClientMessage::Input(b"1".to_vec()));
    client.until(|_, screen| {
        screen
            .dump_text()
            .contains("projects / agents / observations")
    });
    client.send(ClientMessage::Input(b"3".to_vec()));
    client.until(|_, screen| screen.dump_text().contains("No retained runs"));
    client.send(ClientMessage::Input(b"\x1b".to_vec()));
    client.until(|_, screen| screen.dump_text().contains("RUN GRAPH"));
    // The second client first attaches to an already open Graph surface.
    let stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .unwrap();
    let mut second = Client {
        stream,
        decoder: FrameDecoder::new(),
        screen: Terminal::new(100, 30),
    };
    second.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 100,
        rows: 30,
    });
    second.until(|_, screen| screen.dump_text().contains("RUN GRAPH"));
    second.barrier();
    second.send(ClientMessage::Input(b"3".to_vec()));
    second.send(ClientMessage::Command(Command::RunGraph));
    second.send(ClientMessage::PaneList);
    second.until(|message, _| {
        assert!(
            !matches!(message, ServerMessage::RenderOps(_)),
            "selecting the current Graph tab emitted a frame"
        );
        matches!(message, ServerMessage::Panes { .. })
    });
    while second.decoder.decode::<ServerMessage>().unwrap().is_some() {}
    let mut bytes = [0; 32768];
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        match second.stream.read(&mut bytes) {
            Ok(0) => panic!("closed during idle"),
            Ok(n) => {
                second.decoder.push(&bytes[..n]);
                while let Some(message) = second.decoder.decode::<ServerMessage>().unwrap() {
                    assert!(
                        !matches!(message, ServerMessage::RenderOps(_)),
                        "idle graph emitted a frame"
                    );
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(e) => panic!("idle read: {e}"),
        }
    }
    client.send(ClientMessage::SettingsApply(SettingsPatch {
        run_graph: Some(false),
        ..SettingsPatch::default()
    }));
    client.until(|_, screen| !screen.dump_text().contains("RUN GRAPH"));
    client.send(ClientMessage::KillServer);
    drop(client);
    drop(second);
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn graph_project_context_updates_through_control_while_visible_and_cached() {
    use std::io::BufRead;
    use uniterm_core::{ProjectId, RunGraphEvent, RunId, RunKind};
    use uniterm_proto::{
        ControlCommand, ControlFrame, ControlRequest, ControlResult, CONTROL_API_VERSION,
    };
    use uniterm_server::eventlog::{append_line, EventLog, LogEvent};

    common::isolate_state();
    let directory = common::temp_dir("graph-project");
    let workspace = common::unique_workspace_name();
    let socket = directory.join(format!("{workspace}.sock"));
    let mut log = EventLog::open(&workspace);
    for change in [
        RunGraphEvent::Created {
            run: RunId(1),
            parent: None,
            project: ProjectId(1),
            kind: RunKind::Workflow,
            task_id: 1,
            title: "Retained validation".into(),
        },
        RunGraphEvent::Completed {
            run: RunId(1),
            outcome: "pass: reviewed".into(),
        },
    ] {
        let (_, line) = log.record(LogEvent::RunGraph { change }).unwrap();
        append_line(&workspace, &line).unwrap();
    }
    drop(log);
    let server_socket = socket.clone();
    let server =
        thread::spawn(move || uniterm_server::run_server(&server_socket, "/bin/sh", &[]).unwrap());
    common::wait_for_socket(&socket);
    let stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .unwrap();
    let mut client = Client {
        stream,
        decoder: FrameDecoder::new(),
        screen: Terminal::new(120, 32),
    };
    client.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 120,
        rows: 32,
    });
    client.barrier();
    client.send(ClientMessage::SettingsApply(SettingsPatch {
        run_graph: Some(true),
        sidebar: Some(false),
        file_sidebar: Some(false),
        ..SettingsPatch::default()
    }));
    client.barrier();
    client.send(ClientMessage::Command(Command::RunGraph));
    client.until(|_, screen| screen.dump_text().contains("AGENT GRAPH"));
    client.send(ClientMessage::Input(b"n".to_vec()));
    client.until(|_, screen| screen.dump_text().contains("Retained validation"));
    let mutate = |command| {
        let mut stream = UnixStream::connect(socket.with_extension("control.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        serde_json::to_writer(
            &mut stream,
            &ControlRequest {
                version: CONTROL_API_VERSION,
                id: 1,
                workspace: workspace.clone(),
                command,
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
        let mut line = String::new();
        std::io::BufReader::new(stream)
            .read_line(&mut line)
            .unwrap();
        assert!(
            matches!(serde_json::from_str::<ControlFrame>(&line).unwrap(), ControlFrame::Response(response) if matches!(response.result, Some(ControlResult::Mutation { accepted: true, .. })))
        );
    };
    mutate(ControlCommand::ProjectRename {
        project: ProjectId(1),
        name: "Graph renamed project".into(),
    });
    client.until(|_, screen| screen.dump_text().contains("Graph renamed project"));
    mutate(ControlCommand::ProjectDirectory {
        project: ProjectId(1),
        root: directory.to_string_lossy().into_owned(),
    });
    client.until(|_, screen| screen.dump_text().contains(directory.to_str().unwrap()));
    client.assert_quiet();
    client.send(ClientMessage::Input(b"1".to_vec()));
    client.until(|_, screen| {
        screen
            .dump_text()
            .contains("projects / agents / observations")
    });
    mutate(ControlCommand::ProjectRename {
        project: ProjectId(1),
        name: "Cached graph renamed".into(),
    });
    client.send(ClientMessage::Input(b"3".to_vec()));
    client.until(|_, screen| screen.dump_text().contains("Cached graph renamed"));
    assert!(!client.screen.dump_text().contains("Graph renamed project"));
    client.send(ClientMessage::KillServer);
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn ordinary_manual_agents_native_children_work_labels_and_safe_focus_without_runs() {
    use uniterm_core::PaneId;
    common::isolate_state();
    let directory = common::socket_root().join(format!("ut-native-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let socket = directory.join(format!("{}.sock", common::unique_workspace_name()));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 150, 40).unwrap();
        server.set_config(Config {
            run_graph: true,
            auto_tab_names: false,
            sidebar: false,
            file_sidebar: false,
            ..Config::default()
        });
        server.run(&mut poll).unwrap();
    });
    common::wait_for_socket(&socket);
    let connect = || {
        let stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(150)))
            .unwrap();
        Client {
            stream,
            decoder: FrameDecoder::new(),
            screen: Terminal::new(150, 40),
        }
    };
    let mut client = connect();
    client.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 150,
        rows: 40,
    });
    client.barrier();
    let notify = |client: &mut Client, payload: serde_json::Value| {
        let json = serde_json::to_string(&payload)
            .unwrap()
            .replace('\'', "'\\''");
        client.send(ClientMessage::PaneSend {
            pane: PaneId(1),
            bytes: format!("printf '\\033]777;notify;uniterm://cli-agent;{json}\\007'\r")
                .into_bytes(),
        });
    };
    // Ordinary manually launched/detected agents bind through the shared hook
    // path. No workflow, relay, launch command, run or role is created.
    notify(
        &mut client,
        serde_json::json!({"agent":"codex", "event":"session_start", "session_id":"native-root"}),
    );
    client.send(ClientMessage::Command(Command::RunGraph));
    client.until(|_, screen| screen.dump_text().contains("live Pane"));
    // The title must work with automatic Tab naming OFF, including a change
    // after the Graph is already visible. It is a work label, never a status.
    client.send(ClientMessage::PaneSend {
        pane: PaneId(1),
        bytes: b"printf '\\033]0;Validate native task title\\007'\r".to_vec(),
    });
    client.until(|_, screen| screen.dump_text().contains("Validate native task title"));
    notify(
        &mut client,
        serde_json::json!({"agent":"codex", "event":"prompt_submit"}),
    );
    client.until(|_, screen| screen.dump_text().contains("live Pane · Working"));
    // Spinner-only title revisions retain observed work even with auto naming
    // disabled, and a same-label changed-Pane lookup emits no Graph frame.
    for title in ["⠋ Working…", "⠙ Working…"] {
        client.send(ClientMessage::PaneSend {
            pane: PaneId(1),
            bytes: format!("printf '\\033]0;{title}\\007'\r").into_bytes(),
        });
        client.assert_quiet();
        assert!(client
            .screen
            .dump_text()
            .contains("Validate native task title"));
    }
    notify(
        &mut client,
        serde_json::json!({"agent":"codex", "event":"idle", "summary":"Validate cooperative work"}),
    );
    client.until(|_, screen| screen.dump_text().contains("Validate cooperative work"));
    notify(
        &mut client,
        serde_json::json!({"agent":"codex", "event":"session_start", "session_id":"native-child", "parent_session_id":"native-root", "summary":"Inspect child request"}),
    );
    client.until(|_, screen| {
        let text = screen.dump_text();
        text.contains("Inspect child request") && text.contains("unknown")
    });
    assert!(client.screen.dump_text().contains("2 agents"));
    assert!(client.screen.dump_text().contains("Inspect child request"));
    assert!(client
        .screen
        .dump_text()
        .contains("Validate cooperative work"));
    // Live rows use current Project context; historical observations retain
    // their recorded context rather than retroactively rewriting evidence.
    let mutate = |command| {
        use std::io::BufRead;
        use uniterm_proto::{
            ControlCommand, ControlFrame, ControlRequest, ControlResult, CONTROL_API_VERSION,
        };
        let command: ControlCommand = command;
        let mut stream = UnixStream::connect(socket.with_extension("control.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        serde_json::to_writer(
            &mut stream,
            &ControlRequest {
                version: CONTROL_API_VERSION,
                id: 1,
                workspace: socket.file_stem().unwrap().to_str().unwrap().into(),
                command,
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
        let mut line = String::new();
        std::io::BufReader::new(stream)
            .read_line(&mut line)
            .unwrap();
        assert!(
            matches!(serde_json::from_str::<ControlFrame>(&line).unwrap(),
            ControlFrame::Response(response) if matches!(response.result, Some(ControlResult::Mutation { accepted: true, .. })))
        );
    };
    mutate(uniterm_proto::ControlCommand::ProjectRename {
        project: uniterm_core::ProjectId(1),
        name: "Native renamed project".into(),
    });
    client.until(|_, screen| screen.dump_text().contains("Native renamed project"));
    mutate(uniterm_proto::ControlCommand::ProjectDirectory {
        project: uniterm_core::ProjectId(1),
        root: directory.to_string_lossy().into_owned(),
    });
    client.until(|_, screen| screen.dump_text().contains(directory.to_str().unwrap()));
    client.click_tab("Runs / evidence");
    client.until(|_, screen| screen.dump_text().contains("No runs yet"));
    client.send(ClientMessage::Input(b"n".to_vec()));
    client.until(|_, screen| screen.dump_text().contains("AGENT GRAPH"));
    client.send(ClientMessage::Input(b"v".to_vec()));
    client.until(|_, screen| {
        screen
            .dump_text()
            .contains("Work: Validate cooperative work")
    });
    assert!(client.screen.dump_text().contains("Event #"));
    assert!(client.screen.dump_text().contains("Session: native-root"));
    assert!(client.screen.dump_text().contains("Invocation: #"));
    assert!(client.screen.dump_text().contains("Idle"));
    client.send(ClientMessage::Input(b"v".to_vec()));
    client.until(|_, screen| screen.dump_text().contains("AGENT GRAPH"));
    // First attach and selecting the current mode carry no animation/timer.
    let mut second = connect();
    second.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 150,
        rows: 40,
    });
    second.until(|_, screen| screen.dump_text().contains("Inspect child request"));
    second.send(ClientMessage::Command(Command::RunGraph));
    second.send(ClientMessage::Input(b"3".to_vec()));
    second.assert_quiet();
    // A native child shares its parent's PTY, but may never focus that parent.
    client.send(ClientMessage::Input(b"j".to_vec()));
    client.send(ClientMessage::Input(b"\r".to_vec()));
    client.until(|_, screen| screen.dump_text().contains("No matching live invocation"));
    assert!(client.screen.dump_text().contains("AGENT GRAPH"));
    client.send(ClientMessage::Input(b"k".to_vec()));
    client.send(ClientMessage::Input(b"\r".to_vec()));
    client.until(|_, screen| !screen.dump_text().contains("AGENT GRAPH"));
    client.send(ClientMessage::Command(Command::RunGraph));
    client.until(|_, screen| screen.dump_text().contains("live Pane"));
    // A new provider binding reuses Pane 1 but gets a different durable
    // invocation. Stable historical selection must not follow it silently.
    notify(
        &mut client,
        serde_json::json!({"agent":"codex", "event":"session_end"}),
    );
    notify(
        &mut client,
        serde_json::json!({"agent":"claude", "event":"session_start", "session_id":"replacement", "summary":"Review replacement work"}),
    );
    client.until(|_, screen| screen.dump_text().contains("Review replacement work"));
    client.send(ClientMessage::Input(b"\r".to_vec()));
    client.until(|_, screen| screen.dump_text().contains("No matching live invocation"));
    client.send(ClientMessage::Input(b"v".to_vec()));
    client.until(|_, screen| {
        screen
            .dump_text()
            .contains("Work: Validate cooperative work")
    });
    assert!(client.screen.dump_text().contains("historical"));
    assert!(!client
        .screen
        .dump_text()
        .contains("Work: Review replacement work"));
    client.send(ClientMessage::KillServer);
    drop(client);
    drop(second);
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(directory);
}
