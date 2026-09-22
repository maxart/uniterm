//! Server-owned semantic worktree commands and durable projection changes.

use super::*;

const REPOSITORY_KEY: &str = "uniterm.worktree.repository";
const PATH_KEY: &str = "uniterm.worktree.path";
const BRANCH_KEY: &str = "uniterm.worktree.branch";
const CREATED_HEAD_KEY: &str = "uniterm.worktree.created_head";

pub(super) fn reserved_metadata(key: &str) -> bool {
    key.starts_with("uniterm.worktree.")
}

impl Server {
    /// Create a Git-isolated Project and launch a fresh child with the active
    /// parent's orchestration shape, goal, provider assignments, and artifact
    /// references. Live Pane identities and completion tokens are never copied.
    pub(super) fn start_run_fork(
        &mut self,
        reg: &Registry,
        requester: WorktreeRequester,
        fork: uniterm_proto::RunForkRequest,
    ) {
        let Some(parent) = self.run_graph.run(fork.parent).cloned() else {
            self.reply_worktree_result(
                reg,
                requester,
                rejected(
                    uniterm_proto::WorktreeAction::Add,
                    "parent Run does not exist",
                ),
                None,
            );
            return;
        };
        if parent.status != uniterm_core::RunStatus::Active {
            self.reply_worktree_result(
                reg,
                requester,
                rejected(
                    uniterm_proto::WorktreeAction::Add,
                    "only an active workflow or relay can be forked",
                ),
                None,
            );
            return;
        }
        let Some(repository) = self
            .projects
            .iter()
            .find(|project| project.id == parent.project)
            .map(|project| project.root.clone())
        else {
            self.reply_worktree_result(
                reg,
                requester,
                rejected(
                    uniterm_proto::WorktreeAction::Add,
                    "parent Run Project is no longer available",
                ),
                None,
            );
            return;
        };
        let artifact_refs = self
            .artifact_snapshot(None, Some(fork.parent), false)
            .into_iter()
            .map(|artifact| {
                format!(
                    "- {}: {} ({})",
                    artifact.kind.label(),
                    artifact.path,
                    artifact.digest
                )
            })
            .collect::<Vec<_>>();
        let inherited_goal = |goal: &str| {
            if artifact_refs.is_empty() {
                goal.to_string()
            } else {
                format!(
                    "{goal}\n\nInherited artifact references from Run {}:\n{}",
                    fork.parent.0,
                    artifact_refs.join("\n")
                )
            }
        };
        let plan = match parent.kind {
            uniterm_core::RunKind::Workflow => self
                .workflows
                .iter()
                .find(|run| run.task_id == parent.task_id)
                .map(|run| PendingChildLaunch::Workflow {
                    parent: fork.parent,
                    template: run.template.name.to_string(),
                    goal: inherited_goal(&run.goal),
                    role_providers: run
                        .state
                        .roles
                        .iter()
                        .zip(&run.role_providers)
                        .map(
                            |(role, provider)| uniterm_core::orchestrate::RoleProviderSelection {
                                role: role.name.clone(),
                                provider: provider.id.clone(),
                            },
                        )
                        .collect(),
                }),
            uniterm_core::RunKind::Relay => self
                .relays
                .iter()
                .find(|run| run.task_id == parent.task_id)
                .map(|run| PendingChildLaunch::Relay {
                    parent: fork.parent,
                    goal: inherited_goal(&run.goal),
                    role_providers: run
                        .state
                        .roles
                        .iter()
                        .zip(&run.role_providers)
                        .map(
                            |(role, provider)| uniterm_core::orchestrate::RoleProviderSelection {
                                role: role.name.clone(),
                                provider: provider.id.clone(),
                            },
                        )
                        .collect(),
                }),
        };
        let Some(plan) = plan else {
            self.reply_worktree_result(
                reg,
                requester,
                rejected(
                    uniterm_proto::WorktreeAction::Add,
                    "parent Run is not owned by a live orchestration",
                ),
                None,
            );
            return;
        };
        let (kind, roles) = match &plan {
            PendingChildLaunch::Workflow { role_providers, .. } => {
                (uniterm_core::RunKind::Workflow, role_providers.len())
            }
            PendingChildLaunch::Relay { role_providers, .. } => {
                (uniterm_core::RunKind::Relay, role_providers.len())
            }
        };
        if let Err(error) = self.prepare_orchestration_launch(Some(&repository), kind, roles) {
            self.reply_worktree_result(
                reg,
                requester,
                rejected(uniterm_proto::WorktreeAction::Add, error),
                None,
            );
            return;
        }

        let request = self.next_worktree_request;
        self.start_worktree_operation(
            reg,
            requester,
            uniterm_proto::WorktreeOperation::Add {
                name: fork.name,
                repository,
                path: fork.path,
                base: fork.base,
            },
        );
        if let Some(pending) = self.pending_worktrees.get_mut(&request) {
            pending.child_launch = Some(plan);
        }
    }

