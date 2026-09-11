//! Closed output consumers must never turn CLI queries into aborts/core dumps.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Command, Stdio};
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};

mod common;

fn closed_output() -> Stdio {
    let (reader, writer) = UnixStream::pair().unwrap();
    drop(reader);
    Stdio::from(OwnedFd::from(writer))
}

#[test]
fn closed_stdout_and_stderr_do_not_panic() {
    let state = common::isolate_state();
    let runtime = common::temp_dir("output-runtime");
    let protocol = uniterm_proto::WIRE_PROTOCOL_VERSION.to_string();
    for binary in [env!("CARGO_BIN_EXE_ut"), env!("CARGO_BIN_EXE_uniterm")] {
        for args in [
            vec!["--version"],
            vec!["--help"],
            vec!["--skill"],
            vec!["remote-check", "--protocol", protocol.as_str()],
        ] {
            let output = Command::new(binary)
                .args(&args)
                .env("XDG_STATE_HOME", &state)
                .env("XDG_RUNTIME_DIR", &runtime)
                .stdout(closed_output())
                .stderr(Stdio::piped())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {:?}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty());
        }
        let status = Command::new(binary)
            .arg("--invalid-option")
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .stdout(closed_output())
            .stderr(closed_output())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(2));
    }
    std::fs::remove_dir_all(runtime).unwrap();
}

#[test]
fn agent_explain_with_closed_stdout_returns_without_aborting() {
    let state = common::isolate_state();
    let runtime = common::socket_root().join(format!("ut-pipe-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let socket = runtime.join(format!("{}.sock", common::unique_workspace_name()));
    let listener = UnixListener::bind(&socket).unwrap();
    let responder = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut decoder = FrameDecoder::new();
        let mut buffer = [0; 4096];
        loop {
            let n = stream.read(&mut buffer).unwrap();
            assert_ne!(n, 0);
            decoder.push(&buffer[..n]);
            if let Some(message) = decoder.decode::<ClientMessage>().unwrap() {
                assert!(matches!(message, ClientMessage::AgentExplain { .. }));
                stream
                    .write_all(&encode_frame(&ServerMessage::AgentExplanation {
                        entries: Vec::new(),
                    }))
                    .unwrap();
                break;
            }
        }
    });
    let output = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["agent", "explain"])
        .env("UNITERM_SOCKET", &socket)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(closed_output())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    responder.join().unwrap();
    assert!(
        output.status.success(),
        "{:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    std::fs::remove_dir_all(runtime).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn full_stdout_reports_an_error_without_panicking() {
    let state = common::isolate_state();
    let runtime = common::temp_dir("output-full");
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ut"))
        .arg("--version")
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::from(full))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not write stdout"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    std::fs::remove_dir_all(runtime).unwrap();
}
