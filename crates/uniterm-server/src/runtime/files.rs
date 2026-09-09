//! Project-scoped filesystem operations, bounded artifact observation, and
//! event-driven watches. All filesystem ownership checks live with the work.

use crossbeam_channel::Sender;
use mio::Waker;
use notify::Watcher as _;
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniterm_proto::{AgentToCore, FileEntry, FileOperation, ProjectId, FILE_LISTING_LIMIT};

/// A terminal tree cannot usefully render an unbounded single directory.
/// Bounding one listing prevents an imported root with millions of immediate
/// children from exhausting the server while leaving the returned prefix
/// browsable.
/// Expanded folders beyond this bound remain browsable and manually
/// refreshable, but do not consume another OS watch registration.
const MAX_PROJECT_WATCHES: usize = 256;
/// Artifact hashing stays bounded even though it runs away from the mio loop.
const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

pub(super) fn validate_artifacts(
    project_root: &str,
    expected: &[uniterm_proto::ArtifactClaim],
    reported: &[uniterm_proto::ArtifactClaim],
) -> std::io::Result<Vec<uniterm_proto::ArtifactObservation>> {
    let mut artifacts = Vec::new();
    for claim in expected.iter().chain(reported) {
        let Some(observation) = observe_artifact(project_root, claim)? else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("artifact must be a non-empty file: {}", claim.path),
            ));
        };
        if artifacts
            .iter()
            .any(|artifact: &uniterm_proto::ArtifactObservation| artifact.path == observation.path)
        {
            continue;
        }
        artifacts.push(observation);
    }
    Ok(artifacts)
}

pub(super) fn observe_artifact(
    project_root: &str,
    claim: &uniterm_proto::ArtifactClaim,
) -> std::io::Result<Option<uniterm_proto::ArtifactObservation>> {
    use sha2::Digest as _;
    use std::io::Read as _;

    let root = std::fs::canonicalize(project_root)?;
    if !root.is_dir() || claim.path.is_empty() || claim.path.as_bytes().contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Project root and artifact path must be valid",
        ));
    }
    let path = Path::new(&claim.path);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let canonical = match std::fs::canonicalize(&candidate) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !canonical.starts_with(&root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("artifact escapes the Project root: {}", canonical.display()),
        ));
    }
    let mut file = std::fs::File::open(&canonical)?;
    let metadata = file.metadata()?;
    let current = std::fs::canonicalize(&canonical)?;
    let current_metadata = current.metadata()?;
    if !current.starts_with(&root)
        || metadata.dev() != current_metadata.dev()
        || metadata.ino() != current_metadata.ino()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "artifact changed identity while Project ownership was validated",
        ));
    }
    if !metadata.is_file() || metadata.len() == 0 {
        return Ok(None);
    }
    if metadata.len() > MAX_ARTIFACT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("artifact exceeds {MAX_ARTIFACT_BYTES} bytes"),
        ));
    }
    let relative = current.strip_prefix(&root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "artifact lost Project-relative ownership",
        )
    })?;
    let normalized = relative
        .to_str()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "artifact path is not valid UTF-8",
            )
        })?
        .replace(std::path::MAIN_SEPARATOR, "/");
    if normalized.is_empty()
        || normalized.len() > uniterm_core::ARTIFACT_PATH_MAX_BYTES
        || normalized.chars().any(char::is_control)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact path is not bounded safe UTF-8 display data",
        ));
    }
    let mut digest = sha2::Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        if size > MAX_ARTIFACT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("artifact exceeds {MAX_ARTIFACT_BYTES} bytes while reading"),
            ));
        }
        digest.update(&buffer[..read]);
    }
    if size == 0 {
        return Ok(None);
    }
    Ok(Some(uniterm_proto::ArtifactObservation {
        kind: claim.kind,
        path: normalized,
        digest: format!("{:x}", digest.finalize()),
        size,
    }))
}

pub(super) struct ProjectWatcher {
    watcher: notify::RecommendedWatcher,
    watched: HashSet<PathBuf>,
}

pub(super) struct ArtifactProjectWatcher {
    _watcher: notify::RecommendedWatcher,
    root: PathBuf,
    artifacts: HashSet<uniterm_core::ArtifactId>,
}

