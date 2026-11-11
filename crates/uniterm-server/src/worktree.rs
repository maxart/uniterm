//! Git-authoritative worktree lifecycle operations.
//!
//! Every function here is blocking by design and is called only from the
//! Tokio runtime's blocking pool. The mio core receives typed results and
//! changes its Project projection only after Git confirms the side effect.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use uniterm_proto::{
    WorktreeAction, WorktreeEntry, WorktreeRegistration, WorktreeResult, WorktreeRuntimeOperation,
    WorktreeState,
};

#[derive(Debug)]
struct GitWorktree {
    path: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    prunable: bool,
}

pub(crate) fn branch_name(name: &str, project: uniterm_core::ProjectId) -> String {
    let slug: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches(['-', '.']);
    if slug.is_empty() {
        format!("uniterm/worktree-{}", project.0)
    } else {
        format!("uniterm/{slug}")
    }
}

pub(crate) fn run(operation: WorktreeRuntimeOperation) -> WorktreeResult {
    match operation {
        WorktreeRuntimeOperation::Add { registration, base } => add(registration, base),
        WorktreeRuntimeOperation::RollbackAdd { registration } => rollback_add(registration),
        WorktreeRuntimeOperation::Inspect {
            action,
            registrations,
            force,
        } => inspect_operation(action, registrations, force),
    }
}

pub(crate) fn reject(
    operation: WorktreeRuntimeOperation,
    error: impl Into<String>,
) -> WorktreeResult {
    let (action, items) = match operation {
        WorktreeRuntimeOperation::Add { registration, .. } => (
            WorktreeAction::Add,
            vec![WorktreeEntry {
                registration,
                state: WorktreeState::Missing,
                current_branch: None,
                head: None,
                dirty: false,
            }],
        ),
        WorktreeRuntimeOperation::RollbackAdd { registration } => (
            WorktreeAction::Add,
            vec![WorktreeEntry {
                registration,
                state: WorktreeState::Missing,
                current_branch: None,
                head: None,
                dirty: false,
            }],
        ),
        WorktreeRuntimeOperation::Inspect {
            action,
            registrations,
            ..
        } => (
            action,
            registrations
                .into_iter()
                .map(|registration| WorktreeEntry {
                    registration,
                    state: WorktreeState::Missing,
                    current_branch: None,
                    head: None,
                    dirty: false,
                })
                .collect(),
        ),
    };
    failure(action, error, items)
}

fn add(mut registration: WorktreeRegistration, base: Option<String>) -> WorktreeResult {
    let action = WorktreeAction::Add;
    let attempted = registration.clone();
    let outcome = (|| -> Result<WorktreeEntry, String> {
        let requested = PathBuf::from(&registration.path);
        if !requested.is_absolute() {
            return Err("worktree path must be absolute".into());
        }
        let repository = canonical_directory(Path::new(&registration.repository), "repository")?;
        let before = list(&repository)?;
        let primary = before
            .first()
            .ok_or_else(|| "Git reported no primary worktree".to_string())?;
        let repository_root = canonical_or_original(&primary.path)?;
        let mut command = Command::new("git");
        command
            .args(["-C"])
            .arg(&repository)
            .args(["worktree", "add", "-b"])
            .arg(&registration.branch)
            .arg(&requested);
        if let Some(base) = base.as_deref().filter(|value| !value.trim().is_empty()) {
            command.arg(base);
        }
        checked(command, "git worktree add")?;
        let finalized = (|| -> Result<WorktreeEntry, String> {
            let created = canonical_directory(&requested, "created worktree")?;
            let after = list(&repository)?;
            let authoritative = find_path(&after, &created)
                .ok_or_else(|| "Git did not report the created worktree".to_string())?;
            registration.repository = path_text(&repository_root)?;
            registration.path = path_text(&created)?;
            registration.branch = authoritative
                .branch
                .clone()
                .ok_or_else(|| "created worktree has no local branch".to_string())?;
            registration.created_head = authoritative.head.clone().unwrap_or_default();
            Ok(WorktreeEntry {
                registration: registration.clone(),
                state: if authoritative.prunable {
                    WorktreeState::Prunable
                } else {
                    WorktreeState::Active
                },
                current_branch: authoritative.branch.clone(),
                head: authoritative.head.clone(),
                dirty: is_dirty(&created)?,
            })
        })();
        match finalized {
            Ok(item) => Ok(item),
            Err(error) => {
                registration.repository = path_text(&repository_root)?;
                registration.path = path_text(&requested)?;
                let rolled_back = rollback_add(registration);
                if rolled_back.accepted {
                    Err(format!("{error}; Git creation was rolled back"))
                } else {
                    Err(format!(
                        "{error}; automatic Git rollback failed: {}",
                        rolled_back
                            .error
                            .as_deref()
                            .unwrap_or("unknown rollback error")
                    ))
                }
            }
        }
    })();
    match outcome {
        Ok(item) => success(action, vec![item]),
        Err(error) => failure(
            action,
            error,
            vec![WorktreeEntry {
                registration: attempted,
                state: WorktreeState::Missing,
                current_branch: None,
                head: None,
                dirty: false,
            }],
        ),
    }
}

