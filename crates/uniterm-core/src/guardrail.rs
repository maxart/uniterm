//! Pure policy for actions Uniterm owns.
//!
//! This is not a sandbox for provider tool calls. It evaluates bounded facts
//! already owned by Uniterm so every launch, cap, and confirmation path shares
//! one provider-neutral decision vocabulary without I/O, async work, or UI.

use crate::{ProjectId, RunId, RunKind};

/// Hard parser and projection bounds for Workspace policy values.
pub const GUARDRAIL_MAX_ACTIVE_RUNS: u16 = 64;
pub const GUARDRAIL_MAX_ROLE_PANES: u16 = 256;
pub const GUARDRAIL_MAX_ITERATIONS: u32 = 100;
pub const GUARDRAIL_MAX_ELAPSED_SECONDS: u64 = 7 * 24 * 60 * 60;
pub const GUARDRAIL_MAX_PROJECT_SELECTORS: usize = 64;

/// Limits captured by a run at launch so config reload cannot rewrite its
/// contract halfway through an activation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GuardLimits {
    /// Maximum concurrently active workflows and relays in one Workspace.
    pub max_active_runs: u16,
    /// Maximum role Panes reserved by active workflows and relays.
    pub max_role_panes: u16,
    /// Maximum workflow or relay turns before the pure state machine stops.
    pub max_iterations: u32,
    /// Wall-clock boundary that pauses an active run for human direction.
    pub max_elapsed_seconds: u64,
}

impl Default for GuardLimits {
    fn default() -> Self {
        Self {
            max_active_runs: 8,
            max_role_panes: 16,
            max_iterations: 3,
            max_elapsed_seconds: 2 * 60 * 60,
        }
    }
}

impl GuardLimits {
    /// Reject impossible durable values rather than silently changing a run's
    /// policy during recovery.
    pub fn validate(self) -> Result<Self, String> {
        if !(1..=GUARDRAIL_MAX_ACTIVE_RUNS).contains(&self.max_active_runs) {
            return Err("max active runs is outside its bounded range".into());
        }
        if !(1..=GUARDRAIL_MAX_ROLE_PANES).contains(&self.max_role_panes) {
            return Err("max role Panes is outside its bounded range".into());
        }
        if !(1..=GUARDRAIL_MAX_ITERATIONS).contains(&self.max_iterations) {
            return Err("max iterations is outside its bounded range".into());
        }
        if !(1..=GUARDRAIL_MAX_ELAPSED_SECONDS).contains(&self.max_elapsed_seconds) {
            return Err("max elapsed seconds is outside its bounded range".into());
        }
        Ok(self)
    }
}

/// Workspace policy after human selectors have been resolved to stable
/// Project identities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardPolicy {
    /// Bounded capacity and duration limits for this evaluation.
    pub limits: GuardLimits,
    /// Whether the Workspace config contains an allowed-Project restriction.
    pub projects_restricted: bool,
    /// Stable identities matched by the configured exact selectors.
    pub allowed_projects: Vec<ProjectId>,
}

/// Aggregate ownership facts needed before a run creates any Panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaunchFacts {
    /// Stable target Project owned by the current Workspace.
    pub project: ProjectId,
    /// Native run type being launched.
    pub kind: RunKind,
    /// Complete role set that will be reserved before the first Pane spawns.
    pub requested_roles: u16,
    /// Workflows and relays currently in a non-terminal phase.
    pub active_runs: u16,
    /// Role Panes already reserved by active runs.
    pub reserved_role_panes: u16,
}

/// Uniterm-owned commands whose unattended form needs an explicit decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardedCommand {
    /// Create an ordinary Pane under an already resolved Project.
    PaneCreate,
    /// Capture a relay checkpoint before a builder activation.
    CheckpointCreate,
    /// Restore a relay checkpoint and discard later working-tree changes.
    CheckpointRollback,
    /// Resolve more than one Workspace waiting item in one operation.
    BulkWaitingAction,
    /// Stop every running agent in a scope by closing its Pane.
    AgentsStopAll,
    /// Remove a Project and every Pane it owns from the Workspace.
    ProjectRemove,
    /// Stop the Workspace server and all owned child processes.
    WorkspaceStop,
}