pub(super) fn set_artifact_watches(
    watchers: &mut HashMap<ProjectId, ArtifactProjectWatcher>,
    projects: Vec<uniterm_proto::ArtifactWatchProject>,
    tx: Sender<AgentToCore>,
    waker: Arc<Waker>,
) {
    let mut previous_watchers = std::mem::take(watchers);
    let mut next_watchers = HashMap::new();
    let mut reobserve = HashSet::new();
    for project in projects {
        let Ok(root) = std::fs::canonicalize(&project.root) else {
            continue;
        };
        let previous = previous_watchers.remove(&project.project);
        let mut exact: HashMap<PathBuf, uniterm_core::ArtifactId> = HashMap::new();
        let mut parents: HashMap<PathBuf, Vec<uniterm_core::ArtifactId>> = HashMap::new();
        for artifact in project
            .artifacts
            .into_iter()
            .take(uniterm_core::ARTIFACT_LEDGER_CAP)
        {
            let path = root.join(&artifact.path);
            if !path.starts_with(&root) {
                continue;
            }
            let Some(parent) = path.parent() else {
                continue;
            };
            let parent = parent.to_path_buf();
            exact.insert(path, artifact.artifact);
            parents.entry(parent).or_default().push(artifact.artifact);
        }
        if exact.is_empty() {
            continue;
        }
        let artifact_ids: HashSet<_> = exact.values().copied().collect();
        let event_exact_paths: Vec<PathBuf> = exact.keys().cloned().collect();
        let event_exact = exact;
        let event_parents = parents.clone();
        let event_tx = tx.clone();
        let event_waker = waker.clone();
        let Ok(mut watcher) =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else {
                    return;
                };
                let mut changed = HashSet::new();
                for path in event.paths {
                    if let Some(artifact) = event_exact.get(&path) {
                        changed.insert(*artifact);
                    }
                    if let Some(artifacts) = event_parents.get(&path) {
                        changed.extend(artifacts.iter().copied());
                    }
                }
                if changed.is_empty() {
                    return;
                }
                let mut artifacts: Vec<_> = changed.into_iter().collect();
                artifacts.sort();
                if event_tx
                    .send(AgentToCore::ArtifactFilesChanged { artifacts })
                    .is_ok()
                {
                    let _ = event_waker.wake();
                }
            })
        else {
            if let Some(previous) = previous.filter(|previous| previous.root == root) {
                next_watchers.insert(project.project, previous);
            }
            continue;
        };
        let mut watched = 0usize;
        for parent in parents.keys() {
            if watcher
                .watch(parent, notify::RecursiveMode::NonRecursive)
                .is_ok()
            {
                watched += 1;
            }
        }
        // A directory watch reports entries appearing, vanishing, or being
        // renamed, which covers atomic replacement and deletion. An in-place
        // rewrite only changes the file's own vnode, and kqueue does not
        // raise that on the parent the way inotify does, so each existing
        // Artifact file is watched by name as well. Every ledger change
        // rebuilds these watches, so a replaced file's new inode is picked
        // up on the re-observation its parent watch triggers.
        for path in event_exact_paths.iter().filter(|path| path.is_file()) {
            if watcher
                .watch(path, notify::RecursiveMode::NonRecursive)
                .is_ok()
            {
                watched += 1;
            }
        }
        if watched > 0 {
            match previous.as_ref() {
                Some(previous) if previous.root == root => {
                    reobserve.extend(artifact_ids.difference(&previous.artifacts).copied());
                }
                _ => reobserve.extend(artifact_ids.iter().copied()),
            }
            next_watchers.insert(
                project.project,
                ArtifactProjectWatcher {
                    _watcher: watcher,
                    root,
                    artifacts: artifact_ids,
                },
            );
        } else if let Some(previous) = previous.filter(|previous| previous.root == root) {
            next_watchers.insert(project.project, previous);
        }
    }
    *watchers = next_watchers;
    if !reobserve.is_empty() {
        let mut artifacts: Vec<_> = reobserve.into_iter().collect();
        artifacts.sort();
        if tx
            .send(AgentToCore::ArtifactFilesChanged { artifacts })
            .is_ok()
        {
            let _ = waker.wake();
        }
    }
}

pub(super) fn set_project_watches(
    watchers: &mut HashMap<ProjectId, ProjectWatcher>,
    project: ProjectId,
    root: &str,
    directories: &[String],
    tx: Sender<AgentToCore>,
    waker: Arc<Waker>,
) {
    if directories.is_empty() {
        watchers.remove(&project);
        return;
    }
    let Ok(root) = std::fs::canonicalize(root) else {
        watchers.remove(&project);
        return;
    };
    let wanted: HashSet<PathBuf> = directories
        .iter()
        .take(MAX_PROJECT_WATCHES)
        .filter_map(|directory| safe_existing_directory(&root, directory).ok())
        .collect();
    if wanted.is_empty() {
        watchers.remove(&project);
        return;
    }
    if let std::collections::hash_map::Entry::Vacant(entry) = watchers.entry(project) {
        let event_root = root.clone();
        let Ok(watcher) =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else {
                    return;
                };
                let mut directories = HashSet::new();
                for path in event.paths {
                    let directory = if path.is_dir() {
                        path
                    } else {
                        path.parent().unwrap_or(&event_root).to_path_buf()
                    };
                    if directory.starts_with(&event_root) {
                        directories.insert(directory.to_string_lossy().into_owned());
                    }
                }
                for directory in directories {
                    if tx
                        .send(AgentToCore::FileChanged { project, directory })
                        .is_ok()
                    {
                        let _ = waker.wake();
                    }
                }
            })
        else {
            return;
        };
        entry.insert(ProjectWatcher {
            watcher,
            watched: HashSet::new(),
        });
    }
    let Some(state) = watchers.get_mut(&project) else {
        return;
    };
    let removed: Vec<PathBuf> = state.watched.difference(&wanted).cloned().collect();
    for directory in &removed {
        let _ = state.watcher.unwatch(directory);
    }
    let mut watched: HashSet<PathBuf> = state.watched.intersection(&wanted).cloned().collect();
    let added: Vec<PathBuf> = wanted.difference(&watched).cloned().collect();
    for directory in &added {
        if state
            .watcher
            .watch(directory, notify::RecursiveMode::NonRecursive)
            .is_ok()
        {
            watched.insert(directory.clone());
        }
    }
    state.watched = watched;
}

