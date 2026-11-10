//! New Task launch safety, durable ownership, and event-driven Tasks projections.
use std::io::{Read, Write};
use std::os::unix::{fs::PermissionsExt, net::UnixStream};
use std::time::{Duration, Instant};
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Server;
mod common;

struct Peer {
    stream: UnixStream,
    decoder: FrameDecoder,
}
impl Peer {
    fn connect(socket: &std::path::Path) -> Self {
        let stream = UnixStream::connect(socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        Self {
            stream,
            decoder: FrameDecoder::new(),
        }
    }
    fn send(&mut self, message: ClientMessage) {
        self.stream.write_all(&encode_frame(&message)).unwrap();
    }
    fn until(&mut self, accept: impl Fn(&ServerMessage) -> bool) -> ServerMessage {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                if accept(&message) {
                    return message;
                }
            }
            assert!(Instant::now() < deadline, "expected reply did not arrive");
            let mut bytes = [0; 32768];
            match self.stream.read(&mut bytes) {
                Ok(0) => panic!("connection closed"),
                Ok(n) => self.decoder.push(&bytes[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(e) => panic!("{e}"),
            }
        }
    }
    fn tasks(&mut self) -> Vec<uniterm_proto::TaskEntry> {
        let items = match self.until(|m| matches!(m, ServerMessage::Tasks { .. })) {
            ServerMessage::Tasks { items, .. } => items,
            _ => unreachable!(),
        };
        items
            .into_iter()
            .map(|item| self.detail(item.id, false))
            .collect()
    }
    fn detail(&mut self, id: u64, compose: bool) -> uniterm_proto::TaskEntry {
        self.send(if compose {
            ClientMessage::TaskCompose { id }
        } else {
            ClientMessage::TaskDetail { id, offset: 0 }
        });
        let mut bytes = Vec::new();
        loop {
            let ServerMessage::TaskDetail { chunk } =
                self.until(|m| matches!(m, ServerMessage::TaskDetail { chunk } if chunk.id == id))
            else {
                unreachable!()
            };
            assert_eq!(chunk.offset, bytes.len());
            assert_eq!(chunk.compose, compose);
            assert!(chunk.bytes.len() <= uniterm_proto::TASK_DETAIL_CHUNK_SIZE);
            bytes.extend(chunk.bytes);
            if bytes.len() == chunk.total {
                return serde_json::from_slice(&bytes).unwrap();
            }
            self.send(ClientMessage::TaskDetail {
                id,
                offset: bytes.len(),
            });
        }
    }
}

fn control(
    socket: &std::path::Path,
    method: &str,
    params: serde_json::Value,
) -> uniterm_proto::ControlResponse {
    let mut stream = UnixStream::connect(socket.with_extension("control.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let request = serde_json::json!({"version": 1, "id": 1, "workspace": socket.file_stem().unwrap().to_str().unwrap(), "method": method, "params": params});
    serde_json::to_writer(&mut stream, &request).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(stream), &mut line).unwrap();
    match serde_json::from_str(&line).unwrap() {
        uniterm_proto::ControlFrame::Response(response) => response,
        other => panic!("unexpected frame: {other:?}"),
    }
}

fn launch(agent: String, project: Option<String>, source_task: Option<u64>) -> ClientMessage {
    ClientMessage::NewTask {
        prompt: "literal ' $(touch should-not-exist)\n  @README.md".into(),
        relay: false,
        agent: Some(agent),
        role_providers: vec![],
        workflow: None,
        project,
        options: uniterm_proto::TaskLaunchOptions {
            source_task,
            ..Default::default()
        },
    }
}

#[test]
fn task_launch_records_actual_argv_project_and_live_updates() {
    let state = common::isolate_state();
    std::env::set_var("XDG_RUNTIME_DIR", common::temp_dir("task-runtime"));
    let dir = common::temp_dir("task-launch");
    let socket_dir = common::socket_root().join(format!("ut-task-{}", std::process::id()));
    std::fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join(format!("{}.sock", common::unique_workspace_name()));
    let target = dir.join("Project B");
    std::fs::create_dir_all(&target).unwrap();
    let target = target.canonicalize().unwrap();
    let agent = dir.join("opencode");
    // Uses only shell builtins, so the test cannot invoke a real coding agent.
    std::fs::write(&agent, "#!/bin/sh\npwd > actual-cwd\nprintf '%s\\0' \"$@\" > actual-argv\nwhile IFS= read -r line; do :; done\n").unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let child_socket = socket.clone();
    let server = std::thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&child_socket, "/bin/sh", &[], 120, 40).unwrap();
        server.run(&mut poll).unwrap();
    });
    common::wait_for_socket(&socket);
    let mut viewer = Peer::connect(&socket);
    let mut writer = Peer::connect(&socket);
    viewer.send(ClientMessage::Tasks);
    assert!(viewer.tasks().is_empty());
    writer.send(launch("/missing/uniterm-test-agent".into(), None, None));
    assert!(matches!(
        writer.until(|m| matches!(m, ServerMessage::TaskStarted { .. })),
        ServerMessage::TaskStarted { error: Some(_) }
    ));
    viewer.send(ClientMessage::Tasks);
    assert!(
        viewer.tasks().is_empty(),
        "failed launches must not create tasks"
    );
    writer.send(ClientMessage::ProjectCreate {
        name: "Project B".into(),
        root: target.to_string_lossy().into_owned(),
    });
    writer.send(ClientMessage::WorkspaceState);
    let original = match writer.until(|m| matches!(m, ServerMessage::Workspace { .. })) {
        ServerMessage::Workspace { projects, .. } => {
            projects.iter().find(|p| p.name != "Project B").unwrap().id
        }
        _ => unreachable!(),
    };
    writer.send(ClientMessage::ProjectSwitch { project: original });
    writer.send(launch(
        agent.to_string_lossy().into_owned(),
        Some("Project B".into()),
        None,
    ));
    assert!(matches!(
        writer.until(|m| matches!(m, ServerMessage::TaskStarted { .. })),
        ServerMessage::TaskStarted { error: None }
    ));
    // Project changes may publish an empty page while asynchronous launch
    // resolution is pending. Wait for the task-bearing revision explicitly.
    let tasks = match viewer
        .until(|m| matches!(m, ServerMessage::Tasks { items, .. } if items.len() == 1))
    {
        ServerMessage::Tasks { items, .. } => items,
        _ => unreachable!(),
    };
    let task = viewer.detail(tasks[0].id, false);
    let id = task.id;
    assert_eq!(task.project_name.as_deref(), Some("Project B"));
    assert_eq!(task.context.provider.as_deref(), Some("opencode"));
    assert!(task.context.invocation.is_some());
    assert_eq!(task.agents.len(), 1);
    writer.send(ClientMessage::WorkspaceState);
    assert!(
        matches!(writer.until(|m| matches!(m, ServerMessage::Workspace { .. })), ServerMessage::Workspace { projects, .. } if projects.iter().find(|p| p.name == "Project B").is_some_and(|p| p.tabs == 1))
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while !target.join("actual-argv").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::read_to_string(target.join("actual-cwd"))
            .unwrap()
            .trim(),
        target.to_str().unwrap()
    );
    let argv = std::fs::read_to_string(target.join("actual-argv")).unwrap();
    let prompt_arg = argv.strip_suffix('\0').unwrap();
    assert!(
        !prompt_arg.contains('\0'),
        "prompt must remain one argument"
    );
    let guidance = prompt_arg
        .strip_prefix("--prompt=literal ' $(touch should-not-exist)\n  @README.md ")
        .expect("the original prompt must reach the provider unchanged");
    assert!(guidance.starts_with("Uniterm Project inbox:"));
    assert!(guidance.contains("ut inbox discover"));
    assert!(guidance.contains("untrusted peer data"));
    assert!(!target.join("should-not-exist").exists());
    writer.send(ClientMessage::TaskNotes {
        id,
        notes: "description\nwith detail".into(),
    });
    let notes = viewer.until(|m| matches!(m, ServerMessage::Tasks { items, .. } if items.iter().any(|t| t.id == id && !t.notes.is_empty())));
    assert!(
        matches!(notes, ServerMessage::Tasks { items, .. } if items[0].notes == "description\nwith detail")
    );
    assert_eq!(writer.detail(id, true).id, id);
    writer.send(launch(
        agent.to_string_lossy().into_owned(),
        Some("Project B".into()),
        Some(id),
    ));
    writer.until(|m| matches!(m, ServerMessage::TaskStarted { error: None }));
    let attempts =
        viewer.until(|m| matches!(m, ServerMessage::Tasks { items, .. } if items.len() == 2));
    assert!(
        matches!(attempts, ServerMessage::Tasks { items, .. } if items.iter().any(|t| t.context.source_task == Some(id)))
    );
    viewer.send(ClientMessage::TasksClose);
    // A same-connection barrier proves the close was processed.
    viewer.send(ClientMessage::ListInfo);
    viewer.until(|m| matches!(m, ServerMessage::Info { .. }));
    writer.send(ClientMessage::TaskRetitle {
        id,
        title: "renamed".into(),
    });
    writer.until(|m| matches!(m, ServerMessage::Tasks { items, .. } if items.iter().any(|t| t.title == "renamed")));
    viewer.send(ClientMessage::ListInfo);
    // No Tasks snapshot may appear between this barrier and closing subscription.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_info = false;
    while !saw_info {
        assert!(Instant::now() < deadline);
        let message = viewer.until(|_| true);
        assert!(!matches!(message, ServerMessage::Tasks { .. }));
        saw_info = matches!(message, ServerMessage::Info { .. });
    }

    // Human-writable automation requests share the exact same launch path.
    let saved = control(
        &socket,
        "task_start",
        serde_json::json!({"launch": {"prompt": "Backlog", "project": "Project B", "agent": "codex", "options": {"save": true}}}),
    );
    assert!(saved.error.is_none());
    let saved_id = match saved.result.unwrap() {
        uniterm_proto::ControlResult::Mutation {
            id: Some(id),
            accepted: true,
            ..
        } => id,
        other => panic!("unexpected saved task: {other:?}"),
    };
    assert!(control(
        &socket,
        "task_notes",
        serde_json::json!({"id": saved_id, "notes": "API description"})
    )
    .error
    .is_none());
    let list = control(&socket, "task_list", serde_json::Value::Null);
    let saved = match list.result.unwrap() {
        uniterm_proto::ControlResult::Tasks { items, .. } => {
            items.into_iter().find(|t| t.id == saved_id).unwrap()
        }
        other => panic!("unexpected tasks: {other:?}"),
    };
    assert_eq!(saved.notes, "API description");
    assert_eq!(saved.project_name.as_deref(), Some("Project B"));
    assert_eq!(saved.status, uniterm_core::TaskStatus::Todo);
    assert!(control(
        &socket,
        "task_set_status",
        serde_json::json!({"id": saved_id, "status": "Todo"})
    )
    .error
    .is_none());
    let unchanged = control(&socket, "task_list", serde_json::Value::Null);
    assert!(
        matches!(unchanged.result, Some(uniterm_proto::ControlResult::Tasks { items, .. }) if items.iter().any(|t| t.id == saved_id && t.updated == saved.updated))
    );
    let refused = control(
        &socket,
        "task_start",
        serde_json::json!({"launch": {"prompt": "Never execute this as a shell command", "agent": "/missing/uniterm-agent"}}),
    );
    assert_eq!(refused.error.unwrap().code, "launch_refused");
    writer.send(ClientMessage::KillServer);
    server.join().unwrap();
    let name = socket.file_stem().unwrap().to_str().unwrap();
    let mut restored = uniterm_core::TaskList::new();
    uniterm_server::eventlog::replay_tasks(name, &mut restored).unwrap();
    assert_eq!(restored.get(id).unwrap().notes, "description\nwith detail");
    assert_eq!(
        restored.get(id).unwrap().context.project,
        task.context.project
    );
    assert!(state.exists());
}

