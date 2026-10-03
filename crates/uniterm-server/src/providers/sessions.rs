//! Native identity observation uses exact process files or cooperative paths.
//! Never guess ownership from a shared working directory or newest session.

use crossbeam_channel::{bounded, Sender};
use mio::Waker;
use notify::Watcher;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniterm_proto::{AgentToCore, PaneId};

/// Identity owned by a provider's transcript format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Identity {
    pub id: String,
    pub parent: Option<String>,
}

#[derive(Clone)]
struct Target {
    pane: PaneId,
    pid: Option<i32>,
    provider: String,
}

enum Request {
    Observe(Target, Option<(String, Option<String>)>),
    Forget(PaneId),
}

/// One sleeping worker serves all native session identities in a Workspace.
/// Filesystem events observe children even while the parent PTY is silent.
pub(crate) struct Service {
    requests: Sender<()>,
    pending: Arc<std::sync::Mutex<HashMap<PaneId, Request>>>,
}

impl Service {
    pub(crate) fn start(output: Sender<AgentToCore>, waker: Arc<Waker>) -> Self {
        let (requests, incoming) = bounded(1);
        let pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let request_pending = pending.clone();
        std::thread::spawn(move || {
            let (changed, changes) = bounded(1);
            let dirty = Arc::new(std::sync::Mutex::new(HashSet::new()));
            let pending = dirty.clone();
            let mut watcher =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    if let Ok(event) = event {
                        if matches!(event.kind, notify::EventKind::Access(_)) {
                            return;
                        }
                        if let Ok(mut paths) = pending.lock() {
                            for path in event.paths {
                                if paths.len() < 1024
                                    && path.extension().is_some_and(|ext| ext == "jsonl")
                                {
                                    paths.insert(path);
                                }
                            }
                        }
                        let _ = changed.try_send(());
                    }
                })
                .ok();
            let mut state = State::default();
            loop {
                crossbeam_channel::select! {
                    recv(incoming) -> signal => {
                        if signal.is_err() { break; }
                        let requests = request_pending.lock().map(|mut requests| std::mem::take(&mut *requests)).unwrap_or_default();
                        for request in requests.into_values() {
                          match request {
                            Request::Forget(pane) => state.forget(pane),
                            Request::Observe(target, supplied) => {
                            if state.targets.get(&target.pane).is_some_and(|old| old.pid != target.pid || old.provider != target.provider)
                                || supplied.as_ref().is_some_and(|(id, _)| state.owning_sessions.get(&target.pane).is_some_and(|old| old != id)) {
                                state.forget(target.pane);
                            }
                            state.targets.insert(target.pane, target.clone());
                            let observations = if let Some((id, path)) = supplied {
                                vec![(Identity { id, parent: None }, path.map(PathBuf::from))]
                            } else if state.identified.contains(&target.pane) || !super::supports_native_session(&target.provider) {
                                Vec::new()
                            } else {
                                target.pid.map(|pid| process_sessions(&target.provider, pid)).unwrap_or_default()
                            };
                            for (identity, mut path) in observations {
                                if identity.parent.is_some() { continue; }
                                // Lifecycle hooks often repeat only the ID.
                                // Reuse a successfully published path instead
                                // of walking the provider store for every tool.
                                if path.is_none() {
                                    path = state.delivered.get(&(target.pane, target.pid,
                                        target.provider.clone(), identity.id.clone())).cloned().flatten();
                                }
                                let root = path.as_deref().and_then(|path| super::session_watch_root(&target.provider, path))
                                    .or_else(|| super::default_session_root(&target.provider, target.pid));
                                if path.is_none() {
                                    path = root.as_deref().and_then(|root| super::known_session_path(&target.provider, root, &identity.id))
                                        .filter(|path| read_identity(&target.provider, path).is_some_and(|found| found == identity));
                                }
                                let bootstrap = !state.identified.contains(&target.pane);
                                if path.is_some() { state.identified.insert(target.pane); }
                                state.owning_sessions.insert(target.pane, identity.id.clone());
                                state.publish(target.clone(), identity, path.as_deref(), &output, &waker);
                                if let Some(root) = root {
                                    state.root_users.entry(root.clone()).or_default().insert(target.pane);
                                    if let std::collections::hash_map::Entry::Vacant(entry) = state.roots.entry(root) {
                                        if watcher.as_mut().is_some_and(|watcher| watcher.watch(entry.key(), notify::RecursiveMode::Recursive).is_ok()) {
                                            entry.insert(target.provider.clone());
                                        }
                                    }
                                    if bootstrap {
                                        if let Some(path) = path.as_deref() { state.collect_directory(&target.provider, path); }
                                    }
                                }
                            }
                            state.resolve(&output, &waker);
                        }
                          }
                        }
                    },
                    recv(changes) -> _ => {
                        let paths = dirty.lock().map(|mut paths| std::mem::take(&mut *paths)).unwrap_or_default();
                        for path in paths {
                            if state.seen_paths.contains(&path) { continue; }
                            let Some(provider) = state.roots.iter().find_map(|(root, provider)| path.starts_with(root).then(|| provider.clone())) else { continue; };
                            state.observe_file(&provider, path, &output, &waker);
                        }
                        state.resolve(&output, &waker);
                    }
                }
                let unused: Vec<_> = state
                    .roots
                    .keys()
                    .filter(|root| state.root_users.get(*root).is_none_or(HashSet::is_empty))
                    .cloned()
                    .collect();
                for root in unused {
                    if let Some(watcher) = watcher.as_mut() {
                        let _ = watcher.unwatch(&root);
                    }
                    state.roots.remove(&root);
                    state.root_users.remove(&root);
                }
                if state.targets.is_empty() {
                    state.pending.clear();
                }
            }
        });
        Self { requests, pending }
    }

    pub(crate) fn observe(
        &self,
        pane: PaneId,
        pid: Option<i32>,
        provider: String,
        supplied: Option<(String, Option<String>)>,
    ) {
        self.queue(
            pane,
            Request::Observe(
                Target {
                    pane,
                    pid,
                    provider,
                },
                supplied,
            ),
        );
    }

    pub(crate) fn forget(&self, pane: PaneId) {
        self.queue(pane, Request::Forget(pane));
    }

    fn queue(&self, pane: PaneId, mut request: Request) {
        if let Ok(mut pending) = self.pending.lock() {
            // Coalesce screen evidence without losing a hook's richer identity
            // or allowing an old Observe to overtake a lifecycle Forget.
            if let Request::Observe(target, supplied @ None) = &mut request {
                if let Some(Request::Observe(old, previous)) = pending.get(&pane) {
                    if old.pid == target.pid && old.provider == target.provider {
                        *supplied = previous.clone();
                    }
                }
            }
            pending.insert(pane, request);
        }
        let _ = self.requests.try_send(());
    }
}

