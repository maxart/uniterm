//! `uniterm-proto` - shared wire and channel message types.
//!
//! Two boundaries are defined here:
//!
//! 1. The **runtime boundary** ([`CoreToAgent`], [`AgentToCore`]): the typed
//!    messages passed over channels between the single-threaded `mio` core loop
//!    and the `tokio` agent runtime. This is the seam from Decision R1
//!    (`docs/01-language-decision.md`, `docs/03-system-architecture.md`). The
//!    two halves communicate *only* through these types - never shared mutable
//!    state - which is what keeps the render path lock-free.
//!
//! 2. The **client boundary** ([`ClientMessage`], [`ServerMessage`]): what a
//!    thin attach client and the server exchange over the Unix socket. Stubbed
//!    in Phase 0; serialized (serde) when the client lands in Phase 1.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use uniterm_core::AgentStatus;

pub use uniterm_core::orchestrate::RoleProviderSelection;
/// The pane identifier lives in core (the layout tree and grid model use it);
/// re-exported here so protocol users have one name for it.
pub use uniterm_core::{ArtifactId, ArtifactKind, PaneId, ProjectId, RoleId, RunId, SplitDir};

/// Version of the neutral local control API.
pub const CONTROL_API_VERSION: u32 = 1;
/// Maximum encoded request or response line retained by the control transport.
pub const CONTROL_MAX_FRAME_BYTES: u32 = 1024 * 1024;
/// Maximum simultaneous local automation connections for one Workspace.
pub const CONTROL_MAX_CONNECTIONS: u32 = 128;
/// Maximum pending response and event frames retained for one connection.
pub const CONTROL_MAX_QUEUED_FRAMES: u32 = 64;
/// Maximum parsed requests waiting for the agent-runtime dispatcher.
pub const CONTROL_MAX_QUEUED_REQUESTS: u32 = 64;

/// One NDJSON request. Every operation names its Workspace explicitly so a
/// connection can never inherit an unrelated agentic scope.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlRequest {
    pub version: u32,
    pub id: u64,
    pub workspace: String,
    #[serde(flatten)]
    pub command: ControlCommand,
}

/// Stable first-slice command vocabulary for local automation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum ControlCommand {
    Capabilities,
    WorkspaceSnapshot,
    PaneList,
    PaneRead {
        pane: PaneId,
        lines: u32,
    },
    /// Send human-writable UTF-8 terminal input. JSON escapes cover control
    /// bytes such as newline and escape without exposing integer arrays.
    PaneSend {
        pane: PaneId,
        text: String,
    },
    Subscribe {
        after_sequence: u64,
    },
    InstructionList,
    InstructionAdd {
        pane: PaneId,
        text: String,
    },
    InstructionReplace {
        id: u64,
        text: String,
    },
    InstructionCancel {
        id: u64,
    },
    InstructionSendNow {
        id: u64,
    },
    WorktreeList,
    WorktreeAdd {
        name: String,
        repository: String,
        path: String,
        base: Option<String>,
    },
    WorktreeOpen {
        project: ProjectId,
    },
    WorktreeRemove {
        project: ProjectId,
        force: bool,
    },
    WorktreeCleanup {
        project: ProjectId,
    },
    /// Inspect the durable native run graph in this Workspace.
    RunList {
        project: Option<ProjectId>,
        active_only: bool,
    },
    /// Inspect the durable typed artifact ledger in this Workspace.
    ArtifactList {
        project: Option<ProjectId>,
        run: Option<RunId>,
        #[serde(default)]
        include_superseded: bool,
    },
    /// Launch a native workflow or relay through the same semantic server path
    /// as the interactive New Task surface.
    OrchestrationStart {
        launch: OrchestrationLaunch,
    },
    /// Fork one active native run into a freshly created Git worktree Project.
    RunFork {
        fork: RunForkRequest,
    },
    ProjectCreate {
        name: String,
        root: String,
    },
    ProjectRename {
        project: ProjectId,
        name: String,
    },
    ProjectMove {
        project: ProjectId,
        direction: ProjectMoveDirection,
    },
    ProjectSwitch {
        project: ProjectId,
    },
    /// Removing a Project closes every Pane it owns, so the caller must state
    /// that a human confirmed it; an unconfirmed request is refused and the
    /// refusal is recorded as a guardrail decision.
    ProjectRemove {
        project: ProjectId,
        #[serde(default)]
        confirmed: bool,
    },
    TabCreate {
        project: ProjectId,
    },
    TabRename {
        project: ProjectId,
        tab: u32,
        name: String,
    },
    TabMove {
        project: ProjectId,
        tab: u32,
        direction: TabMoveDirection,
    },
    HierarchyFocus {
        project: ProjectId,
        tab: u32,
        pane: Option<u32>,
    },
    AgentList,
    AgentLaunch {
        agent: String,
        target: LaunchTarget,
    },
    AgentFocus {
        pane: PaneId,
    },
    AgentStop {
        pane: PaneId,
    },
    /// A bulk stop closes every agent Pane in `scope`; the caller must state
    /// that a human confirmed it (see [`ControlCommand::ProjectRemove`]).
    AgentStopAll {
        scope: StopScope,
        #[serde(default)]
        confirmed: bool,
    },
    TaskList,
    TaskCreate {
        title: String,
    },
    TaskSetStatus {
        id: u64,
        status: uniterm_core::TaskStatus,
    },
    TaskRetitle {
        id: u64,
        title: String,
    },
    TaskDelete {
        id: u64,
    },
    WaitingList,
    WaitingAct {
        id: u64,
        action: WaitingAction,
        #[serde(default)]
        text: String,
    },
    OrchestrationSubmit {
        kind: OrchestrationKind,
        token: u64,
        status: SubmissionStatus,
        verdict: Option<String>,
        #[serde(default)]
        summary: String,
        #[serde(default)]
        artifacts: Vec<ArtifactClaim>,
    },
}

/// One request-correlated NDJSON response.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlResponse {
    pub version: u32,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ControlResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ControlError>,
}

impl ControlResponse {
    /// Build a successful response correlated to `id`.
    pub fn ok(id: u64, result: ControlResult) -> Self {
        Self {
            version: CONTROL_API_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build a structured failure response correlated to `id`.
    pub fn error(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            version: CONTROL_API_VERSION,
            id,
            result: None,
            error: Some(ControlError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

/// Typed response payloads. New capabilities append variants without changing
/// the binary attach protocol.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ControlResult {
    Capabilities {
        protocol_version: u32,
        capabilities: Vec<String>,
        max_frame_bytes: u32,
        max_connections: u32,
        max_queued_frames: u32,
        max_queued_requests: u32,
    },
    Workspace {
        name: String,
        sequence: u64,
        active_project: ProjectId,
        projects: Vec<ProjectInfo>,
    },
    Panes {
        workspace: String,
        panes: Vec<PaneInfo>,
    },
    PaneOutput {
        pane: PaneId,
        found: bool,
        text: String,
        truncated: bool,
    },
    PaneSent {
        pane: PaneId,
        found: bool,
        accepted: bool,
    },
    Subscribed {
        subscription: u64,
        current_sequence: u64,
    },
    Instructions {
        workspace: String,
        items: Vec<InstructionEntry>,
    },
    InstructionChanged {
        id: u64,
        found: bool,
        accepted: bool,
        items: Vec<InstructionEntry>,
    },
    Worktrees(WorktreeResult),
    Runs {
        /// Workspace that owns this graph projection.
        workspace: String,
        /// Retained runs after the requested filters are applied.
        runs: Vec<RunEntry>,
    },
    Artifacts {
        /// Workspace that owns this artifact projection.
        workspace: String,
        /// Retained artifacts after the requested filters are applied.
        artifacts: Vec<ArtifactEntry>,
    },
    OrchestrationStarted {
        run: RunId,
    },
    RunForked(RunForkResult),
    Fleet {
        entries: Vec<FleetEntry>,
    },
    Tasks {
        items: Vec<TaskEntry>,
    },
    Waiting {
        items: Vec<WaitingEntry>,
    },
    Mutation {
        resource: String,
        id: Option<u64>,
        found: bool,
        accepted: bool,
    },
    OrchestrationSubmitted {
        kind: OrchestrationKind,
        token: u64,
        accepted: bool,
    },
}

/// Stable machine-readable failure returned without closing a valid connection.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlError {
    pub code: String,
    pub message: String,
}

/// An event frame emitted after a successful subscription response.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ControlEvent {
    pub version: u32,
    pub subscription: u64,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub workspace: String,
    pub event: serde_json::Value,
}

/// Terminal failure for an accepted subscription. The connection remains
/// usable for ordinary requests and a later subscription attempt.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlStreamError {
    pub version: u32,
    pub subscription: u64,
    pub code: String,
    pub message: String,
}

/// Every outbound NDJSON line has an explicit frame kind.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ControlFrame {
    Response(ControlResponse),
    Event(ControlEvent),
    StreamError(ControlStreamError),
}

// ---------------------------------------------------------------------------
// Runtime boundary: core loop  ->  agent runtime
// ---------------------------------------------------------------------------

