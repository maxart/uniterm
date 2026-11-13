//! Local-machine saved SSH catalog and its native management surface.
//! The catalog is an append-only machine event stream, separate from Workspace
//! history. Remote queries run only on explicit interaction and wake mio once.
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::overlay::{Overlay, OverlayRowStyle};
use crate::text_input::{decode_key, edit_line, LineKey};
use uniterm_core::machine::{apply, MachineEvent, MachineProfile};

const MAX_LOG: u64 = 16 * 1024 * 1024;
const MAX_REPLY: u64 = 1024 * 1024;

/// Machine-wide catalog location; never relative to the repository.
pub fn catalog_path() -> io::Result<PathBuf> {
    if let Some(root) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(root).join("uniterm/machines.jsonl"));
    }
    std::env::var_os("HOME")
        .map(|root| PathBuf::from(root).join(".local/state/uniterm/machines.jsonl"))
        .ok_or_else(|| io::Error::other("HOME and XDG_STATE_HOME are unset"))
}

fn read_log(path: &Path) -> io::Result<(Vec<MachineProfile>, u64)> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > MAX_LOG {
        return Err(io::Error::other("Server catalog exceeds 16 MiB"));
    }
    let mut profiles = Vec::new();
    let mut sequence = 0;
    let mut reader = io::BufReader::new(file);
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        // A record is committed only with its delimiter. Appending after a
        // complete JSON value whose newline was torn would join two records.
        if !line.ends_with('\n') {
            return Err(io::Error::other(
                "incomplete Server catalog record; existing data preserved",
            ));
        }
        let (version, next, event): (u32, u64, MachineEvent) =
            serde_json::from_str(&line).map_err(io::Error::other)?;
        if version != 1 || next != sequence + 1 {
            return Err(io::Error::other(
                "invalid Server catalog version or sequence; existing data preserved",
            ));
        }
        apply(&mut profiles, event).map_err(io::Error::other)?;
        sequence = next;
        line.clear();
    }
    Ok((profiles, sequence))
}

/// Read saved profiles without starting any SSH process.
pub fn list_at(path: &Path) -> io::Result<Vec<MachineProfile>> {
    read_log(path).map(|(profiles, _)| profiles)
}

struct CatalogLock(File);
impl Drop for CatalogLock {
    fn drop(&mut self) {
        // SAFETY: flock operates on this owned, live sidecar descriptor.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Serialize writers, validate against the latest projection and sync the event
/// before exposing success. A torn or invalid stream is never silently replaced.
pub fn update_at(
    path: &Path,
    change: impl FnOnce(&[MachineProfile], u64) -> io::Result<MachineEvent>,
) -> io::Result<Vec<MachineProfile>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = CatalogLock(
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path.with_extension("lock"))?,
    );
    // SAFETY: the lock file remains open until this scope exits.
    if unsafe { libc::flock(lock.0.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::other("Server catalog is busy; retry the change"));
    }
    let (mut profiles, sequence) = read_log(path)?;
    let next = sequence
        .checked_add(1)
        .ok_or_else(|| io::Error::other("Server catalog sequence exhausted"))?;
    let event = change(&profiles, next)?;
    let previous = profiles.clone();
    apply(&mut profiles, event.clone()).map_err(io::Error::other)?;
    if profiles == previous {
        return Ok(profiles);
    }
    let mut encoded = serde_json::to_vec(&(1u32, next, event)).map_err(io::Error::other)?;
    encoded.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    if file.metadata()?.len() + encoded.len() as u64 > MAX_LOG {
        return Err(io::Error::other("Server catalog exceeds 16 MiB"));
    }
    file.write_all(&encoded)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(profiles)
}

/// Select by stable id or exact case-insensitive label.
pub fn select<'a>(
    profiles: &'a [MachineProfile],
    selector: &str,
) -> io::Result<&'a MachineProfile> {
    profiles
        .iter()
        .find(|profile| {
            profile.id.to_string() == selector || profile.label.eq_ignore_ascii_case(selector)
        })
        .ok_or_else(|| io::Error::other("saved Server not found"))
}

