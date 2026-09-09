//! The Manage Agents modal (Agents menu > Setup...): a near-fullscreen modal
//! with the provider registry on the left (signature colours, install state,
//! live pane counts) and a friendly detail pane on the right - install/toggle
//! the notify-hook connector, start an agent in the current pane / a new pane /
//! a new tab, and stop every running agent (two-step confirm, like delete in
//! the task manager).
//!
//! Pure state + rendering + hit-testing, like the other client surfaces: the
//! attach loop feeds keys/mouse in and sends the returned ops to the server;
//! the server answers every mutation with a fresh snapshot, so the modal is
//! always a projection of server truth.

use crate::overlay::{
    finish_lines, footer_spans, footer_text, modal_hit, modal_rect, modal_visible_rows, nav_list,
    panel_style, panel_style_no_reset, render_list_modal, styled_line, ui_theme, ModalHit, Rect,
};
use uniterm_core::agent::agent_color;
use uniterm_proto::{AgentInfo, ConnectorStatus, LaunchTarget};

/// What a key/click asks the attach loop to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentsAction {
    None,
    /// State changed; repaint the modal.
    Redraw,
    /// Close the modal (and Refresh the screen under it).
    Close,
    /// Send: install/remove this agent's notify-hook connector.
    ToggleConnector(String),
    /// Send: start this agent at the given target (the modal then closes so
    /// the user lands on the agent).
    Launch(String, LaunchTarget),
    /// Send: stop every running agent in the session.
    StopAll,
}

/// The modal's input mode.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    Browse,
    /// `X` pressed once; a second `X` stops all agents, anything else cancels.
    ConfirmStop,
}

/// The open Manage Agents modal.
pub struct AgentsView {
    pub items: Vec<AgentInfo>,
    pub sel: usize,
    scroll: usize,
    mode: Mode,
}

/// The action bar's pairs (shared overlay styling); index 0 is the
/// non-clickable navigation hint.
const BUTTONS: &[(&str, &str)] = &[
    ("\u{2191}\u{2193}", "select"),
    ("c", "current"),
    ("p", "pane"),
    ("t", "tab"),
    ("i", "connector"),
    ("X", "stop all"),
    ("esc", "close"),
];

/// Width of the list column (incl. its padding), fixed for a stable layout.
const LIST_W: u16 = 28;

impl AgentsView {
    pub fn new(items: Vec<AgentInfo>) -> Self {
        AgentsView {
            items,
            sel: 0,
            scroll: 0,
            mode: Mode::Browse,
        }
    }

    /// Replace the items with a fresh server snapshot, keeping the selection
    /// stable by agent id where possible.
    pub fn refresh(&mut self, items: Vec<AgentInfo>) {
        let keep = self.items.get(self.sel).map(|a| a.id.clone());
        self.items = items;
        self.sel = keep
            .and_then(|id| self.items.iter().position(|a| a.id == id))
            .unwrap_or(0)
            .min(self.items.len().saturating_sub(1));
        self.mode = Mode::Browse;
    }

    /// The modal's box rectangle: the shared near-fullscreen geometry.
    pub fn rect(cols: u16, rows: u16) -> Rect {
        modal_rect(cols, rows)
    }

    fn selected(&self) -> Option<&AgentInfo> {
        self.items.get(self.sel)
    }

    fn running_total(&self) -> u32 {
        self.items.iter().map(|a| a.running).sum()
    }

    fn nav(&mut self, down: bool, visible: usize) {
        nav_list(
            &mut self.sel,
            &mut self.scroll,
            down,
            self.items.len(),
            visible,
        );
    }

    /// A launch request for the selected agent, if it is actually launchable.
    fn launch(&self, target: LaunchTarget) -> AgentsAction {
        match self.selected() {
            Some(a) if a.installed => AgentsAction::Launch(a.id.clone(), target),
            _ => AgentsAction::None,
        }
    }

