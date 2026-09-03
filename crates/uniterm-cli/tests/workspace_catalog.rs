//! CLI projection of stopped Workspace definitions.

use std::process::Command;

use uniterm_core::ProjectId;
use uniterm_proto::{
    WorkspaceDefinition, WorkspaceLayoutDefinition, WorkspaceProjectDefinition,
    WorkspaceTabDefinition,
};

#[test]
fn list_includes_stopped_workspaces_and_forget_removes_them() {
    let base = std::env::temp_dir().join(format!(
        "uniterm-cli-workspace-catalog-{}",
        std::process::id()
    ));
    let state = base.join("state");
    let runtime = base.join("runtime");
    let config = base.join("config");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    std::env::set_var("XDG_STATE_HOME", &state);
    std::env::set_var("XDG_CONFIG_HOME", &config);
    let definition = WorkspaceDefinition {
        version: WorkspaceDefinition::VERSION,
        active_project: ProjectId(1),
        agent_scope_workspace: false,
        server_scope_workspace: false,
        projects: vec![WorkspaceProjectDefinition {
            id: ProjectId(1),
            name: "Site".into(),
            root: "/tmp/site".into(),
            worktree: None,
            active_tab: 0,
            tabs: vec![
                WorkspaceTabDefinition {
                    name: None,
                    layout: WorkspaceLayoutDefinition::Pane,
                },
                WorkspaceTabDefinition {
                    name: Some("Web".into()),
                    layout: WorkspaceLayoutDefinition::Pane,
                },
            ],
        }],
    };
    let line = uniterm_server::workspace_catalog::encode(&definition).unwrap();
    uniterm_server::workspace_catalog::append_line("remembered", &line).unwrap();

    let list = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["workspace", "list"])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(list.status.success());
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("remembered\tstopped, 1 Project, 2 Tabs"));

    let forget = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["workspace", "forget", "remembered"])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(forget.status.success());
    assert!(!uniterm_server::workspace_catalog::exists("remembered"));

    for name in ["alpha", "beta"] {
        let started = Command::new(env!("CARGO_BIN_EXE_ut"))
            .args(["workspace", "new", "-d", name])
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(
            started.status.success(),
            "failed to start {name}: {}",
            String::from_utf8_lossy(&started.stderr)
        );
    }

    let refused = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["workspace", "forget", "--all"])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("stop them first"));

    let stop = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["workspace", "stop", "--all"])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "bulk stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(String::from_utf8_lossy(&stop.stdout).contains("stopped 2 Workspaces"));
    assert!(uniterm_server::workspace_catalog::exists("alpha"));
    assert!(uniterm_server::workspace_catalog::exists("beta"));

    // Bulk forget includes orphaned recovery files, even if their catalog
    // definition is missing or damaged.
    uniterm_server::persist::save_bytes("snapshot-only", b"orphan").unwrap();
    uniterm_server::eventlog::append_line("log-only", "{}\n").unwrap();
    let forget_all = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["workspace", "forget", "--all"])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        forget_all.status.success(),
        "bulk forget failed: {}",
        String::from_utf8_lossy(&forget_all.stderr)
    );
    assert!(String::from_utf8_lossy(&forget_all.stdout).contains("forgot 4 Workspaces"));
    for name in ["alpha", "beta", "snapshot-only", "log-only"] {
        assert!(!uniterm_server::workspace_catalog::exists(name));
        assert!(!uniterm_server::persist::exists(name));
        assert!(!uniterm_server::eventlog::exists(name));
    }

    let set_default = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["workspace", "default", "Work"])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", &config)
        .output()
        .unwrap();
    assert!(set_default.status.success());
    let show_default = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["workspace", "default"])
        .env("XDG_CONFIG_HOME", &config)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&show_default.stdout).trim(), "Work");

    // A non-interactive bare invocation cannot attach, but it gets far enough
    // to create its selected Workspace. The preference must prevent the old
    // implicit `default` Workspace from appearing.
    let bare = Command::new(env!("CARGO_BIN_EXE_ut"))
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", &config)
        .output()
        .unwrap();
    assert!(!bare.status.success());
    assert!(runtime.join("uniterm/Work.sock").exists());
    assert!(!runtime.join("uniterm/default.sock").exists());

    let stop_default = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["workspace", "stop", "Work"])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", &config)
        .output()
        .unwrap();
    assert!(stop_default.status.success());
    let _ = std::fs::remove_dir_all(base);
}
