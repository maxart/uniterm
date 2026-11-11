//! Typed artifact projection and orchestration ownership wiring.

use super::*;

impl Server {
    pub(super) fn report_artifact_gate_failure(
        &mut self,
        kind: uniterm_proto::OrchestrationKind,
        token: u64,
        error: &str,
    ) {
        let pane = match kind {
            uniterm_proto::OrchestrationKind::Workflow => self
                .workflows
                .iter()
                .find(|run| run.state.token == token)
                .and_then(|run| run.role_panes.get(run.state.cur))
                .copied(),
            uniterm_proto::OrchestrationKind::Relay => self
                .relays
                .iter()
                .find(|run| run.state.token == token)
                .and_then(|run| run.role_panes.get(run.state.cur))
                .copied(),
        };
        let Some(pane) = pane else {
            return;
        };
        let waiting_kind = match kind {
            uniterm_proto::OrchestrationKind::Workflow => uniterm_core::WaitingKind::Workflow,
            uniterm_proto::OrchestrationKind::Relay => uniterm_core::WaitingKind::Relay,
        };
        let change = self.waiting.request(
            pane,
            None,
            waiting_kind,
            &format!("artifact gate failed: {error}"),
        );
        self.record_waiting_change(change);
    }

    /// Append one validated artifact transition before publishing it to the
    /// live projection.
    pub(super) fn record_artifact_change(&mut self, change: uniterm_core::ArtifactEvent) -> bool {
        if self.durability_error.is_some() {
            return false;
        }
        let mut next = self.artifacts.clone();
        if let Err(error) = next.apply(change.clone()) {
            self.durability_error = Some(format!("artifact projection rejected: {error}"));
            return false;
        }
        self.append_event(crate::eventlog::LogEvent::ArtifactLedger { change });
        self.artifacts = next;
        self.artifact_sequence = self.log.current_sequence();
        true
    }

    /// Attach runtime-authoritative file observations to the role owning the
    /// still-live completion token. The caller invokes this before advancing
    /// the orchestration, so active role ownership cannot race the handoff.
    pub(super) fn record_validated_artifacts(
        &mut self,
        task_id: u64,
        observations: &[uniterm_proto::ArtifactObservation],
    ) -> bool {
        if self.durability_error.is_some() {
            return false;
        }
        let Some(run_id) = self.run_graph.run_for_task(task_id) else {
            self.durability_error = Some(format!(
                "artifact validation for Task {task_id} had no owning Run"
            ));
            return false;
        };
        let Some(run) = self.run_graph.run(run_id) else {
            return false;
        };
        let project = run.project;
        let Some(role) = self.run_graph.active_role(run_id) else {
            self.durability_error = Some(format!(
                "artifact validation for Run {} had no active Role",
                run_id.0
            ));
            return false;
        };
        let mut next = self.artifacts.clone();
        let mut changes = Vec::with_capacity(observations.len());
        for observation in observations {
            let change = uniterm_core::ArtifactEvent::Observed {
                artifact: next.next_artifact_id(),
                project,
                producer_run: run_id,
                producer_role: role,
                kind: observation.kind,
                path: observation.path.clone(),
                digest: observation.digest.clone(),
                size: observation.size,
            };
            if let Err(error) = next.apply(change.clone()) {
                self.durability_error = Some(format!("artifact projection rejected: {error}"));
                return false;
            }
            changes.push(change);
        }
        for change in changes {
            self.append_event(crate::eventlog::LogEvent::ArtifactLedger { change });
        }
        self.artifacts = next;
        self.artifact_sequence = self.log.current_sequence();
        self.sync_artifact_watches();
        true
    }

    pub(super) fn sync_artifact_watches(&self) {
        let mut projects = Vec::new();
        for project in &self.projects {
            let artifacts: Vec<_> = self
                .artifacts
                .for_project(project.id)
                .iter()
                .filter_map(|id| self.artifacts.artifact(*id))
                .filter(|artifact| artifact.status != uniterm_core::ArtifactStatus::Superseded)
                .map(|artifact| uniterm_proto::ArtifactWatchEntry {
                    artifact: artifact.id,
                    path: artifact.path.clone(),
                })
                .collect();
            if !artifacts.is_empty() {
                projects.push(uniterm_proto::ArtifactWatchProject {
                    project: project.id,
                    root: project.root.clone(),
                    artifacts,
                });
            }
        }
        self.agents
            .send(uniterm_proto::CoreToAgent::ArtifactWatchSet { projects });
    }

    pub(super) fn request_artifact_observation(&mut self, artifact: uniterm_core::ArtifactId) {
        let Some(record) = self.artifacts.artifact(artifact) else {
            return;
        };
        if record.status == uniterm_core::ArtifactStatus::Superseded {
            return;
        }
        let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == record.project)
        else {
            return;
        };
        if !self.pending_artifact_observations.insert(artifact) {
            self.dirty_artifact_observations.insert(artifact);
            return;
        }
        self.agents
            .send(uniterm_proto::CoreToAgent::ArtifactObserve {
                artifact,
                project_root: project.root.clone(),
                claim: uniterm_proto::ArtifactClaim {
                    kind: record.kind,
                    path: record.path.clone(),
                },
            });
    }

    pub(super) fn apply_artifact_observation(
        &mut self,
        artifact: uniterm_core::ArtifactId,
        observation: Option<uniterm_proto::ArtifactObservation>,
        missing: bool,
    ) {
        let Some(current) = self.artifacts.artifact(artifact) else {
            return;
        };
        if current.status == uniterm_core::ArtifactStatus::Superseded {
            return;
        }
        if !self
            .projects
            .iter()
            .any(|project| project.id == current.project)
        {
            return;
        }
        if missing {
            if current.status != uniterm_core::ArtifactStatus::Missing {
                self.record_artifact_change(uniterm_core::ArtifactEvent::Missing { artifact });
            }
            return;
        }
        let Some(observation) = observation else {
            return;
        };
        if observation.path != current.path || observation.kind != current.kind {
            return;
        }
        if current.status != uniterm_core::ArtifactStatus::Available
            || current.digest != observation.digest
            || current.size != observation.size
        {
            self.record_artifact_change(uniterm_core::ArtifactEvent::Refreshed {
                artifact,
                digest: observation.digest,
                size: observation.size,
            });
        }
    }
}
