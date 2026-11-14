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
        ttl_ms,
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
    for verb in ["claim", "post", "watch"] {
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
    // Restart destroys bodies and rejects cursors/IDs from the previous epoch.
    page(ws.inbox(
        None,
        post("restart", "PRIVATE_TEST_BODY: lost on restart", 60_000),
    ));
    drop(ws);
    let ws = Workspace::start(socket.clone());
    assert!(page(ws.inbox(None, Operation::Discover))
        .2
        .entries
        .is_empty());
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
