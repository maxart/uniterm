//! End-to-end contract for the neutral evented control socket.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

use uniterm_proto::{
    encode_frame, ClientMessage, ControlCommand, ControlFrame, ControlRequest, ControlResult,
    CONTROL_API_VERSION,
};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_socket() -> std::path::PathBuf {
    let workspace = unique_workspace_name();
    let dir = common::socket_root().join(format!("uniterm-control-{workspace}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{workspace}.sock"))
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

fn send(writer: &mut UnixStream, workspace: &str, id: u64, command: ControlCommand) {
    let request = ControlRequest {
        version: CONTROL_API_VERSION,
        id,
        workspace: workspace.into(),
        command,
    };
    serde_json::to_writer(&mut *writer, &request).unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();
}

fn read_frame(reader: &mut BufReader<UnixStream>) -> ControlFrame {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(
        !line.is_empty(),
        "control connection closed without a frame"
    );
    serde_json::from_str(&line).unwrap()
}

#[test]
fn snapshots_pane_io_and_cursored_events_share_one_private_stream() {
    isolate_state();
    let socket = temp_socket();
    let workspace = socket.file_stem().unwrap().to_str().unwrap().to_string();
    let control = socket.with_extension("control.sock");
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(
            &server_socket,
            "/bin/sh",
            &["-c", "read line; printf 'ECHO:%s' \"$line\"; sleep 30"],
            80,
            24,
        )
        .unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&control);
    // bind() creates the socket with the default mode and the server sets
    // 0600 right after; the parent directory is already 0700, so nothing is
    // reachable in between, but this check can land in that window.
    let mut mode = 0;
    for _ in 0..100 {
        mode = std::fs::metadata(&control).unwrap().permissions().mode() & 0o777;
        if mode == 0o600 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(mode, 0o600);

    let mut writer = UnixStream::connect(&control).unwrap();
    writer
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(writer.try_clone().unwrap());

    send(&mut writer, &workspace, 1, ControlCommand::Capabilities);
    match read_frame(&mut reader) {
        ControlFrame::Response(response) => match response.result.unwrap() {
            ControlResult::Capabilities {
                protocol_version,
                capabilities,
                ..
            } => {
                assert_eq!(protocol_version, 1);
                assert!(capabilities
                    .iter()
                    .any(|item| item == "orchestration.start"));
                assert!(capabilities.iter().any(|item| item == "artifact.list"));
            }
            other => panic!("unexpected capabilities response: {other:?}"),
        },
        other => panic!("unexpected control frame: {other:?}"),
    }

    send(
        &mut writer,
        &workspace,
        100,
        ControlCommand::OrchestrationStart {
            launch: uniterm_proto::OrchestrationLaunch {
                kind: uniterm_proto::OrchestrationKind::Workflow,
                template: Some("pair".into()),
                goal: "must not partially launch".into(),
                provider: None,
                role_providers: vec![uniterm_proto::RoleProviderSelection {
                    role: "not-a-role".into(),
                    provider: "not-a-provider".into(),
                }],
                project: None,
            },
        },
    );
    assert!(matches!(
        read_frame(&mut reader),
        ControlFrame::Response(response)
            if response.error.as_ref().is_some_and(|error| {
                error.code == "invalid_orchestration_launch"
                    && error.message.contains("unknown orchestration role")
            })
    ));

    send(
        &mut writer,
        &workspace,
        2,
        ControlCommand::WorkspaceSnapshot,
    );
    let sequence = match read_frame(&mut reader) {
        ControlFrame::Response(response) => match response.result.unwrap() {
            ControlResult::Workspace {
                name,
                sequence,
                projects,
                ..
            } => {
                assert_eq!(name, workspace);
                assert_eq!(projects.len(), 1);
                sequence
            }
            result => panic!("unexpected workspace result: {result:?}"),
        },
        frame => panic!("unexpected workspace frame: {frame:?}"),
    };

    send(&mut writer, &workspace, 3, ControlCommand::PaneList);
    assert!(matches!(
        read_frame(&mut reader),
        ControlFrame::Response(response)
            if matches!(response.result, Some(ControlResult::Panes { ref panes, .. }) if panes.len() == 1)
    ));

    send(
        &mut writer,
        &workspace,
        31,
        ControlCommand::RunList {
            project: None,
            active_only: false,
        },
    );
    assert!(matches!(
        read_frame(&mut reader),
        ControlFrame::Response(response)
            if matches!(response.result, Some(ControlResult::Runs { workspace: ref response_workspace, ref runs })
                if response_workspace == &workspace && runs.is_empty())
    ));

    send(
        &mut writer,
        &workspace,
        32,
        ControlCommand::ArtifactList {
            project: None,
            run: None,
            include_superseded: false,
        },
    );
    assert!(matches!(
        read_frame(&mut reader),
        ControlFrame::Response(response)
            if matches!(response.result, Some(ControlResult::Artifacts { workspace: ref response_workspace, ref artifacts })
                if response_workspace == &workspace && artifacts.is_empty())
    ));

    writeln!(
        writer,
        "{{\"version\":1,\"id\":4,\"workspace\":{workspace:?},\"method\":\"pane_send\",\"params\":{{\"pane\":1,\"text\":\"hello\\n\"}}}}"
    )
    .unwrap();
    writer.flush().unwrap();
    assert!(matches!(
        read_frame(&mut reader),
        ControlFrame::Response(response)
            if matches!(response.result, Some(ControlResult::PaneSent { found: true, accepted: true, .. }))
    ));
    thread::sleep(Duration::from_millis(100));
    send(
        &mut writer,
        &workspace,
        5,
        ControlCommand::PaneRead {
            pane: uniterm_core::PaneId(1),
            lines: 20,
        },
    );
    assert!(matches!(
        read_frame(&mut reader),
        ControlFrame::Response(response)
            if matches!(response.result, Some(ControlResult::PaneOutput { found: true, ref text, .. }) if text.contains("ECHO:hello"))
    ));

    send(
        &mut writer,
        &workspace,
        6,
        ControlCommand::Subscribe {
            after_sequence: sequence.saturating_sub(1),
        },
    );
    send(
        &mut writer,
        &workspace,
        66,
        ControlCommand::Subscribe {
            after_sequence: sequence,
        },
    );
    assert!(matches!(
        read_frame(&mut reader),
        ControlFrame::Response(response)
            if response.id == 6 && matches!(response.result, Some(ControlResult::Subscribed { .. }))
    ));
    let next = [read_frame(&mut reader), read_frame(&mut reader)];
    assert!(next
        .iter()
        .any(|frame| matches!(frame, ControlFrame::Event(event) if event.sequence >= sequence)));
    assert!(next.iter().any(|frame| matches!(
        frame,
        ControlFrame::Response(response)
            if response.id == 66
                && response.error.as_ref().is_some_and(|error| error.code == "already_subscribed")
    )));

    let mut binary = UnixStream::connect(&socket).unwrap();
    binary
        .write_all(&encode_frame(&ClientMessage::SaveTask {
            title: "from control test".into(),
        }))
        .unwrap();
    let live = read_frame(&mut reader);
    assert!(matches!(live, ControlFrame::Event(event) if event.event.get("TaskCreated").is_some()));

    let foreign = ControlRequest {
        version: CONTROL_API_VERSION,
        id: 7,
        workspace: "other".into(),
        command: ControlCommand::ArtifactList {
            project: None,
            run: None,
            include_superseded: true,
        },
    };
    serde_json::to_writer(&mut writer, &foreign).unwrap();
    writer.write_all(b"\n").unwrap();
    assert!(matches!(
        read_frame(&mut reader),
        ControlFrame::Response(response)
            if response.error.as_ref().is_some_and(|error| error.code == "workspace_mismatch")
    ));

    let renamed_workspace = format!("renamed-{workspace}");
    let renamed_socket = socket.with_file_name(format!("{renamed_workspace}.sock"));
    let renamed_control = renamed_socket.with_extension("control.sock");
    binary
        .write_all(&encode_frame(&ClientMessage::RenameSession {
            name: renamed_workspace.clone(),
        }))
        .unwrap();
    wait_for(&renamed_control);
    assert!(!control.exists());
    let mut renamed_writer = UnixStream::connect(&renamed_control).unwrap();
    let mut renamed_reader = BufReader::new(renamed_writer.try_clone().unwrap());
    send(
        &mut renamed_writer,
        &renamed_workspace,
        8,
        ControlCommand::Capabilities,
    );
    assert!(matches!(
        read_frame(&mut renamed_reader),
        ControlFrame::Response(response) if response.error.is_none()
    ));

    binary
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    drop(binary);
    drop(writer);
    server.join().unwrap();
    assert!(!renamed_control.exists());
}

#[test]
fn control_path_conflict_fails_startup_without_replacing_user_data() {
    isolate_state();
    let socket = temp_socket();
    let control = socket.with_extension("control.sock");
    std::fs::write(&control, b"keep me").unwrap();

    let error = Server::bind(&socket, "/bin/sh", &["-c", "sleep 1"], 80, 24)
        .err()
        .expect("a regular control path must reject startup");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&control).unwrap(), b"keep me");
    assert!(
        !socket.exists(),
        "failed startup left a stale attach socket"
    );
}

