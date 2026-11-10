//! Invocation-scoped agent details: provider-neutral observations that are
//! not statuses. See `docs/06-agentic-supervision.md`.
//!
//! A status answers "what is the agent doing now". A detail answers "what did
//! the agent last tell us": its native session identity, its last notification
//! text, the tool a pending permission prompt names, and whether a self-paced
//! loop scheduled its next wakeup or stopped. Details never change a status:
//! a notification is an event, not completion, and a stopped loop is not proof
//! that a whole task is done.
//!
//! The projection is scoped to one invocation and one native session. A
//! replacement session (same process, new session id) or a new invocation
//! starts from an empty record, so a watcher can never act on an earlier
//! session's prompt or transcript.

use serde::{Deserialize, Serialize};

use crate::AgentStatus;

/// Upper bound, in characters, for free text carried by a detail. Hooks bound
/// their payloads too; this is the projection's own guarantee.
pub const DETAIL_TEXT_LIMIT: usize = 512;

/// Upper bound, in characters, for identifiers and paths.
pub const DETAIL_ID_LIMIT: usize = 1_024;

/// Whether a self-paced loop asked to be woken again or ended itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopState {
    /// The agent scheduled its next iteration. Idle until then is expected.
    Scheduled,
    /// The agent explicitly ended its loop. This covers the loop only.
    Stopped,
}

impl LoopState {
    /// The lowercase wire and display name, shared by redacted streams.
    pub fn label(self) -> &'static str {
        match self {
            LoopState::Scheduled => "scheduled",
            LoopState::Stopped => "stopped",
        }
    }
}

/// One observed detail, as appended to the event log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentDetail {
    /// A provider notification. `kind` is the provider's own type string,
    /// kept verbatim (bounded) so it is never reclassified from free text.
    Notification { kind: String, message: String },
    /// A real permission prompt that named the tool it is asking about.
    PermissionAsked { tool: String, preview: String },
    /// A loop wakeup was scheduled or the loop was stopped, observed only
    /// from a tool call the provider reported as completed.
    Loop {
        state: LoopState,
        delay_seconds: Option<u32>,
    },
}

/// The last notification an invocation reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentNotification {
    /// The provider's own notification type, verbatim and bounded.
    pub kind: String,
    /// The provider's notification text, bounded and control-free.
    pub message: String,
    /// Wall-clock milliseconds when Uniterm observed it (display only).
    pub at_ms: u64,
}

/// The tool a pending permission prompt is asking about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPermission {
    /// The tool the prompt asks to run.
    pub tool: String,
    /// The tool call's most identifying argument, bounded and control-free.
    pub preview: String,
    /// Wall-clock milliseconds when Uniterm observed it (display only).
    pub at_ms: u64,
}

/// The last loop transition an invocation reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLoop {
    /// Whether the loop scheduled its next wakeup or stopped.
    pub state: LoopState,
    /// Requested delay before the next wakeup; `None` once stopped.
    pub delay_seconds: Option<u32>,
    /// Wall-clock milliseconds when Uniterm observed it (display only).
    pub at_ms: u64,
}

/// The observable detail record of one agent invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDetails {
    /// Foreground process group of the invocation these details belong to.
    pub invocation: Option<i32>,
    /// Provider-native session id, when the connector reports one.
    pub session_id: Option<String>,
    /// Provider-native transcript location, when the connector reports one.
    pub transcript_path: Option<String>,
    /// The most recent notification of this session, of any kind.
    pub last_notification: Option<AgentNotification>,
    /// Present only while the status is `Permission`.
    pub pending_permission: Option<AgentPermission>,
    /// The last loop transition, serialized as `loop`.
    #[serde(rename = "loop")]
    pub loop_state: Option<AgentLoop>,
}

impl AgentDetails {
    /// An empty record for one invocation.
    pub fn new(invocation: Option<i32>) -> Self {
        Self {
            invocation,
            ..Self::default()
        }
    }

    /// Record a session identity. Returns true when it replaced a different
    /// session: every earlier detail is dropped (including fields the new
    /// observation does not carry), and the caller must retire anything bound
    /// to the old session, such as an answerable waiting item.
    pub fn observe_session(
        &mut self,
        session_id: Option<&str>,
        transcript_path: Option<&str>,
    ) -> bool {
        let session_id = session_id.map(|value| bounded_text(value, DETAIL_ID_LIMIT));
        let transcript_path = transcript_path.map(|value| bounded_text(value, DETAIL_ID_LIMIT));
        let replaced = matches!(
            (&self.session_id, &session_id),
            (Some(old), Some(new)) if old != new
        );
        if replaced {
            *self = Self {
                invocation: self.invocation,
                session_id,
                transcript_path,
                ..Self::default()
            };
            return true;
        }
        if session_id.is_some() {
            self.session_id = session_id;
        }
        if transcript_path.is_some() {
            self.transcript_path = transcript_path;
        }
        false
    }

