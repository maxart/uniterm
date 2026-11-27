//! Durable tasks (docs/09 tiers 1-2): the human-facing unit of work that a
//! session's panes and agents serve. Pure model + list logic; persistence is a
//! projection of the event log, server-side.

use serde::{Deserialize, Serialize};

/// Accepted work from a provider hook, native transcript, or explicit prompt API.
/// A quiet screen is never evidence that a requested task finished.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskObservation {
    /// The provider accepted this user prompt. Stable ids make replay idempotent.
    Prompt { id: Option<String>, text: String },
    /// The provider explicitly ended its response, without claiming verification.
    Finished { id: Option<String> },
    /// The response was interrupted or failed and needs attention.
    Interrupted { id: Option<String> },
}

/// Bounded provenance for automatically tracked work, retained through recovery.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservedTask {
    /// Native session ownership prevents a new conversation inheriting old work.
    pub session_id: Option<String>,
    /// Most recently accepted provider turn/message identity, when available.
    pub prompt_id: Option<String>,
    /// Recent accepted identities keep delayed transcript reports from reopening old work.
    pub prompt_ids: Vec<String>,
    /// Exact last prompt deduplicates hook and transcript reports of the same input.
    pub last_prompt: String,
    /// Human status edits take precedence over subsequent automatic observations.
    pub manual_status: bool,
    /// Prompt history reached its retained size limit; never imply complete history.
    pub truncated: bool,
}

/// A task's lifecycle status.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum TaskStatus {
    Todo,
    Doing,
    Blocked,
    Done,
}

impl TaskStatus {
    /// Sort order for the list: active/attention-worthy first, Done last.
    pub fn order(self) -> u8 {
        match self {
            TaskStatus::Doing => 0,
            TaskStatus::Blocked => 1,
            TaskStatus::Todo => 2,
            TaskStatus::Done => 3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TaskStatus::Todo => "todo",
            TaskStatus::Doing => "doing",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Done => "done",
        }
    }

    /// The human-facing name shown in the task manager.
    pub fn display(self) -> &'static str {
        match self {
            TaskStatus::Todo => "planned",
            TaskStatus::Doing => "running",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Done => "finished",
        }
    }

    /// The status's signature colour (used for badges/dots wherever tasks are
    /// drawn): planned blue, running amber, blocked red, finished green.
    pub fn color(self) -> crate::Color {
        match self {
            TaskStatus::Todo => crate::Color::Idx(75),
            TaskStatus::Doing => crate::Color::Idx(214),
            TaskStatus::Blocked => crate::Color::Idx(203),
            TaskStatus::Done => crate::Color::Idx(78),
        }
    }

    /// The next status in the manual cycle (planned -> running -> blocked ->
    /// finished -> planned), used by the task manager's status action.
    pub fn next(self) -> TaskStatus {
        match self {
            TaskStatus::Todo => TaskStatus::Doing,
            TaskStatus::Doing => TaskStatus::Blocked,
            TaskStatus::Blocked => TaskStatus::Done,
            TaskStatus::Done => TaskStatus::Todo,
        }
    }
}

/// Durable launch ownership and reusable choices. A retry is a new task linked
/// to its source, so earlier execution outcomes remain inspectable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskContext {
    pub project: Option<crate::ProjectId>,
    pub pane: Option<crate::PaneId>,
    pub invocation: Option<u64>,
    pub source_task: Option<u64>,
    pub provider: Option<String>,
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub profile: Option<String>,
    pub workflow: Option<String>,
    pub relay: bool,
    #[serde(default)]
    pub role_providers: Vec<crate::orchestrate::RoleProviderSelection>,
    /// Present only for observed direct work, not an orchestration's role prompts.
    pub observed: Option<ObservedTask>,
}

/// One task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: u64,
    pub title: String,
    pub status: TaskStatus,
    pub notes: String,
    #[serde(default)]
    pub context: TaskContext,
    #[serde(default)]
    pub updated: u64,
}

/// Retain only submitted text, with terminal controls removed and bounded storage.
pub fn observed_prompt(text: &str) -> String {
    let mut clean = String::new();
    for ch in text
        .trim()
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
    {
        if clean.len() + ch.len_utf8() > 16_384 {
            break;
        }
        clean.push(ch);
    }
    clean
}

