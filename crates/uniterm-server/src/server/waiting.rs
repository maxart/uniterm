//! Workspace-scoped waiting projection and semantic queue commands.
//!
//! Waiting items are event-backed human-attention work. Instruction steering
//! remains a separate projection in `server/instruction.rs`.

use super::*;

impl Server {
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
                if agent.evidence.is_empty() {
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

    pub(super) fn record_waiting_change(&mut self, change: uniterm_core::WaitingChange) {
        match change {
            uniterm_core::WaitingChange::None => {}
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

    pub(super) fn waiting_snapshot(&self) -> Vec<uniterm_proto::WaitingEntry> {
        let mut entries = Vec::new();
        for item in self.waiting.items() {
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
                project: project.id,
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
            uniterm_proto::WaitingAction::Focus => (true, self.focus_pane_target(reg, item.pane)),
            uniterm_proto::WaitingAction::Answer => {
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
                    let mut bytes = Vec::with_capacity(answer.len().saturating_add(16));
                    if pane.term.bracketed_paste() {
                        bytes.extend_from_slice(b"\x1b[200~");
                    }
                    bytes.extend_from_slice(answer.as_bytes());
                    if pane.term.bracketed_paste() {
                        bytes.extend_from_slice(b"\x1b[201~");
                    }
                    bytes.push(b'\r');
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