#[derive(Default)]
struct State {
    targets: HashMap<PaneId, Target>,
    identified: HashSet<PaneId>,
    sessions: HashMap<(String, String), Vec<Target>>,
    owning_sessions: HashMap<PaneId, String>,
    roots: HashMap<PathBuf, String>,
    root_users: HashMap<PathBuf, HashSet<PaneId>>,
    pending: HashMap<(String, String), (Identity, PathBuf)>,
    seen_paths: HashSet<PathBuf>,
    delivered: HashMap<(PaneId, Option<i32>, String, String), Option<PathBuf>>,
}

impl State {
    fn collect_directory(&mut self, provider: &str, path: &Path) {
        let Some(entries) = path.parent().and_then(|path| std::fs::read_dir(path).ok()) else {
            return;
        };
        for entry in entries.take(512).flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                if let Some(identity) = read_identity(provider, &path) {
                    if self.seen_paths.len() < 4096 {
                        self.seen_paths.insert(path.clone());
                    }
                    if identity.parent.is_some() && self.pending.len() < 512 {
                        self.pending
                            .insert((provider.into(), identity.id.clone()), (identity, path));
                    }
                }
            }
        }
    }

    fn observe_file(
        &mut self,
        provider: &str,
        path: PathBuf,
        output: &Sender<AgentToCore>,
        waker: &Waker,
    ) {
        if let Some(identity) = read_identity(provider, &path) {
            if identity.parent.is_some() && self.pending.len() < 512 {
                self.pending.insert(
                    (provider.into(), identity.id.clone()),
                    (identity, path.clone()),
                );
            } else if identity.parent.is_none() {
                if let Some(targets) = self
                    .sessions
                    .get(&(provider.into(), identity.id.clone()))
                    .cloned()
                {
                    for target in targets {
                        self.identified.insert(target.pane);
                        self.publish(target, identity.clone(), Some(&path), output, waker);
                    }
                }
            }
            // Headers are immutable. Cache even unrelated sessions so a
            // global provider watcher never rereads instructions per token.
            if self.seen_paths.len() < 4096 {
                self.seen_paths.insert(path);
            }
        }
    }

    fn forget(&mut self, pane: PaneId) {
        self.targets.remove(&pane);
        for users in self.root_users.values_mut() {
            users.remove(&pane);
        }
        self.identified.remove(&pane);
        self.owning_sessions.remove(&pane);
        self.delivered.retain(|(tracked, ..), _| *tracked != pane);
        self.sessions.retain(|_, targets| {
            targets.retain(|target| target.pane != pane);
            !targets.is_empty()
        });
        // Paths are only an optimization; discarding avoids unbounded growth
        // across repeated invocations and permits resumed sessions to re-link.
        self.seen_paths.clear();
    }

    fn publish(
        &mut self,
        target: Target,
        identity: Identity,
        path: Option<&Path>,
        output: &Sender<AgentToCore>,
        waker: &Waker,
    ) {
        let key = (target.provider.clone(), identity.id.clone());
        let recipient = (
            target.pane,
            target.pid,
            target.provider.clone(),
            identity.id.clone(),
        );
        if self
            .delivered
            .get(&recipient)
            .is_some_and(|delivered| path.is_none() || delivered.as_deref() == path)
        {
            return;
        }
        if self.sessions.len() >= 4096 {
            return;
        }
        if identity.parent.is_none() {
            self.owning_sessions
                .insert(target.pane, identity.id.clone());
        }
        let Some(root_session_id) = self.owning_sessions.get(&target.pane).cloned() else {
            return;
        };
        let message = AgentToCore::AgentSessionDetected {
            pane: target.pane,
            foreground_pid: target.pid,
            provider: target.provider.clone(),
            session_id: identity.id,
            root_session_id,
            parent_session_id: identity.parent,
            transcript_path: path.map(|path| path.to_string_lossy().into_owned()),
        };
        if output.send(message).is_ok() {
            self.delivered.insert(recipient, path.map(Path::to_owned));
            let targets = self.sessions.entry(key).or_default();
            if !targets
                .iter()
                .any(|old| old.pane == target.pane && old.pid == target.pid)
            {
                targets.push(target);
            }
            if let Some(path) = path.filter(|_| self.seen_paths.len() < 4096) {
                self.seen_paths.insert(path.to_owned());
            }
            let _ = waker.wake();
        }
    }

    fn resolve(&mut self, output: &Sender<AgentToCore>, waker: &Waker) {
        loop {
            let ready = self.pending.iter().find_map(|(key, (identity, path))| {
                let parent = identity.parent.as_ref()?;
                let target = self
                    .sessions
                    .get(&(key.0.clone(), parent.clone()))?
                    .first()?;
                Some((key.clone(), identity.clone(), path.clone(), target.clone()))
            });
            let Some((key, identity, path, target)) = ready else {
                break;
            };
            self.pending.remove(&key);
            self.publish(target, identity, Some(&path), output, waker);
        }
    }
}

