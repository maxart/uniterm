//! Agent lifecycle on the core side: launching, kernel-driven exit watches,
//! status detection with dwell, and the fleet snapshots the Observatory reads.
//!
//! Nothing here branches on an agent id; anything agent-specific lives behind
//! the provider trait on the runtime side.

use super::*;

impl Server {
    pub(super) fn register_process_watch(&mut self, reg: &Registry, pane: PaneId, pid: i32) {
        if self
            .pane_watches
            .get(&pane)
            .and_then(|token| self.process_watches.get(token))
            .is_some_and(|(_, watch)| watch.pid == pid)
        {
            return;
        }
        if let Some(token) = self.pane_watches.remove(&pane) {
            if let Some((_, mut watch)) = self.process_watches.remove(&token) {
                watch.deregister(reg);
            }
        }
        let Ok(mut watch) = crate::process_watch::ProcessWatch::new(pid) else {
            return;
        };
        let token = Token(self.next_token);
        self.next_token += 1;
        if watch.register(reg, token).is_ok() {
            self.pane_watches.insert(pane, token);
            self.process_watches.insert(token, (pane, watch));
        }
    }

    pub(super) fn on_process_exit(&mut self, reg: &Registry, token: Token) {
        let Some((pane_id, mut watch)) = self.process_watches.remove(&token) else {
            return;
        };
        watch.deregister(reg);
        self.pane_watches.remove(&pane_id);
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        let Some(agent) = pane.agent.take() else {
            return;
        };
        pane.last_detection = Some(DetectionRecord {
            agent: agent.id,
            status: AgentStatus::Exited,
            authority: uniterm_proto::DetectionAuthority::KernelExit,
            evidence: format!("kernel reported process {} exit", watch.pid),
            foreground_pid: Some(watch.pid),
            provenance: direct_detection_provenance(
                uniterm_proto::DetectionSource::Kernel,
                Some(watch.pid),
            ),
        });
        pane.detection_candidate = None;
        self.append_event(crate::eventlog::LogEvent::AgentStatus {
            pane: pane_id.0,
            status: AgentStatus::Exited,
        });
        self.append_event(crate::eventlog::LogEvent::AgentUnbound { pane: pane_id.0 });
        self.resolve_waiting_for_pane(pane_id, uniterm_core::WaitingResolution::AgentAdvanced);
        self.full_repaint_all(reg);
    }

    /// Bind an agent to a pane at launch time, so the fleet entry appears the
    /// moment Uniterm starts the agent.
    /// Detection must not depend on its notify connector
    /// being installed. The OSC 777 stream (connector hooks, or the launch
    /// wrapper's `session_end` envelope) then refines and eventually clears
    /// the binding. The caller repaints the fleet surfaces.
    pub(super) fn bind_agent(&mut self, pane_id: PaneId, agent_id: &str) {
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        let color = uniterm_core::agent::agent_color_or_default(agent_id);
        pane.agent = Some(PaneAgent {
            id: agent_id.to_string(),
            color,
            status: AgentStatus::Starting,
            authority: uniterm_proto::DetectionAuthority::Process,
            evidence: format!("launched by Uniterm as {agent_id}"),
            provenance: direct_detection_provenance(
                uniterm_proto::DetectionSource::Launch,
                pane.pty.foreground_process_group(),
            ),
            foreground_pid: pane.pty.foreground_process_group(),
            started_at: std::time::Instant::now(),
            session_id: None,
            resume_command: Vec::new(),
        });
        self.append_event(crate::eventlog::LogEvent::AgentBound {
            pane: pane_id.0,
            agent: agent_id.to_string(),
        });
        self.append_event(crate::eventlog::LogEvent::AgentStatus {
            pane: pane_id.0,
            status: AgentStatus::Starting,
        });
    }

    /// Launch a New Task (from the overlay): split a fresh pane in the active
    /// window, focus it, and start the chosen agent there with the prompt as
    /// its argument. The agent is the pane's own command (`$SHELL -c "agent
    /// '...' ; exec $SHELL"`), NOT typed into an interactive shell - a booting
    /// zsh discards typeahead, so a typed launch line raced startup and was
    /// silently eaten. When the agent exits the pane execs back into a shell.
    /// With no usable agent the raw line is typed (the pre-agent behaviour).
    pub(super) fn new_task(
        &mut self,
        reg: &Registry,
        prompt: &str,
        relay: bool,
        agent: Option<&str>,
        project: Option<&str>,
    ) {
        let prompt: String = prompt.chars().take(65_536).collect();
        let launch = if prompt.is_empty() {
            None
        } else {
            crate::workflow::resolve_agent_on_search_path(agent, &self.agent_search_path).map(
                |(id, cmd)| {
                    let invocation = crate::workflow::launch_invocation(&cmd, &prompt);
                    let line = crate::workflow::announce_wrapped(&id, &invocation);
                    (id, line)
                },
            )
        };
        let spawn_args: Vec<String> = match &launch {
            Some((_, line)) => vec!["-c".into(), format!("{line}; exec {}", self.program)],
            None => Vec::new(),
        };
        let arg_refs: Vec<&str> = spawn_args.iter().map(String::as_str).collect();
        if let Ok(new_id) = self.spawn_pane(reg, &arg_refs) {
            self.split_active_pane(new_id, SplitDir::Vertical);
            if let Some((id, _)) = &launch {
                self.bind_agent(new_id, id);
            }
            self.relayout();
            if let Some(pane) = self.panes.get_mut(&new_id) {
                if relay {
                    let banner = "printf '\\033[36m[relay] launching turn-based run\\033[0m\\n'\n";
                    Self::queue_pane_input(reg, pane, banner.as_bytes());
                }
                if !prompt.is_empty() && launch.is_none() {
                    // Legacy path (no agent resolvable): type the raw prompt.
                    let line = match agent {
                        Some(a) => format!("echo 'uniterm: agent {a} not found on PATH'\n"),
                        None => format!("{prompt}\n"),
                    };
                    Self::queue_pane_input(reg, pane, line.as_bytes());
                }
            }
            self.append_event(crate::eventlog::LogEvent::TaskLaunched { relay });
            // Record the launched task, marked in-progress; project-tagged
            // titles keep the `# project NAME:` shape project_names() reads.
            if !prompt.trim().is_empty() {
                let title = match project {
                    Some(p) => format!("# project {p}: {}", prompt.trim()),
                    None => prompt.trim().to_string(),
                };
                self.create_task(&title, uniterm_core::TaskStatus::Doing);
            }
            self.full_repaint_all(reg);
            self.persist();
        }
    }

