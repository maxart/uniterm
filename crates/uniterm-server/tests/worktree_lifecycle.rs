//! End-to-end worktree resource lifecycle over binary and NDJSON control paths.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{
    encode_frame, ClientMessage, ControlCommand, ControlFrame, ControlRequest, ControlResult,
    FrameDecoder, ServerMessage, WorktreeOperation, WorktreeResult, CONTROL_API_VERSION,
};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_root() -> PathBuf {
    let nonce = common::unique_nonce();
    common::socket_root().join(format!("ut-wt-{}-{nonce}", std::process::id()))
}

fn git(repository: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .args(["-C"])
        .arg(repository)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository(root: &Path) -> PathBuf {
    let repository = root.join("repo");
    std::fs::create_dir_all(&repository).unwrap();
    git(&repository, &["init", "-q"]);
    git(
        &repository,
        &["config", "user.email", "uniterm@example.invalid"],
    );
    git(&repository, &["config", "user.name", "Uniterm Test"]);
    std::fs::write(repository.join("README"), "seed\n").unwrap();
    git(&repository, &["add", "README"]);
    git(&repository, &["commit", "-qm", "seed"]);
    repository
}

fn delayed_git(root: &Path) -> (String, String) {
    let original = std::env::var("PATH").unwrap_or_default();
    let real = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    assert!(real.status.success());
    let real = String::from_utf8(real.stdout).unwrap();
    let real = real.trim();
    assert!(!real.contains('\''));
    let directory = root.join("bin");
    std::fs::create_dir_all(&directory).unwrap();
    let script = directory.join("git");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$3\" = worktree ] && [ \"$4\" = list ]; then sleep 1; fi\nexec '{real}' \"$@\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (format!("{}:{original}", directory.display()), original)
}

fn wait_for(path: &Path) {
    for _ in 0..300 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("socket never appeared at {}", path.display());
}

struct Wire {
    stream: UnixStream,
    decoder: FrameDecoder,
}

impl Wire {
    fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        Self {
            stream,
            decoder: FrameDecoder::new(),
        }
    }

    fn send(&mut self, message: ClientMessage) {
        self.stream.write_all(&encode_frame(&message)).unwrap();
    }

    fn next(&mut self, wanted: impl Fn(&ServerMessage) -> bool) -> ServerMessage {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut bytes = [0u8; 16 * 1024];
        loop {
            while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                if wanted(&message) {
                    return message;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for server response"
            );
            match self.stream.read(&mut bytes) {
                Ok(0) => panic!("worktree connection closed"),
                Ok(read) => self.decoder.push(&bytes[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("worktree response failed: {error}"),
            }
        }
    }

    fn worktree(&mut self, operation: WorktreeOperation) -> WorktreeResult {
        self.send(ClientMessage::Worktree { operation });
        match self.next(|message| matches!(message, ServerMessage::Worktrees(_))) {
            ServerMessage::Worktrees(result) => result,
            _ => unreachable!(),
        }
    }
}

fn control(
    writer: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    workspace: &str,
    id: u64,
    command: ControlCommand,
) -> WorktreeResult {
    serde_json::to_writer(
        &mut *writer,
        &ControlRequest {
            version: CONTROL_API_VERSION,
            id,
            workspace: workspace.into(),
            command,
        },
    )
    .unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    match serde_json::from_str::<ControlFrame>(&line).unwrap() {
        ControlFrame::Response(response) => match response.result.unwrap() {
            ControlResult::Worktrees(result) => result,
            result => panic!("unexpected control result: {result:?}"),
        },
        frame => panic!("unexpected control frame: {frame:?}"),
    }
}

#[test]
fn create_open_list_dirty_remove_and_stale_cleanup_share_git_authority() {
    isolate_state();
    let root = temp_root();
    std::fs::create_dir_all(&root).unwrap();
    let repository = repository(&root);
    let (path, original_path) = delayed_git(&root);
    std::env::set_var("PATH", path);
    let workspace = unique_workspace_name();
    let socket = root.join(format!("{workspace}.sock"));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 30"], 80, 24).unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&socket);
    let control_path = socket.with_extension("control.sock");
    wait_for(&control_path);

    let mut wire = Wire::connect(&socket);
    let mut control_writer = UnixStream::connect(&control_path).unwrap();
    control_writer
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut control_reader = BufReader::new(control_writer.try_clone().unwrap());
    let review = root.join("review");
    wire.send(ClientMessage::Worktree {
        operation: WorktreeOperation::Add {
            name: "Review".into(),
            repository: repository.to_string_lossy().into_owned(),
            path: review.to_string_lossy().into_owned(),
            base: None,
        },
    });
    let started = Instant::now();
    serde_json::to_writer(
        &mut control_writer,
        &ControlRequest {
            version: CONTROL_API_VERSION,
            id: 99,
            workspace: workspace.clone(),
            command: ControlCommand::Capabilities,
        },
    )
    .unwrap();
    control_writer.write_all(b"\n").unwrap();
    control_writer.flush().unwrap();
    let mut line = String::new();
    control_reader.read_line(&mut line).unwrap();
    assert!(matches!(
        serde_json::from_str::<ControlFrame>(&line).unwrap(),
        ControlFrame::Response(response)
            if matches!(response.result, Some(ControlResult::Capabilities { .. }))
    ));
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "Git worktree I/O blocked the control dispatcher"
    );
    let added = match wire.next(|message| matches!(message, ServerMessage::Worktrees(_))) {
        ServerMessage::Worktrees(result) => result,
        _ => unreachable!(),
    };
    assert!(added.accepted, "{:?}", added.error);
    let project = added.items[0].registration.project;
    assert!(review.is_dir());

    wire.send(ClientMessage::ProjectSwitch {
        project: uniterm_core::ProjectId(1),
    });
    let _ = wire.next(|message| matches!(message, ServerMessage::Workspace { .. }));
    let opened = wire.worktree(WorktreeOperation::Open { project });
    assert!(opened.accepted, "{:?}", opened.error);
    wire.send(ClientMessage::WorkspaceState);
    match wire.next(|message| matches!(message, ServerMessage::Workspace { .. })) {
        ServerMessage::Workspace {
            active_project,
            projects,
            ..
        } => {
            assert_eq!(active_project, project);
            assert_eq!(
                projects
                    .iter()
                    .find(|item| item.id == project)
                    .unwrap()
                    .worktree,
                Some(added.items[0].registration.clone())
            );
        }
        _ => unreachable!(),
    }

    let listed = control(
        &mut control_writer,
        &mut control_reader,
        &workspace,
        1,
        ControlCommand::WorktreeList,
    );
    assert!(listed.accepted, "{:?}", listed.error);
    assert_eq!(listed.items.len(), 1);

    std::fs::write(review.join("dirty.txt"), "keep\n").unwrap();
    let refused = wire.worktree(WorktreeOperation::Remove {
        project,
        force: false,
    });
    assert!(!refused.accepted);
    assert!(review.is_dir());

    let removed = control(
        &mut control_writer,
        &mut control_reader,
        &workspace,
        2,
        ControlCommand::WorktreeRemove {
            project,
            force: true,
        },
    );
    assert!(removed.accepted, "{:?}", removed.error);
    assert!(!review.exists());

    let stale = root.join("stale");
    let added = wire.worktree(WorktreeOperation::Add {
        name: "Stale".into(),
        repository: repository.to_string_lossy().into_owned(),
        path: stale.to_string_lossy().into_owned(),
        base: None,
    });
    assert!(added.accepted, "{:?}", added.error);
    let stale_project = added.items[0].registration.project;
    git(
        &repository,
        &["worktree", "remove", "--force", stale.to_str().unwrap()],
    );
    let cleaned = wire.worktree(WorktreeOperation::Cleanup {
        project: stale_project,
    });
    assert!(cleaned.accepted, "{:?}", cleaned.error);

    wire.send(ClientMessage::WorkspaceState);
    match wire.next(|message| matches!(message, ServerMessage::Workspace { .. })) {
        ServerMessage::Workspace { projects, .. } => assert_eq!(projects.len(), 1),
        _ => unreachable!(),
    }
    wire.send(ClientMessage::KillServer);
    drop(wire);
    server.join().unwrap();
    std::env::set_var("PATH", original_path);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn agent_sidebar_names_the_worktree_branch_only_for_worktree_projects() {
    isolate_state();
    let root = temp_root();
    std::fs::create_dir_all(&root).unwrap();
    let repository = repository(&root);
    let workspace = unique_workspace_name();
    let socket = root.join(format!("{workspace}.sock"));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 120, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&socket);
    let mut wire = Wire::connect(&socket);
    wire.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 120,
        rows: 30,
    });

    // An agent in the primary checkout carries no worktree marker. Wait for
    // the sidebar row ("Codex"), which sits at a fixed position; the echoed
    // printf command wraps at a prompt-dependent column, so its lowercase
    // "codex" can be split across two rows and never match.
    wire.send(ClientMessage::Input(announce_agent("codex")));
    let plain = render_containing(&mut wire, "Codex");
    assert!(
        !plain.contains('\u{2387}'),
        "primary checkout must not be marked as a worktree"
    );

    let review = root.join("review");
    let added = wire.worktree(WorktreeOperation::Add {
        name: "Review".into(),
        repository: repository.to_string_lossy().into_owned(),
        path: review.to_string_lossy().into_owned(),
        base: None,
    });
    assert!(added.accepted, "{:?}", added.error);
    let registration = added.items[0].registration.clone();
    let opened = wire.worktree(WorktreeOperation::Open {
        project: registration.project,
    });
    assert!(opened.accepted, "{:?}", opened.error);

    // The fresh Pane in the worktree Project runs an agent: its status row
    // names the branch it is working in.
    wire.send(ClientMessage::Input(announce_agent("claude")));
    let marker = format!("\u{2387} {}", registration.branch);
    let frame = render_containing(&mut wire, &marker);
    assert!(frame.contains(&marker));

    wire.send(ClientMessage::KillServer);
    server.join().unwrap();
}

fn announce_agent(agent: &str) -> Vec<u8> {
    format!(
        "printf '\\033]777;notify;uniterm://cli-agent;{{\"agent\":\"{agent}\",\"event\":\"session_start\"}}\\007'\n"
    )
    .into_bytes()
}

fn render_containing(wire: &mut Wire, expected: &str) -> String {
    match wire.next(|message| {
        matches!(message, ServerMessage::RenderOps(ops) if String::from_utf8_lossy(ops).contains(expected))
    }) {
        ServerMessage::RenderOps(ops) => String::from_utf8_lossy(&ops).into_owned(),
        _ => unreachable!(),
    }
}
