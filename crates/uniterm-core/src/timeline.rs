//! Bounded, evidence-based day projections shared by the UI and automation.
//! A selected Project is a visit, not a claim about human effort (docs/27).

use crate::{PaneId, ProjectId};
use serde::{Deserialize, Serialize};

/// Historical context is copied at observation time, never resolved from a
/// mutable Tab ordinal when the user later inspects or follows the event.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineContext {
    pub project: Option<ProjectId>,
    pub project_name: String,
    pub root: String,
    pub branch: String,
    pub tab: String,
    pub pane: Option<PaneId>,
}

/// One meaningful observation, identified by its authoritative log sequence.
/// Sensitive terminal output and arbitrary input bytes are never projected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub context: TimelineContext,
    /// The start event's sequence identifies an invocation even after PID reuse.
    pub invocation: Option<u64>,
    pub provider: String,
    pub session_id: Option<String>,
    /// Provider-reported ancestry, never inferred from a shared directory or Pane.
    #[serde(default)]
    pub parent_session_id: Option<String>,
    pub kind: TimelineKind,
    pub summary: String,
}

/// Visits and annotations must not be confused with observed agent activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineKind {
    Visit,
    Agent,
    Note,
    Prompt,
    Boundary,
}

/// A civil day's UTC bounds come from the runtime's timezone conversion.
/// Cursors page raw observations so dense days never require unbounded memory.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineDay {
    pub date: String,
    pub previous: String,
    pub next: String,
    pub timezone: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub current_sequence: u64,
    pub entries: Vec<TimelineEntry>,
    /// Open invocations and observed descendants omitted from the current page,
    /// including overnight carryover. This does not assert child liveness or duration.
    #[serde(default)]
    pub active_agents: Vec<TimelineEntry>,
    #[serde(default)]
    pub active_agents_truncated: bool,
    pub next_before: Option<u64>,
    pub partial_history: bool,
    /// All touched Projects in this day, independently of observation paging.
    pub projects: Vec<TimelineContext>,
    /// Extremely large days explicitly report a capped Project summary.
    pub projects_truncated: bool,
    /// Server-rendered local times keep timezone conversion outside the core.
    pub times: Vec<String>,
    pub axis: Vec<String>,
}

/// One visual lane segment groups observations belonging to the same invocation.
/// Its end is the last observation, not a guessed completion time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineSpan {
    pub first_ms: u64,
    pub last: TimelineEntry,
    pub observations: usize,
}

/// Group only within one Project and invocation; visits and notes stay discrete.
/// Input is bounded by the day query, and sequence order wins over clock changes.
pub fn spans(entries: &[TimelineEntry]) -> Vec<TimelineSpan> {
    let mut result: Vec<TimelineSpan> = Vec::new();
    let mut invocations = std::collections::HashMap::new();
    for entry in entries {
        if let Some(invocation) = entry
            .invocation
            .filter(|_| entry.kind == TimelineKind::Agent)
        {
            if let Some(&index) = invocations.get(&invocation) {
                let span: &mut TimelineSpan = &mut result[index];
                let same_session = span.last.session_id.is_none()
                    || entry.session_id.is_none()
                    || span.last.session_id == entry.session_id;
                if same_session
                    && (span.last.context.project.is_none()
                        || entry.context.project.is_none()
                        || span.last.context.project == entry.context.project)
                {
                    let context = span.last.context.clone();
                    span.last = entry.clone();
                    if span.last.context.project.is_none() {
                        span.last.context = context;
                    }
                    span.observations += 1;
                    continue;
                }
            }
            invocations.insert(invocation, result.len());
        }
        result.push(TimelineSpan {
            first_ms: entry.timestamp_ms,
            last: entry.clone(),
            observations: 1,
        });
    }
    result
}

/// A bounded relationship row. Missing parents stay visible as uncertain roots,
/// because the selected day or page may exclude their observations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTreeRow {
    pub span: TimelineSpan,
    pub depth: usize,
    pub parent_missing: bool,
}