fn read_identity(provider: &str, path: &Path) -> Option<Identity> {
    // The first record is sufficient for formats we support. Never read a
    // complete transcript or retain prompts in Uniterm's history.
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file.take(256 * 1024));
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    if !line.ends_with('\n') {
        return None;
    }
    super::session_identity(provider, &line)
}

#[cfg(target_os = "linux")]
fn process_sessions(provider: &str, pid: i32) -> Vec<(Identity, Option<PathBuf>)> {
    let mut found = Vec::new();
    for process in process_family(pid) {
        let Some(command) = crate::runtime::process_command(process) else {
            continue;
        };
        if !super::native_process_matches(provider, &command) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(format!("/proc/{process}/fd")) else {
            continue;
        };
        for entry in files.take(256).flatten() {
            // A tool reading another transcript is not its owner. Native
            // session writers must hold a writable descriptor themselves.
            let writable = std::fs::read_to_string(format!(
                "/proc/{process}/fdinfo/{}",
                entry.file_name().to_string_lossy()
            ))
            .ok()
            .and_then(|info| {
                info.lines().find_map(|line| {
                    line.strip_prefix("flags:")
                        .and_then(|flags| u32::from_str_radix(flags.trim(), 8).ok())
                })
            })
            .is_some_and(|flags| flags & 3 != 0);
            if !writable {
                continue;
            }
            let Ok(path) = std::fs::read_link(entry.path()) else {
                continue;
            };
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                if let Some(identity) = read_identity(provider, &path) {
                    found.push((identity, Some(path)));
                }
            }
        }
    }
    // Ambiguous simultaneous native roots are not ownership evidence.
    let mut ids = found
        .iter()
        .filter(|(identity, _)| identity.parent.is_none())
        .map(|(identity, _)| &identity.id);
    if let Some(first) = ids.next() {
        if ids.any(|id| id != first) {
            return Vec::new();
        }
    }
    found
}

