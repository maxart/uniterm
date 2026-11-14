//! Pure, indexed ownership graph for durable agent runs.
//!
//! The event log owns history. This module owns only the deterministic current
//! projection and its scalar indexes, so the server never scans every Pane to
//! answer one run relationship. See `docs/23-native-run-graph.md`.

use std::collections::BTreeMap;

use crate::{PaneId, ProjectId};

/// Closed leaf runs retained in the current projection before older subtrees
/// are left solely in the append-only event log.
pub const RUN_GRAPH_HISTORY_CAP: usize = 4096;

/// Stable identity for one agentic run within a Workspace.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct RunId(pub u64);

/// Stable identity for one role owned by a run.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct RoleId(pub u64);

/// Product-level run shape, independent of any provider implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    /// Deterministic multi-role workflow.
    Workflow,
    /// Turn-based role relay.
    Relay,
}

impl RunKind {
    /// Stable lowercase label for CLI and terminal presentation.
    pub fn label(self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::Relay => "relay",
        }
    }
}

/// Durable lifecycle state projected from append-only run events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Durable identity exists but no role owns an activation yet.
    Created,
    /// One role currently owns the live activation.
    Active,
    /// The run reached its success contract.
    Completed,
    /// The run ended without satisfying its contract.
    Failed,
    /// A human or Pane closure explicitly stopped the run.
    Canceled,
}

impl RunStatus {
    /// Stable lowercase label for CLI and terminal presentation.
    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    /// Whether no later activation may legally occur.
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

/// One role activation. The id changes on every activation, including a retry
/// of the same role, without exposing the orchestration completion token.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunActivation {
    /// Workspace-local public activation identity.
    pub id: u64,
    /// Pane that owns this role invocation.
    pub pane: PaneId,
    /// Provider registry identity used for this activation.
    pub provider: String,
    /// Whether this is the run's current live activation.
    pub active: bool,
}

/// One run node and its stable role ordering.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunRecord {
    /// Stable Workspace-local run identity.
    pub id: RunId,
    /// Delegating run, or `None` for a root run.
    pub parent: Option<RunId>,
    /// Project that owns all effects of this run.
    pub project: ProjectId,
    /// Provider-neutral orchestration shape.
    pub kind: RunKind,
    /// Durable Task that describes this run to humans.
    pub task_id: u64,
    /// Bounded human-readable goal or task title.
    pub title: String,
    /// Current lifecycle projection.
    pub status: RunStatus,
    /// Bounded terminal summary once the run closes.
    pub outcome: Option<String>,
    /// Stable roles in orchestration order.
    pub roles: Vec<RoleId>,
}

/// Provider and Pane ownership for one stable role.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoleRecord {
    /// Stable Workspace-local role identity.
    pub id: RoleId,
    /// Run that owns this role.
    pub run: RunId,
    /// Provider-neutral role name.
    pub name: String,
    /// Stable Pane reserved for this role.
    pub pane: PaneId,
    /// Provider registry identity selected for this role.
    pub provider: String,
    /// Latest public activation, whether live or closed.
    pub activation: Option<RunActivation>,
}

/// The durable vocabulary used by the event log to rebuild the graph.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunGraphEvent {
    /// Mint one root or child run before any role is declared.
    Created {
        run: RunId,
        parent: Option<RunId>,
        project: ProjectId,
        kind: RunKind,
        task_id: u64,
        title: String,
    },
    /// Bind one stable role to its Pane and provider.
    RoleDeclared {
        run: RunId,
        role: RoleId,
        name: String,
        pane: PaneId,
        provider: String,
    },
    /// Open or retry one role without changing role ownership.
    Activated {
        run: RunId,
        role: RoleId,
        activation: u64,
    },
    /// Move the live turn from one role to another.
    Handoff {
        run: RunId,
        from: RoleId,
        to: RoleId,
        activation: u64,
    },
    /// Close the run successfully.
    Completed { run: RunId, outcome: String },
    /// Close the run because its contract could not be satisfied.
    Failed { run: RunId, outcome: String },
    /// Close the run after an explicit stop or Pane removal.
    Canceled { run: RunId, outcome: String },
}

