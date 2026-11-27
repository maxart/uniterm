//! Local-machine saved SSH catalog and its native management surface.
//! The catalog is an append-only machine event stream, separate from Workspace
//! history. Remote queries run only on explicit interaction and wake mio once.
mod editor;
mod remote_shell;
pub use remote_shell::bridge_command;

use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::overlay::{Overlay, OverlayRowStyle, Rect};
use crate::text_input::{decode_key, LineKey};
use editor::Editor;
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
    command
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=8",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
        ])
        .arg(target)
        .arg(remote_shell::catalog_command())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

/// A remote observation is explicitly a snapshot, not an enduring online claim.
#[derive(Clone, Debug)]
pub struct RemoteCatalog {
    /// Host-scoped metadata from the latest explicit discovery request.
    pub workspaces: Vec<uniterm_proto::WorkspaceInfo>,
}

fn parse_reply(bytes: &[u8]) -> io::Result<RemoteCatalog> {
    let marker = remote_shell::CATALOG_READY;
    let payload = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .map_or(bytes, |start| &bytes[start + marker.len()..]);
    let value: serde_json::Value = serde_json::from_slice(payload).map_err(|_| io::Error::other("Remote did not return a Uniterm catalog; verify the installed binary and login-shell output"))?;
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
    // A helper inheriting the SSH streams must not keep cancellation or retry blocked.
    readers: [UnixStream; 2],
    rx: mpsc::Receiver<io::Result<RemoteCatalog>>,
    deadline: Instant,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        for reader in &self.readers {
            let _ = reader.shutdown(std::net::Shutdown::Read);
        }
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
        let (stdout, stdout_writer) = UnixStream::pair()?;
        let (stderr, stderr_writer) = UnixStream::pair()?;
        command
            .stdout(OwnedFd::from(stdout_writer))
            .stderr(OwnedFd::from(stderr_writer));
        Self::spawn(command, [stdout, stderr], waker)
    }

    fn spawn(
        mut command: Command,
        readers: [UnixStream; 2],
        waker: Arc<mio::Waker>,
    ) -> io::Result<Self> {
        let stdout = readers[0].try_clone()?;
        let stderr = readers[1].try_clone()?;
        let child = command.process_group(0).stdin(Stdio::null()).spawn()?;
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let errors = std::thread::spawn(move || discovery_diagnostic(stderr));
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
                (Err(error), Ok(stderr)) if !stderr.is_empty() => {
                    Err(io::Error::other(format!("{error}: {stderr}")))
                }
                (result, _) => result,
            };
            let _ = tx.send(result);
            let _ = waker.wake();
        });
        Ok(Self {
            child,
            readers,
            rx,
            deadline: Instant::now() + Duration::from_secs(20),
            worker: Some(worker),
        })
    }
}

// Drain even verbose SSH configurations to EOF, retaining only the bounded
// tail. Closing stderr at the limit can terminate a healthy SSH writer.
fn discovery_diagnostic(mut stderr: impl Read) -> String {
    const LIMIT: usize = 8192;
    let mut tail = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                tail.extend_from_slice(&buffer[..n]);
                tail.drain(..tail.len().saturating_sub(LIMIT));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&tail)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(clean)
        .unwrap_or_default()
}

fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(512).collect()
}

/// User intent is returned to the CLI only after terminal raw mode is restored.
pub(crate) enum MachineAction {
    None,
    Redraw,
    Close,
    Connect(MachineProfile),
    Local,
}

// The SSH client knows its destination even when the catalog has no matching
// profile. The Workspace name arrives independently from the selected server.
#[derive(Debug)]
struct ConnectedMachine {
    target: String,
    workspace: Option<String>,
    profile: Option<u64>,
}

impl ConnectedMachine {
    fn profile_index(&self, profiles: &[MachineProfile]) -> Option<usize> {
        if let Some(id) = self.profile {
            return profiles
                .iter()
                .position(|p| p.id == id && p.target == self.target);
        }
        // A direct SSH command identifies a target, sometimes shared by several
        // saved profiles. Prefer its exact default Workspace; leave ties unmarked.
        let mut matching = profiles
            .iter()
            .enumerate()
            .filter(|(_, p)| p.target == self.target);
        let (first, _) = matching.clone().next()?;
        let mut exact = matching
            .clone()
            .filter(|(_, p)| Some(p.workspace.as_str()) == self.workspace.as_deref());
        if let Some((index, _)) = exact.next() {
            return exact.next().is_none().then_some(index);
        }
        matching.nth(1).is_none().then_some(first)
    }
}

