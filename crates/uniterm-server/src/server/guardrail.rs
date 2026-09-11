//! Native Workspace automation policy at the mio ownership boundary.
//!
//! Pure decisions live in `uniterm-core`. This module resolves exact Project
//! selectors and captures only facts already owned by the server before any
//! orchestration Pane is spawned.

use super::*;

pub(super) struct GuardedLaunch {
    pub(super) project: ProjectId,
    pub(super) project_name: String,
    pub(super) project_root: PathBuf,
    pub(super) limits: uniterm_core::GuardLimits,
    pub(super) started_at_ms: u64,
}

pub(super) fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub(super) fn elapsed_seconds(started_at_ms: u64, now_ms: u64) -> u64 {
    now_ms.saturating_sub(started_at_ms) / 1_000
}

pub(super) fn elapsed_deadline(
    started_at_ms: u64,
    limits: uniterm_core::GuardLimits,
    already_triggered: bool,
) -> Option<std::time::Instant> {
    if already_triggered {
        return None;
    }
    let now_ms = unix_time_ms();
    let elapsed_ms = now_ms.saturating_sub(started_at_ms);
    let limit_ms = limits.max_elapsed_seconds.saturating_mul(1_000);
    let remaining_ms = limit_ms.saturating_sub(elapsed_ms);
    Some(std::time::Instant::now() + std::time::Duration::from_millis(remaining_ms))
}

impl Server {
    pub(super) fn prepare_orchestration_launch(
        &mut self,
        selector: Option<&str>,
        kind: uniterm_core::RunKind,
        requested_roles: usize,
    ) -> Result<GuardedLaunch, String> {
        let action = uniterm_core::GuardAction::OrchestrationLaunch {
            kind,
            requested_roles: u16::try_from(requested_roles).unwrap_or(u16::MAX),
        };
        let target = match selector {
            Some(selector) => self
                .projects
                .iter()
                .find(|project| project.name == selector || project.root == selector),
            None => self
                .projects
                .iter()
                .find(|project| project.id == self.active_project),
        }
        .map(|project| {
            (
                project.id,
                project.name.clone(),
                PathBuf::from(&project.root),
            )
        });
        let Some((project, project_name, project_root)) = target else {
            let reason = selector
                .map(|selector| format!("unknown Project '{selector}' in this Workspace"))
                .unwrap_or_else(|| "the active Project is not owned by this Workspace".into());
            self.record_guardrail(uniterm_core::GuardrailRecord {
                project: None,
                run: None,
                action,
                decision: uniterm_core::GuardDecision::Deny {
                    reason: reason.clone(),
                },
            });
            return Err(reason);
        };

        let configured = &self.config.guardrail_allowed_projects;
        let allowed_projects = self
            .projects
            .iter()
            .filter(|candidate| {
                configured.iter().any(|selector| {
                    selector == &candidate.name
                        || selector == &candidate.root
                        || Server::worktree_registration(candidate)
                            .is_some_and(|worktree| selector == &worktree.repository)
                })
            })
            .map(|project| project.id)
            .collect();
        let limits = self.config.guardrails;
        let policy = uniterm_core::GuardPolicy {
            limits,
            projects_restricted: !configured.is_empty(),
            allowed_projects,
        };
        let active_runs = self.workflows.len().saturating_add(self.relays.len());
        let reserved_role_panes = self
            .workflows
            .iter()
            .map(|run| run.role_panes.len())
            .chain(self.relays.iter().map(|run| run.role_panes.len()))
            .fold(0usize, usize::saturating_add);
        let decision = uniterm_core::evaluate_launch(
            &policy,
            uniterm_core::LaunchFacts {
                project,
                kind,
                requested_roles: u16::try_from(requested_roles).unwrap_or(u16::MAX),
                active_runs: u16::try_from(active_runs).unwrap_or(u16::MAX),
                reserved_role_panes: u16::try_from(reserved_role_panes).unwrap_or(u16::MAX),
            },
        );
        self.record_guardrail(uniterm_core::GuardrailRecord {
            project: Some(project),
            run: None,
            action,
            decision: decision.clone(),
        });
        match decision {
            uniterm_core::GuardDecision::Allow => Ok(GuardedLaunch {
                project,
                project_name,
                project_root,
                limits,
                started_at_ms: unix_time_ms(),
            }),
            uniterm_core::GuardDecision::Ask { reason }
            | uniterm_core::GuardDecision::Deny { reason } => Err(reason),
        }
    }

    pub(super) fn record_guardrail(&mut self, record: uniterm_core::GuardrailRecord) {
        self.append_event(crate::eventlog::LogEvent::GuardrailDecision { record });
    }

    /// Decide one Uniterm-owned destructive or bulk command, append the
    /// decision before any side effect, and say whether it may proceed.
    ///
    /// `confirmed` is the human's explicit confirmation carried on the wire
    /// (a client confirm step, an explicit CLI command, or `confirmed: true`
    /// in a control request). Nothing here guesses it.
    pub(super) fn guard_semantic(
        &mut self,
        command: uniterm_core::GuardedCommand,
        confirmed: bool,
        project: Option<ProjectId>,
    ) -> bool {
        let decision = uniterm_core::evaluate_semantic(command, confirmed);
        self.record_guardrail(uniterm_core::GuardrailRecord {
            project,
            run: None,
            action: uniterm_core::GuardAction::SemanticCommand { command, confirmed },
            decision: decision.clone(),
        });
        decision == uniterm_core::GuardDecision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_deadline_is_event_armed_and_triggered_state_is_a_no_op() {
        let limits = uniterm_core::GuardLimits {
            max_elapsed_seconds: 10,
            ..uniterm_core::GuardLimits::default()
        };
        let now = unix_time_ms();
        let due = elapsed_deadline(now.saturating_sub(9_000), limits, false).unwrap();
        assert!(due > std::time::Instant::now());
        assert!(elapsed_deadline(now, limits, true).is_none());
    }
}
