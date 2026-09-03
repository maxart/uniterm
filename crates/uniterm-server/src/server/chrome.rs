//! Chrome geometry and painting: the status line, sidebars, the overview,
//! notifications, and the repaint paths that push them to clients.
//!
//! Everything here is damage-driven; nothing redraws on a timer.

use super::*;

impl Server {
    /// The status-line text for each window: ` i ` or ` i:name ` (name
    /// clipped), with `i` shown 1-based to match the prefix digit bindings.
    /// Shared by status painting and [`chrome::tab_bar_layout`] hit testing.
    pub(super) fn window_segments(&self) -> Vec<String> {
        self.windows
            .iter()
            .filter(|window| window.project == self.active_project)
            .enumerate()
            .map(|(i, w)| {
                let num = i + 1;
                let core = match &w.name {
                    Some(name) => format!("{num}:{name}"),
                    None => num.to_string(),
                };
                let width = core.width().saturating_add(4).max(8);
                fit_centered_ellipsis(&core, width)
            })
            .collect()
    }

    pub(super) fn render_session_template(&self, template: &str) -> Option<String> {
        let window = self.windows.get(self.active_window)?;
        let pane = self.panes.get(&window.active)?;
        let project = self
            .projects
            .iter()
            .find(|project| project.id == self.active_project)?;
        let tabs = self.project_window_indices(self.active_project);
        let tab_index = tabs
            .iter()
            .position(|index| *index == self.active_window)
            .unwrap_or(0);
        let tab = window
            .name
            .clone()
            .unwrap_or_else(|| (tab_index + 1).to_string());
        let pane_id = window.active.0.to_string();
        let agent = pane.agent.as_ref().map_or("", |agent| agent.id.as_str());
        let status = pane.agent.as_ref().map_or("", |agent| agent.status.label());
        let zoom = if window.zoomed.is_some() { "ZOOM" } else { "" };
        render_named_template(template, |token| match token {
            "hostname" => Some(self.hostname.as_str()),
            "workspace" => Some(self.name.as_str()),
            "project" => Some(project.name.as_str()),
            "tab" => Some(tab.as_str()),
            "pane" => Some(pane_id.as_str()),
            "terminal_title" => Some(pane.term.terminal_title()),
            "agent" => Some(agent),
            "agent_status" => Some(status),
            "zoom" => Some(zoom),
            _ => None,
        })
    }

    pub(super) fn window_title(&self) -> Option<String> {
        (!self.config.window_title.is_empty())
            .then(|| self.render_session_template(&self.config.window_title))
            .flatten()
            .map(|title| sanitize_chrome_text(&title, 512))
    }

    pub(super) fn status_right_text(&self) -> String {
        self.render_session_template(&self.config.status_right)
            .map(|text| sanitize_chrome_text(text.trim(), 48))
            .unwrap_or_default()
    }

    pub(super) fn status_right_rect(&self) -> Option<Rect> {
        let (_, row) = self.chrome_area();
        let row = row?;
        let text = self.status_right_text();
        let width = u16::try_from(text.width()).ok()?.min(48);
        if width == 0 {
            return None;
        }
        let left = self.workspace_button_width().saturating_add(1);
        let right = self.cols.saturating_sub(self.observatory_width());
        let x = right.saturating_sub(width);
        (x.saturating_sub(left) >= 11).then_some(Rect::new(x, row, width, 1))
    }

    pub(super) fn workspace_button(&self) -> String {
        let width = usize::from(self.workspace_button_width());
        if width < 3 {
            return fit_cell_text("\u{25BE}", width);
        }
        let name = if self.durability_error.is_some() {
            format!("! {}", self.name)
        } else {
            self.name.clone()
        };
        format!(
            " {}\u{25BE} ",
            fit_cell_text(&name, width.saturating_sub(3))
        )
    }

    /// Width of the colored Workspace button. The following cell continues
    /// the Projects rail divider through the status row.
    pub(super) fn workspace_button_width(&self) -> u16 {
        let sidebar = self.sidebar_width();
        if sidebar > 0 {
            sidebar.saturating_sub(1)
        } else {
            u16::try_from(self.name.width().saturating_add(4)).unwrap_or(u16::MAX)
        }
    }

    pub(super) fn tab_bar_layout(&self, follow_active: bool) -> chrome::TabBarLayout {
        let (_, status_row) = self.chrome_area();
        let Some(row) = status_row else {
            return chrome::TabBarLayout::default();
        };
        let x = self
            .workspace_button_width()
            .saturating_add(1)
            .min(self.cols);
        let right = self
            .status_right_rect()
            .map_or_else(
                || self.cols.saturating_sub(self.observatory_width()),
                |rect| rect.x,
            )
            .max(x);
        let area = Rect::new(x, row, right.saturating_sub(x), 1);
        let segments = self.window_segments();
        let widths: Vec<u16> = segments
            .iter()
            .map(|segment| u16::try_from(segment.width()).unwrap_or(u16::MAX))
            .collect();
        let active = self
            .project_window_indices(self.active_project)
            .iter()
            .position(|window| *window == self.active_window)
            .unwrap_or(0);
        chrome::tab_bar_layout(area, &widths, active, self.tab_scroll, follow_active)
    }

    pub(super) fn observatory_tab_slots(&self) -> Vec<(ObservatoryTab, Rect, &'static str)> {
        let width = self.observatory_width();
        let (_, status_row) = self.chrome_area();
        let Some(row) = status_row.filter(|_| width > 0) else {
            return Vec::new();
        };
        let labels = ["Agents", "Files", "Servers"];
        let content_width = width.saturating_sub(1);
        let area = Rect::new(
            self.cols.saturating_sub(width).saturating_add(1),
            row,
            content_width,
            1,
        );
        ObservatoryTab::ALL
            .into_iter()
            .zip(chrome::equal_segments(area, ObservatoryTab::ALL.len()))
            .zip(labels)
            .map(|((tab, rect), label)| (tab, rect, label))
            .collect()
    }

    pub(super) fn observatory_agent_action_slots(
        &self,
    ) -> Vec<(uniterm_proto::ChromeAction, Rect, &'static str)> {
        let width = self.observatory_width();
        let (area, _) = self.chrome_area();
        if width <= 1 || area.h < 2 {
            return Vec::new();
        }
        let actions = [
            (uniterm_proto::ChromeAction::NewTask, "New Task"),
            (uniterm_proto::ChromeAction::Tasks, "Tasks..."),
            (uniterm_proto::ChromeAction::Config, "Config"),
        ];
        let buttons = Rect::new(
            self.cols.saturating_sub(width).saturating_add(1),
            area.bottom().saturating_sub(1),
            width.saturating_sub(1),
            1,
        );
        actions
            .into_iter()
            .zip(chrome::equal_segments_with_gap(buttons, actions.len(), 1))
            .map(|((action, label), rect)| (action, rect, label))
            .collect()
    }

    /// The area available to panes (the screen minus the status line, if any)
    /// and the status line's row.
    pub(super) fn chrome_area(&self) -> (Rect, Option<u16>) {
        let sidebar = self.sidebar_width();
        let file_sidebar = self.observatory_width();
        let width = self
            .cols
            .saturating_sub(sidebar)
            .saturating_sub(file_sidebar)
            .max(1);
        if !self.config.status || self.rows < 2 {
            return (Rect::new(sidebar, 0, width, self.rows), None);
        }
        match self.config.status_position {
            StatusPosition::Bottom => (
                Rect::new(sidebar, 0, width, self.rows - 1),
                Some(self.rows - 1),
            ),
            StatusPosition::Top => (Rect::new(sidebar, 1, width, self.rows - 1), Some(0)),
        }
    }

    pub(super) fn sidebar_width(&self) -> u16 {
        if !self.config.sidebar || self.cols < 72 || self.rows < 8 {
            return 0;
        }
        let preferred = if self.cols < 100 {
            18
        } else {
            self.config.sidebar_width
        };
        preferred.clamp(16, 40).min(self.cols.saturating_sub(48))
    }

    pub(super) fn observatory_width(&self) -> u16 {
        if !self.config.file_sidebar || self.cols < 88 || self.rows < 8 {
            return 0;
        }
        let left = self.sidebar_width();
        let available = self.cols.saturating_sub(left);
        self.config
            .file_sidebar_width
            .clamp(22, 52)
            .min(available.saturating_sub(40))
    }

    pub(super) fn file_sidebar_rows(&self, area: Rect) -> FileSidebarRows {
        file_sidebar_rows_for(
            area,
            self.files
                .git_stats()
                .is_some_and(|stats| stats.has_changes()),
        )
    }

    pub(super) fn sync_file_viewport(&mut self) {
        if !self.file_manager_visible() {
            return;
        }
        let (area, _) = self.chrome_area();
        let capacity = self.file_sidebar_rows(area).capacity();
        self.files.sync_viewport(capacity);
    }

