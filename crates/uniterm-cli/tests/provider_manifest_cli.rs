use std::process::Command;

mod common;

#[test]
fn manifest_validation_is_offline_and_rejects_control_patterns() {
    let nonce = common::unique_nonce();
    let root = std::env::temp_dir().join(format!(
        "uniterm-manifest-cli-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("providers.json");
    let valid = r#"{
      "schema_version": 1,
      "manifest_version": "test-v1",
      "providers": [{
        "id": "test-agent",
        "executable_aliases": ["test-agent"],
        "capabilities": ["process", "screen"],
        "rules": [{
          "id": "screen.permission",
          "evidence": "screen",
          "status": "permission",
          "pattern": "approval required",
          "confidence": 90,
          "dwell_ms": 5000
        }]
      }]
    }"#;
    std::fs::write(&path, valid).unwrap();
    // The validator is offline, but it must not read or write the state of a
    // real Workspace either.
    let state = root.join("state");
    let runtime = root.join("runtime");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&runtime).unwrap();

    let accepted = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["agent", "manifests", "validate"])
        .arg(&path)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(accepted.status.success());
    assert!(String::from_utf8_lossy(&accepted.stdout).contains("valid manifest test-v1"));

    std::fs::write(&path, valid.replace("approval required", "ok\u{1b}[31m")).unwrap();
    let rejected = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["agent", "manifests", "validate"])
        .arg(&path)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("control character"));

    let _ = std::fs::remove_dir_all(root);
}
