//! Agent chrome regression: provider identity belongs in the terminal-native
//! sidebar and must never inset or frame the child application's Pane.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{
    encode_frame, ClientMessage, FrameDecoder, MouseKind, ProjectInfo, ServerMessage,
};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_sock() -> std::path::PathBuf {
    let dir = common::socket_root().join(format!("uniterm-agent-chrome-{}", std::process::id()));
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

/// Read RenderOps frames until one carries `expected`. Chrome now updates as a
/// damage frame with no screen clear, so this no longer requires `\x1b[2J`; the
/// sidebar, Observatory, and status bar are fully redrawn in every chrome frame.
fn read_frame_until(stream: &mut UnixStream, decoder: &mut FrameDecoder, expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buffer = [0u8; 32_768];
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => decoder.push(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("render read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::RenderOps(ops) = message {
                let frame = String::from_utf8_lossy(&ops).into_owned();
                if frame.contains(expected) {
                    return frame;
                }
            }
        }
    }
    panic!("frame containing {expected:?} did not arrive");
}

fn receive_workspace(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
) -> (uniterm_core::ProjectId, Vec<ProjectInfo>) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buffer = [0u8; 16_384];
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => decoder.push(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("Workspace read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::Workspace {
                active_project,
                projects,
                ..
            } = message
            {
                return (active_project, projects);
            }
        }
    }
    panic!("Workspace projection did not arrive");
}

fn announce_agent(agent: &str) -> Vec<u8> {
    announce_agent_event(agent, "session_start")
}

fn announce_agent_event(agent: &str, event: &str) -> Vec<u8> {
    format!(
        "printf '\\033]777;notify;uniterm://cli-agent;{{\"agent\":\"{agent}\",\"event\":\"{event}\"}}\\007'\n"
    )
    .into_bytes()
}

const CLAUDE_RGB_SGR: &str = "38;2;217;119;87";