#[cfg(not(target_os = "linux"))]
fn process_sessions(_provider: &str, _pid: i32) -> Vec<(Identity, Option<PathBuf>)> {
    Vec::new()
}

/// Inspect only this foreground process's descendants on real output events.
/// This is not a process-table scan, and wrappers do not hide the actual CLI.
#[cfg(target_os = "linux")]
pub(crate) fn process_family(pid: i32) -> Vec<i32> {
    let mut pids = vec![pid];
    let mut index = 0;
    while index < pids.len() && pids.len() < 32 {
        if let Ok(children) = std::fs::read_to_string(format!(
            "/proc/{}/task/{}/children",
            pids[index], pids[index]
        )) {
            for child in children
                .split_whitespace()
                .filter_map(|pid| pid.parse::<i32>().ok())
            {
                let same_group = std::fs::read_to_string(format!("/proc/{child}/stat"))
                    .ok()
                    .and_then(|stat| {
                        stat.rsplit_once(')')
                            .and_then(|(_, fields)| fields.split_whitespace().nth(2))
                            .and_then(|group| group.parse::<i32>().ok())
                    })
                    == Some(pid);
                if same_group && pids.len() < 32 && !pids.contains(&child) {
                    pids.push(child);
                }
            }
        }
        index += 1;
    }
    pids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(pane: u64) -> Target {
        Target {
            pane: PaneId(pane),
            pid: Some(pane as i32),
            provider: "codex".into(),
        }
    }

    #[test]
    fn native_watcher_recovers_existing_children_and_observes_silent_descendants() {
        let root = std::env::temp_dir().join(format!(
            "uniterm-session-watch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let directory = root.join("sessions/2026/10/03");
        std::fs::create_dir_all(&directory).unwrap();
        let header = |id: &str, parent: Option<&str>| {
            serde_json::json!({"type":"session_meta", "payload": {
                "id":id, "parent_thread_id":parent, "source":"cli",
                "base_instructions": "not retained ".repeat(3000)
            }})
            .to_string()
                + "\n"
        };
        let root_path = directory.join("rollout-root.jsonl");
        std::fs::write(&root_path, header("root", None)).unwrap();
        std::fs::write(
            directory.join("rollout-child.jsonl"),
            header("child", Some("root")),
        )
        .unwrap();
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), mio::Token(0)).unwrap());
        let (tx, rx) = bounded(16);
        let service = Service::start(tx, waker);
        service.observe(
            PaneId(1),
            Some(1),
            "codex".into(),
            Some((
                "root".into(),
                Some(root_path.to_string_lossy().into_owned()),
            )),
        );
        for expected in ["root", "child"] {
            assert!(
                matches!(rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap(), AgentToCore::AgentSessionDetected { session_id, .. } if session_id == expected)
            );
        }
        std::fs::write(
            directory.join("rollout-grandchild.jsonl"),
            header("grandchild", Some("child")),
        )
        .unwrap();
        assert!(
            matches!(rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap(), AgentToCore::AgentSessionDetected { session_id, parent_session_id: Some(parent), .. } if session_id == "grandchild" && parent == "child")
        );
        // Appending nonmetadata output neither duplicates edges nor emits work.
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&root_path)
            .unwrap()
            .write_all(b"{\"type\":\"response_item\"}\n")
            .unwrap();
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(80))
            .is_err());
        drop(service);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_native_session_can_be_observed_in_two_panes_and_reidentified_with_path() {
        let poll = mio::Poll::new().unwrap();
        let waker = Waker::new(poll.registry(), mio::Token(0)).unwrap();
        let (tx, rx) = bounded(16);
        let mut state = State::default();
        let identity = Identity {
            id: "shared".into(),
            parent: None,
        };
        state.seen_paths.insert("/tmp/shared.jsonl".into());
        state.publish(target(1), identity.clone(), None, &tx, &waker);
        state.publish(target(2), identity.clone(), None, &tx, &waker);
        state.publish(
            target(1),
            identity.clone(),
            Some(Path::new("/tmp/shared.jsonl")),
            &tx,
            &waker,
        );
        assert_eq!(rx.try_iter().count(), 3);
        state.publish(
            target(1),
            identity,
            Some(Path::new("/tmp/shared.jsonl")),
            &tx,
            &waker,
        );
        assert!(rx.try_recv().is_err());
        state.forget(PaneId(1));
        assert_eq!(state.sessions.values().next().unwrap()[0].pane, PaneId(2));
    }

    #[test]
    fn child_identity_waits_for_exact_parent_and_never_crosses_workspaces() {
        let poll = mio::Poll::new().unwrap();
        let waker = Waker::new(poll.registry(), mio::Token(0)).unwrap();
        let (tx, rx) = bounded(16);
        let mut state = State::default();
        state.pending.insert(
            ("codex".into(), "child".into()),
            (
                Identity {
                    id: "child".into(),
                    parent: Some("root".into()),
                },
                "/tmp/child.jsonl".into(),
            ),
        );
        state.pending.insert(
            ("codex".into(), "grandchild".into()),
            (
                Identity {
                    id: "grandchild".into(),
                    parent: Some("child".into()),
                },
                "/tmp/grandchild.jsonl".into(),
            ),
        );
        state.publish(
            target(2),
            Identity {
                id: "unrelated".into(),
                parent: None,
            },
            None,
            &tx,
            &waker,
        );
        rx.recv().unwrap();
        state.resolve(&tx, &waker);
        assert!(rx.try_recv().is_err());
        state.publish(
            target(1),
            Identity {
                id: "root".into(),
                parent: None,
            },
            None,
            &tx,
            &waker,
        );
        rx.recv().unwrap();
        state.resolve(&tx, &waker);
        for expected in ["child", "grandchild"] {
            assert!(
                matches!(rx.recv().unwrap(), AgentToCore::AgentSessionDetected { pane: PaneId(1), session_id, parent_session_id: Some(_), .. } if session_id == expected)
            );
        }
        state.resolve(&tx, &waker);
        assert!(rx.try_recv().is_err());
        state.forget(PaneId(1));
        assert_eq!(state.sessions.len(), 1);
    }
}
