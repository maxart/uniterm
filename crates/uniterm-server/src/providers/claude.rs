use super::{Manifest, Rule, RuleAnchor, RuleRegion};
use uniterm_core::AgentStatus;

/// Claude Code resumes a session by id with `claude --resume <id>`.
pub const RESUME: &[&str] = &["claude", "--resume"];

// Priorities: visible blocked prompts beat activity titles (120 to 115);
// activity titles beat grid activity (110 to 100). Activity is anchored to a spinner or a
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
            anchor: RuleAnchor::AnimatedTitle,
            region: RuleRegion::Title,
            priority: 110,
        },
        Rule {
            status: AgentStatus::Permission,
            patterns: &[
                "do you want to proceed?",
                "would you like to proceed?",
                "do you want to make this edit to ",
                "do you want to create ",
                "do you want to overwrite ",
                "do you want to allow claude to fetch this content?",
                "allow this tool",
                "do you want to allow this connection?",
                "waiting for permission",
                "ctrl+e to explain",
                "yes, and don't ask again",
                "esc to cancel · tab to amend",
            ],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 120,
        },
        Rule {
            status: AgentStatus::Question,
            patterns: &["enter to select", "type your answer"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 115,
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
            anchor: RuleAnchor::SpinnerActivity,
            region: RuleRegion::Bottom,
            priority: 75,
        },
        Rule {
            status: AgentStatus::Tool,
            patterns: &["running…"],
            anchor: RuleAnchor::LineStart,
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

pub(super) fn global_files() -> Vec<(String, std::path::PathBuf)> {
    let mut files = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let root = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(home).join(".claude"));
        files.push(("Claude user instructions".into(), root.join("CLAUDE.md")));
        files.push(("Claude user settings".into(), root.join("settings.json")));
    }
    let system = if cfg!(target_os = "macos") {
        "/Library/Application Support/ClaudeCode"
    } else {
        "/etc/claude-code"
    };
    files.push((
        "Claude system instructions".into(),
        std::path::Path::new(system).join("CLAUDE.md"),
    ));
    files
}

/// Interactive CLI arguments, kept beside this provider's detection rules.
pub(super) const LAUNCH: super::LaunchSpec = super::LaunchSpec {
    prefix: &[],
    prompt_flag: None,
    model_flag: Some("--model"),
    profile_flag: None,
};

// Approval contracts stay beside the provider UI vocabulary.
pub(super) const APPROVAL_DIALOGS: &[super::approval::Dialog] = &[super::approval::Dialog {
    headings: &[
        "do you trust the files in this folder?",
        "accessing workspace:",
        "quick safety check:",
    ],
    allow: &["yes, i trust this folder", "yes, trust this folder"],
    deny: &["no, exit", "no, continue without these permissions"],
}];