/// Messages the core loop sends to the agent runtime.
///
/// These are produced on the hot path but carry no rendering work: the core
/// recognizes a structured event (e.g. an OSC 777 envelope already flowing
/// through the VT parser) and hands it off. The agent runtime is *woken by an
/// event, never by a poll* - see `docs/03-system-architecture.md`.
#[derive(Clone, Debug)]
pub enum CoreToAgent {
    /// A pane emitted a structured OSC 777 agent event (raw JSON payload).
    OscAgentEvent { pane: PaneId, payload: String },
    /// A pane's PTY reached EOF; its child process is gone.
    PtyExited { pane: PaneId },
    /// A pane closed (user action).
    PaneClosed { pane: PaneId },
    /// Gather every registry provider's on-disk facts (PATH probe, connector
    /// state) for the Manage Agents snapshot. Disk work stays off the core
    /// loop; the reply is [`AgentToCore::AgentsDiskState`] tagged with the
    /// requesting client so the core can route the merged snapshot back.
    AgentsDiskQuery {
        client: u64,
        search_path: Vec<String>,
    },
    /// Install/remove a provider's notify-hook connector (a settings-file
    /// edit), then report fresh disk state like [`CoreToAgent::AgentsDiskQuery`].
    ConnectorToggle {
        agent: String,
        client: u64,
        search_path: Vec<String>,
    },
    /// Atomically persist canonical configuration text. The server already
    /// applied the parsed settings in memory; disk I/O stays on this runtime.
    ConfigSave { client: u64, text: String },
    /// Validate proposed editor commands against `$PATH` before the core
    /// commits them to the live Settings projection.
    EditorSettingsValidate {
        client: u64,
        editor: String,
        editor_rules: Vec<uniterm_core::EditorRule>,
    },
    /// Validate the selected file editor at open time. This also catches a
    /// stale or hand-edited config whose executable disappeared.
    EditorOpen {
        project: ProjectId,
        path: String,
        command: String,
    },
    /// Persist an already serialized structural/grid snapshot. Serialization
    /// happens from an immutable core projection; filesystem I/O stays on the
    /// runtime side of the seam.
    SnapshotSave { name: String, bytes: Vec<u8> },
    /// Remove the restore artifact after an intentional Workspace shutdown.
    SnapshotDelete { name: String },
    /// Append one event-log record prepared by the core projection.
    EventAppend { name: String, line: String },
    /// Follow a Workspace rename without opening files on the core loop.
    EventRename { old: String, new: String },
    /// Remove the durable stream after an intentional Workspace shutdown.
    EventDelete { name: String },
    /// Append one lightweight Workspace-definition event. This catalog keeps
    /// only Workspaces, Project roots, and Tabs across intentional shutdowns.
    WorkspaceCatalogAppend { name: String, line: String },
    /// Follow a Workspace rename in the durable definition catalog.
    WorkspaceCatalogRename { old: String, new: String },
    /// Read the Workspace catalog and probe sibling sockets away from the mio
    /// loop, then route the result back to the requesting attach client.
    WorkspaceCatalogQuery { client: u64 },
    /// Event-driven evidence snapshot after PTY output or a foreground process
    /// group change. The runtime may inspect process metadata and provider
    /// manifests; it never receives or mutates a grid.
    PaneEvidence {
        pane: PaneId,
        foreground_pid: Option<i32>,
        /// The foreground process group changed since the previous evidence
        /// snapshot, so provider process identity must be resolved again.
        process_changed: bool,
        tail: String,
        /// The OSC 0/2 title the application last set: agent TUIs animate
        /// their busy spinner and blocked markers there, away from typed text.
        title: String,
        bound_agent: Option<String>,
    },
    /// Inspect one event-driven screen tail for local web-server announce
    /// lines. Detection and TCP liveness probes stay off the mio core loop.
    DevServerEvidence { pane: PaneId, tail: String },
    /// Arm periodic liveness confirmation only while its Observatory surface
    /// is visible. Announce-line detection remains event-driven at all times.
    DevServerWatchSet { active: bool },
    /// Remove one server from the runtime's event-driven deduplication set
    /// after a liveness task reported it down.
    DevServerForget { pane: PaneId, port: u16 },
    /// Deliver an agent attention event through the operating system. Process
    /// spawning remains on the tokio side of the runtime seam.
    SystemNotification { title: String, body: String },
    /// List one file-manager directory inside a Project sandbox.
    FileList {
        project: ProjectId,
        root: String,
        directory: String,
    },
    /// Keep non-recursive event watches exactly on the expanded directories.
    FileWatchSet {
        project: ProjectId,
        root: String,
        directories: Vec<String>,
    },
    /// Enable or disable the repository-root-keyed change watcher used by the
    /// visible file manager. `None` removes the Project subscription.
    GitChangeWatchSet {
        project: ProjectId,
        root: Option<String>,
    },
    /// Apply one sandboxed file-manager operation.
    FileMutate {
        project: ProjectId,
        root: String,
        operation: FileOperation,
    },
    /// Validate declared orchestration artifacts outside the mio core loop.
    ArtifactValidate {
        kind: OrchestrationKind,
        task_id: u64,
        token: u64,
        project_root: String,
        expected: Vec<ArtifactClaim>,
        reported: Vec<ArtifactClaim>,
    },
    /// Replace the runtime's event-driven artifact watch set. Empty removes
    /// every artifact watch and arms no timer.
    ArtifactWatchSet { projects: Vec<ArtifactWatchProject> },
    /// Re-observe one artifact after its watched parent directory changed.
    ArtifactObserve {
        artifact: ArtifactId,
        project_root: String,
        claim: ArtifactClaim,
    },
    RelayCheckpointCreate {
        task_id: u64,
        token: u64,
        project_root: String,
    },
    RelayCheckpointRollback {
        waiting_id: u64,
        task_id: u64,
        project_root: String,
        checkpoint: String,
    },
    /// Return one neutral API response to its Tokio-owned connection.
    ControlResponse {
        connection: u64,
        response: ControlResponse,
    },
    /// Move the private control listener with an authoritative Workspace rename.
    ControlRename { workspace: String, path: String },
    /// Run one Git-authoritative worktree operation away from the mio loop.
    WorktreeRun {
        request: u64,
        workspace: String,
        operation: WorktreeRuntimeOperation,
    },
}

// ---------------------------------------------------------------------------
// Runtime boundary: agent runtime  ->  core loop
// ---------------------------------------------------------------------------

/// Messages the agent runtime sends back to the core loop.
///
/// The agent runtime never touches a grid directly; it asks the core to act.
/// Every variant here is applied by the core loop, which owns all grid state.
#[derive(Clone, Debug)]
pub enum AgentToCore {
    /// Inject text into a pane as a bracketed paste (prompt delivery, answers).
    InjectText {
        pane: PaneId,
        text: String,
    },
    /// The reconciled agent status for a pane changed; redraw its badge.
    SetAgentStatus {
        pane: PaneId,
        status: AgentStatus,
    },
    /// Spawn a new pane running a command (workflow role launch, quick task).
    SpawnPane {
        command: String,
        cwd: String,
    },
    /// Surface a waiting-queue item to the human (stringly-typed for Phase 0).
    WaitingItem {
        pane: PaneId,
        summary: String,
    },
    /// Durable append or snapshot persistence failed. The core keeps the
    /// current in-memory state but must not claim a newer checkpoint exists.
    DurabilityError {
        workspace: String,
        operation: String,
        error: String,
    },
    ArtifactValidated {
        kind: OrchestrationKind,
        task_id: u64,
        token: u64,
        artifacts: Vec<ArtifactObservation>,
        error: Option<String>,
    },
    /// One or more watched artifact paths changed. The core resolves current
    /// ownership before requesting fresh filesystem facts.
    ArtifactFilesChanged {
        artifacts: Vec<ArtifactId>,
    },
    ArtifactObserved {
        artifact: ArtifactId,
        observation: Option<ArtifactObservation>,
        missing: bool,
        error: Option<String>,
    },
    RelayCheckpointCreated {
        task_id: u64,
        token: u64,
        checkpoint: Option<String>,
        error: Option<String>,
    },
    RelayCheckpointRolledBack {
        waiting_id: u64,
        task_id: u64,
        checkpoint: String,
        error: Option<String>,
    },
    /// Reply to [`CoreToAgent::AgentsDiskQuery`]/[`CoreToAgent::ConnectorToggle`]:
    /// per-provider disk facts. The core merges in its own pane state (running
    /// counts) and answers the tagged client.
    AgentsDiskState {
        client: u64,
        providers: Vec<ProviderDiskState>,
    },
    ConfigSaved {
        client: u64,
        error: Option<String>,
    },
    /// Result of a low-frequency editor Settings validation request.
    EditorSettingsValidated {
        client: u64,
        editor: String,
        editor_rules: Vec<uniterm_core::EditorRule>,
        error: Option<String>,
    },
    /// Result of resolving an editor immediately before opening a file.
    EditorResolved {
        project: ProjectId,
        path: String,
        command: String,
        error: Option<String>,
    },
    /// One or more local web servers were announced by a pane.
    DevServersDetected {
        pane: PaneId,
        servers: Vec<DetectedDevServer>,
    },
    /// A tracked local web server stopped accepting loopback connections.
    DevServerDown {
        pane: PaneId,
        port: u16,
    },
    AgentDetected {
        pane: PaneId,
        foreground_pid: Option<i32>,
        agent: Option<String>,
        status: Option<AgentStatus>,
        authority: DetectionAuthority,
        evidence: String,
        provenance: DetectionProvenance,
    },
    /// A watched provider manifest changed and a validated catalog snapshot
    /// replaced the runtime's prior snapshot. The core resubmits current pane
    /// evidence so reload takes effect without waiting for new PTY output.
    ProviderManifestsReloaded,
    FileListing {
        project: ProjectId,
        directory: String,
        entries: Vec<FileEntry>,
        /// More immediate children existed than the bounded UI projection can
        /// retain. The returned prefix remains browsable.
        truncated: bool,
        error: Option<String>,
    },
    /// A watched directory changed. The core requests a fresh listing only
    /// while the optional file rail is visible.
    FileChanged {
        project: ProjectId,
        directory: String,
    },
    FileMutationDone {
        project: ProjectId,
        directory: String,
        error: Option<String>,
    },
    /// Repository change totals recomputed after the watcher debounce.
    GitChangeStats {
        project: ProjectId,
        stats: Option<GitChangeStats>,
    },
    /// A validated NDJSON request from the Tokio-owned control socket.
    ControlRequest {
        connection: u64,
        request: ControlRequest,
    },
    /// A Git worktree operation completed and is ready for core projection.
    WorktreeFinished {
        request: u64,
        result: WorktreeResult,
    },
    /// Result of [`CoreToAgent::WorkspaceCatalogQuery`].
    WorkspaceCatalogState {
        client: u64,
        entries: Vec<WorkspaceInfo>,
    },
}

/// Compact working-tree totals rendered above the visible file manager.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitChangeStats {
    /// Number of tracked files changed against `HEAD`.
    pub files_changed: u32,
    /// Inserted tracked lines against `HEAD`.
    pub insertions: u32,
    /// Deleted tracked lines against `HEAD`.
    pub deletions: u32,
    /// Untracked files, excluding ignored files.
    pub untracked: u32,
}

impl GitChangeStats {
    /// Whether the compact badge has anything to show.
    pub fn has_changes(&self) -> bool {
        self.insertions != 0 || self.deletions != 0 || self.untracked != 0
    }
}

/// One immediate child returned to the integrated file manager.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
}

/// Maximum immediate children retained for one file-manager directory.
/// Oversized directories remain browsable through this bounded prefix rather
/// than exhausting the server's memory.
pub const FILE_LISTING_LIMIT: usize = 10_000;

/// A Project-root-sandboxed file operation.
#[derive(Clone, Debug)]
pub enum FileOperation {
    CreateFile { parent: String, name: String },
    CreateDirectory { parent: String, name: String },
    Rename { path: String, name: String },
    Delete { path: String },
}

/// One registry provider's on-disk facts, gathered on the agent runtime
/// (PATH stat probes and settings reads never run on the core loop).
#[derive(Clone, Debug)]
pub struct ProviderDiskState {
    /// Registry id (e.g. "claude").
    pub id: String,
    /// Whether the CLI is on `$PATH` right now.
    pub installed: bool,
    pub connector: ConnectorStatus,
}

// ---------------------------------------------------------------------------
// Client boundary: attach client  <->  server  (Phase 0 stubs)
// ---------------------------------------------------------------------------

