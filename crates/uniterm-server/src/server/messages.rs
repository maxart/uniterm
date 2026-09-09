//! Client message and command dispatch.
//!
//! One vocabulary: keybindings, CLI controls, and automation all arrive here
//! and resolve to the same semantic command path.

use super::*;

impl Server {
    pub(super) fn handle_msg(
        &mut self,
        reg: &Registry,
        token: Token,
        msg: ClientMessage,
        remove: &mut bool,
    ) {
        let is_direct = self
            .clients
            .get(&token)
            .is_some_and(|client| client.direct_only);
        if is_direct
            && !matches!(
                &msg,
                ClientMessage::PaneAttach { .. }
                    | ClientMessage::Input(_)
                    | ClientMessage::Resize { .. }
                    | ClientMessage::Refresh
                    | ClientMessage::FocusGained
                    | ClientMessage::Detach
            )
        {
            return;
        }
        match msg {
            ClientMessage::Attach { cols, rows, .. } => {
                let was_visible = self.file_manager_visible();
                let had_clients = self
                    .clients
                    .values()
                    .any(|client| client.attached && !client.dead);
                if let Some(c) = self.clients.get_mut(&token) {
                    c.attached = true;
                    c.direct = None;
                    c.cols = cols.max(1);
                    c.rows = rows.max(1);
                }
                let geometry_changed = self.recompute_client_geometry();
                self.reconcile_file_manager_runtime(was_visible, had_clients);
                if geometry_changed {
                    self.relayout();
                    self.full_repaint_all(reg);
                } else {
                    self.full_repaint_client(reg, token);
                }
            }
            ClientMessage::Resize { cols, rows } => {
                if is_direct {
                    self.full_repaint_direct_client(reg, token);
                    return;
                }
                let was_visible = self.file_manager_visible();
                let had_clients = self
                    .clients
                    .values()
                    .any(|client| client.attached && !client.dead);
                if let Some(c) = self.clients.get_mut(&token) {
                    c.cols = cols.max(1);
                    c.rows = rows.max(1);
                }
                let geometry_changed = self.recompute_client_geometry();
                self.reconcile_file_manager_runtime(was_visible, had_clients);
                if geometry_changed {
                    self.relayout();
                    self.full_repaint_all(reg);
                } else {
                    self.full_repaint_client(reg, token);
                }
            }
            ClientMessage::Input(bytes) => {
                if is_direct {
                    if let Some((pane, can_control)) = self.clients.get(&token).and_then(|client| {
                        client
                            .direct
                            .as_ref()
                            .map(|direct| (direct.pane, direct.role.can_control()))
                    }) {
                        if can_control {
                            if let Some(pane) = self.panes.get_mut(&pane) {
                                Self::queue_pane_input(reg, pane, &bytes);
                            }
                        }
                    }
                    return;
                }
                // Context menus, the file tree, overview, and copy-mode own
                // keyboard input while active; none of it reaches the PTY.
                if self.context_menu.is_some() {
                    self.handle_context_input(reg, &bytes);
                } else if self.files.focused && self.file_manager_visible() {
                    let action = self.files.handle(&bytes);
                    self.handle_file_action(reg, action);
                } else if self.overview.is_some() {
                    self.handle_overview_input(reg, &bytes);
                } else if !self.handle_copy_input(reg, &bytes) {
                    let active = self.windows[self.active_window].active;
                    if let Some(p) = self.panes.get_mut(&active) {
                        Self::queue_pane_input(reg, p, &bytes);
                    }
                }
            }
            ClientMessage::PaneAttach { pane, role } => {
                self.attach_direct_client(reg, token, pane, role)
            }
            ClientMessage::Command(cmd) => self.handle_command(reg, cmd),
            ClientMessage::ListInfo => {
                let info = ServerMessage::Info {
                    windows: self.windows.len() as u32,
                    panes: self.panes.len() as u32,
                };
                if let Some(c) = self.clients.get_mut(&token) {
                    c.queue(&encode_frame(&info));
                    c.flush();
                    let _ = set_interest(reg, c, token);
                }
            }
            ClientMessage::KillServer => {
                // Only an explicit stop command (`ut workspace stop`, `ut kill`,
                // the menu's kill action) sends this, so the request itself is
                // the confirmation; the decision is still recorded first.
                self.guard_semantic(uniterm_core::GuardedCommand::WorkspaceStop, true, None);
                self.kill_all_panes();
                self.shutdown(reg);
            }
            ClientMessage::Refresh => self.full_repaint_client(reg, token),
            ClientMessage::FocusGained => self.full_repaint_client(reg, token),
            ClientMessage::OverlayVisible { on } => {
                if let Some(c) = self.clients.get_mut(&token) {
                    c.overlay = on;
                }
                if on && self.context_menu.take().is_some() {
                    self.full_repaint_all(reg);
                } else if !on {
                    // Pane output continued updating the server-side grids
                    // while the client-owned overlay covered the canvas. Send
                    // one current frame now instead of streaming hidden frames
                    // through the overlay and forcing it to repaint repeatedly.
                    self.full_repaint_client(reg, token);
                }
            }
            ClientMessage::RenameSession { name } => {
                self.rename_session(reg, &name);
                self.reply_workspace(reg, token);
            }
            ClientMessage::WorkspaceState => self.reply_workspace(reg, token),
            ClientMessage::ProjectCreate { name, root } => {
                let error = self.create_project(reg, &name, &root).err();
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::ProjectCreated { error }));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
                self.reply_workspace(reg, token);
            }
            ClientMessage::ProjectRename { project, name } => {
                self.rename_project(reg, project, &name);
                self.reply_workspace(reg, token);
            }
            ClientMessage::ProjectMove { project, direction } => {
                self.move_project(reg, project, direction);
                self.reply_workspace(reg, token);
            }
            ClientMessage::ProjectSwitch { project } => {
                self.switch_project(reg, project);
                self.reply_workspace(reg, token);
            }
            ClientMessage::ProjectRemove { project, confirmed } => {
                if !self.guard_semantic(
                    uniterm_core::GuardedCommand::ProjectRemove,
                    confirmed,
                    Some(project),
                ) {
                    self.show_guardrail_toast(reg, "Project removal needs confirmation");
                    return;
                }
                if self
                    .projects
                    .iter()
                    .find(|item| item.id == project)
                    .and_then(Self::worktree_registration)
                    .is_some()
                {
                    self.start_worktree_operation(
                        reg,
                        WorktreeRequester::ClientWorkspace(token),
                        uniterm_proto::WorktreeOperation::Remove {
                            project,
                            force: false,
                        },
                    );
                } else {
                    self.remove_project(reg, project);
                    self.reply_workspace(reg, token);
                }
            }
            ClientMessage::WorkspaceImport { workspace, mode } => {
                let result = self.import_workspace(reg, &workspace, mode);
                let message = match result {
                    Ok((projects_added, tabs_added, projects_merged)) => {
                        ServerMessage::WorkspaceImported {
                            projects_added,
                            tabs_added,
                            projects_merged,
                            error: None,
                        }
                    }
                    Err(error) => ServerMessage::WorkspaceImported {
                        projects_added: 0,
                        tabs_added: 0,
                        projects_merged: 0,
                        error: Some(error),
                    },
                };
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&message));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            ClientMessage::Settings => self.reply_settings(reg, token, true, None),
            ClientMessage::SettingsApply(patch) => self.apply_settings(reg, token, patch),
            ClientMessage::AgentExplain { pane } => {
                let entries = self.detection_snapshot(pane);
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::AgentExplanation { entries }));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            ClientMessage::PaneList => {
                let panes = self.pane_snapshot();
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::Panes {
                        workspace: self.name.clone(),
                        panes,
                    }));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            ClientMessage::RunList {
                project,
                active_only,
            } => {
                let runs = self.run_snapshot(project, active_only);
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::Runs {
                        workspace: self.name.clone(),
                        runs,
                    }));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            ClientMessage::ArtifactList {
                project,
                run,
                include_superseded,
            } => {
                let artifacts = self.artifact_snapshot(project, run, include_superseded);
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::Artifacts {
                        workspace: self.name.clone(),
                        artifacts,
                    }));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            ClientMessage::RunFork { fork } => self.start_run_fork(
                reg,
                WorktreeRequester::RunForkClient {
                    token,
                    parent: fork.parent,
                },
                fork,
            ),
            ClientMessage::PaneFocus { pane } => {
                let found = self.focus_pane_target(reg, pane);
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::PaneFocused { pane, found }));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            ClientMessage::HierarchyFocus { project, tab, pane } => {
                let focused = self.focus_hierarchy_target(reg, project, tab, pane);
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::HierarchyFocused {
                        project,
                        tab,
                        pane,
                        focused,
                    }));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            ClientMessage::TabMove { direction } => {
                let moved = self.move_active_tab(direction);
                if moved {
                    self.relayout();
                    self.full_repaint_all(reg);
                }
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::TabMoved { moved }));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            ClientMessage::PaneRead { pane, lines } => {
                let (found, text, truncated) = self
                    .bounded_pane_output(pane, lines)
                    .map_or((false, String::new(), false), |(text, truncated)| {
                        (true, text, truncated)
                    });
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::PaneOutput {
                        pane,
                        found,
                        text,
                        truncated,
                    }));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            ClientMessage::PaneSend { pane, bytes } => {
                let (found, accepted) = if let Some(pane) = self.panes.get_mut(&pane) {
                    (true, Self::queue_pane_input(reg, pane, &bytes))
                } else {
                    (false, false)
                };
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::PaneSent {
                        pane,
                        found,
                        accepted,
                    }));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            ClientMessage::PaneWaitOutput {
                pane,
                needle,
                timeout_ms,
            } => {
                if let Some(client) = self.clients.get_mut(&token) {
                    client.pending_wait = Some(PendingControlWait::Output {
                        pane,
                        needle: needle.chars().take(4_096).collect(),
                        deadline: std::time::Instant::now()
                            + std::time::Duration::from_millis(timeout_ms.clamp(1, 3_600_000)),
                    });
                }
                self.service_pending_waits(reg);
            }
            ClientMessage::AgentWait {
                pane,
                status,
                timeout_ms,
            } => {
                if let Some(client) = self.clients.get_mut(&token) {
                    client.pending_wait = Some(PendingControlWait::Agent {
                        pane,
                        status,
                        deadline: std::time::Instant::now()
                            + std::time::Duration::from_millis(timeout_ms.clamp(1, 3_600_000)),
                    });
                }
                self.service_pending_waits(reg);
            }
            ClientMessage::WaitingList => self.reply_waiting(reg, token),
            ClientMessage::WaitingAct { id, action, text } => {
                let (found, accepted) = self.waiting_action(reg, id, action, &text);
                let items = self.waiting_snapshot();
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::WaitingActed {
                        id,
                        found,
                        accepted,
                        items,
                    }));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            ClientMessage::InstructionList => self.reply_instructions(reg, token),
            ClientMessage::InstructionAdd { pane, author, text } => {
                let result = self.instruction_command(
                    reg,
                    instruction::InstructionCommand::Add { pane, author, text },
                );
                self.reply_instruction_change(reg, token, result);
            }
            ClientMessage::InstructionReplace { id, author, text } => {
                let result = self.instruction_command(
                    reg,
                    instruction::InstructionCommand::Replace { id, author, text },
                );
                self.reply_instruction_change(reg, token, result);
            }
            ClientMessage::InstructionCancel { id } => {
                let result =
                    self.instruction_command(reg, instruction::InstructionCommand::Cancel { id });
                self.reply_instruction_change(reg, token, result);
            }
            ClientMessage::InstructionSendNow { id } => {
                let result =
                    self.instruction_command(reg, instruction::InstructionCommand::SendNow { id });
                self.reply_instruction_change(reg, token, result);
            }
            ClientMessage::PaneMetadata {
                pane,
                key,
                value,
                ttl_seconds,
            } => self.set_pane_metadata(reg, pane, &key, &value, ttl_seconds),
            ClientMessage::ProjectMetadata {
                project,
                key,
                value,
            } => {
                self.set_project_metadata(reg, project, &key, &value);
                self.reply_workspace(reg, token);
            }
            ClientMessage::Worktree { operation } => {
                self.start_worktree_operation(reg, WorktreeRequester::Client(token), operation)
            }
            ClientMessage::RenameWindow { name } => {
                let trimmed = name.trim();
                let wi = self.active_window;
                self.windows[wi].name = (!trimmed.is_empty()).then(|| trimmed.to_string());
                self.append_event(crate::eventlog::LogEvent::WindowRenamed {
                    window: wi as u64,
                    name: trimmed.to_string(),
                });
                self.full_repaint_all(reg);
                self.persist();
            }
            ClientMessage::NewTask {
                prompt,
                relay,
                agent,
                role_providers,
                workflow,
                project,
            } => {
                let result = match workflow {
                    Some(name) => self.launch_workflow(
                        reg,
                        &name,
                        agent.as_deref(),
                        &role_providers,
                        &prompt,
                        project.as_deref(),
                    ),
                    None if relay => self.launch_relay(
                        reg,
                        agent.as_deref(),
                        &role_providers,
                        &prompt,
                        project.as_deref(),
                    ),
                    None if !role_providers.is_empty() => {
                        Err("role provider selections require a workflow or relay".to_string())
                    }
                    None => {
                        self.new_task(reg, &prompt, false, agent.as_deref(), project.as_deref());
                        Ok(uniterm_core::RunId(0))
                    }
                };
                if let Err(error) = result {
                    self.notification = Some(AgentToast {
                        pane: self.windows[self.active_window].active,
                        title: "Automation launch refused".into(),
                        body: error.chars().take(16_384).collect(),
                        expires: std::time::Instant::now() + std::time::Duration::from_secs(8),
                    });
                    self.full_repaint_all(reg);
                }
            }
            ClientMessage::WorkflowSubmit {
                token,
                failed,
                verdict,
                summary,
            } => self.on_workflow_submit(
                reg,
                token,
                if failed {
                    uniterm_proto::SubmissionStatus::Failed
                } else {
                    uniterm_proto::SubmissionStatus::Done
                },
                verdict,
                summary,
                Vec::new(),
                false,
            ),
            ClientMessage::OrchestrationSubmit {
                kind,
                token,
                status,
                verdict,
                summary,
                artifacts,
            } => match kind {
                uniterm_proto::OrchestrationKind::Workflow => {
                    self.on_workflow_submit(reg, token, status, verdict, summary, artifacts, false)
                }
                uniterm_proto::OrchestrationKind::Relay => {
                    self.on_relay_submit(reg, token, status, summary, artifacts, false)
                }
            },
            ClientMessage::Observatory => {
                let fleet = self.fleet_snapshot();
                let dev_servers = self.dev_server_snapshot();
                let waiting = self.waiting_snapshot();
                if let Some(c) = self.clients.get_mut(&token) {
                    c.queue(&encode_frame(&ServerMessage::Fleet { entries: fleet }));
                    c.queue(&encode_frame(&ServerMessage::DevServers {
                        entries: dev_servers,
                    }));
                    c.queue(&encode_frame(&ServerMessage::Waiting { items: waiting }));
                    c.flush();
                    let _ = set_interest(reg, c, token);
                }
            }
            ClientMessage::Suggest => {
                let projects = self.project_names();
                let agents =
                    crate::workflow::installed_agents_on_search_path(&self.agent_search_path);
                if let Some(c) = self.clients.get_mut(&token) {
                    c.queue(&encode_frame(&ServerMessage::Suggestions {
                        projects,
                        agents,
                    }));
                    c.flush();
                    let _ = set_interest(reg, c, token);
                }
            }
            ClientMessage::TaskSetStatus { id, status } => {
                if self.tasks.set_status(id, status) {
                    self.append_event(crate::eventlog::LogEvent::TaskStatusChanged { id, status });
                }
                self.reply_tasks(reg, token);
            }
            ClientMessage::TaskRetitle { id, title } => {
                let title = title.trim().to_string();
                if !title.is_empty() && self.tasks.set_title(id, &title) {
                    self.append_event(crate::eventlog::LogEvent::TaskRetitled { id, title });
                }
                self.reply_tasks(reg, token);
            }
            ClientMessage::TaskDelete { id } => {
                if self.tasks.remove(id) {
                    self.append_event(crate::eventlog::LogEvent::TaskDeleted { id });
                }
                self.reply_tasks(reg, token);
            }
            ClientMessage::Tasks => self.reply_tasks(reg, token),
            // The Manage Agents surfaces touch the filesystem (PATH probes,
            // settings reads/edits), so the work runs on the agent runtime;
            // the merged snapshot comes back through [`Server::on_agent_reply`].
            // The reply always re-reads reality rather than trusting intent.
            ClientMessage::Agents => {
                self.agents
                    .send(uniterm_proto::CoreToAgent::AgentsDiskQuery {
                        client: token.0 as u64,
                        search_path: self.agent_search_path.clone(),
                    });
            }
            ClientMessage::WorkspaceList => {
                self.agents
                    .send(uniterm_proto::CoreToAgent::WorkspaceCatalogQuery {
                        client: token.0 as u64,
                    });
            }
            ClientMessage::ConnectorToggle { agent } => {
                self.agents
                    .send(uniterm_proto::CoreToAgent::ConnectorToggle {
                        agent,
                        client: token.0 as u64,
                        search_path: self.agent_search_path.clone(),
                    });
            }
            ClientMessage::AgentLaunch { agent, target } => {
                let pane = self.launch_agent(reg, &agent, target);
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::AgentLaunchResult {
                        agent,
                        pane,
                    }));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            ClientMessage::AgentFocus { pane } => {
                self.focus_pane_target(reg, pane);
            }
            ClientMessage::AgentStop { pane } => {
                if self
                    .panes
                    .get(&pane)
                    .is_some_and(|pane| pane.agent.is_some())
                {
                    self.close_pane(reg, pane);
                }
            }
            ClientMessage::AgentsStopAll { scope, confirmed } => {
                let project = match scope {
                    uniterm_proto::StopScope::Project(project) => Some(project),
                    _ => None,
                };
                if !self.guard_semantic(
                    uniterm_core::GuardedCommand::AgentsStopAll,
                    confirmed,
                    project,
                ) {
                    self.show_guardrail_toast(reg, "Stopping every agent needs confirmation");
                    return;
                }
                self.stop_all_agents(reg, scope);
                self.agents
                    .send(uniterm_proto::CoreToAgent::AgentsDiskQuery {
                        client: token.0 as u64,
                        search_path: self.agent_search_path.clone(),
                    });
            }
            ClientMessage::RemoteEnvironment { search_path } => {
                let unattached = self
                    .clients
                    .get(&token)
                    .is_some_and(|client| !client.attached);
                if unattached {
                    if let Some(search_path) = normalize_remote_search_path(search_path) {
                        self.agent_search_path =
                            merge_search_paths(search_path, &self.agent_search_path);
                    }
                }
            }
            ClientMessage::SaveTask { title } => {
                if !title.trim().is_empty() {
                    self.create_task(title.trim(), uniterm_core::TaskStatus::Todo);
                }
            }
            ClientMessage::Mouse { x, y, kind } => self.on_mouse(reg, token, x, y, kind),
            ClientMessage::Detach => {
                if let Some(c) = self.clients.get_mut(&token) {
                    c.queue(&encode_frame(&ServerMessage::Detached));
                    c.flush();
                }
                *remove = true;
            }
        }
    }

    pub(super) fn handle_command(&mut self, reg: &Registry, cmd: Command) {
        let dismissed_context_menu = self.context_menu.take().is_some();
        match cmd {
            Command::Split(axis) => {
                let dir = match axis {
                    SplitAxis::LeftRight => SplitDir::Horizontal,
                    SplitAxis::TopBottom => SplitDir::Vertical,
                };
                if let Ok(new_id) = self.spawn_pane(reg, &[]) {
                    let wi = self.active_window;
                    let active = self.windows[wi].active;
                    self.windows[wi].layout.split(active, dir, new_id);
                    self.last_active_pane = Some(active);
                    self.windows[wi].active = new_id;
                    self.windows[wi].zoomed = None;
                    self.relayout();
                    self.full_repaint_all(reg);
                }
            }
            Command::Focus(fd) => {
                let dir = match fd {
                    FocusDir::Left => Direction::Left,
                    FocusDir::Right => Direction::Right,
                    FocusDir::Up => Direction::Up,
                    FocusDir::Down => Direction::Down,
                };
                let wi = self.active_window;
                if let Some(n) = neighbor(&self.current_layout.panes, self.windows[wi].active, dir)
                {
                    self.last_active_pane = Some(self.windows[wi].active);
                    self.windows[wi].active = n;
                    self.windows[wi].zoomed = None;
                    // Full repaint so the brighten/dim of the newly-focused vs
                    // previously-focused pane is applied (not just a cursor move).
                    self.relayout();
                    self.full_repaint_all(reg);
                }
            }
            Command::Overview => {
                self.overview = match self.overview {
                    Some(_) => None,
                    None => Some(
                        self.project_window_indices(self.active_project)
                            .iter()
                            .position(|window| *window == self.active_window)
                            .unwrap_or(0),
                    ),
                };
                self.full_repaint_all(reg);
            }
            Command::ZoomToggle => {
                let wi = self.active_window;
                let a = self.windows[wi].active;
                self.windows[wi].zoomed = if self.windows[wi].zoomed.is_some() {
                    None
                } else {
                    Some(a)
                };
                self.relayout();
                self.full_repaint_all(reg);
            }
            Command::KillPane => {
                let a = self.windows[self.active_window].active;
                self.close_pane(reg, a);
            }
            Command::NewWindow => {
                if let Ok(id) = self.spawn_pane(reg, &[]) {
                    self.push_window(id);
                    self.relayout();
                    self.full_repaint_all(reg);
                }
            }
            Command::NextWindow => {
                let tabs = self.project_window_indices(self.active_project);
                if tabs.len() > 1 {
                    let current = tabs
                        .iter()
                        .position(|index| *index == self.active_window)
                        .unwrap_or(0);
                    self.activate_window(tabs[(current + 1) % tabs.len()]);
                    self.relayout();
                    self.full_repaint_all(reg);
                }
            }
            Command::PrevWindow => {
                let tabs = self.project_window_indices(self.active_project);
                if tabs.len() > 1 {
                    let current = tabs
                        .iter()
                        .position(|index| *index == self.active_window)
                        .unwrap_or(0);
                    self.activate_window(tabs[(current + tabs.len() - 1) % tabs.len()]);
                    self.relayout();
                    self.full_repaint_all(reg);
                }
            }
            Command::MoveTab(direction) => {
                if self.move_active_tab(direction) {
                    self.relayout();
                    self.full_repaint_all(reg);
                }
            }
            Command::LastPane => {
                let wi = self.active_window;
                if let Some(previous) = self
                    .last_active_pane
                    .filter(|pane| self.windows[wi].layout.contains_pane(*pane))
                {
                    let current = self.windows[wi].active;
                    self.windows[wi].active = previous;
                    self.last_active_pane = Some(current);
                    self.windows[wi].zoomed = None;
                    self.relayout();
                    self.full_repaint_all(reg);
                }
            }
            Command::SelectWindow(n) => {
                let tabs = self.project_window_indices(self.active_project);
                let idx = n as usize;
                if let Some(&window) = tabs.get(idx).filter(|&&i| i != self.active_window) {
                    self.activate_window(window);
                    self.relayout();
                    self.full_repaint_all(reg);
                }
            }
            Command::KillWindow => {
                // Closing every pane cascades through close_pane: the layout
                // collapses, the emptied window is removed, and the server
                // stops if it was the last window.
                let pids = self.windows[self.active_window].layout.pane_ids();
                self.terminate_panes(&pids);
                for pid in pids {
                    self.close_pane(reg, pid);
                    if !self.running {
                        return;
                    }
                }
            }
            Command::CopyMode => self.enter_copy_mode(reg),
            Command::ResizePane(fd) => {
                // Map a direction to (split orientation, ratio delta). A step of
                // 0.05 (5% of the split) feels right for repeated taps.
                let (orient, delta) = match fd {
                    FocusDir::Left => (SplitDir::Horizontal, -0.05),
                    FocusDir::Right => (SplitDir::Horizontal, 0.05),
                    FocusDir::Up => (SplitDir::Vertical, -0.05),
                    FocusDir::Down => (SplitDir::Vertical, 0.05),
                };
                let wi = self.active_window;
                let active = self.windows[wi].active;
                if self.windows[wi].layout.resize_pane(active, orient, delta) {
                    self.relayout();
                    self.full_repaint_all(reg);
                }
            }
            Command::SidebarToggle => {
                self.config.sidebar = !self.config.sidebar;
                self.relayout();
                self.full_repaint_all(reg);
                self.agents.send(uniterm_proto::CoreToAgent::ConfigSave {
                    client: 0,
                    text: self.config.to_text(),
                });
            }
            Command::FileSidebarToggle => {
                self.config.file_sidebar =
                    !self.config.file_sidebar || self.observatory_tab != ObservatoryTab::Files;
                if self.config.file_sidebar {
                    self.observatory_tab = ObservatoryTab::Files;
                    self.sync_file_manager(true);
                } else {
                    self.files.focused = false;
                    self.stop_file_watches();
                }
                self.relayout();
                self.full_repaint_all(reg);
                self.agents.send(uniterm_proto::CoreToAgent::ConfigSave {
                    client: 0,
                    text: self.config.to_text(),
                });
            }
            Command::Observatory => {
                self.config.file_sidebar = !self.config.file_sidebar;
                if self.config.file_sidebar && self.observatory_tab == ObservatoryTab::Files {
                    self.sync_file_manager(false);
                } else if !self.config.file_sidebar {
                    self.files.focused = false;
                    self.stop_file_watches();
                }
                self.relayout();
                self.full_repaint_all(reg);
                self.agents.send(uniterm_proto::CoreToAgent::ConfigSave {
                    client: 0,
                    text: self.config.to_text(),
                });
            }
        }
        if dismissed_context_menu {
            self.full_repaint_all(reg);
        }
        // Any command may have changed the structure; keep the snapshot current.
        self.persist();
    }
}
