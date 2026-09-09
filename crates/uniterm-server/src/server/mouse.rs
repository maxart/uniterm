//! Mouse routing: chrome hit testing, selection, wheel handling, and the
//! reports forwarded to an application that asked for them.
//!
//! Clicks resolve to the same semantic commands the keybindings and the CLI
//! use, so a chrome control can never drift from its keyboard equivalent.

use super::*;

impl Server {
    pub(super) fn handle_status_mouse(
        &mut self,
        reg: &Registry,
        client: Token,
        cx: u16,
        cy: u16,
        kind: MouseKind,
    ) -> bool {
        let (_, status_row) = self.chrome_area();
        if Some(cy) != status_row {
            return false;
        }
        if kind == MouseKind::Click && cx < self.workspace_button_width() {
            self.request_chrome_menu(
                reg,
                client,
                uniterm_proto::ChromeMenu::Workspace,
                Rect::new(0, cy, self.workspace_button_width(), 1),
                self.config.status_position == StatusPosition::Bottom,
            );
            return true;
        }
        if kind == MouseKind::Click {
            if let Some((tab, _, _)) = self
                .observatory_tab_slots()
                .into_iter()
                .find(|(_, rect, _)| rect.contains(cx, cy))
            {
                self.select_observatory_tab(reg, tab);
                return true;
            }
        }

        let layout = self.tab_bar_layout(false);
        if kind == MouseKind::Click && layout.new_tab.is_some_and(|rect| rect.contains(cx, cy)) {
            self.tab_scroll_follow_active = true;
            self.handle_command(reg, Command::NewWindow);
            return true;
        }
        if kind == MouseKind::Click && layout.scroll_left.is_some_and(|rect| rect.contains(cx, cy))
        {
            self.tab_scroll = self.tab_scroll.saturating_sub(1);
            self.tab_scroll_follow_active = false;
            self.full_repaint_all(reg);
            return true;
        }
        if kind == MouseKind::Click
            && layout
                .scroll_right
                .is_some_and(|rect| rect.contains(cx, cy))
        {
            self.tab_scroll = self.tab_scroll.saturating_add(1);
            self.tab_scroll_follow_active = false;
            self.full_repaint_all(reg);
            return true;
        }
        if matches!(kind, MouseKind::WheelUp | MouseKind::WheelDown)
            && cx >= self.workspace_button_width()
            && cx < self.cols.saturating_sub(self.observatory_width())
        {
            self.tab_scroll = if kind == MouseKind::WheelUp {
                self.tab_scroll.saturating_sub(1)
            } else {
                self.tab_scroll.saturating_add(1)
            };
            self.tab_scroll_follow_active = false;
            self.full_repaint_all(reg);
            return true;
        }
        let Some(slot) = layout
            .tabs
            .into_iter()
            .find(|slot| slot.rect.contains(cx, cy))
        else {
            return true;
        };
        let Some(window) = self
            .project_window_indices(self.active_project)
            .get(slot.item)
            .copied()
        else {
            return true;
        };
        if matches!(kind, MouseKind::Click | MouseKind::RightClick) {
            if window != self.active_window {
                self.activate_window(window);
                self.relayout();
                self.tab_scroll_follow_active = true;
                self.full_repaint_all(reg);
                self.persist();
            }
            if kind == MouseKind::RightClick {
                self.request_chrome_menu(
                    reg,
                    client,
                    uniterm_proto::ChromeMenu::Tabs,
                    slot.rect,
                    self.config.status_position == StatusPosition::Bottom,
                );
            }
        }
        true
    }

