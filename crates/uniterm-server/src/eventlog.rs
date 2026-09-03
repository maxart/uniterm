//! The append-only event log (P2-3): the ground-truth record of structural and
//! agent-lifecycle events. Every durable view (the Observatory, the timeline,
//! recovery) is a projection of it - add new state to the log first, then
//! project it, never the reverse (`docs/05`, `docs/07`).
//!
//! Records are JSON lines under the XDG state dir, appended as they happen
//! (event-driven, never polled). Runtime projections retain their own bounded
//! state and recovery streams the log instead of collecting lifetime history.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uniterm_core::{AgentStatus, LayoutNode, PaneId, ProjectId};

/// Current durable event-envelope schema.
pub const EVENT_VERSION: u32 = 1;

/// One startup repair of a suffix that could not have been durably ordered.
pub struct RepairReport {
    /// Private copy of the complete pre-repair stream.
    pub backup: PathBuf,
    /// Number of suffix bytes removed from the active stream.
    pub discarded_bytes: usize,
}

/// Sequence and ownership metadata wrapped around every new event record.
///
/// The sequence is Workspace-local and monotonic. Snapshots retain the last
/// applied value so recovery can stream only the suffix without relying on
/// file offsets or loading lifetime history into memory.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EventEnvelope {
    pub version: u32,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub workspace: String,
    pub event: LogEvent,
}

