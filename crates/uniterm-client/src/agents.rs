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
    finish_lines, footer_spans, footer_text, footer_width, modal_hit, modal_rect,
    modal_visible_rows, nav_list, panel_style, panel_style_no_reset, render_tabbed_list_modal,
    styled_line, ui_theme, ModalHit, Rect,
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
    /// Install all available integrations without toggling installed ones off.
    InstallAllConnectors,
    /// Load the global file tab on demand.
    LoadFiles,
    /// Open the chosen file using the server's configured editor.
    EditFile(String),
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
    files_tab: bool,
    files: Vec<uniterm_proto::AgentConfigFile>,
    file_sel: usize,
    preview_scroll: usize,
    file_error: Option<String>,
    connector_error: Option<String>,
}

/// The action bar's pairs (shared overlay styling); index 0 is the
/// non-clickable navigation hint.
const BUTTONS: &[(&str, &str)] = &[
    ("↑↓", ""),
    ("I", "install all"),
    ("Tab", "files"),
    ("i", "hook"),
    ("c", "here"),
    ("p", "pane"),
    ("t", "tab"),
    ("s", "stack"),
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
            files_tab: false,
            files: Vec::new(),
            file_sel: 0,
            preview_scroll: 0,
            file_error: None,
            connector_error: None,
        }
    }

    fn switch_files(&mut self, files: bool) -> AgentsAction {
        if self.files_tab == files {
            return AgentsAction::None;
        }
        self.mode = Mode::Browse;
        self.files_tab = files;
        if files {
            AgentsAction::LoadFiles
        } else {
            AgentsAction::Redraw
        }
    }

    fn visible_rows(r: Rect) -> usize {
        modal_visible_rows(r.h).saturating_sub(2)
    }

    // Equal-width, full-height targets mirror the main content tab bar.
    fn tab_rects(r: Rect) -> [Rect; 2] {
        let width = r.w.saturating_sub(2);
        let left = width.div_ceil(2);
        [
            Rect {
                x: r.x + 1,
                y: r.y + 1,
                w: left,
                h: 1,
            },
            Rect {
                x: r.x + 1 + left,
                y: r.y + 1,
                w: width - left,
                h: 1,
            },
        ]
    }

    fn tab_strip(&self, r: Rect) -> String {
        use unicode_width::UnicodeWidthStr;
        let theme = ui_theme();
        let mut strip = String::new();
        for (index, (rect, label)) in Self::tab_rects(r)
            .into_iter()
            .zip(["Providers", "Global files"])
            .enumerate()
        {
            let active = self.files_tab == (index == 1);
            let style = if active {
                format!(
                    "\x1b[0;1;{};{}m",
                    theme.status_active_bg.sgr_bg(),
                    theme.status_active_fg.sgr_fg()
                )
            } else {
                format!(
                    "\x1b[0;{};{}m",
                    theme.status_bg.sgr_bg(),
                    theme.status_fg.sgr_fg()
                )
            };
            let width = usize::from(rect.w);
            let label = safe_fit(label, width);
            let label = label.trim_end();
            let left = width.saturating_sub(label.width()) / 2;
            strip.push_str(&style);
            strip.push_str(&safe_fit(&format!("{}{label}", " ".repeat(left)), width));
        }
        strip
    }

    /// Refresh only a requested file view; late replies never reopen a modal.
    pub fn refresh_files(&mut self, files: Vec<uniterm_proto::AgentConfigFile>) {
        self.files = files;
        self.file_sel = self.file_sel.min(self.files.len().saturating_sub(1));
    }

    /// Keep an editor failure visible without discarding the selected file.
    pub fn file_error(&mut self, error: String) {
        self.file_error = Some(error);
    }

    fn handle_files(&mut self, input: &[u8]) -> AgentsAction {
        match input {
            b"q" | b"\x1b" | b"\x03" => AgentsAction::Close,
            b"r" => AgentsAction::LoadFiles,
            b"e" | b"\r" | b"\n" => self
                .files
                .get(self.file_sel)
                .map(|file| AgentsAction::EditFile(file.path.clone()))
                .unwrap_or(AgentsAction::None),
            b"j" | b"\x1b[B" => {
                self.file_sel = (self.file_sel + 1).min(self.files.len().saturating_sub(1));
                self.preview_scroll = 0;
                AgentsAction::Redraw
            }
            b"k" | b"\x1b[A" => {
                self.file_sel = self.file_sel.saturating_sub(1);
                self.preview_scroll = 0;
                AgentsAction::Redraw
            }
            b"\x1b[6~" => {
                self.preview_scroll = self.preview_scroll.saturating_add(10);
                AgentsAction::Redraw
            }
            b"\x1b[5~" => {
                self.preview_scroll = self.preview_scroll.saturating_sub(10);
                AgentsAction::Redraw
            }
            _ => AgentsAction::None,
        }
    }

    fn render_files(&self, cols: u16, rows: u16) -> Vec<u8> {
        let r = Self::rect(cols, rows);
        let visible = Self::visible_rows(r);
        let width = usize::from(r.w.saturating_sub(LIST_W + 3));
        let mut detail = Vec::new();
        if let Some(file) = self.files.get(self.file_sel) {
            detail.push(safe_fit(&file.path, width));
            let status = if file.exists {
                "e: edit existing file"
            } else {
                "e: create file and edit"
            };
            detail.push(safe_fit(status, width));
            detail.push(safe_fit(
                self.file_error
                    .as_deref()
                    .or(file.error.as_deref())
                    .unwrap_or(if file.truncated {
                        "Preview truncated; open editor for full file"
                    } else {
                        "PgUp/PgDn: scroll preview"
                    }),
                width,
            ));
            detail.extend(
                file.preview
                    .lines()
                    .skip(self.preview_scroll)
                    .take(visible.saturating_sub(3))
                    .map(|line| safe_fit(line, width)),
            );
        } else {
            detail.push(safe_fit("Loading global files...", width));
        }
        detail.resize_with(visible, || " ".repeat(width));
        let theme = ui_theme();
        render_tabbed_list_modal(
            (cols, rows),
            &safe_fit(" Agents ", usize::from(r.w.saturating_sub(3))),
            &self.tab_strip(r),
            LIST_W as usize,
            |slot| {
                let first = self.file_sel.saturating_sub(visible.saturating_sub(1));
                self.files.get(first + slot).map(|file| {
                    let text = safe_fit(&format!(" {}", file.label), LIST_W as usize);
                    if first + slot == self.file_sel {
                        format!(
                            "\x1b[{};{}m{text}{}",
                            theme.status_active_bg.sgr_bg(),
                            theme.status_active_fg.sgr_fg(),
                            panel_style()
                        )
                    } else {
                        text
                    }
                })
            },
            &detail,
            &bounded_footer(FILE_BUTTONS, usize::from(r.w.saturating_sub(2))),
        )
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

    /// Surface connector failures without claiming installation succeeded.
    pub fn connector_error(&mut self, error: Option<String>) {
        self.connector_error = error;
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
        if matches!(chunk, b"\t" | b"\x1b[Z") {
            return self.switch_files(!self.files_tab);
        }
        if self.files_tab {
            return self.handle_files(chunk);
        }
        let visible = Self::visible_rows(Self::rect(cols, rows));
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
                b's' => return self.launch(LaunchTarget::NewStack),
                b'I' => return AgentsAction::InstallAllConnectors,
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
        for (index, tab) in Self::tab_rects(r).into_iter().enumerate() {
            if cy == tab.y && cx >= tab.x && cx < tab.x + tab.w {
                return self.switch_files(index == 1);
            }
        }
        if (cy == r.y || cy == r.y + 2) && cx >= r.x && cx < r.x + r.w {
            return AgentsAction::None;
        }
        let body = Rect {
            y: r.y + 2,
            h: r.h.saturating_sub(2),
            ..r
        };
        if self.files_tab {
            let first = self
                .file_sel
                .saturating_sub(Self::visible_rows(r).saturating_sub(1));
            return match modal_hit(body, LIST_W, cx, cy) {
                ModalHit::Outside => AgentsAction::Close,
                ModalHit::ListRow(slot) if first + slot < self.files.len() => {
                    self.file_sel = first + slot;
                    self.preview_scroll = 0;
                    AgentsAction::Redraw
                }
                ModalHit::Bar(rel) => {
                    match footer_spans(FILE_BUTTONS)
                        .into_iter()
                        .find(|(span, _)| {
                            span.end <= usize::from(r.w.saturating_sub(2)) && span.contains(&rel)
                        })
                        .map(|(_, i)| i)
                    {
                        Some(1) => self.handle_files(b"e"),
                        Some(2) => self.handle_files(b"r"),
                        Some(3) => self.switch_files(false),
                        Some(4) => AgentsAction::Close,
                        _ => AgentsAction::None,
                    }
                }
                _ => AgentsAction::None,
            };
        }
        match modal_hit(body, LIST_W, cx, cy) {
            ModalHit::Outside => AgentsAction::Close,
            ModalHit::Bar(rel) => {
                for (span, key) in bar_spans() {
                    if span.end <= usize::from(r.w.saturating_sub(2)) && span.contains(&rel) {
                        return match key {
                            "c" => self.handle(b"c", cols, rows),
                            "p" => self.handle(b"p", cols, rows),
                            "t" => self.handle(b"t", cols, rows),
                            "s" => self.handle(b"s", cols, rows),
                            "i" => self.handle(b"i", cols, rows),
                            "I" => self.handle(b"I", cols, rows),
                            "X" => self.handle(b"X", cols, rows),
                            "Tab" => self.switch_files(true),
                            _ => AgentsAction::Close,
                        };
                    }
                }
                AgentsAction::None
            }
            ModalHit::ListRow(slot) => {
                let row = slot + self.scroll;
                if row < self.items.len() && row < self.scroll + Self::visible_rows(r) {
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
        if self.files_tab {
            return self.render_files(cols, rows);
        }
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
        let visible = Self::visible_rows(r);
        let mut detail = self.detail_lines(inner.saturating_sub(list_w + 1), visible);
        if let Some(error) = &self.connector_error {
            detail.insert(
                0,
                safe_fit(
                    &format!("Connector error: {error}"),
                    inner.saturating_sub(list_w + 1),
                ),
            );
            detail.truncate(visible);
        }
        let installed = self.items.iter().filter(|a| a.installed).count();
        render_tabbed_list_modal(
            (cols, rows),
            &safe_fit(
                &format!(
                    " Agents ({installed} installed \u{00B7} {} running) ",
                    self.running_total()
                ),
                inner.saturating_sub(1),
            ),
            &self.tab_strip(r),
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
        bounded_footer(pairs, width)
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
            // Upgrading rewrites the user's agent config, so it waits for an
            // explicit keypress (or `ut agent connector install AGENT`).
            ConnectorStatus::Outdated => out.push(mk(&[
                (&panel, "  "),
                (dim, "connector   "),
                (off, "\u{25D0} outdated"),
                (dim, " - press i to upgrade the notify hook"),
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

const FILE_BUTTONS: &[(&str, &str)] = &[
    ("↑↓", "select"),
    ("e", "create/edit"),
    ("r", "refresh"),
    ("Tab", "providers"),
    ("esc", "close"),
];

// File contents and paths are untrusted terminal text. Fit by display cells.
fn safe_fit(text: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    let mut result = String::new();
    let mut used = 0;
    for grapheme in clean.graphemes(true) {
        let cells = grapheme.width();
        if used + cells > width {
            break;
        }
        result.push_str(grapheme);
        used += cells;
    }
    result.extend(std::iter::repeat_n(' ', width - used));
    result
}

fn bounded_footer(pairs: &[(&str, &str)], width: usize) -> String {
    let mut count = pairs.len();
    while count > 0 && footer_width(&pairs[..count]) > width {
        count -= 1;
    }
    footer_text(&pairs[..count], width)
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
    fn global_files_are_lazy_and_preserve_untrusted_preview_text() {
        let mut view = view();
        assert_eq!(
            view.handle(b"I", 120, 40),
            AgentsAction::InstallAllConnectors
        );
        assert_eq!(view.handle(b"\t", 120, 40), AgentsAction::LoadFiles);
        view.refresh_files(vec![uniterm_proto::AgentConfigFile {
            label: "Global instructions".into(),
            path: "/tmp/AGENTS.md".into(),
            exists: false,
            preview: "wide 界\x1b]52;bad\x07".into(),
            truncated: false,
            error: None,
        }]);
        assert_eq!(
            view.handle(b"e", 120, 40),
            AgentsAction::EditFile("/tmp/AGENTS.md".into())
        );
        let output = String::from_utf8(view.render(120, 40)).unwrap();
        assert!(!output.contains("\x1b]52;bad"));
        assert!(output.contains("wide 界"));
        assert_eq!(view.handle(b"\t", 120, 40), AgentsAction::Redraw);
        assert_eq!(
            view.handle(b"s", 120, 40),
            AgentsAction::Launch("claude".into(), LaunchTarget::NewStack)
        );
    }

    #[test]
    fn tabs_share_main_tab_colors_centered_labels_and_click_rectangles() {
        for (cols, rows) in [(60, 12), (80, 24), (121, 40)] {
            let mut view = view();
            let r = AgentsView::rect(cols, rows);
            let [providers, files] = AgentsView::tab_rects(r);
            assert_eq!(providers.x + providers.w, files.x);
            assert_eq!(files.x + files.w, r.x + r.w - 1);
            assert!(providers.w.abs_diff(files.w) <= 1);
            let theme = ui_theme();
            let active = format!(
                "\x1b[0;1;{};{}m",
                theme.status_active_bg.sgr_bg(),
                theme.status_active_fg.sgr_fg()
            );
            let inactive = format!(
                "\x1b[0;{};{}m",
                theme.status_bg.sgr_bg(),
                theme.status_fg.sgr_fg()
            );
            let centered = |label: &str, width: u16| {
                let left = (usize::from(width) - label.len()) / 2;
                safe_fit(&format!("{}{label}", " ".repeat(left)), width.into())
            };
            let providers_label = centered("Providers", providers.w);
            let files_label = centered("Global files", files.w);
            assert_eq!(
                view.tab_strip(r),
                format!("{active}{providers_label}{inactive}{files_label}")
            );
            // Clicking an active tab is a no-op, including its full padded edge.
            assert_eq!(
                view.click(cols, rows, providers.x, providers.y),
                AgentsAction::None
            );
            assert_eq!(
                view.click(cols, rows, files.x + files.w - 1, files.y),
                AgentsAction::LoadFiles
            );
            assert_eq!(
                view.tab_strip(r),
                format!("{inactive}{providers_label}{active}{files_label}")
            );
            assert_eq!(view.click(cols, rows, files.x, files.y), AgentsAction::None);
            let output = view.render(cols, rows);
            let rendered = String::from_utf8_lossy(&output);
            let separator = format!(
                "\x1b[{};{}H{}├{}┬{}┤",
                r.y + 2,
                r.x,
                panel_style(),
                "─".repeat(usize::from(LIST_W)),
                "─".repeat(usize::from(r.w.saturating_sub(LIST_W + 3))),
            );
            assert!(rendered.contains(&separator));
            assert!(rendered.contains(" Agents─"));
            assert_eq!(view.click(cols, rows, r.x + 3, r.y + 2), AgentsAction::None);
            let tab_row = crate::overlay::render_segments(&output)
                .into_iter()
                .find(|(row, col, _)| *row == r.y + 1 && *col == r.x)
                .unwrap();
            assert_eq!(tab_row.2, usize::from(r.w));
            assert_eq!(view.handle(b"\x1b[Z", cols, rows), AgentsAction::Redraw);
            assert!(!view.files_tab);
            assert_eq!(
                view.click(cols, rows, r.x + 3, r.y + 3),
                AgentsAction::Redraw
            );
            assert_eq!(view.sel, 0);
        }
    }

    #[test]
    fn global_file_selection_and_geometry_fit_a_small_terminal() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(safe_fit("界 preview", 19).width(), 19);
        let mut view = view();
        view.handle(b"\t", 60, 12);
        view.refresh_files(
            (0..8)
                .map(|n| uniterm_proto::AgentConfigFile {
                    label: format!("File {n}"),
                    path: format!("/tmp/{n}"),
                    exists: true,
                    preview: "preview".into(),
                    truncated: false,
                    error: None,
                })
                .collect(),
        );
        for _ in 0..7 {
            view.handle(b"j", 60, 12);
        }
        let rendered = view.render(60, 12);
        assert!(String::from_utf8_lossy(&rendered).contains("File 7"));
        let rect = AgentsView::rect(60, 12);
        for (row, col, cells) in crate::overlay::render_segments(&rendered) {
            if col == rect.x && row >= rect.y && row < rect.y + rect.h {
                assert_eq!(cells, rect.w as usize);
            }
        }
        assert_eq!(
            view.handle(b"e", 60, 12),
            AgentsAction::EditFile("/tmp/7".into())
        );
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
        assert_eq!(v.click(120, 40, r.x + 3, r.y + 4), AgentsAction::Redraw);
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
        assert!(s.contains("running)──"));
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
