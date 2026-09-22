//! Agent-card actions share navigation and stop semantics with the control API.

use super::*;

impl Server {
    pub(super) fn agent_context_target(&self, cy: u16) -> Option<ContextTarget> {
        let entries = self.observatory_agent_entries();
        let slots = self.observatory_agent_slots(entries.len());
        let (pane, _) = entries.get(chrome::card_at(&slots, cy)?)?;
        let agent = self.panes.get(pane)?.agent.as_ref()?;
        Some(ContextTarget::Agent {
            pane: *pane,
            started_at: agent.started_at,
            foreground_pid: agent.foreground_pid,
        })
    }

    pub(super) fn run_agent_context_action(
        &mut self,
        reg: &Registry,
        client: Token,
        target: ContextTarget,
        action: ContextAction,
    ) {
        let ContextTarget::Agent {
            pane,
            started_at,
            foreground_pid,
        } = target
        else {
            return;
        };
        let Some(value) = self.panes.get(&pane).filter(|value| {
            value.agent.as_ref().is_some_and(|agent| {
                agent.started_at == started_at && agent.foreground_pid == foreground_pid
            })
        }) else {
            return;
        };
        match action {
            ContextAction::FocusAgent => {
                self.tab_scroll_follow_active = true;
                self.focus_pane_target(reg, pane);
            }
            ContextAction::CopyAgentPath => {
                let Some(path) = value.cwd.as_ref() else {
                    return;
                };
                let ops = crate::copymode::osc52(&path.to_string_lossy());
                if let Some(state) = self.clients.get_mut(&client) {
                    state.queue(&encode_frame(&ServerMessage::RenderOps(ops)));
                    state.flush();
                    let _ = set_interest(reg, state, client);
                }
            }
            ContextAction::StopAgent => {
                self.stop_agent_pane(reg, pane);
            }
            _ => {}
        }
    }

    /// The menu, attach protocol and control API all stop the same single Pane.
    pub(super) fn stop_agent_pane(&mut self, reg: &Registry, pane: PaneId) -> bool {
        let found = self
            .panes
            .get(&pane)
            .is_some_and(|pane| pane.agent.is_some());
        if found {
            self.close_pane(reg, pane);
        }
        found
    }
}
