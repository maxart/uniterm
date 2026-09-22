//! Live workflow and relay ownership for the mio server.
//!
//! Pure transitions remain in `uniterm-core`; this module owns only the
//! event-driven runtime state and its typed Tokio-side requests.

use super::*;

pub(super) const ORCHESTRATION_STALL_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10 * 60);
pub(super) const ARTIFACT_VALIDATION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// One live workflow: the pure engine state, the template it runs, and the
/// role-to-pane binding. The agent CLI in each role pane is started lazily on
/// the role's first turn (with the prompt as its argument); later turns paste
/// into the running agent.
pub(super) struct ActiveWorkflow {
    pub(super) state: uniterm_core::orchestrate::State,
    pub(super) template: &'static uniterm_core::orchestrate::WorkflowTemplate,
    pub(super) goal: String,
    /// Provider ownership aligned with `state.roles` and `role_panes`.
    pub(super) role_providers: Vec<crate::workflow::ResolvedRoleProvider>,
    pub(super) role_panes: Vec<PaneId>,
    pub(super) started: Vec<bool>,
    pub(super) task_id: u64,
    pub(super) started_at_ms: u64,
    pub(super) guard_limits: uniterm_core::GuardLimits,
    pub(super) elapsed_guard_triggered: bool,
    pub(super) elapsed_deadline: Option<std::time::Instant>,
    pub(super) stall_deadline: Option<std::time::Instant>,
    pub(super) idle_deadline: Option<std::time::Instant>,
}

/// One live two-role relay driven by the same pure completion engine.
pub(super) struct ActiveRelay {
    pub(super) state: uniterm_core::orchestrate::State,
    pub(super) goal: String,
    pub(super) role_providers: Vec<crate::workflow::ResolvedRoleProvider>,
    pub(super) role_panes: Vec<PaneId>,
    pub(super) started: Vec<bool>,
    pub(super) task_id: u64,
    pub(super) checkpoints: Vec<(u64, String)>,
    pub(super) started_at_ms: u64,
    pub(super) guard_limits: uniterm_core::GuardLimits,
    pub(super) elapsed_guard_triggered: bool,
    pub(super) elapsed_deadline: Option<std::time::Instant>,
    pub(super) stall_deadline: Option<std::time::Instant>,
    pub(super) idle_deadline: Option<std::time::Instant>,
}

pub(super) struct PendingOrchestrationSubmission {
    pub(super) kind: uniterm_proto::OrchestrationKind,
    pub(super) task_id: u64,
    pub(super) token: u64,
    pub(super) pane: PaneId,
    pub(super) status: uniterm_proto::SubmissionStatus,
    pub(super) verdict: Option<String>,
    pub(super) summary: String,
    pub(super) due: std::time::Instant,
}

pub(super) struct PendingPromptDelivery {
    pub(super) kind: uniterm_proto::OrchestrationKind,
    pub(super) task_id: u64,
    pub(super) token: u64,
    pub(super) pane: PaneId,
    pub(super) bytes: Vec<u8>,
    pub(super) first_delivery: bool,
    pub(super) agent_id: String,
    pub(super) attempts: u8,
    pub(super) due: std::time::Instant,
}

pub(super) struct PendingRelayActivation {
    pub(super) task_id: u64,
    pub(super) role: usize,
    pub(super) token: u64,
    pub(super) handoff: Option<String>,
}

#[derive(Clone, Copy)]
pub(super) struct OrchestrationTarget<'a> {
    pub(super) project: Option<&'a str>,
    pub(super) parent: Option<uniterm_core::RunId>,
}

impl ActiveWorkflow {
    pub(super) fn durable(&self) -> crate::eventlog::DurableOrchestration {
        crate::eventlog::DurableOrchestration {
            kind: uniterm_proto::OrchestrationKind::Workflow,
            task_id: self.task_id,
            template: Some(self.template.name.to_string()),
            goal: self.goal.clone(),
            role_providers: self
                .role_providers
                .iter()
                .map(|provider| crate::eventlog::DurableRoleProvider {
                    provider: provider.id.clone(),
                    command: provider.command.clone(),
                })
                .collect(),
            agent_id: self
                .role_providers
                .first()
                .map(|provider| provider.id.clone())
                .unwrap_or_default(),
            agent_cmd: self
                .role_providers
                .first()
                .map(|provider| provider.command.clone())
                .unwrap_or_default(),
            role_panes: self.role_panes.clone(),
            started: self.started.clone(),
            state: self.state.clone(),
            checkpoints: Vec::new(),
            guardrail: Box::new(crate::eventlog::DurableGuardrail {
                started_at_ms: self.started_at_ms,
                limits: self.guard_limits,
                elapsed_triggered: self.elapsed_guard_triggered,
            }),
        }
    }
}

impl ActiveRelay {
    pub(super) fn durable(&self) -> crate::eventlog::DurableOrchestration {
        crate::eventlog::DurableOrchestration {
            kind: uniterm_proto::OrchestrationKind::Relay,
            task_id: self.task_id,
            template: None,
            goal: self.goal.clone(),
            role_providers: self
                .role_providers
                .iter()
                .map(|provider| crate::eventlog::DurableRoleProvider {
                    provider: provider.id.clone(),
                    command: provider.command.clone(),
                })
                .collect(),
            agent_id: self
                .role_providers
                .first()
                .map(|provider| provider.id.clone())
                .unwrap_or_default(),
            agent_cmd: self
                .role_providers
                .first()
                .map(|provider| provider.command.clone())
                .unwrap_or_default(),
            role_panes: self.role_panes.clone(),
            started: self.started.clone(),
            state: self.state.clone(),
            checkpoints: self.checkpoints.clone(),
            guardrail: Box::new(crate::eventlog::DurableGuardrail {
                started_at_ms: self.started_at_ms,
                limits: self.guard_limits,
                elapsed_triggered: self.elapsed_guard_triggered,
            }),
        }
    }
}

impl Server {
    /// Validate, append, then publish one run-graph transition. Validation is
    /// performed against a clone so an impossible server transition can never
    /// enter the authoritative event stream.
    pub(super) fn record_run_graph_change(&mut self, change: uniterm_core::RunGraphEvent) {
        let mut next = self.run_graph.clone();
        if let Err(error) = next.apply(change.clone()) {
            self.durability_error = Some(format!("run graph projection rejected: {error}"));
            return;
        }
        self.append_event(crate::eventlog::LogEvent::RunGraph { change });
        self.run_graph = next;
        self.run_graph_sequence = self.log.current_sequence();
    }

    fn create_orchestration_run(
        &mut self,
        parent: Option<uniterm_core::RunId>,
        kind: uniterm_core::RunKind,
        task_id: u64,
        title: &str,
        project: ProjectId,
        roles: &[(String, PaneId, String)],
    ) -> uniterm_core::RunId {
        let run = self.run_graph.next_run_id();
        self.record_run_graph_change(uniterm_core::RunGraphEvent::Created {
            run,
            parent,
            project,
            kind,
            task_id,
            title: title.chars().take(16_384).collect(),
        });
        for (name, pane, provider) in roles {
            let role = self.run_graph.next_role_id();
            self.record_run_graph_change(uniterm_core::RunGraphEvent::RoleDeclared {
                run,
                role,
                name: name.chars().take(256).collect(),
                pane: *pane,
                provider: provider.chars().take(256).collect(),
            });
        }
        run
    }

    fn record_orchestration_activation(&mut self, task_id: u64, role_index: usize) {
        let Some(run) = self.run_graph.run_for_task(task_id) else {
            return;
        };
        let Some(role) = self.run_graph.role_at(run, role_index) else {
            return;
        };
        let activation = self.run_graph.next_activation_id();
        let change = match self.run_graph.active_role(run) {
            Some(from) if from != role => uniterm_core::RunGraphEvent::Handoff {
                run,
                from,
                to: role,
                activation,
            },
            _ => uniterm_core::RunGraphEvent::Activated {
                run,
                role,
                activation,
            },
        };
        self.record_run_graph_change(change);
    }

    fn record_orchestration_terminal(
        &mut self,
        task_id: u64,
        status: uniterm_core::RunStatus,
        outcome: &str,
    ) {
        let Some(run) = self.run_graph.run_for_task(task_id) else {
            return;
        };
        if self
            .run_graph
            .run(run)
            .is_some_and(|run| run.status.terminal())
        {
            return;
        }
        let outcome: String = outcome.chars().take(16_384).collect();
        let change = match status {
            uniterm_core::RunStatus::Completed => {
                uniterm_core::RunGraphEvent::Completed { run, outcome }
            }
            uniterm_core::RunStatus::Failed => uniterm_core::RunGraphEvent::Failed { run, outcome },
            uniterm_core::RunStatus::Canceled => {
                uniterm_core::RunGraphEvent::Canceled { run, outcome }
            }
            uniterm_core::RunStatus::Created | uniterm_core::RunStatus::Active => return,
        };
        self.record_run_graph_change(change);
    }
}

pub(super) fn recovered_run_shape_error(
    run: &crate::eventlog::DurableOrchestration,
    pane_exists: impl Fn(PaneId) -> bool,
) -> Option<&'static str> {
    use uniterm_core::orchestrate::Phase;

    if !matches!(run.state.phase, Phase::Awaiting | Phase::Paused) {
        return Some("run was not in a restartable phase");
    }
    if run.guardrail.limits.validate().is_err() {
        return Some("run contained invalid guardrail limits");
    }
    if !(1..=uniterm_core::GUARDRAIL_MAX_ITERATIONS).contains(&run.state.max_iterations) {
        return Some("run contained an invalid state-machine iteration limit");
    }
    if run.guardrail.started_at_ms != 0
        && run.guardrail.limits.max_iterations != run.state.max_iterations
    {
        return Some("run guardrail and state-machine iteration limits disagreed");
    }
    if run.state.token == 0 {
        return Some("run had no outstanding activation token");
    }
    if run.state.cur >= run.state.roles.len() {
        return Some("current role was outside the role list");
    }
    if run.role_panes.len() != run.state.roles.len() || run.started.len() != run.role_panes.len() {
        return Some("role, Pane, and launch-state lengths disagreed");
    }
    if !run.role_providers.is_empty() && run.role_providers.len() != run.state.roles.len() {
        return Some("role and provider-selection lengths disagreed");
    }
    if run.role_providers.is_empty() && (run.agent_id.is_empty() || run.agent_cmd.is_empty()) {
        return Some("run had no recoverable provider ownership");
    }
    if run
        .role_providers
        .iter()
        .any(|provider| provider.provider.is_empty() || provider.command.is_empty())
    {
        return Some("one or more role providers were empty");
    }
    if run.role_panes.iter().any(|pane| !pane_exists(*pane)) {
        return Some("one or more role Panes could not be restored");
    }
    match run.kind {
        uniterm_proto::OrchestrationKind::Workflow => {
            let valid = run
                .template
                .as_deref()
                .and_then(uniterm_core::orchestrate::workflow_template)
                .is_some_and(|template| template.roles.len() == run.role_panes.len());
            if !valid {
                return Some("workflow template was missing or incompatible");
            }
        }
        uniterm_proto::OrchestrationKind::Relay if run.role_panes.len() != 2 => {
            return Some("relay did not contain exactly two roles");
        }
        uniterm_proto::OrchestrationKind::Relay => {}
    }
    None
}

