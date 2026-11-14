//! Semantic actions shared by server-card clicks and context menus.

use super::*;

impl Server {
    /// Explicit discovery shares the event-triggered worker and coalesces clicks.
    /// Only the selected Project (or this Workspace) contributes Pane trees.
    pub(super) fn refresh_web_servers(&mut self) {
        for window in &self.windows {
            if self.sidebar_server_scope == SidebarScope::Project
                && window.project != self.active_project
            {
                continue;
            }
            for id in window.layout.pane_ids() {
                let Some(pane) = self.panes.get_mut(&id) else {
                    continue;
                };
                let Some(session) = pane.pty.child_pid() else {
                    continue;
                };
                if pane.dev_discovery_pending {
                    pane.dev_discovery_dirty = true;
                } else {
                    pane.dev_discovery_pending = true;
                    self.agents
                        .send(uniterm_proto::CoreToAgent::DevServerDiscover { pane: id, session });
                }
            }
        }
    }

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
            ContextAction::CopyServerAddress => {
                let ops = crate::copymode::osc52(&server.url);
                if let Some(state) = self.clients.get_mut(&client) {
                    state.queue(&encode_frame(&ServerMessage::RenderOps(ops)));
                    state.flush();
                    let _ = set_interest(reg, state, client);
                }
            }
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

#[cfg(test)]
mod tests {
    use super::super::reaper::common;
    use super::*;

    #[test]
    fn refresh_button_bounds_scope_and_coalescing_add_no_idle_requests() {
        common::isolate_state();
        let name = common::unique_workspace_name();
        let dir = common::socket_root().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut server, mut poll) = Server::bind_internal(
            &dir.join(format!("{name}.sock")),
            "/bin/sh",
            &[],
            120,
            24,
            false,
        )
        .unwrap();
        server.config.file_sidebar = true;
        server.config.sidebar = false;
        server.observatory_tab = ObservatoryTab::WebServers;
        let requests = server.agents.capture_requests();
        let pane = server.windows[0].layout.pane_ids()[0];
        let button = server.web_refresh_button().unwrap();
        let layout = server.right_panel_layout(2);
        for (x, y) in [(button.x, layout.footer.y), (layout.area.x, button.y)] {
            server.on_mouse(poll.registry(), Token(999), x + 1, y + 1, MouseKind::Click);
            assert!(requests.try_recv().is_err());
        }
        for x in [button.x, button.right() - 1] {
            server.on_mouse(
                poll.registry(),
                Token(999),
                x + 1,
                button.y + 1,
                MouseKind::Click,
            );
        }
        assert!(
            matches!(requests.try_recv().unwrap(), uniterm_proto::CoreToAgent::DevServerDiscover { pane: id, .. } if id == pane)
        );
        assert!(
            requests.try_recv().is_err(),
            "in-flight refreshes must coalesce"
        );
        assert!(server.panes[&pane].dev_discovery_dirty);
        server.panes.get_mut(&pane).unwrap().dev_discovery_pending = false;
        server.panes.get_mut(&pane).unwrap().dev_discovery_dirty = false;
        server.sidebar_server_scope = SidebarScope::Project;
        server.active_project = ProjectId(u64::MAX);
        server.refresh_web_servers();
        assert!(
            requests.try_recv().is_err(),
            "another Project must not be scanned"
        );
        server.sidebar_server_scope = SidebarScope::Workspace;
        server.refresh_web_servers();
        assert!(matches!(
            requests.try_recv().unwrap(),
            uniterm_proto::CoreToAgent::DevServerDiscover { .. }
        ));
        for _ in 0..2 {
            server.draw_web_server_sidebar(&mut Vec::new());
        }
        assert!(
            requests.try_recv().is_err(),
            "rendering must not schedule discovery"
        );
        server.cols = 70;
        assert!(server.web_refresh_button().is_none());
        server.retire_all_ptys(&mut poll);
    }
}
