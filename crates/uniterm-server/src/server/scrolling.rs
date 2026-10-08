//! Event-driven viewport controls for Scrolling layout, confined to Pane content.
use super::*;
use uniterm_core::layout::{scrolling_columns, PaneLayout};

impl Server {
    pub(super) fn scrolling_bar_area(&self, content: Rect) -> Option<Rect> {
        let window = self.windows.get(self.active_window)?;
        (window.layout_mode == PaneLayout::Scrolling
            && window.zoomed.is_none()
            && self.today.is_none()
            && self.overview.is_none()
            && content.w >= 3
            && content.h >= 2)
            .then(|| Rect::new(content.x, content.bottom() - 1, content.w, 1))
    }

    fn scrolling_bar_layout(&self) -> Option<chrome::ScrollbarLayout> {
        let area = self.scrolling_bar_area(self.content_area().0)?;
        let window = &self.windows[self.active_window];
        Some(chrome::ScrollbarLayout::new(
            area,
            window.layout.pane_count(),
            scrolling_columns(area.w),
            window.layout_scroll,
        ))
    }

    pub(super) fn draw_scrolling_bar(&self, ops: &mut Vec<u8>) {
        let Some(bar) = self.scrolling_bar_layout() else {
            return;
        };
        let theme = &self.config.theme;
        let active = format!("\x1b[0;49;{}m", theme.muted.sgr_fg());
        ops.extend_from_slice(
            format!(
                "\x1b[{};{}H\x1b[0m{}",
                bar.area.y + 1,
                bar.area.x + 1,
                " ".repeat(usize::from(bar.area.w))
            )
            .as_bytes(),
        );
        ops.extend_from_slice(
            format!(
                "\x1b[{};{}H{active}{}\x1b[0m",
                bar.thumb.y + 1,
                bar.thumb.x + 1,
                "━".repeat(usize::from(bar.thumb.w))
            )
            .as_bytes(),
        );
    }

    fn scroll_columns_to(&mut self, reg: &Registry, requested: usize) {
        let Some(bar) = self.scrolling_bar_layout() else {
            return;
        };
        let start = requested.min(bar.max_scroll);
        let window = &mut self.windows[self.active_window];
        if window.layout_scroll == start {
            return;
        }
        let ids = window.layout.pane_ids();
        let active = ids.iter().position(|id| *id == window.active).unwrap_or(0);
        let end = (start + scrolling_columns(bar.area.w)).min(ids.len());
        let selected = active.clamp(start, end.saturating_sub(1));
        window.layout_scroll = start;
        if selected != active {
            // Keep keyboard focus visible through the same semantic Pane focus path.
            self.focus_pane_target(reg, ids[selected]);
        } else {
            self.relayout();
            self.full_repaint_all(reg);
        }
    }

    pub(super) fn handle_scrolling_mouse(
        &mut self,
        reg: &Registry,
        client: Token,
        x: u16,
        y: u16,
        kind: MouseKind,
    ) -> bool {
        let Some(bar) = self.scrolling_bar_layout() else {
            self.scrollbar_drag = None;
            return false;
        };
        if let Some((owner, anchor, grab)) = self.scrollbar_drag {
            if !self.windows[self.active_window]
                .layout
                .contains_pane(anchor)
            {
                self.scrollbar_drag = None;
            } else if owner == client {
                if kind == MouseKind::Release {
                    self.scrollbar_drag = None;
                    return true;
                }
                if kind == MouseKind::Drag {
                    self.scroll_columns_to(reg, bar.scroll_at(x, grab));
                    return true;
                }
                if kind == MouseKind::Click {
                    self.scrollbar_drag = None;
                }
            }
        }
        if !bar.area.contains(x, y) {
            return false;
        }
        let start = self.windows[self.active_window].layout_scroll;
        match kind {
            MouseKind::WheelUp | MouseKind::WheelLeft => {
                self.scroll_columns_to(reg, start.saturating_sub(1))
            }
            MouseKind::WheelDown | MouseKind::WheelRight => {
                self.scroll_columns_to(reg, start.saturating_add(1))
            }
            MouseKind::Click => {
                let grab = if bar.thumb.contains(x, y) {
                    x - bar.thumb.x
                } else {
                    bar.thumb.w / 2
                };
                self.scrollbar_drag = Some((client, self.windows[self.active_window].active, grab));
                if !bar.thumb.contains(x, y) {
                    self.scroll_columns_to(reg, bar.scroll_at(x, grab));
                }
            }
            _ => {}
        }
        true
    }
}
