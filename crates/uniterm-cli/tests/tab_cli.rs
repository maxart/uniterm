//! CLI coverage for creating and naming Tabs by hierarchy position, the two
//! verbs the agent skill relies on to organise work without attaching.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::ClientMessage;
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
fn tabs_can_be_created_and_named_by_hierarchy_position() {
    let base = std::env::temp_dir().join(format!("uniterm-cli-tab-control-{}", std::process::id()));
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
    wait_for(&socket.with_extension("control.sock"));

    // The initial Project has one Tab; `tab new` adds a second and prints
    // its 1-based ordinal.
    let created = run_ut(&runtime, &state, &["tab", "new", "-w", &workspace]);
    assert!(
        created.status.success(),
        "tab new failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&created.stdout).trim(), "2");

    let panes = run_ut(
        &runtime,
        &state,
        &["pane", "list", "-w", &workspace, "--json"],
    );
    let panes: serde_json::Value = serde_json::from_slice(&panes.stdout).unwrap();
    let project_name = panes["panes"][0]["project_name"]
        .as_str()
        .unwrap()
        .to_string();

    let renamed = run_ut(
        &runtime,
        &state,
        &[
            "tab",
            "rename",
            &project_name,
            "2",
            "Review",
            "-w",
            &workspace,
        ],
    );
    assert!(
        renamed.status.success(),
        "tab rename failed: {}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    let panes = run_ut(
        &runtime,
        &state,
        &["pane", "list", "-w", &workspace, "--json"],
    );
    let panes: serde_json::Value = serde_json::from_slice(&panes.stdout).unwrap();
    let names: Vec<(u64, &str)> = panes["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pane| {
            (
                pane["tab"].as_u64().unwrap(),
                pane["tab_name"].as_str().unwrap(),
            )
        })
        .collect();
    assert!(names.contains(&(2, "Review")), "{names:?}");

    // The name now resolves as a Tab selector, and a missing Tab is an error.
    let focused = run_ut(
        &runtime,
        &state,
        &[
            "tab",
            "rename",
            &project_name,
            "Review",
            "Reviewed",
            "-w",
            &workspace,
        ],
    );
    assert!(focused.status.success());
    let missing = run_ut(
        &runtime,
        &state,
        &[
            "tab",
            "rename",
            &project_name,
            "9",
            "Nope",
            "-w",
            &workspace,
        ],
    );
    assert!(!missing.status.success());

    // The fleet listing the agent skill starts from is scriptable too, and
    // an empty Workspace reports an empty list rather than an error.
    let agents = run_ut(
        &runtime,
        &state,
        &["agent", "list", "-w", &workspace, "--json"],
    );
    assert!(
        agents.status.success(),
        "{}",
        String::from_utf8_lossy(&agents.stderr)
    );
    let agents: serde_json::Value = serde_json::from_slice(&agents.stdout).unwrap();
    assert_eq!(agents["workspace"], workspace);
    assert_eq!(agents["agents"], serde_json::json!([]));

    uniterm_client::control(&socket, ClientMessage::KillServer).unwrap();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(base);
}
