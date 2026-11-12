//! Quiet-surface checks and opt-in five-minute budgets for attached views.
#[cfg(target_os = "linux")]
mod common;

#[test]
#[cfg(target_os = "linux")]
#[ignore = "five-minute performance budget; run explicitly in release mode"]
fn five_minute_today_idle_budget() {
    idle_budget(Surface::Relationships, 300);
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "five-minute performance budget; run explicitly in release mode"]
fn five_minute_flow_idle_budget() {
    idle_budget(Surface::Flow, 300);
}

#[test]
#[cfg(target_os = "linux")]
fn enabled_navigation_graph_and_supervisor_off_are_quiet() {
    idle_budget(Surface::NavigationGraph, 2);
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "five-minute performance budget; run explicitly in release mode"]
fn five_minute_enabled_navigation_graph_idle_budget() {
    idle_budget(Surface::NavigationGraph, 300);
}

#[test]
#[cfg(target_os = "linux")]
fn visible_overwatch_chat_is_quiet() {
    idle_budget(Surface::Overwatch, 2);
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "five-minute performance budget; run explicitly in release mode"]
fn five_minute_overwatch_idle_budget() {
    idle_budget(Surface::Overwatch, 300);
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
enum Surface {
    Relationships,
    Flow,
    NavigationGraph,
    Overwatch,
}

#[cfg(target_os = "linux")]
fn idle_budget(surface: Surface, duration_seconds: u64) {
    // The fixture changes process-wide test environment and cwd. Serialize
    // explicit runs of all ignored budgets as well as the short smoke test.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().unwrap();
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
    let original_path = std::env::var_os("PATH");
    if matches!(surface, Surface::Overwatch) {
        use std::os::unix::fs::PermissionsExt;
        let bin = runtime.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let provider = bin.join("codex");
        std::fs::write(&provider, r#"#!/bin/sh
printf '\033]777;notify;uniterm://cli-agent;{"agent":"codex","event":"idle","session_id":"quiet-supervisor"}\007Overwatch chat ready\n'
exec /bin/cat
"#).unwrap();
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    }
    let workspace = common::unique_workspace_name();
    let socket = runtime.join(format!("{workspace}.sock"));
    let server_socket = socket.clone();
    let server = std::thread::spawn(move || {
        let (mut server, mut poll) =
            uniterm_server::Server::bind(&server_socket, "/bin/sh", &[], 140, 30).unwrap();
        server.set_config(uniterm_core::Config {
            auto_tab_names: true,
            switcher_enabled: true,
            run_graph: true,
            ..uniterm_core::Config::default()
        });
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
    match surface {
        Surface::Overwatch => {
            use uniterm_core::overwatch::{Operation, Phase};
            let call = |operation| {
                let mut stream =
                    UnixStream::connect(socket.with_extension("control.sock")).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let request = uniterm_proto::ControlRequest {
                    version: 1,
                    id: 2,
                    workspace: workspace.clone(),
                    command: uniterm_proto::ControlCommand::Overwatch { operation },
                };
                writeln!(stream, "{}", serde_json::to_string(&request).unwrap()).unwrap();
                let mut reply = String::new();
                std::io::BufRead::read_line(&mut std::io::BufReader::new(stream), &mut reply)
                    .unwrap();
                let uniterm_proto::ControlFrame::Response(response) =
                    serde_json::from_str(&reply).unwrap()
                else {
                    panic!("unexpected Overwatch response");
                };
                assert!(response.error.is_none(), "{:?}", response.error);
                let Some(uniterm_proto::ControlResult::Overwatch { state }) = response.result
                else {
                    panic!("missing Overwatch state");
                };
                state
            };
            call(Operation::Configure {
                provider: "codex".into(),
                model: None,
                project: None,
                manage: false,
            });
            call(Operation::Enable);
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let state = call(Operation::Status);
                if state.phase == Phase::Running {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "Overwatch failed to run: {state:?}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            client
                .write_all(&encode_frame(&ClientMessage::Overwatch {
                    operation: Operation::Chat,
                }))
                .unwrap();
        }
        Surface::NavigationGraph => {
            for message in [
                ClientMessage::Command(Command::Servers),
                ClientMessage::Overwatch {
                    operation: uniterm_core::overwatch::Operation::Open,
                },
                ClientMessage::Command(Command::Switcher),
                ClientMessage::SwitcherSubscribe { on: true },
                ClientMessage::Command(Command::RunGraph),
            ] {
                client.write_all(&encode_frame(&message)).unwrap();
            }
        }
        Surface::Relationships | Surface::Flow => {
            client
                .write_all(&encode_frame(&ClientMessage::Command(Command::Today)))
                .unwrap();
            client
                .write_all(&encode_frame(&ClientMessage::Input(
                    if matches!(surface, Surface::Flow) {
                        b"2"
                    } else {
                        b"a"
                    }
                    .to_vec(),
                )))
                .unwrap();
        }
    }
    let settle = Instant::now();
    let mut bytes = [0; 65536];
    let mut initial_bytes = 0;
    let mut decoder = FrameDecoder::new();
    let mut screen = uniterm_server::Terminal::new(140, 30);
    let mut saw_switcher = false;
    let mut saw_servers = false;
    let mut saw_overwatch_off = false;
    let settle_seconds = if matches!(surface, Surface::Overwatch) {
        8
    } else {
        3
    };
    while settle.elapsed() < Duration::from_secs(settle_seconds) {
        if let Ok(count) = client.read(&mut bytes) {
            initial_bytes += count;
            decoder.push(&bytes[..count]);
            while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
                match message {
                    ServerMessage::RenderOps(ops) => screen.feed(&ops),
                    ServerMessage::Switcher {
                        enabled: true,
                        entries,
                        ..
                    } => {
                        assert!(!entries.is_empty());
                        saw_switcher = true;
                    }
                    ServerMessage::MachineNavigation { local: false } => saw_servers = true,
                    ServerMessage::Overwatch { state, .. }
                        if !matches!(surface, Surface::Overwatch) =>
                    {
                        assert!(!state.enabled);
                        saw_overwatch_off = true;
                    }
                    _ => {}
                }
            }
        }
    }
    if matches!(surface, Surface::NavigationGraph) {
        assert!(screen.dump_text().contains("AGENT GRAPH"));
        assert!(saw_switcher && saw_servers && saw_overwatch_off);
    } else if matches!(surface, Surface::Overwatch) {
        assert!(
            screen.dump_text().contains("Overwatch chat ready"),
            "{}",
            screen.dump_text()
        );
    } else if matches!(surface, Surface::Flow) {
        assert!(screen.dump_text().contains("Agent flow"));
        assert!(screen.dump_text().contains("Pane 1"));
        assert!(!screen.dump_text().contains("idle-child"));
    } else {
        assert!(screen.dump_text().contains("1 Activity"));
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
        .set_read_timeout(Some(Duration::from_secs(duration_seconds)))
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
    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    eprintln!("{surface:?} idle: {seconds:.1}s, {cpu_percent:.4}% CPU, {rss}, initial bytes {initial_bytes}, idle read {idle_read:?}");
    assert!(initial_bytes > 0);
    assert!(
        seconds >= duration_seconds as f64 - 0.1,
        "idle view unexpectedly woke its client"
    );
    assert!(
        matches!(idle_read, Err(ref e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)),
        "idle view emitted bytes"
    );
    assert!(cpu_percent < 0.5, "idle CPU budget regressed");
}
