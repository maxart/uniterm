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
    // Grok imports both vendor hook files. They must stay silent there,
    // while the same installed commands still run for their native agent.
    let helper = root.join("hook-helper");
    let marker = root.join("hook-ran");
    std::fs::write(
        &helper,
        "#!/bin/sh\nprintf ran > \"$UNITERM_TEST_HOOK_MARKER\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (path, event, nested) in [
        (claude.join("settings.json"), "Stop", true),
        (root.join(".cursor/hooks.json"), "stop", false),
    ] {
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let entry = &value["hooks"][event][0];
        let command = if nested {
            &entry["hooks"][0]["command"]
        } else {
            &entry["command"]
        }
        .as_str()
        .unwrap();
        for imported in [true, false] {
            let mut shell = Command::new("/bin/sh");
            shell
                .args(["-c", command])
                .env("UNITERM", "1")
                .env("UNITERM_BIN", &helper)
                .env("UNITERM_TEST_HOOK_MARKER", &marker)
                .env("XDG_STATE_HOME", &state)
                .env("XDG_RUNTIME_DIR", root.join("runtime"));
            if imported {
                shell.env("GROK_HOOK_EVENT", "stop");
            } else {
                shell.env_remove("GROK_HOOK_EVENT");
            }
            assert!(shell.status().unwrap().success());
            assert_eq!(marker.exists(), !imported);
            if marker.exists() {
                std::fs::remove_file(&marker).unwrap();
            }
        }
    }
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

#[test]
fn opencode_install_selects_cli_contract_and_preserves_files_on_probe_failure() {
    let state = common::isolate_state();
    let root = common::temp_dir("opencode-contract");
    let bin = root.join("bin");
    let config = root.join("config");
    std::fs::create_dir_all(&bin).unwrap();
    let executable = bin.join("opencode");
    let set_version = |version: &str| {
        std::fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    let install = || {
        Command::new(env!("CARGO_BIN_EXE_ut"))
            .args(["agent", "connector", "install", "opencode"])
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("PATH", &bin)
            .output()
            .unwrap()
    };
    let plugins = config.join("opencode/plugins");
    let legacy = plugins.join("uniterm-notify.ts");
    let modern = plugins.join("uniterm-notify");
    set_version("1.2.27");
    assert!(install().status.success());
    assert!(std::fs::read_to_string(&legacy)
        .unwrap()
        .contains("export const UnitermNotify"));
    set_version("opencode v2.0.26");
    assert!(install().status.success());
    assert!(!legacy.exists());
    let installed = std::fs::read(modern.join("tui.ts")).unwrap();
    assert!(modern.join("index.ts").exists());
    set_version("unrecognized");
    assert!(!install().status.success());
    assert_eq!(std::fs::read(modern.join("tui.ts")).unwrap(), installed);
    assert!(!legacy.exists());
    set_version("1.2.27");
    assert!(install().status.success());
    assert!(legacy.exists());
    assert!(!modern.exists());
    std::fs::remove_dir_all(root).unwrap();
}
