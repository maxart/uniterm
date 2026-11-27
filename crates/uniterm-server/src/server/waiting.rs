//! Workspace-scoped waiting projection and semantic queue commands.
//!
//! Waiting items are event-backed human-attention work. Instruction steering
//! remains a separate projection in `server/instruction.rs`.

use super::*;

/// A one-shot readiness delay for a specific provider selection and invocation.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct PendingApproval {
    invocation: u64,
    prompt: u64,
    selection: Option<u64>,
    #[serde(with = "super::monotonic")]
    pub(super) due: std::time::Instant,
}

impl Server {
    pub(super) fn flush_approvals_due(&mut self, reg: &Registry) {
        let now = std::time::Instant::now();
        let due: Vec<_> = self
            .pending_approval
            .iter()
            .filter_map(|(&pane, pending)| (pending.due <= now).then_some(pane))
            .collect();
        for pane in due {
            self.maybe_auto_approve(reg, pane);
        }
    }

    /// Both attached clients and the control API use the same immediate toggle.
    pub(super) fn set_auto_approve(&mut self, reg: &Registry, enabled: Option<bool>) {
        let Some(enabled) = enabled else {
            return;
        };
        let had_arms = !self.delegated.is_empty();
        if !enabled {
            self.disarm_all_delegations();
        }
        if enabled == self.auto_approval.enabled() && (enabled || !had_arms) {
            return;
        }
        if enabled != self.auto_approval.enabled() {
            self.append_event(crate::eventlog::LogEvent::AutoApprovalChanged { enabled });
        }
        self.auto_approval.set_enabled(enabled);
        let frame = encode_frame(&ServerMessage::AutoApproval {
            enabled,
            error: None,
        });
        for (token, client) in &mut self.clients {
            client.queue(&frame);
            client.flush();
            let _ = set_interest(reg, client, *token);
        }
        self.repaint_chrome_all(reg);
        if enabled {
            let panes: Vec<_> = self
                .panes
                .iter()
                .filter(|(_, pane)| pane.agent.is_some())
                .map(|(&id, _)| id)
                .collect();
            for pane in panes {
                self.maybe_auto_approve(reg, pane);
            }
        }
    }

    /// Apply independent trust and tool policies to a freshly observed screen.
    pub(super) fn maybe_auto_approve(&mut self, reg: &Registry, pane: PaneId) {
        let permission_automation = !self.delegation_suppressed(pane)
            && (self.auto_approval.enabled() || self.delegated.contains_key(&pane));
        let continuation = self.auto_approval.pending_manual(pane);
        if permission_automation || continuation || self.config.auto_trust_projects {
            self.approve_permission(reg, pane, permission_automation, continuation, true);
        } else {
            self.pending_approval.remove(&pane);
        }
    }