/// Input and lifecycle a client sends to the server.
///
/// Variants are append-only because bincode encodes their ordinal on the wire.
/// Inserting or reordering one disconnects clients attached to an older server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    Attach {
        term: String,
        cols: u16,
        rows: u16,
    },
    Input(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    Detach,
    /// A built-in multiplexer command from a prefix keybinding.
    Command(Command),
    /// Request session info (window/pane counts) without attaching; used by
    /// `uniterm ls`. The server replies with [`ServerMessage::Info`].
    ListInfo,
    /// Ask the server to stop (kill its panes and exit); used by `uniterm kill`.
    KillServer,
    /// Ask the server to send a full frame (redraw). The client uses this to
    /// restore pane content after a client-side overlay closes.
    Refresh,
    /// The attach terminal regained focus or resumed after suspension. Render
    /// caches may no longer describe the physical display, so the server
    /// sends this client a complete authoritative frame.
    FocusGained,
    /// Launch a new task from the New Task overlay. Workflow and relay forms
    /// enter the native guarded orchestration path; a plain task creates one
    /// Pane and injects `prompt` as input.
    NewTask {
        prompt: String,
        relay: bool,
        /// The agent to run (`@agent` in the input); `None` = first installed.
        agent: Option<String>,
        /// Explicit `@role=provider` choices. Unspecified roles use `agent`,
        /// then the first installed provider.
        role_providers: Vec<RoleProviderSelection>,
        /// Launch this workflow template (role panes + the engine) instead of
        /// a single task pane.
        workflow: Option<String>,
        /// Exact Project name or canonical root. `None` selects the active
        /// Project in this Workspace.
        project: Option<String>,
    },
    /// The completion contract (docs/07): an agent inside a role pane ran
    /// `uniterm workflow submit <token> ...`; the CLI delivers it here over
    /// the socket named in `$UNITERM_SOCKET`. The engine ignores forged or
    /// stale tokens.
    WorkflowSubmit {
        token: u64,
        failed: bool,
        /// `approved` / `fix` / `replan` (verifier only).
        verdict: Option<String>,
        /// One-line findings, echoed to the role that receives the loopback.
        summary: String,
    },
    /// Request the Observatory fleet snapshot; the server replies with
    /// [`ServerMessage::Fleet`].
    Observatory,
    /// Request the task list; the server replies with [`ServerMessage::Tasks`].
    Tasks,
    /// Request the Manage Agents snapshot (registry + install/connector/running
    /// state); the server replies with [`ServerMessage::Agents`].
    Agents,
    /// Install or remove the provider's notify-hook connector (whichever its
    /// current state implies), then reply with a fresh [`ServerMessage::Agents`].
    ConnectorToggle {
        agent: String,
    },
    /// Start an agent from the Manage Agents modal.
    AgentLaunch {
        agent: String,
        target: LaunchTarget,
    },
    /// Focus the exact agent pane selected in the Observatory.
    AgentFocus {
        pane: PaneId,
    },
    /// Stop one selected agent pane from the Observatory.
    AgentStop {
        pane: PaneId,
    },
    /// Stop every running agent within `scope` by closing its pane, then
    /// reply with a fresh [`ServerMessage::Agents`]. The scope is explicit on
    /// the wire (invariant 9: a bulk action must never reach into unrelated
    /// work, and narrowing it later must not be a protocol migration).
    AgentsStopAll {
        scope: StopScope,
        /// The human confirmed the bulk stop in the client. The server
        /// records the decision before closing any Pane and refuses an
        /// unconfirmed request.
        confirmed: bool,
    },
    /// Request autocomplete data for the New Task input; the server replies
    /// with [`ServerMessage::Suggestions`] (project names from task history).
    Suggest,
    /// Task-manager actions; each is applied, logged, and answered with a
    /// fresh [`ServerMessage::Tasks`] so the open modal refreshes.
    TaskSetStatus {
        id: u64,
        status: uniterm_core::TaskStatus,
    },
    TaskRetitle {
        id: u64,
        title: String,
    },
    TaskDelete {
        id: u64,
    },
    /// Create a task without launching a pane (New Task overlay `/save <title>`).
    SaveTask {
        title: String,
    },
    /// Rename the active window (shown in the status line, persisted). An
    /// empty name clears back to the bare number. Not a [`Command`] because
    /// commands stay `Copy`.
    RenameWindow {
        name: String,
    },
    /// Rename the session: the socket, snapshot, and event log follow the new
    /// name. Ignored if empty/unchanged or the name is already taken.
    RenameSession {
        name: String,
    },
    /// Request the durable Workspace > Project > Tab > Pane projection.
    WorkspaceState,
    /// Add a project to this Workspace and open its first Tab at `root`.
    ProjectCreate {
        name: String,
        root: String,
    },
    /// Rename a project without changing its stable scope id.
    ProjectRename {
        project: ProjectId,
        name: String,
    },
    /// Move a Project one place in the durable Workspace ordering.
    ProjectMove {
        project: ProjectId,
        direction: ProjectMoveDirection,
    },
    /// Switch to the project's last active Tab.
    ProjectSwitch {
        project: ProjectId,
    },
    /// Remove a project and close every Pane it owns. `confirmed` carries the
    /// human's explicit confirmation from the client; the server records the
    /// guardrail decision before the first Pane closes and refuses an
    /// unconfirmed request.
    ProjectRemove {
        project: ProjectId,
        confirmed: bool,
    },
    /// Install or merge a validated Desktop hierarchy. The migration CLI
    /// canonicalizes paths and resolves Workspace conflicts before this
    /// message reaches the server; the server remains authoritative for
    /// creating Projects, Tabs, and fresh shell Panes.
    WorkspaceImport {
        workspace: ImportedWorkspace,
        mode: WorkspaceImportMode,
    },
    /// Open/refresh the Settings surface from server-authoritative values.
    Settings,
    /// Apply a schema-backed partial update, then persist it atomically.
    SettingsApply(SettingsPatch),
    /// Explain the evidence and authority behind one pane's detected agent.
    AgentExplain {
        pane: Option<PaneId>,
    },
    /// Publish a metadata row for one Pane. Empty values remove the key. A TTL
    /// arms one event-driven expiry deadline rather than a periodic sweep.
    PaneMetadata {
        pane: PaneId,
        key: String,
        value: String,
        ttl_seconds: Option<u64>,
    },
    /// Publish durable Project metadata such as branch or environment.
    ProjectMetadata {
        project: ProjectId,
        key: String,
        value: String,
    },
    /// The client started (or stopped) compositing an overlay over the frame.
    /// While one is up, the client's terminal state no longer matches the
    /// server's per-client render caches (cursor/SGR), so incremental damage
    /// must re-emit absolute positions and styles every batch.
    OverlayVisible {
        on: bool,
    },
    /// A mouse event at 1-based cell `(x, y)`. The server resolves it: over a
    /// pane it focuses the pane; over a status-line window number it selects
    /// that window.
    Mouse {
        x: u16,
        y: u16,
        kind: MouseKind,
    },
    /// Request the live Workspace > Project > Tab > Pane projection.
    PaneList,
    /// Focus one stable Pane id anywhere in this Workspace.
    PaneFocus {
        pane: PaneId,
    },
    /// Focus one 1-based Tab and optional 1-based Pane ordinal within a
    /// Project. A missing Pane keeps that Tab's remembered active Pane.
    HierarchyFocus {
        project: ProjectId,
        tab: u32,
        pane: Option<u32>,
    },
    /// Move the active Tab one place within its Project, wrapping at either end.
    TabMove {
        direction: TabMoveDirection,
    },
    /// Read a bounded plain-text projection owned by the server grid.
    PaneRead {
        pane: PaneId,
        lines: u32,
    },
    /// Send exact bytes to one stable Pane without changing visual focus.
    PaneSend {
        pane: PaneId,
        bytes: Vec<u8>,
    },
    /// Wait until bounded recent output contains a literal string. The server
    /// evaluates this only on PTY output and the armed deadline.
    PaneWaitOutput {
        pane: PaneId,
        needle: String,
        timeout_ms: u64,
    },
    /// Wait for one smoothed agent status. This follows reconciled status
    /// transitions rather than polling the pane or process table.
    AgentWait {
        pane: PaneId,
        status: AgentStatus,
        timeout_ms: u64,
    },
    /// Request active human-attention items for this Workspace.
    WaitingList,
    /// Apply one semantic action to a waiting item.
    WaitingAct {
        id: u64,
        action: WaitingAction,
        /// Required only for [`WaitingAction::Answer`].
        text: String,
    },
    /// Version 3 orchestration completion contract shared by workflows and relays.
    OrchestrationSubmit {
        kind: OrchestrationKind,
        token: u64,
        status: SubmissionStatus,
        verdict: Option<String>,
        summary: String,
        artifacts: Vec<ArtifactClaim>,
    },
    /// Attach this binary connection directly to one Pane without rendering
    /// Workspace chrome or changing Workspace focus.
    PaneAttach {
        pane: PaneId,
        role: PaneAttachRole,
    },
    /// Request the active human-to-agent direction queue.
    InstructionList,
    /// Queue direction for the currently active invocation in one Pane.
    InstructionAdd {
        pane: PaneId,
        author: uniterm_core::InstructionAuthor,
        text: String,
    },
    /// Replace queued direction with a fresh durable identity.
    InstructionReplace {
        id: u64,
        author: uniterm_core::InstructionAuthor,
        text: String,
    },
    /// Cancel queued direction without writing to its Pane.
    InstructionCancel {
        id: u64,
    },
    /// Explicitly bypass the cooperative-ready gate for one instruction.
    InstructionSendNow {
        id: u64,
    },
    /// Run one server-owned worktree lifecycle operation.
    Worktree {
        operation: WorktreeOperation,
    },
    /// Inspect native run relationships without opening the Observatory.
    RunList {
        project: Option<ProjectId>,
        active_only: bool,
    },
    /// Inspect typed artifact ownership without attaching a TTY.
    ArtifactList {
        project: Option<ProjectId>,
        run: Option<RunId>,
        include_superseded: bool,
    },
    /// Create a worktree-backed child of one active workflow or relay.
    RunFork {
        fork: RunForkRequest,
    },
    /// Request the host-owned Workspace catalog. Disk and sibling-socket
    /// probes run on the agent runtime, never on the mio core loop.
    WorkspaceList,
    /// Refresh the executable search path from a remote bridge before its
    /// Attach frame. This is process-local machine state, not durable
    /// Workspace state, and lets a live server recover from SSH's restricted
    /// non-interactive environment without a restart.
    RemoteEnvironment {
        search_path: Vec<String>,
    },
}

/// Input authority requested by a direct Pane attachment.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PaneAttachRole {
    /// Receives snapshots and damage but can never write to the PTY.
    Observer,
    /// Receives input authority only when no controller already owns the Pane.
    Controller,
    /// Revokes the current controller and takes input authority explicitly.
    Takeover,
}

impl PaneAttachRole {
    /// Whether this role is allowed to route input to the Pane.
    pub fn can_control(self) -> bool {
        matches!(self, Self::Controller | Self::Takeover)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum OrchestrationKind {
    #[serde(rename = "workflow", alias = "Workflow")]
    Workflow,
    #[serde(rename = "relay", alias = "Relay")]
    Relay,
}

/// Provider-neutral launch vocabulary shared by interactive and automation
/// entry points. Provider ids remain opaque strings until server resolution.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrchestrationLaunch {
    pub kind: OrchestrationKind,
    /// Required for workflows and ignored for relays.
    pub template: Option<String>,
    pub goal: String,
    /// Global fallback provider. `None` selects the first installed provider.
    pub provider: Option<String>,
    /// Explicit role choices overriding `provider`.
    #[serde(default)]
    pub role_providers: Vec<RoleProviderSelection>,
    /// Optional exact Project name or canonical root in the named Workspace.
    /// `None` selects that Workspace's active Project.
    pub project: Option<String>,
}

/// Provider-neutral request to isolate a child of one active native run.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunForkRequest {
    pub parent: RunId,
    /// Display name and source for the derived worktree branch.
    pub name: String,
    /// Absolute path that Git should create.
    pub path: String,
    /// Optional commit-ish checked out into the new branch.
    pub base: Option<String>,
}