    pub(super) fn sync_chrome_viewports(&mut self) {
        if self.sidebar_width() > 0 {
            self.project_scroll = self.project_slots().first().map_or(0, |slot| slot.item);
        }
        if self.observatory_width() > 0 {
            let agents = self.observatory_agent_entries();
            let agent_index = ObservatoryTab::Agents.index();
            self.observatory_scroll[agent_index] = self
                .observatory_agent_slots(agents.len())
                .first()
                .map_or(0, |slot| slot.item);
            let servers = self.observatory_dev_server_entries();
            let web_index = ObservatoryTab::WebServers.index();
            self.observatory_scroll[web_index] = self
                .observatory_web_slots(servers.len())
                .first()
                .map_or(0, |slot| slot.item);
        }
        let tabs = self.tab_bar_layout(self.tab_scroll_follow_active);
        self.tab_scroll = tabs.scroll;
        self.tab_scroll_follow_active = false;
    }

    pub(super) fn sidebar_project_start(&self) -> u16 {
        // Keep one blank row between the muted heading and its first card.
        self.chrome_area().0.y.saturating_add(3)
    }

    pub(super) fn project_slots(&self) -> Vec<chrome::CardSlot> {
        let (area, _) = self.chrome_area();
        chrome::project_card_slots(
            self.sidebar_project_start(),
            area.bottom(),
            self.projects.len(),
            self.project_scroll,
        )
    }

    pub(super) fn observatory_agent_slots(&self, total: usize) -> Vec<chrome::CardSlot> {
        let (area, _) = self.chrome_area();
        chrome::card_slots(
            area.y.saturating_add(3),
            area.bottom().saturating_sub(2),
            total,
            self.observatory_scroll[ObservatoryTab::Agents.index()],
        )
    }

    pub(super) fn observatory_web_slots(&self, total: usize) -> Vec<chrome::CardSlot> {
        let (area, _) = self.chrome_area();
        chrome::card_slots(
            area.y.saturating_add(3),
            area.bottom(),
            total,
            self.observatory_scroll[ObservatoryTab::WebServers.index()],
        )
    }

    pub(super) fn observatory_scope_button(&self, scope: SidebarScope) -> (u16, u16, &'static str) {
        let width = self.observatory_width();
        let inner = width.saturating_sub(1) as usize;
        let label = scope.label(inner);
        let label_width = u16::try_from(label.len()).unwrap_or(u16::MAX);
        let x = self.cols.saturating_sub(label_width).saturating_sub(1);
        (x, label_width, label)
    }

    pub(super) fn file_manager_visible(&self) -> bool {
        self.observatory_tab == ObservatoryTab::Files && self.observatory_width() > 0
    }

    /// Recompute the active window's layout and resize its panes' terminals and
    /// PTYs to match. A zoomed window lays its zoomed pane over the full area.
    pub(super) fn relayout(&mut self) {
        let (area, _) = self.chrome_area();
        let win = &self.windows[self.active_window];
        let layout = match win.zoomed {
            Some(z) => {
                let mut l = Layout::default();
                l.panes.push((z, area));
                l
            }
            None => win.layout.compute(area),
        };
        for (pid, rect) in &layout.panes {
            if let Some(pane) = self.panes.get_mut(pid) {
                pane.term.resize(rect.w, rect.h);
                if let Some(copy) = pane.copy.as_mut() {
                    copy.resize(pane.term.grid(), *rect);
                }
                let _ = pane.pty.resize(rect.w, rect.h);
            }
        }
        self.current_layout = layout;
    }

    /// Use the smallest attached viewport so a shared layout fits every
    /// client. Larger clients receive the same deterministic canvas with the
    /// remainder cleared, rather than silently resizing all peers on each
    /// individual WINCH.
    pub(super) fn recompute_client_geometry(&mut self) -> bool {
        let geometry = self
            .clients
            .values()
            .filter(|client| client.attached && !client.dead)
            .map(|client| (client.cols.max(1), client.rows.max(1)))
            .reduce(|(aw, ah), (bw, bh)| (aw.min(bw), ah.min(bh)));
        let Some((cols, rows)) = geometry else {
            return false;
        };
        if (cols, rows) == (self.cols, self.rows) {
            return false;
        }
        self.cols = cols;
        self.rows = rows;
        true
    }

    /// Tell attached clients why a refused destructive request did nothing.
    pub(super) fn show_guardrail_toast(&mut self, reg: &Registry, body: &str) {
        let pane = self.windows[self.active_window].active;
        self.notification = Some(AgentToast {
            pane,
            title: "Guardrail".into(),
            body: body.into(),
            expires: std::time::Instant::now() + std::time::Duration::from_secs(8),
        });
        self.full_repaint_all(reg);
    }

    pub(super) fn sync_window_titles(&mut self, reg: &Registry) {
        let Some(title) = self.window_title() else {
            return;
        };
        if self.last_broadcast_title.as_deref() == Some(title.as_str()) {
            return;
        }
        let encoded = encode_frame(&ServerMessage::WindowTitle {
            title: title.clone(),
        });
        for (token, client) in &mut self.clients {
            if !client.attached {
                continue;
            }
            client.queue(&encoded);
            client.flush();
            let _ = set_interest(reg, client, *token);
        }
        self.last_broadcast_title = Some(title);
    }

    /// Build a complete frame for the active window: clear, paint every visible
    /// pane, draw dividers, place the cursor. Identical for every client (each
    /// client's renderer is invalidated around it), so we build it once.
    pub(super) fn build_full_frame(&self) -> Vec<u8> {
        if let Some(sel) = self.overview {
            return self.build_overview_frame(sel);
        }
        let mut r = Renderer::new();
        let mut ops = Vec::new();
        // Reset scroll region + clear before painting, so a full repaint never
        // inherits a leftover margin from the client's prior terminal state.
        ops.extend_from_slice(b"\x1b[r\x1b[2J");
        for (pid, rect) in &self.current_layout.panes {
            if let Some(pane) = self.panes.get(pid) {
                match &pane.copy {
                    Some(copy) => copy.render(pane.term.grid(), *rect, &mut ops),
                    None => {
                        // Child terminal content is never recoloured to express
                        // focus. The semantic divider and active chrome carry
                        // that state without corrupting application palettes.
                        r.set_dim(false);
                        r.render_pane_full(pane.term.grid(), rect.x, rect.y, &mut ops);
                    }
                }
            }
        }
        draw_dividers(&self.current_layout, self.config.theme.divider, &mut ops);
        self.draw_sidebar(&mut ops);
        self.draw_observatory_sidebar(&mut ops);
        self.draw_status(&mut ops);
        self.draw_notification(&mut ops);
        if let Some(menu) = &self.context_menu {
            menu.render(&self.config.theme, self.cols, self.rows, &mut ops);
        }
        self.append_cursor(&mut ops);
        ops
    }

    pub(super) fn draw_sidebar(&self, ops: &mut Vec<u8>) {
        let width = self.sidebar_width();
        if width == 0 {
            return;
        }
        let (area, _) = self.chrome_area();
        // The rail and inactive Projects belong to the terminal canvas, so
        // they follow the host terminal's foreground and background.
        let base = "\x1b[0;39;49m";
        let muted = "\x1b[0;2;39;49m";
        let border = format!("\x1b[0;{};49m", self.config.theme.divider.sgr_fg());
        let active = format!(
            "\x1b[0;1;{};{}m",
            self.config.theme.status_active_fg.sgr_fg(),
            self.config.theme.selection_bg.sgr_bg()
        );
        let active_detail = format!(
            "\x1b[0;2;{};{}m",
            self.config.theme.status_active_fg.sgr_fg(),
            self.config.theme.selection_bg.sgr_bg()
        );
        let inner = width.saturating_sub(1) as usize;
        for row in area.y..area.bottom() {
            ops.extend_from_slice(format!("\x1b[{};1H{base}", row + 1).as_bytes());
            ops.extend(std::iter::repeat_n(b' ', inner));
            ops.extend_from_slice(format!("{border}\u{2502}").as_bytes());
        }
        let write_row = |ops: &mut Vec<u8>, row: u16, text: &str, style: &str| {
            if row >= area.bottom() {
                return;
            }
            let fitted = fit_cell_text(text, inner);
            ops.extend_from_slice(format!("\x1b[{};1H{style}{fitted}", row + 1).as_bytes());
        };
        let heading = format!("\x1b[0;2;{};49m", self.config.theme.muted.sgr_fg());
        let project_slots = self.project_slots();
        let first = project_slots.first().map_or(0, |slot| slot.item);
        let last = project_slots.last().map_or(0, |slot| slot.item + 1);
        let range = if self.projects.len() > project_slots.len() {
            format!(" {}-{}/{}", first + 1, last, self.projects.len())
        } else {
            String::new()
        };
        write_row(
            ops,
            area.y.saturating_add(1),
            &format!(" PROJECTS{range}"),
            &heading,
        );
        let half_padding = format!("\x1b[0;7;{};49m", self.config.theme.selection_bg.sgr_fg());
        let upper_half = "\u{2580}".repeat(inner);
        let lower_half = "\u{2584}".repeat(inner);
        let write_padding =
            |ops: &mut Vec<u8>, row: u16, upper_selected: bool, lower_selected: bool| match (
                upper_selected,
                lower_selected,
            ) {
                (false, false) => write_row(ops, row, "", base),
                (true, true) => write_row(ops, row, "", active.as_str()),
                (true, false) => write_row(ops, row, &lower_half, &half_padding),
                (false, true) => write_row(ops, row, &upper_half, &half_padding),
            };
        let first_selected = project_slots
            .first()
            .and_then(|slot| self.projects.get(slot.item))
            .is_some_and(|project| project.id == self.active_project);
        if let Some(first) = project_slots.first() {
            write_padding(ops, first.rect.y.saturating_sub(1), false, first_selected);
        }
        for (visible_index, slot) in project_slots.iter().enumerate() {
            let Some(project) = self.projects.get(slot.item) else {
                continue;
            };
            let selected = project.id == self.active_project;
            let next_selected = project_slots
                .get(visible_index + 1)
                .and_then(|next| self.projects.get(next.item))
                .is_some_and(|next| next.id == self.active_project);
            let attention = self.project_attention(project.id);
            let marker = if selected { "\u{25B8}" } else { " " };
            let badge = if attention > 0 {
                format!(" !{attention}")
            } else {
                String::new()
            };
            let name_style = if selected { active.as_str() } else { base };
            let detail_style = if selected {
                active_detail.as_str()
            } else {
                muted
            };
            let detail = Self::worktree_registration(project)
                .map(|worktree| {
                    format!(
                        "{} \u{00B7} {}",
                        worktree.branch,
                        compact_project_path(&worktree.repository)
                    )
                })
                .or_else(|| {
                    ["branch", "environment", "task"]
                        .iter()
                        .find_map(|key| project.metadata.get(*key))
                        .cloned()
                })
                .unwrap_or_else(|| compact_project_path(&project.root));
            write_row(
                ops,
                slot.rect.y,
                &format!(" {marker} {}{badge}", project.name),
                name_style,
            );
            write_row(
                ops,
                slot.rect.y.saturating_add(1),
                &format!("   {detail}"),
                detail_style,
            );
            write_padding(ops, slot.rect.y.saturating_add(2), selected, next_selected);
        }
        ops.extend_from_slice(b"\x1b[0m");
    }

