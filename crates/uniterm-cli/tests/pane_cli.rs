//! CLI coverage for stable Pane listing and cross-Project focus.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_core::{PaneId, ProjectId};
use uniterm_proto::{ClientMessage, Command as MultiplexerCommand, SplitAxis};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn wait_for(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("server socket never appeared at {}", path.display());
}

fn run_ut(
    runtime: &std::path::Path,
    state: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", state)
        .output()
        .unwrap()
}

#[test]
fn pane_list_json_and_focus_are_scriptable_across_projects() {
    let base =
        std::env::temp_dir().join(format!("uniterm-cli-pane-control-{}", std::process::id()));
    let runtime = base.join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    let state = isolate_state();

    let workspace = unique_workspace_name();
    let socket = uniterm_server::server::default_socket_path(&workspace);
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 100, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&socket);

    let runs = run_ut(
        &runtime,
        &state,
        &["run", "list", "-w", &workspace, "--json"],
    );
    assert!(
        runs.status.success(),
        "run JSON list failed: {}",
        String::from_utf8_lossy(&runs.stderr)
    );
    let runs: serde_json::Value = serde_json::from_slice(&runs.stdout).unwrap();
    assert_eq!(runs["workspace"], workspace);
    assert_eq!(runs["runs"], serde_json::json!([]));

    uniterm_client::control(
        &socket,
        ClientMessage::ProjectCreate {
            name: "Second".into(),
            root: base.to_string_lossy().into_owned(),
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (_, panes) = uniterm_client::pane_list(&socket).unwrap();
        if panes.len() == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "second Project Pane was not created"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let json = run_ut(
        &runtime,
        &state,
        &["pane", "list", "-w", &workspace, "--json"],
    );
    assert!(
        json.status.success(),
        "pane JSON list failed: {}",
        String::from_utf8_lossy(&json.stderr)
    );
    let document: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(document["workspace"], workspace);
    let panes = document["panes"].as_array().expect("Pane array");
    assert_eq!(panes.len(), 2);
    assert_eq!(panes[0]["id"], 1);
    assert_eq!(panes[0]["project"], 1);
    assert_eq!(panes[0]["tab"], 1);
    assert_eq!(panes[0]["pane"], 1);
    assert_eq!(panes[0]["active"], false);
    assert_eq!(panes[1]["id"], 2);
    assert_eq!(panes[1]["project_name"], "Second");
    assert_eq!(panes[1]["active"], true);

    let human = run_ut(&runtime, &state, &["pane", "list", "-w", &workspace]);
    assert!(human.status.success());
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.contains(&format!("Workspace {workspace}")));
    assert!(human.contains("Project Second (2)"));

    let focus = run_ut(&runtime, &state, &["pane", "focus", "1", "-w", &workspace]);
    assert!(
        focus.status.success(),
        "Pane focus failed: {}",
        String::from_utf8_lossy(&focus.stderr)
    );
    assert!(focus.stdout.is_empty(), "successful focus should be quiet");
    let (_, panes) = uniterm_client::pane_list(&socket).unwrap();
    assert_eq!(
        panes.iter().find(|pane| pane.active).map(|pane| pane.id),
        Some(PaneId(1))
    );

    let missing = run_ut(
        &runtime,
        &state,
        &["pane", "focus", "999", "-w", &workspace],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no Pane 999"));

    uniterm_client::workspace_request(
        &socket,
        ClientMessage::ProjectSwitch {
            project: ProjectId(2),
        },
    )
    .unwrap();
    uniterm_client::control(
        &socket,
        ClientMessage::Command(MultiplexerCommand::NewWindow),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (_, panes) = uniterm_client::pane_list(&socket).unwrap();
        if panes.len() == 3 {
            break;
        }
        assert!(Instant::now() < deadline, "second Tab was not created");
        thread::sleep(Duration::from_millis(10));
    }
    uniterm_client::control(
        &socket,
        ClientMessage::Command(MultiplexerCommand::Split(SplitAxis::LeftRight)),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (_, panes) = uniterm_client::pane_list(&socket).unwrap();
        if panes.len() == 4 {
            break;
        }
        assert!(Instant::now() < deadline, "second Pane was not created");
        thread::sleep(Duration::from_millis(10));
    }

    let tab = run_ut(
        &runtime,
        &state,
        &["tab", "focus", "Second", "1", "-w", &workspace],
    );
    assert!(
        tab.status.success(),
        "Tab focus failed: {}",
        String::from_utf8_lossy(&tab.stderr)
    );
    assert!(tab.stdout.is_empty(), "successful focus should be quiet");
    let (_, panes) = uniterm_client::pane_list(&socket).unwrap();
    assert_eq!(
        panes.iter().find(|pane| pane.active).map(|pane| pane.id),
        Some(PaneId(2))
    );

    let named_tab = run_ut(
        &runtime,
        &state,
        &["tab", "focus", "2", "Tab 2", "-w", &workspace],
    );
    assert!(
        named_tab.status.success(),
        "named Tab focus failed: {}",
        String::from_utf8_lossy(&named_tab.stderr)
    );
    let (_, panes) = uniterm_client::pane_list(&socket).unwrap();
    assert_eq!(
        panes.iter().find(|pane| pane.active).map(|pane| pane.id),
        Some(PaneId(4)),
        "Tab focus should preserve its remembered active Pane"
    );

    let hierarchy = run_ut(
        &runtime,
        &state,
        &["pane", "focus", "Second", "2", "1", "-w", &workspace],
    );
    assert!(
        hierarchy.status.success(),
        "hierarchy Pane focus failed: {}",
        String::from_utf8_lossy(&hierarchy.stderr)
    );
    let (_, panes) = uniterm_client::pane_list(&socket).unwrap();
    assert_eq!(
        panes.iter().find(|pane| pane.active).map(|pane| pane.id),
        Some(PaneId(3))
    );

    let missing_tab = run_ut(
        &runtime,
        &state,
        &["tab", "focus", "Second", "9", "-w", &workspace],
    );
    assert!(!missing_tab.status.success());
    assert!(String::from_utf8_lossy(&missing_tab.stderr).contains("no Tab '9'"));

    let missing_pane = run_ut(
        &runtime,
        &state,
        &["pane", "focus", "Second", "2", "9", "-w", &workspace],
    );
    assert!(!missing_pane.status.success());
    assert!(String::from_utf8_lossy(&missing_pane.stderr).contains("no Pane 9"));

    uniterm_client::kill_server(&socket).unwrap();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(base);
}