    /// Resolve a mouse event at 1-based cell `(x, y)`: a click on a status-line
    /// window number selects that window; a hover or click over a pane focuses
    /// it. Focus-follows-mouse (`?1003h`), so hover moves focus.
    pub(super) fn on_mouse(
        &mut self,
        reg: &Registry,
        client: Token,
        x: u16,
        y: u16,
        kind: MouseKind,
    ) {
        let cx = x.saturating_sub(1);
        let cy = y.saturating_sub(1);
        if self.handle_status_mouse(reg, client, cx, cy, kind) {
            return;
        }
        let sidebar = self.sidebar_width();
        if kind == MouseKind::RightClick && sidebar > 0 && cx < sidebar {
            let project = self
                .project_slots()
                .into_iter()
                .find(|slot| cy >= slot.rect.y && cy < slot.rect.bottom())
                .and_then(|slot| {
                    self.projects
                        .get(slot.item)
                        .map(|project| (project.id, slot.rect))
                });
            let (menu, anchor) = project.map_or_else(
                || {
                    (
                        uniterm_proto::ChromeMenu::Projects,
                        Rect::new(0, cy, sidebar.saturating_sub(1), 1),
                    )
                },
                |(project, rect)| {
                    (
                        uniterm_proto::ChromeMenu::Project(project),
                        Rect::new(0, rect.y, sidebar.saturating_sub(1), rect.h),
                    )
                },
            );
            self.request_chrome_menu(reg, client, menu, anchor, cy >= self.rows / 2);
            return;
        }
        if kind == MouseKind::RightClick {
            if self.config.pane_right_click {
                if let Some(pane) = self
                    .current_layout
                    .panes
                    .iter()
                    .find(|(_, rect)| rect.contains(cx, cy))
                    .map(|(pane, _)| *pane)
                    .filter(|pane| {
                        self.panes
                            .get(pane)
                            .is_some_and(|pane| pane.term.mouse_mode() != MouseMode::Off)
                    })
                {
                    let wi = self.active_window;
                    if self.windows[wi].active != pane {
                        self.last_active_pane = Some(self.windows[wi].active);
                        self.windows[wi].active = pane;
                        self.windows[wi].zoomed = None;
                        self.relayout();
                        self.full_repaint_all(reg);
                    }
                    self.forward_mouse_to_app(reg, pane, cx, cy, kind);
                    return;
                }
            }
            self.open_context_menu(reg, cx, cy);
            return;
        }
        if self.context_menu.is_some() {
            self.handle_context_mouse(reg, cx, cy, kind);
            return;
        }

        let observatory_width = self.observatory_width();
        if observatory_width > 0 && cx >= self.cols.saturating_sub(observatory_width) {
            let (area, _) = self.chrome_area();
            match self.observatory_tab {
                ObservatoryTab::Agents => {
                    if kind == MouseKind::Click {
                        if let Some((action, _, _)) = self
                            .observatory_agent_action_slots()
                            .into_iter()
                            .find(|(_, rect, _)| rect.contains(cx, cy))
                        {
                            if let Some(client_state) = self.clients.get_mut(&client) {
                                client_state.queue(&encode_frame(
                                    &ServerMessage::OpenChromeAction { action },
                                ));
                                client_state.flush();
                                let _ = set_interest(reg, client_state, client);
                            }
                            return;
                        }
                    }
                    if cy == area.bottom().saturating_sub(1) {
                        return;
                    }
                    let (scope_x, scope_width, _) =
                        self.observatory_scope_button(self.sidebar_agent_scope);
                    if kind == MouseKind::Click
                        && cy == area.y.saturating_add(1)
                        && cx >= scope_x
                        && cx < scope_x.saturating_add(scope_width)
                    {
                        self.sidebar_agent_scope = self.sidebar_agent_scope.toggle();
                        self.observatory_scroll[ObservatoryTab::Agents.index()] = 0;
                        self.full_repaint_all(reg);
                        self.persist_workspace_definition();
                        return;
                    }
                    let entries = self.observatory_agent_entries();
                    let slots = self.observatory_agent_slots(entries.len());
                    if matches!(kind, MouseKind::WheelUp | MouseKind::WheelDown) {
                        let index = ObservatoryTab::Agents.index();
                        self.observatory_scroll[index] = if kind == MouseKind::WheelUp {
                            self.observatory_scroll[index].saturating_sub(1)
                        } else {
                            self.observatory_scroll[index].saturating_add(1)
                        };
                        self.full_repaint_all(reg);
                    } else if kind == MouseKind::Click {
                        if let Some((pane, project)) = chrome::card_at(&slots, cy)
                            .and_then(|index| entries.get(index))
                            .copied()
                        {
                            if let Some(window) = self
                                .windows
                                .iter()
                                .position(|tab| tab.layout.contains_pane(pane))
                            {
                                if project != self.active_project {
                                    self.append_event(crate::eventlog::LogEvent::ProjectSelected {
                                        project: project.0,
                                    });
                                }
                                self.activate_window(window);
                                self.windows[window].active = pane;
                                self.tab_scroll_follow_active = true;
                                self.relayout();
                                self.full_repaint_all(reg);
                                self.persist();
                            }
                        }
                    }
                }
                ObservatoryTab::Files => {
                    if matches!(kind, MouseKind::WheelUp | MouseKind::WheelDown) {
                        self.files.focused = true;
                        let input: &[u8] = if kind == MouseKind::WheelUp {
                            b"\x1b[A"
                        } else {
                            b"\x1b[B"
                        };
                        let action = self.files.handle(input);
                        self.handle_file_action(reg, action);
                    } else if kind == MouseKind::Click {
                        let rows = self.file_sidebar_rows(area);
                        if let Some(slot) = rows.slot_at(cy) {
                            let capacity = rows.capacity();
                            let first = self.files.first_visible(capacity);
                            let action = self.files.click(slot, first, capacity);
                            self.handle_file_action(reg, action);
                        } else {
                            self.files.focused = true;
                            self.full_repaint_all(reg);
                        }
                    }
                }
                ObservatoryTab::WebServers => {
                    let (scope_x, scope_width, _) =
                        self.observatory_scope_button(self.sidebar_server_scope);
                    if kind == MouseKind::Click
                        && cy == area.y.saturating_add(1)
                        && cx >= scope_x
                        && cx < scope_x.saturating_add(scope_width)
                    {
                        self.sidebar_server_scope = self.sidebar_server_scope.toggle();
                        self.observatory_scroll[ObservatoryTab::WebServers.index()] = 0;
                        self.full_repaint_all(reg);
                        self.persist_workspace_definition();
                        return;
                    }
                    let servers = self.observatory_dev_server_entries();
                    let slots = self.observatory_web_slots(servers.len());
                    if matches!(kind, MouseKind::WheelUp | MouseKind::WheelDown) {
                        let index = ObservatoryTab::WebServers.index();
                        self.observatory_scroll[index] = if kind == MouseKind::WheelUp {
                            self.observatory_scroll[index].saturating_sub(1)
                        } else {
                            self.observatory_scroll[index].saturating_add(1)
                        };
                        self.full_repaint_all(reg);
                    } else if kind == MouseKind::Click {
                        if let Some(url) = chrome::card_at(&slots, cy)
                            .and_then(|index| servers.get(index))
                            .map(|server| server.url.clone())
                        {
                            if let Some(client_state) = self.clients.get_mut(&client) {
                                client_state.queue(&encode_frame(&ServerMessage::OpenUrl { url }));
                                client_state.flush();
                                let _ = set_interest(reg, client_state, client);
                            }
                        }
                    }
                }
            }
            return;
        }

        if sidebar > 0 && cx < sidebar {
            if matches!(kind, MouseKind::WheelUp | MouseKind::WheelDown) {
                self.project_scroll = if kind == MouseKind::WheelUp {
                    self.project_scroll.saturating_sub(1)
                } else {
                    self.project_scroll.saturating_add(1)
                };
                self.full_repaint_all(reg);
            } else if kind == MouseKind::Click {
                if let Some(index) = chrome::card_at(&self.project_slots(), cy) {
                    if let Some(project) = self.projects.get(index).map(|project| project.id) {
                        self.switch_project(reg, project);
                    }
                }
            }
            return;
        }

        // The overview captures the mouse: hover selects the tile under the
        // pointer, a click switches to it.
        if let Some(sel) = self.overview {
            let (area, _) = self.chrome_area();
            let tiles = uniterm_core::layout::overview_tiles(
                area,
                self.project_window_indices(self.active_project).len(),
            );
            let hit = tiles.iter().position(|t| t.contains(cx, cy));
            match (kind, hit) {
                (MouseKind::Click, Some(i)) => self.leave_overview(reg, Some(i)),
                (MouseKind::Hover, Some(i)) if i != sel => {
                    self.overview = Some(i);
                    self.full_repaint_all(reg);
                }
                _ => {}
            }
            return;
        }
        if kind == MouseKind::Click
            && self
                .notification_rect()
                .is_some_and(|rect| rect.contains(cx, cy))
        {
            let pane = self.notification.as_ref().map(|toast| toast.pane);
            self.notification = None;
            if let Some(pane) = pane {
                if let Some(window) = self
                    .windows
                    .iter()
                    .position(|tab| tab.layout.contains_pane(pane))
                {
                    self.activate_window(window);
                    self.windows[window].active = pane;
                    self.relayout();
                }
            }
            self.full_repaint_all(reg);
            return;
        }
        // A divider under the left button resizes its split: the pointer sets
        // the ratio directly, so the divider follows the hand. Dividers are
        // not inside any Pane, so nothing here competes with app mouse modes.
        if let Some(divider) = self.divider_drag {
            match kind {
                MouseKind::Drag => {
                    let (area, _) = self.chrome_area();
                    let wi = self.active_window;
                    if self.windows[wi].zoomed.is_none()
                        && self.windows[wi]
                            .layout
                            .set_divider_at(area, divider.rect, cx, cy)
                    {
                        self.relayout();
                        self.divider_drag =
                            moved_divider(&self.current_layout, divider, cx, cy).or(Some(divider));
                        self.full_repaint_all(reg);
                    }
                    return;
                }
                MouseKind::Release => {
                    self.divider_drag = None;
                    self.persist();
                    return;
                }
                _ => {}
            }
        }
        if kind == MouseKind::Click && self.mouse_sel.is_none() {
            if let Some(divider) = self
                .current_layout
                .dividers
                .iter()
                .find(|divider| divider.rect.contains(cx, cy))
            {
                self.divider_drag = Some(*divider);
                return;
            }
        }
        // Copy-mode's Latest button acts on release so a drag that starts on
        // it can still become an ordinary selection. Releasing on the button
        // resumes the live screen and consumes the unmatched app release.
        if kind == MouseKind::Release {
            let target = self
                .current_layout
                .panes
                .iter()
                .find_map(|(pane_id, rect)| {
                    let pane = self.panes.get(pane_id)?;
                    let copy = pane.copy.as_ref()?;
                    copy.latest_button_rect(pane.term.grid(), *rect)
                        .filter(|button| button.contains(cx, cy))
                        .map(|_| *pane_id)
                });
            if let Some(pane_id) = target {
                self.mouse_sel = None;
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.copy = None;
                }
                self.full_repaint_all(reg);
                return;
            }
        }
        // An in-flight text selection follows the press's pane even when the
        // pointer leaves it, so handle drag/release before the pane hit-test.
        if kind == MouseKind::Release {
            if let Some((target, url)) = self.plain_click_url(cx, cy) {
                self.mouse_sel = None;
                if let Some(client) = self.clients.get_mut(&target) {
                    client.queue(&encode_frame(&ServerMessage::OpenUrl { url }));
                    client.flush();
                    let _ = set_interest(reg, client, target);
                }
                return;
            }
        }
        match kind {
            MouseKind::Drag if self.drag_selection(reg, cx, cy) => return,
            MouseKind::Release if self.finish_selection(reg, cx, cy) => return,
            _ => {}
        }
        // Otherwise resolve the pane under the cursor.
        let hit = self
            .current_layout
            .panes
            .iter()
            .find(|(_, r)| r.contains(cx, cy))
            .map(|(p, _)| *p);
        let Some(pid) = hit else {
            return;
        };
        match kind {
            MouseKind::Hover | MouseKind::Click => {
                if kind == MouseKind::Hover && !self.config.focus_follows_mouse {
                    return;
                }
                let wi = self.active_window;
                if self.windows[wi].active != pid {
                    self.last_active_pane = Some(self.windows[wi].active);
                    self.windows[wi].active = pid;
                    self.relayout();
                    self.full_repaint_all(reg);
                }
                if kind == MouseKind::Click && self.arm_selection(client, pid, cx, cy) {
                    // The press waits for the release to say whether it was
                    // a plain click (delivered then) or started a selection.
                    return;
                }
                self.forward_mouse_to_app(reg, pid, cx, cy, kind);
            }
            MouseKind::Drag | MouseKind::Release => {
                self.forward_mouse_to_app(reg, pid, cx, cy, kind);
            }
            MouseKind::WheelUp | MouseKind::WheelDown => {
                self.on_wheel(reg, pid, cx, cy, kind == MouseKind::WheelUp);
            }
            MouseKind::RightClick => {}
        }
    }

    /// Arm a text selection at a press. An app that owns the mouse (vim with
    /// `mouse=a`, an agent UI on the alternate screen) normally receives the
    /// press instead and does its own selection. With `freeze-on-select`
    /// uniterm keeps left-button drags for itself everywhere: the press is
    /// withheld (returns `true`) until the release shows whether it was a
    /// plain click, which the app then receives whole. A pane frozen in
    /// copy-mode is always selectable.
    pub(super) fn arm_selection(
        &mut self,
        client: Token,
        pane_id: PaneId,
        cx: u16,
        cy: u16,
    ) -> bool {
        let Some(pane) = self.panes.get(&pane_id) else {
            return false;
        };
        let app_owns_mouse = pane.copy.is_none() && pane.term.mouse_mode() != MouseMode::Off;
        if app_owns_mouse && !self.config.freeze_on_select {
            self.mouse_sel = None;
            return false;
        }
        self.mouse_sel = Some(MouseSel {
            client,
            pane: pane_id,
            press: (cx, cy),
            selecting: false,
            deferred_press: app_owns_mouse,
        });
        app_owns_mouse
    }

    pub(super) fn plain_click_url(&self, cx: u16, cy: u16) -> Option<(Token, String)> {
        let selection = self.mouse_sel?;
        if selection.selecting || selection.press != (cx, cy) {
            return None;
        }
        let rect = self.current_layout.rect_of(selection.pane)?;
        let pane = self.panes.get(&selection.pane)?;
        if pane.copy.is_some() || pane.term.mouse_mode() != MouseMode::Off {
            return None;
        }
        let x = cx.checked_sub(rect.x)?;
        let y = cy.checked_sub(rect.y)?;
        pane.term.url_at(x, y).map(|url| (selection.client, url))
    }

    /// Continue a click-drag text selection; `false` when none is armed (the
    /// drag then falls through to app forwarding). The first drag enters
    /// copy-mode (if needed) and anchors at the press cell.
    pub(super) fn drag_selection(&mut self, reg: &Registry, cx: u16, cy: u16) -> bool {
        let Some(mut ms) = self.mouse_sel else {
            return false;
        };
        let freeze_on_select = self.config.freeze_on_select;
        let (Some(rect), Some(pane)) = (
            self.current_layout.rect_of(ms.pane),
            self.panes.get_mut(&ms.pane),
        ) else {
            self.mouse_sel = None;
            return false;
        };
        if pane.copy.is_none() {
            pane.copy = Some(CopyState::new(pane.term.grid(), rect));
        }
        let copy = pane.copy.as_mut().expect("just ensured");
        if !ms.selecting {
            // The setting is read at the drag, so a copy-mode opened earlier
            // by the wheel or the keyboard honours a change made since.
            copy.set_freeze_on_select(freeze_on_select);
            copy.mouse_anchor(pane.term.grid(), rect, ms.press.0, ms.press.1);
            ms.selecting = true;
        }
        copy.mouse_drag(rect, cx, cy);
        self.mouse_sel = Some(ms);
        self.full_repaint_all(reg);
        true
    }

    /// Finish a click-drag selection on release. With `copy-on-select` the
    /// text is yanked to the clipboard (OSC 52) and the pane returns to the
    /// live screen; otherwise the selection stays highlighted in the frozen
    /// pane until `y` or Enter copies it, or Esc, `q`, or a plain click
    /// dismisses it. `false` when no drag selected anything and the click is
    /// not uniterm's to answer.
    pub(super) fn finish_selection(&mut self, reg: &Registry, cx: u16, cy: u16) -> bool {
        let Some(ms) = self.mouse_sel.take() else {
            return false;
        };
        if !ms.selecting {
            if ms.deferred_press {
                // A plain click in an app that owns the mouse: deliver the
                // press it was owed, then the release, both to that pane.
                self.forward_mouse_to_app(reg, ms.pane, ms.press.0, ms.press.1, MouseKind::Click);
                self.forward_mouse_to_app(reg, ms.pane, cx, cy, MouseKind::Release);
                return true;
            }
            let finished = self
                .panes
                .get(&ms.pane)
                .and_then(|pane| pane.copy.as_ref())
                .is_some_and(CopyState::mouse_selection_finished);
            if finished {
                if let Some(pane) = self.panes.get_mut(&ms.pane) {
                    pane.copy = None;
                }
                self.full_repaint_all(reg);
                return true;
            }
            return false;
        }
        let copy_on_select = self.config.copy_on_select;
        if let Some(pane) = self.panes.get_mut(&ms.pane) {
            if !copy_on_select {
                if let Some(copy) = pane.copy.as_mut() {
                    copy.finish_mouse_selection();
                }
            } else if let Some(copy) = pane.copy.take() {
                let text = copy.yank(pane.term.grid());
                if !text.trim().is_empty() {
                    let clip = crate::copymode::osc52(&text);
                    self.send_raw_ops(reg, &clip);
                }
            }
        }
        self.full_repaint_all(reg);
        true
    }

    /// Forward a mouse event to the pane's app as a translated (pane-relative)
    /// report, if - and at the granularity - the app asked for mouse tracking.
    /// A pane frozen in copy-mode gets nothing.
    pub(super) fn forward_mouse_to_app(
        &mut self,
        reg: &Registry,
        pane_id: PaneId,
        cx: u16,
        cy: u16,
        kind: MouseKind,
    ) {
        let Some(rect) = self.current_layout.rect_of(pane_id) else {
            return;
        };
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        if pane.copy.is_some() {
            return;
        }
        let mode = pane.term.mouse_mode();
        let wanted = match (mode, kind) {
            (MouseMode::Off, _) => false,
            (MouseMode::X10, k) => k == MouseKind::Click, // X10: presses only
            (m, MouseKind::Hover) => m == MouseMode::Any,
            (m, MouseKind::Drag) => matches!(m, MouseMode::Button | MouseMode::Any),
            (_, MouseKind::RightClick) => self.config.pane_right_click,
            _ => true, // Normal and up: press, release, wheel
        };
        if !wanted {
            return;
        }
        // X11-style button codes: 0 left, +32 motion flag, 3+32 no-button motion.
        let (btn, press) = match kind {
            MouseKind::Click => (0u8, true),
            MouseKind::Release => (0, false),
            MouseKind::Drag => (32, true),
            MouseKind::Hover => (35, true),
            MouseKind::WheelUp => (64, true),
            MouseKind::WheelDown => (65, true),
            MouseKind::RightClick => (2, true),
        };
        let px = cx.saturating_sub(rect.x) + 1;
        let py = cy.saturating_sub(rect.y) + 1;
        let seq = encode_mouse_report(pane.term.mouse_sgr(), btn, press, px, py);
        Self::queue_pane_input(reg, pane, &seq);
    }

    /// Route a wheel event over a pane: copy-mode scrolls its frozen viewport;
    /// an app that asked for mouse tracking gets the report (translated to
    /// pane-relative coordinates); other alt-screen apps get arrow keys (the
    /// standard muxer emulation); otherwise the wheel scrolls the pane's
    /// scrollback via copy-mode, and wheel-down at the live bottom leaves it.
    pub(super) fn on_wheel(&mut self, reg: &Registry, pane_id: PaneId, cx: u16, cy: u16, up: bool) {
        /// Lines per wheel notch, matching common terminal defaults.
        const WHEEL_LINES: i32 = 3;
        let Some(rect) = self.current_layout.rect_of(pane_id) else {
            return;
        };
        let freeze_on_select = self.config.freeze_on_select;
        let mut repaint = false;
        {
            let Some(pane) = self.panes.get_mut(&pane_id) else {
                return;
            };
            if let Some(copy) = pane.copy.as_mut() {
                let delta = if up { -WHEEL_LINES } else { WHEEL_LINES };
                let at_bottom = copy.scroll(pane.term.grid(), delta);
                if !up && at_bottom {
                    pane.copy = None; // back at the live screen: resume following
                }
                repaint = true;
            } else if matches!(
                pane.term.mouse_mode(),
                MouseMode::Normal | MouseMode::Button | MouseMode::Any
            ) {
                let btn = if up { 64 } else { 65 };
                let px = cx.saturating_sub(rect.x) + 1;
                let py = cy.saturating_sub(rect.y) + 1;
                let seq = encode_mouse_report(pane.term.mouse_sgr(), btn, true, px, py);
                Self::queue_pane_input(reg, pane, &seq);
            } else if pane.term.is_alt_screen() {
                // No mouse mode but a full-screen app: emulate with arrows.
                let arrow: &[u8] = match (pane.term.app_cursor(), up) {
                    (true, true) => b"\x1bOA",
                    (true, false) => b"\x1bOB",
                    (false, true) => b"\x1b[A",
                    (false, false) => b"\x1b[B",
                };
                let seq: Vec<u8> = arrow.repeat(WHEEL_LINES as usize);
                Self::queue_pane_input(reg, pane, &seq);
            } else if up && pane.term.grid().scrollback_len() > 0 {
                // Plain shell screen: wheel-up opens the scrollback.
                let grid = pane.term.grid();
                let mut copy = CopyState::new(grid, rect);
                copy.set_freeze_on_select(freeze_on_select);
                copy.scroll(grid, -WHEEL_LINES);
                pane.copy = Some(copy);
                repaint = true;
            }
        }
        if repaint {
            self.full_repaint_all(reg);
        }
    }
}

