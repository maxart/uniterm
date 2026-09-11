//! Append-only lightweight Workspace definitions.
//!
//! Cleanly stopping a Workspace intentionally discards its PTYs, terminal
//! snapshot, and runtime event stream. This separate structural event log
//! retains only Workspace > Project > Tab definitions and anonymous split
//! geometry, so a later start can reconstruct fresh shells without
//! resurrecting content or processes.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use uniterm_proto::{
    workspace_catalog_key, workspace_name_from_catalog_key, WorkspaceDefinition,
    WORKSPACE_CATALOG_DIR,
};

fn catalog_dir() -> PathBuf {
    crate::persist::state_dir().join(WORKSPACE_CATALOG_DIR)
}

fn path(name: &str) -> PathBuf {
    catalog_dir().join(format!("{}.jsonl", workspace_catalog_key(name)))
}

/// Prepare one immutable definition event before it crosses the runtime seam.
pub fn encode(definition: &WorkspaceDefinition) -> std::io::Result<String> {
    let mut line = serde_json::to_string(definition)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    line.push('\n');
    Ok(line)
}

/// Append one prepared definition event. Normal server writes call this only
/// from the tokio runtime.
pub fn append_line(name: &str, line: &str) -> std::io::Result<()> {
    let path = path(name);
    let mut file = crate::persist::open_private_append(&path)?;
    let length = file.metadata()?.len();
    if length != 0 {
        use std::os::unix::fs::FileExt as _;
        let mut last = [0];
        std::fs::File::open(&path)?.read_exact_at(&mut last, length - 1)?;
        if last[0] != b'\n' {
            // A previous failed write may have left half a JSON record. End
            // it so this retry remains independently readable by recovery.
            file.write_all(b"\n")?;
        }
    }
    file.write_all(line.as_bytes())?;
    crate::persist::sync_file_for_crash(&file)?;
    crate::persist::sync_parent_directory(&path)
}

/// Load the latest valid structural event for one Workspace.
pub fn load(name: &str) -> Option<WorkspaceDefinition> {
    latest_definition(&std::fs::read_to_string(path(name)).ok()?)
}

fn latest_definition(contents: &str) -> Option<WorkspaceDefinition> {
    latest_valid_line(contents).map(|(_, definition)| definition)
}

fn latest_valid_line(contents: &str) -> Option<(&str, WorkspaceDefinition)> {
    contents.lines().rev().find_map(|line| {
        serde_json::from_str::<WorkspaceDefinition>(line)
            .ok()
            .filter(WorkspaceDefinition::is_valid)
            .map(|definition| (line, definition))
    })
}

/// Fold a definition file down to its latest valid line.
///
/// Only that line is ever read, so everything before it is dead weight that
/// each load and listing would parse again. The rewrite is atomic (temp file,
/// then rename) so a concurrent `ut workspace list` sees the old or the new
/// file, never a torn one, and a file that is already one line is untouched.
pub fn compact(name: &str) -> std::io::Result<()> {
    compact_path(&path(name))
}

fn compact_path(path: &Path) -> std::io::Result<()> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let Some((latest, _)) = latest_valid_line(&contents) else {
        return Ok(());
    };
    if contents.trim_end_matches('\n') == latest {
        return Ok(());
    }
    let tmp = path.with_extension("jsonl.compact.tmp");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(latest.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)
}

/// Enumerate every remembered Workspace and its latest definition.
pub fn list() -> Vec<(String, WorkspaceDefinition)> {
    let Ok(entries) = std::fs::read_dir(catalog_dir()) else {
        return Vec::new();
    };
    let mut definitions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(workspace_name_from_catalog_key)
        else {
            continue;
        };
        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Some(definition) = latest_definition(&contents) {
                definitions.push((name, definition));
            }
        }
    }
    definitions.sort_by(|left, right| left.0.cmp(&right.0));
    definitions
}

