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

fn check_with_curl(script: &str) -> std::process::Output {
    let state = common::isolate_state();
    let runtime = common::temp_dir("update-fallback-runtime");
    let fixture = common::temp_dir("update-fallback-tools");
    let curl = fixture.join("curl");
    std::fs::write(&curl, script).unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["update", "--check", "--json"])
        .env("PATH", &fixture)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert_eq!(std::fs::read_dir(&runtime).unwrap().count(), 0);
    std::fs::remove_dir_all(fixture).unwrap();
    std::fs::remove_dir_all(runtime).unwrap();
    output
}

#[test]
fn update_check_recovers_from_api_403_using_public_stable_release_redirect() {
    let output = check_with_curl(
        r#"#!/bin/sh
[ "$1" = --disable ] || exit 70
head=false
while [ "$#" -gt 0 ]; do
  [ "$1" != --head ] || head=true
  last=$1
  shift
done
case "$last" in
  https://api.github.com/repos/maxart/uniterm/releases/latest)
    printf '403\n%s' "$last"
    printf 'curl: (56) The requested URL returned error: 403' >&2
    exit 56
    ;;
  https://github.com/maxart/uniterm/releases/latest)
    [ "$head" = true ] || exit 71
    printf '200\nhttps://github.com/maxart/uniterm/releases/tag/v9.9.9'
    ;;
  *) exit 72 ;;
esac
"#,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["latest"], "v9.9.9");
    assert_eq!(result["installed"], false);
}

#[test]
fn update_failure_identifies_both_blocked_endpoints_and_http_status() {
    let output = check_with_curl(
        r#"#!/bin/sh
for last do :; done
printf '403\n%s' "$last"
printf 'curl: (56) The requested URL returned error: 403' >&2
exit 56
"#,
    );
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("https://api.github.com/repos/maxart/uniterm/releases/latest"));
    assert!(error.contains("https://github.com/maxart/uniterm/releases/latest"));
    assert!(error.contains("HTTP 403"));
    assert!(error.contains("access restrictions or rate limiting"));
}