#[test]
fn agent_uses_branded_sidebar_without_a_pane_frame() {
    isolate_state();
    let sock = temp_sock();
    let sock_srv = sock.clone();
    let script = "printf '\\033]777;notify;uniterm://cli-agent;{\"agent\":\"claude\",\"event\":\"session_start\"}\\007'; \
                  printf 'BODY'; \
                  sleep 1; \
                  printf '\\033]777;notify;uniterm://cli-agent;{\"agent\":\"claude\",\"event\":\"session_end\"}\\007'; \
                  sleep 2";
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", script], 100, 30).unwrap();
        let _ = server.run(&mut poll);
    });

    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 100,
            rows: 30,
        }))
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    let mut decoder = FrameDecoder::new();
    let mut buffer = [0u8; 32_768];
    // Every RenderOps frame: the attach full frame carries the pane, and the
    // agent bind now arrives as a chrome frame (no screen clear) that brands
    // the fleet rail. The exit later drops the brand in another chrome frame.
    let mut frames: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(branded) = frames.iter().position(|f| f.contains(CLAUDE_RGB_SGR)) {
            if frames[branded + 1..]
                .iter()
                .any(|f| f.contains(" PROJECTS") && !f.contains(CLAUDE_RGB_SGR))
            {
                break;
            }
        }
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                decoder.push(&buffer[..n]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    if let ServerMessage::RenderOps(ops) = message {
                        frames.push(String::from_utf8_lossy(&ops).into_owned());
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
    let _ = stream.flush();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(sock.parent().unwrap());

    // The branding lives in the chrome frame the bind emitted (no screen clear).
    let branded_index = frames
        .iter()
        .position(|frame| frame.contains(CLAUDE_RGB_SGR))
        .unwrap_or_else(|| panic!("no frame used Claude's RGB brand colour"));
    let branded = &frames[branded_index];
    assert!(branded.contains(" PROJECTS"), "Project header missing");
    let muted_heading = format!(
        "\x1b[0;2;{};49m PROJECTS",
        uniterm_core::Theme::dark().muted.sgr_fg()
    );
    assert!(
        branded.contains(&muted_heading),
        "Project header did not use the muted secondary colour"
    );
    assert!(!branded.contains(" WORKSPACE"), "obsolete header remains");
    assert!(branded.contains("Claude Code"), "provider name missing");
    assert!(
        branded.contains("\x1b[2;1H\x1b[0;39;49m"),
        "sidebar did not use terminal default colours"
    );

    // The pane layout (below the top bar, beside the 24-column sidebar, and
    // unframed) is verified against the attach full frame, which carries the
    // pane content the chrome frame deliberately leaves untouched.
    let attach = frames
        .iter()
        .find(|frame| frame.contains("\x1b[r\x1b[2J"))
        .expect("attach full frame");
    assert!(
        attach.contains("\x1b[2;25H"),
        "Pane was not laid out directly below the top bar and beside the 24-column sidebar"
    );
    assert!(!attach.contains('\u{250C}'), "agent Pane still has a frame");

    // After the agent exits, the sidebar redraws without the provider colour.
    assert!(
        frames[branded_index + 1..]
            .iter()
            .any(|frame| frame.contains(" PROJECTS") && !frame.contains(CLAUDE_RGB_SGR)),
        "provider colour remained after the agent exited"
    );
}

#[test]
fn launched_agent_settles_from_starting_to_idle_without_polling() {
    isolate_state();
    let dir = common::socket_root().join(format!("uniterm-agent-settle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join(format!("{}.sock", unique_workspace_name()));
    let server_sock = sock.clone();
    let script = "printf '\\033]777;notify;uniterm://cli-agent;{\"agent\":\"claude\",\"event\":\"session_start\"}\\007'; \
                  printf 'agent prompt ready'; \
                  exec sleep 10";
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_sock, "/bin/sh", &["-c", script], 100, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    thread::sleep(Duration::from_millis(2_500));
    stream
        .write_all(&encode_frame(&ClientMessage::AgentExplain { pane: None }))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let mut buffer = [0u8; 16_384];
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = 'wait: loop {
        assert!(Instant::now() < deadline, "agent explanation timed out");
        match stream.read(&mut buffer) {
            Ok(0) => panic!("server closed before the explanation"),
            Ok(size) => decoder.push(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("agent explanation read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::AgentExplanation { entries } = message {
                break 'wait entries
                    .into_iter()
                    .find(|entry| entry.agent.as_deref() == Some("claude"))
                    .map(|entry| entry.status)
                    .expect("Claude detection entry");
            }
        }
    };
    assert_eq!(status, uniterm_core::AgentStatus::Idle);

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sidebar_agent_scope_toggles_across_projects_and_keeps_project_context() {
    isolate_state();
    let dir = common::socket_root().join(format!("uniterm-agent-scope-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join(format!("{}.sock", unique_workspace_name()));
    let server_sock = sock.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_sock, "/bin/sh", &[], 100, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&sock);

    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 100,
            rows: 30,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Input(announce_agent(
            "codex",
        ))))
        .unwrap();
    read_frame_until(&mut stream, &mut decoder, "Codex");

    stream
        .write_all(&encode_frame(&ClientMessage::ProjectCreate {
            name: "Second".into(),
            root: dir.to_string_lossy().into_owned(),
        }))
        .unwrap();
    let (second, projects) = receive_workspace(&mut stream, &mut decoder);
    let first = projects
        .iter()
        .find(|project| project.id != second)
        .expect("initial Project")
        .clone();
    stream
        .write_all(&encode_frame(&ClientMessage::Input(announce_agent(
            "claude",
        ))))
        .unwrap();
    let project_frame = read_frame_until(&mut stream, &mut decoder, "Claude Code");
    assert!(project_frame.contains("project"));
    assert!(
        !project_frame.contains("Codex"),
        "Project scope leaked an agent from another Project"
    );

    stream
        .write_all(&encode_frame(&ClientMessage::ProjectSwitch {
            project: first.id,
        }))
        .unwrap();
    receive_workspace(&mut stream, &mut decoder);
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 95,
            y: 3,
            kind: MouseKind::Click,
        }))
        .unwrap();
    let workspace_frame = read_frame_until(&mut stream, &mut decoder, "workspace");
    assert!(workspace_frame.contains("Codex"));
    assert!(workspace_frame.contains("Claude Code"));
    assert!(
        workspace_frame.contains("\u{00B7} Second"),
        "Workspace scope omitted the owning Project"
    );

    let theme = uniterm_core::Theme::dark();
    let active_style = format!(
        "\x1b[5;1H\x1b[0;1;{};{}m \u{25B8} {}",
        theme.status_active_fg.sgr_fg(),
        theme.selection_bg.sgr_bg(),
        first.name
    );
    assert!(
        workspace_frame.contains(&active_style),
        "active Project did not receive the selection background"
    );

    // Each Project has equal half-height top and bottom padding. Adjacent cards
    // share one terminal row, and the first and last retain their outer halves.
    // Inactive Projects continue to use the host terminal background.
    assert_eq!(workspace_frame.matches("\x1b[4;1H").count(), 2);
    assert_eq!(workspace_frame.matches("\x1b[5;1H").count(), 2);
    assert_eq!(workspace_frame.matches("\x1b[6;1H").count(), 2);
    assert_eq!(workspace_frame.matches("\x1b[7;1H").count(), 2);
    assert_eq!(workspace_frame.matches("\x1b[8;1H").count(), 2);
    assert!(workspace_frame.contains(&format!(
        "\x1b[4;1H\x1b[0;7;{};49m{}",
        theme.selection_bg.sgr_fg(),
        "\u{2580}".repeat(23)
    )));
    assert!(workspace_frame.contains(&format!(
        "\x1b[7;1H\x1b[0;7;{};49m{}",
        theme.selection_bg.sgr_fg(),
        "\u{2584}".repeat(23)
    )));
    assert!(workspace_frame.contains("\x1b[8;1H\x1b[0;39;49m   Second"));
    assert!(workspace_frame.matches("\x1b[4;65H").count() >= 1);
    assert!(workspace_frame.contains(&format!(
        "\x1b[5;65H\x1b[0;{};49m\u{2502}\x1b[0;1;{};49m",
        uniterm_core::Theme::dark().divider.sgr_fg(),
        uniterm_core::agent::agent_color_or_default("codex").sgr_fg()
    )));
    assert_eq!(workspace_frame.matches("\x1b[7;65H").count(), 1);
    assert!(workspace_frame.contains(&format!(
        "\x1b[8;65H\x1b[0;{};49m\u{2502}\x1b[0;{};49m",
        uniterm_core::Theme::dark().divider.sgr_fg(),
        uniterm_core::agent::agent_color_or_default("claude").sgr_fg()
    )));

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 70,
            y: 8,
            kind: MouseKind::Click,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::WorkspaceState))
        .unwrap();
    let (active, _) = receive_workspace(&mut stream, &mut decoder);
    assert_eq!(active, second, "cross-Project agent click did not focus it");

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 95,
            y: 3,
            kind: MouseKind::Click,
        }))
        .unwrap();
    let project_frame = read_frame_until(&mut stream, &mut decoder, "project");
    assert!(project_frame.contains("Claude Code"));
    assert!(
        !project_frame.contains("Codex"),
        "toggling back to Project scope kept a Workspace agent visible"
    );

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sidebar_agents_group_by_project_and_keep_start_order_when_status_changes() {
    isolate_state();
    let dir = common::socket_root().join(format!("uniterm-agent-order-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join(format!("{}.sock", unique_workspace_name()));
    let server_sock = sock.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_sock, "/bin/sh", &[], 100, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&sock);

    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 100,
            rows: 30,
        }))
        .unwrap();

    stream
        .write_all(&encode_frame(&ClientMessage::Input(announce_agent(
            "codex",
        ))))
        .unwrap();
    read_frame_until(&mut stream, &mut decoder, "Codex");

    stream
        .write_all(&encode_frame(&ClientMessage::ProjectCreate {
            name: "Second".into(),
            root: dir.to_string_lossy().into_owned(),
        }))
        .unwrap();
    let (second, projects) = receive_workspace(&mut stream, &mut decoder);
    let first = projects
        .iter()
        .find(|project| project.id != second)
        .expect("initial Project")
        .id;
    stream
        .write_all(&encode_frame(&ClientMessage::Input(announce_agent(
            "claude",
        ))))
        .unwrap();
    read_frame_until(&mut stream, &mut decoder, "Claude Code");

    stream
        .write_all(&encode_frame(&ClientMessage::ProjectSwitch {
            project: first,
        }))
        .unwrap();
    receive_workspace(&mut stream, &mut decoder);
    stream
        .write_all(&encode_frame(&ClientMessage::Command(
            uniterm_proto::Command::Split(uniterm_proto::SplitAxis::LeftRight),
        )))
        .unwrap();
    read_frame_until(&mut stream, &mut decoder, "Codex");
    stream
        .write_all(&encode_frame(&ClientMessage::Input(announce_agent(
            "gemini",
        ))))
        .unwrap();
    read_frame_until(&mut stream, &mut decoder, "Gemini");

    stream
        .write_all(&encode_frame(&ClientMessage::Input(announce_agent_event(
            "gemini",
            "permission_request",
        ))))
        .unwrap();
    read_frame_until(&mut stream, &mut decoder, "permission");

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 95,
            y: 3,
            kind: MouseKind::Click,
        }))
        .unwrap();
    let frame = read_frame_until(&mut stream, &mut decoder, "workspace");
    let codex = frame.find("Codex").expect("Codex card");
    let gemini = frame.find("Gemini").expect("Gemini card");
    let claude = frame.find("Claude Code").expect("Claude card");
    assert!(
        codex < gemini && gemini < claude,
        "agent cards were not grouped by Project and start time"
    );

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(dir);
}