/// Encode a mouse event for a pane's app: SGR (`?1006h`) or legacy X10 bytes,
/// with 1-based pane-relative coordinates. Legacy coordinates are clamped to
/// the encodable 223 maximum.
pub(super) fn encode_mouse_report(sgr: bool, btn: u8, press: bool, px: u16, py: u16) -> Vec<u8> {
    if sgr {
        format!(
            "\x1b[<{};{};{}{}",
            btn,
            px,
            py,
            if press { 'M' } else { 'm' }
        )
        .into_bytes()
    } else {
        // ESC [ M Cb Cx Cy, all offset by 32; a release encodes as button 3.
        let b = if press { btn } else { 3 };
        vec![
            0x1b,
            b'[',
            b'M',
            32 + b,
            32 + px.min(223) as u8,
            32 + py.min(223) as u8,
        ]
    }
}

/// After a drag moved `previous`, find the same divider in the recomputed
/// layout: the one with the same orientation and cross-axis span that now sits
/// nearest the pointer along the drag axis.
fn moved_divider(
    layout: &Layout,
    previous: uniterm_core::Divider,
    x: u16,
    y: u16,
) -> Option<uniterm_core::Divider> {
    layout
        .dividers
        .iter()
        .filter(|candidate| candidate.dir == previous.dir)
        .filter(|candidate| match previous.dir {
            SplitDir::Horizontal => {
                candidate.rect.y == previous.rect.y && candidate.rect.h == previous.rect.h
            }
            SplitDir::Vertical => {
                candidate.rect.x == previous.rect.x && candidate.rect.w == previous.rect.w
            }
        })
        .min_by_key(|candidate| match previous.dir {
            SplitDir::Horizontal => candidate.rect.x.abs_diff(x),
            SplitDir::Vertical => candidate.rect.y.abs_diff(y),
        })
        .copied()
}
