use super::{Manifest, Rule, RuleAnchor, RuleRegion};
use uniterm_core::AgentStatus;

// Priorities: a title marker beats any grid text (110 to 100), blocked
// prompts beat activity (90 to 85), activity is anchored to a spinner or a
// line start so typed prompt text cannot impersonate it (75 to 60), and idle
// hints are the weakest positive signal (20 to 10). No rule matching means
// idle: the matcher never treats output volume as evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "claude",
    executables: &["claude"],
    rules: &[
        Rule {
            status: AgentStatus::Working,
            patterns: &[""],
            anchor: RuleAnchor::SpinnerLine,
            region: RuleRegion::Title,
            priority: 110,
        },
        Rule {
            status: AgentStatus::Permission,
            patterns: &[
                "do you want to proceed?",
                "allow this tool",
                "yes, and don't ask again",
                "esc to cancel · tab to amend",
            ],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 90,
        },
        Rule {
            status: AgentStatus::Question,
            patterns: &["enter to select", "type your answer"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 85,
        },
        Rule {
            status: AgentStatus::Error,
            patterns: &["api error", "authentication failed", "rate limit reached"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 80,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &[""],
            anchor: RuleAnchor::SpinnerLine,
            region: RuleRegion::Bottom,
            priority: 75,
        },
        Rule {
            status: AgentStatus::Tool,
            patterns: &["running…"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 70,
        },
        Rule {
            status: AgentStatus::Tool,
            patterns: &["running..."],
            anchor: RuleAnchor::LineStart,
            region: RuleRegion::Bottom,
            priority: 70,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &["esc to interrupt"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 60,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["what can i help you with?"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 20,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["claude code"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 10,
        },
    ],
};