/// Build a provider-scoped forest from explicit session ancestry only.
/// Work is linear in the bounded input, and cyclic provider data cannot recurse.
pub fn agent_tree(entries: &[TimelineEntry]) -> Vec<AgentTreeRow> {
    let mut nodes: Vec<TimelineSpan> = Vec::new();
    let mut sessions = std::collections::HashMap::new();
    let mut invocations = std::collections::HashMap::new();
    for span in spans(entries) {
        let entry = &span.last;
        if entry.kind != TimelineKind::Agent || entry.provider.is_empty() {
            continue;
        }
        let key = entry
            .session_id
            .as_ref()
            .map(|id| (entry.provider.clone(), id.clone()));
        let existing = key
            .as_ref()
            .and_then(|key| sessions.get(key))
            .copied()
            .or_else(|| {
                entry
                    .invocation
                    .and_then(|id| invocations.get(&id).copied())
                    .filter(|index: &usize| {
                        nodes[*index].last.session_id.is_none() || entry.session_id.is_none()
                    })
            });
        if let Some(index) = existing {
            let node: &mut TimelineSpan = &mut nodes[index];
            let parent = node
                .last
                .parent_session_id
                .clone()
                .or_else(|| span.last.parent_session_id.clone());
            node.observations += span.observations;
            node.first_ms = node.first_ms.min(span.first_ms);
            if span.last.sequence > node.last.sequence {
                node.last = span.last;
            }
            if node.last.parent_session_id.is_none() {
                node.last.parent_session_id = parent;
            }
            if let Some(key) = key {
                sessions.insert(key, index);
            }
        } else {
            let index = nodes.len();
            if let Some(key) = key {
                sessions.insert(key, index);
            }
            if let Some(id) = entry.invocation {
                invocations.insert(id, index);
            }
            nodes.push(span);
        }
    }
    let mut children = vec![Vec::new(); nodes.len()];
    let mut roots = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let parent = node
            .last
            .parent_session_id
            .as_ref()
            .and_then(|id| sessions.get(&(node.last.provider.clone(), id.clone())))
            .copied()
            .filter(|parent| *parent != index);
        if let Some(parent) = parent {
            children[parent].push(index);
        } else {
            roots.push(index);
        }
    }
    let mut result = Vec::with_capacity(nodes.len());
    let mut visited = vec![false; nodes.len()];
    // Remaining nodes cover cycles without discarding evidence or hanging UI.
    for root in roots.into_iter().chain(0..nodes.len()) {
        let mut stack = vec![(root, 0)];
        while let Some((index, depth)) = stack.pop() {
            if std::mem::replace(&mut visited[index], true) {
                continue;
            }
            result.push(AgentTreeRow {
                span: nodes[index].clone(),
                depth,
                parent_missing: depth == 0 && nodes[index].last.parent_session_id.is_some(),
            });
            stack.extend(
                children[index]
                    .iter()
                    .rev()
                    .map(|child| (*child, depth + 1)),
            );
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sequence: u64, invocation: Option<u64>) -> TimelineEntry {
        TimelineEntry {
            sequence,
            timestamp_ms: sequence * 1000,
            context: TimelineContext {
                project: Some(ProjectId(1)),
                pane: Some(PaneId(1)),
                ..Default::default()
            },
            invocation,
            provider: "test".into(),
            session_id: None,
            parent_session_id: None,
            kind: TimelineKind::Agent,
            summary: "working".into(),
        }
    }

    #[test]
    fn same_pane_new_invocation_and_visits_remain_distinct() {
        let rows = spans(&[
            entry(1, Some(1)),
            entry(2, Some(1)),
            entry(3, Some(3)),
            entry(4, None),
        ]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].observations, 2);
        assert_eq!(rows[0].last.sequence, 2);
        assert_eq!(rows[1].last.invocation, Some(3));
        assert!(spans(&[]).is_empty());
    }

    #[test]
    fn sequence_order_survives_clock_rollback_and_projects_do_not_merge() {
        let first = entry(1, Some(1));
        let mut later = entry(2, Some(1));
        later.timestamp_ms = 0;
        let mut foreign = entry(3, Some(1));
        foreign.context.project = Some(ProjectId(2));
        let rows = spans(&[first, later, foreign]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].last.sequence, 2);
        assert_eq!(rows[0].first_ms, 1000);
    }

    #[test]
    fn ancestry_is_provider_scoped_and_preserves_children_without_panes() {
        let mut root = entry(1, Some(1));
        root.session_id = Some("root".into());
        let mut child = entry(2, None);
        child.session_id = Some("child".into());
        child.parent_session_id = Some("root".into());
        let mut grandchild = entry(3, None);
        grandchild.session_id = Some("grandchild".into());
        grandchild.parent_session_id = Some("child".into());
        let mut foreign = child.clone();
        foreign.sequence = 4;
        foreign.provider = "another".into();
        let rows = agent_tree(&[grandchild, child.clone(), root, foreign, child]);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter().map(|row| row.depth).collect::<Vec<_>>(),
            [0, 1, 2, 0]
        );
        assert_eq!(rows[0].span.last.session_id.as_deref(), Some("root"));
        assert_eq!(rows[1].span.observations, 2);
        assert!(rows[3].parent_missing);
        assert!(agent_tree(&[]).is_empty());
    }

    #[test]
    fn cycles_and_missing_ancestry_keep_every_session_once() {
        let mut a = entry(1, None);
        a.session_id = Some("a".into());
        a.parent_session_id = Some("b".into());
        let mut b = entry(2, None);
        b.session_id = Some("b".into());
        b.parent_session_id = Some("a".into());
        let rows = agent_tree(&[a.clone(), b]);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].parent_missing);
        assert_eq!(rows[1].depth, 1);
        let rows = agent_tree(&[a]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].parent_missing);
    }

    #[test]
    fn interleaved_invocations_of_one_session_keep_the_latest_observation() {
        let mut first = entry(1, Some(1));
        first.session_id = Some("shared".into());
        let mut second = entry(2, Some(2));
        second.session_id = Some("shared".into());
        let mut latest = first.clone();
        latest.sequence = 10;
        let rows = agent_tree(&[first, second, latest]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].span.last.sequence, 10);
        assert_eq!(rows[0].span.observations, 3);
    }

    #[test]
    fn earlier_ancestry_survives_later_statuses_from_a_childs_own_pane() {
        let mut root = entry(1, Some(1));
        root.session_id = Some("root".into());
        let mut child = entry(2, Some(2));
        child.session_id = Some("child".into());
        let mut link = child.clone();
        link.sequence = 3;
        link.invocation = None;
        link.parent_session_id = Some("root".into());
        let mut latest = child.clone();
        latest.sequence = 100;
        let rows = agent_tree(&[root, child, link, latest]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].span.last.sequence, 100);
        assert_eq!(rows[1].span.last.parent_session_id.as_deref(), Some("root"));
        assert_eq!(rows[1].depth, 1);
    }
}