/// Fold accepted prompts and explicit response endings into the existing Task model.
/// Follow-ups share unfinished work; an explicit finished response starts a boundary.
pub fn observe_direct_work(
    previous: Option<&Task>,
    next_id: u64,
    mut context: TaskContext,
    observation: TaskObservation,
) -> Option<Task> {
    let finished = matches!(observation, TaskObservation::Finished { .. });
    match observation {
        TaskObservation::Prompt { id, text } => {
            let truncated = text.trim().len() > 16_384;
            let text = observed_prompt(&text);
            if text.is_empty() || text.starts_with('/') && !text.contains(char::is_whitespace) {
                return None;
            }
            let id = id.map(|id| crate::agent_detail::bounded_text(&id, 512));
            if let Some(previous) = previous {
                if let Some(old) = &previous.context.observed {
                    let same_id = id.is_some() && id == old.prompt_id;
                    let same_text = old.last_prompt == text;
                    // A native report often arrives after its hook. Enrich the
                    // missing id without reopening a response already finished.
                    if same_text
                        && (same_id
                            || previous.status != TaskStatus::Done
                            || old.prompt_id.is_none() && id.is_some())
                    {
                        if id.is_some() && id != old.prompt_id {
                            let mut task = previous.clone();
                            let observed = task.context.observed.as_mut()?;
                            if let Some(id) = &id {
                                remember_prompt_id(observed, id);
                            }
                            observed.prompt_id = id;
                            return Some(task);
                        }
                        return None;
                    }
                }
            }
            let mut task =
                if let Some(previous) = previous.filter(|task| task.status != TaskStatus::Done) {
                    previous.clone()
                } else {
                    context.prompt = Some(text.clone());
                    Task {
                        id: next_id,
                        title: crate::agent_detail::bounded_text(&text, 160),
                        status: TaskStatus::Doing,
                        notes: String::new(),
                        context: context.clone(),
                        updated: 0,
                    }
                };
            let summary_only = task.context.prompt.is_none();
            let observed = task
                .context
                .observed
                .get_or_insert_with(|| context.observed.take().unwrap_or_default());
            if observed.session_id.is_none() {
                observed.session_id = context.observed.as_ref().and_then(|v| v.session_id.clone());
            }
            if let Some(id) = &id {
                remember_prompt_id(observed, id);
            }
            // A different unidentified follow-up retires the preceding current
            // turn. Its historical id remains useful for replay deduplication.
            observed.prompt_id = id;
            observed.last_prompt.clone_from(&text);
            if !observed.manual_status {
                task.status = TaskStatus::Doing;
            }
            observed.truncated |= truncated;
            if summary_only {
                task.context.prompt = Some(text.clone());
            } else if task.context.prompt.as_deref() != Some(&text) {
                let prompt = task.context.prompt.get_or_insert_default();
                let addition = format!("\n\nFollow-up:\n{text}");
                let available = 65_536usize.saturating_sub(prompt.len());
                let mut end = addition.len().min(available);
                while !addition.is_char_boundary(end) {
                    end -= 1;
                }
                observed.truncated |= end < addition.len();
                prompt.push_str(&addition[..end]);
            }
            Some(task)
        }
        TaskObservation::Finished { id } | TaskObservation::Interrupted { id } => {
            let task = previous?;
            let observed = task.context.observed.as_ref()?;
            if observed.manual_status
                || task.status == TaskStatus::Done
                || id
                    .as_ref()
                    .is_some_and(|id| Some(id) != observed.prompt_id.as_ref())
            {
                return None;
            }
            let status = if finished {
                TaskStatus::Done
            } else {
                TaskStatus::Blocked
            };
            if task.status == status {
                return None;
            }
            let mut task = task.clone();
            task.status = status;
            Some(task)
        }
    }
}

fn remember_prompt_id(observed: &mut ObservedTask, id: &str) {
    if !observed.prompt_ids.iter().any(|old| old == id) {
        if observed.prompt_ids.len() == 64 {
            observed.prompt_ids.remove(0);
        }
        observed.prompt_ids.push(id.into());
    }
}

/// Observed liveness may block or resume work, but cannot prove completion.
pub fn observe_work_status(task: &Task, status: crate::AgentStatus) -> Option<Task> {
    if task.status == TaskStatus::Done || task.context.observed.as_ref()?.manual_status {
        return None;
    }
    let status = match status {
        crate::AgentStatus::Permission
        | crate::AgentStatus::Question
        | crate::AgentStatus::Error
        | crate::AgentStatus::Exited => TaskStatus::Blocked,
        crate::AgentStatus::Working | crate::AgentStatus::Tool => TaskStatus::Doing,
        _ => return None,
    };
    if task.status == status {
        return None;
    }
    let mut task = task.clone();
    task.status = status;
    Some(task)
}

