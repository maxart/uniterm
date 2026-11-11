//! Native identity observation uses exact process files or cooperative paths.
//! Never guess ownership from a shared working directory or newest session.

use crossbeam_channel::{bounded, Sender};
use mio::Waker;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniterm_proto::{AgentToCore, PaneId};

mod tasks;
mod watch;

/// Identity owned by a provider's transcript format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Identity {
    pub id: String,
    pub parent: Option<String>,
    pub work_summary: Option<String>,
}

/// Only explicit provider metadata reaches this helper, never prompt bodies.
pub(super) fn metadata_label(text: &str) -> Option<String> {
    let label: String = text
        .chars()
        .filter(|ch| !ch.is_control())
        .take(200)
        .collect();
    let label = label.trim();
    (!label.is_empty()).then(|| label.to_owned())
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
    TaskTracking(bool),
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
            let dirty = watch::Pending::default();
            let mut state = State::default();
            loop {
                crossbeam_channel::select! {
                    recv(incoming) -> signal => {
                        if signal.is_err() { break; }
                        let mut requests = request_pending.lock().map(|mut requests| std::mem::take(&mut *requests)).unwrap_or_default();
                        // Settings apply before pane observations from the same
                        // coalesced batch, including first attach with opt-out.
                        if let Some(Request::TaskTracking(enabled)) = requests.remove(&PaneId(0)) {
                            state.set_task_tracking(enabled);
                        }
                        for request in requests.into_values() {
                          match request {
                            Request::Forget(pane) => state.forget(pane),
                            Request::TaskTracking(enabled) => state.set_task_tracking(enabled),
                            Request::Observe(target, supplied) => {
                            if state.targets.get(&target.pane).is_some_and(|old| old.pid != target.pid || old.provider != target.provider)
                                || supplied.as_ref().is_some_and(|(id, _)| state.owning_sessions.get(&target.pane).is_some_and(|old| old != id)) {
                                state.forget(target.pane);
                            }
                            state.targets.insert(target.pane, target.clone());
                            // A later real observation can recover a failed
                            // watch even after the root session is identified.
                            // No polling or timer is needed while idle.
                            let retry: Vec<_> = state.root_users.iter()
                                .filter(|(root, users)| users.contains(&target.pane) && !state.roots.contains_key(*root))
                                .map(|(root, _)| root.clone()).collect();
                            for root in retry {
                                if let Ok(mut pending) = dirty.lock() { pending.remove(&root); }
                                match watch::Root::new(&root, target.provider.clone(), dirty.clone(), changed.clone()) {
                                    Ok(watcher) => {
                                        state.roots.insert(root.clone(), watcher);
                                        if let Ok(mut pending) = dirty.lock() {
                                            pending.entry(root).or_default().request_rescan();
                                        }
                                        let _ = changed.try_send(());
                                    }
                                    Err(error) => eprintln!("uniterm: cannot restore session watch {}: {error}", root.display()),
                                }
                            }
                            let observations = if let Some((id, path)) = supplied {
                                vec![(Identity { id, parent: None, work_summary: None }, path.map(PathBuf::from))]
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
                                        target.provider.clone(), identity.id.clone())).and_then(|(_, path)| path.clone());
                                }
                                path = path.map(|path| std::fs::canonicalize(&path).unwrap_or(path));
                                let root = path.as_deref().and_then(|path| super::session_watch_root(&target.provider, path))
                                    .or_else(|| super::default_session_root(&target.provider, target.pid))
                                    .map(|path| std::fs::canonicalize(&path).unwrap_or(path));
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
                                        match watch::Root::new(entry.key(), target.provider.clone(), dirty.clone(), changed.clone()) {
                                            Ok(watcher) => { entry.insert(watcher); }
                                            Err(error) => eprintln!("uniterm: cannot watch session root {}: {error}", entry.key().display()),
                                        }
                                    }
                                    if bootstrap {
                                        // Descendants can predate this attach and live in
                                        // another date directory. Reconcile after installing
                                        // the watch so concurrent creates cannot be missed.
                                        state.seen_paths.clear();
                                        if let Ok(mut pending) = dirty.lock() {
                                            for (root, users) in &state.root_users {
                                                if users.contains(&target.pane) {
                                                    pending.entry(root.clone()).or_default().request_rescan();
                                                }
                                            }
                                        }
                                        let _ = changed.try_send(());
                                    }
                                }
                            }
                            state.resolve(&output, &waker);
                        }
                          }
                        }
                    },
                    recv(changes) -> _ => {
                        let changes = dirty.lock().map(|mut paths| std::mem::take(&mut *paths)).unwrap_or_default();
                        for (root, changes) in changes {
                            let Some(watcher) = state.roots.get_mut(&root) else { continue; };
                            let provider = watcher.provider.clone();
                            match watcher.paths(&root, changes) {
                                Ok((paths, more)) => {
                                    for (path, metadata_changed) in paths {
                                        if state.task_tails.get_mut(&path).is_some_and(|tail| tail.drain(&path, &output, &waker)) {
                                            if let Ok(mut pending) = dirty.lock() {
                                                pending.entry(root.clone()).or_default().queue_path(path.clone());
                                            }
                                            let _ = changed.try_send(());
                                        }
                                        if metadata_changed { state.seen_paths.remove(&path); }
                                        if !state.seen_paths.contains(&path) {
                                            state.observe_file(&provider, path, &output, &waker);
                                        }
                                    }
                                    if more {
                                        if let Ok(mut pending) = dirty.lock() { pending.entry(root.clone()).or_default(); }
                                        let _ = changed.try_send(());
                                    }
                                },
                                Err(error) => {
                                    eprintln!("uniterm: session watch {} failed: {error}", root.display());
                                    state.roots.remove(&root);
                                }
                            }
                        }
                        state.resolve(&output, &waker);
                    }
                }
                let unused: Vec<_> = state
                    .root_users
                    .keys()
                    .filter(|root| state.root_users.get(*root).is_none_or(HashSet::is_empty))
                    .cloned()
                    .collect();
                for root in unused {
                    state.roots.remove(&root);
                    state.root_users.remove(&root);
                    if let Ok(mut pending) = dirty.lock() {
                        pending.remove(&root);
                    }
                }
                state.release_unowned_paths();
                if std::mem::take(&mut state.reconcile_requested) {
                    if let Ok(mut pending) = dirty.lock() {
                        for root in state.roots.keys() {
                            pending.entry(root.clone()).or_default().request_rescan();
                        }
                    }
                    let _ = changed.try_send(());
                }
                if state.targets.is_empty() {
                    state.pending.clear();
                    state.unresolved_evicted = false;
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

    pub(crate) fn set_task_tracking(&self, enabled: bool) {
        // Pane ids start at one; zero is reserved for the workspace setting.
        self.queue(PaneId(0), Request::TaskTracking(enabled));
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

type Recipient = (PaneId, Option<i32>, String, String);

#[derive(Default)]
struct State {
    targets: HashMap<PaneId, Target>,
    identified: HashSet<PaneId>,
    sessions: HashMap<(String, String), Vec<Target>>,
    owning_sessions: HashMap<PaneId, String>,
    roots: HashMap<PathBuf, watch::Root>,
    root_users: HashMap<PathBuf, HashSet<PaneId>>,
    pending: HashMap<(String, String), (Identity, PathBuf)>,
    // Only published, reachable descendants. Retained edges let a newly
    // attached owner inherit already-observed children without filename order.
    linked: HashMap<(String, String), (Identity, PathBuf)>,
    seen_paths: HashSet<PathBuf>,
    delivered: HashMap<Recipient, (Identity, Option<PathBuf>)>,
    task_tails: HashMap<PathBuf, tasks::Tail>,
    task_tracking_disabled: bool,
    unresolved_evicted: bool,
    reconcile_requested: bool,
}

const MAX_UNRESOLVED: usize = 1024;

impl State {
    fn set_task_tracking(&mut self, enabled: bool) {
        if self.task_tracking_disabled != enabled {
            return;
        }
        self.task_tracking_disabled = !enabled;
        // Drop retained prompt fragments and turn identities immediately.
        self.task_tails.clear();
        if !enabled {
            return;
        }
        // Reenable only exact currently owned roots, beginning at today's EOF.
        // Historical prompts written during opt-out are never backfilled.
        for ((pane, pid, provider, session), (identity, path)) in &self.delivered {
            let Some(path) = path else {
                continue;
            };
            let Some(target) = self.targets.get(pane).filter(|target| {
                target.pid == *pid
                    && target.provider == *provider
                    && self.owning_sessions.get(pane) == Some(session)
                    && identity.parent.is_none()
            }) else {
                continue;
            };
            match self.task_tails.entry(path.clone()) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().add(target.clone(), session);
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    if let Some(tail) = tasks::Tail::new(target.clone(), session.clone(), path) {
                        entry.insert(tail);
                    }
                }
            }
        }
    }

    fn release_unowned_paths(&mut self) {
        let owned = |path: &Path| {
            self.root_users
                .iter()
                .any(|(root, users)| !users.is_empty() && path.starts_with(root))
        };
        self.pending.retain(|_, (_, path)| owned(path));
        self.seen_paths.retain(|path| owned(path));
    }

    fn remember_path(&mut self, path: PathBuf) {
        if self.seen_paths.len() >= 4096 && !self.seen_paths.contains(&path) {
            if let Some(old) = self.seen_paths.iter().next().cloned() {
                self.seen_paths.remove(&old);
            }
        }
        self.seen_paths.insert(path);
    }

    fn queue_unresolved(&mut self, provider: &str, identity: Identity, path: PathBuf) {
        let key = (provider.to_owned(), identity.id.clone());
        if self.pending.len() >= MAX_UNRESOLVED && !self.pending.contains_key(&key) {
            if let Some(old) = self.pending.keys().next().cloned() {
                self.pending.remove(&old);
            }
            // Newly reachable parents trigger a coalesced traversal, so this
            // memory bound is never a permanent ancestry-discovery cutoff.
            self.unresolved_evicted = true;
        }
        self.pending.insert(key, (identity, path));
    }

    fn observe_file(
        &mut self,
        provider: &str,
        path: PathBuf,
        output: &Sender<AgentToCore>,
        waker: &Waker,
    ) {
        if let Some(identity) = read_identity(provider, &path) {
            if let Some(parent) = &identity.parent {
                if let Some(targets) = self
                    .sessions
                    .get(&(provider.into(), parent.clone()))
                    .cloned()
                {
                    self.pending.remove(&(provider.into(), identity.id.clone()));
                    for target in targets {
                        self.publish(target, identity.clone(), Some(&path), output, waker);
                    }
                } else {
                    self.queue_unresolved(provider, identity, path);
                }
                // Unresolved files must remain eligible on reconciliation.
                return;
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
            self.remember_path(path);
        }
    }

    fn forget(&mut self, pane: PaneId) {
        self.task_tails.retain(|_, tail| tail.forget(pane));
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
        self.linked.retain(|key, _| self.sessions.contains_key(key));
        // Paths are only an optimization; discarding avoids unbounded growth
        // across repeated invocations and permits resumed sessions to re-link.
        self.seen_paths.clear();
        self.release_unowned_paths();
    }

    fn publish(
        &mut self,
        target: Target,
        identity: Identity,
        path: Option<&Path>,
        output: &Sender<AgentToCore>,
        waker: &Waker,
    ) {
        let root_identity = identity.parent.is_none();
        let key = (target.provider.clone(), identity.id.clone());
        let recipient = (
            target.pane,
            target.pid,
            target.provider.clone(),
            identity.id.clone(),
        );
        // Capture EOF before publishing root ownership. A prompt submitted
        // immediately after the core receives that identity must remain new.
        let task_tail = path
            .filter(|path| {
                !self.task_tracking_disabled
                    && root_identity
                    && !self.task_tails.contains_key(*path)
            })
            .and_then(|path| tasks::Tail::new(target.clone(), identity.id.clone(), path));
        if self
            .delivered
            .get(&recipient)
            .is_some_and(|(delivered_identity, delivered)| {
                delivered_identity == &identity && (path.is_none() || delivered.as_deref() == path)
            })
        {
            // A SessionStart hook can precede the first complete transcript
            // header. Retry only tail registration without publishing identity
            // twice when a later file event supplies that header.
            if let (Some(path), Some(tail)) = (path, task_tail) {
                self.task_tails.insert(path.to_owned(), tail);
            }
            return;
        }
        if identity.parent.is_none() {
            self.owning_sessions
                .insert(target.pane, identity.id.clone());
        }
        let Some(root_session_id) = self.owning_sessions.get(&target.pane).cloned() else {
            return;
        };
        let observed = identity.clone();
        let linked = identity
            .parent
            .as_ref()
            .and(path)
            .map(|path| (identity.clone(), path.to_owned()));
        let message = AgentToCore::AgentSessionDetected {
            pane: target.pane,
            foreground_pid: target.pid,
            provider: target.provider.clone(),
            session_id: identity.id,
            root_session_id,
            parent_session_id: identity.parent,
            transcript_path: path.map(|path| path.to_string_lossy().into_owned()),
            work_summary: identity.work_summary,
        };
        if output.send(message).is_ok() {
            if observed.parent.is_none() {
                if let Some(path) = path {
                    self.task_tails
                        .retain(|old_path, tail| old_path == path || tail.forget(target.pane));
                    match self.task_tails.entry(path.to_owned()) {
                        std::collections::hash_map::Entry::Occupied(mut entry) => {
                            entry.get_mut().add(target.clone(), &observed.id)
                        }
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            if let Some(tail) = task_tail {
                                entry.insert(tail);
                            }
                        }
                    }
                }
            }
            if let Some(linked) = linked {
                self.linked.insert(key.clone(), linked);
            }
            self.delivered
                .insert(recipient, (observed, path.map(Path::to_owned)));
            let targets = self.sessions.entry(key).or_default();
            if !targets
                .iter()
                .any(|old| old.pane == target.pane && old.pid == target.pid)
            {
                targets.push(target);
                self.reconcile_requested |= self.unresolved_evicted;
            }
            if let Some(path) = path {
                if self.task_tracking_disabled
                    || !root_identity
                    || self.task_tails.contains_key(path)
                {
                    self.remember_path(path.to_owned());
                }
            }
            let _ = waker.wake();
        }
    }

    fn resolve(&mut self, output: &Sender<AgentToCore>, waker: &Waker) {
        // Propagate to a fixed point before removing resolved headers. A
        // resumed root may gain a second owner after its descendants are
        // already known; HashMap order must not strand a grandchild on the
        // first owner when its parent is processed later in this batch.
        loop {
            let ready = self.pending.iter().chain(self.linked.iter()).find_map(
                |(key, (identity, path))| {
                    let parent = identity.parent.as_ref()?;
                    let targets = self.sessions.get(&(key.0.clone(), parent.clone()))?;
                    let target = targets.iter().find(|target| {
                        !self
                            .delivered
                            .get(&(
                                target.pane,
                                target.pid,
                                target.provider.clone(),
                                identity.id.clone(),
                            ))
                            .is_some_and(|(delivered_identity, delivered)| {
                                delivered_identity == identity && delivered.as_ref() == Some(path)
                            })
                    })?;
                    Some((identity.clone(), path.clone(), target.clone()))
                },
            );
            let Some((identity, path, target)) = ready else {
                break;
            };
            let recipient = (
                target.pane,
                target.pid,
                target.provider.clone(),
                identity.id.clone(),
            );
            self.publish(target, identity.clone(), Some(&path), output, waker);
            if !self
                .delivered
                .get(&recipient)
                .is_some_and(|(delivered_identity, delivered)| {
                    delivered_identity == &identity && delivered.as_ref() == Some(&path)
                })
            {
                // A disconnected receiver cannot make progress.
                return;
            }
        }
        self.pending.retain(|(provider, _), (identity, _)| {
            identity.parent.as_ref().is_none_or(|parent| {
                !self
                    .sessions
                    .contains_key(&(provider.clone(), parent.clone()))
            })
        });
    }
}