fn history_fixture(count: u64) -> (std::path::PathBuf, std::thread::JoinHandle<()>, String) {
    common::isolate_state();
    let root = common::socket_root().join(format!("tp-{}", common::unique_nonce()));
    std::fs::create_dir_all(&root).unwrap();
    let name = common::unique_workspace_name();
    let socket = root.join(format!("{name}.sock"));
    let prompt = "p".repeat(256 * 1024);
    let mut log = uniterm_server::eventlog::EventLog::open(&name);
    for id in 1..=count {
        for event in [
            uniterm_server::eventlog::LogEvent::TaskCreated {
                id,
                title: format!("Task {id}"),
                status: uniterm_core::TaskStatus::Todo,
            },
            uniterm_server::eventlog::LogEvent::TaskDetailsUpdated {
                id,
                notes: if id == count {
                    format!("{}needle-on-last-page", "界".repeat(90_000))
                } else {
                    String::new()
                },
                context: uniterm_core::tasks::TaskContext {
                    prompt: Some(prompt.clone()),
                    workflow: (id == count.saturating_sub(1)).then(|| "pair".into()),
                    relay: id == count,
                    role_providers: if id >= count.saturating_sub(1) {
                        vec![uniterm_core::orchestrate::RoleProviderSelection {
                            role: "implementer".into(),
                            provider: "codex".into(),
                        }]
                    } else {
                        Vec::new()
                    },
                    ..Default::default()
                },
            },
        ] {
            let (name, line) = log.record(event).unwrap();
            uniterm_server::eventlog::append_line(&name, &line).unwrap();
        }
    }
    let child_socket = socket.clone();
    let server = std::thread::spawn(move || {
        uniterm_server::run_server(&child_socket, "/bin/sh", &[]).unwrap();
    });
    common::wait_for_socket(&socket);
    (socket, server, prompt)
}