    pub(super) fn observatory_agent_entries(&self) -> Vec<(PaneId, ProjectId)> {
        let mut panes: Vec<(PaneId, ProjectId)> = self
            .panes
            .iter()
            .filter_map(|(pane, value)| {
                let project = self
                    .windows
                    .iter()
                    .find(|tab| tab.layout.contains_pane(*pane))?
                    .project;
                (value.agent.is_some()
                    && (self.sidebar_agent_scope == SidebarScope::Workspace
                        || project == self.active_project))
                    .then_some((*pane, project))
            })
            .collect();
        panes.sort_by_key(|(pane, project)| {
            let started_at = self
                .panes
                .get(pane)
                .and_then(|pane| pane.agent.as_ref())
                .map(|agent| agent.started_at);
            (
                self.projects
                    .iter()
                    .position(|item| item.id == *project)
                    .unwrap_or(usize::MAX),
                started_at,
                pane.0,
            )
        });
        panes
    }

    pub(super) fn draw_observatory_sidebar(&self, ops: &mut Vec<u8>) {
        match self.observatory_tab {
            ObservatoryTab::Agents => self.draw_agent_sidebar(ops),
            ObservatoryTab::Files => self.draw_file_sidebar(ops),
            ObservatoryTab::WebServers => self.draw_web_server_sidebar(ops),
        }
    }

