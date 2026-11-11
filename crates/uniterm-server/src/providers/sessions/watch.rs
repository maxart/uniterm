//! Transactional native watches and lossless, coalesced event intake.
//! A failed registration drops its entire candidate, including partial OS state.

use crossbeam_channel::Sender;
use notify::Watcher as _;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(super) struct Changes {
    paths: HashSet<PathBuf>,
    rescan: bool,
    failed: bool,
}

impl Changes {
    pub fn queue_path(&mut self, path: PathBuf) {
        if self.paths.len() >= 1024 && !self.paths.contains(&path) {
            self.paths.clear();
            self.rescan = true;
        }
        self.paths.insert(path);
    }

    pub fn request_rescan(&mut self) {
        self.rescan = true;
    }

    fn record(&mut self, event: notify::Result<notify::Event>) {
        let event = match event {
            Ok(event) => event,
            Err(error) if vanished(&error) => return,
            Err(_) => {
                self.failed = true;
                self.rescan = true;
                return;
            }
        };
        if event.need_rescan() {
            self.rescan = true;
        }
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        for path in event.paths {
            if self.paths.len() >= 1024 {
                // The bound is on retained event detail, never on discovery.
                self.rescan = true;
                self.paths.clear();
            }
            self.paths.insert(path);
        }
    }
}

pub(super) type Pending = Arc<Mutex<HashMap<PathBuf, Changes>>>;
type ChangedFiles = Vec<(PathBuf, bool)>;

pub(super) struct Root {
    pub provider: String,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    watcher: notify::RecommendedWatcher,
    queued: HashSet<PathBuf>,
    directories: Vec<std::fs::ReadDir>,
    rescan: bool,
}

// Keep construction transactional even for backends which register half a
// subtree before reporting ENOSPC/EMFILE. Tested with an injected failure.
fn registered<W>(
    mut candidate: W,
    register: impl FnOnce(&mut W) -> notify::Result<()>,
) -> notify::Result<W> {
    register(&mut candidate)?;
    Ok(candidate)
}

impl Root {
    pub fn new(
        path: &Path,
        provider: String,
        pending: Pending,
        changed: Sender<()>,
    ) -> notify::Result<Self> {
        let root = path.to_owned();
        let candidate = notify::RecommendedWatcher::new(
            move |event: notify::Result<notify::Event>| {
                if matches!(&event, Ok(event) if matches!(event.kind, notify::EventKind::Access(_)))
                {
                    return;
                }
                if let Ok(mut pending) = pending.lock() {
                    pending.entry(root.clone()).or_default().record(event);
                }
                let _ = changed.try_send(());
            },
            notify::Config::default().with_follow_symlinks(false),
        )?;
        let watcher = registered(candidate, |watcher| {
            watcher.watch(path, notify::RecursiveMode::Recursive)
        })?;
        Ok(Self {
            provider,
            watcher,
            queued: HashSet::new(),
            directories: Vec::new(),
            rescan: false,
        })
    }

    /// Consume a bounded amount of real filesystem work. Continuations are
    /// scheduled by the worker only while this traversal has entries left.
    pub fn paths(&mut self, root: &Path, changes: Changes) -> notify::Result<(ChangedFiles, bool)> {
        if changes.failed {
            return Err(notify::Error::generic("session filesystem watch failed"));
        }
        self.rescan |= changes.rescan;
        for path in changes.paths {
            if self.queued.len() >= 1024 {
                self.queued.clear();
                self.rescan = true;
            }
            self.queued.insert(path);
        }
        let mut files = Vec::new();
        // Keep directory iterators, not all filenames or all directories.
        // Memory is O(nesting depth), plus at most 1024 queued event paths.
        for _ in 0..128 {
            let mut from_scan = false;
            let path = if let Some(path) = self.queued.iter().next().cloned() {
                self.queued.remove(&path);
                path
            } else if let Some(entries) = self.directories.last_mut() {
                from_scan = true;
                match entries.next() {
                    Some(Ok(entry)) => entry.path(),
                    Some(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Some(Err(error)) => return Err(notify::Error::io(error)),
                    None => {
                        self.directories.pop();
                        continue;
                    }
                }
            } else if std::mem::take(&mut self.rescan) {
                root.to_owned()
            } else {
                break;
            };
            if !path.starts_with(root) {
                continue;
            }
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(notify::Error::io(error)),
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                if !from_scan && !self.directories.is_empty() {
                    // An unrelated event directory must not grow the active
                    // traversal stack by breadth. Revisit it in the next pass.
                    self.rescan = true;
                    continue;
                }
                // Watch before reading to cover children created during setup.
                #[cfg(not(target_os = "macos"))]
                if let Err(error) = self
                    .watcher
                    .watch(&path, notify::RecursiveMode::NonRecursive)
                {
                    if vanished(&error) {
                        continue;
                    }
                    return Err(error);
                }
                match std::fs::read_dir(&path) {
                    Ok(entries) => self.directories.push(entries),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(notify::Error::io(error)),
                }
            } else if metadata.is_file() {
                if let Some(transcript) = super::super::session_event_path(&self.provider, &path) {
                    let metadata_changed = transcript != path;
                    files.push((transcript, metadata_changed));
                }
            }
        }
        let more = self.rescan || !self.queued.is_empty() || !self.directories.is_empty();
        Ok((files, more))
    }
}

fn vanished(error: &notify::Error) -> bool {
    matches!(&error.kind, notify::ErrorKind::PathNotFound)
        || matches!(&error.kind, notify::ErrorKind::Io(error) if error.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_partial_registration_drops_candidate() {
        struct Candidate(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Candidate {
            fn drop(&mut self) {
                self.0.store(0, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let resources = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let result = registered(Candidate(resources.clone()), |candidate| {
            candidate.0.store(100, std::sync::atomic::Ordering::SeqCst);
            Err(notify::Error::generic(
                "injected partial registration failure",
            ))
        });
        assert!(result.is_err());
        assert_eq!(resources.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn overflow_and_kernel_loss_request_reconciliation() {
        let mut changes = Changes::default();
        for id in 0..2048 {
            changes
                .record(Ok(notify::Event::new(notify::EventKind::Any)
                    .add_path(format!("{id}.jsonl").into())));
        }
        assert!(changes.rescan);
        assert!(changes.paths.len() <= 1024);
        let mut changes = Changes::default();
        changes.record(Ok(
            notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan)
        ));
        assert!(changes.rescan);
    }

    #[test]
    fn vanished_entries_do_not_discard_healthy_watches_but_resource_errors_do() {
        let mut changes = Changes::default();
        changes.record(Err(notify::Error::path_not_found()));
        changes.record(Err(notify::Error::io(std::io::Error::from(
            std::io::ErrorKind::NotFound,
        ))));
        assert!(!changes.failed);
        changes.record(Err(notify::Error::io(std::io::Error::from_raw_os_error(
            libc::EMFILE,
        ))));
        assert!(changes.failed);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_uses_subtree_events_without_per_file_descriptors() {
        assert_eq!(
            std::any::TypeId::of::<notify::RecommendedWatcher>(),
            std::any::TypeId::of::<notify::FsEventWatcher>()
        );
    }
}