/// A rejected event means the durable stream does not describe a valid graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunGraphError(pub String);

impl std::fmt::Display for RunGraphError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RunGraphError {}

/// Current run projection plus direct relationship indexes.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunGraph {
    runs: BTreeMap<RunId, RunRecord>,
    roles: BTreeMap<RoleId, RoleRecord>,
    children: BTreeMap<RunId, Vec<RunId>>,
    run_panes: BTreeMap<RunId, Vec<PaneId>>,
    pane_active: BTreeMap<PaneId, (RunId, RoleId)>,
    project_runs: BTreeMap<ProjectId, Vec<RunId>>,
    task_runs: BTreeMap<u64, RunId>,
    next_run_id: u64,
    next_role_id: u64,
    next_activation_id: u64,
}

impl RunGraph {
    /// Create an empty projection with all monotonic allocators at one.
    pub fn new() -> Self {
        Self {
            next_run_id: 1,
            next_role_id: 1,
            next_activation_id: 1,
            ..Self::default()
        }
    }

    /// Identity the next valid creation event must carry.
    pub fn next_run_id(&self) -> RunId {
        RunId(self.next_run_id.max(1))
    }

    /// Identity the next valid role declaration must carry.
    pub fn next_role_id(&self) -> RoleId {
        RoleId(self.next_role_id.max(1))
    }

    /// Identity the next valid activation or handoff must carry.
    pub fn next_activation_id(&self) -> u64 {
        self.next_activation_id.max(1)
    }

    /// Iterate retained runs in stable identity order.
    pub fn runs(&self) -> impl Iterator<Item = &RunRecord> {
        self.runs.values()
    }

    /// Resolve one run directly by identity.
    pub fn run(&self, id: RunId) -> Option<&RunRecord> {
        self.runs.get(&id)
    }

    /// Resolve one role directly by identity.
    pub fn role(&self, id: RoleId) -> Option<&RoleRecord> {
        self.roles.get(&id)
    }

    /// Resolve the run owned by one durable Task.
    pub fn run_for_task(&self, task_id: u64) -> Option<RunId> {
        self.task_runs.get(&task_id).copied()
    }

    /// Resolve one role by its stable orchestration ordering.
    pub fn role_at(&self, run: RunId, index: usize) -> Option<RoleId> {
        self.runs.get(&run)?.roles.get(index).copied()
    }

    /// Resolve a run's parent without scanning other nodes.
    pub fn parent(&self, run: RunId) -> Option<RunId> {
        self.runs.get(&run).and_then(|record| record.parent)
    }

    /// Resolve retained child runs in creation order.
    pub fn children(&self, run: RunId) -> &[RunId] {
        self.children.get(&run).map_or(&[], Vec::as_slice)
    }

    /// Resolve every Pane reserved by a run.
    pub fn panes(&self, run: RunId) -> &[PaneId] {
        self.run_panes.get(&run).map_or(&[], Vec::as_slice)
    }

    /// Resolve the live run and role for one Pane.
    pub fn active_for_pane(&self, pane: PaneId) -> Option<(RunId, RoleId)> {
        self.pane_active.get(&pane).copied()
    }

    /// Resolve the role that currently owns a run's live turn.
    pub fn active_role(&self, run: RunId) -> Option<RoleId> {
        self.runs.get(&run)?.roles.iter().copied().find(|role| {
            self.roles
                .get(role)
                .and_then(|role| role.activation.as_ref())
                .is_some_and(|activation| activation.active)
        })
    }

    /// Resolve the latest public activation for one role.
    pub fn activation_for_role(&self, role: RoleId) -> Option<&RunActivation> {
        self.roles.get(&role)?.activation.as_ref()
    }

    /// Resolve retained runs owned by one Project.
    pub fn runs_for_project(&self, project: ProjectId) -> &[RunId] {
        self.project_runs.get(&project).map_or(&[], Vec::as_slice)
    }

