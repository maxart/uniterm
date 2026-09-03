//! A Workspace's durable files have exactly one owner, whatever runtime
//! directory each process resolves. The Aug 2026 startup failure came from a
//! second server that shared the state directory but not the socket lock.

use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;

use common::unique_workspace_name;

#[test]
fn a_second_server_under_another_runtime_dir_cannot_take_the_workspace() {
    let base = std::env::temp_dir().join(format!("uniterm-durable-lock-{}", std::process::id()));
    let state = base.join("state");
    let runtime_a = base.join("runtime-a");
    let runtime_b = base.join("runtime-b");
    for dir in [&state, &runtime_a, &runtime_b] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let workspace = unique_workspace_name();
    let mut first = Command::new(env!("CARGO_BIN_EXE_uniterm"))
        .args(["serve", &workspace])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime_a)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = runtime_a.join("uniterm").join(format!("{workspace}.sock"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "first server never bound its socket");

    let second = Command::new(env!("CARGO_BIN_EXE_uniterm"))
        .args(["serve", &workspace])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime_b)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(!second.status.success(), "second server started: {stderr}");
    assert!(stderr.contains("already running"), "{stderr}");
    assert!(
        !runtime_b
            .join("uniterm")
            .join(format!("{workspace}.sock"))
            .exists(),
        "second server left a socket behind"
    );
    // The first server is untouched.
    assert!(first.try_wait().unwrap().is_none());
    let _ = first.kill();
    let _ = first.wait();
    let _ = std::fs::remove_dir_all(base);
}
