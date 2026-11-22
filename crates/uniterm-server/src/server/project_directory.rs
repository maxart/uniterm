//! One host-validated, event-backed directory command for every frontend.

use super::*;

/// Retain only the reply address while the runtime validates a directory.
pub(super) enum DirectoryRequester {
    Client(Token),
    Control { connection: u64, id: u64 },
}

impl Server {
    fn directory_change_allowed(&self, project: ProjectId) -> Result<usize, String> {
        if !self.event_writes_enabled || self.durability_error.is_some() {
            return Err("Cannot change directory while the event log is unavailable".into());
        }
        let index = self
            .projects
            .iter()
            .position(|item| item.id == project)
            .ok_or("Project no longer exists")?;
        let owner = &self.projects[index];
        if Self::worktree_registration(owner).is_some() {
            return Err("Registered worktree directories are managed by Git; add or open a worktree Project instead".into());
        }
        if self.run_graph.runs_for_project(project).iter().any(|id| {
            self.run_graph
                .run(*id)
                .is_some_and(|run| run.status == uniterm_core::RunStatus::Active)
        }) {
            return Err(
                "Finish or stop this Project's workflow or relay before changing its directory"
                    .into(),
            );
        }
        Ok(index)
    }

    /// Bound concurrent validation to one request per live Project.
    pub(super) fn start_project_directory(
        &mut self,
        reg: &Registry,
        requester: DirectoryRequester,
        project: ProjectId,
        root: String,
    ) {
        let result = self.directory_change_allowed(project).and_then(|_| {
            if self.pending_project_directories.contains_key(&project) {
                Err("A directory change is already pending for this Project".into())
            } else if root.trim().is_empty() || root.len() > 4096 || root.contains('\0') {
                Err("Enter a directory path of 1 to 4096 bytes without NUL characters".into())
            } else {
                Ok(())
            }
        });
        if let Err(error) = result {
            self.reply_project_directory(reg, requester, project, Some(error));
            return;
        }
        self.pending_project_directories.insert(project, requester);
        self.agents
            .send(uniterm_proto::CoreToAgent::ProjectDirectoryValidate { project, root });
    }

    /// Recheck ownership, then append before updating dependent projections.
    pub(super) fn finish_project_directory(
        &mut self,
        reg: &Registry,
        project: ProjectId,
        result: Result<String, String>,
    ) {
        let Some(requester) = self.pending_project_directories.remove(&project) else {
            return;
        };
        // The Project can disappear or acquire a running workflow while the
        // filesystem worker is busy. Recheck ownership before committing.
        let result = result.and_then(|root| {
            let index = self.directory_change_allowed(project)?;
            if self.projects[index].root == root {
                return Ok(());
            }
            self.append_event(crate::eventlog::LogEvent::ProjectDirectoryChanged {
                project: project.0,
                root: root.clone(),
            });
            self.projects[index].root = root;
            self.sync_artifact_watches();
            if self.active_project == project && self.file_manager_visible() {
                let focus = self.files.focused;
                self.sync_file_manager(focus);
            }
            self.persist();
            self.repaint_chrome_all(reg);
            Ok(())
        });
        self.reply_project_directory(reg, requester, project, result.err());
    }

    fn reply_project_directory(
        &mut self,
        reg: &Registry,
        requester: DirectoryRequester,
        project: ProjectId,
        error: Option<String>,
    ) {
        match requester {
            DirectoryRequester::Client(token) => {
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&ServerMessage::ProjectDirectoryChanged {
                        project,
                        error,
                    }));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
                self.reply_workspace(reg, token);
            }
            DirectoryRequester::Control { connection, id } => {
                let response = match error {
                    Some(error) => uniterm_proto::ControlResponse::error(
                        id,
                        "project_directory_rejected",
                        error,
                    ),
                    None => uniterm_proto::ControlResponse::ok(
                        id,
                        uniterm_proto::ControlResult::Mutation {
                            resource: "project".into(),
                            id: Some(project.0),
                            found: true,
                            accepted: true,
                        },
                    ),
                };
                self.agents
                    .send(uniterm_proto::CoreToAgent::ControlResponse {
                        connection,
                        response,
                    });
            }
        }
    }
}