/// Complete outcome of worktree creation plus child orchestration launch.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunForkResult {
    pub parent: RunId,
    /// Present only after the child has fresh Panes, roles, and activations.
    pub child: Option<RunId>,
    pub worktree: WorktreeResult,
}

/// One provider-neutral artifact declared by a completion submission or
/// workflow template before filesystem authority validates it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactClaim {
    /// Semantic class retained after runtime validation.
    pub kind: ArtifactKind,
    /// User-declared path, resolved only by the runtime inside its Project.
    pub path: String,
}

/// Runtime-authoritative file facts returned after canonical Project-scoped
/// validation and bounded streaming SHA-256 hashing.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactObservation {
    /// Semantic class copied from the validated claim.
    pub kind: ArtifactKind,
    /// Canonical normalized Project-relative path.
    pub path: String,
    /// Lowercase SHA-256 of the bytes read by the runtime.
    pub digest: String,
    /// Number of bytes included in `digest`.
    pub size: u64,
}

/// One current Artifact path assigned to an operating-system watch.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactWatchEntry {
    /// Stable identity returned instead of copying path metadata on events.
    pub artifact: ArtifactId,
    /// Canonical Project-relative path previously accepted by the ledger.
    pub path: String,
}

/// Complete event-driven watch ownership for one Project.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactWatchProject {
    /// Project whose root bounds every entry.
    pub project: ProjectId,
    /// Authoritative root resolved on the runtime before watch registration.
    pub root: String,
    /// Current non-superseded paths, bounded by the ledger cap.
    pub artifacts: Vec<ArtifactWatchEntry>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SubmissionStatus {
    Done,
    NeedsInput,
    Failed,
}

/// Human action applied to one Workspace waiting item.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum WaitingAction {
    Focus,
    Answer,
    Dismiss,
    Stop,
    Resume,
    Rollback,
}

/// Which notification a chime announces; the two are meant to be told apart
/// by ear.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChimeKind {
    /// An agent finished and went idle.
    Done,
    /// An agent needs a human: a permission or a question.
    Attention,
}

/// The kinds of mouse event the client forwards (others are ignored).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum MouseKind {
    /// Motion with no button held (hover).
    Hover,
    /// Left-button press.
    Click,
    /// Right-button press. Uniterm normally consumes this for context menus;
    /// `pane-right-click` may route it to an opted-in child application.
    RightClick,
    /// Left-button release (apps that asked for mouse need press+release pairs).
    Release,
    /// Motion with the left button held (forwarded to apps tracking drags).
    Drag,
    /// Scroll wheel: scrollback/copy-mode on the main screen, arrow-key
    /// emulation for alt-screen apps, a forwarded report for mouse-mode apps.
    WheelUp,
    WheelDown,
}

/// Existing dropdown requested from server-rendered chrome.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChromeMenu {
    Tabs,
    Agents,
    Workspace,
    /// Context menu for empty space in the Projects rail.
    Projects,
    /// Context menu for one server-resolved Project card.
    Project(ProjectId),
}

/// A direct action from an always-visible server-rendered chrome control.
///
/// Unlike [`ChromeMenu`], these controls do not open an intermediate dropdown.
/// The attach client owns the resulting local surface, so the server sends the
/// semantic action instead of trying to synthesize client input.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChromeAction {
    NewTask,
    Tasks,
    Config,
}

/// The built-in multiplexer commands a prefix key can trigger. The full command
/// language and rebindable keys arrive in M4 (`docs/10`); these are the M3 verbs
/// so splits, focus, zoom, and windows are drivable now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Split(SplitAxis),
    Focus(FocusDir),
    ZoomToggle,
    KillPane,
    NewWindow,
    NextWindow,
    PrevWindow,
    /// Select a window by 0-based index (prefix + digit).
    SelectWindow(u8),
    /// Close the active window and every pane in it (close tab).
    KillWindow,
    /// Toggle the zoom-out overview: every window as a tile in a grid; pick
    /// one to switch to.
    Overview,
    /// Enter copy-mode on the active pane (prefix + `[`).
    CopyMode,
    /// Grow/shrink the active pane toward a direction (prefix + Shift-H/J/K/L).
    ResizePane(FocusDir),
    /// Reveal the Observatory's file manager, or hide it when already active.
    FileSidebarToggle,
    /// Toggle the Project sidebar on the left.
    SidebarToggle,
    /// Toggle the right-hand Observatory without changing its selected view.
    Observatory,
    /// Move the active Tab one place within its Project, wrapping at the edge.
    MoveTab(TabMoveDirection),
    /// Swap focus with the previously focused Pane in the active Tab.
    LastPane,
}

/// Split orientation on the wire (maps to `uniterm_core::SplitDir`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitAxis {
    /// Side by side.
    LeftRight,
    /// Stacked.
    TopBottom,
}

/// One bounded step in the active Project's Tab ordering.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum TabMoveDirection {
    Previous,
    Next,
}

/// Focus direction on the wire (maps to `uniterm_core::Direction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// One bounded step in the Workspace Project ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectMoveDirection {
    Up,
    Down,
}

/// What the server sends a client. Carries the minimal damage-diff render ops
/// produced by the renderer (pre-encoded escape sequences).
///
/// Variants are append-only because bincode encodes their ordinal on the wire.
/// Inserting or reordering one makes older peers decode a different message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Pre-encoded terminal escape sequences (changed cells + cursor position).
    RenderOps(Vec<u8>),
    /// The session was renamed: attached clients update the socket path they
    /// believe they are on (switcher "current" detection, rename prefill).
    SessionRenamed {
        name: String,
    },
    /// Autocomplete data for the New Task input (reply to
    /// [`ClientMessage::Suggest`]): distinct project names seen in this
    /// session's task history, and the agents installed on this machine.
    Suggestions {
        projects: Vec<String>,
        agents: Vec<String>,
    },
    Bell,
    /// The server acknowledges a detach; the client should exit cleanly.
    Detached,
    /// The pane's process exited; the client should exit.
    Exited,
    /// Reply to [`ClientMessage::ListInfo`]: the session's window and pane counts.
    Info {
        windows: u32,
        panes: u32,
    },
    /// Reply to [`ClientMessage::Observatory`]: the fleet of agent panes, already
    /// sorted with the ones needing a human first.
    Fleet {
        entries: Vec<FleetEntry>,
    },
    /// Live local web servers for the current Workspace's Observatory.
    DevServers {
        entries: Vec<DevServerEntry>,
    },
    /// Ask this local attach client to open a user-clicked HTTP(S) link.
    OpenUrl {
        url: String,
    },
    /// Reply to [`ClientMessage::Tasks`]: the session's tasks, ordered
    /// active-first.
    Tasks {
        items: Vec<TaskEntry>,
    },
    /// Reply to [`ClientMessage::Agents`]: every registry provider with its
    /// install, connector, and running state (the Manage Agents snapshot).
    Agents {
        items: Vec<AgentInfo>,
    },
    /// The durable hierarchy for the current Workspace.
    Workspace {
        name: String,
        active_project: ProjectId,
        projects: Vec<ProjectInfo>,
    },
    /// Result of one explicit Desktop hierarchy import.
    WorkspaceImported {
        projects_added: u32,
        tabs_added: u32,
        projects_merged: u32,
        error: Option<String>,
    },
    Settings {
        /// Boxed: the snapshot is by far the largest payload and would
        /// otherwise size every `ServerMessage` on the render path.
        settings: Box<SettingsSnapshot>,
        saved: bool,
        error: Option<String>,
    },
    AgentExplanation {
        entries: Vec<AgentDetectionInfo>,
    },
    /// Ask the attach client to composite one existing dropdown at an exact
    /// server-owned chrome anchor. The server resolves the click target so
    /// menu hit testing cannot drift from the rendered Tab or sidebar.
    OpenMenu {
        menu: ChromeMenu,
        /// 1-based anchor cell, matching terminal mouse coordinates.
        x: u16,
        /// 1-based anchor cell, matching terminal mouse coordinates.
        y: u16,
        /// Minimum dropdown width, normally the owning sidebar or Tab width.
        width: u16,
        /// Open above the anchor (Agents footer/bottom status) instead of below.
        open_up: bool,
    },
    /// Ask the attach client to open the surface selected by an always-visible
    /// chrome button, without displaying an intermediate dropdown.
    OpenChromeAction {
        action: ChromeAction,
    },
    /// Reply to [`ClientMessage::PaneList`] with live Pane locations.
    Panes {
        workspace: String,
        panes: Vec<PaneInfo>,
    },
    /// Reply to [`ClientMessage::PaneFocus`] so automation can detect stale ids.
    PaneFocused {
        pane: PaneId,
        found: bool,
    },
    /// Reply to [`ClientMessage::HierarchyFocus`] with the stable Pane that
    /// ultimately received focus, or `None` when any location was stale.
    HierarchyFocused {
        project: ProjectId,
        tab: u32,
        pane: Option<u32>,
        focused: Option<PaneId>,
    },
    /// Confirmation for a semantic Tab reorder request.
    TabMoved {
        moved: bool,
    },
    /// Set the outer terminal's title from server-owned session state.
    WindowTitle {
        title: String,
    },
    /// Bounded server-grid output for automation and diagnostics.
    PaneOutput {
        pane: PaneId,
        found: bool,
        text: String,
        truncated: bool,
    },
    /// Acknowledgement for exact Pane input.
    PaneSent {
        pane: PaneId,
        found: bool,
        /// False when the pane exists but the bytes were dropped because its
        /// pending-input queue is full. Automation must not treat `found` as
        /// delivery: a silently dropped keystroke breaks any script built on
        /// send-then-wait.
        accepted: bool,
    },
    /// Completion of an event-driven output wait.
    PaneOutputWaited {
        pane: PaneId,
        found: bool,
        matched: bool,
        timed_out: bool,
        text: String,
        truncated: bool,
    },
    /// Completion of an event-driven agent-status wait.
    AgentWaited {
        pane: PaneId,
        found: bool,
        matched: bool,
        timed_out: bool,
        status: Option<AgentStatus>,
    },
    /// Authoritative result of an agent launch request. `None` means the
    /// provider was unavailable or the PTY spawn failed.
    AgentLaunchResult {
        agent: String,
        pane: Option<PaneId>,
    },
    /// Active human-attention items, ordered by durable id.
    Waiting {
        items: Vec<WaitingEntry>,
    },
    /// Result of one waiting action followed by the fresh active projection.
    WaitingActed {
        id: u64,
        found: bool,
        accepted: bool,
        items: Vec<WaitingEntry>,
    },
    /// A direct Pane stream was established. Geometry remains server-owned;
    /// client resize messages request only a fresh projection.
    PaneAttached {
        pane: PaneId,
        role: PaneAttachRole,
        cols: u16,
        rows: u16,
    },
    /// A direct Pane stream could not be established.
    PaneAttachRejected {
        pane: PaneId,
        reason: String,
    },
    /// A takeover removed this connection's input authority. The stream
    /// remains attached as an observer.
    PaneAttachRevoked {
        pane: PaneId,
        reason: String,
    },
    /// Active human-to-agent instructions, ordered by durable creation.
    Instructions {
        items: Vec<InstructionEntry>,
    },
    /// Result of one semantic instruction mutation and the fresh projection.
    InstructionChanged {
        id: u64,
        found: bool,
        accepted: bool,
        items: Vec<InstructionEntry>,
    },
    /// Git-authoritative result of one worktree lifecycle operation.
    Worktrees(WorktreeResult),
    /// Workspace-scoped native run graph projection.
    Runs {
        /// Workspace that owns this graph projection.
        workspace: String,
        /// Retained runs after the requested filters are applied.
        runs: Vec<RunEntry>,
    },
    /// Workspace-scoped typed artifact ledger projection.
    Artifacts {
        workspace: String,
        artifacts: Vec<ArtifactEntry>,
    },
    /// Result of one worktree-backed child Run launch.
    RunForked(RunForkResult),
    /// The active Pane contains another Uniterm attach client. While enabled,
    /// one prefix targets the nested client and a doubled prefix reaches this
    /// outer client, so SSH-nested sessions retain their native shortcuts.
    NestedInput {
        enabled: bool,
    },
    /// Host-owned Workspaces available to a local or SSH attach client.
    Workspaces {
        current: String,
        entries: Vec<WorkspaceInfo>,
    },
    /// Authoritative outcome of one Project creation request. The following
    /// [`ServerMessage::Workspace`] remains the hierarchy projection.
    ProjectCreated {
        error: Option<String>,
    },
    /// An agent notification the client should make audible. The Workspace
    /// decides the sound; the client plays it where the human is, so a remote
    /// Workspace chimes locally. `pane_active` lets the client stay quiet for
    /// a completion in the Pane the human is already looking at.
    Chime {
        kind: ChimeKind,
        sound: uniterm_core::NotificationSound,
        file: String,
        pane_active: bool,
    },
}