    /// Apply one event in log order and update every scalar index.
    pub fn apply(&mut self, event: RunGraphEvent) -> Result<(), RunGraphError> {
        match event {
            RunGraphEvent::Created {
                run,
                parent,
                project,
                kind,
                task_id,
                title,
            } => {
                if run.0 != self.next_run_id().0 || self.runs.contains_key(&run) {
                    return Err(RunGraphError(format!("invalid or duplicate Run {}", run.0)));
                }
                if parent.is_some_and(|parent| !self.runs.contains_key(&parent)) {
                    return Err(RunGraphError("run parent does not exist".into()));
                }
                if self.task_runs.contains_key(&task_id) {
                    return Err(RunGraphError(format!("Task {task_id} already owns a run")));
                }
                self.runs.insert(
                    run,
                    RunRecord {
                        id: run,
                        parent,
                        project,
                        kind,
                        task_id,
                        title,
                        status: RunStatus::Created,
                        outcome: None,
                        roles: Vec::new(),
                    },
                );
                self.task_runs.insert(task_id, run);
                self.project_runs.entry(project).or_default().push(run);
                if let Some(parent) = parent {
                    self.children.entry(parent).or_default().push(run);
                }
                self.next_run_id = self.next_run_id.max(run.0.saturating_add(1));
            }
            RunGraphEvent::RoleDeclared {
                run,
                role,
                name,
                pane,
                provider,
            } => {
                let expected_role = self.next_role_id();
                let Some(node) = self.runs.get_mut(&run) else {
                    return Err(RunGraphError("role run does not exist".into()));
                };
                if node.status.terminal() || role != expected_role || self.roles.contains_key(&role)
                {
                    return Err(RunGraphError("invalid or duplicate run role".into()));
                }
                node.roles.push(role);
                self.roles.insert(
                    role,
                    RoleRecord {
                        id: role,
                        run,
                        name,
                        pane,
                        provider,
                        activation: None,
                    },
                );
                let panes = self.run_panes.entry(run).or_default();
                if !panes.contains(&pane) {
                    panes.push(pane);
                }
                self.next_role_id = self.next_role_id.max(role.0.saturating_add(1));
            }
            RunGraphEvent::Activated {
                run,
                role,
                activation,
            } => self.activate(run, role, activation, None)?,
            RunGraphEvent::Handoff {
                run,
                from,
                to,
                activation,
            } => self.activate(run, to, activation, Some(from))?,
            RunGraphEvent::Completed { run, outcome } => {
                self.finish(run, RunStatus::Completed, outcome)?
            }
            RunGraphEvent::Failed { run, outcome } => {
                self.finish(run, RunStatus::Failed, outcome)?
            }
            RunGraphEvent::Canceled { run, outcome } => {
                self.finish(run, RunStatus::Canceled, outcome)?
            }
        }
        Ok(())
    }

    fn activate(
        &mut self,
        run: RunId,
        role: RoleId,
        activation: u64,
        expected_from: Option<RoleId>,
    ) -> Result<(), RunGraphError> {
        if activation == 0 {
            return Err(RunGraphError("activation id must be nonzero".into()));
        }
        if activation != self.next_activation_id() {
            return Err(RunGraphError("activation id is not monotonic".into()));
        }
        let Some(node) = self.runs.get(&run) else {
            return Err(RunGraphError("activation run does not exist".into()));
        };
        if node.status.terminal() {
            return Err(RunGraphError("terminal run cannot activate a role".into()));
        }
        if self.roles.get(&role).is_none_or(|record| record.run != run) {
            return Err(RunGraphError(
                "activation role does not belong to run".into(),
            ));
        }
        if expected_from == Some(role) {
            return Err(RunGraphError(
                "handoff source and target are identical".into(),
            ));
        }
        let target_pane = self.roles.get(&role).expect("role was validated").pane;
        if self
            .pane_active
            .get(&target_pane)
            .is_some_and(|owner| owner.0 != run)
        {
            return Err(RunGraphError(
                "activation Pane is active in another run".into(),
            ));
        }
        let active = self.active_role(run);
        if expected_from.is_some_and(|from| active != Some(from)) {
            return Err(RunGraphError(
                "handoff source is not the active role".into(),
            ));
        }
        if let Some(previous) = active {
            let previous = self
                .roles
                .get_mut(&previous)
                .expect("active role is indexed");
            if let Some(previous_activation) = previous.activation.as_mut() {
                previous_activation.active = false;
                self.pane_active.remove(&previous_activation.pane);
            }
        }
        let target = self.roles.get_mut(&role).expect("role was validated");
        target.activation = Some(RunActivation {
            id: activation,
            pane: target.pane,
            provider: target.provider.clone(),
            active: true,
        });
        self.pane_active.insert(target.pane, (run, role));
        self.runs.get_mut(&run).expect("run was validated").status = RunStatus::Active;
        self.next_activation_id = self.next_activation_id.max(activation.saturating_add(1));
        Ok(())
    }

