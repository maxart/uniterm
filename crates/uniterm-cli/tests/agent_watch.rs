//! The public monitor sees loop stops and real asks without screen scraping.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;
use uniterm_core::PaneId;
use uniterm_proto::{encode_frame, ClientMessage, ControlCommand};
use uniterm_server::Server;
mod common;

struct Monitor {
    child: Child,
    lines: Receiver<Value>,
}
impl Monitor {
    fn start(mut command: Command) -> Self {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                let value = serde_json::from_str(&line).unwrap();
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }
    fn until(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .expect("monitor event before deadline");
            if predicate(&line) {
                return line;
            }
        }
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
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
fn cli(state: &Path, runtime: &Path, workspace: &str, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ut"));
    c.args(args)
        .args(["-w", workspace])
        .env("XDG_STATE_HOME", state)
        .env("XDG_RUNTIME_DIR", runtime);
    c
}

#[test]
fn watch_replays_loop_completion_permission_details_and_session_replacement() {
    let state = common::isolate_state();
    let runtime = common::socket_root().join(format!("ut-watch-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    let name = common::unique_workspace_name();
    let socket = uniterm_server::server::default_socket_path(&name);
    let server_socket = socket.clone();
    let ut = env!("CARGO_BIN_EXE_ut").replace('\'', "'\\''");
    let script=format!("stty -echo; while IFS= read -r line; do case \"$line\" in *hook_event_name*) printf '%s' \"$line\" | '{ut}' agent hook claude ;; *) printf '\\033]777;notify;uniterm://cli-agent;%s\\007' \"$line\" ;; esac; done");
    let join = std::thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", &script], 100, 24).unwrap();
        server.run(&mut poll).unwrap();
    });
    let workspace = Workspace {
        socket: socket.clone(),
        join: Some(join),
    };
    common::wait_for_socket(&socket.with_extension("control.sock"));
    let monitor = Monitor::start(cli(&state, &runtime, &name, &["agent", "watch", "1"]));
    let snapshot = monitor.until(|v| v["event"] == "snapshot");
    assert!(snapshot["agent"].is_null());
    let missing = cli(
        &state,
        &runtime,
        &name,
        &[
            "agent",
            "wait",
            "999999",
            "--event",
            "notification",
            "--after",
            "0",
            "--timeout",
            "1",
        ],
    )
    .output()
    .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no Pane 999999"));

    let emit = |value: Value| {
        let response = uniterm_client::control_request(
            &socket,
            ControlCommand::PanePaste {
                pane: PaneId(1),
                text: value.to_string(),
                submit: true,
            },
        )
        .unwrap();
        assert!(response.error.is_none(), "{response:?}");
    };
    emit(
        json!({"hook_event_name":"SessionStart","session_id":"demo-a","transcript_path":"/tmp/demo-a.jsonl"}),
    );
    let session = monitor.until(|v| v["event"] == "session");
    assert_eq!(session["session_id"], "demo-a");
    emit(json!({"agent":"claude","event":"idle"}));
    monitor.until(|v| v["event"] == "status" && v["status"] == "idle");
    emit(
        json!({"hook_event_name":"PermissionRequest","session_id":"demo-a","tool_name":"Bash","tool_input":{"command":"git status"}}),
    );
    let permission = monitor.until(|v| v["event"] == "waiting" && !v["waiting_id"].is_null());
    assert_eq!(permission["status"], "permission");
    assert_eq!(permission["permission"]["tool"], "Bash");
    assert_eq!(permission["permission"]["preview"], "git status");
    let id = permission["waiting_id"].as_u64().unwrap().to_string();
    let answer = cli(&state, &runtime, &name, &["waiting", "answer", &id, "yes"])
        .output()
        .unwrap();
    assert!(
        answer.status.success(),
        "{}",
        String::from_utf8_lossy(&answer.stderr)
    );
    emit(json!({"agent":"claude","event":"tool_start"}));
    monitor.until(|v| v["event"] == "status" && v["status"] == "tool");
    emit(
        json!({"hook_event_name":"Notification","notification_type":"push_notification","message":"All demo milestones closed; loop stopped.","session_id":"demo-a"}),
    );
    let notification = monitor.until(|v| v["event"] == "notification");
    assert_eq!(notification["status"], "tool");
    assert!(notification["permission"].is_null());
    let cursor = notification["sequence"].as_u64().unwrap().to_string();
    emit(
        json!({"hook_event_name":"PostToolUse","tool_name":"ScheduleWakeup","tool_input":{"stop":true},"session_id":"demo-a"}),
    );
    let stopped = monitor.until(|v| v["event"] == "loop");
    assert_eq!(stopped["loop"]["state"], "stopped");
    emit(json!({"agent":"claude","event":"idle"}));
    monitor.until(|v| v["event"] == "status" && v["status"] == "idle");
    emit(
        json!({"hook_event_name":"Notification","notification_type":"idle_prompt","message":"Waiting for your next prompt","session_id":"demo-a"}),
    );
    let idle = monitor.until(|v| v["event"] == "notification");
    assert_eq!(idle["status"], "idle");
    assert!(idle["waiting_id"].is_null());
    let replay = cli(
        &state,
        &runtime,
        &name,
        &[
            "agent",
            "wait",
            "1",
            "--event",
            "loop-stopped",
            "--after",
            &cursor,
            "--timeout",
            "5",
        ],
    )
    .output()
    .unwrap();
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    let replay: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(replay["session_id"], "demo-a");
    assert_eq!(replay["event"], "loop");
    assert_eq!(replay["sequence"], stopped["sequence"]);
    emit(json!({"hook_event_name":"SessionStart","session_id":"demo-b"}));
    let replacement = monitor.until(|v| v["event"] == "session" && v["session_id"] == "demo-b");
    assert!(replacement["notification"].is_null());
    assert!(replacement["transcript_path"].is_null());
    assert!(replacement["loop"].is_null());
    let explain = cli(
        &state,
        &runtime,
        &name,
        &["agent", "explain", "1", "--json"],
    )
    .output()
    .unwrap();
    assert!(explain.status.success());
    let explain: Value = serde_json::from_slice(&explain.stdout).unwrap();
    assert_eq!(explain["agents"][0]["details"]["session_id"], "demo-b");
    drop(monitor);
    drop(workspace);
}
