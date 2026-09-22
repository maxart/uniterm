//! End-to-end relay and waiting-queue contract.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{
    encode_frame, ClientMessage, FrameDecoder, OrchestrationKind, ServerMessage, SubmissionStatus,
    WaitingAction,
};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_dir() -> std::path::PathBuf {
    let dir = common::socket_root().join(format!("uniterm-relay-waiting-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for(path: &std::path::Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("server socket never appeared at {}", path.display());
}

fn read_messages(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
    timeout: Duration,
) -> Vec<ServerMessage> {
    let deadline = Instant::now() + timeout;
    let mut messages = Vec::new();
    let mut buf = [0u8; 32_768];
    while Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(count) => {
                decoder.push(&buf[..count]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    messages.push(message);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
    messages
}

fn rendered_text(messages: &[ServerMessage]) -> String {
    let mut text = String::new();
    for message in messages {
        if let ServerMessage::RenderOps(bytes) = message {
            text.push_str(&String::from_utf8_lossy(bytes));
        }
    }
    text
}

fn token_after(text: &str, marker: &str) -> Option<u64> {
    let start = text.match_indices(marker).map(|(index, _)| index).last()? + marker.len();
    let digits: String = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

#[test]
fn relay_needs_input_can_be_answered_and_then_completes() {
    isolate_state();
    let dir = temp_dir();
    let agent = dir.join("relayagent");
    std::fs::write(
        &agent,
        "#!/bin/sh\nprintf 'TOKEN %s\\n' \"$(printf '%s' \"$1\" | grep -o 'relay submit [0-9]*' | head -n1)\"\ncat > /dev/null\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let old_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{old_path}", dir.display()));

    let socket = dir.join(format!("{}.sock", unique_workspace_name()));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 30"], 200, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&socket);
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 200,
            rows: 30,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::NewTask {
            prompt: "implement and review".into(),
            relay: true,
            agent: Some("relayagent".into()),
            role_providers: Vec::new(),
            workflow: None,
            project: None,
        }))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let first = read_messages(&mut stream, &mut decoder, Duration::from_secs(3));
    let first_token =
        token_after(&rendered_text(&first), "TOKEN relay submit ").expect("first relay token");
    stream
        .write_all(&encode_frame(&ClientMessage::RunList {
            project: None,
            active_only: false,
        }))
        .unwrap();
    let first_graph = read_messages(&mut stream, &mut decoder, Duration::from_secs(1));
    let first_run = first_graph
        .iter()
        .find_map(|message| match message {
            ServerMessage::Runs { runs, .. } => runs.first(),
            _ => None,
        })
        .expect("active relay run");
    assert_eq!(first_run.kind, uniterm_core::RunKind::Relay);
    assert_eq!(first_run.status, uniterm_core::RunStatus::Active);
    assert_eq!(first_run.roles.len(), 2);
    assert!(first_run.roles[0]
        .activation
        .as_ref()
        .is_some_and(|activation| activation.active));
    let run_id = first_run.id;

    stream
        .write_all(&encode_frame(&ClientMessage::OrchestrationSubmit {
            kind: OrchestrationKind::Relay,
            token: first_token,
            status: SubmissionStatus::NeedsInput,
            verdict: None,
            summary: "Which compatibility target?".into(),
            artifacts: Vec::new(),
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::WaitingList))
        .unwrap();
    let waiting = read_messages(&mut stream, &mut decoder, Duration::from_secs(1));
    let item = waiting
        .iter()
        .find_map(|message| match message {
            ServerMessage::Waiting { items } => items.first(),
            _ => None,
        })
        .expect("relay waiting item");
    assert_eq!(item.kind, uniterm_core::WaitingKind::Relay);
    let waiting_id = item.id;

    stream
        .write_all(&encode_frame(&ClientMessage::WaitingAct {
            id: waiting_id,
            action: WaitingAction::Dismiss,
            text: String::new(),
        }))
        .unwrap();
    let rejected = read_messages(&mut stream, &mut decoder, Duration::from_secs(1));
    assert!(rejected.iter().any(|message| matches!(
        message,
        ServerMessage::WaitingActed {
            id,
            found: true,
            accepted: false,
            items,
        } if *id == waiting_id && items.iter().any(|item| item.id == waiting_id)
    )));

    stream
        .write_all(&encode_frame(&ClientMessage::WaitingAct {
            id: waiting_id,
            action: WaitingAction::Answer,
            text: "target glibc 2.17".into(),
        }))
        .unwrap();
    let acted = read_messages(&mut stream, &mut decoder, Duration::from_secs(1));
    assert!(acted.iter().any(|message| matches!(
        message,
        ServerMessage::WaitingActed {
            id,
            found: true,
            accepted: true,
            items,
        } if *id == waiting_id && items.is_empty()
    )));

    stream
        .write_all(&encode_frame(&ClientMessage::OrchestrationSubmit {
            kind: OrchestrationKind::Relay,
            token: first_token,
            status: SubmissionStatus::Done,
            verdict: None,
            summary: "implementation ready".into(),
            artifacts: Vec::new(),
        }))
        .unwrap();
    let second = read_messages(&mut stream, &mut decoder, Duration::from_secs(3));
    let second_token =
        token_after(&rendered_text(&second), "TOKEN relay submit ").expect("second relay token");
    assert_ne!(first_token, second_token);
    stream
        .write_all(&encode_frame(&ClientMessage::RunList {
            project: None,
            active_only: true,
        }))
        .unwrap();
    let handed_off = read_messages(&mut stream, &mut decoder, Duration::from_secs(1));
    let handed_off = handed_off
        .iter()
        .find_map(|message| match message {
            ServerMessage::Runs { runs, .. } => runs.first(),
            _ => None,
        })
        .expect("handed-off relay run");
    assert_eq!(handed_off.id, run_id);
    assert!(!handed_off.roles[0].activation.as_ref().unwrap().active);
    assert!(handed_off.roles[1].activation.as_ref().unwrap().active);

    stream
        .write_all(&encode_frame(&ClientMessage::OrchestrationSubmit {
            kind: OrchestrationKind::Relay,
            token: second_token,
            status: SubmissionStatus::Done,
            verdict: None,
            summary: "reviewed".into(),
            artifacts: Vec::new(),
        }))
        .unwrap();
    let done = read_messages(&mut stream, &mut decoder, Duration::from_secs(2));
    assert!(rendered_text(&done).contains("relay: done"));
    stream
        .write_all(&encode_frame(&ClientMessage::RunList {
            project: None,
            active_only: false,
        }))
        .unwrap();
    let completed = read_messages(&mut stream, &mut decoder, Duration::from_secs(1));
    assert!(completed.iter().any(|message| matches!(
        message,
        ServerMessage::Runs { runs, .. }
            if runs.iter().any(|run| run.id == run_id
                && run.status == uniterm_core::RunStatus::Completed
                && run.outcome.as_deref() == Some("done"))
    )));

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
}
