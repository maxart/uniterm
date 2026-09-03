//! End-to-end ownership, capacity, audit, and elapsed-time guardrail contract.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, SystemTime};

use uniterm_proto::{
    encode_frame, ClientMessage, ControlCommand, ControlFrame, ControlRequest, ControlResponse,
    ControlResult, FrameDecoder, OrchestrationKind, OrchestrationLaunch, ServerMessage,
    CONTROL_API_VERSION,
};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_dir() -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("uniterm-guardrail-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for(path: &std::path::Path) {
    for _ in 0..300 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("socket never appeared at {}", path.display());
}

fn send_control(writer: &mut UnixStream, workspace: &str, id: u64, command: ControlCommand) {
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

fn read_control(reader: &mut BufReader<UnixStream>) -> ControlFrame {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(!line.is_empty(), "control connection closed");
    serde_json::from_str(&line).unwrap()
}

fn response_with_events(
    reader: &mut BufReader<UnixStream>,
    id: u64,
) -> (ControlResponse, Vec<serde_json::Value>) {
    let mut events = Vec::new();
    loop {
        match read_control(reader) {
            ControlFrame::Response(response) if response.id == id => return (response, events),
            ControlFrame::Event(event) => events.push(event.event),
            _ => {}
        }
    }
}

fn read_workspace(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
) -> Vec<uniterm_proto::ProjectInfo> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 32_768];
    while std::time::Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(count) => {
                decoder.push(&buf[..count]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    if let ServerMessage::Workspace { projects, .. } = message {
                        return projects;
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("binary read failed: {error}"),
        }
    }
    panic!("Workspace response did not arrive");
}

fn read_waiting(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
) -> Vec<uniterm_proto::WaitingEntry> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 32_768];
    while std::time::Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(count) => {
                decoder.push(&buf[..count]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    if let ServerMessage::Waiting { items } = message {
                        return items;
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("binary read failed: {error}"),
        }
    }
    panic!("waiting response did not arrive");
}

fn guard_event(events: &[serde_json::Value], outcome: &str) -> bool {
    events.iter().any(|event| {
        event.get("GuardrailDecision").is_some()
            && event
                .to_string()
                .contains(&format!("\"outcome\":\"{outcome}\""))
    })
}

fn await_guard_event(
    reader: &mut BufReader<UnixStream>,
    mut events: Vec<serde_json::Value>,
    outcome: &str,
) {
    while !guard_event(&events, outcome) {
        if let ControlFrame::Event(event) = read_control(reader) {
            events.push(event.event);
        }
    }
}