fn inspect_operation(
    action: WorktreeAction,
    registrations: Vec<WorktreeRegistration>,
    force: bool,
) -> WorktreeResult {
    match action {
        WorktreeAction::List => inspect_all(registrations),
        WorktreeAction::Open => one_registration(action, registrations, |registration| {
            let entry = inspect(&registration)?;
            if entry.state != WorktreeState::Active || !Path::new(&registration.path).is_dir() {
                return Err("worktree is not available to open".into());
            }
            Ok(entry)
        }),
        WorktreeAction::Remove => one_registration(action, registrations, |registration| {
            remove(registration, force)
        }),
        WorktreeAction::Cleanup => one_registration(action, registrations, cleanup_registration),
        WorktreeAction::Add => failure(action, "invalid worktree operation", Vec::new()),
    }
}

fn rollback_add(registration: WorktreeRegistration) -> WorktreeResult {
    let outcome = (|| -> Result<WorktreeEntry, String> {
        let inspected = inspect(&registration)?;
        let repository = canonical_directory(Path::new(&registration.repository), "repository")?;
        if inspected.state != WorktreeState::Missing {
            let mut command = Command::new("git");
            command
                .args(["-C"])
                .arg(&repository)
                .args(["worktree", "remove", "--force"])
                .arg(&registration.path);
            checked(command, "git worktree rollback")?;
        }
        let worktrees = list(&repository)?;
        if find_path(&worktrees, Path::new(&registration.path)).is_some() {
            return Err("Git still reports the worktree after rollback".into());
        }
        let mut command = Command::new("git");
        command
            .args(["-C"])
            .arg(&repository)
            .args(["branch", "-D"])
            .arg(&registration.branch);
        checked(command, "git worktree branch rollback")?;
        Ok(WorktreeEntry {
            registration,
            state: WorktreeState::Missing,
            current_branch: None,
            head: None,
            dirty: false,
        })
    })();
    match outcome {
        Ok(item) => success(WorktreeAction::Add, vec![item]),
        Err(error) => failure(WorktreeAction::Add, error, Vec::new()),
    }
}

fn one_registration(
    action: WorktreeAction,
    registrations: Vec<WorktreeRegistration>,
    operation: impl FnOnce(WorktreeRegistration) -> Result<WorktreeEntry, String>,
) -> WorktreeResult {
    let Some(registration) = registrations.into_iter().next() else {
        return failure(action, "worktree Project was not found", Vec::new());
    };
    match operation(registration) {
        Ok(item) => success(action, vec![item]),
        Err(error) => failure(action, error, Vec::new()),
    }
}

fn inspect_all(registrations: Vec<WorktreeRegistration>) -> WorktreeResult {
    let mut items = Vec::with_capacity(registrations.len());
    let mut errors = Vec::new();
    let mut cache: HashMap<PathBuf, Vec<GitWorktree>> = HashMap::new();
    for registration in registrations {
        let inspected = (|| {
            let repository =
                canonical_directory(Path::new(&registration.repository), "repository")?;
            if !cache.contains_key(&repository) {
                cache.insert(repository.clone(), list(&repository)?);
            }
            inspect_against(
                &registration,
                cache.get(&repository).expect("worktree list was inserted"),
            )
        })();
        match inspected {
            Ok(item) => items.push(item),
            Err(error) => {
                errors.push(format!("{}: {error}", registration.project_name));
                items.push(WorktreeEntry {
                    registration,
                    state: WorktreeState::Missing,
                    current_branch: None,
                    head: None,
                    dirty: false,
                });
            }
        }
    }
    WorktreeResult {
        action: WorktreeAction::List,
        accepted: errors.is_empty(),
        error: (!errors.is_empty()).then(|| errors.join("; ")),
        items,
    }
}

fn inspect(registration: &WorktreeRegistration) -> Result<WorktreeEntry, String> {
    let repository = canonical_directory(Path::new(&registration.repository), "repository")?;
    let worktrees = list(&repository)?;
    inspect_against(registration, &worktrees)
}