/// An ordered collection of tasks with monotonically-assigned ids.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "TaskRecords")]
pub struct TaskList {
    tasks: Vec<Task>,
    next_id: u64,
    #[serde(skip)]
    by_id: std::collections::HashMap<u64, usize>,
}

#[derive(Deserialize)]
struct TaskRecords {
    tasks: Vec<Task>,
    next_id: u64,
}

impl From<TaskRecords> for TaskList {
    fn from(records: TaskRecords) -> Self {
        let mut result = Self {
            tasks: records.tasks,
            next_id: records.next_id,
            by_id: Default::default(),
        };
        result.reindex();
        result
    }
}

impl TaskList {
    fn reindex(&mut self) {
        self.by_id.clear();
        self.by_id.extend(
            self.tasks
                .iter()
                .enumerate()
                .map(|(index, task)| (task.id, index)),
        );
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut Task> {
        self.tasks.get_mut(*self.by_id.get(&id)?)
    }

    /// Apply an atomic observed record during live projection or replay.
    pub fn observe(&mut self, task: Task) {
        self.next_id = self.next_id.max(task.id + 1);
        if let Some(previous) = self.get_mut(task.id) {
            *previous = task;
        } else {
            self.by_id.insert(task.id, self.tasks.len());
            self.tasks.push(task);
        }
    }

    /// A human or management API status edit wins over future provider signals.
    pub fn manual_status(&mut self, id: u64) {
        if let Some(observed) = self
            .get_mut(id)
            .and_then(|task| task.context.observed.as_mut())
        {
            observed.manual_status = true;
        }
    }
    pub fn new() -> Self {
        TaskList {
            tasks: Vec::new(),
            next_id: 1,
            by_id: Default::default(),
        }
    }

    /// Transfer only Tasks proven to belong to the exact resumed invocation.
    /// One durable native-resume event supplies both identities atomically.
    pub fn rebind_invocation(&mut self, pane: crate::PaneId, previous: u64, current: u64) -> usize {
        let mut changed = 0;
        for task in &mut self.tasks {
            if task.context.pane == Some(pane) && task.context.invocation == Some(previous) {
                task.context.invocation = Some(current);
                task.updated = current;
                changed += 1;
            }
        }
        changed
    }

    /// Add a task, returning its new id.
    pub fn add(&mut self, title: &str, status: TaskStatus) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.by_id.insert(id, self.tasks.len());
        self.tasks.push(Task {
            id,
            title: title.to_string(),
            status,
            notes: String::new(),
            context: TaskContext::default(),
            updated: id,
        });
        id
    }

    /// Insert a task with an explicit id (used when projecting the event log).
    pub fn insert(&mut self, id: u64, title: &str, status: TaskStatus) {
        self.next_id = self.next_id.max(id + 1);
        if let Some(t) = self.get_mut(id) {
            t.title = title.to_string();
            t.status = status;
        } else {
            self.by_id.insert(id, self.tasks.len());
            self.tasks.push(Task {
                id,
                title: title.to_string(),
                status,
                notes: String::new(),
                context: TaskContext::default(),
                updated: id,
            });
        }
    }

    /// Update a task's status; returns whether it existed.
    pub fn set_status(&mut self, id: u64, status: TaskStatus) -> bool {
        if let Some(t) = self.get_mut(id) {
            t.status = status;
            true
        } else {
            false
        }
    }

    /// Rename a task; returns whether it existed.
    pub fn set_title(&mut self, id: u64, title: &str) -> bool {
        if let Some(t) = self.get_mut(id) {
            t.title = title.to_string();
            true
        } else {
            false
        }
    }

    /// Apply event-backed descriptions and launch ownership.
    pub fn set_details(&mut self, id: u64, notes: &str, context: TaskContext) -> bool {
        if let Some(task) = self.get_mut(id) {
            task.notes = notes.to_string();
            task.context = context;
            true
        } else {
            false
        }
    }

    /// Event sequence gives a stable recency ordering without clock polling.
    pub fn touch(&mut self, id: u64, sequence: u64) {
        if let Some(task) = self.get_mut(id) {
            task.updated = sequence;
        }
    }

