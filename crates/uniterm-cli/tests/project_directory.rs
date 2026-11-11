//! Directory editing shares the host command, preserves live Panes, and survives stop.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;
use uniterm_core::ProjectId;
use uniterm_proto::{encode_frame, FrameDecoder, ServerMessage};
use uniterm_proto::{ClientMessage, ControlCommand, ControlResult};

mod common;
use common::{isolate_state, unique_workspace_name};

fn ut(runtime: &Path, state: &Path, workspace: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(args)
        .args(["-w", workspace])
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", state)
        .output()
        .unwrap()
}

fn drain_renders(stream: &mut UnixStream, decoder: &mut FrameDecoder) -> usize {
    let mut count = 0;
    let mut buffer = [0; 65536];
    loop {
        while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            if matches!(message, ServerMessage::RenderOps(_)) {
                count += 1;
            }
        }
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => decoder.push(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(error) => panic!("attach read failed: {error}"),
        }
    }
    count
}

#[test]
fn directory_change_preserves_panes_and_focus_and_updates_future_launches() {
    let state = isolate_state();
    let workspace = unique_workspace_name();
    let runtime = common::socket_root().join(format!("ut-dir-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    let root = state.join("new directory 界");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let alias = state.join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();
    let socket = uniterm_server::server::default_socket_path(&workspace);
    let path = socket.clone();
    let server = thread::spawn(move || {
        uniterm_server::server::run_server(&path, "/bin/sh", &[]).unwrap();
    });
    common::wait_for_socket(&socket);
    common::wait_for_socket(&socket.with_extension("control.sock"));
    let mut attached = UnixStream::connect(&socket).unwrap();
    attached
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    attached
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 240,
            rows: 30,
        }))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    assert!(drain_renders(&mut attached, &mut decoder) > 0);
    let snapshot =
        || uniterm_client::workspace_request(&socket, ClientMessage::WorkspaceState).unwrap();
    let original = snapshot().2[0].clone();
    let old_pane = uniterm_client::pane_list(&socket).unwrap().1[0].id;
    let created = uniterm_client::control_request(
        &socket,
        ControlCommand::ProjectCreate {
            name: "Other".into(),
            root: state.to_string_lossy().into_owned(),
            background: false,
        },
    )
    .unwrap();
    assert!(created.error.is_none());
    let selected = snapshot().1;
    assert_ne!(selected, original.id);
    let changed = ut(
        &runtime,
        &state,
        &workspace,
        &[
            "project",
            "directory",
            &original.name,
            alias.to_str().unwrap(),
        ],
    );
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let (_, active, projects) = snapshot();
    assert_eq!(active, selected);
    let updated = projects.iter().find(|p| p.id == original.id).unwrap();
    assert_eq!(updated.root, root.to_str().unwrap());
    assert_eq!(updated.name, original.name);
    assert_eq!(updated.tabs, original.tabs);
    assert!(uniterm_client::pane_list(&socket)
        .unwrap()
        .1
        .iter()
        .any(|p| p.id == old_pane));

    // Both control and CLI accept canonical aliases as successful no-ops.
    drain_renders(&mut attached, &mut decoder);
    let noop = uniterm_client::control_request(
        &socket,
        ControlCommand::ProjectDirectory {
            project: original.id,
            root: alias.to_string_lossy().into_owned(),
        },
    )
    .unwrap();
    assert!(matches!(
        noop.result,
        Some(ControlResult::Mutation { accepted: true, .. })
    ));
    assert_eq!(
        drain_renders(&mut attached, &mut decoder),
        0,
        "an unchanged root must emit no frames"
    );
    for invalid in [
        "",
        "relative",
        state.join("missing").to_str().unwrap(),
        file!(),
    ] {
        let failed = ut(
            &runtime,
            &state,
            &workspace,
            &["project", "directory", "1", invalid],
        );
        assert!(!failed.status.success());
        assert_eq!(snapshot().2[0].root, root.to_str().unwrap());
    }
    let missing = uniterm_client::control_request(
        &socket,
        ControlCommand::ProjectDirectory {
            project: ProjectId(u64::MAX),
            root: root.to_string_lossy().into_owned(),
        },
    )
    .unwrap();
    assert!(missing.error.is_some());

    // Existing shells retain their process cwd; future Tabs start at the new root.
    uniterm_client::pane_send(
        &socket,
        old_pane,
        b"printf 'OLD_CWD:%s\\n' \"$PWD\"\r".to_vec(),
    )
    .unwrap();
    let wait = uniterm_client::pane_wait_output(
        &socket,
        old_pane,
        format!("OLD_CWD:{}", original.root),
        Duration::from_secs(3),
    )
    .unwrap();
    assert!(
        wait.matched,
        "output: {:?}; original root: {:?}",
        wait.text, original.root
    );
    let tab = uniterm_client::control_request(
        &socket,
        ControlCommand::TabCreate {
            project: original.id,
            background: true,
        },
    )
    .unwrap();
    assert!(tab.error.is_none());
    let new_pane = uniterm_client::pane_list(&socket)
        .unwrap()
        .1
        .into_iter()
        .find(|p| p.project == original.id && p.id != old_pane)
        .unwrap()
        .id;
    uniterm_client::pane_send(
        &socket,
        new_pane,
        b"printf 'NEW_CWD:%s\\n' \"$PWD\"\r".to_vec(),
    )
    .unwrap();
    let wait = uniterm_client::pane_wait_output(
        &socket,
        new_pane,
        format!("NEW_CWD:{}", root.display()),
        Duration::from_secs(3),
    )
    .unwrap();
    assert!(
        wait.matched,
        "output: {:?}; original root: {:?}",
        wait.text, original.root
    );
    assert_eq!(snapshot().1, selected);

    uniterm_client::control(&socket, ClientMessage::KillServer).unwrap();
    server.join().unwrap();
    let log = std::fs::read_to_string(
        uniterm_server::persist::snapshot_path(&workspace).with_extension("log"),
    )
    .unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("ProjectDirectoryChanged"))
            .count(),
        1
    );
    let path = socket.clone();
    let server =
        thread::spawn(move || uniterm_server::server::run_server(&path, "/bin/sh", &[]).unwrap());
    common::wait_for_socket(&socket);
    assert_eq!(snapshot().2[0].root, root.to_str().unwrap());
    uniterm_client::control(&socket, ClientMessage::KillServer).unwrap();
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(runtime);
}
