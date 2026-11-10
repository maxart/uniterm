//! Event-backed workspace projections for the mio server.
//!
//! Append ordering, snapshots, and recovery projections stay here so future
//! control subscriptions can share one event authority.

use super::*;

/// Maximum time newly dirty terminal history waits for a crash checkpoint.
const DIRTY_SNAPSHOT_CADENCE: std::time::Duration = std::time::Duration::from_secs(2);

fn arm_dirty_snapshot(
    current: Option<std::time::Instant>,
    now: std::time::Instant,
) -> std::time::Instant {
    current.unwrap_or(now + DIRTY_SNAPSHOT_CADENCE)
}

impl Server {
    pub(super) fn append_event(&mut self, event: crate::eventlog::LogEvent) {
        if !self.event_writes_enabled {
            return;
        }
        use crate::eventlog::LogEvent;
        let task_id = match &event {
            LogEvent::TaskCreated { id, .. }
            | LogEvent::TaskStatusChanged { id, .. }
            | LogEvent::TaskRetitled { id, .. }
            | LogEvent::TaskDetailsUpdated { id, .. } => Some(*id),
            _ => None,
        };
        self.tasks_dirty |= task_id.is_some()
            || matches!(
                &event,
                LogEvent::TaskDeleted { .. }
                    | LogEvent::ArtifactLedger { .. }
                    | LogEvent::ProjectSelected { .. }
                    | LogEvent::ProjectRenamed { .. }
                    | LogEvent::ProjectDirectoryChanged { .. }
                    | LogEvent::AgentStatus { .. }
                    | LogEvent::AgentBound { .. }
                    | LogEvent::AgentResumed { .. }
                    | LogEvent::AgentUnbound { .. }
                    | LogEvent::PaneClosed { .. }
                    | LogEvent::RunGraph { .. }
                    | LogEvent::WaitingCreated { .. }
                    | LogEvent::WaitingResolved { .. }
                    | LogEvent::WaitingUpdated { .. }
            );
        let invocation = match &event {
            LogEvent::AgentBound { pane, .. } | LogEvent::AgentResumed { pane, .. } => {
                Some((PaneId(*pane), true))
            }
            crate::eventlog::LogEvent::AgentUnbound { pane }
            | crate::eventlog::LogEvent::PaneClosed { pane } => Some((PaneId(*pane), false)),
            _ => None,
        };
        let resumed = match &event {
            LogEvent::AgentResumed {
                pane,
                previous_invocation: Some(previous),
                ..
            } => Some((PaneId(*pane), *previous)),
            _ => None,
        };
        if let Some((name, line)) = self.log.record(event) {
            if let Some((pane, previous)) = resumed {
                self.tasks
                    .rebind_invocation(pane, previous, self.log.current_sequence());
            }
            if let Some((pane, started)) = invocation {
                if started {
                    self.timeline_invocations
                        .insert(pane, self.log.current_sequence());
                } else {
                    self.timeline_invocations.remove(&pane);
                }
            }
            if let Some(id) = task_id {
                self.tasks.touch(id, self.log.current_sequence());
            }
            self.agents
                .send(uniterm_proto::CoreToAgent::EventAppend { name, line });
        }
    }

    // --- persistence -------------------------------------------------------

    pub(super) fn workspace_definition(&self) -> Option<uniterm_proto::WorkspaceDefinition> {
        let projects: Vec<_> = self
            .projects
            .iter()
            .filter_map(|project| {
                let windows = self.project_window_indices(project.id);
                if windows.is_empty() {
                    return None;
                }
                let active_tab = if project.id == self.active_project {
                    windows
                        .iter()
                        .position(|index| *index == self.active_window)
                        .unwrap_or(0)
                } else {
                    project
                        .active_pane
                        .and_then(|pane| {
                            windows
                                .iter()
                                .position(|index| self.windows[*index].layout.contains_pane(pane))
                        })
                        .unwrap_or(0)
                };
                Some(uniterm_proto::WorkspaceProjectDefinition {
                    id: project.id,
                    name: project.name.clone(),
                    root: project.root.clone(),
                    worktree: Self::worktree_registration(project),
                    active_tab,
                    tabs: windows
                        .iter()
                        .map(|index| uniterm_proto::WorkspaceTabDefinition {
                            stack_expanded: self.windows[*index].stack_expanded,
                            parent_tab: self.windows[*index]
                                .stack_parent
                                .and_then(|parent| {
                                    windows.iter().position(|index| {
                                        self.windows[*index].layout.contains_pane(parent)
                                    })
                                })
                                .map(|index| index as u32 + 1),
                            layout_mode: self.windows[*index].layout_mode,
                            name: self.windows[*index].name.clone(),
                            layout: workspace_layout_definition(&self.windows[*index].layout),
                        })
                        .collect(),
                })
            })
            .collect();
        (!projects.is_empty()).then_some(uniterm_proto::WorkspaceDefinition {
            version: uniterm_proto::WorkspaceDefinition::VERSION,
            active_project: self.active_project,
            agent_scope_workspace: self.sidebar_agent_scope == SidebarScope::Workspace,
            server_scope_workspace: self.sidebar_server_scope == SidebarScope::Workspace,
            file_sort: self.files.sort,
            projects,
        })
    }

    pub(super) fn persist_workspace_definition(&mut self) {
        if !self.workspace_catalog_enabled {
            return;
        }
        let Some(definition) = self.workspace_definition() else {
            return;
        };
        if let Ok(line) = crate::workspace_catalog::encode(&definition) {
            // Every dirty checkpoint records the definition, but only the
            // latest line is ever read back. An unchanged definition would
            // grow the catalog by a full copy every couple of seconds of
            // terminal output for no information, exactly as the event log
            // suppresses identical adjacent records.
            if self.pending_catalog_line.is_none()
                && self.last_catalog_line.as_deref() == Some(line.as_str())
            {
                return;
            }
            self.pending_catalog_line = Some(line.clone());
            self.agents
                .send(uniterm_proto::CoreToAgent::WorkspaceCatalogAppend {
                    name: self.name.clone(),
                    line,
                });
        }
    }