/// One host-owned Workspace shown by the Manage Workspaces surface.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub name: String,
    pub windows: u32,
    pub panes: u32,
    pub projects: u32,
    pub running: bool,
}

/// Authority ordering for reconciled agent evidence. Higher-authority signals
/// replace lower-authority ones; permission/question grid evidence may still
/// outrank stale working evidence for safety.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum DetectionAuthority {
    Process,
    Grid,
    Log,
    Osc777,
    KernelExit,
}

/// Origin of the evidence that won agent detection reconciliation.
/// Manifest sources are ordered separately through `precedence`; cooperative
/// and kernel evidence do not come from a manifest at all.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum DetectionSource {
    None,
    Bundled,
    LastKnownGood,
    VerifiedCache,
    LocalOverride,
    Launch,
    Cooperative,
    Kernel,
}

/// Detection surfaces a provider declares. Keeping connector support distinct
/// from process-only recognition prevents a manifest from implying that
/// Uniterm can install a cooperative hook merely because it knows a binary.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum DetectionCapability {
    Process,
    Screen,
    Log,
    Connector,
}

/// Structured explanation for one reconciled detection result.
/// This is carried with the evidence across the runtime seam so the mio core
/// never reads or interprets provider manifests.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DetectionProvenance {
    pub source: DetectionSource,
    pub manifest_version: Option<String>,
    pub matched_rule: Option<String>,
    pub confidence: Option<u8>,
    pub dwell_ms: Option<u64>,
    pub precedence: u8,
    pub capabilities: Vec<DetectionCapability>,
    pub evidence_timestamp_ms: u64,
    pub invocation_pid: Option<i32>,
}

impl DetectionProvenance {
    /// Provenance for an observation that does not originate in a detection
    /// manifest, such as launch, OSC 777, or kernel process exit.
    pub fn direct(source: DetectionSource, timestamp_ms: u64, invocation_pid: Option<i32>) -> Self {
        Self {
            source,
            manifest_version: None,
            matched_rule: None,
            confidence: None,
            dwell_ms: None,
            precedence: u8::MAX,
            capabilities: Vec::new(),
            evidence_timestamp_ms: timestamp_ms,
            invocation_pid,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentDetectionInfo {
    pub pane: PaneId,
    pub project: ProjectId,
    pub tab: u32,
    pub agent: Option<String>,
    pub status: AgentStatus,
    pub authority: DetectionAuthority,
    pub evidence: String,
    pub foreground_pid: Option<i32>,
    pub provenance: DetectionProvenance,
}

/// Editable Settings fields. Optional values make the wire format forward
/// compatible and let one interaction update exactly one row.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettingsPatch {
    pub theme: Option<String>,
    pub status: Option<bool>,
    pub status_top: Option<bool>,
    pub sidebar: Option<bool>,
    pub sidebar_width: Option<u16>,
    pub file_sidebar: Option<bool>,
    pub file_sidebar_width: Option<u16>,
    pub notification_delivery: Option<String>,
    pub notify_completion: Option<bool>,
    pub notification_sound: Option<String>,
    pub notification_sound_file: Option<String>,
    pub focus_follows_mouse: Option<bool>,
    /// Hold a Pane's screen still while a text selection is in progress.
    pub freeze_on_select: Option<bool>,
    /// Copy a drag selection to the clipboard when the mouse is released.
    pub copy_on_select: Option<bool>,
    pub confirm_close: Option<bool>,
    pub confirm_tab_close: Option<bool>,
    pub scrollback_limit: Option<usize>,
    pub restore: Option<bool>,
    /// Maximum concurrently active native workflows and relays.
    pub guardrail_max_active_runs: Option<u16>,
    /// Maximum role Panes reserved by active native runs.
    pub guardrail_max_role_panes: Option<u16>,
    /// Maximum workflow or relay iterations captured at launch.
    pub guardrail_max_iterations: Option<u32>,
    /// Elapsed-time waiting boundary expressed in whole minutes.
    pub guardrail_max_elapsed_minutes: Option<u64>,
    /// Semicolon-separated exact Project names or stored roots.
    pub guardrail_allowed_projects: Option<String>,
    pub editor: Option<String>,
    /// Semicolon-separated `extension=command` overrides from Settings.
    pub editor_rules: Option<String>,
}

/// Server-authoritative Settings projection and schema choices.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettingsSnapshot {
    pub theme: String,
    pub themes: Vec<String>,
    pub status: bool,
    pub status_top: bool,
    pub sidebar: bool,
    pub sidebar_width: u16,
    pub file_sidebar: bool,
    pub file_sidebar_width: u16,
    pub notification_delivery: String,
    pub notification_deliveries: Vec<String>,
    pub notify_completion: bool,
    pub notification_sound: String,
    pub notification_sounds: Vec<String>,
    pub notification_sound_file: String,
    pub focus_follows_mouse: bool,
    /// Hold a Pane's screen still while a text selection is in progress.
    pub freeze_on_select: bool,
    /// Copy a drag selection to the clipboard when the mouse is released.
    pub copy_on_select: bool,
    pub confirm_close: bool,
    pub confirm_tab_close: bool,
    pub scrollback_limit: usize,
    pub restore: bool,
    /// Maximum concurrently active native workflows and relays.
    pub guardrail_max_active_runs: u16,
    /// Maximum role Panes reserved by active native runs.
    pub guardrail_max_role_panes: u16,
    /// Maximum workflow or relay iterations captured at launch.
    pub guardrail_max_iterations: u32,
    /// Elapsed-time waiting boundary expressed in whole minutes.
    pub guardrail_max_elapsed_minutes: u64,
    /// Semicolon-separated exact Project names or stored roots.
    pub guardrail_allowed_projects: String,
    pub editor: String,
    /// Semicolon-separated `extension=command` overrides for editing.
    pub editor_rules: String,
}

/// The reach of a bulk agent action. The session is the workspace unit today;
/// finer scopes ride the same field, so adding one is additive, not a wire
/// migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopScope {
    /// Every Project, Tab, and Pane in this Workspace.
    Workspace,
    /// Every Tab and Pane owned by one Project.
    Project(ProjectId),
    /// One Tab (0-based ordinal within the active Project).
    Tab(u32),
    /// Compatibility spelling for older clients. New clients send Workspace.
    Session,
    /// Compatibility spelling for older clients. New clients send Tab.
    Window(u32),
}

/// One Project in a Workspace projection. Tabs are counted within the
/// project, never by their mutable global storage index.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectInfo {
    pub id: ProjectId,
    pub name: String,
    pub root: String,
    pub tabs: u32,
    pub panes: u32,
    pub active: bool,
    pub attention: u32,
    /// Durable Git worktree provenance when Uniterm created this Project.
    pub worktree: Option<WorktreeRegistration>,
}

/// Durable identity of a Git worktree owned by one Uniterm Project.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeRegistration {
    /// Stable Workspace-scoped owner used for every later mutation.
    pub project: ProjectId,
    /// Display name captured from the owning Project projection.
    pub project_name: String,
    /// Canonical primary worktree reported first by Git's porcelain list.
    pub repository: String,
    /// Canonical path created for this linked worktree.
    pub path: String,
    /// Short local branch name captured after creation.
    pub branch: String,
    /// Commit checked out when Uniterm registered the worktree.
    pub created_head: String,
}

/// One human or automation operation over the worktree resource lifecycle.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum WorktreeOperation {
    /// Inspect every registered worktree in the current Workspace.
    List,
    /// Create and register one linked worktree and its Project.
    Add {
        /// Display name and source for the derived `uniterm/<name>` branch.
        name: String,
        /// Any path inside the repository; Git resolves its primary worktree.
        repository: String,
        /// Absolute path that Git should create.
        path: String,
        /// Optional commit-ish checked out into the new branch.
        base: Option<String>,
    },
    /// Revalidate and switch to one worktree Project.
    Open { project: ProjectId },
    /// Remove one worktree through Git before forgetting its Project.
    Remove {
        project: ProjectId,
        /// Permit Git to discard dirty files; the human CLI separately confirms.
        force: bool,
    },
    /// Forget only a worktree that Git proves absent or prunable.
    Cleanup { project: ProjectId },
}

/// Internal runtime form after the core has resolved Workspace-scoped ids.
#[derive(Clone, Debug)]
pub enum WorktreeRuntimeOperation {
    /// Perform the blocking Git creation for one already allocated Project id.
    Add {
        registration: WorktreeRegistration,
        base: Option<String>,
    },
    /// Compensate a Git creation that the core could not register as a live
    /// Project. This is internal and never expands the public command surface.
    RollbackAdd { registration: WorktreeRegistration },
    /// Inspect or mutate registrations already resolved by the owning core.
    Inspect {
        action: WorktreeAction,
        registrations: Vec<WorktreeRegistration>,
        force: bool,
    },
}

/// Stable operation label returned by binary and NDJSON clients.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeAction {
    /// Fresh resource inspection.
    List,
    /// Git creation followed by Project registration.
    Add,
    /// Revalidated Project selection.
    Open,
    /// Git removal followed by Project removal.
    Remove,
    /// Stale Git prune followed by Project removal.
    Cleanup,
}

