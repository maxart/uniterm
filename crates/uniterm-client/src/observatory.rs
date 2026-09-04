//! Interactive Workspace Observatory for agents and local web servers.

use crate::overlay::{
    finish_lines, footer_spans, footer_text, modal_hit, modal_rect, modal_visible_rows, nav_list,
    panel_style, panel_style_no_reset, render_list_modal, styled_line, ui_theme, ModalHit, Rect,
};
use crate::task::LineInput;
use crate::text_input::{decode_key, LineKey};
use uniterm_core::AgentStatus;
use uniterm_proto::{
    DetectionAuthority, DevServerEntry, FleetEntry, PaneId, WaitingAction, WaitingEntry,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservatoryAction {
    None,
    Redraw,
    Close,
    Refresh,
    Jump(PaneId),
    Stop(PaneId),
    Waiting {
        id: u64,
        action: WaitingAction,
        text: String,
    },
    OpenUrl(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    All,
    NeedsYou,
    Active,
}

impl Filter {
    fn label(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::NeedsYou => "needs you",
            Filter::Active => "active",
        }
    }

    fn next(self) -> Filter {
        match self {
            Filter::All => Filter::NeedsYou,
            Filter::NeedsYou => Filter::Active,
            Filter::Active => Filter::All,
        }
    }

    fn includes(self, entry: &FleetEntry) -> bool {
        match self {
            Filter::All => true,
            Filter::NeedsYou => entry.status.needs_human(),
            Filter::Active => matches!(
                entry.status,
                AgentStatus::Starting | AgentStatus::Working | AgentStatus::Tool
            ),
        }
    }
}

pub struct ObservatoryView {
    pub entries: Vec<FleetEntry>,
    pub servers: Vec<DevServerEntry>,
    pub waiting: Vec<WaitingEntry>,
    sel: usize,
    scroll: usize,
    filter: Filter,
    confirm_stop: bool,
    answering: Option<(u64, LineInput)>,
}

const LIST_W: u16 = 34;
const BUTTONS: &[(&str, &str)] = &[
    ("\u{2191}\u{2193}", "select"),
    ("enter", "open"),
    ("f", "filter"),
    ("p", "focus pane"),
    ("i", "answer"),
    ("d", "dismiss"),
    ("b", "rollback"),
    ("r", "refresh"),
    ("x", "stop"),
    ("esc", "close"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VisibleItem {
    Agent(usize),
    Server(usize),
}

#[derive(Clone, Copy)]
enum Selected<'a> {
    Agent(&'a FleetEntry),
    Server(&'a DevServerEntry),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionKey {
    Agent(PaneId),
    Server(PaneId, u16),
}

impl<'a> Selected<'a> {
    fn key(self) -> SelectionKey {
        match self {
            Selected::Agent(entry) => SelectionKey::Agent(entry.pane_id),
            Selected::Server(entry) => SelectionKey::Server(entry.pane_id, entry.port),
        }
    }

    fn pane_id(self) -> PaneId {
        match self {
            Selected::Agent(entry) => entry.pane_id,
            Selected::Server(entry) => entry.pane_id,
        }
    }

    fn open(self) -> ObservatoryAction {
        match self {
            Selected::Agent(entry) => ObservatoryAction::Jump(entry.pane_id),
            Selected::Server(entry) => ObservatoryAction::OpenUrl(entry.url.clone()),
        }
    }
    fn as_agent(self) -> Option<&'a FleetEntry> {
        match self {
            Selected::Agent(entry) => Some(entry),
            Selected::Server(_) => None,
        }
    }
}

impl ObservatoryView {
    pub fn new(entries: Vec<FleetEntry>) -> Self {
        ObservatoryView {
            entries,
            servers: Vec::new(),
            waiting: Vec::new(),
            sel: 0,
            scroll: 0,
            filter: Filter::All,
            confirm_stop: false,
            answering: None,
        }
    }

    pub fn refresh(&mut self, entries: Vec<FleetEntry>) {
        let keep = self.selected().map(Selected::key);
        self.entries = entries;
        self.restore_selection(keep);
        self.confirm_stop = false;
        self.clamp();
    }

    pub fn refresh_servers(&mut self, servers: Vec<DevServerEntry>) {
        let keep = self.selected().map(Selected::key);
        self.servers = servers;
        self.restore_selection(keep);
        self.confirm_stop = false;
        self.clamp();
    }

    pub fn refresh_waiting(&mut self, waiting: Vec<WaitingEntry>) {
        self.waiting = waiting;
        if self
            .answering
            .as_ref()
            .is_some_and(|(id, _)| !self.waiting.iter().any(|item| item.id == *id))
        {
            self.answering = None;
        }
    }

    fn selected_waiting(&self) -> Option<&WaitingEntry> {
        let pane = self.selected()?.pane_id();
        self.waiting.iter().find(|item| item.pane == pane)
    }

    pub fn rect(cols: u16, rows: u16) -> Rect {
        modal_rect(cols, rows)
    }

    fn visible_items(&self) -> Vec<VisibleItem> {
        let mut items: Vec<VisibleItem> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                self.filter
                    .includes(entry)
                    .then_some(VisibleItem::Agent(index))
            })
            .collect();
        items.extend((0..self.servers.len()).map(VisibleItem::Server));
        items
    }

    fn selected(&self) -> Option<Selected<'_>> {
        match *self.visible_items().get(self.sel)? {
            VisibleItem::Agent(index) => self.entries.get(index).map(Selected::Agent),
            VisibleItem::Server(index) => self.servers.get(index).map(Selected::Server),
        }
    }

    fn restore_selection(&mut self, keep: Option<SelectionKey>) {
        self.sel = keep
            .and_then(|key| {
                self.visible_items()
                    .iter()
                    .position(|item| self.item_key(*item) == Some(key))
            })
            .unwrap_or(0);
    }

    fn item_key(&self, item: VisibleItem) -> Option<SelectionKey> {
        match item {
            VisibleItem::Agent(index) => self
                .entries
                .get(index)
                .map(|entry| SelectionKey::Agent(entry.pane_id)),
            VisibleItem::Server(index) => self
                .servers
                .get(index)
                .map(|entry| SelectionKey::Server(entry.pane_id, entry.port)),
        }
    }

    fn clamp(&mut self) {
        let len = self.visible_items().len();
        self.sel = self.sel.min(len.saturating_sub(1));
        self.scroll = self.scroll.min(self.sel);
    }

    pub fn handle(&mut self, chunk: &[u8], cols: u16, rows: u16) -> ObservatoryAction {
        let visible_rows = modal_visible_rows(Self::rect(cols, rows).h);
        if let Some((id, input)) = &mut self.answering {
            let mut index = 0;
            while index < chunk.len() {
                let (key, used) = decode_key(chunk, index);
                index += used.max(1);
                match key {
                    LineKey::Escape | LineKey::Cancel => {
                        self.answering = None;
                        return ObservatoryAction::Redraw;
                    }
                    LineKey::Enter if !input.buf.trim().is_empty() => {
                        let id = *id;
                        let text = std::mem::take(&mut input.buf);
                        self.answering = None;
                        return ObservatoryAction::Waiting {
                            id,
                            action: WaitingAction::Answer,
                            text,
                        };
                    }
                    LineKey::Char(ch) if input.buf.len() + ch.len_utf8() > 4_096 => {}
                    key => {
                        input.edit(key);
                    }
                }
            }
            return ObservatoryAction::Redraw;
        }
        if self.confirm_stop {
            self.confirm_stop = false;
            return if matches!(chunk.first(), Some(b'x' | b'X' | b'\r' | b'\n')) {
                self.selected()
                    .and_then(Selected::as_agent)
                    .map(|entry| ObservatoryAction::Stop(entry.pane_id))
                    .unwrap_or(ObservatoryAction::Redraw)
            } else {
                ObservatoryAction::Redraw
            };
        }
        let mut index = 0;
        let mut redraw = false;
        while index < chunk.len() {
            let byte = chunk[index];
            if byte == 0x1b {
                if chunk.get(index + 1) == Some(&b'[') {
                    let down = match chunk.get(index + 2) {
                        Some(b'A') => Some(false),
                        Some(b'B') => Some(true),
                        _ => None,
                    };
                    if let Some(down) = down {
                        let item_count = self.visible_items().len();
                        nav_list(
                            &mut self.sel,
                            &mut self.scroll,
                            down,
                            item_count,
                            visible_rows,
                        );
                        redraw = true;
                    }
                    index += 3;
                    continue;
                }
                return ObservatoryAction::Close;
            }
            match byte {
                b'q' | 0x03 => return ObservatoryAction::Close,
                b'j' | b'k' => {
                    let item_count = self.visible_items().len();
                    nav_list(
                        &mut self.sel,
                        &mut self.scroll,
                        byte == b'j',
                        item_count,
                        visible_rows,
                    );
                    redraw = true;
                }
                b'\r' | b'\n' => {
                    return self
                        .selected()
                        .map(Selected::open)
                        .unwrap_or(ObservatoryAction::None)
                }
                b'f' => {
                    self.filter = self.filter.next();
                    self.sel = 0;
                    self.scroll = 0;
                    redraw = true;
                }
                b'a' => {
                    self.filter = Filter::All;
                    self.sel = 0;
                    self.scroll = 0;
                    redraw = true;
                }
                b'w' => {
                    self.filter = Filter::NeedsYou;
                    self.sel = 0;
                    self.scroll = 0;
                    redraw = true;
                }
                b'r' => return ObservatoryAction::Refresh,
                b'i' => {
                    if let Some(id) = self.selected_waiting().map(|item| item.id) {
                        self.answering = Some((id, LineInput::new("Answer waiting item")));
                        redraw = true;
                    }
                }
                b'd' => {
                    if let Some(item) = self.selected_waiting() {
                        return ObservatoryAction::Waiting {
                            id: item.id,
                            action: WaitingAction::Dismiss,
                            text: String::new(),
                        };
                    }
                }
                b'b' => {
                    if let Some(item) = self
                        .selected_waiting()
                        .filter(|item| item.kind == uniterm_core::WaitingKind::Relay)
                    {
                        return ObservatoryAction::Waiting {
                            id: item.id,
                            action: WaitingAction::Rollback,
                            text: String::new(),
                        };
                    }
                }
                b'p' => {
                    return self
                        .selected()
                        .map(|selected| ObservatoryAction::Jump(selected.pane_id()))
                        .unwrap_or(ObservatoryAction::None)
                }
                b'x' if self.selected().and_then(Selected::as_agent).is_some() => {
                    self.confirm_stop = true;
                    redraw = true;
                }
                _ => {}
            }
            index += 1;
        }
        if redraw {
            ObservatoryAction::Redraw
        } else {
            ObservatoryAction::None
        }
    }

    pub fn click(&mut self, cols: u16, rows: u16, x: u16, y: u16) -> ObservatoryAction {
        let rect = Self::rect(cols, rows);
        if modal_visible_rows(rect.h) > 8
            && y == rect.y + 9
            && x > rect.x + LIST_W + 1
            && x < rect.x + rect.w - 1
        {
            if let Some(Selected::Server(server)) = self.selected() {
                return ObservatoryAction::OpenUrl(server.url.clone());
            }
        }
        match modal_hit(rect, LIST_W, x, y) {
            ModalHit::Outside => ObservatoryAction::Close,
            ModalHit::ListRow(slot) => {
                let item = self.scroll + slot;
                if item >= self.visible_items().len() {
                    return ObservatoryAction::None;
                }
                if item == self.sel {
                    self.selected()
                        .map(Selected::open)
                        .unwrap_or(ObservatoryAction::None)
                } else {
                    self.sel = item;
                    ObservatoryAction::Redraw
                }
            }
            ModalHit::Bar(relative) => {
                for (span, key) in footer_spans(BUTTONS) {
                    if span.contains(&relative) {
                        return match BUTTONS[key].0 {
                            "enter" => self.handle(b"\r", cols, rows),
                            "f" => self.handle(b"f", cols, rows),
                            "p" => self.handle(b"p", cols, rows),
                            "i" => self.handle(b"i", cols, rows),
                            "d" => self.handle(b"d", cols, rows),
                            "b" => self.handle(b"b", cols, rows),
                            "r" => ObservatoryAction::Refresh,
                            "x" => self.handle(b"x", cols, rows),
                            _ => ObservatoryAction::Close,
                        };
                    }
                }
                ObservatoryAction::None
            }
            ModalHit::None => ObservatoryAction::None,
        }
    }

    pub fn render(&self, cols: u16, rows: u16) -> Vec<u8> {
        let rect = Self::rect(cols, rows);
        let panel = panel_style();
        let theme = ui_theme();
        let selected = format!(
            "\x1b[{};{}m",
            theme.status_active_bg.sgr_bg(),
            theme.status_active_fg.sgr_fg()
        );
        let inner = rect.w.saturating_sub(2) as usize;
        let visible = modal_visible_rows(rect.h);
        let items = self.visible_items();
        let attention = self.waiting.len();
        let active = self
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.status,
                    AgentStatus::Starting | AgentStatus::Working | AgentStatus::Tool
                )
            })
            .count();
        let full_title = format!(
            " Observatory  {} agents  \u{00B7}  {} servers  \u{00B7}  {} need you  \u{00B7}  {} active ",
            self.entries.len(),
            self.servers.len(),
            attention,
            active
        );
        let title = if full_title.chars().count() < inner {
            full_title
        } else {
            format!(
                " Observatory  {} agents  \u{00B7}  {} servers ",
                self.entries.len(),
                self.servers.len()
            )
        };
        let detail = self.detail_lines(inner.saturating_sub(LIST_W as usize + 1), visible);
        render_list_modal(
            cols,
            rows,
            &title,
            LIST_W as usize,
            |slot| {
                let visible_index = self.scroll + slot;
                let item = *items.get(visible_index)?;
                let (color, signature, status) = match item {
                    VisibleItem::Agent(index) => {
                        let entry = self.entries.get(index)?;
                        let agent = uniterm_core::agent::agent_name(&entry.agent);
                        (
                            uniterm_core::agent::agent_color_or_default(&entry.agent).sgr_fg(),
                            format!(" \u{25CF} {}", fixed_width(agent, 12)),
                            format!(" {}", fixed_width(entry.status.label(), 12)),
                        )
                    }
                    VisibleItem::Server(index) => {
                        let entry = self.servers.get(index)?;
                        (
                            theme.success.sgr_fg(),
                            format!(" \u{25C6} {}", fixed_width(&entry.label, 12)),
                            format!(" {}", fixed_width(&format!(":{}", entry.port), 12)),
                        )
                    }
                };
                let fill = " "
                    .repeat(LIST_W as usize - signature.chars().count() - status.chars().count());
                Some(if visible_index == self.sel {
                    format!(
                        "{selected}\x1b[{color}m{signature}\x1b[{}m{status}{fill}{panel}",
                        theme.status_active_fg.sgr_fg()
                    )
                } else {
                    format!(
                        "\x1b[{color}m{signature}\x1b[{}m{status}{panel}{fill}",
                        theme.muted.sgr_fg()
                    )
                })
            },
            &detail,
            &footer_text(
                if self.confirm_stop {
                    &[("x/enter", "confirm stop"), ("any key", "cancel")]
                } else if self.answering.is_some() {
                    &[("enter", "send answer"), ("esc", "cancel")]
                } else {
                    BUTTONS
                },
                inner,
            ),
        )
    }

    fn detail_lines(&self, width: usize, count: usize) -> Vec<String> {
        let panel = panel_style_no_reset();
        let Some(selected) = self.selected() else {
            let dim = format!("\x1b[{}m", ui_theme().muted.sgr_fg());
            let mut lines = vec![styled_line(&[])];
            lines.push(styled_line(&[
                (&panel, "  "),
                (&dim, "No agents or web servers are visible."),
            ]));
            lines.push(styled_line(&[
                (&panel, "  "),
                (&dim, "Press f to cycle filters or a for all."),
            ]));
            return finish_lines(lines, &panel, width, count);
        };
        let lines = match selected {
            Selected::Agent(entry) => self.agent_detail_lines(entry, width),
            Selected::Server(entry) => self.server_detail_lines(entry, width),
        };
        finish_lines(lines, &panel, width, count)
    }

    fn agent_detail_lines(&self, entry: &FleetEntry, width: usize) -> Vec<(String, usize)> {
        let panel = panel_style_no_reset();
        let theme = ui_theme();
        let dim = format!("\x1b[{}m", theme.muted.sgr_fg());
        let strong = format!("\x1b[1;{}m", theme.foreground.sgr_fg());
        let warning = format!("\x1b[1;{}m", theme.warning.sgr_fg());
        let mut lines = vec![styled_line(&[])];
        let agent_style = format!(
            "\x1b[1;{}m",
            uniterm_core::agent::agent_color_or_default(&entry.agent).sgr_fg()
        );
        let status_style = format!("\x1b[1;{}m", status_color(entry.status).sgr_fg());
        lines.push(styled_line(&[
            (&panel, "  "),
            (&agent_style, uniterm_core::agent::agent_name(&entry.agent)),
        ]));
        lines.push(styled_line(&[
            (&panel, "  "),
            (&agent_style, "\u{25CF} "),
            (&status_style, entry.status.label()),
            (
                &dim,
                if entry.status.needs_human() {
                    "  - action required"
                } else {
                    ""
                },
            ),
        ]));
        lines.push(styled_line(&[]));
        lines.push(styled_line(&[
            (&panel, "  "),
            (&dim, "Project"),
            (&panel, "  "),
            (&strong, &entry.project_name),
        ]));
        let location = format!("{}  \u{00B7}  Pane {}", entry.tab_name, entry.pane);
        lines.push(styled_line(&[
            (&panel, "  "),
            (&dim, "Location"),
            (&panel, " "),
            (&panel, &location),
        ]));
        if let (Some(run), Some(role), Some(role_name)) =
            (entry.run, entry.role, entry.role_name.as_deref())
        {
            let relationship = format!("Run {}  \u{00B7}  Role {} ({role_name})", run.0, role.0);
            lines.push(styled_line(&[
                (&panel, "  "),
                (&dim, "Ownership"),
                (&panel, " "),
                (&strong, &relationship),
            ]));
        }
        lines.push(styled_line(&[]));
        lines.push(styled_line(&[(&panel, "  "), (&dim, "Detection signal")]));
        let authority = authority_label(entry.authority);
        lines.push(styled_line(&[(&panel, "  "), (&warning, authority)]));
        for line in wrap(&entry.evidence, width.saturating_sub(4))
            .into_iter()
            .take(5)
        {
            lines.push(styled_line(&[(&panel, "  "), (&panel, &line)]));
        }
        if let Some(waiting) = self.waiting.iter().find(|item| item.pane == entry.pane_id) {
            lines.push(styled_line(&[]));
            lines.push(styled_line(&[
                (&panel, "  "),
                (
                    &warning,
                    &format!("Waiting #{} - {}", waiting.id, waiting.kind.label()),
                ),
            ]));
            for line in wrap(&waiting.summary, width.saturating_sub(4))
                .into_iter()
                .take(4)
            {
                lines.push(styled_line(&[(&panel, "  "), (&panel, &line)]));
            }
        }
        if let Some((_, input)) = &self.answering {
            lines.push(styled_line(&[]));
            lines.push(styled_line(&[
                (&panel, "  "),
                (&warning, "Answer: "),
                (&panel, &input.buf),
            ]));
        }
        lines.push(styled_line(&[]));
        let filter = format!("Showing: {}", self.filter.label());
        lines.push(styled_line(&[(&panel, "  "), (&dim, &filter)]));
        if self.confirm_stop {
            lines.push(styled_line(&[
                (&panel, "  "),
                (&warning, "Stop this agent and close its Pane?"),
            ]));
        }
        lines
    }

    fn server_detail_lines(&self, entry: &DevServerEntry, width: usize) -> Vec<(String, usize)> {
        let panel = panel_style_no_reset();
        let theme = ui_theme();
        let dim = format!("\x1b[{}m", theme.muted.sgr_fg());
        let strong = format!("\x1b[1;{}m", theme.foreground.sgr_fg());
        let live = format!("\x1b[1;{}m", theme.success.sgr_fg());
        let accent = format!("\x1b[1;4;{}m", theme.accent.sgr_fg());
        let mut lines = vec![styled_line(&[])];
        lines.push(styled_line(&[(&panel, "  "), (&strong, &entry.label)]));
        lines.push(styled_line(&[
            (&panel, "  "),
            (&live, "\u{25C6} Running"),
            (&dim, &format!("  - loopback port {}", entry.port)),
        ]));
        lines.push(styled_line(&[]));
        lines.push(styled_line(&[
            (&panel, "  "),
            (&dim, "Project"),
            (&panel, "  "),
            (&strong, &entry.project_name),
        ]));
        let location = format!("{}  \u{00B7}  Pane {}", entry.tab_name, entry.pane);
        lines.push(styled_line(&[
            (&panel, "  "),
            (&dim, "Location"),
            (&panel, " "),
            (&panel, &location),
        ]));
        lines.push(styled_line(&[]));
        lines.push(styled_line(&[(&panel, "  "), (&dim, "URL")]));
        let url = fixed_width(&entry.url, width.saturating_sub(4));
        lines.push(styled_line(&[(&panel, "  "), (&accent, &url)]));
        lines.push(styled_line(&[]));
        lines.push(styled_line(&[(&panel, "  "), (&dim, "Project root")]));
        for line in wrap(&entry.project_root, width.saturating_sub(4))
            .into_iter()
            .take(3)
        {
            lines.push(styled_line(&[(&panel, "  "), (&panel, &line)]));
        }
        lines.push(styled_line(&[]));
        for line in wrap(
            "Enter or click the URL to open it. Press p to focus its Pane.",
            width.saturating_sub(4),
        ) {
            lines.push(styled_line(&[(&panel, "  "), (&dim, &line)]));
        }
        lines
    }
}