/// One native Servers dialog. It holds no steady-state timer or SSH connection.
pub(crate) struct MachinesView {
    profiles: Vec<MachineProfile>,
    connected: Option<ConnectedMachine>,
    selected: usize,
    error: Option<String>,
    edit: Option<Editor>,
    last_rect: Cell<Option<Rect>>,
    remove_confirm: bool,
    probe: Option<Probe>,
    catalog: Option<RemoteCatalog>,
    workspace: usize,
    waker: Arc<mio::Waker>,
}
impl MachinesView {
    pub fn open(
        waker: Arc<mio::Waker>,
        options: &crate::AttachOptions,
        workspace: Option<&str>,
        error: Option<&str>,
    ) -> Self {
        let mut view = Self {
            profiles: Vec::new(),
            connected: options
                .remote_target
                .as_ref()
                .map(|target| ConnectedMachine {
                    target: target.clone(),
                    workspace: workspace.map(str::to_owned),
                    profile: options.remote_profile,
                }),
            selected: 0,
            error: None,
            edit: None,
            last_rect: Cell::default(),
            remove_confirm: false,
            probe: None,
            catalog: None,
            workspace: 0,
            waker,
        };
        view.reload();
        if let Some(index) = view
            .connected
            .as_ref()
            .and_then(|c| c.profile_index(&view.profiles))
        {
            view.selected = index;
        }
        if let Some(error) = error {
            view.error = Some(error.to_owned());
        }
        view
    }