/// Git-authoritative state of one registered worktree.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeState {
    /// Git reports the linked worktree and its path exists.
    Active,
    /// Git no longer reports the registered path.
    Missing,
    /// Git reports an administrative entry eligible for prune.
    Prunable,
}

/// One freshly inspected resource returned to callers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// Durable Uniterm provenance for the inspected resource.
    pub registration: WorktreeRegistration,
    /// Current Git-authoritative lifecycle state.
    pub state: WorktreeState,
    /// Current short branch, which may differ from the creation branch.
    pub current_branch: Option<String>,
    /// Current commit reported by the worktree porcelain record.
    pub head: Option<String>,
    /// Whether tracked or untracked changes make default removal unsafe.
    pub dirty: bool,
}

/// Complete outcome of one lifecycle operation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeResult {
    /// Semantic operation that produced this result.
    pub action: WorktreeAction,
    /// Whether both Git and any required Project projection change succeeded.
    pub accepted: bool,
    /// Human-readable refusal or failure without hiding returned inspection data.
    pub error: Option<String>,
    /// Fresh Git-authoritative resources relevant to the operation.
    pub items: Vec<WorktreeEntry>,
}

/// One live Pane and its stable location in the Workspace hierarchy.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneInfo {
    /// Stable Pane id accepted by [`ClientMessage::PaneFocus`].
    pub id: PaneId,
    /// Stable id of the owning Project.
    pub project: ProjectId,
    /// Human-readable owning Project name.
    pub project_name: String,
    /// 1-based Tab ordinal within the owning Project.
    pub tab: u32,
    /// Human-readable Tab name, including the generated numbered fallback.
    pub tab_name: String,
    /// 1-based Pane ordinal within the Tab's current layout traversal.
    pub pane: u32,
    /// Whether this is the Workspace's currently focused Pane.
    pub active: bool,
}

/// Lightweight durable Workspace state used after an intentional stop.
///
/// Terminal content, commands, agents, processes, and detected servers are
/// deliberately absent. Reviving this definition creates fresh shell Panes
/// in each Tab's remembered split layout at its Project root.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceDefinition {
    pub version: u32,
    pub active_project: ProjectId,
    /// Whether the Agents rail includes every Project instead of only active.
    #[serde(default)]
    pub agent_scope_workspace: bool,
    /// Whether Web Servers includes every Project instead of only active.
    #[serde(default)]
    pub server_scope_workspace: bool,
    pub projects: Vec<WorkspaceProjectDefinition>,
}

impl WorkspaceDefinition {
    pub const VERSION: u32 = 2;

    const LEGACY_SINGLE_PANE_VERSION: u32 = 1;

    pub fn is_valid(&self) -> bool {
        matches!(
            self.version,
            Self::LEGACY_SINGLE_PANE_VERSION | Self::VERSION
        ) && !self.projects.is_empty()
            && self.projects.iter().all(|project| {
                !project.tabs.is_empty() && project.tabs.iter().all(|tab| tab.layout.is_valid())
            })
    }

    pub fn tab_count(&self) -> usize {
        self.projects.iter().map(|project| project.tabs.len()).sum()
    }
}

/// State-directory child containing append-only Workspace definition logs.
pub const WORKSPACE_CATALOG_DIR: &str = "workspaces";

/// Maximum byte length of a Workspace name used in socket and state paths.
/// Keeping this short leaves room under macOS's small Unix-socket path limit.
pub const MAX_WORKSPACE_NAME_BYTES: usize = 64;

/// Machine config key used by every front end to choose the Workspace used
/// when no explicit name is supplied.
pub const DEFAULT_WORKSPACE_KEY: &str = "default-workspace";

/// Why a Workspace name cannot safely become a socket or state-file key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceNameError {
    Empty,
    TooLong,
    SurroundingWhitespace,
    UnsafeCharacter,
    TraversalComponent,
}

impl std::fmt::Display for WorkspaceNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            WorkspaceNameError::Empty => "must not be empty",
            WorkspaceNameError::TooLong => "must be at most 64 bytes",
            WorkspaceNameError::SurroundingWhitespace => "must not start or end with whitespace",
            WorkspaceNameError::UnsafeCharacter => {
                "may contain only ASCII letters, digits, '.', '-', and '_'"
            }
            WorkspaceNameError::TraversalComponent => "must not be '.' or '..'",
        };
        f.write_str(message)
    }
}

impl std::error::Error for WorkspaceNameError {}

/// Validate the one canonical Workspace identity accepted at every path boundary.
/// Restricting the alphabet prevents separators and platform-specific path tricks.
pub fn validate_workspace_name(name: &str) -> Result<(), WorkspaceNameError> {
    if name.is_empty() {
        return Err(WorkspaceNameError::Empty);
    }
    if name.len() > MAX_WORKSPACE_NAME_BYTES {
        return Err(WorkspaceNameError::TooLong);
    }
    if name != name.trim() {
        return Err(WorkspaceNameError::SurroundingWhitespace);
    }
    if matches!(name, "." | "..") {
        return Err(WorkspaceNameError::TraversalComponent);
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(WorkspaceNameError::UnsafeCharacter);
    }
    Ok(())
}

/// Read the last valid default Workspace assignment from config text.
/// Invalid hand-edited names are ignored before they can reach a path boundary.
pub fn configured_default_workspace(text: &str) -> Option<String> {
    text.lines().rev().find_map(|raw| {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (key, value) = line.split_once('=')?;
        if key.trim() != DEFAULT_WORKSPACE_KEY {
            return None;
        }
        let value = value
            .trim()
            .split_once(" #")
            .map_or(value.trim(), |(value, _)| value.trim())
            .trim_matches('"');
        validate_workspace_name(value)
            .is_ok()
            .then(|| value.to_string())
    })
}

/// Replace every default Workspace assignment with one canonical value while
/// retaining comments, unknown keys, and all unrelated settings.
pub fn merge_default_workspace(text: &str, name: &str) -> String {
    let mut output = String::new();
    let mut replaced = false;
    for raw in text.lines() {
        let is_preference = raw
            .split_once('=')
            .is_some_and(|(key, _)| key.trim() == DEFAULT_WORKSPACE_KEY);
        if is_preference {
            if !replaced {
                output.push_str(DEFAULT_WORKSPACE_KEY);
                output.push_str(" = ");
                output.push_str(name);
                output.push('\n');
                replaced = true;
            }
        } else {
            output.push_str(raw);
            output.push('\n');
        }
    }
    if !replaced {
        if !output.is_empty() && !output.ends_with("\n\n") {
            output.push('\n');
        }
        output.push_str(DEFAULT_WORKSPACE_KEY);
        output.push_str(" = ");
        output.push_str(name);
        output.push('\n');
    }
    output
}

/// Encode a Workspace name as a path-safe, reversible UTF-8 hex key.
pub fn workspace_catalog_key(name: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut key = String::with_capacity(name.len() * 2);
    for byte in name.as_bytes() {
        key.push(HEX[(byte >> 4) as usize] as char);
        key.push(HEX[(byte & 0x0f) as usize] as char);
    }
    key
}

/// Decode a path-safe Workspace catalog key back to its original name.
pub fn workspace_name_from_catalog_key(key: &str) -> Option<String> {
    if key.is_empty() || !key.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(key.len() / 2);
    for pair in key.as_bytes().chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        bytes.push(((hi << 4) | lo) as u8);
    }
    String::from_utf8(bytes).ok()
}

/// One remembered Project, including its root and ordered Tabs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceProjectDefinition {
    pub id: ProjectId,
    pub name: String,
    pub root: String,
    /// Worktree provenance survives a clean stop without retaining Pane state.
    #[serde(default)]
    pub worktree: Option<WorktreeRegistration>,
    pub active_tab: usize,
    pub tabs: Vec<WorkspaceTabDefinition>,
}

/// One remembered Tab. A missing name retains the ordinary numbered label.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceTabDefinition {
    pub name: Option<String>,
    /// Anonymous split geometry. Each leaf becomes one fresh shell Pane.
    #[serde(default)]
    pub layout: WorkspaceLayoutDefinition,
}

/// Pane-free split geometry retained across an intentional Workspace stop.
///
/// Leaves carry no process, terminal content, working directory, or Pane id.
/// Split ratios use fixed-point ten-thousandths so catalog JSON remains stable
/// and exactly comparable without serializing floating-point values.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum WorkspaceLayoutDefinition {
    /// One fresh shell Pane.
    #[default]
    Pane,
    /// Two child layouts separated in `dir`.
    Split {
        dir: SplitDir,
        /// Fraction assigned to `first`, in ten-thousandths.
        first_ratio: u16,
        first: Box<WorkspaceLayoutDefinition>,
        second: Box<WorkspaceLayoutDefinition>,
    },
}

impl WorkspaceLayoutDefinition {
    /// Maximum fresh Panes one remembered Tab may create during restore.
    pub const MAX_PANES: usize = 64;

    /// Count anonymous Pane leaves, rejecting malformed or excessive trees.
    pub fn pane_count(&self) -> Option<usize> {
        match self {
            WorkspaceLayoutDefinition::Pane => Some(1),
            WorkspaceLayoutDefinition::Split {
                first_ratio,
                first,
                second,
                ..
            } => {
                if !(1..10_000).contains(first_ratio) {
                    return None;
                }
                let panes = first.pane_count()?.checked_add(second.pane_count()?)?;
                (panes <= Self::MAX_PANES).then_some(panes)
            }
        }
    }

    /// Whether the tree can safely be recreated as fresh Panes.
    pub fn is_valid(&self) -> bool {
        self.pane_count().is_some()
    }
}

/// A Desktop Workspace reduced to the durable hierarchy Uniterm CLI can
/// faithfully recreate. Runtime processes, Pane contents, and layouts are
/// deliberately absent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportedWorkspace {
    pub source_id: String,
    pub projects: Vec<ImportedProject>,
}

/// One imported Project and its fresh Tabs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportedProject {
    pub source_id: String,
    pub name: String,
    pub root: String,
    pub tabs: Vec<ImportedTab>,
}

/// A Tab needs only its optional user-given name. The target creates one fresh
/// shell Pane at the owning Project root.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportedTab {
    pub name: Option<String>,
}

/// Whether an import replaces only a freshly-created bootstrap hierarchy or
/// adds missing Projects and Tabs without modifying existing ones.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum WorkspaceImportMode {
    Fresh,
    Merge,
}

/// Where the Manage Agents modal starts an agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaunchTarget {
    /// Type the launch command into the focused pane's shell.
    CurrentPane,
    /// Split a new pane in the active window (the New Task pattern).
    NewPane,
    /// Open a new window (tab) running the agent.
    NewWindow,
}

/// Connector (notify-hook) state for one provider, shown in Manage Agents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectorStatus {
    /// The provider's notify hook is installed; OSC 777 status flows live.
    Installed,
    /// The provider supports a notify hook but it is not installed.
    NotInstalled,
    /// This provider has no notify hook to install (status via fallbacks).
    Unsupported,
}