fn inspect_against(
    registration: &WorktreeRegistration,
    worktrees: &[GitWorktree],
) -> Result<WorktreeEntry, String> {
    verify_repository(registration, worktrees)?;
    let registered_path = Path::new(&registration.path);
    let Some(current) = find_path(worktrees, registered_path) else {
        return Ok(WorktreeEntry {
            registration: registration.clone(),
            state: WorktreeState::Missing,
            current_branch: None,
            head: None,
            dirty: false,
        });
    };
    let state = if current.prunable {
        WorktreeState::Prunable
    } else {
        WorktreeState::Active
    };
    Ok(WorktreeEntry {
        registration: registration.clone(),
        state,
        current_branch: current.branch.clone(),
        head: current.head.clone(),
        dirty: state == WorktreeState::Active && is_dirty(registered_path)?,
    })
}

fn remove(registration: WorktreeRegistration, force: bool) -> Result<WorktreeEntry, String> {
    let inspected = inspect(&registration)?;
    if inspected.state == WorktreeState::Missing {
        return Err("worktree is already absent; use cleanup to forget the stale Project".into());
    }
    if inspected.dirty && !force {
        return Err("worktree has uncommitted changes; removal was refused".into());
    }
    let repository = canonical_directory(Path::new(&registration.repository), "repository")?;
    let mut command = Command::new("git");
    command
        .args(["-C"])
        .arg(&repository)
        .args(["worktree", "remove"]);
    if force {
        command.arg("--force");
    }
    command.arg(&registration.path);
    checked(command, "git worktree remove")?;
    let worktrees = list(&repository)?;
    if find_path(&worktrees, Path::new(&registration.path)).is_some() {
        return Err("Git still reports the worktree after removal".into());
    }
    Ok(WorktreeEntry {
        registration,
        state: WorktreeState::Missing,
        current_branch: None,
        head: None,
        dirty: false,
    })
}

fn cleanup_registration(registration: WorktreeRegistration) -> Result<WorktreeEntry, String> {
    let inspected = inspect(&registration)?;
    if inspected.state == WorktreeState::Active && Path::new(&registration.path).exists() {
        return Err("worktree still exists; use remove instead of cleanup".into());
    }
    let repository = canonical_directory(Path::new(&registration.repository), "repository")?;
    let mut command = Command::new("git");
    command
        .args(["-C"])
        .arg(&repository)
        .args(["worktree", "prune"]);
    checked(command, "git worktree prune")?;
    let worktrees = list(&repository)?;
    if find_path(&worktrees, Path::new(&registration.path)).is_some() {
        return Err("Git still reports the worktree after prune".into());
    }
    Ok(WorktreeEntry {
        registration,
        state: WorktreeState::Missing,
        current_branch: None,
        head: None,
        dirty: false,
    })
}

fn verify_repository(
    registration: &WorktreeRegistration,
    worktrees: &[GitWorktree],
) -> Result<(), String> {
    let primary = worktrees
        .first()
        .ok_or_else(|| "Git reported no primary worktree".to_string())?;
    let expected = canonical_directory(Path::new(&registration.repository), "repository")?;
    let actual = canonical_or_original(&primary.path)?;
    if expected != actual {
        return Err("registered repository no longer matches Git authority".into());
    }
    Ok(())
}

fn list(repository: &Path) -> Result<Vec<GitWorktree>, String> {
    let mut command = Command::new("git");
    command
        .args(["-C"])
        .arg(repository)
        .args(["worktree", "list", "--porcelain", "-z"]);
    let output = checked(command, "git worktree list")?;
    parse_porcelain(&output.stdout)
}

fn parse_porcelain(bytes: &[u8]) -> Result<Vec<GitWorktree>, String> {
    let mut worktrees = Vec::new();
    let mut current: Option<GitWorktree> = None;
    for field in bytes.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let Some(entry) = current.take() {
                worktrees.push(entry);
            }
            continue;
        }
        let text = std::str::from_utf8(field)
            .map_err(|_| "Git returned a non-UTF-8 worktree path".to_string())?;
        if let Some(path) = text.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                worktrees.push(entry);
            }
            current = Some(GitWorktree {
                path: PathBuf::from(path),
                head: None,
                branch: None,
                prunable: false,
            });
        } else if let Some(entry) = current.as_mut() {
            if let Some(head) = text.strip_prefix("HEAD ") {
                entry.head = Some(head.to_string());
            } else if let Some(branch) = text.strip_prefix("branch refs/heads/") {
                entry.branch = Some(branch.to_string());
            } else if text == "prunable" || text.starts_with("prunable ") {
                entry.prunable = true;
            }
        }
    }
    if let Some(entry) = current {
        worktrees.push(entry);
    }
    if worktrees.is_empty() {
        Err("Git returned an empty worktree list".into())
    } else {
        Ok(worktrees)
    }
}

