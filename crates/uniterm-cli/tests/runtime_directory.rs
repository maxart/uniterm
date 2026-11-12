//! Stale login environments must resolve the same sockets in every CLI path.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output};

mod common;

fn invoke(state: &Path, config: &Path, runtime: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(args)
        .env("XDG_STATE_HOME", state)
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("SHELL", "/bin/sh")
        .env_remove("UNITERM_SOCKET")
        .output()
        .unwrap()
}

#[test]
fn stale_runtime_falls_back_for_start_attach_discovery_and_stop() {
    let state = common::isolate_state();
    let root = common::socket_root().join(format!("ut-xdg-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let valid = root.join("valid");
    std::fs::create_dir(&valid).unwrap();
    std::fs::set_permissions(&valid, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.join("config");
    std::fs::create_dir_all(config.join("uniterm")).unwrap();
    std::fs::write(
        config.join("uniterm/uniterm.conf"),
        "auto-update-connectors = false\nauto-install-connectors = false\n",
    )
    .unwrap();
    let file = root.join("file");
    std::fs::write(&file, b"keep").unwrap();
    let missing = root.join("missing");
    for runtime in [
        &missing,
        &file,
        Path::new(""),
        Path::new("relative/runtime"),
        &valid,
    ] {
        let name = common::unique_workspace_name();
        let run = |args: &[&str]| invoke(&state, &config, runtime, args);
        let start = run(&["workspace", "new", "-d", &name]);
        assert!(start.status.success(), "{start:?}");
        // Always stop before asserting query results so a regression cannot
        // leave the successfully launched fixture running after a panic.
        let listing = run(&["workspace", "list"]);
        let attach = run(&["attach", &name]);
        let duplicate = run(&["workspace", "new", "-d", &name]);
        // A different resolver must still respect the durable-state claim.
        let other = if runtime == valid { &missing } else { &valid };
        let collision = invoke(&state, &config, other, &["serve", &name]);
        let stop = run(&["workspace", "stop", &name]);
        assert!(stop.status.success(), "{stop:?}");
        assert!(listing.status.success(), "{listing:?}");
        assert!(String::from_utf8_lossy(&listing.stdout).contains(&name));
        assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already exists"));
        assert!(String::from_utf8_lossy(&collision.stderr).contains("already running"));
        let attach_error = String::from_utf8_lossy(&attach.stderr);
        assert!(
            attach_error.contains("uniterm attach: Inappropriate ioctl for device"),
            "attach should reach terminal setup on our non-TTY stdin: {attach:?}"
        );
        let warning = String::from_utf8_lossy(&start.stderr);
        if runtime == valid {
            assert!(warning.is_empty(), "{warning}");
        } else {
            assert_eq!(
                warning.matches("ignoring unusable XDG_RUNTIME_DIR").count(),
                1,
                "{warning}"
            );
            assert!(warning.contains("using"), "{warning}");
        }
    }
    assert!(!missing.exists(), "must not recreate a session directory");
    assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    // A valid runtime base is never repaired or chmodded by resolution.
    assert_eq!(
        std::fs::metadata(&valid).unwrap().permissions().mode() & 0o777,
        0o700
    );
    std::fs::remove_dir_all(root).unwrap();
}
