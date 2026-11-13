//! Bounded, metadata-only facts shared with the Workspace assistant.
use serde::{Deserialize, Serialize};
use uniterm_core::{ArtifactId, PaneId, ProjectId, RoleId, RunId};

/// One typed fact. Prompts, notes, terminal evidence, transcripts and turn tokens
/// are deliberately excluded from this read-only projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fact {
    /// Project identity and location, including the current selection.
    Project {
        id: ProjectId,
        name: String,
        root: String,
        active: bool,
    },
    /// A Tab is addressed through its current anchor Pane at this event cursor.
    Tab {
        anchor: PaneId,
        project: ProjectId,
        name: String,
        parent: Option<PaneId>,
        active: bool,
    },
    /// Terminal availability without its private screen or command arguments.
    Pane {
        id: PaneId,
        project: ProjectId,
        tab: PaneId,
        cwd: String,
        recovery: bool,
    },
    /// Provider-neutral current agent facts, scoped to this invocation.
    Agent {
        pane: PaneId,
        project: ProjectId,
        provider: String,
        status: uniterm_core::AgentStatus,
        invocation: Option<u64>,
        run: Option<RunId>,
        role: Option<RoleId>,
    },
    /// Task summary excludes its prompt and private notes.
    Task {
        id: u64,
        title: String,
        status: uniterm_core::TaskStatus,
        project: Option<ProjectId>,
        pane: Option<PaneId>,
        source: Option<u64>,
        updated: u64,
    },
    /// Run ownership and recorded outcome status, without private summaries.
    Run {
        id: RunId,
        project: ProjectId,
        parent: Option<RunId>,
        task: u64,
        title: String,
        status: uniterm_core::RunStatus,
        shape: uniterm_core::RunKind,
    },
    /// Public activation identity is not a private completion token.
    Role {
        id: RoleId,
        run: RunId,
        pane: PaneId,
        name: String,
        provider: String,
        activation: Option<u64>,
        active: bool,
    },
    /// Artifact metadata; reading the contents is a separate private action.
    Artifact {
        id: ArtifactId,
        project: ProjectId,
        run: RunId,
        role: RoleId,
        path: String,
        status: uniterm_core::ArtifactStatus,
        artifact_kind: uniterm_core::ArtifactKind,
        size: u64,
        digest: String,
    },
    /// An agent needs attention; the actual question/tool text remains private.
    Waiting {
        id: u64,
        pane: PaneId,
        waiting_kind: uniterm_core::WaitingKind,
    },
}

/// Exact Project cleanup target, including the number of Panes that will close.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PruneProject {
    pub id: uniterm_core::ProjectId,
    pub name: String,
    pub panes: usize,
    pub reason: String,
}

/// A scoped preview is re-derived at the same event cursor before applying.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PruneReport {
    pub workspace: String,
    pub date: String,
    pub sequence: u64,
    pub basis: String,
    pub candidates: Vec<PruneProject>,
    pub retained: Vec<PruneProject>,
    pub applied: bool,
}
