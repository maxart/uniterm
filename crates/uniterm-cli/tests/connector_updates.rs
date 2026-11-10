//! Connector policies use isolated provider homes and CLIs across every provider.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

struct Fixture {
    root: PathBuf,
    state: PathBuf,
}
impl Fixture {
    fn process(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ut"));
        command
            .args(args)
            .env("HOME", &self.root)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env("XDG_CONFIG_HOME", self.root.join(".config"))
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("CODEX_HOME", self.root.join(".codex"))
            .env("GROK_HOME", self.root.join(".grok"))
            .env("PI_CODING_AGENT_DIR", self.root.join(".pi/agent"))
            .env("PATH", self.root.join("bin"))
            .env_remove("UNITERM_SOCKET")
            .env_remove("UNITERM_PANE");
        command
    }
    fn command(&self, args: &[&str]) -> Output {
        self.process(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Output {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn policy(&self, update: bool, install: bool) {
        let path = self.root.join(".config/uniterm/uniterm.conf");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!("auto-update-connectors = {update}\nauto-install-connectors = {install}\n"),
        )
        .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn stale(path: &Path) {
    let text = fs::read_to_string(path).unwrap();
    if path.extension().is_some_and(|extension| extension == "ts") {
        fs::write(path, format!("{text}\n// previous build\n")).unwrap();
    } else {
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        fn age(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::String(text) if text.contains("uniterm://cli-agent") => {
                    text.push_str(" # previous build")
                }
                serde_json::Value::Array(values) => values.iter_mut().for_each(age),
                serde_json::Value::Object(values) => values.values_mut().for_each(age),
                _ => {}
            }
        }
        age(&mut value);
        value["user_setting"] = serde_json::json!("preserve me");
        value["hooks"]["UserEvent"] = serde_json::json!([{"command":"echo user-owned-hook"}]);
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }
}

#[test]
fn default_policies_install_all_providers_preserve_user_config_and_retry_failures() {
    let fixture = Fixture {
        root: common::temp_dir("connector-update"),
        state: common::isolate_state(),
    };
    fs::create_dir_all(fixture.root.join("bin")).unwrap();
    for provider in uniterm_core::agent::PROVIDERS {
        let path = fixture.root.join("bin").join(provider.command);
        fs::write(&path, "#!/bin/sh\nprintf '2.0.0\\n'\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Both off skips even provider lookup and lock creation on first use.
    fixture.policy(false, false);
    fixture.ok(&["agent", "connector", "auto-update"]);
    assert!(!fixture
        .root
        .join(".config/uniterm/connectors.lock")
        .exists());
    assert!(!fixture.root.join(".claude").exists());
    fs::remove_file(fixture.root.join(".config/uniterm/uniterm.conf")).unwrap();
    // With no saved config, both policies are on and all available CLIs get hooks.
    fixture.ok(&["agent", "connector", "auto-update"]);

    let paths: Vec<_> = [
        ".claude/settings.json",
        ".codex/hooks.json",
        ".gemini/settings.json",
        ".grok/hooks/uniterm-notify.json",
        ".kiro/agents/kiro_default.json",
        ".cursor/hooks.json",
        ".pi/agent/extensions/uniterm-notify.ts",
        ".config/opencode/plugins/uniterm-notify/index.ts",
    ]
    .iter()
    .map(|path| fixture.root.join(path))
    .collect();
    for path in &paths {
        stale(path);
    }
    let before: Vec<_> = paths.iter().map(|path| fs::read(path).unwrap()).collect();
    fixture.policy(false, false);
    fixture.ok(&["agent", "connector", "auto-update"]);
    for (path, bytes) in paths.iter().zip(&before) {
        assert_eq!(&fs::read(path).unwrap(), bytes);
    }
    let statuses = fixture.ok(&["agent", "connector", "status", "--json"]);
    let statuses: serde_json::Value = serde_json::from_slice(&statuses.stdout).unwrap();
    for entry in statuses["connectors"].as_array().unwrap() {
        assert_eq!(entry["connector"], "Outdated", "{entry}");
    }
    fixture.policy(true, false);
    // An overlapping updater skips safely; no success marker suppresses retry.
    {
        use std::os::fd::AsRawFd;
        let lock = fs::File::open(fixture.root.join(".config/uniterm/connectors.lock")).unwrap();
        // SAFETY: the owned test file remains open until after the child exits.
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        fixture.ok(&["agent", "connector", "auto-update"]);
        for (path, bytes) in paths.iter().zip(&before) {
            assert_eq!(&fs::read(path).unwrap(), bytes);
        }
    }
    let codex_config = fixture.root.join(".codex/config.toml");
    let config_before = fs::read(&codex_config).unwrap();
    fs::remove_file(&codex_config).unwrap();
    fs::create_dir(&codex_config).unwrap(); // Companion write must fail before hooks look current.
    fs::write(&paths[0], "{ broken uniterm://cli-agent").unwrap();
    let failed = fixture.command(&["agent", "connector", "auto-update"]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("claude"));
    assert!(String::from_utf8_lossy(&failed.stderr).contains("codex"));
    assert_eq!(fs::read(&paths[1]).unwrap(), before[1]);
    fs::remove_dir(&codex_config).unwrap();
    fs::write(&codex_config, config_before).unwrap();
    assert_eq!(
        fs::read_to_string(&paths[0]).unwrap(),
        "{ broken uniterm://cli-agent"
    );
    fs::write(&paths[0], &before[0]).unwrap();
    fixture.ok(&["agent", "connector", "auto-update"]);
    let statuses = fixture.ok(&["agent", "connector", "status", "--json"]);
    let statuses: serde_json::Value = serde_json::from_slice(&statuses.stdout).unwrap();
    for entry in statuses["connectors"].as_array().unwrap() {
        assert_eq!(entry["connector"], "Installed", "{entry}");
    }
    for path in &paths[..6] {
        let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(value["user_setting"], "preserve me", "{}", path.display());
        assert_eq!(
            value["hooks"]["UserEvent"][0]["command"],
            "echo user-owned-hook"
        );
    }
    let times: Vec<_> = paths
        .iter()
        .map(|path| fs::metadata(path).unwrap().modified().unwrap())
        .collect();
    fixture.ok(&["agent", "connector", "auto-update"]);
    for (path, before) in paths.iter().zip(times) {
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), before);
    }
    // Disabling auto-install preserves a manual removal; missing CLIs are skipped.
    fixture.ok(&["agent", "connector", "remove", "codex"]);
    stale(&paths[6]);
    fs::remove_file(fixture.root.join("bin/pi")).unwrap();
    let pi_before = fs::read(&paths[6]).unwrap();
    fixture.ok(&["agent", "connector", "auto-update"]);
    assert_eq!(fs::read(&paths[6]).unwrap(), pi_before);
    assert!(!fs::read_to_string(&paths[1])
        .unwrap()
        .contains("uniterm://cli-agent"));
    // Install on / update off repairs the missing connector but leaves old hooks alone.
    fixture.policy(false, true);
    stale(&paths[0]);
    let claude_before = fs::read(&paths[0]).unwrap();
    fixture.ok(&["agent", "connector", "auto-update"]);
    assert!(fs::read_to_string(&paths[1])
        .unwrap()
        .contains("uniterm://cli-agent"));
    assert_eq!(fs::read(&paths[0]).unwrap(), claude_before);
    assert_eq!(fs::read(&paths[6]).unwrap(), pi_before);
    fixture.policy(true, true);
    fixture.ok(&["agent", "connector", "auto-update"]);
    assert!(!fs::read_to_string(&paths[0])
        .unwrap()
        .contains("previous build"));
}

#[test]
fn startup_refresh_and_saved_settings_share_the_same_policy() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::process::{Child, Stdio};
    use std::time::Duration;
    use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage, SettingsPatch};
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let fixture = Fixture {
        root: common::temp_dir("startup-connectors"),
        state: common::isolate_state(),
    };
    let runtime = common::socket_root().join(format!("cu-{}", common::unique_nonce()));
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(fixture.root.join("bin")).unwrap();
    let cli = fixture.root.join("bin/claude");
    fs::write(&cli, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(cli, fs::Permissions::from_mode(0o755)).unwrap();
    fixture.ok(&["agent", "connector", "install", "claude"]);
    let hooks = fixture.root.join(".claude/settings.json");
    stale(&hooks);
    fixture.policy(true, false);
    let workspace = common::unique_workspace_name();
    let mut child = ChildGuard(
        fixture
            .process(&["serve", &workspace])
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("SHELL", "/bin/sh")
            .current_dir(&fixture.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let socket = runtime.join("uniterm").join(format!("{workspace}.sock"));
    common::wait_for_socket(&socket);
    assert!(!fs::read_to_string(&hooks)
        .unwrap()
        .contains("previous build"));
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    for enabled in [false, true] {
        stream
            .write_all(&encode_frame(&ClientMessage::SettingsApply(
                SettingsPatch {
                    auto_update_connectors: Some(enabled),
                    auto_install_connectors: Some(!enabled),
                    ..Default::default()
                },
            )))
            .unwrap();
        'saved: loop {
            while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
                if let ServerMessage::Settings {
                    settings,
                    saved: true,
                    error,
                } = message
                {
                    assert!(error.is_none(), "{error:?}");
                    assert_eq!(settings.auto_update_connectors, enabled);
                    assert_eq!(settings.auto_install_connectors, !enabled);
                    let text =
                        fs::read_to_string(fixture.root.join(".config/uniterm/uniterm.conf"))
                            .unwrap();
                    assert_eq!(
                        uniterm_core::Config::parse(&text).auto_update_connectors,
                        enabled
                    );
                    assert_eq!(
                        uniterm_core::Config::parse(&text).auto_install_connectors,
                        !enabled
                    );
                    break 'saved;
                }
            }
            let mut bytes = [0; 65536];
            let count = stream.read(&mut bytes).unwrap();
            assert_ne!(count, 0);
            decoder.push(&bytes[..count]);
        }
    }
    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    assert!(child.0.wait().unwrap().success());
    fs::remove_dir_all(runtime).unwrap();
}