/// Structural state stored without terminal cells.
///
/// Snapshots remain the bounded checkpoint for grid and scrollback content.
/// This projection makes every structural mutation after that checkpoint
/// replayable and can recreate fresh panes even when the snapshot is absent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StructuralProjection {
    pub active_window: usize,
    pub next_pane_id: u64,
    pub active_project: ProjectId,
    pub next_project_id: u64,
    pub projects: Vec<StructuralProject>,
    pub windows: Vec<StructuralWindow>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StructuralProject {
    pub id: ProjectId,
    pub name: String,
    pub root: String,
    pub active_pane: Option<PaneId>,
    pub metadata: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StructuralWindow {
    pub project: ProjectId,
    pub layout: LayoutNode,
    pub active: PaneId,
    pub zoomed: Option<PaneId>,
    pub name: Option<String>,
    pub panes: Vec<StructuralPane>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StructuralPane {
    pub id: PaneId,
    pub cwd: Option<String>,
    pub metadata: Vec<(String, String)>,
    pub launch_args: Vec<String>,
    pub agent_launch: Option<StructuralAgentLaunch>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StructuralAgentLaunch {
    pub provider: String,
    pub session_id: Option<String>,
    pub resume_command: Vec<String>,
}

/// Restartable orchestration state. Pane processes are restored separately
/// from the structural checkpoint; this record preserves the pure decision
/// state and the exact outstanding activation token.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DurableRoleProvider {
    pub provider: String,
    pub command: String,
}

/// Compact extension state for native run policy.
///
/// This is boxed in [`DurableOrchestration`] so adding bounded policy facts
/// does not inflate every event-log enum value on the hot server stack.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DurableGuardrail {
    /// Unix epoch milliseconds captured before the run's first Pane spawn.
    /// Zero migrates events written before native elapsed-time guards.
    #[serde(default)]
    pub started_at_ms: u64,
    /// Launch-time limits retained so config reload cannot rewrite a live run.
    #[serde(default)]
    pub limits: uniterm_core::GuardLimits,
    /// Whether the elapsed boundary already entered the waiting queue and was
    /// therefore explicitly visible to a human.
    #[serde(default)]
    pub elapsed_triggered: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DurableOrchestration {
    pub kind: uniterm_proto::OrchestrationKind,
    pub task_id: u64,
    pub template: Option<String>,
    pub goal: String,
    /// Per-role provider ownership aligned with `state.roles` and
    /// `role_panes`. Empty only when replaying a pre-selection event record.
    #[serde(default)]
    pub role_providers: Vec<DurableRoleProvider>,
    /// Legacy scalar retained only to migrate orchestration events written
    /// before per-role provider selection.
    #[serde(default)]
    pub agent_id: String,
    /// Legacy scalar retained only to migrate orchestration events written
    /// before per-role provider selection.
    #[serde(default)]
    pub agent_cmd: String,
    pub role_panes: Vec<PaneId>,
    pub started: Vec<bool>,
    pub state: uniterm_core::orchestrate::State,
    pub checkpoints: Vec<(u64, String)>,
    /// Launch-time policy and elapsed recovery facts.
    #[serde(default)]
    pub guardrail: Box<DurableGuardrail>,
}

impl StructuralProjection {
    /// Strip terminal content from one snapshot before writing the event log.
    pub fn from_snapshot(snapshot: &crate::persist::Snapshot) -> Self {
        Self {
            active_window: snapshot.active_window,
            next_pane_id: snapshot.next_pane_id,
            active_project: snapshot.active_project,
            next_project_id: snapshot.next_project_id,
            projects: snapshot
                .projects
                .iter()
                .map(|project| StructuralProject {
                    id: project.id,
                    name: project.name.clone(),
                    root: project.root.clone(),
                    active_pane: project.active_pane,
                    metadata: project.metadata.clone(),
                })
                .collect(),
            windows: snapshot
                .windows
                .iter()
                .map(|window| StructuralWindow {
                    project: window.project,
                    layout: window.layout.clone(),
                    active: window.active,
                    zoomed: window.zoomed,
                    name: window.name.clone(),
                    panes: window
                        .panes
                        .iter()
                        .map(|pane| StructuralPane {
                            id: pane.id,
                            cwd: pane.cwd.clone(),
                            metadata: pane.metadata.clone(),
                            launch_args: pane.launch_args.clone(),
                            agent_launch: pane.agent_launch.as_ref().map(|launch| {
                                StructuralAgentLaunch {
                                    provider: launch.provider.clone(),
                                    session_id: launch.session_id.clone(),
                                    resume_command: launch.resume_command.clone(),
                                }
                            }),
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// Apply structural truth while retaining checkpointed terminal content
    /// for Pane ids that still exist.
    pub fn into_snapshot(
        self,
        previous: Option<crate::persist::Snapshot>,
        event_sequence: u64,
    ) -> crate::persist::Snapshot {
        let mut content = std::collections::HashMap::new();
        let mut run_graph = uniterm_core::RunGraph::new();
        let mut run_graph_sequence = 0;
        if let Some(previous) = previous {
            run_graph = previous.run_graph;
            run_graph_sequence = previous.run_graph_sequence;
            for window in previous.windows {
                for pane in window.panes {
                    content.insert(pane.id, pane.content);
                }
            }
        }
        let mut snapshot = crate::persist::Snapshot::new_with_sequence(
            self.active_window,
            self.next_pane_id,
            self.active_project,
            self.next_project_id,
            self.projects
                .into_iter()
                .map(|project| crate::persist::ProjectSnap {
                    id: project.id,
                    name: project.name,
                    root: project.root,
                    active_pane: project.active_pane,
                    metadata: project.metadata,
                })
                .collect(),
            self.windows
                .into_iter()
                .map(|window| crate::persist::WinSnap {
                    project: window.project,
                    layout: window.layout,
                    active: window.active,
                    zoomed: window.zoomed,
                    name: window.name,
                    panes: window
                        .panes
                        .into_iter()
                        .map(|pane| crate::persist::PaneSnap {
                            id: pane.id,
                            cwd: pane.cwd,
                            content: content.remove(&pane.id).unwrap_or_default(),
                            metadata: pane.metadata,
                            launch_args: pane.launch_args,
                            agent_launch: pane.agent_launch.map(|launch| {
                                crate::persist::AgentLaunchSnap {
                                    provider: launch.provider,
                                    session_id: launch.session_id,
                                    resume_command: launch.resume_command,
                                }
                            }),
                        })
                        .collect(),
                })
                .collect(),
            event_sequence,
        );
        snapshot.run_graph = run_graph;
        snapshot.run_graph_sequence = run_graph_sequence;
        snapshot
    }
}

/// One logged event.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum LogEvent {
    /// Complete structural truth at one durable mutation boundary, without
    /// terminal cells. This is deliberately compact enough to append on
    /// structural changes and makes the snapshot suffix replayable.
    WorkspaceProjected {
        state: StructuralProjection,
    },
    /// Authoritative ownership transition for a renamed Workspace stream.
    /// The envelope carrying this event still belongs to `old`; subsequent
    /// envelopes belong to `new` without rewriting lifetime history.
    WorkspaceRenamed {
        old: String,
        new: String,
    },
    /// A Project was added under this Workspace. The event is written before
    /// the in-memory projection is changed.
    ProjectCreated {
        project: u64,
        name: String,
        root: String,
    },
    ProjectRenamed {
        project: u64,
        name: String,
    },
    /// The complete Project id order after a user-initiated move.
    ProjectReordered {
        projects: Vec<u64>,
    },
    ProjectRemoved {
        project: u64,
    },
    ProjectSelected {
        project: u64,
    },
    ProjectMetadataSet {
        project: u64,
        key: String,
        value: String,
    },
    /// Durable intent written before Git is allowed to create the worktree.
    WorktreeCreateRequested {
        project: u64,
        name: String,
        repository: String,
        branch: String,
        path: String,
        base: Option<String>,
    },
    /// Git completed a worktree creation attempt before the Project projection
    /// was changed. Failed attempts remain explainable without inventing state.
    WorktreeCreateResult {
        project: u64,
        repository: String,
        branch: String,
        path: String,
        head: String,
        accepted: bool,
        error: Option<String>,
    },
    /// Durable intent written before Git is allowed to remove a worktree.
    WorktreeRemoveRequested {
        project: u64,
        repository: String,
        branch: String,
        path: String,
        forced: bool,
    },
    /// Git removed the registered worktree before Uniterm forgot its Project.
    WorktreeRemoved {
        project: u64,
        repository: String,
        branch: String,
        path: String,
        forced: bool,
    },
    /// Durable intent written before Git is allowed to prune a stale entry.
    WorktreeCleanupRequested {
        project: u64,
        repository: String,
        branch: String,
        path: String,
    },
    /// Git proved a stale worktree absent and pruned its administrative entry.
    WorktreeCleaned {
        project: u64,
        repository: String,
        branch: String,
        path: String,
    },
    PaneMetadataSet {
        pane: u64,
        key: String,
        value: String,
    },
    PaneSpawned {
        pane: u64,
    },
    PaneClosed {
        pane: u64,
    },
    WindowNew,
    /// The user renamed a window (an empty name clears back to the number).
    WindowRenamed {
        window: u64,
        name: String,
    },
    /// A Tab moved within one Project. Ordinals are zero-based.
    TabMoved {
        project: u64,
        from: u32,
        to: u32,
    },
    AgentBound {
        pane: u64,
        agent: String,
    },
    AgentStatus {
        pane: u64,
        status: AgentStatus,
    },
    /// The agent closed (session_end/exiting): the pane-agent binding ended.
    AgentUnbound {
        pane: u64,
    },
    TaskLaunched {
        relay: bool,
    },
    TaskCreated {
        id: u64,
        title: String,
        status: uniterm_core::TaskStatus,
    },
    TaskStatusChanged {
        id: u64,
        status: uniterm_core::TaskStatus,
    },
    TaskRetitled {
        id: u64,
        title: String,
    },
    TaskDeleted {
        id: u64,
    },
    WaitingCreated {
        item: uniterm_core::WaitingItem,
    },
    WaitingResolved {
        id: u64,
        resolution: uniterm_core::WaitingResolution,
    },
    InstructionQueued {
        item: uniterm_core::InstructionItem,
    },
    InstructionReplaced {
        replaced: u64,
        item: uniterm_core::InstructionItem,
    },
    InstructionCanceled {
        id: u64,
        reason: uniterm_core::InstructionCancellation,
    },
    InstructionDelivery {
        id: u64,
        delivery_id: u64,
        boundary: uniterm_core::InstructionBoundary,
        accepted: bool,
    },
    /// One native run-graph transition. The reducer is pure and replayable;
    /// this event is written before the server updates its live projection.
    RunGraph {
        change: uniterm_core::RunGraphEvent,
    },
    /// One typed artifact lifecycle transition. The event log remains the
    /// authority and snapshots checkpoint only the bounded projection.
    ArtifactLedger {
        change: uniterm_core::ArtifactEvent,
    },
    /// One pure Workspace automation policy decision, appended before any
    /// corresponding side effect or human escalation.
    GuardrailDecision {
        record: uniterm_core::GuardrailRecord,
    },
    OrchestrationSubmitted {
        kind: uniterm_proto::OrchestrationKind,
        task_id: u64,
        token: u64,
        status: uniterm_proto::SubmissionStatus,
        verdict: Option<String>,
        summary: String,
        artifacts: Vec<String>,
    },
    OrchestrationActivated {
        kind: uniterm_proto::OrchestrationKind,
        task_id: u64,
        role: usize,
        pane: PaneId,
        token: u64,
    },
    OrchestrationDelivery {
        kind: uniterm_proto::OrchestrationKind,
        task_id: u64,
        token: u64,
        accepted: bool,
    },
    OrchestrationArtifactsValidated {
        kind: uniterm_proto::OrchestrationKind,
        task_id: u64,
        token: u64,
        artifacts: Vec<String>,
    },
    RelayCheckpointCreated {
        task_id: u64,
        token: u64,
        checkpoint: Option<String>,
        error: Option<String>,
    },
    RelayCheckpointRolledBack {
        task_id: u64,
        checkpoint: String,
    },
    OrchestrationProjected {
        run: DurableOrchestration,
    },
    OrchestrationFinished {
        kind: uniterm_proto::OrchestrationKind,
        task_id: u64,
        outcome: String,
    },
    /// Exact invocation arguments observed after a successful PTY spawn.
    PaneLaunchProfile {
        pane: u64,
        args: Vec<String>,
    },
    /// Provider-owned native resume identity observed for one invocation.
    AgentSessionObserved {
        pane: u64,
        provider: String,
        session_id: Option<String>,
        resume_command: Vec<String>,
    },
    /// The invocation ended, so its launch overrides must not be reused.
    PaneLaunchProfileCleared {
        pane: u64,
    },
}

/// An append-only log writer that retains only the last event for deduplication.
pub struct EventLog {
    name: String,
    last: Option<LogEvent>,
    next_sequence: u64,
}

impl EventLog {
    /// Create the in-memory projection for Workspace `name`. Durable file
    /// opening and writes are performed by [`append_line`] on the runtime.
    pub fn open(name: &str) -> Self {
        EventLog {
            name: name.to_string(),
            last: None,
            next_sequence: next_sequence(name),
        }
    }

    /// Project an event immediately and prepare its durable JSON line for the
    /// runtime writer. No filesystem handle or write exists on the core loop.
    pub fn record(&mut self, event: LogEvent) -> Option<(String, String)> {
        // Identical adjacent records carry no ordering, timestamp, or state
        // information. Suppressing them keeps noisy cooperative integrations
        // from growing the durable stream or an in-memory history without
        // changing any projection.
        if self.last.as_ref() == Some(&event) {
            return None;
        }
        let envelope = EventEnvelope {
            version: EVENT_VERSION,
            sequence: self.next_sequence,
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            workspace: self.name.clone(),
            event: event.clone(),
        };
        let mut line = serde_json::to_string(&envelope).ok()?;
        line.push('\n');
        self.last = Some(event);
        self.next_sequence = self.next_sequence.saturating_add(1);
        Some((self.name.clone(), line))
    }

    /// Last sequence represented by the in-memory projection.
    pub fn current_sequence(&self) -> u64 {
        self.next_sequence.saturating_sub(1)
    }

    pub fn rename_projection(&mut self, new_name: &str) -> String {
        std::mem::replace(&mut self.name, new_name.to_string())
    }

    fn path(name: &str) -> PathBuf {
        crate::persist::snapshot_path(name).with_extension("log")
    }

    /// Read all events back from disk for round-trip tests.
    #[cfg(test)]
    pub fn load(name: &str) -> Vec<LogEvent> {
        std::fs::File::open(Self::path(name))
            .map(std::io::BufReader::new)
            .map(load_reader)
            .unwrap_or_default()
    }
}

/// Stream owned event envelopes after `sequence` without retaining history.
pub fn visit_after(
    name: &str,
    sequence: u64,
    mut visit: impl FnMut(EventEnvelope) -> std::io::Result<()>,
) -> std::io::Result<()> {
    visit_range(name, sequence, None, &mut visit)
}

/// Stream owned event envelopes in `(sequence, through]` and prove that the
/// durable stream reaches the requested high-water mark.
///
/// A bounded catch-up worker uses this after the core reports its current
/// cursor. Stopping at that cursor prevents concurrently appended live events
/// from being delivered once by history and once by the live stream.
pub fn visit_through(
    name: &str,
    sequence: u64,
    through: u64,
    mut visit: impl FnMut(EventEnvelope) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if through < sequence {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "event catch-up cursor precedes its starting cursor",
        ));
    }
    visit_range(name, sequence, Some(through), &mut visit)
}

fn visit_range(
    name: &str,
    sequence: u64,
    through: Option<u64>,
    visit: &mut impl FnMut(EventEnvelope) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if through == Some(0) {
        return Ok(());
    }
    let file = match std::fs::File::open(EventLog::path(name)) {
        Ok(file) => file,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && through.is_none_or(|cursor| cursor == 0) =>
        {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    use std::io::BufRead as _;
    let mut previous = 0u64;
    let mut synthetic = 0u64;
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        synthetic = synthetic.saturating_add(1);
        let envelope = match serde_json::from_str::<EventEnvelope>(&line) {
            Ok(envelope) if envelope.version == EVENT_VERSION => envelope,
            Ok(envelope) => {
                return Err(future_schema_error(envelope.version));
            }
            Err(_) => EventEnvelope {
                version: EVENT_VERSION,
                sequence: synthetic,
                timestamp_ms: 0,
                workspace: name.to_string(),
                event: serde_json::from_str::<LogEvent>(&line)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?,
            },
        };
        if envelope.workspace != name {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "foreign Workspace event in subscription stream",
            ));
        }
        // The first record is the stream origin, which may exceed 1.
        if previous != 0 && envelope.sequence != previous.saturating_add(1) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "event sequence gap in subscription stream",
            ));
        }
        previous = envelope.sequence;
        if through.is_some_and(|cursor| envelope.sequence > cursor) {
            break;
        }
        if envelope.sequence > sequence {
            visit(envelope)?;
        }
    }
    if through.is_some_and(|cursor| previous < cursor) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "event stream ended before the requested catch-up cursor",
        ));
    }
    Ok(())
}