fn find_path<'a>(worktrees: &'a [GitWorktree], path: &Path) -> Option<&'a GitWorktree> {
    let expected = canonical_or_original(path).ok()?;
    worktrees.iter().find(|entry| {
        canonical_or_original(&entry.path).is_ok_and(|candidate| candidate == expected)
    })
}

fn is_dirty(path: &Path) -> Result<bool, String> {
    let mut command = Command::new("git");
    command
        .args(["-C"])
        .arg(path)
        .args(["status", "--porcelain=v1", "--untracked-files=normal"]);
    Ok(!checked(command, "git status")?.stdout.is_empty())
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let path = std::fs::canonicalize(path)
        .map_err(|error| format!("invalid {label} {}: {error}", path.display()))?;
    if !path.is_dir() {
        return Err(format!("{label} is not a directory: {}", path.display()));
    }
    Ok(path)
}

fn canonical_or_original(path: &Path) -> Result<PathBuf, String> {
    if path.exists() {
        std::fs::canonicalize(path).map_err(|error| error.to_string())
    } else if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Err(format!("path is not absolute: {}", path.display()))
    }
}

fn path_text(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| format!("path is not UTF-8: {}", path.display()))
}

fn checked(mut command: Command, label: &str) -> Result<Output, String> {
    let output = command
        .output()
        .map_err(|error| format!("could not run {label}: {error}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("{label} failed: {}", stderr.trim()))
    }
}

fn success(action: WorktreeAction, items: Vec<WorktreeEntry>) -> WorktreeResult {
    WorktreeResult {
        action,
        accepted: true,
        error: None,
        items,
    }
}

fn failure(
    action: WorktreeAction,
    error: impl Into<String>,
    items: Vec<WorktreeEntry>,
) -> WorktreeResult {
    WorktreeResult {
        action,
        accepted: false,
        error: Some(error.into()),
        items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    use uniterm_core::ProjectId;

    fn temp_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "uniterm-worktree-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn git(repository: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(["-C"])
            .arg(repository)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository(label: &str) -> PathBuf {
        let repository = temp_root(label).join("repo");
        fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-q"]);
        git(
            &repository,
            &["config", "user.email", "uniterm@example.invalid"],
        );
        git(&repository, &["config", "user.name", "Uniterm Test"]);
        fs::write(repository.join("README"), "seed\n").unwrap();
        git(&repository, &["add", "README"]);
        git(&repository, &["commit", "-qm", "seed"]);
        repository
    }

    fn registration(repository: &Path, path: &Path) -> WorktreeRegistration {
        WorktreeRegistration {
            project: ProjectId(7),
            project_name: "Review".into(),
            repository: repository.to_string_lossy().into_owned(),
            path: path.to_string_lossy().into_owned(),
            branch: "uniterm/review".into(),
            created_head: String::new(),
        }
    }

    #[test]
    fn lifecycle_refuses_dirty_remove_and_force_is_explicit() {
        let repository = repository("lifecycle");
        let root = repository.parent().unwrap().to_path_buf();
        let target = root.join("review tree");
        let added = run(WorktreeRuntimeOperation::Add {
            registration: registration(&repository, &target),
            base: None,
        });
        assert!(added.accepted, "{:?}", added.error);
        let registered = added.items[0].registration.clone();
        fs::write(target.join("notes.txt"), "dirty\n").unwrap();

        let refused = run(WorktreeRuntimeOperation::Inspect {
            action: WorktreeAction::Remove,
            registrations: vec![registered.clone()],
            force: false,
        });
        assert!(!refused.accepted);
        assert!(refused.error.unwrap().contains("uncommitted changes"));
        assert!(target.is_dir());

        let removed = run(WorktreeRuntimeOperation::Inspect {
            action: WorktreeAction::Remove,
            registrations: vec![registered],
            force: true,
        });
        assert!(removed.accepted, "{:?}", removed.error);
        assert!(!target.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_only_accepts_an_absent_or_prunable_worktree() {
        let repository = repository("cleanup");
        let root = repository.parent().unwrap().to_path_buf();
        let target = root.join("cleanup-tree");
        let added = run(WorktreeRuntimeOperation::Add {
            registration: registration(&repository, &target),
            base: None,
        });
        let registered = added.items[0].registration.clone();

        let refused = run(WorktreeRuntimeOperation::Inspect {
            action: WorktreeAction::Cleanup,
            registrations: vec![registered.clone()],
            force: false,
        });
        assert!(!refused.accepted);
        assert!(target.exists());

        fs::remove_dir_all(&target).unwrap();
        let cleaned = run(WorktreeRuntimeOperation::Inspect {
            action: WorktreeAction::Cleanup,
            registrations: vec![registered],
            force: false,
        });
        assert!(cleaned.accepted, "{:?}", cleaned.error);
        fs::remove_dir_all(root).unwrap();
    }
}
