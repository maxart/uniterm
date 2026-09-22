//! CLI coverage for creating and naming Tabs by hierarchy position, the two
//! verbs the agent skill relies on to organise work without attaching.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::ClientMessage;
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn wait_for(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("server socket never appeared at {}", path.display());
}

fn run_ut(
    runtime: &std::path::Path,
    state: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", state)
        .output()
        .unwrap()
}

fn drain_render_frames(stream: &mut std::os::unix::net::UnixStream) -> Vec<Vec<u8>> {
    use std::io::Read as _;
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mut decoder = uniterm_proto::FrameDecoder::new();
    let mut frames = Vec::new();
    let mut bytes = [0; 32768];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) => break,
            Ok(count) => decoder.push(&bytes[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(error) => panic!("reading human client: {error}"),
        }
        while let Some(message) = decoder.decode::<uniterm_proto::ServerMessage>().unwrap() {
            if let uniterm_proto::ServerMessage::RenderOps(ops) = message {
                frames.push(ops);
            }
        }
    }
    frames
}

#[test]
fn tabs_can_be_created_and_named_by_hierarchy_position() {
    let base =
        common::socket_root().join(format!("uniterm-cli-tab-control-{}", std::process::id()));
    let runtime = base.join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    let state = isolate_state();

    // An old v1 server can silently ignore new JSON fields. The client must
    // query capabilities and send no mutation when background support is absent.
    let old_workspace = unique_workspace_name();
    let old_socket = uniterm_server::server::default_socket_path(&old_workspace);
    std::fs::create_dir_all(old_socket.parent().unwrap()).unwrap();
    let listener =
        std::os::unix::net::UnixListener::bind(old_socket.with_extension("control.sock")).unwrap();
    let old_server = thread::spawn(move || {
        use std::io::{BufRead as _, Write as _};
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = std::io::BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: uniterm_proto::ControlRequest = serde_json::from_str(&line).unwrap();
        assert!(matches!(
            request.command,
            uniterm_proto::ControlCommand::Capabilities
        ));
        // Use the typed response to keep the test independent of tag spelling.
        let response: uniterm_proto::ControlFrame =
            uniterm_proto::ControlFrame::Response(uniterm_proto::ControlResponse::ok(
                request.id,
                uniterm_proto::ControlResult::Capabilities {
                    protocol_version: 1,
                    capabilities: Vec::new(),
                    max_frame_bytes: 1048576,
                    max_connections: 128,
                    max_queued_frames: 64,
                    max_queued_requests: 64,
                },
            ));
        serde_json::to_writer(reader.get_mut(), &response).unwrap();
        reader.get_mut().write_all(b"\n").unwrap();
        line.clear();
        assert_eq!(
            reader.read_line(&mut line).unwrap(),
            0,
            "old server must receive no mutation"
        );
    });
    let refused = run_ut(
        &runtime,
        &state,
        &[
            "project",
            "add",
            "NeverCreated",
            state.to_str().unwrap(),
            "--background",
            "-w",
            &old_workspace,
        ],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("does not support background automation")
    );
    old_server.join().unwrap();

    let workspace = unique_workspace_name();
    let socket = uniterm_server::server::default_socket_path(&workspace);
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 100, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&socket);
    wait_for(&socket.with_extension("control.sock"));

    // The initial Project has one Tab; `tab new` adds a second and prints
    // its 1-based ordinal.
    let created = run_ut(&runtime, &state, &["tab", "new", "-w", &workspace]);
    assert!(
        created.status.success(),
        "tab new failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&created.stdout).trim(), "2");

    let panes = run_ut(
        &runtime,
        &state,
        &["pane", "list", "-w", &workspace, "--json"],
    );
    let panes: serde_json::Value = serde_json::from_slice(&panes.stdout).unwrap();
    let project_name = panes["panes"][0]["project_name"]
        .as_str()
        .unwrap()
        .to_string();

    let renamed = run_ut(
        &runtime,
        &state,
        &[
            "tab",
            "rename",
            &project_name,
            "2",
            "Review",
            "-w",
            &workspace,
        ],
    );
    assert!(
        renamed.status.success(),
        "tab rename failed: {}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    let panes = run_ut(
        &runtime,
        &state,
        &["pane", "list", "-w", &workspace, "--json"],
    );
    let panes: serde_json::Value = serde_json::from_slice(&panes.stdout).unwrap();
    let names: Vec<(u64, &str)> = panes["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pane| {
            (
                pane["tab"].as_u64().unwrap(),
                pane["tab_name"].as_str().unwrap(),
            )
        })
        .collect();
    assert!(names.contains(&(2, "Review")), "{names:?}");

    // The name now resolves as a Tab selector, and a missing Tab is an error.
    let focused = run_ut(
        &runtime,
        &state,
        &[
            "tab",
            "rename",
            &project_name,
            "Review",
            "Reviewed",
            "-w",
            &workspace,
        ],
    );
    assert!(focused.status.success());
    let missing = run_ut(
        &runtime,
        &state,
        &[
            "tab",
            "rename",
            &project_name,
            "9",
            "Nope",
            "-w",
            &workspace,
        ],
    );
    assert!(!missing.status.success());

    // The fleet listing the agent skill starts from is scriptable too, and
    // an empty Workspace reports an empty list rather than an error.
    let agents = run_ut(
        &runtime,
        &state,
        &["agent", "list", "-w", &workspace, "--json"],
    );
    assert!(
        agents.status.success(),
        "{}",
        String::from_utf8_lossy(&agents.stderr)
    );
    let agents: serde_json::Value = serde_json::from_slice(&agents.stdout).unwrap();
    assert_eq!(agents["workspace"], workspace);
    assert_eq!(agents["agents"], serde_json::json!([]));

    // Automation can create in another Project, edit its hierarchy, launch a
    // custom provider, and close resources while the human stays on this Pane.
    use std::io::Write as _;
    let mut human = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    human
        .write_all(&uniterm_proto::encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 200,
            rows: 30,
        }))
        .unwrap();
    assert!(
        !drain_render_frames(&mut human).is_empty(),
        "first attach must paint"
    );
    let snapshot = || uniterm_client::pane_list(&socket).unwrap().1;
    let selected = snapshot().into_iter().find(|pane| pane.active).unwrap().id;
    let run = |args: &[&str]| {
        let mut full = args.to_vec();
        full.extend(["-w", workspace.as_str()]);
        let result = run_ut(&runtime, &state, &full);
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            snapshot().into_iter().find(|pane| pane.active).unwrap().id,
            selected
        );
        String::from_utf8(result.stdout).unwrap().trim().to_string()
    };
    let project = run(&[
        "project",
        "add",
        "Background",
        state.to_str().unwrap(),
        "--background",
    ]);
    assert_eq!(run(&["tab", "new", &project, "--background"]), "2");
    run(&["tab", "rename", &project, "2", "Work"]);
    let anchor = snapshot()
        .into_iter()
        .find(|pane| pane.project.0.to_string() == project && pane.tab == 2)
        .unwrap()
        .id
        .0
        .to_string();
    run(&["tab", "move", &project, "Work", "left"]);
    run(&["tab", "move", &project, "Work", "right"]);
    run(&[
        "tab",
        "prompt",
        &project,
        "Work",
        "printf 'TAB_%s\\n' INPUT",
    ]);
    let tab_input = run_ut(
        &runtime,
        &state,
        &[
            "pane",
            "wait-output",
            &anchor,
            "TAB_INPUT",
            "--timeout",
            "5",
            "-w",
            &workspace,
        ],
    );
    assert!(tab_input.status.success());
    let before = snapshot().len();
    for args in [
        vec!["tab", "rename", &project, "99", "Missing"],
        vec!["pane", "split", "99999999", "--background"],
        vec![
            "agent",
            "start",
            "uniterm-nonexistent-provider",
            "--pane",
            &anchor,
            "--background",
        ],
    ] {
        let mut args = args;
        args.extend(["-w", workspace.as_str()]);
        assert!(!run_ut(&runtime, &state, &args).status.success());
        assert_eq!(snapshot().len(), before);
        assert_eq!(
            snapshot().into_iter().find(|pane| pane.active).unwrap().id,
            selected
        );
    }
    let split = run(&["pane", "split", &anchor, "--background"]);
    let agent = run(&[
        "agent",
        "start",
        "sh",
        "--pane",
        &split,
        "--tab",
        "--background",
    ]);
    assert!(snapshot()
        .iter()
        .any(|pane| pane.id.0.to_string() == agent && pane.project.0.to_string() == project));
    run(&["pane", "close", &agent]);
    run(&["pane", "close", &split]);
    run(&["tab", "close", &project, "Work"]);
    run(&["project", "remove", &project]);

    let frames = drain_render_frames(&mut human);
    assert!(
        frames
            .iter()
            .all(|frame| !frame.windows(4).any(|bytes| bytes == b"\x1b[2J")),
        "background work must not clear the human's terminal"
    );

    // Inspect bytes in a raw PTY: this is independent of the host OS's line
    // discipline and proves multiline text plus Enter is one paste + CR.
    let receiver = run(&["pane", "split", &selected.0.to_string(), "--background"]);
    run(&["pane", "send-keys", &receiver,
        "stty raw -echo; printf '\\033[?2004hPROMPT_%s' READY; dd bs=1 count=24 2>/dev/null | od -An -tx1; stty sane; printf '\\nPROMPT_%s\\n' DONE", "--enter"]);
    let ready = run_ut(
        &runtime,
        &state,
        &[
            "pane",
            "wait-output",
            &receiver,
            "PROMPT_READY",
            "--timeout",
            "5",
            "-w",
            &workspace,
        ],
    );
    assert!(ready.status.success());
    let mut sender = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["agent", "prompt", &receiver, "--stdin", "-w", &workspace])
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &state)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    sender
        .stdin
        .take()
        .unwrap()
        .write_all(b"first\nnext")
        .unwrap();
    let submitted = sender.wait_with_output().unwrap();
    assert!(
        submitted.status.success(),
        "{}",
        String::from_utf8_lossy(&submitted.stderr)
    );
    // 6 prefix + 10 text + 6 suffix + CR = 23 bytes; the separate submit
    // command contributes exactly one further CR, not a pasted newline.
    run(&["pane", "submit", &receiver]);
    let received = run_ut(
        &runtime,
        &state,
        &[
            "pane",
            "wait-output",
            &receiver,
            "PROMPT_DONE",
            "--timeout",
            "5",
            "-w",
            &workspace,
        ],
    );
    let text = uniterm_client::pane_read(
        &socket,
        uniterm_core::PaneId(receiver.parse().unwrap()),
        200,
    )
    .unwrap()
    .unwrap()
    .0;
    assert!(
        received.status.success(),
        "{}\n{text}",
        String::from_utf8_lossy(&received.stderr)
    );
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        compact.contains("1b5b3230307e66697273740a6e6578741b5b3230317e0d0d"),
        "{text}"
    );
    run(&["pane", "close", &receiver]);

    for pane in snapshot().into_iter().filter(|pane| pane.id != selected) {
        run(&["pane", "close", &pane.id.0.to_string()]);
    }
    let closed = run_ut(
        &runtime,
        &state,
        &["pane", "close", &selected.0.to_string(), "-w", &workspace],
    );
    assert!(
        closed.status.success(),
        "last Pane close: {}",
        String::from_utf8_lossy(&closed.stderr)
    );
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(base);
}