#[cfg(test)]
fn load_reader(reader: impl std::io::BufRead) -> Vec<LogEvent> {
    reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| decode_record(&line).map(|record| record.event))
        .collect()
}

#[derive(Clone, Debug)]
struct DecodedRecord {
    sequence: Option<u64>,
    workspace: Option<String>,
    event: LogEvent,
}

enum RecordDecodeError {
    FutureVersion(u32),
    Malformed,
}

fn decode_record(line: &str) -> Option<DecodedRecord> {
    decode_record_strict(line).ok()
}

fn decode_record_strict(line: &str) -> Result<DecodedRecord, RecordDecodeError> {
    if let Ok(envelope) = serde_json::from_str::<EventEnvelope>(line) {
        if envelope.version == EVENT_VERSION {
            return Ok(DecodedRecord {
                sequence: Some(envelope.sequence),
                workspace: Some(envelope.workspace),
                event: envelope.event,
            });
        }
        return Err(RecordDecodeError::FutureVersion(envelope.version));
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
        if let Some(version) = value.get("version").and_then(serde_json::Value::as_u64) {
            return Err(RecordDecodeError::FutureVersion(
                version.min(u32::MAX as u64) as u32,
            ));
        }
    }
    serde_json::from_str::<LogEvent>(line)
        .map(|event| DecodedRecord {
            sequence: None,
            workspace: None,
            event,
        })
        .map_err(|_| RecordDecodeError::Malformed)
}

/// A record written by a newer Uniterm. This is the one recovery failure that
/// must stay fatal: an older binary cannot understand the stream, and must not
/// quarantine or truncate it, because the newer binary still can.
fn future_schema_error(version: u32) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("unsupported event version {version}"),
    )
}

/// Whether a recovery error means the stream was written by a newer schema.
pub fn is_future_schema_error(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::Unsupported
}

/// Move a Workspace stream that recovery cannot interpret out of the way so
/// the server can start from the durable catalog definition instead of
/// refusing forever. Nothing is deleted: the file keeps its bytes under a
/// timestamped `.log.corrupt-*` name next to the original for manual
/// inspection. Returns the backup path, or `None` when no stream existed.
pub fn quarantine(name: &str) -> std::io::Result<Option<PathBuf>> {
    let path = EventLog::path(name);
    if !path.exists() {
        return Ok(None);
    }
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup = path.with_extension(format!("log.corrupt-{suffix}"));
    std::fs::rename(&path, &backup)?;
    Ok(Some(backup))
}

fn next_sequence(name: &str) -> u64 {
    let Ok(file) = std::fs::File::open(EventLog::path(name)) else {
        return 1;
    };
    use std::io::BufRead as _;
    let mut synthetic = 0u64;
    let mut highest = 0u64;
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let Some(record) = decode_record(&line) else {
            continue;
        };
        synthetic = synthetic.saturating_add(1);
        highest = highest.max(record.sequence.unwrap_or(synthetic));
    }
    highest.saturating_add(1).max(1)
}

/// Preserve and remove only an invalid event-log suffix before production
/// recovery starts.
///
/// A failed append freezes later runtime writes, so the only automatically
/// repairable damage is a partial final line or records after the first
/// sequence break. Future schemas and ownership changes remain hard errors.
pub fn repair_consistent_prefix(name: &str) -> std::io::Result<Option<RepairReport>> {
    let path = EventLog::path(name);
    repair_consistent_prefix_path(&path, name)
}

fn repair_consistent_prefix_path(
    path: &Path,
    expected_workspace: &str,
) -> std::io::Result<Option<RepairReport>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let keep = consistent_prefix_len(&bytes, expected_workspace)?;
    if keep == bytes.len() {
        return Ok(None);
    }
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup = path.with_extension(format!("log.corrupt-{suffix}"));
    std::fs::copy(path, &backup)?;
    std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600))?;
    let tmp = path.with_extension("log.repair.tmp");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&bytes[..keep])?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(Some(RepairReport {
        backup,
        discarded_bytes: bytes.len() - keep,
    }))
}

