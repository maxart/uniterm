//! The multiplexer server: a single-threaded `mio` event loop that owns the
//! panes, windows, terminal models, and attached clients.
//!
//! A session owns windows, each with a layout tree of panes. All panes
//! (even in background windows) run and update their model; only visible panes
//! in the active window are painted, at their computed offsets - the "inactive
//! panes update their model and draw nothing" property from `docs/03`/`docs/04`.
//! State lives in the server; clients are ephemeral and survive detach.
//!
//! Built-in commands (split, focus, zoom, kill, windows) arrive over the socket
//! from the client's prefix keybindings; the full command language + rebindable
//! keys are M4. Everything runs on one thread, with no async or synchronization
//! lock on the hot path.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{
    DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _,
    PermissionsExt as _,
};
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};

use mio::net::{UnixListener, UnixStream};
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Registry, Token};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;
use uniterm_core::layout::{neighbor, Layout};
use uniterm_core::{
    AgentStatus, Color, Config, Direction, LayoutNode, PaneId, ProjectId, Rect, SplitDir,
    StatusPosition,
};
use uniterm_proto::{
    encode_frame, ClientMessage, Command, FocusDir, FrameDecoder, MouseKind, PaneAttachRole,
    ServerMessage, SplitAxis, TabMoveDirection, MAX_CLIENT_FRAME, MAX_SERVER_FRAME,
};

use crate::chrome::{self, ObservatoryTab};
use crate::context_menu::{ContextAction, ContextInput, ContextMenu, ContextTarget};
use crate::copymode::{CopyAction, CopyState};
use crate::file_manager::{FileAction, FileManager};
use crate::pty::PtyProcess;
use crate::renderer::Renderer;
use crate::terminal::{MouseMode, Terminal};

mod agents;
mod artifact;
// The chrome-painting half of the server lives in `server/chrome.rs`; the
// module is bound under a different name so `crate::chrome` stays reachable
// from here and from every sibling module's `use super::*`.
#[path = "server/chrome.rs"]
mod chrome_ui;
mod control;
mod event_projection;
mod guardrail;
mod instruction;
mod io;
mod messages;
mod mouse;
mod orchestration;
mod projects;
mod socket;
mod waiting;
mod worktree;

pub use socket::{
    config_path, default_socket_path, load_config, run_server, socket_dir, WorkspaceLock,
};

use agents::{
    detection_candidate_can_apply, direct_detection_provenance, evidence_hash, native_resume_args,
    same_detection_provenance,
};
use chrome_ui::sanitize_chrome_text;
use io::set_interest;
use projects::{workspace_layout_definition, workspace_layout_with_panes};
use socket::{
    bind_workspace_listener, prepare_socket_parent, remove_socket_if_unchanged, socket_identity,
};

use orchestration::{
    next_orchestration_deadline, orchestration_token_seed, ActiveRelay, ActiveWorkflow,
    PendingOrchestrationSubmission, PendingPromptDelivery, PendingRelayActivation,
};

const LISTENER: Token = Token(0);
/// The agent runtime's waker: the tokio side signals "replies are queued"
/// through it, so the core loop needs no timeout to notice them.
const AGENT_WAKER: Token = Token(1);
/// Client and pane tokens are allocated from here so they never hit LISTENER.
const FIRST_TOKEN: usize = 16;
const MAX_PENDING_INPUT: usize = 8 * 1024 * 1024;
const MAX_PENDING_CLIENT: usize = MAX_SERVER_FRAME as usize + 4;
const MAX_CLIENTS: usize = 128;
const MAX_ACCEPTS_PER_EVENT: usize = 64;
const MAX_CLIENT_MESSAGES_PER_EVENT: usize = 1024;
/// Bytes one PTY or client may deliver per readiness event before the loop
/// moves on. Sockets are drained on write until real backpressure, because a
/// half-written frame gets no further writable edge; reads are different: a
/// process streaming faster than the parser can drain would otherwise keep
/// the single thread inside one `read` loop and starve keystrokes, other
/// Panes, and rendering. A source that hits the budget is queued in
/// `pending_reads` and re-read before the next blocking poll.
const PTY_IO_BUDGET: usize = 256 * 1024;
const MAX_REMOTE_SEARCH_PATH_ENTRIES: usize = 256;
const MAX_REMOTE_SEARCH_PATH_BYTES: usize = 64 * 1024;

fn normalize_remote_search_path(entries: Vec<String>) -> Option<Vec<String>> {
    if entries.is_empty() || entries.len() > MAX_REMOTE_SEARCH_PATH_ENTRIES {
        return None;
    }
    let mut total = 0usize;
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(entries.len());
    for entry in entries {
        total = total.checked_add(entry.len())?;
        if total > MAX_REMOTE_SEARCH_PATH_BYTES
            || entry.is_empty()
            || entry.as_bytes().contains(&0)
            || !Path::new(&entry).is_absolute()
        {
            return None;
        }
        if seen.insert(entry.clone()) {
            normalized.push(entry);
        }
    }
    (!normalized.is_empty()).then_some(normalized)
}