/// Build one bounded noninteractive catalog request. Host trust stays strict;
/// passwords and first-use host acceptance happen in interactive connect only.
pub fn query_command(target: &str) -> io::Result<Command> {
    uniterm_core::machine::validate_target(target).map_err(io::Error::other)?;
    let mut command = Command::new("ssh");
    command.process_group(0);
    command.args(["-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=8", "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=2"])
        .arg(target).arg("if command -v uniterm >/dev/null 2>&1; then exec uniterm machine-catalog; else exec ut machine-catalog; fi")
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    Ok(command)
}

/// A remote observation is explicitly a snapshot, not an enduring online claim.
#[derive(Clone, Debug)]
pub struct RemoteCatalog {
    /// Host-scoped metadata from the latest explicit discovery request.
    pub workspaces: Vec<uniterm_proto::WorkspaceInfo>,
}

fn parse_reply(bytes: &[u8]) -> io::Result<RemoteCatalog> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| io::Error::other("Remote did not return a Uniterm catalog; verify the installed binary and login-shell output"))?;
    let version = value.get("protocol").and_then(|v| v.as_u64());
    if version != Some(u64::from(uniterm_proto::WIRE_PROTOCOL_VERSION)) {
        return Err(io::Error::other(
            "Remote Uniterm protocol differs; install the same build on both machines",
        ));
    }
    let workspaces: Vec<uniterm_proto::WorkspaceInfo> =
        serde_json::from_value(value["workspaces"].clone()).map_err(io::Error::other)?;
    if workspaces.len() > 1024
        || workspaces
            .iter()
            .any(|w| uniterm_proto::validate_workspace_name(&w.name).is_err())
    {
        return Err(io::Error::other(
            "Remote catalog contains invalid or excessive Workspaces",
        ));
    }
    Ok(RemoteCatalog { workspaces })
}

struct Probe {
    child: Child,
    rx: mpsc::Receiver<io::Result<RemoteCatalog>>,
    deadline: Instant,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        // SAFETY: discovery creates this process group and its leader has not
        // been reaped, so the group identity cannot have been reused.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Probe {
    fn start(target: &str, waker: Arc<mio::Waker>) -> io::Result<Self> {
        Self::start_command(query_command(target)?, waker)
    }
    fn start_command(mut command: Command, waker: Arc<mio::Waker>) -> io::Result<Self> {
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing SSH stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing SSH stderr"))?;
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let errors = std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = stderr.take(8192).read_to_end(&mut bytes);
                bytes
            });
            let mut bytes = Vec::new();
            let result = stdout
                .take(MAX_REPLY + 1)
                .read_to_end(&mut bytes)
                .map_err(io::Error::other)
                .and_then(|_| {
                    if bytes.len() as u64 > MAX_REPLY {
                        Err(io::Error::other("Remote catalog exceeds 1 MiB"))
                    } else {
                        parse_reply(&bytes)
                    }
                });
            if bytes.len() as u64 > MAX_REPLY {
                let _ = tx.send(result);
                let _ = waker.wake();
                // The receiver cancels the process group before joining this
                // worker, closing stderr even if SSH keeps the stream open.
                let _ = errors.join();
                return;
            }
            let result = match (result, errors.join()) {
                (Err(error), Ok(stderr)) if !stderr.is_empty() => Err(io::Error::other(format!(
                    "{error}: {}",
                    clean(&String::from_utf8_lossy(&stderr))
                ))),
                (result, _) => result,
            };
            let _ = tx.send(result);
            let _ = waker.wake();
        });
        Ok(Self {
            child,
            rx,
            deadline: Instant::now() + Duration::from_secs(20),
            worker: Some(worker),
        })
    }
}

fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(512).collect()
}

/// User intent is returned to the CLI only after terminal raw mode is restored.
pub(crate) enum MachineAction {
    None,
    Close,
    Connect(MachineProfile),
    Local,
}

