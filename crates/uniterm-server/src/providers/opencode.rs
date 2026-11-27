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
    if !screen.contains("Permission required")
        || super::approval::policy_blocked(&screen.to_ascii_lowercase())
    {
        return None;
    }
    let lines: Vec<_> = screen.split('\n').collect();
    let header = lines.iter().rposition(|line| {
        line.trim_start_matches(|c: char| c.is_whitespace() || ['┃', '│', '△'].contains(&c))
            .trim_end()
            == "Permission required"
    })?;
    // Take the controls from the bottom of the current panel. A command or
    // transcript containing button labels above a newer hint is not a picker.
    let mut tail = lines
        .iter()
        .enumerate()
        .skip(header + 1)
        .rev()
        .filter(|(_, line)| !super::approval::content(line).is_empty());
    let (last_row, last) = tail.next()?;
    let (footer, buttons, hints) = if let Some((buttons, hints)) = last.split_once("Reject") {
        (last_row, buttons, hints)
    } else {
        let (row, line) = tail.next()?;
        (
            row,
            line.strip_suffix("Reject")?,
            super::approval::content(last),
        )
    };
    let grid = term.grid();
    // A compact TUI clips the confirmation hint at the physical right edge.
    // Require the entire selection hint and at least "enter con" in that case.
    let clipped = unicode_width::UnicodeWidthStr::width(*last) == usize::from(grid.width());
    let mut words = hints.split_whitespace();
    if words.clone().next() == Some("ctrl+f")
        && (words.next(), words.next()) != (Some("ctrl+f"), Some("fullscreen"))
    {
        return None;
    }
    if (words.next(), words.next(), words.next()) != (Some("⇆"), Some("select"), Some("enter")) {
        return None;
    }
    let confirm = words.next()?;
    if words.next().is_some()
        || !(confirm == "confirm"
            || clipped && confirm.starts_with("con") && "confirm".starts_with(confirm))
    {
        return None;
    }
    let choices: Vec<_> = super::approval::content(buttons)
        .split_whitespace()
        .collect();
    let always = match choices.as_slice() {
        ["Allow", "once"] => None,
        ["Allow", "once", "Allow", "always"] => Some("Allow always"),
        ["Allow", "once", "Always", "allow"] => Some("Always allow"),
        _ => return None,
    };
    let y = grid.height() as usize - lines.len() + footer;
    let style = |label: &str| {
        let offset = lines[footer].find(label)?;
        let x = u16::try_from(unicode_width::UnicodeWidthStr::width(
            &lines[footer][..offset],
        ))
        .ok()?;
        if usize::from(x) + label.len() > usize::from(grid.width()) {
            return None;
        }
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
    if let Some(label) = always {
        let always = style(label)?;
        if always != reject || once.1 == always.1 {
            return None;
        }
    } else {
        // Current two-button layouts draw the unselected choice on the
        // footer background. Resolve that background from the adjacent hint,
        // so custom themes and indexed/true-color palettes need no constants.
        let hint = last.rfind("enter ")?;
        let x = u16::try_from(unicode_width::UnicodeWidthStr::width(&last[..hint])).ok()?;
        if x >= grid.width() {
            return None;
        }
        let hint_y = grid.height() as usize - lines.len() + last_row;
        let background = grid.get(x, hint_y as u16).bg;
        if once.1 == background || reject.1 != background {
            return None;
        }
    }
    let mut choice = super::approval::fingerprint(&lines[header..=footer], b"\r");
    choice.watch_for_dismissal = true;
    Some(choice)
}

/// OpenCode puts its task summary after a stable application marker.
pub(super) fn task_title(title: &str) -> &str {
    title.strip_prefix("OC | ").unwrap_or(title)
}

pub(super) fn command_dismissed(term: &crate::terminal::Terminal) -> bool {
    let screen = term.evidence_text(10);
    let mut rows = screen
        .lines()
        .rev()
        .map(str::trim)
        .filter(|row| !row.is_empty());
    rows.next()
        .is_some_and(|footer| footer.ends_with("ctrl+p commands"))
        && rows.next().is_some_and(|row| {
            row.strip_prefix('╹')
                .is_some_and(|fill| fill.starts_with("▀▀") && fill.chars().all(|c| c == '▀'))
        })
        && rows
            .next()
            .is_some_and(|row| row.starts_with('┃') && row.contains(" · "))
        && rows.next() == Some("┃")
}