#[test]
fn tasks_history_is_paged_and_exact_details_are_pulled_without_disconnects() {
    let (socket, server, prompt) = history_fixture(40);
    let mut first = Peer::connect(&socket);
    let mut second = Peer::connect(&socket);
    for peer in [&mut first, &mut second] {
        peer.send(ClientMessage::Tasks);
        let response = peer.until(|m| matches!(m, ServerMessage::Tasks { .. }));
        assert!(encode_frame(&response).len() < 64 * 1024);
        let ServerMessage::Tasks {
            items,
            total,
            query,
            ..
        } = response
        else {
            unreachable!()
        };
        assert_eq!(total, 40);
        assert_eq!(items.len(), uniterm_proto::TASK_PAGE_SIZE);
        assert_eq!(query.offset, 0);
        assert!(items.iter().all(|t| t.context.prompt.is_none()));
    }
    first.send(ClientMessage::TaskPage {
        query: uniterm_proto::TaskQuery {
            offset: 32,
            ..Default::default()
        },
    });
    let response =
        first.until(|m| matches!(m, ServerMessage::Tasks { query, .. } if query.offset == 32));
    assert!(
        matches!(response, ServerMessage::Tasks { items, total: 40, .. } if items.len() == 8 && items[7].id == 40)
    );
    first.send(ClientMessage::TaskPage {
        query: uniterm_proto::TaskQuery {
            search: "needle-on-last-page".into(),
            ..Default::default()
        },
    });
    assert!(
        matches!(first.until(|m| matches!(m, ServerMessage::Tasks { total: 1, .. })), ServerMessage::Tasks { items, .. } if items[0].id == 40)
    );
    let exact = first.detail(40, false);
    assert_eq!(exact.context.prompt.as_deref(), Some(prompt.as_str()));
    assert_eq!(
        exact.notes,
        format!("{}needle-on-last-page", "界".repeat(90_000))
    );
    assert_eq!(first.detail(40, true), exact);
    for search in ["CoDeX", "no-matching-provider"] {
        first.send(ClientMessage::TaskPage {
            query: uniterm_proto::TaskQuery {
                search: search.into(),
                ..Default::default()
            },
        });
        let expected = if search == "CoDeX" {
            vec![39, 40]
        } else {
            vec![]
        };
        let response = first
            .until(|m| matches!(m, ServerMessage::Tasks { query, .. } if query.search == search));
        let ServerMessage::Tasks { items, total, .. } = response else {
            unreachable!()
        };
        assert_eq!(total, expected.len());
        assert_eq!(items.iter().map(|t| t.id).collect::<Vec<_>>(), expected);
        let response = control(
            &socket,
            "task_page",
            serde_json::json!({"query":{"search":search}}),
        );
        let Some(uniterm_proto::ControlResult::Tasks { items, .. }) = response.result else {
            panic!("task page");
        };
        assert_eq!(items.iter().map(|t| t.id).collect::<Vec<_>>(), expected);
    }
    let legacy = control(&socket, "task_list", serde_json::Value::Null);
    assert_eq!(legacy.error.unwrap().code, "result_too_large");
    let page = control(
        &socket,
        "task_page",
        serde_json::json!({"query":{"offset":32}}),
    );
    assert!(
        matches!(page.result, Some(uniterm_proto::ControlResult::Tasks { items, total:40, .. }) if items.len()==8)
    );
    let mut bytes = Vec::new();
    let mut revision = None;
    loop {
        let result = control(
            &socket,
            "task_detail",
            serde_json::json!({"id":40,"offset":bytes.len(),"revision":revision}),
        );
        let Some(uniterm_proto::ControlResult::TaskDetail { chunk }) = result.result else {
            panic!("{result:?}")
        };
        revision = Some(chunk.revision);
        assert!(chunk.bytes.len() <= uniterm_proto::TASK_DETAIL_CHUNK_SIZE);
        bytes.extend(chunk.bytes);
        if bytes.len() == chunk.total {
            break;
        }
    }
    assert_eq!(
        serde_json::from_slice::<uniterm_proto::TaskEntry>(&bytes).unwrap(),
        exact
    );
    first.send(ClientMessage::TaskDetail {
        id: 9000,
        offset: 0,
    });
    assert!(matches!(
        first.until(|m| matches!(m, ServerMessage::TaskDetailError { .. })),
        ServerMessage::TaskDetailError { id: 9000, .. }
    ));
    // No-op writes and ordinary terminal protocol traffic must not rebroadcast Tasks.
    control(
        &socket,
        "task_notes",
        serde_json::json!({"id":40,"notes":exact.notes}),
    );
    second.send(ClientMessage::ListInfo);
    loop {
        match second.until(|_| true) {
            ServerMessage::Info { .. } => break,
            ServerMessage::Tasks { .. } => panic!("no-op emitted a Tasks update"),
            _ => {}
        }
    }
    first.send(ClientMessage::KillServer);
    server.join().unwrap();
}