/// Enumerate every Workspace catalog key, including a definition whose latest
/// record is incomplete or invalid, so bulk deletion can still remove it.
pub fn list_names() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(catalog_dir()) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(name) = path
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(workspace_name_from_catalog_key)
        {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Return whether a remembered definition exists for `name`.
pub fn exists(name: &str) -> bool {
    path(name).is_file()
}

/// Move a dormant or live Workspace definition to a new name.
pub fn rename(old: &str, new: &str) -> std::io::Result<()> {
    let old = path(old);
    let new = path(new);
    if let Some(parent) = new.parent() {
        crate::persist::ensure_private_dir(parent)?;
    }
    match std::fs::rename(old, new) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Permanently forget a stopped Workspace definition.
pub fn delete(name: &str) -> std::io::Result<()> {
    match std::fs::remove_file(path(name)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniterm_core::ProjectId;
    use uniterm_proto::{
        WorkspaceLayoutDefinition, WorkspaceProjectDefinition, WorkspaceTabDefinition,
    };

    fn definition(root: &str, tabs: usize) -> WorkspaceDefinition {
        WorkspaceDefinition {
            version: WorkspaceDefinition::VERSION,
            active_project: ProjectId(7),
            agent_scope_workspace: false,
            server_scope_workspace: false,
            projects: vec![WorkspaceProjectDefinition {
                id: ProjectId(7),
                name: "Site".into(),
                root: root.into(),
                worktree: None,
                active_tab: tabs.saturating_sub(1),
                tabs: (0..tabs)
                    .map(|index| WorkspaceTabDefinition {
                        name: Some(format!("Tab {}", index + 1)),
                        layout: WorkspaceLayoutDefinition::Pane,
                    })
                    .collect(),
            }],
        }
    }

    #[test]
    fn latest_valid_definition_wins_and_partial_tail_is_ignored() {
        let first = definition("/tmp/first", 1);
        let latest = definition("/tmp/latest", 2);
        let contents = format!(
            "{}{}{{\"partial\"",
            encode(&first).unwrap(),
            encode(&latest).unwrap()
        );
        assert_eq!(latest_definition(&contents), Some(latest));
    }

    #[test]
    fn compaction_keeps_only_the_latest_valid_line_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!(
            "uniterm-catalog-compact-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("576f726b.jsonl");
        let first = encode(&definition("/tmp/first", 1)).unwrap();
        let latest = encode(&definition("/tmp/latest", 2)).unwrap();
        // Hundreds of identical checkpoints, one newer definition, then a
        // partial tail from an interrupted append.
        let mut contents = first.repeat(300);
        contents.push_str(&latest);
        contents.push_str("{\"partial\"");
        std::fs::write(&path, &contents).unwrap();

        compact_path(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), latest);
        assert_eq!(
            latest_definition(&std::fs::read_to_string(&path).unwrap()),
            Some(definition("/tmp/latest", 2))
        );
        let compacted = std::fs::metadata(&path).unwrap().modified().unwrap();

        // Already compact: no rewrite, and a missing file is not an error.
        compact_path(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            compacted
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), latest);
        compact_path(&dir.join("missing.jsonl")).unwrap();
        assert!(!dir.join("576f726b.jsonl.compact.tmp").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn legacy_tabs_without_layout_restore_as_one_fresh_pane() {
        let json = r#"{"version":1,"active_project":7,"projects":[{"id":7,"name":"Legacy","root":"/tmp/legacy","active_tab":0,"tabs":[{"name":"Old"}]}]}"#;
        let parsed: WorkspaceDefinition = serde_json::from_str(json).unwrap();
        let definition = latest_definition(json).unwrap();
        assert_eq!(definition, parsed);
        assert!(definition.is_valid());
        assert_eq!(
            definition.projects[0].tabs[0].layout,
            WorkspaceLayoutDefinition::Pane
        );
    }

    #[test]
    fn worktree_provenance_survives_clean_stop_catalog_round_trip() {
        let mut definition = definition("/tmp/review", 1);
        definition.projects[0].worktree = Some(uniterm_proto::WorktreeRegistration {
            project: ProjectId(7),
            project_name: "Site".into(),
            repository: "/tmp/site".into(),
            path: "/tmp/review".into(),
            branch: "uniterm/review".into(),
            created_head: "0123456789abcdef".into(),
        });
        let encoded = encode(&definition).unwrap();
        assert_eq!(latest_definition(&encoded), Some(definition));
    }
}
