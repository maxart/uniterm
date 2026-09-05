use super::{Manifest, Rule, RuleAnchor, RuleRegion};
use uniterm_core::AgentStatus;

// Priorities: a title marker beats any grid text (110 to 100), blocked
// prompts beat activity (90 to 85), activity is anchored to a spinner or a
// line start so typed prompt text cannot impersonate it (75 to 60), and idle
// hints are the weakest positive signal (20 to 10). No rule matching means
// idle: the matcher never treats output volume as evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "opencode",
    executables: &["opencode"],
    rules: &[
        Rule {
            status: AgentStatus::Working,
            patterns: &[""],
            anchor: RuleAnchor::SpinnerLine,
            region: RuleRegion::Title,
            priority: 105,
        },
        Rule {
            status: AgentStatus::Permission,
            patterns: &["permission required", "allow once", "allow always", "deny"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 90,
        },
        Rule {
            status: AgentStatus::Question,
            patterns: &["type your response", "select an option"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 85,
        },
        Rule {
            status: AgentStatus::Error,
            patterns: &["provider error", "request failed", "unauthorized"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 80,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &["thinking", "generating", "running"],
            anchor: RuleAnchor::SpinnerLine,
            region: RuleRegion::Bottom,
            priority: 75,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["type a message"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 20,
        },
    ],
};
