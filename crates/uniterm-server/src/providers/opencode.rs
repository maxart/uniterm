use super::{Manifest, Rule, RuleAnchor, RuleRegion};
use uniterm_core::AgentStatus;

/// OpenCode opens a session by id with `opencode --session <id>`.
pub const RESUME: &[&str] = &["opencode", "--session"];

// Priorities: visible blocked prompts beat activity titles (120 to 115);
// activity titles beat grid activity (110 to 100). Activity is anchored to a spinner or a
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
            priority: 120,
        },
        Rule {
            status: AgentStatus::Question,
            patterns: &["type your response", "select an option"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 115,
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

/// Interactive CLI arguments, kept beside this provider's detection rules.
pub(super) const LAUNCH: super::LaunchSpec = super::LaunchSpec {
    prefix: &[],
    prompt_flag: Some("--prompt"),
    // OpenCode 2 removed these v1 TUI flags. Keep the common interactive contract.
    model_flag: None,
    profile_flag: None,
};

/// OpenCode marks its horizontal choice with colors, not a text cursor.
/// Verify the selected button from the live grid rather than assuming Enter
/// means allow; moving onto Always or Reject must suppress automation.
pub(super) fn approval(term: &crate::terminal::Terminal, screen: &str) -> Option<super::Approval> {
    let lines: Vec<_> = screen.split('\n').collect();
    let header = lines.iter().rposition(|line| {
        line.trim_start_matches(|c: char| c.is_whitespace() || ['┃', '│', '△'].contains(&c))
            .trim_end()
            == "Permission required"
    })?;
    let footer = lines
        .iter()
        .enumerate()
        .skip(header + 1)
        .find_map(|(row, line)| {
            (line.contains("Allow once") && line.contains("Reject")).then_some(row)
        })?;
    if !lines[footer..]
        .iter()
        .any(|line| line.contains("enter") && line.contains("confirm"))
        || !lines[footer..]
            .iter()
            .any(|line| line.contains("⇆") && line.contains("select"))
    {
        return None;
    }
    let grid = term.grid();
    let y = grid.height() as usize - lines.len() + footer;
    let style = |label: &str| {
        let offset = lines[footer].find(label)?;
        let x = unicode_width::UnicodeWidthStr::width(&lines[footer][..offset]) as u16;
        let first = grid.get(x, y as u16);
        for dx in 0..label.len() as u16 {
            let cell = grid.get(x + dx, y as u16);
            if (cell.fg, cell.bg, cell.attrs) != (first.fg, first.bg, first.attrs) {
                return None;
            }
        }
        Some((first.fg, first.bg, first.attrs))
    };
    let once = style("Allow once")?;
    let reject = style("Reject")?;
    if let Some(always) = style("Allow always") {
        if always != reject || once.1 == always.1 || once.0 == always.0 {
            return None;
        }
    } else {
        // Current two-button layouts draw the unselected choice on the
        // footer background. Resolve that background from the adjacent hint,
        // so custom themes and indexed/true-color palettes need no constants.
        let hint = lines[footer].find("enter")?;
        let x = unicode_width::UnicodeWidthStr::width(&lines[footer][..hint]) as u16;
        let background = grid.get(x, y as u16).bg;
        if once.1 == background || reject.1 != background || once.0 == reject.0 {
            return None;
        }
    }
    Some(super::approval::fingerprint(&lines[header..=footer], b"\r"))
}
