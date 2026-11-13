//! Provider resume and manager scope survive an actual server restart.
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
mod common;

struct ServerProcess {
    child: Child,
    stage: &'static str,
    stderr: PathBuf,
}

impl ServerProcess {
    fn diagnostics(&self) -> String {
        let mut text = String::new();
        match std::fs::File::open(&self.stderr)
            .and_then(|file| file.take(16 * 1024).read_to_string(&mut text))
        {
            Ok(_) => text,
            Err(error) => format!("could not read {}: {error}", self.stderr.display()),
        }
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn explicit_resume_preserves_focus_and_manager_scope_survives_crash() {
    let state = common::isolate_state();
    let runtime = common::socket_root().join(format!("today-resume-{}", std::process::id()));
    let bin = runtime.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let capture = runtime.join("launches");
    let fake = bin.join("codex");
    std::fs::write(&fake, format!(r#"#!/bin/sh
printf '%s|%s\n' "$UNITERM_SOCKET" "$*" >> {}
printf '\033]777;notify;uniterm://cli-agent;{{"agent":"codex","event":"prompt_submit","session_id":"test-session","resume_command":["codex","resume","test-session"]}}\007'
exec /bin/cat
"#, uniterm_server::workflow::shell_quote(&capture.to_string_lossy()))).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let workspace = common::unique_workspace_name();
    let socket = runtime.join("uniterm").join(format!("{workspace}.sock"));
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ut"));
        command
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", &runtime)
            .env("SHELL", "/bin/sh")
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env_remove("UNITERM_SOCKET")
            .env_remove("UNITERM_STATE_KEY_FILE");
        command
    };
    let start = |stage| {
        let stderr = runtime.join(format!("{stage}-server.stderr"));
        let child = command()
            .args(["serve", &workspace])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        ServerProcess {
            child,
            stage,
            stderr,
        }
    };
    let wait = |server: &mut ServerProcess| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = server.child.try_wait().unwrap() {
                panic!(
                    "{} server exited before readiness ({status}), socket {}: {}",
                    server.stage,
                    socket.display(),
                    server.diagnostics()
                );
            }
            let error = match uniterm_client::pane_list(&socket) {
                Ok(_) => return,
                Err(error) => error,
            };
            assert!(
                Instant::now() < deadline,
                "{} server readiness timed out, socket {}: {error}; stderr: {}\n{}",
                server.stage,
                socket.display(),
                server.diagnostics(),
                common::startup_diagnostics(server.child.id())
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let run = |args: &[&str]| {
        let output = command()
            .args(args)
            .args(["-w", &workspace])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let mut server = start("initial");
    wait(&mut server);
    let selected = uniterm_client::pane_list(&socket)
        .unwrap()
        .1
        .into_iter()
        .find(|p| p.active)
        .unwrap()
        .id;
    run(&[
        "agent",
        "start",
        "codex",
        "--tab",
        "--background",
        "--pane",
        &selected.0.to_string(),
    ]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let sequence = loop {
        let day: serde_json::Value =
            serde_json::from_str(&run(&["today", "list", "--json"])).unwrap();
        if let Some(entry) = day["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["session_id"] == "test-session")
        {
            break entry["sequence"].as_u64().unwrap();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    run(&["today", "resume", &sequence.to_string()]);
    assert_eq!(
        uniterm_client::pane_list(&socket)
            .unwrap()
            .1
            .into_iter()
            .find(|p| p.active)
            .unwrap()
            .id,
        selected
    );
    let manager_pane: u64 = run(&["today", "manager", "codex", "1", "--background"])
        .trim()
        .parse()
        .unwrap();
    let manager_socket = socket
        .parent()
        .unwrap()
        .join("manager")
        .join(socket.file_name().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let history = run(&["today", "list", "--json"]);
        let recorded = std::fs::read_to_string(&capture).unwrap_or_default();
        let history: serde_json::Value = serde_json::from_str(&history).unwrap();
        if recorded.contains(manager_socket.to_str().unwrap())
            && history["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["context"]["pane"] == manager_pane && e["session_id"] == "test-session")
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    // A durable note/query barrier flushes the preceding resume profiles.
    run(&["today", "note", "1", "scope-recovery-barrier"]);
    run(&["today", "list", "--json"]);
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    let mut recovered = start("recovered");
    wait(&mut recovered);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let recorded = std::fs::read_to_string(&capture).unwrap_or_default();
        if recorded
            .lines()
            .filter(|line| line.starts_with(manager_socket.to_str().unwrap()))
            .count()
            >= 2
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "read-only manager did not resume with its scope: {recorded}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let stopped = command()
        .args(["workspace", "stop", &workspace])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(recovered.child.wait().unwrap().success());
}
