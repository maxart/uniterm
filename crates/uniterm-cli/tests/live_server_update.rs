//! Exercise the actual source coordinator and replacement executable on private
//! runtime and state directories. The only fixture seam is release discovery.
mod common;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};

struct Fixture(Child, std::path::PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(mut c) = UnixStream::connect(&self.1) {
            let _ = c.write_all(&encode_frame(&ClientMessage::KillServer));
        }
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn launch(workspace: &str, _: &str, channel: UnixStream) -> std::io::Result<Option<Child>> {
    Command::new(env!("CARGO_BIN_EXE_uniterm"))
        .args(["--internal-live-server", workspace])
        .stdin(Stdio::from(OwnedFd::from(channel)))
        .spawn()
        .map(Some)
}

fn send(stream: &mut UnixStream, request: ClientMessage) {
    stream.write_all(&encode_frame(&request)).unwrap();
}

fn reply(stream: &mut UnixStream) -> ServerMessage {
    let mut decoder = FrameDecoder::new();
    let mut buf = [0; 8192];
    loop {
        let n = stream.read(&mut buf).unwrap();
        assert_ne!(n, 0, "serving engine closed its client");
        decoder.push(&buf[..n]);
        if let Some(response) = decoder.decode().unwrap() {
            return response;
        }
    }
}

fn upgrade(path: &Path, release: &str) -> Result<String, String> {
    let mut peer = UnixStream::connect(path).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(25)))
        .unwrap();
    send(
        &mut peer,
        ClientMessage::UpgradeServer {
            release: release.into(),
        },
    );
    match reply(&mut peer) {
        ServerMessage::ServerUpgradeResult(report) => report.error.map_or(Ok(report.version), Err),
        other => panic!("unexpected upgrade response: {other:?}"),
    }
}

#[test]
fn installed_engine_preserves_two_shells_and_an_open_client() {
    if let Some(name) = std::env::var_os("UNITERM_FIXTURE_SERVE") {
        uniterm_server::server::handoff::register_launcher(launch);
        uniterm_server::server::run_server(
            &uniterm_server::server::default_socket_path(name.to_str().unwrap()),
            "/bin/sh",
            &[],
        )
        .unwrap();
        return;
    }
    let state = common::isolate_state();
    let runtime = common::temp_dir("live-update");
    let name = common::unique_workspace_name();
    let path = runtime.join("uniterm").join(format!("{name}.sock"));
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "installed_engine_preserves_two_shells_and_an_open_client",
            "--nocapture",
        ])
        .env("UNITERM_FIXTURE_SERVE", &name)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .current_dir(&runtime)
        .spawn()
        .unwrap();
    let mut fixture = Fixture(child, path.clone());
    common::wait_for_socket(&path);
    let mut peer = UnixStream::connect(&path).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    send(
        &mut peer,
        ClientMessage::Command(uniterm_proto::Command::NewWindow),
    );
    send(&mut peer, ClientMessage::ListInfo);
    assert!(matches!(
        reply(&mut peer),
        ServerMessage::Info { panes: 2, .. }
    ));
    // Keep a pre-rename client open through transfer and connect the updater
    // through the new path. Both still have the original kernel bind address.
    let renamed = common::unique_workspace_name();
    send(
        &mut peer,
        ClientMessage::RenameSession {
            name: renamed.clone(),
        },
    );
    assert!(matches!(reply(&mut peer), ServerMessage::Workspace { name, .. } if name == renamed));
    let path = path.with_file_name(format!("{renamed}.sock"));
    common::wait_for_socket(&path);
    fixture.1 = path.clone();
    let panes = uniterm_client::pane_list(&path).unwrap().1;
    let record = |phase: &str| {
        for pane in &panes {
            let name = format!("{phase}-{}", pane.id.0);
            let command = format!("printf '%s' \"$$\" > {}/{}\n", runtime.display(), name);
            assert_eq!(
                uniterm_client::pane_send(&path, pane.id, command.into_bytes()).unwrap(),
                Some(true)
            );
            common::wait_for_socket(&runtime.join(name));
        }
    };
    record("before");
    assert!(upgrade(&path, "v0.0.0").unwrap_err().contains("release"));
    let version = uniterm_proto::BUILD_VERSION;
    let release = format!("v{version}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let started = Instant::now();
        match upgrade(&path, &release) {
            Ok(actual) => {
                eprintln!(
                    "Two-pane activation including preparation: {:?}",
                    started.elapsed()
                );
                assert_eq!(actual, version);
                break;
            }
            Err(error)
                if (error.contains("pending") || error.contains("operation in progress"))
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => panic!("upgrade failed: {error}"),
        }
    }
    assert!(fixture.0.wait().unwrap().success());
    send(&mut peer, ClientMessage::ListInfo);
    assert!(matches!(
        reply(&mut peer),
        ServerMessage::Info { panes: 2, .. }
    ));
    record("after");
    for pane in panes {
        let before = std::fs::read(runtime.join(format!("before-{}", pane.id.0))).unwrap();
        assert!(!before.is_empty());
        assert_eq!(
            before,
            std::fs::read(runtime.join(format!("after-{}", pane.id.0))).unwrap()
        );
    }
    send(&mut peer, ClientMessage::KillServer);
    let deadline = Instant::now() + Duration::from_secs(10);
    while path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!path.exists());
    std::fs::remove_dir_all(runtime).unwrap();
}