    pub(super) fn draw_agent_sidebar(&self, ops: &mut Vec<u8>) {
        let width = self.observatory_width();
        if width == 0 {
            return;
        }
        let (area, _) = self.chrome_area();
        let x = self.cols.saturating_sub(width);
        let inner = width.saturating_sub(1) as usize;
        let base = "\x1b[0;39;49m";
        let muted = "\x1b[0;2;39;49m";
        let border = format!("\x1b[0;{};49m", self.config.theme.divider.sgr_fg());
        for row in area.y..area.bottom() {
            ops.extend_from_slice(
                format!("\x1b[{};{}H{border}\u{2502}{base}", row + 1, x + 1).as_bytes(),
            );
            ops.extend(std::iter::repeat_n(b' ', inner));
        }
        let write_row = |ops: &mut Vec<u8>, row: u16, text: &str, style: &str| {
            if row >= area.bottom() {
                return;
            }
            let fitted = fit_cell_text(text, inner);
            ops.extend_from_slice(
                format!("\x1b[{};{}H{border}\u{2502}{style}{fitted}", row + 1, x + 1).as_bytes(),
            );
        };

        let entries = self.observatory_agent_entries();
        let slots = self.observatory_agent_slots(entries.len());
        let first = slots.first().map_or(0, |slot| slot.item);
        let last = slots.last().map_or(0, |slot| slot.item + 1);
        let range = if entries.len() > slots.len() {
            format!(" {}-{}/{}", first + 1, last, entries.len())
        } else {
            format!(" {}", entries.len())
        };
        let heading = format!("\x1b[0;1;{};49m", self.config.theme.muted.sgr_fg());
        write_row(
            ops,
            area.y.saturating_add(1),
            &format!(" AGENTS{range}"),
            &heading,
        );
        let (scope_x, _, scope_label) = self.observatory_scope_button(self.sidebar_agent_scope);
        ops.extend_from_slice(
            format!(
                "\x1b[{};{}H\x1b[0;{};49m{scope_label}",
                area.y.saturating_add(2),
                scope_x + 1,
                self.config.theme.muted.sgr_fg()
            )
            .as_bytes(),
        );
        for slot in slots {
            let Some((pane_id, project_id)) = entries.get(slot.item).copied() else {
                continue;
            };
            let Some(pane) = self.panes.get(&pane_id) else {
                continue;
            };
            let Some(agent) = &pane.agent else {
                continue;
            };
            let glyph = match agent.status {
                AgentStatus::Permission | AgentStatus::Question => "!",
                AgentStatus::Working | AgentStatus::Tool | AgentStatus::Starting => "\u{25CF}",
                AgentStatus::Idle => "\u{2713}",
                AgentStatus::Error => "\u{00D7}",
                AgentStatus::Exited => "\u{25CB}",
                AgentStatus::Unknown => "?",
            };
            let active = self.windows[self.active_window].active == pane_id;
            let marker = if active { "\u{25B8}" } else { " " };
            let agent_style = if active {
                format!("\x1b[0;1;{};49m", agent.color.sgr_fg())
            } else {
                format!("\x1b[0;{};49m", agent.color.sgr_fg())
            };
            let metadata = ["task", "model", "branch", "title", "cwd"]
                .iter()
                .find_map(|key| pane.metadata.get(*key))
                .map(|value| format!(" \u{00B7} {}", value.value))
                .unwrap_or_default();
            let owner = self
                .projects
                .iter()
                .find(|project| project.id == project_id);
            let project = if self.sidebar_agent_scope == SidebarScope::Workspace {
                owner
                    .map(|project| format!(" \u{00B7} {}", project.name))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            // Only a Project that is a Git worktree shows its branch, so an
            // agent working on `main` in the primary checkout stays unmarked
            // and one in a detached worktree says exactly where it is.
            let worktree = owner
                .and_then(Self::worktree_registration)
                .map(|registration| format!(" \u{00B7} \u{2387} {}", registration.branch))
                .unwrap_or_default();
            let run = self
                .run_graph
                .active_for_pane(pane_id)
                .and_then(|(run, role)| {
                    self.run_graph
                        .role(role)
                        .map(|role| format!(" \u{00B7} Run {} / {}", run.0, role.name))
                })
                .unwrap_or_default();
            write_row(
                ops,
                slot.rect.y,
                &format!(
                    " {marker} {glyph} {}",
                    uniterm_core::agent::agent_name(&agent.id)
                ),
                &agent_style,
            );
            write_row(
                ops,
                slot.rect.y.saturating_add(1),
                &format!(
                    "    {}{worktree}{project}{run}{metadata}",
                    agent.status.label()
                ),
                muted,
            );
        }

        if area.h >= 2 {
            write_row(ops, area.bottom() - 2, &"\u{2500}".repeat(inner), &border);
            let button = format!(
                "\x1b[0;1;{};{}m",
                self.config.theme.foreground.sgr_fg(),
                self.config.theme.status_bg.sgr_bg()
            );
            for (_, rect, label) in self.observatory_agent_action_slots() {
                let fitted = fit_centered_button_label(label, usize::from(rect.w));
                ops.extend_from_slice(
                    format!(
                        "\x1b[{};{}H{button}{fitted}",
                        rect.y.saturating_add(1),
                        rect.x.saturating_add(1)
                    )
                    .as_bytes(),
                );
            }
        }
        ops.extend_from_slice(b"\x1b[0m");
    }

    pub(super) fn draw_web_server_sidebar(&self, ops: &mut Vec<u8>) {
        let width = self.observatory_width();
        if width == 0 {
            return;
        }
        let (area, _) = self.chrome_area();
        let x = self.cols.saturating_sub(width);
        let inner = width.saturating_sub(1) as usize;
        let base = "\x1b[0;39;49m";
        let muted = "\x1b[0;2;39;49m";
        let border = format!("\x1b[0;{};49m", self.config.theme.divider.sgr_fg());
        for row in area.y..area.bottom() {
            ops.extend_from_slice(
                format!("\x1b[{};{}H{border}\u{2502}{base}", row + 1, x + 1).as_bytes(),
            );
            ops.extend(std::iter::repeat_n(b' ', inner));
        }
        let write_row = |ops: &mut Vec<u8>, row: u16, text: &str, style: &str| {
            if row >= area.bottom() {
                return;
            }
            let fitted = fit_cell_text(text, inner);
            ops.extend_from_slice(
                format!("\x1b[{};{}H{border}\u{2502}{style}{fitted}", row + 1, x + 1).as_bytes(),
            );
        };
        let servers = self.observatory_dev_server_entries();
        let slots = self.observatory_web_slots(servers.len());
        let first = slots.first().map_or(0, |slot| slot.item);
        let last = slots.last().map_or(0, |slot| slot.item + 1);
        let range = if servers.len() > slots.len() {
            format!(" {}-{}/{}", first + 1, last, servers.len())
        } else {
            format!(" {}", servers.len())
        };
        let heading = format!("\x1b[0;1;{};49m", self.config.theme.muted.sgr_fg());
        write_row(
            ops,
            area.y.saturating_add(1),
            &format!(" WEB SERVERS{range}"),
            &heading,
        );
        let (scope_x, _, scope_label) = self.observatory_scope_button(self.sidebar_server_scope);
        ops.extend_from_slice(
            format!(
                "\x1b[{};{}H\x1b[0;{};49m{scope_label}",
                area.y.saturating_add(2),
                scope_x + 1,
                self.config.theme.muted.sgr_fg()
            )
            .as_bytes(),
        );
        let live = format!("\x1b[0;1;{};49m", self.config.theme.success.sgr_fg());
        for slot in slots {
            let Some(server) = servers.get(slot.item) else {
                continue;
            };
            write_row(
                ops,
                slot.rect.y,
                &format!(" \u{25C6} {} :{}", server.label, server.port),
                &live,
            );
            let project = if self.sidebar_server_scope == SidebarScope::Workspace {
                format!("{} \u{00B7} ", server.project_name)
            } else {
                String::new()
            };
            write_row(
                ops,
                slot.rect.y.saturating_add(1),
                &format!("   {project}{}", server.url),
                muted,
            );
        }
        ops.extend_from_slice(b"\x1b[0m");
    }

    pub(super) fn draw_file_sidebar(&self, ops: &mut Vec<u8>) {
        let width = self.observatory_width();
        if width == 0 {
            return;
        }
        let (area, _) = self.chrome_area();
        let x = self.cols.saturating_sub(width);
        let inner = width.saturating_sub(1) as usize;
        let base = "\x1b[0;39;49m";
        let muted = format!("\x1b[0;2;{};49m", self.config.theme.muted.sgr_fg());
        let border = format!("\x1b[0;{};49m", self.config.theme.divider.sgr_fg());
        let selection = if self.files.focused {
            format!(
                "\x1b[0;{};{}m",
                self.config.theme.selection_bg.sgr_bg(),
                self.config.theme.status_active_fg.sgr_fg()
            )
        } else {
            format!("\x1b[0;1;{};49m", self.config.theme.accent.sgr_fg())
        };
        for row in area.y..area.bottom() {
            ops.extend_from_slice(
                format!("\x1b[{};{}H{border}\u{2502}{base}", row + 1, x + 1).as_bytes(),
            );
            ops.extend(std::iter::repeat_n(b' ', inner));
        }
        let write_row = |ops: &mut Vec<u8>, row: u16, text: &str, style: &str| {
            if row >= area.bottom() {
                return;
            }
            let fitted = fit_cell_text(text, inner);
            ops.extend_from_slice(
                format!("\x1b[{};{}H{border}\u{2502}{style}{fitted}", row + 1, x + 1).as_bytes(),
            );
        };
        let project = self
            .projects
            .iter()
            .find(|project| project.id == self.files.project)
            .map(|project| project.name.as_str())
            .unwrap_or("Project");
        write_row(
            ops,
            area.y.saturating_add(1),
            &format!(" FILES \u{00B7} {project}"),
            &format!("\x1b[0;1;{};49m", self.config.theme.muted.sgr_fg()),
        );
        let sidebar_rows = self.file_sidebar_rows(area);
        let git_stats = self.files.git_stats().filter(|stats| stats.has_changes());
        if let Some(stats) = git_stats {
            let git_row = area.y.saturating_add(3);
            if git_row < area.bottom() {
                ops.extend_from_slice(
                    format!("\x1b[{};{}H{border}\u{2502}{muted} Git", git_row + 1, x + 1)
                        .as_bytes(),
                );
                let mut used = 4_usize;
                let segments = [
                    (
                        stats.insertions,
                        "+",
                        format!("\x1b[0;{};49m", self.config.theme.success.sgr_fg()),
                    ),
                    (
                        stats.deletions,
                        "-",
                        format!("\x1b[0;{};49m", self.config.theme.error.sgr_fg()),
                    ),
                    (
                        stats.untracked,
                        "?",
                        format!("\x1b[0;{};49m", self.config.theme.warning.sgr_fg()),
                    ),
                ];
                for (count, prefix, style) in segments {
                    if count == 0 {
                        continue;
                    }
                    let text = format!(" {prefix}{}", format_change_count(count));
                    if used + text.len() > inner {
                        break;
                    }
                    ops.extend_from_slice(style.as_bytes());
                    ops.extend_from_slice(text.as_bytes());
                    used += text.len();
                }
            }
        }
        if let Some(divider) = sidebar_rows.divider {
            write_row(ops, divider, &"\u{2500}".repeat(inner), &border);
        }

        let rows = self.files.rows();
        let capacity = sidebar_rows.capacity();
        let first = self.files.first_visible(capacity);
        for slot in 0..capacity {
            let Some(row) = rows.get(first + slot) else {
                break;
            };
            let indent = "  ".repeat(row.depth.min(8));
            let glyph = if row.is_dir {
                if row.expanded {
                    "\u{25BE}"
                } else {
                    "\u{25B8}"
                }
            } else if row.is_symlink {
                "@"
            } else {
                " "
            };
            let suffix = if row.is_dir { "/" } else { "" };
            let text = format!(" {indent}{glyph} {}{suffix}", row.name);
            let style = if first + slot == self.files.selected {
                selection.as_str()
            } else {
                base
            };
            write_row(ops, sidebar_rows.tree_start + slot as u16, &text, style);
        }
        if area.h >= 3 {
            write_row(ops, area.bottom() - 3, &"\u{2500}".repeat(inner), &border);
            let status = if let Some((label, value)) = self.files.prompt_label() {
                format!(" {label}: {value}\u{2588}")
            } else {
                format!(" {}", self.files.status_line())
            };
            write_row(ops, area.bottom() - 2, &status, &muted);
            write_row(
                ops,
                area.bottom() - 1,
                " n:new N:dir R:rename d:delete",
                &muted,
            );
        }
        ops.extend_from_slice(b"\x1b[0m");
    }

    pub(super) fn notification_rect(&self) -> Option<Rect> {
        self.notification.as_ref()?;
        let (area, _) = self.chrome_area();
        if area.w < 24 || area.h < 4 {
            return None;
        }
        let width = area.w.min(44);
        Some(Rect::new(
            area.right().saturating_sub(width),
            area.y.saturating_add(1),
            width,
            3,
        ))
    }

    pub(super) fn draw_notification(&self, ops: &mut Vec<u8>) {
        let Some(toast) = &self.notification else {
            return;
        };
        let Some(rect) = self.notification_rect() else {
            return;
        };
        let style = format!(
            "\x1b[0;{};{}m",
            self.config.theme.foreground.sgr_fg(),
            self.config.theme.surface.sgr_bg()
        );
        let accent = format!(
            "\x1b[0;1;{};{}m",
            self.config.theme.attention.sgr_fg(),
            self.config.theme.surface.sgr_bg()
        );
        let width = rect.w as usize;
        for row in 0..rect.h {
            let (text, row_style) = match row {
                0 => (format!(" ! {}", toast.title), accent.as_str()),
                1 => (format!("   {}", toast.body), style.as_str()),
                _ => ("   click to open  \u{00B7}  8s".into(), style.as_str()),
            };
            let fitted = fit_cell_text(&text, width);
            ops.extend_from_slice(
                format!(
                    "\x1b[{};{}H{row_style}{fitted}",
                    rect.y + row + 1,
                    rect.x + 1
                )
                .as_bytes(),
            );
        }
        ops.extend_from_slice(b"\x1b[0m");
    }

    /// The zoom-out overview frame (S2): every window as a bordered tile with
    /// its number/name and a snapshot of its active pane's most recent output.
    /// Static by design - it is a picker, not a monitor; pane damage is not
    /// broadcast while it is open (no idle work, no streaming repaints).
    pub(super) fn build_overview_frame(&self, sel: usize) -> Vec<u8> {
        let mut ops = Vec::new();
        ops.extend_from_slice(b"\x1b[r\x1b[2J");
        let (area, _) = self.chrome_area();
        let tabs = self.project_window_indices(self.active_project);
        let n = tabs.len();
        let sel = sel.min(n.saturating_sub(1));
        let tiles = uniterm_core::layout::overview_tiles(area, n);
        for (i, (window, tile)) in tabs.iter().zip(&tiles).enumerate() {
            self.draw_overview_tile(&self.windows[*window], i, *tile, i == sel, &mut ops);
        }
        self.draw_sidebar(&mut ops);
        self.draw_observatory_sidebar(&mut ops);
        self.draw_status(&mut ops);
        ops.extend_from_slice(b"\x1b[H");
        ops
    }

    /// One overview tile: a border (bold + active colour when selected), a
    /// ` i:name ` label in the top edge, and the bottom-left crop of the
    /// window's active pane (its most recent output), dimmed.
    pub(super) fn draw_overview_tile(
        &self,
        win: &Win,
        idx: usize,
        tile: Rect,
        selected: bool,
        ops: &mut Vec<u8>,
    ) {
        if tile.w < 4 || tile.h < 3 {
            return; // too small to draw a box with content
        }
        let t = &self.config.theme;
        let color = if selected {
            t.status_active_bg
        } else {
            t.divider
        };
        let (x, y) = (tile.x, tile.y);
        let right = tile.right() - 1;
        let bottom = tile.bottom() - 1;
        let inner_w = (tile.w - 2) as usize;
        ops.extend_from_slice(format!("\x1b[0;{}m", color.sgr_fg()).as_bytes());
        if selected {
            ops.extend_from_slice(b"\x1b[1m");
        }
        // Top edge with the label (1-based, matching the status line and the
        // digit bindings).
        let num = idx + 1;
        let name = match &win.name {
            Some(nm) => format!(" {num}:{nm} "),
            None => format!(" {num} "),
        };
        let label: String = name.chars().take(inner_w.saturating_sub(1)).collect();
        let mut top = String::from("\u{250C}");
        top.push_str(&label);
        for _ in 0..inner_w.saturating_sub(label.chars().count()) {
            top.push('\u{2500}');
        }
        top.push('\u{2510}');
        ops.extend_from_slice(format!("\x1b[{};{}H{}", y + 1, x + 1, top).as_bytes());
        // Sides.
        for row in (y + 1)..bottom {
            ops.extend_from_slice(format!("\x1b[{};{}H\u{2502}", row + 1, x + 1).as_bytes());
            ops.extend_from_slice(format!("\x1b[{};{}H\u{2502}", row + 1, right + 1).as_bytes());
        }
        // Bottom edge.
        let mut bot = String::from("\u{2514}");
        for _ in 0..inner_w {
            bot.push('\u{2500}');
        }
        bot.push('\u{2518}');
        ops.extend_from_slice(format!("\x1b[{};{}H{}", bottom + 1, x + 1, bot).as_bytes());
        // Content: a true miniature of the window - its real split layout
        // computed at tile size (mini dividers included), every pane's grid
        // sampled down into its scaled rect with colours preserved. The
        // selected tile renders at full brightness, the rest faint.
        let interior = Rect::new(tile.x + 1, tile.y + 1, tile.w - 2, tile.h - 2);
        let mini = match win.zoomed {
            Some(z) => {
                let mut l = Layout::default();
                l.panes.push((z, interior));
                l
            }
            None => win.layout.compute(interior),
        };
        draw_dividers(&mini, t.divider, ops);
        for (pid, mr) in &mini.panes {
            let Some(pane) = self.panes.get(pid) else {
                continue;
            };
            if mr.w == 0 || mr.h == 0 {
                continue;
            }
            let g = pane.term.grid();
            let (gw, gh) = (g.width() as u32, g.height() as u32);
            for my in 0..mr.h {
                let sy = ((my as u32 * gh) / mr.h as u32).min(gh - 1) as u16;
                ops.extend_from_slice(format!("\x1b[{};{}H", mr.y + my + 1, mr.x + 1).as_bytes());
                ops.extend_from_slice(if selected { b"\x1b[0m" } else { b"\x1b[0;2m" });
                let mut row = String::with_capacity(mr.w as usize * 2);
                let mut last: Option<(Color, Color)> = None;
                for mx in 0..mr.w {
                    let sx = ((mx as u32 * gw) / mr.w as u32).min(gw - 1) as u16;
                    let cell = g.get(sx, sy);
                    if last != Some((cell.fg, cell.bg)) {
                        row.push_str(&format!("\x1b[{};{}m", cell.fg.sgr_fg(), cell.bg.sgr_bg()));
                        last = Some((cell.fg, cell.bg));
                    }
                    if cell.is_continuation() {
                        row.push(' ');
                    } else if cell.width == 2 {
                        // Miniatures have one cell per sample. Emitting a wide
                        // glyph here would shift the remaining tile columns.
                        row.push('#');
                    } else {
                        let text = g.cell_text_owned(cell);
                        if text.chars().any(|ch| (ch as u32) < 0x20) {
                            row.push(' ');
                        } else {
                            row.push_str(&text);
                        }
                    }
                }
                ops.extend_from_slice(row.as_bytes());
            }
        }
        ops.extend_from_slice(b"\x1b[0m");
    }

    /// Drive the overview from raw key bytes: arrows/hjkl move the selection,
    /// digits jump, Enter switches to the selected window, Esc/q/w cancels.
    pub(super) fn handle_overview_input(&mut self, reg: &Registry, bytes: &[u8]) {
        let n = self.project_window_indices(self.active_project).len();
        let cols = uniterm_core::layout::overview_cols(n);
        let Some(cur) = self.overview else {
            return;
        };
        let mut sel = cur.min(n.saturating_sub(1));
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b == 0x1b {
                if bytes.get(i + 1) == Some(&b'[') {
                    match bytes.get(i + 2) {
                        Some(b'A') => sel = sel.saturating_sub(cols),
                        Some(b'B') => {
                            if sel + cols < n {
                                sel += cols;
                            }
                        }
                        Some(b'C') => {
                            if sel + 1 < n {
                                sel += 1;
                            }
                        }
                        Some(b'D') => sel = sel.saturating_sub(1),
                        _ => {}
                    }
                    i += 3;
                    continue;
                }
                self.leave_overview(reg, None); // lone Esc cancels
                return;
            }
            match b {
                b'q' | b'w' | 0x03 => {
                    self.leave_overview(reg, None);
                    return;
                }
                0x0d | 0x0a => {
                    self.leave_overview(reg, Some(sel));
                    return;
                }
                b'k' => sel = sel.saturating_sub(cols),
                b'j' => {
                    if sel + cols < n {
                        sel += cols;
                    }
                }
                b'l' => {
                    if sel + 1 < n {
                        sel += 1;
                    }
                }
                b'h' => sel = sel.saturating_sub(1),
                d @ b'0'..=b'9' => {
                    // 1-based like the tile labels: `1` is the first window,
                    // `0` jumps to window 10.
                    let idx = if d == b'0' { 9 } else { (d - b'1') as usize };
                    if idx < n {
                        self.leave_overview(reg, Some(idx));
                        return;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if self.overview != Some(sel) {
            self.overview = Some(sel);
            self.full_repaint_all(reg);
        }
    }

    /// Close the overview, optionally switching to a picked window.
    pub(super) fn leave_overview(&mut self, reg: &Registry, pick: Option<usize>) {
        self.overview = None;
        if let Some(tab) = pick {
            if let Some(wi) = self
                .project_window_indices(self.active_project)
                .get(tab)
                .copied()
            {
                self.activate_window(wi);
                self.relayout();
                self.persist();
            }
        }
        self.full_repaint_all(reg);
    }

    /// Draw the status line as three aligned controls: the colored Workspace
    /// button, the horizontally scrollable Tab bar, and Observatory tabs.
    pub(super) fn draw_status(&self, ops: &mut Vec<u8>) {
        let (_, status_row) = self.chrome_area();
        let Some(row) = status_row else {
            return;
        };
        let t = &self.config.theme;
        let cols = self.cols as usize;
        let base = format!("\x1b[{};{}m", t.status_bg.sgr_bg(), t.status_fg.sgr_fg());
        let active = format!(
            "\x1b[1;{};{}m",
            t.status_active_bg.sgr_bg(),
            t.status_active_fg.sgr_fg()
        );
        let button = format!(
            "\x1b[1;{};{}m",
            t.accent_muted.sgr_bg(),
            t.foreground.sgr_fg()
        );
        let disabled = format!("\x1b[2;{};{}m", t.status_bg.sgr_bg(), t.muted.sgr_fg());
        let rail_divider = format!("\x1b[0;{};{}m", t.status_bg.sgr_bg(), t.divider.sgr_fg());

        ops.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
        ops.extend_from_slice(base.as_bytes());

        let mut col = 0usize;
        let put = |ops: &mut Vec<u8>, s: &str, col: &mut usize| {
            for grapheme in s.graphemes(true) {
                let width = grapheme.width();
                if (*col).saturating_add(width) > cols {
                    break;
                }
                ops.extend_from_slice(grapheme.as_bytes());
                *col += width;
            }
        };

        let workspace_width = usize::from(self.workspace_button_width()).min(cols);
        let workspace_label = fit_cell_text(&self.workspace_button(), workspace_width);
        ops.extend_from_slice(button.as_bytes());
        put(ops, &workspace_label, &mut col);
        ops.extend_from_slice(base.as_bytes());
        ops.extend_from_slice(rail_divider.as_bytes());
        put(ops, "\u{2502}", &mut col);
        ops.extend_from_slice(base.as_bytes());

        let pad_to = |ops: &mut Vec<u8>, target: usize, col: &mut usize| {
            while *col < target.min(cols) {
                ops.push(b' ');
                *col += 1;
            }
        };
        let tabs = self.tab_bar_layout(false);
        let active_tab = self
            .project_window_indices(self.active_project)
            .iter()
            .position(|window| *window == self.active_window)
            .unwrap_or(0);
        let segments = self.window_segments();
        if let Some(rect) = tabs.scroll_left {
            pad_to(ops, usize::from(rect.x), &mut col);
            ops.extend_from_slice(if tabs.hidden_before {
                button.as_bytes()
            } else {
                disabled.as_bytes()
            });
            put(ops, &fit_cell_text(" < ", usize::from(rect.w)), &mut col);
            ops.extend_from_slice(base.as_bytes());
        }
        for slot in &tabs.tabs {
            pad_to(ops, usize::from(slot.rect.x), &mut col);
            if slot.item == active_tab {
                ops.extend_from_slice(active.as_bytes());
                put(
                    ops,
                    &fit_cell_text(&segments[slot.item], usize::from(slot.rect.w)),
                    &mut col,
                );
                ops.extend_from_slice(base.as_bytes());
            } else {
                put(
                    ops,
                    &fit_cell_text(&segments[slot.item], usize::from(slot.rect.w)),
                    &mut col,
                );
            }
        }
        if let Some(rect) = tabs.scroll_right {
            pad_to(ops, usize::from(rect.x), &mut col);
            ops.extend_from_slice(if tabs.hidden_after {
                button.as_bytes()
            } else {
                disabled.as_bytes()
            });
            put(ops, &fit_cell_text(" > ", usize::from(rect.w)), &mut col);
            ops.extend_from_slice(base.as_bytes());
        }
        if let Some(rect) = tabs.new_tab {
            pad_to(ops, usize::from(rect.x), &mut col);
            ops.extend_from_slice(button.as_bytes());
            put(ops, &fit_cell_text(" + ", usize::from(rect.w)), &mut col);
            ops.extend_from_slice(base.as_bytes());
        }
        if let Some(rect) = self.status_right_rect() {
            pad_to(ops, usize::from(rect.x), &mut col);
            ops.extend_from_slice(disabled.as_bytes());
            put(
                ops,
                &fit_cell_text(&self.status_right_text(), usize::from(rect.w)),
                &mut col,
            );
            ops.extend_from_slice(base.as_bytes());
        }
        let observatory_x = self.cols.saturating_sub(self.observatory_width());
        pad_to(ops, usize::from(observatory_x), &mut col);
        if self.observatory_width() > 0 {
            ops.extend_from_slice(rail_divider.as_bytes());
            put(ops, "\u{2502}", &mut col);
            ops.extend_from_slice(base.as_bytes());
        }
        for (tab, rect, label) in self.observatory_tab_slots() {
            pad_to(ops, usize::from(rect.x), &mut col);
            if tab == self.observatory_tab {
                ops.extend_from_slice(button.as_bytes());
            }
            put(
                ops,
                &fit_centered_ellipsis(label, usize::from(rect.w)),
                &mut col,
            );
            if tab == self.observatory_tab {
                ops.extend_from_slice(base.as_bytes());
            }
        }
        while col < cols {
            ops.push(b' ');
            col += 1;
        }
        ops.extend_from_slice(b"\x1b[0m");
    }

    /// Screen position of the active pane's visible cursor (its copy-mode
    /// cursor when in copy-mode, otherwise the terminal cursor).
    pub(super) fn active_cursor_pos(&self) -> Option<(u16, u16)> {
        let win = &self.windows[self.active_window];
        let pane = self.panes.get(&win.active)?;
        let rect = self.current_layout.rect_of(win.active)?;
        Some(match &pane.copy {
            Some(copy) => copy.cursor_pos(rect),
            None => {
                let (cx, cy) = pane.term.cursor();
                (rect.x + cx, rect.y + cy)
            }
        })
    }

    pub(super) fn active_cursor_visible(&self) -> bool {
        if self.context_menu.is_some() {
            return false;
        }
        if self.files.focused && self.file_manager_visible() {
            return false;
        }
        let win = &self.windows[self.active_window];
        self.panes
            .get(&win.active)
            .is_none_or(|pane| pane.copy.is_some() || pane.term.cursor_visible())
    }

    /// The cursor-position escape for the active pane. Only for full frames,
    /// where every client's renderer is invalidated around the send; on the
    /// incremental path the cursor must go through `Renderer::place_cursor`
    /// so the position cache stays truthful.
    pub(super) fn append_cursor(&self, ops: &mut Vec<u8>) {
        if let Some((col, row)) = self.active_cursor_pos() {
            ops.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
        }
        ops.extend_from_slice(if self.active_cursor_visible() {
            b"\x1b[?25h"
        } else {
            b"\x1b[?25l"
        });
    }

    pub(super) fn full_repaint_direct_client(&mut self, reg: &Registry, token: Token) {
        let Server { clients, panes, .. } = self;
        let Some(pane_id) = clients
            .get(&token)
            .and_then(|client| client.direct.as_ref().map(|direct| direct.pane))
        else {
            return;
        };
        let Some(pane) = panes.get(&pane_id) else {
            return;
        };
        let cursor = pane.term.cursor();
        let cursor_visible = pane.term.cursor_visible();
        let Some(client) = clients.get_mut(&token) else {
            return;
        };
        let mut ops = Vec::new();
        client.renderer.set_dim(false);
        client.renderer.render_full(pane.term.grid(), &mut ops);
        client.renderer.place_cursor(cursor.0, cursor.1, &mut ops);
        ops.extend_from_slice(if cursor_visible {
            b"\x1b[?25h"
        } else {
            b"\x1b[?25l"
        });
        if let Some(direct) = client.direct.as_mut() {
            direct.last_cursor_visible = Some(cursor_visible);
        }
        client.queue_render(&encode_frame(&ServerMessage::RenderOps(ops)));
        client.flush();
        let _ = set_interest(reg, client, token);
    }

    pub(super) fn attach_direct_client(
        &mut self,
        reg: &Registry,
        token: Token,
        pane: PaneId,
        role: PaneAttachRole,
    ) {
        let was_attached = self
            .clients
            .get(&token)
            .is_some_and(|client| client.attached);
        if let Some(client) = self.clients.get_mut(&token) {
            client.direct_only = true;
            client.attached = false;
        }
        let Some(target) = self.panes.get(&pane) else {
            if let Some(client) = self.clients.get_mut(&token) {
                client.queue(&encode_frame(&ServerMessage::PaneAttachRejected {
                    pane,
                    reason: "Pane does not exist".into(),
                }));
                client.flush();
                let _ = set_interest(reg, client, token);
            }
            if was_attached && self.recompute_client_geometry() {
                self.relayout();
                self.full_repaint_all(reg);
            }
            return;
        };
        let cols = target.term.grid().width();
        let rows = target.term.grid().height();
        let controller = self.clients.iter().find_map(|(candidate, client)| {
            (*candidate != token)
                .then_some(client.direct.as_ref())
                .flatten()
                .filter(|direct| direct.pane == pane && direct.role.can_control())
                .map(|_| *candidate)
        });
        if role == PaneAttachRole::Controller && controller.is_some() {
            if let Some(client) = self.clients.get_mut(&token) {
                client.queue(&encode_frame(&ServerMessage::PaneAttachRejected {
                    pane,
                    reason: "Pane already has a controller; request takeover explicitly".into(),
                }));
                client.flush();
                let _ = set_interest(reg, client, token);
            }
            if was_attached && self.recompute_client_geometry() {
                self.relayout();
                self.full_repaint_all(reg);
            }
            return;
        }
        if role == PaneAttachRole::Takeover {
            let revoked: Vec<Token> = self
                .clients
                .iter()
                .filter_map(|(candidate, client)| {
                    (*candidate != token)
                        .then_some(client.direct.as_ref())
                        .flatten()
                        .filter(|direct| direct.pane == pane && direct.role.can_control())
                        .map(|_| *candidate)
                })
                .collect();
            for revoked_token in revoked {
                if let Some(client) = self.clients.get_mut(&revoked_token) {
                    if let Some(direct) = client.direct.as_mut() {
                        direct.role = PaneAttachRole::Observer;
                    }
                    client.queue(&encode_frame(&ServerMessage::PaneAttachRevoked {
                        pane,
                        reason: "another attachment took control".into(),
                    }));
                    client.flush();
                    let _ = set_interest(reg, client, revoked_token);
                }
            }
        }

        if let Some(client) = self.clients.get_mut(&token) {
            client.attached = false;
            client.overlay = false;
            client.direct = Some(DirectAttachment {
                pane,
                role,
                last_cursor_visible: None,
            });
            client.renderer.invalidate();
            client.queue(&encode_frame(&ServerMessage::PaneAttached {
                pane,
                role,
                cols,
                rows,
            }));
            client.flush();
            let _ = set_interest(reg, client, token);
        }
        if was_attached && self.recompute_client_geometry() {
            self.relayout();
            self.full_repaint_all(reg);
        }
        self.full_repaint_direct_client(reg, token);
    }

    pub(super) fn active_nested_input(&self) -> bool {
        if self.overview.is_some() || self.context_menu.is_some() {
            return false;
        }
        self.windows
            .get(self.active_window)
            .and_then(|window| self.panes.get(&window.active))
            .is_some_and(|pane| pane.term.nested_input())
    }

    pub(super) fn broadcast_nested_input(&mut self, reg: &Registry) {
        let message = encode_frame(&ServerMessage::NestedInput {
            enabled: self.active_nested_input(),
        });
        for (token, client) in &mut self.clients {
            if client.attached && client.direct.is_none() {
                client.queue(&message);
                client.flush();
                let _ = set_interest(reg, client, *token);
            }
        }
    }

    pub(super) fn full_repaint_all(&mut self, reg: &Registry) {
        self.sync_chrome_viewports();
        self.sync_file_viewport();
        let frame = self.build_full_frame();
        let nested_input = self.active_nested_input();
        // The frame parked the cursor itself (except the overview, which has
        // no pane cursor); record what clients now show so the next
        // cursor-only broadcast compares against reality.
        self.last_cursor = if self.overview.is_some() || self.context_menu.is_some() {
            None
        } else {
            self.active_cursor_pos()
        };
        self.last_cursor_visible = Some(self.active_cursor_visible());
        // Visible panes' damage is now reflected in the full frame; clear it so
        // the next incremental render starts clean.
        let visible: Vec<PaneId> = self.current_layout.panes.iter().map(|(p, _)| *p).collect();
        for pid in visible {
            if let Some(p) = self.panes.get_mut(&pid) {
                p.term.grid_mut().clear_damage();
            }
        }
        let encoded = encode_frame(&ServerMessage::RenderOps(frame));
        let title = self.window_title();
        // Every attached client receives this title, so later output must not
        // send it again until it changes.
        self.last_broadcast_title = title.clone();
        let direct_tokens: Vec<Token> = self
            .clients
            .iter()
            .filter_map(|(token, client)| client.direct.as_ref().map(|_| *token))
            .collect();
        let Server { clients, .. } = self;
        for (tok, c) in clients.iter_mut() {
            if !c.attached {
                continue;
            }
            if let Some(title) = &title {
                c.queue(&encode_frame(&ServerMessage::WindowTitle {
                    title: title.clone(),
                }));
            }
            if c.overlay {
                c.flush();
                let _ = set_interest(reg, c, *tok);
                continue;
            }
            c.renderer.invalidate();
            c.queue_render(&encoded);
            c.queue(&encode_frame(&ServerMessage::NestedInput {
                enabled: nested_input,
            }));
            c.flush();
            let _ = set_interest(reg, c, *tok);
        }
        for token in direct_tokens {
            self.full_repaint_direct_client(reg, token);
        }
    }

    pub(super) fn full_repaint_client(&mut self, reg: &Registry, token: Token) {
        if let Some(client) = self.clients.get(&token) {
            if client.direct.is_some() {
                self.full_repaint_direct_client(reg, token);
                return;
            }
            if client.direct_only {
                return;
            }
        }
        self.sync_chrome_viewports();
        self.sync_file_viewport();
        let frame = self.build_full_frame();
        let nested_input = self.active_nested_input();
        if self.overview.is_none() && self.context_menu.is_none() {
            self.last_cursor = self.active_cursor_pos();
        }
        self.last_cursor_visible = Some(self.active_cursor_visible());
        let title = self.window_title();
        // When this is the only attached client, its title is the broadcast
        // state; with peers, the next change still reaches everyone.
        let sole_client = self
            .clients
            .values()
            .filter(|client| client.attached)
            .count()
            <= 1;
        if sole_client {
            self.last_broadcast_title = title.clone();
        }
        let Server { clients, .. } = self;
        if let Some(c) = clients.get_mut(&token) {
            if let Some(title) = title {
                c.queue(&encode_frame(&ServerMessage::WindowTitle { title }));
            }
            c.renderer.invalidate();
            c.queue_render(&encode_frame(&ServerMessage::RenderOps(frame)));
            c.queue(&encode_frame(&ServerMessage::NestedInput {
                enabled: nested_input,
            }));
            c.flush();
            let _ = set_interest(reg, c, token);
        }
    }

    pub(super) fn broadcast_direct_pane_damage(&mut self, reg: &Registry, pane_id: PaneId) {
        let Server { clients, panes, .. } = self;
        let Some(pane) = panes.get(&pane_id) else {
            return;
        };
        let cursor = pane.term.cursor();
        let cursor_visible = pane.term.cursor_visible();
        for (token, client) in clients.iter_mut() {
            let Some(direct) = client.direct.as_mut() else {
                continue;
            };
            if direct.pane != pane_id {
                continue;
            }
            let mut ops = Vec::new();
            client.renderer.set_dim(false);
            if pane.term.grid().pending_scroll_up() != 0 {
                client
                    .renderer
                    .render_pane_damage_with_scroll(pane.term.grid(), 0, 0, &mut ops);
            } else {
                client
                    .renderer
                    .render_pane_damage(pane.term.grid(), 0, 0, &mut ops);
            }
            client.renderer.place_cursor(cursor.0, cursor.1, &mut ops);
            if direct.last_cursor_visible != Some(cursor_visible) {
                ops.extend_from_slice(if cursor_visible {
                    b"\x1b[?25h"
                } else {
                    b"\x1b[?25l"
                });
                direct.last_cursor_visible = Some(cursor_visible);
            }
            if ops.is_empty() {
                continue;
            }
            client.queue_render(&encode_frame(&ServerMessage::RenderOps(ops)));
            client.flush();
            let _ = set_interest(reg, client, *token);
        }
    }

    /// Paint one pane's damage to Workspace and direct attachments, then clear it.
    /// Also re-parks the visible cursor whenever it moved since the last
    /// frame, even with no damage at all - typing a space over an already
    /// blank cell changes no cell but does move the cursor.
    pub(super) fn broadcast_pane_damage(&mut self, reg: &Registry, pane_id: PaneId) {
        self.broadcast_direct_pane_damage(reg, pane_id);
        // The overview is a static picker; pane output keeps updating the
        // model silently and repaints when the overview closes.
        if self.overview.is_some() || self.context_menu.is_some() {
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.term.grid_mut().clear_damage();
            }
            return;
        }
        let Some(rect) = self.current_layout.rect_of(pane_id) else {
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.term.grid_mut().clear_damage();
            }
            return; // pane is in a background window or occluded by zoom
        };
        let cursor = self.active_cursor_pos();
        let cursor_visible = self.active_cursor_visible();
        let terminal_cols = self.cols;
        let Server {
            clients,
            panes,
            last_cursor,
            last_cursor_visible,
            ..
        } = self;
        let Some(pane) = panes.get_mut(&pane_id) else {
            return;
        };
        // In copy-mode the viewport is frozen; live output updates the model
        // (and scrollback) but must not paint over the copy view.
        if pane.copy.is_some() {
            return;
        }
        // Zero frames when nothing visible changed: no damage and the cursor
        // is already where the clients show it.
        if !pane.term.grid().is_dirty()
            && cursor == *last_cursor
            && Some(cursor_visible) == *last_cursor_visible
        {
            return;
        }
        for (tok, c) in clients.iter_mut() {
            if !c.attached || c.overlay {
                continue;
            }
            let mut ops = Vec::new();
            c.renderer.set_dim(false);
            if rect.x == 0 && rect.w == terminal_cols && pane.term.grid().pending_scroll_up() != 0 {
                c.renderer.render_pane_damage_with_scroll(
                    pane.term.grid(),
                    rect.x,
                    rect.y,
                    &mut ops,
                );
            } else {
                c.renderer
                    .render_pane_damage(pane.term.grid(), rect.x, rect.y, &mut ops);
            }
            // Park the visible cursor through the renderer so its position
            // cache stays truthful; a raw CUP appended here desyncs the cache
            // and the next batch paints its runs at the wrong place.
            if let Some((col, row)) = cursor {
                c.renderer.place_cursor(col, row, &mut ops);
            }
            if Some(cursor_visible) != *last_cursor_visible {
                ops.extend_from_slice(if cursor_visible {
                    b"\x1b[?25h"
                } else {
                    b"\x1b[?25l"
                });
            }
            if ops.is_empty() {
                continue; // this client's terminal already matches
            }
            c.queue_render(&encode_frame(&ServerMessage::RenderOps(ops)));
            c.flush();
            let _ = set_interest(reg, c, *tok);
        }
        pane.term.grid_mut().clear_damage();
        *last_cursor = cursor;
        *last_cursor_visible = Some(cursor_visible);
    }
}

pub(super) fn format_change_count(value: u32) -> String {
    if value >= 100_000 {
        format!("{}k", value.saturating_add(500) / 1_000)
    } else if value >= 10_000 {
        let tenths = value.saturating_add(50) / 100;
        format!("{}.{:01}k", tenths / 10, tenths % 10)
    } else {
        value.to_string()
    }
}

pub(super) fn fit_cell_text(text: &str, width: usize) -> String {
    let mut fitted = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = grapheme.width();
        if used.saturating_add(grapheme_width) > width {
            break;
        }
        fitted.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    fitted.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    fitted
}

pub(super) fn sanitize_chrome_text(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|character| !character.is_control() && *character != '\u{7f}')
        .take(limit)
        .collect()
}

/// Render a small bounded template without allocation proportional to unknown
/// input. Unknown or unclosed tokens reject the whole template, so a typo never
/// leaks literal braces into terminal chrome. Doubled braces are literals.
pub(super) fn render_named_template<'a>(
    template: &str,
    mut value: impl FnMut(&str) -> Option<&'a str>,
) -> Option<String> {
    if template.len() > 2048 {
        return None;
    }
    let mut output = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' if bytes.get(index + 1) == Some(&b'{') => {
                output.push('{');
                index += 2;
            }
            b'}' if bytes.get(index + 1) == Some(&b'}') => {
                output.push('}');
                index += 2;
            }
            b'{' => {
                let rest = &template[index + 1..];
                let end = rest.find('}')?;
                output.push_str(value(&rest[..end])?);
                index += end + 2;
            }
            b'}' => return None,
            _ => {
                let character = template[index..].chars().next()?;
                output.push(character);
                index += character.len_utf8();
            }
        }
        if output.len() > 4096 {
            return None;
        }
    }
    Some(output)
}