#[test]
fn destructive_control_methods_require_explicit_confirmation() {
    isolate_state();
    let socket = temp_socket();
    let workspace = socket.file_stem().unwrap().to_str().unwrap().to_string();
    let control = socket.with_extension("control.sock");
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 30"], 80, 24).unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&control);
    let mut writer = UnixStream::connect(&control).unwrap();
    writer
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(writer.try_clone().unwrap());

    let root = socket.parent().unwrap().join("second");
    std::fs::create_dir_all(&root).unwrap();
    send(
        &mut writer,
        &workspace,
        1,
        ControlCommand::ProjectCreate {
            name: "Second".into(),
            root: root.to_string_lossy().into_owned(),
        },
    );
    let second = match read_frame(&mut reader) {
        ControlFrame::Response(response) => match response.result.unwrap() {
            ControlResult::Mutation { id: Some(id), .. } => uniterm_core::ProjectId(id),
            result => panic!("unexpected create result: {result:?}"),
        },
        frame => panic!("unexpected frame: {frame:?}"),
    };

    // Unconfirmed destructive and bulk requests change nothing and say why.
    for (id, command) in [
        (
            2,
            ControlCommand::ProjectRemove {
                project: second,
                confirmed: false,
            },
        ),
        (
            3,
            ControlCommand::AgentStopAll {
                scope: uniterm_proto::StopScope::Workspace,
                confirmed: false,
            },
        ),
    ] {
        send(&mut writer, &workspace, id, command);
        assert!(matches!(
            read_frame(&mut reader),
            ControlFrame::Response(response)
                if response.error.as_ref().is_some_and(|error| error.code == "confirmation_required")
        ));
    }
    send(
        &mut writer,
        &workspace,
        4,
        ControlCommand::WorkspaceSnapshot,
    );
    match read_frame(&mut reader) {
        ControlFrame::Response(response) => match response.result.unwrap() {
            ControlResult::Workspace { projects, .. } => assert_eq!(projects.len(), 2),
            result => panic!("unexpected snapshot result: {result:?}"),
        },
        frame => panic!("unexpected frame: {frame:?}"),
    }

    // The confirmed request proceeds.
    send(
        &mut writer,
        &workspace,
        5,
        ControlCommand::ProjectRemove {
            project: second,
            confirmed: true,
        },
    );
    assert!(matches!(
        read_frame(&mut reader),
        ControlFrame::Response(response)
            if matches!(response.result, Some(ControlResult::Mutation { found: true, accepted: true, .. }))
    ));

    let mut binary = UnixStream::connect(&socket).unwrap();
    binary
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();
}
