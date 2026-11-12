use super::{Manifest, Rule, RuleAnchor, RuleRegion};
use uniterm_core::AgentStatus;

/// Grok Build resumes a session by id with `grok --resume <id>`.
pub const RESUME: &[&str] = &["grok", "--resume"];

// Priorities: visible blocked prompts beat activity titles (120 to 115);
// activity titles beat grid activity (110 to 100). Activity is anchored to a spinner or a
// line start so typed prompt text cannot impersonate it (75 to 60), and idle
// hints are the weakest positive signal (20 to 10). No rule matching means
// idle: the matcher never treats output volume as evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "grok",
    executables: &["grok", "agent"],
    rules: &[
        Rule {
            status: AgentStatus::Permission,
            patterns: &["action required"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Title,
            priority: 125,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &[""],
            anchor: RuleAnchor::AnimatedTitle,
            region: RuleRegion::Title,
            priority: 105,
        },
        Rule {
            status: AgentStatus::Permission,
            patterns: &[
                "approval required",
                "allow this command",
                "confirm execution",
                "ctrl+o:yolo",
                "←/→:scope",
            ],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 120,
        },
        Rule {
            status: AgentStatus::Question,
            patterns: &[
                "waiting for your response",
                "select an option",
                "shift+x:dismiss",
            ],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 115,
        },
        Rule {
            status: AgentStatus::Error,
            patterns: &["grok api error", "rate limit", "request failed"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 80,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &["[stop]"],
            anchor: RuleAnchor::SpinnerSuffix,
            region: RuleRegion::Bottom,
            priority: 110,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &["thinking", "running tool", "generating"],
            anchor: RuleAnchor::SpinnerLine,
            region: RuleRegion::Bottom,
            priority: 75,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["ask grok", "type a message"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 20,
        },
    ],
};

/// Interactive CLI arguments, kept beside this provider's detection rules.
pub(super) const LAUNCH: super::LaunchSpec = super::LaunchSpec {
    prefix: &[],
    prompt_flag: None,
    model_flag: Some("--model"),
    profile_flag: Some("--agent"),
};

// Approval contracts stay beside the provider UI vocabulary.
pub(super) const APPROVAL_DIALOGS: &[super::approval::Dialog] = &[super::approval::Dialog {
    headings: &[
        "approval required",
        "allow this command",
        "confirm execution",
    ],
    allow: &["allow once", "allow (once)", "yes"],
    deny: &["reject once", "reject", "deny"],
}];

/// Grok 1.x uses numbered radio buttons, with the one-time option third.
/// Scope editing and persistent grants are deliberately separate controls.
pub(super) fn approval(screen: &str) -> Option<super::Approval> {
    if let Some(choice) = trust_approval(screen) {
        return Some(choice);
    }
    let lines: Vec<_> = screen.lines().collect();
    let text = |line: &str| line.trim().trim_matches('┃').trim().to_lowercase();
    let choice = lines
        .iter()
        .rposition(|line| text(line) == "3 (●) yes, proceed")?;
    let start = choice.checked_sub(2)?;
    if !text(lines[start]).starts_with("1 (○) yes, and don't ask again for anything")
        || !text(lines[start + 1]).starts_with("2 (○) always allow:")
        || !text(lines.get(choice + 1)?).starts_with("4 (○) no, reject")
        || !text(lines.get(choice + 2)?).starts_with("5 (○) never allow:")
    {
        return None;
    }
    let scope = lines[..start].iter().rposition(|line| {
        let line = text(line);
        line.contains("narrow scope") && line.contains("edit pattern")
    })?;
    if !lines[choice + 2..].iter().any(|line| {
        let line = text(line);
        line.contains("tab:next option") && line.contains("ctrl+c:cancel") && line.contains("esc:")
    }) {
        return None;
    }
    // Include the request description and command above scope, not the
    // animated elapsed-time indicator higher in the transcript.
    let request = scope.saturating_sub(2);
    Some(super::approval::fingerprint(
        &lines[request..=choice + 2],
        b"\r",
    ))
}

// The startup selector advertises y/n directly and has no selected-row marker.
// Require the entire warning and both shortcuts, with no newer prompt below it.
fn trust_approval(screen: &str) -> Option<super::Approval> {
    const WARNING: &str = "Grok Build may run or modify contents in this directory,";
    if !screen
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case(WARNING))
    {
        return None;
    }
    let lines: Vec<_> = screen
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let normalized: Vec<_> = lines
        .iter()
        .map(|line| {
            line.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        })
        .collect();
    let start = normalized
        .iter()
        .rposition(|line| line == "grok build may run or modify contents in this directory,")?;
    if normalized.get(start + 1)? != "posing security risks." {
        return None;
    }
    let allow = normalized.get(start + 2)?;
    let deny = normalized.get(start + 3)?;
    if !["yes, proceed y", "y yes, proceed"].contains(&allow.as_str())
        || !["no, quit n", "n no, quit"].contains(&deny.as_str())
        || normalized[start + 4..]
            .iter()
            .any(|line| line != "enter or y to trust · n or esc to quit")
    {
        return None;
    }
    Some(super::approval::fingerprint(&lines[..start + 4], b"y"))
}
