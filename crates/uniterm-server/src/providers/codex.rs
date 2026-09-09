use super::{Manifest, Rule, RuleAnchor, RuleRegion};
use uniterm_core::AgentStatus;

// Priorities: a title marker beats any grid text (110 to 100), blocked
// prompts beat activity (90 to 85), activity is anchored to a spinner or a
// line start so typed prompt text cannot impersonate it (75 to 60), and idle
// hints are the weakest positive signal (20 to 10). No rule matching means
// idle: the matcher never treats output volume as evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "codex",
    executables: &["codex"],
    rules: &[
        Rule {
            status: AgentStatus::Permission,
            patterns: &["action required"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Title,
            priority: 110,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &[""],
            anchor: RuleAnchor::SpinnerLine,
            region: RuleRegion::Title,
            priority: 105,
        },
        Rule {
            status: AgentStatus::Permission,
            patterns: &[
                "would you like to run the following command?",
                "press enter to confirm",
                "allow codex to",
            ],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 90,
        },
        Rule {
            status: AgentStatus::Question,
            patterns: &["waiting for your response", "answer the question"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 85,
        },
        Rule {
            status: AgentStatus::Error,
            patterns: &["stream disconnected", "usage limit", "failed to"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 80,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &["working", "thinking"],
            anchor: RuleAnchor::SpinnerLine,
            region: RuleRegion::Bottom,
            priority: 75,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &["running command"],
            anchor: RuleAnchor::LineStart,
            region: RuleRegion::Bottom,
            priority: 60,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["codex>"],
            anchor: RuleAnchor::LineStart,
            region: RuleRegion::Bottom,
            priority: 20,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["ask codex"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 10,
        },
    ],
};
