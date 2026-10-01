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
                if span.last.context.project.is_none()
                    || entry.context.project.is_none()
                    || span.last.context.project == entry.context.project
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
}