    /// Start an agent from the Manage Agents modal. The pane targets spawn the
    /// agent as the pane's own command (the race-free New Task pattern, execing
    /// back to the shell on exit); `CurrentPane` types the launch line into the
    /// focused pane's shell, exactly as the user would - but only when that
    /// shell is actually at its prompt. Every target binds the agent
    /// immediately in fleet surfaces, independent of any connector.
    pub(super) fn launch_agent(
        &mut self,
        reg: &Registry,
        agent: &str,
        target: uniterm_proto::LaunchTarget,
    ) -> Option<PaneId> {
        use uniterm_proto::LaunchTarget;
        let Some((id, cmd)) =
            crate::workflow::resolve_agent_on_search_path(Some(agent), &self.agent_search_path)
        else {
            return None; // not installed; the modal greys this out, but stay safe
        };
        match target {
            LaunchTarget::CurrentPane => {
                let active = self.windows[self.active_window].active;
                let p = self.panes.get_mut(&active)?;
                // A busy pane (a bound agent, or any foreground program -
                // editor, pager) would receive the launch line as junk input,
                // and the binding would point at something we never started.
                // Keep the user's intent safe: launch into a new pane instead.
                if p.agent.is_some() || !p.pty.child_owns_foreground() {
                    return self.launch_agent(reg, agent, LaunchTarget::NewPane);
                }
                // The trailing envelope clears the binding when the agent
                // exits (the parser unbinds on `session_end`), so a typed
                // launch cannot leave a stale binding behind.
                let line = format!(
                    "{}; {}\n",
                    crate::workflow::shell_quote(&cmd),
                    crate::workflow::osc777_announce(&id, "session_end")
                );
                if !Self::queue_pane_input(reg, p, line.as_bytes()) {
                    return None;
                }
                self.bind_agent(active, &id);
                self.full_repaint_all(reg);
                Some(active)
            }
            LaunchTarget::NewPane | LaunchTarget::NewWindow => {
                let invocation = crate::workflow::shell_quote(&cmd);
                let line = crate::workflow::announce_wrapped(&id, &invocation);
                let spawn_args = ["-c".to_string(), format!("{line}; exec {}", self.program)];
                let arg_refs: Vec<&str> = spawn_args.iter().map(String::as_str).collect();
                let Ok(new_id) = self.spawn_pane(reg, &arg_refs) else {
                    return None;
                };
                if target == LaunchTarget::NewPane {
                    // Agents open beside the pane you were in, not under it:
                    // agent output is tall, so the side-by-side split keeps
                    // both the shell and the agent readable.
                    self.split_active_pane(new_id, SplitDir::Horizontal);
                } else {
                    self.push_window(new_id);
                }
                self.bind_agent(new_id, &id);
                self.relayout();
                self.full_repaint_all(reg);
                self.persist();
                Some(new_id)
            }
        }
    }

    pub(super) fn notify_agent_transition(
        &mut self,
        pane: PaneId,
        previous: AgentStatus,
        status: AgentStatus,
    ) {
        self.sync_waiting_agent(pane, status);
        self.reconcile_orchestration_idle(pane, status);
        self.pending_notifications.remove(&pane);
        let attention = status.needs_human() && !previous.needs_human();
        let completion = self.config.notify_completion
            && status == AgentStatus::Idle
            && matches!(
                previous,
                AgentStatus::Starting | AgentStatus::Working | AgentStatus::Tool
            );
        if !attention && !completion {
            return;
        }
        if self.config.notifications == uniterm_core::NotificationDelivery::Off {
            return;
        }
        self.pending_notifications.insert(
            pane,
            PendingAgentNotification {
                previous,
                status,
                due: std::time::Instant::now() + std::time::Duration::from_secs(1),
            },
        );
    }

    pub(super) fn deliver_agent_notification(
        &mut self,
        reg: &Registry,
        pane: PaneId,
        previous: AgentStatus,
        status: AgentStatus,
    ) {
        let attention = status.needs_human() && !previous.needs_human();
        let Some(agent) = self.panes.get(&pane).and_then(|pane| pane.agent.as_ref()) else {
            return;
        };
        let agent_name = uniterm_core::agent::agent_name(&agent.id);
        let title = if attention {
            format!("{agent_name} needs input")
        } else {
            format!("{agent_name} finished")
        };
        let location = self
            .windows
            .iter()
            .position(|tab| tab.layout.contains_pane(pane))
            .and_then(|window| {
                let tab = &self.windows[window];
                let project = self.projects.iter().find(|item| item.id == tab.project)?;
                let tab_number = self
                    .project_window_indices(tab.project)
                    .iter()
                    .position(|index| *index == window)
                    .unwrap_or(0)
                    + 1;
                Some(format!("{} / Tab {tab_number}", project.name))
            })
            .unwrap_or_else(|| self.name.clone());
        let body = if attention {
            format!("{} in {location}", status.label())
        } else {
            location
        };
        // Sound rides the same smoothed transition as the visible notice, and
        // it is the client's to play: the human may be attached over SSH.
        if self.config.notification_sound != uniterm_core::NotificationSound::Off {
            let kind = if attention {
                uniterm_proto::ChimeKind::Attention
            } else {
                uniterm_proto::ChimeKind::Done
            };
            let pane_active = self.windows[self.active_window].active == pane;
            let chime = encode_frame(&ServerMessage::Chime {
                kind,
                sound: self.config.notification_sound,
                file: self.config.notification_sound_file.clone(),
                pane_active,
            });
            for (token, client) in &mut self.clients {
                if !client.attached {
                    continue;
                }
                client.queue(&chime);
                client.flush();
                let _ = set_interest(reg, client, *token);
            }
        }
        match self.config.notifications {
            uniterm_core::NotificationDelivery::Off => {}
            uniterm_core::NotificationDelivery::Uniterm => {
                self.notification = Some(AgentToast {
                    pane,
                    title,
                    body,
                    expires: std::time::Instant::now() + std::time::Duration::from_secs(8),
                });
                self.full_repaint_all(reg);
            }
            uniterm_core::NotificationDelivery::Terminal => {
                let message =
                    format!("{}: {}", title, body).replace(['\x1b', '\x07', '\r', '\n'], " ");
                self.send_raw_ops(reg, format!("\x1b]9;{message}\x07").as_bytes());
            }
            uniterm_core::NotificationDelivery::System => {
                self.agents
                    .send(uniterm_proto::CoreToAgent::SystemNotification { title, body });
            }
        }
    }

    /// Close every pane with a bound agent inside `scope` (Manage Agents
    /// "stop all"). Closing the pane drops the PTY, which HUPs the agent's
    /// whole session. If agents were all that was left, the session ends like
    /// it would after closing those panes by hand.
    pub(super) fn stop_all_agents(&mut self, reg: &Registry, scope: uniterm_proto::StopScope) {
        let in_scope: Vec<PaneId> = match scope {
            uniterm_proto::StopScope::Workspace | uniterm_proto::StopScope::Session => {
                self.panes.keys().copied().collect()
            }
            uniterm_proto::StopScope::Project(project) => self
                .windows
                .iter()
                .filter(|tab| tab.project == project)
                .flat_map(|tab| tab.layout.pane_ids())
                .collect(),
            uniterm_proto::StopScope::Tab(tab) | uniterm_proto::StopScope::Window(tab) => self
                .project_window_indices(self.active_project)
                .get(tab as usize)
                .and_then(|index| self.windows.get(*index))
                .map(|w| w.layout.pane_ids())
                .unwrap_or_default(),
        };
        let ids: Vec<PaneId> = in_scope
            .into_iter()
            .filter(|id| self.panes.get(id).is_some_and(|p| p.agent.is_some()))
            .collect();
        self.terminate_panes(&ids);
        for id in ids {
            self.close_pane(reg, id);
        }
    }