    fn approve_permission(
        &mut self,
        reg: &Registry,
        pane: PaneId,
        permission_automation: bool,
        continuation: bool,
        automatic_request: bool,
    ) -> bool {
        let pending = self.pending_approval.remove(&pane);
        // A deadline can coincide with a backpressured or budget-limited PTY
        // read. Its normal drain will inspect the completed screen.
        if self
            .panes
            .get(&pane)
            .is_some_and(|target| self.pending_reads.contains(&target.token))
        {
            return false;
        }
        if self.durability_error.is_some() || !self.event_writes_enabled {
            return false;
        }
        let Some(target) = self.panes.get(&pane) else {
            return false;
        };
        let Some(agent) = target.agent.as_ref() else {
            return false;
        };
        if target.input.len() != target.input_offset || target.term.synchronized_output() {
            return false;
        }
        // Recheck the kernel's owner, not only the cached detection pid.
        let Some(pid) = agent.foreground_pid.filter(|pid| *pid > 0) else {
            return false;
        };
        if target.pty.foreground_process_group() != Some(pid) {
            return false;
        }
        let Some(&invocation) = self.timeline_invocations.get(&pane) else {
            return false;
        };
        let Some(choice) = crate::providers::permission_approval(&agent.id, &target.term) else {
            if self.auto_approval.awaiting_dismissal(pane, invocation)
                && crate::providers::permission_dismissed(&agent.id, &target.term)
            {
                self.auto_approval.dismissed(pane, invocation);
            }
            return false;
        };
        let automatic = automatic_request && !continuation;
        if automatic {
            match choice.kind {
                crate::providers::ApprovalKind::Permission if !permission_automation => {
                    return false
                }
                crate::providers::ApprovalKind::WorkspaceTrust => {
                    if !self.config.auto_trust_projects {
                        return false;
                    }
                    let Some(directory) = choice.directory.as_deref() else {
                        return false;
                    };
                    // Exact absolute paths fail closed for aliases, symlinks,
                    // subdirectories and unknown roots, without filesystem I/O.
                    if self.project_root_for_pane(pane).as_deref() != Some(directory) {
                        return false;
                    }
                }
                _ => {}
            }
        }
        if !automatic && agent.status != AgentStatus::Permission {
            return false;
        }
        if continuation
            && !self
                .auto_approval
                .can_continue_manual(pane, invocation, choice.fingerprint)
        {
            return false;
        }
        let manual_retry = !automatic_request
            && (!choice.settle.is_zero() || choice.watch_for_dismissal)
            && self
                .auto_approval
                .can_retry_automatic(pane, invocation, choice.fingerprint);
        if !manual_retry
            && !self
                .auto_approval
                .is_new(pane, invocation, choice.fingerprint, choice.navigation)
        {
            return false;
        }
        if automatic
            && permission_automation
            && choice.kind == crate::providers::ApprovalKind::Permission
            && !self.delegation_allows(pane, choice.fingerprint)
        {
            return false;
        }
        if automatic && !choice.settle.is_zero() {
            let now = std::time::Instant::now();
            let pending = pending
                .filter(|pending| {
                    pending.invocation == invocation
                        && pending.prompt == choice.fingerprint
                        && pending.selection == choice.navigation
                })
                .unwrap_or_else(|| PendingApproval {
                    invocation,
                    prompt: choice.fingerprint,
                    selection: choice.navigation,
                    due: now + choice.settle,
                });
            if pending.due > now {
                self.pending_approval.insert(pane, pending);
                return false;
            }
        }
        self.append_event(crate::eventlog::LogEvent::PermissionApprovalAttempt {
            pane,
            invocation,
            automatic,
        });
        let accepted = self
            .panes
            .get_mut(&pane)
            .is_some_and(|target| Self::queue_pane_input(reg, target, &choice.keys));
        self.append_event(crate::eventlog::LogEvent::PermissionApprovalDelivery {
            pane,
            invocation,
            accepted,
        });
        if accepted {
            if automatic
                && permission_automation
                && choice.kind == crate::providers::ApprovalKind::Permission
                && choice.navigation.is_none()
            {
                self.delegation_delivered(pane);
            }
            self.auto_approval.delivered(
                pane,
                invocation,
                choice.fingerprint,
                choice.navigation,
                automatic,
                choice.watch_for_dismissal,
            );
            // Leave the item pending until actual provider progress confirms
            // that the dialog closed. Queue acceptance is not approval success.
        }
        accepted
    }

    pub(super) fn sync_waiting_agent(&mut self, pane: PaneId, status: AgentStatus) {
        let Some(pane_state) = self.panes.get(&pane) else {
            return;
        };
        let invocation = pane_state
            .agent
            .as_ref()
            .and_then(|agent| agent.foreground_pid)
            .or(pane_state.foreground_pid);
        let summary = pane_state
            .agent
            .as_ref()
            .map(|agent| {
                let name = uniterm_core::agent::agent_name(&agent.id);
                if let Some(permission) = agent
                    .details
                    .pending_permission
                    .as_ref()
                    .filter(|_| status == AgentStatus::Permission)
                {
                    if permission.preview.is_empty() {
                        format!("{name} asks to use {}", permission.tool)
                    } else {
                        format!(
                            "{name} asks to use {}: {}",
                            permission.tool, permission.preview
                        )
                    }
                } else if agent.evidence.is_empty() {
                    format!("{name} is waiting for {}", status.label())
                } else {
                    format!("{name}: {}", agent.evidence)
                }
            })
            .unwrap_or_else(|| format!("Pane {} is waiting for {}", pane.0, status.label()));
        let change = self
            .waiting
            .observe_agent(pane, invocation, status, &summary);
        self.record_waiting_change(change);
    }

