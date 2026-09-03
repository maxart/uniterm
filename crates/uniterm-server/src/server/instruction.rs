//! Event-backed human-to-agent instruction queue and semantic commands.
//!
//! Automatic delivery is called only from a cooperative OSC 777 ready event.
//! Generic or heuristic idle transitions must never call into this module.

use super::*;

#[derive(Clone, Debug)]
pub(super) enum InstructionCommand {
    Add {
        pane: PaneId,
        author: uniterm_core::InstructionAuthor,
        text: String,
    },
    Replace {
        id: u64,
        author: uniterm_core::InstructionAuthor,
        text: String,
    },
    Cancel {
        id: u64,
    },
    SendNow {
        id: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct InstructionCommandResult {
    pub id: u64,
    pub found: bool,
    pub accepted: bool,
}

impl Server {
    fn instruction_invocation(&self, pane: PaneId) -> Option<i32> {
        let pane = self.panes.get(&pane)?;
        let agent = pane.agent.as_ref()?;
        match (agent.foreground_pid, pane.foreground_pid) {
            (Some(agent), Some(foreground)) if agent != foreground => None,
            (Some(agent), _) => Some(agent),
            (None, foreground) => foreground,
        }
    }

    pub(super) fn instruction_snapshot(&self) -> Vec<uniterm_proto::InstructionEntry> {
        let mut entries = Vec::new();
        for item in self.instructions.items() {
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
            entries.push(uniterm_proto::InstructionEntry {
                id: item.id,
                pane: item.pane,
                invocation: item.invocation,
                author: item.author,
                created_sequence: item.created_sequence,
                policy: item.policy,
                state: item.state,
                text: item.text.clone(),
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
        entries
    }

    pub(super) fn reply_instructions(&mut self, reg: &Registry, token: Token) {
        let items = self.instruction_snapshot();
        if let Some(client) = self.clients.get_mut(&token) {
            client.queue(&encode_frame(&ServerMessage::Instructions { items }));
            client.flush();
            let _ = set_interest(reg, client, token);
        }
    }

    pub(super) fn reply_instruction_change(
        &mut self,
        reg: &Registry,
        token: Token,
        result: InstructionCommandResult,
    ) {
        let items = self.instruction_snapshot();
        if let Some(client) = self.clients.get_mut(&token) {
            client.queue(&encode_frame(&ServerMessage::InstructionChanged {
                id: result.id,
                found: result.found,
                accepted: result.accepted,
                items,
            }));
            client.flush();
            let _ = set_interest(reg, client, token);
        }
    }

    pub(super) fn instruction_command(
        &mut self,
        reg: &Registry,
        command: InstructionCommand,
    ) -> InstructionCommandResult {
        if self.durability_error.is_some() {
            let (id, found) = match command {
                InstructionCommand::Add { pane, .. } => (0, self.panes.contains_key(&pane)),
                InstructionCommand::Replace { id, .. }
                | InstructionCommand::Cancel { id }
                | InstructionCommand::SendNow { id } => (id, self.instructions.get(id).is_some()),
            };
            return InstructionCommandResult {
                id,
                found,
                accepted: false,
            };
        }
        match command {
            InstructionCommand::Add { pane, author, text } => {
                let Some(invocation) = self.instruction_invocation(pane) else {
                    return InstructionCommandResult {
                        id: 0,
                        found: self.panes.contains_key(&pane),
                        accepted: false,
                    };
                };
                if text.trim().is_empty()
                    || text.chars().count() > uniterm_core::instruction::MAX_INSTRUCTION_CHARS
                    || !self.instructions.can_enqueue(pane, invocation)
                {
                    return InstructionCommandResult {
                        id: 0,
                        found: true,
                        accepted: false,
                    };
                }
                let item = self.instructions.allocate(
                    pane,
                    invocation,
                    author,
                    self.log.current_sequence().saturating_add(1),
                    &text,
                );
                self.append_event(crate::eventlog::LogEvent::InstructionQueued {
                    item: item.clone(),
                });
                self.instructions.insert(item.clone());
                InstructionCommandResult {
                    id: item.id,
                    found: true,
                    accepted: true,
                }
            }
            InstructionCommand::Replace { id, author, text } => {
                let Some(current) = self.instructions.get(id).cloned() else {
                    return InstructionCommandResult {
                        id,
                        found: false,
                        accepted: false,
                    };
                };
                if text.trim().is_empty()
                    || text.chars().count() > uniterm_core::instruction::MAX_INSTRUCTION_CHARS
                    || self.instruction_invocation(current.pane) != Some(current.invocation)
                {
                    return InstructionCommandResult {
                        id,
                        found: true,
                        accepted: false,
                    };
                }
                let item = self.instructions.allocate(
                    current.pane,
                    current.invocation,
                    author,
                    self.log.current_sequence().saturating_add(1),
                    &text,
                );
                self.append_event(crate::eventlog::LogEvent::InstructionReplaced {
                    replaced: id,
                    item: item.clone(),
                });
                self.instructions.replace(id, item.clone());
                InstructionCommandResult {
                    id: item.id,
                    found: true,
                    accepted: true,
                }
            }
            InstructionCommand::Cancel { id } => {
                let found = self.instructions.get(id).is_some();
                if found {
                    self.append_event(crate::eventlog::LogEvent::InstructionCanceled {
                        id,
                        reason: uniterm_core::InstructionCancellation::Canceled,
                    });
                    self.instructions.remove(id);
                }
                InstructionCommandResult {
                    id,
                    found,
                    accepted: found,
                }
            }
            InstructionCommand::SendNow { id } => {
                self.deliver_instruction(reg, id, uniterm_core::InstructionBoundary::SendNow)
            }
        }
    }

    /// Deliver at most one item for one cooperative ready event.
    pub(super) fn deliver_cooperative_instruction(&mut self, reg: &Registry, pane: PaneId) {
        if self.durability_error.is_some() {
            return;
        }
        let invocation = self.instruction_invocation(pane);
        self.cancel_stale_instructions(pane, invocation);
        let Some(invocation) = invocation else {
            return;
        };
        let Some(id) = self
            .instructions
            .next_for(pane, invocation)
            .map(|item| item.id)
        else {
            return;
        };
        self.deliver_instruction(reg, id, uniterm_core::InstructionBoundary::CooperativeReady);
    }

    fn deliver_instruction(
        &mut self,
        reg: &Registry,
        id: u64,
        boundary: uniterm_core::InstructionBoundary,
    ) -> InstructionCommandResult {
        let Some(item) = self.instructions.get(id).cloned() else {
            return InstructionCommandResult {
                id,
                found: false,
                accepted: false,
            };
        };
        if self.instruction_invocation(item.pane) != Some(item.invocation) {
            self.append_event(crate::eventlog::LogEvent::InstructionCanceled {
                id,
                reason: uniterm_core::InstructionCancellation::InvocationEnded,
            });
            self.instructions.remove(id);
            return InstructionCommandResult {
                id,
                found: true,
                accepted: false,
            };
        }
        let delivery_id = self.instructions.mint_delivery_id();
        let accepted = self
            .panes
            .get_mut(&item.pane)
            .is_some_and(|pane| queue_submitted_text(reg, pane, &item.text));
        self.append_event(crate::eventlog::LogEvent::InstructionDelivery {
            id,
            delivery_id,
            boundary,
            accepted,
        });
        if accepted {
            self.instructions.remove(id);
        }
        InstructionCommandResult {
            id,
            found: true,
            accepted,
        }
    }

    pub(super) fn cancel_stale_instructions(&mut self, pane: PaneId, invocation: Option<i32>) {
        let stale: Vec<_> = self
            .instructions
            .items()
            .iter()
            .filter(|item| item.pane == pane && Some(item.invocation) != invocation)
            .map(|item| item.id)
            .collect();
        for id in stale {
            self.append_event(crate::eventlog::LogEvent::InstructionCanceled {
                id,
                reason: uniterm_core::InstructionCancellation::InvocationEnded,
            });
            self.instructions.remove(id);
        }
    }

    pub(super) fn cancel_pane_instructions(&mut self, pane: PaneId) {
        let ids: Vec<_> = self
            .instructions
            .items()
            .iter()
            .filter(|item| item.pane == pane)
            .map(|item| item.id)
            .collect();
        for id in ids {
            self.append_event(crate::eventlog::LogEvent::InstructionCanceled {
                id,
                reason: uniterm_core::InstructionCancellation::PaneClosed,
            });
            self.instructions.remove(id);
        }
    }
}

fn queue_submitted_text(reg: &Registry, pane: &mut Pane, text: &str) -> bool {
    let bracketed = pane.term.bracketed_paste();
    let mut bytes = Vec::with_capacity(text.len().saturating_add(16));
    if bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
    }
    bytes.extend_from_slice(text.as_bytes());
    if bracketed {
        bytes.extend_from_slice(b"\x1b[201~");
    }
    bytes.push(b'\r');
    Server::queue_pane_input(reg, pane, &bytes)
}
