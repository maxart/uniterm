//! Opt-in navigation and automatic labels are projections of observed state.
//! Closed navigation surfaces have no subscriptions, timers, or background scans.
use super::*;

impl Server {
    pub(super) fn effective_tab_name(&self, index: usize) -> Option<String> {
        let window = self.windows.get(index)?;
        window.name.clone().or_else(|| {
            self.config
                .auto_tab_names
                .then(|| self.automatic_pane_name(window.active))
                .flatten()
        })
    }

    pub(super) fn automatic_pane_name(&self, id: PaneId) -> Option<String> {
        let pane = self.panes.get(&id)?;
        let cwd = pane.cwd.as_deref().and_then(Path::to_str).unwrap_or("");
        let label = if let Some(agent) = &pane.agent {
            // These explicit metadata keys are shared by every provider and
            // automation client. Native terminal titles are a provider fallback.
            agent
                .details
                .summary
                .clone()
                .or_else(|| {
                    ["summary", "task", "title"]
                        .iter()
                        .find_map(|key| pane.metadata.get(*key).map(|entry| entry.value.as_str()))
                        .filter(|text| !text.trim().is_empty())
                        .map(str::to_owned)
                })
                .or_else(|| {
                    (pane.term.title_revision() > agent.title_revision_floor)
                        .then(|| {
                            crate::providers::task_summary(
                                &agent.id,
                                pane.term.terminal_title(),
                                cwd,
                            )
                        })
                        .flatten()
                })
                .or_else(|| agent.title_summary.clone())
        } else {
            let directory = Path::new(cwd)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(cwd);
            let process = self
                .process_names
                .get(&id)
                .filter(|(pid, _)| {
                    pane.foreground_pid == Some(*pid)
                        && pane
                            .pty
                            .child_pid()
                            .is_some_and(|child| i64::from(child) != i64::from(*pid))
                })
                .map(|(_, name)| name.as_str());
            match (directory.is_empty(), process) {
                (false, Some(process)) => Some(format!("{directory}: {process}")),
                (false, None) => Some(directory.to_owned()),
                (true, Some(process)) => Some(process.to_owned()),
                (true, None) => None,
            }
        };
        label
            .map(|label| sanitize_chrome_text(label.trim(), 80))
            .filter(|label| !label.is_empty())
    }

    pub(super) fn switcher_entries(&self) -> Vec<uniterm_proto::SwitcherEntry> {
        if !self.config.switcher_enabled {
            return Vec::new();
        }
        let mut entries = self
            .pane_snapshot()
            .into_iter()
            .take(512)
            .filter_map(|location| {
                let pane = self.panes.get(&location.id)?;
                let project = self
                    .projects
                    .iter()
                    .find(|project| project.id == location.project);
                let branch = pane
                    .metadata
                    .get("branch")
                    .map(|entry| entry.value.clone())
                    .or_else(|| project.and_then(|project| project.metadata.get("branch").cloned()))
                    .or_else(|| {
                        project
                            .and_then(Self::worktree_registration)
                            .map(|worktree| worktree.branch)
                    })
                    .unwrap_or_default();
                Some(uniterm_proto::SwitcherEntry {
                    pane: location,
                    cwd: pane
                        .cwd
                        .as_deref()
                        .map(|path| sanitize_chrome_text(&path.to_string_lossy(), 240))
                        .unwrap_or_default(),
                    branch: sanitize_chrome_text(&branch, 120),
                    agent: pane
                        .agent
                        .as_ref()
                        .map(|agent| agent.id.clone())
                        .unwrap_or_default(),
                    status: pane
                        .agent
                        .as_ref()
                        .map(|agent| agent.status.label().to_owned())
                        .unwrap_or_default(),
                    preview: pane
                        .term
                        .recent_output_text(3)
                        .lines()
                        .map(|line| sanitize_chrome_text(line, 100))
                        .collect::<Vec<_>>()
                        .join("\n"),
                })
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| {
            (
                entry.pane.active,
                !matches!(entry.status.as_str(), "permission" | "question" | "error"),
                entry.pane.project.0,
                entry.pane.tab,
                entry.pane.pane,
            )
        });
        entries
    }

    pub(super) fn open_switcher(&mut self, reg: &Registry) {
        let bytes = encode_frame(&ServerMessage::Switcher {
            open: true,
            enabled: self.config.switcher_enabled,
            entries: self.switcher_entries(),
        });
        for (&token, client) in &mut self.clients {
            if client.attached && !client.direct_only {
                client.queue(&bytes);
                client.flush();
                let _ = set_interest(reg, client, token);
            }
        }
    }

    pub(super) fn broadcast_switcher(&mut self, reg: &Registry) {
        if !self
            .clients
            .values()
            .any(|client| client.switcher_subscribed)
        {
            return;
        }
        let bytes = encode_frame(&ServerMessage::Switcher {
            open: false,
            enabled: self.config.switcher_enabled,
            entries: self.switcher_entries(),
        });
        for (&token, client) in &mut self.clients {
            if client.switcher_subscribed && client.last_switcher != bytes {
                client.queue(&bytes);
                client.last_switcher.clone_from(&bytes);
                client.flush();
                let _ = set_interest(reg, client, token);
            }
            if !self.config.switcher_enabled {
                client.switcher_subscribed = false;
            }
        }
    }
}
