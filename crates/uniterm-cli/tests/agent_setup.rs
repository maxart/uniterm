//! Bulk connector setup is explicit, idempotent, and scoped to installed CLIs.
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
mod common;

#[test]
fn install_all_connectors_preserves_user_settings_and_skips_missing_clis() {
    let state = common::isolate_state();
    let root = common::temp_dir("connectors-all");
    let bin = root.join("bin");
    let claude = root.join("claude");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(claude.join("settings.json"), r#"{"userSetting":42}"#).unwrap();
    for name in ["claude", "agent"] {
        let path = bin.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ut"))
            .args(args)
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("CODEX_HOME", root.join("codex"))
            .env("PATH", &bin)
            .output()
            .unwrap()
    };
    let first = run(&["agent", "connector", "install", "all"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let output = String::from_utf8_lossy(&first.stdout);
    assert!(output.contains("claude connector: installed"), "{output}");
    assert!(output.contains("cursor connector: installed"), "{output}");
    assert!(
        !root.join("codex").exists(),
        "missing CLIs must not receive integration files"
    );
    let settings = std::fs::read(claude.join("settings.json")).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&settings).unwrap()["userSetting"],
        42
    );
    assert!(run(&["agent", "connector", "install", "all"])
        .status
        .success());
    assert_eq!(
        std::fs::read(claude.join("settings.json")).unwrap(),
        settings
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundled_monitoring_skill_explains_stacks_and_blockers() {
    let state = common::isolate_state();
    let runtime = common::temp_dir("skill-output");
    let output = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["--skill", "monitoring"])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for required in [
        "name: manage-uniterm",
        "--stack --background",
        "capacity",
        "ut agent watch",
        "ut waiting answer",
    ] {
        assert!(text.contains(required), "missing {required}");
    }
    std::fs::remove_dir_all(runtime).unwrap();
}
