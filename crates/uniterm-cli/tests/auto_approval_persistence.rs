//! Isolated restart, crash and checkpoint coverage for saved Workspace consent.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use uniterm_proto::{
    encode_frame, ClientMessage, ControlCommand, ControlResult, FrameDecoder, ServerMessage,
    SettingsPatch, SettingsSnapshot,
};
mod common;

struct ServerProcess(std::process::Child);
impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn mode(socket: &Path, enabled: Option<bool>) -> (bool, bool) {
    let response = uniterm_client::control_request(
        socket,
        ControlCommand::AutoApprove {
            enabled,
            confirmed: false,
        },
    )
    .unwrap();
    assert!(response.error.is_none(), "{:?}", response.error);
    match response.result.unwrap() {
        ControlResult::AutoApproval {
            enabled, persist, ..
        } => (enabled, persist),
        other => panic!("unexpected {other:?}"),
    }
}

fn settings(socket: &Path, patch: Option<SettingsPatch>) -> SettingsSnapshot {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let saving = patch.is_some();
    stream
        .write_all(&encode_frame(
            &patch.map_or(ClientMessage::Settings, ClientMessage::SettingsApply),
        ))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let mut bytes = [0; 16384];
    loop {
        let n = stream.read(&mut bytes).unwrap();
        assert!(n > 0);
        decoder.push(&bytes[..n]);
        while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            if let ServerMessage::Settings {
                settings,
                saved,
                error,
            } = message
            {
                assert!(error.is_none(), "{error:?}");
                if !saving || saved {
                    return *settings;
                }
            }
        }
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "condition timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn approval_persistence_requires_opt_in_and_preserves_on_off_through_restart_crash_and_compaction()
{
    let state_home = common::isolate_state();
    let runtime = common::socket_root().join(format!("ap-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let workspace = common::unique_workspace_name();
    let socket = runtime.join("uniterm").join(format!("{workspace}.sock"));
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ut"));
        command
            .env("XDG_STATE_HOME", &state_home)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", &runtime)
            .env("SHELL", "/bin/sh")
            .env_remove("UNITERM_SOCKET")
            .env_remove("UNITERM_PANE_ID")
            .env_remove("UNITERM_STATE_KEY_FILE");
        command
    };
    let start = |name: &str| {
        let process = ServerProcess(
            command()
                .args(["serve", name])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let target = runtime.join("uniterm").join(format!("{name}.sock"));
        wait_until(|| uniterm_client::pane_list(&target).is_ok());
        process
    };
    let stop = |server: &mut ServerProcess| {
        uniterm_client::kill_server(&socket).unwrap();
        assert!(server.0.wait().unwrap().success());
    };
    let mut server = start(&workspace);
    assert_eq!(mode(&socket, None), (false, false));
    assert!(!settings(&socket, None).persist_auto_approve);
    assert_eq!(mode(&socket, Some(true)), (true, false));
    stop(&mut server);
    server = start(&workspace);
    assert_eq!(
        mode(&socket, None),
        (false, false),
        "default restart revokes ON"
    );

    // A prior ON event must not be resurrected by changing the preference.
    assert!(
        settings(
            &socket,
            Some(SettingsPatch {
                persist_auto_approve: Some(true),
                ..Default::default()
            })
        )
        .persist_auto_approve
    );
    assert_eq!(mode(&socket, None), (false, true));
    // A second attachment sees the saved preference.
    assert!(settings(&socket, None).persist_auto_approve);
    stop(&mut server);
    server = start(&workspace);
    assert_eq!(mode(&socket, None), (false, true));
    assert_eq!(mode(&socket, Some(true)), (true, true));
    stop(&mut server);
    assert!(uniterm_server::history::compact(&workspace).unwrap());
    assert!(uniterm_server::eventlog::replay_auto_approval(&workspace).unwrap());
    server = start(&workspace);
    assert_eq!(mode(&socket, None), (true, true));

    let output = command()
        .args(["agent", "auto-approve", "status", "-w", &workspace])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("saved across restarts"));
    // A shared config preference must never copy one Workspace's mode to another.
    let other = common::unique_workspace_name();
    let mut second = start(&other);
    let other_socket = runtime.join("uniterm").join(format!("{other}.sock"));
    assert_eq!(mode(&other_socket, None), (false, true));
    uniterm_client::kill_server(&other_socket).unwrap();
    assert!(second.0.wait().unwrap().success());

    // Crash after a committed ON, then after a committed OFF. Approval mode
    // comes from the event stream, independently of the terminal snapshot.
    wait_until(|| uniterm_server::eventlog::replay_auto_approval(&workspace).unwrap());
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    server = start(&workspace);
    assert_eq!(mode(&socket, None), (true, true));
    assert_eq!(mode(&socket, Some(false)), (false, true));
    assert_eq!(
        mode(&socket, Some(false)),
        (false, true),
        "OFF is idempotent"
    );
    wait_until(|| !uniterm_server::eventlog::replay_auto_approval(&workspace).unwrap());
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    assert!(uniterm_server::history::compact(&workspace).unwrap());
    assert!(!uniterm_server::eventlog::replay_auto_approval(&workspace).unwrap());
    server = start(&workspace);
    assert_eq!(mode(&socket, None), (false, true));
    stop(&mut server);
    server = start(&workspace);
    assert_eq!(mode(&socket, None), (false, true));

    // Restore controls Pane content, not this separate explicit preference.
    settings(
        &socket,
        Some(SettingsPatch {
            restore: Some(false),
            ..Default::default()
        }),
    );
    assert_eq!(mode(&socket, Some(true)), (true, true));
    stop(&mut server);
    server = start(&workspace);
    assert_eq!(mode(&socket, None), (true, true));
    settings(
        &socket,
        Some(SettingsPatch {
            persist_auto_approve: Some(false),
            ..Default::default()
        }),
    );
    assert_eq!(
        mode(&socket, None),
        (true, false),
        "opting out does not change the live mode"
    );
    stop(&mut server);
    server = start(&workspace);
    assert_eq!(mode(&socket, None), (false, false));
    settings(
        &socket,
        Some(SettingsPatch {
            persist_auto_approve: Some(true),
            ..Default::default()
        }),
    );
    assert_eq!(mode(&socket, None), (false, true));
    stop(&mut server);
}