    pub(super) fn worktree_registration(
        project: &Project,
    ) -> Option<uniterm_proto::WorktreeRegistration> {
        Some(uniterm_proto::WorktreeRegistration {
            project: project.id,
            project_name: project.name.clone(),
            repository: project.metadata.get(REPOSITORY_KEY)?.clone(),
            path: project.metadata.get(PATH_KEY)?.clone(),
            branch: project.metadata.get(BRANCH_KEY)?.clone(),
            created_head: project.metadata.get(CREATED_HEAD_KEY)?.clone(),
        })
    }

    pub(super) fn worktree_metadata(
        registration: &uniterm_proto::WorktreeRegistration,
    ) -> HashMap<String, String> {
        HashMap::from([
            (REPOSITORY_KEY.into(), registration.repository.clone()),
            (PATH_KEY.into(), registration.path.clone()),
            (BRANCH_KEY.into(), registration.branch.clone()),
            (CREATED_HEAD_KEY.into(), registration.created_head.clone()),
        ])
    }

    pub(super) fn start_worktree_operation(
        &mut self,
        reg: &Registry,
        requester: WorktreeRequester,
        operation: uniterm_proto::WorktreeOperation,
    ) {
        use uniterm_proto::{WorktreeAction, WorktreeOperation, WorktreeRuntimeOperation};

        let runtime = match &operation {
            WorktreeOperation::List => WorktreeRuntimeOperation::Inspect {
                action: WorktreeAction::List,
                registrations: self
                    .projects
                    .iter()
                    .filter_map(Self::worktree_registration)
                    .collect(),
                force: false,
            },
            WorktreeOperation::Add {
                name,
                repository,
                path,
                base,
            } => {
                let name = name.trim();
                if name.is_empty() || name.len() > 256 {
                    self.reply_worktree_result(
                        reg,
                        requester,
                        rejected(WorktreeAction::Add, "Project name is invalid"),
                        None,
                    );
                    return;
                }
                if self
                    .projects
                    .iter()
                    .any(|project| project.name.eq_ignore_ascii_case(name))
                {
                    self.reply_worktree_result(
                        reg,
                        requester,
                        rejected(WorktreeAction::Add, "Project name already exists"),
                        None,
                    );
                    return;
                }
                let project = ProjectId(self.next_project_id);
                self.next_project_id = self.next_project_id.saturating_add(1);
                let branch = crate::worktree::branch_name(name, project);
                self.append_event(crate::eventlog::LogEvent::WorktreeCreateRequested {
                    project: project.0,
                    name: name.to_string(),
                    repository: repository.clone(),
                    branch: branch.clone(),
                    path: path.clone(),
                    base: base.clone(),
                });
                WorktreeRuntimeOperation::Add {
                    registration: uniterm_proto::WorktreeRegistration {
                        project,
                        project_name: name.to_string(),
                        repository: repository.clone(),
                        path: path.clone(),
                        branch,
                        created_head: String::new(),
                    },
                    base: base.clone(),
                }
            }
            WorktreeOperation::Open { project }
            | WorktreeOperation::Remove { project, .. }
            | WorktreeOperation::Cleanup { project } => {
                let action = match operation {
                    WorktreeOperation::Open { .. } => WorktreeAction::Open,
                    WorktreeOperation::Remove { .. } => WorktreeAction::Remove,
                    WorktreeOperation::Cleanup { .. } => WorktreeAction::Cleanup,
                    _ => unreachable!(),
                };
                if matches!(action, WorktreeAction::Remove | WorktreeAction::Cleanup)
                    && self.projects.len() <= 1
                {
                    self.reply_worktree_result(
                        reg,
                        requester,
                        rejected(action, "the final Project cannot be removed"),
                        None,
                    );
                    return;
                }
                let registration = self
                    .projects
                    .iter()
                    .find(|item| item.id == *project)
                    .and_then(Self::worktree_registration);
                let Some(registration) = registration else {
                    self.reply_worktree_result(
                        reg,
                        requester,
                        rejected(action, "Project is not a registered Uniterm worktree"),
                        None,
                    );
                    return;
                };
                match &operation {
                    WorktreeOperation::Remove { force, .. } => {
                        self.append_event(crate::eventlog::LogEvent::WorktreeRemoveRequested {
                            project: registration.project.0,
                            repository: registration.repository.clone(),
                            branch: registration.branch.clone(),
                            path: registration.path.clone(),
                            forced: *force,
                        });
                    }
                    WorktreeOperation::Cleanup { .. } => {
                        self.append_event(crate::eventlog::LogEvent::WorktreeCleanupRequested {
                            project: registration.project.0,
                            repository: registration.repository.clone(),
                            branch: registration.branch.clone(),
                            path: registration.path.clone(),
                        });
                    }
                    _ => {}
                }
                WorktreeRuntimeOperation::Inspect {
                    action,
                    registrations: vec![registration],
                    force: matches!(operation, WorktreeOperation::Remove { force: true, .. }),
                }
            }
        };

        let request = self.next_worktree_request;
        self.next_worktree_request = self.next_worktree_request.saturating_add(1);
        self.pending_worktrees.insert(
            request,
            PendingWorktree {
                requester,
                operation,
                rollback_error: None,
                child_launch: None,
            },
        );
        self.agents.send(uniterm_proto::CoreToAgent::WorktreeRun {
            request,
            workspace: self.name.clone(),
            operation: runtime,
        });
    }

