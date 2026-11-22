//! Compact trace geometry shared by observed agents and durable runs.
use super::*;

/// Keep the inspector dock fixed as selection changes, including short terminals.
pub(super) fn regions(area: Rect) -> (Rect, Rect) {
    let dock = if area.h >= 12 {
        6
    } else if area.h >= 8 {
        4
    } else {
        0
    };
    (
        Rect::new(area.x, area.y, area.w, area.h - dock),
        Rect::new(area.x, area.bottom() - dock, area.w, dock),
    )
}

/// Locate selected ancestors once; siblings never acquire selection emphasis.
pub(super) fn selected_path(depths: &[usize], selected: usize) -> Vec<usize> {
    let Some(&depth) = depths.get(selected) else {
        return Vec::new();
    };
    let mut path = vec![selected];
    let mut needed = depth;
    for index in (0..selected).rev() {
        if depths[index] < needed {
            path.push(index);
            needed = depths[index];
        }
        if needed == 0 {
            break;
        }
    }
    path.reverse();
    path
}

/// A single factual item, with topology and state kept separate from evidence.
pub(super) struct TraceRow<'a> {
    pub rail: &'a str,
    pub depth: usize,
    pub index: usize,
    pub path: &'a [usize],
    pub selected: bool,
    pub glyph: &'a str,
    pub identity: &'a str,
    pub label: &'a str,
    pub context: &'a str,
    pub status: &'a str,
    pub evidence: &'a str,
    pub color: Color,
}

// Labels elide visibly; fitting remains grapheme- and display-cell-aware.
fn compact_text(text: &str, width: usize) -> String {
    let safe = sanitize_chrome_text(text, 8192);
    if safe.width() <= width {
        return safe;
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", fit_cell_text(&safe, width - 1).trim_end())
}

/// Reserve the right status/evidence lane before fitting a useful label.
/// All external text crosses the same sanitized display-cell boundary.
pub(super) fn trace_row(
    grid: &mut Grid,
    rect: Rect,
    row: TraceRow<'_>,
    theme: &uniterm_core::Theme,
) {
    if rect.w == 0 {
        return;
    }
    let bg = if row.selected {
        theme.surface
    } else {
        Color::Default
    };
    row_text(grid, rect, "", theme.foreground, bg);
    let safe = |s: &str| sanitize_chrome_text(s, 8192);
    let status = safe(row.status);
    let evidence = safe(row.evidence);
    let tail = if evidence.is_empty() {
        status
    } else {
        format!("{status} {evidence}")
    };
    let tail_w = tail
        .width()
        .min(22)
        .min(usize::from(rect.w.saturating_sub(14)));
    let tail_w = if tail_w < 4 { 0 } else { tail_w as u16 };
    let tail_x = rect.right() - tail_w;
    if tail_w > 0 {
        row_text(
            grid,
            Rect::new(tail_x, rect.y, tail_w, 1),
            &tail,
            row.color,
            bg,
        );
    }
    let end = tail_x.saturating_sub(u16::from(tail_w > 0));
    let rail_w = row.rail.width() as u16;
    let mut x = rect.x;
    if row.selected {
        put(grid, x, rect.y, "▎", theme.accent, bg);
    } else if row.path.contains(&row.index) {
        put(grid, x, rect.y, "▏", theme.accent, bg);
    }
    x += 1;
    let shown_levels = row.rail.chars().filter(|ch| *ch != '…').count() / 2;
    let omitted = row.depth.saturating_sub(shown_levels);
    for (offset, ch) in row.rail.chars().enumerate() {
        if x + offset as u16 >= end {
            break;
        }
        let level = omitted + offset.saturating_sub(usize::from(omitted > 0)) / 2;
        let lit = row
            .path
            .get(level)
            .zip(row.path.get(level + 1))
            .is_some_and(|(parent, child)| {
                *parent < row.index
                    && row.index <= *child
                    && (offset % 2 == usize::from(omitted > 0) || row.index == *child)
            });
        put(
            grid,
            x + offset as u16,
            rect.y,
            &ch.to_string(),
            if lit { theme.accent } else { theme.border },
            bg,
        );
    }
    x = x.saturating_add(rail_w).min(end);
    if x >= end {
        return;
    }
    put(grid, x, rect.y, row.glyph, row.color, bg);
    x = x.saturating_add(2).min(end);
    let identity = safe(row.identity);
    let identity_w = identity
        .width()
        .min(usize::from(end.saturating_sub(x)))
        .min(12) as u16;
    if identity_w > 0 {
        row_text(
            grid,
            Rect::new(x, rect.y, identity_w, 1),
            &identity,
            row.color,
            bg,
        );
        x = x.saturating_add(identity_w + 1).min(end);
    }
    let context = safe(row.context);
    let context_w = context.width().min(20) as u16;
    let context_w = if end.saturating_sub(x) > context_w + 24 {
        context_w
    } else {
        0
    };
    let label_end = end.saturating_sub(context_w + u16::from(context_w > 0));
    row_text(
        grid,
        Rect::new(x, rect.y, label_end.saturating_sub(x), 1),
        &compact_text(row.label, usize::from(label_end.saturating_sub(x))),
        theme.foreground,
        bg,
    );
    if context_w > 0 {
        row_text(
            grid,
            Rect::new(end - context_w, rect.y, context_w, 1),
            &context,
            theme.muted,
            bg,
        );
    }
}

/// Selected-item facts for a stable dock; complete identities remain in history.
pub(super) struct Inspector<'a> {
    pub identity: &'a str,
    pub status: &'a str,
    pub lines: &'a [String],
    pub footer: &'a str,
    pub color: Color,
}

