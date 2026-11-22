//! Pure policy for delegated permission answering (`docs/06-agentic-supervision.md`).
//!
//! A human arms one invocation with a captured hourly budget. The server uses
//! the existing provider recognizer and shared approval delivery path; neither
//! cooperative status nor this pure kind gate alone authorizes terminal input.
//! Decisions are queued before delivery, and known history failures suppress it.

use serde::{Deserialize, Serialize};

use crate::{PaneId, WaitingKind};

/// Hard bound on the hourly auto-answer budget.
pub const DELEGATION_MAX_ANSWERS_PER_HOUR: u16 = 240;
/// Default hourly budget when the Workspace config does not set one.
pub const DELEGATION_DEFAULT_ANSWERS_PER_HOUR: u16 = 30;

/// Per-invocation budget captured when the human arms approval.
/// Time is supplied by the server; checking it never schedules idle work.
#[derive(Clone, Debug)]
pub struct DelegationBudget {
    limit: u16,
    window_started_ms: u64,
    used: u16,
}

impl DelegationBudget {
    /// Capture a validated policy so later config edits cannot widen consent.
    pub fn new(policy: &DelegationPolicy, now_ms: u64) -> Self {
        Self {
            limit: policy.max_answers_per_hour,
            window_started_ms: now_ms,
            used: 0,
        }
    }

    /// Answers charged in the current one-hour window.
    pub fn used(&self, now_ms: u64) -> u16 {
        if now_ms.saturating_sub(self.window_started_ms) >= 3_600_000 {
            0
        } else {
            self.used
        }
    }

    /// Remaining submissions; navigation never spends an answer.
    pub fn remaining(&self, now_ms: u64) -> u16 {
        self.limit.saturating_sub(self.used(now_ms))
    }

    /// Charge only after the shared input queue accepts a submission.
    pub fn delivered(&mut self, now_ms: u64) {
        if now_ms.saturating_sub(self.window_started_ms) >= 3_600_000 {
            self.window_started_ms = now_ms;
            self.used = 0;
        }
        self.used = self.used.saturating_add(1);
    }
}

/// Workspace delegation policy after config parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationPolicy {
    /// Maximum auto-answers per armed Pane per hour before delegation
    /// disarms itself until the human re-arms.
    pub max_answers_per_hour: u16,
}

impl Default for DelegationPolicy {
    fn default() -> Self {
        Self {
            max_answers_per_hour: DELEGATION_DEFAULT_ANSWERS_PER_HOUR,
        }
    }
}

impl DelegationPolicy {
    /// Reject impossible durable values rather than silently changing an
    /// armed Pane's contract.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=DELEGATION_MAX_ANSWERS_PER_HOUR).contains(&self.max_answers_per_hour) {
            return Err("max answers per hour is outside its bounded range".into());
        }
        Ok(())
    }
}

/// Pure outcome. There is deliberately no `Deny` verdict that types anything:
/// the supervisor only ever types an approval, and only on `Allow`.
/// `Withhold` leaves the waiting item untouched for the human and says why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DelegateVerdict {
    /// The prompt is a permission prompt, which an armed Pane answers.
    Allow,
    /// The prompt stays with the human; the reason is auditable.
    Withhold {
        /// Human-readable explanation, recorded in the event log.
        reason: String,
    },
}

/// The serde shape of a verdict. serde's internally-tagged enum form needs
/// `deserialize_any`, which bincode (the attach protocol) does not implement,
/// so a derived tagged enum silently fails to decode on that transport while
/// working everywhere JSON carries it. This plain struct keeps the `outcome`
/// tag in the JSON event log and decodes on every transport; `reason`
/// defaults so event streams written before it existed still replay.
#[derive(Serialize, Deserialize)]
struct VerdictSerde {
    outcome: String,
    #[serde(default)]
    reason: Option<String>,
}

