//! Project-scoped filesystem operations, bounded artifact observation, and
//! event-driven watches. All filesystem ownership checks live with the work.

use crossbeam_channel::Sender;
use mio::Waker;
use notify::Watcher as _;
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::{DirEntryExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniterm_proto::{AgentToCore, FileEntry, FileOperation, ProjectId, FILE_LISTING_LIMIT};

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
    namespaces: Arc<std::sync::Mutex<HashMap<PathBuf, Option<u64>>>>,
}

// FSEvents may deliver a queued event for the directory just registered.
// Compare its namespace rather than invalidating on reads or timestamps.
// Child-file events still refresh sizes even when names have not changed.
fn namespace_version(path: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mut version = 0u64;
    let mut count = 0u64;
    for entry in std::fs::read_dir(path).ok()? {
        let entry = entry.ok()?;
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        entry.file_name().hash(&mut hash);
        entry.ino().hash(&mut hash);
        version = version.wrapping_add(hash.finish());
        count += 1;
    }
    Some(version.wrapping_add(count))
}

pub(super) struct ArtifactProjectWatcher {
    _watcher: notify::RecommendedWatcher,
    root: PathBuf,
    artifacts: HashSet<uniterm_core::ArtifactId>,
}

// Ignore access times and queued FSEvents for already-observed files. Identity,
// content timestamps and size still detect replacement, removal and rewrites.
#[derive(Debug, PartialEq, Eq)]
struct ArtifactFileVersion {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn artifact_file_version(path: &Path) -> Option<ArtifactFileVersion> {
    let metadata = path.metadata().ok()?;
    Some(ArtifactFileVersion {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    })
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
        let mut versions: HashMap<_, _> = event_exact_paths
            .iter()
            .map(|path| (path.clone(), artifact_file_version(path)))
            .collect();
        let event_exact = exact;
        let event_parents = parents.clone();
        let event_tx = tx.clone();
        let event_waker = waker.clone();
        let Ok(mut watcher) =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else {
                    return;
                };
                if matches!(event.kind, notify::EventKind::Access(_)) {
                    return;
                }
                let mut changed = HashSet::new();
                if event.need_rescan() {
                    changed.extend(event_exact.values().copied());
                }
                for path in &event.paths {
                    if let Some(artifact) = event_exact.get(path) {
                        changed.insert(*artifact);
                    }
                    if let Some(artifacts) = event_parents.get(path) {
                        changed.extend(artifacts.iter().copied());
                    }
                }
                if changed.is_empty() {
                    return;
                }
                let mut artifacts = Vec::new();
                for (path, artifact) in &event_exact {
                    if !changed.contains(artifact) {
                        continue;
                    }
                    let next = artifact_file_version(path);
                    if versions.get(path) != Some(&next) {
                        versions.insert(path.clone(), next);
                        artifacts.push(*artifact);
                    }
                }
                if artifacts.is_empty() {
                    return;
                }
                artifacts.sort();
                artifacts.dedup();
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
        // Both inotify and FSEvents report child-file changes on a directory
        // watch, including in-place writes and atomic replacement. Watching
        // every file as well is unnecessary and duplicates native events.
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
        .filter_map(|directory| safe_existing_directory(&root, directory).ok())
        .collect();
    if wanted.is_empty() {
        watchers.remove(&project);
        return;
    }
    if let std::collections::hash_map::Entry::Vacant(entry) = watchers.entry(project) {
        let event_root = root.clone();
        let namespaces = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let event_namespaces = Arc::clone(&namespaces);
        let Ok(watcher) =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else {
                    return;
                };
                // Listing a directory opens it. Read/access notifications
                // must not invalidate that same listing and loop forever.
                // Metadata-only changes do not affect the displayed entries.
                if matches!(
                    event.kind,
                    notify::EventKind::Access(_)
                        | notify::EventKind::Modify(notify::event::ModifyKind::Metadata(_))
                ) {
                    return;
                }
                let mut directories = HashSet::new();
                for path in event.paths {
                    let Ok(mut namespaces) = event_namespaces.lock() else {
                        return;
                    };
                    if namespaces.contains_key(&path) {
                        let version = namespace_version(&path);
                        if namespaces.insert(path.clone(), version) != Some(version) {
                            directories.insert(path.to_string_lossy().into_owned());
                        } else if !matches!(
                            event.kind,
                            notify::EventKind::Create(_)
                                | notify::EventKind::Remove(_)
                                | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                        ) {
                            // A queued unchanged directory event must not
                            // invalidate its expanded parent either.
                            continue;
                        }
                    }
                    if let Some(parent) = path
                        .parent()
                        .filter(|parent| parent.starts_with(&event_root))
                    {
                        if namespaces.contains_key(parent) {
                            directories.insert(parent.to_string_lossy().into_owned());
                        }
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
            namespaces,
        });
    }
    let Some(state) = watchers.get_mut(&project) else {
        return;
    };
    let removed: Vec<PathBuf> = state.watched.difference(&wanted).cloned().collect();
    for directory in &removed {
        let _ = state.watcher.unwatch(directory);
        if let Ok(mut namespaces) = state.namespaces.lock() {
            namespaces.remove(directory);
        }
    }
    let mut watched: HashSet<PathBuf> = state.watched.intersection(&wanted).cloned().collect();
    let added: Vec<PathBuf> = wanted.difference(&watched).cloned().collect();
    for directory in &added {
        if let Ok(mut namespaces) = state.namespaces.lock() {
            namespaces.insert(directory.clone(), namespace_version(directory));
        }
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
    fn artifact_versions_ignore_reads_and_detect_rewrites_replacement_and_removal() {
        let root =
            std::env::temp_dir().join(format!("uniterm-artifact-version-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("report.md");
        assert_eq!(artifact_file_version(&path), None);
        std::fs::write(&path, b"ready").unwrap();
        let initial = artifact_file_version(&path);
        std::fs::read(&path).unwrap();
        assert_eq!(artifact_file_version(&path), initial);
        std::fs::write(&path, b"updated report").unwrap();
        let rewritten = artifact_file_version(&path);
        assert_ne!(rewritten, initial);
        std::fs::write(root.join("replacement"), b"updated report").unwrap();
        std::fs::rename(root.join("replacement"), &path).unwrap();
        assert_ne!(artifact_file_version(&path), rewritten);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(artifact_file_version(&path), None);
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn namespace_versions_ignore_reads_and_detect_folder_rename_and_removal() {
        let root = std::env::temp_dir().join(format!("uniterm-namespace-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let empty = namespace_version(&root);
        assert_eq!(namespace_version(&root), empty);
        std::fs::create_dir(root.join("folder")).unwrap();
        let created = namespace_version(&root);
        assert_ne!(created, empty);
        std::fs::rename(root.join("folder"), root.join("renamed")).unwrap();
        assert_ne!(namespace_version(&root), created);
        std::fs::remove_dir(root.join("renamed")).unwrap();
        assert_eq!(namespace_version(&root), empty);
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn expanded_directories_have_no_silent_watch_count_cutoff() {
        let root =
            std::env::temp_dir().join(format!("uniterm-many-folders-{}", std::process::id()));
        let directories: Vec<String> = (0..270)
            .map(|id| {
                let path = root.join(id.to_string());
                std::fs::create_dir_all(&path).unwrap();
                path.to_string_lossy().into_owned()
            })
            .collect();
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), mio::Token(91)).unwrap());
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut watchers = HashMap::new();
        set_project_watches(
            &mut watchers,
            ProjectId(1),
            &root.to_string_lossy(),
            &directories,
            tx,
            waker,
        );
        assert_eq!(watchers[&ProjectId(1)].watched.len(), 270);
        drop(watchers);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_reads_do_not_trigger_refresh_but_file_changes_do() {
        let root = std::env::temp_dir().join(format!("uniterm-watch-reads-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), mio::Token(91)).unwrap());
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut watchers = HashMap::new();
        let root_text = root.to_string_lossy().into_owned();
        set_project_watches(
            &mut watchers,
            ProjectId(1),
            &root_text,
            std::slice::from_ref(&root_text),
            tx,
            waker,
        );
        assert_eq!(watchers.len(), 1);
        for _ in 0..3 {
            list_project_directory(&root_text, &root_text).unwrap();
        }
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(250))
                .is_err(),
            "directory reads invalidated their own listing"
        );
        std::fs::write(root.join("created.txt"), "changed").unwrap();
        let event = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(matches!(
            event,
            AgentToCore::FileChanged {
                project: ProjectId(1),
                ..
            }
        ));
        drop(watchers);
        std::fs::remove_dir_all(root).unwrap();
    }

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
