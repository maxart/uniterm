use super::{approval, Manifest, Rule};
use crate::terminal::Terminal;

/// Kimi CLI resumes a session by id with `kimi --session <id>`.
pub const RESUME: &[&str] = &["kimi", "--session"];

/// Kimi CLI process ownership; status is cooperative or activity-based.
pub const MANIFEST: Manifest = Manifest {
    id: "kimi",
    executables: &["kimi"],
    rules: &[] as &[Rule],
};

pub(super) const PANEL_TITLE: &str = "╭─ approval ";

const OPTIONS: [&str; 4] = [
    "[1] Approve once",
    "[2] Approve for this session",
    "[3] Reject",
    "[4] Reject, tell the model what to do instead",
];

/// Kimi handles '1' as an immediate one-time grant even when another option
/// is highlighted. In its inline rejection editor the same key inserts text.
pub(super) fn approval(term: &Terminal, screen: &str) -> Option<approval::Approval> {
    if approval::policy_blocked(&screen.to_ascii_lowercase()) {
        return None;
    }
    let mut grant = dialog(screen)?;
    grant.watch_for_dismissal = true;
    native_selection(term, screen).then_some(grant)
}

/// Kimi removes the normal input header whenever a modal owns the prompt.
pub(super) fn command_dismissed(term: &Terminal) -> bool {
    if !term.cursor_visible() {
        return false;
    }
    let (x, y) = term.cursor();
    let grid = term.grid();
    let start = grid.height().saturating_sub(48);
    if x != 1 || y <= start || y >= grid.height() || !matches!(grid.get(0, y - 1).ch, '─' | '╌')
    {
        return false;
    }
    let screen = term.bottom_text(48);
    let mut rows = screen.trim_end().lines();
    let Some(header) = rows.nth(usize::from(y - start - 1)) else {
        return false;
    };
    let plan = header.starts_with("╌╌ input · plan ");
    let (prefix, dash) = if plan {
        ("╌╌ input · plan ", '╌')
    } else {
        ("── input ", '─')
    };
    if !header
        .strip_prefix(prefix)
        .is_some_and(|fill| !fill.is_empty() && fill.chars().all(|c| c == dash))
    {
        return false;
    }
    let mut rows = rows.rev();
    let Some(context) = rows.next().and_then(|l| l.trim().strip_prefix("context: ")) else {
        return false;
    };
    let Some((percent, _)) = context.split_once('%') else {
        return false;
    };
    if percent.parse::<f32>().is_err()
        || rows.next().and_then(agent_toolbar_plan) != Some(plan)
        || !rows
            .next()
            .is_some_and(|l| !l.is_empty() && l.chars().all(|c| c == '─'))
    {
        return false;
    }
    rows.all(|line| line.trim().is_empty())
}

// Native toolbar flags precede the mode in a fixed order. Plan mode also
// changes the empty composer's header, so dismissal requires both to agree.
fn agent_toolbar_plan(line: &str) -> Option<bool> {
    let line = line.trim_start();
    let line = line.strip_prefix("yolo  ").unwrap_or(line);
    let line = line.strip_prefix("afk  ").unwrap_or(line);
    let without_plan = line.strip_prefix("plan  ");
    (without_plan.unwrap_or(line).split_whitespace().next() == Some("agent"))
        .then_some(without_plan.is_some())
}

// Rich paints the active arrow and option in cyan, and the other three in a
// common muted color. A multiline paste in Kimi's composer retains the text
// and box geometry even when its input heading has scrolled off the screen.
fn native_selection(term: &Terminal, screen: &str) -> bool {
    let grid = term.grid();
    if grid.width() < 5 {
        return false;
    }
    for (row, y) in screen.rsplit('\n').zip((0..grid.height()).rev()) {
        let Some(label) = row.strip_prefix("│ → ").and_then(|s| s.strip_suffix('│')) else {
            continue;
        };
        let Some(index) = OPTIONS
            .iter()
            .position(|option| *option == label.trim_end())
        else {
            continue;
        };
        let Some(top) = y.checked_sub(index as u16) else {
            return false;
        };
        if u32::from(top) + OPTIONS.len() as u32 > u32::from(grid.height()) {
            return false;
        }
        let active = grid.get(2, y).fg;
        let inactive = grid.get(4, top + u16::from(index == 0)).fg;
        return active != inactive
            && active != uniterm_core::Color::Default
            && (0..4).all(|offset| {
                let expected = if offset == index as u16 {
                    active
                } else {
                    inactive
                };
                grid.get(4, top + offset).fg == expected
            });
    }
    false
}

