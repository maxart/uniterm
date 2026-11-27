//! Real transport, CLI, process ownership and restart tests in a private Workspace.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use uniterm_core::inbox::{Author, Kind, Operation};
use uniterm_proto::{
    ControlCommand, ControlFrame, ControlRequest, ControlResponse, ControlResult, InboxIdentity,
    PaneId, ProjectId,
};
mod common;
struct Workspace {
    socket: PathBuf,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Workspace {
    fn start(socket: PathBuf) -> Self {
        let target = socket.clone();
        let thread = std::thread::spawn(move || {
            let script = r#"printf '\033]777;notify;uniterm://cli-agent;{"agent":"codex","event":"session_start"}\007'; while IFS= read -r command; do eval "$command"; done"#;
            let (mut server, mut poll) =
                uniterm_server::Server::bind(&target, "/bin/bash", &["-c", script], 120, 30)
                    .unwrap();
            server.set_config(uniterm_server::server::load_config());
            server.run(&mut poll).unwrap();
        });
        common::wait_for_socket(&socket.with_extension("control.sock"));
        Self {
            socket,
            thread: Some(thread),
        }
    }
    fn request(&self, command: ControlCommand) -> ControlResponse {
        request(&self.socket, false, command)
    }
    fn inbox(&self, epoch: Option<u64>, operation: Operation) -> ControlResponse {
        self.request(ControlCommand::Inbox {
            project: None,
            identity: None,
            epoch,
            operation,
        })
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        if let Ok(mut client) = UnixStream::connect(&self.socket) {
            let _ = client.write_all(&uniterm_proto::encode_frame(
                &uniterm_proto::ClientMessage::KillServer,
            ));
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
fn request(socket: &Path, restricted: bool, command: ControlCommand) -> ControlResponse {
    let control = socket.with_extension("control.sock");
    let control = if restricted {
        control
            .parent()
            .unwrap()
            .join("inbox")
            .join(control.file_name().unwrap())
    } else {
        control
    };
    let mut stream = UnixStream::connect(control).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        serde_json::to_string(&ControlRequest {
            version: 1,
            id: 1,
            workspace: socket.file_stem().unwrap().to_str().unwrap().into(),
            command
        })
        .unwrap()
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    match serde_json::from_str(&line).unwrap() {
        ControlFrame::Response(r) => r,
        other => panic!("{other:?}"),
    }
}
fn page(r: ControlResponse) -> (u64, ProjectId, uniterm_core::inbox::Page, Option<u64>) {
    match r.result {
        Some(ControlResult::Inbox {
            epoch,
            project,
            page,
            id,
            ..
        }) => (epoch, project, page, id),
        _ => panic!("{r:?}"),
    }
}
fn post(key: &str, body: &str, ttl_ms: u64) -> Operation {
    Operation::Post {
        kind: Kind::Proposal,
        body: body.into(),
        reply_to: None,
        to: None,
        task: None,
        key: key.into(),
        ttl_ms: Some(ttl_ms),
    }
}
// The attach socket uses bincode, while agent posts use the JSON control socket.
fn reader_reply(socket: &Path, msg: uniterm_proto::ClientMessage) -> uniterm_proto::ServerMessage {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(&uniterm_proto::encode_frame(&msg))
        .unwrap();
    next_reader_reply(&mut stream, &mut uniterm_proto::FrameDecoder::new())
}

fn next_reader_reply(
    stream: &mut UnixStream,
    decoder: &mut uniterm_proto::FrameDecoder,
) -> uniterm_proto::ServerMessage {
    use std::io::Read;
    loop {
        while let Some(message) = decoder.decode().unwrap() {
            if matches!(
                message,
                uniterm_proto::ServerMessage::InboxProjects { .. }
                    | uniterm_proto::ServerMessage::InboxPage { .. }
                    | uniterm_proto::ServerMessage::InboxProjectRemoved { .. }
                    | uniterm_proto::ServerMessage::InboxThreadPage { .. }
                    | uniterm_proto::ServerMessage::InboxDelta { .. }
                    | uniterm_proto::ServerMessage::Settings { saved: true, .. }
                    | uniterm_proto::ServerMessage::Settings { error: Some(_), .. }
            ) {
                return message;
            }
        }
        let mut buf = [0; 8192];
        let n = stream.read(&mut buf).unwrap();
        assert_ne!(n, 0);
        decoder.push(&buf[..n]);
    }
}

fn cli(socket: &Path, state: &Path, runtime: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ut"))
        .arg("inbox")
        .args(args)
        .env("UNITERM_SOCKET", socket)
        .env_remove("UNITERM_PANE_ID")
        .env_remove("UNITERM_INBOX_IDENTITY")
        .env("XDG_STATE_HOME", state)
        .env("XDG_RUNTIME_DIR", runtime)
        .output()
        .unwrap()
}
#[test]
fn collaboration_cli_scope_watch_claims_expiry_and_restart() {
    let state = common::isolate_state();
    // A server launched by an agent must not pass that agent's grant to new Panes.
    std::env::set_var("UNITERM_INBOX_IDENTITY", "obsolete-parent-identity");
    let runtime = common::socket_root().join(format!("uib-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    std::env::set_var("XDG_CONFIG_HOME", runtime.join("config"));
    let socket = uniterm_server::server::default_socket_path(&common::unique_workspace_name());
    for verb in ["read", "reply", "comment", "claim", "post", "watch"] {
        let help = cli(&socket, &state, &runtime, &[verb, "--help"]);
        assert!(
            help.status.success(),
            "help must work before a server is started"
        );
        assert!(String::from_utf8_lossy(&help.stdout).contains("--epoch"));
    }
    let mut hook = Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(["agent", "inbox-context", "claude"])
        .env("UNITERM", "1")
        .env("UNITERM_SOCKET", &socket)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(br#"{"hook_event_name":"SessionStart"}"#)
        .unwrap();
    let context = hook.wait_with_output().unwrap();
    assert!(context.status.success());
    let context: serde_json::Value = serde_json::from_slice(&context.stdout).unwrap();
    assert!(context["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .contains("ut inbox discover"));
    let ws = Workspace::start(socket.clone());
    assert!(matches!(
        ws.request(ControlCommand::Capabilities).result,
        Some(ControlResult::Capabilities { capabilities, .. })
            if capabilities.iter().any(|capability| capability == "inbox.threads.v1")
    ));
    let (epoch, project, initial, _) = page(ws.inbox(None, Operation::Discover));
    assert!(initial.entries.is_empty());
    let discovery = cli(&socket, &state, &runtime, &["discover", "--json"]);
    assert!(discovery.status.success(), "{:?}", discovery);
    assert!(String::from_utf8_lossy(&discovery.stdout).contains("untrusted"));
    let (same_epoch, _, posted, id) = page(ws.inbox(
        None,
        post("proposal-1", "PRIVATE_TEST_BODY: proposed design", 60_000),
    ));
    assert_eq!(same_epoch, epoch);
    let id = id.unwrap();
    assert_eq!(posted.entries.len(), 1);
    use uniterm_proto::{ClientMessage, ServerMessage};
    assert!(
        matches!(reader_reply(&socket, ClientMessage::InboxProjects {request: 11}),
        ServerMessage::InboxProjects {request: 11, active, projects} if active == project && projects.iter().any(|p| p.0 == project))
    );
    assert!(
        matches!(reader_reply(&socket, ClientMessage::InboxRead {request: 12, project, cursor: None}),
        ServerMessage::InboxPage {request: 12, project: p, posts, status: uniterm_proto::InboxPageStatus::Complete { retained: 1 }, ..} if p == project && posts.len() == 1 && posts[0].id == id && posts[0].body == "PRIVATE_TEST_BODY: proposed design" && posts[0].author == uniterm_proto::InboxAuthor::Human)
    );
    assert!(matches!(
        reader_reply(
            &socket,
            ClientMessage::InboxRead {
                request: 13,
                project: ProjectId(u64::MAX),
                cursor: None
            }
        ),
        ServerMessage::InboxProjectRemoved {
            project: ProjectId(u64::MAX)
        }
    ));

    assert_eq!(
        page(ws.inbox(
            None,
            post("proposal-1", "PRIVATE_TEST_BODY: proposed design", 60_000)
        ))
        .3,
        Some(id)
    );
    assert_eq!(
        ws.inbox(None, post("proposal-1", "different", 60_000))
            .error
            .unwrap()
            .code,
        "idempotency_conflict"
    );
    assert_eq!(
        ws.inbox(None, Operation::Ack { id }).error.unwrap().code,
        "epoch_required"
    );
    assert_eq!(
        ws.inbox(Some(epoch + 1), Operation::Ack { id })
            .error
            .unwrap()
            .code,
        "epoch_mismatch"
    );
    let cursor = page(ws.inbox(Some(epoch), Operation::Ack { id })).2.cursor;
    let watch_socket = socket.clone();
    let watch = std::thread::spawn(move || {
        request(
            &watch_socket,
            false,
            ControlCommand::Inbox {
                project: Some(project),
                identity: None,
                epoch: Some(epoch),
                operation: Operation::Watch {
                    after: cursor,
                    timeout_ms: 3000,
                },
            },
        )
    });
    let feedback = cli(
        &socket,
        &state,
        &runtime,
        &[
            "post",
            "--type",
            "critique",
            "--key",
            "critique-1",
            "--reply-to",
            &id.to_string(),
            "--epoch",
            &epoch.to_string(),
            "Consider a smaller interface.",
            "--json",
        ],
    );
    assert!(
        feedback.status.success(),
        "{}",
        String::from_utf8_lossy(&feedback.stderr)
    );
    assert!(page(watch.join().unwrap())
        .2
        .entries
        .iter()
        .any(|e| e.message.kind == Kind::Critique));
    // A discussion may exceed a control frame page, and acknowledgements
    // can move an old post late in the change-ordered listing.
    let context = page(ws.inbox(None, post("context", "Build log", 60_000)))
        .3
        .unwrap();
    let feedback: serde_json::Value = serde_json::from_slice(&feedback.stdout).unwrap();
    let feedback_id = feedback["data"]["id"].as_u64().unwrap();
    let comment = cli(
        &socket,
        &state,
        &runtime,
        &[
            "comment",
            &feedback_id.to_string(),
            "--ref",
            &context.to_string(),
            "--epoch",
            &epoch.to_string(),
            "--key",
            "nested",
            "--json",
            "The test passed.",
        ],
    );
    assert!(comment.status.success(), "{comment:?}");
    let nested: serde_json::Value = serde_json::from_slice(&comment.stdout).unwrap();
    let nested = nested["data"]["id"].as_u64().unwrap();
    for index in 0..19 {
        let mut op = post(&format!("reply-{index}"), "Further evidence", 60_000);
        if let Operation::Post { reply_to, .. } = &mut op {
            *reply_to = Some(id);
        }
        let (receipt_epoch, _, receipt, affected) = page(ws.inbox(Some(epoch), op));
        assert_eq!(receipt_epoch, epoch);
        assert!(!receipt.more);
        assert_eq!(receipt.entries.len(), 1);
        assert_eq!(Some(receipt.entries[0].message.id), affected);
        assert_eq!(receipt.entries[0].message.thread, id);
        assert_eq!(receipt.cursor, receipt.entries[0].changed);
    }
    let (_, _, receipt, _) = page(ws.inbox(Some(epoch), Operation::Ack { id: feedback_id }));
    assert_eq!(receipt.entries.len(), 1);
    assert_eq!(receipt.entries[0].message.id, feedback_id);
    assert!(receipt.entries[0].acknowledged.contains(&Author::Human));
    let read = cli(
        &socket,
        &state,
        &runtime,
        &[
            "read",
            &format!("thread:{nested}"),
            "--epoch",
            &epoch.to_string(),
            "--json",
        ],
    );
    assert!(read.status.success(), "{read:?}");
    let read: serde_json::Value = serde_json::from_slice(&read.stdout).unwrap();
    let messages = read["data"]["page"]["entries"].as_array().unwrap();
    assert_eq!(messages.len(), 22);
    assert_eq!(messages[0]["message"]["id"], id);
    assert_eq!(messages[1]["message"]["id"], feedback_id);
    assert_eq!(messages[2]["message"]["reply_to"], feedback_id);
    assert!(messages[2]["message"]["body"]
        .as_str()
        .unwrap()
        .contains(&format!("thread:{context}")));
    let claim = cli(
        &socket,
        &state,
        &runtime,
        &[
            "claim",
            &id.to_string(),
            "--revision",
            "0",
            "--epoch",
            &epoch.to_string(),
            "--json",
        ],
    );
    assert!(claim.status.success(), "{:?}", claim);
    assert_eq!(
        ws.inbox(
            Some(epoch),
            Operation::Claim {
                id,
                revision: 0,
                lease_ms: 2000
            }
        )
        .error
        .unwrap()
        .code,
        "revision_conflict"
    );
    page(ws.inbox(Some(epoch), Operation::Release { id, revision: 1 }));
    let contenders: Vec<_> = (0..2)
        .map(|_| {
            let socket = socket.clone();
            std::thread::spawn(move || {
                request(
                    &socket,
                    false,
                    ControlCommand::Inbox {
                        project: Some(project),
                        identity: None,
                        epoch: Some(epoch),
                        operation: Operation::Claim {
                            id,
                            revision: 2,
                            lease_ms: 2000,
                        },
                    },
                )
            })
        })
        .collect();
    let results: Vec<_> = contenders.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.error.is_none()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| r
                .error
                .as_ref()
                .is_some_and(|e| e.code == "revision_conflict"))
            .count(),
        1
    );
    page(ws.inbox(Some(epoch), Operation::Release { id, revision: 3 }));
    // A separate Project cannot see or acknowledge this Project's post.
    let other = match ws
        .request(ControlCommand::ProjectCreate {
            name: "other".into(),
            root: runtime.to_string_lossy().into_owned(),
            background: true,
        })
        .result
        .unwrap()
    {
        ControlResult::Mutation { id: Some(id), .. } => ProjectId(id),
        other => panic!("{other:?}"),
    };
    let other_request = |operation| {
        ws.request(ControlCommand::Inbox {
            project: Some(other),
            identity: None,
            epoch: Some(epoch),
            operation,
        })
    };
    assert!(page(other_request(Operation::List {
        after: 0,
        thread: None,
        limit: 50
    }))
    .2
    .entries
    .is_empty());
    assert_eq!(
        other_request(Operation::Ack { id }).error.unwrap().code,
        "not_found"
    );
    // Linking a Task must not bypass the inbox's Project boundary.
    let foreign_task = match ws
        .request(ControlCommand::TaskStart {
            launch: Box::new(uniterm_proto::TaskLaunchRequest {
                prompt: "Other Project's saved task".into(),
                project: Some("other".into()),
                options: uniterm_proto::TaskLaunchOptions {
                    save: true,
                    ..Default::default()
                },
                ..Default::default()
            }),
        })
        .result
        .unwrap()
    {
        ControlResult::Mutation { id: Some(id), .. } => id,
        result => panic!("{result:?}"),
    };
    let mut linked = post("linked-task", "Scoped assignment", 60_000);
    if let Operation::Post { task, .. } = &mut linked {
        *task = Some(foreign_task);
    }
    assert_eq!(
        ws.inbox(Some(epoch), linked.clone()).error.unwrap().code,
        "not_found"
    );
    assert!(other_request(linked).error.is_none());
    // Restricted endpoint accepts no admin operation, even when identity is omitted.
    assert_eq!(
        request(
            &socket,
            true,
            ControlCommand::Inbox {
                project: None,
                identity: None,
                epoch: None,
                operation: Operation::Discover
            }
        )
        .error
        .unwrap()
        .code,
        "inbox_only"
    );
    assert_eq!(
        request(
            &socket,
            true,
            ControlCommand::PaneSend {
                pane: PaneId(1),
                text: "bad".into()
            }
        )
        .error
        .unwrap()
        .code,
        "inbox_only"
    );
    let started = Instant::now();
    let identity = loop {
        if let Some(ControlResult::AgentHookContext {
            foreground_pid: Some(pid),
            invocation: Some(invocation),
            ..
        }) = ws
            .request(ControlCommand::AgentHookContext { pane: PaneId(1) })
            .result
        {
            break InboxIdentity {
                capability: None,
                capability_project: None,
                pane: PaneId(1),
                invocation,
                foreground_pid: pid,
            };
        }
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(10));
    };
    // This test process is not in the Pane's process tree; it cannot impersonate it.
    assert_eq!(
        request(
            &socket,
            true,
            ControlCommand::Inbox {
                project: None,
                identity: Some(identity.clone()),
                epoch: None,
                operation: Operation::Discover
            }
        )
        .error
        .unwrap()
        .code,
        "stale_invocation"
    );
    // The real CLI inside the PTY is accepted and attributed to the invocation.
    let output = runtime.join("agent.json");
    let error = runtime.join("agent.err");
    let script = format!("env XDG_STATE_HOME={} XDG_RUNTIME_DIR={} {} inbox post --key from-agent --type finding --json 'agent evidence' >{} 2>{}\n", uniterm_server::workflow::shell_quote(state.to_str().unwrap()), uniterm_server::workflow::shell_quote(runtime.to_str().unwrap()), uniterm_server::workflow::shell_quote(env!("CARGO_BIN_EXE_ut")), uniterm_server::workflow::shell_quote(output.to_str().unwrap()), uniterm_server::workflow::shell_quote(error.to_str().unwrap()));
    assert!(ws
        .request(ControlCommand::PaneSend {
            pane: PaneId(1),
            text: script
        })
        .error
        .is_none());
    let started = Instant::now();
    let agent_result: ControlResult = loop {
        if let Ok(text) = std::fs::read_to_string(&output) {
            if let Ok(result) = serde_json::from_str(&text) {
                break result;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{}",
            std::fs::read_to_string(&error).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let agent = Author::Agent {
        pane: PaneId(1),
        invocation: identity.invocation,
    };
    assert!(matches!(agent_result, ControlResult::Inbox { author, .. } if author == agent));
    // A verified invocation can delegate only its board identity to a detached backend.
    let credential_file = runtime.join("credential.json");
    let script = format!(
        "env XDG_STATE_HOME={} XDG_RUNTIME_DIR={} {} inbox credential >{} 2>{}\n",
        uniterm_server::workflow::shell_quote(state.to_str().unwrap()),
        uniterm_server::workflow::shell_quote(runtime.to_str().unwrap()),
        uniterm_server::workflow::shell_quote(env!("CARGO_BIN_EXE_ut")),
        uniterm_server::workflow::shell_quote(credential_file.to_str().unwrap()),
        uniterm_server::workflow::shell_quote(error.to_str().unwrap())
    );
    ws.request(ControlCommand::PaneSend {
        pane: PaneId(1),
        text: script,
    });
    let start = Instant::now();
    let credential: InboxIdentity = loop {
        if let Ok(text) = std::fs::read_to_string(&credential_file) {
            if let Ok(identity) = serde_json::from_str(&text) {
                break identity;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "credential bootstrap failed"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let backend = || {
        Command::new(env!("CARGO_BIN_EXE_ut"))
            .args([
                "inbox",
                "post",
                "--key",
                "backend",
                "--json",
                "backend evidence",
            ])
            .env("UNITERM_SOCKET", &socket)
            .env_remove("UNITERM_PANE_ID")
            .env(
                "UNITERM_INBOX_IDENTITY",
                serde_json::to_string(&credential).unwrap(),
            )
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap()
    };
    let result = backend();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        matches!(serde_json::from_slice::<ControlResult>(&result.stdout).unwrap(), ControlResult::Inbox { author, .. } if author == agent)
    );
    let mut forged = credential.clone();
    forged.capability = Some("wrong".into());
    assert_eq!(
        request(
            &socket,
            true,
            ControlCommand::Inbox {
                project: None,
                identity: Some(forged),
                epoch: None,
                operation: Operation::Discover
            }
        )
        .error
        .unwrap()
        .code,
        "stale_invocation"
    );
    page(ws.inbox(Some(epoch), Operation::Revoke { author: agent }));
    assert!(
        !backend().status.success(),
        "revocation must revoke backend access"
    );
    let denied_output = runtime.join("denied.json");
    let denied_error = runtime.join("denied.err");
    let done = runtime.join("denied.done");
    let script = format!("env XDG_STATE_HOME={} XDG_RUNTIME_DIR={} {} inbox post --key revoked-agent --json 'should not post' >{} 2>{}; printf done >{}\n", uniterm_server::workflow::shell_quote(state.to_str().unwrap()), uniterm_server::workflow::shell_quote(runtime.to_str().unwrap()), uniterm_server::workflow::shell_quote(env!("CARGO_BIN_EXE_ut")), uniterm_server::workflow::shell_quote(denied_output.to_str().unwrap()), uniterm_server::workflow::shell_quote(denied_error.to_str().unwrap()), uniterm_server::workflow::shell_quote(done.to_str().unwrap()));
    ws.request(ControlCommand::PaneSend {
        pane: PaneId(1),
        text: script,
    });
    let start = Instant::now();
    while !done.exists() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(std::fs::read_to_string(denied_error)
        .unwrap()
        .contains("revoked"));
    // Clear requires actual caller confirmation on the wire.
    assert_eq!(
        ws.inbox(Some(epoch), Operation::Clear { confirmed: false })
            .error
            .unwrap()
            .code,
        "forbidden"
    );
    page(ws.inbox(Some(epoch), Operation::Clear { confirmed: true }));
    let expired = page(ws.inbox(None, post("short", "expires", 1000)))
        .3
        .unwrap();
    let cursor = page(ws.inbox(
        None,
        Operation::List {
            after: 0,
            thread: None,
            limit: 50,
        },
    ))
    .2
    .cursor;
    let expiry = ws.inbox(
        Some(epoch),
        Operation::Watch {
            after: cursor,
            timeout_ms: 3000,
        },
    );
    assert_eq!(expiry.error.unwrap().code, "cursor_expired");
    assert!(page(ws.inbox(
        None,
        Operation::List {
            after: 0,
            thread: None,
            limit: 50
        }
    ))
    .2
    .entries
    .iter()
    .all(|e| e.message.id != expired));
    // No provider hook or provider-specific board adapter is needed. Exercise
    // the same real CLI/PTY path across the bundled provider identities.
    let mut previous = "codex";
    for provider in [
        "claude", "codex", "grok", "cursor", "opencode", "pi", "gemini", "omp",
    ] {
        let announce = format!(
            "{}; {}\n",
            uniterm_server::workflow::osc777_announce(previous, "session_end"),
            uniterm_server::workflow::osc777_announce(provider, "session_start")
        );
        ws.request(ControlCommand::PaneSend {
            pane: PaneId(1),
            text: announce,
        });
        let start = Instant::now();
        loop {
            if matches!(ws.request(ControlCommand::AgentList).result, Some(ControlResult::Fleet { entries, .. }) if entries.iter().any(|e| e.pane_id == PaneId(1) && e.agent == provider))
            {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "provider {provider} never bound"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = runtime.join(format!("{provider}.json"));
        let error = runtime.join(format!("{provider}.err"));
        let script = format!("env XDG_STATE_HOME={} XDG_RUNTIME_DIR={} {} inbox post --type finding --key parity --json 'shared protocol works' >{} 2>{}\n", uniterm_server::workflow::shell_quote(state.to_str().unwrap()), uniterm_server::workflow::shell_quote(runtime.to_str().unwrap()), uniterm_server::workflow::shell_quote(env!("CARGO_BIN_EXE_ut")), uniterm_server::workflow::shell_quote(output.to_str().unwrap()), uniterm_server::workflow::shell_quote(error.to_str().unwrap()));
        ws.request(ControlCommand::PaneSend {
            pane: PaneId(1),
            text: script,
        });
        let start = Instant::now();
        let result: ControlResult = loop {
            if let Ok(text) = std::fs::read_to_string(&output) {
                if let Ok(result) = serde_json::from_str(&text) {
                    break result;
                }
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "{provider}: {}",
                std::fs::read_to_string(&error).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            matches!(
                result,
                ControlResult::Inbox {
                    author: Author::Agent {
                        pane: PaneId(1),
                        ..
                    },
                    ..
                }
            ),
            "{provider}"
        );
        previous = provider;
    }
    // Keep an actual reader attached after its final page. A removal from
    // another connection must retire the open Project without a refresh.
    let mut reader = UnixStream::connect(&socket).unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut decoder = uniterm_proto::FrameDecoder::new();
    for message in [
        ClientMessage::Attach {
            term: "xterm".into(),
            rows: 30,
            cols: 120,
        },
        ClientMessage::OverlayVisible { on: true },
        ClientMessage::InboxProjects { request: 60 },
        ClientMessage::InboxRead {
            request: 61,
            project: other,
            cursor: None,
        },
    ] {
        reader
            .write_all(&uniterm_proto::encode_frame(&message))
            .unwrap();
    }
    assert!(matches!(
        next_reader_reply(&mut reader, &mut decoder),
        ServerMessage::InboxProjects { request: 60, .. }
    ));
    assert!(
        matches!(next_reader_reply(&mut reader, &mut decoder), ServerMessage::InboxPage { request: 61, posts, status: uniterm_proto::InboxPageStatus::Complete { .. }, .. } if !posts.is_empty())
    );
    // A genuine post rearms this attachment through the ordinary snapshot path.
    assert!(other_request(post("live-update", "New discussion", 60_000))
        .error
        .is_none());
    assert!(matches!(
        next_reader_reply(&mut reader, &mut decoder),
        ServerMessage::InboxProjects { request: 61, .. }
    ));
    reader
        .write_all(&uniterm_proto::encode_frame(&ClientMessage::InboxRead {
            request: 62,
            project: other,
            cursor: None,
        }))
        .unwrap();
    assert!(
        matches!(next_reader_reply(&mut reader, &mut decoder), ServerMessage::InboxPage {request:62, posts, ..}
        if posts.iter().any(|post| post.body=="New discussion"))
    );
    // New attachments explicitly opt into bounded row updates; legacy readers above
    // still receive only the wire variants their installed decoder understands.
    let live_request = uniterm_proto::INBOX_LIVE_REQUEST | 63;
    reader
        .write_all(&uniterm_proto::encode_frame(&ClientMessage::InboxRead {
            request: live_request,
            project: other,
            cursor: None,
        }))
        .unwrap();
    let root = match next_reader_reply(&mut reader, &mut decoder) {
        ServerMessage::InboxThreadPage { request, posts, .. } if request == live_request => {
            posts
                .iter()
                .find(|post| post.body == "New discussion")
                .unwrap()
                .id
        }
        message => panic!("expected extended page: {message:?}"),
    };
    let mut answer = post("delta-reply", "One changed reply", 60_000);
    if let Operation::Post { reply_to, .. } = &mut answer {
        *reply_to = Some(root);
    }
    let reply = page(other_request(answer)).3.unwrap();
    assert!(
        matches!(next_reader_reply(&mut reader,&mut decoder), ServerMessage::InboxDelta {
        request,project,delta,..
    } if request==live_request && project==other && delta.posts.len()==1
        && delta.posts[0].id==reply && delta.parents==[(reply,root)] && delta.removed.is_empty())
    );
    assert!(ws
        .request(ControlCommand::ProjectRename {
            project: other,
            name: "Renamed board".into()
        })
        .error
        .is_none());
    assert!(matches!(next_reader_reply(&mut reader, &mut decoder),
        ServerMessage::InboxProjects { request: uniterm_proto::INBOX_LIVE_REQUEST, projects, .. }
        if projects.iter().any(|(id,name)| *id == other && name == "Renamed board")));
    assert!(ws
        .request(ControlCommand::ProjectRemove {
            project: other,
            confirmed: true
        })
        .error
        .is_none());
    assert!(
        matches!(next_reader_reply(&mut reader, &mut decoder), ServerMessage::InboxProjectRemoved { project } if project == other)
    );
    drop(reader);
    // Apply the Settings choice over the attach transport, post with the CLI's
    // implicit lifetime, then cold-start the real board from the same host config.
    use uniterm_core::inbox::Retention;
    let patch = uniterm_proto::SettingsPatch {
        inbox_retention: Some(Retention::Forever),
        ..Default::default()
    };
    // Block only this fixture's atomic config write, then retry the identical choice.
    let temporary_config = uniterm_server::server::config_path()
        .unwrap()
        .with_extension("conf.tmp");
    std::fs::create_dir_all(&temporary_config).unwrap();
    assert!(matches!(
        reader_reply(&socket, ClientMessage::SettingsApply(patch.clone())),
        ServerMessage::Settings {
            saved: false,
            error: Some(_),
            ..
        }
    ));
    std::fs::remove_dir(&temporary_config).unwrap();
    assert!(
        matches!(reader_reply(&socket, ClientMessage::SettingsApply(patch)), ServerMessage::Settings { settings, saved: true, error: None } if settings.inbox_retention == Retention::Forever)
    );
    assert_eq!(
        uniterm_server::server::load_config().inbox_retention,
        Retention::Forever
    );
    let body = "PRIVATE_TEST_BODY: retained across restart";
    let posted = cli(
        &socket,
        &state,
        &runtime,
        &["post", "--type", "finding", "--key", "restart", body],
    );
    assert!(posted.status.success(), "{:?}", posted);
    drop(ws);
    let ws = Workspace::start(socket.clone());
    let found = page(ws.inbox(
        None,
        Operation::List {
            after: 0,
            thread: None,
            limit: 20,
        },
    ))
    .2
    .entries;
    assert!(found
        .iter()
        .any(|entry| entry.message.body == body && entry.message.expires_ms.is_none()));
    assert_eq!(
        ws.inbox(Some(epoch), Operation::Ack { id })
            .error
            .unwrap()
            .code,
        "epoch_mismatch"
    );
    drop(ws);
    fn verify_no_body(path: &Path) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                verify_no_body(&path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                assert!(
                    !String::from_utf8_lossy(&bytes).contains("PRIVATE_TEST_BODY"),
                    "body persisted in {}",
                    path.display()
                );
            }
        }
    }
    verify_no_body(&state);
    std::fs::remove_dir_all(runtime).unwrap();
}

/// Published v1.2.28 returns an explicit serde refusal and keeps the socket open.
#[test]
fn legacy_thread_fallback_reuses_the_connection_and_never_downgrades_citations() {
    let state = common::isolate_state();
    let runtime = common::temp_dir("inbox-legacy");
    let socket = runtime.join(format!("{}.sock", common::unique_workspace_name()));
    let listener =
        std::os::unix::net::UnixListener::bind(socket.with_extension("control.sock")).unwrap();
    let server = std::thread::spawn(move || {
        let project = ProjectId(1);
        let mut board = uniterm_core::inbox::Board::default();
        let root = board
            .apply(project, Author::Human, post("root", "opening", 60_000), 0)
            .unwrap()
            .unwrap();
        let mut reply = post("reply", "comment", 60_000);
        if let Operation::Post { reply_to, .. } = &mut reply {
            *reply_to = Some(root);
        }
        let child = board
            .apply(project, Author::Human, reply, 0)
            .unwrap()
            .unwrap();
        assert_eq!((root, child), (1, 2));

        for verb in ["read", "cite"] {
            let mut client = BufReader::new(listener.accept().unwrap().0);
            client
                .get_ref()
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut line = String::new();
            client.read_line(&mut line).unwrap();
            let wire: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(wire["params"]["operation"]["op"], verb);
            let refused = ControlResponse::error(1,"invalid_request",format!("unknown variant `{verb}`, expected one of `discover`, `credential`, `list`, `watch`, `post`, `ack`, `claim`, `release`, `pause`, `clear`, `revoke`"));
            writeln!(
                client.get_mut(),
                "{}",
                serde_json::to_string(&ControlFrame::Response(refused)).unwrap()
            )
            .unwrap();
            if verb == "read" {
                // Old servers match only exact root IDs on their thread filter.
                for thread in [Some(child), None, Some(root)] {
                    line.clear();
                    client.read_line(&mut line).unwrap();
                    let wire: ControlRequest = serde_json::from_str(&line).unwrap();
                    assert!(matches!(wire.command, ControlCommand::Inbox {
                        epoch:Some(42), operation: Operation::List{after:0, thread: t,..},..
                    } if t == thread));
                    let page = if thread == Some(child) {
                        board.mutation_page(project, None)
                    } else {
                        board.list(project, 0, thread, 20).unwrap()
                    };
                    let response = ControlResponse::ok(
                        wire.id,
                        ControlResult::Inbox {
                            epoch: 42,
                            project,
                            author: Author::Human,
                            page,
                            id: None,
                            guidance: String::new(),
                        },
                    );
                    writeln!(
                        client.get_mut(),
                        "{}",
                        serde_json::to_string(&ControlFrame::Response(response)).unwrap()
                    )
                    .unwrap();
                }
            }
            line.clear();
            assert_eq!(client.read_line(&mut line).unwrap(), 0, "{line}");
        }
    });
    let read = cli(
        &socket,
        &state,
        &runtime,
        &["read", "2", "--epoch", "42", "--json"],
    );
    assert!(read.status.success(), "{read:?}");
    let parsed: ControlResult = serde_json::from_slice(&read.stdout).unwrap();
    assert!(
        matches!(parsed, ControlResult::Inbox {page,..} if page.entries.iter().map(|entry|entry.message.id).collect::<Vec<_>>() == [1,2])
    );
    let cited = cli(
        &socket,
        &state,
        &runtime,
        &[
            "post",
            "--ref",
            "2",
            "--epoch",
            "42",
            "--key",
            "cited",
            "see comment",
        ],
    );
    assert!(!cited.status.success());
    assert!(String::from_utf8_lossy(&cited.stderr).contains("updated Workspace server"));
    server.join().unwrap();
    std::fs::remove_dir_all(runtime).unwrap();
}
