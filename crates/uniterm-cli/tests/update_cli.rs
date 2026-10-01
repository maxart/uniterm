//! The public updater command checks once without starting a Workspace.
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
mod common;

#[test]
fn update_check_uses_public_release_metadata_and_never_downloads_binaries() {
    let state = common::isolate_state();
    let runtime = common::temp_dir("update-runtime");
    let fixture = common::temp_dir("update-tools");
    let curl = fixture.join("curl");
    std::fs::write(
        &curl,
        r#"#!/bin/sh
output=
while [ "$#" -gt 0 ]; do
  case "$1" in --output) shift; output=$1 ;; esac
  last=$1
  shift
done
[ "$last" = https://api.github.com/repos/maxart/uniterm/releases/latest ] || exit 71
[ -n "$output" ] || exit 72
printf '{"tag_name":"v9.9.9","draft":false,"prerelease":false}' > "$output"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["update", "--check", "--json"])
        .env("PATH", &fixture)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["latest"], "v9.9.9");
    assert_eq!(result["update_available"], true);
    assert_eq!(result["installed"], false);
    assert_eq!(std::fs::read_dir(&runtime).unwrap().count(), 0);
    std::fs::remove_dir_all(fixture).unwrap();
    std::fs::remove_dir_all(runtime).unwrap();
}