/// Put a bridge's login-shell directories first without dropping anything the
/// server could already resolve. The search path is process-wide, so a later
/// bridge from a narrower shell must widen what agents are found, never
/// silently remove providers from every other attached client.
fn merge_search_paths(preferred: Vec<String>, existing: &[String]) -> Vec<String> {
    let mut merged = preferred;
    for entry in existing {
        if !merged.iter().any(|known| known == entry) {
            merged.push(entry.clone());
        }
    }
    merged
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileSidebarRows {
    divider: Option<u16>,
    tree_start: u16,
    tree_end: u16,
}

impl FileSidebarRows {
    fn capacity(self) -> usize {
        usize::from(self.tree_end.saturating_sub(self.tree_start))
    }

    fn slot_at(self, row: u16) -> Option<usize> {
        (row >= self.tree_start && row < self.tree_end).then(|| usize::from(row - self.tree_start))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarScope {
    Project,
    Workspace,
}

impl SidebarScope {
    fn toggle(self) -> SidebarScope {
        match self {
            SidebarScope::Project => SidebarScope::Workspace,
            SidebarScope::Workspace => SidebarScope::Project,
        }
    }

    fn label(self, available: usize) -> &'static str {
        match (self, available >= 17) {
            (SidebarScope::Project, true) => "project",
            (SidebarScope::Workspace, true) => "workspace",
            (SidebarScope::Project, false) => "proj",
            (SidebarScope::Workspace, false) => "all",
        }
    }
}

/// One pane: a PTY, its terminal model, and the mio token of its master fd.
/// The pane's id is its key in `Server::panes`, so it is not stored again here.
struct Pane {
    pty: PtyProcess,
    term: Terminal,
    token: Token,
    /// Last known working directory, seeded from the authoritative launch
    /// directory and advanced only by cooperative OSC 7 reports.
    ///
    /// macOS has no `/proc/<pid>/cwd`, so keeping this event-driven cache is
    /// what makes crash restore retain Pane paths on every supported host.
    cwd: Option<PathBuf>,
    /// `Some` while this pane is in copy-mode (scrollback/selection/search).
    copy: Option<CopyState>,
    /// The AI agent detected in this pane (via OSC 777), if any - drives the
    /// Agents tab and other Observatory surfaces.
    agent: Option<PaneAgent>,
    foreground_pid: Option<i32>,
    last_evidence_hash: u64,
    last_dev_server_evidence_hash: u64,
    detection_candidate: Option<DetectionCandidate>,
    last_detection: Option<DetectionRecord>,
    metadata: HashMap<String, MetadataValue>,
    input: Vec<u8>,
    input_offset: usize,
    /// Exact arguments used for this Pane invocation. Starting a new
    /// invocation replaces this vector so stale overrides cannot leak.
    launch_args: Vec<String>,
}

/// The agent bound to a pane: its id, signature colour, and reconciled status.
/// Fields are consumed by the sidebar and Observatory surfaces.
#[allow(dead_code)]
struct PaneAgent {
    id: String,
    color: Color,
    status: AgentStatus,
    authority: uniterm_proto::DetectionAuthority,
    evidence: String,
    provenance: uniterm_proto::DetectionProvenance,
    foreground_pid: Option<i32>,
    /// First binding time for this agent run. Status updates retain it so the
    /// docked Agents rail never reorders a run merely because its state changed.
    started_at: std::time::Instant,
    session_id: Option<String>,
    resume_command: Vec<String>,
}

struct DetectionCandidate {
    status: AgentStatus,
    authority: uniterm_proto::DetectionAuthority,
    evidence: String,
    provenance: uniterm_proto::DetectionProvenance,
    dwell: std::time::Duration,
    since: std::time::Instant,
}

#[derive(Clone)]
struct DetectionRecord {
    agent: String,
    status: AgentStatus,
    authority: uniterm_proto::DetectionAuthority,
    evidence: String,
    foreground_pid: Option<i32>,
    provenance: uniterm_proto::DetectionProvenance,
}

struct MetadataValue {
    value: String,
    expires: Option<std::time::Instant>,
}

struct AgentToast {
    pane: PaneId,
    title: String,
    body: String,
    expires: std::time::Instant,
}

struct PendingAgentNotification {
    previous: AgentStatus,
    status: AgentStatus,
    due: std::time::Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackedDevServer {
    label: String,
    url: String,
    detected: u64,
}

/// One window: a layout tree over pane ids, the active pane, and an optional
/// zoomed pane (which, while set, fills the whole window).
struct Win {
    /// Stable owner in the Workspace > Project > Tab > Pane hierarchy.
    project: ProjectId,
    layout: LayoutNode,
    active: PaneId,
    zoomed: Option<PaneId>,
    /// User-given name (rename tab), shown as ` i:name ` in the status line.
    name: Option<String>,
}

/// A durable Project under this Workspace. Tabs retain a stable Project id,
/// while their storage indices may shift as other Tabs close.
#[derive(Clone)]
struct Project {
    id: ProjectId,
    name: String,
    root: String,
    /// Stable focus memory across mutable Tab storage indices.
    active_pane: Option<PaneId>,
    metadata: HashMap<String, String>,
}

enum PendingControlWait {
    Output {
        pane: PaneId,
        needle: String,
        deadline: std::time::Instant,
    },
    Agent {
        pane: PaneId,
        status: AgentStatus,
        deadline: std::time::Instant,
    },
}

enum WorktreeRequester {
    Client(Token),
    ClientWorkspace(Token),
    Control {
        connection: u64,
        id: u64,
    },
    RunForkClient {
        token: Token,
        parent: uniterm_core::RunId,
    },
    RunForkControl {
        connection: u64,
        id: u64,
        parent: uniterm_core::RunId,
    },
}

#[derive(Clone)]
enum PendingChildLaunch {
    Workflow {
        parent: uniterm_core::RunId,
        template: String,
        goal: String,
        role_providers: Vec<uniterm_core::orchestrate::RoleProviderSelection>,
    },
    Relay {
        parent: uniterm_core::RunId,
        goal: String,
        role_providers: Vec<uniterm_core::orchestrate::RoleProviderSelection>,
    },
}

struct PendingWorktree {
    requester: WorktreeRequester,
    operation: uniterm_proto::WorktreeOperation,
    rollback_error: Option<String>,
    child_launch: Option<PendingChildLaunch>,
}

impl PendingControlWait {
    fn deadline(&self) -> std::time::Instant {
        match self {
            PendingControlWait::Output { deadline, .. }
            | PendingControlWait::Agent { deadline, .. } => *deadline,
        }
    }
}

/// One attached client. Clients attach at different times so each keeps an
/// independent renderer (cursor/SGR caches) plus an outbound buffer.
struct DirectAttachment {
    pane: PaneId,
    role: PaneAttachRole,
    last_cursor_visible: Option<bool>,
}

struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
    renderer: Renderer,
    outbuf: Vec<u8>,
    out_offset: usize,
    /// One past the last byte of the queued RenderOps frame, when a render is
    /// actually pending in `outbuf`. Ordinary protocol replies must not make a
    /// later render look superseded.
    render_end: Option<usize>,
    attached: bool,
    /// Once a connection requests Pane attach it cannot fall back to
    /// Workspace commands or input, including after a rejected claim.
    direct_only: bool,
    /// A focused Pane stream is independent of the Workspace attach canvas.
    direct: Option<DirectAttachment>,
    /// The client is compositing an overlay: its terminal no longer matches
    /// our render caches, so damage batches must re-emit absolute state.
    overlay: bool,
    cols: u16,
    rows: u16,
    dead: bool,
    /// Render output is supersedable. If the terminal transport is still
    /// draining an older frame, collapse later damage into one authoritative
    /// full repaint instead of growing the queue until the client detaches.
    repaint_pending: bool,
    /// Whether WRITABLE is currently part of this stream's mio interest.
    /// Re-registering an unchanged kqueue filter can strand a raced read edge.
    write_interest: bool,
    /// At most one event-driven automation wait per control connection.
    pending_wait: Option<PendingControlWait>,
}

impl Client {
    fn queue(&mut self, bytes: &[u8]) {
        if self.dead {
            return;
        }
        let pending = self.outbuf.len().saturating_sub(self.out_offset);
        if pending.saturating_add(bytes.len()) > MAX_PENDING_CLIENT {
            self.outbuf.clear();
            self.out_offset = 0;
            self.render_end = None;
            self.dead = true;
            let _ = self.stream.shutdown(Shutdown::Both);
            return;
        }
        if self.out_offset != 0 {
            let drained = self.out_offset;
            self.outbuf.drain(..drained);
            self.render_end = self
                .render_end
                .and_then(|end| end.checked_sub(drained))
                .filter(|end| *end != 0);
            self.out_offset = 0;
        }
        self.outbuf.extend_from_slice(bytes);
    }

    /// Queue terminal render output without allowing repaint bursts to evict a
    /// healthy but temporarily backpressured client.
    ///
    /// An incomplete frame already on the stream cannot be removed. Later
    /// render frames are therefore collapsed into one full repaint after that
    /// frame drains. Non-render protocol replies still use [`Self::queue`] and
    /// retain the hard memory bound.
    fn queue_render(&mut self, bytes: &[u8]) {
        if self.dead {
            return;
        }
        if self.render_end.is_some() {
            self.repaint_pending = true;
            return;
        }
        self.queue(bytes);
        if !self.dead {
            self.render_end = Some(self.outbuf.len());
        }
    }

    fn flush(&mut self) {
        // Once render output has been superseded, only an actual writable
        // readiness event may drain the older partial frame. This guarantees
        // the event handler observes the empty transition and schedules the
        // authoritative repaint instead of silently clearing the queue from an
        // unrelated broadcast.
        if self.repaint_pending {
            return;
        }
        self.flush_ready();
    }

    fn flush_ready(&mut self) {
        // mio readiness is edge-triggered on supported Unix pollers. Drain
        // until the socket reports real backpressure; voluntarily stopping at
        // a byte budget can leave an incomplete protocol frame with no future
        // writable edge to finish it.
        while self.out_offset < self.outbuf.len() {
            match self.stream.write(&self.outbuf[self.out_offset..]) {
                Ok(0) => {
                    self.outbuf.clear();
                    self.out_offset = 0;
                    self.render_end = None;
                    self.dead = true;
                    break;
                }
                Ok(n) => {
                    self.out_offset += n;
                    if self
                        .render_end
                        .is_some_and(|render_end| self.out_offset >= render_end)
                    {
                        self.render_end = None;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.outbuf.clear();
                    self.out_offset = 0;
                    self.render_end = None;
                    self.dead = true;
                    break;
                }
            }
        }
        if self.out_offset == self.outbuf.len() {
            self.outbuf.clear();
            self.out_offset = 0;
            self.render_end = None;
        }
    }
    fn wants_write(&self) -> bool {
        !self.dead && self.out_offset < self.outbuf.len()
    }
}

/// The multiplexer server.
pub struct Server {
    listener: UnixListener,
    panes: HashMap<PaneId, Pane>,
    pane_tokens: HashMap<Token, PaneId>,
    process_watches: HashMap<Token, (PaneId, crate::process_watch::ProcessWatch)>,
    pane_watches: HashMap<PaneId, Token>,
    windows: Vec<Win>,
    active_window: usize,
    last_active_pane: Option<PaneId>,
    projects: Vec<Project>,
    active_project: ProjectId,
    next_project_id: u64,
    /// Vertical origin of the Projects-only left rail.
    project_scroll: usize,
    sidebar_agent_scope: SidebarScope,
    sidebar_server_scope: SidebarScope,
    /// Persistent right-hand Observatory view and independent viewports.
    observatory_tab: ObservatoryTab,
    observatory_scroll: [usize; 3],
    /// Horizontal origin of the active Project's Tab bar.
    tab_scroll: usize,
    tab_scroll_follow_active: bool,
    /// The active window's computed layout (rects + dividers), refreshed by
    /// [`Server::relayout`]; a zoomed window computes to a single full-area pane.
    current_layout: Layout,
    clients: HashMap<Token, Client>,
    cols: u16,
    rows: u16,
    next_token: usize,
    next_pane_id: u64,
    program: String,
    /// Executable lookup inherited at startup or refreshed by a remote bridge.
    /// Agent launches retain concrete paths so live Workspaces do not depend
    /// on SSH's restricted process environment or an older pane shell.
    agent_search_path: Vec<String>,
    /// Last outer-terminal title broadcast to attached clients. Output that
    /// leaves the title unchanged must not cost a single byte on any client;
    /// attach and full repaints always send the current title regardless.
    last_broadcast_title: Option<String>,
    /// Last Workspace definition line handed to the runtime, so a checkpoint
    /// whose structure did not change appends nothing to the catalog.
    last_catalog_line: Option<String>,
    /// Sources that still had readable bytes when their `PTY_IO_BUDGET` ran
    /// out. Edge-triggered readiness never fires again for bytes already
    /// buffered, so they are serviced again before the next blocking poll.
    pending_reads: Vec<Token>,
    sock_path: PathBuf,
    socket_identity: (u64, u64),
    /// Held until socket cleanup and runtime flushing have both completed.
    workspace_lock: WorkspaceLock,
    /// Session name (the socket's stem), shown in the status line.
    name: String,
    /// Machine that owns the panes, resolved once so remote clients see the
    /// server identity without per-frame environment or filesystem work.
    hostname: String,
    config: Config,
    /// The append-only event log (ground truth for the Observatory).
    log: crate::eventlog::EventLog,
    /// Disabled only while the production server reconstructs an existing
    /// Workspace, so its temporary bootstrap Pane cannot pollute history.
    event_writes_enabled: bool,
    /// Durable tasks (AG7), projected from the event log.
    tasks: uniterm_core::TaskList,
    /// Active human-attention items, projected from the same event stream.
    waiting: uniterm_core::WaitingQueue,
    /// Human direction waiting for an exact active agent invocation.
    instructions: uniterm_core::InstructionQueue,
    /// Native run relationships projected from append-only lifecycle events.
    run_graph: uniterm_core::RunGraph,
    /// Last event cursor reflected by the in-memory graph checkpoint.
    run_graph_sequence: u64,
    /// Typed artifact ownership projected from append-only lifecycle events.
    artifacts: uniterm_core::ArtifactLedger,
    /// Last event cursor reflected by the artifact checkpoint.
    artifact_sequence: u64,
    /// Filesystem observations already delegated to the runtime. This keeps
    /// bursty notify events from scheduling duplicate hashes for one Artifact.
    pending_artifact_observations: HashSet<uniterm_core::ArtifactId>,
    /// Artifacts that changed again while an observation was in flight. Each
    /// gets exactly one follow-up observation so coalescing cannot hide the
    /// final filesystem state.
    dirty_artifact_observations: HashSet<uniterm_core::ArtifactId>,
    /// Announced local web servers, kept live by runtime-side TCP probes.
    dev_servers: HashMap<(PaneId, u16), TrackedDevServer>,
    next_dev_server_sequence: u64,
    /// Enabled by the production `run_server` entry point. Direct `Server::bind`
    /// users (primarily isolated protocol tests) do not write machine catalog
    /// entries as a side effect.
    workspace_catalog_enabled: bool,
    /// An in-flight click-drag text selection (S1): selection is always on,
    /// so a press on a pane the app doesn't own arms this, dragging selects,
    /// and release yanks to the clipboard.
    mouse_sel: Option<MouseSel>,
    /// The divider being dragged with the left button, as last drawn; the
    /// drag ends on release.
    divider_drag: Option<uniterm_core::Divider>,
    /// Zoom-out overview (S2): `Some(selected tile)` while every window is
    /// shown as a static tile in a grid; input picks one to switch to.
    overview: Option<usize>,
    /// Live workflow runs (AG5 wired): the pure engine plus the panes its
    /// roles live in. Advanced only by explicit `WorkflowSubmit` events.
    workflows: Vec<ActiveWorkflow>,
    /// Live turn-based relays. Only the active role receives input.
    relays: Vec<ActiveRelay>,
    /// Startup-seeded generator for activation tokens. Tokens are unique
    /// across concurrent workflow and relay runs, not merely within one run.
    orchestration_token_state: u64,
    durability_error: Option<String>,
    pending_orchestration_submissions: Vec<PendingOrchestrationSubmission>,
    pending_prompt_deliveries: Vec<PendingPromptDelivery>,
    pending_relay_activations: Vec<PendingRelayActivation>,
    /// Where the visible cursor was last broadcast to clients. A cursor-only
    /// change (no grid damage, e.g. a space typed over an already-blank cell,
    /// or a bare cursor move) must still emit a move; comparing against this
    /// keeps the clean-grid + unmoved-cursor case at zero bytes.
    last_cursor: Option<(u16, u16)>,
    last_cursor_visible: Option<bool>,
    /// The tokio half (Decision R1). Disk-touching agent work (connector
    /// toggles, PATH/settings probes) runs there; replies arrive through the
    /// [`AGENT_WAKER`] and are applied by [`Server::on_agent_reply`].
    agents: crate::runtime::AgentRuntime,
    next_worktree_request: u64,
    pending_worktrees: HashMap<u64, PendingWorktree>,
    /// A dirty terminal projection arms one fixed checkpoint deadline. The
    /// deadline is not reset by later output, so continuous scrollback is
    /// checkpointed at a bounded cadence without a free-running timer.
    snapshot_deadline: Option<std::time::Instant>,
    /// Optional right-hand file tree. It owns no filesystem handles; all disk
    /// work and event watches live on the agent runtime.
    files: FileManager,
    /// Event-driven right-click menu targeting a Pane or file-tree row.
    context_menu: Option<ContextMenu>,
    notification: Option<AgentToast>,
    pending_notifications: HashMap<PaneId, PendingAgentNotification>,
    running: bool,
}

/// A pending/active mouse text selection: where the press landed and whether
/// a drag has started selecting yet.
#[derive(Clone, Copy)]
struct MouseSel {
    client: Token,
    pane: PaneId,
    press: (u16, u16),
    selecting: bool,
}

impl Server {
    /// Bind the socket, spawn the first pane running `program args`, and lay it
    /// out. Returns the server and the `Poll` it runs on.
    pub fn bind(
        sock_path: &Path,
        program: &str,
        args: &[&str],
        cols: u16,
        rows: u16,
    ) -> std::io::Result<(Server, Poll)> {
        Self::bind_internal(sock_path, program, args, cols, rows, true)
    }

    fn bind_internal(
        sock_path: &Path,
        program: &str,
        args: &[&str],
        cols: u16,
        rows: u16,
        event_writes_enabled: bool,
    ) -> std::io::Result<(Server, Poll)> {
        let name = sock_path
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Workspace socket needs a UTF-8 file stem",
                )
            })?;
        uniterm_proto::validate_workspace_name(name).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid Workspace name '{name}': {error}"),
            )
        })?;
        prepare_socket_parent(sock_path)?;
        let workspace_lock = WorkspaceLock::acquire(sock_path)?;
        let mut listener = bind_workspace_listener(sock_path).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "could not bind Workspace socket {}: {error}",
                    sock_path.display()
                ),
            )
        })?;
        let bound_socket_identity = socket_identity(sock_path)?;

        let poll = Poll::new().map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("could not create core poller: {error}"),
            )
        })?;
        poll.registry()
            .register(&mut listener, LISTENER, Interest::READABLE)
            .map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("could not register Workspace socket: {error}"),
                )
            })?;
        let waker = std::sync::Arc::new(mio::Waker::new(poll.registry(), AGENT_WAKER).map_err(
            |error| {
                std::io::Error::new(
                    error.kind(),
                    format!("could not register agent-runtime waker: {error}"),
                )
            },
        )?);
        let agents = match crate::runtime::spawn_agent_runtime_with_control(
            waker,
            sock_path.with_extension("control.sock"),
        ) {
            Ok(agents) => agents,
            Err(error) => {
                let _ = remove_socket_if_unchanged(sock_path, bound_socket_identity);
                return Err(error);
            }
        };

        let mut server = Server {
            listener,
            panes: HashMap::new(),
            pane_tokens: HashMap::new(),
            process_watches: HashMap::new(),
            pane_watches: HashMap::new(),
            windows: Vec::new(),
            active_window: 0,
            last_active_pane: None,
            projects: Vec::new(),
            active_project: ProjectId(1),
            next_project_id: 2,
            project_scroll: 0,
            sidebar_agent_scope: SidebarScope::Project,
            sidebar_server_scope: SidebarScope::Project,
            observatory_tab: ObservatoryTab::Agents,
            observatory_scroll: [0; 3],
            tab_scroll: 0,
            tab_scroll_follow_active: true,
            current_layout: Layout::default(),
            clients: HashMap::new(),
            cols: cols.max(1),
            rows: rows.max(1),
            next_token: FIRST_TOKEN,
            next_pane_id: 1,
            program: program.to_string(),
            agent_search_path: crate::workflow::search_path_from_env(),
            last_broadcast_title: None,
            last_catalog_line: None,
            pending_reads: Vec::new(),
            sock_path: sock_path.to_path_buf(),
            socket_identity: bound_socket_identity,
            workspace_lock,
            name: name.to_string(),
            hostname: system_hostname(),
            config: Config::default(),
            log: crate::eventlog::EventLog::open(name),
            event_writes_enabled,
            tasks: uniterm_core::TaskList::new(),
            waiting: uniterm_core::WaitingQueue::new(),
            instructions: uniterm_core::InstructionQueue::new(),
            run_graph: uniterm_core::RunGraph::new(),
            run_graph_sequence: 0,
            artifacts: uniterm_core::ArtifactLedger::new(),
            artifact_sequence: 0,
            pending_artifact_observations: HashSet::new(),
            dirty_artifact_observations: HashSet::new(),
            dev_servers: HashMap::new(),
            next_dev_server_sequence: 1,
            workspace_catalog_enabled: false,
            mouse_sel: None,
            divider_drag: None,
            overview: None,
            workflows: Vec::new(),
            relays: Vec::new(),
            orchestration_token_state: orchestration_token_seed(),
            durability_error: None,
            pending_orchestration_submissions: Vec::new(),
            pending_prompt_deliveries: Vec::new(),
            pending_relay_activations: Vec::new(),
            last_cursor: None,
            last_cursor_visible: None,
            agents,
            next_worktree_request: 1,
            pending_worktrees: HashMap::new(),
            snapshot_deadline: None,
            files: FileManager::new(ProjectId(1), String::new()),
            context_menu: None,
            notification: None,
            pending_notifications: HashMap::new(),
            running: true,
        };

        let root = std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let project_name = Path::new(&root)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("Default")
            .to_string();
        server.append_event(crate::eventlog::LogEvent::ProjectCreated {
            project: 1,
            name: project_name.clone(),
            root: root.clone(),
        });
        server.projects.push(Project {
            id: ProjectId(1),
            name: project_name,
            root: root.clone(),
            active_pane: None,
            metadata: HashMap::new(),
        });
        server.files.reset(ProjectId(1), root, false);
        let first = server.spawn_pane(poll.registry(), args).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("could not create the initial Pane: {error}"),
            )
        })?;
        server.windows.push(Win {
            project: ProjectId(1),
            layout: LayoutNode::Leaf(first),
            active: first,
            zoomed: None,
            name: None,
        });
        server.projects[0].active_pane = Some(first);
        server.relayout();
        Ok((server, poll))
    }

    /// Run the event loop until no panes remain.
    pub fn run(&mut self, poll: &mut Poll) -> std::io::Result<()> {
        let mut events = Events::with_capacity(256);
        let mut accepts_pending = false;
        while self.running {
            if accepts_pending {
                accepts_pending = self.on_accept(poll.registry());
            }
            self.service_pending_reads(poll.registry());
            // A stray signal interrupts epoll_wait (EINTR); retry, don't die.
            let timeout = if accepts_pending || !self.pending_reads.is_empty() {
                // A real connection burst remains queued. Poll without
                // blocking so other ready PTYs and clients get a turn between
                // bounded accept batches; once drained, idle polling blocks.
                Some(std::time::Duration::ZERO)
            } else {
                self.next_detection_deadline()
                    .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()))
            };
            if let Err(e) = poll.poll(&mut events, timeout) {
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            for ev in events.iter() {
                let reg = poll.registry();
                let token = ev.token();
                if token == LISTENER {
                    accepts_pending |= self.on_accept(reg);
                } else if token == AGENT_WAKER {
                    for reply in self.agents.drain() {
                        self.on_agent_reply(reg, reply);
                    }
                } else if self.pane_tokens.contains_key(&token) {
                    self.on_pty(
                        reg,
                        token,
                        ev.is_readable() || ev.is_read_closed() || ev.is_error(),
                        ev.is_writable(),
                    );
                } else if self.process_watches.contains_key(&token) {
                    self.on_process_exit(reg, token);
                } else {
                    self.on_client(reg, token, ev.is_readable(), ev.is_writable());
                }
            }
            self.flush_detection_due(poll.registry());
            self.flush_prompt_deliveries_due(poll.registry());
            self.flush_artifact_validations_due();
            self.flush_orchestration_elapsed_due(poll.registry());
            self.flush_orchestration_idle_due(poll.registry());
            self.flush_orchestration_stalls(poll.registry());
            self.flush_snapshot_due();
        }
        Ok(())
    }

    fn next_detection_deadline(&self) -> Option<std::time::Instant> {
        self.panes
            .values()
            .flat_map(|pane| {
                pane.detection_candidate
                    .as_ref()
                    .map(|candidate| candidate.since + candidate.dwell)
                    .into_iter()
                    .chain(pane.metadata.values().filter_map(|value| value.expires))
            })
            .chain(self.notification.as_ref().map(|toast| toast.expires))
            .chain(
                self.pending_notifications
                    .values()
                    .map(|pending| pending.due),
            )
            .chain(self.clients.values().filter_map(|client| {
                client
                    .pending_wait
                    .as_ref()
                    .map(PendingControlWait::deadline)
            }))
            .chain(next_orchestration_deadline(
                &self.pending_orchestration_submissions,
                &self.pending_prompt_deliveries,
                &self.workflows,
                &self.relays,
            ))
            .chain(self.snapshot_deadline)
            .min()
    }

    fn flush_detection_due(&mut self, reg: &Registry) {
        let now = std::time::Instant::now();
        let due: Vec<PaneId> = self
            .panes
            .iter()
            .filter_map(|(id, pane)| {
                pane.detection_candidate
                    .as_ref()
                    .filter(|candidate| now >= candidate.since + candidate.dwell)
                    .map(|_| *id)
            })
            .collect();
        let mut changed = false;
        let mut transitions = Vec::new();
        for pane_id in due {
            let candidate = self
                .panes
                .get_mut(&pane_id)
                .and_then(|pane| pane.detection_candidate.take());
            let Some(candidate) = candidate else {
                continue;
            };
            let Some(agent) = self
                .panes
                .get_mut(&pane_id)
                .and_then(|pane| pane.agent.as_mut())
            else {
                continue;
            };
            if !detection_candidate_can_apply(
                agent.status,
                agent.authority,
                candidate.status,
                candidate.authority,
            ) {
                continue;
            }
            if agent.status != candidate.status
                || agent.authority != candidate.authority
                || agent.evidence != candidate.evidence
                || !same_detection_provenance(&agent.provenance, &candidate.provenance)
            {
                let previous = agent.status;
                agent.status = candidate.status;
                agent.authority = candidate.authority;
                agent.evidence = candidate.evidence;
                agent.provenance = candidate.provenance;
                if previous != candidate.status {
                    self.append_event(crate::eventlog::LogEvent::AgentStatus {
                        pane: pane_id.0,
                        status: candidate.status,
                    });
                    transitions.push((pane_id, previous, candidate.status));
                }
                changed = true;
            } else {
                agent.provenance = candidate.provenance;
            }
        }
        for (pane, previous, status) in transitions {
            self.notify_agent_transition(pane, previous, status);
        }
        if changed {
            self.full_repaint_all(reg);
        }
        let mut metadata_changed = false;
        for pane in self.panes.values_mut() {
            let before = pane.metadata.len();
            pane.metadata
                .retain(|_, value| value.expires.is_none_or(|expires| expires > now));
            metadata_changed |= pane.metadata.len() != before;
        }
        if metadata_changed {
            self.full_repaint_all(reg);
            self.persist();
        }
        let due_notifications: Vec<PaneId> = self
            .pending_notifications
            .iter()
            .filter_map(|(pane, pending)| (pending.due <= now).then_some(*pane))
            .collect();
        for pane in due_notifications {
            let Some(pending) = self.pending_notifications.remove(&pane) else {
                continue;
            };
            let still_current = self
                .panes
                .get(&pane)
                .and_then(|pane| pane.agent.as_ref())
                .is_some_and(|agent| agent.status == pending.status);
            if still_current {
                self.deliver_agent_notification(reg, pane, pending.previous, pending.status);
            }
        }
        if self
            .notification
            .as_ref()
            .is_some_and(|toast| toast.expires <= now)
        {
            self.notification = None;
            self.full_repaint_all(reg);
        }
        self.service_pending_waits(reg);
    }

    // --- pane lifecycle ----------------------------------------------------

    fn spawn_pane(&mut self, reg: &Registry, args: &[&str]) -> std::io::Result<PaneId> {
        let cwd = self
            .projects
            .iter()
            .find(|project| project.id == self.active_project)
            .map(|project| PathBuf::from(&project.root));
        self.spawn_pane_at(reg, args, cwd.as_deref())
    }

    fn spawn_pane_at(
        &mut self,
        reg: &Registry,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> std::io::Result<PaneId> {
        let id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        self.spawn_pane_with_id(reg, id, args, cwd)?;
        Ok(id)
    }

    /// Spawn a pane with an explicit id and optional working dir (used by restore
    /// so restored panes keep their layout-tree ids and cwd).
    fn spawn_pane_with_id(
        &mut self,
        reg: &Registry,
        id: PaneId,
        args: &[&str],
        cwd: Option<&std::path::Path>,
    ) -> std::io::Result<()> {
        let token = Token(self.next_token);
        self.next_token += 1;

        let sock = self.sock_path.to_string_lossy().into_owned();
        let pane_id_text = id.0.to_string();
        let pty = PtyProcess::spawn(
            &self.program,
            args,
            self.cols,
            self.rows,
            cwd,
            &[
                ("UNITERM_SOCKET", sock.as_str()),
                ("UNITERM_PANE_ID", pane_id_text.as_str()),
                ("UNITERM", "1"),
            ],
        )
        .map_err(|error| {
            std::io::Error::new(error.kind(), format!("PTY process spawn failed: {error}"))
        })?;
        pty.set_nonblocking().map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("PTY nonblocking setup failed: {error}"),
            )
        })?;
        reg.register(&mut SourceFd(&pty.raw_fd()), token, Interest::READABLE)
            .map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("PTY readiness registration failed: {error}"),
                )
            })?;
        let mut term = Terminal::new(self.cols, self.rows);
        term.set_scrollback_limit(self.config.scrollback_limit);
        term.set_default_colors(self.config.theme.foreground, self.config.theme.background);

        self.append_event(crate::eventlog::LogEvent::PaneLaunchProfile {
            pane: id.0,
            args: args.iter().map(|value| (*value).to_string()).collect(),
        });

        self.panes.insert(
            id,
            Pane {
                pty,
                term,
                token,
                cwd: cwd.map(Path::to_path_buf),
                copy: None,
                agent: None,
                foreground_pid: None,
                last_evidence_hash: 0,
                last_dev_server_evidence_hash: 0,
                detection_candidate: None,
                last_detection: None,
                metadata: HashMap::new(),
                input: Vec::new(),
                input_offset: 0,
                launch_args: args.iter().map(|value| (*value).to_string()).collect(),
            },
        );
        self.pane_tokens.insert(token, id);
        self.append_event(crate::eventlog::LogEvent::PaneSpawned { pane: id.0 });
        Ok(())
    }

    fn settings_snapshot(&self) -> uniterm_proto::SettingsSnapshot {
        uniterm_proto::SettingsSnapshot {
            theme: self.config.theme_preset.name().to_string(),
            themes: uniterm_core::ThemePreset::ALL
                .iter()
                .map(|preset| preset.name().to_string())
                .collect(),
            status: self.config.status,
            status_top: self.config.status_position == StatusPosition::Top,
            sidebar: self.config.sidebar,
            sidebar_width: self.config.sidebar_width,
            file_sidebar: self.config.file_sidebar,
            file_sidebar_width: self.config.file_sidebar_width,
            notification_delivery: self.config.notifications.name().to_string(),
            notification_deliveries: uniterm_core::NotificationDelivery::ALL
                .iter()
                .map(|delivery| delivery.name().to_string())
                .collect(),
            notify_completion: self.config.notify_completion,
            focus_follows_mouse: self.config.focus_follows_mouse,
            confirm_close: self.config.confirm_close,
            confirm_tab_close: self.config.confirm_tab_close,
            scrollback_limit: self.config.scrollback_limit,
            restore: self.config.restore,
            guardrail_max_active_runs: self.config.guardrails.max_active_runs,
            guardrail_max_role_panes: self.config.guardrails.max_role_panes,
            guardrail_max_iterations: self.config.guardrails.max_iterations,
            guardrail_max_elapsed_minutes: self.config.guardrails.max_elapsed_seconds / 60,
            guardrail_allowed_projects: self.config.guardrail_allowed_projects_text(),
            editor: self.config.editor.clone(),
            editor_rules: self.config.editor_rules_text(),
        }
    }

    fn sync_file_manager(&mut self, focus: bool) {
        let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == self.active_project)
        else {
            return;
        };
        if self.files.project != project.id || self.files.root != project.root {
            if !self.files.root.is_empty() {
                self.agents.send(uniterm_proto::CoreToAgent::FileWatchSet {
                    project: self.files.project,
                    root: self.files.root.clone(),
                    directories: Vec::new(),
                });
                self.agents
                    .send(uniterm_proto::CoreToAgent::GitChangeWatchSet {
                        project: self.files.project,
                        root: None,
                    });
            }
            self.files.reset(project.id, project.root.clone(), focus);
        } else if focus {
            self.files.focused = true;
        }
        let root = self.files.root.clone();
        self.request_file_listing(root);
        self.sync_file_watches();
        self.sync_git_change_watch();
    }

    /// Start file work only when a client can see the File manager, and tear
    /// it down when geometry or attachment state makes that view inactive.
    fn reconcile_file_manager_runtime(&mut self, was_visible: bool, had_clients: bool) {
        let has_clients = self
            .clients
            .values()
            .any(|client| client.attached && !client.dead);
        let visible = self.file_manager_visible();
        if visible && has_clients {
            if !was_visible || !had_clients {
                self.sync_file_manager(false);
            }
        } else {
            self.files.focused = false;
            self.stop_file_watches();
        }
    }

    fn request_file_listing(&mut self, directory: String) {
        if !self.file_manager_visible()
            || !self
                .clients
                .values()
                .any(|client| client.attached && !client.dead)
            || !self.files.request(&directory)
        {
            return;
        }
        self.agents.send(uniterm_proto::CoreToAgent::FileList {
            project: self.files.project,
            root: self.files.root.clone(),
            directory,
        });
    }

    fn sync_file_watches(&self) {
        if !self.file_manager_visible()
            || !self
                .clients
                .values()
                .any(|client| client.attached && !client.dead)
        {
            return;
        }
        self.agents.send(uniterm_proto::CoreToAgent::FileWatchSet {
            project: self.files.project,
            root: self.files.root.clone(),
            directories: self.files.watched_directories(),
        });
    }

    fn sync_git_change_watch(&self) {
        if !self.file_manager_visible()
            || !self
                .clients
                .values()
                .any(|client| client.attached && !client.dead)
        {
            return;
        }
        self.agents
            .send(uniterm_proto::CoreToAgent::GitChangeWatchSet {
                project: self.files.project,
                root: Some(self.files.root.clone()),
            });
    }

    fn stop_file_watches(&self) {
        self.agents.send(uniterm_proto::CoreToAgent::FileWatchSet {
            project: self.files.project,
            root: self.files.root.clone(),
            directories: Vec::new(),
        });
        self.agents
            .send(uniterm_proto::CoreToAgent::GitChangeWatchSet {
                project: self.files.project,
                root: None,
            });
    }

    fn handle_file_action(&mut self, reg: &Registry, action: FileAction) {
        match action {
            FileAction::None => return,
            FileAction::Redraw | FileAction::Blur => self.full_repaint_all(reg),
            FileAction::Refresh(directories) => {
                for directory in directories {
                    self.request_file_listing(directory);
                }
                self.sync_file_watches();
                self.full_repaint_all(reg);
            }
            FileAction::Mutate(operation) => {
                self.agents.send(uniterm_proto::CoreToAgent::FileMutate {
                    project: self.files.project,
                    root: self.files.root.clone(),
                    operation,
                });
                self.full_repaint_all(reg);
            }
            FileAction::Open(path) => {
                let command = self.config.editor_for_path(&path).to_string();
                self.files.error = None;
                self.agents.send(uniterm_proto::CoreToAgent::EditorOpen {
                    project: self.files.project,
                    path,
                    command,
                });
                self.full_repaint_all(reg);
            }
            FileAction::Copy(path) => {
                self.send_raw_ops(reg, &crate::copymode::osc52(&path));
                self.full_repaint_all(reg);
            }
        }
        self.sync_file_watches();
    }

    fn open_context_menu(&mut self, reg: &Registry, cx: u16, cy: u16) {
        let file_sidebar = self.observatory_width();
        let target = if self.file_manager_visible()
            && file_sidebar > 0
            && cx >= self.cols.saturating_sub(file_sidebar)
        {
            self.files.focused = true;
            let (area, _) = self.chrome_area();
            let rows = self.file_sidebar_rows(area);
            let row = if let Some(slot) = rows.slot_at(cy) {
                let capacity = rows.capacity();
                let first = self.files.first_visible(capacity);
                self.files.select_at(slot, first, capacity)
            } else {
                None
            };
            Some(match row {
                Some(row) => ContextTarget::File {
                    path: row.path,
                    name: row.name,
                    is_dir: row.is_dir,
                    expanded: row.expanded,
                },
                None => ContextTarget::FileRoot {
                    path: self.files.root.clone(),
                    show_hidden: self.files.show_hidden,
                },
            })
        } else {
            self.current_layout
                .panes
                .iter()
                .find(|(_, rect)| rect.contains(cx, cy))
                .map(|(pane, _)| ContextTarget::Pane(*pane))
        };

        if matches!(target, Some(ContextTarget::Pane(_))) {
            self.files.focused = false;
        }
        self.mouse_sel = None;
        self.context_menu = target.map(|target| match target {
            ContextTarget::Pane(pane) => {
                ContextMenu::pane(pane, cx, cy, self.pane_move_destinations(pane))
            }
            other => ContextMenu::new(other, cx, cy),
        });
        self.full_repaint_all(reg);
    }

    fn handle_context_mouse(&mut self, reg: &Registry, cx: u16, cy: u16, kind: MouseKind) {
        match kind {
            MouseKind::Hover => {
                if self
                    .context_menu
                    .as_mut()
                    .is_some_and(|menu| menu.hover(self.cols, self.rows, cx, cy))
                {
                    self.full_repaint_all(reg);
                }
            }
            MouseKind::Click | MouseKind::Release => {
                let Some(menu) = self.context_menu.take() else {
                    return;
                };
                if let Some(action) = menu.action_at(self.cols, self.rows, cx, cy) {
                    self.run_context_action(reg, menu, action);
                } else {
                    self.full_repaint_all(reg);
                }
            }
            MouseKind::WheelUp | MouseKind::WheelDown => {
                let input: &[u8] = if kind == MouseKind::WheelUp {
                    b"k"
                } else {
                    b"j"
                };
                if let Some(menu) = self.context_menu.as_mut() {
                    let _ = menu.handle(input);
                    self.full_repaint_all(reg);
                }
            }
            MouseKind::RightClick | MouseKind::Drag => {}
        }
    }

    fn handle_context_input(&mut self, reg: &Registry, input: &[u8]) {
        let result = self
            .context_menu
            .as_mut()
            .map(|menu| menu.handle(input))
            .unwrap_or(ContextInput::None);
        match result {
            ContextInput::None => {}
            ContextInput::Redraw => self.full_repaint_all(reg),
            ContextInput::Close => {
                self.context_menu = None;
                self.full_repaint_all(reg);
            }
            ContextInput::Run(action) => {
                if let Some(menu) = self.context_menu.take() {
                    self.run_context_action(reg, menu, action);
                }
            }
        }
    }

    fn run_context_action(&mut self, reg: &Registry, menu: ContextMenu, action: ContextAction) {
        match action {
            ContextAction::SplitRight | ContextAction::SplitDown => {
                let ContextTarget::Pane(pane) = menu.target else {
                    self.full_repaint_all(reg);
                    return;
                };
                if self.focus_context_pane(pane) {
                    let axis = if action == ContextAction::SplitRight {
                        SplitAxis::LeftRight
                    } else {
                        SplitAxis::TopBottom
                    };
                    self.handle_command(reg, Command::Split(axis));
                } else {
                    self.full_repaint_all(reg);
                }
            }
            ContextAction::Zoom => {
                let ContextTarget::Pane(pane) = menu.target else {
                    self.full_repaint_all(reg);
                    return;
                };
                if self.focus_context_pane(pane) {
                    self.handle_command(reg, Command::ZoomToggle);
                } else {
                    self.full_repaint_all(reg);
                }
            }
            ContextAction::Overview => {
                let ContextTarget::Pane(pane) = menu.target else {
                    self.full_repaint_all(reg);
                    return;
                };
                if self.focus_context_pane(pane) {
                    self.handle_command(reg, Command::Overview);
                } else {
                    self.full_repaint_all(reg);
                }
            }
            ContextAction::CopyMode => {
                let ContextTarget::Pane(pane) = menu.target else {
                    self.full_repaint_all(reg);
                    return;
                };
                if self.focus_context_pane(pane) {
                    self.handle_command(reg, Command::CopyMode);
                } else {
                    self.full_repaint_all(reg);
                }
            }
            ContextAction::NewTab => self.handle_command(reg, Command::NewWindow),
            ContextAction::MoveToTab(target) => {
                let ContextTarget::Pane(pane) = menu.target else {
                    self.full_repaint_all(reg);
                    return;
                };
                if !self.move_pane_to_window(reg, pane, target) {
                    self.full_repaint_all(reg);
                }
            }
            ContextAction::MoveToNewTab => {
                let ContextTarget::Pane(pane) = menu.target else {
                    self.full_repaint_all(reg);
                    return;
                };
                if !self.move_pane_to_new_window(reg, pane) {
                    self.full_repaint_all(reg);
                }
            }
            ContextAction::ClosePane => {
                let ContextTarget::Pane(pane) = menu.target else {
                    self.full_repaint_all(reg);
                    return;
                };
                self.close_pane(reg, pane);
            }
            ContextAction::Cancel => self.full_repaint_all(reg),
            ContextAction::Open
            | ContextAction::CopyPath
            | ContextAction::CopyRelativePath
            | ContextAction::NewFile
            | ContextAction::NewFolder
            | ContextAction::Rename
            | ContextAction::Delete
            | ContextAction::ConfirmFileDelete
            | ContextAction::Refresh
            | ContextAction::ToggleHidden => self.run_file_context_action(reg, menu, action),
        }
    }

    fn run_file_context_action(
        &mut self,
        reg: &Registry,
        menu: ContextMenu,
        action: ContextAction,
    ) {
        let (path, is_dir, name, is_root) = match &menu.target {
            ContextTarget::File {
                path, name, is_dir, ..
            } => (path.clone(), *is_dir, Some(name.clone()), false),
            ContextTarget::FileRoot { path, .. } => (path.clone(), true, None, true),
            ContextTarget::ConfirmFileDelete { path, is_dir } => {
                (path.clone(), *is_dir, None, false)
            }
            _ => {
                self.full_repaint_all(reg);
                return;
            }
        };
        let file_action = match action {
            ContextAction::Open if !is_root => self.files.open_path(&path, is_dir),
            ContextAction::CopyPath => FileAction::Copy(path),
            ContextAction::CopyRelativePath => FileAction::Copy(self.files.relative_path(&path)),
            ContextAction::NewFile => self.files.begin_create(&path, is_dir, false),
            ContextAction::NewFolder => self.files.begin_create(&path, is_dir, true),
            ContextAction::Rename if !is_root => self
                .files
                .begin_rename(&path, name.as_deref().unwrap_or("")),
            ContextAction::Delete if !is_root => {
                self.context_menu = Some(ContextMenu::new(
                    ContextTarget::ConfirmFileDelete { path, is_dir },
                    menu.x,
                    menu.y,
                ));
                self.full_repaint_all(reg);
                return;
            }
            ContextAction::ConfirmFileDelete => {
                FileAction::Mutate(uniterm_proto::FileOperation::Delete { path })
            }
            ContextAction::Refresh => {
                let directory = if is_dir {
                    path
                } else {
                    Path::new(&path)
                        .parent()
                        .unwrap_or_else(|| Path::new(&self.files.root))
                        .to_string_lossy()
                        .into_owned()
                };
                FileAction::Refresh(vec![directory])
            }
            ContextAction::ToggleHidden if is_root => self.files.toggle_hidden(),
            _ => FileAction::Redraw,
        };
        self.handle_file_action(reg, file_action);
    }

    fn focus_context_pane(&mut self, pane: PaneId) -> bool {
        let window = &mut self.windows[self.active_window];
        if !window.layout.contains_pane(pane) {
            return false;
        }
        if window.active != pane {
            self.last_active_pane = Some(window.active);
            window.active = pane;
        }
        true
    }

    /// Focus a stable Pane id across Project and Tab boundaries.
    fn focus_pane_target(&mut self, reg: &Registry, pane: PaneId) -> bool {
        let Some(window) = self
            .windows
            .iter()
            .position(|tab| tab.layout.contains_pane(pane))
        else {
            return false;
        };
        let project = self.windows[window].project;
        if project != self.active_project {
            self.append_event(crate::eventlog::LogEvent::ProjectSelected { project: project.0 });
        }
        let was_zoomed = self.windows[window].zoomed.is_some();
        let previous = self.windows[self.active_window].active;
        self.activate_window(window);
        if previous != pane {
            self.last_active_pane = Some(previous);
        }
        self.windows[window].active = pane;
        if was_zoomed {
            self.windows[window].zoomed = Some(pane);
        }
        if let Some(project) = self.projects.iter_mut().find(|item| item.id == project) {
            project.active_pane = Some(pane);
        }
        self.files.focused = false;
        self.overview = None;
        self.context_menu = None;
        self.relayout();
        self.full_repaint_all(reg);
        self.persist();
        true
    }

    /// Focus a 1-based Tab and optional Pane ordinal within one Project.
    ///
    /// Ordinals are resolved against one server-owned hierarchy snapshot so
    /// automation never races a client-side list lookup against a later focus.
    /// Omitting `pane` preserves the Tab's remembered active Pane.
    fn focus_hierarchy_target(
        &mut self,
        reg: &Registry,
        project: ProjectId,
        tab: u32,
        pane: Option<u32>,
    ) -> Option<PaneId> {
        let tab_index = usize::try_from(tab.checked_sub(1)?).ok()?;
        let window = *self.project_window_indices(project).get(tab_index)?;
        let target = match pane {
            Some(pane) => {
                let pane_index = usize::try_from(pane.checked_sub(1)?).ok()?;
                *self.windows[window].layout.pane_ids().get(pane_index)?
            }
            None => self.windows[window].active,
        };
        self.focus_pane_target(reg, target).then_some(target)
    }

    fn reply_settings(&mut self, reg: &Registry, token: Token, saved: bool, error: Option<String>) {
        let message = ServerMessage::Settings {
            settings: self.settings_snapshot(),
            saved,
            error,
        };
        if let Some(client) = self.clients.get_mut(&token) {
            client.queue(&encode_frame(&message));
            client.flush();
            let _ = set_interest(reg, client, token);
        }
    }

    fn set_pane_metadata(
        &mut self,
        reg: &Registry,
        pane: PaneId,
        key: &str,
        value: &str,
        ttl_seconds: Option<u64>,
    ) {
        let key = sanitize_metadata_key(key);
        let value: String = value.trim().chars().take(200).collect();
        if key.is_empty() || !self.panes.contains_key(&pane) {
            return;
        }
        self.append_event(crate::eventlog::LogEvent::PaneMetadataSet {
            pane: pane.0,
            key: key.clone(),
            value: value.clone(),
        });
        let pane_state = self.panes.get_mut(&pane).unwrap();
        if value.is_empty() {
            pane_state.metadata.remove(&key);
        } else {
            pane_state.metadata.insert(
                key,
                MetadataValue {
                    value,
                    expires: ttl_seconds.map(|seconds| {
                        std::time::Instant::now()
                            + std::time::Duration::from_secs(seconds.clamp(1, 86_400))
                    }),
                },
            );
        }
        self.full_repaint_all(reg);
        self.persist();
    }

    fn set_project_metadata(&mut self, reg: &Registry, project: ProjectId, key: &str, value: &str) {
        let key = sanitize_metadata_key(key);
        let value: String = value.trim().chars().take(200).collect();
        let Some(index) = self.projects.iter().position(|item| item.id == project) else {
            return;
        };
        if key.is_empty() || worktree::reserved_metadata(&key) {
            return;
        }
        self.append_event(crate::eventlog::LogEvent::ProjectMetadataSet {
            project: project.0,
            key: key.clone(),
            value: value.clone(),
        });
        if value.is_empty() {
            self.projects[index].metadata.remove(&key);
        } else {
            self.projects[index].metadata.insert(key, value);
        }
        self.full_repaint_all(reg);
        self.persist();
    }

    fn apply_settings(
        &mut self,
        reg: &Registry,
        token: Token,
        mut patch: uniterm_proto::SettingsPatch,
    ) {
        if patch.editor.is_some() || patch.editor_rules.is_some() {
            let editor = patch
                .editor
                .take()
                .unwrap_or_else(|| self.config.editor.clone())
                .trim()
                .to_string();
            if editor.is_empty() {
                self.reply_settings(
                    reg,
                    token,
                    false,
                    Some("Default editor command cannot be empty".into()),
                );
                return;
            }
            let editor_rules = match patch.editor_rules.take() {
                Some(value) => match Config::parse_editor_rules(&value) {
                    Ok(rules) => rules,
                    Err(error) => {
                        self.reply_settings(reg, token, false, Some(error));
                        return;
                    }
                },
                None => self.config.editor_rules.clone(),
            };
            self.agents
                .send(uniterm_proto::CoreToAgent::EditorSettingsValidate {
                    client: token.0 as u64,
                    editor,
                    editor_rules,
                });
            return;
        }
        let guardrail_allowed_projects = match patch.guardrail_allowed_projects.take() {
            Some(value) => match Config::parse_guardrail_allowed_projects(&value) {
                Ok(selectors) => Some(selectors),
                Err(error) => {
                    self.reply_settings(reg, token, false, Some(error));
                    return;
                }
            },
            None => None,
        };
        if let Some(theme) = patch.theme {
            self.config.theme_preset = uniterm_core::ThemePreset::parse(&theme);
            self.config.theme = uniterm_core::Theme::named(&theme);
            for pane in self.panes.values_mut() {
                pane.term
                    .set_default_colors(self.config.theme.foreground, self.config.theme.background);
                let response = pane.term.take_responses();
                if !response.is_empty() {
                    Self::queue_pane_input(reg, pane, &response);
                }
            }
        }
        if let Some(status) = patch.status {
            self.config.status = status;
        }
        if let Some(top) = patch.status_top {
            self.config.status_position = if top {
                StatusPosition::Top
            } else {
                StatusPosition::Bottom
            };
        }
        if let Some(sidebar) = patch.sidebar {
            self.config.sidebar = sidebar;
        }
        if let Some(width) = patch.sidebar_width {
            self.config.sidebar_width = width.clamp(16, 40);
        }
        if let Some(sidebar) = patch.file_sidebar {
            self.config.file_sidebar = sidebar;
        }
        if let Some(width) = patch.file_sidebar_width {
            self.config.file_sidebar_width = width.clamp(22, 52);
        }
        if let Some(delivery) = patch.notification_delivery {
            self.config.notifications = uniterm_core::NotificationDelivery::parse(&delivery);
            if self.config.notifications == uniterm_core::NotificationDelivery::Off {
                self.pending_notifications.clear();
                self.notification = None;
            }
        }
        if let Some(completion) = patch.notify_completion {
            self.config.notify_completion = completion;
        }
        if let Some(focus) = patch.focus_follows_mouse {
            self.config.focus_follows_mouse = focus;
        }
        if let Some(confirm) = patch.confirm_close {
            self.config.confirm_close = confirm;
        }
        if let Some(confirm) = patch.confirm_tab_close {
            self.config.confirm_tab_close = confirm;
        }
        if let Some(limit) = patch.scrollback_limit {
            self.config.scrollback_limit = limit.clamp(100, 1_000_000);
            for pane in self.panes.values_mut() {
                pane.term.set_scrollback_limit(self.config.scrollback_limit);
            }
        }
        if let Some(restore) = patch.restore {
            self.config.restore = restore;
        }
        if let Some(limit) = patch.guardrail_max_active_runs {
            self.config.guardrails.max_active_runs =
                limit.clamp(1, uniterm_core::GUARDRAIL_MAX_ACTIVE_RUNS);
        }
        if let Some(limit) = patch.guardrail_max_role_panes {
            self.config.guardrails.max_role_panes =
                limit.clamp(1, uniterm_core::GUARDRAIL_MAX_ROLE_PANES);
        }
        if let Some(limit) = patch.guardrail_max_iterations {
            self.config.guardrails.max_iterations =
                limit.clamp(1, uniterm_core::GUARDRAIL_MAX_ITERATIONS);
        }
        if let Some(minutes) = patch.guardrail_max_elapsed_minutes {
            let max_minutes = uniterm_core::GUARDRAIL_MAX_ELAPSED_SECONDS / 60;
            self.config.guardrails.max_elapsed_seconds = minutes.clamp(1, max_minutes) * 60;
        }
        if let Some(selectors) = guardrail_allowed_projects {
            self.config.guardrail_allowed_projects = selectors;
        }
        if self.file_manager_visible() {
            self.sync_file_manager(false);
        } else {
            self.files.focused = false;
            self.stop_file_watches();
        }
        self.relayout();
        self.full_repaint_all(reg);
        self.agents.send(uniterm_proto::CoreToAgent::ConfigSave {
            client: token.0 as u64,
            text: self.config.to_text(),
        });
    }

    /// Create a task, append it to the event log, and keep the projection.
    fn create_task(&mut self, title: &str, status: uniterm_core::TaskStatus) -> u64 {
        let id = self.tasks.add(title, status);
        self.append_event(crate::eventlog::LogEvent::TaskCreated {
            id,
            title: title.to_string(),
            status,
        });
        id
    }

    /// The task-management snapshot: tasks ordered active-first.
    fn task_snapshot(&self) -> Vec<uniterm_proto::TaskEntry> {
        self.tasks
            .ordered()
            .into_iter()
            .map(|t| uniterm_proto::TaskEntry {
                id: t.id,
                title: t.title.clone(),
                status: t.status,
                notes: t.notes.clone(),
            })
            .collect()
    }

    /// Send the requesting client a fresh task snapshot (opens or refreshes
    /// its task-manager modal).
    fn reply_tasks(&mut self, reg: &Registry, token: Token) {
        let items = self.task_snapshot();
        if let Some(c) = self.clients.get_mut(&token) {
            c.queue(&encode_frame(&ServerMessage::Tasks { items }));
            c.flush();
            let _ = set_interest(reg, c, token);
        }
    }

    fn request_chrome_menu(
        &mut self,
        reg: &Registry,
        client: Token,
        menu: uniterm_proto::ChromeMenu,
        anchor: Rect,
        open_up: bool,
    ) {
        // The repaint removes any prior client-composited dropdown. Queue the
        // new anchor after it so the attach client composites in wire order.
        self.full_repaint_client(reg, client);
        if let Some(client_state) = self.clients.get_mut(&client) {
            client_state.queue(&encode_frame(&ServerMessage::OpenMenu {
                menu,
                x: anchor.x.saturating_add(1),
                y: anchor.y.saturating_add(1),
                width: anchor.w,
                open_up,
            }));
            client_state.flush();
            let _ = set_interest(reg, client_state, client);
        }
    }

    fn select_observatory_tab(&mut self, reg: &Registry, tab: ObservatoryTab) {
        if tab == self.observatory_tab {
            return;
        }
        if self.observatory_tab == ObservatoryTab::Files {
            self.files.focused = false;
            self.stop_file_watches();
        }
        self.observatory_tab = tab;
        self.agents
            .send(uniterm_proto::CoreToAgent::DevServerWatchSet {
                active: tab == ObservatoryTab::WebServers,
            });
        if tab == ObservatoryTab::Files {
            self.sync_file_manager(false);
        }
        self.relayout();
        self.full_repaint_all(reg);
    }

    /// Apply a loaded config and re-lay-out (used by the CLI after `bind`).
    pub fn set_config(&mut self, config: Config) {
        self.config = config;
        for pane in self.panes.values_mut() {
            pane.term.set_scrollback_limit(self.config.scrollback_limit);
            pane.term
                .set_default_colors(self.config.theme.foreground, self.config.theme.background);
        }
        if self.file_manager_visible() {
            self.sync_file_manager(false);
        } else {
            self.files.focused = false;
            self.stop_file_watches();
        }
        self.relayout();
    }

    // --- rendering ---------------------------------------------------------

    // --- event handlers ----------------------------------------------------

    /// Enter copy-mode on the active pane.
    fn enter_copy_mode(&mut self, reg: &Registry) {
        let active = self.windows[self.active_window].active;
        let Some(rect) = self.current_layout.rect_of(active) else {
            return;
        };
        if let Some(pane) = self.panes.get_mut(&active) {
            if pane.copy.is_none() {
                pane.copy = Some(CopyState::new(pane.term.grid(), rect));
                self.full_repaint_all(reg);
            }
        }
    }

    /// Route a key batch to the active pane's copy-mode handler. Returns whether
    /// the pane was in copy-mode (so the caller knows not to forward to the PTY).
    fn handle_copy_input(&mut self, reg: &Registry, bytes: &[u8]) -> bool {
        let active = self.windows[self.active_window].active;
        let Some(pane) = self.panes.get_mut(&active) else {
            return false;
        };
        let Some(copy) = pane.copy.as_mut() else {
            return false;
        };
        let action = copy.handle(bytes, pane.term.grid());
        match action {
            CopyAction::None => {}
            CopyAction::Redraw => self.full_repaint_all(reg),
            CopyAction::Exit => {
                pane.copy = None;
                self.full_repaint_all(reg);
            }
            CopyAction::Copy(text) => {
                pane.copy = None;
                let clip = crate::copymode::osc52(&text);
                self.send_raw_ops(reg, &clip);
                self.full_repaint_all(reg);
            }
        }
        true
    }

    fn shutdown(&mut self, _reg: &Registry) {
        for c in self.clients.values_mut() {
            c.queue(&encode_frame(&ServerMessage::Exited));
            c.flush();
        }
        // Record the lightweight hierarchy before deleting runtime state. A
        // later start gets fresh shells for these Tabs, never old PTY content.
        self.persist_workspace_definition();
        // Clean shutdown drops the terminal snapshot, which is the crash
        // marker: its absence tells the next start to rebuild structure from
        // the catalog. The event stream is retained, because tasks, the run
        // graph, the artifact ledger, and the audit trail are projections of
        // it and must outlive an intentional stop.
        self.agents
            .send(uniterm_proto::CoreToAgent::SnapshotDelete {
                name: self.name.clone(),
            });
        // The runtime applies messages in order, so a checkpoint falling due
        // later in this poll batch would recreate the snapshot and turn the
        // next start into a crash restore. Freeze durable writes now; the
        // socket and PTYs still drain normally.
        self.event_writes_enabled = false;
        self.running = false;
    }

    /// Rename the session: the socket file moves (the bound listener keeps
    /// serving; new attaches and the switcher use the new path), and the
    /// snapshot + event log follow so restore and observability stay keyed to
    /// the session. Ignored when the name is empty, unchanged, or taken.
    fn rename_session(&mut self, reg: &Registry, name: &str) {
        if uniterm_proto::validate_workspace_name(name).is_err() || name == self.name {
            return;
        }
        let new = name.to_string();
        let new_path = self.sock_path.with_file_name(format!("{new}.sock"));
        let Ok(new_lock) = WorkspaceLock::acquire(&new_path) else {
            return;
        };
        if new_path.exists()
            || crate::persist::exists(&new)
            || crate::eventlog::exists(&new)
            || crate::workspace_catalog::exists(&new)
            || std::fs::rename(&self.sock_path, &new_path).is_err()
        {
            return;
        }
        // The old-name snapshot is orphaned; persist() below writes the new one.
        self.agents
            .send(uniterm_proto::CoreToAgent::SnapshotDelete {
                name: self.name.clone(),
            });
        if self.workspace_catalog_enabled {
            self.agents
                .send(uniterm_proto::CoreToAgent::WorkspaceCatalogRename {
                    old: self.name.clone(),
                    new: new.clone(),
                });
        }
        self.agents.send(uniterm_proto::CoreToAgent::ControlRename {
            workspace: new.clone(),
            path: new_path
                .with_extension("control.sock")
                .to_string_lossy()
                .into_owned(),
        });
        let old_log_name = self.name.clone();
        self.append_event(crate::eventlog::LogEvent::WorkspaceRenamed {
            old: old_log_name.clone(),
            new: new.clone(),
        });
        self.log.rename_projection(&new);
        self.agents.send(uniterm_proto::CoreToAgent::EventRename {
            old: old_log_name,
            new: new.clone(),
        });
        self.sock_path = new_path;
        self.workspace_lock = new_lock;
        self.name = new;
        // Tell attached clients so their notion of "my socket" follows.
        let note = encode_frame(&ServerMessage::SessionRenamed {
            name: self.name.clone(),
        });
        for (tok, c) in self.clients.iter_mut() {
            if c.attached {
                c.queue(&note);
                c.flush();
                let _ = set_interest(reg, c, *tok);
            }
        }
        self.full_repaint_all(reg); // the status line shows the new name
        self.persist();
    }

    /// The session name (socket stem), used for the snapshot filename.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Keep the socket discoverable until the runtime has flushed the final
        // Workspace definition and cleanup messages queued by shutdown.
        self.agents.shutdown();
        let _ = remove_socket_if_unchanged(&self.sock_path, self.socket_identity);
    }
}

