//! Agent status comes from positive evidence: a cooperative signal or an
//! anchored screen match. Neither typing nor plain output may flip an idle
//! agent to working, and a cooperative working state must not be undone by a
//! screen that merely looks quiet.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_core::AgentStatus;
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn announce(agent: &str, event: &str) -> Vec<u8> {
    format!(
        "printf '\\033]777;notify;uniterm://cli-agent;{{\"agent\":\"{agent}\",\"event\":\"{event}\"}}\\007'\n"
    )
    .into_bytes()
}

struct Attached {
    stream: UnixStream,
    decoder: FrameDecoder,
}

impl Attached {
    fn start() -> (Self, thread::JoinHandle<()>) {
        isolate_state();
        let dir = common::socket_root().join(format!(
            "uniterm-agent-status-{}-{}",
            std::process::id(),
            unique_workspace_name()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join(format!("{}.sock", unique_workspace_name()));
        let server_socket = socket.clone();
        let server = thread::spawn(move || {
            let (mut server, mut poll) =
                Server::bind(&server_socket, "/bin/sh", &[], 120, 30).unwrap();
            let _ = server.run(&mut poll);
        });
        for _ in 0..300 {
            if socket.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut attached = Attached {
            stream,
            decoder: FrameDecoder::new(),
        };
        attached.send(ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 120,
            rows: 30,
        });
        (attached, server)
    }

    fn send(&mut self, message: ClientMessage) {
        self.stream.write_all(&encode_frame(&message)).unwrap();
    }

    fn type_line(&mut self, text: &str) {
        self.send(ClientMessage::Input(format!("{text}\n").into_bytes()));
    }

    /// The next full frame whose agent sidebar carries `expected`.
    fn full_frame_with(&mut self, expected: &str) -> String {
        self.frame_matching(expected, true)
    }

    /// The next frame, full or damage-only, carrying `expected`.
    fn frame_with(&mut self, expected: &str) -> String {
        self.frame_matching(expected, false)
    }

    fn frame_matching(&mut self, expected: &str, full: bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut buffer = [0u8; 65_536];
        while Instant::now() < deadline {
            match self.stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => self.decoder.push(&buffer[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("render read failed: {error}"),
            }
            while let Ok(Some(message)) = self.decoder.decode::<ServerMessage>() {
                if let ServerMessage::RenderOps(ops) = message {
                    let frame = String::from_utf8_lossy(&ops).into_owned();
                    if (!full || frame.contains("\x1b[r\x1b[2J")) && frame.contains(expected) {
                        return frame;
                    }
                }
            }
        }
        panic!("no full frame containing {expected:?}");
    }

    /// The fleet snapshot after `after`: status, authority, and evidence of
    /// every agent Pane, straight from the server rather than parsed chrome.
    fn fleet_after(&mut self, after: Duration) -> Vec<uniterm_proto::FleetEntry> {
        thread::sleep(after);
        self.send(ClientMessage::Observatory);
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut buffer = [0u8; 65_536];
        while Instant::now() < deadline {
            match self.stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => self.decoder.push(&buffer[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("fleet read failed: {error}"),
            }
            while let Ok(Some(message)) = self.decoder.decode::<ServerMessage>() {
                if let ServerMessage::Fleet { entries } = message {
                    return entries;
                }
            }
        }
        panic!("fleet snapshot did not arrive");
    }

    fn status_of(&mut self, agent: &str, after: Duration) -> (AgentStatus, String) {
        let entry = self
            .fleet_after(after)
            .into_iter()
            .find(|entry| entry.agent == agent)
            .unwrap_or_else(|| panic!("{agent} is not in the fleet"));
        (entry.status, entry.evidence)
    }
}

#[test]
fn typing_and_plain_output_leave_a_bound_agent_idle() {
    let (mut client, server) = Attached::start();
    client.send(ClientMessage::Input(announce("codex", "session_start")));
    client.full_frame_with("Codex");
    // The first screen verdict moves a cooperative `starting` to idle.
    let (status, evidence) = client.status_of("codex", Duration::from_millis(1200));
    assert_eq!(status, AgentStatus::Idle, "{evidence}");

    // Words that used to match the working rules, typed and echoed by the
    // shell, change nothing: no spinner, no anchored activity line.
    client.type_line("echo thinking about running command tests");
    client.frame_with("about running");
    let (status, evidence) = client.status_of("codex", Duration::from_millis(1200));
    assert_eq!(status, AgentStatus::Idle, "{evidence}");

    client.send(ClientMessage::KillServer);
    let _ = server.join();
}

#[test]
fn cooperative_working_outlives_typing_until_the_agent_reports_idle() {
    let (mut client, server) = Attached::start();
    client.send(ClientMessage::Input(announce("codex", "session_start")));
    client.full_frame_with("Codex");
    client.send(ClientMessage::Input(announce("codex", "prompt_submit")));
    let (status, evidence) = client.status_of("codex", Duration::from_millis(300));
    assert_eq!(status, AgentStatus::Working, "{evidence}");

    // A quiet screen and typed text are not evidence against the hook; the
    // stale fallback only applies after thirty seconds.
    client.type_line("echo still going");
    client.frame_with("still going");
    let (status, evidence) = client.status_of("codex", Duration::from_millis(1500));
    assert_eq!(status, AgentStatus::Working, "{evidence}");

    client.send(ClientMessage::Input(announce("codex", "idle")));
    let (status, evidence) = client.status_of("codex", Duration::from_millis(300));
    assert_eq!(status, AgentStatus::Idle, "{evidence}");

    client.send(ClientMessage::KillServer);
    let _ = server.join();
}

/// Read every message for `window`, returning the chimes in arrival order.
fn chimes_within(client: &mut Attached, window: Duration) -> Vec<ServerMessage> {
    let deadline = Instant::now() + window;
    let mut buffer = [0u8; 65_536];
    let mut chimes = Vec::new();
    while Instant::now() < deadline {
        match client.stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => client.decoder.push(&buffer[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("chime read failed: {error}"),
        }
        while let Ok(Some(message)) = client.decoder.decode::<ServerMessage>() {
            if matches!(message, ServerMessage::Chime { .. }) {
                chimes.push(message);
            }
        }
    }
    chimes
}

#[test]
fn a_permission_prompt_chimes_the_client_and_a_quiet_idle_does_not() {
    let (mut client, server) = Attached::start();
    client.send(ClientMessage::Input(announce("codex", "session_start")));
    client.full_frame_with("Codex");
    let (status, evidence) = client.status_of("codex", Duration::from_millis(1200));
    assert_eq!(status, AgentStatus::Idle, "{evidence}");

    // Attention rides the smoothed transition: the chime arrives after the
    // notification settles, carrying the Workspace's sound choice so the
    // client can play it wherever the human is attached from.
    client.send(ClientMessage::Input(announce(
        "codex",
        "permission_request",
    )));
    let chimes = chimes_within(&mut client, Duration::from_secs(10));
    assert_eq!(
        chimes.len(),
        1,
        "expected one attention chime, got {chimes:?}"
    );
    match &chimes[0] {
        ServerMessage::Chime {
            kind,
            sound,
            file,
            pane_active,
        } => {
            assert_eq!(*kind, uniterm_proto::ChimeKind::Attention);
            assert_eq!(*sound, uniterm_core::NotificationSound::Bell);
            assert!(file.is_empty());
            assert!(*pane_active, "the only Pane is the active one");
        }
        other => panic!("unexpected message {other:?}"),
    }

    // Completion notices are off by default, so settling back to idle is
    // silent: no chime for the no-op case.
    client.send(ClientMessage::Input(announce("codex", "idle")));
    let chimes = chimes_within(&mut client, Duration::from_millis(2500));
    assert!(chimes.is_empty(), "unexpected chimes {chimes:?}");

    client.send(ClientMessage::KillServer);
    let _ = server.join();
}