pub(super) fn fit_centered_ellipsis(text: &str, width: usize) -> String {
    let text_width = text.width();
    if text_width <= width {
        let remaining = width - text_width;
        let left = remaining / 2;
        let right = remaining - left;
        return format!("{}{}{}", " ".repeat(left), text, " ".repeat(right));
    }
    if width == 0 {
        return String::new();
    }

    let prefix_width = width - 1;
    let mut fitted = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = grapheme.width();
        if used.saturating_add(grapheme_width) > prefix_width {
            break;
        }
        fitted.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    while fitted.ends_with(' ') {
        fitted.pop();
        used = used.saturating_sub(1);
    }
    fitted.push('…');
    fitted.extend(std::iter::repeat_n(' ', width.saturating_sub(used + 1)));
    fitted
}

/// Center a button label while placing an indivisible spare cell on its
/// leading side. This keeps near-full-width labels from appearing left-aligned.
pub(super) fn fit_centered_button_label(text: &str, width: usize) -> String {
    let text_width = text.width();
    if text_width > width {
        return fit_centered_ellipsis(text, width);
    }
    let remaining = width - text_width;
    let left = remaining.div_ceil(2);
    let right = remaining - left;
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

pub(super) fn file_sidebar_rows_for(area: Rect, has_git_changes: bool) -> FileSidebarRows {
    let content_start = area.y.saturating_add(3);
    let divider = has_git_changes.then(|| content_start.saturating_add(1));
    let tree_start = divider.map_or(content_start, |row| row.saturating_add(1));
    let tree_end = area.bottom().saturating_sub(3).max(tree_start);
    FileSidebarRows {
        divider,
        tree_start,
        tree_end,
    }
}

/// Shell-prompt-style Project path: home becomes `~` and every parent folder
/// is reduced to its first grapheme while the leaf stays readable.
pub(super) fn compact_project_path(path: &str) -> String {
    let home = std::env::var_os("HOME")
        .and_then(|value| value.into_string().ok())
        .filter(|value| path == value || path.starts_with(&format!("{value}/")));
    let (prefix, relative) = if let Some(home) = home {
        (
            "~",
            path.strip_prefix(&home)
                .unwrap_or(path)
                .trim_start_matches('/'),
        )
    } else if path.starts_with('/') {
        ("", path.trim_start_matches('/'))
    } else {
        ("", path)
    };
    let parts: Vec<&str> = relative
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return if prefix == "~" {
            "~".into()
        } else {
            "/".into()
        };
    }
    let mut out = if prefix == "~" {
        "~/".to_string()
    } else if path.starts_with('/') {
        "/".to_string()
    } else {
        String::new()
    };
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            out.push('/');
        }
        if index + 1 == parts.len() {
            out.push_str(part);
        } else if let Some(grapheme) = part.graphemes(true).next() {
            out.push_str(grapheme);
        }
    }
    out
}