fn sanitize_metadata_key(key: &str) -> String {
    key.trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        .take(32)
        .collect()
}

fn system_hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|hostname| !hostname.is_empty())
        .map(|hostname| sanitize_chrome_text(&hostname, 255))
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

/// Convert the shell's cooperative OSC 7 file URI into a local absolute path.
///
/// The authority is intentionally ignored: shells commonly include the local
/// hostname, while the PTY already has the same filesystem authority as the
/// process that emitted the report. Relative paths and malformed escapes are
/// rejected so they cannot turn into a surprising restore directory.
fn osc7_working_directory(uri: &str) -> Option<PathBuf> {
    let encoded = if let Some(rest) = uri.strip_prefix("file://") {
        if rest.starts_with('/') {
            rest
        } else {
            &rest[rest.find('/')?..]
        }
    } else {
        let rest = uri.strip_prefix("file:")?;
        rest.starts_with('/').then_some(rest)?
    };

    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_digit(*bytes.get(index + 1)?)?;
            let low = hex_digit(*bytes.get(index + 2)?)?;
            let byte = high * 16 + low;
            if byte == 0 {
                return None;
            }
            decoded.push(byte);
            index += 3;
        } else {
            if bytes[index] == 0 || bytes[index].is_ascii_control() {
                return None;
            }
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let path = PathBuf::from(String::from_utf8(decoded).ok()?);
    path.is_absolute().then_some(path)
}

