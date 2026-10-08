//! Tab ancestry shared by explicit grouping, agent launches, and chrome.
use super::*;

impl Server {
    pub(super) fn tab_parent(&self, window: usize) -> Option<usize> {
        let tab = self.windows.get(window)?;
        let pane = tab.stack_parent?;
        self.windows
            .iter()
            .position(|parent| parent.project == tab.project && parent.layout.contains_pane(pane))
    }

    pub(super) fn stack_root(&self, mut window: usize) -> usize {
        for _ in 0..self.windows.len() {
            let Some(parent) = self.tab_parent(window) else {
                break;
            };
            if parent == window {
                break;
            }
            window = parent;
        }
        window
    }

    pub(super) fn root_tabs(&self) -> Vec<usize> {
        self.project_window_indices(self.active_project)
            .into_iter()
            .filter(|window| self.tab_parent(*window).is_none())
            .collect()
    }

    pub(super) fn stack_tabs(&self) -> Vec<usize> {
        if self.today.is_some() || self.windows.is_empty() {
            return Vec::new();
        }
        let root = self.stack_root(self.active_window);
        let mut tabs = vec![root];
        for window in self.project_window_indices(self.active_project) {
            if window != root && self.stack_root(window) == root {
                tabs.push(window);
            }
        }
        if tabs.len() == 1 {
            tabs.clear();
        }
        tabs
    }

    /// Reject cross-Project links and cycles before changing a projection.
    pub(super) fn assign_tab_parent(&mut self, pane: PaneId, parent: Option<PaneId>) -> bool {
        let Some(child) = self
            .windows
            .iter()
            .position(|w| w.layout.contains_pane(pane))
        else {
            return false;
        };
        if let Some(parent) = parent {
            let Some(mut ancestor) = self.windows.iter().position(|w| {
                w.layout.contains_pane(parent) && w.project == self.windows[child].project
            }) else {
                return false;
            };
            for _ in 0..=self.windows.len() {
                if ancestor == child {
                    return false;
                }
                match self.tab_parent(ancestor) {
                    Some(next) => ancestor = next,
                    None => break,
                }
            }
        }
        self.append_event(crate::eventlog::LogEvent::TabConfigured {
            pane,
            layout_mode: self.windows[child].layout_mode,
            stack_parent: parent,
        });
        self.windows[child].stack_parent = parent;
        true
    }

    pub(super) fn set_tab_parent(
        &mut self,
        reg: &Registry,
        pane: PaneId,
        parent: Option<PaneId>,
    ) -> bool {
        let previous = self
            .windows
            .iter()
            .find(|w| w.layout.contains_pane(pane))
            .map(|w| w.stack_parent);
        if previous == Some(parent) {
            return true;
        }
        if !self.assign_tab_parent(pane, parent) {
            return false;
        }
        self.tab_scroll_follow_active = true;
        self.stack_scroll = 0;
        self.persist();
        self.relayout();
        self.full_repaint_all(reg);
        true
    }

    /// Keep a parent Tab identity when its anchor Pane disappears; promote
    /// children to its own parent when the final Pane closes.
    pub(super) fn repair_stack_anchor(&mut self, pane: PaneId) {
        let Some(source) = self.windows.iter().find(|w| w.layout.contains_pane(pane)) else {
            return;
        };
        let replacement = source
            .layout
            .pane_ids()
            .into_iter()
            .find(|id| *id != pane)
            .or(source.stack_parent);
        let children: Vec<_> = self
            .windows
            .iter()
            .filter(|tab| tab.stack_parent == Some(pane))
            .map(|tab| tab.active)
            .collect();
        for child in children {
            self.assign_tab_parent(child, replacement);
        }
    }
}

impl Server {
    pub(super) fn stack_tab_layout(&self, follow: bool) -> chrome::TabBarLayout {
        let tabs = self.stack_tabs();
        if tabs.is_empty() || !self.config.status || self.rows < 4 {
            return chrome::TabBarLayout::default();
        }
        let y = if self.config.status_position == StatusPosition::Top {
            1
        } else {
            self.rows - 2
        };
        let x = self.sidebar_width();
        let width = self
            .cols
            .saturating_sub(x)
            .saturating_sub(self.observatory_width());
        let active = tabs
            .iter()
            .position(|window| *window == self.active_window)
            .unwrap_or(0);
        chrome::tab_bar_layout(
            Rect::new(x, y, width, 1),
            tabs.len(),
            active,
            self.stack_scroll,
            follow,
        )
    }

