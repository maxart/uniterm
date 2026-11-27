//! Human CLI contract for worktree safety and clean-stop restoration.

use std::io::{Read as _, Seek as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

mod common;

use common::unique_workspace_name;

struct WorkspaceGuard {
    root: PathBuf,
    workspace: String,
    socket: PathBuf,
    state_lock: PathBuf,
}

impl WorkspaceGuard {
    fn owner_pid(&self) -> std::io::Result<Option<i32>> {
        let file = std::fs::File::open(&self.state_lock)?;
        let mut lock = libc::flock {
            l_type: libc::F_WRLCK as libc::c_short,
            l_whence: libc::SEEK_SET as libc::c_short,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        // SAFETY: F_GETLK only observes the isolated fixture's state claim;
        // the valid file and output structure outlive this nonblocking call.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((lock.l_type != libc::F_UNLCK as libc::c_short).then_some(lock.l_pid))
    }

    fn diagnostic_command(&self, arguments: &[&str], label: &str) -> String {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ut"));
        command
            .args(arguments)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env_remove("UNITERM_SOCKET")
            .env_remove("UNITERM_PANE_ID");
        common::bounded_diagnostic(&mut command, label, Duration::from_secs(2))
    }

    fn diagnose(&self) {
        let owner = self.owner_pid();
        let _ = writeln!(
            std::io::stderr(),
            "worktree fixture before cleanup: socket={} exists={} owner_pid={owner:?}; daemon logs={:?}; shell={:?}",
            self.socket.display(),
            self.socket.exists(),
            std::env::var_os("UNITERM_DAEMON_LOG_DIR"),
            std::env::var_os("SHELL"),
        );
        let events = self
            .root
            .join("state/uniterm")
            .join(format!("{}.log", self.workspace));
        if let Ok(mut file) = std::fs::File::open(events) {
            let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            let _ = file.seek(std::io::SeekFrom::Start(size.saturating_sub(64 * 1024)));
            let mut tail = Vec::new();
            let _ = file.take(64 * 1024).read_to_end(&mut tail);
            let _ = writeln!(
                std::io::stderr(),
                "worktree fixture event tail: {}",
                String::from_utf8_lossy(&tail)
            );
        }
        for (arguments, label) in [
            (
                vec!["pane", "list", "--json", "-w", &self.workspace],
                "worktree fixture panes",
            ),
            (
                vec![
                    "pane",
                    "read",
                    "1",
                    "--lines",
                    "10",
                    "--json",
                    "-w",
                    &self.workspace,
                ],
                "worktree fixture pane 1",
            ),
        ] {
            let _ = writeln!(
                std::io::stderr(),
                "{}",
                self.diagnostic_command(&arguments, label)
            );
        }
        if let Some(pid) = owner.ok().flatten().and_then(|pid| u32::try_from(pid).ok()) {
            let _ = writeln!(std::io::stderr(), "{}", common::startup_diagnostics(pid));
        }
    }
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.diagnose();
            let _ = writeln!(
                std::io::stderr(),
                "{}",
                self.diagnostic_command(
                    &["workspace", "stop", &self.workspace],
                    "worktree fixture cleanup",
                )
            );
        }
    }
}

fn root() -> PathBuf {
    let nonce = common::unique_nonce();
    std::env::temp_dir().join(format!("ut-wtc-{}-{nonce}", std::process::id()))
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

fn ut(root: &Path, current: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ut"))
        .args(arguments)
        .current_dir(current)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .output()
        .unwrap()
}

#[test]
fn cli_requires_separate_force_confirmation_and_restores_provenance() {
    common::isolate_state();
    let root = root();
    let workspace = unique_workspace_name();
    let repository = root.join("repo");
    let target = root.join("review");
    std::fs::create_dir_all(&repository).unwrap();
    for directory in ["state", "runtime", "config"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    git(&repository, &["init", "-q"]);
    git(
        &repository,
        &["config", "user.email", "uniterm@example.invalid"],
    );
    git(&repository, &["config", "user.name", "Uniterm Test"]);
    std::fs::write(repository.join("README"), "seed\n").unwrap();
    git(&repository, &["add", "README"]);
    git(&repository, &["commit", "-qm", "seed"]);

    let started = ut(&root, &repository, &["workspace", "new", "-d", &workspace]);
    let _workspace_guard = WorkspaceGuard {
        root: root.clone(),
        workspace: workspace.clone(),
        socket: root
            .join("runtime/uniterm")
            .join(format!("{workspace}.sock")),
        state_lock: root
            .join("state/uniterm/locks")
            .join(format!("{workspace}.lock")),
    };
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let added = ut(
        &root,
        &repository,
        &[
            "project",
            "worktree",
            "add",
            "Review",
            repository.to_str().unwrap(),
            target.to_str().unwrap(),
            "-w",
            &workspace,
        ],
    );
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(String::from_utf8_lossy(&added.stdout).contains("uniterm/review"));

    let directory = ut(
        &root,
        &repository,
        &[
            "project",
            "directory",
            "Review",
            repository.to_str().unwrap(),
            "-w",
            &workspace,
        ],
    );
    assert!(!directory.status.success());
    assert!(String::from_utf8_lossy(&directory.stderr).contains("managed by Git"));

    std::fs::write(target.join("keep.txt"), "dirty\n").unwrap();
    let generic = ut(
        &root,
        &repository,
        &["project", "remove", "Review", "-w", &workspace],
    );
    assert!(!generic.status.success());
    assert!(String::from_utf8_lossy(&generic.stderr).contains("Git can verify"));

    let unconfirmed = ut(
        &root,
        &repository,
        &[
            "project", "worktree", "remove", "Review", "--force", "-w", &workspace,
        ],
    );
    assert_eq!(unconfirmed.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unconfirmed.stderr).contains("both --force and --yes"));

    let stopped = ut(&root, &repository, &["workspace", "stop", &workspace]);
    assert!(
        stopped.status.success(),
        "workspace stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let restarted = ut(&root, &repository, &["workspace", "new", "-d", &workspace]);
    assert!(restarted.status.success());
    let listed = ut(
        &root,
        &repository,
        &["project", "worktree", "list", "-w", &workspace],
    );
    assert!(listed.status.success());
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(listed.contains("Review"));
    assert!(listed.contains("uniterm/review"));

    let safe = ut(
        &root,
        &repository,
        &["project", "worktree", "remove", "Review", "-w", &workspace],
    );
    assert!(!safe.status.success());
    assert!(String::from_utf8_lossy(&safe.stderr).contains("uncommitted changes"));
    assert!(target.is_dir());

    let forced = ut(
        &root,
        &repository,
        &[
            "project", "worktree", "remove", "Review", "--force", "--yes", "-w", &workspace,
        ],
    );
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(!target.exists());

    let stopped = ut(&root, &repository, &["workspace", "stop", &workspace]);
    assert!(stopped.status.success());
    std::fs::remove_dir_all(root).unwrap();
}
