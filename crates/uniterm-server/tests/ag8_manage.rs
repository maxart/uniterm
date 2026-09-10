//! AG8 integration: the Manage Agents snapshot over a real socket.
//!
//! A client sends `Agents` and must get back one row per registry provider,
//! each with a connector status; `AgentsStopAll` with nothing running is a
//! safe no-op that still answers with a fresh snapshot.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_sock(tag: &str) -> std::path::PathBuf {
    let dir = common::socket_root().join(format!("uniterm-it-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{}.sock", unique_workspace_name()))
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

#[test]
fn agents_snapshot_lists_every_provider() {
    isolate_state();
    let sock = temp_sock("agents");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut s, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", "sleep 3"], 80, 24).unwrap();
        let _ = s.run(&mut poll);
    });

    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Agents))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::AgentsStopAll {
            scope: uniterm_proto::StopScope::Session,
            confirmed: true,
        }))
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 16384];
    let mut snapshots = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && snapshots < 2 {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                dec.push(&buf[..n]);
                while let Ok(Some(msg)) = dec.decode::<ServerMessage>() {
                    if let ServerMessage::Agents { items } = msg {
                        snapshots += 1;
                        assert_eq!(items.len(), uniterm_core::agent::PROVIDERS.len());
                        let claude = items.iter().find(|a| a.id == "claude").unwrap();
                        assert_eq!(claude.name, "Claude Code");
                        assert_eq!(claude.running, 0);
                        let cursor = items.iter().find(|a| a.id == "cursor").unwrap();
                        assert_eq!(cursor.name, "Cursor Agent");
                        assert_eq!(cursor.command, "agent");
                        // Every provider ported from the Tauri app has a
                        // connector arm (its state depends on this machine).
                        assert!(items
                            .iter()
                            .all(|a| a.connector != uniterm_proto::ConnectorStatus::Unsupported));
                        assert!(items.iter().any(|a| a.id == "kiro"));
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    // Kill the pane so the server thread ends promptly.
    let _ = stream.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = server.join();
    assert_eq!(
        snapshots, 2,
        "expected snapshots for both Agents and AgentsStopAll"
    );
}

#[test]
fn launched_agent_is_bound_and_listed_without_a_connector() {
    isolate_state();
    // The user's bug: agents started through uniterm showed neither a fleet
    // entry nor a border because binding waited for OSC 777 from a connector.
    // Launch binds immediately now; the Observatory must list it.
    let sock = temp_sock("launch-bind");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut s, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", "sleep 3"], 80, 24).unwrap();
        let _ = s.run(&mut poll);
    });

    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        }))
        .unwrap();
    // "sh" is not a registry agent but is on PATH, so it launches as a
    // custom agent and must still bind under its own name.
    stream
        .write_all(&encode_frame(&ClientMessage::AgentLaunch {
            agent: "sh".into(),
            target: uniterm_proto::LaunchTarget::NewPane,
        }))
        .unwrap();
    // Same connection: the server handles messages in order, so the fleet
    // snapshot cannot race the launch.
    stream
        .write_all(&encode_frame(&ClientMessage::Observatory))
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 16384];
    let mut fleet: Option<Vec<uniterm_proto::FleetEntry>> = None;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && fleet.is_none() {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                dec.push(&buf[..n]);
                while let Ok(Some(msg)) = dec.decode::<ServerMessage>() {
                    if let ServerMessage::Fleet { entries } = msg {
                        fleet = Some(entries);
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    let _ = stream.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = server.join();
    let fleet = fleet.expect("no Fleet reply");
    assert_eq!(
        fleet.len(),
        1,
        "launched agent missing from fleet: {fleet:?}"
    );
    assert_eq!(fleet[0].agent, "sh");
}

#[test]
fn remote_environment_refreshes_a_live_servers_detection_and_launch_path() {
    isolate_state();
    let sock = temp_sock("remote-path");
    let bin = sock.parent().unwrap().join("remote-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(&claude, "#!/bin/sh\nsleep 2\n").unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();

    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", "sleep 5"], 80, 24).unwrap();
        let _ = server.run(&mut poll);
    });

    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::RemoteEnvironment {
            search_path: vec![bin.to_string_lossy().into_owned()],
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Agents))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::AgentLaunch {
            agent: "claude".into(),
            target: uniterm_proto::LaunchTarget::NewPane,
        }))
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    let mut decoder = FrameDecoder::new();
    let mut buffer = [0u8; 16384];
    let mut installed = false;
    let mut launched = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !(installed && launched) {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                decoder.push(&buffer[..read]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    match message {
                        ServerMessage::Agents { items } => {
                            installed = items
                                .iter()
                                .any(|provider| provider.id == "claude" && provider.installed);
                        }
                        ServerMessage::AgentLaunchResult { agent, pane } => {
                            launched = agent == "claude" && pane.is_some();
                        }
                        _ => {}
                    }
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    let _ = stream.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = server.join();
    assert!(installed, "remote provider path was not used for detection");
    assert!(launched, "remote provider path was not used for launch");
}