    /// Fresh provider details may update a waiting item without a status change.
    /// Answer and dismiss must not call this on unchanged cached evidence.
    pub(super) fn refresh_waiting_for_pane(&mut self, pane: PaneId) {
        if let Some(status) = self
            .panes
            .get(&pane)
            .and_then(|pane| pane.agent.as_ref())
            .map(|agent| agent.status)
        {
            self.sync_waiting_agent(pane, status);
        }
    }

    pub(super) fn record_waiting_change(&mut self, change: uniterm_core::WaitingChange) {
        match change {
            uniterm_core::WaitingChange::None => {}
            uniterm_core::WaitingChange::Updated { id, summary } => {
                self.append_event(crate::eventlog::LogEvent::WaitingUpdated { id, summary });
            }
            uniterm_core::WaitingChange::Created(item) => {
                self.append_event(crate::eventlog::LogEvent::WaitingCreated { item });
            }
            uniterm_core::WaitingChange::Replaced { resolved, created } => {
                self.append_event(crate::eventlog::LogEvent::WaitingResolved {
                    id: resolved,
                    resolution: uniterm_core::WaitingResolution::AgentAdvanced,
                });
                self.append_event(crate::eventlog::LogEvent::WaitingCreated { item: created });
            }
            uniterm_core::WaitingChange::Resolved { id, resolution } => {
                self.append_event(crate::eventlog::LogEvent::WaitingResolved { id, resolution });
            }
        }
    }

    pub(super) fn resolve_waiting_for_pane(
        &mut self,
        pane: PaneId,
        resolution: uniterm_core::WaitingResolution,
    ) {
        if let Some(item) = self.waiting.resolve_pane(pane) {
            self.append_event(crate::eventlog::LogEvent::WaitingResolved {
                id: item.id,
                resolution,
            });
        }
    }

    /// The active waiting item for one Pane, resolved from the queue that
    /// owns it rather than by scanning other Panes.
    pub(super) fn waiting_id_for(&self, pane: PaneId) -> Option<u64> {
        self.waiting
            .items()
            .iter()
            .find(|item| item.pane == pane)
            .map(|item| item.id)
    }

    pub(super) fn waiting_snapshot(&self) -> Vec<uniterm_proto::WaitingEntry> {
        let mut entries = Vec::new();
        for item in self.waiting.items() {
            if self.overwatch_chat_pane() == Some(item.pane) {
                entries.push(uniterm_proto::WaitingEntry {
                    id: item.id,
                    pane: item.pane,
                    kind: item.kind,
                    summary: item.summary.clone(),
                    agent: self
                        .panes
                        .get(&item.pane)
                        .and_then(|pane| pane.agent.as_ref())
                        .map(|agent| agent.id.clone()),
                    project: None,
                    project_name: "Overwatch".into(),
                    tab: 0,
                });
                continue;
            }
            let Some((window_index, window)) = self
                .windows
                .iter()
                .enumerate()
                .find(|(_, window)| window.layout.contains_pane(item.pane))
            else {
                continue;
            };
            let Some(project) = self
                .projects
                .iter()
                .find(|project| project.id == window.project)
            else {
                continue;
            };
            let tab = self
                .project_window_indices(window.project)
                .iter()
                .position(|index| *index == window_index)
                .unwrap_or(0) as u32
                + 1;
            entries.push(uniterm_proto::WaitingEntry {
                id: item.id,
                pane: item.pane,
                kind: item.kind,
                summary: item.summary.clone(),
                agent: self
                    .panes
                    .get(&item.pane)
                    .and_then(|pane| pane.agent.as_ref())
                    .map(|agent| agent.id.clone()),
                project: Some(project.id),
                project_name: project.name.clone(),
                tab,
            });
        }
        entries.sort_by_key(|item| item.id);
        entries
    }

    pub(super) fn reply_waiting(&mut self, reg: &Registry, token: Token) {
        let items = self.waiting_snapshot();
        if let Some(client) = self.clients.get_mut(&token) {
            client.queue(&encode_frame(&ServerMessage::Waiting { items }));
            client.flush();
            let _ = set_interest(reg, client, token);
        }
    }

