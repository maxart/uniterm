//! A bounded launch forest drawn directly into Today's damage-tracked grid.
use super::*;
use std::time::{Duration, Instant};

impl Today {
    pub(super) fn compose_flow(
        &mut self,
        grid: &mut Grid,
        area: Rect,
        theme: &uniterm_core::Theme,
    ) {
        panel(
            grid,
            area,
            " Agent flow · Enter details · g go to Pane ",
            theme.border,
        );
        if area.w < 8 || area.h < 6 {
            return;
        }
        let capacity = usize::from(area.h.saturating_sub(2) / 5).max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + capacity {
            self.scroll = self.selected + 1 - capacity;
        }
        if self.flow.is_empty() {
            put(
                grid,
                area.x + 2,
                area.y + 2,
                &fit_cell_text(
                    "No Pane agents observed for this day/filter.",
                    area.w.saturating_sub(4) as usize,
                ),
                theme.muted,
                Color::Default,
            );
            return;
        }
        let card_width = area.w.saturating_sub(4).min(30);
        let step = card_width + 5;
        let levels = usize::from(area.w.saturating_sub(card_width + 4) / step);
        let rects: Vec<_> = self
            .flow
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(capacity)
            .map(|(index, node)| {
                (
                    index,
                    Rect::new(
                        area.x + 2 + node.depth.min(levels) as u16 * step,
                        area.y + 1 + (index - self.scroll) as u16 * 5,
                        card_width,
                        4,
                    ),
                )
            })
            .collect();
        // Draw routes before cards so every endpoint ends at a card boundary.
        for &(index, rect) in &rects {
            let node = &self.flow[index];
            let Some(parent) = node.parent else { continue };
            let child_color = uniterm_core::agent::agent_color_or_default(&node.span.last.provider);
            if let Some((_, source)) = rects.iter().find(|(id, _)| *id == parent) {
                let parent_color = uniterm_core::agent::agent_color_or_default(
                    &self.flow[parent].span.last.provider,
                );
                let mut route = Vec::new();
                if source.x < rect.x {
                    let trunk = rect.x - 3;
                    for x in source.right()..=trunk {
                        route.push((x, source.y + 2, '─'));
                    }
                    if let Some(last) = route.last_mut() {
                        last.2 = '┐';
                    }
                    for y in source.y + 3..rect.y + 2 {
                        route.push((trunk, y, '│'));
                    }
                    route.push((trunk, rect.y + 2, '└'));
                    for x in trunk + 1..rect.x {
                        route.push((x, rect.y + 2, '─'));
                    }
                } else {
                    // Narrow viewports retain explicit parent routing down the gutter.
                    let trunk = rect.x - 1;
                    route.push((trunk, source.y + 2, '┌'));
                    for y in source.y + 3..rect.y + 2 {
                        route.push((trunk, y, '│'));
                    }
                    route.push((trunk, rect.y + 2, '└'));
                }
                let length = route.len().max(1);
                for (offset, &(x, y, glyph)) in route.iter().enumerate() {
                    let color = blend(parent_color, child_color, offset, length);
                    let moving = self.pulse > 0
                        && node
                            .span
                            .last
                            .invocation
                            .is_some_and(|id| self.updated_invocations.contains(&id))
                        && offset == (usize::from(self.pulse - 1) * length / 8).min(length - 1);
                    put(
                        grid,
                        x,
                        y,
                        if moving {
                            "●"
                        } else {
                            match glyph {
                                '┐' => "┐",
                                '└' => "└",
                                '┌' => "┌",
                                '│' => "│",
                                _ => "─",
                            }
                        },
                        color,
                        Color::Default,
                    );
                }
            } else {
                put(
                    grid,
                    rect.x - 1,
                    rect.y + 2,
                    "↑",
                    child_color,
                    Color::Default,
                );
            }
        }
        for (index, rect) in rects {
            let node = &self.flow[index];
            let entry = &node.span.last;
            let color = uniterm_core::agent::agent_color_or_default(&entry.provider);
            let selected = index == self.selected;
            let bg = if selected {
                theme.surface
            } else {
                Color::Default
            };
            panel(grid, rect, "", color);
            let lines = [
                format!(
                    "{} {} · Pane {}",
                    if selected { "▸" } else { "●" },
                    uniterm_core::agent::agent_name(&entry.provider),
                    entry.context.pane.unwrap().0
                ),
                format!(
                    "{}{}",
                    if node.parent_missing {
                        "? parent unavailable · "
                    } else {
                        ""
                    },
                    entry.summary
                ),
            ];
            for (offset, line) in lines.iter().enumerate() {
                put(
                    grid,
                    rect.x + 1,
                    rect.y + 1 + offset as u16,
                    &fit_cell_text(
                        &sanitize_chrome_text(line, 512),
                        rect.w.saturating_sub(2) as usize,
                    ),
                    if offset == 0 { color } else { theme.muted },
                    bg,
                );
            }
            // A short update pulse also works for roots without incoming routes.
            if self.pulse > 0
                && entry
                    .invocation
                    .is_some_and(|id| self.updated_invocations.contains(&id))
            {
                let x = rect.x + 1 + u16::from(self.pulse - 1) * rect.w.saturating_sub(3) / 8;
                put(grid, x, rect.y, "●", color, Color::Default);
            }
            self.hits.push((rect, TodayHit::Row(index)));
        }
    }

