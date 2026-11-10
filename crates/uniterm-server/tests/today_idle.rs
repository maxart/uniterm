//! Opt-in five-minute idle budget for the actual attached Today surface.
#[cfg(target_os = "linux")]
mod common;

#[test]
#[cfg(target_os = "linux")]
#[ignore = "five-minute performance budget; run explicitly in release mode"]
fn five_minute_today_idle_budget() {
    today_idle_budget(false);
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "five-minute performance budget; run explicitly in release mode"]
fn five_minute_flow_idle_budget() {
    today_idle_budget(true);
}

#[cfg(target_os = "linux")]
fn today_idle_budget(flow: bool) {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};
    use uniterm_proto::{encode_frame, ClientMessage, Command, FrameDecoder, ServerMessage};
    common::isolate_state();
    let runtime = common::socket_root().join(format!("ut-idle-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    std::env::set_var("CODEX_HOME", &runtime);
    std::fs::create_dir_all(runtime.join("sessions")).unwrap();
    std::env::set_current_dir(&runtime).unwrap();
    let workspace = common::unique_workspace_name();
    let socket = runtime.join(format!("{workspace}.sock"));
    let server_socket = socket.clone();
    let server = std::thread::spawn(move || {
        let (mut server, mut poll) =
            uniterm_server::Server::bind(&server_socket, "/bin/sh", &[], 140, 30).unwrap();
        server.run(&mut poll).unwrap();
    });
    common::wait_for_socket(&socket);
    // A retained inbox body arms only its actual expiry, never a scan timer.
    common::wait_for_socket(&socket.with_extension("control.sock"));
    let mut inbox = UnixStream::connect(socket.with_extension("control.sock")).unwrap();
    let request = uniterm_proto::ControlRequest {
        version: 1,
        id: 1,
        workspace: workspace.clone(),
        command: uniterm_proto::ControlCommand::Inbox {
            project: None,
            identity: None,
            epoch: None,
            operation: uniterm_core::inbox::Operation::Post {
                kind: uniterm_core::inbox::Kind::Plan,
                body: "Idle budget: keep this plan retained without polling.".into(),
                reply_to: None,
                to: None,
                task: None,
                key: "idle-plan".into(),
                ttl_ms: uniterm_core::inbox::TTL_MS,
            },
        },
    };
    writeln!(inbox, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    inbox
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reply = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(inbox), &mut reply).unwrap();
    assert!(matches!(
        serde_json::from_str::<uniterm_proto::ControlFrame>(&reply).unwrap(),
        uniterm_proto::ControlFrame::Response(uniterm_proto::ControlResponse {
            result: Some(uniterm_proto::ControlResult::Inbox { .. }),
            ..
        })
    ));
    let mut client = UnixStream::connect(&socket).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 140,
            rows: 30,
        }))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::PaneSend {
            pane: uniterm_core::PaneId(1),
            bytes: b"printf '\\033]777;notify;uniterm://cli-agent;{\"agent\":\"codex\",\"event\":\"session_start\",\"session_id\":\"idle-parent\"}\\007\\033]777;notify;uniterm://cli-agent;{\"agent\":\"codex\",\"event\":\"session_start\",\"session_id\":\"idle-child\",\"parent_session_id\":\"idle-parent\"}\\007'\r".to_vec(),
        }))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::AutoApprove {
            enabled: Some(true),
            confirmed: true,
        }))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Command(Command::Today)))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Input(
            if flow { b"3" } else { b"a" }.to_vec(),
        )))
        .unwrap();
    let settle = Instant::now();
    let mut bytes = [0; 65536];
    let mut initial_bytes = 0;
    let mut decoder = FrameDecoder::new();
    let mut screen = uniterm_server::Terminal::new(140, 30);
    while settle.elapsed() < Duration::from_secs(3) {
        if let Ok(count) = client.read(&mut bytes) {
            initial_bytes += count;
            decoder.push(&bytes[..count]);
            while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
                if let ServerMessage::RenderOps(ops) = message {
                    screen.feed(&ops);
                }
            }
        }
    }
    if flow {
        assert!(screen.dump_text().contains("Agent flow"));
        assert!(screen.dump_text().contains("Pane 1"));
        assert!(!screen.dump_text().contains("idle-child"));
    } else {
        assert!(screen.dump_text().contains("Agent relationships"));
        assert!(screen.dump_text().contains("idle-parent"));
        assert!(screen.dump_text().contains("idle-child"));
    }
    fn cpu_seconds() -> f64 {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: initialized, writable timespec for the process CPU clock.
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) },
            0
        );
        time.tv_sec as f64 + time.tv_nsec as f64 / 1_000_000_000.0
    }
    client
        .set_read_timeout(Some(Duration::from_secs(300)))
        .unwrap();
    let start = Instant::now();
    let cpu_start = cpu_seconds();
    let idle_read = client.read(&mut bytes);
    let seconds = start.elapsed().as_secs_f64();
    let cpu_percent = (cpu_seconds() - cpu_start) * 100.0 / seconds;
    let rss = std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|s| s.starts_with("VmRSS:"))
        .unwrap()
        .to_string();
    client
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();
    eprintln!("Today idle: {seconds:.1}s, {cpu_percent:.4}% CPU, {rss}, initial bytes {initial_bytes}, idle read {idle_read:?}");
    assert!(initial_bytes > 0);
    assert!(seconds >= 299.0, "idle view unexpectedly woke its client");
    assert!(
        matches!(idle_read, Err(ref e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)),
        "idle view emitted bytes"
    );
    assert!(cpu_percent < 0.5, "idle CPU budget regressed");
}
