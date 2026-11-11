//! Protection migrates a stopped Workspace and locks recovery without a key.

use std::path::Path;
use std::process::{Command, Output};
mod common;

#[test]
fn protected_history_restores_with_key_and_locked_start_never_repairs_it() {
    let state = common::isolate_state();
    let runtime = common::socket_root().join(format!("privacy-cli-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let name = common::unique_workspace_name();
    let key = runtime.join("state.key");
    let wrong_key = runtime.join("wrong.key");
    let run = |args: &[&str], key: Option<&Path>| -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ut"));
        command
            .args(args)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", &runtime)
            .env("SHELL", "/bin/sh")
            .env_remove("UNITERM_STATE_KEY_FILE")
            .env_remove("UNITERM_SOCKET");
        if let Some(key) = key {
            command.env("UNITERM_STATE_KEY_FILE", key);
        }
        command.output().unwrap()
    };
    let ok = |args: &[&str], key| {
        let result = run(args, key);
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        result
    };
    ok(&["privacy", "keygen", key.to_str().unwrap()], None);
    ok(&["privacy", "keygen", wrong_key.to_str().unwrap()], None);
    ok(&["workspace", "new", "-d", &name], None);
    ok(
        &["today", "note", "1", "private-timeline-canary", "-w", &name],
        None,
    );
    let live_refusal = run(&["privacy", "protect", &name, key.to_str().unwrap()], None);
    assert!(!live_refusal.status.success());
    ok(&["workspace", "stop", &name], None);
    ok(&["privacy", "protect", &name, key.to_str().unwrap()], None);
    let log = state.join("uniterm").join(format!("{name}.log"));
    let encrypted = std::fs::read(&log).unwrap();
    assert!(!String::from_utf8_lossy(&encrypted).contains("private-timeline-canary"));
    for supplied in [None, Some(wrong_key.as_path())] {
        let result = run(&["serve", &name], supplied);
        assert!(!result.status.success());
        assert_eq!(
            std::fs::read(&log).unwrap(),
            encrypted,
            "locked startup must not quarantine or repair ciphertext"
        );
    }
    ok(&["workspace", "new", "-d", &name], Some(&key));
    let history = ok(&["today", "list", "--json", "-w", &name], None);
    assert!(String::from_utf8_lossy(&history.stdout).contains("private-timeline-canary"));
    ok(
        &["today", "note", "1", "another-private-canary", "-w", &name],
        None,
    );
    let socket = runtime.join("uniterm").join(format!("{name}.sock"));
    let anchor = uniterm_client::pane_list(&socket).unwrap().1[0]
        .id
        .0
        .to_string();
    let launch = ok(
        &[
            "agent",
            "start",
            "/bin/cat",
            "--pane",
            &anchor,
            "--tab",
            "--background",
            "-w",
            &name,
        ],
        None,
    );
    let pane = String::from_utf8(launch.stdout).unwrap();
    ok(
        &[
            "today",
            "prompt",
            pane.trim(),
            "private-prompt-canary",
            "--retain",
            "-w",
            &name,
        ],
        None,
    );
    let history = ok(&["today", "list", "--json", "-w", &name], None);
    assert!(String::from_utf8_lossy(&history.stdout).contains("private-prompt-canary"));
    ok(&["workspace", "stop", &name], None);
    fn private_files(path: &Path) {
        for item in std::fs::read_dir(path).unwrap() {
            let path = item.unwrap().path();
            if path.is_dir() {
                private_files(&path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                let text = String::from_utf8_lossy(&bytes);
                assert!(
                    !text.contains("private-timeline-canary")
                        && !text.contains("another-private-canary")
                        && !text.contains("private-prompt-canary"),
                    "plaintext leaked to {}",
                    path.display()
                );
            }
        }
    }
    private_files(&state.join("uniterm"));
    ok(&["workspace", "forget", &name], None);
    assert!(!log.exists());
    assert!(!state
        .join("uniterm")
        .join(format!("{name}.timeline"))
        .exists());
    assert!(key.exists(), "forget must not delete the external key");
}