#[test]
fn control_launch_obeys_project_capacity_and_elapsed_guards_without_partial_panes() {
    isolate_state();
    let dir = temp_dir();
    let target_root = dir.join("target");
    std::fs::create_dir_all(&target_root).unwrap();
    let provider = dir.join("guard-agent");
    std::fs::write(&provider, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o755)).unwrap();

    let socket = dir.join(format!("{}.sock", unique_workspace_name()));
    let control = socket.with_extension("control.sock");
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 30"], 100, 30).unwrap();
        let mut config = uniterm_core::Config::default();
        config.guardrails.max_active_runs = 1;
        config.guardrails.max_role_panes = 2;
        config.guardrails.max_elapsed_seconds = 1;
        config.guardrail_allowed_projects = vec!["guard-target".into()];
        server.set_config(config);
        let _ = server.run(&mut poll);
    });
    wait_for(&control);

    let mut binary = UnixStream::connect(&socket).unwrap();
    binary
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    binary
        .write_all(&encode_frame(&ClientMessage::ProjectCreate {
            name: "guard-target".into(),
            root: target_root.to_string_lossy().into_owned(),
        }))
        .unwrap();
    let projects = read_workspace(&mut binary, &mut decoder);
    let target = projects
        .iter()
        .find(|project| project.name == "guard-target")
        .expect("target Project was created")
        .id;

    let workspace = socket.file_stem().unwrap().to_str().unwrap().to_string();
    let mut writer = UnixStream::connect(&control).unwrap();
    writer
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(writer.try_clone().unwrap());
    send_control(
        &mut writer,
        &workspace,
        1,
        ControlCommand::WorkspaceSnapshot,
    );
    let (snapshot, _) = response_with_events(&mut reader, 1);
    let sequence = match snapshot.result.unwrap() {
        ControlResult::Workspace { sequence, .. } => sequence,
        result => panic!("unexpected Workspace result: {result:?}"),
    };
    send_control(
        &mut writer,
        &workspace,
        2,
        ControlCommand::Subscribe {
            after_sequence: sequence,
        },
    );
    let (subscribed, _) = response_with_events(&mut reader, 2);
    assert!(subscribed.error.is_none());

    let launch = OrchestrationLaunch {
        kind: OrchestrationKind::Workflow,
        template: Some("solo".into()),
        goal: "guarded work".into(),
        provider: Some(provider.to_string_lossy().into_owned()),
        role_providers: Vec::new(),
        project: Some("guard-target".into()),
    };
    send_control(
        &mut writer,
        &workspace,
        3,
        ControlCommand::OrchestrationStart {
            launch: launch.clone(),
        },
    );
    let (started, events) = response_with_events(&mut reader, 3);
    assert!(matches!(
        started.result,
        Some(ControlResult::OrchestrationStarted { .. })
    ));
    await_guard_event(&mut reader, events, "allow");

    send_control(&mut writer, &workspace, 4, ControlCommand::PaneList);
    let (panes, _) = response_with_events(&mut reader, 4);
    let panes = match panes.result.unwrap() {
        ControlResult::Panes { panes, .. } => panes,
        result => panic!("unexpected Pane result: {result:?}"),
    };
    assert_eq!(panes.len(), 3);
    assert_eq!(
        panes.iter().filter(|pane| pane.project == target).count(),
        2
    );

    let mut capped = launch.clone();
    capped.kind = OrchestrationKind::Relay;
    capped.template = None;
    send_control(
        &mut writer,
        &workspace,
        5,
        ControlCommand::OrchestrationStart { launch: capped },
    );
    let (denied, events) = response_with_events(&mut reader, 5);
    assert!(denied.error.as_ref().is_some_and(|error| {
        error.code == "invalid_orchestration_launch" && error.message.contains("active run cap")
    }));
    await_guard_event(&mut reader, events, "deny");

    let mut foreign = launch;
    foreign.project = Some("not-in-this-workspace".into());
    send_control(
        &mut writer,
        &workspace,
        6,
        ControlCommand::OrchestrationStart { launch: foreign },
    );
    let (denied, events) = response_with_events(&mut reader, 6);
    assert!(denied
        .error
        .as_ref()
        .is_some_and(|error| error.message.contains("unknown Project")));
    await_guard_event(&mut reader, events, "deny");

    binary
        .write_all(&encode_frame(&ClientMessage::NewTask {
            prompt: "interactive launch must share the cap".into(),
            relay: true,
            agent: Some(provider.to_string_lossy().into_owned()),
            role_providers: Vec::new(),
            workflow: None,
            project: Some("guard-target".into()),
        }))
        .unwrap();
    binary
        .write_all(&encode_frame(&ClientMessage::WorkspaceState))
        .unwrap();
    let _ = read_workspace(&mut binary, &mut decoder);

    send_control(&mut writer, &workspace, 7, ControlCommand::PaneList);
    let (panes, _) = response_with_events(&mut reader, 7);
    assert!(matches!(
        panes.result,
        Some(ControlResult::Panes { ref panes, .. }) if panes.len() == 3
    ));

    thread::sleep(Duration::from_millis(1_200));
    binary
        .write_all(&encode_frame(&ClientMessage::WaitingList))
        .unwrap();
    let waiting = read_waiting(&mut binary, &mut decoder);
    assert!(waiting
        .iter()
        .any(|item| item.summary.contains("elapsed-time cap")));
    await_guard_event(&mut reader, Vec::new(), "ask");

    binary
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    drop(binary);
    drop(writer);
    server.join().unwrap();
    assert!(!control.exists());
}