    /// Reserve identity without publishing an unlogged projection.
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Delete a task; returns whether it existed.
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|t| t.id != id);
        let changed = self.tasks.len() != before;
        if changed {
            self.reindex();
        }
        changed
    }

    pub fn get(&self, id: u64) -> Option<&Task> {
        self.tasks.get(*self.by_id.get(&id)?)
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Visit metadata in stable creation order without cloning private prompt text.
    pub fn records(&self) -> impl Iterator<Item = &Task> {
        self.tasks.iter()
    }

    /// Tasks ordered active-first (Doing, Blocked, Todo, Done), ties by id.
    pub fn ordered(&self) -> Vec<&Task> {
        let mut v: Vec<&Task> = self.tasks.iter().collect();
        v.sort_by_key(|t| (t.status.order(), t.id));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed_context() -> TaskContext {
        TaskContext {
            pane: Some(crate::PaneId(1)),
            invocation: Some(7),
            observed: Some(ObservedTask {
                session_id: Some("session".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn direct_work_groups_followups_deduplicates_and_requires_explicit_completion() {
        let prompt = |id: &str, text: &str| TaskObservation::Prompt {
            id: Some(id.into()),
            text: text.into(),
        };
        let first =
            observe_direct_work(None, 1, observed_context(), prompt("one", "Fix login")).unwrap();
        assert_eq!(first.context.prompt.as_deref(), Some("Fix login"));
        assert!(observe_direct_work(
            Some(&first),
            2,
            observed_context(),
            prompt("one", "Fix login")
        )
        .is_none());
        assert!(observe_work_status(&first, crate::AgentStatus::Idle).is_none());
        let blocked = observe_work_status(&first, crate::AgentStatus::Question).unwrap();
        let followup = observe_direct_work(
            Some(&blocked),
            2,
            observed_context(),
            prompt("two", "Use OAuth"),
        )
        .unwrap();
        assert_eq!(followup.id, 1);
        assert_eq!(followup.status, TaskStatus::Doing);
        assert!(followup
            .context
            .prompt
            .as_ref()
            .unwrap()
            .contains("Use OAuth"));
        assert!(observe_direct_work(
            Some(&followup),
            2,
            observed_context(),
            TaskObservation::Finished {
                id: Some("one".into())
            }
        )
        .is_none());
        let finished = observe_direct_work(
            Some(&followup),
            2,
            observed_context(),
            TaskObservation::Finished {
                id: Some("two".into()),
            },
        )
        .unwrap();
        assert_eq!(finished.status, TaskStatus::Done);
        let next = observe_direct_work(
            Some(&finished),
            2,
            observed_context(),
            prompt("three", "Write docs"),
        )
        .unwrap();
        assert_eq!(next.id, 2);
        assert!(observe_work_status(&finished, crate::AgentStatus::Exited).is_none());
    }

    #[test]
    fn unidentified_followup_retires_old_turn_until_native_identity_arrives() {
        let first = observe_direct_work(
            None,
            1,
            observed_context(),
            TaskObservation::Prompt {
                id: Some("turn-A".into()),
                text: "Fix login".into(),
            },
        )
        .unwrap();
        let followup = observe_direct_work(
            Some(&first),
            2,
            observed_context(),
            TaskObservation::Prompt {
                id: None,
                text: "Also fix refresh".into(),
            },
        )
        .unwrap();
        let observed = followup.context.observed.as_ref().unwrap();
        assert_eq!(observed.prompt_id, None);
        assert_eq!(observed.prompt_ids, ["turn-A"]);
        for observation in [
            TaskObservation::Finished {
                id: Some("turn-A".into()),
            },
            TaskObservation::Interrupted {
                id: Some("turn-A".into()),
            },
        ] {
            assert!(
                observe_direct_work(Some(&followup), 2, observed_context(), observation).is_none()
            );
        }
        let enriched = observe_direct_work(
            Some(&followup),
            2,
            observed_context(),
            TaskObservation::Prompt {
                id: Some("turn-B".into()),
                text: "Also fix refresh".into(),
            },
        )
        .unwrap();
        assert_eq!(enriched.id, first.id);
        assert_eq!(enriched.status, TaskStatus::Doing);
        assert_eq!(enriched.context.prompt, followup.context.prompt);
        assert_eq!(
            enriched.context.observed.as_ref().unwrap().prompt_ids,
            ["turn-A", "turn-B"]
        );
        // Providers without turn identifiers can still finish their current work.
        assert_eq!(
            observe_direct_work(
                Some(&followup),
                2,
                observed_context(),
                TaskObservation::Finished { id: None }
            )
            .unwrap()
            .status,
            TaskStatus::Done
        );
        assert_eq!(
            observe_direct_work(
                Some(&enriched),
                2,
                observed_context(),
                TaskObservation::Finished {
                    id: Some("turn-B".into())
                }
            )
            .unwrap()
            .status,
            TaskStatus::Done
        );
    }

    #[test]
    fn direct_work_preserves_manual_choices_and_bounds_retained_text() {
        let mut list = TaskList::new();
        let task = observe_direct_work(
            None,
            1,
            observed_context(),
            TaskObservation::Prompt {
                id: None,
                text: "Fix \u{1b}login".into(),
            },
        )
        .unwrap();
        assert_eq!(task.context.prompt.as_deref(), Some("Fix login"));
        list.observe(task);
        list.manual_status(1);
        list.set_status(1, TaskStatus::Todo);
        assert!(observe_work_status(list.get(1).unwrap(), crate::AgentStatus::Working).is_none());
        assert!(observe_direct_work(
            list.get(1),
            2,
            observed_context(),
            TaskObservation::Finished { id: None }
        )
        .is_none());
        let mut previous = list.get(1).unwrap().clone();
        for id in 0..10 {
            previous = observe_direct_work(
                Some(&previous),
                2,
                observed_context(),
                TaskObservation::Prompt {
                    id: Some(id.to_string()),
                    text: format!("{id}{}", "界".repeat(6_000)),
                },
            )
            .unwrap();
        }
        assert!(previous.context.prompt.as_ref().unwrap().len() <= 65_536);
        assert!(
            previous
                .context
                .observed
                .as_ref()
                .unwrap()
                .last_prompt
                .len()
                <= 16_384
        );
        assert!(previous.context.observed.unwrap().truncated);
        assert_eq!(previous.status, TaskStatus::Todo);
    }

    #[test]
    fn native_rebind_changes_only_exact_owner_and_is_idempotent() {
        let mut tasks = TaskList::new();
        for (pane, invocation) in [(1, 7), (1, 6), (2, 7)] {
            let id = tasks.add("task", TaskStatus::Doing);
            tasks.set_details(
                id,
                "retained",
                TaskContext {
                    pane: Some(crate::PaneId(pane)),
                    invocation: Some(invocation),
                    ..Default::default()
                },
            );
        }
        assert_eq!(tasks.rebind_invocation(crate::PaneId(1), 7, 10), 1);
        assert_eq!(tasks.rebind_invocation(crate::PaneId(1), 7, 10), 0);
        assert_eq!(tasks.get(1).unwrap().context.invocation, Some(10));
        assert_eq!(tasks.get(1).unwrap().updated, 10);
        assert_eq!(tasks.get(1).unwrap().notes, "retained");
        assert_eq!(tasks.get(2).unwrap().context.invocation, Some(6));
        assert_eq!(tasks.get(3).unwrap().context.invocation, Some(7));
    }

    #[test]
    fn add_get_and_status_transitions() {
        let mut l = TaskList::new();
        let a = l.add("fix the parser", TaskStatus::Doing);
        let b = l.add("write docs", TaskStatus::Todo);
        assert_eq!(l.len(), 2);
        assert_eq!(l.get(a).unwrap().title, "fix the parser");
        assert!(l.set_status(b, TaskStatus::Done));
        assert_eq!(l.get(b).unwrap().status, TaskStatus::Done);
        assert!(!l.set_status(999, TaskStatus::Done)); // missing id
    }

    #[test]
    fn ordered_puts_active_first_done_last() {
        let mut l = TaskList::new();
        l.add("done one", TaskStatus::Done);
        l.add("doing one", TaskStatus::Doing);
        l.add("todo one", TaskStatus::Todo);
        l.add("blocked one", TaskStatus::Blocked);
        let order: Vec<&str> = l.ordered().iter().map(|t| t.title.as_str()).collect();
        assert_eq!(order, ["doing one", "blocked one", "todo one", "done one"]);
    }

    #[test]
    fn insert_projects_without_duplicating() {
        // Simulating replay of the event log: create then status-change.
        let mut l = TaskList::new();
        l.insert(5, "ship it", TaskStatus::Doing);
        l.insert(5, "ship it", TaskStatus::Done); // same id -> update, not dup
        assert_eq!(l.len(), 1);
        assert_eq!(l.get(5).unwrap().status, TaskStatus::Done);
        // next add must not collide with the projected id.
        let n = l.add("next", TaskStatus::Todo);
        assert!(n > 5);
    }
}
