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

pub(super) struct Root {
    pub provider: String,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    watcher: notify::RecommendedWatcher,
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
        Ok(Self { provider, watcher })
    }

    pub fn paths(&mut self, root: &Path, changes: Changes) -> notify::Result<Vec<PathBuf>> {
        if changes.failed {
            return Err(notify::Error::generic("session filesystem watch failed"));
        }
        let paths = if changes.rescan {
            vec![root.to_owned()]
        } else {
            changes.paths.into_iter().collect()
        };
        let mut files = Vec::new();
        let mut directories = Vec::new();
        for path in paths {
            if path.is_dir() {
                directories.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
        // Register before reading: files created before inotify installed a
        // new directory's watch are found here, later writes arrive as events.
        let mut visited = HashSet::new();
        while let Some(directory) = directories.pop() {
            if !visited.insert(directory.clone()) {
                continue;
            }
            #[cfg(not(target_os = "macos"))]
            if let Err(error) = self
                .watcher
                .watch(&directory, notify::RecursiveMode::Recursive)
            {
                if vanished(&error) {
                    continue;
                }
                return Err(error);
            }
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(notify::Error::io(error)),
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(notify::Error::io(error)),
                };
                let path = entry.path();
                // Never follow a symlink outside this provider's root.
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    directories.push(path);
                } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                    files.push(path);
                }
            }
        }
        Ok(files)
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