fn update_working_directory(cached: &mut Option<PathBuf>, reported: Option<&str>) -> bool {
    let Some(path) = reported.and_then(osc7_working_directory) else {
        return false;
    };
    if cached.as_ref() == Some(&path) {
        return false;
    }
    *cached = Some(path);
    true
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn pane_input_has_capacity(pending: usize, incoming: usize) -> bool {
    pending.saturating_add(incoming) <= MAX_PENDING_INPUT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_search_path_accepts_only_bounded_absolute_entries() {
        assert_eq!(
            normalize_remote_search_path(vec![
                "/home/test/bin".into(),
                "/usr/bin".into(),
                "/home/test/bin".into(),
            ]),
            Some(vec!["/home/test/bin".into(), "/usr/bin".into()])
        );
        assert_eq!(normalize_remote_search_path(Vec::new()), None);
        assert_eq!(
            normalize_remote_search_path(vec!["relative/bin".into()]),
            None
        );
        assert_eq!(
            normalize_remote_search_path(vec!["/bin".into(); MAX_REMOTE_SEARCH_PATH_ENTRIES + 1]),
            None
        );
    }

    #[test]
    fn remote_search_path_widens_but_never_narrows_agent_lookup() {
        let existing = vec!["/usr/bin".to_string(), "/opt/agents/bin".to_string()];
        let merged = merge_search_paths(
            vec!["/home/u/.local/bin".to_string(), "/usr/bin".to_string()],
            &existing,
        );
        assert_eq!(
            merged,
            vec![
                "/home/u/.local/bin".to_string(),
                "/usr/bin".to_string(),
                "/opt/agents/bin".to_string()
            ]
        );
    }

    #[test]
    fn slow_client_queue_is_bounded_and_disconnected() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut client = Client {
            stream: UnixStream::from_std(stream),
            decoder: FrameDecoder::new(),
            renderer: Renderer::new(),
            outbuf: Vec::new(),
            out_offset: 0,
            render_end: None,
            attached: true,
            direct_only: false,
            direct: None,
            overlay: false,
            cols: 80,
            rows: 24,
            dead: false,
            repaint_pending: false,
            write_interest: false,
            pending_wait: None,
        };
        client.queue(&vec![0; MAX_PENDING_CLIENT]);
        assert_eq!(client.outbuf.len(), MAX_PENDING_CLIENT);
        client.queue(&[1]);
        assert!(client.dead);
        assert!(client.outbuf.is_empty());
    }

    #[test]
    fn protocol_output_before_first_render_does_not_defer_the_frame() {
        let (stream, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        let mut client = Client {
            stream: UnixStream::from_std(stream),
            decoder: FrameDecoder::new(),
            renderer: Renderer::new(),
            outbuf: Vec::new(),
            out_offset: 0,
            render_end: None,
            attached: true,
            direct_only: false,
            direct: None,
            overlay: false,
            cols: 120,
            rows: 40,
            dead: false,
            repaint_pending: false,
            write_interest: false,
            pending_wait: None,
        };
        let title = encode_frame(&ServerMessage::WindowTitle {
            title: "host: Work".into(),
        });
        let render = encode_frame(&ServerMessage::RenderOps(b"first frame".to_vec()));
        let nested = encode_frame(&ServerMessage::NestedInput { enabled: false });

        client.queue(&title);
        client.queue_render(&render);
        client.queue(&nested);
        assert!(!client.repaint_pending);
        client.flush();

        assert!(!client.wants_write());
        assert!(client.render_end.is_none());
        let expected = [title, render, nested].concat();
        let mut received = vec![0; expected.len()];
        peer.read_exact(&mut received).unwrap();
        assert_eq!(received, expected);
    }

    #[test]
    fn backpressured_render_bursts_collapse_to_one_repaint() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut client = Client {
            stream: UnixStream::from_std(stream),
            decoder: FrameDecoder::new(),
            renderer: Renderer::new(),
            outbuf: Vec::new(),
            out_offset: 0,
            render_end: None,
            attached: true,
            direct_only: false,
            direct: None,
            overlay: false,
            cols: 480,
            rows: 135,
            dead: false,
            repaint_pending: false,
            write_interest: false,
            pending_wait: None,
        };
        let first = vec![0; 2 * 1024 * 1024];
        let later = vec![1; 2 * 1024 * 1024];
        client.queue_render(&first);
        for _ in 0..16 {
            client.queue_render(&later);
        }

        assert_eq!(client.outbuf.len(), first.len());
        assert!(client.repaint_pending);
        assert!(!client.dead);
        let before = client.out_offset;
        client.flush();
        assert_eq!(client.out_offset, before);
    }

    #[test]
    fn osc7_working_directories_are_absolute_and_percent_decoded() {
        assert_eq!(
            osc7_working_directory("file://work-mac/Users/max/Work/Uniterm%20CLI"),
            Some(PathBuf::from("/Users/max/Work/Uniterm CLI"))
        );
        assert_eq!(
            osc7_working_directory("file:///tmp/project"),
            Some(PathBuf::from("/tmp/project"))
        );
        assert_eq!(
            osc7_working_directory("file:/tmp/project"),
            Some(PathBuf::from("/tmp/project"))
        );
        assert_eq!(osc7_working_directory("file:relative"), None);
        assert_eq!(osc7_working_directory("file:///tmp/bad%2"), None);
        assert_eq!(osc7_working_directory("https://example.com/tmp"), None);
    }

    #[test]
    fn cwd_cache_advances_only_for_a_new_valid_osc7_path() {
        let mut cwd = Some(PathBuf::from("/work/old"));
        assert!(!update_working_directory(
            &mut cwd,
            Some("file:///work/old")
        ));
        assert!(!update_working_directory(&mut cwd, Some("file:relative")));
        assert!(update_working_directory(
            &mut cwd,
            Some("file://host/work/new")
        ));
        assert_eq!(cwd, Some(PathBuf::from("/work/new")));
    }

    #[test]
    fn pane_input_capacity_rejects_only_overflow() {
        assert!(pane_input_has_capacity(MAX_PENDING_INPUT, 0));
        assert!(pane_input_has_capacity(MAX_PENDING_INPUT - 1, 1));
        assert!(!pane_input_has_capacity(MAX_PENDING_INPUT, 1));
        assert!(!pane_input_has_capacity(usize::MAX, 1));
    }

    #[test]
    fn sidebar_scope_uses_full_and_compact_labels() {
        assert_eq!(SidebarScope::Project.label(23), "project");
        assert_eq!(SidebarScope::Workspace.label(23), "workspace");
        assert_eq!(SidebarScope::Project.label(15), "proj");
        assert_eq!(SidebarScope::Workspace.label(15), "all");
    }
}