fn read_identity(provider: &str, path: &Path) -> Option<Identity> {
    // Claude can prefix identity with bookkeeping records. Bound the entire
    // prefix, skip bodies during provider parsing, and require complete lines.
    // Identity discovery does not read prompt bodies. The separately scoped
    // incremental root tail observes only newly accepted work after ownership.
    if std::fs::symlink_metadata(path)
        .ok()?
        .file_type()
        .is_symlink()
    {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file.take(256 * 1024));
    let mut line = String::new();
    for _ in 0..super::session_header_records(provider) {
        line.clear();
        reader.read_line(&mut line).ok()?;
        if !line.ends_with('\n') {
            return None;
        }
        if let Some(identity) = super::session_identity(provider, &line, path) {
            return Some(identity);
        }
    }
    None
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
        for entry in files.flatten() {
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
    while index < pids.len() {
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
                if same_group && !pids.contains(&child) {
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

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "uniterm-discovery-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn write(&self, relative: &str, text: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rollout(id: &str, parent: Option<&str>) -> String {
        let source = parent.map_or_else(|| serde_json::json!("cli"), |parent| serde_json::json!({"subagent":{"thread_spawn":{"parent_thread_id":parent,"depth":1,"agent_role":"reviewer"}}}));
        serde_json::json!({"type":"session_meta","payload":{"id":id,"source":source,"base_instructions":{"text":"PRIVATE BODY NEVER RETAINED"}}}).to_string() + "\n"
    }

    fn claude_record(session: &str, agent: Option<&str>) -> String {
        serde_json::json!({"type":"user","sessionId":session,"isSidechain":agent.is_some(),"agentId":agent,"parentUuid":"MESSAGE-ID-NOT-AN-AGENT","message":{"role":"user","content":"PRIVATE PROMPT NEVER RETAINED"}}).to_string() + "\n"
    }

    #[test]
    fn codex_bootstrap_crosses_dates_and_replays_children_for_later_observers() {
        let fixture = Fixture::new();
        let root = fixture.write(
            "sessions/2026/10/01/rollout-root.jsonl",
            &rollout("root", None),
        );
        fixture.write(
            "sessions/2026/10/02/rollout-child.jsonl",
            &rollout("child", Some("root")),
        );
        fixture.write(
            "sessions/2026/10/03/rollout-grandchild.jsonl",
            &rollout("grandchild", Some("child")),
        );
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), mio::Token(0)).unwrap());
        let (tx, rx) = bounded(32);
        let service = Service::start(tx, waker);
        for pane in [1, 2] {
            service.observe(
                PaneId(pane),
                Some(pane as i32),
                "codex".into(),
                Some(("root".into(), Some(root.to_string_lossy().into_owned()))),
            );
            let mut sessions = HashSet::new();
            for _ in 0..3 {
                match rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap() {
                    AgentToCore::AgentSessionDetected {
                        pane: owner,
                        session_id,
                        root_session_id,
                        ..
                    } => {
                        assert_eq!(owner, PaneId(pane));
                        assert_eq!(root_session_id, "root");
                        sessions.insert(session_id);
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
            assert_eq!(
                sessions,
                HashSet::from(["root".into(), "child".into(), "grandchild".into()])
            );
        }
        // A new silent descendant must reach every current owner, not the first.
        fixture.write(
            "sessions/2026/10/04/rollout-new.jsonl",
            &rollout("new", Some("child")),
        );
        let mut owners = HashSet::new();
        for _ in 0..2 {
            match rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap() {
                AgentToCore::AgentSessionDetected {
                    pane,
                    session_id,
                    parent_session_id,
                    ..
                } => {
                    assert_eq!(session_id, "new");
                    assert_eq!(parent_session_id.as_deref(), Some("child"));
                    owners.insert(pane);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(owners.len(), 2);
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(80))
            .is_err());
    }

    #[test]
    fn claude_native_metadata_waits_for_sidecar_and_preserves_nested_parent() {
        let fixture = Fixture::new();
        let root = fixture.write(
            "project/root.jsonl",
            &("{\"type\":\"file-history-snapshot\",\"snapshot\":{}}\n".to_owned()
                + &claude_record("root", None)),
        );
        let child = fixture.write(
            "project/root/subagents/agent-child.jsonl",
            &claude_record("root", Some("child")),
        );
        assert_eq!(
            read_identity("claude", &root),
            Some(Identity {
                id: "root".into(),
                parent: None,
                work_summary: None,
            })
        );
        assert_eq!(read_identity("claude", &child), None);
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), mio::Token(0)).unwrap());
        let (tx, rx) = bounded(32);
        let service = Service::start(tx, waker);
        service.observe(
            PaneId(1),
            Some(1),
            "claude".into(),
            Some(("root".into(), Some(root.to_string_lossy().into_owned()))),
        );
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
            AgentToCore::AgentSessionDetected {
                parent_session_id: None,
                ..
            }
        ));
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(80))
            .is_err());
        fixture.write(
            "project/root/subagents/agent-child.meta.json",
            r#"{"toolUseId":"tool_child","parentAgentId":null,"description":"Review rendering","prompt":"PRIVATE PROMPT"}"#,
        );
        assert!(
            matches!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), AgentToCore::AgentSessionDetected { session_id, parent_session_id: Some(parent), work_summary: Some(label), .. } if session_id == "root/agent/child" && parent == "root" && label == "Review rendering")
        );
        fixture.write(
            "project/root/subagents/workflows/run/agent-nested.meta.json",
            r#"{"toolUseId":"tool_nested","parentAgentId":"child"}"#,
        );
        fixture.write(
            "project/root/subagents/workflows/run/agent-nested.jsonl",
            &claude_record("root", Some("nested")),
        );
        assert!(
            matches!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), AgentToCore::AgentSessionDetected { session_id, parent_session_id: Some(parent), work_summary: None, .. } if session_id == "root/agent/nested" && parent == "root/agent/child")
        );
        fixture.write("project/root/subagents/agent-child.meta.json", r#"{"toolUseId":"tool_child","parentAgentId":null,"name":"Key reviewer","agentType":"reviewer","description":"Review keyboard","prompt":"PRIVATE PROMPT"}"#);
        assert!(
            matches!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), AgentToCore::AgentSessionDetected { session_id, work_summary: Some(label), .. } if session_id == "root/agent/child" && label == "Review keyboard")
        );
        fixture.write(
            "project/root/subagents/agent-child.meta.json",
            r#"{"toolUseId":"tool_child","parentAgentId":null,"prompt":"PRIVATE PROMPT"}"#,
        );
        assert!(
            matches!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), AgentToCore::AgentSessionDetected { session_id, work_summary: None, .. } if session_id == "root/agent/child")
        );
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(80))
            .is_err());
    }

    #[test]
    fn native_identity_rejects_path_mismatch_partial_records_and_private_body_ids() {
        let fixture = Fixture::new();
        let mismatch = fixture.write("project/wrong.jsonl", &claude_record("root", None));
        assert_eq!(read_identity("claude", &mismatch), None);
        let child = fixture.write(
            "project/wrong/subagents/agent-child.jsonl",
            &claude_record("root", Some("child")),
        );
        fixture.write(
            "project/wrong/subagents/agent-child.meta.json",
            r#"{"toolUseId":"tool_child"}"#,
        );
        assert_eq!(read_identity("claude", &child), None);
        let partial = fixture.write("sessions/partial.jsonl", rollout("root", None).trim_end());
        assert_eq!(read_identity("codex", &partial), None);
        let private = fixture.write(
            "project/root.jsonl",
            "{\"type\":\"user\",\"message\":{\"sessionId\":\"root\",\"content\":\"private\"}}\n",
        );
        assert_eq!(read_identity("claude", &private), None);
        let oversize = fixture.write(
            "project/root.jsonl",
            &(" ".repeat(256 * 1024) + &claude_record("root", None)),
        );
        assert_eq!(read_identity("claude", &oversize), None);
        assert!(super::super::codex::session_identity(
            r#"{"type":"session_meta","payload":{"id":"root","parent_thread_id":"root"}}"#
        )
        .is_none());
    }

    #[test]
    fn claude_subagent_storage_identity_supports_sparse_records_without_guessing_message_parents() {
        let fixture = Fixture::new();
        let child = fixture.write("project/root/subagents/agent-sparse.jsonl", "{\"type\":\"assistant\",\"sessionId\":\"root\",\"parentUuid\":\"message-not-agent\",\"message\":{\"content\":\"PRIVATE RESULT\"}}\n");
        fixture.write("project/root/subagents/agent-sparse.meta.json", r#"{"toolUseId":"tool_sparse","parentAgentId":"parent","agentType":"reviewer","name":"Render audit","prompt":"PRIVATE PROMPT"}"#);
        let identity = read_identity("claude", &child).unwrap();
        assert_eq!(identity.id, "root/agent/sparse");
        assert_eq!(identity.parent.as_deref(), Some("root/agent/parent"));
        assert_eq!(identity.work_summary.as_deref(), Some("Render audit"));
        fixture.write(
            "project/root/subagents/agent-sparse.meta.json",
            r#"{"toolUseId":"tool_sparse","parentAgentId":"parent","agentType":"reviewer"}"#,
        );
        assert_eq!(
            read_identity("claude", &child)
                .unwrap()
                .work_summary
                .as_deref(),
            Some("reviewer")
        );
        fixture.write(
            "project/root/subagents/agent-sparse.jsonl",
            &claude_record("root", Some("different")),
        );
        assert_eq!(read_identity("claude", &child), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn manually_started_native_roots_require_the_providers_own_writable_descriptor() {
        use std::os::unix::process::CommandExt;
        struct Process(std::process::Child);
        impl Drop for Process {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let fixture = Fixture::new();
        for (provider, filename, header) in [
            (
                "codex",
                "sessions/rollout-root.jsonl",
                rollout("root", None),
            ),
            ("claude", "project/root.jsonl", claude_record("root", None)),
            (
                "pi",
                "pi/root.jsonl",
                "{\"type\":\"session\",\"id\":\"root\"}\n".into(),
            ),
        ] {
            let path = fixture.write(filename, &header);
            for writable in [false, true] {
                let script = if writable {
                    "exec 3>>\"$1\"; printf 'ready\\n'; read stop"
                } else {
                    "exec 3<\"$1\"; printf 'ready\\n'; read stop"
                };
                let mut child = Process(
                    std::process::Command::new("/bin/sh")
                        .arg0(provider)
                        .args(["-c", script, provider])
                        .arg(&path)
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .spawn()
                        .unwrap(),
                );
                let mut ready = [0; 6];
                child
                    .0
                    .stdout
                    .as_mut()
                    .unwrap()
                    .read_exact(&mut ready)
                    .unwrap();
                assert_eq!(&ready, b"ready\n");
                let found = process_sessions(provider, child.0.id() as i32);
                if writable {
                    assert_eq!(
                        found,
                        vec![(
                            Identity {
                                id: "root".into(),
                                parent: None,
                                work_summary: None,
                            },
                            Some(path.clone())
                        )]
                    );
                } else {
                    assert!(
                        found.is_empty(),
                        "reading another agent's transcript does not establish ownership"
                    );
                }
            }
        }
    }

    fn target(pane: u64) -> Target {
        Target {
            pane: PaneId(pane),
            pid: Some(pane as i32),
            provider: "codex".into(),
        }
    }

    #[test]
    fn native_task_opt_out_releases_capture_and_reenables_at_eof() {
        use std::io::Write;
        let fixture = Fixture::new();
        let path = fixture.write("root.jsonl", &rollout("root", None));
        let poll = mio::Poll::new().unwrap();
        let waker = Waker::new(poll.registry(), mio::Token(0)).unwrap();
        let (tx, rx) = bounded(16);
        let mut state = State::default();
        let target = target(9);
        state.targets.insert(target.pane, target.clone());
        let identity = Identity {
            id: "root".into(),
            parent: None,
            work_summary: None,
        };
        state.publish(target.clone(), identity.clone(), Some(&path), &tx, &waker);
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentToCore::AgentSessionDetected { .. }
        ));
        assert_eq!(state.task_tails.len(), 1);
        state.set_task_tracking(false);
        assert!(state.task_tails.is_empty());
        let append = |message: &str| {
            let line = serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":message}}).to_string() + "\n";
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(line.as_bytes())
                .unwrap();
        };
        append("Private opt-out prompt");
        state.publish(target, identity, Some(&path), &tx, &waker);
        assert!(
            state.task_tails.is_empty(),
            "identity refresh must not restore capture"
        );
        assert!(rx.try_recv().is_err());
        state.set_task_tracking(true);
        let tail = state.task_tails.get_mut(&path).unwrap();
        assert!(!tail.drain(&path, &tx, &waker));
        assert!(
            rx.try_recv().is_err(),
            "opt-out history must not be imported"
        );
        append("New accepted work");
        assert!(!tail.drain(&path, &tx, &waker));
        assert!(
            matches!(rx.try_recv().unwrap(), AgentToCore::AgentTaskObserved {
            observation: uniterm_core::tasks::TaskObservation::Prompt { text, .. }, ..
        } if text == "New accepted work")
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn native_task_watcher_routes_new_root_prompts_without_importing_children_or_history() {
        use std::io::Write;
        use uniterm_core::tasks::TaskObservation;
        let fixture = Fixture::new();
        let path = fixture.write("sessions/2026/11/11/root.jsonl", &(rollout("root", None) + "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"old history\"}}\n"));
        let child = fixture.write(
            "sessions/2026/11/11/child.jsonl",
            &rollout("child", Some("root")),
        );
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), mio::Token(0)).unwrap());
        let (tx, rx) = bounded(16);
        let service = Service::start(tx, waker);
        service.observe(
            PaneId(9),
            Some(999),
            "codex".into(),
            Some(("root".into(), Some(path.to_string_lossy().into_owned()))),
        );
        for expected in ["root", "child"] {
            assert!(
                matches!(rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap(), AgentToCore::AgentSessionDetected { session_id, .. } if session_id == expected)
            );
        }
        assert!(rx.try_recv().is_err());
        let user = b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"turn_id\":\"one\",\"message\":\"Repair login\"}}\n";
        std::fs::OpenOptions::new()
            .append(true)
            .open(child)
            .unwrap()
            .write_all(user)
            .unwrap();
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(80))
            .is_err());
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(user)
            .unwrap();
        assert!(
            matches!(rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap(), AgentToCore::AgentTaskObserved { pane: PaneId(9), foreground_pid: Some(999), session_id, observation: TaskObservation::Prompt { text, .. }, .. } if session_id == "root" && text == "Repair login")
        );
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(80))
            .is_err());
        drop(service);
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
            work_summary: None,
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
    fn session_publication_has_no_fixed_fleet_cutoff() {
        let poll = mio::Poll::new().unwrap();
        let waker = Waker::new(poll.registry(), mio::Token(0)).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut state = State::default();
        for id in 0..4200 {
            state.publish(
                target(id),
                Identity {
                    id: format!("session-{id}"),
                    parent: None,
                    work_summary: None,
                },
                None,
                &tx,
                &waker,
            );
        }
        assert_eq!(rx.try_iter().count(), 4200);
        assert_eq!(state.sessions.len(), 4200);
        state.forget(PaneId(4199));
        assert_eq!(state.sessions.len(), 4199);
    }

    #[test]
    fn bootstrap_does_not_truncate_children_at_directory_or_pending_limits() {
        let fixture = Fixture::new();
        let root = fixture.write("sessions/parent.jsonl", &rollout("parent", None));
        for id in 0..600 {
            fixture.write(
                &format!("sessions/child-{id}.jsonl"),
                &rollout(&format!("child-{id}"), Some("parent")),
            );
        }
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), mio::Token(0)).unwrap());
        let (tx, rx) = bounded(32);
        let service = Service::start(tx, waker);
        service.observe(
            PaneId(1),
            Some(1),
            "codex".into(),
            Some(("parent".into(), Some(root.to_string_lossy().into_owned()))),
        );
        let mut sessions = HashSet::new();
        for _ in 0..601 {
            match rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap() {
                AgentToCore::AgentSessionDetected { session_id, .. } => {
                    sessions.insert(session_id);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(sessions.len(), 601);
    }

    #[test]
    fn unrelated_history_is_bounded_and_evicted_children_reconcile_on_parent_evidence() {
        let fixture = Fixture::new();
        for id in 0..5000 {
            fixture.write(
                &format!("sessions/child-{id}.jsonl"),
                &rollout(&format!("child-{id}"), Some(&format!("parent-{id}"))),
            );
        }
        let root = fixture.0.join("sessions");
        let poll = mio::Poll::new().unwrap();
        let waker = Waker::new(poll.registry(), mio::Token(0)).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let (changed, _) = bounded(1);
        let mut watcher =
            watch::Root::new(&root, "codex".into(), watch::Pending::default(), changed).unwrap();
        let mut state = State::default();
        state.targets.insert(PaneId(1), target(1));
        state
            .root_users
            .insert(root.clone(), HashSet::from([PaneId(1)]));
        state.publish(
            target(1),
            Identity {
                id: "root".into(),
                parent: None,
                work_summary: None,
            },
            None,
            &tx,
            &waker,
        );
        rx.recv().unwrap();
        let mut changes = watch::Changes::default();
        changes.request_rescan();
        let mut batches = 0;
        loop {
            let (paths, more) = watcher.paths(&root, std::mem::take(&mut changes)).unwrap();
            assert!(paths.len() <= 128);
            for (path, _) in paths {
                state.observe_file("codex", path, &tx, &waker);
            }
            state.resolve(&tx, &waker);
            assert!(state.pending.len() <= MAX_UNRESOLVED);
            assert!(state.seen_paths.len() <= 4096);
            batches += 1;
            if !more {
                break;
            }
        }
        assert!(batches > 39);
        assert_eq!(state.pending.len(), MAX_UNRESOLVED);
        assert!(state.unresolved_evicted);
        assert!(
            rx.try_recv().is_err(),
            "unrelated history is not assigned to this root"
        );

        // Deterministically evict the cache's next victim, then introduce its
        // previously unknown parent through a real native metadata observation.
        let (key, (victim, path)) = state
            .pending
            .iter()
            .next()
            .map(|(key, value)| (key.clone(), value.clone()))
            .unwrap();
        let extra = fixture.write("sessions/extra.jsonl", &rollout("extra", Some("unrelated")));
        state.observe_file("codex", extra, &tx, &waker);
        assert!(!state.pending.contains_key(&key));
        assert!(!state.seen_paths.contains(&path));
        let parent_id = victim.parent.as_deref().unwrap();
        let parent = fixture.write(
            "sessions/late-parent.jsonl",
            &rollout(parent_id, Some("root")),
        );
        state.observe_file("codex", parent, &tx, &waker);
        assert!(std::mem::take(&mut state.reconcile_requested));
        changes.request_rescan();
        let mut passes = 0;
        loop {
            let (paths, more) = watcher.paths(&root, std::mem::take(&mut changes)).unwrap();
            for (path, _) in paths {
                if !state.seen_paths.contains(&path) {
                    state.observe_file("codex", path, &tx, &waker);
                }
            }
            state.resolve(&tx, &waker);
            assert!(state.pending.len() <= MAX_UNRESOLVED);
            let reconcile = std::mem::take(&mut state.reconcile_requested);
            if reconcile {
                changes.request_rescan();
            }
            passes += 1;
            assert!(
                passes < 200,
                "reconciliation must drain without a periodic scan"
            );
            if !more && !reconcile {
                break;
            }
        }
        let found: HashSet<_> = rx
            .try_iter()
            .filter_map(|event| match event {
                AgentToCore::AgentSessionDetected { session_id, .. } => Some(session_id),
                _ => None,
            })
            .collect();
        assert_eq!(found, HashSet::from([parent_id.to_owned(), victim.id]));
        state.forget(PaneId(1));
        assert!(state.pending.is_empty());
        assert!(state.seen_paths.is_empty());
        assert!(state.delivered.is_empty());
        assert!(state.sessions.is_empty());
        eprintln!("native history: 5000 unrelated files, bootstrap {batches} batches; max 128 entries/batch, 1024 unresolved identities, 4096 cached paths; late parent reconciliation {passes} batches");
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
                    work_summary: None,
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
                    work_summary: None,
                },
                "/tmp/grandchild.jsonl".into(),
            ),
        );
        state.publish(
            target(2),
            Identity {
                id: "unrelated".into(),
                parent: None,
                work_summary: None,
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
                work_summary: None,
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