#[test]
#[ignore = "release-only latency budget, run explicitly in CI"]
#[allow(
    clippy::assertions_on_constants,
    reason = "reject accidental debug budget runs at runtime"
)]
fn task_history_pty_latency_budget() {
    assert!(!cfg!(debug_assertions), "run this budget in release mode");
    let (socket, server, _) = history_fixture(128);
    let mut terminal = Peer::connect(&socket);
    let mut observer = Peer::connect(&socket);
    observer.send(ClientMessage::Tasks);
    observer.until(|m| matches!(m, ServerMessage::Tasks { .. }));
    terminal.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 120,
        rows: 40,
    });
    terminal.send(ClientMessage::Input(
        b"stty -echo; printf 'READY_FOR_LATENCY\\n'\r".to_vec(),
    ));
    terminal.until(|m| matches!(m, ServerMessage::RenderOps(bytes) if String::from_utf8_lossy(bytes).contains("READY_FOR_LATENCY")));
    // A second attached renderer increases realistic fan-out; its socket is drained.
    let mut second = Peer::connect(&socket);
    second.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 120,
        rows: 40,
    });
    second.send(ClientMessage::Tasks);
    second.until(|m| matches!(m, ServerMessage::Tasks { .. }));
    let mut samples = Vec::new();
    for sample in 0..105 {
        let marker = format!("LATENCY_{sample:03}_VISIBLE");
        let start = Instant::now();
        terminal.send(ClientMessage::Input(
            format!("printf '{marker}\\n'\r").into_bytes(),
        ));
        terminal.until(|m| matches!(m, ServerMessage::RenderOps(bytes) if String::from_utf8_lossy(bytes).contains(&marker)));
        if sample >= 5 {
            samples.push(start.elapsed());
        }
        second.send(ClientMessage::ListInfo);
        second.until(|m| matches!(m, ServerMessage::Info { .. }));
    }
    samples.sort_unstable();
    let p95 = samples[94];
    eprintln!("Tasks latency: 128 x 256 KiB prompts, two attached renderers, two subscriptions, 100 samples: p95={p95:?}");
    assert!(
        p95 < Duration::from_millis(50),
        "input-to-visible-output exceeds 50 ms p95"
    );
    // Ordinary output and a quiet interval must never refresh Tasks.
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        while let Some(message) = observer.decoder.decode::<ServerMessage>().unwrap() {
            assert!(
                !matches!(message, ServerMessage::Tasks { .. }),
                "idle task traffic"
            );
        }
        let mut bytes = [0; 32768];
        match observer.stream.read(&mut bytes) {
            Ok(0) => panic!("task observer disconnected"),
            Ok(n) => observer.decoder.push(&bytes[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("{error}"),
        }
    }
    terminal.send(ClientMessage::KillServer);
    server.join().unwrap();
}