    /// Drive the modal from raw key bytes. Only the first action-producing
    /// key of a chunk is honoured (modal semantics).
    pub fn handle(&mut self, chunk: &[u8], cols: u16, rows: u16) -> AgentsAction {
        let visible = modal_visible_rows(Self::rect(cols, rows).h);
        let mut redraw = false;
        let mut i = 0;
        while i < chunk.len() {
            let b = chunk[i];
            if self.mode == Mode::ConfirmStop {
                self.mode = Mode::Browse;
                if b == b'X' {
                    return AgentsAction::StopAll;
                }
                redraw = true;
                i += 1;
                continue;
            }
            if b == 0x1b {
                if chunk.get(i + 1) == Some(&b'[') {
                    match chunk.get(i + 2) {
                        Some(b'A') => self.nav(false, visible),
                        Some(b'B') => self.nav(true, visible),
                        _ => {}
                    }
                    redraw = true;
                    i += 3;
                    continue;
                }
                return AgentsAction::Close; // lone Esc
            }
            match b {
                b'q' | 0x03 => return AgentsAction::Close,
                b'k' => {
                    self.nav(false, visible);
                    redraw = true;
                }
                b'j' => {
                    self.nav(true, visible);
                    redraw = true;
                }
                b'c' => return self.launch(LaunchTarget::CurrentPane),
                b'p' | 0x0d | 0x0a => return self.launch(LaunchTarget::NewPane),
                b't' => return self.launch(LaunchTarget::NewWindow),
                b'i' => {
                    if let Some(a) = self.selected() {
                        if a.connector != ConnectorStatus::Unsupported {
                            return AgentsAction::ToggleConnector(a.id.clone());
                        }
                    }
                }
                b'X' if self.running_total() > 0 => {
                    self.mode = Mode::ConfirmStop;
                    redraw = true;
                }
                _ => {}
            }
            i += 1;
        }
        if redraw {
            AgentsAction::Redraw
        } else {
            AgentsAction::None
        }
    }

    /// Resolve a click at 1-based `(cx, cy)`: select a list row, press an
    /// action button, or (outside the box) close.
    pub fn click(&mut self, cols: u16, rows: u16, cx: u16, cy: u16) -> AgentsAction {
        let r = Self::rect(cols, rows);
        match modal_hit(r, LIST_W, cx, cy) {
            ModalHit::Outside => AgentsAction::Close,
            ModalHit::Bar(rel) => {
                for (span, key) in bar_spans() {
                    if span.contains(&rel) {
                        return match key {
                            "c" => self.handle(b"c", cols, rows),
                            "p" => self.handle(b"p", cols, rows),
                            "t" => self.handle(b"t", cols, rows),
                            "i" => self.handle(b"i", cols, rows),
                            "X" => self.handle(b"X", cols, rows),
                            _ => AgentsAction::Close,
                        };
                    }
                }
                AgentsAction::None
            }
            ModalHit::ListRow(slot) => {
                let row = slot + self.scroll;
                if row < self.items.len() && row < self.scroll + modal_visible_rows(r.h) {
                    self.sel = row;
                    self.mode = Mode::Browse;
                    return AgentsAction::Redraw;
                }
                AgentsAction::None
            }
            ModalHit::None => AgentsAction::None,
        }
    }

