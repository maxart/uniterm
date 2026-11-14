//! Real PTY regression coverage for opt-in permission automation.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use uniterm_proto::{
    encode_frame, ClientMessage, ControlCommand, ControlFrame, ControlRequest, ControlResponse,
    ControlResult, FrameDecoder, ServerMessage, WaitingAction, CONTROL_API_VERSION,
};

mod common;

struct Workspace {
    socket: PathBuf,
    thread: Option<thread::JoinHandle<()>>,
}

impl Workspace {
    fn start(socket: PathBuf, script: String) -> Self {
        let server_socket = socket.clone();
        let thread = thread::spawn(move || {
            let (mut server, mut poll) = uniterm_server::Server::bind(
                &server_socket,
                "/bin/bash",
                &["-c", &script],
                200,
                60,
            )
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
        self.request_in(self.socket.file_stem().unwrap().to_str().unwrap(), command)
    }

    fn request_in(&self, workspace: &str, command: ControlCommand) -> ControlResponse {
        let mut stream = UnixStream::connect(self.socket.with_extension("control.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        serde_json::to_writer(
            &mut stream,
            &ControlRequest {
                version: CONTROL_API_VERSION,
                id: 1,
                workspace: workspace.into(),
                command,
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        match serde_json::from_str(&line).unwrap() {
            ControlFrame::Response(response) => response,
            other => panic!("unexpected {other:?}"),
        }
    }

    fn waiting(&self) -> Vec<uniterm_proto::WaitingEntry> {
        match self.request(ControlCommand::WaitingList).result.unwrap() {
            ControlResult::Waiting { items } => items,
            other => panic!("unexpected {other:?}"),
        }
    }

    fn mode(&self, enabled: Option<bool>) -> bool {
        match self
            .request(ControlCommand::AutoApprove {
                enabled,
                confirmed: false,
            })
            .result
            .unwrap()
        {
            ControlResult::AutoApproval { enabled, .. } => enabled,
            other => panic!("unexpected {other:?}"),
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if let Ok(mut stream) = UnixStream::connect(&self.socket) {
            let _ = stream.write_all(&encode_frame(&ClientMessage::KillServer));
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !predicate() {
        assert!(Instant::now() < deadline, "condition did not become true");
        thread::sleep(Duration::from_millis(20));
    }
}

fn exists(root: &Path, name: &str) -> bool {
    root.join(name).exists()
}

#[test]
fn opt_in_scopes_deduplicates_drains_and_resets_on_restart() {
    common::isolate_state();
    let root = common::temp_dir("approval");
    std::env::set_var("XDG_RUNTIME_DIR", root.join("runtime"));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    let sockets = common::socket_root().join(format!("uap-{}", std::process::id()));
    std::fs::create_dir_all(&sockets).unwrap();
    let socket = sockets.join(format!("{}.sock", common::unique_workspace_name()));
    let script = format!(
        r#"
stty -echo
prompt() {{
  printf '\033[2J\033[H\033]777;notify;uniterm://cli-agent;{{"v":1,"event":"permission_request","agent":"codex"}}\007'
  printf 'Would you like to run the following command?\r\n echo %s\r\n› 1. Yes, proceed (y)\r\n  2. No, and tell Codex what to do differently (esc)\r\nPress enter to confirm' "$1"
}}
prompt first
IFS= read -r answer
touch '{root}/first'
prompt first
if IFS= read -r -t 1 duplicate; then touch '{root}/duplicate'; fi
prompt second
IFS= read -r answer
touch '{root}/second'
printf '\033[2J\033[H\033]777;notify;uniterm://cli-agent;{{"v":1,"event":"question","agent":"codex"}}\007Choose a database'
IFS= read -r answer
touch '{root}/question-answered'
prompt third
IFS= read -r answer
touch '{root}/third'
printf '\033[2J\033[H\033]777;notify;uniterm://cli-agent;{{"v":1,"event":"idle","agent":"codex"}}\007Done'
read -r answer
"#,
        root = root.display()
    );
    let workspace = Workspace::start(socket.clone(), script);
    wait_until(|| workspace.waiting().len() == 1);
    assert!(!workspace.mode(None));
    assert!(!exists(&root, "first"));
    let wrong = workspace.request_in(
        "another-workspace",
        ControlCommand::AutoApprove {
            enabled: Some(true),
            confirmed: false,
        },
    );
    assert_eq!(wrong.error.unwrap().code, "workspace_mismatch");
    assert!(!workspace.mode(None));
    assert!(workspace.mode(Some(true)));
    wait_until(|| exists(&root, "second"));
    assert!(
        !exists(&root, "duplicate"),
        "repaint was mistaken for a new prompt"
    );
    wait_until(|| {
        workspace
            .waiting()
            .first()
            .is_some_and(|item| item.kind == uniterm_core::WaitingKind::Question)
    });
    let question = workspace.waiting().remove(0);
    let result = workspace.request(ControlCommand::WaitingAct {
        id: question.id,
        action: WaitingAction::Approve,
        text: String::new(),
    });
    assert!(matches!(
        result.result,
        Some(ControlResult::Mutation {
            accepted: false,
            ..
        })
    ));
    assert!(!exists(&root, "question-answered"));

    // A newly attached client sees the mode already enabled.
    let mut attached = UnixStream::connect(&socket).unwrap();
    attached
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    attached
        .write_all(&encode_frame(&ClientMessage::Agents))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let mut buf = [0; 16384];
    loop {
        let count = attached.read(&mut buf).unwrap();
        assert!(count > 0);
        decoder.push(&buf[..count]);
        if let Some(ServerMessage::Agents { auto_approve, .. }) = decoder.decode().unwrap() {
            assert!(auto_approve);
            break;
        }
    }
    assert!(!workspace.mode(Some(false)));
    workspace.request(ControlCommand::WaitingAct {
        id: question.id,
        action: WaitingAction::Answer,
        text: "SQLite".into(),
    });
    wait_until(|| {
        workspace
            .waiting()
            .first()
            .is_some_and(|item| item.kind == uniterm_core::WaitingKind::Permission)
    });
    assert!(!exists(&root, "third"));
    let permission = workspace.waiting().remove(0);
    let result = workspace.request(ControlCommand::WaitingAct {
        id: permission.id,
        action: WaitingAction::Approve,
        text: String::new(),
    });
    assert!(matches!(
        result.result,
        Some(ControlResult::Mutation { accepted: true, .. })
    ));
    wait_until(|| exists(&root, "third"));
    assert!(workspace.mode(Some(true)));
    drop(attached);
    drop(workspace);
    let restarted = Workspace::start(
        socket.clone(),
        format!(
            r#"
stty -echo
printf '\033[2J\033[H\033]777;notify;uniterm://cli-agent;{{"v":1,"event":"permission_request","agent":"codex"}}\007'
printf 'Would you like to run the following command?\r\n echo stale\r\n› 1. Yes, proceed (y)\r\n  2. No, and tell Codex what to do differently (esc)'
read -r gate
set -m
sleep 2
if IFS= read -r -t 1 unexpected; then touch '{root}/stale-input'; fi
touch '{root}/owner-returned'
read -r gate
"#,
            root = root.display()
        ),
    );
    assert!(!restarted.mode(None));
    wait_until(|| restarted.waiting().len() == 1);
    let old = restarted.waiting().remove(0);
    let pid = match restarted
        .request(ControlCommand::AgentHookContext { pane: old.pane })
        .result
        .unwrap()
    {
        ControlResult::AgentHookContext { foreground_pid, .. } => foreground_pid,
        other => panic!("unexpected {other:?}"),
    };
    assert!(pid.is_some());
    restarted.request(ControlCommand::PaneSend {
        pane: old.pane,
        text: "advance\r".into(),
    });
    wait_until(|| {
        matches!(restarted.request(ControlCommand::AgentHookContext { pane: old.pane }).result,
        Some(ControlResult::AgentHookContext { foreground_pid: Some(current), .. }) if Some(current) != pid)
    });
    assert!(restarted.mode(Some(true)));
    let refused = restarted.request(ControlCommand::WaitingAct {
        id: old.id,
        action: WaitingAction::Approve,
        text: String::new(),
    });
    assert!(matches!(
        refused.result,
        Some(ControlResult::Mutation {
            accepted: false,
            ..
        })
    ));
    wait_until(|| exists(&root, "owner-returned"));
    assert!(!exists(&root, "stale-input"));
    drop(restarted);
    // Audit records precede input and report acceptance without recording the
    // requested command a second time. Startup revokes prior authorization.
    let workspace_name = socket.file_stem().unwrap().to_str().unwrap();
    let mut events = Vec::new();
    uniterm_server::eventlog::visit_after(workspace_name, 0, |event| {
        events.push(event.event);
        Ok(())
    })
    .unwrap();
    let deliveries: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                uniterm_server::eventlog::LogEvent::PermissionApprovalDelivery {
                    accepted: true,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(deliveries.len(), 3);
    for (index, event) in events.iter().enumerate() {
        if let uniterm_server::eventlog::LogEvent::PermissionApprovalDelivery {
            pane,
            invocation,
            ..
        } = event
        {
            assert!(
                matches!(events.get(index - 1), Some(uniterm_server::eventlog::LogEvent::PermissionApprovalAttempt { pane: before, invocation: owner, .. }) if before == pane && owner == invocation)
            );
        }
    }
    fast_path_provider_matrix(&root, &sockets);
    workspace_trust_does_not_need_auto_approve(&root, &sockets);
    manual_navigation_advances_with_auto_mode_off(&root, &sockets);
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(sockets).unwrap();
}

fn workspace_trust_does_not_need_auto_approve(root: &Path, sockets: &Path) {
    let result = root.join("implicit-workspace-trust");
    let tool = root.join("implicit-workspace-tool");
    let checked = root.join("implicit-workspace-tool-checked");
    let script = format!(
        r#"
stty -echo
printf '\033]777;notify;uniterm://cli-agent;{{"v":1,"event":"prompt_submit","agent":"codex"}}\007'
printf 'Trust this workspace?\r\n%s\r\n❯ Yes, trust this workspace\r\nNo, exit\r\nEnter to confirm · Esc to cancel' "$PWD"
IFS= read -r answer
touch '{}'
printf '\033[2J\033[HWould you like to run the following command?\r\n echo no\r\n❯ 1. Yes, proceed (y)\r\n2. No, and tell Codex what to do differently (esc)\r\nPress enter to confirm'
if IFS= read -r -t 0.2 answer; then touch '{}'; fi
touch '{}'
read -r stop
"#,
        result.display(),
        tool.display(),
        checked.display()
    );
    let workspace = Workspace::start(
        sockets.join(format!("{}.sock", common::unique_workspace_name())),
        script,
    );
    assert!(!workspace.mode(None));
    let start = Instant::now();
    wait_until(|| result.exists());
    assert!(start.elapsed() < Duration::from_millis(500));
    wait_until(|| checked.exists());
    assert!(!tool.exists(), "workspace trust enabled tool approval");
    assert!(
        workspace.waiting().is_empty(),
        "workspace trust waited for the permission dwell"
    );
}

// Exercise the actual PTY -> VT -> provider -> input queue path while the
// agent is Working. Permission status dwell must not delay automatic input.
fn fast_path_provider_matrix(root: &Path, sockets: &Path) {
    let cases = [
        ("codex", include_str!("fixtures/approval/codex-trust.txt")),
        (
            "claude",
            include_str!("fixtures/approval/claude-trust.ansi"),
        ),
        (
            "opencode",
            include_str!("fixtures/approval/opencode-live.ansi"),
        ),
        ("grok", include_str!("fixtures/approval/grok-live.ansi")),
        ("agent", include_str!("fixtures/approval/grok-live.ansi")),
        ("pi", include_str!("fixtures/approval/pi-trust.txt")),
        (
            "gemini",
            include_str!("fixtures/approval/gemini-command.txt"),
        ),
    ];
    for (provider, prompt) in cases {
        let prompt_path = root.join(format!("{provider}.prompt"));
        // Text fixtures use LF; PTY output uses explicit CRLF with raw mode.
        let prompt = prompt
            .replace("/work", &std::env::current_dir().unwrap().to_string_lossy())
            .replace("\r\n", "\n")
            .replace('\n', "\r\n");
        let mut bytes = if provider == "codex" {
            // Exercise bounded-read draining with a burst ending at the
            // 256-KiB read budget, including the clear/home prefix below.
            vec![0; 256 * 1024 - 7 - prompt.len()]
        } else {
            Vec::new()
        };
        bytes.extend_from_slice(prompt.as_bytes());
        std::fs::write(&prompt_path, bytes).unwrap();
        let result = root.join(format!("{provider}.accepted"));
        let ready = root.join(format!("{provider}.ready"));
        // The same provider appears with command and startup dialogs.
        let _ = std::fs::remove_file(&result);
        let _ = std::fs::remove_file(&ready);
        let navigation = if provider == "claude" {
            r#"IFS= read -rsn3 arrow
printf '\033[2J\033[HAccessing workspace:\r\n%s\r\nNo, exit\r\n❯ Yes, I trust this folder\r\nEnter to confirm · Esc to cancel' "$PWD"
"#
        } else {
            ""
        };
        let accept = "IFS= read -r answer";
        let script = format!(
            r#"
stty -echo -onlcr
printf '\033]777;notify;uniterm://cli-agent;{{"v":1,"event":"prompt_submit","agent":"{provider}"}}\007Ready'
touch '{ready}'
read -r show
printf '\033[2J\033[H'
cat '{prompt}'
{navigation}
{accept}
touch '{result}'
# A delayed working/idle verdict must not duplicate an accepted dialog.
printf '\033]777;notify;uniterm://cli-agent;{{"v":1,"event":"idle","agent":"{provider}"}}\007'
if IFS= read -r -t 0.2 extra; then touch '{result}.duplicate'; fi
read -r stop
"#,
            ready = ready.display(),
            prompt = prompt_path.display(),
            result = result.display()
        );
        let workspace = Workspace::start(
            sockets.join(format!("{}.sock", common::unique_workspace_name())),
            script,
        );
        wait_until(|| ready.exists());
        assert!(workspace.mode(Some(true)));
        let pane = match workspace.request(ControlCommand::PaneList).result.unwrap() {
            ControlResult::Panes { panes, .. } => panes[0].id,
            other => panic!("unexpected {other:?}"),
        };
        let start = Instant::now();
        workspace.request(ControlCommand::PaneSend {
            pane,
            text: "show\r".into(),
        });
        while !result.exists() {
            assert!(
                start.elapsed() < Duration::from_millis(500),
                "{provider}: approval waited for status dwell; {:?}",
                workspace.request(ControlCommand::PaneRead { pane, lines: 60 })
            );
            thread::sleep(Duration::from_millis(1));
        }
        eprintln!("{provider} PTY approval: {:?}", start.elapsed());
        thread::sleep(Duration::from_millis(250));
        assert!(
            !result.with_extension("accepted.duplicate").exists(),
            "{provider}: delayed idle repeated approval"
        );
    }
}

fn manual_navigation_advances_with_auto_mode_off(root: &Path, sockets: &Path) {
    let result = root.join("manual-navigation");
    let script = format!(
        r#"
stty -echo
printf '\033]777;notify;uniterm://cli-agent;{{"v":1,"event":"permission_request","agent":"claude"}}\007'
printf 'Do you want to proceed?\r\n❯ 2. No, continue without running it\r\n1. Yes\r\nEnter to confirm · Esc to cancel'
IFS= read -rsn3 arrow
printf '\033[2J\033[HDo you want to proceed?\r\n2. No, continue without running it\r\n❯ 1. Yes\r\nEnter to confirm · Esc to cancel'
IFS= read -r answer
touch '{}'
read -r stop
"#,
        result.display()
    );
    let workspace = Workspace::start(
        sockets.join(format!("{}.sock", common::unique_workspace_name())),
        script,
    );
    wait_until(|| workspace.waiting().len() == 1);
    assert!(!workspace.mode(None));
    let item = workspace.waiting().remove(0);
    let response = workspace.request(ControlCommand::WaitingAct {
        id: item.id,
        action: WaitingAction::Approve,
        text: String::new(),
    });
    assert!(matches!(
        response.result,
        Some(ControlResult::Mutation { accepted: true, .. })
    ));
    wait_until(|| result.exists());
    assert!(!workspace.mode(None));
}
