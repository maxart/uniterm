//! Session-scoped permission automation. No timers, I/O, or provider rules.

use std::collections::HashMap;

use crate::{AgentStatus, PaneId};

/// One Workspace's explicit opt-in and last delivered prompt per live Pane.
/// This intentionally does not survive server restart or crash recovery.
#[derive(Default)]
pub struct AutoApproval {
    enabled: bool,
    delivered: HashMap<PaneId, Delivery>,
}

struct Delivery {
    invocation: u64,
    prompt: u64,
    navigation: Option<u64>,
    automatic: bool,
}

impl AutoApproval {
    /// Whether the human enabled automation in this server lifetime.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Change the mode without forgetting previously delivered prompts.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Forget a dead invocation, or a prompt that has observably advanced.
    pub fn observe(&mut self, pane: PaneId, status: AgentStatus) {
        if status != AgentStatus::Permission {
            self.delivered.remove(&pane);
        }
    }

    /// Check live dialog progress only for Panes with a delivered choice.
    /// This keeps status changes allocation-free when automation was never used.
    pub fn has_delivery(&self, pane: PaneId) -> bool {
        self.delivered.contains_key(&pane)
    }

    /// Continue an explicitly requested manual navigation even with auto mode off.
    pub fn pending_manual(&self, pane: PaneId) -> bool {
        self.delivered
            .get(&pane)
            .is_some_and(|delivery| !delivery.automatic && delivery.navigation.is_some())
    }

    /// A manual approval authorizes navigation of only its original dialog and
    /// invocation, never a replacement request that happens to reuse the Pane.
    pub fn can_continue_manual(&self, pane: PaneId, invocation: u64, prompt: u64) -> bool {
        self.delivered.get(&pane).is_some_and(|delivery| {
            !delivery.automatic
                && delivery.navigation.is_some()
                && delivery.invocation == invocation
                && delivery.prompt == prompt
        })
    }

    /// A repaint or toggle cannot resend a choice, but a changed selection can
    /// advance navigation. A submitted dialog stays consumed until progress.
    pub fn is_new(
        &self,
        pane: PaneId,
        invocation: u64,
        prompt: u64,
        navigation: Option<u64>,
    ) -> bool {
        match self.delivered.get(&pane) {
            Some(delivery) if delivery.invocation == invocation && delivery.prompt == prompt => {
                delivery.navigation.is_some() && delivery.navigation != navigation
            }
            _ => true,
        }
    }

    /// Cache only after the bounded input queue accepts the bytes. Navigation
    /// and submission are separate deliveries, driven by actual screen updates.
    pub fn delivered(
        &mut self,
        pane: PaneId,
        invocation: u64,
        prompt: u64,
        navigation: Option<u64>,
        automatic: bool,
    ) {
        self.delivered.insert(
            pane,
            Delivery {
                invocation,
                prompt,
                navigation,
                automatic,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_off_deduplicates_and_scopes_to_invocation() {
        let mut state = AutoApproval::default();
        let pane = PaneId(1);
        assert!(!state.enabled());
        state.set_enabled(true);
        assert!(state.is_new(pane, 10, 20, None));
        // A failed delivery must not consume the prompt.
        assert!(state.is_new(pane, 10, 20, None));
        state.delivered(pane, 10, 20, None, true);
        assert!(!state.is_new(pane, 10, 20, None));
        state.set_enabled(false);
        state.set_enabled(true);
        assert!(!state.is_new(pane, 10, 20, None));
        assert!(state.is_new(pane, 10, 21, None));
        assert!(state.is_new(pane, 11, 20, None));
        assert!(state.is_new(PaneId(2), 10, 20, None));
        state.observe(pane, AgentStatus::Permission);
        assert!(!state.is_new(pane, 10, 20, None));
        state.observe(pane, AgentStatus::Working);
        assert!(state.is_new(pane, 10, 20, None));
    }
    #[test]
    fn navigation_waits_for_screen_progress_and_submission_consumes_the_dialog() {
        let mut state = AutoApproval::default();
        let pane = PaneId(1);
        assert!(state.is_new(pane, 10, 20, Some(1)));
        state.delivered(pane, 10, 20, Some(1), false);
        assert!(state.pending_manual(pane));
        assert!(state.can_continue_manual(pane, 10, 20));
        assert!(!state.can_continue_manual(pane, 11, 20));
        assert!(!state.can_continue_manual(pane, 10, 21));
        assert!(!state.is_new(pane, 10, 20, Some(1)));
        assert!(state.is_new(pane, 10, 20, None));
        state.delivered(pane, 10, 20, None, false);
        assert!(!state.pending_manual(pane));
        assert!(!state.is_new(pane, 10, 20, Some(1)));
        assert!(!state.is_new(pane, 10, 20, None));
        assert!(state.is_new(pane, 11, 20, Some(1)));
    }
}