    /// Render the modal through the shared list+detail frame.
    pub fn render(&self, cols: u16, rows: u16) -> Vec<u8> {
        let r = Self::rect(cols, rows);
        let panel = panel_style();
        let theme = ui_theme();
        let selected_style = format!(
            "\x1b[{};{}m",
            theme.status_active_bg.sgr_bg(),
            theme.status_active_fg.sgr_fg()
        );
        let inner = r.w.saturating_sub(2) as usize;
        let list_w = LIST_W as usize;
        let visible = modal_visible_rows(r.h);
        let detail = self.detail_lines(inner.saturating_sub(list_w + 1), visible);
        let installed = self.items.iter().filter(|a| a.installed).count();
        render_list_modal(
            cols,
            rows,
            &format!(
                " Agents ({installed} installed \u{00B7} {} running) ",
                self.running_total()
            ),
            list_w,
            |slot| {
                let idx = self.scroll + slot;
                let a = self.items.get(idx)?;
                let selected = idx == self.sel;
                let dot = provider_dot(a);
                let mut name = a.name.clone();
                if a.running > 0 {
                    name.push_str(&format!(" ({})", a.running));
                }
                // The cell is ` ● Name (n)` filled to EXACTLY list_w cells.
                let name: String = name.chars().take(list_w - 5).collect();
                let fill = " ".repeat(list_w - 3 - name.chars().count());
                let name_fg = if a.installed {
                    theme.foreground
                } else {
                    theme.muted
                }
                .sgr_fg();
                Some(if selected {
                    format!(
                        "{selected_style} \x1b[{dot}m\u{25CF}\x1b[{name_fg}m {name}{fill}\x1b[0m{panel}"
                    )
                } else {
                    format!(" \x1b[{dot}m\u{25CF}{panel}\x1b[{name_fg}m {name}{fill}{panel}")
                })
            },
            &detail,
            &self.bar_text(inner),
        )
    }

    /// The action bar's text (mode-dependent), padded to `width`, in the
    /// shared overlay footer styling.
    fn bar_text(&self, width: usize) -> String {
        let pairs: &[(&str, &str)] = match &self.mode {
            Mode::ConfirmStop => &[("X", "confirm stop all"), ("any key", "cancels")],
            Mode::Browse => BUTTONS,
        };
        footer_text(pairs, width)
    }

    /// The detail pane's lines (styled, padded to `width`), `count` rows -
    /// the friendly half: what this agent is, what works, and what each key
    /// will do to it.
    fn detail_lines(&self, width: usize, count: usize) -> Vec<String> {
        let panel = panel_style_no_reset();
        let theme = ui_theme();
        let dim = format!("\x1b[{}m", theme.muted.sgr_fg());
        let dim = dim.as_str();
        let on = format!("\x1b[{}m", theme.success.sgr_fg());
        let on = on.as_str();
        let off = format!("\x1b[{}m", theme.muted.sgr_fg());
        let off = off.as_str();
        let error = format!("\x1b[1;{}m", theme.error.sgr_fg());
        let mk = styled_line;
        let mut out: Vec<(String, usize)> = Vec::new();
        let Some(a) = self.selected() else {
            out.push(mk(&[]));
            out.push(mk(&[(&panel, "  "), (dim, "No agents in the registry.")]));
            return finish_lines(out, &panel, width, count);
        };
        // Header: every agent name uses ANSI Compact in its signature colour;
        // plain text is the narrow-panel fallback.
        let title_style = format!("\x1b[1;{}m", provider_dot(a));
        out.push(mk(&[]));
        let banner = uniterm_core::agent::agent_logo(&a.id)
            .filter(|art| art.iter().all(|l| l.chars().count() + 2 <= width));
        match banner {
            Some(art) => {
                for line in &art {
                    out.push(mk(&[(&panel, "  "), (&title_style, line)]));
                }
            }
            None => out.push(mk(&[(&panel, "  "), (&title_style, &a.name)])),
        }
        out.push(mk(&[]));
        // The facts column: command, CLI, connector, running.
        let cmd = a.command.clone();
        out.push(mk(&[(&panel, "  "), (dim, "command     "), (&panel, &cmd)]));
        if a.installed {
            out.push(mk(&[
                (&panel, "  "),
                (dim, "cli         "),
                (on, "\u{25CF} installed"),
            ]));
        } else {
            let miss = format!("\u{25CB} not found on PATH ({})", a.command);
            out.push(mk(&[(&panel, "  "), (dim, "cli         "), (off, &miss)]));
        }
        match a.connector {
            ConnectorStatus::Installed => {
                out.push(mk(&[
                    (&panel, "  "),
                    (dim, "connector   "),
                    (on, "\u{25CF} on"),
                    (dim, " - live status flows to the Observatory"),
                ]));
                // The hook config is read at agent startup, so a connector
                // installed mid-flight cannot reach already-running agents.
                out.push(mk(&[
                    (&panel, "              "),
                    (dim, "(loads when the agent starts - restart running ones)"),
                ]));
            }
            ConnectorStatus::NotInstalled => out.push(mk(&[
                (&panel, "  "),
                (dim, "connector   "),
                (off, "\u{25CB} off"),
                (dim, " - press i to install the notify hook"),
            ])),
            ConnectorStatus::Unsupported => out.push(mk(&[
                (&panel, "  "),
                (dim, "connector   "),
                (dim, "- none for this agent (fallback detection)"),
            ])),
        }
        let run = match a.running {
            0 => "-".to_string(),
            1 => "1 pane".to_string(),
            n => format!("{n} panes"),
        };
        let run_style = if a.running > 0 { on } else { dim };
        out.push(mk(&[
            (&panel, "  "),
            (dim, "running     "),
            (run_style, &run),
        ]));
        out.push(mk(&[]));
        // What the keys do, right here where the eye is.
        if a.installed {
            out.push(mk(&[(&panel, "  "), (dim, "start")]));
            out.push(mk(&[
                (&panel, "    "),
                (&panel, "c"),
                (dim, "  in the current pane"),
            ]));
            out.push(mk(&[
                (&panel, "    "),
                (&panel, "p"),
                (dim, "  in a new pane to the right (also enter)"),
            ]));
            out.push(mk(&[
                (&panel, "    "),
                (&panel, "t"),
                (dim, "  in a new tab"),
            ]));
        } else {
            out.push(mk(&[
                (&panel, "  "),
                (dim, "install the CLI to start this agent from here"),
            ]));
        }
        if self.mode == Mode::ConfirmStop {
            out.push(mk(&[]));
            let warn = format!(
                "stop all {} running agents (closes their panes)? press X again to confirm",
                self.running_total()
            );
            out.push(mk(&[(&panel, "  "), (&error, &warn)]));
        }
        finish_lines(out, &panel, width, count)
    }
}