fn consistent_prefix_len(bytes: &[u8], expected_workspace: &str) -> std::io::Result<usize> {
    let mut offset = 0usize;
    let mut previous_sequence = 0u64;
    let mut synthetic_sequence = 0u64;
    let mut owner: Option<String> = None;
    while offset < bytes.len() {
        let Some(relative_end) = bytes[offset..].iter().position(|byte| *byte == b'\n') else {
            break;
        };
        let end = offset + relative_end;
        let line = match std::str::from_utf8(&bytes[offset..end]) {
            Ok(line) => line,
            Err(_) => break,
        };
        let record = match decode_record_strict(line) {
            Ok(record) => record,
            Err(RecordDecodeError::Malformed) => break,
            Err(RecordDecodeError::FutureVersion(version)) => {
                return Err(future_schema_error(version));
            }
        };
        synthetic_sequence = synthetic_sequence.saturating_add(1);
        let sequence = record.sequence.unwrap_or(synthetic_sequence);
        // The first record establishes the stream origin. A clean stop
        // deletes the file while a still-live writer may keep its counter, and
        // a future compaction keeps only the suffix after a checkpoint, so an
        // origin above 1 is a contiguous stream, not damage. Only a break in
        // contiguity after the origin is repairable damage.
        if previous_sequence != 0 && sequence != previous_sequence.saturating_add(1) {
            break;
        }
        if let Some(actual) = record.workspace.as_deref() {
            let current = owner.get_or_insert_with(|| actual.to_string());
            if current != actual {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "event Workspace changed without an authoritative rename from {current} to {actual}"
                    ),
                ));
            }
        }
        let renamed_to = match &record.event {
            LogEvent::WorkspaceRenamed { old, new }
                if record.workspace.as_deref() == Some(old.as_str())
                    && owner.as_deref() == Some(old.as_str())
                    && old != new =>
            {
                Some(new.clone())
            }
            LogEvent::WorkspaceRenamed { .. } => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid Workspace rename ownership event",
                ));
            }
            _ => None,
        };
        previous_sequence = sequence;
        offset = end + 1;
        if let Some(new) = renamed_to {
            owner = Some(new);
        }
    }
    if let Some(actual) = owner.as_deref() {
        if actual != expected_workspace {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("event belongs to Workspace {actual}, expected {expected_workspace}"),
            ));
        }
    }
    Ok(offset)
}

/// Replay only the task projection in one streaming pass.
/// Recovery therefore uses memory proportional to live tasks rather than the
/// lifetime event count, while the full append-only history remains available
/// for the future timeline and audit surfaces.
pub fn replay_tasks(name: &str, tasks: &mut uniterm_core::TaskList) -> std::io::Result<usize> {
    match std::fs::File::open(EventLog::path(name)) {
        Ok(file) => replay_tasks_reader_scoped(std::io::BufReader::new(file), Some(name), tasks),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

/// Rebuild active human-attention items without retaining resolved history.
pub fn replay_waiting(
    name: &str,
    waiting: &mut uniterm_core::WaitingQueue,
) -> std::io::Result<usize> {
    let file = match std::fs::File::open(EventLog::path(name)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut applied = 0;
    for_each_record(
        std::io::BufReader::new(file),
        Some(name),
        |record| match record.event {
            LogEvent::WaitingCreated { item } => {
                waiting.insert(item);
                applied += 1;
            }
            LogEvent::WaitingResolved { id, .. } => {
                applied += usize::from(waiting.resolve(id).is_some());
            }
            _ => {}
        },
    )?;
    Ok(applied)
}

/// Rebuild queued human direction and both monotonic allocators.
pub fn replay_instructions(
    name: &str,
    instructions: &mut uniterm_core::InstructionQueue,
) -> std::io::Result<usize> {
    let file = match std::fs::File::open(EventLog::path(name)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    replay_instructions_reader(std::io::BufReader::new(file), Some(name), instructions)
}

/// Advance a checkpointed run graph through the remaining Workspace events.
/// Disk streaming stays on startup; steady-state updates are event driven.
pub fn replay_run_graph(
    name: &str,
    after_sequence: u64,
    graph: &mut uniterm_core::RunGraph,
) -> std::io::Result<usize> {
    let file = match std::fs::File::open(EventLog::path(name)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    replay_run_graph_reader(
        std::io::BufReader::new(file),
        Some(name),
        after_sequence,
        graph,
    )
}

fn replay_run_graph_reader(
    reader: impl std::io::BufRead,
    workspace: Option<&str>,
    after_sequence: u64,
    graph: &mut uniterm_core::RunGraph,
) -> std::io::Result<usize> {
    let mut applied = 0;
    let mut synthetic_sequence = 0u64;
    let mut projection_error = None;
    for_each_record(reader, workspace, |record| {
        synthetic_sequence = synthetic_sequence.saturating_add(1);
        let sequence = record.sequence.unwrap_or(synthetic_sequence);
        if sequence <= after_sequence || projection_error.is_some() {
            return;
        }
        if let LogEvent::RunGraph { change } = record.event {
            match graph.apply(change) {
                Ok(()) => applied += 1,
                Err(error) => projection_error = Some(error),
            }
        }
    })?;
    if let Some(error) = projection_error {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid run graph event: {error}"),
        ));
    }
    Ok(applied)
}

/// Advance a checkpointed artifact ledger through the remaining Workspace
/// events without collecting lifetime history in memory.
pub fn replay_artifacts(
    name: &str,
    after_sequence: u64,
    artifacts: &mut uniterm_core::ArtifactLedger,
) -> std::io::Result<usize> {
    let file = match std::fs::File::open(EventLog::path(name)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    replay_artifacts_reader(
        std::io::BufReader::new(file),
        Some(name),
        after_sequence,
        artifacts,
    )
}

fn replay_artifacts_reader(
    reader: impl std::io::BufRead,
    workspace: Option<&str>,
    after_sequence: u64,
    artifacts: &mut uniterm_core::ArtifactLedger,
) -> std::io::Result<usize> {
    let mut applied = 0;
    let mut synthetic_sequence = 0u64;
    let mut projection_error = None;
    for_each_record(reader, workspace, |record| {
        synthetic_sequence = synthetic_sequence.saturating_add(1);
        let sequence = record.sequence.unwrap_or(synthetic_sequence);
        if sequence <= after_sequence || projection_error.is_some() {
            return;
        }
        if let LogEvent::ArtifactLedger { change } = record.event {
            match artifacts.apply(change) {
                Ok(()) => applied += 1,
                Err(error) => projection_error = Some(error),
            }
        }
    })?;
    if let Some(error) = projection_error {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid artifact ledger event: {error}"),
        ));
    }
    Ok(applied)
}

fn replay_instructions_reader(
    reader: impl std::io::BufRead,
    workspace: Option<&str>,
    instructions: &mut uniterm_core::InstructionQueue,
) -> std::io::Result<usize> {
    let mut applied = 0;
    for_each_record(reader, workspace, |record| match record.event {
        LogEvent::InstructionQueued { item } => {
            instructions.insert(item);
            applied += 1;
        }
        LogEvent::InstructionReplaced { replaced, item } => {
            if !instructions.replace(replaced, item.clone()) {
                instructions.insert(item);
            }
            applied += 1;
        }
        LogEvent::InstructionCanceled { id, .. } => {
            applied += usize::from(instructions.remove(id).is_some());
        }
        LogEvent::InstructionDelivery {
            id,
            delivery_id,
            accepted,
            ..
        } => {
            instructions.observe_delivery_id(delivery_id);
            if accepted {
                applied += usize::from(instructions.remove(id).is_some());
            }
        }
        _ => {}
    })?;
    Ok(applied)
}

/// Rebuild only currently active orchestration runs in one streaming pass.
pub fn replay_orchestrations(name: &str) -> std::io::Result<Vec<DurableOrchestration>> {
    let file = match std::fs::File::open(EventLog::path(name)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut active = std::collections::BTreeMap::new();
    for_each_record(
        std::io::BufReader::new(file),
        Some(name),
        |record| match record.event {
            LogEvent::OrchestrationProjected { run } => {
                active.insert(run.task_id, run);
            }
            LogEvent::OrchestrationFinished { task_id, .. } => {
                active.remove(&task_id);
            }
            _ => {}
        },
    )?;
    Ok(active.into_values().collect())
}

#[cfg(test)]
fn replay_tasks_reader(
    reader: impl std::io::BufRead,
    tasks: &mut uniterm_core::TaskList,
) -> std::io::Result<usize> {
    replay_tasks_reader_scoped(reader, None, tasks)
}

fn replay_tasks_reader_scoped(
    reader: impl std::io::BufRead,
    workspace: Option<&str>,
    tasks: &mut uniterm_core::TaskList,
) -> std::io::Result<usize> {
    let mut applied = 0;
    for_each_record(reader, workspace, |record| {
        let changed = match record.event {
            LogEvent::TaskCreated { id, title, status } => {
                tasks.insert(id, &title, status);
                true
            }
            LogEvent::TaskStatusChanged { id, status } => tasks.set_status(id, status),
            LogEvent::TaskRetitled { id, title } => tasks.set_title(id, &title),
            LogEvent::TaskDeleted { id } => tasks.remove(id),
            _ => false,
        };
        applied += usize::from(changed);
    })?;
    Ok(applied)
}

fn for_each_record(
    reader: impl std::io::BufRead,
    workspace: Option<&str>,
    mut apply: impl FnMut(DecodedRecord),
) -> std::io::Result<()> {
    let mut lines = reader.lines().peekable();
    let mut owner: Option<String> = None;
    while let Some(line) = lines.next() {
        let line = line?;
        let record = match decode_record_strict(&line) {
            Ok(record) => record,
            Err(RecordDecodeError::Malformed) if lines.peek().is_none() => break,
            Err(RecordDecodeError::Malformed) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "malformed event-log record before end of stream",
                ));
            }
            Err(RecordDecodeError::FutureVersion(version)) => {
                return Err(future_schema_error(version));
            }
        };
        if let Some(actual) = record.workspace.as_deref() {
            let current = owner.get_or_insert_with(|| actual.to_string());
            if current != actual {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "event Workspace changed without an authoritative rename from {current} to {actual}"
                    ),
                ));
            }
        }
        let renamed_to = match &record.event {
            LogEvent::WorkspaceRenamed { old, new }
                if record.workspace.as_deref() == Some(old.as_str())
                    && owner.as_deref() == Some(old.as_str())
                    && old != new =>
            {
                Some(new.clone())
            }
            LogEvent::WorkspaceRenamed { .. } => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid Workspace rename ownership event",
                ));
            }
            _ => None,
        };
        apply(record);
        if let Some(new) = renamed_to {
            owner = Some(new);
        }
    }
    if let (Some(expected), Some(actual)) = (workspace, owner.as_deref()) {
        if actual != expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("event belongs to Workspace {actual}, expected {expected}"),
            ));
        }
    }
    Ok(())
}