    /// Build a snapshot of the current session structure.
    pub(super) fn snapshot(&self) -> crate::persist::Snapshot<uniterm_core::GridCapture> {
        let windows = self
            .windows
            .iter()
            .map(|w| crate::persist::WinSnap {
                stack_expanded: w.stack_expanded,
                stack_parent: w.stack_parent,
                layout_mode: w.layout_mode,
                project: w.project,
                layout: w.layout.clone(),
                active: w.active,
                zoomed: w.zoomed,
                name: w.name.clone(),
                panes: w
                    .layout
                    .pane_ids()
                    .into_iter()
                    .map(|id| {
                        let pane = self.panes.get(&id);
                        if let Some(recovery) = pane.and_then(|pane| pane.recovery.as_ref()) {
                            let mut saved = recovery.saved.clone();
                            // Metadata can still be changed through normal commands.
                            saved.metadata = pane
                                .unwrap()
                                .metadata
                                .iter()
                                .filter(|(_, value)| value.expires.is_none())
                                .map(|(key, value)| (key.clone(), value.value.clone()))
                                .collect();
                            saved.metadata.sort();
                            return saved;
                        }
                        crate::persist::PaneSnap {
                            id,
                            cwd: pane
                                .and_then(|p| p.pty.cwd().or_else(|| p.cwd.clone()))
                                .map(|p| p.to_string_lossy().into_owned()),
                            content: pane
                                .map(|p| {
                                    p.term
                                        .grid()
                                        .capture_lines(crate::persist::CONTENT_LINE_CAP)
                                })
                                .unwrap_or_default(),
                            metadata: pane
                                .map(|pane| {
                                    let mut values: Vec<(String, String)> = pane
                                        .metadata
                                        .iter()
                                        .filter(|(_, value)| value.expires.is_none())
                                        .map(|(key, value)| (key.clone(), value.value.clone()))
                                        .collect();
                                    values.sort();
                                    values
                                })
                                .unwrap_or_default(),
                            launch_args: pane
                                .map(|pane| pane.launch_args.clone())
                                .unwrap_or_default(),
                            agent_launch: pane.and_then(|pane| {
                                pane.agent
                                    .as_ref()
                                    .map(|agent| crate::persist::AgentLaunchSnap {
                                        provider: agent.id.clone(),
                                        session_id: agent.session_id.clone(),
                                        resume_command: agent.resume_command.clone(),
                                    })
                            }),
                        }
                    })
                    .collect(),
            })
            .collect();
        let projects = self
            .projects
            .iter()
            .map(|project| crate::persist::ProjectSnap {
                id: project.id,
                name: project.name.clone(),
                root: project.root.clone(),
                active_pane: if project.id == self.active_project {
                    self.windows.get(self.active_window).map(|tab| tab.active)
                } else {
                    project.active_pane
                },
                metadata: {
                    let mut values: Vec<(String, String)> = project
                        .metadata
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                    values.sort();
                    values
                },
            })
            .collect();
        let mut snapshot = crate::persist::Snapshot::new_with_sequence(
            self.active_window,
            self.next_pane_id,
            self.active_project,
            self.next_project_id,
            projects,
            windows,
            self.log.current_sequence(),
        );
        snapshot.run_graph = self.run_graph.clone();
        snapshot.run_graph_sequence = self.log.current_sequence();
        snapshot.artifacts = self.artifacts.clone();
        snapshot.artifact_sequence = self.log.current_sequence();
        snapshot
    }

    /// Persist the current structure atomically (called after structural change).
    pub(super) fn persist(&mut self) {
        self.snapshot_deadline = None;
        // A clean stop has already recorded the definition and queued the
        // deletion of the snapshot and stream; a later checkpoint must not
        // resurrect either.
        if !self.event_writes_enabled {
            return;
        }
        self.persist_workspace_definition();
        self.persist_snapshot(self.snapshot());
    }

    /// Retain only unavailable process profiles on clean stop. Other Panes
    /// restart as fresh shells, preserving the ordinary intentional-stop rule.
    pub(super) fn persist_unavailable_on_stop(&mut self) {
        let mut snapshot = self.snapshot();
        for pane in snapshot
            .windows
            .iter_mut()
            .flat_map(|window| &mut window.panes)
        {
            if self
                .panes
                .get(&pane.id)
                .is_none_or(|pane| pane.recovery.is_none())
            {
                pane.content = Default::default();
                pane.launch_args.clear();
                pane.agent_launch = None;
                pane.metadata.clear();
            }
        }
        self.persist_snapshot(snapshot);
    }

    fn persist_snapshot(
        &mut self,
        mut snapshot: crate::persist::Snapshot<uniterm_core::GridCapture>,
    ) {
        self.append_event(crate::eventlog::LogEvent::WorkspaceProjected {
            state: crate::eventlog::StructuralProjection::from_snapshot(&snapshot),
        });
        snapshot.event_sequence = self.log.current_sequence();
        self.agents.send(uniterm_proto::CoreToAgent::SnapshotSave {
            name: self.name.clone(),
            snapshot: Box::new(snapshot),
        });
    }

    /// Arm one bounded checkpoint after terminal output first makes the
    /// projection dirty. Later output keeps the original deadline, which
    /// guarantees progress even when a Pane streams continuously.
    pub(super) fn mark_snapshot_dirty(&mut self) {
        self.snapshot_deadline = Some(arm_dirty_snapshot(
            self.snapshot_deadline,
            std::time::Instant::now(),
        ));
    }

    /// Persist a due dirty terminal projection and fully disarm when clean.
    pub(super) fn flush_snapshot_due(&mut self) {
        if self
            .snapshot_deadline
            .is_some_and(|deadline| deadline <= std::time::Instant::now())
        {
            self.persist();
        }
    }

