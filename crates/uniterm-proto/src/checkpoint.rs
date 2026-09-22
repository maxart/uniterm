//! Owned checkpoint DTOs for the runtime seam and durable schema.

use serde::{Deserialize, Serialize};
use uniterm_core::{GridCapture, LayoutNode, PaneId, ProjectId, StoredLine};

/// Current durable schema. Changing capture transport does not change it.
pub const SNAPSHOT_VERSION: u32 = 12;

/// Event-aligned recovery projection. Transport captures use compact content;
/// durable snapshots use resolved lines without changing the storage schema.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot<C = Vec<StoredLine>> {
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
    pub windows: Vec<WinSnap<C>>,
}

/// Project identity and focus retained across checkpoint recovery.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectSnap {
    pub id: ProjectId,
    pub name: String,
    pub root: String,
    pub active_pane: Option<PaneId>,
    pub metadata: Vec<(String, String)>,
}

/// A Tab's layout and owned Panes, independent of the runtime.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WinSnap<C = Vec<StoredLine>> {
    pub project: ProjectId,
    pub layout: LayoutNode,
    pub active: PaneId,
    pub zoomed: Option<PaneId>,
    /// User-given window name (rename tab), if any.
    pub name: Option<String>,
    pub panes: Vec<PaneSnap<C>>,
}

/// Pane launch metadata plus an owned terminal-content projection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneSnap<C = Vec<StoredLine>> {
    pub id: PaneId,
    pub cwd: Option<String>,
    /// Saved grid + scrollback content (P2-2), replayed into scrollback on
    /// restore. Older schema migrations supply empty content.
    pub content: C,
    pub metadata: Vec<(String, String)>,
    /// Arguments originally passed to Uniterm's configured shell/program.
    #[serde(default)]
    pub launch_args: Vec<String>,
    /// Native provider session identity and opaque resume command, when the
    /// provider supplied one cooperatively.
    #[serde(default)]
    pub agent_launch: Option<AgentLaunchSnap>,
}

/// Provider-owned native resume identity, never inferred during restore.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentLaunchSnap {
    pub provider: String,
    pub session_id: Option<String>,
    /// Complete argv, including the provider executable. Uniterm never
    /// manufactures provider-specific flags outside the provider boundary.
    pub resume_command: Vec<String>,
}

impl<C> Snapshot<C> {
    /// Transform terminal content without changing ownership or event cursors.
    /// Useful for reference projections and schema-compatibility checks.
    pub fn map_content<D>(self, mut map: impl FnMut(C) -> D) -> Snapshot<D> {
        Snapshot {
            version: self.version,
            event_sequence: self.event_sequence,
            run_graph_sequence: self.run_graph_sequence,
            run_graph: self.run_graph,
            artifact_sequence: self.artifact_sequence,
            artifacts: self.artifacts,
            active_window: self.active_window,
            next_pane_id: self.next_pane_id,
            active_project: self.active_project,
            next_project_id: self.next_project_id,
            projects: self.projects,
            windows: self
                .windows
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
                            content: map(pane.content),
                            metadata: pane.metadata,
                            launch_args: pane.launch_args,
                            agent_launch: pane.agent_launch,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// Construct a checkpoint before any event has been applied.
    pub fn new(
        active_window: usize,
        next_pane_id: u64,
        active_project: ProjectId,
        next_project_id: u64,
        projects: Vec<ProjectSnap>,
        windows: Vec<WinSnap<C>>,
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

    /// Bind the projection to the authoritative event prefix it includes.
    pub fn new_with_sequence(
        active_window: usize,
        next_pane_id: u64,
        active_project: ProjectId,
        next_project_id: u64,
        projects: Vec<ProjectSnap>,
        windows: Vec<WinSnap<C>>,
        event_sequence: u64,
    ) -> Self {
        Snapshot {
            version: SNAPSHOT_VERSION,
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

impl Snapshot<GridCapture> {
    /// Budget detached transport memory without serializing terminal cells.
    /// Captures account for capacities; bounded metadata gets extra headroom
    /// for collection headers and allocation slack absent from its encoding.
    pub fn retained_bytes(&self) -> usize {
        fn encoded<T: Serialize>(value: &T) -> usize {
            bincode::serialized_size(value)
                .ok()
                .and_then(|size| usize::try_from(size).ok())
                .unwrap_or(usize::MAX)
        }
        let mut metadata = std::mem::size_of::<Self>()
            .saturating_add(encoded(&self.run_graph))
            .saturating_add(encoded(&self.artifacts))
            .saturating_add(encoded(&self.projects));
        let mut content = 0usize;
        for window in &self.windows {
            metadata = metadata
                .saturating_add(std::mem::size_of_val(window))
                .saturating_add(encoded(&window.layout))
                .saturating_add(window.name.as_ref().map_or(0, String::capacity));
            for pane in &window.panes {
                content = content.saturating_add(pane.content.retained_bytes());
                metadata = metadata
                    .saturating_add(std::mem::size_of_val(pane))
                    .saturating_add(pane.cwd.as_ref().map_or(0, String::capacity))
                    .saturating_add(encoded(&(
                        &pane.metadata,
                        &pane.launch_args,
                        &pane.agent_launch,
                    )));
            }
        }
        content.saturating_add(metadata.saturating_mul(4))
    }
}
