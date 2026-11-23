//! Opt-in host palette observation. Only Linux builds contain a native watcher.
//! No polling fallback, periodic retry, or work on the mio loop.

use super::*;

#[derive(Default)]
pub(super) struct Service {
    #[cfg(target_os = "linux")]
    host: Option<std::path::PathBuf>,
    #[cfg(target_os = "linux")]
    task: Option<tokio::task::JoinHandle<()>>,
    #[cfg(target_os = "linux")]
    serial: Option<Arc<tokio::sync::Semaphore>>,
}

impl Service {
    pub(super) async fn discover() -> Self {
        #[cfg(target_os = "linux")]
        {
            let host = tokio::task::spawn_blocking(linux::detect_host)
                .await
                .ok()
                .flatten();
            Self {
                host,
                task: None,
                serial: None,
            }
        }
        #[cfg(not(target_os = "linux"))]
        Self::default()
    }

    pub(super) fn available(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.host.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    pub(super) fn configure(
        &mut self,
        enabled: bool,
        generation: u64,
        tx: Sender<AgentToCore>,
        waker: Arc<Waker>,
    ) {
        #[cfg(target_os = "linux")]
        {
            if let Some(task) = self.task.take() {
                task.abort();
            }
            if enabled {
                if let Some(root) = self.host.clone() {
                    let serial = self
                        .serial
                        .get_or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
                        .clone();
                    self.task = Some(tokio::spawn(linux::observe(
                        root, generation, tx, waker, serial,
                    )));
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (enabled, generation, tx, waker);
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use notify::{Event, EventKind, RecursiveMode, Watcher};
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use uniterm_core::Theme;

    // Field order is deliberate: cancellation must destroy the native watcher
    // before releasing the permit to the next observer, including when a
    // detached blocking task owns the returned value.
    struct LeasedWatcher {
        watcher: notify::INotifyWatcher,
        _permit: Arc<tokio::sync::OwnedSemaphorePermit>,
    }

    #[derive(Default)]
    pub(super) struct EventSignal {
        epoch: AtomicU64,
        failed: AtomicBool,
    }

    impl EventSignal {
        pub(super) fn signal(&self, tx: &tokio::sync::mpsc::Sender<()>, failed: bool) {
            if failed {
                self.failed.store(true, Ordering::SeqCst);
            }
            self.epoch.fetch_add(1, Ordering::SeqCst);
            // The pending wake is sufficient, but fatal state cannot be lost
            // when this capacity-one channel has already coalesced an event.
            let _ = tx.try_send(());
        }

        pub(super) fn failure(&self) -> Option<String> {
            self.failed
                .load(Ordering::SeqCst)
                .then(|| "Omarchy native theme watcher failed".into())
        }
    }

    const MAX_BYTES: u64 = 64 * 1024;
    const DEBOUNCE: Duration = Duration::from_millis(75);

    fn read_bounded(path: &Path) -> std::io::Result<String> {
        // Reject devices/FIFOs before reading, without blocking on their open.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("theme source is not a regular file"));
        }
        let mut text = String::new();
        file.take(MAX_BYTES + 1).read_to_string(&mut text)?;
        if text.len() as u64 > MAX_BYTES {
            return Err(std::io::Error::other("theme source exceeds 64 KiB"));
        }
        Ok(text)
    }

    fn is_omarchy(text: &str) -> bool {
        let mut ids = text
            .lines()
            .filter_map(|line| line.trim().split_once('='))
            .filter(|(key, _)| *key == "ID")
            .map(|(_, value)| value.trim());
        matches!(ids.next(), Some("omarchy" | "\"omarchy\"" | "'omarchy'")) && ids.next().is_none()
    }

    pub(super) fn detect_host() -> Option<PathBuf> {
        // os-release may be a distribution-owned symlink. Its bounded read is
        // separate from untrusted theme file reads, which refuse symlinks.
        let path = std::fs::canonicalize("/etc/os-release").ok()?;
        if !is_omarchy(&read_bounded(&path).ok()?) {
            return None;
        }
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let current = home.join(".local/state/omarchy/current");
        let legacy = home.join(".config/omarchy/current");
        Some(if !current.exists() && legacy.is_dir() {
            legacy
        } else {
            current
        })
    }

    fn relevant(root: &Path, event: &Event) -> bool {
        if matches!(event.kind, EventKind::Access(_)) {
            return false;
        }
        event.need_rescan()
            || event.paths.iter().any(|path| {
                path == root
                    || path == &root.join("theme")
                    || path == &root.join("theme.name")
                    || path == &root.join("theme/colors.toml")
            })
    }

    fn identity(path: &Path) -> std::io::Result<(u64, u64)> {
        let metadata = path.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }

    pub(super) fn watch_directory(
        path: &Path,
        mut watch: impl FnMut(&Path) -> notify::Result<()>,
    ) -> Result<Option<(u64, u64)>, String> {
        // Capture identity before registration. A concurrent replacement then
        // forces rearming instead of labelling an old watch as new.
        let before = identity(path).ok();
        if before.is_some() {
            if let Err(error) = watch(path) {
                // Replacement can remove the child between stat and watch.
                // Keep the already-registered parent alive so creation rearms it.
                let missing = matches!(&error.kind, notify::ErrorKind::PathNotFound)
                    || matches!(&error.kind, notify::ErrorKind::Io(error) if error.kind() == std::io::ErrorKind::NotFound);
                return if missing {
                    Ok(None)
                } else {
                    Err(error.to_string())
                };
            }
        }
        Ok(before)
    }

    fn load(root: &Path) -> Result<Theme, String> {
        let theme_dir = root.join("theme");
        let before = identity(&theme_dir).map_err(|e| e.to_string())?;
        let text = read_bounded(&theme_dir.join("colors.toml")).map_err(|e| e.to_string())?;
        if identity(&theme_dir).map_err(|e| e.to_string())? != before {
            return Err("Omarchy theme changed while being read".into());
        }
        Theme::from_omarchy_colors(&text)
    }

    fn send(
        tx: &Sender<AgentToCore>,
        waker: &Waker,
        generation: u64,
        result: Result<Theme, String>,
    ) {
        let (theme, error) = match result {
            Ok(theme) => (Some(theme), None),
            Err(error) => (None, Some(error)),
        };
        if tx
            .send(AgentToCore::OmarchyTheme {
                generation,
                theme,
                error,
            })
            .is_ok()
        {
            let _ = waker.wake();
        }
    }

    pub(super) async fn observe(
        root: PathBuf,
        generation: u64,
        tx: Sender<AgentToCore>,
        waker: Arc<Waker>,
        serial: Arc<tokio::sync::Semaphore>,
    ) {
        let Ok(permit) = serial.acquire_owned().await else {
            return;
        };
        let permit = Arc::new(permit);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(1);
        let signal = Arc::new(EventSignal::default());
        let callback_signal = signal.clone();
        let setup_permit = permit.clone();
        let callback_root = root.clone();
        let setup = tokio::task::spawn_blocking(move || {
            let event_root = callback_root.clone();
            let callback_permit = setup_permit.clone();
            let watcher = notify::INotifyWatcher::new(
                move |event: notify::Result<Event>| {
                    // notify's Drop only signals shutdown. Its callback owns
                    // the lease until the native thread has actually exited.
                    let _permit = &callback_permit;
                    if event
                        .as_ref()
                        .map_or(true, |event| relevant(&event_root, event))
                    {
                        callback_signal.signal(&events_tx, event.is_err());
                    }
                },
                notify::Config::default(),
            )
            .map_err(|e| e.to_string())?;
            let mut watcher = LeasedWatcher {
                watcher,
                _permit: setup_permit,
            };
            watcher
                .watcher
                .watch(&callback_root, RecursiveMode::NonRecursive)
                .map_err(|e| e.to_string())?;
            let theme_dir = callback_root.join("theme");
            let theme_identity = watch_directory(&theme_dir, |path| {
                watcher.watcher.watch(path, RecursiveMode::NonRecursive)
            })?;
            Ok::<_, String>((watcher, theme_identity))
        })
        .await;
        let (mut watcher, mut watched_identity) = match setup {
            Ok(Ok(watcher)) => watcher,
            other => {
                send(
                    &tx,
                    &waker,
                    generation,
                    Err(match other {
                        Ok(Err(e)) => e,
                        Err(e) => e.to_string(),
                        _ => unreachable!(),
                    }),
                );
                return;
            }
        };
        let mut last: Option<Result<Theme, String>> = None;
        loop {
            if let Some(error) = signal.failure() {
                send(&tx, &waker, generation, Err(error));
                return;
            }
            let observed_epoch = signal.epoch.load(Ordering::SeqCst);
            let read_root = root.clone();
            // Rearm a replaced directory before reading it. Both this and the
            // bounded read run on the runtime's blocking pool, never mio.
            let observation = tokio::task::spawn_blocking(move || {
                let directory = read_root.join("theme");
                let current = identity(&directory).ok();
                let result = if current != watched_identity {
                    let _ = watcher.watcher.unwatch(&directory);
                    watched_identity = None;
                    match watcher
                        .watcher
                        .watch(&directory, RecursiveMode::NonRecursive)
                    {
                        Ok(()) => {
                            watched_identity = current;
                            load(&read_root)
                        }
                        Err(error) => Err(error.to_string()),
                    }
                } else {
                    load(&read_root)
                };
                (watcher, watched_identity, result)
            })
            .await;
            let result;
            match observation {
                Ok((next_watcher, next_identity, next_result)) => {
                    watcher = next_watcher;
                    watched_identity = next_identity;
                    result = next_result;
                }
                Err(error) => {
                    send(&tx, &waker, generation, Err(error.to_string()));
                    return;
                }
            }
            if let Some(error) = signal.failure() {
                send(&tx, &waker, generation, Err(error));
                return;
            }
            if signal.epoch.load(Ordering::SeqCst) == observed_epoch
                && last.as_ref() != Some(&result)
            {
                send(&tx, &waker, generation, result.clone());
                last = Some(result);
            }
            let Some(()) = events_rx.recv().await else {
                return;
            };
            if let Some(error) = signal.failure() {
                send(&tx, &waker, generation, Err(error));
                return;
            }
            // This one-shot delay exists only after an actual filesystem event.
            tokio::time::sleep(DEBOUNCE).await;
            while events_rx.try_recv().is_ok() {}
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn eligibility_requires_exact_identity_and_ignores_access() {
            for text in [
                "ID=arch\nID_LIKE=omarchy",
                "ID=ubuntu",
                "ID=omarchy\nID=arch",
                "ID_LIKE=arch",
                "ID=omarchy-extra",
            ] {
                assert!(!is_omarchy(text));
            }
            for text in ["ID=omarchy\nID_LIKE=arch", "ID=\"omarchy\"", "ID='omarchy'"] {
                assert!(is_omarchy(text));
            }
            let root = Path::new("/tmp/current");
            let event = Event::new(EventKind::Access(notify::event::AccessKind::Any))
                .add_path(root.join("theme/colors.toml"));
            assert!(!relevant(root, &event));
            let event = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                .add_path(root.join("background"));
            assert!(!relevant(root, &event));
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "omarchy_tests.rs"]
mod tests;