/// One provider row in the Manage Agents snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Registry id (e.g. "claude").
    pub id: String,
    /// Display name (e.g. "Claude Code").
    pub name: String,
    /// The CLI command that launches it.
    pub command: String,
    /// Whether the CLI is on `$PATH` right now.
    pub installed: bool,
    pub connector: ConnectorStatus,
    /// How many panes in this session are running this agent.
    pub running: u32,
}

/// One task in the task-management snapshot.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskEntry {
    pub id: u64,
    pub title: String,
    pub status: uniterm_core::TaskStatus,
    /// Free-form notes shown in the task manager's detail pane.
    pub notes: String,
}

/// One agent pane in the Observatory fleet snapshot.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetEntry {
    /// Agent id (e.g. "claude").
    pub agent: String,
    pub status: AgentStatus,
    pub pane_id: PaneId,
    pub project: ProjectId,
    pub project_name: String,
    /// 1-based Tab ordinal within the Project.
    pub tab: u32,
    pub tab_name: String,
    /// 1-based window and pane ordinals, for locating the pane.
    pub window: u32,
    pub pane: u32,
    pub authority: DetectionAuthority,
    pub evidence: String,
    /// Active native run and role for this Pane, when it owns the live turn.
    pub run: Option<RunId>,
    pub role: Option<RoleId>,
    pub role_name: Option<String>,
}

/// One role in a run-graph inspection response.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunRoleEntry {
    /// Stable role identity.
    pub id: RoleId,
    /// Provider-neutral role name.
    pub name: String,
    /// Stable Pane reserved for this role.
    pub pane: PaneId,
    /// Provider registry identity selected for this role.
    pub provider: String,
    /// Latest public activation, whether live or closed.
    pub activation: Option<uniterm_core::RunActivation>,
}

/// One native run with direct parent, child, Pane, and role relationships.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunEntry {
    /// Stable run identity.
    pub id: RunId,
    /// Delegating run, or `None` for a root.
    pub parent: Option<RunId>,
    /// Retained children in creation order.
    pub children: Vec<RunId>,
    /// Project that owns the run's effects.
    pub project: ProjectId,
    /// Provider-neutral orchestration shape.
    pub kind: uniterm_core::RunKind,
    /// Durable Task associated with this run.
    pub task_id: u64,
    /// Bounded human-readable task title.
    pub title: String,
    /// Current lifecycle state.
    pub status: uniterm_core::RunStatus,
    /// Terminal summary, when the run has closed.
    pub outcome: Option<String>,
    /// Stable Panes reserved by the run.
    pub panes: Vec<PaneId>,
    /// Roles in orchestration order.
    pub roles: Vec<RunRoleEntry>,
}

/// Wire projection of one retained typed artifact record.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactEntry {
    /// Stable Artifact identity.
    pub id: ArtifactId,
    /// Canonical Project owner.
    pub project: ProjectId,
    /// Run that produced the observation.
    pub producer_run: RunId,
    /// Role that produced the observation.
    pub producer_role: RoleId,
    /// Provider-neutral semantic class.
    pub kind: ArtifactKind,
    /// Canonical normalized Project-relative path.
    pub path: String,
    /// Lowercase SHA-256 digest of the last available observation.
    pub digest: String,
    /// Bytes represented by `digest`.
    pub size: u64,
    /// Current availability or replacement state.
    pub status: uniterm_core::ArtifactStatus,
    /// Prior identity at the same path, when retained.
    pub supersedes: Option<ArtifactId>,
}

/// Wire projection of one active human-attention item.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WaitingEntry {
    pub id: u64,
    pub pane: PaneId,
    pub kind: uniterm_core::WaitingKind,
    pub summary: String,
    pub agent: Option<String>,
    pub project: ProjectId,
    pub project_name: String,
    pub tab: u32,
}

/// Wire projection of one queued human instruction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstructionEntry {
    pub id: u64,
    pub pane: PaneId,
    pub invocation: i32,
    pub author: uniterm_core::InstructionAuthor,
    pub created_sequence: u64,
    pub policy: uniterm_core::InstructionPolicy,
    pub state: uniterm_core::InstructionState,
    pub text: String,
    pub agent: Option<String>,
    pub project: ProjectId,
    pub project_name: String,
    pub tab: u32,
}

/// A normalized local server match produced by the runtime-side detector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectedDevServer {
    pub label: String,
    pub url: String,
    pub port: u16,
}

/// One live local web server in the Workspace-scoped Observatory projection.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DevServerEntry {
    pub label: String,
    pub url: String,
    pub port: u16,
    pub pane_id: PaneId,
    pub project: ProjectId,
    pub project_name: String,
    pub project_root: String,
    pub tab: u32,
    pub tab_name: String,
    pub pane: u32,
}

// ---------------------------------------------------------------------------
// Frame codec: length-prefixed messages over a byte stream (the Unix socket).
// ---------------------------------------------------------------------------

/// Compatibility version for the client/server wire vocabulary.
///
/// Local clients normally come from the same installed binary. SSH bridges
/// check this explicitly before exposing a remote socket so mismatched bincode
/// enums fail with a useful error instead of corrupting the terminal stream.
pub const WIRE_PROTOCOL_VERSION: u32 = 18;

/// Cap for trusted server replies, including full terminal repaints.
pub const MAX_SERVER_FRAME: u32 = 8 * 1024 * 1024;

/// Tighter cap for untrusted client control frames received by the server.
/// This still admits the tested 1 MiB paste path while bounding imports and abuse.
pub const MAX_CLIENT_FRAME: u32 = 2 * 1024 * 1024;

/// Encode a message as a length-prefixed frame: `[len: u32 big-endian][payload]`.
pub fn encode_frame<M: Serialize>(msg: &M) -> Vec<u8> {
    let payload = bincode::serialize(msg).expect("serialize message");
    let len = payload.len() as u32;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Accumulates bytes from chunked socket reads and yields whole messages.
///
/// mio hands us arbitrary-sized reads, so a message may arrive split across
/// several reads or several messages in one read. This buffers until a full
/// frame is present, then decodes it.
pub struct FrameDecoder {
    buf: Vec<u8>,
    offset: usize,
    max_frame: u32,
    failed: Option<FrameError>,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self {
            buf: Vec::new(),
            offset: 0,
            max_frame: MAX_SERVER_FRAME,
            failed: None,
        }
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        FrameDecoder::default()
    }

    /// Construct a decoder with a direction-specific payload limit.
    pub fn with_max_frame(max_frame: u32) -> Self {
        Self {
            max_frame: max_frame.max(1),
            ..Self::default()
        }
    }

    /// Append freshly read bytes without ever growing beyond the configured cap.
    /// A limit violation is remembered and returned by the next [`Self::decode`].
    pub fn push(&mut self, data: &[u8]) {
        if self.failed.is_some() {
            return;
        }
        if self.offset != 0 {
            self.buf.drain(..self.offset);
            self.offset = 0;
        }
        let max_buffer = self.max_frame as usize + 4;
        let Some(new_len) = self.buf.len().checked_add(data.len()) else {
            self.failed = Some(FrameError::BufferOverflow(usize::MAX));
            return;
        };
        if new_len > max_buffer {
            self.failed = Some(FrameError::BufferOverflow(new_len));
            return;
        }
        self.buf.extend_from_slice(data);
        if self.buf.len() >= 4 {
            let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
            if len > self.max_frame {
                self.failed = Some(FrameError::Oversized(len));
            }
        }
    }

    /// Decode the next complete message, if one is fully buffered.
    /// Returns `Err` only on a corrupt frame (oversized length or bad payload).
    pub fn decode<M: DeserializeOwned>(&mut self) -> Result<Option<M>, FrameError> {
        if let Some(error) = self.failed.take() {
            self.buf.clear();
            self.offset = 0;
            return Err(error);
        }
        let pending = &self.buf[self.offset..];
        if pending.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([pending[0], pending[1], pending[2], pending[3]]);
        if len > self.max_frame {
            return Err(FrameError::Oversized(len));
        }
        let total = 4 + len as usize;
        if pending.len() < total {
            return Ok(None); // wait for more bytes
        }
        let msg = bincode::deserialize(&pending[4..total]).map_err(|_| FrameError::Corrupt)?;
        self.offset += total;
        if self.offset == self.buf.len() {
            self.buf.clear();
            self.offset = 0;
        }
        Ok(Some(msg))
    }
}

