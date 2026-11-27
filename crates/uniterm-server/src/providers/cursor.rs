use super::{Manifest, Rule, RuleAnchor, RuleRegion};
use uniterm_core::AgentStatus;

/// Cursor Agent resumes a chat by id with `cursor-agent --resume <id>`.
pub const RESUME: &[&str] = &["cursor-agent", "--resume"];

// Priorities: visible blocked prompts beat activity titles (120 to 115);
// activity titles beat grid activity (110 to 100). Activity is anchored to a spinner or a
// line start so typed prompt text cannot impersonate it (75 to 60), and idle
// hints are the weakest positive signal (20 to 10). No rule matching means
// idle: the matcher never treats output volume as evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "cursor",
    executables: &["agent", "cursor-agent"],
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
                "approval required",
                "approve this command",
                "allow this command",
                "ask every time",
                "run everything",
                "run (once) (y)",
                "skip (esc or n)",
                "write to this file?",
                "reject & propose changes",
            ],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 120,
        },
        Rule {
            status: AgentStatus::Question,
            patterns: &["answer the question", "select an option", "enter to select"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 115,
        },
        Rule {
            status: AgentStatus::Error,
            patterns: &[
                "not authenticated",
                "authentication failed",
                "usage limit",
                "rate limit",
                "request failed",
            ],
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
            status: AgentStatus::Working,
            patterns: &["ctrl+c to stop"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 60,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["plan, search, build anything", "add a follow-up"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 20,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["cursor agent"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 10,
        },
    ],
};

/// Interactive CLI arguments, kept beside this provider's detection rules.
pub(super) const LAUNCH: super::LaunchSpec = super::LaunchSpec {
    prefix: &[],
    prompt_flag: None,
    model_flag: Some("--model"),
    profile_flag: None,
};

// Cursor Agent advertises a direct one-time shortcut in its active command
// panel. Match from the bottom so transcript text and newer prompts cannot
// supply missing choices. Hash the preview, independent of spinner updates.
pub(super) fn approval(screen: &str) -> Option<super::Approval> {
    // The review shortcut is independent of the permission dialog and can
    // remain visible after the agent has made an edit.
    let mut rows = screen.trim_end().lines().rev().peekable();
    if rows.peek()?.trim() == "ctrl+r to review changed files" {
        rows.next();
        while rows.peek().is_some_and(|row| row.trim().is_empty()) {
            rows.next();
        }
    }
    if rows.next()?.trim() != "Skip & tell the agent what to do instead (esc or n)"
        || rows.next()?.trim() != "Run Everything (shift+tab)"
        || rows.next()?.trim() != "→ Run (once) (y)"
    {
        return None;
    }
    // Ink wraps the reason and the preview at different columns. Compare
    // both rendered copies so a separate advisory cannot become a continuation.
    let mut reason = Vec::new();
    loop {
        let row = rows.next()?.strip_prefix(' ')?;
        if let Some(first) = row.strip_prefix("Not in allowlist: ") {
            reason.push(first);
            break;
        }
        if row.is_empty() || row.starts_with(char::is_whitespace) {
            return None;
        }
        reason.push(row);
    }
    if rows.next()?.trim() != "Run this command?" || !rows.next()?.trim().is_empty() {
        return None;
    }
    let mut preview = Vec::new();
    loop {
        let row = rows.next()?;
        if let Some(first) = row.trim_start().strip_prefix("$ ") {
            preview.push(first.trim_start());
            break;
        }
        preview.push(row.strip_prefix("    ")?);
    }
    let divider = rows.next()?.trim();
    if !divider.starts_with("────────") || !divider.chars().all(|c| c == '─') {
        return None;
    }
    reason.reverse();
    preview.reverse();
    let reason = reason.concat();
    let preview = preview.concat();
    if reason.is_empty()
        || preview
            .strip_prefix(&reason)
            .is_none_or(|dir| !dir.starts_with(" in ") || dir.trim_end() == " in")
    {
        return None;
    }
    let mut choice = super::approval::fingerprint(&[&preview], b"y");
    choice.watch_for_dismissal = true;
    Some(choice)
}

pub(super) fn command_dismissed(term: &crate::terminal::Terminal) -> bool {
    let screen = term.evidence_text(10);
    let mut rows = screen
        .lines()
        .rev()
        .map(str::trim)
        .filter(|row| !row.is_empty());
    let Some(directory) = rows.next() else {
        return false;
    };
    directory.starts_with('/')
        && rows
            .next()
            .is_some_and(|model| model.contains(" · ") && model.contains('%'))
        && rows.next() == Some("→ Add a follow-up")
}