/// One auditable policy evaluation target.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuardAction {
    /// Reserve a complete native orchestration role set.
    OrchestrationLaunch {
        /// Native run type being launched.
        kind: RunKind,
        /// Number of role Panes reserved atomically.
        requested_roles: u16,
    },
    /// Pause an active run at its captured wall-clock boundary.
    ElapsedLimit {
        /// Observed wall-clock duration at evaluation time.
        elapsed_seconds: u64,
        /// Captured duration limit for the run.
        limit_seconds: u64,
    },
    /// Evaluate one shared Uniterm semantic command.
    SemanticCommand {
        /// Semantic operation being evaluated.
        command: GuardedCommand,
        /// Whether the human already confirmed this exact operation.
        confirmed: bool,
    },
}

/// Pure outcome. `Ask` means the server must pause into an existing human
/// decision surface instead of guessing consent.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum GuardDecision {
    /// The action may proceed without another interaction.
    Allow,
    /// The action must enter an explicit human decision surface.
    Ask {
        /// Human-readable explanation placed in the decision surface.
        reason: String,
    },
    /// The action is outside the Workspace's configured contract.
    Deny {
        /// Human-readable refusal returned through the shared command path.
        reason: String,
    },
}

/// Durable audit fact carried by the Workspace event envelope.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GuardrailRecord {
    /// Stable Project scope, when the action has a Project owner.
    pub project: Option<ProjectId>,
    /// Stable run scope, when the action belongs to an existing run.
    pub run: Option<RunId>,
    /// Evaluated action and the facts captured in it.
    pub action: GuardAction,
    /// Pure decision recorded before the corresponding side effect.
    pub decision: GuardDecision,
}

/// Decide whether a native orchestration may reserve its complete role set.
pub fn evaluate_launch(policy: &GuardPolicy, facts: LaunchFacts) -> GuardDecision {
    if let Err(reason) = policy.limits.validate() {
        return GuardDecision::Deny { reason };
    }
    if facts.project.0 == 0 || facts.requested_roles == 0 {
        return GuardDecision::Deny {
            reason: "launch requires an owned Project and at least one role".into(),
        };
    }
    if policy.projects_restricted && !policy.allowed_projects.contains(&facts.project) {
        return GuardDecision::Deny {
            reason: format!(
                "Project {} is not allowed by Workspace policy",
                facts.project.0
            ),
        };
    }
    if facts.active_runs >= policy.limits.max_active_runs {
        return GuardDecision::Deny {
            reason: format!("active run cap {} reached", policy.limits.max_active_runs),
        };
    }
    if facts
        .reserved_role_panes
        .saturating_add(facts.requested_roles)
        > policy.limits.max_role_panes
    {
        return GuardDecision::Deny {
            reason: format!(
                "role Pane cap {} would be exceeded",
                policy.limits.max_role_panes
            ),
        };
    }
    GuardDecision::Allow
}

/// Ask at the exact armed elapsed boundary and remain a no-op beforehand.
pub fn evaluate_elapsed(elapsed_seconds: u64, limit_seconds: u64) -> GuardDecision {
    if elapsed_seconds < limit_seconds {
        GuardDecision::Allow
    } else {
        GuardDecision::Ask {
            reason: format!("run elapsed-time cap of {limit_seconds} seconds reached"),
        }
    }
}

/// Require explicit confirmation only for destructive or bulk Uniterm-owned
/// actions. Ordinary Pane and checkpoint creation remains governed by launch
/// ownership and capacity policy.
pub fn evaluate_semantic(command: GuardedCommand, confirmed: bool) -> GuardDecision {
    let confirmation_required = matches!(
        command,
        GuardedCommand::CheckpointRollback
            | GuardedCommand::BulkWaitingAction
            | GuardedCommand::AgentsStopAll
            | GuardedCommand::ProjectRemove
            | GuardedCommand::WorkspaceStop
    );
    if confirmation_required && !confirmed {
        GuardDecision::Ask {
            reason: format!("{} requires explicit confirmation", command.label()),
        }
    } else {
        GuardDecision::Allow
    }
}

