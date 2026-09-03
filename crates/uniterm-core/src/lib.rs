//! `uniterm-core` - the pure model and pure decision logic for Uniterm.
//!
//! This crate is deliberately free of any UI, async runtime, or I/O dependency.
//! It holds the terminal grid model, the agent-state model, and (as they land)
//! the pure workflow/relay decision brains. Everything here is synchronous,
//! deterministic, and unit-testable in isolation. See `docs/03-system-architecture.md`.

pub mod agent;
pub mod artifact;
pub mod config;
pub mod grid;
pub mod guardrail;
pub mod instruction;
pub mod layout;
pub mod menu;
pub mod orchestrate;
pub mod run_graph;
pub mod tasks;
pub mod waiting;

pub use agent::AgentStatus;
pub use artifact::{
    ArtifactError, ArtifactEvent, ArtifactId, ArtifactKind, ArtifactLedger, ArtifactRecord,
    ArtifactStatus, ARTIFACT_LEDGER_CAP, ARTIFACT_PATH_MAX_BYTES,
};
pub use config::{
    Config, ConfigDiagnostic, EditorRule, KeyBinding, NotificationDelivery, StatusPosition, Theme,
    ThemePreset,
};
pub use grid::{Attrs, Cell, Color, Grid, StoredCell, StoredLine, UnderlineStyle};
pub use guardrail::{
    evaluate_elapsed, evaluate_launch, evaluate_semantic, GuardAction, GuardDecision, GuardLimits,
    GuardPolicy, GuardedCommand, GuardrailRecord, LaunchFacts, GUARDRAIL_MAX_ACTIVE_RUNS,
    GUARDRAIL_MAX_ELAPSED_SECONDS, GUARDRAIL_MAX_ITERATIONS, GUARDRAIL_MAX_PROJECT_SELECTORS,
    GUARDRAIL_MAX_ROLE_PANES,
};
pub use instruction::{
    InstructionAuthor, InstructionBoundary, InstructionCancellation, InstructionItem,
    InstructionPolicy, InstructionQueue, InstructionState,
};
pub use layout::{Direction, Divider, LayoutNode, Rect, SplitDir};
pub use run_graph::{
    RoleId, RoleRecord, RunActivation, RunGraph, RunGraphError, RunGraphEvent, RunId, RunKind,
    RunRecord, RunStatus, RUN_GRAPH_HISTORY_CAP,
};
pub use tasks::{Task, TaskList, TaskStatus};
pub use waiting::{WaitingChange, WaitingItem, WaitingKind, WaitingQueue, WaitingResolution};

/// Opaque pane identifier. A newtype so panes and windows can't be confused,
/// and so the layout tree, the server's pane map, and the protocol all agree.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct PaneId(pub u64);

/// Opaque project identifier. Projects are the durable ownership boundary
/// between a Workspace and its Tabs, so agent and bulk-action scopes carry
/// this id instead of relying on a mutable tab index.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct ProjectId(pub u64);
