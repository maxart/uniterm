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
        if let Some((name, line)) = self.log.record(event) {
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
        let mut snapshot = self.snapshot();
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
                    project: project.id,
                    layout,
                    active,
                    zoomed: None,
                    name: tab.name.clone(),
                });
                project_panes.push(active);
            }
            if let Some(active) = project_panes
                .get(
                    project
                        .active_tab
                        .min(project_panes.len().saturating_sub(1)),
                )
                .copied()
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
        self.append_event(crate::eventlog::LogEvent::ProjectSelected {
            project: self.active_project.0,
        });
        if self.file_manager_visible() {
            self.sync_file_manager(false);
        }
        self.relayout();
    }

    /// Rebuild the session from a snapshot, replacing whatever `bind` set up.
    pub(super) fn restore(&mut self, reg: &Registry, snap: crate::persist::Snapshot) {
        // Tear down the default window/pane that `bind` created.
        let existing: Vec<PaneId> = self.panes.keys().copied().collect();
        self.terminate_panes(&existing);
        for (_, pane) in std::mem::take(&mut self.panes) {
            let _ = reg.deregister(&mut SourceFd(&pane.pty.raw_fd()));
            self.retire_pty(reg, pane.pty);
        }
        self.pane_tokens.clear();
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
                let saved_cwd = ps
                    .cwd
                    .as_ref()
                    .map(std::path::PathBuf::from)
                    .filter(|path| path.is_absolute());
                let cwd = saved_cwd.clone().or_else(|| project_cwd.clone());
                let resume = ps
                    .agent_launch
                    .as_ref()
                    .and_then(|profile| native_resume_args(&self.program, profile));
                // An active agent without a provider-owned resume command gets
                // a fresh shell. Replaying its old launch arguments would
                // silently create a different session under stale identity.
                let launch_args = if ps.agent_launch.is_some() {
                    resume.unwrap_or_default()
                } else {
                    ps.launch_args.clone()
                };
                let launch_arg_refs: Vec<&str> = launch_args.iter().map(String::as_str).collect();
                let mut spawned = self
                    .spawn_pane_with_id(reg, ps.id, &launch_arg_refs, cwd.as_deref())
                    .is_ok();
                if !spawned && saved_cwd.is_some() && saved_cwd.as_ref() != project_cwd.as_ref() {
                    // A nested directory may have been removed while the
                    // machine was down. Retain the Pane at its owning Project
                    // root instead of dropping it from the restored layout.
                    spawned = self
                        .spawn_pane_with_id(reg, ps.id, &launch_arg_refs, project_cwd.as_deref())
                        .is_ok();
                }
                if spawned && !ps.content.is_empty() {
                    // Replay saved content into scrollback (visible via copy-mode);
                    // the fresh shell draws its new prompt on the blank screen.
                    if let Some(pane) = self.panes.get_mut(&ps.id) {
                        pane.term.grid_mut().load_scrollback(&ps.content);
                    }
                }
                if let Some(pane) = self.panes.get_mut(&ps.id) {
                    pane.metadata = ps
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
                }
                if !launch_args.is_empty() {
                    if let Some(profile) = &ps.agent_launch {
                        self.bind_agent(ps.id, &profile.provider);
                        if let Some(agent) = self
                            .panes
                            .get_mut(&ps.id)
                            .and_then(|pane| pane.agent.as_mut())
                        {
                            agent.session_id = profile.session_id.clone();
                            agent.resume_command = profile.resume_command.clone();
                            agent.evidence = "restored through provider-owned native resume".into();
                        }
                    }
                }
            }
            // Prune panes whose process could not be restored (for example a
            // vanished cwd). A layout must never retain a dangling PaneId.
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
                    project: w.project,
                    layout,
                    active,
                    zoomed: w.zoomed.filter(|pane| self.panes.contains_key(pane)),
                    name: w.name,
                });
            }
        }
        if self.windows.is_empty() {
            // Restore produced nothing usable; fall back to a fresh pane.
            if let Ok(id) = self.spawn_pane(reg, &[]) {
                self.windows.push(Win {
                    project: self.projects.first().map(|p| p.id).unwrap_or(ProjectId(1)),
                    layout: LayoutNode::Leaf(id),
                    active: id,
                    zoomed: None,
                    name: None,
                });
            }
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
    }

    /// Restore durable Observatory filters that are independent of terminal
    /// process state and therefore apply to both clean and crash recovery.
    pub(super) fn restore_workspace_preferences(
        &mut self,
        definition: &uniterm_proto::WorkspaceDefinition,
    ) {
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
                if let Some(snapshot) = recovered.snapshot {
                    self.restore(reg, snapshot);
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
mod tests {
    use super::*;

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
