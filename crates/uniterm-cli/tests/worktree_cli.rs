//! Human CLI contract for worktree safety and clean-stop restoration.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

mod common;

use common::unique_workspace_name;

fn root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
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
    assert!(stopped.status.success());
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