    pub fn remote_workspace_renamed(&mut self, name: &str) {
        if let Some(connection) = &mut self.connected {
            connection.workspace = Some(name.to_owned());
        }
    }
    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }

    // Discovery may run only for the same profile the user chose before the
    // reload. A concurrent removal or retarget must never probe another host.
    fn reload(&mut self) -> bool {
        match catalog_path().and_then(|path| list_at(&path)) {
            Ok(profiles) => self.replace_profiles(profiles),
            Err(error) => {
                self.set_error(error.to_string());
                false
            }
        }
    }

    fn replace_profiles(&mut self, profiles: Vec<MachineProfile>) -> bool {
        let selected = self.profiles.get(self.selected);
        let next = selected.and_then(|p| profiles.iter().position(|new| new.id == p.id));
        let unchanged = next.is_some_and(|i| selected == profiles.get(i));
        self.error = (selected.is_some() && !unchanged)
            .then(|| "The selected server changed. Review the list before connecting.".into());
        self.selected = next.unwrap_or_else(|| self.selected.min(profiles.len().saturating_sub(1)));
        self.profiles = profiles;
        unchanged
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
                self.replace_profiles(profiles);
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
                self.workspace = self
                    .connected
                    .as_ref()
                    .filter(|c| {
                        self.profiles
                            .get(self.selected)
                            .is_some_and(|p| p.target == c.target)
                    })
                    .and_then(|c| {
                        catalog
                            .workspaces
                            .iter()
                            .position(|w| Some(w.name.as_str()) == c.workspace.as_deref())
                    })
                    .unwrap_or(0);
                self.catalog = Some(catalog);
                self.error = None;
            }
            Err(error) => {
                self.error = Some(error.to_string());
                self.catalog = None;
            }
        }
        self.edit.is_none()
    }
    pub fn handle(&mut self, bytes: &[u8]) -> MachineAction {
        let bytes = self
            .edit
            .as_mut()
            .map_or(std::borrow::Cow::Borrowed(bytes), |e| e.join_input(bytes));
        let mut changed = false;
        let mut offset = 0;
        while offset < bytes.len() {
            let (key, used) = match &mut self.edit {
                Some(edit) => edit.decode(&bytes[offset..]),
                None => decode_key(&bytes, offset),
            };
            offset += used.max(1);
            if let Some(edit) = &mut self.edit {
                match key {
                    LineKey::Escape | LineKey::Cancel => self.edit = None,
                    LineKey::Enter => self.save_edit(),
                    key => {
                        if !edit.input(key) {
                            continue;
                        }
                    }
                }
                changed = true;
                continue;
            }
            if self.remove_confirm {
                changed = true;
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
                    let index = if key == LineKey::Up {
                        self.selected.saturating_sub(1)
                    } else {
                        (self.selected + 1).min(self.profiles.len().saturating_sub(1))
                    };
                    if index == self.selected {
                        continue;
                    }
                    self.selected = index;
                    self.probe = None;
                    self.catalog = None;
                }
                LineKey::Tab => {
                    let Some(c) = self.catalog.as_ref().filter(|c| c.workspaces.len() > 1) else {
                        continue;
                    };
                    self.workspace = (self.workspace + 1) % c.workspaces.len();
                }
                LineKey::Char('a') => {
                    self.edit = Some(Editor::new(None));
                }
                LineKey::Char('n') => {
                    if let Some(p) = self.profiles.get(self.selected) {
                        self.edit = Some(Editor::new(Some(p)));
                    } else {
                        continue;
                    }
                }
                LineKey::Char('d') => {
                    if self.profiles.is_empty() {
                        continue;
                    }
                    self.remove_confirm = true;
                }
                LineKey::Char('e') => {
                    let Some(p) = self.profiles.get(self.selected) else {
                        continue;
                    };
                    let mut p = p.clone();
                    p.enabled = !p.enabled;
                    self.update(MachineEvent::Saved(p));
                }
                LineKey::Char('r') => {
                    self.probe = None;
                    self.catalog = None;
                    if !self.reload() {
                        return MachineAction::Redraw;
                    }
                    if let Some(p) = self.profiles.get(self.selected).filter(|p| p.enabled) {
                        match Probe::start(&p.target, self.waker.clone()) {
                            Ok(probe) => self.probe = Some(probe),
                            Err(error) => self.error = Some(error.to_string()),
                        }
                    }
                }
                LineKey::Enter => {
                    if !self.reload() {
                        self.catalog = None;
                        self.probe = None;
                        return MachineAction::Redraw;
                    }
                    return self.selected_connection();
                }
                _ => continue,
            }
            changed = true;
        }
        if changed {
            MachineAction::Redraw
        } else {
            MachineAction::None
        }
    }

    fn selected_connection(&self) -> MachineAction {
        let Some(mut p) = self
            .profiles
            .get(self.selected)
            .filter(|p| p.enabled)
            .cloned()
        else {
            return MachineAction::None;
        };
        if let Some(w) = self
            .catalog
            .as_ref()
            .and_then(|c| c.workspaces.get(self.workspace))
        {
            p.workspace.clone_from(&w.name);
        }
        if self
            .connected
            .as_ref()
            .is_some_and(|c| c.target == p.target && c.workspace.as_deref() == Some(&p.workspace))
        {
            return MachineAction::Close;
        }
        MachineAction::Connect(p)
    }

    fn save_edit(&mut self) {
        let Some(edit) = &mut self.edit else { return };
        let Some(mut profile) = edit.profile() else {
            return;
        };
        let mut saved_id = 0;
        let result = catalog_path().and_then(|path| {
            update_at(&path, |latest, next| {
                if let Some(id) = edit.id {
                    let current = latest.iter().find(|p| p.id == id);
                    if current != self.profiles.iter().find(|p| p.id == id) {
                        return Err(io::Error::other(
                            "Server changed in another client; cancel and refresh",
                        ));
                    }
                    profile.enabled = current.is_none_or(|p| p.enabled);
                } else {
                    profile.id = next;
                }
                if latest
                    .iter()
                    .any(|p| p.id != profile.id && p.label.eq_ignore_ascii_case(&profile.label))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "A server already uses this label.",
                    ));
                }
                saved_id = profile.id;
                Ok(MachineEvent::Saved(profile))
            })
        });
        match result {
            Ok(profiles) => {
                self.selected = profiles.iter().position(|p| p.id == saved_id).unwrap_or(0);
                self.profiles = profiles;
                self.edit = None;
                self.error = None;
                self.probe = None;
                self.catalog = None;
                self.workspace = 0;
            }
            Err(error) => {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    edit.focus(0);
                }
                edit.reject(error.to_string());
            }
        }
    }

    fn overlay(&self, rows: u16) -> Overlay {
        let capacity = rows.saturating_sub(17).max(1) as usize;
        let start = self.selected.saturating_sub(capacity - 1);
        let mut lines = vec![
            match &self.connected {
                Some(c) => match &c.workspace {
                    Some(workspace) => {
                        format!("Connected to {} / {}", clean(&c.target), clean(workspace))
                    }
                    None => format!("Connected to {}", clean(&c.target)),
                },
                None => "Connected to Local".into(),
            },
            "Saved servers".into(),
            String::new(),
        ];
        let active = self
            .connected
            .as_ref()
            .and_then(|c| c.profile_index(&self.profiles));
        for (i, p) in self.profiles.iter().enumerate().skip(start).take(capacity) {
            lines.push(format!(
                "{} {}  {} / {}  [{}]",
                if i == self.selected { ">" } else { " " },
                clean(&p.label),
                clean(&p.target),
                clean(&p.workspace),
                match (active == Some(i), p.enabled) {
                    (true, true) => "connected",
                    (true, false) => "connected, disabled",
                    (false, true) => "saved",
                    (false, false) => "disabled",
                }
            ));
        }
        if self.profiles.is_empty() {
            lines.push("No saved Servers. Press a to add one.".into());
        }
        let profile_row = self.selected - start + 3;
        let mut focused_row = profile_row;
        let mut workspace_row = None;
        if self.probe.is_some() {
            focused_row = lines.len();
            lines.push("Connecting for discovery... Local remains available (l).".into());
        }
        if let Some(catalog) = &self.catalog {
            focused_row = lines.len();
            lines.push("Remote snapshot (Tab selects a Workspace; Enter connects)".into());
            for (i, w) in catalog
                .workspaces
                .iter()
                .enumerate()
                .skip(self.workspace.saturating_sub(4))
                .take(5)
            {
                if i == self.workspace {
                    focused_row = lines.len();
                    workspace_row = Some(focused_row);
                }
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
        if self.remove_confirm {
            focused_row = lines.len();
            lines.push("Forget selected Server? y confirms; remote sessions keep running.".into());
        }
        if let Some(error) = &self.error {
            focused_row = lines.len();
            lines.push(format!("Attention: {}", clean(error)));
        }
        let styles = lines
            .iter()
            .enumerate()
            .map(|(i, _)| {
                if self.error.is_some() && i == focused_row {
                    OverlayRowStyle::Error
                } else if Some(i) == workspace_row
                    || (!self.profiles.is_empty() && i == profile_row)
                {
                    OverlayRowStyle::CardSelected
                } else {
                    OverlayRowStyle::Plain
                }
            })
            .collect();
        let mut footer = Vec::new();
        if self.remove_confirm {
            footer.extend([("y", "Forget"), ("Esc", "Cancel")]);
        } else {
            if self.connected.is_some() {
                footer.push(("l", "Local"));
            }
            footer.push(("a", "Add"));
            if let Some(profile) = self.profiles.get(self.selected) {
                footer.push(("n", "Edit"));
                if profile.enabled {
                    footer.extend([("Enter", "Connect"), ("r", "Refresh")]);
                    if self
                        .catalog
                        .as_ref()
                        .is_some_and(|c| c.workspaces.len() > 1)
                    {
                        footer.push(("Tab", "Workspace"));
                    }
                }
                footer.extend([
                    ("e", if profile.enabled { "Disable" } else { "Enable" }),
                    ("d", "Remove"),
                ]);
            }
            footer.push(("Esc", "Close"));
        }
        Overlay::with_footer("Servers", lines, &footer)
            .with_focused_row(focused_row)
            .with_wrapped_footer()
            .with_row_styles(styles)
    }
    pub fn render(&self, cols: u16, rows: u16) -> Vec<u8> {
        let overlay = self.active_overlay(cols, rows);
        self.last_rect.set(Some(overlay.geometry(cols, rows)));
        overlay.render(cols, rows)
    }
    fn active_overlay(&self, cols: u16, rows: u16) -> Overlay {
        match &self.edit {
            Some(edit) => edit.form(cols, rows).overlay,
            None => self.overlay(rows),
        }
    }

    /// A smaller or moved dialog exposes underlying Pane cells. Ask the server
    /// for one background frame, then composite the current dialog over it.
    pub fn repaint(&self, cols: u16, rows: u16, server_out: &mut Vec<u8>) {
        let overlay = self.active_overlay(cols, rows);
        let rect = overlay.geometry(cols, rows);
        if self.last_rect.get().is_some_and(|old| old != rect) {
            server_out.extend(uniterm_proto::encode_frame(
                &uniterm_proto::ClientMessage::Refresh,
            ));
        }
        self.last_rect.set(Some(rect));
        crate::tty::write_sync_frame(&overlay.render(cols, rows));
    }

    pub fn click(&mut self, cols: u16, rows: u16, x: u16, y: u16) -> MachineAction {
        let form = self.edit.as_ref().map(|edit| edit.form(cols, rows));
        let list;
        let overlay = if let Some(form) = &form {
            &form.overlay
        } else {
            list = self.overlay(rows);
            &list
        };
        if let Some(key) = overlay.footer_key_at(cols, rows, x, y) {
            let bytes: &[u8] = match key {
                "Enter" => b"\r",
                "Esc" => b"\x1b",
                "Tab" => b"\t",
                key => key.as_bytes(),
            };
            return self.handle(bytes);
        }
        if let (Some(form), Some(edit)) = (form.as_ref(), self.edit.as_mut()) {
            if form
                .field_at(cols, rows, x, y)
                .is_some_and(|field| edit.focus(field))
            {
                return MachineAction::Redraw;
            }
            return MachineAction::None;
        }
        if self.remove_confirm {
            return MachineAction::None;
        }
        if let Some(row) = overlay.content_row_at(cols, rows, x, y) {
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
                return MachineAction::Redraw;
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
    fn discovery_drains_verbose_stderr_and_cancels_with_open_writers_before_retry() {
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(mio::Waker::new(poll.registry(), mio::Token(0)).unwrap());
        let (stdout, stdout_writer) = UnixStream::pair().unwrap();
        let (stderr, stderr_writer) = UnixStream::pair().unwrap();
        let inherited = [
            stdout_writer.try_clone().unwrap(),
            stderr_writer.try_clone().unwrap(),
        ];
        let mut hanging = Command::new("/bin/sh");
        hanging
            .args(["-c", "exec sleep 60"])
            .stdout(OwnedFd::from(stdout_writer))
            .stderr(OwnedFd::from(stderr_writer));
        let probe = Probe::spawn(hanging, [stdout, stderr], waker.clone()).unwrap();
        let (done, wait) = mpsc::channel();
        let cleanup = std::thread::spawn(move || {
            drop(probe);
            let _ = done.send(());
        });
        let finished = wait.recv_timeout(Duration::from_secs(2));
        drop(inherited);
        cleanup.join().unwrap();
        assert!(
            finished.is_ok(),
            "reader cleanup waited on an inherited writer"
        );

        let mut verbose = Command::new("/bin/sh");
        verbose.args([
            "-c",
            r#"i=0
while [ "$i" -lt 2048 ]; do
    printf 'ssh verbose diagnostics before authentication failure\n' >&2 || exit 1
    i=$((i+1))
done
printf 'Permission denied (fixture).\n' >&2
exit 255"#,
        ]);
        let retry = Probe::start_command(verbose, waker).unwrap();
        let result = retry.rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let error = result.unwrap_err().to_string();
        assert!(error.ends_with("Permission denied (fixture)."), "{error}");
        assert!(!error.contains("verbose diagnostics"));
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
            connected: None,
            selected: 0,
            error: None,
            edit: None,
            last_rect: Cell::default(),
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
        let overlay = view.overlay(12);
        let rect = overlay.geometry(30, 12);
        let target = (rect.y..rect.y + rect.h)
            .find_map(|y| {
                overlay
                    .content_row_at(30, 12, rect.x + 1, y)
                    .filter(|&row| overlay.lines[row].contains("agents"))
            })
            .expect("selected remote Workspace stays visible above wrapped controls");
        assert_eq!(overlay.row_style_at(target), OverlayRowStyle::CardSelected);
        assert!(String::from_utf8(overlay.render(30, 12))
            .unwrap()
            .contains("agents"));
        assert!(rendered.contains("Remote snapshot"));
        assert!(rendered.contains("agents"));
        assert!(
            matches!(view.selected_connection(), MachineAction::Connect(p) if p.workspace == "agents")
        );
        assert!(matches!(view.handle(b"l"), MachineAction::Local));
        assert!(view.timeout(Instant::now()).is_none());
    }

    #[test]
    fn active_server_identity_survives_aliases_without_marking_ambiguous_profiles() {
        let profile = |id, workspace: &str| MachineProfile {
            id,
            label: id.to_string(),
            target: "build".into(),
            workspace: workspace.into(),
            enabled: true,
        };
        let profiles = vec![profile(1, "dev"), profile(2, "prod"), profile(3, "prod")];
        let mut current = ConnectedMachine {
            profile: None,
            workspace: Some("prod".into()),
            target: "build".into(),
        };
        assert_eq!(current.profile_index(&profiles), None);
        assert_eq!(current.profile_index(&profiles[..2]), Some(1));
        current.profile = Some(3);
        assert_eq!(current.profile_index(&profiles), Some(2));
        current.workspace = Some("another-workspace".into());
        assert_eq!(current.profile_index(&profiles), Some(2));
        assert_eq!(current.profile_index(&profiles[..2]), None);
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
