//! Neutral control API dispatch on the mio-owned semantic command path.

use super::*;

impl Server {
    pub(super) fn on_control_request(
        &mut self,
        reg: &Registry,
        connection: u64,
        request: uniterm_proto::ControlRequest,
    ) {
        use uniterm_proto::{ControlCommand, ControlResponse, ControlResult};

        if request.version == uniterm_proto::CONTROL_API_VERSION && request.workspace == self.name {
            let operation = match &request.command {
                ControlCommand::WorktreeList => Some(uniterm_proto::WorktreeOperation::List),
                ControlCommand::WorktreeAdd {
                    name,
                    repository,
                    path,
                    base,
                } => Some(uniterm_proto::WorktreeOperation::Add {
                    name: name.clone(),
                    repository: repository.clone(),
                    path: path.clone(),
                    base: base.clone(),
                }),
                ControlCommand::WorktreeOpen { project } => {
                    Some(uniterm_proto::WorktreeOperation::Open { project: *project })
                }
                ControlCommand::WorktreeRemove { project, force } => {
                    Some(uniterm_proto::WorktreeOperation::Remove {
                        project: *project,
                        force: *force,
                    })
                }
                ControlCommand::WorktreeCleanup { project } => {
                    Some(uniterm_proto::WorktreeOperation::Cleanup { project: *project })
                }
                _ => None,
            };
            if let Some(operation) = operation {
                self.start_worktree_operation(
                    reg,
                    WorktreeRequester::Control {
                        connection,
                        id: request.id,
                    },
                    operation,
                );
                return;
            }
            if let ControlCommand::RunFork { fork } = &request.command {
                self.start_run_fork(
                    reg,
                    WorktreeRequester::RunForkControl {
                        connection,
                        id: request.id,
                        parent: fork.parent,
                    },
                    fork.clone(),
                );
                return;
            }
        }

        let response = if request.version != uniterm_proto::CONTROL_API_VERSION {
            ControlResponse::error(
                request.id,
                "unsupported_version",
                "unsupported control API version",
            )
        } else if request.workspace != self.name {
            ControlResponse::error(
                request.id,
                "workspace_mismatch",
                "request does not own this Workspace",
            )
        } else {
            match request.command {
                ControlCommand::Capabilities => ControlResponse::ok(
                    request.id,
                    ControlResult::Capabilities {
                        protocol_version: uniterm_proto::CONTROL_API_VERSION,
                        capabilities: vec![
                            "workspace.snapshot".into(),
                            "pane.list".into(),
                            "pane.read".into(),
                            "pane.send".into(),
                            "pane.attach.binary.v6".into(),
                            "instruction.list".into(),
                            "instruction.add".into(),
                            "instruction.replace".into(),
                            "instruction.cancel".into(),
                            "instruction.send_now".into(),
                            "worktree.list".into(),
                            "worktree.add".into(),
                            "worktree.open".into(),
                            "worktree.remove".into(),
                            "worktree.cleanup".into(),
                            "run.list".into(),
                            "artifact.list".into(),
                            "orchestration.start".into(),
                            "run.fork".into(),
                            "project.create".into(),
                            "project.rename".into(),
                            "project.move".into(),
                            "project.switch".into(),
                            "project.remove".into(),
                            "tab.create".into(),
                            "tab.rename".into(),
                            "tab.move".into(),
                            "hierarchy.focus".into(),
                            "agent.list".into(),
                            "agent.launch".into(),
                            "agent.focus".into(),
                            "agent.stop".into(),
                            "agent.stop_all".into(),
                            "task.list".into(),
                            "task.create".into(),
                            "task.set_status".into(),
                            "task.retitle".into(),
                            "task.delete".into(),
                            "waiting.list".into(),
                            "waiting.act".into(),
                            "orchestration.submit".into(),
                            "events.subscribe".into(),
                        ],
                        max_frame_bytes: uniterm_proto::CONTROL_MAX_FRAME_BYTES,
                        max_connections: uniterm_proto::CONTROL_MAX_CONNECTIONS,
                        max_queued_frames: uniterm_proto::CONTROL_MAX_QUEUED_FRAMES,
                        max_queued_requests: uniterm_proto::CONTROL_MAX_QUEUED_REQUESTS,
                    },
                ),
                ControlCommand::WorkspaceSnapshot => ControlResponse::ok(
                    request.id,
                    ControlResult::Workspace {
                        name: self.name.clone(),
                        sequence: self.log.current_sequence(),
                        active_project: self.active_project,
                        projects: self.workspace_snapshot(),
                    },
                ),
                ControlCommand::ProjectCreate { name, root } => {
                    let accepted = self.create_project(reg, &name, &root).is_ok();
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "project".into(),
                            id: accepted
                                .then(|| self.projects.last().map(|item| item.id.0))
                                .flatten(),
                            found: true,
                            accepted,
                        },
                    )
                }
                ControlCommand::ProjectRename { project, name } => {
                    let found = self.projects.iter().any(|item| item.id == project);
                    let accepted = found && !name.trim().is_empty();
                    if accepted {
                        self.rename_project(reg, project, &name);
                    }
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "project".into(),
                            id: Some(project.0),
                            found,
                            accepted,
                        },
                    )
                }
                ControlCommand::ProjectMove { project, direction } => {
                    let before: Vec<_> = self.projects.iter().map(|item| item.id).collect();
                    let found = before.contains(&project);
                    self.move_project(reg, project, direction);
                    let after: Vec<_> = self.projects.iter().map(|item| item.id).collect();
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "project".into(),
                            id: Some(project.0),
                            found,
                            accepted: found && before != after,
                        },
                    )
                }
                ControlCommand::ProjectSwitch { project } => {
                    let found = self.projects.iter().any(|item| item.id == project);
                    self.switch_project(reg, project);
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "project".into(),
                            id: Some(project.0),
                            found,
                            accepted: found && self.active_project == project,
                        },
                    )
                }
                ControlCommand::ProjectRemove { project, confirmed }
                    if !self.guard_semantic(
                        uniterm_core::GuardedCommand::ProjectRemove,
                        confirmed,
                        Some(project),
                    ) =>
                {
                    ControlResponse::error(
                        request.id,
                        "confirmation_required",
                        "project_remove closes every Pane the Project owns; pass \"confirmed\": true",
                    )
                }
                ControlCommand::ProjectRemove { project, .. } => {
                    let found = self.projects.iter().any(|item| item.id == project);
                    let worktree = self
                        .projects
                        .iter()
                        .find(|item| item.id == project)
                        .and_then(Self::worktree_registration)
                        .is_some();
                    let allowed = found && !worktree && self.projects.len() > 1;
                    if allowed {
                        self.remove_project(reg, project);
                    }
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "project".into(),
                            id: Some(project.0),
                            found,
                            accepted: allowed
                                && !self.projects.iter().any(|item| item.id == project),
                        },
                    )
                }
                ControlCommand::TabCreate { project } => {
                    let found = self.projects.iter().any(|item| item.id == project);
                    let before = self.project_window_indices(project).len();
                    if found {
                        self.switch_project(reg, project);
                        self.handle_command(reg, uniterm_proto::Command::NewWindow);
                    }
                    let accepted = self.project_window_indices(project).len() > before;
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "tab".into(),
                            id: accepted.then_some((before + 1) as u64),
                            found,
                            accepted,
                        },
                    )
                }
                ControlCommand::TabRename { project, tab, name } => {
                    let focused = self.focus_hierarchy_target(reg, project, tab, None);
                    let accepted = focused.is_some() && !name.trim().is_empty();
                    if accepted {
                        let trimmed = name.trim();
                        let window = self.active_window;
                        self.windows[window].name = Some(trimmed.to_string());
                        self.append_event(crate::eventlog::LogEvent::WindowRenamed {
                            window: window as u64,
                            name: trimmed.to_string(),
                        });
                        self.full_repaint_all(reg);
                        self.persist();
                    }
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "tab".into(),
                            id: Some(u64::from(tab)),
                            found: focused.is_some(),
                            accepted,
                        },
                    )
                }
                ControlCommand::TabMove {
                    project,
                    tab,
                    direction,
                } => {
                    let focused = self.focus_hierarchy_target(reg, project, tab, None);
                    let accepted = focused.is_some() && self.move_active_tab(direction);
                    if accepted {
                        self.relayout();
                        self.full_repaint_all(reg);
                    }
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "tab".into(),
                            id: Some(u64::from(tab)),
                            found: focused.is_some(),
                            accepted,
                        },
                    )
                }
                ControlCommand::HierarchyFocus { project, tab, pane } => {
                    let focused = self.focus_hierarchy_target(reg, project, tab, pane);
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "pane".into(),
                            id: focused.map(|pane| pane.0),
                            found: focused.is_some(),
                            accepted: focused.is_some(),
                        },
                    )
                }
                ControlCommand::PaneList => ControlResponse::ok(
                    request.id,
                    ControlResult::Panes {
                        workspace: self.name.clone(),
                        panes: self.pane_snapshot(),
                    },
                ),
                ControlCommand::AgentList => ControlResponse::ok(
                    request.id,
                    ControlResult::Fleet {
                        entries: self.fleet_snapshot(),
                    },
                ),
                ControlCommand::AgentLaunch { agent, target } => {
                    let pane = self.launch_agent(reg, &agent, target);
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "agent".into(),
                            id: pane.map(|pane| pane.0),
                            found: crate::workflow::resolve_agent_on_search_path(
                                Some(&agent),
                                &self.agent_search_path,
                            )
                            .is_some(),
                            accepted: pane.is_some(),
                        },
                    )
                }
                ControlCommand::AgentFocus { pane } => {
                    let found = self
                        .panes
                        .get(&pane)
                        .is_some_and(|pane| pane.agent.is_some());
                    let accepted = found && self.focus_pane_target(reg, pane);
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "agent".into(),
                            id: Some(pane.0),
                            found,
                            accepted,
                        },
                    )
                }
                ControlCommand::AgentStop { pane } => {
                    let found = self
                        .panes
                        .get(&pane)
                        .is_some_and(|pane| pane.agent.is_some());
                    if found {
                        self.close_pane(reg, pane);
                    }
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "agent".into(),
                            id: Some(pane.0),
                            found,
                            accepted: found && !self.panes.contains_key(&pane),
                        },
                    )
                }
                ControlCommand::AgentStopAll { scope, confirmed }
                    if !self.guard_semantic(
                        uniterm_core::GuardedCommand::AgentsStopAll,
                        confirmed,
                        match scope {
                            uniterm_proto::StopScope::Project(project) => Some(project),
                            _ => None,
                        },
                    ) =>
                {
                    ControlResponse::error(
                        request.id,
                        "confirmation_required",
                        "agent_stop_all closes every agent Pane in scope; pass \"confirmed\": true",
                    )
                }
                ControlCommand::AgentStopAll { scope, .. } => {
                    let before = self.fleet_snapshot().len();
                    self.stop_all_agents(reg, scope);
                    let after = self.fleet_snapshot().len();
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "agent".into(),
                            id: None,
                            found: before > 0,
                            accepted: after < before,
                        },
                    )
                }
                ControlCommand::TaskList => ControlResponse::ok(
                    request.id,
                    ControlResult::Tasks {
                        items: self.task_snapshot(),
                    },
                ),
                ControlCommand::TaskCreate { title } => {
                    let title = title.trim();
                    let id = (!title.is_empty())
                        .then(|| self.create_task(title, uniterm_core::TaskStatus::Todo));
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "task".into(),
                            id,
                            found: true,
                            accepted: id.is_some(),
                        },
                    )
                }
                ControlCommand::TaskSetStatus { id, status } => {
                    let found = self.tasks.get(id).is_some();
                    let accepted = self.tasks.set_status(id, status);
                    if accepted {
                        self.append_event(crate::eventlog::LogEvent::TaskStatusChanged {
                            id,
                            status,
                        });
                    }
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "task".into(),
                            id: Some(id),
                            found,
                            accepted,
                        },
                    )
                }
                ControlCommand::TaskRetitle { id, title } => {
                    let found = self.tasks.get(id).is_some();
                    let title = title.trim();
                    let accepted = !title.is_empty() && self.tasks.set_title(id, title);
                    if accepted {
                        self.append_event(crate::eventlog::LogEvent::TaskRetitled {
                            id,
                            title: title.to_string(),
                        });
                    }
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "task".into(),
                            id: Some(id),
                            found,
                            accepted,
                        },
                    )
                }
                ControlCommand::TaskDelete { id } => {
                    let found = self.tasks.get(id).is_some();
                    let accepted = self.tasks.remove(id);
                    if accepted {
                        self.append_event(crate::eventlog::LogEvent::TaskDeleted { id });
                    }
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "task".into(),
                            id: Some(id),
                            found,
                            accepted,
                        },
                    )
                }
                ControlCommand::WaitingList => ControlResponse::ok(
                    request.id,
                    ControlResult::Waiting {
                        items: self.waiting_snapshot(),
                    },
                ),
                ControlCommand::WaitingAct { id, action, text } => {
                    let (found, accepted) = self.waiting_action(reg, id, action, &text);
                    ControlResponse::ok(
                        request.id,
                        ControlResult::Mutation {
                            resource: "waiting".into(),
                            id: Some(id),
                            found,
                            accepted,
                        },
                    )
                }
                ControlCommand::PaneRead { pane, lines } => {
                    let (found, text, truncated) = self
                        .bounded_pane_output(pane, lines)
                        .map_or((false, String::new(), false), |(text, truncated)| {
                            (true, text, truncated)
                        });
                    ControlResponse::ok(
                        request.id,
                        ControlResult::PaneOutput {
                            pane,
                            found,
                            text,
                            truncated,
                        },
                    )
                }
                ControlCommand::PaneSend { pane, text } => {
                    let (found, accepted) = if let Some(target) = self.panes.get_mut(&pane) {
                        (true, Self::queue_pane_input(reg, target, text.as_bytes()))
                    } else {
                        (false, false)
                    };
                    ControlResponse::ok(
                        request.id,
                        ControlResult::PaneSent {
                            pane,
                            found,
                            accepted,
                        },
                    )
                }
                ControlCommand::Subscribe { after_sequence } => {
                    let current_sequence = self.log.current_sequence();
                    if after_sequence > current_sequence {
                        ControlResponse::error(
                            request.id,
                            "invalid_cursor",
                            "after_sequence is newer than the Workspace event cursor",
                        )
                    } else {
                        ControlResponse::ok(
                            request.id,
                            ControlResult::Subscribed {
                                subscription: connection,
                                current_sequence,
                            },
                        )
                    }
                }
                ControlCommand::InstructionList => ControlResponse::ok(
                    request.id,
                    ControlResult::Instructions {
                        workspace: self.name.clone(),
                        items: self.instruction_snapshot(),
                    },
                ),
                ControlCommand::InstructionAdd { pane, text } => {
                    let result = self.instruction_command(
                        reg,
                        super::instruction::InstructionCommand::Add {
                            pane,
                            author: uniterm_core::InstructionAuthor::ControlApi,
                            text,
                        },
                    );
                    ControlResponse::ok(
                        request.id,
                        ControlResult::InstructionChanged {
                            id: result.id,
                            found: result.found,
                            accepted: result.accepted,
                            items: self.instruction_snapshot(),
                        },
                    )
                }
                ControlCommand::InstructionReplace { id, text } => {
                    let result = self.instruction_command(
                        reg,
                        super::instruction::InstructionCommand::Replace {
                            id,
                            author: uniterm_core::InstructionAuthor::ControlApi,
                            text,
                        },
                    );
                    ControlResponse::ok(
                        request.id,
                        ControlResult::InstructionChanged {
                            id: result.id,
                            found: result.found,
                            accepted: result.accepted,
                            items: self.instruction_snapshot(),
                        },
                    )
                }
                ControlCommand::InstructionCancel { id } => {
                    let result = self.instruction_command(
                        reg,
                        super::instruction::InstructionCommand::Cancel { id },
                    );
                    ControlResponse::ok(
                        request.id,
                        ControlResult::InstructionChanged {
                            id: result.id,
                            found: result.found,
                            accepted: result.accepted,
                            items: self.instruction_snapshot(),
                        },
                    )
                }
                ControlCommand::InstructionSendNow { id } => {
                    let result = self.instruction_command(
                        reg,
                        super::instruction::InstructionCommand::SendNow { id },
                    );
                    ControlResponse::ok(
                        request.id,
                        ControlResult::InstructionChanged {
                            id: result.id,
                            found: result.found,
                            accepted: result.accepted,
                            items: self.instruction_snapshot(),
                        },
                    )
                }
                ControlCommand::RunList {
                    project,
                    active_only,
                } => ControlResponse::ok(
                    request.id,
                    ControlResult::Runs {
                        workspace: self.name.clone(),
                        runs: self.run_snapshot(project, active_only),
                    },
                ),
                ControlCommand::ArtifactList {
                    project,
                    run,
                    include_superseded,
                } => ControlResponse::ok(
                    request.id,
                    ControlResult::Artifacts {
                        workspace: self.name.clone(),
                        artifacts: self.artifact_snapshot(project, run, include_superseded),
                    },
                ),
                ControlCommand::OrchestrationStart { launch } => {
                    let result = match launch.kind {
                        uniterm_proto::OrchestrationKind::Workflow => {
                            match launch.template.as_deref() {
                                Some(template) => self.launch_workflow(
                                    reg,
                                    template,
                                    launch.provider.as_deref(),
                                    &launch.role_providers,
                                    &launch.goal,
                                    launch.project.as_deref(),
                                ),
                                None => Err("workflow launch requires a template".into()),
                            }
                        }
                        uniterm_proto::OrchestrationKind::Relay => {
                            if launch.template.is_some() {
                                Err("relay launch does not accept a workflow template".into())
                            } else {
                                self.launch_relay(
                                    reg,
                                    launch.provider.as_deref(),
                                    &launch.role_providers,
                                    &launch.goal,
                                    launch.project.as_deref(),
                                )
                            }
                        }
                    };
                    match result {
                        Ok(run) => ControlResponse::ok(
                            request.id,
                            ControlResult::OrchestrationStarted { run },
                        ),
                        Err(error) => ControlResponse::error(
                            request.id,
                            "invalid_orchestration_launch",
                            error,
                        ),
                    }
                }
                ControlCommand::OrchestrationSubmit {
                    kind,
                    token,
                    status,
                    verdict,
                    summary,
                    artifacts,
                } => {
                    let accepted = match kind {
                        uniterm_proto::OrchestrationKind::Workflow => {
                            self.workflows.iter().any(|run| {
                                run.state.token == token
                                    && matches!(
                                        run.state.phase,
                                        uniterm_core::orchestrate::Phase::Awaiting
                                    )
                            })
                        }
                        uniterm_proto::OrchestrationKind::Relay => self.relays.iter().any(|run| {
                            run.state.token == token
                                && matches!(
                                    run.state.phase,
                                    uniterm_core::orchestrate::Phase::Awaiting
                                )
                        }),
                    };
                    if accepted {
                        match kind {
                            uniterm_proto::OrchestrationKind::Workflow => self.on_workflow_submit(
                                reg, token, status, verdict, summary, artifacts, false,
                            ),
                            uniterm_proto::OrchestrationKind::Relay => {
                                self.on_relay_submit(reg, token, status, summary, artifacts, false)
                            }
                        }
                    }
                    ControlResponse::ok(
                        request.id,
                        ControlResult::OrchestrationSubmitted {
                            kind,
                            token,
                            accepted,
                        },
                    )
                }
                ControlCommand::WorktreeList
                | ControlCommand::WorktreeAdd { .. }
                | ControlCommand::WorktreeOpen { .. }
                | ControlCommand::WorktreeRemove { .. }
                | ControlCommand::WorktreeCleanup { .. }
                | ControlCommand::RunFork { .. } => {
                    unreachable!("worktree requests return through the runtime")
                }
            }
        };
        self.agents
            .send(uniterm_proto::CoreToAgent::ControlResponse {
                connection,
                response,
            });
    }
}