// Paint only occupied spans over the border, retaining its continuous rail.
fn border_labels(grid: &mut Grid, area: Rect, y: u16, left: &str, right: &str, color: Color) {
    let room = usize::from(area.w.saturating_sub(2));
    if room == 0 {
        return;
    }
    let right = compact_text(right, (room / 2).saturating_sub(2));
    let right = if right.is_empty() {
        right
    } else {
        format!(" {right} ")
    };
    let right_w = right.width();
    let left = compact_text(left, room.saturating_sub(right_w + 5));
    let left = if left.is_empty() {
        left
    } else {
        format!(" {left} ")
    };
    let left_w = left.width();
    if left_w > 0 {
        row_text(
            grid,
            Rect::new(area.x + 1, y, left_w as u16, 1),
            &left,
            color,
            Color::Default,
        );
    }
    if right_w > 0 {
        row_text(
            grid,
            Rect::new(area.right() - 1 - right_w as u16, y, right_w as u16, 1),
            &right,
            color,
            Color::Default,
        );
    }
}

/// The compact dock keeps facts visible; v opens the complete scrolling history.
pub(super) fn inspector(
    grid: &mut Grid,
    area: Rect,
    item: Inspector<'_>,
    theme: &uniterm_core::Theme,
) {
    if area.h < 3 || area.w < 4 {
        return;
    }
    panel(grid, area, "", item.color);
    border_labels(grid, area, area.y, item.identity, item.status, item.color);
    for (offset, line) in item.lines.iter().take(usize::from(area.h - 2)).enumerate() {
        row_text(
            grid,
            Rect::new(area.x + 2, area.y + 1 + offset as u16, area.w - 4, 1),
            &compact_text(line, usize::from(area.w - 4)),
            if offset == 0 {
                theme.foreground
            } else {
                theme.muted
            },
            Color::Default,
        );
    }
    border_labels(
        grid,
        area,
        area.bottom() - 1,
        item.footer,
        "v history",
        item.color,
    );
}

/// Live status controls the node ink without implying successful completion.
pub(super) fn agent_state(
    status: AgentStatus,
    theme: &uniterm_core::Theme,
) -> (&'static str, Color) {
    match status {
        AgentStatus::Working | AgentStatus::Tool | AgentStatus::Starting => ("◎", theme.accent),
        AgentStatus::Permission | AgentStatus::Question => ("●", theme.warning),
        AgentStatus::Error => ("●", theme.error),
        AgentStatus::Idle => ("○", theme.muted),
        AgentStatus::Unknown | AgentStatus::Exited => ("·", theme.muted),
    }
}