#[test]
#[ignore = "scripts/test-live-server-update.sh supplies independently built release images"]
fn public_update_verifies_the_installed_pair_before_switching_engines() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let release = std::path::PathBuf::from(
        std::env::var_os("UNITERM_TEST_RELEASE_DIR")
            .expect("run scripts/test-live-server-update.sh"),
    );
    let state = common::isolate_state();
    let runtime = common::temp_dir("release");
    let workspace = common::unique_workspace_name();
    let path = runtime.join("uniterm").join(format!("{workspace}.sock"));
    let install = runtime.join("bin");
    let tools = runtime.join("tools");
    std::fs::create_dir(&install).unwrap();
    std::fs::create_dir(&tools).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_uniterm"), install.join("uniterm")).unwrap();
    let version = env!("CARGO_PKG_VERSION");
    let tag = format!("v{version}");
    let curl = tools.join("curl");
    std::fs::write(
        &curl,
        r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
    case "$1" in --output) shift; output=$1 ;; esac
    url=$1
    shift
done
case "$url" in
    "https://api.github.com/repos/maxart/uniterm/releases/tags/$FIXTURE_TAG")
        printf '{"tag_name":"%s","draft":false,"prerelease":false}' "$FIXTURE_TAG" > "$output" ;;
    "https://github.com/maxart/uniterm/releases/download/$FIXTURE_TAG/SHA256SUMS")
        cat "$FIXTURE_SUMS" > "$output" ;;
    *) exit 76 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    let sums = runtime.join("sums");
    let log = std::fs::File::create(runtime.join("engine.log")).unwrap();
    let child = Command::new(install.join("uniterm"))
        .args(["serve", &workspace])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("SHELL", "/bin/sh")
        .env("FIXTURE_TAG", &tag)
        .env("FIXTURE_SUMS", &sums)
        .env(
            "PATH",
            std::env::join_paths(
                std::iter::once(tools)
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap(),
        )
        .current_dir(&runtime)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut fixture = Fixture(child, path.clone());
    common::wait_for_socket(&path);
    let pane = uniterm_client::pane_list(&path).unwrap().1[0].id;
    let run = |command: &str| {
        let bytes = format!("{command}\n").into_bytes();
        assert_eq!(
            uniterm_client::pane_send(&path, pane, bytes).unwrap(),
            Some(true)
        );
    };
    run("saved=retained; printf '%s %s' \"$$\" \"$saved\" > before");
    common::wait_for_socket(&runtime.join("before"));
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "macos-arm64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        _ => panic!("unqualified fixture platform"),
    };
    let mut lines = Vec::new();
    for name in ["uniterm", "ut"] {
        let staged = install.join("candidate");
        std::fs::copy(release.join(name), &staged).unwrap();
        let mut hash = Sha256::new();
        std::io::copy(&mut std::fs::File::open(&staged).unwrap(), &mut hash).unwrap();
        lines.push(format!("{:x}  {name}-{platform}\n", hash.finalize()));
        std::fs::rename(staged, install.join(name)).unwrap();
    }
    let request = |operation: &str| {
        let output = Command::new(install.join("ut"))
            .args(["update", operation, "--workspace", &workspace, "--json"])
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .unwrap_or_else(|_| panic!("updater failed: {output:?}"))
    };
    std::fs::write(
        &sums,
        format!("{}{}  ut-{platform}\n", lines[0], "0".repeat(64)),
    )
    .unwrap();
    let refusal = request("--activate-server");
    assert!(
        refusal["error"]
            .as_str()
            .unwrap()
            .contains("checksum mismatch for ut"),
        "{refusal}"
    );
    assert!(fixture.0.try_wait().unwrap().is_none());
    std::fs::write(sums, lines.concat()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let result = loop {
        let result = request("--activate-server");
        if result["activated"] == true {
            break result;
        }
        let error = result["error"].as_str().unwrap_or_default();
        assert!(
            Instant::now() < deadline
                && (error.contains("pending") || error.contains("operation in progress")),
            "{result}; engine: {}",
            std::fs::read_to_string(runtime.join("engine.log")).unwrap()
        );
    };
    assert_eq!(result["version"], version);
    assert!(fixture.0.wait().unwrap().success());
    assert_eq!(request("--status")["server"]["version"], version);
    let unchanged = request("--activate-server");
    assert_eq!(unchanged["activated"], false, "{unchanged}");
    assert!(unchanged["error"].is_null(), "{unchanged}");
    assert_eq!(unchanged["version"], version);
    run("printf '%s %s' \"$$\" \"$saved\" > after");
    common::wait_for_socket(&runtime.join("after"));
    let before = std::fs::read_to_string(runtime.join("before")).unwrap();
    assert!(before.ends_with(" retained"));
    assert_eq!(
        std::fs::read_to_string(runtime.join("after")).unwrap(),
        before
    );
    run("exit");
    let deadline = Instant::now() + Duration::from_secs(10);
    while path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!path.exists());
    std::fs::remove_dir_all(runtime).unwrap();
}
