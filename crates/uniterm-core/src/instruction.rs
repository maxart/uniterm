//! Pure Workspace-scoped instruction queue projection.
//!
//! Instructions are human direction waiting to reach one active agent
//! invocation. They are deliberately distinct from waiting items, which are
//! requests from an agent that need a human decision. See `docs/20-instruction-queue.md`.

use serde::{Deserialize, Serialize};

use crate::PaneId;

/// Maximum Unicode scalar count retained for one instruction.
pub const MAX_INSTRUCTION_CHARS: usize = 16_384;
/// Maximum active instructions retained by one Workspace.
pub const MAX_WORKSPACE_INSTRUCTIONS: usize = 1_024;
/// Maximum active instructions retained for one invocation.
pub const MAX_INVOCATION_INSTRUCTIONS: usize = 64;

/// Where a queued instruction originated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionAuthor {
    Cli,
    Client,
    ControlApi,
}

impl InstructionAuthor {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Client => "client",
            Self::ControlApi => "control_api",
        }
    }
}

/// When an instruction may be injected without another explicit command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionPolicy {
    /// Wait for an authoritative cooperative ready event from the invocation.
    NextReady,
}

/// Current state of an item retained by the active projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionState {
    Queued,
}

/// One active instruction bound to an exact foreground process group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionItem {
    pub id: u64,
    pub pane: PaneId,
    pub invocation: i32,
    pub author: InstructionAuthor,
    pub created_sequence: u64,
    pub policy: InstructionPolicy,
    pub state: InstructionState,
    pub text: String,
}

/// Why an instruction left the active queue without successful delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionCancellation {
    Canceled,
    Replaced,
    InvocationEnded,
    PaneClosed,
}

/// Authority that permitted one delivery attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionBoundary {
    CooperativeReady,
    SendNow,
}

/// Active instructions and replay-safe monotonic allocators.
#[derive(Clone, Debug)]
pub struct InstructionQueue {
    items: Vec<InstructionItem>,
    next_id: u64,
    next_delivery_id: u64,
}