/// Restore the newest structural projection after an optional grid checkpoint.
///
/// The scan retains only one structural state, so memory is proportional to
/// the live Workspace rather than event-log lifetime. Legacy unwrapped events
/// remain readable, but cannot supersede a versioned snapshot cursor because
/// they have no stable sequence.
pub fn recover_snapshot(
    name: &str,
    checkpoint: Option<crate::persist::Snapshot>,
) -> std::io::Result<Option<crate::persist::Snapshot>> {
    let file = match std::fs::File::open(EventLog::path(name)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(checkpoint),
        Err(error) => return Err(error),
    };
    recover_snapshot_reader_scoped(std::io::BufReader::new(file), checkpoint, Some(name))
}

#[cfg(test)]
fn recover_snapshot_reader(
    reader: impl std::io::BufRead,
    checkpoint: Option<crate::persist::Snapshot>,
) -> std::io::Result<Option<crate::persist::Snapshot>> {
    recover_snapshot_reader_scoped(reader, checkpoint, None)
}

fn recover_snapshot_reader_scoped(
    reader: impl std::io::BufRead,
    checkpoint: Option<crate::persist::Snapshot>,
    workspace: Option<&str>,
) -> std::io::Result<Option<crate::persist::Snapshot>> {
    let checkpoint_sequence = checkpoint
        .as_ref()
        .map_or(0, |snapshot| snapshot.event_sequence);
    let mut latest: Option<(u64, StructuralProjection)> = None;
    let mut previous_sequence = 0u64;
    let mut synthetic_sequence = 0u64;
    for_each_record(reader, workspace, |record| {
        synthetic_sequence = synthetic_sequence.saturating_add(1);
        let sequence = record.sequence.unwrap_or(synthetic_sequence);
        // The origin may exceed 1 (see `consistent_prefix_len`); only a gap or
        // repeat after the origin is invalid.
        if previous_sequence != 0 && sequence != previous_sequence.saturating_add(1) {
            previous_sequence = u64::MAX;
            return;
        }
        previous_sequence = sequence;
        if sequence <= checkpoint_sequence {
            return;
        }
        if let LogEvent::WorkspaceProjected { state } = record.event {
            latest = Some((sequence, state));
        }
    })?;
    if previous_sequence == u64::MAX {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "event sequence is duplicate, out of order, or contains a gap",
        ));
    }
    Ok(match latest {
        Some((sequence, state)) => Some(state.into_snapshot(checkpoint, sequence)),
        None => checkpoint,
    })
}

/// Return whether a durable Workspace event stream exists, including an
/// orphaned stream whose snapshot was lost after a crash.
pub fn exists(name: &str) -> bool {
    EventLog::path(name).is_file()
}

