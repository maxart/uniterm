//! Synced breadcrumbs survive an actual process crash and torn event tail.
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
mod common;

#[test]
fn synced_day_history_survives_sigkill_and_prefix_repair() {
    let state = common::isolate_state();
    let runtime = common::socket_root().join(format!("today-crash-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let workspace = common::unique_workspace_name();
    let socket = runtime.join("uniterm").join(format!("{workspace}.sock"));
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ut"));
        command
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", &runtime)
            .env("SHELL", "/bin/sh")
            .env_remove("UNITERM_SOCKET")
            .env_remove("UNITERM_STATE_KEY_FILE");
        command
    };
    let start = || {
        command()
            .args(["serve", &workspace])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let query = || {
        uniterm_client::control_request(
            &socket,
            uniterm_proto::ControlCommand::TimelineQuery {
                date: "today".into(),
                filter: String::new(),
                before: None,
            },
        )
    };
    let wait = || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(response) = query() {
                if response.error.is_none() {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "isolated server did not become ready"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let mut server = start();
    wait();
    let note = command()
        .args([
            "today",
            "note",
            "1",
            "crash-resume-breadcrumb",
            "-w",
            &workspace,
        ])
        .output()
        .unwrap();
    assert!(
        note.status.success(),
        "{}",
        String::from_utf8_lossy(&note.stderr)
    );
    let before = serde_json::to_value(query().unwrap().result.unwrap()).unwrap();
    assert!(before.to_string().contains("crash-resume-breadcrumb"));
    server.kill().unwrap();
    server.wait().unwrap();
    let log = state.join("uniterm").join(format!("{workspace}.log"));
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(b"{\"torn_tail\"")
        .unwrap();
    let mut recovered = start();
    wait();
    let after = serde_json::to_value(query().unwrap().result.unwrap()).unwrap();
    assert!(after.to_string().contains("crash-resume-breadcrumb"));
    assert!(after
        .to_string()
        .contains("earlier open invocations may have been interrupted"));
    assert!(
        !after.to_string().contains("Visited Project"),
        "recovery alone is not a human Project visit"
    );
    let stopped = command()
        .args(["workspace", "stop", &workspace])
        .output()
        .unwrap();
    assert!(stopped.status.success());
    assert!(recovered.wait().unwrap().success());
}