    pub(super) fn finish_worktree_operation(
        &mut self,
        reg: &Registry,
        request: u64,
        mut result: uniterm_proto::WorktreeResult,
    ) {
        use uniterm_proto::WorktreeOperation;

        let Some(pending) = self.pending_worktrees.remove(&request) else {
            return;
        };
        if let Some(original) = pending.rollback_error {
            let error = if result.accepted {
                format!("{original}; Git creation was rolled back")
            } else {
                format!(
                    "{original}; automatic Git rollback failed: {}",
                    result.error.as_deref().unwrap_or("unknown rollback error")
                )
            };
            self.reply_worktree_result(
                reg,
                pending.requester,
                rejected(uniterm_proto::WorktreeAction::Add, error),
                None,
            );
            return;
        }
        let mut child = None;
        match &pending.operation {
            WorktreeOperation::Add { .. } => {
                let registration_error = if let Some(plan) = pending.child_launch {
                    match self.finish_worktree_child(reg, &mut result, plan) {
                        Ok(run) => {
                            child = run;
                            None
                        }
                        Err(failure) => Some(*failure),
                    }
                } else {
                    self.finish_worktree_add(reg, &mut result)
                };
                if let Some((registration, error)) = registration_error {
                    let rollback = self.next_worktree_request;
                    self.next_worktree_request = self.next_worktree_request.saturating_add(1);
                    self.pending_worktrees.insert(
                        rollback,
                        PendingWorktree {
                            requester: pending.requester,
                            operation: pending.operation,
                            rollback_error: Some(error),
                            child_launch: None,
                        },
                    );
                    self.agents.send(uniterm_proto::CoreToAgent::WorktreeRun {
                        request: rollback,
                        workspace: self.name.clone(),
                        operation: uniterm_proto::WorktreeRuntimeOperation::RollbackAdd {
                            registration,
                        },
                    });
                    return;
                }
            }
            WorktreeOperation::Open { project } if result.accepted => {
                self.switch_project(reg, *project);
            }
            WorktreeOperation::Remove { project, force } if result.accepted => {
                if let Some(item) = result.items.first() {
                    self.append_event(crate::eventlog::LogEvent::WorktreeRemoved {
                        project: project.0,
                        repository: item.registration.repository.clone(),
                        branch: item.registration.branch.clone(),
                        path: item.registration.path.clone(),
                        forced: *force,
                    });
                    self.remove_project(reg, *project);
                } else {
                    result.accepted = false;
                    result.error = Some("Git returned no removed worktree".into());
                }
            }
            WorktreeOperation::Cleanup { project } if result.accepted => {
                if let Some(item) = result.items.first() {
                    self.append_event(crate::eventlog::LogEvent::WorktreeCleaned {
                        project: project.0,
                        repository: item.registration.repository.clone(),
                        branch: item.registration.branch.clone(),
                        path: item.registration.path.clone(),
                    });
                    self.remove_project(reg, *project);
                } else {
                    result.accepted = false;
                    result.error = Some("Git returned no cleaned worktree".into());
                }
            }
            _ => {}
        }
        self.reply_worktree_result(reg, pending.requester, result, child);
    }