    /// Spawn fresh shells for one catalog layout without carrying Pane state
    /// across the stop boundary. Partial spawn failures are rolled back so a
    /// Tab never enters the projection with a dangling layout leaf.
    pub(super) fn spawn_workspace_layout(
        &mut self,
        reg: &Registry,
        definition: &uniterm_proto::WorkspaceLayoutDefinition,
        cwd: Option<&Path>,
    ) -> std::io::Result<LayoutNode> {
        let pane_count = definition.pane_count().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid remembered Tab layout",
            )
        })?;
        let mut panes = Vec::with_capacity(pane_count);
        for _ in 0..pane_count {
            match self.spawn_pane_at(reg, &[], cwd) {
                Ok(pane) => panes.push(pane),
                Err(error) => {
                    for pane_id in panes {
                        if let Some(pane) = self.panes.remove(&pane_id) {
                            let _ = reg.deregister(&mut SourceFd(&pane.pty.raw_fd()));
                            self.pane_tokens.remove(&pane.token);
                            self.retire_pty(reg, pane.pty);
                            self.append_event(crate::eventlog::LogEvent::PaneClosed {
                                pane: pane_id.0,
                            });
                        }
                    }
                    return Err(error);
                }
            }
        }
        workspace_layout_with_panes(definition, &mut panes.into_iter()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "remembered Tab layout did not match its Pane count",
            )
        })
    }

    /// Recreate a stopped Workspace from its lightweight definition. Every
    /// anonymous layout leaf gets a fresh shell at its Project root; no Pane
    /// content, process, command, metadata, or identity crosses the boundary.
    pub(super) fn restore_workspace_definition(
        &mut self,
        reg: &Registry,
        definition: uniterm_proto::WorkspaceDefinition,
    ) {
        if !definition.is_valid() {
            return;
        }
        self.restore_workspace_preferences(&definition);
        let existing: Vec<PaneId> = self.panes.keys().copied().collect();
        self.terminate_panes(&existing);
        for (_, pane) in std::mem::take(&mut self.panes) {
            let _ = reg.deregister(&mut SourceFd(&pane.pty.raw_fd()));
            self.retire_pty(reg, pane.pty);
        }
        self.pane_tokens.clear();
        self.windows.clear();
        self.projects.clear();

        for project in &definition.projects {
            self.append_event(crate::eventlog::LogEvent::ProjectCreated {
                project: project.id.0,
                name: project.name.clone(),
                root: project.root.clone(),
            });
            self.projects.push(Project {
                id: project.id,
                name: project.name.clone(),
                root: project.root.clone(),
                active_pane: None,
                metadata: project
                    .worktree
                    .as_ref()
                    .map(Self::worktree_metadata)
                    .unwrap_or_default(),
            });
        }

        for project in &definition.projects {
            let cwd = Path::new(&project.root)
                .is_dir()
                .then(|| Path::new(&project.root));
            let mut project_panes = Vec::new();
            for tab in &project.tabs {
                let layout = self
                    .spawn_workspace_layout(reg, &tab.layout, cwd)
                    .or_else(|_| self.spawn_workspace_layout(reg, &tab.layout, None));
                let Ok(layout) = layout else {
                    project_panes.push(None);
                    continue;
                };
                let active = layout.first_pane();
                self.append_event(crate::eventlog::LogEvent::WindowNew);
                if let Some(name) = &tab.name {
                    self.append_event(crate::eventlog::LogEvent::WindowRenamed {
                        window: self.windows.len() as u64,
                        name: name.clone(),
                    });
                }
                self.windows.push(Win {
                    stack_expanded: tab.stack_expanded,
                    stack_parent: None,
                    layout_scroll: 0,
                    layout_mode: tab.layout_mode,
                    project: project.id,
                    layout,
                    active,
                    zoomed: None,
                    name: tab.name.clone(),
                });
                project_panes.push(Some(active));
            }
            for (index, tab) in project.tabs.iter().enumerate() {
                if let (Some(child), Some(parent)) = (
                    project_panes.get(index).copied().flatten(),
                    tab.parent_tab
                        .and_then(|p| p.checked_sub(1))
                        .and_then(|p| project_panes.get(p as usize))
                        .copied()
                        .flatten(),
                ) {
                    self.assign_tab_parent(child, Some(parent));
                }
            }
            if let Some(active) = project_panes
                .get(
                    project
                        .active_tab
                        .min(project_panes.len().saturating_sub(1)),
                )
                .copied()
                .flatten()
                .or_else(|| project_panes.iter().flatten().copied().next())
            {
                if let Some(restored) = self
                    .projects
                    .iter_mut()
                    .find(|restored| restored.id == project.id)
                {
                    restored.active_pane = Some(active);
                }
            }
        }

        let live_projects: Vec<ProjectId> = self.windows.iter().map(|tab| tab.project).collect();
        self.projects
            .retain(|project| live_projects.contains(&project.id));
        if self.windows.is_empty() || self.projects.is_empty() {
            return;
        }
        self.next_project_id = self
            .projects
            .iter()
            .map(|project| project.id.0 + 1)
            .max()
            .unwrap_or(2);
        self.active_project = if self
            .projects
            .iter()
            .any(|project| project.id == definition.active_project)
        {
            definition.active_project
        } else {
            self.projects[0].id
        };
        let active_pane = self
            .projects
            .iter()
            .find(|project| project.id == self.active_project)
            .and_then(|project| project.active_pane);
        self.active_window = active_pane
            .and_then(|pane| {
                self.windows
                    .iter()
                    .position(|window| window.layout.contains_pane(pane))
            })
            .unwrap_or(0);
        if self.file_manager_visible() {
            self.sync_file_manager(false);
        }
        self.relayout();
    }

    /// Rebuild the session from a snapshot, replacing whatever `bind` set up.
    pub(super) fn restore(
        &mut self,
        reg: &Registry,
        snap: crate::persist::Snapshot,
    ) -> std::io::Result<()> {
        // Tear down the default window/pane that `bind` created.
        let existing: Vec<PaneId> = self.panes.keys().copied().collect();
        self.terminate_panes(&existing);
        for (_, pane) in std::mem::take(&mut self.panes) {
            let _ = reg.deregister(&mut SourceFd(&pane.pty.raw_fd()));
            self.retire_pty(reg, pane.pty);
        }
        self.pane_tokens.clear();
        self.recovery_panes.clear();
        self.native_resumes.clear();
        self.native_restore_requests.clear();
        self.windows.clear();
        self.run_graph = snap.run_graph.clone();
        self.run_graph_sequence = snap.run_graph_sequence;
        self.artifacts = snap.artifacts.clone();
        self.artifact_sequence = snap.artifact_sequence;
        self.projects = snap
            .projects
            .iter()
            .map(|project| Project {
                id: project.id,
                name: project.name.clone(),
                root: project.root.clone(),
                active_pane: project.active_pane,
                metadata: project.metadata.iter().cloned().collect(),
            })
            .collect();

        for w in snap.windows {
            // Snapshots written on macOS before Pane cwd caching always held
            // `None`, because `/proc/<pid>/cwd` is Linux-only. Project roots
            // are independently durable, so they are the safe migration and
            // last-resort directory for every Pane in that Project.
            let project_cwd = self
                .projects
                .iter()
                .find(|project| project.id == w.project)
                .map(|project| std::path::PathBuf::from(&project.root));
            for ps in &w.panes {
                self.restore_pane(reg, ps.clone(), Some(w.project), project_cwd.as_deref())?;
            }
            // Invalid checkpoint leaves without a Pane record cannot appear
            // in the layout. Failed process launches already have placeholders.
            let mut layout = Some(w.layout);
            for id in layout
                .as_ref()
                .map(LayoutNode::pane_ids)
                .unwrap_or_default()
            {
                if !self.panes.contains_key(&id) {
                    layout = layout.and_then(|tree| tree.without(id));
                }
            }
            if let Some(layout) = layout {
                let active = if layout.contains_pane(w.active) {
                    w.active
                } else {
                    layout.first_pane()
                };
                self.windows.push(Win {
                    stack_expanded: w.stack_expanded,
                    stack_parent: w.stack_parent,
                    layout_scroll: 0,
                    layout_mode: w.layout_mode,
                    project: w.project,
                    layout,
                    active,
                    zoomed: w.zoomed.filter(|pane| self.panes.contains_key(pane)),
                    name: w.name,
                });
            }
        }
        if self.windows.is_empty() {
            return Err(std::io::Error::other(
                "snapshot has no recoverable Panes; original checkpoint retained",
            ));
        }
        self.next_pane_id = snap.next_pane_id.max(self.next_pane_id);
        self.next_project_id = snap.next_project_id.max(
            self.projects
                .iter()
                .map(|project| project.id.0 + 1)
                .max()
                .unwrap_or(2),
        );
        self.active_window = snap.active_window.min(self.windows.len().saturating_sub(1));
        self.active_project = self
            .windows
            .get(self.active_window)
            .map(|tab| tab.project)
            .filter(|project| *project == snap.active_project)
            .unwrap_or_else(|| self.windows[self.active_window].project);
        if self.file_manager_visible() {
            self.sync_file_manager(false);
        }
        self.relayout();
        Ok(())
    }

    fn restore_pane(
        &mut self,
        reg: &Registry,
        saved: crate::persist::PaneSnap,
        project: Option<ProjectId>,
        project_cwd: Option<&Path>,
    ) -> std::io::Result<()> {
        self.restore_pane_resolved(reg, saved, project, project_cwd, None)
    }

    /// Rebuild a saved Pane only after any native executable check completes.
    pub(super) fn restore_pane_resolved(
        &mut self,
        reg: &Registry,
        mut saved: crate::persist::PaneSnap,
        project: Option<ProjectId>,
        project_cwd: Option<&Path>,
        resolved: Option<Result<Vec<String>, String>>,
    ) -> std::io::Result<()> {
        if let Some(profile) = saved.agent_launch.as_mut() {
            crate::providers::complete_resume(profile);
        }
        // An agent seen without a session id has nothing to resume, and every
        // retry would fail the same way: start the provider fresh, or, with no
        // provider-owned launch, restore its plain shell.
        let unresumable = saved
            .agent_launch
            .take_if(|profile| {
                profile.resume_command.is_empty()
                    && crate::providers::fresh_argv(&profile.provider).is_none()
            })
            .map(|profile| profile.provider);
        let fresh = saved
            .agent_launch
            .as_ref()
            .filter(|profile| profile.resume_command.is_empty())
            .map(|profile| sanitize_chrome_text(&profile.provider, 40));
        let pending_native = saved.agent_launch.is_some() && resolved.is_none();
        let profile_to_resolve = pending_native.then(|| saved.agent_launch.clone()).flatten();
        let id = saved.id;
        let mut preflight_error = None;
        let mut resume = match resolved {
            Some(Ok(argv)) => saved
                .agent_launch
                .as_ref()
                .and_then(|profile| native_resume::resolved_args(&self.program, profile, &argv)),
            Some(Err(error)) => {
                preflight_error = Some(error);
                None
            }
            None => None,
        };
        if pending_native {
            preflight_error = Some("Checking the saved provider executable".into());
        } else if saved.agent_launch.is_some() && resume.is_none() && preflight_error.is_none() {
            preflight_error = Some("The recorded provider resume command is not trusted".into());
        }
        let project_cwd = project_cwd.filter(|path| path.is_absolute());
        let saved_cwd = saved
            .cwd
            .as_ref()
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
        let cwd = saved_cwd.as_deref().or(project_cwd);
        if saved
            .metadata
            .iter()
            .any(|(key, value)| key == "uniterm.manager.scope" && value == "read_only")
        {
            if let Some(args) = resume.as_mut() {
                self.restrict_manager_resume(args);
            }
        }
        let launch_args = if saved.agent_launch.is_some() {
            resume.unwrap_or_default()
        } else {
            saved.launch_args.clone()
        };
        let args: Vec<_> = launch_args.iter().map(String::as_str).collect();
        let mut result = if let Some(error) = preflight_error.as_ref() {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                error.clone(),
            ))
        } else {
            cwd.map_or_else(
                || {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "no absolute saved or Project directory",
                    ))
                },
                |cwd| self.spawn_pane_with_id(reg, saved.id, &args, Some(cwd)),
            )
        };
        if result.is_err()
            && preflight_error.is_none()
            && project_cwd.is_some()
            && saved_cwd.is_some()
            && saved_cwd.as_deref() != project_cwd
        {
            result = self.spawn_pane_with_id(reg, saved.id, &args, project_cwd);
        }
        let failure = result.err();
        if failure.is_some() {
            let pty = PtyProcess::unavailable(self.cols, self.rows).map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!(
                        "cannot retain unavailable Pane {}: {error}; original checkpoint retained",
                        saved.id.0
                    ),
                )
            })?;
            self.insert_pane(reg, saved.id, &[], saved_cwd.as_deref(), pty)?;
        }
        let pane = self
            .panes
            .get_mut(&saved.id)
            .expect("inserted restored Pane");
        pane.term.grid_mut().load_scrollback(&saved.content);
        pane.metadata = saved
            .metadata
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    MetadataValue {
                        value: value.clone(),
                        expires: None,
                    },
                )
            })
            .collect();
        if let Some(error) = failure {
            let diagnostic = sanitize_chrome_text(&error.to_string(), 300);
            if pending_native {
                pane.term
                    .feed(b"Restoring saved agent...\r\nSaved output remains in scrollback.\r\n");
            } else {
                pane.term.feed(format!("Uniterm could not restore this Pane.\r\n{diagnostic}\r\nSaved output remains in scrollback.\r\nRestore the directory or command, then press Enter to retry.\r\n").as_bytes());
            }
            pane.launch_args = saved.launch_args.clone();
            pane.recovery = Some(PaneRecovery {
                saved: crate::persist::PaneSnap {
                    id: saved.id,
                    cwd: saved.cwd,
                    content: uniterm_core::GridCapture::from_stored_lines(saved.content),
                    metadata: saved.metadata,
                    launch_args: saved.launch_args,
                    agent_launch: saved.agent_launch,
                },
                retry: false,
                pending: self.recovery_pending.clone(),
            });
            if !self.recovery_panes.contains(&saved.id) {
                self.recovery_panes.push(saved.id);
            }
        } else if let Some(provider) = unresumable {
            let provider = sanitize_chrome_text(&provider, 40);
            pane.term.feed(
                format!("Uniterm saved no {provider} session for this Pane.\r\nStarted a shell.\r\nSaved output remains in scrollback.\r\n")
                    .as_bytes(),
            );
        } else if !launch_args.is_empty() {
            if let Some(provider) = fresh {
                pane.term.feed(
                    format!("Uniterm saved no {provider} session for this Pane.\r\nStarted a new {provider} session.\r\nSaved output remains in scrollback.\r\n")
                        .as_bytes(),
                );
            }
            if let Some(profile) = saved.agent_launch {
                let previous = self.restored_invocations.get(&saved.id).copied();
                self.bind_native_agent(saved.id, profile, previous, None);
            }
        }
        if let Some(profile) = profile_to_resolve {
            self.request_native_restore(id, project, project_cwd.map(Path::to_path_buf), profile);
        }
        Ok(())
    }

    /// Retry only on explicit input. No timer or filesystem probe runs while
    /// an unavailable Pane waits for its directory or executable to return.
    pub(super) fn retry_restored_panes(&mut self, reg: &Registry) {
        if !self.running {
            return;
        }
        if !self
            .recovery_pending
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        self.recovery_pending
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let pending = std::mem::take(&mut self.recovery_panes);
        for id in pending {
            if self.native_restore_pending(id) {
                self.recovery_panes.push(id);
                if let Some(recovery) = self
                    .panes
                    .get_mut(&id)
                    .and_then(|pane| pane.recovery.as_mut())
                {
                    recovery.retry = false;
                }
                continue;
            }
            let Some(pane) = self.panes.get_mut(&id) else {
                continue;
            };
            let Some(recovery) = pane.recovery.as_mut() else {
                continue;
            };
            if !std::mem::take(&mut recovery.retry) {
                self.recovery_panes.push(id);
                continue;
            }
            let saved = recovery.saved.clone();
            let saved = crate::persist::PaneSnap {
                id: saved.id,
                cwd: saved.cwd,
                content: saved.content.into_stored_lines(),
                metadata: pane
                    .metadata
                    .iter()
                    .filter(|(_, value)| value.expires.is_none())
                    .map(|(key, value)| (key.clone(), value.value.clone()))
                    .collect(),
                launch_args: saved.launch_args,
                agent_launch: saved.agent_launch,
            };
            let project = self
                .windows
                .iter()
                .find(|window| window.layout.contains_pane(id))
                .map(|window| window.project);
            let project_cwd = self
                .windows
                .iter()
                .find(|window| window.layout.contains_pane(id))
                .and_then(|window| {
                    self.projects
                        .iter()
                        .find(|project| project.id == window.project)
                })
                .map(|project| PathBuf::from(&project.root));
            // The old quiet placeholder remains usable if even allocating a
            // replacement PTY fails. Deregister only after replacement succeeds.
            let old = self.panes.remove(&id).expect("retry Pane exists");
            if let Err(error) = self.restore_pane(reg, saved, project, project_cwd.as_deref()) {
                eprintln!("uniterm: Pane {} retry failed: {error}", id.0);
                self.panes.insert(id, old);
                self.recovery_panes.push(id);
            } else {
                let _ = reg.deregister(&mut SourceFd(&old.pty.raw_fd()));
                self.pane_tokens.remove(&old.token);
                self.relayout();
                self.persist();
                self.full_repaint_all(reg);
            }
        }
    }

    /// Restore durable Observatory filters that are independent of terminal
    /// process state and therefore apply to both clean and crash recovery.
    pub(super) fn restore_workspace_preferences(
        &mut self,
        definition: &uniterm_proto::WorkspaceDefinition,
    ) {
        self.files.set_sort(definition.file_sort);
        self.sidebar_agent_scope = if definition.agent_scope_workspace {
            SidebarScope::Workspace
        } else {
            SidebarScope::Project
        };
        self.sidebar_server_scope = if definition.server_scope_workspace {
            SidebarScope::Workspace
        } else {
            SidebarScope::Project
        };
    }

    /// Repair and rebuild every durable projection before the event loop starts.
    ///
    /// Every projection is replayed into scratch state first and only then
    /// applied, so a damaged stream cannot leave half-restored Panes behind.
    /// A stream this binary cannot interpret is quarantined together with its
    /// snapshot and the Workspace starts from its catalog definition; only a
    /// stream written by a newer schema stays fatal, because a newer binary can
    /// still read it and an older one must not touch it.
    pub(super) fn recover_workspace(
        &mut self,
        reg: &Registry,
        restore: bool,
    ) -> std::io::Result<()> {
        let workspace = self.name.clone();
        match crate::eventlog::repair_consistent_prefix(&workspace) {
            Ok(Some(report)) => {
                eprintln!(
                    "uniterm: repaired Workspace {workspace} to its last consistent event prefix; discarded {} bytes and preserved the original at {}",
                    report.discarded_bytes,
                    report.backup.display()
                );
                // `bind_internal` opened its in-memory sequence projection
                // while taking the Workspace lock. Reopen it after truncation
                // so the next event is exactly the repaired prefix's successor.
                self.log = crate::eventlog::EventLog::open(&workspace);
            }
            Ok(None) => {}
            Err(error) if crate::eventlog::is_future_schema_error(&error) => return Err(error),
            // Ownership damage (a stream that names another Workspace, an
            // invalid rename record) is not something a truncation can fix,
            // but it must not lock the user out either.
            Err(error) => self.quarantine_durable_state(&workspace, &error)?,
        }

        // A crash snapshot is the richer recovery source when enabled.
        // Otherwise a remembered clean-stop definition recreates Projects
        // and Tabs without resurrecting stale process state.
        let mut restored_snapshot = false;
        if restore {
            let recovered = match RecoveredProjections::load(&workspace) {
                Ok(recovered) => Some(recovered),
                Err(error) if crate::eventlog::is_future_schema_error(&error) => {
                    return Err(error);
                }
                Err(error) => {
                    self.quarantine_durable_state(&workspace, &error)?;
                    None
                }
            };
            if let Some(recovered) = recovered {
                restored_snapshot = recovered.snapshot.is_some();
                self.restored_invocations = recovered.invocations;
                if let Some(snapshot) = recovered.snapshot {
                    self.restore(reg, snapshot)?;
                }
                self.tasks = recovered.tasks;
                self.waiting = recovered.waiting;
                self.instructions = recovered.instructions;
                self.run_graph = recovered.run_graph;
                self.run_graph_sequence = self.log.current_sequence();
                self.artifacts = recovered.artifacts;
                self.artifact_sequence = self.log.current_sequence();
                self.event_writes_enabled = true;
                let instruction_panes: Vec<_> = self
                    .instructions
                    .items()
                    .iter()
                    .map(|item| item.pane)
                    .collect();
                for pane in instruction_panes {
                    let invocation = self
                        .panes
                        .get(&pane)
                        .and_then(|pane| pane.agent.as_ref())
                        .and_then(|agent| agent.foreground_pid);
                    self.cancel_stale_instructions(pane, invocation);
                }
                self.resolve_stale_waiting_items();
                self.restore_orchestrations(recovered.runs);
            } else {
                self.event_writes_enabled = true;
            }
        } else {
            crate::persist::delete(self.name());
            self.event_writes_enabled = true;
        }

        // Catalogs written before identical definitions were suppressed can
        // hold thousands of dead copies; fold them once, atomically, so every
        // later load and listing parses one line.
        if let Err(error) = crate::workspace_catalog::compact(self.name()) {
            eprintln!(
                "uniterm: could not compact the Workspace {} definition catalog: {error}",
                self.name()
            );
        }
        let workspace_definition = crate::workspace_catalog::load(self.name());
        if restored_snapshot {
            if let Some(definition) = workspace_definition.as_ref() {
                self.restore_workspace_preferences(definition);
            }
        } else if let Some(definition) = workspace_definition {
            self.restore_workspace_definition(reg, definition);
        }

        // The ledger is recovered before the runtime receives filesystem
        // ownership. Rebuild its complete watch set once, after Projects have
        // also been restored, without polling or touching files on the core.
        self.sync_artifact_watches();

        // Make even an untouched single-Pane Workspace crash-restorable.
        self.persist();
        Ok(())
    }

    /// A waiting item belongs to one agent invocation in one Pane. After any
    /// restart that invocation is gone (a clean stop closed it, a crash lost
    /// it, a resumed agent is a new invocation), so an item whose Pane no
    /// longer runs that exact invocation is resolved before fresh shells can
    /// reuse its Pane id and wear a prompt that was never theirs.
    fn resolve_stale_waiting_items(&mut self) {
        let stale: Vec<u64> = self
            .waiting
            .items()
            .iter()
            .filter(|item| {
                let live = self
                    .panes
                    .get(&item.pane)
                    .and_then(|pane| pane.agent.as_ref())
                    .and_then(|agent| agent.foreground_pid);
                item.invocation.is_none() || live != item.invocation
            })
            .map(|item| item.id)
            .collect();
        for id in stale {
            if self.waiting.resolve(id).is_some() {
                self.append_event(crate::eventlog::LogEvent::WaitingResolved {
                    id,
                    resolution: uniterm_core::WaitingResolution::PaneClosed,
                });
            }
        }
    }

    /// Set aside a stream and snapshot that recovery rejected, keeping every
    /// byte under a timestamped name, and restart the in-memory sequence so
    /// the fresh stream is self-consistent from its first record.
    fn quarantine_durable_state(
        &mut self,
        workspace: &str,
        error: &std::io::Error,
    ) -> std::io::Result<()> {
        let log_backup = crate::eventlog::quarantine(workspace)?;
        let snapshot_backup = crate::persist::quarantine(workspace)?;
        let preserved = log_backup
            .iter()
            .chain(snapshot_backup.iter())
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(" and ");
        eprintln!(
            "uniterm: Workspace {workspace} crash-recovery state could not be replayed ({error}); starting from the Workspace definition and preserving the original at {preserved}"
        );
        self.log = crate::eventlog::EventLog::open(workspace);
        Ok(())
    }
}

