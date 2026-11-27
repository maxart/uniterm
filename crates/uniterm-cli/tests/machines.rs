//! Saved-machine metadata and SSH discovery never touch a live user Workspace.
mod common;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn invoke(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(args)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                root.join("bin").display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env_remove("UNITERM_SOCKET")
        .env_remove("UNITERM_PANE_ID")
        .output()
        .unwrap()
}
fn isolated() -> PathBuf {
    common::isolate_state();
    let root = common::temp_dir(&common::unique_workspace_name());
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::create_dir_all(root.join("runtime")).unwrap();
    root
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn saved_machines_crud_is_event_backed_and_reads_do_no_work() {
    let root = isolated();
    assert_eq!(
        success(invoke(&root, &["machine", "list", "--json"])).trim(),
        "[]"
    );
    assert!(!root.join("state/uniterm/machines.jsonl").exists());
    success(invoke(
        &root,
        &["machine", "add", "Build", "user@build", "agents"],
    ));
    success(invoke(&root, &["machine", "rename", "1", "Production"]));
    success(invoke(&root, &["machine", "disable", "1"]));
    success(invoke(&root, &["machine", "disable", "1"]));
    let profiles: serde_json::Value =
        serde_json::from_str(&success(invoke(&root, &["machine", "list", "--json"]))).unwrap();
    assert_eq!(profiles[0]["label"], "Production");
    assert_eq!(profiles[0]["enabled"], false);
    assert!(!invoke(&root, &["machine", "status", "1"]).status.success());
    let path = root.join("state/uniterm/machines.jsonl");
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 3);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    success(invoke(&root, &["machine", "remove", "1"]));
    success(invoke(&root, &["machine", "add", "Next", "next"]));
    let profiles: serde_json::Value =
        serde_json::from_str(&success(invoke(&root, &["machine", "list", "--json"]))).unwrap();
    assert_eq!(profiles[0]["id"], 5);
    assert!(
        !invoke(&root, &["machine", "add", "Bad", "-oProxyCommand=bad"])
            .status
            .success()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 5);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn saved_machine_discovery_reports_remote_sessions_without_starting_them() {
    let root = isolated();
    let ssh = root.join("bin/ssh");
    let payload = serde_json::json!({"protocol":uniterm_proto::WIRE_PROTOCOL_VERSION,"workspaces":[{"name":"agents","windows":3,"panes":4,"projects":2,"running":true},{"name":"archived","windows":1,"panes":0,"projects":1,"running":false}]}).to_string();
    std::fs::write(&ssh, format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", payload)).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    success(invoke(&root, &["machine", "add", "Build", "build"]));
    let result: serde_json::Value = serde_json::from_str(&success(invoke(
        &root,
        &["machine", "status", "Build", "--json"],
    )))
    .unwrap();
    assert_eq!(result["state"], "reachable_snapshot");
    assert_eq!(result["workspaces"][0]["panes"], 4);
    assert_eq!(result["workspaces"][1]["running"], false);
    std::fs::write(
        &ssh,
        "#!/bin/sh\necho 'Permission denied (publickey).' >&2\nexit 255\n",
    )
    .unwrap();
    let error = invoke(&root, &["machine", "status", "Build"]);
    assert!(!error.status.success());
    assert!(String::from_utf8_lossy(&error.stderr).contains("Permission denied"));
    assert_eq!(
        std::fs::read_to_string(root.join("state/uniterm/machines.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    std::fs::remove_dir_all(root).unwrap();
}
