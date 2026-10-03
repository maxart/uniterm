//! Ordinary CLI startup must finish replay and preserve a large hierarchy.

use std::io::Write;
use std::process::{Command, Output};
use std::time::Instant;

use uniterm_core::{LayoutNode, PaneId, ProjectId};
use uniterm_proto::{
    WorkspaceDefinition, WorkspaceLayoutDefinition, WorkspaceProjectDefinition,
    WorkspaceTabDefinition,
};
use uniterm_server::eventlog::{
    EventEnvelope, LogEvent, StructuralPane, StructuralProject, StructuralProjection,
    StructuralWindow, EVENT_VERSION,
};

mod common;

#[test]
fn ordinary_start_restores_23_projects_and_44_tabs_and_restarts_after_clean_stop() {
    let state = common::isolate_state();
    let name = common::unique_workspace_name();
    let runtime = common::socket_root().join(format!("ut-start-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let config = state.join("config");
    std::fs::create_dir_all(config.join("uniterm")).unwrap();
    std::fs::write(
        config.join("uniterm/uniterm.conf"),
        format!("default-workspace = {name}\n"),
    )
    .unwrap();
    let projects: Vec<_> = (1..=23)
        .map(|id| WorkspaceProjectDefinition {
            id: ProjectId(id),
            name: format!("Project {id}"),
            root: runtime.to_string_lossy().into_owned(),
            worktree: None,
            active_tab: 0,
            tabs: (0..if id <= 21 { 2 } else { 1 })
                .map(|tab| WorkspaceTabDefinition {
                    name: Some(format!("Tab {tab}")),
                    layout: WorkspaceLayoutDefinition::Pane,
                })
                .collect(),
        })
        .collect();
    let definition = WorkspaceDefinition {
        version: WorkspaceDefinition::VERSION,
        active_project: ProjectId(1),
        agent_scope_workspace: false,
        server_scope_workspace: false,
        projects,
    };
    uniterm_server::workspace_catalog::append_line(
        &name,
        &uniterm_server::workspace_catalog::encode(&definition).unwrap(),
    )
    .unwrap();
    let mut pane = 0;
    let projection = StructuralProjection {
        active_window: 0,
        next_pane_id: 45,
        active_project: ProjectId(1),
        next_project_id: 24,
        projects: definition
            .projects
            .iter()
            .map(|project| StructuralProject {
                id: project.id,
                name: project.name.clone(),
                root: project.root.clone(),
                active_pane: None,
                metadata: vec![],
            })
            .collect(),
        windows: definition
            .projects
            .iter()
            .flat_map(|project| {
                project
                    .tabs
                    .iter()
                    .map(|tab| {
                        pane += 1;
                        StructuralWindow {
                            project: project.id,
                            layout: LayoutNode::Leaf(PaneId(pane)),
                            active: PaneId(pane),
                            zoomed: None,
                            name: tab.name.clone(),
                            panes: vec![StructuralPane {
                                id: PaneId(pane),
                                cwd: Some(project.root.clone()),
                                metadata: vec![],
                                launch_args: vec![],
                                agent_launch: None,
                            }],
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect(),
    };
    // Set UNITERM_STARTUP_EVENTS=458498 for the reported lifetime-history
    // scale without imposing a half-gigabyte fixture on every CI run.
    let events: u64 = std::env::var("UNITERM_STARTUP_EVENTS")
        .ok()
        .map(|n| n.parse().unwrap())
        .unwrap_or(64);
    let path = state.join("uniterm").join(format!("{name}.log"));
    let mut log = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    for sequence in 1..=events {
        let event = if sequence % 16 == 0 {
            LogEvent::WorkspaceProjected {
                state: projection.clone(),
            }
        } else {
            LogEvent::PaneSpawned { pane: sequence }
        };
        serde_json::to_writer(
            &mut log,
            &EventEnvelope {
                version: EVENT_VERSION,
                sequence,
                timestamp_ms: 0,
                workspace: name.clone(),
                event,
            },
        )
        .unwrap();
        log.write_all(b"\n").unwrap();
    }
    log.flush().unwrap();
    drop(log);
    let original_size = std::fs::metadata(&path).unwrap().len();
    let command = |binary: &str, args: &[&str]| -> Output {
        Command::new(binary)
            .args(args)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", &config)
            .env("SHELL", "/bin/sh")
            .output()
            .unwrap()
    };
    let socket = runtime.join("uniterm").join(format!("{name}.sock"));
    for binary in [env!("CARGO_BIN_EXE_ut"), env!("CARGO_BIN_EXE_uniterm")] {
        let start = Instant::now();
        // No terminal is attached to this test, so attach itself fails after
        // startup. The detached server must survive that caller's exit.
        let output = command(binary, &[]);
        let info = uniterm_client::query_info(&socket);
        // The same entry point must also reuse an already running server.
        let second = command(binary, &[]);
        let listing = command(binary, &["workspace", "list"]);
        let stop = command(binary, &["workspace", "stop", &name]);
        eprintln!(
            "restored {events} events ({original_size} bytes) and stopped in {:?}",
            start.elapsed()
        );
        assert_eq!(
            info.unwrap_or_else(|e| panic!(
                "startup: {e}; {}",
                String::from_utf8_lossy(&output.stderr)
            )),
            (44, 44)
        );
        assert!(String::from_utf8_lossy(&listing.stdout).contains("23 Projects, 44 Tabs"));
        assert!(
            stop.status.success(),
            "{}",
            String::from_utf8_lossy(&stop.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("exited during startup"));
        assert!(!String::from_utf8_lossy(&second.stderr).contains("exited during startup"));
        assert!(
            std::fs::metadata(&path).unwrap().len() >= original_size,
            "clean stop must retain durable history"
        );
    }
    std::fs::remove_dir_all(runtime).unwrap();
}

#[test]
fn failed_recovery_reports_the_error_without_modifying_a_future_log() {
    let state = common::isolate_state();
    let name = common::unique_workspace_name();
    let runtime = common::socket_root().join(format!("ut-fail-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let original = format!("{{\"version\":999,\"sequence\":1,\"timestamp_ms\":0,\"workspace\":\"{name}\",\"event\":{{\"PaneSpawned\":{{\"pane\":1}}}}}}\n");
    uniterm_server::eventlog::append_line(&name, &original).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["workspace", "new", "-d", &name])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", &runtime)
        .env("SHELL", "/bin/sh")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("exited during startup"), "{message}");
    assert!(
        message.contains("unsupported event version 999"),
        "{message}"
    );
    assert_eq!(
        std::fs::read_to_string(state.join("uniterm").join(format!("{name}.log"))).unwrap(),
        original
    );
    std::fs::remove_dir_all(runtime).unwrap();
}