    pub(super) fn draw_stack_tabs(&self, ops: &mut Vec<u8>) {
        let layout = self.stack_tab_layout(false);
        let Some(new) = layout.new_tab else {
            return;
        };
        let theme = &self.config.theme;
        let base = format!(
            "\x1b[0;{};{}m",
            theme.status_bg.sgr_bg(),
            theme.status_fg.sgr_fg()
        );
        ops.extend_from_slice(
            format!(
                "\x1b[{};{}H{base}{}",
                new.y + 1,
                self.sidebar_width() + 1,
                " ".repeat(usize::from(
                    self.cols
                        .saturating_sub(self.sidebar_width())
                        .saturating_sub(self.observatory_width())
                ))
            )
            .as_bytes(),
        );
        let windows = self.stack_tabs();
        let project = self.project_window_indices(self.active_project);
        for slot in &layout.tabs {
            let window = windows[slot.item];
            let ordinal = project
                .iter()
                .position(|index| *index == window)
                .unwrap_or(0)
                + 1;
            let name = self.windows[window].name.as_deref().unwrap_or("Tab");
            let mut depth = 0;
            let mut ancestor = window;
            while ancestor != windows[0] && depth < 3 {
                let Some(parent) = self.tab_parent(ancestor) else {
                    break;
                };
                ancestor = parent;
                depth += 1;
            }
            let prefix = if depth == 0 {
                "↑".to_owned()
            } else {
                "↳".repeat(depth)
            };
            let label = format!(
                "{prefix} {ordinal}:{}",
                super::chrome_ui::sanitize_chrome_text(name, 512)
            );
            let style = if window == self.active_window {
                format!(
                    "\x1b[1;{};{}m",
                    theme.accent_muted.sgr_bg(),
                    theme.foreground.sgr_fg()
                )
            } else {
                base.clone()
            };
            ops.extend_from_slice(
                format!(
                    "\x1b[{};{}H{style}{}",
                    slot.rect.y + 1,
                    slot.rect.x + 1,
                    super::chrome_ui::fit_cell_text(&label, slot.rect.w as usize)
                )
                .as_bytes(),
            );
        }
        for (rect, label) in [
            (layout.scroll_left, " < "),
            (layout.scroll_right, " > "),
            (Some(new), " + "),
        ] {
            if let Some(rect) = rect {
                ops.extend_from_slice(
                    format!(
                        "\x1b[{};{}H{base}{}",
                        rect.y + 1,
                        rect.x + 1,
                        super::chrome_ui::fit_cell_text(label, rect.w as usize)
                    )
                    .as_bytes(),
                );
            }
        }
        ops.extend_from_slice(b"\x1b[0m");
    }

    pub(super) fn handle_stack_mouse(
        &mut self,
        reg: &Registry,
        client: Token,
        x: u16,
        y: u16,
        kind: MouseKind,
    ) -> bool {
        if x < self.sidebar_width() || x >= self.cols.saturating_sub(self.observatory_width()) {
            return false;
        }
        let layout = self.stack_tab_layout(false);
        let Some(new) = layout.new_tab.filter(|rect| rect.y == y) else {
            return false;
        };
        let left = matches!(kind, MouseKind::WheelUp | MouseKind::WheelLeft)
            || kind == MouseKind::Click
                && layout.scroll_left.is_some_and(|rect| rect.contains(x, y));
        let right = matches!(kind, MouseKind::WheelDown | MouseKind::WheelRight)
            || kind == MouseKind::Click
                && layout.scroll_right.is_some_and(|rect| rect.contains(x, y));
        if left || right {
            self.stack_scroll = if left {
                self.stack_scroll.saturating_sub(1)
            } else {
                self.stack_scroll.saturating_add(1)
            };
            self.tab_scroll_follow_active = false;
            self.stack_scroll = self.stack_tab_layout(false).scroll;
            if self.stack_scroll != layout.scroll {
                self.repaint_chrome_all(reg);
            }
            return true;
        }
        if kind == MouseKind::Click && new.contains(x, y) {
            let anchor = self.windows[self.active_window].active;
            if let Some(pane) = self.create_tab(reg, self.active_project, false) {
                self.set_tab_parent(reg, pane, Some(anchor));
            }
            return true;
        }
        if let Some(slot) = layout.tabs.iter().find(|slot| slot.rect.contains(x, y)) {
            let window = self.stack_tabs()[slot.item];
            if matches!(kind, MouseKind::Click | MouseKind::RightClick) {
                let pane = self.windows[window].active;
                self.focus_pane_target(reg, pane);
                if kind == MouseKind::Click {
                    self.tab_drag = Some((client, pane));
                } else {
                    self.request_chrome_menu(
                        reg,
                        client,
                        uniterm_proto::ChromeMenu::Tabs,
                        slot.rect,
                        self.config.status_position == StatusPosition::Bottom,
                    );
                }
            }
        }
        true
    }
}