pub(super) fn list_project_directory(
    root: &str,
    directory: &str,
) -> std::io::Result<(Vec<FileEntry>, bool)> {
    let root = std::fs::canonicalize(root)?;
    let directory = safe_existing_directory(&root, directory)?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in std::fs::read_dir(directory)? {
        if entries.len() == FILE_LISTING_LIMIT {
            truncated = true;
            break;
        }
        let item = item?;
        let path = item.path();
        let file_type = item.file_type()?;
        let metadata = item.metadata().ok();
        entries.push(FileEntry {
            name: item.file_name().to_string_lossy().into_owned(),
            path: path.to_string_lossy().into_owned(),
            is_dir: file_type.is_dir(),
            is_symlink: file_type.is_symlink(),
            size: metadata.map_or(0, |metadata| metadata.len()),
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok((entries, truncated))
}

fn safe_existing_directory(root: &Path, directory: &str) -> std::io::Result<PathBuf> {
    let requested = Path::new(directory);
    let requested = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let canonical = std::fs::canonicalize(requested)?;
    if canonical.starts_with(root) && canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "path is outside the Project root",
        ))
    }
}

fn safe_entry_path(root: &Path, value: &str) -> std::io::Result<PathBuf> {
    let requested = Path::new(value);
    let requested = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let parent = requested.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let parent = std::fs::canonicalize(parent)?;
    if parent.starts_with(root) {
        Ok(parent.join(requested.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
        })?))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "path is outside the Project root",
        ))
    }
}

fn validate_file_name(name: &str) -> std::io::Result<&str> {
    let name = name.trim();
    if name.is_empty()
        || matches!(name, "." | "..")
        || Path::new(name).components().count() != 1
        || name.contains('/')
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "enter one file or folder name",
        ));
    }
    Ok(name)
}

pub(super) fn operation_parent(operation: &FileOperation) -> String {
    match operation {
        FileOperation::CreateFile { parent, .. }
        | FileOperation::CreateDirectory { parent, .. } => parent.clone(),
        FileOperation::Rename { path, .. } | FileOperation::Delete { path } => Path::new(path)
            .parent()
            .unwrap_or_else(|| Path::new(path))
            .to_string_lossy()
            .into_owned(),
    }
}

pub(super) fn mutate_project_file(root: &str, operation: FileOperation) -> std::io::Result<()> {
    let root = std::fs::canonicalize(root)?;
    match operation {
        FileOperation::CreateFile { parent, name } => {
            let parent = safe_existing_directory(&root, &parent)?;
            let name = validate_file_name(&name)?;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(parent.join(name))?;
        }
        FileOperation::CreateDirectory { parent, name } => {
            let parent = safe_existing_directory(&root, &parent)?;
            std::fs::create_dir(parent.join(validate_file_name(&name)?))?;
        }
        FileOperation::Rename { path, name } => {
            let source = safe_entry_path(&root, &path)?;
            if source == root {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the Project root cannot be renamed",
                ));
            }
            let target = source
                .parent()
                .unwrap_or(&root)
                .join(validate_file_name(&name)?);
            if target.try_exists()? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "a file or folder with that name already exists",
                ));
            }
            std::fs::rename(source, target)?;
        }
        FileOperation::Delete { path } => {
            let target = safe_entry_path(&root, &path)?;
            if target == root {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the Project root cannot be deleted",
                ));
            }
            let metadata = std::fs::symlink_metadata(&target)?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                std::fs::remove_dir_all(target)?;
            } else {
                std::fs::remove_file(target)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod file_tests {
    use super::*;

    #[test]
    fn file_operations_stay_inside_the_project_root() {
        let root =
            std::env::temp_dir().join(format!("uniterm-file-manager-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root_text = root.to_string_lossy().into_owned();

        mutate_project_file(
            &root_text,
            FileOperation::CreateDirectory {
                parent: root_text.clone(),
                name: "src".into(),
            },
        )
        .unwrap();
        mutate_project_file(
            &root_text,
            FileOperation::CreateFile {
                parent: root.join("src").to_string_lossy().into_owned(),
                name: "main.rs".into(),
            },
        )
        .unwrap();
        let (entries, truncated) = list_project_directory(&root_text, &root_text).unwrap();
        assert!(!truncated);
        assert_eq!(entries[0].name, "src");
        assert!(entries[0].is_dir);

        let outside = root.with_extension("outside");
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        let error = mutate_project_file(
            &root_text,
            FileOperation::CreateFile {
                parent: outside.to_string_lossy().into_owned(),
                name: "escape".into(),
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }
}
