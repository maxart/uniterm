//! Invocation-scoped agent details, end to end: real connector envelopes
//! (built by the same code `ut agent hook` runs) travel through a Pane's PTY,
//! and the fleet snapshot, waiting queue, and event log must agree.
//!
//! The scripted "agent" is a job-controlled `sh -c` in its own process group,
//! like a real provider, and its hooks print from inside that group.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use uniterm_proto::{ControlCommand, ControlRequest, CONTROL_API_VERSION};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

struct Control {
    workspace: String,
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Control {
    fn connect(socket: &Path) -> Self {
        let control = socket.with_extension("control.sock");
        for _ in 0..300 {
            if control.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let writer = UnixStream::connect(&control).unwrap();
        writer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        Control {
            workspace: socket.file_stem().unwrap().to_str().unwrap().into(),
            reader: BufReader::new(writer.try_clone().unwrap()),
            writer,
            next_id: 1,
        }
    }

    /// One request, answered by its own response; event frames are skipped.
    fn request(&mut self, command: ControlCommand) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = ControlRequest {
            version: CONTROL_API_VERSION,
            id,
            workspace: self.workspace.clone(),
            command,
        };
        serde_json::to_writer(&mut self.writer, &request).unwrap();
        self.writer.write_all(b"\n").unwrap();
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            assert!(!line.is_empty(), "control connection closed");
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["frame"] == "response" && frame["id"] == id {
                assert!(frame["error"].is_null(), "control error: {frame}");
                return frame["result"]["data"].clone();
            }
        }
    }

    fn agents(&mut self) -> Value {
        self.request(ControlCommand::AgentList)
    }

    fn agent(&mut self) -> Value {
        self.agents()["entries"][0].clone()
    }

