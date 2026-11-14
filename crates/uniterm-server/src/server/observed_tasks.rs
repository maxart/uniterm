//! Direct work shares Tasks, Graph's invocation identity, and durable history.
use super::*;
use uniterm_core::tasks::{ObservedTask, Task, TaskContext, TaskObservation, TaskStatus};

#[derive(Default)]
pub(super) struct Evidence {
    invocation: u64,
    session: Option<String>,
    ids: HashMap<String, u64>,
    texts: HashMap<u64, u64>,
    dismissed: Option<ObservedTask>,
    // Only work captured in this live observation span may gain a late identity.
    // Recovery cannot prove which conversation owned an unidentified Task.
    can_refine_session: bool,
}

fn fingerprint(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl Server {
    pub(super) fn index_task_owner(&mut self, id: u64) {
        if let Some(pane) = self
            .tasks
            .get(id)
            .and_then(|task| task.context.pane)
            .filter(|pane| self.panes.contains_key(pane))
        {
            let owner = self.task_owners.entry(pane).or_insert(id);
            *owner = (*owner).max(id);
            let task = self.tasks.get(id).unwrap();
            if let (Some(invocation), Some(observed)) =
                (task.context.invocation, &task.context.observed)
            {
                let evidence = self.task_evidence.entry(pane).or_default();
                if evidence.invocation != invocation || evidence.session != observed.session_id {
                    if *owner != id {
                        return;
                    }
                    if evidence.invocation == invocation && evidence.session.is_none() {
                        // Late discovery refines all evidence from this invocation,
                        // including finished Tasks whose native reports may lag.
                        evidence.session.clone_from(&observed.session_id);
                    } else {
                        *evidence = Evidence {
                            invocation,
                            session: observed.session_id.clone(),
                            ..Default::default()
                        };
                    }
                }
                evidence.dismissed = None;
                for turn in &observed.prompt_ids {
                    evidence.ids.insert(turn.clone(), id);
                }
                if !observed.last_prompt.is_empty() {
                    evidence
                        .texts
                        .insert(fingerprint(&observed.last_prompt), id);
                }
            }
        }
    }

    pub(super) fn rebuild_task_owners(&mut self) {
        self.task_owners.clear();
        self.task_evidence.clear();
        let ids: Vec<_> = self.tasks.records().map(|task| task.id).collect();
        for id in ids {
            self.index_task_owner(id);
        }
    }

    fn observed_task_context(&self, pane: PaneId) -> Option<TaskContext> {
        if !self.config.auto_track_tasks
            || self.overwatch.pane == Some(pane)
            || self.run_graph.active_for_pane(pane).is_some()
        {
            return None;
        }
        let current = self.panes.get(&pane)?;
        let agent = current.agent.as_ref()?;
        Some(TaskContext {
            project: self.timeline_context(pane).project,
            pane: Some(pane),
            invocation: Some(*self.timeline_invocations.get(&pane)?),
            provider: Some(agent.id.clone()),
            observed: Some(ObservedTask {
                session_id: agent.session_id.clone(),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    pub(super) fn observe_direct_task(&mut self, pane: PaneId, observation: TaskObservation) {
        let Some(context) = self.observed_task_context(pane) else {
            return;
        };
        if self.replayed_task_observation(pane, &context, &observation) {
            return;
        }
        let existing = self
            .task_owners
            .get(&pane)
            .and_then(|id| self.tasks.get(*id))
            .filter(|task| task.context.invocation == context.invocation)
            .filter(|task| {
                task.context.observed.as_ref().is_none_or(|old| {
                    old.session_id == context.observed.as_ref().unwrap().session_id
                })
            });
        if let Some(task) = uniterm_core::tasks::observe_direct_work(
            existing,
            self.tasks.next_id(),
            context,
            observation,
        ) {
            self.record_observed_task(task);
        }
    }

    fn replayed_task_observation(
        &mut self,
        pane: PaneId,
        context: &TaskContext,
        observation: &TaskObservation,
    ) -> bool {
        let Some(evidence) = self.task_evidence.get_mut(&pane) else {
            return false;
        };
        if Some(evidence.invocation) != context.invocation
            || evidence.session != context.observed.as_ref().and_then(|o| o.session_id.clone())
        {
            return false;
        }
        let current_id = self.task_owners.get(&pane).copied();
        let (turn, text) = match observation {
            TaskObservation::Prompt { id, text } => (id, Some(text)),
            TaskObservation::Finished { id } | TaskObservation::Interrupted { id } => (id, None),
        };
        if let Some(dismissed) = &evidence.dismissed {
            if text.is_some_and(|text| {
                uniterm_core::tasks::observed_prompt(text) == dismissed.last_prompt
            }) && (turn.is_none()
                || dismissed.prompt_id.is_none()
                || turn
                    .as_ref()
                    .is_some_and(|turn| dismissed.prompt_ids.contains(turn)))
            {
                return true;
            }
        }
        if let Some(owner) = turn.as_ref().and_then(|turn| evidence.ids.get(turn)) {
            if Some(*owner) != current_id || self.tasks.get(*owner).is_none() {
                return true;
            }
            if self
                .tasks
                .get(*owner)
                .and_then(|task| task.context.observed.as_ref())
                .is_some_and(|old| old.prompt_id.as_ref() != turn.as_ref())
            {
                return true;
            }
        }
        if let (Some(turn), Some(text)) = (turn, text) {
            let clean = uniterm_core::tasks::observed_prompt(text);
            if !evidence.ids.contains_key(turn) {
                if let Some(owner) = evidence
                    .texts
                    .get(&fingerprint(&clean))
                    .copied()
                    .filter(|id| Some(*id) != current_id)
                {
                    if self
                        .tasks
                        .get(owner)
                        .and_then(|task| task.context.observed.as_ref())
                        .is_some_and(|old| old.prompt_id.is_none() && old.last_prompt == clean)
                    {
                        evidence.ids.insert(turn.clone(), owner);
                        return true;
                    }
                }
            }
        }
        false
    }

    fn record_observed_task(&mut self, mut task: Task) {
        if !self.event_writes_enabled || self.durability_error.is_some() {
            return;
        }
        let id = task.id;
        let newly_unidentified = task
            .context
            .observed
            .as_ref()
            .is_some_and(|observed| observed.session_id.is_none())
            && self
                .tasks
                .get(id)
                .is_none_or(|previous| previous.context.observed.is_none());
        let pane = task.context.pane;
        self.append_event(crate::eventlog::LogEvent::TaskObserved { task: task.clone() });
        task.updated = self.log.current_sequence();
        self.tasks.observe(task);
        self.index_task_owner(id);
        if newly_unidentified {
            if let Some(evidence) = pane.and_then(|pane| self.task_evidence.get_mut(&pane)) {
                evidence.can_refine_session = true;
            }
        }
    }

    /// Follow observed activity, never translate idle or process exit into success.
    pub(super) fn observe_task_status(&mut self, pane: PaneId, status: AgentStatus) {
        let Some(id) = self.task_owners.get(&pane) else {
            return;
        };
        let Some(task) = self.tasks.get(*id) else {
            return;
        };
        if !self.config.auto_track_tasks
            || task.context.invocation != self.timeline_invocations.get(&pane).copied()
        {
            return;
        }
        if task.context.observed.as_ref().is_some_and(|observed| {
            self.panes
                .get(&pane)
                .and_then(|pane| pane.agent.as_ref())
                .is_some_and(|agent| agent.session_id != observed.session_id)
        }) {
            return;
        }
        if let Some(task) = uniterm_core::tasks::observe_work_status(task, status) {
            self.record_observed_task(task);
        }
    }

    pub(super) fn retire_task_session(&mut self, pane: PaneId, session: &str) {
        if !self.config.auto_track_tasks {
            // A conversation observed during opt-out cannot safely inherit the
            // previous captured work when tracking resumes. Keep history intact.
            if self
                .task_owners
                .get(&pane)
                .and_then(|id| self.tasks.get(*id))
                .is_some_and(|task| task.context.observed.is_some())
            {
                self.task_owners.remove(&pane);
                self.task_evidence.remove(&pane);
            }
            return;
        }
        let Some(task) = self
            .task_owners
            .get(&pane)
            .and_then(|id| self.tasks.get(*id))
        else {
            return;
        };
        if task.context.invocation != self.timeline_invocations.get(&pane).copied() {
            return;
        }
        let Some(observed) = &task.context.observed else {
            return;
        };
        if observed.session_id.is_none() {
            if !self
                .task_evidence
                .get(&pane)
                .is_some_and(|evidence| evidence.can_refine_session)
            {
                return;
            }
            // Discovery may identify the conversation after its first prompt.
            // Persist that identity before a later /new can reuse this Pane.
            let mut task = task.clone();
            task.context.observed.as_mut().unwrap().session_id = Some(session.to_owned());
            self.record_observed_task(task);
            return;
        }
        if observed.session_id.as_deref() == Some(session) {
            return;
        }
        if let Some(task) = uniterm_core::tasks::observe_work_status(task, AgentStatus::Exited) {
            self.record_observed_task(task);
        }
    }

    pub(super) fn dismiss_observed_task(&mut self, id: u64) {
        if let Some(task) = self.tasks.get(id) {
            if let (Some(pane), Some(observed)) = (task.context.pane, &task.context.observed) {
                if let Some(evidence) = self.task_evidence.get_mut(&pane) {
                    evidence.dismissed = Some(observed.clone());
                }
            }
        }
    }

    pub(super) fn observe_task_summary(&mut self, pane: PaneId, summary: &str) {
        let Some(context) = self.observed_task_context(pane) else {
            return;
        };
        if self
            .task_owners
            .get(&pane)
            .and_then(|id| self.tasks.get(*id))
            .is_some_and(|task| {
                task.context.invocation == context.invocation
                    && task.context.observed.as_ref().is_none_or(|old| {
                        old.session_id == context.observed.as_ref().unwrap().session_id
                    })
            })
        {
            return;
        }
        if self.task_evidence.get(&pane).is_some_and(|evidence| {
            evidence.dismissed.is_some() && Some(evidence.invocation) == context.invocation
        }) {
            return;
        }
        let title = uniterm_core::agent_detail::bounded_text(summary.trim(), 160);
        if title.is_empty() {
            return;
        }
        let status = self
            .panes
            .get(&pane)
            .and_then(|pane| pane.agent.as_ref())
            .map(|agent| agent.status);
        let task = Task {
            id: self.tasks.next_id(),
            title,
            status: if matches!(
                status,
                Some(
                    AgentStatus::Permission
                        | AgentStatus::Question
                        | AgentStatus::Error
                        | AgentStatus::Exited
                )
            ) {
                TaskStatus::Blocked
            } else {
                TaskStatus::Doing
            },
            notes: String::new(),
            context,
            updated: 0,
        };
        self.record_observed_task(task);
    }
}

#[cfg(test)]
mod tests {
    use super::super::reaper::common;
    use super::*;

    fn fixture() -> (Server, Poll) {
        common::isolate_state();
        let name = common::unique_workspace_name();
        let dir = common::socket_root().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut server, poll) = Server::bind_internal(
            &dir.join(format!("{name}.sock")),
            "/bin/sh",
            &[],
            100,
            30,
            false,
        )
        .unwrap();
        server.event_writes_enabled = true;
        (server, poll)
    }

    fn hook(server: &mut Server, poll: &Poll, value: serde_json::Value) {
        let pane = server.windows[server.active_window].active;
        let token = server.panes[&pane].token;
        let wire = format!("\x1b]777;notify;uniterm://cli-agent;{value}\x07");
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .term
            .feed(wire.as_bytes());
        server.on_pty(poll.registry(), token, false, false);
    }

    #[test]
    fn submitted_work_is_shared_scoped_durable_and_never_finished_by_idle() {
        let (mut server, mut poll) = fixture();
        let pane = server.windows[server.active_window].active;
        for provider in ["codex", "claude", "grok", "opencode", "pi"] {
            hook(
                &mut server,
                &poll,
                serde_json::json!({"agent":provider,"event":"prompt_submit","session_id":provider,"turn_id":"one","prompt":"Fix login"}),
            );
            let id = server.tasks.next_id() - 1;
            let task = server.task_detail(id).unwrap();
            assert_eq!(task.context.prompt.as_deref(), Some("Fix login"));
            assert_eq!(task.context.pane, Some(pane));
            assert_eq!(task.context.project, Some(server.active_project));
            assert_eq!(task.context.provider.as_deref(), Some(provider));
            let count = server.tasks.len();
            hook(
                &mut server,
                &poll,
                serde_json::json!({"agent":provider,"event":"prompt_submit","session_id":provider,"turn_id":"one","prompt":"Fix login"}),
            );
            assert_eq!(server.tasks.len(), count);
            hook(
                &mut server,
                &poll,
                serde_json::json!({"agent":provider,"event":"idle","session_id":provider}),
            );
            assert_eq!(server.tasks.get(id).unwrap().status, TaskStatus::Doing);
            hook(
                &mut server,
                &poll,
                serde_json::json!({"agent":provider,"event":"permission_request","session_id":provider}),
            );
            assert_eq!(server.tasks.get(id).unwrap().status, TaskStatus::Blocked);
            hook(
                &mut server,
                &poll,
                serde_json::json!({"agent":provider,"event":"prompt_submit","session_id":provider,"turn_id":"two","prompt":"Use OAuth"}),
            );
            assert_eq!(server.tasks.len(), count);
            assert_eq!(server.tasks.get(id).unwrap().status, TaskStatus::Doing);
            assert!(server
                .tasks
                .get(id)
                .unwrap()
                .context
                .prompt
                .as_ref()
                .unwrap()
                .contains("Use OAuth"));
            // Child hook reports must never finish the owning root's Task.
            hook(
                &mut server,
                &poll,
                serde_json::json!({"agent":provider,"event":"turn_complete","session_id":"child","parent_session_id":provider}),
            );
            assert_eq!(server.tasks.get(id).unwrap().status, TaskStatus::Doing);
            hook(
                &mut server,
                &poll,
                serde_json::json!({"agent":provider,"event":"turn_complete","session_id":provider,"turn_id":"two"}),
            );
            assert_eq!(server.tasks.get(id).unwrap().status, TaskStatus::Done);
            let page = serde_json::to_value(server.overwatch_context_page(0, 64))
                .unwrap()
                .to_string();
            assert!(!page.contains("Fix login"));
            assert!(page.contains(&format!("Task #{id}")));
            assert!(
                !page.contains("Use OAuth"),
                "private follow-up content leaked into metadata"
            );
            hook(
                &mut server,
                &poll,
                serde_json::json!({"agent":provider,"event":"session_end"}),
            );
        }
        let encoded = serde_json::to_vec(&server.tasks).unwrap();
        let decoded: uniterm_core::TaskList = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, server.tasks);
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn unidentified_hook_followup_rejects_delayed_native_completion() {
        let (mut server, mut poll) = fixture();
        let pane = server.windows[server.active_window].active;
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"s","turn_id":"turn-A","prompt":"Fix login"}),
        );
        let id = server.tasks.next_id() - 1;
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"s","prompt":"Also fix refresh"}),
        );
        assert_eq!(
            server
                .tasks
                .get(id)
                .unwrap()
                .context
                .observed
                .as_ref()
                .unwrap()
                .prompt_id,
            None
        );
        server.observe_direct_task(
            pane,
            TaskObservation::Finished {
                id: Some("turn-A".into()),
            },
        );
        assert_eq!(server.tasks.get(id).unwrap().status, TaskStatus::Doing);
        server.observe_direct_task(
            pane,
            TaskObservation::Prompt {
                id: Some("turn-B".into()),
                text: "Also fix refresh".into(),
            },
        );
        assert_eq!(server.tasks.len(), 1);
        assert_eq!(
            server
                .tasks
                .get(id)
                .unwrap()
                .context
                .observed
                .as_ref()
                .unwrap()
                .prompt_id
                .as_deref(),
            Some("turn-B")
        );
        server.observe_direct_task(
            pane,
            TaskObservation::Finished {
                id: Some("turn-B".into()),
            },
        );
        assert_eq!(server.tasks.get(id).unwrap().status, TaskStatus::Done);
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn delayed_native_turns_cannot_reopen_or_finish_newer_direct_work() {
        let (mut server, mut poll) = fixture();
        let pane = server.windows[server.active_window].active;
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"s","turn_id":"a","prompt":"Fix login"}),
        );
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"turn_complete","turn_id":"a"}),
        );
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"s","turn_id":"b","prompt":"Write docs"}),
        );
        let id = server.tasks.next_id() - 1;
        server.observe_direct_task(
            pane,
            TaskObservation::Prompt {
                id: Some("a".into()),
                text: "Fix login".into(),
            },
        );
        server.observe_direct_task(
            pane,
            TaskObservation::Finished {
                id: Some("a".into()),
            },
        );
        assert_eq!(
            server.tasks.get(id).unwrap().context.prompt.as_deref(),
            Some("Write docs")
        );
        assert_eq!(server.tasks.get(id).unwrap().status, TaskStatus::Doing);
        // Steering can reuse a turn id with new text, and must be retained.
        server.observe_direct_task(
            pane,
            TaskObservation::Prompt {
                id: Some("b".into()),
                text: "Include screenshots".into(),
            },
        );
        assert!(server
            .tasks
            .get(id)
            .unwrap()
            .context
            .prompt
            .as_ref()
            .unwrap()
            .contains("Include screenshots"));
        // A same-text next turn advances the identity so its completion is accepted.
        server.observe_direct_task(
            pane,
            TaskObservation::Prompt {
                id: Some("c".into()),
                text: "Include screenshots".into(),
            },
        );
        server.observe_direct_task(
            pane,
            TaskObservation::Finished {
                id: Some("c".into()),
            },
        );
        assert_eq!(server.tasks.get(id).unwrap().status, TaskStatus::Done);
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn late_session_identity_cannot_transfer_work_to_a_new_conversation() {
        let (mut server, mut poll) = fixture();
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","prompt":"Original work"}),
        );
        let id = server.tasks.next_id() - 1;
        assert!(server
            .tasks
            .get(id)
            .unwrap()
            .context
            .observed
            .as_ref()
            .unwrap()
            .session_id
            .is_none());
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"session_start","session_id":"first"}),
        );
        assert_eq!(
            server
                .tasks
                .get(id)
                .unwrap()
                .context
                .observed
                .as_ref()
                .unwrap()
                .session_id
                .as_deref(),
            Some("first")
        );
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"session_start","session_id":"second"}),
        );
        assert_eq!(server.tasks.get(id).unwrap().status, TaskStatus::Blocked);
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"second","prompt":"New work"}),
        );
        assert_eq!(server.tasks.len(), 2);
        assert_eq!(
            server.tasks.get(id).unwrap().context.prompt.as_deref(),
            Some("Original work")
        );
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn late_session_identity_preserves_deduplication_of_earlier_tasks() {
        let (mut server, mut poll) = fixture();
        let pane = server.windows[server.active_window].active;
        for value in [
            serde_json::json!({"agent":"codex","event":"prompt_submit","prompt":"Earlier work"}),
            serde_json::json!({"agent":"codex","event":"turn_complete"}),
            serde_json::json!({"agent":"codex","event":"prompt_submit","prompt":"Current work"}),
            serde_json::json!({"agent":"codex","event":"session_start","session_id":"discovered"}),
        ] {
            hook(&mut server, &poll, value);
        }
        assert_eq!(server.tasks.len(), 2);
        let id = server.tasks.next_id() - 1;
        // Both live refinement and recovery must retain the earlier evidence.
        for restored in [false, true] {
            if restored {
                server.rebuild_task_owners();
            }
            server.observe_direct_task(
                pane,
                TaskObservation::Prompt {
                    id: Some("earlier-native-turn".into()),
                    text: "Earlier work".into(),
                },
            );
            server.observe_direct_task(
                pane,
                TaskObservation::Finished {
                    id: Some("earlier-native-turn".into()),
                },
            );
            assert_eq!(
                server.tasks.get(id).unwrap().context.prompt.as_deref(),
                Some("Current work")
            );
            assert_eq!(server.tasks.get(id).unwrap().status, TaskStatus::Doing);
        }
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn disabled_tracking_does_not_refine_or_adopt_work_from_an_unknown_session() {
        let (mut server, mut poll) = fixture();
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","prompt":"Before opt-out"}),
        );
        let id = server.tasks.next_id() - 1;
        let before = server.tasks.get(id).unwrap().clone();
        server.config.auto_track_tasks = false;
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"session_start","session_id":"another"}),
        );
        assert_eq!(server.tasks.get(id).unwrap(), &before);
        // Recovery must not undo the ownership break made during opt-out.
        server.rebuild_task_owners();
        server.config.auto_track_tasks = true;
        let pane = server.windows[server.active_window].active;
        server.observe_task_status(pane, AgentStatus::Permission);
        assert_eq!(server.tasks.get(id).unwrap(), &before);
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"another","prompt":"After opt-in"}),
        );
        assert_eq!(server.tasks.len(), 2);
        assert_eq!(server.tasks.get(id).unwrap(), &before);
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn observed_task_noop_budget_is_independent_of_retained_task_history() {
        let (mut server, mut poll) = fixture();
        for id in 1..=20_000 {
            server.tasks.insert(id, "retained task", TaskStatus::Done);
        }
        let pane = server.windows[server.active_window].active;
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"bench","turn_id":"one","prompt":"Current work"}),
        );
        let sequence = server.log.current_sequence();
        let started = std::time::Instant::now();
        for _ in 0..10_000 {
            server.observe_task_status(pane, AgentStatus::Working);
        }
        let elapsed = started.elapsed();
        eprintln!("10,000 observed Task no-op updates with 20,000 retained Tasks: {elapsed:?}");
        assert!(elapsed < std::time::Duration::from_secs(1));
        assert_eq!(
            server.log.current_sequence(),
            sequence,
            "unchanged observations append nothing"
        );
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn direct_work_opt_out_session_replacement_manual_override_and_service_exclusion() {
        let (mut server, mut poll) = fixture();
        let pane = server.windows[server.active_window].active;
        server.config.auto_track_tasks = false;
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"first","prompt":"Ignored"}),
        );
        assert!(server.tasks.is_empty());
        server.config.auto_track_tasks = true;
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"first","prompt":"Tracked"}),
        );
        let id = server.tasks.next_id() - 1;
        server.append_event(crate::eventlog::LogEvent::TaskStatusChanged {
            id,
            status: TaskStatus::Todo,
        });
        server.tasks.set_status(id, TaskStatus::Todo);
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"turn_complete","session_id":"first"}),
        );
        assert_eq!(server.tasks.get(id).unwrap().status, TaskStatus::Todo);
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"session_start","session_id":"second"}),
        );
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"permission_request","session_id":"second"}),
        );
        assert_eq!(server.tasks.get(id).unwrap().status, TaskStatus::Todo);
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"second","turn_id":"second-one","prompt":"New conversation"}),
        );
        assert_eq!(server.tasks.len(), 2);
        server.overwatch.pane = Some(pane);
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"second","prompt":"Workspace question"}),
        );
        assert_eq!(server.tasks.len(), 2);
        server.overwatch.pane = None;
        let deleted = server.tasks.next_id() - 1;
        server.append_event(crate::eventlog::LogEvent::TaskDeleted { id: deleted });
        server.tasks.remove(deleted);
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"second","turn_id":"second-one","prompt":"New conversation"}),
        );
        assert_eq!(
            server.tasks.len(),
            1,
            "duplicate evidence cannot resurrect a deleted Task"
        );
        hook(
            &mut server,
            &poll,
            serde_json::json!({"agent":"codex","event":"prompt_submit","session_id":"second","turn_id":"second-two","prompt":"New conversation"}),
        );
        assert_eq!(server.tasks.len(), 2);
        server.retire_all_ptys(&mut poll);
    }
}
