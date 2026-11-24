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

#[test]
fn offline_status_queries_only_the_selected_server_and_accepts_legacy_replies() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use uniterm_proto::{
        ControlCommand, ControlFrame, ControlRequest, ControlResponse, ControlResult,
        WorkspaceBuild,
    };
    let state = common::isolate_state();
    let runtime = common::temp_dir("status");
    let workspace = common::unique_workspace_name();
    let dir = runtime.join("uniterm");
    std::fs::create_dir(&dir).unwrap();
    let control = dir.join(format!("{workspace}.control.sock"));
    let listener = UnixListener::bind(control).unwrap();
    let selected = workspace.clone();
    let server = std::thread::spawn(move || {
        for build in [
            Some(WorkspaceBuild {
                live_handoff: None,
                version: "1.0.0".into(),
                wire_protocol: 7,
            }),
            None,
        ] {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: ControlRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(request.workspace, selected);
            assert!(matches!(request.command, ControlCommand::Capabilities));
            let reply = ControlFrame::Response(ControlResponse::ok(
                request.id,
                ControlResult::Capabilities {
                    protocol_version: 1,
                    server_build: build,
                    capabilities: vec![],
                    max_frame_bytes: 1048576,
                    max_connections: 8,
                    max_queued_frames: 8,
                    max_queued_requests: 8,
                },
            ));
            serde_json::to_writer(reader.get_mut(), &reply).unwrap();
            reader.get_mut().write_all(b"\n").unwrap();
        }
    });
    for expected in [Some("1.0.0"), None] {
        let output = Command::new(env!("CARGO_BIN_EXE_ut"))
            .args(["update", "--status", "--workspace", &workspace, "--json"])
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("UNITERM_SOCKET", dir.join("other.sock"))
            .env("PATH", &runtime)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["workspace"], workspace);
        assert_eq!(status["cli_version"], uniterm_proto::BUILD_VERSION);
        assert_eq!(status["server"]["version"].as_str(), expected);
        assert_eq!(
            status["server"]["wire_protocol"].as_u64(),
            expected.map(|_| 7)
        );
    }
    server.join().unwrap();
    std::fs::remove_dir_all(runtime).unwrap();
}

#[test]
fn missing_activation_acknowledgement_reports_an_unknown_outcome() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixListener;
    use uniterm_proto::{
        ControlFrame, ControlRequest, ControlResponse, ControlResult, WorkspaceBuild,
    };
    let state = common::isolate_state();
    let runtime = common::temp_dir("lost-ack");
    let workspace = common::unique_workspace_name();
    let dir = runtime.join("uniterm");
    std::fs::create_dir(&dir).unwrap();
    let control = UnixListener::bind(dir.join(format!("{workspace}.control.sock"))).unwrap();
    let attach = UnixListener::bind(dir.join(format!("{workspace}.sock"))).unwrap();
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(control.accept().unwrap().0);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: ControlRequest = serde_json::from_str(&line).unwrap();
        let reply = ControlFrame::Response(ControlResponse::ok(
            request.id,
            ControlResult::Capabilities {
                protocol_version: 1,
                server_build: Some(WorkspaceBuild {
                    version: "1.0.0".into(),
                    wire_protocol: uniterm_proto::WIRE_PROTOCOL_VERSION,
                    live_handoff: Some(uniterm_server::server::handoff::CHECKPOINT_VERSION),
                }),
                capabilities: vec![],
                max_frame_bytes: 1048576,
                max_connections: 8,
                max_queued_frames: 8,
                max_queued_requests: 8,
            },
        ));
        serde_json::to_writer(reader.get_mut(), &reply).unwrap();
        reader.get_mut().write_all(b"\n").unwrap();
        let mut client = attach.accept().unwrap().0;
        let expected = uniterm_proto::encode_frame(&uniterm_proto::ClientMessage::UpgradeServer {
            release: "v9.9.9".into(),
        });
        let mut bytes = vec![0; expected.len()];
        client.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, expected);
        // EOF after receipt cannot tell the caller which process owns the PTYs.
    });
    let output = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args([
            "update",
            "--activate-server",
            "--workspace",
            &workspace,
            "--version",
            "9.9.9",
            "--json",
        ])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    server.join().unwrap();
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!output.status.success());
    assert_eq!(result["workspace"], workspace);
    assert_eq!(result.get("activated"), Some(&serde_json::Value::Null));
    assert!(result["error"]
        .as_str()
        .unwrap()
        .contains("outcome unknown"));
    std::fs::remove_dir_all(runtime).unwrap();
}
