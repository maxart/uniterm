//! Accepted direct prompts share durable Tasks and Overwatch's scoped management API.
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};
use uniterm_core::{PaneId, TaskStatus};
use uniterm_proto::{ControlCommand, ControlResult};
use uniterm_server::Server;
mod common;
#[path = "common/control.rs"]
mod control;

fn request(socket: &Path, command: ControlCommand) -> ControlResult {
    // The main socket appears before the runtime binds its control endpoints.
    // Wait for the requested endpoint to accept connections, including manager.
    let frame = control::Control::connect(socket).request_raw(command);
    let uniterm_proto::ControlFrame::Response(response) = serde_json::from_value(frame).unwrap()
    else {
        panic!("unexpected control frame");
    };
    assert!(response.error.is_none(), "{:?}", response.error);
    response.result.unwrap()
}

fn tasks(socket: &Path) -> Vec<uniterm_proto::TaskEntry> {
    match request(socket, ControlCommand::TaskList) {
        ControlResult::Tasks { items, .. } => items,
        other => panic!("unexpected {other:?}"),
    }
}

fn wait_for(
    socket: &Path,
    condition: impl Fn(&[uniterm_proto::TaskEntry]) -> bool,
) -> Vec<uniterm_proto::TaskEntry> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let items = tasks(socket);
        if condition(&items) {
            return items;
        }
        assert!(Instant::now() < deadline, "unexpected Tasks: {items:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn direct_prompt_tasks_are_manageable_visible_and_replay_after_shutdown() {
    common::isolate_state();
    let name = common::unique_workspace_name();
    let dir = common::socket_root().join(&name);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{name}.sock"));
    let child_socket = socket.clone();
    let worker = std::thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(
            &child_socket,
            "/bin/sh",
            &["-c", "stty raw -echo; printf READY; exec cat"],
            100,
            30,
        )
        .unwrap();
        server.set_config(uniterm_core::Config {
            auto_tab_names: true,
            auto_install_connectors: false,
            auto_update_connectors: false,
            ..Default::default()
        });
        server.run(&mut poll).unwrap();
    });
    common::wait_for_socket(&socket);
    let ControlResult::Panes { panes, .. } = request(&socket, ControlCommand::PaneList) else {
        panic!("pane list");
    };
    let pane = panes[0].id;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let ControlResult::PaneOutput { text, .. } =
            request(&socket, ControlCommand::PaneRead { pane, lines: 10 })
        {
            if text.contains("READY") {
                break;
            }
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let hook = |value: serde_json::Value| {
        let text = format!("\x1b]777;notify;uniterm://cli-agent;{value}\x07");
        request(&socket, ControlCommand::PaneSend { pane, text });
    };
    hook(
        serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"direct-session","turn_id":"request-one","prompt":"PRIVATE-SENTINEL-123 repair login\nKeep the keyboard shortcut.","summary":"PRIVATE-SENTINEL-123 repair login"}),
    );
    let items = wait_for(&socket, |items| items.len() == 1);
    let id = items[0].id;
    assert!(items[0].title.contains("PRIVATE-SENTINEL-123"));
    assert_eq!(items[0].context.pane, Some(pane));
    assert!(items[0]
        .context
        .prompt
        .as_deref()
        .unwrap()
        .contains("keyboard shortcut"));
    hook(serde_json::json!({"agent":"codex","event":"idle","session_id":"direct-session"}));
    assert_eq!(tasks(&socket)[0].status, TaskStatus::Doing);
    hook(
        serde_json::json!({"agent":"codex","event":"turn_complete","session_id":"direct-session","turn_id":"request-one"}),
    );
    wait_for(&socket, |items| items[0].status == TaskStatus::Done);
    let context = request(&socket, ControlCommand::OverwatchContext);
    let context = serde_json::to_string(&context).unwrap();
    assert!(context.contains(&format!("Task #{id}")));
    assert!(!context.contains("PRIVATE-SENTINEL-123"));
    assert!(!context.contains("last_prompt"));
    let manager = socket
        .parent()
        .unwrap()
        .join("manager")
        .join(socket.file_name().unwrap());
    let full_fleet = request(&socket, ControlCommand::AgentList);
    assert!(serde_json::to_string(&full_fleet)
        .unwrap()
        .contains("PRIVATE-SENTINEL-123"));
    let metadata_fleet = request(&manager, ControlCommand::AgentList);
    assert!(!serde_json::to_string(&metadata_fleet)
        .unwrap()
        .contains("PRIVATE-SENTINEL-123"));
    let mut offset = 0;
    let mut expected_sequence = None;
    loop {
        let context = request(
            &manager,
            ControlCommand::OverwatchContextPage {
                offset,
                limit: 1,
                expected_sequence,
            },
        );
        assert!(!serde_json::to_string(&context)
            .unwrap()
            .contains("PRIVATE-SENTINEL-123"));
        let ControlResult::OverwatchContext {
            sequence,
            next_offset,
            private_content_excluded,
            ..
        } = context
        else {
            panic!("context page");
        };
        assert!(private_content_excluded);
        expected_sequence = Some(sequence);
        let Some(next) = next_offset else {
            break;
        };
        assert!(next > offset);
        offset = next;
    }
    // Authorized detail keeps the original title and prompt, even though the
    // metadata projection uses a neutral label for this same durable Task.
    let ControlResult::TaskDetail { chunk } = request(
        &socket,
        ControlCommand::TaskDetail {
            id,
            offset: 0,
            revision: None,
        },
    ) else {
        panic!("Task detail");
    };
    assert_eq!(chunk.bytes.len(), chunk.total);
    let detail: uniterm_proto::TaskEntry = serde_json::from_slice(&chunk.bytes).unwrap();
    assert!(detail.title.contains("PRIVATE-SENTINEL-123"));
    assert!(detail
        .context
        .prompt
        .as_deref()
        .unwrap()
        .contains("PRIVATE-SENTINEL-123"));
    // Build the authorized index before the restricted query. This exercises
    // previously materialized rows, not merely newly redacted event ingestion.
    let timeline = |endpoint: &Path, before| {
        request(
            endpoint,
            ControlCommand::TimelineQuery {
                date: "today".into(),
                filter: String::new(),
                before,
            },
        )
    };
    let indexed = timeline(&socket, None);
    assert!(serde_json::to_string(&indexed)
        .unwrap()
        .contains("PRIVATE-SENTINEL-123"));
    // Push the still-open invocation off the first page so its private work
    // label also reaches the active-agent carryover boundary.
    for index in 0..135 {
        request(
            &socket,
            ControlCommand::TimelineNote {
                project: uniterm_core::ProjectId(1),
                pane: None,
                text: format!("paging breadcrumb {index}"),
            },
        );
    }
    let ControlResult::Timeline { day } = timeline(&socket, None) else {
        panic!("timeline");
    };
    assert!(day.active_agents.iter().any(|entry| entry
        .work_summary
        .as_deref()
        .is_some_and(|text| text.contains("PRIVATE-SENTINEL-123"))));
    let mut before = None;
    let mut saw_carryover = false;
    let mut pages = 0;
    loop {
        let result = timeline(&manager, before);
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("PRIVATE-SENTINEL-123"));
        let ControlResult::Timeline { day } = result else {
            panic!("metadata timeline");
        };
        assert_eq!(day.entries.len(), day.times.len());
        assert!(day
            .entries
            .iter()
            .chain(&day.active_agents)
            .all(|entry| entry.work_summary.is_none()));
        saw_carryover |= !day.active_agents.is_empty();
        pages += 1;
        let Some(next) = day.next_before else {
            break;
        };
        assert!(before.is_none_or(|previous| next < previous));
        before = Some(next);
    }
    assert!(saw_carryover);
    assert!(pages >= 2);
    // Overwatch with a management grant uses these exact existing operations.
    request(
        &socket,
        ControlCommand::TaskRetitle {
            id,
            title: "Login follow-up".into(),
        },
    );
    request(
        &socket,
        ControlCommand::TaskSetStatus {
            id,
            status: TaskStatus::Todo,
        },
    );
    hook(serde_json::json!({"agent":"codex","event":"tool_start","session_id":"direct-session"}));
    let items = tasks(&socket);
    assert_eq!(items[0].status, TaskStatus::Todo);
    assert_eq!(items[0].title, "Login follow-up");
    UnixStream::connect(&socket)
        .unwrap()
        .write_all(&uniterm_proto::encode_frame(
            &uniterm_proto::ClientMessage::KillServer,
        ))
        .unwrap();
    worker.join().unwrap();
    let mut replayed = uniterm_core::TaskList::new();
    uniterm_server::eventlog::replay_tasks(&name, &mut replayed).unwrap();
    assert_eq!(replayed.len(), 1);
    let task = replayed.get(id).unwrap();
    assert_eq!(task.title, "Login follow-up");
    assert_eq!(task.status, TaskStatus::Todo);
    assert_eq!(task.context.pane, Some(PaneId(pane.0)));
    assert!(task.context.observed.as_ref().unwrap().manual_status);
}