pub(super) fn dialog(screen: &str) -> Option<approval::Approval> {
    let mut rows = screen.trim_end().lines().rev().map(str::trim);
    let mut bottom = rows.next()?;
    if !bottom.starts_with('╰') {
        // Kimi's running prompt can keep its two-line toolbar below the panel.
        // The regular composer has an additional input heading above this bar.
        let (_, context) = bottom.rsplit_once("context: ")?;
        let (percent, _) = context.split_once('%')?;
        percent.parse::<f32>().ok()?;
        agent_toolbar_plan(rows.next()?)?;
        let separator = rows.next()?;
        if separator.is_empty() || !separator.chars().all(|c| c == '─') {
            return None;
        }
        bottom = rows.find(|line| !line.is_empty())?;
    }
    let border = bottom.strip_prefix('╰')?.strip_suffix('╯')?;
    if border.is_empty() || !border.chars().all(|c| c == '─') {
        return None;
    }
    fn cell(line: &str) -> Option<&str> {
        line.strip_prefix('│')?.strip_suffix('│').map(str::trim)
    }
    let footer = cell(rows.next()?)?;
    if !matches!(
        footer,
        "▲/▼ select  1/2/3/4 choose  ↵ confirm"
            | "▲/▼ select  1/2/3/4 choose  ↵ confirm  ctrl-e expand"
    ) || !cell(rows.next()?)?.is_empty()
    {
        return None;
    }
    let mut selection = 0;
    for option in OPTIONS.iter().rev() {
        let line = cell(rows.next()?)?;
        let label = if let Some(label) = line.strip_prefix("→ ") {
            selection += 1;
            label
        } else {
            line
        };
        if label != *option {
            return None;
        }
    }
    if selection != 1 || !cell(rows.next()?)?.is_empty() {
        return None;
    }
    let mut request: Vec<&str> = Vec::new();
    for line in rows {
        if let Some(title_border) = line.strip_prefix(PANEL_TITLE) {
            if !title_border.strip_suffix('╮')?.chars().all(|c| c == '─') {
                return None;
            }
            request.reverse();
            let (sender, action) = request
                .first()
                .copied()?
                .split_once(" is requesting approval to ")?;
            if sender.is_empty() || action.strip_suffix(':')?.is_empty() {
                return None;
            }
            return Some(approval::fingerprint(&request, b"1"));
        }
        request.push(cell(line)?);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{permission_approval, ApprovalKind};
    use crate::terminal::Terminal;

    #[test]
    fn native_empty_composer_supports_only_the_ordered_mode_flags() {
        for capture in [
            include_bytes!("../../tests/fixtures/approval/kimi-composer-yolo.ansi").as_slice(),
            include_bytes!("../../tests/fixtures/approval/kimi-composer-afk.ansi").as_slice(),
            include_bytes!("../../tests/fixtures/approval/kimi-composer-plan.ansi").as_slice(),
            include_bytes!("../../tests/fixtures/approval/kimi-composer-all.ansi").as_slice(),
        ] {
            let mut term = Terminal::new(80, 32);
            term.feed(capture);
            assert!(super::command_dismissed(&term));
        }
        let mut term = Terminal::new(80, 32);
        term.feed(include_bytes!(
            "../../tests/fixtures/approval/kimi-composer-plan.ansi"
        ));
        // Remove the plan flag but leave the plan header and cursor intact.
        term.feed(b"\x1b[31;1Hagent  \x1b[K\x1b[3;2H");
        assert!(!super::command_dismissed(&term));
        assert!(super::agent_toolbar_plan("plan  afk  agent").is_none());
        assert!(super::agent_toolbar_plan("afk  afk  agent").is_none());
    }

    #[test]
    fn native_one_time_shortcut_requires_the_complete_current_kimi_panel() {
        let mut term = Terminal::new(80, 32);
        term.feed(include_bytes!(
            "../../tests/fixtures/approval/kimi-command.ansi"
        ));
        let catalog = super::super::Catalog::load_from_paths(None, None, None);
        let evidence = super::super::status_evidence(&term, Some("kimi"));
        assert_eq!(
            catalog.screen("kimi", &evidence, "").unwrap().status,
            Some(uniterm_core::AgentStatus::Permission)
        );
        let grant = permission_approval("kimi", &term).unwrap();
        assert_eq!(grant.keys, b"1");
        assert_eq!(grant.kind, ApprovalKind::Permission);
        assert!(permission_approval("copilot", &term).is_none());

        let screen = term.evidence_text(32);
        let mut session_selected = Terminal::new(80, 32);
        session_selected.feed(include_bytes!(
            "../../tests/fixtures/approval/kimi-command.ansi"
        ));
        session_selected.feed(b"\x1b[6;3H\x1b[38;5;244m  [1] Approve once\x1b[0m");
        session_selected.feed("\x1b[7;3H\x1b[36m→ [2] Approve for this session\x1b[0m".as_bytes());
        let second = permission_approval("kimi", &session_selected).unwrap();
        assert_eq!(second.keys, b"1");
        assert_eq!(second.fingerprint, grant.fingerprint);
        assert_ne!(
            dialog(&screen.replace("pwd", "ls")).unwrap().fingerprint,
            grant.fingerprint
        );
        let toolbar = "──────────────────────\nagent (kimi ○)  ~/work\ncontext: 2.0%";
        assert!(dialog(&format!("{}\n{toolbar}", screen.trim_end())).is_some());

        let mut pasted = Terminal::new(80, 80);
        pasted.feed("── input ────────────────────\r\n".as_bytes());
        pasted.feed("draft line\r\n".repeat(60).as_bytes());
        pasted.feed(screen.trim_end().replace('\n', "\r\n").as_bytes());
        pasted.feed(format!("\r\n{}", toolbar.replace('\n', "\r\n")).as_bytes());
        assert!(!pasted.evidence_text(48).contains("── input"));
        assert!(permission_approval("kimi", &pasted).is_none());

        for invalid in [
            screen.replace("approval ─", "question ─"),
            screen.replace("is requesting approval to", "has already approved"),
            screen.replace("Approve once", "Approve for this session"),
            screen.replace("→ [1]", "  [1]"),
            screen.replace("  [2]", "→ [2]"),
            screen.replace(
                "[4] Reject, tell the model what to do instead",
                "→ [4] Reject: fix it",
            ),
            screen.replace(
                "▲/▼ select  1/2/3/4 choose  ↵ confirm",
                "Type your feedback, then press Enter to submit.",
            ),
            format!("{}\n── input ──\n❯ draft\n{toolbar}", screen.trim_end()),
            format!("{}\n❯ draft", screen.trim_end()),
        ] {
            assert!(dialog(&invalid).is_none(), "{invalid}");
        }
        term.feed(b"\r\nBlocked by policy");
        assert!(permission_approval("kimi", &term).is_none());
    }
}
