//! Workspace-scoped waiting projection and semantic queue commands.
//!
//! Waiting items are event-backed human-attention work. Instruction steering
//! remains a separate projection in `server/instruction.rs`.

use super::*;

impl Server {
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

    /// Output-driven trust and tool policies are independent; neither arms a timer.
    pub(super) fn maybe_auto_approve(&mut self, reg: &Registry, pane: PaneId) {
        let permission_automation = !self.delegation_suppressed(pane)
            && (self.auto_approval.enabled() || self.delegated.contains_key(&pane));
        let continuation = self.auto_approval.pending_manual(pane);
        if permission_automation || continuation || self.config.auto_trust_projects {
            self.approve_permission(reg, pane, permission_automation, continuation, true);
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
        if self.durability_error.is_some() || !self.event_writes_enabled {
            return false;
        }
        let Some(target) = self.panes.get(&pane) else {
            return false;
        };
        let Some(agent) = target.agent.as_ref() else {
            return false;
        };
        if target.input.len() != target.input_offset {
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
        if !self
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
        let (mut server, poll) = Server::bind_internal(
            &dir.join(format!("{name}.sock")),
            "/bin/sh",
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
