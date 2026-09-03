//! Worktree creation and child orchestration launch share one semantic result.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime};

use uniterm_core::{RunId, RunStatus};
use uniterm_proto::{
    encode_frame, ClientMessage, ControlCommand, ControlFrame, ControlRequest, ControlResult,
    OrchestrationKind, OrchestrationLaunch, RunForkRequest, CONTROL_API_VERSION,
};

fn temp_root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("ut-child-run-{}-{nonce}", std::process::id()))
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

fn wait_for(path: &Path) {
    for _ in 0..300 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("socket never appeared at {}", path.display());
}

fn request(
    writer: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    workspace: &str,
    id: u64,
    command: ControlCommand,
) -> ControlResult {
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
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "control connection closed");
        if let ControlFrame::Response(response) = serde_json::from_str(&line).unwrap() {
            if response.id == id {
                assert!(response.error.is_none(), "{:?}", response.error);
                return response.result.unwrap();
            }
        }
    }
}

#[test]
fn active_run_forks_into_fresh_worktree_owned_identities() {
    let root = temp_root();
    let repository = root.join("repository");
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

    let provider = root.join("test-provider");
    std::fs::write(&provider, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("XDG_STATE_HOME", &root);
    let original_directory = std::env::current_dir().unwrap();
    std::env::set_current_dir(&repository).unwrap();

    let workspace = "child-run";
    let socket = root.join(format!("{workspace}.sock"));
    let control = socket.with_extension("control.sock");
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let _ = uniterm_server::run_server(&server_socket, "/bin/sh", &["-c", "sleep 30"]);
    });
    wait_for(&control);

    let mut writer = UnixStream::connect(&control).unwrap();
    writer
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(writer.try_clone().unwrap());
    let parent = match request(
        &mut writer,
        &mut reader,
        workspace,
        1,
        ControlCommand::OrchestrationStart {
            launch: OrchestrationLaunch {
                kind: OrchestrationKind::Workflow,
                template: Some("pair".into()),
                goal: "implement the feature".into(),
                provider: Some(provider.to_string_lossy().into_owned()),
                role_providers: Vec::new(),
                project: None,
            },
        },
    ) {
        ControlResult::OrchestrationStarted { run } => run,
        result => panic!("unexpected launch result: {result:?}"),
    };

    let child_path = root.join("alternative");
    let forked = match request(
        &mut writer,
        &mut reader,
        workspace,
        2,
        ControlCommand::RunFork {
            fork: RunForkRequest {
                parent,
                name: "Alternative".into(),
                path: child_path.to_string_lossy().into_owned(),
                base: None,
            },
        },
    ) {
        ControlResult::RunForked(result) => result,
        result => panic!("unexpected fork result: {result:?}"),
    };
    assert!(forked.worktree.accepted, "{:?}", forked.worktree.error);
    let child = forked.child.expect("accepted fork has a child Run");
    assert_ne!(child, parent);
    assert!(child_path.is_dir());

    let runs = match request(
        &mut writer,
        &mut reader,
        workspace,
        3,
        ControlCommand::RunList {
            project: None,
            active_only: true,
        },
    ) {
        ControlResult::Runs { runs, .. } => runs,
        result => panic!("unexpected run list result: {result:?}"),
    };
    let parent_record = runs.iter().find(|run| run.id == parent).unwrap();
    let child_record = runs.iter().find(|run| run.id == child).unwrap();
    assert_eq!(parent_record.status, RunStatus::Active);
    assert_eq!(child_record.status, RunStatus::Active);
    assert_eq!(child_record.parent, Some(parent));
    assert_eq!(parent_record.children, vec![child]);
    assert_ne!(child_record.project, parent_record.project);
    assert_ne!(child_record.task_id, parent_record.task_id);
    assert!(parent_record
        .panes
        .iter()
        .all(|pane| !child_record.panes.contains(pane)));
    assert!(parent_record
        .roles
        .iter()
        .all(|role| child_record.roles.iter().all(|child| child.id != role.id)));

    let mut binary = UnixStream::connect(&socket).unwrap();
    let _ = binary.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = binary.flush();
    let _ = server.join();
    std::env::set_current_dir(original_directory).unwrap();
    let _ = std::fs::remove_dir_all(&root);

    assert_ne!(child, RunId(0));
}