/// The SGR fg parameters for an agent's signature dot: its provider colour
/// when the CLI is installed, dim grey when not.
fn provider_dot(a: &AgentInfo) -> String {
    if a.installed {
        agent_color(&a.id)
            .unwrap_or_else(|| ui_theme().muted)
            .sgr_fg()
    } else {
        ui_theme().muted.sgr_fg()
    }
}

/// The clickable spans of the action bar's buttons, as interior column ranges
/// (the shared footer layout; pair 0 is the navigation hint, not a button).
fn bar_spans() -> Vec<(std::ops::Range<usize>, &'static str)> {
    footer_spans(BUTTONS)
        .into_iter()
        .filter(|(_, i)| *i > 0)
        .map(|(range, i)| (range, BUTTONS[i].0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, name: &str, installed: bool, conn: ConnectorStatus, run: u32) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            name: name.into(),
            command: id.into(),
            installed,
            connector: conn,
            running: run,
        }
    }

    fn view() -> AgentsView {
        AgentsView::new(vec![
            agent("claude", "Claude Code", true, ConnectorStatus::Installed, 2),
            agent("codex", "Codex", true, ConnectorStatus::NotInstalled, 0),
            agent("grok", "Grok", false, ConnectorStatus::Unsupported, 0),
        ])
    }

    #[test]
    fn launch_keys_map_to_targets() {
        let mut v = view();
        assert_eq!(
            v.handle(b"c", 120, 40),
            AgentsAction::Launch("claude".into(), LaunchTarget::CurrentPane)
        );
        assert_eq!(
            v.handle(b"p", 120, 40),
            AgentsAction::Launch("claude".into(), LaunchTarget::NewPane)
        );
        assert_eq!(
            v.handle(b"\r", 120, 40),
            AgentsAction::Launch("claude".into(), LaunchTarget::NewPane)
        );
        assert_eq!(
            v.handle(b"t", 120, 40),
            AgentsAction::Launch("claude".into(), LaunchTarget::NewWindow)
        );
        // An uninstalled agent cannot be launched.
        v.handle(b"jj", 120, 40); // select grok
        assert_eq!(v.handle(b"p", 120, 40), AgentsAction::None);
    }

    #[test]
    fn connector_toggle_respects_support() {
        let mut v = view();
        assert_eq!(
            v.handle(b"i", 120, 40),
            AgentsAction::ToggleConnector("claude".into())
        );
        v.handle(b"jj", 120, 40); // grok: unsupported
        assert_eq!(v.handle(b"i", 120, 40), AgentsAction::None);
    }

    #[test]
    fn stop_all_needs_a_second_x() {
        let mut v = view();
        assert_eq!(v.handle(b"X", 120, 40), AgentsAction::Redraw); // arm
        assert_eq!(v.handle(b"j", 120, 40), AgentsAction::Redraw); // cancels
        assert_eq!(v.handle(b"X", 120, 40), AgentsAction::Redraw);
        assert_eq!(v.handle(b"X", 120, 40), AgentsAction::StopAll);
        // With nothing running, X is inert.
        let mut idle = AgentsView::new(vec![agent(
            "codex",
            "Codex",
            true,
            ConnectorStatus::NotInstalled,
            0,
        )]);
        assert_eq!(idle.handle(b"X", 120, 40), AgentsAction::None);
    }

    #[test]
    fn refresh_keeps_selection_by_id() {
        let mut v = view();
        v.handle(b"j", 120, 40); // select codex
        v.refresh(vec![
            agent("codex", "Codex", true, ConnectorStatus::Installed, 1),
            agent("grok", "Grok", false, ConnectorStatus::Unsupported, 0),
        ]);
        assert_eq!(v.items[v.sel].id, "codex");
    }

    #[test]
    fn clicks_select_rows_and_press_buttons() {
        let mut v = view();
        let r = AgentsView::rect(120, 40);
        assert_eq!(v.click(120, 40, r.x + 3, r.y + 2), AgentsAction::Redraw);
        assert_eq!(v.sel, 1);
        let bar_y = r.y + r.h - 2;
        let (span, _) = bar_spans().into_iter().find(|(_, k)| *k == "p").unwrap();
        let bx = r.x + 1 + span.start as u16 + 1;
        assert_eq!(
            v.click(120, 40, bx, bar_y),
            AgentsAction::Launch("codex".into(), LaunchTarget::NewPane)
        );
        assert_eq!(v.click(120, 40, 1, 1), AgentsAction::Close);
    }

    #[test]
    fn render_shows_registry_states_and_bar() {
        let v = view();
        let s = String::from_utf8(v.render(120, 40)).unwrap();
        assert!(s.contains("Agents (2 installed \u{00B7} 2 running)"));
        assert!(s.contains("Claude Code (2)")); // running count badge
        assert!(s.contains("38;2;217;119;87")); // Claude's desktop brand colour
        assert!(s.contains("▄█████ ██     ▄████▄")); // ANSI Compact detail logo
        assert!(s.contains("installed"));
        assert!(s.contains("connector"));
        assert!(s.contains("stop all")); // action bar
        assert!(s.contains('\u{25CF}'));
    }

    #[test]
    fn every_box_row_paints_exactly_its_full_width() {
        // The "grey stripe" regression class: any row painting fewer cells
        // than the box width lets the pane underneath bleed through.
        let mut confirming = view();
        confirming.handle(b"X", 120, 40);
        for v in [view(), AgentsView::new(Vec::new()), confirming] {
            let r = AgentsView::rect(120, 40);
            let segs = crate::overlay::render_segments(&v.render(120, 40));
            for y in r.y..r.y + r.h {
                let seg = segs
                    .iter()
                    .find(|(row, col, _)| *row == y && *col == r.x)
                    .unwrap_or_else(|| panic!("row {y} never drawn from the box origin"));
                assert_eq!(
                    seg.2 as u16, r.w,
                    "row {y} paints {} cells, box is {} wide",
                    seg.2, r.w
                );
            }
        }
    }
}