/// Enumerate Workspace names with durable event streams so bulk cleanup can
/// remove orphaned crash-recovery state as well as catalog definitions.
pub fn list_names() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(crate::persist::state_dir()) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("log") {
            if let Some(name) = path.file_stem().and_then(|value| value.to_str()) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Append one prepared record. Called only by the agent runtime.
pub fn append_line(name: &str, line: &str) -> std::io::Result<()> {
    let path = EventLog::path(name);
    crate::persist::open_private_append(&path)?.write_all(line.as_bytes())
}

/// Make every event appended before a snapshot durable before that snapshot
/// can advertise its sequence as checkpointed.
pub fn sync(name: &str) -> std::io::Result<()> {
    match std::fs::File::open(EventLog::path(name)) {
        Ok(file) => crate::persist::sync_file_for_crash(&file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Move a Workspace's durable event stream. Called only by the runtime.
pub fn rename(old: &str, new: &str) -> std::io::Result<()> {
    std::fs::rename(EventLog::path(old), EventLog::path(new))
}

/// Delete a Workspace's event stream after intentional shutdown.
pub fn delete(name: &str) -> std::io::Result<()> {
    match std::fs::remove_file(EventLog::path(name)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot(sequence: u64, name: &str) -> crate::persist::Snapshot {
        crate::persist::Snapshot::new_with_sequence(
            0,
            2,
            ProjectId(1),
            2,
            vec![crate::persist::ProjectSnap {
                id: ProjectId(1),
                name: name.into(),
                root: "/tmp/project".into(),
                active_pane: Some(PaneId(1)),
                metadata: Vec::new(),
            }],
            vec![crate::persist::WinSnap {
                project: ProjectId(1),
                layout: LayoutNode::Leaf(PaneId(1)),
                active: PaneId(1),
                zoomed: None,
                name: None,
                panes: vec![crate::persist::PaneSnap {
                    id: PaneId(1),
                    cwd: Some("/tmp/project".into()),
                    content: vec![uniterm_core::StoredLine {
                        cells: Vec::new(),
                        wrapped: false,
                    }],
                    metadata: Vec::new(),
                    launch_args: Vec::new(),
                    agent_launch: None,
                }],
            }],
            sequence,
        )
    }

    fn envelope_line(sequence: u64, event: LogEvent) -> String {
        envelope_line_for("recover", sequence, event)
    }

    fn envelope_line_for(workspace: &str, sequence: u64, event: LogEvent) -> String {
        let mut line = serde_json::to_string(&EventEnvelope {
            version: EVENT_VERSION,
            sequence,
            timestamp_ms: sequence,
            workspace: workspace.into(),
            event,
        })
        .unwrap();
        line.push('\n');
        line
    }

    #[test]
    fn append_projects_and_persists() {
        let base = std::env::temp_dir().join(format!("uniterm-log-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::env::set_var("XDG_STATE_HOME", &base);

        let mut log = EventLog::open("logtest");
        let mut persist = |event| {
            let (name, line) = log.record(event).unwrap();
            append_line(&name, &line).unwrap();
        };
        persist(LogEvent::PaneSpawned { pane: 1 });
        persist(LogEvent::ProjectCreated {
            project: 1,
            name: "Uniterm".into(),
            root: "/tmp/uniterm".into(),
        });
        persist(LogEvent::AgentBound {
            pane: 1,
            agent: "claude".into(),
        });
        persist(LogEvent::AgentStatus {
            pane: 1,
            status: AgentStatus::Permission,
        });
        persist(LogEvent::ProjectReordered {
            projects: vec![2, 1],
        });
        // Round-trips through disk.
        let back = EventLog::load("logtest");
        assert_eq!(back.len(), 5);
        assert_eq!(back[0], LogEvent::PaneSpawned { pane: 1 });
        assert!(matches!(back[3], LogEvent::AgentStatus { .. }));
        assert_eq!(
            back[4],
            LogEvent::ProjectReordered {
                projects: vec![2, 1]
            }
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn adjacent_duplicates_do_not_grow_the_stream() {
        let mut log = EventLog::open("dedupe");
        let event = LogEvent::AgentStatus {
            pane: 7,
            status: AgentStatus::Working,
        };
        assert!(log.record(event.clone()).is_some());
        assert!(log.record(event).is_none());
        assert!(log
            .record(LogEvent::AgentStatus {
                pane: 7,
                status: AgentStatus::Idle,
            })
            .is_some());
    }

    #[test]
    fn run_graph_replay_advances_a_checkpoint_through_handoff_and_completion() {
        use uniterm_core::{RoleId, RunGraphEvent, RunId, RunKind, RunStatus};

        let changes = vec![
            RunGraphEvent::Created {
                run: RunId(1),
                parent: None,
                project: ProjectId(7),
                kind: RunKind::Relay,
                task_id: 11,
                title: "review the change".into(),
            },
            RunGraphEvent::RoleDeclared {
                run: RunId(1),
                role: RoleId(1),
                name: "builder".into(),
                pane: PaneId(20),
                provider: "provider".into(),
            },
            RunGraphEvent::RoleDeclared {
                run: RunId(1),
                role: RoleId(2),
                name: "reviewer".into(),
                pane: PaneId(21),
                provider: "provider".into(),
            },
            RunGraphEvent::Activated {
                run: RunId(1),
                role: RoleId(1),
                activation: 1,
            },
            RunGraphEvent::Handoff {
                run: RunId(1),
                from: RoleId(1),
                to: RoleId(2),
                activation: 2,
            },
            RunGraphEvent::Completed {
                run: RunId(1),
                outcome: "done".into(),
            },
        ];
        let mut checkpoint = uniterm_core::RunGraph::new();
        for change in changes.iter().take(3).cloned() {
            checkpoint.apply(change).unwrap();
        }
        let input = changes
            .into_iter()
            .enumerate()
            .map(|(index, change)| {
                envelope_line(
                    u64::try_from(index).unwrap() + 1,
                    LogEvent::RunGraph { change },
                )
            })
            .collect::<String>();

        let applied = replay_run_graph_reader(
            std::io::Cursor::new(input),
            Some("recover"),
            3,
            &mut checkpoint,
        )
        .unwrap();
        assert_eq!(applied, 3);
        assert_eq!(
            checkpoint.run(RunId(1)).unwrap().status,
            RunStatus::Completed
        );
        assert_eq!(
            checkpoint.run(RunId(1)).unwrap().outcome.as_deref(),
            Some("done")
        );
        assert_eq!(checkpoint.active_for_pane(PaneId(21)), None);
        assert_eq!(checkpoint.next_activation_id(), 3);
    }

    #[test]
    fn run_graph_replay_rejects_a_foreign_workspace_stream() {
        let input = envelope_line_for(
            "foreign",
            1,
            LogEvent::RunGraph {
                change: uniterm_core::RunGraphEvent::Created {
                    run: uniterm_core::RunId(1),
                    parent: None,
                    project: ProjectId(1),
                    kind: uniterm_core::RunKind::Workflow,
                    task_id: 1,
                    title: "foreign".into(),
                },
            },
        );
        let error = replay_run_graph_reader(
            std::io::Cursor::new(input),
            Some("recover"),
            0,
            &mut uniterm_core::RunGraph::new(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn artifact_replay_advances_only_the_suffix_after_a_checkpoint() {
        use uniterm_core::{
            ArtifactEvent, ArtifactId, ArtifactKind, ArtifactLedger, ArtifactStatus, RoleId, RunId,
        };

        let observed = ArtifactEvent::Observed {
            artifact: ArtifactId(1),
            project: ProjectId(7),
            producer_run: RunId(3),
            producer_role: RoleId(4),
            kind: ArtifactKind::Plan,
            path: "WORKFLOW_PLAN.md".into(),
            digest: "a".repeat(64),
            size: 12,
        };
        let refreshed = ArtifactEvent::Refreshed {
            artifact: ArtifactId(1),
            digest: "b".repeat(64),
            size: 18,
        };
        let missing = ArtifactEvent::Missing {
            artifact: ArtifactId(1),
        };
        let mut checkpoint = ArtifactLedger::new();
        checkpoint.apply(observed.clone()).unwrap();
        let input = [observed, refreshed, missing]
            .into_iter()
            .enumerate()
            .map(|(index, change)| {
                envelope_line(
                    u64::try_from(index).unwrap() + 1,
                    LogEvent::ArtifactLedger { change },
                )
            })
            .collect::<String>();

        let applied = replay_artifacts_reader(
            std::io::Cursor::new(input),
            Some("recover"),
            1,
            &mut checkpoint,
        )
        .unwrap();
        assert_eq!(applied, 2);
        let artifact = checkpoint.artifact(ArtifactId(1)).unwrap();
        assert_eq!(artifact.digest, "b".repeat(64));
        assert_eq!(artifact.size, 18);
        assert_eq!(artifact.status, ArtifactStatus::Missing);
    }

    #[test]
    fn artifact_replay_rejects_a_foreign_workspace_stream() {
        let input = envelope_line_for(
            "foreign",
            1,
            LogEvent::ArtifactLedger {
                change: uniterm_core::ArtifactEvent::Observed {
                    artifact: uniterm_core::ArtifactId(1),
                    project: ProjectId(1),
                    producer_run: uniterm_core::RunId(1),
                    producer_role: uniterm_core::RoleId(1),
                    kind: uniterm_core::ArtifactKind::Report,
                    path: "report.md".into(),
                    digest: "c".repeat(64),
                    size: 1,
                },
            },
        );
        let error = replay_artifacts_reader(
            std::io::Cursor::new(input),
            Some("recover"),
            0,
            &mut uniterm_core::ArtifactLedger::new(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn envelope_sequences_are_monotonic_and_workspace_scoped() {
        let mut log = EventLog {
            name: "scope".into(),
            last: None,
            next_sequence: 7,
        };
        let (_, first) = log
            .record(LogEvent::PaneSpawned { pane: 1 })
            .expect("first event");
        let (_, second) = log
            .record(LogEvent::PaneClosed { pane: 1 })
            .expect("second event");
        let first: EventEnvelope = serde_json::from_str(first.trim()).unwrap();
        let second: EventEnvelope = serde_json::from_str(second.trim()).unwrap();
        assert_eq!(first.workspace, "scope");
        assert_eq!(first.sequence, 7);
        assert_eq!(second.sequence, 8);
        assert_eq!(log.current_sequence(), 8);
    }

    #[test]
    fn structural_suffix_overrides_checkpoint_and_keeps_grid_content() {
        let checkpoint = sample_snapshot(1, "before");
        let after = sample_snapshot(0, "after");
        let input = format!(
            "{}{}",
            envelope_line(1, LogEvent::PaneSpawned { pane: 1 }),
            envelope_line(
                2,
                LogEvent::WorkspaceProjected {
                    state: StructuralProjection::from_snapshot(&after),
                }
            )
        );
        let recovered =
            recover_snapshot_reader(std::io::Cursor::new(input.into_bytes()), Some(checkpoint))
                .unwrap()
                .unwrap();
        assert_eq!(recovered.event_sequence, 2);
        assert_eq!(recovered.projects[0].name, "after");
        assert_eq!(recovered.windows[0].panes[0].content.len(), 1);
    }

    #[test]
    fn structural_log_recovers_without_a_snapshot() {
        let state = sample_snapshot(0, "from-log");
        let input = envelope_line(
            1,
            LogEvent::WorkspaceProjected {
                state: StructuralProjection::from_snapshot(&state),
            },
        );
        let recovered = recover_snapshot_reader(std::io::Cursor::new(input), None)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.event_sequence, 1);
        assert_eq!(recovered.projects[0].name, "from-log");
        assert!(recovered.windows[0].panes[0].content.is_empty());
    }

    #[test]
    fn sequence_gap_is_rejected_instead_of_silently_losing_state() {
        let state = sample_snapshot(0, "gap");
        let event = LogEvent::WorkspaceProjected {
            state: StructuralProjection::from_snapshot(&state),
        };
        let input = format!(
            "{}{}",
            envelope_line(1, event.clone()),
            envelope_line(3, event)
        );
        let error = match recover_snapshot_reader(std::io::Cursor::new(input), None) {
            Err(error) => error,
            Ok(_) => panic!("sequence gap was accepted"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn duplicate_sequence_is_rejected_instead_of_replaying_twice() {
        let event = LogEvent::PaneSpawned { pane: 1 };
        let input = format!(
            "{}{}",
            envelope_line(1, event.clone()),
            envelope_line(1, event)
        );
        let error = match recover_snapshot_reader(std::io::Cursor::new(input), None) {
            Err(error) => error,
            Ok(_) => panic!("duplicate sequence was accepted"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn stream_origin_above_one_is_a_contiguous_stream_not_damage() {
        // A clean stop deleted the file while a live writer kept counting, so
        // the surviving stream starts mid-history. It is still contiguous and
        // must be both accepted by repair and replayed by recovery.
        let state = sample_snapshot(0, "recover");
        let projected = LogEvent::WorkspaceProjected {
            state: StructuralProjection::from_snapshot(&state),
        };
        let input = format!(
            "{}{}{}",
            envelope_line(4309, LogEvent::PaneSpawned { pane: 5 }),
            envelope_line(4310, projected),
            envelope_line(4311, LogEvent::PaneClosed { pane: 5 })
        );
        assert_eq!(
            consistent_prefix_len(input.as_bytes(), "recover").unwrap(),
            input.len()
        );
        let recovered = recover_snapshot_reader(std::io::Cursor::new(input.clone()), None)
            .unwrap()
            .expect("structural event after the origin is restored");
        assert_eq!(recovered.event_sequence, 4310);

        // A checkpoint older than the origin does not hide the suffix.
        let recovered = recover_snapshot_reader(
            std::io::Cursor::new(input.clone()),
            Some(sample_snapshot(4000, "recover")),
        )
        .unwrap()
        .unwrap();
        assert_eq!(recovered.event_sequence, 4310);

        // A gap after the origin is still repairable damage.
        let gapped = format!(
            "{input}{}",
            envelope_line(4313, LogEvent::PaneSpawned { pane: 6 })
        );
        assert_eq!(
            consistent_prefix_len(gapped.as_bytes(), "recover").unwrap(),
            input.len()
        );
    }

    #[test]
    fn repair_keeps_the_prefix_before_a_partial_failed_append() {
        let complete = envelope_line(1, LogEvent::PaneSpawned { pane: 1 });
        let input = format!("{complete}{{\"version\":{EVENT_VERSION},\"sequence\":2");
        let keep = consistent_prefix_len(input.as_bytes(), "recover").unwrap();
        assert_eq!(keep, complete.len());

        let recovered = recover_snapshot_reader(
            std::io::Cursor::new(input.as_bytes()[..keep].to_vec()),
            None,
        )
        .unwrap();
        assert!(recovered.is_none());
    }

    #[test]
    fn repair_discards_a_gapped_suffix_but_never_a_future_schema() {
        let complete = envelope_line(1, LogEvent::PaneSpawned { pane: 1 });
        let gapped = format!(
            "{complete}{}",
            envelope_line(3, LogEvent::PaneClosed { pane: 1 })
        );
        assert_eq!(
            consistent_prefix_len(gapped.as_bytes(), "recover").unwrap(),
            complete.len()
        );

        let future = format!(
            "{complete}{{\"version\":999,\"sequence\":2,\"timestamp_ms\":2,\"workspace\":\"recover\",\"event\":{{\"PaneClosed\":{{\"pane\":1}}}}}}\n"
        );
        let error = consistent_prefix_len(future.as_bytes(), "recover").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert!(is_future_schema_error(&error));
        assert!(error.to_string().contains("unsupported event version 999"));
    }

    #[test]
    fn repair_atomically_preserves_the_original_and_activates_the_prefix() {
        let dir = std::env::temp_dir().join(format!(
            "uniterm-repair-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recover.log");
        let complete = envelope_line(1, LogEvent::PaneSpawned { pane: 1 });
        let original = format!("{complete}{{\"version\":{EVENT_VERSION},\"sequence\":2");
        std::fs::write(&path, &original).unwrap();

        let report = repair_consistent_prefix_path(&path, "recover")
            .unwrap()
            .expect("repair report");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), complete);
        assert_eq!(std::fs::read_to_string(&report.backup).unwrap(), original);
        assert_eq!(report.discarded_bytes, original.len() - complete.len());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn truncated_final_record_does_not_hide_the_last_complete_projection() {
        let state = sample_snapshot(0, "complete");
        let input = format!(
            "{}{{\"version\":1,\"sequence\":2",
            envelope_line(
                1,
                LogEvent::WorkspaceProjected {
                    state: StructuralProjection::from_snapshot(&state),
                }
            )
        );
        let recovered = recover_snapshot_reader(std::io::Cursor::new(input), None)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.event_sequence, 1);
        assert_eq!(recovered.projects[0].name, "complete");
    }

    #[test]
    fn scoped_recovery_rejects_a_foreign_workspace_record() {
        let state = sample_snapshot(0, "foreign");
        let input = envelope_line(
            1,
            LogEvent::WorkspaceProjected {
                state: StructuralProjection::from_snapshot(&state),
            },
        );
        let error = match recover_snapshot_reader_scoped(
            std::io::Cursor::new(input),
            None,
            Some("different"),
        ) {
            Err(error) => error,
            Ok(_) => panic!("foreign Workspace event was accepted"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn scoped_recovery_follows_only_authoritative_workspace_renames() {
        let first = sample_snapshot(0, "first");
        let final_state = sample_snapshot(0, "final");
        let input = format!(
            "{}{}{}{}{}",
            envelope_line_for(
                "old",
                1,
                LogEvent::WorkspaceProjected {
                    state: StructuralProjection::from_snapshot(&first),
                },
            ),
            envelope_line_for(
                "old",
                2,
                LogEvent::WorkspaceRenamed {
                    old: "old".into(),
                    new: "middle".into(),
                },
            ),
            envelope_line_for("middle", 3, LogEvent::PaneSpawned { pane: 7 }),
            envelope_line_for(
                "middle",
                4,
                LogEvent::WorkspaceRenamed {
                    old: "middle".into(),
                    new: "final".into(),
                },
            ),
            envelope_line_for(
                "final",
                5,
                LogEvent::WorkspaceProjected {
                    state: StructuralProjection::from_snapshot(&final_state),
                },
            ),
        );
        let recovered =
            recover_snapshot_reader_scoped(std::io::Cursor::new(input), None, Some("final"))
                .unwrap()
                .unwrap();
        assert_eq!(recovered.event_sequence, 5);
        assert_eq!(recovered.projects[0].name, "final");
    }

    #[test]
    fn rename_event_must_be_owned_by_its_old_workspace() {
        let input = envelope_line_for(
            "intruder",
            1,
            LogEvent::WorkspaceRenamed {
                old: "old".into(),
                new: "final".into(),
            },
        );
        let error = match recover_snapshot_reader_scoped(
            std::io::Cursor::new(input),
            None,
            Some("final"),
        ) {
            Err(error) => error,
            Ok(_) => panic!("foreign Workspace forged a rename"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn unknown_event_versions_fail_with_an_explicit_diagnostic() {
        let input = r#"{"version":999,"sequence":1,"timestamp_ms":1,"workspace":"recover","event":{"PaneSpawned":{"pane":1}}}
"#;
        let error = match recover_snapshot_reader(std::io::Cursor::new(input), None) {
            Err(error) => error,
            Ok(_) => panic!("future event version was accepted"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert!(is_future_schema_error(&error));
        assert!(error.to_string().contains("unsupported event version 999"));
    }

    #[test]
    fn large_task_history_replays_without_collecting_the_event_stream() {
        use uniterm_core::{TaskList, TaskStatus};

        const EVENTS: usize = 100_000;
        let mut input = Vec::with_capacity(EVENTS * 64);
        writeln!(
            input,
            "{}",
            serde_json::to_string(&LogEvent::TaskCreated {
                id: 1,
                title: "long-lived task".into(),
                status: TaskStatus::Todo,
            })
            .unwrap()
        )
        .unwrap();
        for index in 1..EVENTS {
            let status = if index % 2 == 0 {
                TaskStatus::Doing
            } else {
                TaskStatus::Blocked
            };
            writeln!(
                input,
                "{}",
                serde_json::to_string(&LogEvent::TaskStatusChanged { id: 1, status }).unwrap()
            )
            .unwrap();
        }

        let started = std::time::Instant::now();
        let mut tasks = TaskList::new();
        let applied = replay_tasks_reader(std::io::Cursor::new(input), &mut tasks).unwrap();
        assert_eq!(applied, EVENTS);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks.get(1).unwrap().status, TaskStatus::Blocked);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn orchestration_projection_replay_keeps_only_active_runs() {
        use uniterm_core::orchestrate::{step, Event, Role, State};

        let mut state = State::new(vec![Role::new("builder", false)], 1);
        step(&mut state, Event::Start);
        let run = DurableOrchestration {
            kind: uniterm_proto::OrchestrationKind::Workflow,
            task_id: 8,
            template: Some("solo".into()),
            goal: "ship it".into(),
            role_providers: vec![DurableRoleProvider {
                provider: "fake".into(),
                command: "fake".into(),
            }],
            agent_id: "fake".into(),
            agent_cmd: "fake".into(),
            role_panes: vec![PaneId(7)],
            started: vec![true],
            state,
            checkpoints: Vec::new(),
            guardrail: Box::new(DurableGuardrail {
                started_at_ms: 123,
                ..DurableGuardrail::default()
            }),
        };
        let mut legacy_json = serde_json::to_value(&run).unwrap();
        legacy_json
            .as_object_mut()
            .unwrap()
            .remove("role_providers");
        legacy_json.as_object_mut().unwrap().remove("guardrail");
        let legacy: DurableOrchestration = serde_json::from_value(legacy_json).unwrap();
        assert!(legacy.role_providers.is_empty());
        assert_eq!(legacy.agent_id, "fake");
        assert_eq!(legacy.guardrail.started_at_ms, 0);
        assert_eq!(
            legacy.guardrail.limits,
            uniterm_core::GuardLimits::default()
        );
        assert!(!legacy.guardrail.elapsed_triggered);
        let input = format!(
            "{}{}",
            envelope_line(1, LogEvent::OrchestrationProjected { run: run.clone() }),
            envelope_line(
                2,
                LogEvent::OrchestrationFinished {
                    kind: uniterm_proto::OrchestrationKind::Workflow,
                    task_id: 8,
                    outcome: "done".into(),
                }
            )
        );
        let mut active = std::collections::BTreeMap::new();
        for_each_record(
            std::io::Cursor::new(input),
            Some("recover"),
            |record| match record.event {
                LogEvent::OrchestrationProjected { run } => {
                    active.insert(run.task_id, run);
                }
                LogEvent::OrchestrationFinished { task_id, .. } => {
                    active.remove(&task_id);
                }
                _ => {}
            },
        )
        .unwrap();
        assert!(active.is_empty());
    }

    #[test]
    fn instruction_replay_restores_only_pending_items_and_allocators() {
        use uniterm_core::{
            InstructionAuthor, InstructionBoundary, InstructionItem, InstructionPolicy,
            InstructionState,
        };

        let item = |id, sequence, text: &str| InstructionItem {
            id,
            pane: PaneId(7),
            invocation: 44,
            author: InstructionAuthor::Cli,
            created_sequence: sequence,
            policy: InstructionPolicy::NextReady,
            state: InstructionState::Queued,
            text: text.into(),
        };
        let input = [
            envelope_line(
                1,
                LogEvent::InstructionQueued {
                    item: item(10, 1, "old"),
                },
            ),
            envelope_line(
                2,
                LogEvent::InstructionReplaced {
                    replaced: 10,
                    item: item(11, 2, "replacement"),
                },
            ),
            envelope_line(
                3,
                LogEvent::InstructionDelivery {
                    id: 11,
                    delivery_id: 29,
                    boundary: InstructionBoundary::CooperativeReady,
                    accepted: true,
                },
            ),
            envelope_line(
                4,
                LogEvent::InstructionQueued {
                    item: item(20, 4, "still pending"),
                },
            ),
            envelope_line(
                5,
                LogEvent::InstructionDelivery {
                    id: 20,
                    delivery_id: 30,
                    boundary: InstructionBoundary::SendNow,
                    accepted: false,
                },
            ),
        ]
        .concat();
        let mut queue = uniterm_core::InstructionQueue::new();
        replay_instructions_reader(std::io::Cursor::new(input), Some("recover"), &mut queue)
            .unwrap();
        assert_eq!(queue.items().len(), 1);
        assert_eq!(queue.items()[0].id, 20);
        let next = queue.allocate(PaneId(8), 45, InstructionAuthor::ControlApi, 6, "next");
        assert_eq!(next.id, 21);
        assert_eq!(queue.mint_delivery_id(), 31);
    }
}
