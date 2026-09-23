//! Workspace-scoped human-attention queue.
//!
//! This is pure projection logic. Detection, pane input, workflow advancement,
//! persistence, and UI live outside core. See `docs/08-observatory.md`.

use serde::{Deserialize, Serialize};

use crate::{AgentStatus, PaneId};

/// Why an item requires a human.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitingKind {
    Permission,
    Question,
    Workflow,
    Relay,
}

impl WaitingKind {
    /// Map only genuine human-blocking agent states into the queue.
    pub fn from_agent_status(status: AgentStatus) -> Option<Self> {
        match status {
            AgentStatus::Permission => Some(Self::Permission),
            AgentStatus::Question => Some(Self::Question),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Workflow => "workflow",
            Self::Relay => "relay",
        }
    }
}

/// One active item in the Workspace waiting queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingItem {
    pub id: u64,
    pub pane: PaneId,
    /// Foreground process-group identity when known. Reusing a Pane id for a
    /// later invocation must never inherit an earlier prompt.
    pub invocation: Option<i32>,
    pub kind: WaitingKind,
    pub summary: String,
}

/// Durable reason an active waiting item left the projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitingResolution {
    Answered,
    Dismissed,
    Stopped,
    AgentAdvanced,
    PaneClosed,
    Resumed,
}

/// Result of reconciling one pane's observed state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitingChange {
    None,
    Created(WaitingItem),
    Replaced {
        resolved: u64,
        created: WaitingItem,
    },
    Resolved {
        id: u64,
        resolution: WaitingResolution,
    },
}

/// Active waiting items with monotonic ids.
#[derive(Clone, Debug, Default)]
pub struct WaitingQueue {
    items: Vec<WaitingItem>,
    next_id: u64,
}

impl WaitingQueue {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
        }
    }

    /// Reconcile a smoothed agent status for one invocation.
    pub fn observe_agent(
        &mut self,
        pane: PaneId,
        invocation: Option<i32>,
        status: AgentStatus,
        summary: &str,
    ) -> WaitingChange {
        let kind = WaitingKind::from_agent_status(status);
        let existing = self.items.iter().position(|item| item.pane == pane);
        match (existing, kind) {
            (Some(index), Some(kind)) => {
                let item = &mut self.items[index];
                if item.invocation == invocation && item.kind == kind {
                    item.summary = bounded_summary(summary);
                    WaitingChange::None
                } else {
                    let old = self.items.remove(index);
                    let item = self.create(pane, invocation, kind, summary);
                    self.items.push(item.clone());
                    WaitingChange::Replaced {
                        resolved: old.id,
                        created: item,
                    }
                }
            }
            (None, Some(kind)) => {
                let item = self.create(pane, invocation, kind, summary);
                self.items.push(item.clone());
                WaitingChange::Created(item)
            }
            (Some(index), None) => {
                let item = self.items.remove(index);
                WaitingChange::Resolved {
                    id: item.id,
                    resolution: WaitingResolution::AgentAdvanced,
                }
            }
            (None, None) => WaitingChange::None,
        }
    }

    /// Create an explicit workflow, relay, or runtime request for a human.
    pub fn request(
        &mut self,
        pane: PaneId,
        invocation: Option<i32>,
        kind: WaitingKind,
        summary: &str,
    ) -> WaitingChange {
        let existing = self.items.iter().position(|item| item.pane == pane);
        let item = self.create(pane, invocation, kind, summary);
        if let Some(index) = existing {
            let old = self.items.remove(index);
            self.items.push(item.clone());
            WaitingChange::Replaced {
                resolved: old.id,
                created: item,
            }
        } else {
            self.items.push(item.clone());
            WaitingChange::Created(item)
        }
    }

    fn create(
        &mut self,
        pane: PaneId,
        invocation: Option<i32>,
        kind: WaitingKind,
        summary: &str,
    ) -> WaitingItem {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        WaitingItem {
            id,
            pane,
            invocation,
            kind,
            summary: bounded_summary(summary),
        }
    }

    /// Insert an event-log item with its original id.
    pub fn insert(&mut self, item: WaitingItem) {
        self.next_id = self.next_id.max(item.id.saturating_add(1));
        self.items
            .retain(|current| current.id != item.id && current.pane != item.pane);
        self.items.push(item);
    }

    /// Resolve one item and return its owning pane.
    pub fn resolve(&mut self, id: u64) -> Option<WaitingItem> {
        let index = self.items.iter().position(|item| item.id == id)?;
        Some(self.items.remove(index))
    }

    /// Resolve any item tied to a pane that is being destroyed.
    pub fn resolve_pane(&mut self, pane: PaneId) -> Option<WaitingItem> {
        let index = self.items.iter().position(|item| item.pane == pane)?;
        Some(self.items.remove(index))
    }

    /// Resolve an orchestration item without consuming an unrelated agent
    /// permission prompt that happens to share the Pane.
    pub fn resolve_pane_kind(&mut self, pane: PaneId, kind: WaitingKind) -> Option<WaitingItem> {
        let index = self
            .items
            .iter()
            .position(|item| item.pane == pane && item.kind == kind)?;
        Some(self.items.remove(index))
    }

    pub fn get(&self, id: u64) -> Option<&WaitingItem> {
        self.items.iter().find(|item| item.id == id)
    }

    pub fn items(&self) -> &[WaitingItem] {
        &self.items
    }
}

fn bounded_summary(summary: &str) -> String {
    summary.chars().take(2_048).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_state_creates_once_and_advancing_resolves() {
        let mut queue = WaitingQueue::new();
        let created =
            queue.observe_agent(PaneId(7), Some(44), AgentStatus::Permission, "approve tool");
        assert!(matches!(created, WaitingChange::Created(_)));
        assert_eq!(queue.items().len(), 1);
        assert_eq!(
            queue.observe_agent(PaneId(7), Some(44), AgentStatus::Permission, "approve tool",),
            WaitingChange::None
        );
        assert!(matches!(
            queue.observe_agent(PaneId(7), Some(44), AgentStatus::Working, "working"),
            WaitingChange::Resolved {
                resolution: WaitingResolution::AgentAdvanced,
                ..
            }
        ));
        assert!(queue.items().is_empty());
    }

    #[test]
    fn reused_pane_does_not_inherit_an_old_invocation() {
        let mut queue = WaitingQueue::new();
        let first = match queue.observe_agent(PaneId(1), Some(10), AgentStatus::Question, "first") {
            WaitingChange::Created(item) => item,
            other => panic!("unexpected {other:?}"),
        };
        let second = match queue.observe_agent(PaneId(1), Some(11), AgentStatus::Question, "second")
        {
            WaitingChange::Replaced { created, .. } => created,
            other => panic!("unexpected {other:?}"),
        };
        assert_ne!(first.id, second.id);
        assert_eq!(queue.items(), &[second]);
    }

    #[test]
    fn replayed_ids_advance_the_allocator() {
        let mut queue = WaitingQueue::new();
        queue.insert(WaitingItem {
            id: 99,
            pane: PaneId(2),
            invocation: None,
            kind: WaitingKind::Workflow,
            summary: "blocked".into(),
        });
        let created = queue.observe_agent(PaneId(3), None, AgentStatus::Permission, "permission");
        assert!(matches!(
            created,
            WaitingChange::Created(WaitingItem { id: 100, .. })
        ));
    }
}
