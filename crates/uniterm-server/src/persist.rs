//! Session persistence: atomic snapshots of the session tree (the built-in
//! resurrect/continuum, `docs/05`). It persists structure, per-pane cwd, and
//! resolved grapheme-aware grid and scrollback lines.
//!
//! Q2 decision: a bincode snapshot file written atomically (temp + rename), plus
//! a custom append-only event log. No rusqlite for now.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthChar as _;
use uniterm_core::{Attrs, Color, LayoutNode, PaneId, ProjectId, StoredCell, StoredLine};

/// Current snapshot schema version (bump on incompatible changes).
/// v12: Checkpoint the typed artifact ledger with its own event cursor.
/// v11: Checkpoint the native run graph with its own applied event cursor.
/// v10: Retain the last event-log sequence included by this checkpoint.
/// v9: Persist extended underline colours alongside the existing cell styles.
/// v8: Panes retain their launch arguments and provider-owned native resume
/// profile, so restore does not have to infer process identity from chrome.
const VERSION: u32 = 12;

/// Max scrollback+screen lines persisted per pane (bounds the snapshot size).
pub const CONTENT_LINE_CAP: usize = 1000;

#[derive(Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    /// Last event sequence reflected by the structural projection.
    pub event_sequence: u64,
    /// Last event sequence reflected by `run_graph`.
    pub run_graph_sequence: u64,
    /// Bounded current run projection. The event log remains authoritative.
    pub run_graph: uniterm_core::RunGraph,
    /// Last event sequence reflected by `artifacts`.
    pub artifact_sequence: u64,
    /// Bounded current artifact projection. The event log remains authoritative.
    pub artifacts: uniterm_core::ArtifactLedger,
    pub active_window: usize,
    pub next_pane_id: u64,
    pub active_project: ProjectId,
    pub next_project_id: u64,
    pub projects: Vec<ProjectSnap>,
    pub windows: Vec<WinSnap>,
}

#[derive(Deserialize, Serialize)]
struct LegacySnapshotV11 {
    version: u32,
    event_sequence: u64,
    run_graph_sequence: u64,
    run_graph: uniterm_core::RunGraph,
    active_window: usize,
    next_pane_id: u64,
    active_project: ProjectId,
    next_project_id: u64,
    projects: Vec<ProjectSnap>,
    windows: Vec<WinSnap>,
}

#[derive(Deserialize, Serialize)]
struct LegacySnapshotV10 {
    version: u32,
    event_sequence: u64,
    active_window: usize,
    next_pane_id: u64,
    active_project: ProjectId,
    next_project_id: u64,
    projects: Vec<ProjectSnap>,
    windows: Vec<WinSnap>,
}

#[derive(Deserialize, Serialize)]
struct LegacySnapshotV9 {
    version: u32,
    active_window: usize,
    next_pane_id: u64,
    active_project: ProjectId,
    next_project_id: u64,
    projects: Vec<ProjectSnap>,
    windows: Vec<WinSnap>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ProjectSnap {
    pub id: ProjectId,
    pub name: String,
    pub root: String,
    pub active_pane: Option<PaneId>,
    pub metadata: Vec<(String, String)>,
}

#[derive(Deserialize, Serialize)]
struct LegacySnapshotV6 {
    version: u32,
    active_window: usize,
    next_pane_id: u64,
    active_project: ProjectId,
    next_project_id: u64,
    projects: Vec<LegacyProjectSnapV6>,
    windows: Vec<LegacyWinSnapV7>,
}

#[derive(Deserialize, Serialize)]
struct LegacyProjectSnapV6 {
    id: ProjectId,
    name: String,
    root: String,
    metadata: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize)]
pub struct WinSnap {
    pub project: ProjectId,
    pub layout: LayoutNode,
    pub active: PaneId,
    pub zoomed: Option<PaneId>,
    /// User-given window name (rename tab), if any.
    pub name: Option<String>,
    pub panes: Vec<PaneSnap>,
}

#[derive(Deserialize, Serialize)]
struct LegacySnapshotV5 {
    version: u32,
    active_window: usize,
    next_pane_id: u64,
    windows: Vec<LegacyWinSnapV5>,
}

#[derive(Deserialize, Serialize)]
struct LegacyWinSnapV5 {
    layout: LayoutNode,
    active: PaneId,
    zoomed: Option<PaneId>,
    name: Option<String>,
    panes: Vec<LegacyPaneSnapV5>,
}

#[derive(Deserialize, Serialize)]
struct LegacyPaneSnapV5 {
    id: PaneId,
    cwd: Option<String>,
    content: Vec<LegacyStoredLineV8>,
}