fn recovered_role_providers(
    run: &crate::eventlog::DurableOrchestration,
) -> Vec<crate::workflow::ResolvedRoleProvider> {
    if run.role_providers.is_empty() {
        return (0..run.state.roles.len())
            .map(|_| crate::workflow::ResolvedRoleProvider {
                id: run.agent_id.clone(),
                command: run.agent_cmd.clone(),
            })
            .collect();
    }
    run.role_providers
        .iter()
        .map(|provider| crate::workflow::ResolvedRoleProvider {
            id: provider.provider.clone(),
            command: provider.command.clone(),
        })
        .collect()
}

fn restored_provider_conflicts(expected: &str, observed: Option<&str>) -> bool {
    observed.is_some_and(|observed| observed != expected)
}

pub(super) fn orchestration_idle_allowed(
    has_waiting_item: bool,
    has_pending_validation: bool,
    verifier: bool,
    has_expected_artifacts: bool,
) -> bool {
    !has_waiting_item && !has_pending_validation && !verifier && !has_expected_artifacts
}

pub(super) fn next_orchestration_deadline(
    submissions: &[PendingOrchestrationSubmission],
    deliveries: &[PendingPromptDelivery],
    workflows: &[ActiveWorkflow],
    relays: &[ActiveRelay],
) -> Option<std::time::Instant> {
    submissions
        .iter()
        .map(|submission| submission.due)
        .chain(deliveries.iter().map(|delivery| delivery.due))
        .chain(workflows.iter().filter_map(|run| run.stall_deadline))
        .chain(relays.iter().filter_map(|run| run.stall_deadline))
        .chain(workflows.iter().filter_map(|run| run.idle_deadline))
        .chain(relays.iter().filter_map(|run| run.idle_deadline))
        .chain(workflows.iter().filter_map(|run| {
            (run.state.phase == uniterm_core::orchestrate::Phase::Awaiting)
                .then_some(run.elapsed_deadline)
                .flatten()
        }))
        .chain(relays.iter().filter_map(|run| {
            (run.state.phase == uniterm_core::orchestrate::Phase::Awaiting)
                .then_some(run.elapsed_deadline)
                .flatten()
        }))
        .min()
}