    fn waiting(&mut self) -> Vec<Value> {
        self.request(ControlCommand::WaitingList)["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn type_line(&mut self, pane: u64, text: &str) {
        self.request(ControlCommand::PaneSend {
            pane: uniterm_core::PaneId(pane),
            text: format!("{text}\r"),
        });
    }

    fn read(&mut self, pane: u64) -> String {
        self.request(ControlCommand::PaneRead {
            pane: uniterm_core::PaneId(pane),
            lines: 200,
        })["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// Poll the fleet until `check` holds for the first agent entry.
    fn until(&mut self, what: &str, check: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let agent = self.agent();
            if check(&agent) {
                return agent;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {agent}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Write the envelope the connector would print for one hook invocation.
fn hook_file(dir: &Path, name: &str, hook: Value) -> PathBuf {
    let envelope = uniterm_server::connectors::hook_envelope("claude", hook.to_string().as_bytes())
        .unwrap_or_else(|| panic!("connector rejected {hook}"));
    let path = dir.join(name);
    std::fs::write(&path, envelope).unwrap();
    path
}

/// A status-only envelope, exactly as the plain printf hooks emit it.
fn plain_file(dir: &Path, name: &str, event: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("\x1b]777;notify;uniterm://cli-agent;{{\"agent\":\"claude\",\"event\":\"{event}\"}}\x07"),
    )
    .unwrap();
    path
}

fn session_start(session: &str, transcript: Option<&str>) -> Value {
    let mut hook = json!({
        "hook_event_name": "SessionStart",
        "source": "startup",
        "session_id": session,
        "cwd": "/tmp/demo",
    });
    if let Some(path) = transcript {
        hook["transcript_path"] = Value::String(path.into());
    }
    hook
}

fn notification(session: &str, kind: &str, message: &str) -> Value {
    json!({
        "hook_event_name": "Notification",
        "session_id": session,
        "notification_type": kind,
        "message": message,
    })
}

fn permission(session: &str, tool: &str, command: &str) -> Value {
    json!({
        "hook_event_name": "PermissionRequest",
        "session_id": session,
        "tool_name": tool,
        "tool_input": {"command": command},
    })
}

fn wakeup(session: &str, input: Value) -> Value {
    json!({
        "hook_event_name": "PostToolUse",
        "session_id": session,
        "tool_name": "ScheduleWakeup",
        "tool_input": input,
        "tool_response": {},
    })
}

fn cat(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| format!("cat '{}'", path.display()))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Every event appended so far, read back through a cursored subscription.
fn events(socket: &Path) -> Vec<Value> {
    let mut control = Control::connect(socket);
    let current = control.request(ControlCommand::AgentList)["sequence"]
        .as_u64()
        .unwrap();
    let request = ControlRequest {
        version: CONTROL_API_VERSION,
        id: 99,
        workspace: control.workspace.clone(),
        command: ControlCommand::Subscribe { after_sequence: 0 },
    };
    serde_json::to_writer(&mut control.writer, &request).unwrap();
    control.writer.write_all(b"\n").unwrap();
    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        control.reader.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "subscription closed");
        let frame: Value = serde_json::from_str(&line).unwrap();
        if frame["frame"] == "event" {
            let sequence = frame["sequence"].as_u64().unwrap();
            out.push(frame["event"].clone());
            if sequence >= current {
                return out;
            }
        }
    }
}

fn position(events: &[Value], from: usize, test: impl Fn(&Value) -> bool) -> usize {
    events[from..]
        .iter()
        .position(test)
        .map(|index| index + from)
        .unwrap_or_else(|| panic!("event not found after {from}"))
}

#[test]
fn hook_details_follow_one_invocation_and_session() {
    isolate_state();
    let workspace = unique_workspace_name();
    let dir = common::socket_root().join(format!("uniterm-details-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{workspace}.sock"));
    let files = common::temp_dir("agent-details");
    let server_socket = socket.clone();
    thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 120, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    let mut control = Control::connect(&socket);
    let pane = control.request(ControlCommand::PaneList)["panes"][0]["id"]
        .as_u64()
        .unwrap();
    // A plain shell Pane has no agent until a hook speaks.
    assert!(control.agents()["entries"].as_array().unwrap().is_empty());

    let start = hook_file(
        &files,
        "start",
        session_start("s-1", Some("/tmp/demo/one.jsonl")),
    );
    let prompt = plain_file(&files, "prompt", "prompt_submit");
    let idle_notice = hook_file(
        &files,
        "idle-notice",
        notification("s-1", "idle_prompt", "Claude is waiting for your input"),
    );
    let ask = hook_file(&files, "ask", permission("s-1", "Bash", "git push"));
    let ask_notice = hook_file(
        &files,
        "ask-notice",
        notification(
            "s-1",
            "permission_prompt",
            "Claude needs your permission to use Bash",
        ),
    );
    let tool_end = plain_file(&files, "tool-end", "tool_end");
    let ask_again = hook_file(&files, "ask-again", permission("s-1", "Write", "demo.txt"));
    let replaced = hook_file(&files, "replaced", session_start("s-2", None));
    let scheduled = hook_file(
        &files,
        "scheduled",
        wakeup("s-2", json!({"delaySeconds": 90})),
    );
    let stopped = hook_file(&files, "stopped", wakeup("s-2", json!({"stop": true})));
    let push = hook_file(
        &files,
        "push",
        notification("s-2", "push_notification", "Demo done; loop stopped."),
    );
    let idle = plain_file(&files, "idle", "idle");
    let script = format!(
        "{}; read go; {}; read answer; echo \"GOT:$answer\"; {}; read go; {}; read go; {}; read go; {}; read go",
        cat(&[start, prompt, idle_notice]),
        cat(&[ask, ask_notice]),
        cat(&[tool_end]),
        cat(&[ask_again]),
        cat(&[replaced]),
        cat(&[scheduled, stopped, push, idle]),
    );
    std::fs::write(files.join("agent.sh"), script).unwrap();
    control.type_line(pane, &format!("sh '{}'", files.join("agent.sh").display()));

    // An idle reminder is a notification event: the working agent stays working.
    let agent = control.until("idle reminder", |agent| {
        agent["details"]["last_notification"]["kind"] == "idle_prompt"
    });
    assert_eq!(agent["status"], "Working");
    assert_eq!(agent["details"]["session_id"], "s-1");
    assert_eq!(agent["details"]["transcript_path"], "/tmp/demo/one.jsonl");
    let invocation = agent["details"]["invocation"].as_i64().unwrap();
    assert!(control.waiting().is_empty());
    control.type_line(pane, "go");

    // A real permission names its tool and has one answerable waiting id; the
    // later generic permission notification neither erases it nor churns it.
    let agent = control.until("permission", |agent| {
        agent["details"]["last_notification"]["kind"] == "permission_prompt"
    });
    assert_eq!(agent["status"], "Permission");
    assert_eq!(agent["details"]["pending_permission"]["tool"], "Bash");
    assert_eq!(
        agent["details"]["pending_permission"]["preview"],
        "git push"
    );
    let waiting = control.waiting();
    assert_eq!(waiting.len(), 1);
    let id = waiting[0]["id"].as_u64().unwrap();
    assert_eq!(agent["waiting"], id);
    assert!(
        waiting[0]["summary"]
            .as_str()
            .unwrap()
            .contains("Bash: git push"),
        "{waiting:?}"
    );
    // `ut waiting answer ID yes`: guarded terminal delivery into the prompt.
    let acted = control.request(ControlCommand::WaitingAct {
        id,
        action: uniterm_proto::WaitingAction::Answer,
        text: "yes".into(),
    });
    assert_eq!(acted["accepted"], true, "{acted}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !control.read(pane).contains("GOT:yes") {
        assert!(Instant::now() < deadline, "answer never reached the prompt");
        thread::sleep(Duration::from_millis(20));
    }

    // Moving on clears the stale permission payload and the queue.
    let agent = control.until("tool end", |agent| agent["status"] == "Working");
    assert!(agent["details"]["pending_permission"].is_null());
    assert!(agent["waiting"].is_null());
    assert!(control.waiting().is_empty());
    control.type_line(pane, "go");

    // The old session blocks again, with an answerable item of its own.
    let agent = control.until("second permission", |agent| {
        agent["details"]["pending_permission"]["tool"] == "Write"
    });
    let second_ask = agent["waiting"].as_u64().unwrap();
    assert_ne!(second_ask, id);
    control.type_line(pane, "go");

    // A replacement session in the same process retires the earlier
    // session's prompt and every detail, including the absent transcript.
    let agent = control.until("session replacement", |agent| {
        agent["details"]["session_id"] == "s-2"
    });
    assert_eq!(agent["status"], "Starting");
    assert!(agent["details"]["transcript_path"].is_null());
    assert!(agent["details"]["pending_permission"].is_null());
    assert!(agent["details"]["last_notification"].is_null());
    assert!(
        control.waiting().is_empty(),
        "old session's prompt is answerable"
    );
    assert_eq!(agent["details"]["invocation"], invocation);
    control.type_line(pane, "go");

    // Loop transitions come only from completed wakeup calls; the push
    // notification text is kept verbatim, and none of it is a status.
    let agent = control.until("loop stop", |agent| {
        agent["details"]["loop"]["state"] == "stopped" && agent["status"] == "Idle"
    });
    assert!(agent["details"]["loop"]["delay_seconds"].is_null());
    assert_eq!(
        agent["details"]["last_notification"]["message"],
        "Demo done; loop stopped."
    );
    control.type_line(pane, "go");

    // A new invocation of the same agent in the same Pane starts clean.
    let restart = hook_file(
        &files,
        "restart",
        session_start("s-3", Some("/tmp/demo/three.jsonl")),
    );
    let ask_three = hook_file(&files, "ask-three", permission("s-3", "Bash", "ls"));
    std::fs::write(
        files.join("restart.sh"),
        format!("{}; read go", cat(&[restart, ask_three])),
    )
    .unwrap();
    // Leave the first job's process group before starting the next one.
    thread::sleep(Duration::from_millis(200));
    control.type_line(
        pane,
        &format!("sh '{}'", files.join("restart.sh").display()),
    );
    let agent = control.until("restart", |agent| agent["details"]["session_id"] == "s-3");
    assert_ne!(agent["details"]["invocation"], invocation);
    assert!(agent["details"]["loop"].is_null());
    assert_eq!(agent["details"]["transcript_path"], "/tmp/demo/three.jsonl");
    let agent = control.until("restart permission", |agent| {
        agent["status"] == "Permission"
    });
    let fresh = agent["waiting"].as_u64().unwrap();
    control.type_line(pane, "go");

    // The durable log tells the same story in order.
    let log = events(&socket);
    let is = |name: &str, pane_id: u64| {
        let name = name.to_string();
        move |event: &Value| event[&name]["pane"] == pane_id
    };
    let first_bind = position(&log, 0, is("AgentBound", pane));
    let details: Vec<&Value> = log
        .iter()
        .filter_map(|event| event.get("AgentDetail"))
        .collect();
    assert!(details
        .iter()
        .any(|detail| detail["detail"]["Loop"]["state"] == "stopped"));
    assert!(details
        .iter()
        .any(|detail| { detail["detail"]["PermissionAsked"]["tool"] == "Bash" }));
    let created: Vec<u64> = log
        .iter()
        .filter_map(|event| event["WaitingCreated"]["item"]["id"].as_u64())
        .collect();
    assert_eq!(
        created.first(),
        Some(&id),
        "permission churned: {created:?}"
    );
    assert_eq!(created.iter().filter(|created| **created == id).count(), 1);
    assert_eq!(created.last(), Some(&fresh));
    // Session replacement: the observation, then the retirement.
    let replaced_at = position(&log, first_bind, |event| {
        event["AgentSessionObserved"]["session_id"] == "s-2"
    });
    assert!(created.contains(&second_ask));
    let retired = position(&log, replaced_at, |event| {
        event["WaitingResolved"]["id"] == second_ask
    });
    assert_eq!(retired, replaced_at + 1);
    // Restart: a fresh binding with its own invocation.
    let rebound = position(&log, retired, is("AgentBound", pane));
    assert_ne!(log[rebound]["AgentBound"]["invocation"], invocation);
}

#[test]
fn a_detail_only_envelope_never_binds_an_agent() {
    isolate_state();
    let workspace = unique_workspace_name();
    let dir = common::socket_root().join(format!("uniterm-details-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{workspace}.sock"));
    let files = common::temp_dir("agent-details-unbound");
    let server_socket = socket.clone();
    thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 120, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    let mut control = Control::connect(&socket);
    let pane = control.request(ControlCommand::PaneList)["panes"][0]["id"]
        .as_u64()
        .unwrap();
    let notice = hook_file(
        &files,
        "notice",
        notification("s-1", "push_notification", "hello"),
    );
    let loop_stop = hook_file(&files, "loop", wakeup("s-1", json!({"stop": true})));
    control.type_line(
        pane,
        &format!("{}; echo DONE-MARK", cat(&[notice, loop_stop])),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !control.read(pane).contains("DONE-MARK\n") {
        assert!(Instant::now() < deadline, "script never ran");
        thread::sleep(Duration::from_millis(20));
    }
    // Neither a binding nor a default Working status was created.
    assert!(control.agents()["entries"].as_array().unwrap().is_empty());
    assert!(!events(&socket)
        .iter()
        .any(|event| event.get("AgentBound").is_some() || event.get("AgentStatus").is_some()));
}

#[test]
fn a_new_invocation_already_blocked_gets_its_own_waiting_item() {
    isolate_state();
    let workspace = unique_workspace_name();
    let dir = common::socket_root().join(format!("uniterm-details-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{workspace}.sock"));
    let files = common::temp_dir("agent-details-regroup");
    let server_socket = socket.clone();
    thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 120, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    let mut control = Control::connect(&socket);
    let pane = control.request(ControlCommand::PaneList)["panes"][0]["id"]
        .as_u64()
        .unwrap();
    let start = hook_file(&files, "start", session_start("s-1", None));
    let ask = hook_file(&files, "ask", permission("s-1", "Bash", "git push"));
    // The first invocation blocks on a permission, then hands the terminal to
    // an interactive child shell in its own process group while it lives on.
    // The invocation is the group that owns the terminal when the envelope is
    // parsed, so the child may start only after the first prompt is observed.
    std::fs::write(
        files.join("first.sh"),
        format!("{}; read go; sh -i", cat(&[start, ask])),
    )
    .unwrap();
    control.type_line(pane, &format!("sh '{}'", files.join("first.sh").display()));
    let first = control.until("first permission", |agent| {
        agent["status"] == "Permission" && agent["details"]["pending_permission"]["tool"] == "Bash"
    });
    let old_id = first["waiting"].as_u64().unwrap();
    let old_invocation = first["details"]["invocation"].clone();
    control.type_line(pane, "go");
    // The child shell proves it is reading the terminal before it speaks.
    control.type_line(pane, "echo CHILD_$((40+2))");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !control
        .read(pane)
        .lines()
        .any(|line| line.trim() == "CHILD_42")
    {
        assert!(Instant::now() < deadline, "child shell never became ready");
        thread::sleep(Duration::from_millis(20));
    }
    // Screen evidence from the child never relabels the first invocation.
    assert_eq!(control.agent()["details"]["invocation"], old_invocation);
    assert_eq!(control.agent()["waiting"], old_id);
    // The new process group reports a bare permission (no tool payload).
    control.type_line(
        pane,
        r#"printf '\033]777;notify;uniterm://cli-agent;{"agent":"claude","event":"permission_request"}\007'"#,
    );
    let second = control.until("new invocation", |agent| {
        agent["details"]["invocation"] != old_invocation
    });
    assert_eq!(second["status"], "Permission");
    assert!(second["details"]["pending_permission"].is_null());
    assert!(second["details"]["session_id"].is_null());
    let deadline = Instant::now() + Duration::from_secs(5);
    let items = loop {
        let items = control.waiting();
        if items.len() == 1 && items[0]["id"] != old_id {
            break items;
        }
        assert!(
            Instant::now() < deadline,
            "no fresh waiting item: {items:?}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(control.agent()["waiting"], items[0]["id"]);
    control.type_line(pane, "exit");
}

#[test]
fn a_batched_session_replacement_never_creates_the_old_sessions_prompt() {
    isolate_state();
    let workspace = unique_workspace_name();
    let dir = common::socket_root().join(format!("uniterm-details-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{workspace}.sock"));
    let files = common::temp_dir("agent-details-batched");
    let server_socket = socket.clone();
    thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 120, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    let mut control = Control::connect(&socket);
    let pane = control.request(ControlCommand::PaneList)["panes"][0]["id"]
        .as_u64()
        .unwrap();
    let start = hook_file(&files, "start", session_start("s-1", None));
    let ask = hook_file(&files, "ask", permission("s-1", "Bash", "git push"));
    let replaced = hook_file(&files, "replaced", session_start("s-2", None));
    // One file and one write: the permission and the replacement reach the
    // server in the same read, so their transitions share one batch.
    let mut batch = std::fs::read(&ask).unwrap();
    batch.extend(std::fs::read(&replaced).unwrap());
    std::fs::write(files.join("batch"), batch).unwrap();
    std::fs::write(
        files.join("agent.sh"),
        format!(
            "{}; read go; {}; read go",
            cat(&[start]),
            cat(&[files.join("batch")])
        ),
    )
    .unwrap();
    control.type_line(pane, &format!("sh '{}'", files.join("agent.sh").display()));
    control.until("first session", |agent| {
        agent["details"]["session_id"] == "s-1"
    });
    control.type_line(pane, "go");
    let agent = control.until("replacement", |agent| {
        agent["details"]["session_id"] == "s-2"
    });
    assert_eq!(agent["status"], "Starting");
    assert!(agent["waiting"].is_null());
    assert!(control.waiting().is_empty());
    // However the bytes were split into reads, no prompt of the old session
    // is answerable now, and none was created after the replacement.
    let log = events(&socket);
    let replaced_at = position(&log, 0, |event| {
        event["AgentSessionObserved"]["session_id"] == "s-2"
    });
    assert!(!log[replaced_at..]
        .iter()
        .any(|event| event.get("WaitingCreated").is_some()));
    control.type_line(pane, "go");
}
