//! Preview and revalidate observed-day Project cleanup before any Pane closes.
use super::*;
use uniterm_proto::overwatch::{PruneProject, PruneReport};
use uniterm_proto::{ControlResponse, ControlResult, CoreToAgent};

impl Server {
    pub(super) fn request_overwatch_prune(
        &mut self,
        connection: u64,
        id: u64,
        expected_sequence: Option<u64>,
        expected_date: Option<String>,
        projects: Vec<ProjectId>,
        confirmed: bool,
    ) {
        let error = if expected_sequence.is_some() && !confirmed {
            Some(("confirmation_required", "Project cleanup closes all candidate Panes; pass --confirm after reviewing the preview"))
        } else if expected_sequence.is_some() && expected_date.is_none() {
            Some((
                "date_required",
                "Apply must include the local date returned by preview",
            ))
        } else if expected_sequence.is_none()
            && (!projects.is_empty() || confirmed || expected_date.is_some())
        {
            Some((
                "invalid_preview",
                "Preview must not contain an apply list or confirmation",
            ))
        } else if expected_sequence.is_some_and(|sequence| sequence != self.log.current_sequence())
        {
            Some((
                "stale_preview",
                "Workspace changed since preview; request a fresh preview",
            ))
        } else if !self.event_writes_enabled || self.durability_error.is_some() {
            Some((
                "history_unavailable",
                "Project cleanup requires healthy durable history",
            ))
        } else {
            None
        };
        if let Some((code, error)) = error {
            self.agents.send(CoreToAgent::ControlResponse {
                connection,
                response: ControlResponse::error(id, code, error),
            });
            return;
        }
        self.agents.send(CoreToAgent::OverwatchPruneRead {
            name: self.name.clone(),
            through: self.log.current_sequence(),
            connection,
            id,
            expected_sequence,
            expected_date,
            projects,
            confirmed,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_overwatch_prune(
        &mut self,
        reg: &Registry,
        connection: u64,
        id: u64,
        expected_sequence: Option<u64>,
        expected_date: Option<String>,
        projects: Vec<ProjectId>,
        confirmed: bool,
        result: Result<uniterm_core::timeline::TimelineDay, String>,
    ) {
        let response = self.apply_overwatch_prune(
            reg,
            id,
            expected_sequence,
            expected_date,
            projects,
            confirmed,
            result,
        );
        self.agents.send(CoreToAgent::ControlResponse {
            connection,
            response,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_overwatch_prune(
        &mut self,
        reg: &Registry,
        id: u64,
        expected_sequence: Option<u64>,
        expected_date: Option<String>,
        requested: Vec<ProjectId>,
        confirmed: bool,
        result: Result<uniterm_core::timeline::TimelineDay, String>,
    ) -> ControlResponse {
        let day = match result {
            Ok(day) => day,
            Err(error) => return ControlResponse::error(id, "history_unavailable", error),
        };
        if expected_date.as_ref().is_some_and(|date| date != &day.date) {
            return ControlResponse::error(
                id,
                "stale_day",
                "Local date changed since preview; request a fresh preview",
            );
        }
        if day.current_sequence != self.log.current_sequence()
            || expected_sequence.is_some_and(|seq| seq != day.current_sequence)
        {
            return ControlResponse::error(
                id,
                "stale_preview",
                "Workspace changed while reading history; request a fresh preview",
            );
        }
        if self.projects.len() > 512 {
            return ControlResponse::error(
                id,
                "too_many_projects",
                "Cleanup preview is limited to 512 Projects; use explicit scoped removals",
            );
        }
        if day.end_ms <= super::guardrail::unix_time_ms() {
            return ControlResponse::error(
                id,
                "stale_day",
                "Local day changed during preview; request a fresh preview",
            );
        }
        if day.projects_truncated {
            return ControlResponse::error(
                id,
                "incomplete_history",
                "Today's Project summary is truncated; cleanup cannot infer missing activity",
            );
        }
        if !self.event_writes_enabled || self.durability_error.is_some() {
            return ControlResponse::error(
                id,
                "history_unavailable",
                "Durable history became unavailable",
            );
        }
        let touched: HashSet<_> = day.projects.iter().filter_map(|p| p.project).collect();
        let busy: HashSet<_> = self
            .windows
            .iter()
            .filter(|w| {
                w.layout.pane_ids().iter().any(|id| {
                    self.panes
                        .get(id)
                        .and_then(|p| p.agent.as_ref())
                        .is_some_and(|a| {
                            !matches!(a.status, AgentStatus::Idle | AgentStatus::Exited)
                        })
                })
            })
            .map(|w| w.project)
            .collect();
        let mut report = PruneReport {
            workspace: self.name.clone(), date: day.date, sequence: day.current_sequence,
            basis: "No recorded Uniterm Project visit, note, or agent activity today; activity outside Uniterm is not observed".into(),
            candidates: Vec::new(), retained: Vec::new(), applied: false,
        };
        for project in &self.projects {
            let reason = if touched.contains(&project.id) {
                Some("Recorded activity today")
            } else if project.id == self.active_project {
                Some("Currently selected Project")
            } else if busy.contains(&project.id) {
                Some("Agent is active or needs attention")
            } else if Self::worktree_registration(project).is_some() {
                Some("Managed worktree requires its dedicated removal flow")
            } else {
                None
            };
            let panes = self
                .windows
                .iter()
                .filter(|w| w.project == project.id)
                .map(|w| w.layout.pane_count())
                .sum();
            let item = PruneProject {
                id: project.id,
                name: project
                    .name
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(128)
                    .collect(),
                panes,
                reason: reason.unwrap_or("No recorded activity today").into(),
            };
            if reason.is_some() {
                report.retained.push(item);
            } else {
                report.candidates.push(item);
            }
        }
        if expected_sequence.is_some() {
            let wanted: HashSet<_> = requested.iter().copied().collect();
            let candidates: HashSet<_> = report.candidates.iter().map(|p| p.id).collect();
            if wanted != candidates || wanted.len() != requested.len() {
                return ControlResponse::error(
                    id,
                    "preview_mismatch",
                    "Apply must name exactly the current preview's stable Project IDs",
                );
            }
            if !confirmed {
                return ControlResponse::error(
                    id,
                    "confirmation_required",
                    "Project cleanup requires caller confirmation",
                );
            }
            // Audit every decision before closing the first Pane; the flag comes
            // only from the control caller's explicit confirmed request.
            for project in &report.candidates {
                if !self.guard_semantic(
                    uniterm_core::GuardedCommand::ProjectRemove,
                    confirmed,
                    Some(project.id),
                ) {
                    return ControlResponse::error(
                        id,
                        "confirmation_required",
                        "Project removal was refused",
                    );
                }
            }
            // A projected cursor proves each decision was queued, not that the
            // runtime has fsynced it. Refuse if any audit record was suppressed
            // or persistence became unavailable before destructive work starts.
            let audited_through = report.sequence.checked_add(report.candidates.len() as u64);
            if !self.event_writes_enabled
                || self.durability_error.is_some()
                || audited_through != Some(self.log.current_sequence())
            {
                return ControlResponse::error(
                    id,
                    "history_unavailable",
                    "Cleanup audit was not fully recorded; no Project was removed",
                );
            }
            for project in &report.candidates {
                self.remove_project(reg, project.id);
            }
            self.full_repaint_all(reg);
            report.applied = true;
        }
        ControlResponse::ok(id, ControlResult::OverwatchPrune { report })
    }
}

#[cfg(test)]
mod tests {
    use super::super::reaper::common;
    use super::*;
    use uniterm_core::timeline::{TimelineContext, TimelineDay};

    fn fixture() -> (Server, Poll, PathBuf) {
        common::isolate_state();
        let name = common::unique_workspace_name();
        let root = common::socket_root().join(&name);
        std::fs::create_dir_all(&root).unwrap();
        let (mut server, poll) = Server::bind_internal(
            &root.join(format!("{name}.sock")),
            "/bin/sh",
            &[],
            80,
            24,
            false,
        )
        .unwrap();
        server.event_writes_enabled = true;
        (server, poll, root)
    }
    fn project(
        server: &mut Server,
        poll: &Poll,
        root: &std::path::Path,
        name: &str,
    ) -> (ProjectId, PaneId, PathBuf) {
        let path = root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("source.txt"), b"keep this source directory\n").unwrap();
        server
            .create_project_with_focus(poll.registry(), name, path.to_str().unwrap(), true)
            .unwrap();
        let id = server.projects.iter().find(|p| p.name == name).unwrap().id;
        let pane = server
            .windows
            .iter()
            .find(|w| w.project == id)
            .unwrap()
            .active;
        (id, pane, path)
    }
    fn day(server: &Server) -> TimelineDay {
        TimelineDay {
            date: "2099-01-01".into(),
            end_ms: u64::MAX,
            current_sequence: server.log.current_sequence(),
            ..TimelineDay::default()
        }
    }
    fn report(response: ControlResponse) -> PruneReport {
        assert!(response.error.is_none(), "{:?}", response.error);
        let Some(ControlResult::OverwatchPrune { report }) = response.result else {
            panic!("missing prune report")
        };
        report
    }

    #[test]
    fn prune_refuses_stale_unconfirmed_mismatched_and_truncated_previews_without_mutation() {
        let (mut server, poll, root) = fixture();
        let (candidate, _, path) = project(&mut server, &poll, &root, "candidate");
        let baseline = day(&server);
        let original: Vec<_> = server.projects.iter().map(|p| p.id).collect();
        let panes = server.panes.len();
        let cases = [
            (
                "confirmation_required",
                false,
                vec![candidate],
                baseline.date.clone(),
                baseline.clone(),
                baseline.current_sequence,
            ),
            (
                "preview_mismatch",
                true,
                vec![],
                baseline.date.clone(),
                baseline.clone(),
                baseline.current_sequence,
            ),
            (
                "preview_mismatch",
                true,
                vec![candidate, candidate],
                baseline.date.clone(),
                baseline.clone(),
                baseline.current_sequence,
            ),
            (
                "preview_mismatch",
                true,
                vec![ProjectId(u64::MAX)],
                baseline.date.clone(),
                baseline.clone(),
                baseline.current_sequence,
            ),
            (
                "stale_day",
                true,
                vec![candidate],
                "2098-12-31".into(),
                baseline.clone(),
                baseline.current_sequence,
            ),
            (
                "stale_preview",
                true,
                vec![candidate],
                baseline.date.clone(),
                baseline.clone(),
                baseline.current_sequence + 1,
            ),
            (
                "stale_preview",
                true,
                vec![candidate],
                baseline.date.clone(),
                TimelineDay {
                    current_sequence: baseline.current_sequence + 1,
                    ..baseline.clone()
                },
                baseline.current_sequence,
            ),
            (
                "incomplete_history",
                true,
                vec![candidate],
                baseline.date.clone(),
                TimelineDay {
                    projects_truncated: true,
                    ..baseline.clone()
                },
                baseline.current_sequence,
            ),
            (
                "stale_day",
                true,
                vec![candidate],
                baseline.date.clone(),
                TimelineDay {
                    end_ms: 1,
                    ..baseline.clone()
                },
                baseline.current_sequence,
            ),
        ];
        for (code, confirmed, projects, date, snapshot, sequence) in cases {
            let response = server.apply_overwatch_prune(
                poll.registry(),
                1,
                Some(sequence),
                Some(date),
                projects,
                confirmed,
                Ok(snapshot),
            );
            assert_eq!(response.error.unwrap().code, code);
            assert_eq!(
                server.projects.iter().map(|p| p.id).collect::<Vec<_>>(),
                original
            );
            assert_eq!(server.panes.len(), panes);
            assert_eq!(server.log.current_sequence(), baseline.current_sequence);
            assert_eq!(
                std::fs::read(path.join("source.txt")).unwrap(),
                b"keep this source directory\n"
            );
        }
        server.durability_error = Some("writer unavailable".into());
        let response = server.apply_overwatch_prune(
            poll.registry(),
            1,
            Some(baseline.current_sequence),
            Some(baseline.date.clone()),
            vec![candidate],
            true,
            Ok(baseline),
        );
        assert_eq!(response.error.unwrap().code, "history_unavailable");
        assert_eq!(server.panes.len(), panes);
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prune_retains_touched_current_busy_unknown_overwatch_and_worktree_projects() {
        let (mut server, poll, root) = fixture();
        let current = server.active_project;
        let (touched, _, _) = project(&mut server, &poll, &root, "touched");
        let (busy, busy_pane, _) = project(&mut server, &poll, &root, "busy");
        let (unknown, unknown_pane, _) = project(&mut server, &poll, &root, "unknown");
        let (watcher, _, _) = project(&mut server, &poll, &root, "watcher");
        let (worktree, _, worktree_path) = project(&mut server, &poll, &root, "worktree");
        let (idle, idle_pane, _) = project(&mut server, &poll, &root, "idle");
        server.bind_agent(busy_pane, "codex");
        server
            .panes
            .get_mut(&busy_pane)
            .unwrap()
            .agent
            .as_mut()
            .unwrap()
            .status = AgentStatus::Working;
        server.bind_agent(unknown_pane, "pi");
        server
            .panes
            .get_mut(&unknown_pane)
            .unwrap()
            .agent
            .as_mut()
            .unwrap()
            .status = AgentStatus::Unknown;
        server.bind_agent(idle_pane, "claude");
        server
            .panes
            .get_mut(&idle_pane)
            .unwrap()
            .agent
            .as_mut()
            .unwrap()
            .status = AgentStatus::Idle;
        server.overwatch.enabled = true;
        server.overwatch.project = Some(watcher);
        let registration = uniterm_proto::WorktreeRegistration {
            project: worktree,
            project_name: "worktree".into(),
            repository: root.to_string_lossy().into_owned(),
            path: worktree_path.to_string_lossy().into_owned(),
            branch: "test".into(),
            created_head: "abc".into(),
        };
        server
            .projects
            .iter_mut()
            .find(|p| p.id == worktree)
            .unwrap()
            .metadata = Server::worktree_metadata(&registration);
        let mut snapshot = day(&server);
        snapshot.projects.push(TimelineContext {
            project: Some(touched),
            ..TimelineContext::default()
        });
        let sequence = snapshot.current_sequence;
        let preview = report(server.apply_overwatch_prune(
            poll.registry(),
            1,
            None,
            None,
            vec![],
            false,
            Ok(snapshot),
        ));
        assert_eq!(
            preview.candidates.iter().map(|p| p.id).collect::<Vec<_>>(),
            [watcher, idle]
        );
        let retained: HashSet<_> = preview.retained.iter().map(|p| p.id).collect();
        assert_eq!(
            retained,
            HashSet::from([current, touched, busy, unknown, worktree])
        );
        assert!(preview
            .retained
            .iter()
            .find(|p| p.id == unknown)
            .unwrap()
            .reason
            .contains("active or needs attention"));
        assert_eq!(
            server.log.current_sequence(),
            sequence,
            "preview is read-only"
        );
        assert_eq!(server.projects.len(), 7);
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prune_refuses_a_missing_audit_record_and_preserves_source_directories_on_success() {
        let (mut server, poll, root) = fixture();
        let (candidate, _, path) = project(&mut server, &poll, &root, "candidate");
        // The log deduplicates identical adjacent facts. A prior identical
        // decision must not count as the fresh audit for this apply request.
        assert!(server.guard_semantic(
            uniterm_core::GuardedCommand::ProjectRemove,
            true,
            Some(candidate)
        ));
        let snapshot = day(&server);
        let response = server.apply_overwatch_prune(
            poll.registry(),
            1,
            Some(snapshot.current_sequence),
            Some(snapshot.date.clone()),
            vec![candidate],
            true,
            Ok(snapshot),
        );
        assert_eq!(response.error.unwrap().code, "history_unavailable");
        assert!(server.projects.iter().any(|p| p.id == candidate));
        // A distinct event allows an exact fresh audit, then removes only the
        // Workspace navigation and Panes, never the source tree on disk.
        server.append_event(crate::eventlog::LogEvent::ProjectSelected {
            project: server.active_project.0,
        });
        let snapshot = day(&server);
        let result = report(server.apply_overwatch_prune(
            poll.registry(),
            2,
            Some(snapshot.current_sequence),
            Some(snapshot.date.clone()),
            vec![candidate],
            true,
            Ok(snapshot),
        ));
        assert!(result.applied);
        assert!(!server.projects.iter().any(|p| p.id == candidate));
        assert_eq!(
            std::fs::read(path.join("source.txt")).unwrap(),
            b"keep this source directory\n"
        );
        assert!(path.is_dir());
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }
}