    /// Project one detail observed at `at_ms` (wall clock, for display).
    pub fn apply(&mut self, detail: &AgentDetail, at_ms: u64) {
        match detail {
            AgentDetail::Notification { kind, message } => {
                self.last_notification = Some(AgentNotification {
                    kind: bounded_text(kind, DETAIL_TEXT_LIMIT),
                    message: bounded_text(message, DETAIL_TEXT_LIMIT),
                    at_ms,
                });
            }
            AgentDetail::PermissionAsked { tool, preview } => {
                self.pending_permission = Some(AgentPermission {
                    tool: bounded_text(tool, DETAIL_TEXT_LIMIT),
                    preview: bounded_text(preview, DETAIL_TEXT_LIMIT),
                    at_ms,
                });
            }
            AgentDetail::Loop {
                state,
                delay_seconds,
            } => {
                self.loop_state = Some(AgentLoop {
                    state: *state,
                    delay_seconds: *delay_seconds,
                    at_ms,
                });
            }
        }
    }

    /// Follow a reconciled status: a permission payload describes only the
    /// prompt on screen, so it leaves as soon as the agent moves on. A later
    /// generic permission signal (no tool) keeps the richer payload.
    pub fn observe_status(&mut self, status: AgentStatus) {
        if status != AgentStatus::Permission {
            self.pending_permission = None;
        }
    }
}

/// Bound free text to `limit` characters and drop every control character
/// (C0, DEL, C1), so a detail can never carry an escape sequence to a
/// terminal, title, or notification channel downstream.
pub fn bounded_text(value: &str, limit: usize) -> String {
    value
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(message: &str) -> AgentDetail {
        AgentDetail::Notification {
            kind: "push_notification".into(),
            message: message.into(),
        }
    }

    #[test]
    fn details_follow_one_session_and_reset_on_replacement() {
        let mut details = AgentDetails::new(Some(41));
        assert!(!details.observe_session(Some("a"), Some("/t/a.jsonl")));
        details.apply(&notification("done"), 1);
        details.apply(
            &AgentDetail::PermissionAsked {
                tool: "Bash".into(),
                preview: "git push".into(),
            },
            2,
        );
        // The same session without a transcript keeps what it had.
        assert!(!details.observe_session(Some("a"), None));
        assert_eq!(details.transcript_path.as_deref(), Some("/t/a.jsonl"));
        // A different session in the same process drops everything,
        // including fields the replacement does not report.
        assert!(details.observe_session(Some("b"), None));
        assert_eq!(details.invocation, Some(41));
        assert_eq!(details.session_id.as_deref(), Some("b"));
        assert_eq!(details.transcript_path, None);
        assert_eq!(details.last_notification, None);
        assert_eq!(details.pending_permission, None);
    }

    #[test]
    fn first_session_observation_is_not_a_replacement() {
        let mut details = AgentDetails::new(None);
        details.apply(&notification("early"), 1);
        assert!(!details.observe_session(Some("a"), None));
        assert!(details.last_notification.is_some());
        assert!(!details.observe_session(None, Some("/t")));
        assert_eq!(details.transcript_path.as_deref(), Some("/t"));
    }

    #[test]
    fn permission_payload_lives_only_while_permission_is_shown() {
        let mut details = AgentDetails::new(None);
        details.apply(
            &AgentDetail::PermissionAsked {
                tool: "Write".into(),
                preview: "src/main.rs".into(),
            },
            5,
        );
        // A generic permission signal arriving later keeps the tool.
        details.observe_status(AgentStatus::Permission);
        assert_eq!(details.pending_permission.as_ref().unwrap().tool, "Write");
        details.observe_status(AgentStatus::Working);
        assert_eq!(details.pending_permission, None);
    }

    #[test]
    fn loop_and_notification_never_touch_status_or_each_other() {
        let mut details = AgentDetails::new(None);
        details.apply(
            &AgentDetail::Loop {
                state: LoopState::Scheduled,
                delay_seconds: Some(90),
            },
            1,
        );
        details.apply(&notification("Loop stopped."), 2);
        // Free text never implies a loop transition.
        assert_eq!(
            details.loop_state.as_ref().unwrap().state,
            LoopState::Scheduled
        );
        details.apply(
            &AgentDetail::Loop {
                state: LoopState::Stopped,
                delay_seconds: None,
            },
            3,
        );
        assert_eq!(
            details.loop_state.as_ref().unwrap().state,
            LoopState::Stopped
        );
        assert_eq!(
            details.last_notification.as_ref().unwrap().message,
            "Loop stopped."
        );
    }

    #[test]
    fn text_is_bounded_and_control_free() {
        assert_eq!(bounded_text("a\x1b]0;x\x07b\nc\u{9b}", 64), "a]0;xb c");
        assert_eq!(bounded_text(&"é".repeat(600), 512).chars().count(), 512);
        let mut details = AgentDetails::new(None);
        details.apply(&notification(&"x".repeat(4_000)), 1);
        assert_eq!(
            details.last_notification.unwrap().message.chars().count(),
            DETAIL_TEXT_LIMIT
        );
    }
}