    fn advance_flow(&mut self, now: Instant) -> bool {
        if self.animation_due.is_none_or(|due| due > now) {
            return false;
        }
        if self.mode != TodayMode::Flow || self.expanded {
            self.animation_due = None;
            self.pulse = 0;
            return false;
        }
        self.pulse = if self.pulse >= 8 { 0 } else { self.pulse + 1 };
        self.animation_due = (self.pulse > 0).then_some(now + Duration::from_millis(60));
        true
    }
}

fn blend(from: Color, to: Color, offset: usize, length: usize) -> Color {
    match (from, to) {
        (Color::Rgb(r, g, b), Color::Rgb(rr, gg, bb)) => {
            let mix = |a: u8, b: u8| {
                ((usize::from(a) * (length - offset) + usize::from(b) * offset) / length) as u8
            };
            Color::Rgb(mix(r, rr), mix(g, gg), mix(b, bb))
        }
        _ if offset * 2 < length => from,
        _ => to,
    }
}

impl Server {
    pub(in crate::server) fn today_animation_deadline(&self) -> Option<Instant> {
        let view = self.today.as_ref()?;
        let deadline = view.animation_due?;
        if view.mode != TodayMode::Flow
            || view.expanded
            || self.context_menu.is_some()
            || !self
                .clients
                .values()
                .any(|c| c.attached && !c.overlay && c.direct.is_none())
        {
            return None;
        }
        Some(deadline)
    }

    pub(in crate::server) fn flush_today_animation(&mut self, reg: &Registry) {
        if self.today_animation_deadline().is_none() {
            if let Some(view) = self.today.as_mut() {
                view.animation_due = None;
                view.pulse = 0;
            }
            return;
        }
        if self
            .today
            .as_mut()
            .is_some_and(|view| view.advance_flow(Instant::now()))
        {
            self.paint_today(reg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniterm_core::timeline::{TimelineEntry, TimelineKind};

    #[test]
    fn flow_cards_use_provider_colors_shared_hits_and_bounded_animation() {
        for theme in [uniterm_core::Theme::dark(), uniterm_core::Theme::light()] {
            let mut view = Today::new(Rect::new(0, 0, 140, 44));
            view.mode = TodayMode::Flow;
            for (index, provider) in uniterm_core::agent::PROVIDERS.iter().enumerate() {
                view.day.entries.push(TimelineEntry {
                    sequence: index as u64 + 1,
                    timestamp_ms: index as u64,
                    context: TimelineContext {
                        pane: Some(PaneId(index as u64 + 1)),
                        project_name: "日本語\x1b[2J".into(),
                        ..Default::default()
                    },
                    invocation: Some(index as u64 + 1),
                    provider: provider.id.into(),
                    session_id: None,
                    parent_session_id: None,
                    parent_invocation: (index > 0).then_some(1),
                    kind: TimelineKind::Agent,
                    summary: "Working 👩‍💻".into(),
                });
            }
            view.rebuild_rows();
            view.compose(&theme);
            for (rect, hit) in &view.hits {
                if let TodayHit::Row(index) = hit {
                    assert_eq!(
                        view.grid.get(rect.x, rect.y).fg,
                        uniterm_core::agent::PROVIDERS[*index].color
                    );
                    assert_eq!(view.grid.get(rect.x, rect.y).ch, '╭');
                    assert_eq!(view.grid.get(rect.right() - 1, rect.bottom() - 1).ch, '╯');
                }
            }
            let start = Instant::now();
            view.pulse = 1;
            view.animation_due = Some(start);
            view.compose(&theme);
            let mut frames = 0;
            for tick in 0..12 {
                view.grid.clear_damage();
                if view.advance_flow(start + Duration::from_millis(tick * 60)) {
                    view.compose(&theme);
                    assert!(view.grid.is_dirty());
                    frames += 1;
                }
            }
            assert_eq!(frames, 8);
            assert_eq!(view.animation_due, None);
            assert_eq!(view.pulse, 0);
            view.grid.clear_damage();
            let start = Instant::now();
            for _ in 0..100 {
                view.compose(&theme);
                assert!(!view.grid.is_dirty());
            }
            eprintln!("Flow 140x44 compose average: {:?}", start.elapsed() / 100);
            let mut output = Vec::new();
            Renderer::new().render_pane_damage(&view.grid, 0, 0, &mut output);
            assert!(output.is_empty());
            view.selected = view.rows.len() - 1;
            for (width, height) in [(1, 1), (20, 5), (40, 12), (80, 24)] {
                view.grid = Grid::new(width, height);
                view.compose(&theme);
                assert!(view
                    .hits
                    .iter()
                    .all(|(rect, _)| rect.right() <= width && rect.bottom() <= height));
                view.grid.clear_damage();
                view.compose(&theme);
                assert!(!view.grid.is_dirty());
            }
        }
    }
}