    fn finish(
        &mut self,
        run: RunId,
        status: RunStatus,
        outcome: String,
    ) -> Result<(), RunGraphError> {
        let Some(node) = self.runs.get(&run) else {
            return Err(RunGraphError("terminal event run does not exist".into()));
        };
        if node.status.terminal() {
            return Err(RunGraphError("run already has a terminal outcome".into()));
        }
        if let Some(role) = self.active_role(run) {
            let role = self.roles.get_mut(&role).expect("active role is indexed");
            if let Some(activation) = role.activation.as_mut() {
                activation.active = false;
                self.pane_active.remove(&activation.pane);
            }
        }
        let node = self.runs.get_mut(&run).expect("run was validated");
        node.status = status;
        node.outcome = Some(outcome);
        self.prune_to(RUN_GRAPH_HISTORY_CAP);
        Ok(())
    }

    fn prune_to(&mut self, cap: usize) {
        while self.runs.len() > cap {
            let candidate = self.runs.iter().find_map(|(id, run)| {
                (run.status.terminal() && self.children(*id).is_empty()).then_some(*id)
            });
            let Some(run_id) = candidate else {
                break;
            };
            let run = self.runs.remove(&run_id).expect("candidate exists");
            for role in run.roles {
                self.roles.remove(&role);
            }
            self.run_panes.remove(&run_id);
            self.task_runs.remove(&run.task_id);
            if let Some(runs) = self.project_runs.get_mut(&run.project) {
                runs.retain(|id| *id != run_id);
                if runs.is_empty() {
                    self.project_runs.remove(&run.project);
                }
            }
            self.children.remove(&run_id);
            if let Some(parent) = run.parent {
                if let Some(children) = self.children.get_mut(&parent) {
                    children.retain(|id| *id != run_id);
                    if children.is_empty() {
                        self.children.remove(&parent);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(graph: &mut RunGraph, run: u64, parent: Option<u64>, project: u64, task: u64) {
        graph
            .apply(RunGraphEvent::Created {
                run: RunId(run),
                parent: parent.map(RunId),
                project: ProjectId(project),
                kind: RunKind::Workflow,
                task_id: task,
                title: format!("run {run}"),
            })
            .unwrap();
    }

    #[test]
    fn indexes_parent_project_roles_panes_and_activations() {
        let mut graph = RunGraph::new();
        create(&mut graph, 1, None, 7, 10);
        create(&mut graph, 2, Some(1), 7, 11);
        for (id, name, pane) in [(1, "builder", 20), (2, "reviewer", 21)] {
            graph
                .apply(RunGraphEvent::RoleDeclared {
                    run: RunId(2),
                    role: RoleId(id),
                    name: name.into(),
                    pane: PaneId(pane),
                    provider: "test-provider".into(),
                })
                .unwrap();
        }
        graph
            .apply(RunGraphEvent::Activated {
                run: RunId(2),
                role: RoleId(1),
                activation: 1,
            })
            .unwrap();
        graph
            .apply(RunGraphEvent::Handoff {
                run: RunId(2),
                from: RoleId(1),
                to: RoleId(2),
                activation: 2,
            })
            .unwrap();

        assert_eq!(graph.parent(RunId(2)), Some(RunId(1)));
        assert_eq!(graph.children(RunId(1)), &[RunId(2)]);
        assert_eq!(graph.runs_for_project(ProjectId(7)), &[RunId(1), RunId(2)]);
        assert_eq!(graph.panes(RunId(2)), &[PaneId(20), PaneId(21)]);
        assert_eq!(graph.active_for_pane(PaneId(20)), None);
        assert_eq!(
            graph.active_for_pane(PaneId(21)),
            Some((RunId(2), RoleId(2)))
        );
        assert_eq!(
            graph.activation_for_role(RoleId(1)),
            Some(&RunActivation {
                id: 1,
                pane: PaneId(20),
                provider: "test-provider".into(),
                active: false,
            })
        );
    }

    #[test]
    fn terminal_event_releases_active_pane_and_cannot_repeat() {
        let mut graph = RunGraph::new();
        create(&mut graph, 1, None, 1, 9);
        graph
            .apply(RunGraphEvent::RoleDeclared {
                run: RunId(1),
                role: RoleId(1),
                name: "worker".into(),
                pane: PaneId(3),
                provider: "provider".into(),
            })
            .unwrap();
        graph
            .apply(RunGraphEvent::Activated {
                run: RunId(1),
                role: RoleId(1),
                activation: 1,
            })
            .unwrap();
        graph
            .apply(RunGraphEvent::Canceled {
                run: RunId(1),
                outcome: "stopped".into(),
            })
            .unwrap();

        assert_eq!(graph.run(RunId(1)).unwrap().status, RunStatus::Canceled);
        assert_eq!(graph.active_for_pane(PaneId(3)), None);
        assert!(graph
            .apply(RunGraphEvent::Failed {
                run: RunId(1),
                outcome: "late".into(),
            })
            .is_err());
    }

    #[test]
    fn rejects_missing_parent_and_cross_run_handoff() {
        let mut graph = RunGraph::new();
        assert!(graph
            .apply(RunGraphEvent::Created {
                run: RunId(2),
                parent: Some(RunId(1)),
                project: ProjectId(1),
                kind: RunKind::Relay,
                task_id: 2,
                title: "child".into(),
            })
            .is_err());
        create(&mut graph, 1, None, 1, 1);
        assert_eq!(graph.next_run_id(), RunId(2));
        assert_eq!(graph.next_role_id(), RoleId(1));
        assert_eq!(graph.next_activation_id(), 1);
    }

    #[test]
    fn pruning_removes_old_terminal_leaves_but_keeps_allocators_monotonic() {
        let mut graph = RunGraph::new();
        for id in 1..=4 {
            create(&mut graph, id, None, 1, id);
            graph
                .apply(RunGraphEvent::Completed {
                    run: RunId(id),
                    outcome: "done".into(),
                })
                .unwrap();
        }
        graph.prune_to(2);
        assert!(graph.run(RunId(1)).is_none());
        assert!(graph.run(RunId(2)).is_none());
        assert!(graph.run(RunId(3)).is_some());
        assert!(graph.run(RunId(4)).is_some());
        assert_eq!(graph.runs_for_project(ProjectId(1)), &[RunId(3), RunId(4)]);
        assert_eq!(graph.next_run_id(), RunId(5));
    }

    #[test]
    fn one_pane_cannot_be_active_in_two_runs() {
        let mut graph = RunGraph::new();
        create(&mut graph, 1, None, 1, 1);
        create(&mut graph, 2, None, 1, 2);
        for (run, role) in [(1, 1), (2, 2)] {
            graph
                .apply(RunGraphEvent::RoleDeclared {
                    run: RunId(run),
                    role: RoleId(role),
                    name: "worker".into(),
                    pane: PaneId(9),
                    provider: "provider".into(),
                })
                .unwrap();
        }
        graph
            .apply(RunGraphEvent::Activated {
                run: RunId(1),
                role: RoleId(1),
                activation: 1,
            })
            .unwrap();
        assert!(graph
            .apply(RunGraphEvent::Activated {
                run: RunId(2),
                role: RoleId(2),
                activation: 2,
            })
            .is_err());
        assert_eq!(
            graph.active_for_pane(PaneId(9)),
            Some((RunId(1), RoleId(1)))
        );
        assert_eq!(graph.next_activation_id(), 2);
    }
}