    fn finish_worktree_child(
        &mut self,
        reg: &Registry,
        result: &mut uniterm_proto::WorktreeResult,
        plan: PendingChildLaunch,
    ) -> Result<Option<uniterm_core::RunId>, Box<(uniterm_proto::WorktreeRegistration, String)>>
    {
        let Some(item) = result.items.first().cloned().filter(|_| result.accepted) else {
            let pending = result.items.first().map(|item| &item.registration);
            self.append_event(crate::eventlog::LogEvent::WorktreeCreateResult {
                project: pending.map_or(self.next_project_id, |item| item.project.0),
                repository: pending.map_or_else(String::new, |item| item.repository.clone()),
                branch: pending.map_or_else(String::new, |item| item.branch.clone()),
                path: pending.map_or_else(String::new, |item| item.path.clone()),
                head: pending.map_or_else(String::new, |item| item.created_head.clone()),
                accepted: false,
                error: result.error.clone(),
            });
            return Ok(None);
        };
        let registration = item.registration;
        if self
            .projects
            .iter()
            .any(|project| project.id == registration.project)
        {
            let error = "stale worktree Project allocation".to_string();
            result.accepted = false;
            result.error = Some(error.clone());
            self.record_failed_worktree_add(&registration, &error);
            return Err(Box::new((registration, error)));
        }

        self.append_event(crate::eventlog::LogEvent::WorktreeCreateResult {
            project: registration.project.0,
            repository: registration.repository.clone(),
            branch: registration.branch.clone(),
            path: registration.path.clone(),
            head: registration.created_head.clone(),
            accepted: true,
            error: None,
        });
        self.append_event(crate::eventlog::LogEvent::ProjectCreated {
            project: registration.project.0,
            name: registration.project_name.clone(),
            root: registration.path.clone(),
        });
        let metadata = Self::worktree_metadata(&registration);
        for (key, value) in &metadata {
            self.append_event(crate::eventlog::LogEvent::ProjectMetadataSet {
                project: registration.project.0,
                key: key.clone(),
                value: value.clone(),
            });
        }
        self.projects.push(Project {
            id: registration.project,
            name: registration.project_name.clone(),
            root: registration.path.clone(),
            active_pane: None,
            metadata,
        });

        let launch = match plan {
            PendingChildLaunch::Workflow {
                parent,
                template,
                goal,
                role_providers,
            } => self.launch_workflow_with_parent(
                reg,
                &template,
                None,
                &role_providers,
                &goal,
                super::orchestration::OrchestrationTarget {
                    project: Some(&registration.path),
                    parent: Some(parent),
                },
            ),
            PendingChildLaunch::Relay {
                parent,
                goal,
                role_providers,
            } => self.launch_relay_with_parent(
                reg,
                None,
                &role_providers,
                &goal,
                super::orchestration::OrchestrationTarget {
                    project: Some(&registration.path),
                    parent: Some(parent),
                },
            ),
        };
        match launch {
            Ok(run) => Ok(Some(run)),
            Err(error) => {
                self.append_event(crate::eventlog::LogEvent::ProjectRemoved {
                    project: registration.project.0,
                });
                self.projects
                    .retain(|project| project.id != registration.project);
                result.accepted = false;
                result.error = Some(format!(
                    "Git created {}, but child Run launch was refused: {error}",
                    registration.path
                ));
                Err(Box::new((
                    registration,
                    result.error.clone().unwrap_or_default(),
                )))
            }
        }
    }