// A child process gives the restart regression a real unclean shutdown without
// killing this test harness or ever touching a user's Workspace.
#[test]
#[ignore = "subprocess entry point for native restore regression"]
fn task_restore_child() {
    let Ok(socket) = std::env::var("UNITERM_TEST_RESTORE_SOCKET") else {
        return;
    };
    let state = std::env::var_os("XDG_STATE_HOME").unwrap();
    common::isolate_state();
    std::env::set_var("XDG_STATE_HOME", state);
    uniterm_server::run_server(std::path::Path::new(&socket), "/bin/sh", &[]).unwrap();
}

struct RestoreChild(std::process::Child);
impl Drop for RestoreChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_restore_child(socket: &std::path::Path, state: &std::path::Path) -> RestoreChild {
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "task_restore_child", "--nocapture"])
        .env("UNITERM_TEST_RESTORE_SOCKET", socket)
        .env("PATH", socket.parent().unwrap())
        .env("XDG_STATE_HOME", state)
        .env("XDG_RUNTIME_DIR", socket.parent().unwrap())
        .env("XDG_CONFIG_HOME", socket.parent().unwrap().join("config"))
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    common::wait_for_socket(socket);
    RestoreChild(child)
}

#[test]
fn native_restart_reassociates_only_the_exact_task_invocation() {
    use uniterm_core::{LayoutNode, PaneId, ProjectId, TaskStatus};
    use uniterm_server::eventlog::{append_line, EventLog, LogEvent};
    use uniterm_server::persist::{
        self, AgentLaunchSnap, PaneSnap, ProjectSnap, Snapshot, WinSnap,
    };
    let state = common::isolate_state();
    let root = common::socket_root().join(format!("tr-{}", common::unique_nonce()));
    std::fs::create_dir_all(&root).unwrap();
    let name = common::unique_workspace_name();
    let socket = root.join(format!("{name}.sock"));
    let executable = root.join("codex");
    std::fs::write(
        &executable,
        "#!/bin/sh\nwhile IFS= read -r line; do :; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let resume = vec!["codex".into(), "resume".into(), "test-session".into()];
    let mut log = EventLog::open(&name);
    let mut record = |event| {
        let (name, line) = log.record(event).unwrap();
        append_line(&name, &line).unwrap();
        log.current_sequence()
    };
    let original = record(LogEvent::AgentBound {
        pane: 1,
        agent: "codex".into(),
        invocation: None,
    });
    record(LogEvent::AgentSessionObserved {
        pane: 1,
        provider: "codex".into(),
        session_id: Some("test-session".into()),
        resume_command: resume.clone(),
        transcript_path: None,
    });
    for (id, invocation) in [(1, original), (2, original + 999)] {
        record(LogEvent::TaskCreated {
            id,
            title: format!("attempt {id}"),
            status: TaskStatus::Doing,
        });
        record(LogEvent::TaskDetailsUpdated {
            id,
            notes: String::new(),
            context: uniterm_core::tasks::TaskContext {
                pane: Some(PaneId(1)),
                project: Some(ProjectId(1)),
                invocation: Some(invocation),
                provider: Some("codex".into()),
                ..Default::default()
            },
        });
    }
    let snapshot = Snapshot::new_with_sequence(
        0,
        2,
        ProjectId(1),
        2,
        vec![ProjectSnap {
            id: ProjectId(1),
            name: "Default".into(),
            root: root.to_string_lossy().into_owned(),
            active_pane: Some(PaneId(1)),
            metadata: Vec::new(),
        }],
        vec![WinSnap {
            stack_expanded: false,
            stack_parent: None,
            layout_mode: Default::default(),
            project: ProjectId(1),
            layout: LayoutNode::Leaf(PaneId(1)),
            active: PaneId(1),
            zoomed: None,
            name: None,
            panes: vec![PaneSnap {
                id: PaneId(1),
                cwd: Some(root.to_string_lossy().into_owned()),
                content: Vec::new(),
                metadata: Vec::new(),
                launch_args: Vec::new(),
                agent_launch: Some(AgentLaunchSnap {
                    provider: "codex".into(),
                    session_id: Some("test-session".into()),
                    resume_command: resume,
                }),
            }],
        }],
        log.current_sequence(),
    );
    persist::save(&name, &snapshot).unwrap();
    let mut previous = original;
    for restart in 0..2 {
        if restart > 0 {
            let _ = std::fs::remove_file(&socket);
        }
        let child = start_restore_child(&socket, &state);
        let mut peer = Peer::connect(&socket);
        // Socket readiness precedes the blocking native-resume validation.
        // Wait for the new invocation instead of racing the saved projection.
        let deadline = Instant::now() + Duration::from_secs(5);
        let task = loop {
            let task = peer.detail(1, false);
            if task
                .context
                .invocation
                .is_some_and(|invocation| invocation != previous)
            {
                break task;
            }
            assert!(
                Instant::now() < deadline,
                "native resume did not rebind the task"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let invocation = task.context.invocation.unwrap();
        assert_eq!(task.agents.len(), 1, "native restart lost task ownership");
        let unrelated = peer.detail(2, false);
        assert!(
            unrelated.agents.is_empty(),
            "Pane reuse captured an unrelated attempt"
        );
        assert_eq!(unrelated.context.invocation, Some(original + 999));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut tasks = uniterm_core::TaskList::new();
            uniterm_server::eventlog::replay_tasks(&name, &mut tasks).unwrap();
            if tasks
                .get(1)
                .is_some_and(|task| task.context.invocation == Some(invocation))
            {
                break;
            }
            assert!(Instant::now() < deadline, "reassociation was not durable");
            std::thread::sleep(Duration::from_millis(10));
        }
        previous = invocation;
        if restart == 0 {
            drop(child); // SIGKILL preserves crash marker and exercises real restart.
        } else {
            peer.send(ClientMessage::KillServer);
            let mut child = child;
            assert!(child.0.wait().unwrap().success());
        }
    }
}