/// Every durable projection replayed into scratch state, so recovery either
/// applies all of them or none.
struct RecoveredProjections {
    invocations: HashMap<PaneId, u64>,
    snapshot: Option<crate::persist::Snapshot>,
    tasks: uniterm_core::TaskList,
    waiting: uniterm_core::WaitingQueue,
    instructions: uniterm_core::InstructionQueue,
    run_graph: uniterm_core::RunGraph,
    artifacts: uniterm_core::ArtifactLedger,
    runs: Vec<crate::eventlog::DurableOrchestration>,
}

impl RecoveredProjections {
    fn load(workspace: &str) -> std::io::Result<Self> {
        // The snapshot is the crash marker. A clean stop deletes it and keeps
        // the stream, so without one the structure comes from the catalog and
        // the stream contributes only the agentic projections below.
        let checkpoint = crate::persist::load(workspace);
        let snapshot = match checkpoint {
            Some(checkpoint) => crate::eventlog::recover_snapshot(workspace, Some(checkpoint))?,
            None => None,
        };
        let invocations = snapshot
            .as_ref()
            .map(|snapshot| crate::eventlog::replay_invocations(workspace, snapshot.event_sequence))
            .transpose()?
            .unwrap_or_default();
        // Each reducer streams independently, so recovery memory is bounded
        // by live projection size rather than lifetime history.
        let mut tasks = uniterm_core::TaskList::new();
        crate::eventlog::replay_tasks(workspace, &mut tasks)?;
        let mut waiting = uniterm_core::WaitingQueue::new();
        crate::eventlog::replay_waiting(workspace, &mut waiting)?;
        let mut instructions = uniterm_core::InstructionQueue::new();
        crate::eventlog::replay_instructions(workspace, &mut instructions)?;
        let (mut run_graph, run_graph_sequence) = snapshot
            .as_ref()
            .map(|snapshot| (snapshot.run_graph.clone(), snapshot.run_graph_sequence))
            .unwrap_or_else(|| (uniterm_core::RunGraph::new(), 0));
        crate::eventlog::replay_run_graph(workspace, run_graph_sequence, &mut run_graph)?;
        let (mut artifacts, artifact_sequence) = snapshot
            .as_ref()
            .map(|snapshot| (snapshot.artifacts.clone(), snapshot.artifact_sequence))
            .unwrap_or_else(|| (uniterm_core::ArtifactLedger::new(), 0));
        crate::eventlog::replay_artifacts(workspace, artifact_sequence, &mut artifacts)?;
        let runs = crate::eventlog::replay_orchestrations(workspace)?;
        Ok(Self {
            invocations,
            snapshot,
            tasks,
            waiting,
            instructions,
            run_graph,
            artifacts,
            runs,
        })
    }
}

