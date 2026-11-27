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
                "e:edit pattern",
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
    alternatives: &[],
    kind: super::approval::ApprovalKind::Permission,
}];

/// Grok 1.x uses numbered radio buttons, with the one-time option third.
/// Scope editing and persistent grants are deliberately separate controls.
pub(super) fn approval(screen: &str) -> Option<super::Approval> {
    if let Some(choice) = trust_approval(screen) {
        return Some(choice);
    }
    if !screen.contains("3 (●) Yes, proceed") {
        return None;
    }
    // Only the trailing framed request is active. Transcript animations above
    // the title must not change the fingerprint of an already answered prompt.
    let lines: Vec<_> = screen.trim_end().lines().collect();
    let (footer, body) = lines.split_last()?;
    let mut hints = footer.split('│').map(str::trim);
    if hints.next()? != "1/5:select"
        || hints.next()? != "Tab:next option"
        || !footer.split('│').any(|s| s.trim() == "e:edit pattern")
        || !footer.split('│').any(|s| s.trim() == "Ctrl+c:cancel")
    {
        return None;
    }
    let title = body
        .iter()
        .rposition(|line| line.trim_start().starts_with("┃  ›  "))?;
    let panel = body[title + 1..]
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.trim().strip_prefix('┃').map(str::trim))
        .collect::<Option<Vec<_>>>()?;
    let mut rows = panel.into_iter().filter(|line| !line.is_empty());
    let preview = rows.next()?.strip_prefix("│ ")?;
    if preview.is_empty() {
        return None;
    }
    // A long command can occupy several framed preview rows.
    let scope = rows.find(|row| !row.starts_with("│ ") && !row.starts_with("┆ "))?;
    if !matches!(
        scope,
        "e edit pattern" | "← → narrow scope  ·  e edit pattern"
    ) {
        return None;
    }
    const GLOBAL: &str = "Yes, and don't ask again for anything (always-approve mode)";
    // Older builds put the global grant first. In 1.0.92 it follows Never
    // allow and has no number. Neither variant may select a persistent grant.
    let first = rows.next()?;
    let older = first.strip_prefix("1 (○) ") == Some(GLOBAL);
    let allow = if older {
        rows.next()?.strip_prefix("2 (○) Always allow: ")?
    } else {
        first.strip_prefix("1 (○) Always allow: ")?
    };
    if allow.is_empty()
        || rows.next()? != "3 (●) Yes, proceed"
        || rows.next()? != "4 (○) No, reject (type to add feedback)"
        || !rows.next()?.starts_with("5 (○) Never allow: ")
        || (!older && rows.next()?.strip_prefix("(○) ") != Some(GLOBAL))
        || rows.next().is_some()
    {
        return None;
    }
    let mut choice = super::approval::fingerprint(&body[title..], b"\r");
    choice.watch_for_dismissal = true;
    Some(choice)
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
            .any(|line| line != "enter or y to trust · n or esc to quit" && !grok_version(line))
    {
        return None;
    }
    let mut choice = super::approval::fingerprint(&lines[..start + 4], b"y");
    choice.kind = super::approval::ApprovalKind::WorkspaceTrust;
    Some(choice)
}

// Current startup pickers include the product version in the bottom corner.
fn grok_version(line: &str) -> bool {
    let Some(version) = line.strip_prefix("grok build ") else {
        return false;
    };
    let mut parts = version.split('.');
    (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    }) && parts.next().is_none()
}

pub(super) fn command_dismissed(term: &crate::terminal::Terminal) -> bool {
    let screen = term.evidence_text(8);
    let mut rows = screen
        .lines()
        .rev()
        .map(str::trim)
        .filter(|row| !row.is_empty());
    rows.next().is_some_and(|footer| {
        footer.starts_with("Shift+Tab:mode") && footer.ends_with("Ctrl+x:shortcuts")
    }) && rows
        .next()
        .is_some_and(|row| row.starts_with("╰─") && row.ends_with("─╯"))
        && rows.next().is_some_and(|row| {
            row.strip_prefix("│ ❯")
                .and_then(|s| s.strip_suffix('│'))
                .is_some_and(|s| s.trim().is_empty())
        })
        && rows
            .next()
            .is_some_and(|row| row.starts_with("╭─") && row.ends_with("─╮"))
}