    /// Build the Observatory fleet snapshot: every pane with a bound agent,
    /// sorted with the ones needing a human first.
    pub(super) fn fleet_snapshot(&self) -> Vec<uniterm_proto::FleetEntry> {
        let mut entries = Vec::new();
        for (wi, win) in self.windows.iter().enumerate() {
            let project = self
                .projects
                .iter()
                .find(|project| project.id == win.project);
            let tab = self
                .project_window_indices(win.project)
                .iter()
                .position(|window| *window == wi)
                .unwrap_or(0) as u32
                + 1;
            for (pi, pid) in win.layout.pane_ids().into_iter().enumerate() {
                if let Some(a) = self.panes.get(&pid).and_then(|p| p.agent.as_ref()) {
                    let active_run = self.run_graph.active_for_pane(pid);
                    entries.push(uniterm_proto::FleetEntry {
                        agent: a.id.clone(),
                        status: a.status,
                        pane_id: pid,
                        project: win.project,
                        project_name: project
                            .map(|project| project.name.clone())
                            .unwrap_or_else(|| "Project".into()),
                        tab,
                        tab_name: win.name.clone().unwrap_or_else(|| format!("Tab {tab}")),
                        window: wi as u32 + 1,
                        pane: pi as u32 + 1,
                        authority: a.authority,
                        evidence: a.evidence.clone(),
                        run: active_run.map(|(run, _)| run),
                        role: active_run.map(|(_, role)| role),
                        role_name: active_run.and_then(|(_, role)| {
                            self.run_graph.role(role).map(|role| role.name.clone())
                        }),
                    });
                }
            }
        }
        uniterm_core::agent::fleet_sort(&mut entries, |e| e.status);
        entries
    }

    pub(super) fn run_snapshot(
        &self,
        project: Option<ProjectId>,
        active_only: bool,
    ) -> Vec<uniterm_proto::RunEntry> {
        self.run_graph
            .runs()
            .filter(|run| project.is_none_or(|project| run.project == project))
            .filter(|run| !active_only || run.status == uniterm_core::RunStatus::Active)
            .map(|run| uniterm_proto::RunEntry {
                id: run.id,
                parent: run.parent,
                children: self.run_graph.children(run.id).to_vec(),
                project: run.project,
                kind: run.kind,
                task_id: run.task_id,
                title: run.title.clone(),
                status: run.status,
                outcome: run.outcome.clone(),
                panes: self.run_graph.panes(run.id).to_vec(),
                roles: run
                    .roles
                    .iter()
                    .filter_map(|role| self.run_graph.role(*role))
                    .map(|role| uniterm_proto::RunRoleEntry {
                        id: role.id,
                        name: role.name.clone(),
                        pane: role.pane,
                        provider: role.provider.clone(),
                        activation: role.activation.clone(),
                    })
                    .collect(),
            })
            .collect()
    }

    pub(super) fn artifact_snapshot(
        &self,
        project: Option<ProjectId>,
        run: Option<uniterm_core::RunId>,
        include_superseded: bool,
    ) -> Vec<uniterm_proto::ArtifactEntry> {
        let include = |record: &&uniterm_core::ArtifactRecord| {
            project.is_none_or(|project| record.project == project)
                && (include_superseded || record.status != uniterm_core::ArtifactStatus::Superseded)
        };
        let records: Vec<_> = if let Some(run) = run {
            self.artifacts
                .for_run(run)
                .iter()
                .filter_map(|id| self.artifacts.artifact(*id))
                .filter(include)
                .collect()
        } else if let Some(project) = project {
            self.artifacts
                .for_project(project)
                .iter()
                .filter_map(|id| self.artifacts.artifact(*id))
                .filter(include)
                .collect()
        } else {
            self.artifacts.artifacts().filter(include).collect()
        };
        records
            .into_iter()
            .map(|record| uniterm_proto::ArtifactEntry {
                id: record.id,
                project: record.project,
                producer_run: record.producer_run,
                producer_role: record.producer_role,
                kind: record.kind,
                path: record.path.clone(),
                digest: record.digest.clone(),
                size: record.size,
                status: record.status,
                supersedes: record.supersedes,
            })
            .collect()
    }

    pub(super) fn dev_server_snapshot(&self) -> Vec<uniterm_proto::DevServerEntry> {
        self.dev_server_snapshot_for_project(None)
    }

    pub(super) fn observatory_dev_server_entries(&self) -> Vec<uniterm_proto::DevServerEntry> {
        let project =
            (self.sidebar_server_scope == SidebarScope::Project).then_some(self.active_project);
        self.dev_server_snapshot_for_project(project)
    }

    pub(super) fn dev_server_snapshot_for_project(
        &self,
        project_filter: Option<ProjectId>,
    ) -> Vec<uniterm_proto::DevServerEntry> {
        let mut entries = Vec::new();
        for ((pane_id, port), server) in &self.dev_servers {
            let Some((window_index, win)) = self
                .windows
                .iter()
                .enumerate()
                .find(|(_, win)| win.layout.contains_pane(*pane_id))
            else {
                continue;
            };
            let Some(project) = self.projects.iter().find(|item| item.id == win.project) else {
                continue;
            };
            if project_filter.is_some_and(|wanted| project.id != wanted) {
                continue;
            }
            let tab = self
                .project_window_indices(win.project)
                .iter()
                .position(|index| *index == window_index)
                .unwrap_or(0) as u32
                + 1;
            let pane = win
                .layout
                .pane_ids()
                .iter()
                .position(|candidate| candidate == pane_id)
                .unwrap_or(0) as u32
                + 1;
            entries.push((
                server.detected,
                uniterm_proto::DevServerEntry {
                    label: server.label.clone(),
                    url: server.url.clone(),
                    port: *port,
                    pane_id: *pane_id,
                    project: project.id,
                    project_name: project.name.clone(),
                    project_root: project.root.clone(),
                    tab,
                    tab_name: win.name.clone().unwrap_or_else(|| format!("Tab {tab}")),
                    pane,
                },
            ));
        }
        entries.sort_by(|left, right| {
            left.1
                .port
                .cmp(&right.1.port)
                .then_with(|| right.0.cmp(&left.0))
        });
        entries.dedup_by_key(|(_, entry)| entry.port);
        let mut entries: Vec<_> = entries.into_iter().map(|(_, entry)| entry).collect();
        entries.sort_by(|left, right| {
            left.project_name
                .cmp(&right.project_name)
                .then_with(|| left.port.cmp(&right.port))
                .then_with(|| left.pane_id.0.cmp(&right.pane_id.0))
        });
        entries
    }

    pub(super) fn broadcast_dev_servers(&mut self, reg: &Registry) {
        let entries = self.dev_server_snapshot();
        let message = encode_frame(&ServerMessage::DevServers { entries });
        for (token, client) in &mut self.clients {
            if !client.attached {
                continue;
            }
            client.queue(&message);
            client.flush();
            let _ = set_interest(reg, client, *token);
        }
        if self.observatory_tab == ObservatoryTab::WebServers && self.observatory_width() > 0 {
            self.full_repaint_all(reg);
        }
    }