#[cfg(test)]
use super::reaper::common;

#[cfg(test)]
mod tests {
    use super::*;

    fn recovery_fixture() -> (Server, Poll, PathBuf) {
        common::isolate_state();
        let name = common::unique_workspace_name();
        let dir = common::socket_root().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join(format!("{name}.sock"));
        let (server, poll) = Server::bind_internal(&socket, "/bin/sh", &[], 80, 24, false).unwrap();
        (server, poll, dir)
    }

    #[test]
    fn unavailable_restore_retains_history_profile_and_retries_through_shared_input() {
        let (mut server, mut poll, dir) = recovery_fixture();
        let id = server.windows[0].active;
        server
            .panes
            .get_mut(&id)
            .unwrap()
            .term
            .feed(b"irreplaceable saved output");
        let mut snapshot = server
            .snapshot()
            .map_content(uniterm_core::GridCapture::into_stored_lines);
        let root = dir.join("missing-project");
        snapshot.projects[0].root = root.to_string_lossy().into_owned();
        snapshot.windows[0].panes[0].cwd =
            Some(root.join("missing-child").to_string_lossy().into_owned());
        snapshot.windows[0].panes[0].launch_args = vec![
            "-c".into(),
            "printf restored-ok; while read -r line; do :; done".into(),
        ];
        snapshot.windows[0].panes[0].metadata = vec![("task-note".into(), "keep-me".into())];
        let saved = snapshot.windows[0].panes[0].clone();
        server.restore(poll.registry(), snapshot).unwrap();
        server.drain_retiring_ptys(&mut poll);
        assert_eq!(server.panes[&id].pty.child_pid(), None);
        assert!(server.panes[&id].agent.is_none());
        let diagnostic = server.panes[&id].term.dump_text();
        let diagnostic_words = diagnostic.split_whitespace().collect::<String>();
        assert!(
            diagnostic_words.contains("PTYprocessspawnfailed"),
            "missing recovery failure: {diagnostic:?}"
        );
        assert!(
            diagnostic_words.contains("pressEntertoretry"),
            "missing retry instruction: {diagnostic:?}"
        );
        assert_eq!(server.windows[0].layout.pane_ids(), vec![id]);
        for _ in 0..3 {
            // The automation send path retries, but a failed attempt must not
            // replace original args/cwd/history with diagnostics or an empty shell.
            server.handle_msg(
                poll.registry(),
                Token(usize::MAX),
                ClientMessage::PaneSend {
                    pane: id,
                    bytes: b"\r".to_vec(),
                },
                &mut false,
            );
            server.retry_restored_panes(poll.registry());
            let current = server
                .snapshot()
                .map_content(uniterm_core::GridCapture::into_stored_lines);
            let pane = &current.windows[0].panes[0];
            assert_eq!(pane.content, saved.content);
            assert_eq!(pane.cwd, saved.cwd);
            assert_eq!(pane.launch_args, saved.launch_args);
            assert_eq!(pane.metadata, saved.metadata);
            assert_eq!(pane.agent_launch, saved.agent_launch);
            assert!(!server
                .recovery_pending
                .load(std::sync::atomic::Ordering::Relaxed));
        }
        // No ordinary key is buffered to be run as a command after recovery.
        assert!(!Server::queue_pane_input(
            poll.registry(),
            server.panes.get_mut(&id).unwrap(),
            b"unsafe command"
        ));
        assert!(server.panes[&id].input.is_empty());
        std::fs::create_dir_all(&root).unwrap();
        // Human Enter follows the same semantic input path as the targeted CLI.
        server.handle_msg(
            poll.registry(),
            Token(usize::MAX),
            ClientMessage::Input(b"\r".to_vec()),
            &mut false,
        );
        server.retry_restored_panes(poll.registry());
        assert!(server.panes[&id].recovery.is_none());
        assert!(server.panes[&id].pty.child_pid().is_some());
        assert_eq!(server.panes[&id].cwd.as_deref(), Some(root.as_path()));
        assert!(server.recovery_panes.is_empty());
        let token = server.panes[&id].token;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !server.panes[&id].term.dump_text().contains("restored-ok") {
            assert!(std::time::Instant::now() < deadline);
            server.on_pty(poll.registry(), token, true, false);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(server.panes[&id].metadata["task-note"].value, "keep-me");
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn clean_stop_retains_unavailable_profiles_but_clears_live_commands() {
        let (mut server, mut poll, dir) = recovery_fixture();
        let first = server.windows[0].active;
        let second = server.spawn_pane(poll.registry(), &[]).unwrap();
        server.push_window(second);
        let mut snapshot = server
            .snapshot()
            .map_content(uniterm_core::GridCapture::into_stored_lines);
        snapshot.projects[0].root = dir.join("still-missing").to_string_lossy().into_owned();
        snapshot.windows[0].panes[0].cwd =
            Some(dir.join("missing-pane").to_string_lossy().into_owned());
        snapshot.windows[0].panes[0].launch_args =
            vec!["-c".into(), "printf recover-original; read -r line".into()];
        snapshot.windows[1].panes[0].cwd = Some(dir.to_string_lossy().into_owned());
        snapshot.windows[1].panes[0].launch_args =
            vec!["-c".into(), "printf live-command; read -r line".into()];
        let original_args = snapshot.windows[0].panes[0].launch_args.clone();
        server.restore(poll.registry(), snapshot).unwrap();
        server.event_writes_enabled = true;
        server.shutdown(poll.registry());
        server.retire_all_ptys(&mut poll);
        server.agents.shutdown();
        let saved =
            crate::persist::load(server.name()).expect("unavailable Pane checkpoint retained");
        assert_eq!(saved.windows[0].panes[0].id, first);
        assert_eq!(saved.windows[0].panes[0].launch_args, original_args);
        assert_eq!(saved.windows[1].panes[0].id, second);
        assert!(saved.windows[1].panes[0].launch_args.is_empty());
        assert!(saved.windows[1].panes[0].agent_launch.is_none());
        assert!(saved.windows[1].panes[0].content.is_empty());
    }

    #[test]
    fn missing_executable_keeps_agent_profile_without_claiming_a_live_agent() {
        let (mut server, mut poll, dir) = recovery_fixture();
        let mut snapshot = server
            .snapshot()
            .map_content(uniterm_core::GridCapture::into_stored_lines);
        snapshot.projects[0].root = dir.to_string_lossy().into_owned();
        let id = snapshot.windows[0].panes[0].id;
        let profile = crate::persist::AgentLaunchSnap {
            provider: "codex".into(),
            session_id: Some("saved-session".into()),
            resume_command: vec!["codex".into(), "resume".into(), "saved-session".into()],
        };
        snapshot.windows[0].panes[0].agent_launch = Some(profile.clone());
        server.agent_search_path = vec![dir.to_string_lossy().into_owned()];
        server.restore(poll.registry(), snapshot).unwrap();
        assert!(server.panes[&id].agent.is_none());
        assert_eq!(server.panes[&id].pty.child_pid(), None);
        assert_eq!(
            server.snapshot().windows[0].panes[0].agent_launch,
            Some(profile)
        );
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn unlaunchable_agent_without_a_session_restores_its_shell_instead_of_waiting_for_retry() {
        let (mut server, mut poll, dir) = recovery_fixture();
        let mut snapshot = server
            .snapshot()
            .map_content(uniterm_core::GridCapture::into_stored_lines);
        snapshot.projects[0].root = dir.to_string_lossy().into_owned();
        let id = snapshot.windows[0].panes[0].id;
        snapshot.windows[0].panes[0].cwd = Some(dir.to_string_lossy().into_owned());
        snapshot.windows[0].panes[0].agent_launch = Some(crate::persist::AgentLaunchSnap {
            provider: "custom-agent".into(),
            session_id: None,
            resume_command: Vec::new(),
        });
        server.restore(poll.registry(), snapshot).unwrap();
        let pane = &server.panes[&id];
        assert!(pane.pty.child_pid().is_some());
        assert!(pane.recovery.is_none());
        assert!(pane.agent.is_none());
        assert!(!server.recovery_panes.contains(&id));
        assert!(!server.native_restore_pending(id));
        let text = pane.term.dump_text();
        assert!(text.contains("Started a shell."), "{text:?}");
        assert!(!text.contains("press Enter to retry"));
        assert!(server.snapshot().windows[0].panes[0].agent_launch.is_none());
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn exhausted_pty_allocation_preserves_checkpoint_and_cleans_up() {
        const CHILD: &str = "UNITERM_RECOVERY_FD_EXHAUSTION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let state = common::isolate_state();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "server::event_projection::tests::exhausted_pty_allocation_preserves_checkpoint_and_cleans_up", "--nocapture"])
                .env(CHILD, "1")
                .env("XDG_STATE_HOME", &state)
                .env("XDG_RUNTIME_DIR", common::socket_root())
                .status().unwrap();
            assert!(status.success());
            return;
        }
        let (mut server, mut poll, _) = recovery_fixture();
        let snapshot = server
            .snapshot()
            .map_content(uniterm_core::GridCapture::into_stored_lines);
        crate::persist::save(server.name(), &snapshot).unwrap();
        let path = crate::persist::snapshot_path(server.name());
        let before = std::fs::read(&path).unwrap();
        // Run in a separate test process so descriptor exhaustion cannot race
        // another test's runtime, state directory, or socket registration.
        let mut original = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: these are valid output/input pointers and affect only this child.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut original) },
            0
        );
        let limit = libc::rlimit {
            rlim_cur: original.rlim_cur.min(256),
            rlim_max: original.rlim_max,
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let mut files = Vec::new();
        while let Ok(file) = std::fs::File::open("/dev/null") {
            files.push(file);
        }
        let result = server.restore(poll.registry(), snapshot);
        drop(files);
        assert_eq!(
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &original) },
            0
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        server.retire_all_ptys(&mut poll);
        assert!(server.panes.is_empty());
        assert!(server.retiring_ptys.is_empty());
    }

    #[test]
    fn empty_restore_returns_error_and_preserves_the_checkpoint() {
        let (mut server, mut poll, _) = recovery_fixture();
        let snapshot = server
            .snapshot()
            .map_content(uniterm_core::GridCapture::into_stored_lines);
        crate::persist::save(server.name(), &snapshot).unwrap();
        let path = crate::persist::snapshot_path(server.name());
        let before = std::fs::read(&path).unwrap();
        let mut empty = snapshot;
        empty.windows.clear();
        assert!(server.restore(poll.registry(), empty).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn continuous_output_does_not_postpone_the_first_dirty_checkpoint() {
        let now = std::time::Instant::now();
        let first = arm_dirty_snapshot(None, now);
        assert_eq!(first, now + DIRTY_SNAPSHOT_CADENCE);
        assert_eq!(
            arm_dirty_snapshot(Some(first), now + std::time::Duration::from_secs(1)),
            first
        );
    }
}