/// One native Servers dialog. It holds no steady-state timer or SSH connection.
pub(crate) struct MachinesView {
    profiles: Vec<MachineProfile>,
    selected: usize,
    error: Option<String>,
    edit: Option<(usize, [String; 3], usize)>,
    editing_id: Option<u64>,
    remove_confirm: bool,
    probe: Option<Probe>,
    catalog: Option<RemoteCatalog>,
    workspace: usize,
    waker: Arc<mio::Waker>,
}
impl MachinesView {
    pub fn with_error(waker: Arc<mio::Waker>, error: &str) -> Self {
        let mut view = Self::open(waker);
        view.error = Some(error.into());
        view
    }
    pub fn open(waker: Arc<mio::Waker>) -> Self {
        let mut view = Self {
            profiles: Vec::new(),
            selected: 0,
            error: None,
            edit: None,
            editing_id: None,
            remove_confirm: false,
            probe: None,
            catalog: None,
            workspace: 0,
            waker,
        };
        view.reload();
        view
    }
    fn reload(&mut self) {
        match catalog_path().and_then(|path| list_at(&path)) {
            Ok(profiles) => {
                self.profiles = profiles;
                self.selected = self.selected.min(self.profiles.len().saturating_sub(1));
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }
    fn update(&mut self, event: MachineEvent) {
        match catalog_path().and_then(|path| {
            update_at(&path, |latest, _| {
                let id = match &event {
                    MachineEvent::Saved(profile) => profile.id,
                    MachineEvent::Removed(id) => *id,
                };
                if latest.iter().find(|p| p.id == id) != self.profiles.iter().find(|p| p.id == id) {
                    return Err(io::Error::other(
                        "Server changed in another client; refresh before editing",
                    ));
                }
                Ok(event)
            })
        }) {
            Ok(profiles) => {
                self.profiles = profiles;
                self.selected = self.selected.min(self.profiles.len().saturating_sub(1));
                self.error = None;
                self.probe = None;
                self.catalog = None;
                self.workspace = 0;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }
    pub fn timeout(&self, now: Instant) -> Option<Duration> {
        self.probe
            .as_ref()
            .map(|p| p.deadline.saturating_duration_since(now))
    }
    pub fn complete(&mut self) -> bool {
        let Some(probe) = &self.probe else {
            return false;
        };
        let result = match probe.rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(io::Error::other(
                "SSH discovery ended without a response",
            ))),
            Err(mpsc::TryRecvError::Empty) if Instant::now() >= probe.deadline => {
                Some(Err(io::Error::other(
                    "SSH discovery timed out; reconnect to authenticate or retry refresh",
                )))
            }
            _ => None,
        };
        let Some(result) = result else {
            return false;
        };
        self.probe = None;
        match result {
            Ok(catalog) => {
                self.catalog = Some(catalog);
                self.workspace = 0;
                self.error = None;
            }
            Err(error) => {
                self.error = Some(error.to_string());
                self.catalog = None;
            }
        }
        true
    }
    pub fn handle(&mut self, bytes: &[u8]) -> MachineAction {
        let mut offset = 0;
        while offset < bytes.len() {
            let (key, used) = decode_key(bytes, offset);
            offset += used.max(1);
            if let Some((field, values, cursor)) = &mut self.edit {
                match key {
                    LineKey::Escape | LineKey::Cancel => self.edit = None,
                    LineKey::Tab => {
                        *field = (*field + 1) % 3;
                        *cursor = values[*field].len();
                    }
                    LineKey::Enter => {
                        let values = values.clone();
                        match catalog_path().and_then(|path| {
                            update_at(&path, |latest, next| {
                                if let Some(id) = self.editing_id {
                                    if latest.iter().find(|p| p.id == id)
                                        != self.profiles.iter().find(|p| p.id == id)
                                    {
                                        return Err(io::Error::other(
                                            "Server changed in another client; cancel and refresh",
                                        ));
                                    }
                                }
                                Ok(MachineEvent::Saved(MachineProfile {
                                    id: self.editing_id.unwrap_or(next),
                                    label: values[0].clone(),
                                    target: values[1].clone(),
                                    workspace: if values[2].is_empty() {
                                        "default".into()
                                    } else {
                                        values[2].clone()
                                    },
                                    enabled: self
                                        .editing_id
                                        .and_then(|id| latest.iter().find(|p| p.id == id))
                                        .is_none_or(|p| p.enabled),
                                }))
                            })
                        }) {
                            Ok(profiles) => {
                                self.profiles = profiles;
                                self.edit = None;
                                self.error = None;
                                self.probe = None;
                                self.catalog = None;
                                self.workspace = 0;
                            }
                            Err(error) => self.error = Some(error.to_string()),
                        }
                    }
                    key => {
                        edit_line(&mut values[*field], cursor, key);
                    }
                }
                continue;
            }
            if self.remove_confirm {
                self.remove_confirm = false;
                if key == LineKey::Char('y') {
                    if let Some(p) = self.profiles.get(self.selected) {
                        self.update(MachineEvent::Removed(p.id));
                    }
                }
                continue;
            }
            match key {
                LineKey::Escape | LineKey::Cancel => return MachineAction::Close,
                LineKey::Char('l') => return MachineAction::Local,
                LineKey::Up | LineKey::Down => {
                    self.probe = None;
                    self.catalog = None;
                    if key == LineKey::Up {
                        self.selected = self.selected.saturating_sub(1);
                    } else {
                        self.selected =
                            (self.selected + 1).min(self.profiles.len().saturating_sub(1));
                    }
                }
                LineKey::Tab => {
                    if let Some(c) = &self.catalog {
                        self.workspace = (self.workspace + 1) % c.workspaces.len().max(1);
                    }
                }
                LineKey::Char('a') => {
                    self.editing_id = None;
                    self.edit = Some((0, [String::new(), String::new(), String::new()], 0));
                }
                LineKey::Char('n') => {
                    if let Some(p) = self.profiles.get(self.selected) {
                        self.editing_id = Some(p.id);
                        self.edit = Some((
                            0,
                            [p.label.clone(), p.target.clone(), p.workspace.clone()],
                            p.label.len(),
                        ));
                    }
                }
                LineKey::Char('d') => {
                    self.remove_confirm = !self.profiles.is_empty();
                }
                LineKey::Char('e') => {
                    if let Some(p) = self.profiles.get(self.selected) {
                        let mut p = p.clone();
                        p.enabled = !p.enabled;
                        self.update(MachineEvent::Saved(p));
                    }
                }
                LineKey::Char('r') => {
                    self.reload();
                    self.probe = None;
                    self.catalog = None;
                    if let Some(p) = self.profiles.get(self.selected).filter(|p| p.enabled) {
                        match Probe::start(&p.target, self.waker.clone()) {
                            Ok(probe) => self.probe = Some(probe),
                            Err(error) => self.error = Some(error.to_string()),
                        }
                    }
                }
                LineKey::Enter => {
                    if let Some(p) = self.profiles.get(self.selected).filter(|p| p.enabled) {
                        let mut p = p.clone();
                        if let Some(w) = self
                            .catalog
                            .as_ref()
                            .and_then(|c| c.workspaces.get(self.workspace))
                        {
                            p.workspace = w.name.clone();
                        }
                        return MachineAction::Connect(p);
                    }
                }
                _ => {}
            }
        }
        MachineAction::None
    }
    fn overlay(&self, rows: u16) -> Overlay {
        let capacity = rows.saturating_sub(17).max(1) as usize;
        let start = self.selected.saturating_sub(capacity - 1);
        let mut lines = vec![
            "Local  /  Servers".into(),
            "Saved SSH targets. Refresh explicitly to discover remote Workspaces.".into(),
            String::new(),
        ];
        for (i, p) in self.profiles.iter().enumerate().skip(start).take(capacity) {
            lines.push(format!(
                "{} {}  {} / {}  [{}]",
                if i == self.selected { ">" } else { " " },
                clean(&p.label),
                clean(&p.target),
                clean(&p.workspace),
                if p.enabled { "saved" } else { "disabled" }
            ));
        }
        if self.profiles.is_empty() {
            lines.push("No saved Servers. Press a to add one.".into());
        }
        if self.probe.is_some() {
            lines.push("Connecting for discovery... Local remains available (l).".into());
        }
        if let Some(catalog) = &self.catalog {
            lines.push("Remote snapshot (Tab selects a Workspace; Enter connects)".into());
            for (i, w) in catalog
                .workspaces
                .iter()
                .enumerate()
                .skip(self.workspace.saturating_sub(4))
                .take(5)
            {
                lines.push(format!(
                    "  {} {}  {} Projects, {} Tabs, {} Panes  {}",
                    if i == self.workspace { ">" } else { " " },
                    clean(&w.name),
                    w.projects,
                    w.windows,
                    w.panes,
                    if w.running { "running" } else { "stopped" }
                ));
            }
        }
        if let Some((field, values, _)) = &self.edit {
            lines.push("Server details: Tab changes field, Enter saves, Esc cancels".into());
            for (i, label) in ["Label", "SSH target", "Workspace (default)"]
                .iter()
                .enumerate()
            {
                lines.push(format!(
                    "{} {label}: {}",
                    if i == *field { ">" } else { " " },
                    clean(&values[i])
                ));
            }
        }
        if self.remove_confirm {
            lines.push("Forget selected Server? y confirms; remote sessions keep running.".into());
        }
        if let Some(error) = &self.error {
            lines.push(format!("Attention: {}", clean(error)));
        }
        let styles = lines
            .iter()
            .enumerate()
            .map(|(i, _)| {
                if i == self.selected - start + 3 {
                    OverlayRowStyle::CardSelected
                } else {
                    OverlayRowStyle::Plain
                }
            })
            .collect();
        Overlay::with_footer(
            "Servers",
            lines,
            &[
                ("Enter", "Connect"),
                ("r", "Refresh"),
                ("a", "Add"),
                ("n", "Edit"),
                ("e", "Enable"),
                ("d", "Remove"),
                ("l", "Local"),
                ("Esc", "Close"),
            ],
        )
        .with_row_styles(styles)
    }
    pub fn render(&self, cols: u16, rows: u16) -> Vec<u8> {
        self.overlay(rows).render(cols, rows)
    }
    pub fn click(&mut self, cols: u16, rows: u16, x: u16, y: u16) -> MachineAction {
        if self.edit.is_some() || self.remove_confirm {
            return MachineAction::None;
        }
        if let Some(row) = self.overlay(rows).row_at(cols, rows, x, y) {
            if row == 0 && x < self.overlay(rows).geometry(cols, rows).x + 9 {
                return MachineAction::Local;
            }
            let capacity = rows.saturating_sub(17).max(1) as usize;
            let start = self.selected.saturating_sub(capacity - 1);
            let remote_start = 4 + self.profiles.len().min(capacity);
            if let Some(catalog) = &self.catalog {
                let workspace = self.workspace.saturating_sub(4) + row.saturating_sub(remote_start);
                if row >= remote_start
                    && row < remote_start + catalog.workspaces.len().min(5)
                    && workspace < catalog.workspaces.len()
                {
                    self.workspace = workspace;
                    return self.handle(b"\r");
                }
            }
            let index = row.saturating_sub(3) + start;
            if row >= 3
                && row < 3 + self.profiles.len().min(capacity)
                && index < self.profiles.len()
            {
                if index == self.selected {
                    return self.handle(b"\r");
                }
                self.selected = index;
                self.probe = None;
                self.catalog = None;
            }
        }
        MachineAction::None
    }
}

/// Explicit CLI discovery shares the same bounded subprocess and strict SSH path
/// as the dialog; the caller is outside terminal raw mode.
pub fn discover(target: &str) -> io::Result<RemoteCatalog> {
    let mut poll = mio::Poll::new()?;
    let waker = Arc::new(mio::Waker::new(poll.registry(), mio::Token(0))?);
    let probe = Probe::start(target, waker.clone())?;
    let mut events = mio::Events::with_capacity(2);
    loop {
        match probe.rx.try_recv() {
            Ok(result) => return result,
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(io::Error::other("SSH discovery ended"))
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let remaining = probe.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH discovery exceeded 20 seconds",
            ));
        }
        match poll.poll(&mut events, Some(remaining)) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => result?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn machine_catalog_appends_replays_and_preserves_invalid_history() {
        let root = std::env::temp_dir().join(format!(
            "uniterm-machines-{}-{}",
            std::process::id(),
            crate::tty::sanitize_terminal_title(&format!("{:?}", Instant::now()))
                .replace([' ', ':', '{', '}'], "")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("machines.jsonl");
        assert!(list_at(&path).unwrap().is_empty());
        assert!(!path.exists());
        update_at(&path, |_, next| {
            Ok(MachineEvent::Saved(MachineProfile {
                id: next,
                label: "Build".into(),
                target: "build".into(),
                workspace: "default".into(),
                enabled: true,
            }))
        })
        .unwrap();
        assert_eq!(list_at(&path).unwrap()[0].id, 1);
        let complete = std::fs::read(&path).unwrap();
        let torn = &complete[..complete.len() - 1];
        std::fs::write(&path, torn).unwrap();
        assert!(list_at(&path).is_err());
        assert!(update_at(&path, |_, _| Ok(MachineEvent::Removed(1))).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), torn);
        std::fs::write(&path, complete).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"broken")
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(update_at(&path, |_, _| Ok(MachineEvent::Removed(1))).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancelling_discovery_reaps_its_process_group_and_joins_readers() {
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(mio::Waker::new(poll.registry(), mio::Token(0)).unwrap());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 60 & wait"]);
        let probe = Probe::start_command(command, waker).unwrap();
        let started = Instant::now();
        drop(probe);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn servers_dialog_has_no_idle_deadline_and_keeps_remote_workspace_selection() {
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(mio::Waker::new(poll.registry(), mio::Token(0)).unwrap());
        let profile = MachineProfile {
            id: 1,
            label: "Build".into(),
            target: "build".into(),
            workspace: "default".into(),
            enabled: true,
        };
        let mut view = MachinesView {
            profiles: vec![profile],
            selected: 0,
            error: None,
            edit: None,
            editing_id: None,
            remove_confirm: false,
            probe: None,
            catalog: Some(RemoteCatalog {
                workspaces: vec![uniterm_proto::WorkspaceInfo {
                    name: "agents".into(),
                    windows: 2,
                    panes: 3,
                    projects: 1,
                    running: true,
                }],
            }),
            workspace: 0,
            waker,
        };
        assert!(view.timeout(Instant::now()).is_none());
        assert!(!view.complete());
        let rendered = String::from_utf8(view.render(100, 30)).unwrap();
        assert!(rendered.contains("Remote snapshot"));
        assert!(rendered.contains("agents"));
        assert!(matches!(view.handle(b"\r"), MachineAction::Connect(p) if p.workspace == "agents"));
        assert!(matches!(view.handle(b"l"), MachineAction::Local));
        assert!(view.timeout(Instant::now()).is_none());
    }

    #[test]
    fn remote_discovery_preserves_ssh_trust_and_bounds_protocol() {
        let command = query_command("user@build").unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"BatchMode=yes".into()));
        assert!(args.contains(&"StrictHostKeyChecking=yes".into()));
        assert!(query_command("-oProxyCommand=bad").is_err());
        assert!(parse_reply(br#"{"protocol":0,"workspaces":[]}"#).is_err());
        let bytes = serde_json::to_vec(
            &serde_json::json!({"protocol":uniterm_proto::WIRE_PROTOCOL_VERSION,"workspaces":[]}),
        )
        .unwrap();
        assert!(parse_reply(&bytes).unwrap().workspaces.is_empty());
    }
}