/// A frame-decoding error. Both variants are fatal for the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    Oversized(u32),
    BufferOverflow(usize),
    Corrupt,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_variant<T: Serialize>(value: &T) -> u32 {
        let bytes = bincode::serialize(value).unwrap();
        u32::from_le_bytes(bytes[..4].try_into().unwrap())
    }

    #[test]
    fn frame_round_trips() {
        let msg = ClientMessage::Input(b"hello".to_vec());
        let bytes = encode_frame(&msg);
        let mut dec = FrameDecoder::new();
        dec.push(&bytes);
        let out: ClientMessage = dec.decode().unwrap().unwrap();
        assert!(matches!(out, ClientMessage::Input(v) if v == b"hello"));
    }

    #[test]
    fn guardrail_settings_round_trip_on_current_wire_protocol() {
        assert_eq!(WIRE_PROTOCOL_VERSION, 18);
        let bytes = encode_frame(&ClientMessage::SettingsApply(SettingsPatch {
            guardrail_max_active_runs: Some(4),
            guardrail_max_role_panes: Some(12),
            guardrail_max_iterations: Some(5),
            guardrail_max_elapsed_minutes: Some(90),
            guardrail_allowed_projects: Some("api; /work/web".into()),
            ..SettingsPatch::default()
        }));
        let mut decoder = FrameDecoder::new();
        decoder.push(&bytes);
        assert!(matches!(
            decoder.decode::<ClientMessage>().unwrap().unwrap(),
            ClientMessage::SettingsApply(SettingsPatch {
                guardrail_max_active_runs: Some(4),
                guardrail_max_role_panes: Some(12),
                guardrail_max_iterations: Some(5),
                guardrail_max_elapsed_minutes: Some(90),
                guardrail_allowed_projects: Some(ref selectors),
                ..
            }) if selectors == "api; /work/web"
        ));
    }

    #[test]
    fn direct_pane_attach_roles_round_trip_on_the_binary_stream() {
        let bytes = encode_frame(&ClientMessage::PaneAttach {
            pane: PaneId(7),
            role: PaneAttachRole::Takeover,
        });
        let mut decoder = FrameDecoder::new();
        decoder.push(&bytes);
        assert!(matches!(
            decoder.decode::<ClientMessage>().unwrap().unwrap(),
            ClientMessage::PaneAttach {
                pane: PaneId(7),
                role: PaneAttachRole::Takeover,
            }
        ));
    }

    #[test]
    fn remote_workspace_catalog_and_project_result_round_trip() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&encode_frame(&ClientMessage::WorkspaceList));
        assert!(matches!(
            decoder.decode::<ClientMessage>().unwrap(),
            Some(ClientMessage::WorkspaceList)
        ));

        decoder.push(&encode_frame(&ServerMessage::Workspaces {
            current: "Work".into(),
            entries: vec![WorkspaceInfo {
                name: "Work".into(),
                windows: 3,
                panes: 5,
                projects: 2,
                running: true,
            }],
        }));
        assert!(matches!(
            decoder.decode::<ServerMessage>().unwrap(),
            Some(ServerMessage::Workspaces { current, entries })
                if current == "Work" && entries[0].panes == 5
        ));

        decoder.push(&encode_frame(&ServerMessage::ProjectCreated {
            error: Some("missing remote folder".into()),
        }));
        assert!(matches!(
            decoder.decode::<ServerMessage>().unwrap(),
            Some(ServerMessage::ProjectCreated { error: Some(error) })
                if error == "missing remote folder"
        ));
    }

    #[test]
    fn run_inspection_round_trips_on_binary_and_handwritten_json() {
        let request = ClientMessage::RunList {
            project: Some(ProjectId(7)),
            active_only: true,
        };
        let bytes = encode_frame(&request);
        let mut decoder = FrameDecoder::new();
        decoder.push(&bytes);
        assert!(matches!(
            decoder.decode::<ClientMessage>().unwrap(),
            Some(ClientMessage::RunList {
                project: Some(ProjectId(7)),
                active_only: true,
            })
        ));

        let control = ControlRequest {
            version: CONTROL_API_VERSION,
            id: 3,
            workspace: "work".into(),
            command: ControlCommand::RunList {
                project: Some(ProjectId(7)),
                active_only: true,
            },
        };
        let json = serde_json::to_value(control).unwrap();
        assert_eq!(json["method"], "run_list");
        assert_eq!(json["params"]["project"], 7);
        assert_eq!(json["params"]["active_only"], true);
    }

    #[test]
    fn artifact_inspection_round_trips_on_binary_and_handwritten_json() {
        let request = ClientMessage::ArtifactList {
            project: Some(ProjectId(7)),
            run: Some(RunId(8)),
            include_superseded: true,
        };
        let bytes = encode_frame(&request);
        let mut decoder = FrameDecoder::new();
        decoder.push(&bytes);
        assert!(matches!(
            decoder.decode::<ClientMessage>().unwrap(),
            Some(ClientMessage::ArtifactList {
                project: Some(ProjectId(7)),
                run: Some(RunId(8)),
                include_superseded: true,
            })
        ));

        let control: ControlRequest = serde_json::from_str(
            r#"{"version":1,"id":5,"workspace":"work","method":"artifact_list","params":{"project":7,"run":8,"include_superseded":false}}"#,
        )
        .unwrap();
        assert_eq!(
            control.command,
            ControlCommand::ArtifactList {
                project: Some(ProjectId(7)),
                run: Some(RunId(8)),
                include_superseded: false,
            }
        );
    }

    #[test]
    fn child_run_fork_round_trips_on_binary_and_control_protocols() {
        let fork = RunForkRequest {
            parent: RunId(8),
            name: "alternative".into(),
            path: "/work/alternative".into(),
            base: Some("main".into()),
        };
        let bytes = encode_frame(&ClientMessage::RunFork { fork: fork.clone() });
        let mut decoder = FrameDecoder::new();
        decoder.push(&bytes);
        assert!(matches!(
            decoder.decode::<ClientMessage>().unwrap(),
            Some(ClientMessage::RunFork { fork: decoded }) if decoded == fork
        ));

        let control: ControlRequest = serde_json::from_str(
            r#"{"version":1,"id":9,"workspace":"work","method":"run_fork","params":{"fork":{"parent":8,"name":"alternative","path":"/work/alternative","base":"main"}}}"#,
        )
        .unwrap();
        assert_eq!(control.command, ControlCommand::RunFork { fork });
    }

    #[test]
    fn broader_control_resources_keep_human_writable_json_shapes() {
        let task: ControlRequest = serde_json::from_str(
            r#"{"version":1,"id":10,"workspace":"work","method":"task_set_status","params":{"id":4,"status":"Doing"}}"#,
        )
        .unwrap();
        assert_eq!(
            task.command,
            ControlCommand::TaskSetStatus {
                id: 4,
                status: uniterm_core::TaskStatus::Doing,
            }
        );

        let tab: ControlRequest = serde_json::from_str(
            r#"{"version":1,"id":11,"workspace":"work","method":"tab_move","params":{"project":2,"tab":3,"direction":"Next"}}"#,
        )
        .unwrap();
        assert_eq!(
            tab.command,
            ControlCommand::TabMove {
                project: ProjectId(2),
                tab: 3,
                direction: TabMoveDirection::Next,
            }
        );
    }

    #[test]
    fn orchestration_launch_has_one_human_writable_control_shape() {
        let request = ControlRequest {
            version: CONTROL_API_VERSION,
            id: 4,
            workspace: "work".into(),
            command: ControlCommand::OrchestrationStart {
                launch: OrchestrationLaunch {
                    kind: OrchestrationKind::Workflow,
                    template: Some("pair".into()),
                    goal: "ship it".into(),
                    provider: Some("claude".into()),
                    role_providers: vec![RoleProviderSelection {
                        role: "verifier".into(),
                        provider: "codex".into(),
                    }],
                    project: None,
                },
            },
        };
        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["method"], "orchestration_start");
        assert_eq!(json["params"]["launch"]["kind"], "workflow");
        assert_eq!(json["params"]["launch"]["provider"], "claude");
        assert_eq!(
            json["params"]["launch"]["role_providers"][0]["role"],
            "verifier"
        );
        assert_eq!(
            json["params"]["launch"]["role_providers"][0]["provider"],
            "codex"
        );
        assert_eq!(
            serde_json::from_str::<OrchestrationKind>("\"Workflow\"").unwrap(),
            OrchestrationKind::Workflow
        );
    }

    #[test]
    fn established_wire_variants_keep_their_discriminants() {
        assert_eq!(
            wire_variant(&ClientMessage::Mouse {
                x: 1,
                y: 1,
                kind: MouseKind::Click,
            }),
            39
        );
        assert_eq!(wire_variant(&ClientMessage::PaneList), 40);
        assert_eq!(
            wire_variant(&ClientMessage::PaneFocus { pane: PaneId(1) }),
            41
        );
        assert_eq!(
            wire_variant(&ClientMessage::HierarchyFocus {
                project: ProjectId(1),
                tab: 1,
                pane: Some(1),
            }),
            42
        );

        assert_eq!(
            wire_variant(&ServerMessage::OpenMenu {
                menu: ChromeMenu::Tabs,
                x: 1,
                y: 1,
                width: 1,
                open_up: false,
            }),
            16
        );
        assert_eq!(
            wire_variant(&ServerMessage::OpenChromeAction {
                action: ChromeAction::NewTask,
            }),
            17
        );
        assert_eq!(
            wire_variant(&ServerMessage::Panes {
                workspace: String::new(),
                panes: Vec::new(),
            }),
            18
        );
        assert_eq!(
            wire_variant(&ServerMessage::PaneFocused {
                pane: PaneId(1),
                found: true,
            }),
            19
        );
        assert_eq!(
            wire_variant(&ServerMessage::HierarchyFocused {
                project: ProjectId(1),
                tab: 1,
                pane: Some(1),
                focused: Some(PaneId(1)),
            }),
            20
        );
    }

    #[test]
    fn handles_split_reads() {
        // A frame arriving in two chunks must still decode.
        let bytes = encode_frame(&ClientMessage::Resize { cols: 80, rows: 24 });
        let (a, b) = bytes.split_at(3);
        let mut dec = FrameDecoder::new();
        dec.push(a);
        assert!(matches!(dec.decode::<ClientMessage>(), Ok(None)));
        dec.push(b);
        assert!(matches!(
            dec.decode::<ClientMessage>().unwrap().unwrap(),
            ClientMessage::Resize { cols: 80, rows: 24 }
        ));
    }

    #[test]
    fn handles_multiple_frames_in_one_push() {
        let mut buf = encode_frame(&ClientMessage::Detach);
        buf.extend(encode_frame(&ClientMessage::Input(b"x".to_vec())));
        let mut dec = FrameDecoder::new();
        dec.push(&buf);
        assert!(matches!(
            dec.decode::<ClientMessage>().unwrap().unwrap(),
            ClientMessage::Detach
        ));
        assert!(matches!(
            dec.decode::<ClientMessage>().unwrap().unwrap(),
            ClientMessage::Input(_)
        ));
        assert!(matches!(dec.decode::<ClientMessage>(), Ok(None)));
    }

    #[test]
    fn rejects_oversized_frame() {
        let mut dec = FrameDecoder::new();
        dec.push(&u32::to_be_bytes(MAX_SERVER_FRAME + 1));
        assert!(matches!(
            dec.decode::<ClientMessage>(),
            Err(FrameError::Oversized(_))
        ));
    }

    #[test]
    fn rejects_buffer_growth_before_appending_past_the_limit() {
        let mut dec = FrameDecoder::with_max_frame(8);
        dec.push(&8u32.to_be_bytes());
        dec.push(&[0; 9]);
        assert!(dec.buf.len() <= 4);
        assert!(matches!(
            dec.decode::<ClientMessage>(),
            Err(FrameError::BufferOverflow(13))
        ));
    }

    #[test]
    fn workspace_names_are_path_safe_and_bounded() {
        for valid in ["default", "work.api-2", "_scratch"] {
            assert_eq!(validate_workspace_name(valid), Ok(()));
        }
        for invalid in ["", ".", "..", "../work", "a/b", "two words", " trailing"] {
            assert!(validate_workspace_name(invalid).is_err(), "{invalid}");
        }
        assert_eq!(
            validate_workspace_name(&"x".repeat(MAX_WORKSPACE_NAME_BYTES + 1)),
            Err(WorkspaceNameError::TooLong)
        );
    }

    #[test]
    fn agent_evidence_authority_matches_reconciliation_order() {
        assert!(DetectionAuthority::Grid > DetectionAuthority::Process);
        assert!(DetectionAuthority::Log > DetectionAuthority::Grid);
        assert!(DetectionAuthority::Osc777 > DetectionAuthority::Log);
        assert!(DetectionAuthority::KernelExit > DetectionAuthority::Osc777);
    }

    #[test]
    fn workspace_catalog_keys_are_path_safe_and_reversible() {
        for name in ["default", "Client Work", "../../outside", "café"] {
            let key = workspace_catalog_key(name);
            assert!(key.chars().all(|character| character.is_ascii_hexdigit()));
            assert_eq!(workspace_name_from_catalog_key(&key).as_deref(), Some(name));
        }
        assert_eq!(workspace_name_from_catalog_key("../bad"), None);
    }

    #[test]
    fn workspace_layout_rejects_invalid_ratios_and_excessive_panes() {
        let invalid_ratio = WorkspaceLayoutDefinition::Split {
            dir: SplitDir::Horizontal,
            first_ratio: 0,
            first: Box::new(WorkspaceLayoutDefinition::Pane),
            second: Box::new(WorkspaceLayoutDefinition::Pane),
        };
        assert!(!invalid_ratio.is_valid());

        let mut excessive = WorkspaceLayoutDefinition::Pane;
        for _ in 1..=WorkspaceLayoutDefinition::MAX_PANES {
            excessive = WorkspaceLayoutDefinition::Split {
                dir: SplitDir::Vertical,
                first_ratio: 5_000,
                first: Box::new(WorkspaceLayoutDefinition::Pane),
                second: Box::new(excessive),
            };
        }
        assert!(!excessive.is_valid());
    }
}