impl Serialize for DelegateVerdict {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = match self {
            DelegateVerdict::Allow => VerdictSerde {
                outcome: "allow".into(),
                reason: None,
            },
            DelegateVerdict::Withhold { reason } => VerdictSerde {
                outcome: "withhold".into(),
                reason: Some(reason.clone()),
            },
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DelegateVerdict {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = VerdictSerde::deserialize(deserializer)?;
        match wire.outcome.as_str() {
            "allow" => Ok(DelegateVerdict::Allow),
            "withhold" => Ok(DelegateVerdict::Withhold {
                reason: wire.reason.unwrap_or_default(),
            }),
            other => Err(serde::de::Error::custom(format!(
                "unknown delegation verdict '{other}'"
            ))),
        }
    }
}

/// Audit fact queued before delivery; shared delivery records distinguish an
/// attempted answer from input queue acceptance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRecord {
    /// Pane whose prompt was evaluated.
    pub pane: PaneId,
    /// The waiting item if the status dwell has already created one.
    /// Recognition may authorize a dialog before that notification dwell.
    #[serde(default)]
    pub waiting_id: Option<u64>,
    /// Foreground process-group identity the verdict applied to.
    pub invocation: Option<i32>,
    /// Provider id, recorded for audit only; the engine never branches on it.
    pub agent: String,
    /// Tool the prompt asked about.
    pub tool: String,
    /// Opaque dialog fingerprint; does not duplicate command text.
    pub preview: String,
    /// The verdict reached before any side effect.
    pub verdict: DelegateVerdict,
}

/// Evaluate one waiting item for an armed Pane. The server layers invocation
/// scoping, the budget, and the loop guard on top.
pub fn evaluate_delegation(kind: WaitingKind) -> DelegateVerdict {
    if kind != WaitingKind::Permission {
        return DelegateVerdict::Withhold {
            reason: "only permission prompts can be delegated".into(),
        };
    }
    DelegateVerdict::Allow
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_counts_submissions_and_resets_only_after_an_hour() {
        let mut policy = DelegationPolicy {
            max_answers_per_hour: 2,
        };
        let mut budget = DelegationBudget::new(&policy, 100);
        policy.max_answers_per_hour = 30;
        assert_eq!(policy.max_answers_per_hour, 30);
        assert_eq!(budget.remaining(100), 2);
        budget.delivered(100);
        assert_eq!(budget.remaining(99), 1);
        budget.delivered(200);
        assert_eq!(budget.remaining(3_600_099), 0);
        assert_eq!(budget.remaining(3_600_100), 2);
        budget.delivered(3_600_100);
        assert_eq!(budget.used(3_600_100), 1);
    }

    #[test]
    fn only_permission_prompts_allow() {
        assert_eq!(
            evaluate_delegation(WaitingKind::Permission),
            DelegateVerdict::Allow
        );
        for kind in [
            WaitingKind::Question,
            WaitingKind::Workflow,
            WaitingKind::Relay,
        ] {
            assert_eq!(
                evaluate_delegation(kind),
                DelegateVerdict::Withhold {
                    reason: "only permission prompts can be delegated".into(),
                },
                "{kind:?}"
            );
        }
    }

    #[test]
    fn verdict_serialization_keeps_the_outcome_tag_and_replays_legacy_streams() {
        // The audit contract is the `outcome` tag; bincode (the attach
        // protocol) cannot decode serde's internally-tagged enums, so this
        // shape comes from the plain VerdictSerde struct, not a derive.
        assert_eq!(
            serde_json::to_value(DelegateVerdict::Allow).unwrap(),
            serde_json::json!({ "outcome": "allow", "reason": null })
        );
        assert_eq!(
            serde_json::to_value(DelegateVerdict::Withhold {
                reason: "the identical prompt was re-asked".into(),
            })
            .unwrap(),
            serde_json::json!({ "outcome": "withhold", "reason": "the identical prompt was re-asked" })
        );
        // Streams written before `reason` existed still replay.
        assert_eq!(
            serde_json::from_str::<DelegateVerdict>(r#"{"outcome":"allow"}"#).unwrap(),
            DelegateVerdict::Allow
        );
        assert_eq!(
            serde_json::from_str::<DelegateVerdict>(r#"{"outcome":"withhold","reason":"x"}"#)
                .unwrap(),
            DelegateVerdict::Withhold { reason: "x".into() }
        );
        assert!(serde_json::from_str::<DelegateVerdict>(r#"{"outcome":"deny"}"#).is_err());
    }

    #[test]
    fn policy_validation_enforces_the_budget_bound() {
        assert!(DelegationPolicy::default().validate().is_ok());
        for max_answers_per_hour in [0, DELEGATION_MAX_ANSWERS_PER_HOUR + 1] {
            assert!(
                DelegationPolicy {
                    max_answers_per_hour
                }
                .validate()
                .is_err(),
                "{max_answers_per_hour}"
            );
        }
        assert!(DelegationPolicy {
            max_answers_per_hour: 1
        }
        .validate()
        .is_ok());
    }
}
