//! Semantic actions shared by server-card clicks and context menus.

use super::*;

impl Server {
    pub(super) fn dev_server_context_target(&self, cy: u16) -> Option<ContextTarget> {
        let entries = self.observatory_dev_server_entries();
        let slots = self.observatory_web_slots(entries.len());
        let entry = entries.get(chrome::card_at(&slots, cy)?)?;
        let server = self.dev_servers.get(&(entry.pane_id, entry.port))?;
        Some(ContextTarget::DevServer {
            pane: entry.pane_id,
            port: entry.port,
            detected: server.detected,
        })
    }

    pub(super) fn run_dev_server_action(
        &mut self,
        reg: &Registry,
        client: Token,
        target: ContextTarget,
        action: ContextAction,
    ) {
        let ContextTarget::DevServer {
            pane,
            port,
            detected,
        } = target
        else {
            return;
        };
        let Some(server) = self
            .dev_servers
            .get_mut(&(pane, port))
            .filter(|server| server.detected == detected)
        else {
            return;
        };
        match action {
            ContextAction::OpenServer => {
                let url = server.url.clone();
                if let Some(state) = self.clients.get_mut(&client) {
                    state.queue(&encode_frame(&ServerMessage::OpenUrl { url }));
                    state.flush();
                    let _ = set_interest(reg, state, client);
                }
            }
            ContextAction::FocusServer => {
                self.focus_pane_target(reg, pane);
            }
            ContextAction::StopServer if !server.stopping => {
                let Some(session) = self.panes.get(&pane).and_then(|pane| pane.pty.child_pid())
                else {
                    return;
                };
                server.stopping = true;
                self.agents.send(uniterm_proto::CoreToAgent::DevServerStop {
                    pane,
                    port,
                    detected,
                    session,
                });
            }
            _ => {}
        }
    }
}
