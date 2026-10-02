use super::{Manifest, Rule, RuleAnchor, RuleRegion};
use uniterm_core::AgentStatus;

// Priorities: a title marker beats any grid text (110 to 100), blocked
// prompts beat activity (90 to 85), activity is anchored to a spinner or a
// line start so typed prompt text cannot impersonate it (75 to 60), and idle
// hints are the weakest positive signal (20 to 10). No rule matching means
// idle: the matcher never treats output volume as evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "pi",
    executables: &["pi", "pi-coding-agent"],
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
            patterns: &["project trust", "saved decision:", "current session:"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 90,
        },
        Rule {
            status: AgentStatus::Error,
            patterns: &[
                "api error",
                "authentication failed",
                "rate limit",
                "request failed",
                "no api key",
            ],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 80,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &[
                "working...",
                "retrying (",
                "compacting context...",
                "auto-compacting...",
                "summarizing branch...",
            ],
            anchor: RuleAnchor::LineStart,
            region: RuleRegion::Bottom,
            priority: 60,
        },
    ],
};