impl GuardedCommand {
    fn label(self) -> &'static str {
        match self {
            Self::PaneCreate => "Pane creation",
            Self::CheckpointCreate => "checkpoint creation",
            Self::CheckpointRollback => "checkpoint rollback",
            Self::BulkWaitingAction => "bulk waiting action",
            Self::AgentsStopAll => "bulk agent stop",
            Self::ProjectRemove => "Project removal",
            Self::WorkspaceStop => "Workspace stop",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> GuardPolicy {
        GuardPolicy {
            limits: GuardLimits {
                max_active_runs: 2,
                max_role_panes: 4,
                max_iterations: 3,
                max_elapsed_seconds: 60,
            },
            allowed_projects: vec![ProjectId(7)],
            projects_restricted: true,
        }
    }

    fn launch() -> LaunchFacts {
        LaunchFacts {
            project: ProjectId(7),
            kind: RunKind::Workflow,
            requested_roles: 2,
            active_runs: 0,
            reserved_role_panes: 0,
        }
    }

    #[test]
    fn launch_allows_owned_capacity_and_denies_each_boundary() {
        assert_eq!(evaluate_launch(&policy(), launch()), GuardDecision::Allow);

        let mut facts = launch();
        facts.project = ProjectId(8);
        assert!(matches!(
            evaluate_launch(&policy(), facts),
            GuardDecision::Deny { reason } if reason.contains("not allowed")
        ));

        let mut facts = launch();
        facts.active_runs = 2;
        assert!(matches!(
            evaluate_launch(&policy(), facts),
            GuardDecision::Deny { reason } if reason.contains("active run cap")
        ));

        let mut facts = launch();
        facts.reserved_role_panes = 3;
        assert!(matches!(
            evaluate_launch(&policy(), facts),
            GuardDecision::Deny { reason } if reason.contains("role Pane cap")
        ));
    }

    #[test]
    fn unrestricted_policy_allows_any_workspace_owned_project() {
        let mut policy = policy();
        policy.projects_restricted = false;
        policy.allowed_projects.clear();
        let mut facts = launch();
        facts.project = ProjectId(99);
        assert_eq!(evaluate_launch(&policy, facts), GuardDecision::Allow);
    }

    #[test]
    fn restricted_policy_with_no_selector_matches_denies_every_project() {
        let mut policy = policy();
        policy.allowed_projects.clear();
        assert!(matches!(
            evaluate_launch(&policy, launch()),
            GuardDecision::Deny { reason } if reason.contains("not allowed")
        ));
    }

    #[test]
    fn elapsed_and_semantic_asks_are_exact_and_confirmation_is_a_no_op() {
        assert_eq!(evaluate_elapsed(59, 60), GuardDecision::Allow);
        assert!(matches!(
            evaluate_elapsed(60, 60),
            GuardDecision::Ask { .. }
        ));
        assert!(matches!(
            evaluate_semantic(GuardedCommand::CheckpointRollback, false),
            GuardDecision::Ask { .. }
        ));
        assert_eq!(
            evaluate_semantic(GuardedCommand::CheckpointRollback, true),
            GuardDecision::Allow
        );
        assert_eq!(
            evaluate_semantic(GuardedCommand::PaneCreate, false),
            GuardDecision::Allow
        );
    }

    #[test]
    fn destructive_and_bulk_commands_ask_until_confirmed() {
        for command in [
            GuardedCommand::AgentsStopAll,
            GuardedCommand::ProjectRemove,
            GuardedCommand::WorkspaceStop,
            GuardedCommand::BulkWaitingAction,
        ] {
            assert!(matches!(
                evaluate_semantic(command, false),
                GuardDecision::Ask { .. }
            ));
            assert_eq!(evaluate_semantic(command, true), GuardDecision::Allow);
        }
        assert_eq!(
            evaluate_semantic(GuardedCommand::CheckpointCreate, false),
            GuardDecision::Allow
        );
    }
}
