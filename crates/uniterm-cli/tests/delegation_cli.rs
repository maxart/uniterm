//! CLI explanations must preserve Workspace approval and opt-out verdicts.
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::time::Duration;
use uniterm_core::{DelegateVerdict, PaneId};
use uniterm_proto::{ClientMessage, FrameDecoder, ServerMessage};
mod common;

#[test]
fn explain_reports_workspace_allow_and_exclusion_without_a_pane_arm() {
    let state = common::isolate_state();
    let root = common::socket_root().join(format!("ut-delegate-cli-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for (verdict, expected) in [
        (DelegateVerdict::Allow, "would auto-answer"),
        (
            DelegateVerdict::Withhold {
                reason: "This invocation is excluded from automatic approval; re-arm to resume"
                    .into(),
            },
            "excluded from automatic approval",
        ),
    ] {
        let socket = root.join(format!("{}.sock", common::unique_workspace_name()));
        let listener = UnixListener::bind(&socket).unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut decoder = FrameDecoder::new();
            let mut buf = [0; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                decoder.push(&buf[..n]);
                if let Some(message) = decoder.decode::<ClientMessage>().unwrap() {
                    assert!(matches!(
                        message,
                        ClientMessage::DelegationExplain { pane: PaneId(7) }
                    ));
                    stream
                        .write_all(&uniterm_proto::encode_frame(
                            &ServerMessage::DelegationExplained {
                                pane: PaneId(7),
                                found: false,
                                verdict,
                            },
                        ))
                        .unwrap();
                    return;
                }
            }
        });
        let output = Command::new(env!("CARGO_BIN_EXE_ut"))
            .args(["delegate", "explain", "7"])
            .env("UNITERM_SOCKET", &socket)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .output()
            .unwrap();
        worker.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(expected),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
