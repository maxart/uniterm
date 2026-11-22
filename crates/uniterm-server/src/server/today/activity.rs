//! Observations and agent ancestry share one context-rich, bounded Activity list.
use super::*;

impl Today {
    /// Keep each observation and its context in one shared paint/hit rectangle.
    pub(super) fn compose_activity(
        &mut self,
        grid: &mut Grid,
        area: Rect,
        theme: &uniterm_core::Theme,
    ) {
        panel(
            grid,
            area,
            " Activity · projects / agents / observations ",
            theme.border,
        );
        if area.w < 4 || area.h < 5 {
            return;
        }
        let times: std::collections::HashMap<_, _> = self
            .day
            .entries
            .iter()
            .zip(&self.day.times)
            .map(|(entry, time)| (entry.sequence, time.as_str()))
            .collect();
        let capacity = usize::from(area.h.saturating_sub(2) / 3);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }
        if self.selected >= self.scroll + capacity {
            self.scroll = self.selected + 1 - capacity;
        }
        if self.rows.is_empty() {
            row_text(
                grid,
                Rect::new(area.x + 2, area.y + 2, area.w - 4, 1),
                "No activity recorded for this day/filter.",
                theme.muted,
                Color::Default,
            );
        }
        for (index, span) in self
            .rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(capacity)
        {
            let e = &span.last;
            let rect = Rect::new(
                area.x + 1,
                area.y + 1 + (index - self.scroll) as u16 * 3,
                area.w - 2,
                3,
            );
            let selected = self.selected == index;
            let bg = if selected {
                theme.surface
            } else {
                Color::Default
            };
            let color = if e.provider.is_empty() {
                theme.muted
            } else {
                uniterm_core::agent::agent_color_or_default(&e.provider)
            };
            let (depth, missing) = self.ancestry.get(index).copied().unwrap_or_default();
            let project = if e.context.project_name.is_empty() {
                "Workspace"
            } else {
                &e.context.project_name
            };
            let kind = match e.kind {
                uniterm_core::timeline::TimelineKind::Agent => {
                    uniterm_core::agent::agent_name(&e.provider)
                }
                uniterm_core::timeline::TimelineKind::Visit => "Project visit",
                uniterm_core::timeline::TimelineKind::Note => "Note",
                uniterm_core::timeline::TimelineKind::Prompt => "Prompt",
                uniterm_core::timeline::TimelineKind::Boundary => "History boundary",
            };
            let ancestry_marker = if depth > 0 {
                format!(
                    "{}└─{} ",
                    "│ ".repeat(depth.saturating_sub(1).min(3)),
                    if depth > 4 {
                        format!(" {depth}")
                    } else {
                        String::new()
                    }
                )
            } else if missing {
                "? ".into()
            } else {
                "● ".into()
            };
            let title = format!(
                "{} {}{} · {}{}",
                if selected { "▸" } else { " " },
                ancestry_marker,
                project,
                kind,
                e.session_id
                    .as_ref()
                    .map_or_else(String::new, |session| format!(" · {session}"))
            );
            // A compact observation rail supplements context only when width permits.
            let rail_width = if rect.w >= 104 { 22 } else { 0 };
            row_text(
                grid,
                Rect::new(rect.x, rect.y, rect.w - rail_width, 1),
                &title,
                color,
                bg,
            );
            if rail_width > 0 {
                let rail = Rect::new(rect.right() - rail_width, rect.y, rail_width, 1);
                row_text(grid, rail, "", theme.muted, bg);
                if e.timestamp_ms < self.day.start_ms {
                    row_text(grid, rail, "carried over", theme.muted, bg);
                } else {
                    let duration = self.day.end_ms.saturating_sub(self.day.start_ms).max(1);
                    let pos = |ms: u64| {
                        ((u128::from(ms.saturating_sub(self.day.start_ms).min(duration))
                            * u128::from(rail_width - 1))
                            / u128::from(duration)) as u16
                    };
                    for x in rail.x..rail.right() {
                        put(grid, x, rail.y, "┄", theme.border, bg);
                    }
                    for x in pos(span.first_ms)..=pos(e.timestamp_ms).max(pos(span.first_ms)) {
                        put(grid, rail.x + x, rail.y, "━", color, bg);
                    }
                }
            }
            let path = format!(
                "  {}{}",
                if e.context.root.is_empty() {
                    "Path not recorded"
                } else {
                    &e.context.root
                },
                if e.context.branch.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", e.context.branch)
                }
            );
            row_text(
                grid,
                Rect::new(rect.x, rect.y + 1, rect.w, 1),
                &path,
                theme.muted,
                bg,
            );
            let ancestry = if missing {
                " · ? parent outside this page".into()
            } else if let Some(parent) = &e.parent_session_id {
                format!(" · parent {parent}")
            } else if let Some(parent) = e.parent_invocation {
                format!(" · launch #{parent}")
            } else {
                String::new()
            };
            let time = times.get(&e.sequence).map_or_else(
                || {
                    if e.timestamp_ms < self.day.start_ms {
                        "earlier day".into()
                    } else {
                        format!("{} ms UTC", e.timestamp_ms)
                    }
                },
                |time| (*time).to_string(),
            );
            let identity = if e.kind == uniterm_core::timeline::TimelineKind::Agent
                && e.invocation.is_none()
            {
                " · child session; liveness unknown".into()
            } else {
                e.context
                    .pane
                    .map_or_else(String::new, |pane| format!(" · Pane {}", pane.0))
            };
            let context = format!(
                "  {time} · observed: {}{identity}{ancestry} · {} observations",
                e.summary, span.observations
            );
            row_text(
                grid,
                Rect::new(rect.x, rect.y + 2, rect.w, 1),
                &context,
                theme.foreground,
                bg,
            );
            self.hits.push((rect, TodayHit::Row(index)));
        }
    }
}
