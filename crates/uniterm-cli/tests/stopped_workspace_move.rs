//! A stopped Workspace's saved Inbox moves with its other durable files or
//! not at all: a refused or failed rename or archive leaves it attached to
//! the original name, and a retry after the cause is gone moves everything.

use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

/// Each test owns its state directory, so one test's injected failure never
/// reaches another. Files are seeded by path: nothing here depends on this
/// process's own environment.
struct Fixture {
    root: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        common::isolate_state();
        let root = common::temp_dir(tag);
        let state = root.join("state").join("uniterm");
        for directory in [
            state.clone(),
            state.join("inbox"),
            state.join(uniterm_proto::WORKSPACE_CATALOG_DIR),
        ] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)
                .unwrap();
        }
        Self { root, state }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ut"))
            .args(args)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("SHELL", "/bin/sh")
            .env_remove("UNITERM_STATE_KEY_FILE")
            .env_remove("UNITERM_SOCKET")
            .output()
            .unwrap()
    }

    fn file(&self, name: &str, extension: &str) -> PathBuf {
        self.state.join(format!("{name}.{extension}"))
    }

    fn definitions(&self) -> PathBuf {
        self.state.join(uniterm_proto::WORKSPACE_CATALOG_DIR)
    }

    fn definition(&self, name: &str) -> PathBuf {
        self.definitions().join(format!(
            "{}.jsonl",
            uniterm_proto::workspace_catalog_key(name)
        ))
    }

    fn inbox(&self, name: &str) -> PathBuf {
        self.state.join("inbox").join(format!("{name}.inbox"))
    }

    /// The store is opaque to a move, so sealed bytes are stand-ins here.
    fn seed_inbox(&self, name: &str) {
        let directory = self.inbox(name);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        std::fs::write(directory.join("board"), format!("board of {name}")).unwrap();
        std::fs::write(directory.join("key"), format!("key of {name}")).unwrap();
    }

    /// A stopped Workspace with a definition, a snapshot and an event log.
    fn seed_workspace(&self, name: &str) {
        std::fs::write(self.definition(name), "{}\n").unwrap();
        std::fs::write(self.file(name, "snap"), format!("snapshot of {name}")).unwrap();
        std::fs::write(self.file(name, "log"), "").unwrap();
    }

    fn assert_whole(&self, name: &str, origin: &str) {
        assert!(self.definition(name).is_file());
        assert_eq!(
            std::fs::read_to_string(self.file(name, "snap")).unwrap(),
            format!("snapshot of {origin}")
        );
        assert!(self.file(name, "log").is_file());
        assert_eq!(
            std::fs::read_to_string(self.inbox(name).join("board")).unwrap(),
            format!("board of {origin}")
        );
        assert_eq!(
            std::fs::read_to_string(self.inbox(name).join("key")).unwrap(),
            format!("key of {origin}")
        );
    }

    fn assert_absent(&self, name: &str) {
        assert!(!self.definition(name).exists());
        assert!(!self.file(name, "snap").exists());
        assert!(!self.file(name, "log").exists());
        assert!(!self.file(name, "privacy").exists());
        assert!(!self.inbox(name).exists());
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A refusal or a returned move never reports the rename as done.
fn assert_refused(output: &Output) {
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("renamed"), "{stdout}");
    assert!(!stderr(output).is_empty());
}

#[test]
fn failed_stopped_rename_returns_the_inbox_and_a_retry_moves_everything() {
    let fixture = Fixture::new("move-rename");
    let old = common::unique_workspace_name();
    let new = common::unique_workspace_name();
    fixture.seed_workspace(&old);
    fixture.seed_inbox(&old);
    // The definition moves last. A directory in its place fails that step
    // after the Inbox, the snapshot and the event log have all moved.
    let blocker = fixture.definition(&new);
    std::fs::create_dir_all(blocker.join("occupied")).unwrap();

    let failed = fixture.run(&["workspace", "rename", &old, &new]);
    assert_refused(&failed);
    assert!(
        stderr(&failed).contains(&format!("it is still '{old}'")),
        "{}",
        stderr(&failed)
    );
    fixture.assert_whole(&old, &old);
    assert!(!fixture.file(&new, "snap").exists());
    assert!(!fixture.file(&new, "log").exists());
    assert!(!fixture.inbox(&new).exists());

    std::fs::remove_dir_all(blocker).unwrap();
    let retried = fixture.run(&["workspace", "rename", &old, &new]);
    assert!(retried.status.success(), "{}", stderr(&retried));
    fixture.assert_whole(&new, &old);
    fixture.assert_absent(&old);
}

#[test]
fn finished_protected_rename_moves_the_marker_and_releases_the_old_name() {
    let fixture = Fixture::new("move-marker");
    let old = common::unique_workspace_name();
    let new = common::unique_workspace_name();
    fixture.seed_workspace(&old);
    fixture.seed_inbox(&old);
    // A move copies the marker without opening it, so any bytes stand in.
    std::fs::write(fixture.file(&old, "privacy"), "marker").unwrap();

    let renamed = fixture.run(&["workspace", "rename", &old, &new]);
    assert!(renamed.status.success(), "{}", stderr(&renamed));
    fixture.assert_whole(&new, &old);
    assert_eq!(
        std::fs::read_to_string(fixture.file(&new, "privacy")).unwrap(),
        "marker"
    );
    // Left behind, the marker would protect whoever reuses the old name.
    fixture.assert_absent(&old);
}