#[derive(Serialize, Deserialize)]
pub struct PaneSnap {
    pub id: PaneId,
    pub cwd: Option<String>,
    /// Saved grid + scrollback content (P2-2), replayed into scrollback on
    /// restore. `None` for older snapshots.
    #[serde(default)]
    pub content: Vec<StoredLine>,
    pub metadata: Vec<(String, String)>,
    /// Arguments originally passed to Uniterm's configured shell/program.
    #[serde(default)]
    pub launch_args: Vec<String>,
    /// Native provider session identity and opaque resume command, when the
    /// provider supplied one cooperatively.
    #[serde(default)]
    pub agent_launch: Option<AgentLaunchSnap>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentLaunchSnap {
    pub provider: String,
    pub session_id: Option<String>,
    /// Complete argv, including the provider executable. Uniterm never
    /// manufactures provider-specific flags outside the provider boundary.
    pub resume_command: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct LegacySnapshotV8 {
    version: u32,
    active_window: usize,
    next_pane_id: u64,
    active_project: ProjectId,
    next_project_id: u64,
    projects: Vec<ProjectSnap>,
    windows: Vec<LegacyWinSnapV8>,
}

#[derive(Deserialize, Serialize)]
struct LegacyWinSnapV8 {
    project: ProjectId,
    layout: LayoutNode,
    active: PaneId,
    zoomed: Option<PaneId>,
    name: Option<String>,
    panes: Vec<LegacyPaneSnapV8>,
}

#[derive(Deserialize, Serialize)]
struct LegacyPaneSnapV8 {
    id: PaneId,
    cwd: Option<String>,
    content: Vec<LegacyStoredLineV8>,
    metadata: Vec<(String, String)>,
    launch_args: Vec<String>,
    agent_launch: Option<AgentLaunchSnap>,
}

#[derive(Clone, Deserialize, Serialize)]
struct LegacyStoredLineV8 {
    cells: Vec<LegacyStoredCellV8>,
    wrapped: bool,
}

#[derive(Clone, Deserialize, Serialize)]
struct LegacyStoredCellV8 {
    text: String,
    fg: Color,
    bg: Color,
    attrs: Attrs,
    width: u8,
    continuation: bool,
}

#[derive(Deserialize, Serialize)]
struct LegacySnapshotV7 {
    version: u32,
    active_window: usize,
    next_pane_id: u64,
    active_project: ProjectId,
    next_project_id: u64,
    projects: Vec<ProjectSnap>,
    windows: Vec<LegacyWinSnapV7>,
}

#[derive(Deserialize, Serialize)]
struct LegacyWinSnapV7 {
    project: ProjectId,
    layout: LayoutNode,
    active: PaneId,
    zoomed: Option<PaneId>,
    name: Option<String>,
    panes: Vec<LegacyPaneSnapV7>,
}

#[derive(Deserialize, Serialize)]
struct LegacyPaneSnapV7 {
    id: PaneId,
    cwd: Option<String>,
    content: Vec<LegacyStoredLineV8>,
    metadata: Vec<(String, String)>,
}

#[derive(Deserialize, Serialize)]
struct LegacySnapshotV4 {
    version: u32,
    active_window: usize,
    next_pane_id: u64,
    windows: Vec<LegacyWinSnapV4>,
}

#[derive(Deserialize, Serialize)]
struct LegacyWinSnapV4 {
    layout: LayoutNode,
    active: PaneId,
    zoomed: Option<PaneId>,
    name: Option<String>,
    panes: Vec<LegacyPaneSnapV4>,
}

#[derive(Deserialize, Serialize)]
struct LegacyPaneSnapV4 {
    id: PaneId,
    cwd: Option<String>,
    content: Vec<Vec<LegacyCellV4>>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
struct LegacyAttrsV4(u8);

#[derive(Clone, Copy, Deserialize, Serialize)]
struct LegacyCellV4 {
    ch: char,
    fg: Color,
    bg: Color,
    attrs: LegacyAttrsV4,
}

impl Snapshot {
    pub fn new(
        active_window: usize,
        next_pane_id: u64,
        active_project: ProjectId,
        next_project_id: u64,
        projects: Vec<ProjectSnap>,
        windows: Vec<WinSnap>,
    ) -> Self {
        Self::new_with_sequence(
            active_window,
            next_pane_id,
            active_project,
            next_project_id,
            projects,
            windows,
            0,
        )
    }

    pub fn new_with_sequence(
        active_window: usize,
        next_pane_id: u64,
        active_project: ProjectId,
        next_project_id: u64,
        projects: Vec<ProjectSnap>,
        windows: Vec<WinSnap>,
        event_sequence: u64,
    ) -> Self {
        Snapshot {
            version: VERSION,
            event_sequence,
            run_graph_sequence: 0,
            run_graph: uniterm_core::RunGraph::new(),
            artifact_sequence: 0,
            artifacts: uniterm_core::ArtifactLedger::new(),
            active_window,
            next_pane_id,
            active_project,
            next_project_id,
            projects,
            windows,
        }
    }
}

/// `$XDG_STATE_HOME/uniterm/` (or `~/.local/state/uniterm/`).
pub(crate) fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("uniterm");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local").join("state").join("uniterm")
}

pub(crate) fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

pub(crate) fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    sync_file_for_crash(&file)
}

/// Flush one durability checkpoint strongly enough for sudden power loss.
///
/// `sync_all` is the portable baseline. macOS additionally exposes
/// `F_FULLFSYNC`, which asks the storage device itself to commit its cache and
/// is therefore the appropriate boundary for a crash-recovery snapshot.
pub(crate) fn sync_file_for_crash(file: &std::fs::File) -> std::io::Result<()> {
    file.sync_all()?;
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd as _;

        // SAFETY: `file` owns a valid descriptor for the duration of this
        // call, and F_FULLFSYNC takes no pointer arguments.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } < 0 {
            let error = std::io::Error::last_os_error();
            if !matches!(
                error.raw_os_error(),
                Some(libc::EINVAL | libc::ENOTSUP | libc::ENOTTY)
            ) {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    match std::fs::File::open(parent).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EINVAL | libc::ENOTSUP | libc::EPERM | libc::EROFS)
            ) =>
        {
            // Some sandboxed and network filesystems reject directory fsync.
            // The fully synced snapshot file and atomic rename still provide
            // the strongest checkpoint that filesystem exposes.
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub fn snapshot_path(name: &str) -> PathBuf {
    state_dir().join(format!("{name}.snap"))
}

/// The sidecar that a running server, or a maintenance command, locks to own
/// the Workspace's durable files. It sits beside them so ownership follows
/// the state directory rather than the runtime directory (see
/// `server::WorkspaceLock`).
pub fn lock_path(name: &str) -> PathBuf {
    state_dir().join("locks").join(format!("{name}.lock"))
}

/// Move a dormant Workspace snapshot without decoding or rewriting it. Live
/// Workspace renames go through the server so its socket and projection move
/// together; this helper is only for conflict-safe migration archives.
pub fn rename(old: &str, new: &str) -> std::io::Result<()> {
    std::fs::rename(snapshot_path(old), snapshot_path(new))
}

/// Write a snapshot atomically (temp file + rename) so a crash mid-write never
/// corrupts the current snapshot.
pub fn save(name: &str, snap: &Snapshot) -> std::io::Result<()> {
    save_bytes(name, &serialize(snap)?)
}

/// Serialize the immutable core projection before it crosses the runtime
/// seam. Kept separate from [`save_bytes`] so filesystem work can live solely
/// on the tokio side during normal server operation.
pub fn serialize(snap: &Snapshot) -> std::io::Result<Vec<u8>> {
    bincode::serialize(snap).map_err(|error| std::io::Error::other(error.to_string()))
}

/// Atomically persist bytes prepared by [`serialize`].
pub fn save_bytes(name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let path = snapshot_path(name);
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    let tmp = path.with_extension("snap.tmp");
    write_private(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    sync_parent_directory(&path)
}

/// Load a snapshot for `name`, if a compatible one exists.
pub fn load(name: &str) -> Option<Snapshot> {
    let bytes = std::fs::read(snapshot_path(name)).ok()?;
    decode_snapshot(&bytes)
}

fn decode_snapshot(bytes: &[u8]) -> Option<Snapshot> {
    if let Ok(snap) = bincode::deserialize::<Snapshot>(bytes) {
        if snap.version == VERSION && !snap.windows.is_empty() && !snap.projects.is_empty() {
            return Some(snap);
        }
    }
    if let Ok(legacy) = bincode::deserialize::<LegacySnapshotV11>(bytes) {
        if legacy.version == 11 && !legacy.windows.is_empty() && !legacy.projects.is_empty() {
            return Some(Snapshot {
                version: VERSION,
                event_sequence: legacy.event_sequence,
                run_graph_sequence: legacy.run_graph_sequence,
                run_graph: legacy.run_graph,
                artifact_sequence: 0,
                artifacts: uniterm_core::ArtifactLedger::new(),
                active_window: legacy.active_window,
                next_pane_id: legacy.next_pane_id,
                active_project: legacy.active_project,
                next_project_id: legacy.next_project_id,
                projects: legacy.projects,
                windows: legacy.windows,
            });
        }
    }
    if let Ok(legacy) = bincode::deserialize::<LegacySnapshotV10>(bytes) {
        if legacy.version == 10 && !legacy.windows.is_empty() && !legacy.projects.is_empty() {
            return Some(Snapshot {
                version: VERSION,
                event_sequence: legacy.event_sequence,
                run_graph_sequence: 0,
                run_graph: uniterm_core::RunGraph::new(),
                artifact_sequence: 0,
                artifacts: uniterm_core::ArtifactLedger::new(),
                active_window: legacy.active_window,
                next_pane_id: legacy.next_pane_id,
                active_project: legacy.active_project,
                next_project_id: legacy.next_project_id,
                projects: legacy.projects,
                windows: legacy.windows,
            });
        }
    }
    if let Ok(legacy) = bincode::deserialize::<LegacySnapshotV9>(bytes) {
        if legacy.version == 9 && !legacy.windows.is_empty() && !legacy.projects.is_empty() {
            return Some(Snapshot {
                version: VERSION,
                event_sequence: 0,
                run_graph_sequence: 0,
                run_graph: uniterm_core::RunGraph::new(),
                artifact_sequence: 0,
                artifacts: uniterm_core::ArtifactLedger::new(),
                active_window: legacy.active_window,
                next_pane_id: legacy.next_pane_id,
                active_project: legacy.active_project,
                next_project_id: legacy.next_project_id,
                projects: legacy.projects,
                windows: legacy.windows,
            });
        }
    }
    if let Ok(legacy) = bincode::deserialize::<LegacySnapshotV8>(bytes) {
        if legacy.version == 8 && !legacy.windows.is_empty() && !legacy.projects.is_empty() {
            return Some(Snapshot {
                version: VERSION,
                event_sequence: 0,
                run_graph_sequence: 0,
                run_graph: uniterm_core::RunGraph::new(),
                artifact_sequence: 0,
                artifacts: uniterm_core::ArtifactLedger::new(),
                active_window: legacy.active_window,
                next_pane_id: legacy.next_pane_id,
                active_project: legacy.active_project,
                next_project_id: legacy.next_project_id,
                projects: legacy.projects,
                windows: migrate_v8_windows(legacy.windows),
            });
        }
    }
    if let Ok(legacy) = bincode::deserialize::<LegacySnapshotV7>(bytes) {
        if legacy.version == 7 && !legacy.windows.is_empty() && !legacy.projects.is_empty() {
            return Some(Snapshot {
                version: VERSION,
                event_sequence: 0,
                run_graph_sequence: 0,
                run_graph: uniterm_core::RunGraph::new(),
                artifact_sequence: 0,
                artifacts: uniterm_core::ArtifactLedger::new(),
                active_window: legacy.active_window,
                next_pane_id: legacy.next_pane_id,
                active_project: legacy.active_project,
                next_project_id: legacy.next_project_id,
                projects: legacy.projects,
                windows: migrate_v7_windows(legacy.windows),
            });
        }
    }
    if let Ok(legacy) = bincode::deserialize::<LegacySnapshotV6>(bytes) {
        if legacy.version == 6 && !legacy.windows.is_empty() && !legacy.projects.is_empty() {
            return Some(Snapshot {
                version: VERSION,
                event_sequence: 0,
                run_graph_sequence: 0,
                run_graph: uniterm_core::RunGraph::new(),
                artifact_sequence: 0,
                artifacts: uniterm_core::ArtifactLedger::new(),
                active_window: legacy.active_window,
                next_pane_id: legacy.next_pane_id,
                active_project: legacy.active_project,
                next_project_id: legacy.next_project_id,
                projects: legacy
                    .projects
                    .into_iter()
                    .map(|project| ProjectSnap {
                        active_pane: legacy
                            .windows
                            .iter()
                            .find(|tab| tab.project == project.id)
                            .map(|tab| tab.active),
                        id: project.id,
                        name: project.name,
                        root: project.root,
                        metadata: project.metadata,
                    })
                    .collect(),
                windows: migrate_v7_windows(legacy.windows),
            });
        }
    }
    if let Ok(legacy) = bincode::deserialize::<LegacySnapshotV5>(bytes) {
        if legacy.version == 5 && !legacy.windows.is_empty() {
            return Some(migrate_v5(legacy));
        }
    }
    let legacy = bincode::deserialize::<LegacySnapshotV4>(bytes).ok()?;
    if legacy.version != 4 || legacy.windows.is_empty() {
        return None;
    }
    Some(Snapshot {
        version: VERSION,
        event_sequence: 0,
        run_graph_sequence: 0,
        run_graph: uniterm_core::RunGraph::new(),
        artifact_sequence: 0,
        artifacts: uniterm_core::ArtifactLedger::new(),
        active_window: legacy.active_window,
        next_pane_id: legacy.next_pane_id,
        active_project: ProjectId(1),
        next_project_id: 2,
        projects: vec![legacy_project()],
        windows: legacy
            .windows
            .into_iter()
            .map(|window| WinSnap {
                project: ProjectId(1),
                layout: window.layout,
                active: window.active,
                zoomed: window.zoomed,
                name: window.name,
                panes: window
                    .panes
                    .into_iter()
                    .map(|pane| PaneSnap {
                        id: pane.id,
                        cwd: pane.cwd,
                        content: pane.content.into_iter().map(migrate_legacy_line).collect(),
                        metadata: Vec::new(),
                        launch_args: Vec::new(),
                        agent_launch: None,
                    })
                    .collect(),
            })
            .collect(),
    })
}

fn migrate_v5(legacy: LegacySnapshotV5) -> Snapshot {
    Snapshot {
        version: VERSION,
        event_sequence: 0,
        run_graph_sequence: 0,
        run_graph: uniterm_core::RunGraph::new(),
        artifact_sequence: 0,
        artifacts: uniterm_core::ArtifactLedger::new(),
        active_window: legacy.active_window,
        next_pane_id: legacy.next_pane_id,
        active_project: ProjectId(1),
        next_project_id: 2,
        projects: vec![legacy_project()],
        windows: legacy
            .windows
            .into_iter()
            .map(|window| WinSnap {
                project: ProjectId(1),
                layout: window.layout,
                active: window.active,
                zoomed: window.zoomed,
                name: window.name,
                panes: window
                    .panes
                    .into_iter()
                    .map(|pane| PaneSnap {
                        id: pane.id,
                        cwd: pane.cwd,
                        content: migrate_stored_lines_v8(pane.content),
                        metadata: Vec::new(),
                        launch_args: Vec::new(),
                        agent_launch: None,
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn migrate_v7_windows(windows: Vec<LegacyWinSnapV7>) -> Vec<WinSnap> {
    windows
        .into_iter()
        .map(|window| WinSnap {
            project: window.project,
            layout: window.layout,
            active: window.active,
            zoomed: window.zoomed,
            name: window.name,
            panes: window
                .panes
                .into_iter()
                .map(|pane| PaneSnap {
                    id: pane.id,
                    cwd: pane.cwd,
                    content: migrate_stored_lines_v8(pane.content),
                    metadata: pane.metadata,
                    launch_args: Vec::new(),
                    agent_launch: None,
                })
                .collect(),
        })
        .collect()
}

fn migrate_v8_windows(windows: Vec<LegacyWinSnapV8>) -> Vec<WinSnap> {
    windows
        .into_iter()
        .map(|window| WinSnap {
            project: window.project,
            layout: window.layout,
            active: window.active,
            zoomed: window.zoomed,
            name: window.name,
            panes: window
                .panes
                .into_iter()
                .map(|pane| PaneSnap {
                    id: pane.id,
                    cwd: pane.cwd,
                    content: migrate_stored_lines_v8(pane.content),
                    metadata: pane.metadata,
                    launch_args: pane.launch_args,
                    agent_launch: pane.agent_launch,
                })
                .collect(),
        })
        .collect()
}

fn migrate_stored_lines_v8(lines: Vec<LegacyStoredLineV8>) -> Vec<StoredLine> {
    lines
        .into_iter()
        .map(|line| StoredLine {
            cells: line
                .cells
                .into_iter()
                .map(|cell| StoredCell {
                    text: cell.text,
                    fg: cell.fg,
                    bg: cell.bg,
                    attrs: cell.attrs,
                    underline_color: Color::Default,
                    width: cell.width,
                    continuation: cell.continuation,
                })
                .collect(),
            wrapped: line.wrapped,
        })
        .collect()
}

fn legacy_project() -> ProjectSnap {
    ProjectSnap {
        id: ProjectId(1),
        name: "Default".into(),
        root: String::new(),
        active_pane: None,
        metadata: Vec::new(),
    }
}

fn migrate_legacy_line(line: Vec<LegacyCellV4>) -> StoredLine {
    let mut cells = Vec::with_capacity(line.len());
    let mut x = 0;
    while x < line.len() {
        let cell = line[x];
        let width = cell.ch.width().unwrap_or(0).clamp(1, 2) as u8;
        cells.push(StoredCell {
            text: cell.ch.to_string(),
            fg: cell.fg,
            bg: cell.bg,
            attrs: Attrs(cell.attrs.0 as u16),
            underline_color: Color::Default,
            width,
            continuation: false,
        });
        if width == 2 && x + 1 < line.len() {
            cells.push(StoredCell {
                text: String::new(),
                fg: cell.fg,
                bg: cell.bg,
                attrs: Attrs(cell.attrs.0 as u16),
                underline_color: Color::Default,
                width: 0,
                continuation: true,
            });
            x += 2;
        } else {
            x += 1;
        }
    }
    StoredLine {
        cells,
        wrapped: false,
    }
}

/// Delete a session's snapshot (on a clean shutdown - the session ended on
/// purpose, so it should not be resurrected next time).
pub fn delete(name: &str) {
    let _ = std::fs::remove_file(snapshot_path(name));
}

/// Move a snapshot that recovery rejected to a timestamped `.snap.corrupt-*`
/// sibling instead of deleting it, so nothing is lost while the Workspace
/// restarts from its catalog definition. Returns the backup path, or `None`
/// when no snapshot existed.
pub fn quarantine(name: &str) -> std::io::Result<Option<PathBuf>> {
    let path = snapshot_path(name);
    if !path.exists() {
        return Ok(None);
    }
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup = path.with_extension(format!("snap.corrupt-{suffix}"));
    std::fs::rename(&path, &backup)?;
    Ok(Some(backup))
}

/// Whether a path exists (helper kept here so callers don't import fs).
pub fn exists(name: &str) -> bool {
    Path::new(&snapshot_path(name)).exists()
}

/// Enumerate Workspace names with crash-recovery snapshots so bulk cleanup
/// also covers state whose catalog record was lost or damaged.
pub fn list_names() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(state_dir()) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("snap") {
            if let Some(name) = path.file_stem().and_then(|value| value.to_str()) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_directories_and_files_are_owner_only() {
        let dir =
            std::env::temp_dir().join(format!("uniterm-private-state-{}", std::process::id()));
        let file = dir.join("events.log");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        open_private_append(&file)
            .unwrap()
            .write_all(b"event\n")
            .unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    use uniterm_core::SplitDir;

    #[test]
    fn snapshot_round_trips() {
        let mut layout = LayoutNode::Leaf(PaneId(1));
        layout.split(PaneId(1), SplitDir::Horizontal, PaneId(2));
        let snap = Snapshot::new_with_sequence(
            0,
            3,
            ProjectId(1),
            2,
            vec![ProjectSnap {
                id: ProjectId(1),
                name: "Uniterm".into(),
                root: "/tmp/uniterm".into(),
                active_pane: Some(PaneId(2)),
                metadata: Vec::new(),
            }],
            vec![WinSnap {
                project: ProjectId(1),
                layout,
                active: PaneId(2),
                zoomed: None,
                name: Some("build".into()),
                panes: vec![
                    PaneSnap {
                        id: PaneId(1),
                        cwd: Some("/tmp".into()),
                        content: vec![StoredLine {
                            cells: vec![StoredCell {
                                text: "x".into(),
                                fg: uniterm_core::Color::Default,
                                bg: uniterm_core::Color::Default,
                                attrs: uniterm_core::Attrs::NONE,
                                underline_color: uniterm_core::Color::Rgb(10, 20, 30),
                                width: 1,
                                continuation: false,
                            }],
                            wrapped: false,
                        }],
                        metadata: Vec::new(),
                        launch_args: vec!["-l".into()],
                        agent_launch: Some(AgentLaunchSnap {
                            provider: "codex".into(),
                            session_id: Some("session-1".into()),
                            resume_command: vec![
                                "codex".into(),
                                "resume".into(),
                                "session-1".into(),
                            ],
                        }),
                    },
                    PaneSnap {
                        id: PaneId(2),
                        cwd: None,
                        content: vec![],
                        metadata: Vec::new(),
                        launch_args: Vec::new(),
                        agent_launch: None,
                    },
                ],
            }],
            42,
        );
        let mut snap = snap;
        snap.run_graph
            .apply(uniterm_core::RunGraphEvent::Created {
                run: uniterm_core::RunId(1),
                parent: None,
                project: ProjectId(1),
                kind: uniterm_core::RunKind::Workflow,
                task_id: 8,
                title: "persisted run".into(),
            })
            .unwrap();
        snap.run_graph_sequence = 41;
        snap.artifacts
            .apply(uniterm_core::ArtifactEvent::Observed {
                artifact: uniterm_core::ArtifactId(1),
                project: ProjectId(1),
                producer_run: uniterm_core::RunId(1),
                producer_role: uniterm_core::RoleId(1),
                kind: uniterm_core::ArtifactKind::Plan,
                path: "WORKFLOW_PLAN.md".into(),
                digest: "a".repeat(64),
                size: 12,
            })
            .unwrap();
        snap.artifact_sequence = 42;
        let bytes = bincode::serialize(&snap).unwrap();
        let back: Snapshot = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.version, VERSION);
        assert_eq!(back.event_sequence, 42);
        assert_eq!(back.run_graph_sequence, 41);
        assert_eq!(back.artifact_sequence, 42);
        assert_eq!(
            back.artifacts
                .artifact(uniterm_core::ArtifactId(1))
                .unwrap()
                .path,
            "WORKFLOW_PLAN.md"
        );
        assert_eq!(
            back.run_graph.run(uniterm_core::RunId(1)).unwrap().title,
            "persisted run"
        );
        assert_eq!(back.next_pane_id, 3);
        assert_eq!(back.windows.len(), 1);
        assert_eq!(back.projects[0].name, "Uniterm");
        assert_eq!(back.projects[0].active_pane, Some(PaneId(2)));
        assert_eq!(back.windows[0].project, ProjectId(1));
        assert_eq!(back.windows[0].active, PaneId(2));
        assert_eq!(back.windows[0].name.as_deref(), Some("build"));
        assert_eq!(back.windows[0].layout.pane_ids().len(), 2);
        assert_eq!(back.windows[0].panes[0].cwd.as_deref(), Some("/tmp"));
        assert_eq!(back.windows[0].panes[0].content[0].cells[0].text, "x");
        assert_eq!(
            back.windows[0].panes[0].content[0].cells[0].underline_color,
            Color::Rgb(10, 20, 30)
        );
    }

    #[test]
    fn v9_snapshot_migrates_with_an_empty_event_cursor() {
        let legacy = LegacySnapshotV9 {
            version: 9,
            active_window: 0,
            next_pane_id: 2,
            active_project: ProjectId(1),
            next_project_id: 2,
            projects: vec![ProjectSnap {
                id: ProjectId(1),
                name: "Legacy v9".into(),
                root: "/tmp".into(),
                active_pane: Some(PaneId(1)),
                metadata: Vec::new(),
            }],
            windows: vec![WinSnap {
                project: ProjectId(1),
                layout: LayoutNode::Leaf(PaneId(1)),
                active: PaneId(1),
                zoomed: None,
                name: None,
                panes: vec![PaneSnap {
                    id: PaneId(1),
                    cwd: None,
                    content: Vec::new(),
                    metadata: Vec::new(),
                    launch_args: Vec::new(),
                    agent_launch: None,
                }],
            }],
        };
        let migrated = decode_snapshot(&bincode::serialize(&legacy).unwrap()).unwrap();
        assert_eq!(migrated.version, VERSION);
        assert_eq!(migrated.event_sequence, 0);
        assert_eq!(migrated.projects[0].name, "Legacy v9");
    }

    #[test]
    fn v10_snapshot_retains_its_structural_cursor_and_starts_an_empty_graph() {
        let legacy = LegacySnapshotV10 {
            version: 10,
            event_sequence: 44,
            active_window: 0,
            next_pane_id: 2,
            active_project: ProjectId(1),
            next_project_id: 2,
            projects: vec![ProjectSnap {
                id: ProjectId(1),
                name: "Legacy v10".into(),
                root: "/tmp".into(),
                active_pane: Some(PaneId(1)),
                metadata: Vec::new(),
            }],
            windows: vec![WinSnap {
                project: ProjectId(1),
                layout: LayoutNode::Leaf(PaneId(1)),
                active: PaneId(1),
                zoomed: None,
                name: None,
                panes: vec![PaneSnap {
                    id: PaneId(1),
                    cwd: None,
                    content: Vec::new(),
                    metadata: Vec::new(),
                    launch_args: Vec::new(),
                    agent_launch: None,
                }],
            }],
        };
        let migrated = decode_snapshot(&bincode::serialize(&legacy).unwrap()).unwrap();
        assert_eq!(migrated.version, VERSION);
        assert_eq!(migrated.event_sequence, 44);
        assert_eq!(migrated.run_graph_sequence, 0);
        assert_eq!(migrated.run_graph.runs().count(), 0);
    }

    #[test]
    fn v11_snapshot_retains_its_graph_and_starts_an_empty_artifact_ledger() {
        let mut run_graph = uniterm_core::RunGraph::new();
        run_graph
            .apply(uniterm_core::RunGraphEvent::Created {
                run: uniterm_core::RunId(1),
                parent: None,
                project: ProjectId(1),
                kind: uniterm_core::RunKind::Workflow,
                task_id: 9,
                title: "legacy graph".into(),
            })
            .unwrap();
        let legacy = LegacySnapshotV11 {
            version: 11,
            event_sequence: 51,
            run_graph_sequence: 49,
            run_graph,
            active_window: 0,
            next_pane_id: 2,
            active_project: ProjectId(1),
            next_project_id: 2,
            projects: vec![ProjectSnap {
                id: ProjectId(1),
                name: "Legacy v11".into(),
                root: "/tmp".into(),
                active_pane: Some(PaneId(1)),
                metadata: Vec::new(),
            }],
            windows: vec![WinSnap {
                project: ProjectId(1),
                layout: LayoutNode::Leaf(PaneId(1)),
                active: PaneId(1),
                zoomed: None,
                name: None,
                panes: vec![PaneSnap {
                    id: PaneId(1),
                    cwd: None,
                    content: Vec::new(),
                    metadata: Vec::new(),
                    launch_args: Vec::new(),
                    agent_launch: None,
                }],
            }],
        };
        let migrated = decode_snapshot(&bincode::serialize(&legacy).unwrap()).unwrap();
        assert_eq!(migrated.version, VERSION);
        assert_eq!(migrated.event_sequence, 51);
        assert_eq!(migrated.run_graph_sequence, 49);
        assert_eq!(
            migrated
                .run_graph
                .run(uniterm_core::RunId(1))
                .unwrap()
                .title,
            "legacy graph"
        );
        assert_eq!(migrated.artifact_sequence, 0);
        assert_eq!(migrated.artifacts.artifacts().count(), 0);
    }

    #[test]
    fn v8_snapshot_migrates_with_default_underline_colour() {
        let legacy = LegacySnapshotV8 {
            version: 8,
            active_window: 0,
            next_pane_id: 2,
            active_project: ProjectId(1),
            next_project_id: 2,
            projects: vec![ProjectSnap {
                id: ProjectId(1),
                name: "Legacy".into(),
                root: "/tmp".into(),
                active_pane: Some(PaneId(1)),
                metadata: Vec::new(),
            }],
            windows: vec![LegacyWinSnapV8 {
                project: ProjectId(1),
                layout: LayoutNode::Leaf(PaneId(1)),
                active: PaneId(1),
                zoomed: None,
                name: None,
                panes: vec![LegacyPaneSnapV8 {
                    id: PaneId(1),
                    cwd: None,
                    content: vec![LegacyStoredLineV8 {
                        cells: vec![LegacyStoredCellV8 {
                            text: "x".into(),
                            fg: Color::Default,
                            bg: Color::Default,
                            attrs: Attrs::UNDERLINE,
                            width: 1,
                            continuation: false,
                        }],
                        wrapped: false,
                    }],
                    metadata: Vec::new(),
                    launch_args: vec!["-l".into()],
                    agent_launch: None,
                }],
            }],
        };
        let migrated = decode_snapshot(&bincode::serialize(&legacy).unwrap()).unwrap();
        let cell = &migrated.windows[0].panes[0].content[0].cells[0];
        assert_eq!(migrated.version, VERSION);
        assert_eq!(
            cell.attrs.underline_style(),
            uniterm_core::UnderlineStyle::Single
        );
        assert_eq!(cell.underline_color, Color::Default);
        assert_eq!(migrated.windows[0].panes[0].launch_args, ["-l"]);
    }

    #[test]
    fn v6_project_focus_is_migrated_from_its_first_tab() {
        let legacy = LegacySnapshotV6 {
            version: 6,
            active_window: 0,
            next_pane_id: 2,
            active_project: ProjectId(7),
            next_project_id: 8,
            projects: vec![LegacyProjectSnapV6 {
                id: ProjectId(7),
                name: "Legacy".into(),
                root: "/tmp".into(),
                metadata: Vec::new(),
            }],
            windows: vec![LegacyWinSnapV7 {
                project: ProjectId(7),
                layout: LayoutNode::Leaf(PaneId(1)),
                active: PaneId(1),
                zoomed: None,
                name: None,
                panes: vec![LegacyPaneSnapV7 {
                    id: PaneId(1),
                    cwd: None,
                    content: Vec::new(),
                    metadata: Vec::new(),
                }],
            }],
        };
        let migrated = decode_snapshot(&bincode::serialize(&legacy).unwrap()).unwrap();
        assert_eq!(migrated.projects[0].active_pane, Some(PaneId(1)));
        assert!(migrated.windows[0].panes[0].launch_args.is_empty());
    }

    #[test]
    fn v4_snapshot_migrates_without_losing_structure() {
        let legacy = LegacySnapshotV4 {
            version: 4,
            active_window: 0,
            next_pane_id: 2,
            windows: vec![LegacyWinSnapV4 {
                layout: LayoutNode::Leaf(PaneId(1)),
                active: PaneId(1),
                zoomed: None,
                name: Some("legacy".into()),
                panes: vec![LegacyPaneSnapV4 {
                    id: PaneId(1),
                    cwd: Some("/tmp".into()),
                    content: vec![vec![LegacyCellV4 {
                        ch: 'x',
                        fg: Color::Default,
                        bg: Color::Default,
                        attrs: LegacyAttrsV4(1),
                    }]],
                }],
            }],
        };
        let migrated = decode_snapshot(&bincode::serialize(&legacy).unwrap()).unwrap();
        assert_eq!(migrated.version, VERSION);
        assert_eq!(migrated.active_project, ProjectId(1));
        assert_eq!(migrated.windows[0].project, ProjectId(1));
        assert_eq!(migrated.windows[0].name.as_deref(), Some("legacy"));
        assert_eq!(migrated.windows[0].panes[0].content[0].cells[0].text, "x");
        assert_eq!(
            migrated.windows[0].panes[0].content[0].cells[0].attrs,
            Attrs::BOLD
        );
    }
}
