//! Explicit invocation and provider-session ancestry for the native Graph.
use super::{TimelineEntry, TimelineKind, TimelineSpan};
use crate::{PaneId, ProjectId};
use std::collections::{BTreeMap, HashMap};

/// A session can be resumed by multiple invocations; neither Pane nor session
/// alone is a safe identity for a historical focus target.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentGraphIdentity {
    pub invocation: Option<u64>,
    pub provider: String,
    pub session: Option<String>,
    pub project: Option<ProjectId>,
    pub pane: Option<PaneId>,
}

/// One observed agent, with explicit ancestry and bounded source observations.
/// Parent indexes precede children even when hostile evidence contains cycles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentGraphNode {
    pub identity: AgentGraphIdentity,
    pub span: TimelineSpan,
    pub parent: Option<usize>,
    pub depth: usize,
    pub parent_missing: bool,
    pub status: Option<String>,
    pub evidence: Vec<TimelineEntry>,
}

/// Reduce a bounded timeline page plus carryovers, deduplicating event sequences.
/// Shared directories, providers, Pane reuse, idle and silence create no edges
/// or completion claims. Ambiguous session parents remain unresolved roots.
pub fn agent_graph(entries: &[TimelineEntry]) -> Vec<AgentGraphNode> {
    let ordered: BTreeMap<_, _> = entries
        .iter()
        .filter(|e| e.kind == TimelineKind::Agent && !e.provider.is_empty())
        .map(|e| (e.sequence, e))
        .collect();
    let mut nodes: Vec<AgentGraphNode> = Vec::new();
    let mut identities = HashMap::new();
    let mut invocations: HashMap<(String, u64), usize> = HashMap::new();
    for entry in ordered.into_values() {
        let mut identity = AgentGraphIdentity {
            invocation: entry.invocation,
            provider: entry.provider.clone(),
            session: entry.session_id.clone(),
            project: entry.context.project,
            pane: entry.context.pane,
        };
        // A start preceding session discovery belongs to that same invocation.
        // A later distinct session gets its own node and focus guard.
        let previous = entry
            .invocation
            .and_then(|id| invocations.get(&(entry.provider.clone(), id)).copied());
        let existing = identities.get(&identity).copied().or_else(|| {
            previous.filter(|&index| {
                nodes[index].identity.session.is_none() || identity.session.is_none()
            })
        });
        let index = if let Some(index) = existing {
            if identity.session.is_none() {
                identity.session.clone_from(&nodes[index].identity.session);
            }
            identities.remove(&nodes[index].identity);
            nodes[index].identity = identity.clone();
            identities.insert(identity, index);
            index
        } else {
            let index = nodes.len();
            identities.insert(identity.clone(), index);
            nodes.push(AgentGraphNode {
                identity,
                span: TimelineSpan {
                    first_ms: entry.timestamp_ms,
                    last: entry.clone(),
                    observations: 0,
                },
                parent: None,
                depth: 0,
                parent_missing: false,
                status: None,
                evidence: Vec::new(),
            });
            index
        };
        if let Some(id) = entry.invocation {
            invocations.insert((entry.provider.clone(), id), index);
        }
        let node = &mut nodes[index];
        let parent_invocation = node.span.last.parent_invocation;
        let parent_session = node.span.last.parent_session_id.clone();
        let context = node.span.last.context.clone();
        node.span.first_ms = node.span.first_ms.min(entry.timestamp_ms);
        node.span.last = entry.clone();
        node.span.last.session_id.clone_from(&node.identity.session);
        node.span.last.parent_invocation = entry.parent_invocation.or(parent_invocation);
        node.span.last.parent_session_id = entry.parent_session_id.clone().or(parent_session);
        if node.span.last.context.project.is_none() {
            node.span.last.context = context;
        }
        node.span.observations += 1;
        if matches!(
            entry.summary.as_str(),
            "Starting"
                | "Working"
                | "Tool"
                | "Permission"
                | "Question"
                | "Idle"
                | "Error"
                | "Exited"
                | "Ended"
        ) {
            node.status = Some(entry.summary.clone());
        }
        node.evidence.push(entry.clone());
    }
    // Preserve ambiguity rather than choosing a resumed incarnation arbitrarily.
    let mut invocation_owners: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut sessions = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        if let Some(id) = node.identity.invocation {
            invocation_owners.entry(id).or_default().push(index);
        }
        if let Some(session) = &node.identity.session {
            sessions
                .entry((
                    node.identity.provider.clone(),
                    node.identity.project,
                    session.clone(),
                ))
                .or_insert_with(Vec::new)
                .push(index);
        }
    }
    let mut children = vec![Vec::new(); nodes.len()];
    let mut roots = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let entry = &node.span.last;
        let candidates = if let Some(parent) = entry.parent_invocation {
            invocation_owners.get(&parent)
        } else {
            entry.parent_session_id.as_ref().and_then(|id| {
                sessions.get(&(entry.provider.clone(), node.identity.project, id.clone()))
            })
        };
        let parent = candidates
            .filter(|ids| ids.len() == 1)
            .map(|ids| ids[0])
            .filter(|parent| *parent != index);
        if let Some(parent) = parent {
            children[parent].push(index);
        } else {
            roots.push(index);
        }
    }
    let mut result = Vec::with_capacity(nodes.len());
    let mut seen = vec![false; nodes.len()];
    for root in roots.into_iter().chain(0..nodes.len()) {
        let mut pending = vec![(root, None, 0)];
        while let Some((index, parent, depth)) = pending.pop() {
            if std::mem::replace(&mut seen[index], true) {
                continue;
            }
            let mut node = nodes[index].clone();
            node.parent = parent;
            node.depth = depth;
            node.parent_missing = parent.is_none()
                && (node.span.last.parent_invocation.is_some()
                    || node.span.last.parent_session_id.is_some());
            let output = result.len();
            result.push(node);
            pending.extend(
                children[index]
                    .iter()
                    .rev()
                    .map(|child| (*child, Some(output), depth + 1)),
            );
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::TimelineContext;

    fn entry(
        sequence: u64,
        invocation: Option<u64>,
        provider: &str,
        session: Option<&str>,
    ) -> TimelineEntry {
        TimelineEntry {
            sequence,
            timestamp_ms: sequence * 100,
            invocation,
            context: TimelineContext {
                project: Some(ProjectId(1)),
                pane: Some(PaneId(1)),
                ..TimelineContext::default()
            },
            provider: provider.into(),
            session_id: session.map(str::to_string),
            parent_invocation: None,
            parent_session_id: None,
            kind: TimelineKind::Agent,
            work_summary: None,
            summary: "Idle".into(),
        }
    }

    #[test]
    fn graph_keeps_exact_invocations_sessions_status_and_deduplicated_evidence() {
        let mut identified = entry(2, Some(1), "provider-a", Some("session"));
        identified.summary = "Session identified".into();
        let start = entry(1, Some(1), "provider-a", None);
        let resumed = entry(3, Some(3), "provider-a", Some("session"));
        let changed = entry(4, Some(3), "provider-a", Some("new-session"));
        let nodes = agent_graph(&[resumed, changed, identified.clone(), start, identified]);
        assert_eq!(
            nodes.len(),
            3,
            "resumed and replaced sessions must not collapse"
        );
        assert_eq!(nodes[0].identity.invocation, Some(1));
        assert_eq!(nodes[0].identity.session.as_deref(), Some("session"));
        assert_eq!(nodes[0].evidence.len(), 2);
        assert_eq!(nodes[0].span.first_ms, 100);
        assert_eq!(nodes[0].status.as_deref(), Some("Idle"));
        assert!(
            nodes.iter().all(|n| n.parent.is_none()),
            "shared Pane/session is not ancestry"
        );
    }

    #[test]
    fn graph_combines_explicit_cross_provider_launches_and_native_children_only() {
        for provider in [
            "codex",
            "claude",
            "opencode",
            "pi",
            "grok",
            "custom-provider",
        ] {
            let root = entry(1, Some(1), provider, Some("root"));
            let mut native = entry(2, None, provider, Some("child"));
            native.parent_session_id = Some("root".into());
            let mut launch = entry(3, Some(3), "another-provider", Some("launched"));
            launch.parent_invocation = Some(1);
            launch.context.project = Some(ProjectId(2));
            let unrelated = entry(4, Some(4), provider, Some("unrelated"));
            let nodes = agent_graph(&[root, native, launch, unrelated]);
            assert_eq!(
                nodes.iter().map(|n| n.parent).collect::<Vec<_>>(),
                [None, Some(0), Some(0), None]
            );
            assert_eq!(
                nodes[1].identity.invocation, None,
                "child cannot inherit root focus/liveness"
            );
            assert_eq!(
                nodes[2].identity.project,
                Some(ProjectId(2)),
                "explicit cross-project launch is valid"
            );
        }
    }

    #[test]
    fn graph_ambiguous_foreign_missing_self_and_cyclic_parents_are_bounded() {
        let root = entry(1, Some(1), "a", Some("root"));
        let resumed = entry(2, Some(2), "a", Some("root"));
        let mut child = entry(3, None, "a", Some("child"));
        child.parent_session_id = Some("root".into());
        let mut foreign = entry(4, None, "b", Some("foreign"));
        foreign.parent_session_id = Some("root".into());
        let mut cycle_a = entry(5, Some(5), "a", Some("cycle-a"));
        cycle_a.parent_invocation = Some(6);
        let mut cycle_b = entry(6, Some(6), "b", Some("cycle-b"));
        cycle_b.parent_invocation = Some(5);
        let mut own = entry(7, Some(7), "a", Some("self"));
        own.parent_invocation = Some(7);
        let mut missing = entry(8, Some(8), "a", Some("missing"));
        missing.parent_invocation = Some(999);
        let nodes = agent_graph(&[
            root, resumed, child, foreign, cycle_a, cycle_b, own, missing,
        ]);
        assert_eq!(nodes.len(), 8);
        assert_eq!(nodes.iter().filter(|n| n.parent_missing).count(), 5);
        for (index, node) in nodes.iter().enumerate() {
            assert!(node.parent.is_none_or(|parent| parent < index));
        }
        assert_eq!(
            nodes.iter().filter(|n| n.parent.is_some()).count(),
            1,
            "only cycle's observed forward edge survives"
        );
    }
}