/// Durable run state controls ink; artifact presence does not change its authority.
pub(super) fn run_state(
    status: uniterm_core::RunStatus,
    theme: &uniterm_core::Theme,
) -> (&'static str, Color) {
    match status {
        uniterm_core::RunStatus::Completed => ("●", theme.success),
        uniterm_core::RunStatus::Failed => ("●", theme.error),
        uniterm_core::RunStatus::Canceled | uniterm_core::RunStatus::Created => ("○", theme.muted),
        _ => ("◎", theme.accent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_rows_reserve_status_and_evidence_and_emphasize_only_selected_ancestry() {
        let theme = uniterm_core::Theme::dark();
        let depths = [0, 1, 2, 1];
        let path = selected_path(&depths, 3);
        assert_eq!(path, vec![0, 3]);
        assert_eq!(native::ancestry_rail(3, [3, 2, 1].into_iter(), 2), "…│ ├─");
        for width in [52, 80, 140] {
            let mut grid = Grid::new(width, 4);
            for (index, depth) in depths.iter().enumerate() {
                let rail = native::ancestry_rail(*depth, depths[index + 1..].iter().copied(), 8);
                trace_row(&mut grid, Rect::new(0, index as u16, width, 1), TraceRow {
                    rail: &rail, depth: *depth, index, path: &path, selected: index == 3,
                    glyph: "●", identity: "#42", label: "日本語 e\u{301} repeated title that needs clipping in a narrow viewport\x1b\x07",
                    context: "provider-neutral", status: "completed", evidence: "2 art", color: theme.success,
                }, &theme);
            }
            let text = fixture_text(&grid);
            assert!(!text.contains('\x1b'));
            assert!(!text.contains('\x07'));
            assert_eq!(grid.get(width - 1, 3).ch, 't');
            assert_eq!(grid.get(width - 5, 3).ch, '2');
            assert_eq!(grid.get(1, 1).fg, theme.accent, "selected ancestor trunk");
            assert_eq!(grid.get(2, 1).fg, theme.border, "unselected sibling branch");
            assert_eq!(grid.get(3, 2).fg, theme.border, "unselected nested branch");
            assert_eq!(grid.get(2, 3).fg, theme.accent, "selected branch");
            assert_eq!(grid.get(5, 3).fg, theme.success, "state-colored identity");
            assert_eq!(grid.get(0, 3).bg, theme.surface);
            assert_eq!(grid.get(0, 2).bg, Color::Default);
            if width == 52 {
                assert!(text.contains('…'));
            }
        }
        assert_eq!(compact_text("界", 1), "…");
        assert_eq!(compact_text("e\u{301}", 1), "e\u{301}");
    }

    #[test]
    fn inspector_keeps_state_and_action_right_anchored_without_erasing_border_rails() {
        let theme = uniterm_core::Theme::dark();
        for width in [12, 52, 80, 140] {
            let mut grid = Grid::new(width, 6);
            inspector(
                &mut grid,
                Rect::new(0, 0, width, 6),
                Inspector {
                    identity: "review",
                    status: "idle",
                    lines: &["Work 日本語".into()],
                    footer: "12:40",
                    color: theme.muted,
                },
                &theme,
            );
            assert_eq!(grid.get(0, 0).ch, '╭');
            assert_eq!(grid.get(width - 1, 0).ch, '╮');
            assert_eq!(grid.get(width - 2, 0).ch, ' ');
            assert_eq!(grid.get(width - 1, 5).ch, '╯');
            assert_eq!(grid.get(width - 2, 5).fg, theme.muted);
            let top = (1..width - 1).filter(|x| grid.get(*x, 0).ch == '─').count();
            assert!(top >= 3);
            if width >= 52 {
                assert_eq!(grid.get(width - 3, 0).ch, 'e');
                assert_eq!(grid.get(1, 1).ch, ' ');
                assert_eq!(grid.get(2, 1).ch, 'W');
                assert_eq!(grid.get(width - 3, 5).ch, 'y');
                assert_eq!(grid.get(width - 11, 5).ch, 'v');
            }
        }
    }

    #[test]
    fn dock_geometry_is_fixed_and_keeps_rows_available_at_small_sizes() {
        for height in 0..50 {
            let area = Rect::new(2, 4, 52, height);
            let (trace, dock) = regions(area);
            assert_eq!(trace.bottom(), dock.y);
            assert_eq!(dock.bottom(), area.bottom());
            assert!(dock.h == 0 || trace.h >= 4);
        }
    }
}
