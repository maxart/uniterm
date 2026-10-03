//! Detached provider hooks must retain exact Pane identity without a tty.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use uniterm_proto::{
    encode_frame, ClientMessage, ControlCommand, ControlFrame, ControlRequest, ControlResult,
    PaneId, CONTROL_API_VERSION,
};
use uniterm_server::Server;
mod common;

struct Workspace {
    socket: PathBuf,
    join: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Workspace {
    fn drop(&mut self) {
        if let Ok(mut stream) = UnixStream::connect(&self.socket) {
            let _ = stream.write_all(&encode_frame(&ClientMessage::KillServer));
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn detached_hook(state: &Path, runtime: &Path, socket: Option<&Path>, input: &[u8]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ut"));
    command
        .args(["agent", "hook", "codex"])
        .env("XDG_STATE_HOME", state)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("CODEX_HOME", runtime)
        .env("UNITERM", "1")
        .env("UNITERM_PANE_ID", "1")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("UNITERM_SOCKET")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(socket) = socket {
        command.env("UNITERM_SOCKET", socket);
    }
    // Match Codex's hook launcher: a separate session without a controlling tty.
    // SAFETY: setsid and open are async-signal-safe; the child exits on failure.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let fd = libc::open(c"/dev/tty".as_ptr(), libc::O_WRONLY);
            if fd >= 0 {
                libc::close(fd);
                return Err(std::io::Error::other("fixture still owns a tty"));
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn request(socket: &Path, workspace: &str, command: ControlCommand) -> ControlFrame {
    let mut stream = UnixStream::connect(socket.with_extension("control.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        serde_json::to_string(&ControlRequest {
            version: CONTROL_API_VERSION,
            id: 1,
            workspace: workspace.to_owned(),
            command,
        })
        .unwrap()
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn detached_session_hook_reaches_agents_and_today_without_crossing_scope() {
    let state = common::isolate_state();
    let runtime = common::socket_root().join(format!("ut-hooks-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    std::env::set_var("CODEX_HOME", &runtime);
    let name = common::unique_workspace_name();
    let socket = uniterm_server::server::default_socket_path(&name);
    let server_socket = socket.clone();
    let join = std::thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 100, 24).unwrap();
        server.run(&mut poll).unwrap();
    });
    let _workspace = Workspace {
        socket: socket.clone(),
        join: Some(join),
    };
    common::wait_for_socket(&socket.with_extension("control.sock"));
    let payload = serde_json::to_vec(&json!({
        "hook_event_name": "SessionStart", "session_id": "detached-hook-session",
        "transcript_path": runtime.join("fixture.jsonl"), "source": "startup"
    }))
    .unwrap();
    let payload_file = runtime.join("payload.json");
    let result_file = runtime.join("result.json");
    std::fs::write(&payload_file, &payload).unwrap();
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    let script = format!(
        "UNITERM_HOOK_FIXTURE={} UNITERM_HOOK_RESULT={} {} --exact detached_hook_child_fixture --ignored\r",
        quote(&payload_file), quote(&result_file), quote(&std::env::current_exe().unwrap()),
    );
    let mut pane = UnixStream::connect(&socket).unwrap();
    pane.write_all(&encode_frame(&ClientMessage::PaneSend {
        pane: PaneId(1),
        bytes: script.into_bytes(),
    }))
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !result_file.exists() {
        assert!(Instant::now() < deadline, "detached child never reported");
        std::thread::sleep(Duration::from_millis(10));
    }
    let output: Value = serde_json::from_slice(&std::fs::read(&result_file).unwrap()).unwrap();
    assert_eq!(output, json!({"success":true,"stdout":"","stderr":""}));
    let cli = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_ut"))
            .args(args)
            .args(["-w", &name])
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("CODEX_HOME", &runtime)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let observed = cli(&["agent", "explain", "1", "--json"]);
    assert_eq!(
        observed["agents"][0]["details"]["session_id"], "detached-hook-session",
        "{observed}"
    );
    let day = cli(&["today", "list", "--json"]);
    assert!(day["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["session_id"] == "detached-hook-session"));

    // A missing inherited socket cannot select the user's default Workspace.
    assert!(!detached_hook(&state, &runtime, None, &payload)
        .status
        .success());
    assert!(!detached_hook(&state, &runtime, Some(&socket), b"not json")
        .status
        .success());
    assert!(
        matches!(request(&socket, "unrelated-workspace", ControlCommand::AgentHookContext { pane: PaneId(1) }), ControlFrame::Response(response) if response.error.is_some())
    );
    let context = request(
        &socket,
        &name,
        ControlCommand::AgentHookContext { pane: PaneId(1) },
    );
    let ControlFrame::Response(response) = context else {
        panic!("expected context response")
    };
    let Some(ControlResult::AgentHookContext {
        foreground_pid: Some(pid),
        invocation,
        ..
    }) = response.result
    else {
        panic!("missing hook context")
    };
    // Even a caller with the current context cannot impersonate a process
    // belonging to another Pane or a previous invocation.
    let unrelated = request(
        &socket,
        &name,
        ControlCommand::AgentHookReport {
            pane: PaneId(1),
            source_pid: std::process::id() as i32,
            foreground_pid: pid,
            invocation,
            json: json!({"agent":"codex","event":"session_start","session_id":"unrelated-session"})
                .to_string(),
        },
    );
    assert!(
        matches!(unrelated, ControlFrame::Response(response) if response.error.is_some() || matches!(response.result, Some(ControlResult::Mutation { accepted: false, .. })))
    );
    let screen_before = cli(&["pane", "read", "1", "--json"]);
    for invalid in ["not json".to_owned(), " ".repeat(64 * 1024 + 1)] {
        let response = request(
            &socket,
            &name,
            ControlCommand::AgentHookReport {
                pane: PaneId(1),
                source_pid: pid,
                foreground_pid: pid,
                invocation,
                json: invalid,
            },
        );
        assert!(
            matches!(response, ControlFrame::Response(response) if response.error.is_some() || matches!(response.result, Some(ControlResult::Mutation { accepted: false, .. })))
        );
    }
    assert_eq!(cli(&["pane", "read", "1", "--json"]), screen_before);
    let stale = request(
        &socket,
        &name,
        ControlCommand::AgentHookReport {
            pane: PaneId(1),
            source_pid: std::process::id() as i32,
            foreground_pid: pid.saturating_add(1),
            invocation,
            json: json!({"agent":"codex","event":"session_start","session_id":"wrong-session"})
                .to_string(),
        },
    );
    assert!(
        matches!(stale, ControlFrame::Response(response) if response.error.is_some() || matches!(response.result, Some(ControlResult::Mutation { accepted: false, .. })))
    );
    assert_eq!(
        cli(&["agent", "explain", "1", "--json"])["agents"][0]["details"]["session_id"],
        "detached-hook-session"
    );

    // An unavailable responder cannot hold up the harness indefinitely.
    let stalled_socket = runtime.join("stalled.sock");
    let listener = UnixListener::bind(stalled_socket.with_extension("control.sock")).unwrap();
    let stalled = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_millis(800));
    });
    let start = Instant::now();
    assert!(
        !detached_hook(&state, &runtime, Some(&stalled_socket), &payload)
            .status
            .success()
    );
    assert!(
        start.elapsed() < Duration::from_millis(750),
        "hook exceeded its bounded wait"
    );
    stalled.join().unwrap();
    #[cfg(target_os = "linux")]
    {
        let full_socket = runtime.join("full.sock");
        let listener = UnixListener::bind(full_socket.with_extension("control.sock")).unwrap();
        // SAFETY: the listener owns this valid socket; backlog zero leaves
        // exactly one pending connection on Linux, filled below.
        assert_eq!(
            unsafe { libc::listen(std::os::fd::AsRawFd::as_raw_fd(&listener), 0) },
            0
        );
        let _pending = UnixStream::connect(full_socket.with_extension("control.sock")).unwrap();
        let started = Instant::now();
        assert!(
            !detached_hook(&state, &runtime, Some(&full_socket), &payload)
                .status
                .success()
        );
        assert!(
            started.elapsed() < Duration::from_millis(750),
            "full backlog blocked hook connect"
        );
    }
    std::fs::write(result_file.with_extension("release"), b"").unwrap();
}

/// Run only as a child of the test Pane, preserving authoritative ancestry.
#[test]
#[ignore = "subprocess fixture invoked by detached_session_hook test"]
fn detached_hook_child_fixture() {
    let Some(payload) = std::env::var_os("UNITERM_HOOK_FIXTURE") else {
        return;
    };
    let state = PathBuf::from(std::env::var_os("XDG_STATE_HOME").unwrap());
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let socket = PathBuf::from(std::env::var_os("UNITERM_SOCKET").unwrap());
    let result = PathBuf::from(std::env::var_os("UNITERM_HOOK_RESULT").unwrap());
    let output = detached_hook(
        &state,
        &runtime,
        Some(&socket),
        &std::fs::read(payload).unwrap(),
    );
    let value = json!({"success":output.status.success(), "stdout":String::from_utf8_lossy(&output.stdout), "stderr":String::from_utf8_lossy(&output.stderr)});
    let temporary = result.with_extension("tmp");
    std::fs::write(&temporary, serde_json::to_vec(&value).unwrap()).unwrap();
    std::fs::rename(temporary, result).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !PathBuf::from(std::env::var_os("UNITERM_HOOK_RESULT").unwrap())
        .with_extension("release")
        .exists()
    {
        assert!(Instant::now() < deadline, "parent never released fixture");
        std::thread::sleep(Duration::from_millis(10));
    }
}