    fn finish_worktree_add(
        &mut self,
        reg: &Registry,
        result: &mut uniterm_proto::WorktreeResult,
    ) -> Option<(uniterm_proto::WorktreeRegistration, String)> {
        let Some(item) = result.items.first().cloned().filter(|_| result.accepted) else {
            let pending = result.items.first().map(|item| &item.registration);
            self.append_event(crate::eventlog::LogEvent::WorktreeCreateResult {
                project: pending.map_or(self.next_project_id, |item| item.project.0),
                repository: pending.map_or_else(String::new, |item| item.repository.clone()),
                branch: pending.map_or_else(String::new, |item| item.branch.clone()),
                path: pending.map_or_else(String::new, |item| item.path.clone()),
                head: pending.map_or_else(String::new, |item| item.created_head.clone()),
                accepted: false,
                error: result.error.clone(),
            });
            return None;
        };
        let registration = item.registration;
        if self
            .projects
            .iter()
            .any(|project| project.id == registration.project)
        {
            result.accepted = false;
            result.error = Some("stale worktree Project allocation".into());
            let error = result.error.clone().unwrap_or_default();
            self.record_failed_worktree_add(&registration, &error);
            return Some((registration, error));
        }
        let pane = match self.spawn_pane_at(reg, &[], Some(Path::new(&registration.path))) {
            Ok(pane) => pane,
            Err(error) => {
                result.accepted = false;
                result.error = Some(format!(
                    "Git created {}, but Uniterm could not open its Project: {error}",
                    registration.path
                ));
                let error = result.error.clone().unwrap_or_default();
                self.record_failed_worktree_add(&registration, &error);
                return Some((registration, error));
            }
        };

        self.append_event(crate::eventlog::LogEvent::WorktreeCreateResult {
            project: registration.project.0,
            repository: registration.repository.clone(),
            branch: registration.branch.clone(),
            path: registration.path.clone(),
            head: registration.created_head.clone(),
            accepted: true,
            error: None,
        });
        self.append_event(crate::eventlog::LogEvent::ProjectCreated {
            project: registration.project.0,
            name: registration.project_name.clone(),
            root: registration.path.clone(),
        });
        let metadata = Self::worktree_metadata(&registration);
        for (key, value) in &metadata {
            self.append_event(crate::eventlog::LogEvent::ProjectMetadataSet {
                project: registration.project.0,
                key: key.clone(),
                value: value.clone(),
            });
        }
        self.projects.push(Project {
            id: registration.project,
            name: registration.project_name,
            root: registration.path,
            active_pane: Some(pane),
            metadata,
        });
        self.windows.push(Win {
            project: registration.project,
            layout: LayoutNode::Leaf(pane),
            active: pane,
            zoomed: None,
            name: None,
        });
        self.activate_window(self.windows.len() - 1);
        self.relayout();
        self.full_repaint_all(reg);
        self.persist();
        None
    }

    fn record_failed_worktree_add(
        &mut self,
        registration: &uniterm_proto::WorktreeRegistration,
        error: &str,
    ) {
        self.append_event(crate::eventlog::LogEvent::WorktreeCreateResult {
            project: registration.project.0,
            repository: registration.repository.clone(),
            branch: registration.branch.clone(),
            path: registration.path.clone(),
            head: registration.created_head.clone(),
            accepted: false,
            error: Some(error.to_string()),
        });
    }

    fn reply_worktree_result(
        &mut self,
        reg: &Registry,
        requester: WorktreeRequester,
        result: uniterm_proto::WorktreeResult,
        child: Option<uniterm_core::RunId>,
    ) {
        match requester {
            WorktreeRequester::Client(token) => {
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::Worktrees(result)));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            WorktreeRequester::ClientWorkspace(token) => self.reply_workspace(reg, token),
            WorktreeRequester::Control { connection, id } => {
                self.agents
                    .send(uniterm_proto::CoreToAgent::ControlResponse {
                        connection,
                        response: uniterm_proto::ControlResponse::ok(
                            id,
                            uniterm_proto::ControlResult::Worktrees(result),
                        ),
                    });
            }
            WorktreeRequester::RunForkClient { token, parent } => {
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::RunForked(
                        uniterm_proto::RunForkResult {
                            parent,
                            child,
                            worktree: result,
                        },
                    )));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
            WorktreeRequester::RunForkControl {
                connection,
                id,
                parent,
            } => self
                .agents
                .send(uniterm_proto::CoreToAgent::ControlResponse {
                    connection,
                    response: uniterm_proto::ControlResponse::ok(
                        id,
                        uniterm_proto::ControlResult::RunForked(uniterm_proto::RunForkResult {
                            parent,
                            child,
                            worktree: result,
                        }),
                    ),
                }),
        }
    }
}

fn rejected(
    action: uniterm_proto::WorktreeAction,
    error: impl Into<String>,
) -> uniterm_proto::WorktreeResult {
    uniterm_proto::WorktreeResult {
        action,
        accepted: false,
        error: Some(error.into()),
        items: Vec::new(),
    }
}