    pub(super) fn waiting_action(
        &mut self,
        reg: &Registry,
        id: u64,
        action: uniterm_proto::WaitingAction,
        text: &str,
    ) -> (bool, bool) {
        let Some(item) = self.waiting.get(id).cloned() else {
            return (false, false);
        };
        let current_invocation = self
            .panes
            .get(&item.pane)
            .and_then(|pane| pane.agent.as_ref())
            .and_then(|agent| agent.foreground_pid)
            .or_else(|| {
                self.panes
                    .get(&item.pane)
                    .and_then(|pane| pane.foreground_pid)
            });
        if item.invocation.is_some() && item.invocation != current_invocation {
            self.resolve_waiting_for_pane(
                item.pane,
                uniterm_core::WaitingResolution::AgentAdvanced,
            );
            return (true, false);
        }
        let orchestration_phase = self
            .workflows
            .iter()
            .find(|run| run.role_panes.get(run.state.cur).copied() == Some(item.pane))
            .map(|run| run.state.phase)
            .or_else(|| {
                self.relays
                    .iter()
                    .find(|run| run.role_panes.get(run.state.cur).copied() == Some(item.pane))
                    .map(|run| run.state.phase)
            });
        match action {
            uniterm_proto::WaitingAction::Approve => (
                true,
                item.kind == uniterm_core::WaitingKind::Permission
                    && self.approve_permission(reg, item.pane, false, false, false),
            ),
            uniterm_proto::WaitingAction::Focus => (true, self.focus_pane_target(reg, item.pane)),
            uniterm_proto::WaitingAction::Answer => {
                // Answering types into the Pane, so it is guarded terminal
                // delivery, not a provider approval protocol: revalidate that
                // the prompt this item describes is still the one on screen.
                let current_status = self
                    .panes
                    .get(&item.pane)
                    .and_then(|pane| pane.agent.as_ref())
                    .map(|agent| agent.status);
                if matches!(
                    item.kind,
                    uniterm_core::WaitingKind::Permission | uniterm_core::WaitingKind::Question
                ) && current_status.and_then(uniterm_core::WaitingKind::from_agent_status)
                    != Some(item.kind)
                {
                    self.resolve_waiting_for_pane(
                        item.pane,
                        uniterm_core::WaitingResolution::AgentAdvanced,
                    );
                    return (true, false);
                }
                if matches!(
                    item.kind,
                    uniterm_core::WaitingKind::Workflow | uniterm_core::WaitingKind::Relay
                ) && orchestration_phase != Some(uniterm_core::orchestrate::Phase::Awaiting)
                {
                    return (true, false);
                }
                let answer: String = text
                    .chars()
                    .filter(|character| *character != '\0')
                    .take(16_384)
                    .collect();
                if answer.trim().is_empty() {
                    return (true, false);
                }
                let accepted = if let Some(pane) = self.panes.get_mut(&item.pane) {
                    let bytes = super::io::paste_input(&answer, pane.term.bracketed_paste(), true);
                    Self::queue_pane_input(reg, pane, &bytes)
                } else {
                    false
                };
                if accepted {
                    self.resolve_waiting_for_pane(
                        item.pane,
                        uniterm_core::WaitingResolution::Answered,
                    );
                    self.arm_orchestration_deadline(item.pane);
                }
                (true, accepted)
            }
            uniterm_proto::WaitingAction::Dismiss => {
                if matches!(
                    item.kind,
                    uniterm_core::WaitingKind::Workflow | uniterm_core::WaitingKind::Relay
                ) {
                    return (true, false);
                }
                self.resolve_waiting_for_pane(
                    item.pane,
                    uniterm_core::WaitingResolution::Dismissed,
                );
                (true, true)
            }
            uniterm_proto::WaitingAction::Stop => {
                self.resolve_waiting_for_pane(item.pane, uniterm_core::WaitingResolution::Stopped);
                let panes = self
                    .workflows
                    .iter()
                    .find(|run| run.role_panes.contains(&item.pane))
                    .map(|run| run.role_panes.clone())
                    .or_else(|| {
                        self.relays
                            .iter()
                            .find(|run| run.role_panes.contains(&item.pane))
                            .map(|run| run.role_panes.clone())
                    })
                    .unwrap_or_else(|| vec![item.pane]);
                self.terminate_panes(&panes);
                for pane in panes {
                    if self.panes.contains_key(&pane) {
                        self.close_pane(reg, pane);
                    }
                }
                (true, true)
            }
            uniterm_proto::WaitingAction::Resume => {
                if orchestration_phase.is_none() {
                    return (true, false);
                }
                if let Some(item) = self.waiting.resolve(id) {
                    self.append_event(crate::eventlog::LogEvent::WaitingResolved {
                        id: item.id,
                        resolution: uniterm_core::WaitingResolution::Resumed,
                    });
                    self.resume_waiting_orchestration(reg, item.pane);
                }
                (true, true)
            }
            uniterm_proto::WaitingAction::Rollback => {
                if item.kind != uniterm_core::WaitingKind::Relay {
                    return (true, false);
                }
                let Some((task_id, checkpoint)) = self
                    .relays
                    .iter()
                    .find(|run| run.role_panes.get(run.state.cur).copied() == Some(item.pane))
                    .and_then(|run| {
                        run.checkpoints
                            .last()
                            .map(|(_, checkpoint)| (run.task_id, checkpoint.clone()))
                    })
                else {
                    return (true, false);
                };
                let Some(project_root) = self.project_root_for_pane(item.pane) else {
                    return (true, false);
                };
                let stable_run = self.run_graph.run_for_task(task_id);
                let project = stable_run
                    .and_then(|id| self.run_graph.run(id))
                    .map(|record| record.project);
                let decision = uniterm_core::evaluate_semantic(
                    uniterm_core::GuardedCommand::CheckpointRollback,
                    true,
                );
                self.record_guardrail(uniterm_core::GuardrailRecord {
                    project,
                    run: stable_run,
                    action: uniterm_core::GuardAction::SemanticCommand {
                        command: uniterm_core::GuardedCommand::CheckpointRollback,
                        confirmed: true,
                    },
                    decision: decision.clone(),
                });
                if decision != uniterm_core::GuardDecision::Allow {
                    return (true, false);
                }
                self.agents
                    .send(uniterm_proto::CoreToAgent::RelayCheckpointRollback {
                        waiting_id: id,
                        task_id,
                        project_root,
                        checkpoint,
                    });
                (true, true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::reaper::common;
    use super::*;

    fn fixture() -> (Server, Poll, PaneId) {
        common::isolate_state();
        let name = common::unique_workspace_name();
        let dir = common::socket_root().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        // Approval keys go to a real PTY. Keep one stable foreground owner:
        // a shell can interpret a prior picker's "y" plus Enter as a new job.
        let (mut server, poll) = Server::bind_internal(
            &dir.join(format!("{name}.sock")),
            "/bin/cat",
            &[],
            120,
            30,
            false,
        )
        .unwrap();
        server.event_writes_enabled = true;
        let pane = server.windows[0].active;
        server.bind_agent(pane, "codex");
        (server, poll, pane)
    }

    #[test]
    fn codex_startup_trust_waits_once_and_revalidates_the_visible_project() {
        let (mut server, mut poll, pane) = fixture();
        server.projects[0].root = "/work".into();
        let capture = include_bytes!("../../tests/fixtures/approval/codex-acc-live.ansi");
        let terminal = &mut server.panes.get_mut(&pane).unwrap().term;
        terminal.resize(120, 80);
        terminal.feed(capture);
        terminal.feed(b"\x1b[79;1H");
        assert!(!terminal.evidence_text(48).contains("Folder access"));
        let evidence = crate::providers::status_evidence(terminal, Some("codex"));
        assert_eq!(
            crate::providers::Catalog::load_from_paths(None, None, None)
                .screen("codex", &evidence, "")
                .unwrap()
                .status,
            Some(AgentStatus::Permission)
        );

        server.maybe_auto_approve(poll.registry(), pane);
        let due = server.pending_approval[&pane].due;
        server.maybe_auto_approve(poll.registry(), pane);
        assert_eq!(server.pending_approval[&pane].due, due);
        assert!(!server.auto_approval.has_delivery(pane));

        // A different foreground surface can arrive during startup. It may not
        // receive a queued key just because the earlier picker was complete.
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .term
            .feed(b"\x1b[40;1H> Choose a database");
        server.pending_approval.get_mut(&pane).unwrap().due = std::time::Instant::now();
        server.flush_approvals_due(poll.registry());
        assert!(server.pending_approval.is_empty());
        assert!(!server.auto_approval.has_delivery(pane));

        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .term
            .feed(b"\x1b[40;1H\x1b[2K");
        server.maybe_auto_approve(poll.registry(), pane);
        server.projects[0].root = "/elsewhere".into();
        server.pending_approval.get_mut(&pane).unwrap().due = std::time::Instant::now();
        server.flush_approvals_due(poll.registry());
        assert!(server.pending_approval.is_empty());
        assert!(!server.auto_approval.has_delivery(pane));

        server.projects[0].root = "/work".into();
        server.maybe_auto_approve(poll.registry(), pane);
        server.pending_approval.get_mut(&pane).unwrap().due = std::time::Instant::now();
        server.flush_approvals_due(poll.registry());
        assert!(server.auto_approval.has_delivery(pane));
        let sequence = server.log.current_sequence();
        server.maybe_auto_approve(poll.registry(), pane);
        assert!(server.pending_approval.is_empty());
        assert_eq!(server.log.current_sequence(), sequence);
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn claude_clipped_bash_settles_then_delivers_once_when_the_panel_is_revealed() {
        let (mut server, mut poll, pane) = fixture();
        server.bind_agent(pane, "claude");
        server.auto_approval.set_enabled(true);
        let clipped = "Bash command \u{b7} from the general-purpose agent\r\nReset test directory\r\n\r\nDo you want to proceed?\r\n\u{276f} 1. Yes";
        let rest = b"\r\n  2. Yes, and don't ask again\r\n  3. No\r\nEsc to cancel";
        let paint = |server: &mut Server, bytes: &[u8]| {
            server.panes.get_mut(&pane).unwrap().term.feed(bytes);
        };
        let terminal = &mut server.panes.get_mut(&pane).unwrap().term;
        terminal.resize(120, 30);
        terminal.feed("\r\n".repeat(29).as_bytes());
        terminal.feed(clipped.as_bytes());

        // First attach of the clipped picture arms one deadline and sends nothing.
        let sequence = server.log.current_sequence();
        server.maybe_auto_approve(poll.registry(), pane);
        let due = server.pending_approval[&pane].due;
        server.maybe_auto_approve(poll.registry(), pane);
        assert_eq!(server.pending_approval[&pane].due, due);
        assert!(!server.auto_approval.has_delivery(pane));
        assert_eq!(server.log.current_sequence(), sequence);

        // A split frame completes before the deadline. The complete panel is
        // the same prompt and is submitted exactly once.
        paint(&mut server, rest);
        server.maybe_auto_approve(poll.registry(), pane);
        assert!(server.auto_approval.has_delivery(pane));
        let delivered = server.log.current_sequence();
        assert!(delivered > sequence);
        server.pending_approval.remove(&pane);
        for _ in 0..2 {
            server.maybe_auto_approve(poll.registry(), pane);
            server.flush_approvals_due(poll.registry());
        }
        assert_eq!(server.log.current_sequence(), delivered);

        // A picker that stays clipped is delivered at its deadline. Rows
        // revealed afterwards do not produce a second Enter.
        server.auto_approval.observe(pane, AgentStatus::Working);
        paint(&mut server, b"\x1b[2J\x1b[30;1H");
        paint(&mut server, "\r\n".repeat(4).as_bytes());
        paint(&mut server, clipped.as_bytes());
        server.maybe_auto_approve(poll.registry(), pane);
        assert!(!server.auto_approval.has_delivery(pane));
        server.pending_approval.get_mut(&pane).unwrap().due = std::time::Instant::now();
        server.flush_approvals_due(poll.registry());
        assert!(server.auto_approval.has_delivery(pane));
        let delivered = server.log.current_sequence();
        paint(&mut server, rest);
        server.maybe_auto_approve(poll.registry(), pane);
        server.flush_approvals_due(poll.registry());
        assert_eq!(server.log.current_sequence(), delivered);
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn native_picker_rearms_only_after_positive_dismissal_without_status_dwell() {
        let (mut server, mut poll, pane) = fixture();
        let reg = poll.registry();
        for (id, height, picker, dismissed) in [
            (
                "codex",
                24,
                include_bytes!("../../tests/fixtures/approval/codex-command-folded.ansi")
                    .as_slice(),
                include_bytes!("../../tests/fixtures/approval/codex-command-dismissed.ansi")
                    .as_slice(),
            ),
            (
                "cursor",
                24,
                include_bytes!("../../tests/fixtures/approval/cursor-command.ansi").as_slice(),
                include_bytes!("../../tests/fixtures/approval/cursor-composer.ansi").as_slice(),
            ),
            (
                "grok",
                24,
                include_bytes!("../../tests/fixtures/approval/grok-command-1.0.92.ansi").as_slice(),
                include_bytes!("../../tests/fixtures/approval/grok-composer.ansi").as_slice(),
            ),
            (
                "opencode",
                24,
                include_bytes!("../../tests/fixtures/approval/opencode-command-clipped.ansi")
                    .as_slice(),
                include_bytes!("../../tests/fixtures/approval/opencode-composer.ansi").as_slice(),
            ),
            (
                "kimi",
                32,
                include_bytes!("../../tests/fixtures/approval/kimi-command.ansi").as_slice(),
                include_bytes!("../../tests/fixtures/approval/kimi-idle-composer.ansi").as_slice(),
            ),
        ] {
            server.bind_agent(pane, id);
            server.auto_approval = Default::default();
            server.auto_approval.set_enabled(true);
            let target = server.panes.get_mut(&pane).unwrap();
            target.term = crate::terminal::Terminal::new(80, height);
            target.term.feed(picker);
            let original = crate::providers::permission_approval(id, &target.term)
                .unwrap_or_else(|| panic!("{id} picker: {}", target.term.evidence_text(48)))
                .fingerprint;
            let invocation = server.timeline_invocations[&pane];
            // Expiring readiness must never submit through an open synchronized frame.
            if id == "codex" {
                let before = server.log.current_sequence();
                server.maybe_auto_approve(reg, pane);
                server.pending_approval.get_mut(&pane).unwrap().due = std::time::Instant::now();
                server
                    .panes
                    .get_mut(&pane)
                    .unwrap()
                    .term
                    .feed(b"\x1b[?2026h");
                server.flush_approvals_due(reg);
                assert_eq!(server.log.current_sequence(), before);
                assert!(server.pending_approval.is_empty());
                server
                    .panes
                    .get_mut(&pane)
                    .unwrap()
                    .term
                    .feed(b"\x1b[?2026l");
            }
            for occurrence in 0..2 {
                let before = server.log.current_sequence();
                server.maybe_auto_approve(reg, pane);
                if let Some(pending) = server.pending_approval.get_mut(&pane) {
                    pending.due = std::time::Instant::now();
                    server.flush_approvals_due(reg);
                }
                assert_eq!(
                    server.log.current_sequence(),
                    before + 2,
                    "{id} occurrence {occurrence}"
                );
                assert!(server.auto_approval.awaiting_dismissal(pane, invocation));
                server.maybe_auto_approve(reg, pane);
                assert!(server.pending_approval.is_empty());
                assert_eq!(server.log.current_sequence(), before + 2);
                if occurrence == 1 {
                    break;
                }
                let term = &mut server.panes.get_mut(&pane).unwrap().term;
                term.feed(b"\x1b[?25h");
                server.maybe_auto_approve(reg, pane);
                assert!(
                    server.auto_approval.awaiting_dismissal(pane, invocation),
                    "cursor"
                );
                server
                    .panes
                    .get_mut(&pane)
                    .unwrap()
                    .term
                    .feed(b"\x1b[2J\x1b[H");
                server.maybe_auto_approve(reg, pane);
                assert!(
                    server.auto_approval.awaiting_dismissal(pane, invocation),
                    "blank"
                );
                let term = &mut server.panes.get_mut(&pane).unwrap().term;
                if id == "codex" {
                    term.feed(picker);
                    term.feed(dismissed.strip_suffix(b"\x1b[?2026l").unwrap());
                } else {
                    term.feed(b"\x1b[?2026h");
                    term.feed(dismissed);
                }
                assert!(term.synchronized_output());
                server.maybe_auto_approve(reg, pane);
                assert!(
                    server.auto_approval.awaiting_dismissal(pane, invocation),
                    "incomplete repaint"
                );
                server
                    .panes
                    .get_mut(&pane)
                    .unwrap()
                    .term
                    .feed(b"\x1b[?2026l");
                server.maybe_auto_approve(reg, pane);
                assert!(
                    !server.auto_approval.has_delivery(pane),
                    "{id} did not return to its composer"
                );
                let term = &mut server.panes.get_mut(&pane).unwrap().term;
                term.feed(b"\x1b[2J\x1b[H");
                term.feed(picker);
                assert_eq!(
                    crate::providers::permission_approval(id, term)
                        .unwrap()
                        .fingerprint,
                    original
                );
            }
        }
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn project_trust_is_scoped_independent_and_deduplicated() {
        let (mut server, mut poll, pane) = fixture();
        server.projects[0].root = "/Work/Project".into();
        for provider in uniterm_core::agent::PROVIDERS {
            let provider = crate::providers::launch_manifest(provider.id).unwrap();
            for provider in std::iter::once(provider.id).chain(provider.executables.iter().copied())
            {
                server.bind_agent(pane, provider);
                for trust in [false, true] {
                    for tools in [false, true] {
                        for directory in [
                            "/Work/Project",
                            "/Work",
                            "/other",
                            "",
                            "/work/project",
                            "/Work/Project/subdir",
                            "/Work/Project/../other",
                            "/Work/Project\r\n/other",
                        ] {
                            server.auto_approval = Default::default();
                            server.auto_approval.set_enabled(tools);
                            server.config.auto_trust_projects = trust;
                            let screen = format!("\x1b[2J\x1b[HTrust this workspace?\r\n{directory}\r\n❯ Yes, trust this workspace\r\nNo, exit\r\nEnter to confirm · Esc to cancel");
                            server
                                .panes
                                .get_mut(&pane)
                                .unwrap()
                                .term
                                .feed(screen.as_bytes());
                            server.maybe_auto_approve(poll.registry(), pane);
                            let expected = trust && directory == "/Work/Project";
                            assert_eq!(
                                server.auto_approval.has_delivery(pane),
                                expected,
                                "{provider}: trust={trust}, tools={tools}, directory={directory:?}"
                            );
                            let sequence = server.log.current_sequence();
                            server.maybe_auto_approve(poll.registry(), pane);
                            assert_eq!(server.log.current_sequence(), sequence, "unchanged output");
                        }
                    }
                }
            }
        }
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn explicit_trust_approval_can_continue_when_automatic_trust_is_off() {
        let (mut server, mut poll, pane) = fixture();
        server.config.auto_trust_projects = false;
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .agent
            .as_mut()
            .unwrap()
            .status = AgentStatus::Permission;
        let screen = "\x1b[2J\x1b[HTrust this workspace?\r\nUnknown directory\r\n> No, exit\r\nYes, trust this workspace\r\nEsc to cancel";
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .term
            .feed(screen.as_bytes());
        server.maybe_auto_approve(poll.registry(), pane);
        assert!(!server.auto_approval.has_delivery(pane));
        assert!(server.approve_permission(poll.registry(), pane, false, false, false));
        assert!(server.auto_approval.pending_manual(pane));
        server.panes.get_mut(&pane).unwrap().term.feed(
            screen
                .replace(
                    "> No, exit\r\nYes, trust this workspace",
                    "No, exit\r\n> Yes, trust this workspace",
                )
                .as_bytes(),
        );
        server.maybe_auto_approve(poll.registry(), pane);
        assert!(!server.auto_approval.pending_manual(pane));
        assert!(server.auto_approval.has_delivery(pane));
        assert!(!server.auto_approval.enabled());
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn project_trust_setting_enables_visible_prompt_without_tool_authority() {
        let (mut server, mut poll, pane) = fixture();
        server.projects[0].root = "/Work/Project".into();
        server.config.auto_trust_projects = false;
        server.panes.get_mut(&pane).unwrap().term.feed(b"\x1b[2J\x1b[HTrust this workspace?\r\n/Work/Project\r\n> Yes, trust this workspace\r\nNo, exit\r\nEnter to confirm, Esc to cancel");
        server.maybe_auto_approve(poll.registry(), pane);
        assert!(!server.auto_approval.has_delivery(pane));
        let patch = uniterm_proto::SettingsPatch {
            auto_trust_projects: Some(true),
            ..Default::default()
        };
        server.apply_settings(poll.registry(), Token(9999), patch.clone());
        assert!(server.auto_approval.has_delivery(pane));
        assert!(!server.auto_approval.enabled());
        let sequence = server.log.current_sequence();
        server.apply_settings(poll.registry(), Token(9999), patch);
        assert_eq!(server.log.current_sequence(), sequence);
        server.retire_all_ptys(&mut poll);
    }
}