pub(super) fn orchestration_token_seed() -> u64 {
    use std::io::Read as _;

    let mut bytes = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok()
    {
        let seed = u64::from_ne_bytes(bytes);
        if seed != 0 {
            return seed;
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    nanos ^ u64::from(std::process::id()).rotate_left(17) ^ 0x9e37_79b9_7f4a_7c15
}

impl Server {
    pub(super) fn reconcile_orchestration_idle(&mut self, pane: PaneId, status: AgentStatus) {
        let due = (status == AgentStatus::Idle)
            .then(|| std::time::Instant::now() + std::time::Duration::from_secs(5));
        let waiting = self.waiting.items().iter().any(|item| item.pane == pane);
        let pending_tokens: std::collections::HashSet<u64> = self
            .pending_orchestration_submissions
            .iter()
            .map(|pending| pending.token)
            .collect();
        for run in &mut self.workflows {
            if run.role_panes.get(run.state.cur).copied() != Some(pane)
                || run.state.phase != uniterm_core::orchestrate::Phase::Awaiting
            {
                continue;
            }
            let spec = &run.template.roles[run.state.cur];
            run.idle_deadline = if orchestration_idle_allowed(
                waiting,
                pending_tokens.contains(&run.state.token),
                spec.verifier,
                !spec.expected_artifacts.is_empty(),
            ) {
                due
            } else {
                None
            };
        }
        for run in &mut self.relays {
            if run.role_panes.get(run.state.cur).copied() == Some(pane)
                && run.state.phase == uniterm_core::orchestrate::Phase::Awaiting
            {
                run.idle_deadline = if orchestration_idle_allowed(
                    waiting,
                    pending_tokens.contains(&run.state.token),
                    false,
                    false,
                ) {
                    due
                } else {
                    None
                };
            }
        }
    }

    pub(super) fn next_activation_token(&mut self) -> u64 {
        loop {
            let mut value = self.orchestration_token_state;
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            self.orchestration_token_state = value.max(1);
            let token = self.orchestration_token_state;
            let occupied = self.workflows.iter().any(|run| run.state.token == token)
                || self.relays.iter().any(|run| run.state.token == token)
                || self
                    .pending_orchestration_submissions
                    .iter()
                    .any(|pending| pending.token == token);
            if !occupied {
                return token;
            }
        }
    }

    pub(super) fn scope_activation_token(
        &mut self,
        state: &mut uniterm_core::orchestrate::State,
        action: uniterm_core::orchestrate::Action,
    ) -> uniterm_core::orchestrate::Action {
        match action {
            uniterm_core::orchestrate::Action::Inject { role, .. } => {
                let token = self.next_activation_token();
                state.token = token;
                uniterm_core::orchestrate::Action::Inject { role, token }
            }
            action => action,
        }
    }

    pub(super) fn schedule_prompt_retry(&mut self, mut delivery: PendingPromptDelivery) {
        let base_ms = 2_000u64.saturating_mul(1u64 << delivery.attempts.saturating_sub(1));
        let jitter = delivery.token.rotate_left(u32::from(delivery.attempts)) % 401;
        delivery.due = std::time::Instant::now()
            + std::time::Duration::from_millis(base_ms.saturating_add(jitter));
        self.pending_prompt_deliveries
            .retain(|pending| pending.kind != delivery.kind || pending.token != delivery.token);
        self.pending_prompt_deliveries.push(delivery);
    }

    pub(super) fn flush_prompt_deliveries_due(&mut self, reg: &Registry) {
        let now = std::time::Instant::now();
        let mut due = Vec::new();
        let mut pending = Vec::with_capacity(self.pending_prompt_deliveries.len());
        for delivery in self.pending_prompt_deliveries.drain(..) {
            if delivery.due <= now {
                due.push(delivery);
            } else {
                pending.push(delivery);
            }
        }
        self.pending_prompt_deliveries = pending;
        for mut delivery in due {
            let active = match delivery.kind {
                uniterm_proto::OrchestrationKind::Workflow => self.workflows.iter().any(|run| {
                    run.task_id == delivery.task_id
                        && run.state.token == delivery.token
                        && run.role_panes.get(run.state.cur).copied() == Some(delivery.pane)
                }),
                uniterm_proto::OrchestrationKind::Relay => self.relays.iter().any(|run| {
                    run.task_id == delivery.task_id
                        && run.state.token == delivery.token
                        && run.role_panes.get(run.state.cur).copied() == Some(delivery.pane)
                }),
            };
            if !active {
                continue;
            }
            let accepted = self
                .panes
                .get_mut(&delivery.pane)
                .is_some_and(|pane| Self::queue_pane_input(reg, pane, &delivery.bytes));
            self.append_event(crate::eventlog::LogEvent::OrchestrationDelivery {
                kind: delivery.kind,
                task_id: delivery.task_id,
                token: delivery.token,
                accepted,
            });
            if accepted {
                match delivery.kind {
                    uniterm_proto::OrchestrationKind::Workflow => {
                        if let Some(run) = self
                            .workflows
                            .iter_mut()
                            .find(|run| run.task_id == delivery.task_id)
                        {
                            run.stall_deadline = Some(now + ORCHESTRATION_STALL_TIMEOUT);
                            if delivery.first_delivery {
                                run.started[run.state.cur] = true;
                            }
                        }
                    }
                    uniterm_proto::OrchestrationKind::Relay => {
                        if let Some(run) = self
                            .relays
                            .iter_mut()
                            .find(|run| run.task_id == delivery.task_id)
                        {
                            run.stall_deadline = Some(now + ORCHESTRATION_STALL_TIMEOUT);
                            if delivery.first_delivery {
                                run.started[run.state.cur] = true;
                            }
                        }
                    }
                }
                if delivery.first_delivery {
                    self.bind_agent(delivery.pane, &delivery.agent_id);
                }
                let waiting_kind = match delivery.kind {
                    uniterm_proto::OrchestrationKind::Workflow => {
                        uniterm_core::WaitingKind::Workflow
                    }
                    uniterm_proto::OrchestrationKind::Relay => uniterm_core::WaitingKind::Relay,
                };
                if let Some(item) = self.waiting.resolve_pane_kind(delivery.pane, waiting_kind) {
                    self.append_event(crate::eventlog::LogEvent::WaitingResolved {
                        id: item.id,
                        resolution: uniterm_core::WaitingResolution::Resumed,
                    });
                }
                let projection = match delivery.kind {
                    uniterm_proto::OrchestrationKind::Workflow => self
                        .workflows
                        .iter()
                        .find(|run| run.task_id == delivery.task_id)
                        .map(ActiveWorkflow::durable),
                    uniterm_proto::OrchestrationKind::Relay => self
                        .relays
                        .iter()
                        .find(|run| run.task_id == delivery.task_id)
                        .map(ActiveRelay::durable),
                };
                if let Some(run) = projection {
                    self.append_event(crate::eventlog::LogEvent::OrchestrationProjected { run });
                }
                continue;
            }
            delivery.attempts = delivery.attempts.saturating_add(1);
            if delivery.attempts < 3 {
                self.schedule_prompt_retry(delivery);
                continue;
            }
            let waiting_kind = match delivery.kind {
                uniterm_proto::OrchestrationKind::Workflow => uniterm_core::WaitingKind::Workflow,
                uniterm_proto::OrchestrationKind::Relay => uniterm_core::WaitingKind::Relay,
            };
            let change = self.waiting.request(
                delivery.pane,
                None,
                waiting_kind,
                "prompt delivery failed after three bounded attempts",
            );
            self.record_waiting_change(change);
        }
    }

    pub(super) fn flush_orchestration_stalls(&mut self, reg: &Registry) {
        let now = std::time::Instant::now();
        let workflow_tasks: Vec<u64> = self
            .workflows
            .iter()
            .filter(|run| run.stall_deadline.is_some_and(|deadline| deadline <= now))
            .map(|run| run.task_id)
            .collect();
        for task_id in workflow_tasks {
            let Some(index) = self.workflows.iter().position(|run| run.task_id == task_id) else {
                continue;
            };
            let mut run = self.workflows.swap_remove(index);
            run.stall_deadline = None;
            let action = uniterm_core::orchestrate::decide_workflow_next(
                &mut run.state,
                uniterm_core::orchestrate::Event::Stall,
            );
            self.apply_workflow_action(reg, &mut run, action, None);
            self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
                run: run.durable(),
            });
            if !matches!(
                run.state.phase,
                uniterm_core::orchestrate::Phase::Done | uniterm_core::orchestrate::Phase::Aborted
            ) {
                self.workflows.push(run);
            }
        }
        let relay_tasks: Vec<u64> = self
            .relays
            .iter()
            .filter(|run| run.stall_deadline.is_some_and(|deadline| deadline <= now))
            .map(|run| run.task_id)
            .collect();
        for task_id in relay_tasks {
            let Some(index) = self.relays.iter().position(|run| run.task_id == task_id) else {
                continue;
            };
            let mut run = self.relays.swap_remove(index);
            run.stall_deadline = None;
            let action = uniterm_core::orchestrate::decide_relay_next(
                &mut run.state,
                uniterm_core::orchestrate::Event::Stall,
            );
            self.apply_relay_action(reg, &mut run, action, None);
            self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
                run: run.durable(),
            });
            if !matches!(
                run.state.phase,
                uniterm_core::orchestrate::Phase::Done | uniterm_core::orchestrate::Phase::Aborted
            ) {
                self.relays.push(run);
            }
        }
    }

    pub(super) fn flush_orchestration_elapsed_due(&mut self, reg: &Registry) {
        let now = std::time::Instant::now();
        let now_ms = super::guardrail::unix_time_ms();
        let workflow_tasks: Vec<u64> = self
            .workflows
            .iter()
            .filter(|run| {
                run.state.phase == uniterm_core::orchestrate::Phase::Awaiting
                    && run.elapsed_deadline.is_some_and(|deadline| deadline <= now)
            })
            .map(|run| run.task_id)
            .collect();
        for task_id in workflow_tasks {
            let Some(index) = self.workflows.iter().position(|run| run.task_id == task_id) else {
                continue;
            };
            let mut run = self.workflows.swap_remove(index);
            run.elapsed_deadline = None;
            run.elapsed_guard_triggered = true;
            run.stall_deadline = None;
            run.idle_deadline = None;
            let elapsed = super::guardrail::elapsed_seconds(run.started_at_ms, now_ms)
                .max(run.guard_limits.max_elapsed_seconds);
            let decision =
                uniterm_core::evaluate_elapsed(elapsed, run.guard_limits.max_elapsed_seconds);
            let stable_run = self.run_graph.run_for_task(task_id);
            let project = stable_run
                .and_then(|id| self.run_graph.run(id))
                .map(|record| record.project);
            self.record_guardrail(uniterm_core::GuardrailRecord {
                project,
                run: stable_run,
                action: uniterm_core::GuardAction::ElapsedLimit {
                    elapsed_seconds: elapsed,
                    limit_seconds: run.guard_limits.max_elapsed_seconds,
                },
                decision: decision.clone(),
            });
            let reason = match decision {
                uniterm_core::GuardDecision::Ask { reason }
                | uniterm_core::GuardDecision::Deny { reason } => reason,
                uniterm_core::GuardDecision::Allow => {
                    run.elapsed_guard_triggered = false;
                    run.elapsed_deadline = super::guardrail::elapsed_deadline(
                        run.started_at_ms,
                        run.guard_limits,
                        false,
                    );
                    self.workflows.push(run);
                    continue;
                }
            };
            let action = uniterm_core::orchestrate::decide_workflow_next(
                &mut run.state,
                uniterm_core::orchestrate::Event::Guardrail { reason },
            );
            self.apply_workflow_action(reg, &mut run, action, None);
            self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
                run: run.durable(),
            });
            self.workflows.push(run);
        }

        let relay_tasks: Vec<u64> = self
            .relays
            .iter()
            .filter(|run| {
                run.state.phase == uniterm_core::orchestrate::Phase::Awaiting
                    && run.elapsed_deadline.is_some_and(|deadline| deadline <= now)
            })
            .map(|run| run.task_id)
            .collect();
        for task_id in relay_tasks {
            let Some(index) = self.relays.iter().position(|run| run.task_id == task_id) else {
                continue;
            };
            let mut run = self.relays.swap_remove(index);
            run.elapsed_deadline = None;
            run.elapsed_guard_triggered = true;
            run.stall_deadline = None;
            run.idle_deadline = None;
            let elapsed = super::guardrail::elapsed_seconds(run.started_at_ms, now_ms)
                .max(run.guard_limits.max_elapsed_seconds);
            let decision =
                uniterm_core::evaluate_elapsed(elapsed, run.guard_limits.max_elapsed_seconds);
            let stable_run = self.run_graph.run_for_task(task_id);
            let project = stable_run
                .and_then(|id| self.run_graph.run(id))
                .map(|record| record.project);
            self.record_guardrail(uniterm_core::GuardrailRecord {
                project,
                run: stable_run,
                action: uniterm_core::GuardAction::ElapsedLimit {
                    elapsed_seconds: elapsed,
                    limit_seconds: run.guard_limits.max_elapsed_seconds,
                },
                decision: decision.clone(),
            });
            let reason = match decision {
                uniterm_core::GuardDecision::Ask { reason }
                | uniterm_core::GuardDecision::Deny { reason } => reason,
                uniterm_core::GuardDecision::Allow => {
                    run.elapsed_guard_triggered = false;
                    run.elapsed_deadline = super::guardrail::elapsed_deadline(
                        run.started_at_ms,
                        run.guard_limits,
                        false,
                    );
                    self.relays.push(run);
                    continue;
                }
            };
            let action = uniterm_core::orchestrate::decide_relay_next(
                &mut run.state,
                uniterm_core::orchestrate::Event::Guardrail { reason },
            );
            self.apply_relay_action(reg, &mut run, action, None);
            self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
                run: run.durable(),
            });
            self.relays.push(run);
        }
    }

    pub(super) fn flush_orchestration_idle_due(&mut self, reg: &Registry) {
        let now = std::time::Instant::now();
        let waiting_panes: std::collections::HashSet<PaneId> =
            self.waiting.items().iter().map(|item| item.pane).collect();
        let pending_tokens: std::collections::HashSet<u64> = self
            .pending_orchestration_submissions
            .iter()
            .map(|pending| pending.token)
            .collect();
        for run in &mut self.workflows {
            let pane = run.role_panes.get(run.state.cur).copied();
            if pane.is_some_and(|pane| waiting_panes.contains(&pane))
                || pending_tokens.contains(&run.state.token)
            {
                run.idle_deadline = None;
            }
        }
        for run in &mut self.relays {
            let pane = run.role_panes.get(run.state.cur).copied();
            if pane.is_some_and(|pane| waiting_panes.contains(&pane))
                || pending_tokens.contains(&run.state.token)
            {
                run.idle_deadline = None;
            }
        }
        let workflow_tasks: Vec<u64> = self
            .workflows
            .iter()
            .filter(|run| run.idle_deadline.is_some_and(|deadline| deadline <= now))
            .map(|run| run.task_id)
            .collect();
        for task_id in workflow_tasks {
            let Some(index) = self.workflows.iter().position(|run| run.task_id == task_id) else {
                continue;
            };
            let mut run = self.workflows.swap_remove(index);
            run.idle_deadline = None;
            run.stall_deadline = None;
            let role = run.state.cur;
            let action = uniterm_core::orchestrate::decide_workflow_next(
                &mut run.state,
                uniterm_core::orchestrate::Event::Idle { role },
            );
            let action = self.scope_activation_token(&mut run.state, action);
            self.apply_workflow_action(reg, &mut run, action, None);
            if !matches!(
                run.state.phase,
                uniterm_core::orchestrate::Phase::Done | uniterm_core::orchestrate::Phase::Aborted
            ) {
                self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
                    run: run.durable(),
                });
            }
            if !matches!(
                run.state.phase,
                uniterm_core::orchestrate::Phase::Done | uniterm_core::orchestrate::Phase::Aborted
            ) {
                self.workflows.push(run);
            }
        }
        let relay_tasks: Vec<u64> = self
            .relays
            .iter()
            .filter(|run| run.idle_deadline.is_some_and(|deadline| deadline <= now))
            .map(|run| run.task_id)
            .collect();
        for task_id in relay_tasks {
            let Some(index) = self.relays.iter().position(|run| run.task_id == task_id) else {
                continue;
            };
            let mut run = self.relays.swap_remove(index);
            run.idle_deadline = None;
            run.stall_deadline = None;
            let role = run.state.cur;
            let action = uniterm_core::orchestrate::decide_relay_next(
                &mut run.state,
                uniterm_core::orchestrate::Event::Idle { role },
            );
            let action = self.scope_activation_token(&mut run.state, action);
            self.apply_relay_action(reg, &mut run, action, None);
            if !matches!(
                run.state.phase,
                uniterm_core::orchestrate::Phase::Done | uniterm_core::orchestrate::Phase::Aborted
            ) {
                self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
                    run: run.durable(),
                });
            }
            if !matches!(
                run.state.phase,
                uniterm_core::orchestrate::Phase::Done | uniterm_core::orchestrate::Phase::Aborted
            ) {
                self.relays.push(run);
            }
        }
    }

    pub(super) fn flush_artifact_validations_due(&mut self) {
        let now = std::time::Instant::now();
        let mut due = Vec::new();
        self.pending_orchestration_submissions.retain(|pending| {
            if pending.due <= now {
                due.push((pending.kind, pending.task_id, pending.token, pending.pane));
                false
            } else {
                true
            }
        });
        for (kind, task_id, token, pane) in due {
            let active = match kind {
                uniterm_proto::OrchestrationKind::Workflow => self.workflows.iter().any(|run| {
                    run.task_id == task_id
                        && run.state.token == token
                        && run.role_panes.get(run.state.cur).copied() == Some(pane)
                }),
                uniterm_proto::OrchestrationKind::Relay => self.relays.iter().any(|run| {
                    run.task_id == task_id
                        && run.state.token == token
                        && run.role_panes.get(run.state.cur).copied() == Some(pane)
                }),
            };
            if !active {
                continue;
            }
            let waiting_kind = match kind {
                uniterm_proto::OrchestrationKind::Workflow => uniterm_core::WaitingKind::Workflow,
                uniterm_proto::OrchestrationKind::Relay => uniterm_core::WaitingKind::Relay,
            };
            let change = self.waiting.request(
                pane,
                None,
                waiting_kind,
                "artifact validation timed out; resume the role and submit again",
            );
            self.record_waiting_change(change);
        }
    }

    /// Launch a workflow (AG5, wired): a new window with one pane per role,
    /// the pure engine driving which role's turn is open, and per-turn tokens
    /// embedded in the injected prompts. The engine advances only on
    /// `uniterm workflow submit <token>` (delivered as [`ClientMessage::WorkflowSubmit`]).
    pub(super) fn launch_workflow(
        &mut self,
        reg: &Registry,
        name: &str,
        agent: Option<&str>,
        role_selections: &[uniterm_core::orchestrate::RoleProviderSelection],
        goal: &str,
        project: Option<&str>,
    ) -> Result<uniterm_core::RunId, String> {
        self.launch_workflow_with_parent(
            reg,
            name,
            agent,
            role_selections,
            goal,
            OrchestrationTarget {
                project,
                parent: None,
            },
        )
    }

    pub(super) fn launch_workflow_with_parent(
        &mut self,
        reg: &Registry,
        name: &str,
        agent: Option<&str>,
        role_selections: &[uniterm_core::orchestrate::RoleProviderSelection],
        goal: &str,
        target: OrchestrationTarget<'_>,
    ) -> Result<uniterm_core::RunId, String> {
        use uniterm_core::orchestrate::{decide_workflow_next, Event, Phase, State};
        let goal: String = goal.chars().take(65_536).collect();
        let Some(template) = uniterm_core::orchestrate::workflow_template(name) else {
            return Err(format!(
                "unknown workflow template '{name}' (expected solo, pair, or triad)"
            ));
        };
        let engine_roles = template.engine_roles();
        let role_providers = crate::workflow::resolve_role_providers_on_search_path(
            &engine_roles,
            agent,
            role_selections,
            &self.agent_search_path,
        )?;
        if role_providers.len() != engine_roles.len() {
            return Err("provider resolution returned an incomplete role mapping".into());
        }
        let guarded = self.prepare_orchestration_launch(
            target.project,
            uniterm_core::RunKind::Workflow,
            template.roles.len(),
        )?;
        // One pane per role, split side by side in a fresh named window. Each
        // pane boots a one-line reader instead of an interactive shell: the
        // turn's Inject writes the agent launch line, `read` consumes it, and
        // `eval` starts the agent - no interactive-shell startup to race (zsh
        // discards typeahead while booting).
        let reader = r#"printf 'waiting for turn...\n'; IFS= read -r l; eval "$l""#;
        let mut role_panes: Vec<PaneId> = Vec::new();
        for _ in template.roles {
            match self.spawn_pane_at(reg, &["-c", reader], Some(&guarded.project_root)) {
                Ok(id) => role_panes.push(id),
                Err(_) => break,
            }
        }
        if role_panes.len() != template.roles.len() {
            self.terminate_panes(&role_panes);
            for pane in role_panes {
                self.close_pane(reg, pane);
            }
            return Err("could not create every workflow role Pane".into());
        }
        let mut layout = LayoutNode::Leaf(role_panes[0]);
        for pair in role_panes.windows(2) {
            layout.split(pair[0], SplitDir::Vertical, pair[1]);
        }
        self.windows.push(Win {
            project: guarded.project,
            layout,
            active: role_panes[0],
            zoomed: None,
            name: Some(format!("wf:{name}")),
        });
        self.activate_window(self.windows.len() - 1);
        self.relayout();

        let goal_title = format!(
            "# project {}: workflow {name}: {}",
            guarded.project_name,
            goal.trim()
        );
        let task_id = self.create_task(&goal_title, uniterm_core::TaskStatus::Doing);
        let mut wf = ActiveWorkflow {
            state: State::new(engine_roles, guarded.limits.max_iterations),
            template,
            goal,
            role_providers,
            started: vec![false; role_panes.len()],
            role_panes,
            task_id,
            started_at_ms: guarded.started_at_ms,
            guard_limits: guarded.limits,
            elapsed_guard_triggered: false,
            elapsed_deadline: super::guardrail::elapsed_deadline(
                guarded.started_at_ms,
                guarded.limits,
                false,
            ),
            stall_deadline: None,
            idle_deadline: None,
        };
        let roles: Vec<_> = wf
            .state
            .roles
            .iter()
            .zip(wf.role_panes.iter().zip(&wf.role_providers))
            .map(|(role, (pane, provider))| (role.name.clone(), *pane, provider.id.clone()))
            .collect();
        let run = self.create_orchestration_run(
            target.parent,
            uniterm_core::RunKind::Workflow,
            task_id,
            &goal_title,
            guarded.project,
            &roles,
        );
        let action = decide_workflow_next(&mut wf.state, Event::Start);
        let action = self.scope_activation_token(&mut wf.state, action);
        self.apply_workflow_action(reg, &mut wf, action, None);
        self.append_event(crate::eventlog::LogEvent::OrchestrationProjected { run: wf.durable() });
        if matches!(wf.state.phase, Phase::Awaiting | Phase::Paused) {
            self.workflows.push(wf);
        }
        self.full_repaint_all(reg);
        self.persist();
        Ok(run)
    }

    /// Launch a two-role asynchronous relay. Roles are isolated in their own
    /// Panes and only the role owning the live token may advance the run.
    pub(super) fn launch_relay(
        &mut self,
        reg: &Registry,
        agent: Option<&str>,
        role_selections: &[uniterm_core::orchestrate::RoleProviderSelection],
        goal: &str,
        project: Option<&str>,
    ) -> Result<uniterm_core::RunId, String> {
        self.launch_relay_with_parent(
            reg,
            agent,
            role_selections,
            goal,
            OrchestrationTarget {
                project,
                parent: None,
            },
        )
    }

    pub(super) fn launch_relay_with_parent(
        &mut self,
        reg: &Registry,
        agent: Option<&str>,
        role_selections: &[uniterm_core::orchestrate::RoleProviderSelection],
        goal: &str,
        target: OrchestrationTarget<'_>,
    ) -> Result<uniterm_core::RunId, String> {
        use uniterm_core::orchestrate::{decide_relay_next, Event, Phase, Role, State};
        let goal: String = goal.chars().take(65_536).collect();
        let engine_roles = vec![
            Role::requiring("builder", false, ["interactive_cli"]),
            Role::requiring("reviewer", false, ["interactive_cli"]),
        ];
        let role_providers = crate::workflow::resolve_role_providers_on_search_path(
            &engine_roles,
            agent,
            role_selections,
            &self.agent_search_path,
        )?;
        let guarded =
            self.prepare_orchestration_launch(target.project, uniterm_core::RunKind::Relay, 2)?;
        let reader = r#"printf 'waiting for relay turn...\n'; IFS= read -r l; eval "$l""#;
        let mut role_panes = Vec::new();
        for _ in 0..2 {
            match self.spawn_pane_at(reg, &["-c", reader], Some(&guarded.project_root)) {
                Ok(id) => role_panes.push(id),
                Err(_) => break,
            }
        }
        if role_panes.len() != 2 {
            self.terminate_panes(&role_panes);
            for pane in role_panes {
                self.close_pane(reg, pane);
            }
            return Err("could not create both relay role Panes".into());
        }
        let layout = LayoutNode::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf(role_panes[0])),
            second: Box::new(LayoutNode::Leaf(role_panes[1])),
        };
        self.windows.push(Win {
            project: guarded.project,
            layout,
            active: role_panes[0],
            zoomed: None,
            name: Some("relay".into()),
        });
        self.activate_window(self.windows.len() - 1);
        self.relayout();
        let title = format!("# project {}: relay: {}", guarded.project_name, goal.trim());
        let task_id = self.create_task(&title, uniterm_core::TaskStatus::Doing);
        let mut relay = ActiveRelay {
            state: State::new(engine_roles, guarded.limits.max_iterations),
            goal,
            role_providers,
            role_panes,
            started: vec![false; 2],
            task_id,
            checkpoints: Vec::new(),
            started_at_ms: guarded.started_at_ms,
            guard_limits: guarded.limits,
            elapsed_guard_triggered: false,
            elapsed_deadline: super::guardrail::elapsed_deadline(
                guarded.started_at_ms,
                guarded.limits,
                false,
            ),
            stall_deadline: None,
            idle_deadline: None,
        };
        let roles: Vec<_> = relay
            .state
            .roles
            .iter()
            .zip(relay.role_panes.iter().zip(&relay.role_providers))
            .map(|(role, (pane, provider))| (role.name.clone(), *pane, provider.id.clone()))
            .collect();
        let run = self.create_orchestration_run(
            target.parent,
            uniterm_core::RunKind::Relay,
            task_id,
            &title,
            guarded.project,
            &roles,
        );
        let action = decide_relay_next(&mut relay.state, Event::Start);
        let action = self.scope_activation_token(&mut relay.state, action);
        let checkpoint = match &action {
            uniterm_core::orchestrate::Action::Inject { role, token }
                if relay.state.roles[*role].name == "builder" =>
            {
                self.project_root_for_pane(relay.role_panes[*role])
                    .map(|root| (*role, *token, root))
            }
            _ => None,
        };
        if checkpoint.is_none() {
            self.apply_relay_action(reg, &mut relay, action, None);
        }
        self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
            run: relay.durable(),
        });
        if matches!(relay.state.phase, Phase::Awaiting | Phase::Paused) {
            self.relays.push(relay);
        }
        if let Some((role, token, project_root)) = checkpoint {
            self.pending_relay_activations.push(PendingRelayActivation {
                task_id,
                role,
                token,
                handoff: None,
            });
            self.agents
                .send(uniterm_proto::CoreToAgent::RelayCheckpointCreate {
                    task_id,
                    token,
                    project_root,
                });
        }
        self.full_repaint_all(reg);
        self.persist();
        Ok(run)
    }

    pub(super) fn apply_relay_action(
        &mut self,
        reg: &Registry,
        relay: &mut ActiveRelay,
        action: uniterm_core::orchestrate::Action,
        handoff: Option<&str>,
    ) {
        use uniterm_core::orchestrate::Action;
        match action {
            Action::Inject { role, token } => {
                let role_name = &relay.state.roles[role].name;
                let submit = format!("uniterm relay submit {token}");
                let handoff = handoff
                    .filter(|summary| !summary.trim().is_empty())
                    .map(|summary| format!(" Previous role summary: {summary}."))
                    .unwrap_or_default();
                let prompt = format!(
                    "You are the {role_name} in a Uniterm relay. Goal: {}.{handoff} Work in the shared Project, then run exactly: {submit}",
                    relay.goal
                );
                let pane_id = relay.role_panes[role];
                self.record_orchestration_activation(relay.task_id, role);
                self.append_event(crate::eventlog::LogEvent::OrchestrationActivated {
                    kind: uniterm_proto::OrchestrationKind::Relay,
                    task_id: relay.task_id,
                    role,
                    pane: pane_id,
                    token,
                });
                let Some(pane) = self.panes.get_mut(&pane_id) else {
                    return;
                };
                let Some(provider) = relay.role_providers.get(role) else {
                    return;
                };
                let first_delivery = !relay.started[role];
                let bytes = if first_delivery {
                    let invocation = crate::workflow::launch_invocation(&provider.command, &prompt);
                    format!(
                        "{}\n",
                        crate::workflow::announce_wrapped(&provider.id, &invocation)
                    )
                    .into_bytes()
                } else {
                    let mut bytes = Vec::with_capacity(prompt.len().saturating_add(16));
                    if pane.term.bracketed_paste() {
                        bytes.extend_from_slice(b"\x1b[200~");
                    }
                    bytes.extend_from_slice(prompt.as_bytes());
                    if pane.term.bracketed_paste() {
                        bytes.extend_from_slice(b"\x1b[201~");
                    }
                    bytes.push(b'\r');
                    bytes
                };
                let accepted = Self::queue_pane_input(reg, pane, &bytes);
                self.append_event(crate::eventlog::LogEvent::OrchestrationDelivery {
                    kind: uniterm_proto::OrchestrationKind::Relay,
                    task_id: relay.task_id,
                    token,
                    accepted,
                });
                if accepted {
                    relay.stall_deadline =
                        Some(std::time::Instant::now() + ORCHESTRATION_STALL_TIMEOUT);
                }
                if accepted && first_delivery {
                    relay.started[role] = true;
                    self.bind_agent(pane_id, &provider.id);
                } else if !accepted {
                    self.schedule_prompt_retry(PendingPromptDelivery {
                        kind: uniterm_proto::OrchestrationKind::Relay,
                        task_id: relay.task_id,
                        token,
                        pane: pane_id,
                        bytes,
                        first_delivery,
                        agent_id: provider.id.clone(),
                        attempts: 1,
                        due: std::time::Instant::now(),
                    });
                }
            }
            Action::Complete => {
                self.finish_relay(reg, relay, uniterm_core::TaskStatus::Done, "done")
            }
            Action::Escalate { reason } => {
                let pane = relay.role_panes[relay.state.cur];
                let change =
                    self.waiting
                        .request(pane, None, uniterm_core::WaitingKind::Relay, &reason);
                self.record_waiting_change(change);
                if self
                    .tasks
                    .set_status(relay.task_id, uniterm_core::TaskStatus::Blocked)
                {
                    self.append_event(crate::eventlog::LogEvent::TaskStatusChanged {
                        id: relay.task_id,
                        status: uniterm_core::TaskStatus::Blocked,
                    });
                }
            }
            Action::Abort { reason } => {
                self.finish_relay(reg, relay, uniterm_core::TaskStatus::Blocked, &reason)
            }
            Action::AwaitSubmit | Action::Hold => {}
        }
    }

    pub(super) fn finish_relay(
        &mut self,
        reg: &Registry,
        relay: &ActiveRelay,
        status: uniterm_core::TaskStatus,
        outcome: &str,
    ) {
        self.pending_orchestration_submissions
            .retain(|pending| pending.task_id != relay.task_id);
        let run_status = if status == uniterm_core::TaskStatus::Done {
            uniterm_core::RunStatus::Completed
        } else if outcome == "stopped by human" {
            uniterm_core::RunStatus::Canceled
        } else {
            uniterm_core::RunStatus::Failed
        };
        self.record_orchestration_terminal(relay.task_id, run_status, outcome);
        self.append_event(crate::eventlog::LogEvent::OrchestrationFinished {
            kind: uniterm_proto::OrchestrationKind::Relay,
            task_id: relay.task_id,
            outcome: outcome.to_string(),
        });
        if self.tasks.set_status(relay.task_id, status) {
            self.append_event(crate::eventlog::LogEvent::TaskStatusChanged {
                id: relay.task_id,
                status,
            });
        }
        if let Some(window) = self
            .windows
            .iter_mut()
            .find(|window| window.layout.contains_pane(relay.role_panes[0]))
        {
            window.name = Some(format!("relay: {outcome}"));
        }
        self.full_repaint_all(reg);
        self.persist();
    }

    /// Interpret an engine [`Action`](uniterm_core::orchestrate::Action):
    /// inject a role's prompt (starting its agent on the first turn, pasting
    /// into the running agent on loopbacks), or finish/abort the run.
    pub(super) fn apply_workflow_action(
        &mut self,
        reg: &Registry,
        wf: &mut ActiveWorkflow,
        action: uniterm_core::orchestrate::Action,
        findings: Option<&str>,
    ) {
        use uniterm_core::orchestrate::{render_role_prompt, Action};
        match action {
            Action::Inject { role, token } => {
                let spec = &wf.template.roles[role];
                let mut prompt = render_role_prompt(spec, &wf.goal, token);
                if let Some(f) = findings {
                    prompt = format!("The verifier sent this back. Findings: {f}. {prompt}");
                }
                let pane_id = wf.role_panes[role];
                self.record_orchestration_activation(wf.task_id, role);
                self.append_event(crate::eventlog::LogEvent::OrchestrationActivated {
                    kind: uniterm_proto::OrchestrationKind::Workflow,
                    task_id: wf.task_id,
                    role,
                    pane: pane_id,
                    token,
                });
                let Some(pane) = self.panes.get_mut(&pane_id) else {
                    return; // role pane was closed; the run dies quietly
                };
                let Some(provider) = wf.role_providers.get(role) else {
                    return;
                };
                let first_delivery = !wf.started[role];
                let bytes = if first_delivery {
                    // The pane's reader is blocked on one line: the launch,
                    // wrapped in lifecycle envelopes so the stream announces
                    // the agent even without a connector.
                    let invocation = crate::workflow::launch_invocation(&provider.command, &prompt);
                    format!(
                        "{}\n",
                        crate::workflow::announce_wrapped(&provider.id, &invocation)
                    )
                    .into_bytes()
                } else {
                    // The agent is already running. Honor its negotiated paste
                    // mode so delimiters never become visible prompt text.
                    let mut bytes = Vec::new();
                    if pane.term.bracketed_paste() {
                        bytes.extend_from_slice(b"\x1b[200~");
                    }
                    bytes.extend_from_slice(prompt.as_bytes());
                    if pane.term.bracketed_paste() {
                        bytes.extend_from_slice(b"\x1b[201~");
                    }
                    bytes.push(b'\r');
                    bytes
                };
                let accepted = Self::queue_pane_input(reg, pane, &bytes);
                self.append_event(crate::eventlog::LogEvent::OrchestrationDelivery {
                    kind: uniterm_proto::OrchestrationKind::Workflow,
                    task_id: wf.task_id,
                    token,
                    accepted,
                });
                if accepted {
                    wf.stall_deadline =
                        Some(std::time::Instant::now() + ORCHESTRATION_STALL_TIMEOUT);
                }
                if accepted && first_delivery {
                    wf.started[role] = true;
                    self.bind_agent(pane_id, &provider.id);
                    self.full_repaint_all(reg);
                } else if !accepted {
                    self.schedule_prompt_retry(PendingPromptDelivery {
                        kind: uniterm_proto::OrchestrationKind::Workflow,
                        task_id: wf.task_id,
                        token,
                        pane: pane_id,
                        bytes,
                        first_delivery,
                        agent_id: provider.id.clone(),
                        attempts: 1,
                        due: std::time::Instant::now(),
                    });
                }
            }
            Action::Complete => {
                self.finish_workflow(reg, wf, uniterm_core::TaskStatus::Done, "done");
            }
            Action::Escalate { reason } => {
                let pane = wf.role_panes[wf.state.cur];
                let change =
                    self.waiting
                        .request(pane, None, uniterm_core::WaitingKind::Workflow, &reason);
                self.record_waiting_change(change);
                if self
                    .tasks
                    .set_status(wf.task_id, uniterm_core::TaskStatus::Blocked)
                {
                    self.append_event(crate::eventlog::LogEvent::TaskStatusChanged {
                        id: wf.task_id,
                        status: uniterm_core::TaskStatus::Blocked,
                    });
                }
            }
            Action::Abort { reason } => {
                self.finish_workflow(reg, wf, uniterm_core::TaskStatus::Blocked, &reason);
            }
            Action::AwaitSubmit | Action::Hold => {}
        }
    }

    /// Mark a finished/aborted workflow: task status, event log, and the
    /// window title flips to `wf:<name>: <outcome>` so the status line says
    /// what happened without touching the agents' panes.
    pub(super) fn finish_workflow(
        &mut self,
        reg: &Registry,
        wf: &ActiveWorkflow,
        status: uniterm_core::TaskStatus,
        outcome: &str,
    ) {
        self.pending_orchestration_submissions
            .retain(|pending| pending.task_id != wf.task_id);
        let run_status = if status == uniterm_core::TaskStatus::Done {
            uniterm_core::RunStatus::Completed
        } else if outcome == "stopped by human" {
            uniterm_core::RunStatus::Canceled
        } else {
            uniterm_core::RunStatus::Failed
        };
        self.record_orchestration_terminal(wf.task_id, run_status, outcome);
        self.append_event(crate::eventlog::LogEvent::OrchestrationFinished {
            kind: uniterm_proto::OrchestrationKind::Workflow,
            task_id: wf.task_id,
            outcome: outcome.to_string(),
        });
        if self.tasks.set_status(wf.task_id, status) {
            self.append_event(crate::eventlog::LogEvent::TaskStatusChanged {
                id: wf.task_id,
                status,
            });
        }
        let first = wf.role_panes[0];
        if let Some(w) = self
            .windows
            .iter_mut()
            .find(|w| w.layout.contains_pane(first))
        {
            w.name = Some(format!("wf:{}: {}", wf.template.name, outcome));
        }
        self.full_repaint_all(reg);
        self.persist();
    }

    /// Route a delivered completion-contract submission into the matching
    /// workflow's engine. Forged/stale tokens match nothing and are dropped.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_workflow_submit(
        &mut self,
        reg: &Registry,
        token: u64,
        status: uniterm_proto::SubmissionStatus,
        verdict: Option<String>,
        summary: String,
        artifacts: Vec<uniterm_proto::ArtifactClaim>,
        artifacts_validated: bool,
    ) {
        use uniterm_core::orchestrate::{
            decide_workflow_next, Event, Phase, Submit, SubmitStatus, Verdict,
        };
        let Some(idx) = self
            .workflows
            .iter()
            .position(|w| w.state.token == token && w.state.phase == Phase::Awaiting)
        else {
            return;
        };
        let summary: String = summary.chars().take(16_384).collect();
        let artifacts: Vec<uniterm_proto::ArtifactClaim> = artifacts
            .into_iter()
            .take(64)
            .map(|claim| uniterm_proto::ArtifactClaim {
                kind: claim.kind,
                path: claim.path.chars().take(4_096).collect(),
            })
            .collect();
        self.workflows[idx].stall_deadline = None;
        self.workflows[idx].idle_deadline = None;
        if !artifacts_validated {
            self.append_event(crate::eventlog::LogEvent::OrchestrationSubmitted {
                kind: uniterm_proto::OrchestrationKind::Workflow,
                task_id: self.workflows[idx].task_id,
                token,
                status,
                verdict: verdict.clone(),
                summary: summary.clone(),
                artifacts: artifacts.iter().map(|claim| claim.path.clone()).collect(),
            });
        }
        let expected: Vec<uniterm_proto::ArtifactClaim> = self.workflows[idx].template.roles
            [self.workflows[idx].state.cur]
            .expected_artifacts
            .iter()
            .map(|artifact| uniterm_proto::ArtifactClaim {
                kind: artifact.kind,
                path: artifact.path.to_string(),
            })
            .collect();
        if !artifacts_validated
            && status == uniterm_proto::SubmissionStatus::Done
            && (!expected.is_empty() || !artifacts.is_empty())
        {
            if self
                .pending_orchestration_submissions
                .iter()
                .any(|pending| {
                    pending.kind == uniterm_proto::OrchestrationKind::Workflow
                        && pending.token == token
                })
            {
                return;
            }
            let pane = self.workflows[idx].role_panes[self.workflows[idx].state.cur];
            let Some(project_root) = self.project_root_for_pane(pane) else {
                return;
            };
            self.pending_orchestration_submissions
                .push(PendingOrchestrationSubmission {
                    kind: uniterm_proto::OrchestrationKind::Workflow,
                    task_id: self.workflows[idx].task_id,
                    token,
                    pane,
                    status,
                    verdict,
                    summary,
                    due: std::time::Instant::now() + ARTIFACT_VALIDATION_TIMEOUT,
                });
            self.agents
                .send(uniterm_proto::CoreToAgent::ArtifactValidate {
                    kind: uniterm_proto::OrchestrationKind::Workflow,
                    task_id: self.workflows[idx].task_id,
                    token,
                    project_root,
                    expected,
                    reported: artifacts,
                });
            return;
        }
        if artifacts_validated {
            self.append_event(crate::eventlog::LogEvent::OrchestrationArtifactsValidated {
                kind: uniterm_proto::OrchestrationKind::Workflow,
                task_id: self.workflows[idx].task_id,
                token,
                artifacts: artifacts.iter().map(|claim| claim.path.clone()).collect(),
            });
        }
        if status == uniterm_proto::SubmissionStatus::NeedsInput {
            let pane = self.workflows[idx].role_panes[self.workflows[idx].state.cur];
            let invocation = self
                .panes
                .get(&pane)
                .and_then(|pane| pane.agent.as_ref())
                .and_then(|agent| agent.foreground_pid)
                .or_else(|| self.panes.get(&pane).and_then(|pane| pane.foreground_pid));
            let summary = if summary.trim().is_empty() {
                "workflow role needs input".to_string()
            } else {
                summary
            };
            let change = self.waiting.request(
                pane,
                invocation,
                uniterm_core::WaitingKind::Workflow,
                &summary,
            );
            self.record_waiting_change(change);
            self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
                run: self.workflows[idx].durable(),
            });
            return;
        }
        let active_pane = self.workflows[idx].role_panes[self.workflows[idx].state.cur];
        if let Some(item) = self
            .waiting
            .resolve_pane_kind(active_pane, uniterm_core::WaitingKind::Workflow)
        {
            self.append_event(crate::eventlog::LogEvent::WaitingResolved {
                id: item.id,
                resolution: uniterm_core::WaitingResolution::AgentAdvanced,
            });
        }
        let mut wf = self.workflows.swap_remove(idx);
        let verdict = verdict.as_deref().map(|v| match v {
            "approved" => Verdict::Approved,
            "replan" => Verdict::Replan(summary.clone()),
            _ => Verdict::Fix(summary.clone()),
        });
        let event = Event::Submit(Submit {
            token,
            role: wf.state.cur,
            status: match status {
                uniterm_proto::SubmissionStatus::Done => SubmitStatus::Done,
                uniterm_proto::SubmissionStatus::NeedsInput => SubmitStatus::NeedsInput,
                uniterm_proto::SubmissionStatus::Failed => SubmitStatus::Failed,
            },
            verdict,
        });
        let action = decide_workflow_next(&mut wf.state, event);
        let action = self.scope_activation_token(&mut wf.state, action);
        let findings = (!summary.is_empty()).then_some(summary.as_str());
        self.apply_workflow_action(reg, &mut wf, action, findings);
        if matches!(wf.state.phase, Phase::Awaiting | Phase::Paused) {
            self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
                run: wf.durable(),
            });
        }
        if matches!(wf.state.phase, Phase::Awaiting | Phase::Paused) {
            self.workflows.push(wf);
        }
    }

    pub(super) fn on_relay_submit(
        &mut self,
        reg: &Registry,
        token: u64,
        status: uniterm_proto::SubmissionStatus,
        summary: String,
        artifacts: Vec<uniterm_proto::ArtifactClaim>,
        artifacts_validated: bool,
    ) {
        use uniterm_core::orchestrate::{decide_relay_next, Event, Phase, Submit, SubmitStatus};
        let Some(index) = self
            .relays
            .iter()
            .position(|relay| relay.state.token == token && relay.state.phase == Phase::Awaiting)
        else {
            return;
        };
        let summary: String = summary.chars().take(16_384).collect();
        let artifacts: Vec<uniterm_proto::ArtifactClaim> = artifacts
            .into_iter()
            .take(64)
            .map(|claim| uniterm_proto::ArtifactClaim {
                kind: claim.kind,
                path: claim.path.chars().take(4_096).collect(),
            })
            .collect();
        self.relays[index].stall_deadline = None;
        self.relays[index].idle_deadline = None;
        if !artifacts_validated {
            self.append_event(crate::eventlog::LogEvent::OrchestrationSubmitted {
                kind: uniterm_proto::OrchestrationKind::Relay,
                task_id: self.relays[index].task_id,
                token,
                status,
                verdict: None,
                summary: summary.clone(),
                artifacts: artifacts.iter().map(|claim| claim.path.clone()).collect(),
            });
        }
        if !artifacts_validated
            && status == uniterm_proto::SubmissionStatus::Done
            && !artifacts.is_empty()
        {
            if self
                .pending_orchestration_submissions
                .iter()
                .any(|pending| {
                    pending.kind == uniterm_proto::OrchestrationKind::Relay
                        && pending.token == token
                })
            {
                return;
            }
            let pane = self.relays[index].role_panes[self.relays[index].state.cur];
            let Some(project_root) = self.project_root_for_pane(pane) else {
                return;
            };
            self.pending_orchestration_submissions
                .push(PendingOrchestrationSubmission {
                    kind: uniterm_proto::OrchestrationKind::Relay,
                    task_id: self.relays[index].task_id,
                    token,
                    pane,
                    status,
                    verdict: None,
                    summary,
                    due: std::time::Instant::now() + ARTIFACT_VALIDATION_TIMEOUT,
                });
            self.agents
                .send(uniterm_proto::CoreToAgent::ArtifactValidate {
                    kind: uniterm_proto::OrchestrationKind::Relay,
                    task_id: self.relays[index].task_id,
                    token,
                    project_root,
                    expected: Vec::new(),
                    reported: artifacts,
                });
            return;
        }
        if artifacts_validated {
            self.append_event(crate::eventlog::LogEvent::OrchestrationArtifactsValidated {
                kind: uniterm_proto::OrchestrationKind::Relay,
                task_id: self.relays[index].task_id,
                token,
                artifacts: artifacts.iter().map(|claim| claim.path.clone()).collect(),
            });
        }
        if status == uniterm_proto::SubmissionStatus::NeedsInput {
            let pane = self.relays[index].role_panes[self.relays[index].state.cur];
            let invocation = self
                .panes
                .get(&pane)
                .and_then(|pane| pane.agent.as_ref())
                .and_then(|agent| agent.foreground_pid)
                .or_else(|| self.panes.get(&pane).and_then(|pane| pane.foreground_pid));
            let summary = if summary.trim().is_empty() {
                "relay role needs input".to_string()
            } else {
                summary
            };
            let change =
                self.waiting
                    .request(pane, invocation, uniterm_core::WaitingKind::Relay, &summary);
            self.record_waiting_change(change);
            self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
                run: self.relays[index].durable(),
            });
            return;
        }
        let active_pane = self.relays[index].role_panes[self.relays[index].state.cur];
        if let Some(item) = self
            .waiting
            .resolve_pane_kind(active_pane, uniterm_core::WaitingKind::Relay)
        {
            self.append_event(crate::eventlog::LogEvent::WaitingResolved {
                id: item.id,
                resolution: uniterm_core::WaitingResolution::AgentAdvanced,
            });
        }
        let mut relay = self.relays.swap_remove(index);
        let event = Event::Submit(Submit {
            token,
            role: relay.state.cur,
            status: match status {
                uniterm_proto::SubmissionStatus::Done => SubmitStatus::Done,
                uniterm_proto::SubmissionStatus::NeedsInput => SubmitStatus::NeedsInput,
                uniterm_proto::SubmissionStatus::Failed => SubmitStatus::Failed,
            },
            verdict: None,
        });
        let action = decide_relay_next(&mut relay.state, event);
        let action = self.scope_activation_token(&mut relay.state, action);
        let handoff = (!summary.is_empty()).then_some(summary.as_str());
        self.apply_relay_action(reg, &mut relay, action, handoff);
        if matches!(relay.state.phase, Phase::Awaiting | Phase::Paused) {
            self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
                run: relay.durable(),
            });
        }
        if matches!(relay.state.phase, Phase::Awaiting | Phase::Paused) {
            self.relays.push(relay);
        }
    }

    pub(super) fn resume_waiting_orchestration(&mut self, reg: &Registry, pane: PaneId) {
        if let Some(index) = self.workflows.iter().position(|run| {
            run.role_panes.get(run.state.cur).copied() == Some(pane)
                && matches!(
                    run.state.phase,
                    uniterm_core::orchestrate::Phase::Awaiting
                        | uniterm_core::orchestrate::Phase::Paused
                )
        }) {
            let mut run = self.workflows.swap_remove(index);
            let action = if run.state.phase == uniterm_core::orchestrate::Phase::Paused {
                run.elapsed_deadline = super::guardrail::elapsed_deadline(
                    run.started_at_ms,
                    run.guard_limits,
                    run.elapsed_guard_triggered,
                );
                let action = uniterm_core::orchestrate::decide_workflow_next(
                    &mut run.state,
                    uniterm_core::orchestrate::Event::Resume,
                );
                self.scope_activation_token(&mut run.state, action)
            } else {
                uniterm_core::orchestrate::Action::Inject {
                    role: run.state.cur,
                    token: run.state.token,
                }
            };
            if self
                .tasks
                .set_status(run.task_id, uniterm_core::TaskStatus::Doing)
            {
                self.append_event(crate::eventlog::LogEvent::TaskStatusChanged {
                    id: run.task_id,
                    status: uniterm_core::TaskStatus::Doing,
                });
            }
            self.apply_workflow_action(reg, &mut run, action, None);
            self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
                run: run.durable(),
            });
            self.workflows.push(run);
            return;
        }
        if let Some(index) = self.relays.iter().position(|run| {
            run.role_panes.get(run.state.cur).copied() == Some(pane)
                && matches!(
                    run.state.phase,
                    uniterm_core::orchestrate::Phase::Awaiting
                        | uniterm_core::orchestrate::Phase::Paused
                )
        }) {
            let mut run = self.relays.swap_remove(index);
            let action = if run.state.phase == uniterm_core::orchestrate::Phase::Paused {
                run.elapsed_deadline = super::guardrail::elapsed_deadline(
                    run.started_at_ms,
                    run.guard_limits,
                    run.elapsed_guard_triggered,
                );
                let action = uniterm_core::orchestrate::decide_relay_next(
                    &mut run.state,
                    uniterm_core::orchestrate::Event::Resume,
                );
                self.scope_activation_token(&mut run.state, action)
            } else {
                uniterm_core::orchestrate::Action::Inject {
                    role: run.state.cur,
                    token: run.state.token,
                }
            };
            if self
                .tasks
                .set_status(run.task_id, uniterm_core::TaskStatus::Doing)
            {
                self.append_event(crate::eventlog::LogEvent::TaskStatusChanged {
                    id: run.task_id,
                    status: uniterm_core::TaskStatus::Doing,
                });
            }
            self.apply_relay_action(reg, &mut run, action, None);
            self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
                run: run.durable(),
            });
            self.relays.push(run);
        }
    }

    pub(super) fn arm_orchestration_deadline(&mut self, pane: PaneId) {
        let due = std::time::Instant::now() + ORCHESTRATION_STALL_TIMEOUT;
        if let Some(run) = self.workflows.iter_mut().find(|run| {
            run.role_panes.get(run.state.cur).copied() == Some(pane)
                && run.state.phase == uniterm_core::orchestrate::Phase::Awaiting
        }) {
            run.stall_deadline = Some(due);
        }
        if let Some(run) = self.relays.iter_mut().find(|run| {
            run.role_panes.get(run.state.cur).copied() == Some(pane)
                && run.state.phase == uniterm_core::orchestrate::Phase::Awaiting
        }) {
            run.stall_deadline = Some(due);
        }
    }

    pub(super) fn restore_orchestrations(
        &mut self,
        runs: Vec<crate::eventlog::DurableOrchestration>,
    ) {
        let mut tokens = std::collections::HashSet::new();
        for run in runs {
            if let Some(reason) =
                recovered_run_shape_error(&run, |pane| self.panes.contains_key(&pane))
            {
                self.reject_recovered_orchestration(&run, reason);
                continue;
            }
            let role_providers = recovered_role_providers(&run);
            let provider_mismatch =
                run.role_panes
                    .iter()
                    .zip(&role_providers)
                    .any(|(pane, provider)| {
                        let observed = self
                            .panes
                            .get(pane)
                            .and_then(|pane| pane.agent.as_ref())
                            .map(|agent| agent.id.as_str());
                        restored_provider_conflicts(&provider.id, observed)
                    });
            if provider_mismatch {
                self.reject_recovered_orchestration(
                    &run,
                    "a restored Pane was owned by a different provider",
                );
                continue;
            }
            if !tokens.insert(run.state.token) {
                self.reject_recovered_orchestration(
                    &run,
                    "activation token duplicated another recovered run",
                );
                continue;
            }
            let current_pane = run.role_panes[run.state.cur];
            let started: Vec<bool> = run
                .role_panes
                .iter()
                .zip(&role_providers)
                .map(|(pane, provider)| {
                    self.panes
                        .get(pane)
                        .and_then(|pane| pane.agent.as_ref())
                        .is_some_and(|agent| agent.id == provider.id)
                })
                .collect();
            let safely_resumed = started[run.state.cur];
            let started_at_ms = if run.guardrail.started_at_ms == 0 {
                super::guardrail::unix_time_ms()
            } else {
                run.guardrail.started_at_ms
            };
            let mut guard_limits = run.guardrail.limits;
            if run.guardrail.started_at_ms == 0 {
                guard_limits.max_iterations = run.state.max_iterations;
            }
            let elapsed_guard_triggered = run.guardrail.elapsed_triggered;
            let elapsed_deadline = (run.state.phase == uniterm_core::orchestrate::Phase::Awaiting)
                .then(|| {
                    super::guardrail::elapsed_deadline(
                        started_at_ms,
                        guard_limits,
                        elapsed_guard_triggered,
                    )
                })
                .flatten();
            match run.kind {
                uniterm_proto::OrchestrationKind::Workflow => {
                    let template = run
                        .template
                        .as_deref()
                        .and_then(uniterm_core::orchestrate::workflow_template)
                        .expect("recovered workflow shape validated its template");
                    self.workflows.push(ActiveWorkflow {
                        state: run.state,
                        template,
                        goal: run.goal,
                        role_providers,
                        role_panes: run.role_panes,
                        started,
                        task_id: run.task_id,
                        started_at_ms,
                        guard_limits,
                        elapsed_guard_triggered,
                        elapsed_deadline,
                        stall_deadline: safely_resumed
                            .then(|| std::time::Instant::now() + ORCHESTRATION_STALL_TIMEOUT),
                        idle_deadline: None,
                    });
                }
                uniterm_proto::OrchestrationKind::Relay => {
                    self.relays.push(ActiveRelay {
                        state: run.state,
                        goal: run.goal,
                        role_providers,
                        role_panes: run.role_panes,
                        started,
                        task_id: run.task_id,
                        checkpoints: run.checkpoints,
                        started_at_ms,
                        guard_limits,
                        elapsed_guard_triggered,
                        elapsed_deadline,
                        stall_deadline: safely_resumed
                            .then(|| std::time::Instant::now() + ORCHESTRATION_STALL_TIMEOUT),
                        idle_deadline: None,
                    });
                }
            }
            if !safely_resumed
                && !self
                    .waiting
                    .items()
                    .iter()
                    .any(|item| item.pane == current_pane)
            {
                let kind = match run.kind {
                    uniterm_proto::OrchestrationKind::Workflow => {
                        uniterm_core::WaitingKind::Workflow
                    }
                    uniterm_proto::OrchestrationKind::Relay => uniterm_core::WaitingKind::Relay,
                };
                let change = self.waiting.request(
                    current_pane,
                    None,
                    kind,
                    "run recovered, but the provider supplied no safe native resume command; resume to start a fresh role invocation",
                );
                self.record_waiting_change(change);
            }
        }
    }

    pub(super) fn reject_recovered_orchestration(
        &mut self,
        run: &crate::eventlog::DurableOrchestration,
        reason: &str,
    ) {
        self.record_orchestration_terminal(
            run.task_id,
            uniterm_core::RunStatus::Failed,
            &format!("recovery rejected: {reason}"),
        );
        self.append_event(crate::eventlog::LogEvent::OrchestrationFinished {
            kind: run.kind,
            task_id: run.task_id,
            outcome: format!("recovery rejected: {reason}"),
        });
        if self
            .tasks
            .set_status(run.task_id, uniterm_core::TaskStatus::Blocked)
        {
            self.append_event(crate::eventlog::LogEvent::TaskStatusChanged {
                id: run.task_id,
                status: uniterm_core::TaskStatus::Blocked,
            });
        }
    }

    pub(super) fn cancel_orchestrations_for_pane(&mut self, pane: PaneId) {
        let mut workflow_index = 0;
        while workflow_index < self.workflows.len() {
            if !self.workflows[workflow_index].role_panes.contains(&pane) {
                workflow_index += 1;
                continue;
            }
            let run = self.workflows.swap_remove(workflow_index);
            self.pending_orchestration_submissions
                .retain(|pending| pending.task_id != run.task_id);
            self.record_orchestration_terminal(
                run.task_id,
                uniterm_core::RunStatus::Canceled,
                "role Pane closed",
            );
            self.append_event(crate::eventlog::LogEvent::OrchestrationFinished {
                kind: uniterm_proto::OrchestrationKind::Workflow,
                task_id: run.task_id,
                outcome: "role Pane closed".into(),
            });
            if self
                .tasks
                .set_status(run.task_id, uniterm_core::TaskStatus::Blocked)
            {
                self.append_event(crate::eventlog::LogEvent::TaskStatusChanged {
                    id: run.task_id,
                    status: uniterm_core::TaskStatus::Blocked,
                });
            }
        }
        let mut relay_index = 0;
        while relay_index < self.relays.len() {
            if !self.relays[relay_index].role_panes.contains(&pane) {
                relay_index += 1;
                continue;
            }
            let run = self.relays.swap_remove(relay_index);
            self.pending_orchestration_submissions
                .retain(|pending| pending.task_id != run.task_id);
            self.record_orchestration_terminal(
                run.task_id,
                uniterm_core::RunStatus::Canceled,
                "role Pane closed",
            );
            self.append_event(crate::eventlog::LogEvent::OrchestrationFinished {
                kind: uniterm_proto::OrchestrationKind::Relay,
                task_id: run.task_id,
                outcome: "role Pane closed".into(),
            });
            if self
                .tasks
                .set_status(run.task_id, uniterm_core::TaskStatus::Blocked)
            {
                self.append_event(crate::eventlog::LogEvent::TaskStatusChanged {
                    id: run.task_id,
                    status: uniterm_core::TaskStatus::Blocked,
                });
            }
            self.pending_relay_activations
                .retain(|pending| pending.task_id != run.task_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn durable_workflow(
        phase: uniterm_core::orchestrate::Phase,
    ) -> crate::eventlog::DurableOrchestration {
        let template = uniterm_core::orchestrate::workflow_template("solo").unwrap();
        let mut state = uniterm_core::orchestrate::State::new(template.engine_roles(), 1);
        uniterm_core::orchestrate::step(&mut state, uniterm_core::orchestrate::Event::Start);
        state.phase = phase;
        crate::eventlog::DurableOrchestration {
            kind: uniterm_proto::OrchestrationKind::Workflow,
            task_id: 7,
            template: Some("solo".into()),
            goal: "test recovery".into(),
            role_providers: Vec::new(),
            agent_id: "fake".into(),
            agent_cmd: "fake".into(),
            role_panes: vec![PaneId(11)],
            started: vec![true],
            state,
            checkpoints: Vec::new(),
            guardrail: Box::default(),
        }
    }

    #[test]
    fn recovered_runs_accept_only_restartable_phases_and_valid_shapes() {
        use uniterm_core::orchestrate::Phase;

        for phase in [Phase::Awaiting, Phase::Paused] {
            let workflow = durable_workflow(phase);
            assert_eq!(
                recovered_run_shape_error(&workflow, |pane| pane == PaneId(11)),
                None
            );

            let mut relay = workflow.clone();
            relay.kind = uniterm_proto::OrchestrationKind::Relay;
            relay.template = None;
            relay
                .state
                .roles
                .push(uniterm_core::orchestrate::Role::new("reviewer", true));
            relay.role_panes.push(PaneId(12));
            relay.started.push(false);
            assert_eq!(recovered_run_shape_error(&relay, |_| true), None);
        }

        for phase in [Phase::Idle, Phase::Done, Phase::Aborted] {
            assert!(recovered_run_shape_error(&durable_workflow(phase), |_| true).is_some());
        }

        let mut invalid = durable_workflow(Phase::Awaiting);
        invalid.template = Some("missing".into());
        assert!(recovered_run_shape_error(&invalid, |_| true).is_some());
        invalid = durable_workflow(Phase::Awaiting);
        invalid.state.token = 0;
        assert!(recovered_run_shape_error(&invalid, |_| true).is_some());
        invalid = durable_workflow(Phase::Awaiting);
        invalid.started.clear();
        assert!(recovered_run_shape_error(&invalid, |_| true).is_some());
        invalid = durable_workflow(Phase::Awaiting);
        invalid.guardrail.limits.max_active_runs = 0;
        assert_eq!(
            recovered_run_shape_error(&invalid, |_| true),
            Some("run contained invalid guardrail limits")
        );
        invalid = durable_workflow(Phase::Awaiting);
        invalid.guardrail.started_at_ms = 1;
        assert_eq!(
            recovered_run_shape_error(&invalid, |_| true),
            Some("run guardrail and state-machine iteration limits disagreed")
        );
        invalid = durable_workflow(Phase::Awaiting);
        assert!(recovered_run_shape_error(&invalid, |_| false).is_some());
    }

    #[test]
    fn recovery_preserves_mixed_providers_and_migrates_legacy_scalar_ownership() {
        let mut run = durable_workflow(uniterm_core::orchestrate::Phase::Awaiting);
        run.state
            .roles
            .push(uniterm_core::orchestrate::Role::new("verifier", true));
        run.role_panes.push(PaneId(12));
        run.started.push(false);
        run.kind = uniterm_proto::OrchestrationKind::Relay;
        run.template = None;
        let legacy = recovered_role_providers(&run);
        assert_eq!(legacy.len(), 2);
        assert!(legacy.iter().all(|provider| provider.id == "fake"));

        run.role_providers = vec![
            crate::eventlog::DurableRoleProvider {
                provider: "provider-a".into(),
                command: "agent-a".into(),
            },
            crate::eventlog::DurableRoleProvider {
                provider: "provider-b".into(),
                command: "agent-b".into(),
            },
        ];
        let recovered = recovered_role_providers(&run);
        assert_eq!(recovered[0].id, "provider-a");
        assert_eq!(recovered[0].command, "agent-a");
        assert_eq!(recovered[1].id, "provider-b");
        assert_eq!(recovered[1].command, "agent-b");
        assert_eq!(recovered_run_shape_error(&run, |_| true), None);
        assert!(!restored_provider_conflicts("provider-a", None));
        assert!(!restored_provider_conflicts(
            "provider-a",
            Some("provider-a")
        ));
        assert!(restored_provider_conflicts(
            "provider-a",
            Some("provider-b")
        ));
    }

    #[test]
    fn idle_fallback_never_bypasses_human_or_artifact_gates() {
        assert!(orchestration_idle_allowed(false, false, false, false));
        assert!(!orchestration_idle_allowed(true, false, false, false));
        assert!(!orchestration_idle_allowed(false, true, false, false));
        assert!(!orchestration_idle_allowed(false, false, true, false));
        assert!(!orchestration_idle_allowed(false, false, false, true));
    }

    #[test]
    fn orchestration_deadlines_disarm_to_no_wakeup() {
        let template = uniterm_core::orchestrate::workflow_template("solo").unwrap();
        let due = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut workflows = vec![ActiveWorkflow {
            state: durable_workflow(uniterm_core::orchestrate::Phase::Awaiting).state,
            template,
            goal: "test deadlines".into(),
            role_providers: vec![crate::workflow::ResolvedRoleProvider {
                id: "fake".into(),
                command: "fake".into(),
            }],
            role_panes: vec![PaneId(11)],
            started: vec![true],
            task_id: 7,
            started_at_ms: super::super::guardrail::unix_time_ms(),
            guard_limits: uniterm_core::GuardLimits::default(),
            elapsed_guard_triggered: true,
            elapsed_deadline: None,
            stall_deadline: Some(due),
            idle_deadline: None,
        }];
        let mut submissions = vec![PendingOrchestrationSubmission {
            kind: uniterm_proto::OrchestrationKind::Workflow,
            task_id: 7,
            token: 1,
            pane: PaneId(11),
            status: uniterm_proto::SubmissionStatus::Done,
            verdict: None,
            summary: String::new(),
            due,
        }];
        assert_eq!(
            next_orchestration_deadline(&submissions, &[], &workflows, &[]),
            Some(due)
        );

        submissions.clear();
        workflows[0].stall_deadline = None;
        assert_eq!(
            next_orchestration_deadline(&submissions, &[], &workflows, &[]),
            None
        );

        workflows[0].elapsed_deadline = Some(due);
        assert_eq!(
            next_orchestration_deadline(&[], &[], &workflows, &[]),
            Some(due)
        );
        workflows[0].state.phase = uniterm_core::orchestrate::Phase::Paused;
        assert_eq!(next_orchestration_deadline(&[], &[], &workflows, &[]), None);
    }
}