    pub(super) fn detection_snapshot(
        &self,
        filter: Option<PaneId>,
    ) -> Vec<uniterm_proto::AgentDetectionInfo> {
        let mut entries = Vec::new();
        for project in &self.projects {
            let tabs = self.project_window_indices(project.id);
            for (tab_index, window_index) in tabs.into_iter().enumerate() {
                let tab = &self.windows[window_index];
                for pane_id in tab.layout.pane_ids() {
                    if filter.is_some_and(|wanted| wanted != pane_id) {
                        continue;
                    }
                    let Some(pane) = self.panes.get(&pane_id) else {
                        continue;
                    };
                    let (agent, status, authority, evidence, foreground_pid, provenance) =
                        if let Some(active) = &pane.agent {
                            (
                                Some(active.id.clone()),
                                active.status,
                                active.authority,
                                active.evidence.clone(),
                                active.foreground_pid,
                                active.provenance.clone(),
                            )
                        } else if let Some(last) = &pane.last_detection {
                            (
                                Some(last.agent.clone()),
                                last.status,
                                last.authority,
                                last.evidence.clone(),
                                last.foreground_pid,
                                last.provenance.clone(),
                            )
                        } else {
                            (
                                None,
                                AgentStatus::Unknown,
                                uniterm_proto::DetectionAuthority::Grid,
                                "no agent evidence observed".into(),
                                pane.foreground_pid,
                                uniterm_proto::DetectionProvenance::direct(
                                    uniterm_proto::DetectionSource::None,
                                    0,
                                    pane.foreground_pid,
                                ),
                            )
                        };
                    entries.push(uniterm_proto::AgentDetectionInfo {
                        pane: pane_id,
                        project: project.id,
                        tab: tab_index as u32 + 1,
                        agent,
                        status,
                        authority,
                        evidence,
                        foreground_pid,
                        provenance,
                    });
                }
            }
        }
        entries.sort_by_key(|entry| entry.pane.0);
        entries
    }

    pub(super) fn refresh_provider_evidence(&self) {
        let evidence: Vec<_> = self
            .panes
            .iter()
            .map(|(pane_id, pane)| {
                (
                    *pane_id,
                    pane.pty.foreground_process_group(),
                    pane.term.evidence_text(12),
                    pane.term.terminal_title().to_string(),
                    pane.agent.as_ref().map(|agent| agent.id.clone()),
                )
            })
            .collect();
        for (pane, foreground_pid, tail, title, bound_agent) in evidence {
            self.agents.send(uniterm_proto::CoreToAgent::PaneEvidence {
                pane,
                foreground_pid,
                process_changed: true,
                tail,
                title,
                bound_agent,
            });
        }
    }

    pub(super) fn pane_snapshot(&self) -> Vec<uniterm_proto::PaneInfo> {
        let mut entries = Vec::with_capacity(self.panes.len());
        for project in &self.projects {
            for (tab_index, window_index) in self
                .project_window_indices(project.id)
                .into_iter()
                .enumerate()
            {
                let window = &self.windows[window_index];
                let tab = u32::try_from(tab_index.saturating_add(1)).unwrap_or(u32::MAX);
                let tab_name = window.name.clone().unwrap_or_else(|| format!("Tab {tab}"));
                for (pane_index, pane) in window.layout.pane_ids().into_iter().enumerate() {
                    entries.push(uniterm_proto::PaneInfo {
                        id: pane,
                        project: project.id,
                        project_name: project.name.clone(),
                        tab,
                        tab_name: tab_name.clone(),
                        pane: u32::try_from(pane_index.saturating_add(1)).unwrap_or(u32::MAX),
                        active: self.active_window == window_index && window.active == pane,
                    });
                }
            }
        }
        entries
    }

    pub(super) fn bounded_pane_output(&self, pane: PaneId, lines: u32) -> Option<(String, bool)> {
        const MAX_LINES: usize = 2_000;
        const MAX_BYTES: usize = 256 * 1024;
        let mut text = self
            .panes
            .get(&pane)?
            .term
            .automation_output_text((lines as usize).clamp(1, MAX_LINES));
        let truncated = retain_recent_utf8(&mut text, MAX_BYTES);
        Some((text, truncated))
    }

    /// Complete control waits only when their observed state changed or their
    /// individually armed deadline fired. No pane scan or free-running timer
    /// exists when no caller is waiting.
    pub(super) fn service_pending_waits(&mut self, reg: &Registry) {
        let now = std::time::Instant::now();
        let tokens: Vec<Token> = self
            .clients
            .iter()
            .filter_map(|(token, client)| client.pending_wait.as_ref().map(|_| *token))
            .collect();
        for token in tokens {
            let response = match self
                .clients
                .get(&token)
                .and_then(|client| client.pending_wait.as_ref())
            {
                Some(PendingControlWait::Output {
                    pane,
                    needle,
                    deadline,
                }) => match self.bounded_pane_output(*pane, 2_000) {
                    Some((text, truncated)) => {
                        let matched = text.contains(needle);
                        (matched || now >= *deadline).then_some(ServerMessage::PaneOutputWaited {
                            pane: *pane,
                            found: true,
                            matched,
                            timed_out: !matched,
                            text,
                            truncated,
                        })
                    }
                    None => Some(ServerMessage::PaneOutputWaited {
                        pane: *pane,
                        found: false,
                        matched: false,
                        timed_out: false,
                        text: String::new(),
                        truncated: false,
                    }),
                },
                Some(PendingControlWait::Agent {
                    pane,
                    status,
                    deadline,
                }) => match self.panes.get(pane) {
                    Some(pane_state) => {
                        let current = pane_state.agent.as_ref().map(|agent| agent.status);
                        let matched = current == Some(*status);
                        (matched || now >= *deadline).then_some(ServerMessage::AgentWaited {
                            pane: *pane,
                            found: true,
                            matched,
                            timed_out: !matched,
                            status: current,
                        })
                    }
                    None => Some(ServerMessage::AgentWaited {
                        pane: *pane,
                        found: false,
                        matched: false,
                        timed_out: false,
                        status: None,
                    }),
                },
                None => None,
            };
            let Some(response) = response else {
                continue;
            };
            if let Some(client) = self.clients.get_mut(&token) {
                client.pending_wait = None;
                client.queue(&encode_frame(&response));
                client.flush();
                let _ = set_interest(reg, client, token);
            }
        }
    }

    /// How many panes run each agent right now, keyed by registry id where
    /// the binding maps to one, else by the bound id itself (custom agents
    /// count too - they are as running, and as stoppable, as any other).
    pub(super) fn running_agents(&self) -> HashMap<String, u32> {
        let mut running: HashMap<String, u32> = HashMap::new();
        for p in self.panes.values() {
            if let Some(a) = &p.agent {
                let id = uniterm_core::agent::provider(&a.id)
                    .map(|p| p.id.to_string())
                    .unwrap_or_else(|| a.id.clone());
                *running.entry(id).or_default() += 1;
            }
        }
        running
    }