impl Default for InstructionQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl InstructionQueue {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
            next_delivery_id: 1,
        }
    }

    /// Allocate a bounded item without projecting it. The caller can append
    /// its durable event before inserting it into the active view.
    pub fn allocate(
        &mut self,
        pane: PaneId,
        invocation: i32,
        author: InstructionAuthor,
        created_sequence: u64,
        text: &str,
    ) -> InstructionItem {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        InstructionItem {
            id,
            pane,
            invocation,
            author,
            created_sequence,
            policy: InstructionPolicy::NextReady,
            state: InstructionState::Queued,
            text: bounded_text(text),
        }
    }

    /// Insert a new or replayed item while preserving global queue order.
    pub fn insert(&mut self, item: InstructionItem) {
        self.next_id = self.next_id.max(item.id.saturating_add(1));
        self.items.retain(|current| current.id != item.id);
        self.items.push(item);
        self.items
            .sort_by_key(|item| (item.created_sequence, item.id));
    }

    /// Replace an item in place in the queue ordering with a newly allocated
    /// durable identity.
    pub fn replace(&mut self, replaced: u64, item: InstructionItem) -> bool {
        let Some(index) = self.items.iter().position(|current| current.id == replaced) else {
            return false;
        };
        self.next_id = self.next_id.max(item.id.saturating_add(1));
        self.items[index] = item;
        true
    }

    pub fn remove(&mut self, id: u64) -> Option<InstructionItem> {
        let index = self.items.iter().position(|item| item.id == id)?;
        Some(self.items.remove(index))
    }

    pub fn remove_pane(&mut self, pane: PaneId) -> Vec<InstructionItem> {
        let mut removed = Vec::new();
        self.items.retain(|item| {
            if item.pane == pane {
                removed.push(item.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    pub fn remove_other_invocations(
        &mut self,
        pane: PaneId,
        invocation: Option<i32>,
    ) -> Vec<InstructionItem> {
        let mut removed = Vec::new();
        self.items.retain(|item| {
            if item.pane == pane && Some(item.invocation) != invocation {
                removed.push(item.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    pub fn next_for(&self, pane: PaneId, invocation: i32) -> Option<&InstructionItem> {
        self.items
            .iter()
            .find(|item| item.pane == pane && item.invocation == invocation)
    }

    pub fn get(&self, id: u64) -> Option<&InstructionItem> {
        self.items.iter().find(|item| item.id == id)
    }

    pub fn items(&self) -> &[InstructionItem] {
        &self.items
    }

    pub fn can_enqueue(&self, pane: PaneId, invocation: i32) -> bool {
        self.items.len() < MAX_WORKSPACE_INSTRUCTIONS
            && self
                .items
                .iter()
                .filter(|item| item.pane == pane && item.invocation == invocation)
                .count()
                < MAX_INVOCATION_INSTRUCTIONS
    }

    pub fn mint_delivery_id(&mut self) -> u64 {
        let id = self.next_delivery_id;
        self.next_delivery_id = self.next_delivery_id.saturating_add(1);
        id
    }

    /// Advance the allocator while replaying delivery attempts that are no
    /// longer present in the active projection.
    pub fn observe_delivery_id(&mut self, id: u64) {
        self.next_delivery_id = self.next_delivery_id.max(id.saturating_add(1));
    }
}

fn bounded_text(text: &str) -> String {
    text.chars().take(MAX_INSTRUCTION_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_is_ordered_and_one_ready_boundary_selects_one_item() {
        let mut queue = InstructionQueue::new();
        let first = queue.allocate(PaneId(7), 44, InstructionAuthor::Cli, 10, "first");
        let second = queue.allocate(PaneId(7), 44, InstructionAuthor::Cli, 11, "second");
        queue.insert(second.clone());
        queue.insert(first.clone());
        assert_eq!(queue.next_for(PaneId(7), 44), Some(&first));
        queue.remove(first.id);
        assert_eq!(queue.next_for(PaneId(7), 44), Some(&second));
    }

    #[test]
    fn invocation_change_removes_only_stale_pane_items() {
        let mut queue = InstructionQueue::new();
        for (pane, invocation, sequence) in [(1, 10, 1), (1, 11, 2), (2, 10, 3)] {
            let item = queue.allocate(
                PaneId(pane),
                invocation,
                InstructionAuthor::Client,
                sequence,
                "direction",
            );
            queue.insert(item);
        }
        let removed = queue.remove_other_invocations(PaneId(1), Some(11));
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].invocation, 10);
        assert_eq!(queue.items().len(), 2);
    }

    #[test]
    fn replay_advances_item_and_delivery_allocators() {
        let mut queue = InstructionQueue::new();
        queue.insert(InstructionItem {
            id: 90,
            pane: PaneId(1),
            invocation: 12,
            author: InstructionAuthor::ControlApi,
            created_sequence: 50,
            policy: InstructionPolicy::NextReady,
            state: InstructionState::Queued,
            text: "later".into(),
        });
        queue.observe_delivery_id(70);
        let item = queue.allocate(PaneId(2), 13, InstructionAuthor::Cli, 51, "next");
        assert_eq!(item.id, 91);
        assert_eq!(queue.mint_delivery_id(), 71);
    }

    #[test]
    fn text_is_bounded_without_breaking_utf8() {
        let mut queue = InstructionQueue::new();
        let text = "🦀".repeat(MAX_INSTRUCTION_CHARS + 10);
        let item = queue.allocate(PaneId(1), 10, InstructionAuthor::Cli, 1, &text);
        assert_eq!(item.text.chars().count(), MAX_INSTRUCTION_CHARS);
        assert!(item.text.is_char_boundary(item.text.len()));
    }

    #[test]
    fn invocation_queue_has_a_hard_bound() {
        let mut queue = InstructionQueue::new();
        for sequence in 0..MAX_INVOCATION_INSTRUCTIONS {
            assert!(queue.can_enqueue(PaneId(1), 10));
            let item = queue.allocate(
                PaneId(1),
                10,
                InstructionAuthor::Cli,
                sequence as u64,
                "direction",
            );
            queue.insert(item);
        }
        assert!(!queue.can_enqueue(PaneId(1), 10));
        assert!(queue.can_enqueue(PaneId(1), 11));
    }
}