/// Draw the pane-boundary dividers (box-drawing lines in the theme colour).
pub(super) fn draw_dividers(layout: &Layout, color: uniterm_core::Color, ops: &mut Vec<u8>) {
    if layout.dividers.is_empty() {
        return;
    }
    ops.extend_from_slice(format!("\x1b[{}m", color.sgr_fg()).as_bytes());
    for d in &layout.dividers {
        match d.dir {
            SplitDir::Horizontal => {
                for y in d.rect.y..d.rect.bottom() {
                    ops.extend_from_slice(format!("\x1b[{};{}H", y + 1, d.rect.x + 1).as_bytes());
                    ops.extend_from_slice("\u{2502}".as_bytes()); // vertical line
                }
            }
            SplitDir::Vertical => {
                ops.extend_from_slice(
                    format!("\x1b[{};{}H", d.rect.y + 1, d.rect.x + 1).as_bytes(),
                );
                for _ in 0..d.rect.w {
                    ops.extend_from_slice("\u{2500}".as_bytes()); // horizontal line
                }
            }
        }
    }
    ops.extend_from_slice(b"\x1b[0m");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_chrome_text_clips_and_pads_to_its_cell_width() {
        assert_eq!(fit_cell_text("workspace", 4), "work");
        assert_eq!(fit_cell_text("tab", 8), "tab     ");
        assert_eq!(fit_cell_text("馈x", 2), "馈");
        assert_eq!(fit_cell_text("👩‍💻x", 2), "👩‍💻");
    }

    #[test]
    fn observatory_tab_labels_center_and_ellipsize() {
        assert_eq!(fit_centered_ellipsis("Agents", 12), "   Agents   ");
        assert_eq!(fit_centered_ellipsis("Files", 12), "   Files    ");
        assert_eq!(fit_centered_ellipsis("Servers", 6), "Serve…");
        assert_eq!(fit_centered_ellipsis("Servers", 1), "…");
        assert_eq!(fit_centered_ellipsis("馈x", 6), " 馈x  ");
    }

    #[test]
    fn templates_reject_unknown_tokens_and_sanitize_terminal_controls() {
        let workspace = "Uniterm";
        assert_eq!(
            render_named_template("{{{workspace}}}", |token| {
                (token == "workspace").then_some(workspace)
            })
            .as_deref(),
            Some("{Uniterm}")
        );
        assert!(render_named_template("{misspelled}", |_| None).is_none());
        assert_eq!(
            sanitize_chrome_text("safe\x1b]9;forged\x07", 512),
            "safe]9;forged"
        );
    }

    #[test]
    fn footer_button_labels_keep_odd_padding_on_the_leading_side() {
        assert_eq!(fit_centered_button_label("New Task", 9), " New Task");
        assert_eq!(fit_centered_button_label("Tasks...", 11), "  Tasks... ");
        assert_eq!(fit_centered_button_label("Config", 11), "   Config  ");
    }

    #[test]
    fn file_sidebar_click_rows_follow_the_optional_git_summary() {
        let area = Rect::new(0, 1, 30, 12);
        let clean = file_sidebar_rows_for(area, false);
        assert_eq!(clean.divider, None);
        assert_eq!(clean.slot_at(3), None);
        assert_eq!(clean.slot_at(4), Some(0));

        let changed = file_sidebar_rows_for(area, true);
        assert_eq!(changed.divider, Some(5));
        assert_eq!(changed.slot_at(5), None);
        assert_eq!(changed.slot_at(6), Some(0));
        assert_eq!(changed.capacity(), clean.capacity().saturating_sub(2));
    }

    #[test]
    fn large_git_change_counts_stay_compact() {
        assert_eq!(format_change_count(9_999), "9999");
        assert_eq!(format_change_count(12_345), "12.3k");
        assert_eq!(format_change_count(250_000), "250k");
    }

    #[test]
    fn project_paths_use_shell_prompt_abbreviation() {
        assert_eq!(compact_project_path("/var/lib/uniterm"), "/v/l/uniterm");
        if let Some(home) = std::env::var_os("HOME").and_then(|value| value.into_string().ok()) {
            assert_eq!(
                compact_project_path(&format!("{home}/Work/uniterm")),
                "~/W/uniterm"
            );
        }
    }
}
