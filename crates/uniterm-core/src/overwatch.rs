//! Durable Workspace supervisor policy, independent of providers and I/O.
use crate::{PaneId, ProjectId};
use serde::{Deserialize, Serialize};

/// Recovery is bounded across restarts until the human explicitly retries.
pub const MAX_RECOVERIES: u8 = 3;

/// Visible lifecycle, distinct from the chosen agent's working/idle status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    #[default]
    Off,
    Starting,
    Running,
    Recovering,
    Attention,
}

/// Event-backed desired state and exact owned invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub enabled: bool,
    pub provider: String,
    pub project: Option<ProjectId>,
    pub manage: bool,
    pub pane: Option<PaneId>,
    pub invocation: Option<u64>,
    pub recoveries: u8,
    pub phase: Phase,
    pub message: String,
    /// Explicit provider model override; absent means the provider's default.
    #[serde(default)]
    pub model: Option<String>,
    /// Native reasoning effort or model variant; absent keeps the harness default.
    #[serde(default)]
    pub effort: Option<String>,
    /// Provider-owned launch capability retained across recovery.
    #[serde(default)]
    pub model_mode: Option<String>,
}

impl State {
    /// Failures consume a persistent budget; no elapsed time alone causes work.
    pub fn failed(&mut self, message: impl Into<String>) -> Option<u64> {
        if !self.enabled {
            return None;
        }
        self.message = message.into();
        if self.recoveries >= MAX_RECOVERIES {
            self.phase = Phase::Attention;
            return None;
        }
        let delay = 1u64 << (self.recoveries * 2);
        self.recoveries += 1;
        self.phase = Phase::Recovering;
        Some(delay)
    }

    /// Turning off cancels all recovery and preserves the user's configuration.
    pub fn disable(&mut self) {
        self.enabled = false;
        self.pane = None;
        self.invocation = None;
        self.phase = Phase::Off;
        self.message.clear();
    }

    /// A saved process identity never proves that a process survived a restart.
    pub fn recovered(&mut self) {
        self.invocation = None;
        if self.enabled && self.phase != Phase::Attention {
            self.phase = Phase::Recovering;
        }
    }
}

/// The same verbs are used by keyboard, menus and the control API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Status,
    Open,
    Configure {
        provider: String,
        #[serde(default)]
        model: Option<String>,
        project: Option<ProjectId>,
        manage: bool,
    },
    /// Explicit management grant; a live change restarts chat with the new endpoint.
    Management {
        enabled: bool,
    },
    Enable,
    Disable,
    Retry,
    Chat,
    /// Change only the model, preserving provider and access choices.
    /// While enabled, apply a changed value by starting a fresh assistant chat.
    Model {
        model: Option<String>,
    },
    /// Request an on-demand model catalog for an attached settings picker.
    Models {
        request: u64,
    },
    /// Apply model and effort together, fencing a stale provider picker.
    Selection {
        provider: String,
        model: Option<String>,
        effort: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn off_and_exhausted_supervisors_never_schedule_work() {
        let mut state = State::default();
        assert_eq!(state.failed("exit"), None);
        state.enabled = true;
        assert_eq!(state.failed("exit"), Some(1));
        state.recovered();
        assert_eq!(state.failed("exit"), Some(4));
        assert_eq!(state.failed("exit"), Some(16));
        assert_eq!(state.failed("exit"), None);
        assert_eq!(state.phase, Phase::Attention);
        state.recovered();
        assert_eq!(state.phase, Phase::Attention);
        state.disable();
        assert_eq!(state.failed("late exit"), None);
        assert_eq!(state.phase, Phase::Off);
    }
    #[test]
    fn restart_preserves_configuration_but_invalidates_invocation() {
        let mut state = State {
            enabled: true,
            provider: "any-provider".into(),
            model: Some("chosen-model".into()),
            project: Some(ProjectId(2)),
            pane: Some(PaneId(3)),
            invocation: Some(9),
            phase: Phase::Running,
            ..State::default()
        };
        state.recovered();
        assert_eq!(state.invocation, None);
        assert_eq!(state.pane, Some(PaneId(3)));
        assert_eq!(state.phase, Phase::Recovering);
        assert_eq!(state.model.as_deref(), Some("chosen-model"));
        state.disable();
        assert_eq!(state.model.as_deref(), Some("chosen-model"));
    }
}