#[test]
fn stopped_rename_refuses_an_unfinished_protection_migration_before_moving_anything() {
    let fixture = Fixture::new("move-pending");
    let old = common::unique_workspace_name();
    let new = common::unique_workspace_name();
    fixture.seed_workspace(&old);
    fixture.seed_inbox(&old);
    let pending = fixture.file(&old, "privacy.pending");
    std::fs::write(&pending, "interrupted").unwrap();

    assert_refused(&fixture.run(&["workspace", "rename", &old, &new]));
    fixture.assert_whole(&old, &old);
    fixture.assert_absent(&new);

    std::fs::remove_file(pending).unwrap();
    let retried = fixture.run(&["workspace", "rename", &old, &new]);
    assert!(retried.status.success(), "{}", stderr(&retried));
    fixture.assert_whole(&new, &old);
    fixture.assert_absent(&old);
}

#[test]
fn stopped_rename_refuses_an_occupied_destination_inbox_even_with_only_a_key() {
    let fixture = Fixture::new("move-collide");
    // Without an Inbox of its own, the renamed Workspace would silently adopt
    // whatever is stored under the destination name, a lone key included.
    for (source_inbox, key_only) in [(false, false), (false, true), (true, false), (true, true)] {
        let case = format!("source Inbox {source_inbox}, key-only destination {key_only}");
        let old = common::unique_workspace_name();
        let new = common::unique_workspace_name();
        fixture.seed_workspace(&old);
        if source_inbox {
            fixture.seed_inbox(&old);
        }
        fixture.seed_inbox(&new);
        if key_only {
            std::fs::remove_file(fixture.inbox(&new).join("board")).unwrap();
        }

        assert_refused(&fixture.run(&["workspace", "rename", &old, &new]));
        assert!(fixture.definition(&old).is_file(), "{case}");
        assert!(fixture.file(&old, "snap").is_file(), "{case}");
        assert!(fixture.file(&old, "log").is_file(), "{case}");
        assert_eq!(fixture.inbox(&old).exists(), source_inbox, "{case}");
        if source_inbox {
            fixture.assert_whole(&old, &old);
        }
        assert!(!fixture.definition(&new).exists(), "{case}");
        assert!(!fixture.file(&new, "snap").exists(), "{case}");
        assert!(!fixture.file(&new, "log").exists(), "{case}");
        assert_eq!(
            std::fs::read_to_string(fixture.inbox(&new).join("key")).unwrap(),
            format!("key of {new}"),
            "{case}"
        );
        assert_eq!(
            fixture.inbox(&new).join("board").exists(),
            !key_only,
            "{case}"
        );
    }
}

#[test]
fn stopped_rename_refuses_a_name_that_kept_another_workspaces_protection_marker() {
    let fixture = Fixture::new("move-stale");
    let old = common::unique_workspace_name();
    let new = common::unique_workspace_name();
    fixture.seed_workspace(&old);
    fixture.seed_inbox(&old);
    let stale = fixture.file(&new, "privacy");
    std::fs::write(&stale, "stale marker").unwrap();

    assert_refused(&fixture.run(&["workspace", "rename", &old, &new]));
    fixture.assert_whole(&old, &old);
    assert_eq!(std::fs::read_to_string(&stale).unwrap(), "stale marker");
    std::fs::remove_file(stale).unwrap();
    fixture.assert_absent(&new);
}

fn desktop_data(root: &Path, workspace: &str) -> PathBuf {
    let data = root.join("desktop");
    let project = root.join("project");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        data.join("project_workspaces.json"),
        format!(r#"[{{"id":"work","name":"{workspace}","sort_order":0}}]"#),
    )
    .unwrap();
    std::fs::write(
        data.join("projects.json"),
        format!(
            r#"[{{"id":"p1","name":"Site","path":"{}","sort_order":0,"workspace_id":"work"}}]"#,
            project.display()
        ),
    )
    .unwrap();
    std::fs::write(
        data.join("workspaces_v3.json"),
        r#"[{"project_id":"p1","tabs":[{"custom_title":null}]}]"#,
    )
    .unwrap();
    data
}

#[test]
fn refused_stopped_archive_keeps_the_inbox_attached_and_a_retry_archives_it() {
    let fixture = Fixture::new("move-archive");
    let name = common::unique_workspace_name();
    fixture.seed_workspace(&name);
    fixture.seed_inbox(&name);
    let pending = fixture.file(&name, "privacy.pending");
    std::fs::write(&pending, "interrupted").unwrap();
    let data = desktop_data(&fixture.root, &name);
    let migrate = [
        "migrate",
        "from-desktop",
        "--data-dir",
        data.to_str().unwrap(),
        "--on-conflict",
        "archive",
        "--yes",
    ];
    let archives = || {
        std::fs::read_dir(fixture.state.join("inbox"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|file| file.starts_with(&format!("{name}-before-desktop-")))
            .collect::<Vec<_>>()
    };

    let refused = fixture.run(&migrate);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("could not archive the existing Workspace"),
        "{}",
        stderr(&refused)
    );
    let stdout = String::from_utf8_lossy(&refused.stdout);
    assert!(
        !stdout.contains("Archived") && !stdout.contains("Imported"),
        "{stdout}"
    );
    fixture.assert_whole(&name, &name);
    assert_eq!(archives(), Vec::<String>::new());

    std::fs::remove_file(pending).unwrap();
    let retried = fixture.run(&migrate);
    let stopped = fixture.run(&["workspace", "stop", &name]);
    assert!(retried.status.success(), "{}", stderr(&retried));
    assert!(stopped.status.success(), "{}", stderr(&stopped));
    let archived = archives();
    assert_eq!(archived.len(), 1, "{archived:?}");
    let archive = archived[0].strip_suffix(".inbox").unwrap();
    fixture.assert_whole(archive, &name);
}