fn fixed_width(value: &str, width: usize) -> String {
    let mut result: String = value.chars().take(width).collect();
    result.extend(std::iter::repeat_n(
        ' ',
        width.saturating_sub(result.chars().count()),
    ));
    result
}

fn status_color(status: AgentStatus) -> uniterm_core::Color {
    let theme = ui_theme();
    match status {
        AgentStatus::Permission | AgentStatus::Question => theme.warning,
        AgentStatus::Working | AgentStatus::Tool | AgentStatus::Starting => theme.accent,
        AgentStatus::Idle => theme.success,
        AgentStatus::Error | AgentStatus::Exited => theme.error,
        AgentStatus::Unknown => theme.muted,
    }
}

fn authority_label(authority: DetectionAuthority) -> &'static str {
    match authority {
        DetectionAuthority::Osc777 => "OSC 777 connector - cooperative",
        DetectionAuthority::Log => "Agent log - native",
        DetectionAuthority::Grid => "Terminal output - heuristic",
        DetectionAuthority::Process => "Foreground process - identity",
        DetectionAuthority::KernelExit => "Kernel process exit - definitive",
    }
}

fn wrap(value: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in value.split_whitespace() {
        if !current.is_empty() && current.chars().count() + word.chars().count() + 1 > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.extend(word.chars().take(width));
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pane: u64, status: AgentStatus) -> FleetEntry {
        FleetEntry {
            agent: "claude".into(),
            status,
            pane_id: PaneId(pane),
            project: uniterm_core::ProjectId(1),
            project_name: "Uniterm".into(),
            tab: 1,
            tab_name: "Tests".into(),
            window: 1,
            pane: pane as u32,
            authority: DetectionAuthority::Grid,
            evidence: "permission prompt visible".into(),
            run: None,
            role: None,
            role_name: None,
        }
    }

    fn server(pane: u64, port: u16) -> DevServerEntry {
        DevServerEntry {
            label: "vite".into(),
            url: format!("http://localhost:{port}"),
            port,
            pane_id: PaneId(pane),
            project: uniterm_core::ProjectId(1),
            project_name: "Uniterm".into(),
            project_root: "/work/uniterm".into(),
            tab: 1,
            tab_name: "Web".into(),
            pane: 2,
        }
    }

    fn waiting(pane: u64, kind: uniterm_core::WaitingKind) -> WaitingEntry {
        WaitingEntry {
            id: 41,
            pane: PaneId(pane),
            kind,
            summary: "the agent needs a decision".into(),
            agent: Some("claude".into()),
            project: uniterm_core::ProjectId(1),
            project_name: "Uniterm".into(),
            tab: 1,
        }
    }

    #[test]
    fn filters_and_jumps_by_stable_pane_id() {
        let mut view = ObservatoryView::new(vec![
            entry(1, AgentStatus::Working),
            entry(2, AgentStatus::Permission),
        ]);
        assert_eq!(view.handle(b"w", 120, 40), ObservatoryAction::Redraw);
        assert_eq!(
            view.handle(b"\r", 120, 40),
            ObservatoryAction::Jump(PaneId(2))
        );
    }

    #[test]
    fn agent_identity_uses_the_sidebar_provider_colors() {
        let claude = entry(1, AgentStatus::Working);
        let mut codex = entry(2, AgentStatus::Working);
        codex.agent = "codex".into();
        let rendered =
            String::from_utf8(ObservatoryView::new(vec![claude, codex]).render(120, 40)).unwrap();

        for id in ["claude", "codex"] {
            let color = uniterm_core::agent::agent_color_or_default(id).sgr_fg();
            let name = uniterm_core::agent::agent_name(id);
            assert!(rendered.contains(&format!("\x1b[{color}m \u{25CF} {name}")));
        }
    }

    #[test]
    fn web_server_rows_open_urls_and_can_focus_their_panes() {
        let mut view = ObservatoryView::new(Vec::new());
        view.refresh_servers(vec![server(7, 5173)]);
        assert_eq!(
            view.handle(b"\r", 120, 40),
            ObservatoryAction::OpenUrl("http://localhost:5173".into())
        );
        assert_eq!(
            view.handle(b"p", 120, 40),
            ObservatoryAction::Jump(PaneId(7))
        );
        let rect = ObservatoryView::rect(120, 40);
        assert_eq!(
            view.click(120, 40, rect.x + LIST_W + 3, rect.y + 9),
            ObservatoryAction::OpenUrl("http://localhost:5173".into())
        );
        let rendered = String::from_utf8(view.render(120, 40)).unwrap();
        assert!(rendered.contains("1 servers"));
        assert!(rendered.contains("http://localhost:5173"));
    }

    #[test]
    fn live_server_refresh_preserves_a_selected_server() {
        let mut view = ObservatoryView::new(vec![entry(1, AgentStatus::Working)]);
        view.refresh_servers(vec![server(7, 5173), server(8, 3000)]);
        assert_eq!(view.handle(b"j", 120, 40), ObservatoryAction::Redraw);
        view.refresh(vec![entry(1, AgentStatus::Idle)]);
        assert_eq!(
            view.handle(b"\r", 120, 40),
            ObservatoryAction::OpenUrl("http://localhost:5173".into())
        );
    }

    #[test]
    fn waiting_answer_uses_the_semantic_action_and_preserves_utf8() {
        let mut view = ObservatoryView::new(vec![entry(7, AgentStatus::Question)]);
        view.refresh_waiting(vec![waiting(7, uniterm_core::WaitingKind::Question)]);
        assert_eq!(view.handle(b"i", 120, 40), ObservatoryAction::Redraw);
        assert_eq!(
            view.handle("évidence\r".as_bytes(), 120, 40),
            ObservatoryAction::Waiting {
                id: 41,
                action: WaitingAction::Answer,
                text: "évidence".into(),
            }
        );
    }

    #[test]
    fn rollback_is_only_offered_for_a_relay_wait() {
        let mut view = ObservatoryView::new(vec![entry(7, AgentStatus::Question)]);
        view.refresh_waiting(vec![waiting(7, uniterm_core::WaitingKind::Question)]);
        assert_eq!(view.handle(b"b", 120, 40), ObservatoryAction::None);
        view.refresh_waiting(vec![waiting(7, uniterm_core::WaitingKind::Relay)]);
        assert_eq!(
            view.handle(b"b", 120, 40),
            ObservatoryAction::Waiting {
                id: 41,
                action: WaitingAction::Rollback,
                text: String::new(),
            }
        );
    }
}