    /// Apply a reply from the agent runtime. Disk facts (PATH probes,
    /// connector state) arrive here; the core merges its own pane state
    /// (running counts) and answers the client the query was tagged with.
    pub(super) fn on_agent_reply(&mut self, reg: &Registry, reply: uniterm_proto::AgentToCore) {
        match reply {
            uniterm_proto::AgentToCore::ControlRequest {
                connection,
                request,
            } => {
                self.on_control_request(reg, connection, request);
            }
            uniterm_proto::AgentToCore::WorktreeFinished { request, result } => {
                self.finish_worktree_operation(reg, request, result);
            }
            uniterm_proto::AgentToCore::AgentsDiskState { client, providers } => {
                let items = self.merge_agents_snapshot(providers);
                let token = Token(client as usize);
                if let Some(c) = self.clients.get_mut(&token) {
                    c.queue(&encode_frame(&ServerMessage::Agents { items }));
                    c.flush();
                    let _ = set_interest(reg, c, token);
                }
            }
            uniterm_proto::AgentToCore::WorkspaceCatalogState {
                client,
                mut entries,
            } => {
                if let Some(current) = entries.iter_mut().find(|entry| entry.name == self.name) {
                    current.windows = u32::try_from(self.windows.len()).unwrap_or(u32::MAX);
                    current.panes = u32::try_from(self.panes.len()).unwrap_or(u32::MAX);
                    current.projects = u32::try_from(self.projects.len()).unwrap_or(u32::MAX);
                    current.running = true;
                } else {
                    entries.push(uniterm_proto::WorkspaceInfo {
                        name: self.name.clone(),
                        windows: u32::try_from(self.windows.len()).unwrap_or(u32::MAX),
                        panes: u32::try_from(self.panes.len()).unwrap_or(u32::MAX),
                        projects: u32::try_from(self.projects.len()).unwrap_or(u32::MAX),
                        running: true,
                    });
                }
                entries.sort_by(|left, right| left.name.cmp(&right.name));
                let token = Token(client as usize);
                if let Some(c) = self.clients.get_mut(&token) {
                    c.queue(&encode_frame(&ServerMessage::Workspaces {
                        current: self.name.clone(),
                        entries,
                    }));
                    c.flush();
                    let _ = set_interest(reg, c, token);
                }
            }
            uniterm_proto::AgentToCore::ConfigSaved { client, error } => {
                self.reply_settings(reg, Token(client as usize), error.is_none(), error);
            }
            uniterm_proto::AgentToCore::EditorSettingsValidated {
                client,
                editor,
                editor_rules,
                error,
            } => {
                let token = Token(client as usize);
                if let Some(error) = error {
                    self.reply_settings(reg, token, false, Some(error));
                } else {
                    self.config.editor = editor;
                    self.config.editor_rules = editor_rules;
                    self.agents.send(uniterm_proto::CoreToAgent::ConfigSave {
                        client,
                        text: self.config.to_text(),
                    });
                }
            }
            uniterm_proto::AgentToCore::EditorResolved {
                project,
                path,
                command,
                error,
            } => {
                if project != self.files.project || project != self.active_project {
                    return;
                }
                if let Some(error) = error {
                    self.files.error = Some(error);
                    if self.file_manager_visible() {
                        self.full_repaint_all(reg);
                    }
                    return;
                }
                let launch = format!(
                    "{command} -- {}; exec {}",
                    crate::workflow::shell_quote(&path),
                    crate::workflow::shell_quote(&self.program)
                );
                let args = ["-c", launch.as_str()];
                match self.spawn_pane(reg, &args) {
                    Ok(pane) => {
                        self.push_window(pane);
                        self.files.focused = false;
                        self.relayout();
                        self.full_repaint_all(reg);
                        self.persist();
                    }
                    Err(error) => {
                        self.files.error = Some(format!("Could not open editor: {error}"));
                        self.full_repaint_all(reg);
                    }
                }
            }
            uniterm_proto::AgentToCore::DevServersDetected { pane, servers } => {
                if !self.panes.contains_key(&pane) {
                    return;
                }
                let mut changed = false;
                for server in servers {
                    let key = (pane, server.port);
                    let same = self.dev_servers.get(&key).is_some_and(|current| {
                        current.label == server.label && current.url == server.url
                    });
                    if !same {
                        let value = TrackedDevServer {
                            label: server.label,
                            url: server.url,
                            detected: self.next_dev_server_sequence,
                        };
                        self.next_dev_server_sequence += 1;
                        self.dev_servers.insert(key, value);
                        changed = true;
                    }
                }
                if changed {
                    self.broadcast_dev_servers(reg);
                }
            }
            uniterm_proto::AgentToCore::DevServerDown { pane, port } => {
                self.agents
                    .send(uniterm_proto::CoreToAgent::DevServerForget { pane, port });
                if self.dev_servers.remove(&(pane, port)).is_some() {
                    self.broadcast_dev_servers(reg);
                }
            }
            uniterm_proto::AgentToCore::AgentDetected {
                pane,
                foreground_pid,
                agent,
                status,
                authority,
                evidence,
                provenance,
            } => self.apply_agent_detection(
                reg,
                pane,
                foreground_pid,
                agent,
                status,
                authority,
                evidence,
                provenance,
            ),
            uniterm_proto::AgentToCore::ProviderManifestsReloaded => {
                self.refresh_provider_evidence();
            }
            uniterm_proto::AgentToCore::SetAgentStatus { pane, status } => {
                let transition = self
                    .panes
                    .get_mut(&pane)
                    .and_then(|pane| pane.agent.as_mut())
                    .and_then(|agent| {
                        if agent.status != status {
                            let previous = agent.status;
                            agent.status = status;
                            Some((previous, status))
                        } else {
                            None
                        }
                    });
                if let Some((previous, status)) = transition {
                    self.append_event(crate::eventlog::LogEvent::AgentStatus {
                        pane: pane.0,
                        status,
                    });
                    self.notify_agent_transition(pane, previous, status);
                    self.full_repaint_all(reg);
                }
            }
            uniterm_proto::AgentToCore::InjectText { pane, text } => {
                if let Some(pane) = self.panes.get_mut(&pane) {
                    Self::queue_pane_input(reg, pane, text.as_bytes());
                }
            }
            uniterm_proto::AgentToCore::FileListing {
                project,
                directory,
                entries,
                truncated,
                error,
            } => {
                if self
                    .files
                    .finish_listing(project, directory, entries, truncated, error)
                    && self.file_manager_visible()
                {
                    self.sync_file_watches();
                    self.full_repaint_all(reg);
                }
            }
            uniterm_proto::AgentToCore::FileChanged { project, directory } => {
                if self.file_manager_visible() && project == self.files.project {
                    self.request_file_listing(directory);
                }
            }
            uniterm_proto::AgentToCore::FileMutationDone {
                project,
                directory,
                error,
            } => {
                if self.files.finish_mutation(project, error) && self.file_manager_visible() {
                    self.request_file_listing(directory);
                    self.full_repaint_all(reg);
                }
            }
            uniterm_proto::AgentToCore::GitChangeStats { project, stats } => {
                if self.file_manager_visible() && self.files.finish_git_stats(project, stats) {
                    self.full_repaint_all(reg);
                }
            }
            uniterm_proto::AgentToCore::WaitingItem { pane, summary } => {
                let invocation = self.panes.get(&pane).and_then(|pane| pane.foreground_pid);
                let change =
                    self.waiting
                        .observe_agent(pane, invocation, AgentStatus::Question, &summary);
                self.record_waiting_change(change);
            }
            uniterm_proto::AgentToCore::ArtifactFilesChanged { artifacts } => {
                for artifact in artifacts {
                    self.request_artifact_observation(artifact);
                }
            }
            uniterm_proto::AgentToCore::ArtifactObserved {
                artifact,
                observation,
                missing,
                error,
            } => {
                self.pending_artifact_observations.remove(&artifact);
                if error.is_none() {
                    self.apply_artifact_observation(artifact, observation, missing);
                }
                if self.dirty_artifact_observations.remove(&artifact) {
                    self.request_artifact_observation(artifact);
                }
            }
            uniterm_proto::AgentToCore::ArtifactValidated {
                kind,
                task_id,
                token,
                artifacts,
                error,
            } => {
                let Some(index) =
                    self.pending_orchestration_submissions
                        .iter()
                        .position(|pending| {
                            pending.kind == kind
                                && pending.task_id == task_id
                                && pending.token == token
                        })
                else {
                    return;
                };
                let pending = self.pending_orchestration_submissions.swap_remove(index);
                if let Some(error) = error {
                    self.report_artifact_gate_failure(kind, token, &error);
                    return;
                }
                if !self.record_validated_artifacts(task_id, &artifacts) {
                    let error = self
                        .durability_error
                        .clone()
                        .unwrap_or_else(|| "Artifact ownership could not be recorded".into());
                    self.report_artifact_gate_failure(kind, token, &error);
                    return;
                }
                let artifact_claims: Vec<_> = artifacts
                    .iter()
                    .map(|artifact| uniterm_proto::ArtifactClaim {
                        kind: artifact.kind,
                        path: artifact.path.clone(),
                    })
                    .collect();
                match kind {
                    uniterm_proto::OrchestrationKind::Workflow => self.on_workflow_submit(
                        reg,
                        token,
                        pending.status,
                        pending.verdict,
                        pending.summary,
                        artifact_claims,
                        true,
                    ),
                    uniterm_proto::OrchestrationKind::Relay => self.on_relay_submit(
                        reg,
                        token,
                        pending.status,
                        pending.summary,
                        artifact_claims,
                        true,
                    ),
                }
            }
            uniterm_proto::AgentToCore::RelayCheckpointCreated {
                task_id,
                token,
                checkpoint,
                error,
            } => {
                let Some(pending_index) = self
                    .pending_relay_activations
                    .iter()
                    .position(|pending| pending.task_id == task_id && pending.token == token)
                else {
                    return;
                };
                let pending = self.pending_relay_activations.swap_remove(pending_index);
                let Some(run_index) = self
                    .relays
                    .iter()
                    .position(|run| run.task_id == task_id && run.state.token == token)
                else {
                    return;
                };
                self.append_event(crate::eventlog::LogEvent::RelayCheckpointCreated {
                    task_id,
                    token,
                    checkpoint: checkpoint.clone(),
                    error,
                });
                let mut run = self.relays.swap_remove(run_index);
                if let Some(checkpoint) = checkpoint {
                    run.checkpoints.push((token, checkpoint));
                }
                self.apply_relay_action(
                    reg,
                    &mut run,
                    uniterm_core::orchestrate::Action::Inject {
                        role: pending.role,
                        token,
                    },
                    pending.handoff.as_deref(),
                );
                self.append_event(crate::eventlog::LogEvent::OrchestrationProjected {
                    run: run.durable(),
                });
                self.relays.push(run);
            }
            uniterm_proto::AgentToCore::RelayCheckpointRolledBack {
                waiting_id,
                task_id,
                checkpoint,
                error,
            } => {
                if let Some(error) = error {
                    if let Some(item) = self.waiting.get(waiting_id).cloned() {
                        let change = self.waiting.request(
                            item.pane,
                            item.invocation,
                            uniterm_core::WaitingKind::Relay,
                            &format!("checkpoint rollback failed: {error}"),
                        );
                        self.record_waiting_change(change);
                    }
                    return;
                }
                self.append_event(crate::eventlog::LogEvent::RelayCheckpointRolledBack {
                    task_id,
                    checkpoint,
                });
                if let Some(item) = self.waiting.resolve(waiting_id) {
                    self.append_event(crate::eventlog::LogEvent::WaitingResolved {
                        id: item.id,
                        resolution: uniterm_core::WaitingResolution::Resumed,
                    });
                    self.resume_waiting_orchestration(reg, item.pane);
                }
            }
            uniterm_proto::AgentToCore::DurabilityError {
                workspace,
                operation,
                error,
            } => {
                let message = format!("{operation} failed for Workspace {workspace}: {error}");
                if self.durability_error.as_deref() != Some(&message) {
                    eprintln!("uniterm: {message}");
                    self.durability_error = Some(message);
                    self.full_repaint_all(reg);
                }
            }
            uniterm_proto::AgentToCore::SpawnPane { .. } => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn apply_agent_detection(
        &mut self,
        reg: &Registry,
        pane_id: PaneId,
        foreground_pid: Option<i32>,
        detected_agent: Option<String>,
        status: Option<AgentStatus>,
        authority: uniterm_proto::DetectionAuthority,
        evidence: String,
        provenance: uniterm_proto::DetectionProvenance,
    ) {
        if detected_agent.is_some() {
            if let Some(pid) = foreground_pid {
                self.register_process_watch(reg, pane_id, pid);
            }
        }
        let previous_status = self
            .panes
            .get(&pane_id)
            .and_then(|pane| pane.agent.as_ref())
            .map(|agent| agent.status)
            .unwrap_or(AgentStatus::Unknown);
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        pane.foreground_pid = foreground_pid;
        let mut changed = false;
        if let Some(id) = detected_agent {
            let replace = pane
                .agent
                .as_ref()
                .is_none_or(|current| current.id != id && authority >= current.authority);
            if replace {
                let color = uniterm_core::agent::agent_color_or_default(&id);
                let initial = status.unwrap_or(AgentStatus::Starting);
                if let Some((name, line)) = self.log.record(crate::eventlog::LogEvent::AgentBound {
                    pane: pane_id.0,
                    agent: id.clone(),
                }) {
                    self.agents
                        .send(uniterm_proto::CoreToAgent::EventAppend { name, line });
                }
                pane.agent = Some(PaneAgent {
                    id,
                    color,
                    status: initial,
                    authority,
                    evidence: evidence.clone(),
                    provenance: provenance.clone(),
                    foreground_pid,
                    started_at: std::time::Instant::now(),
                    session_id: None,
                    resume_command: Vec::new(),
                });
                changed = true;
            } else if let Some(agent) = pane.agent.as_mut() {
                agent.foreground_pid = foreground_pid.or(agent.foreground_pid);
            }
        }
        let Some(status) = status else {
            if changed {
                self.full_repaint_all(reg);
            }
            return;
        };
        let Some(agent) = pane.agent.as_ref() else {
            return;
        };
        // Lower-authority grid evidence cannot replace a current cooperative
        // signal, with three exceptions. Permission and question are safety
        // states that must outrank a stale "working" hook. Starting is a
        // bootstrap state that any real verdict may leave, so a cooperative
        // `session_start` does not freeze the Pane until the next hook event.
        // And a screen that keeps saying idle may, after a long stretch,
        // replace a cooperative Working whose idle hook never fired.
        let stale_fallback = stale_cooperative_fallback(agent.status, agent.authority, status);
        if authority < agent.authority
            && !status.needs_human()
            && agent.status != AgentStatus::Starting
            && !stale_fallback
        {
            return;
        }
        if agent.status == status
            && agent.authority == authority
            && agent.evidence == evidence
            && same_detection_provenance(&agent.provenance, &provenance)
            && !changed
        {
            self.panes
                .get_mut(&pane_id)
                .and_then(|pane| pane.agent.as_mut())
                .expect("agent was present above")
                .provenance = provenance;
            return;
        }
        let mut dwell = provenance
            .dwell_ms
            .map(std::time::Duration::from_millis)
            .unwrap_or_else(|| detection_dwell(status));
        if stale_fallback && authority < agent.authority {
            dwell = dwell.max(STALE_COOPERATIVE_ACTIVITY);
        }
        let mut transition = None;
        if dwell.is_zero() || authority >= uniterm_proto::DetectionAuthority::Osc777 {
            let pane = self.panes.get_mut(&pane_id).unwrap();
            let agent = pane.agent.as_mut().unwrap();
            agent.status = status;
            agent.authority = authority;
            agent.evidence = evidence;
            agent.provenance = provenance;
            pane.detection_candidate = None;
            if previous_status != status || changed {
                self.append_event(crate::eventlog::LogEvent::AgentStatus {
                    pane: pane_id.0,
                    status,
                });
            }
            if previous_status != status {
                transition = Some((previous_status, status));
            }
            changed = true;
        } else {
            let keep_since = pane
                .detection_candidate
                .as_ref()
                .filter(|candidate| {
                    candidate.status == status
                        && candidate.authority == authority
                        && candidate.evidence == evidence
                        && same_detection_provenance(&candidate.provenance, &provenance)
                        && candidate.dwell == dwell
                })
                .map(|candidate| candidate.since);
            pane.detection_candidate = Some(DetectionCandidate {
                status,
                authority,
                evidence,
                provenance,
                dwell,
                since: keep_since.unwrap_or_else(std::time::Instant::now),
            });
        }
        if let Some((previous, status)) = transition {
            self.notify_agent_transition(pane_id, previous, status);
        }
        if changed {
            self.full_repaint_all(reg);
        }
    }

    /// Build the Manage Agents snapshot from the runtime's disk facts plus
    /// this session's pane state: every registry provider, then any custom
    /// agent bound in a pane (so counts, stop-all, and the modal agree on
    /// what is running).
    pub(super) fn merge_agents_snapshot(
        &self,
        providers: Vec<uniterm_proto::ProviderDiskState>,
    ) -> Vec<uniterm_proto::AgentInfo> {
        let mut running = self.running_agents();
        let mut items: Vec<uniterm_proto::AgentInfo> = providers
            .into_iter()
            .map(|d| {
                let p = uniterm_core::agent::provider(&d.id);
                uniterm_proto::AgentInfo {
                    name: p
                        .map(|p| p.name.to_string())
                        .unwrap_or_else(|| d.id.clone()),
                    command: p
                        .map(|p| p.command.to_string())
                        .unwrap_or_else(|| d.id.clone()),
                    running: running.remove(&d.id).unwrap_or(0),
                    id: d.id,
                    installed: d.installed,
                    connector: d.connector,
                }
            })
            .collect();
        // Custom agents (bound under a literal command): running here is the
        // evidence of installation; no connector exists for them.
        let mut custom: Vec<(String, u32)> = running.into_iter().collect();
        custom.sort();
        items.extend(custom.into_iter().map(|(id, n)| uniterm_proto::AgentInfo {
            name: id.clone(),
            command: id.clone(),
            id,
            installed: true,
            connector: uniterm_proto::ConnectorStatus::Unsupported,
            running: n,
        }));
        items
    }

    /// Close a pane (its process exited, or the user killed it): deregister,
    /// remove it, collapse its window's layout, and close the window / stop the
    /// server if it was the last one.
    pub(super) fn close_pane(&mut self, reg: &Registry, pane_id: PaneId) {
        for (token, client) in self.clients.iter_mut() {
            if client
                .direct
                .as_ref()
                .is_some_and(|direct| direct.pane == pane_id)
            {
                client.queue(&encode_frame(&ServerMessage::Exited));
                client.flush();
                let _ = set_interest(reg, client, *token);
            }
        }
        self.pending_prompt_deliveries
            .retain(|delivery| delivery.pane != pane_id);
        self.cancel_orchestrations_for_pane(pane_id);
        self.agents
            .send(uniterm_proto::CoreToAgent::PaneClosed { pane: pane_id });
        self.resolve_waiting_for_pane(pane_id, uniterm_core::WaitingResolution::PaneClosed);
        self.cancel_pane_instructions(pane_id);
        let dev_servers_changed = {
            let before = self.dev_servers.len();
            self.dev_servers.retain(|(pane, _), _| *pane != pane_id);
            self.dev_servers.len() != before
        };
        self.pending_notifications.remove(&pane_id);
        if self
            .notification
            .as_ref()
            .is_some_and(|toast| toast.pane == pane_id)
        {
            self.notification = None;
        }
        let empty_project = self
            .windows
            .iter()
            .find(|tab| tab.layout.contains_pane(pane_id))
            .filter(|tab| tab.layout.pane_ids().len() == 1)
            .map(|tab| tab.project)
            .filter(|project| {
                self.projects.len() > 1
                    && self
                        .windows
                        .iter()
                        .filter(|tab| tab.project == *project)
                        .count()
                        == 1
                    && self.projects.iter().any(|item| item.id == *project)
            });
        if let Some(project) = empty_project {
            self.append_event(crate::eventlog::LogEvent::ProjectRemoved { project: project.0 });
            self.projects.retain(|item| item.id != project);
            self.sync_artifact_watches();
        }
        if let Some(token) = self.pane_watches.remove(&pane_id) {
            if let Some((_, mut watch)) = self.process_watches.remove(&token) {
                watch.deregister(reg);
            }
        }
        if let Some(mut pane) = self.panes.remove(&pane_id) {
            let _ = reg.deregister(&mut SourceFd(&pane.pty.raw_fd()));
            self.pane_tokens.remove(&pane.token);
            let _ = pane.pty.kill();
            self.append_event(crate::eventlog::LogEvent::PaneClosed { pane: pane_id.0 });
        }
        let mut removed_window: Option<(usize, ProjectId)> = None;
        for wi in 0..self.windows.len() {
            if !self.windows[wi].layout.contains_pane(pane_id) {
                continue;
            }
            match self.windows[wi].layout.without(pane_id) {
                Some(new_layout) => {
                    if self.windows[wi].active == pane_id {
                        self.windows[wi].active = new_layout.first_pane();
                    }
                    if self.windows[wi].zoomed == Some(pane_id) {
                        self.windows[wi].zoomed = None;
                    }
                    self.windows[wi].layout = new_layout;
                }
                None => {
                    let project = self.windows[wi].project;
                    self.windows.remove(wi);
                    removed_window = Some((wi, project));
                }
            }
            break;
        }
        if self.windows.is_empty() {
            self.shutdown(reg);
            return;
        }
        // Preserve the active window across earlier removals. When the active
        // Tab itself closes, prefer its left neighbor in the same Project.
        if let Some((wi, project)) = removed_window {
            if wi == self.active_window {
                self.active_window = (0..wi)
                    .rev()
                    .find(|index| self.windows[*index].project == project)
                    .or_else(|| self.windows.iter().position(|tab| tab.project == project))
                    .unwrap_or_else(|| wi.saturating_sub(1).min(self.windows.len() - 1));
            } else if wi < self.active_window {
                self.active_window -= 1;
            }
            if self.active_window >= self.windows.len() {
                self.active_window = self.windows.len() - 1;
            }
        }
        let project_changed = self.active_project != self.windows[self.active_window].project;
        self.active_project = self.windows[self.active_window].project;
        if let Some(project) = self
            .projects
            .iter_mut()
            .find(|project| project.id == self.active_project)
        {
            project.active_pane = Some(self.windows[self.active_window].active);
        }
        if project_changed && self.file_manager_visible() {
            self.sync_file_manager(false);
        }
        self.relayout();
        self.full_repaint_all(reg);
        if dev_servers_changed {
            self.broadcast_dev_servers(reg);
        }
        self.persist();
    }
}

pub(super) fn evidence_hash(text: &str) -> u64 {
    // FNV-1a: deterministic, allocation-free, and sufficient for suppressing
    // duplicate evidence snapshots. This is not a security boundary.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub(super) fn direct_detection_provenance(
    source: uniterm_proto::DetectionSource,
    invocation_pid: Option<i32>,
) -> uniterm_proto::DetectionProvenance {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    uniterm_proto::DetectionProvenance::direct(source, timestamp_ms, invocation_pid)
}

pub(super) fn same_detection_provenance(
    left: &uniterm_proto::DetectionProvenance,
    right: &uniterm_proto::DetectionProvenance,
) -> bool {
    left.source == right.source
        && left.manifest_version == right.manifest_version
        && left.matched_rule == right.matched_rule
        && left.confidence == right.confidence
        && left.dwell_ms == right.dwell_ms
        && left.precedence == right.precedence
        && left.capabilities == right.capabilities
        && left.invocation_pid == right.invocation_pid
}

/// How long a cooperative (OSC 777) Working or Tool state may stand against a
/// screen that keeps saying idle before the screen wins. A connector whose
/// idle hook failed or was never installed would otherwise pin the Pane at
/// Working until exit; only a cooperative event, a kernel exit, or a visible
/// permission prompt could move it.
pub(super) const STALE_COOPERATIVE_ACTIVITY: std::time::Duration =
    std::time::Duration::from_secs(30);

pub(super) fn detection_dwell(status: AgentStatus) -> std::time::Duration {
    match status {
        AgentStatus::Permission | AgentStatus::Question => std::time::Duration::from_secs(5),
        // Idle rests on a positive or default screen verdict, not on silence,
        // so its dwell only has to outlast one redraw flicker.
        AgentStatus::Idle => std::time::Duration::from_millis(crate::providers::IDLE_DWELL_MS),
        AgentStatus::Error | AgentStatus::Exited => std::time::Duration::from_secs(2),
        AgentStatus::Unknown | AgentStatus::Starting | AgentStatus::Working | AgentStatus::Tool => {
            std::time::Duration::ZERO
        }
    }
}

pub(super) fn detection_candidate_can_apply(
    current_status: AgentStatus,
    current_authority: uniterm_proto::DetectionAuthority,
    candidate_status: AgentStatus,
    candidate_authority: uniterm_proto::DetectionAuthority,
) -> bool {
    candidate_authority >= current_authority
        || candidate_status.needs_human()
        || current_status == AgentStatus::Starting
        || stale_cooperative_fallback(current_status, current_authority, candidate_status)
}

/// A lower-authority Idle verdict may replace a cooperative Working or Tool
/// state once it has stood for `STALE_COOPERATIVE_ACTIVITY` (the candidate's
/// dwell is stretched to that length when it is parked).
pub(super) fn stale_cooperative_fallback(
    current_status: AgentStatus,
    current_authority: uniterm_proto::DetectionAuthority,
    candidate_status: AgentStatus,
) -> bool {
    current_authority >= uniterm_proto::DetectionAuthority::Log
        && matches!(current_status, AgentStatus::Working | AgentStatus::Tool)
        && candidate_status == AgentStatus::Idle
}

/// Build the wrapped invocation for a provider-owned native resume profile.
/// The resume argv came from child-controlled OSC 777 output in a previous
/// session, so argv[0] must stay inside the provider's declared executables;
/// anything else falls back to a fresh shell rather than executing a forged
/// command under a trusted restore path.
pub(super) fn native_resume_args(
    program: &str,
    profile: &crate::persist::AgentLaunchSnap,
) -> Option<Vec<String>> {
    let argv0 = profile.resume_command.first()?;
    if !crate::providers::resume_argv_allowed(&profile.provider, argv0) {
        return None;
    }
    let invocation = crate::workflow::shell_join(&profile.resume_command)?;
    let wrapped = crate::workflow::announce_wrapped(&profile.provider, &invocation);
    Some(vec![
        "-c".to_string(),
        format!("{wrapped}; exec {}", crate::workflow::shell_quote(program)),
    ])
}

/// Retain the newest complete UTF-8 suffix within `max_bytes` and report
/// whether older bytes were dropped.
pub(super) fn retain_recent_utf8(text: &mut String, max_bytes: usize) -> bool {
    if text.len() <= max_bytes {
        return false;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text.drain(..start);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_resume_rejects_forged_or_foreign_argv() {
        let legit = crate::persist::AgentLaunchSnap {
            provider: "codex".into(),
            session_id: Some("abc".into()),
            resume_command: vec!["codex".into(), "resume".into(), "abc".into()],
        };
        let args = native_resume_args("/bin/sh", &legit).unwrap();
        assert_eq!(args.len(), 2);
        assert!(args[1].contains("'codex' 'resume' 'abc'"));

        let forged = crate::persist::AgentLaunchSnap {
            provider: "codex".into(),
            session_id: None,
            resume_command: vec!["sh".into(), "-c".into(), "rm -rf ~".into()],
        };
        assert!(native_resume_args("/bin/sh", &forged).is_none());

        let self_named_shell = crate::persist::AgentLaunchSnap {
            provider: "sh".into(),
            session_id: Some("forged".into()),
            resume_command: vec!["sh".into(), "-c".into(), "touch /tmp/forged".into()],
        };
        assert!(native_resume_args("/bin/sh", &self_named_shell).is_none());

        let empty = crate::persist::AgentLaunchSnap {
            provider: "codex".into(),
            session_id: None,
            resume_command: Vec::new(),
        };
        assert!(native_resume_args("/bin/sh", &empty).is_none());
    }

    #[test]
    fn recent_output_truncation_keeps_the_utf8_safe_tail() {
        let mut text = "oldénew".to_string();
        assert!(retain_recent_utf8(&mut text, 4));
        assert_eq!(text, "new");

        let mut exact = "énew".to_string();
        assert!(!retain_recent_utf8(&mut exact, 5));
        assert_eq!(exact, "énew");
    }

    #[test]
    fn screen_verdicts_override_cooperative_state_only_when_bootstrapping_or_stale() {
        use uniterm_proto::DetectionAuthority;

        // A cooperative Working may be replaced by a screen Idle, but only
        // through the stale fallback, whose candidate dwell is stretched to
        // `STALE_COOPERATIVE_ACTIVITY` before it can apply.
        assert!(stale_cooperative_fallback(
            AgentStatus::Working,
            DetectionAuthority::Osc777,
            AgentStatus::Idle,
        ));
        assert!(detection_candidate_can_apply(
            AgentStatus::Working,
            DetectionAuthority::Osc777,
            AgentStatus::Idle,
            DetectionAuthority::Grid,
        ));
        // A screen Working never displaces a cooperative Idle, and a screen
        // Idle never displaces a cooperative permission prompt.
        assert!(!detection_candidate_can_apply(
            AgentStatus::Idle,
            DetectionAuthority::Osc777,
            AgentStatus::Working,
            DetectionAuthority::Grid,
        ));
        assert!(!stale_cooperative_fallback(
            AgentStatus::Permission,
            DetectionAuthority::Osc777,
            AgentStatus::Idle,
        ));
        assert!(!stale_cooperative_fallback(
            AgentStatus::Working,
            DetectionAuthority::Grid,
            AgentStatus::Idle,
        ));
        // Starting is a bootstrap state that any real verdict may leave.
        assert!(detection_candidate_can_apply(
            AgentStatus::Starting,
            DetectionAuthority::Osc777,
            AgentStatus::Idle,
            DetectionAuthority::Grid,
        ));
        assert_eq!(
            detection_dwell(AgentStatus::Idle),
            std::time::Duration::from_millis(crate::providers::IDLE_DWELL_MS)
        );
        assert!(detection_candidate_can_apply(
            AgentStatus::Working,
            DetectionAuthority::Log,
            AgentStatus::Permission,
            DetectionAuthority::Grid,
        ));
    }
}
