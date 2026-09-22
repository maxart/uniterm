use super::{Manifest, Rule, RuleAnchor, RuleRegion};
use uniterm_core::AgentStatus;

// Priorities: a title marker beats any grid text (110 to 100), blocked
// prompts beat activity (90 to 85), activity is anchored to a spinner or a
// line start so typed prompt text cannot impersonate it (75 to 60), and idle
// hints are the weakest positive signal (20 to 10). No rule matching means
// idle: the matcher never treats output volume as evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "gemini",
    executables: &["gemini"],
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
            patterns: &[
                "allow execution?",
                "confirm action",
                "yes, allow",
                "│ apply this change",
                "│ allow execution",
            ],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 90,
        },
        Rule {
            status: AgentStatus::Question,
            patterns: &["waiting for input", "choose an option"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 85,
        },
        Rule {
            status: AgentStatus::Error,
            patterns: &["gemini api error", "quota exceeded", "request failed"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 80,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &["thinking", "executing tool"],
            anchor: RuleAnchor::SpinnerLine,
            region: RuleRegion::Bottom,
            priority: 75,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &["esc to cancel"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 60,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["type your message"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 20,
        },
    ],
};
