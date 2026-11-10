//! The task manager (AG7's management surface, v2): a modal with a
//! colour-coded task list on the left, a detail pane on the right, and an
//! action bar - edit (inline), cycle status, delete (two-step confirm).
//!
//! Pure state + rendering + hit-testing, like the other client surfaces: the
//! attach loop feeds keys/mouse in and sends the returned ops to the server;
//! the server answers every mutation with a fresh snapshot, so the modal is
//! always a projection of server truth.

use crate::overlay::{clip_text, plain_text};
use crate::overlay::{
    finish_lines, footer_spans, footer_text, modal_hit, modal_rect, modal_visible_rows, nav_list,
    panel_style, panel_style_no_reset, render_list_modal, styled_line, ui_theme, ModalHit, Rect,
};
use crate::text_input::{decode_key, edit_line, line_with_cursor, LineKey};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;
use uniterm_core::TaskStatus;
use uniterm_proto::TaskEntry;

/// What a key/click asks the attach loop to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskAction {
    None,
    /// State changed; repaint the modal.
    Redraw,
    /// Close the modal (and Refresh the screen under it).
    Close,
    /// Send: set this task's status.
    SetStatus(u64, TaskStatus),
    /// Send: rename this task.
    Retitle(u64, String),
    /// Send: delete this task.
    Delete(u64),
    Notes(u64, String),
    Compose(u64),
    Focus(uniterm_core::PaneId),
    Answer(u64, String),
    Resume(u64),
    WaitingFocus(u64),
}

/// The modal's input mode.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    Browse,
    /// Inline title editing in the detail pane.
    Edit {
        id: u64,
        notes: bool,
        answer: bool,
        buf: String,
        cursor: usize,
    },
    /// `x` pressed once; a second `x` deletes, anything else cancels.
    ConfirmDelete(u64),
    Search,
}

/// The open task manager.
pub struct TaskView {
    pub items: Vec<TaskEntry>,
    pub sel: usize,
    scroll: usize,
    mode: Mode,
    all_items: Vec<TaskEntry>,
    filter: usize,
    project_only: bool,
    recent: bool,
    query: String,
    visible: usize,
    detail_scroll: usize,
    pub notice: Option<String>,
    pub compose_pending: Option<u64>,
    remote: bool,
    generation: u64,
    page_revision: u64,
    page_loading: bool,
    total: usize,
    offset: usize,
    page_offset: usize,
    query_pending: bool,
    detail: Option<TaskEntry>,
    detail_loading: Option<u64>,
    detail_bytes: Vec<u8>,
    detail_revision: u64,
    detail_needs_refresh: bool,
}

/// The action bar's pairs (shared overlay styling); index 0 is the
/// non-clickable navigation hint.
const BUTTONS: &[(&str, &str)] = &[
    ("\u{2191}\u{2193}", "select"),
    ("enter", "go"),
    ("l", "launch"),
    ("e", "title"),
    ("/", "search"),
    ("f", "filter"),
    ("p", "scope"),
    ("r", "sort"),
    ("spc", "status"),
    ("x", "delete"),
    ("esc", "close"),
];

/// Width of the list column (incl. its padding), fixed for a stable layout.
const LIST_W: u16 = 28;

impl TaskView {
    pub fn new(items: Vec<TaskEntry>) -> Self {
        TaskView {
            remote: false,
            generation: 0,
            page_revision: 0,
            page_loading: false,
            total: items.len(),
            offset: 0,
            page_offset: 0,
            query_pending: false,
            detail: None,
            detail_loading: None,
            detail_bytes: Vec::new(),
            detail_revision: 0,
            detail_needs_refresh: false,
            all_items: items.clone(),
            items,
            filter: 0,
            project_only: false,
            recent: false,
            query: String::new(),
            visible: 1,
            detail_scroll: 0,
            notice: None,
            compose_pending: None,
            sel: 0,
            scroll: 0,
            mode: Mode::Browse,
        }
    }

    /// Enter the bounded server-side query mode used by real attachments.
    pub fn paged(
        items: Vec<TaskEntry>,
        total: usize,
        query: uniterm_proto::TaskQuery,
        revision: u64,
    ) -> Self {
        let mut view = Self::new(Vec::new());
        view.remote = true;
        view.generation = query.generation;
        view.refresh_page(items, total, query, revision);
        view
    }

    /// Apply a page while preserving selection by stable task id.
    pub fn refresh_page(
        &mut self,
        items: Vec<TaskEntry>,
        total: usize,
        query: uniterm_proto::TaskQuery,
        revision: u64,
    ) {
        if query.generation < self.generation {
            return;
        }
        if self.compose_pending.is_some() {
            // A launch pins its own task, but still acknowledge the outstanding
            // page so a refused launch can return to a usable list.
            self.page_loading = false;
            self.query_pending = true;
            return;
        }
        self.page_revision = revision;
        let changing_query = std::mem::take(&mut self.page_loading);
        let keep = self.items.get(self.sel).map(|t| t.id);
        let backwards = query.offset < self.page_offset;
        self.items = items;
        self.total = total;
        self.offset = query.offset;
        self.page_offset = query.offset;
        self.filter = query.filter as usize;
        self.project_only = query.project_only;
        self.recent = query.recent;
        self.query = query.search;
        self.sel = keep
            .and_then(|id| self.items.iter().position(|t| t.id == id))
            .unwrap_or_else(|| {
                if backwards {
                    self.items.len().saturating_sub(1)
                } else {
                    0
                }
            });
        let selected = self.items.get(self.sel).map(|t| t.id);
        if changing_query || selected != keep || self.detail_loading != selected {
            self.detail = None;
            self.detail_loading = None;
            self.detail_bytes.clear();
        }
        self.query_pending = false;
        if let Mode::Edit {
            id, answer: false, ..
        }
        | Mode::ConfirmDelete(id) = self.mode
        {
            if !self.items.iter().any(|t| t.id == id) {
                self.mode = Mode::Browse;
            }
        }
        self.reveal();
    }

    /// Emit only demand-driven page/detail reads after navigation or a live update.
    pub fn requests(&mut self) -> Vec<uniterm_proto::ClientMessage> {
        if !self.remote {
            return Vec::new();
        }
        if self.compose_pending.is_some() {
            return Vec::new();
        }
        if std::mem::take(&mut self.query_pending) {
            self.generation = self.generation.saturating_add(1);
            self.page_loading = true;
            return vec![uniterm_proto::ClientMessage::TaskPage {
                query: uniterm_proto::TaskQuery {
                    generation: self.generation,
                    selected: None,
                    offset: self.offset,
                    search: self.query.clone(),
                    filter: self.filter as u8,
                    project_only: self.project_only,
                    recent: self.recent,
                },
            }];
        }
        if self.page_loading {
            return Vec::new();
        }
        let Some(id) = self.items.get(self.sel).map(|t| t.id) else {
            return Vec::new();
        };
        if self.compose_pending.is_none()
            && (self.detail_needs_refresh || self.detail.as_ref().is_none_or(|t| t.id != id))
            && self.detail_loading != Some(id)
        {
            self.detail_loading = Some(id);
            self.detail_needs_refresh = false;
            self.detail_bytes.clear();
            vec![uniterm_proto::ClientMessage::TaskDetail { id, offset: 0 }]
        } else {
            Vec::new()
        }
    }

    /// Assemble one immutable transfer, returning a draft only for an explicit launch.
    pub fn receive_detail(&mut self, chunk: &uniterm_proto::TaskDetailChunk) -> Option<TaskEntry> {
        let wanted = if chunk.compose {
            self.compose_pending
        } else if self.compose_pending.is_some() {
            None
        } else {
            self.items.get(self.sel).map(|t| t.id)
        };
        if wanted != Some(chunk.id) || (self.page_loading && !chunk.compose) {
            return None;
        }
        if chunk.offset == 0 {
            self.detail_bytes.clear();
            self.detail_revision = chunk.revision;
        }
        if chunk.offset != self.detail_bytes.len() || chunk.revision != self.detail_revision {
            return None;
        }
        self.detail_bytes.extend_from_slice(&chunk.bytes);
        if self.detail_bytes.len() == chunk.total {
            let task = serde_json::from_slice::<TaskEntry>(&self.detail_bytes).ok()?;
            self.detail_bytes.clear();
            self.detail_loading = None;
            if chunk.compose {
                return Some(task);
            }
            self.detail = Some(task);
            self.detail_needs_refresh = chunk.revision < self.page_revision;
        }
        None
    }

    /// A refused transfer must not leave launch or detail loading stuck.
    pub fn detail_error(&mut self, id: u64, error: String) {
        if self.compose_pending == Some(id) {
            self.compose_pending = None;
        }
        if self.detail_loading == Some(id) {
            self.detail_loading = None;
        }
        self.detail_bytes.clear();
        self.notice = Some(error);
    }

    /// A pull acknowledges exactly the bytes received; no stream fills the queue.
    pub fn detail_continuation(
        &self,
        chunk: &uniterm_proto::TaskDetailChunk,
    ) -> Option<uniterm_proto::ClientMessage> {
        let wanted = if chunk.compose {
            self.compose_pending
        } else if self.compose_pending.is_some() {
            None
        } else {
            self.items.get(self.sel).map(|t| t.id)
        };
        (wanted == Some(chunk.id)
            && (!self.page_loading || chunk.compose)
            && self.detail_revision == chunk.revision
            && self.detail_bytes.len() == chunk.offset.saturating_add(chunk.bytes.len())
            && self.detail_bytes.len() < chunk.total)
            .then_some(uniterm_proto::ClientMessage::TaskDetail {
                id: chunk.id,
                offset: chunk.offset + chunk.bytes.len(),
            })
    }

    /// Replace the items with a fresh server snapshot, keeping the selection
    /// stable by task id where possible.
    pub fn refresh(&mut self, items: Vec<TaskEntry>) {
        self.all_items = items;
        self.refilter();
    }

    fn refilter(&mut self) {
        if self.remote {
            self.offset = 0;
            self.query_pending = true;
            return;
        }
        let keep = self.items.get(self.sel).map(|t| t.id);
        self.items = self
            .all_items
            .iter()
            .filter(|t| {
                (!self.project_only || t.active_project)
                    && match self.filter {
                        1 => matches!(t.status, TaskStatus::Doing | TaskStatus::Blocked),
                        2 => t.status == TaskStatus::Todo,
                        3 => t.status == TaskStatus::Done,
                        _ => true,
                    }
                    && (self.query.is_empty()
                        || format!(
                            "{} {} {}",
                            t.title,
                            t.notes,
                            t.agents
                                .iter()
                                .map(|a| a.agent.as_str())
                                .collect::<Vec<_>>()
                                .join(" ")
                        )
                        .to_lowercase()
                        .contains(&self.query.to_lowercase()))
            })
            .cloned()
            .collect();
        if self.recent {
            self.items
                .sort_by_key(|t| std::cmp::Reverse((t.updated, t.id)));
        }
        self.sel = keep
            .and_then(|id| self.items.iter().position(|t| t.id == id))
            .unwrap_or(0);
        if let Mode::Edit {
            id, answer: false, ..
        }
        | Mode::ConfirmDelete(id) = self.mode
        {
            if !self.all_items.iter().any(|t| t.id == id) {
                self.mode = Mode::Browse;
            }
        }
        self.reveal();
    }

    fn reveal(&mut self) {
        self.scroll = self
            .scroll
            .min(self.items.len().saturating_sub(self.visible));
        if self.sel < self.scroll {
            self.scroll = self.sel;
        }
        if self.sel >= self.scroll + self.visible {
            self.scroll = self.sel + 1 - self.visible;
        }
    }

    /// The modal's box rectangle: the shared near-fullscreen geometry.
    pub fn rect(cols: u16, rows: u16) -> Rect {
        modal_rect(cols, rows)
    }

    fn selected(&self) -> Option<&TaskEntry> {
        let item = self.items.get(self.sel)?;
        self.detail
            .as_ref()
            .filter(|detail| detail.id == item.id)
            .or(Some(item))
    }

    fn nav(&mut self, down: bool, visible: usize) {
        self.detail_scroll = 0;
        if self.remote {
            if down
                && self.sel + 1 == self.items.len()
                && self.offset + self.items.len() < self.total
            {
                self.offset += uniterm_proto::TASK_PAGE_SIZE;
                self.query_pending = true;
                return;
            }
            if !down && self.sel == 0 && self.offset > 0 {
                self.offset = self.offset.saturating_sub(uniterm_proto::TASK_PAGE_SIZE);
                self.query_pending = true;
                return;
            }
        }
        nav_list(
            &mut self.sel,
            &mut self.scroll,
            down,
            self.items.len(),
            visible,
        );
    }

    /// Drive the modal from raw key bytes. Only the first action-producing
    /// key of a chunk is honoured (modal semantics).
    pub fn handle(&mut self, chunk: &[u8], cols: u16, rows: u16) -> TaskAction {
        let visible = modal_visible_rows(Self::rect(cols, rows).h);
        self.visible = visible.max(1);
        self.reveal();
        let mut redraw = false;
        let mut i = 0;
        while i < chunk.len() {
            let b = chunk[i];
            if self.mode == Mode::Browse
                && (chunk[i..].starts_with(b"\x1b[6~") || chunk[i..].starts_with(b"\x1b[5~"))
            {
                let width = Self::rect(cols, rows).w.saturating_sub(LIST_W + 3) as usize;
                let count = self.detail_lines(width, usize::MAX).len();
                self.detail_scroll = if chunk[i + 2] == b'6' {
                    self.detail_scroll.saturating_add(visible)
                } else {
                    self.detail_scroll.saturating_sub(visible)
                }
                .min(count.saturating_sub(visible));
                return TaskAction::Redraw;
            }

            if self.mode == Mode::Search {
                let (key, used) = decode_key(chunk, i);
                i += used.max(1);
                match key {
                    LineKey::Escape => {
                        self.query.clear();
                        self.mode = Mode::Browse;
                    }
                    LineKey::Enter => self.mode = Mode::Browse,
                    LineKey::Char(c) if !c.is_control() => self.query.push(c),
                    LineKey::Backspace => {
                        self.query.pop();
                    }
                    _ => {}
                }
                self.refilter();
                redraw = true;
                continue;
            }
            if chunk[i] == 0x0a {
                if let Mode::Edit {
                    notes: true,
                    buf,
                    cursor,
                    ..
                } = &mut self.mode
                {
                    edit_line(buf, cursor, LineKey::Char('\n'));
                    i += 1;
                    redraw = true;
                    continue;
                }
            }
            // Inline edit mode captures everything printable.
            if matches!(self.mode, Mode::Edit { .. }) {
                let (key, used) = decode_key(chunk, i);
                i += used.max(1);
                match key {
                    LineKey::Escape | LineKey::Cancel => {
                        self.mode = Mode::Browse;
                        redraw = true;
                    }
                    LineKey::Enter => {
                        let action = match &self.mode {
                            Mode::Edit {
                                id,
                                buf,
                                notes: true,
                                ..
                            } => Some(TaskAction::Notes(*id, buf.clone())),
                            Mode::Edit {
                                id,
                                buf,
                                answer: true,
                                ..
                            } if !buf.trim().is_empty() => {
                                Some(TaskAction::Answer(*id, buf.clone()))
                            }
                            Mode::Edit { id, buf, .. } if !buf.trim().is_empty() => {
                                Some(TaskAction::Retitle(*id, buf.trim().into()))
                            }
                            _ => None,
                        };
                        self.mode = Mode::Browse;
                        if let Some(action) = action {
                            return action;
                        }
                        redraw = true;
                    }
                    key => {
                        if let Mode::Edit { buf, cursor, .. } = &mut self.mode {
                            redraw |= edit_line(buf, cursor, key);
                        }
                    }
                }
                continue;
            }
            if let Mode::ConfirmDelete(id) = self.mode {
                self.mode = Mode::Browse;
                if b == b'x' {
                    return TaskAction::Delete(id);
                }
                redraw = true;
                i += 1;
                continue;
            }
            // Browse mode.
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
                return TaskAction::Close; // lone Esc
            }
            match b {
                b'q' | 0x03 => return TaskAction::Close,
                b'k' => {
                    self.nav(false, visible);
                    redraw = true;
                }
                b'j' => {
                    self.nav(true, visible);
                    redraw = true;
                }
                b'f' => {
                    self.filter = (self.filter + 1) % 4;
                    self.refilter();
                    redraw = true;
                }
                b'p' => {
                    self.project_only = !self.project_only;
                    self.refilter();
                    redraw = true;
                }
                b'r' => {
                    self.recent = !self.recent;
                    self.refilter();
                    redraw = true;
                }
                b'/' => {
                    self.mode = Mode::Search;
                    redraw = true;
                }
                b'l' => {
                    if let Some(t) = self.selected() {
                        return TaskAction::Compose(t.id);
                    }
                }
                b'\r' | b'\n' => {
                    if let Some(item) = self.selected().and_then(|t| t.waiting.first()) {
                        return TaskAction::WaitingFocus(item.id);
                    }
                    if let Some(a) = self.selected().and_then(|t| t.agents.first()) {
                        return TaskAction::Focus(a.pane_id);
                    }
                }
                b'u' => {
                    if let Some(item) = self.selected().and_then(|t| t.waiting.first()) {
                        return TaskAction::Resume(item.id);
                    }
                }
                b'a' => {
                    if let Some(id) = self
                        .selected()
                        .and_then(|t| t.waiting.first().map(|w| w.id))
                    {
                        self.mode = Mode::Edit {
                            id,
                            notes: false,
                            answer: true,
                            buf: String::new(),
                            cursor: 0,
                        };
                        redraw = true;
                    }
                }
                b'e' | b'n'
                    if !self.remote
                        || self.detail.as_ref().is_some_and(|t| {
                            self.items.get(self.sel).is_some_and(|item| item.id == t.id)
                        }) =>
                {
                    if let Some(t) = self.selected() {
                        self.mode = Mode::Edit {
                            id: t.id,
                            notes: b == b'n',
                            answer: false,
                            buf: if b == b'n' {
                                t.notes.clone()
                            } else {
                                t.title.clone()
                            },
                            cursor: if b == b'n' {
                                t.notes.len()
                            } else {
                                t.title.len()
                            },
                        };
                        redraw = true;
                    }
                }
                b' ' => {
                    if let Some(t) = self.selected() {
                        return TaskAction::SetStatus(t.id, t.status.next());
                    }
                }
                b'x' if self.selected().is_some() => {
                    self.mode = Mode::ConfirmDelete(self.selected().unwrap().id);
                    redraw = true;
                }
                _ => {}
            }
            i += 1;
        }
        if redraw {
            TaskAction::Redraw
        } else {
            TaskAction::None
        }
    }

    /// Resolve a click at 1-based `(cx, cy)`: select a list row, press an
    /// action button, or (outside the box) close.
    pub fn click(&mut self, cols: u16, rows: u16, cx: u16, cy: u16) -> TaskAction {
        let r = Self::rect(cols, rows);
        match modal_hit(r, LIST_W, cx, cy) {
            ModalHit::Outside => TaskAction::Close,
            ModalHit::Bar(rel) => {
                for (span, index) in footer_spans(self.buttons()) {
                    let key = self.buttons()[index].0;
                    if span.contains(&rel) {
                        return match key {
                            "e" => self.handle(b"e", cols, rows),
                            "enter" => self.handle(b"\r", cols, rows),
                            "l" => self.handle(b"l", cols, rows),
                            "n" => self.handle(b"n", cols, rows),
                            "a" => self.handle(b"a", cols, rows),
                            "/" => self.handle(b"/", cols, rows),
                            "f" => self.handle(b"f", cols, rows),
                            "p" => self.handle(b"p", cols, rows),
                            "r" => self.handle(b"r", cols, rows),
                            "esc" => self.handle(b"\x1b", cols, rows),
                            "spc" => self.handle(b" ", cols, rows),
                            "x" => self.handle(b"x", cols, rows),
                            _ => TaskAction::None,
                        };
                    }
                }
                TaskAction::None
            }
            ModalHit::ListRow(slot) => {
                let row = slot + self.scroll;
                if row < self.items.len() && row < self.scroll + modal_visible_rows(r.h) {
                    if matches!(self.mode, Mode::Edit { .. }) {
                        return TaskAction::None;
                    }
                    self.mode = Mode::Browse;
                    self.sel = row;
                    self.detail_scroll = 0;
                    return TaskAction::Redraw;
                }
                TaskAction::None
            }
            ModalHit::None => TaskAction::None,
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
        let all_detail = self.detail_lines(inner.saturating_sub(list_w + 1), usize::MAX);
        let start = self
            .detail_scroll
            .min(all_detail.len().saturating_sub(visible));
        let mut detail: Vec<_> = all_detail.into_iter().skip(start).take(visible).collect();
        detail.resize_with(visible, || {
            format!("{panel}{}", " ".repeat(inner.saturating_sub(list_w + 1)))
        });
        let scroll = self
            .scroll
            .min(self.sel)
            .max(self.sel.saturating_sub(visible.saturating_sub(1)));
        render_list_modal(
            cols,
            rows,
            &format!(
                " Tasks ({}) | {} | {} | {} {} ",
                if self.remote {
                    self.total
                } else {
                    self.items.len()
                },
                ["all", "active", "planned", "completed"][self.filter],
                if self.project_only {
                    "project"
                } else {
                    "workspace"
                },
                if self.recent { "recent" } else { "status" },
                clip_text(&self.query, 16)
            ),
            list_w,
            |slot| {
                let idx = scroll + slot;
                let t = self.items.get(idx)?;
                let selected = idx == self.sel;
                let dot = task_status_color(t.status).sgr_fg();
                let name = clip_text(&t.title, list_w - 5);
                // The cell is ` ● name…` filled to EXACTLY list_w cells:
                // 3 fixed cells (space, dot, space) + name + fill.
                let fill = " ".repeat(list_w - 3 - name.width());
                Some(if selected {
                    format!(
                        "{selected_style} \x1b[{dot}m\u{25CF}\x1b[{}m {name}{fill}\x1b[0m{panel}",
                        theme.status_active_fg.sgr_fg()
                    )
                } else {
                    format!(" \x1b[{dot}m\u{25CF}{panel} {name}{fill}")
                })
            },
            &detail,
            &self.bar_text(inner),
        )
    }

    /// The action bar's text (mode-dependent), padded to `width`, in the
    /// shared overlay footer styling (grey keycaps, dim captions).
    fn bar_text(&self, width: usize) -> String {
        footer_text(self.buttons(), width)
    }

    fn buttons(&self) -> &'static [(&'static str, &'static str)] {
        match &self.mode {
            Mode::Edit { .. } => &[("enter", "save/send"), ("esc", "cancel")],
            Mode::ConfirmDelete(_) => &[("x", "confirm delete record"), ("esc", "cancel")],
            Mode::Search => &[("enter", "apply search"), ("esc", "clear")],
            Mode::Browse => BUTTONS,
        }
    }

    /// The detail pane's lines (styled, padded to `width`), `count` rows.
    fn detail_lines(&self, width: usize, count: usize) -> Vec<String> {
        let panel = panel_style_no_reset();
        let theme = ui_theme();
        let dim = format!("\x1b[{}m", theme.muted.sgr_fg());
        let strong = format!("\x1b[1;{}m", theme.foreground.sgr_fg());
        let error = format!("\x1b[1;{}m", theme.error.sgr_fg());
        let mk = styled_line;
        let mut out: Vec<(String, usize)> = Vec::new();
        let Some(t) = self.selected() else {
            out.push(mk(&[]));
            out.push(mk(&[(&panel, "  "), (&dim, "No tasks yet.")]));
            out.push(mk(&[
                (&panel, "  "),
                (&dim, "Launch one with Ctrl-A N (New Task)."),
            ]));
            return finish_lines(out.clone(), &panel, width, count.min(out.len()));
        };
        if let Some(notice) = &self.notice {
            out.push(mk(&[(&error, &clip_text(notice, width))]));
        }
        out.push(mk(&[]));
        // Title, wrapped over up to 3 rows (or the edit buffer).
        if let Mode::Edit { buf, cursor, .. } = &self.mode {
            let shown = line_with_cursor(buf, *cursor, width.saturating_sub(6));
            out.push(mk(&[(&panel, "  "), (&strong, "> "), (&strong, &shown)]));
        } else {
            let preview: String = t.title.chars().take(width.saturating_mul(3)).collect();
            for line in wrap(&preview, width.saturating_sub(4)).into_iter().take(3) {
                out.push(mk(&[(&panel, "  "), (&strong, &line)]));
            }
        }
        out.push(mk(&[]));
        let dot_style = format!("\x1b[{}m", task_status_color(t.status).sgr_fg());
        let status_text = format!("\u{25CF} {}", t.status.display());
        out.push(mk(&[
            (&panel, "  "),
            (&dim, "progress"),
            (&panel, "  "),
            (&dot_style, &status_text),
        ]));
        let idline = format!("#{}", t.id);
        out.push(mk(&[
            (&panel, "  "),
            (&dim, "task"),
            (&panel, "    "),
            (&panel, &idline),
        ]));
        out.push(mk(&[]));
        if matches!(self.mode, Mode::ConfirmDelete(_)) {
            out.push(mk(&[
                (&panel, "  "),
                (&error, "delete this task? press x again to confirm"),
            ]));
        } else if t.notes.is_empty() {
            out.push(mk(&[(&panel, "  "), (&dim, "notes"), (&panel, "   -")]));
        } else {
            out.push(mk(&[(&panel, "  "), (&dim, "notes")]));
            let preview: String = t.notes.chars().take(width.saturating_mul(4)).collect();
            for line in wrap(&preview, width.saturating_sub(4)).into_iter().take(4) {
                out.push(mk(&[(&panel, "  "), (&panel, &line)]));
            }
        }
        let mut facts =
            vec!["e: title | n: notes | a: answer | u: resume | PgUp/PgDn: details".into()];
        if let Some(project) = &t.project_name {
            facts.push(format!("Project: {project}"));
        }
        if let Some(source) = t.context.source_task {
            facts.push(format!("New attempt of task #{source}"));
        }
        if let Some(run) = &t.run {
            facts.push(format!("Run #{}: {}", run.id.0, run.status.label()));
            for role in &run.roles {
                facts.push(format!(
                    "{}: {} / pane {}",
                    role.name, role.provider, role.pane.0
                ));
            }
            if let Some(outcome) = &run.outcome {
                facts.push(format!("Outcome: {outcome}"));
            }
        }
        for agent in &t.agents {
            facts.push(format!(
                "{}: {} / pane {}",
                uniterm_core::agent::agent_name(&agent.agent),
                agent.status.label(),
                agent.pane_id.0
            ));
            if agent.waiting.is_some() {
                facts.push("Waiting: Enter opens pane; a answers the request".into());
            }
        }
        for item in &t.waiting {
            facts.push(format!("Waiting: {}", item.summary));
        }
        for artifact in &t.artifacts {
            facts.push(format!(
                "Artifact #{}: {} ({:?})",
                artifact.id.0, artifact.path, artifact.status
            ));
        }
        facts.push("Progress is bookkeeping; agent idle does not mean done.".into());
        if matches!(self.mode, Mode::ConfirmDelete(_)) {
            facts.push("Deleting this record does not stop its agents or run.".into());
        }
        for fact in facts {
            for line in wrap(&fact, width.saturating_sub(4)) {
                out.push(mk(&[(&panel, "  "), (&dim, &line)]));
            }
        }
        let count = count.min(out.len());
        finish_lines(out, &panel, width, count)
    }
}

fn task_status_color(status: TaskStatus) -> uniterm_core::Color {
    let theme = ui_theme();
    match status {
        TaskStatus::Todo => theme.accent,
        TaskStatus::Doing => theme.warning,
        TaskStatus::Blocked => theme.error,
        TaskStatus::Done => theme.success,
    }
}

/// The clickable spans of the action bar's buttons, as interior column ranges
/// (the shared footer layout; pair 0 is the navigation hint, not a button).
#[cfg(test)]
fn bar_spans() -> Vec<(std::ops::Range<usize>, &'static str)> {
    footer_spans(BUTTONS)
        .into_iter()
        .filter(|(_, i)| *i > 0)
        .map(|(range, i)| (range, BUTTONS[i].0))
        .collect()
}

/// Greedy word wrap (words longer than `width` are hard-split).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in plain_text(text).split_whitespace() {
        if !current.is_empty() {
            if current.width() + 1 + word.width() > width {
                lines.push(std::mem::take(&mut current));
            } else {
                current.push(' ');
            }
        }
        for g in word.graphemes(true) {
            if current.width() + g.width() > width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            if g.width() <= width {
                current.push_str(g);
            }
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, title: &str, status: TaskStatus) -> TaskEntry {
        TaskEntry {
            id,
            title: title.into(),
            status,
            notes: String::new(),
            context: Default::default(),
            updated: id,
            project_name: None,
            active_project: false,
            run: None,
            agents: Vec::new(),
            artifacts: Vec::new(),
            waiting: Vec::new(),
        }
    }

    fn view() -> TaskView {
        TaskView::new(vec![
            entry(1, "make the tests pass", TaskStatus::Doing),
            entry(2, "refactor the parser", TaskStatus::Todo),
            entry(3, "ship v2", TaskStatus::Done),
        ])
    }

    #[test]
    fn navigation_and_status_cycle() {
        let mut v = view();
        assert_eq!(v.handle(b"j", 120, 40), TaskAction::Redraw);
        assert_eq!(v.sel, 1);
        // Space cycles the selected task's status (planned -> running).
        assert_eq!(
            v.handle(b" ", 120, 40),
            TaskAction::SetStatus(2, TaskStatus::Doing)
        );
        // Arrows work too.
        assert_eq!(v.handle(b"\x1b[A", 120, 40), TaskAction::Redraw);
        assert_eq!(v.sel, 0);
    }

    #[test]
    fn delete_needs_a_second_x() {
        let mut v = view();
        assert_eq!(v.handle(b"x", 120, 40), TaskAction::Redraw); // arm confirm
        assert_eq!(v.handle(b"j", 120, 40), TaskAction::Redraw); // cancels
        assert_eq!(v.handle(b"x", 120, 40), TaskAction::Redraw);
        assert_eq!(v.handle(b"x", 120, 40), TaskAction::Delete(1));
    }

    #[test]
    fn inline_edit_round_trip() {
        let mut v = view();
        assert_eq!(v.handle(b"e", 120, 40), TaskAction::Redraw);
        // Type ", please" then save.
        v.handle(b", please", 120, 40);
        match v.handle(b"\r", 120, 40) {
            TaskAction::Retitle(1, t) => assert_eq!(t, "make the tests pass, please"),
            other => panic!("expected Retitle, got {other:?}"),
        }
        // Esc cancels an edit without sending anything.
        v.handle(b"e", 120, 40);
        assert_eq!(v.handle(b"\x1b", 120, 40), TaskAction::Redraw);
        assert_eq!(v.handle(b"\x1b", 120, 40), TaskAction::Close);
    }

    #[test]
    fn refresh_keeps_selection_by_id() {
        let mut v = view();
        v.handle(b"j", 120, 40); // select id 2
        v.refresh(vec![
            entry(2, "refactor the parser", TaskStatus::Doing),
            entry(3, "ship v2", TaskStatus::Done),
        ]);
        assert_eq!(v.items[v.sel].id, 2);
        // The selected task disappearing clamps the selection.
        v.refresh(vec![entry(3, "ship v2", TaskStatus::Done)]);
        assert_eq!(v.items[v.sel].id, 3);
    }

    #[test]
    fn clicks_select_rows_and_press_buttons() {
        let mut v = view();
        let r = TaskView::rect(120, 40);
        // Click the second list row.
        assert_eq!(v.click(120, 40, r.x + 3, r.y + 2), TaskAction::Redraw);
        assert_eq!(v.sel, 1);
        // Click the delete button twice (arm, then confirm via key).
        let bar_y = r.y + r.h - 2;
        let (span, _) = bar_spans().into_iter().find(|(_, k)| *k == "x").unwrap();
        let bx = r.x + 1 + span.start as u16 + 1;
        assert_eq!(v.click(120, 40, bx, bar_y), TaskAction::Redraw);
        assert_eq!(v.handle(b"x", 120, 40), TaskAction::Delete(2));
        // A click outside the box closes.
        assert_eq!(v.click(120, 40, 1, 1), TaskAction::Close);
    }

    #[test]
    fn render_shows_badges_details_and_bar() {
        let v = view();
        let s = String::from_utf8(v.render(120, 40)).unwrap();
        assert!(s.contains("Tasks (3)"));
        assert!(s.contains("make the tests pass"));
        assert!(s.contains("running")); // status display name
        let theme = ui_theme();
        assert!(s.contains(&theme.warning.sgr_fg()));
        assert!(s.contains(&theme.accent.sgr_fg()));
        assert!(s.contains(&theme.success.sgr_fg()));
        assert!(s.contains("delete")); // action bar
        assert!(s.contains('\u{25CF}')); // status dots
    }

    #[test]
    fn every_box_row_paints_exactly_its_full_width() {
        // The "grey stripe" regression: any row painting fewer cells than the
        // box width lets the pane underneath bleed through and breaks the
        // right border. Check every state: browse, edit, confirm, empty.
        let mut views = vec![view(), TaskView::new(Vec::new())];
        let mut editing = view();
        editing.handle(b"e", 120, 40);
        views.push(editing);
        let mut confirming = view();
        confirming.handle(b"x", 120, 40);
        views.push(confirming);
        for v in &views {
            let r = TaskView::rect(120, 40);
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

    #[test]
    fn tiny_terminals_render_without_underflow() {
        // rows <= 3 used to drive the box height to 0 and the `y + h - 3`
        // row math through a u16 underflow; the modal rect now clamps to its
        // minimum and overhangs instead (terminals clip the overhang).
        for (cols, rows) in [(3, 3), (20, 2), (1, 1), (48, 10)] {
            let _ = view().render(cols, rows);
            let mut v = view();
            assert_eq!(v.click(cols, rows, 0, 0), TaskAction::Close);
        }
    }

    #[test]
    fn wrap_is_greedy_and_safe() {
        assert_eq!(wrap("aaaa bbbb cccc", 9), vec!["aaaa bbbb", "cccc"]);
        let long = wrap("abcdefghijklmnop", 8);
        assert_eq!(long, vec!["abcdefgh", "ijklmnop"]);
        assert_eq!(wrap("", 10), vec![String::new()]);
    }
    #[test]
    fn editing_and_confirmation_never_follow_a_different_selection() {
        let mut v = view();
        let r = TaskView::rect(120, 40);
        v.handle(b"e", 120, 40);
        assert_eq!(v.click(120, 40, r.x + 3, r.y + 2), TaskAction::None);
        v.handle(b"!", 120, 40);
        assert_eq!(
            v.handle(b"\r", 120, 40),
            TaskAction::Retitle(1, "make the tests pass!".into())
        );
        v.handle(b"e", 120, 40);
        v.refresh(vec![entry(2, "other", TaskStatus::Todo)]);
        assert!(!matches!(
            v.handle(b"\r", 120, 40),
            TaskAction::Retitle(_, _)
        ));
        v = view();
        v.handle(b"x", 120, 40);
        v.click(120, 40, r.x + 3, r.y + 2);
        assert_eq!(v.handle(b"x", 120, 40), TaskAction::Redraw);
        assert_eq!(v.handle(b"x", 120, 40), TaskAction::Delete(2));
    }

    #[test]
    fn filtering_recency_notes_and_launch_use_stable_ids() {
        let mut v = view();
        assert_eq!(v.handle(b"l", 120, 40), TaskAction::Compose(1));
        v.handle(b"n", 120, 40);
        v.handle(b"description", 120, 40);
        assert_eq!(
            v.handle(b"\r", 120, 40),
            TaskAction::Notes(1, "description".into())
        );
        v.handle(b"r", 120, 40);
        assert_eq!(v.items[0].id, 3);
        v.handle(b"ff", 120, 40);
        assert_eq!(v.items.iter().map(|t| t.id).collect::<Vec<_>>(), [2]);
        v.handle(b"/parser\r", 120, 40);
        assert_eq!(v.items.len(), 1);
        v.handle(b"p", 120, 40);
        assert!(v.items.is_empty());
    }

    #[test]
    fn controls_and_unicode_cannot_escape_task_geometry() {
        let text = "界界 e\u{301} \x1b]52;c;payload\x07\x1b[2J";
        let v = TaskView::new(vec![entry(1, text, TaskStatus::Todo)]);
        let rendered = v.render(72, 25);
        let rendered_text = String::from_utf8_lossy(&rendered);
        assert!(!rendered_text.contains("\x1b]52"));
        assert!(!rendered_text.contains("\x1b[2J"));
        for line in wrap(text, 9) {
            assert!(line.width() <= 9);
            assert!(!line.contains('\x1b'));
        }
        assert_eq!(clip_text("界界e\u{301}", 5).width(), 5);
        let r = TaskView::rect(72, 25);
        for (row, col, width) in crate::overlay::render_segments(&rendered) {
            if col == r.x && (r.y..r.y + r.h).contains(&row) {
                assert_eq!(width, r.w as usize);
            }
        }
    }

    #[test]
    fn refresh_reveals_selection_after_reordering_and_deletion() {
        let mut v = TaskView::new(
            (1..60)
                .map(|id| entry(id, "task", TaskStatus::Todo))
                .collect(),
        );
        for _ in 0..50 {
            v.handle(b"j", 80, 20);
        }
        let id = v.selected().unwrap().id;
        v.refresh(vec![entry(id, "retained", TaskStatus::Done)]);
        assert_eq!((v.sel, v.scroll), (0, 0));
        assert!(String::from_utf8(v.render(80, 20))
            .unwrap()
            .contains("retained"));
    }
    #[test]
    fn remote_pages_load_exact_detail_before_editing_and_preserve_selection() {
        let items = (1..=32)
            .map(|id| entry(id, "summary", TaskStatus::Todo))
            .collect();
        let mut view = TaskView::paged(items, 40, Default::default(), 0);
        assert!(matches!(
            view.requests().as_slice(),
            [uniterm_proto::ClientMessage::TaskDetail { id: 1, offset: 0 }]
        ));
        assert!(view.requests().is_empty());
        view.handle(b"n", 120, 40);
        assert_eq!(view.mode, Mode::Browse);
        let mut detail = entry(1, "exact title", TaskStatus::Todo);
        detail.notes = "full notes".repeat(40_000);
        let bytes = serde_json::to_vec(&detail).unwrap();
        for (index, bytes_chunk) in bytes
            .chunks(uniterm_proto::TASK_DETAIL_CHUNK_SIZE)
            .enumerate()
        {
            view.receive_detail(&uniterm_proto::TaskDetailChunk {
                id: 1,
                offset: index * uniterm_proto::TASK_DETAIL_CHUNK_SIZE,
                total: bytes.len(),
                revision: 7,
                bytes: bytes_chunk.to_vec(),
                compose: false,
            });
        }
        view.handle(b"n", 120, 40);
        assert!(matches!(&view.mode, Mode::Edit { buf, .. } if *buf == detail.notes));
        view.handle(b"\x1b", 120, 40);
        view.sel = 31;
        view.handle(b"j", 120, 40);
        assert!(
            matches!(view.requests().as_slice(), [uniterm_proto::ClientMessage::TaskPage { query }] if query.offset == 32)
        );
        view.refresh_page(
            (33..=40)
                .map(|id| entry(id, "next", TaskStatus::Todo))
                .collect(),
            40,
            uniterm_proto::TaskQuery {
                offset: 32,
                generation: 1,
                ..Default::default()
            },
            0,
        );
        assert_eq!(view.items[view.sel].id, 33);
        view.handle(b"k", 120, 40);
        assert!(
            matches!(view.requests().as_slice(), [uniterm_proto::ClientMessage::TaskPage { query }] if query.offset == 0)
        );
        view.refresh_page(
            (1..=32)
                .map(|id| entry(id, "first", TaskStatus::Todo))
                .collect(),
            40,
            uniterm_proto::TaskQuery {
                generation: 2,
                ..Default::default()
            },
            0,
        );
        assert_eq!(view.items[view.sel].id, 32);
        view.handle(b"/needle", 120, 40);
        assert!(
            matches!(view.requests().as_slice(), [uniterm_proto::ClientMessage::TaskPage { query }] if query.search == "needle" && query.offset == 0)
        );
    }

    #[test]
    fn compose_completes_while_filter_or_page_reply_is_pending() {
        for navigation in [false, true] {
            for page_after_chunks in 0..=2 {
                let mut view = TaskView::paged(
                    (1..=32)
                        .map(|id| entry(id, "summary", TaskStatus::Doing))
                        .collect(),
                    40,
                    Default::default(),
                    1,
                );
                if navigation {
                    view.sel = 31;
                }
                view.handle(if navigation { b"j" } else { b"f" }, 120, 40);
                let requests = view.requests();
                let [uniterm_proto::ClientMessage::TaskPage { query }] = requests.as_slice() else {
                    panic!("page request");
                };
                let id = view.items[view.sel].id;
                assert_eq!(view.handle(b"l", 120, 40), TaskAction::Compose(id));
                view.compose_pending = Some(id);
                let task = entry(id, "exact original task", TaskStatus::Doing);
                let bytes = serde_json::to_vec(&task).unwrap();
                let split = bytes.len() / 2;
                let mut draft = None;
                for step in 0..=2 {
                    if step == page_after_chunks {
                        // The list may now select an entirely different task.
                        view.refresh_page(
                            vec![entry(40, "other", TaskStatus::Doing)],
                            40,
                            query.clone(),
                            2,
                        );
                    }
                    if step < 2 {
                        let offset = if step == 0 { 0 } else { split };
                        let end = if step == 0 { split } else { bytes.len() };
                        let chunk = uniterm_proto::TaskDetailChunk {
                            id,
                            offset,
                            total: bytes.len(),
                            revision: 1,
                            bytes: bytes[offset..end].to_vec(),
                            compose: true,
                        };
                        draft = view.receive_detail(&chunk);
                        assert_eq!(view.detail_continuation(&chunk).is_some(), step == 0);
                    }
                    assert!(view.requests().is_empty());
                }
                assert_eq!(draft, Some(task));
                assert!(!view.page_loading);
                // A refused composition also returns to a usable list.
                view.detail_error(id, "removed".into());
                assert!(matches!(
                    view.requests().as_slice(),
                    [uniterm_proto::ClientMessage::TaskPage { .. }]
                ));
            }
        }
    }

    #[test]
    fn compose_ignores_old_detail_and_missing_task_releases_pending_state() {
        let mut view = TaskView::paged(
            vec![entry(1, "summary", TaskStatus::Todo)],
            1,
            Default::default(),
            0,
        );
        view.compose_pending = Some(1);
        let mut detail = entry(1, "exact", TaskStatus::Todo);
        detail.notes = "notes".into();
        let bytes = serde_json::to_vec(&detail).unwrap();
        let chunk = uniterm_proto::TaskDetailChunk {
            id: 1,
            offset: 0,
            total: bytes.len(),
            revision: 1,
            bytes,
            compose: false,
        };
        assert!(view.receive_detail(&chunk).is_none());
        assert!(view.detail.is_none());
        let draft = view.receive_detail(&uniterm_proto::TaskDetailChunk {
            compose: true,
            ..chunk
        });
        assert_eq!(draft, Some(detail));
        view.detail_error(1, "gone".into());
        assert_eq!(view.compose_pending, None);
        assert_eq!(view.notice.as_deref(), Some("gone"));
    }
    #[test]
    fn live_updates_do_not_restart_an_immutable_detail_transfer() {
        let mut view = TaskView::paged(
            vec![entry(1, "summary", TaskStatus::Todo)],
            1,
            Default::default(),
            1,
        );
        assert_eq!(view.requests().len(), 1);
        let mut detail = entry(1, "exact", TaskStatus::Todo);
        detail.notes = "large notes".repeat(10000);
        let bytes = serde_json::to_vec(&detail).unwrap();
        let split = uniterm_proto::TASK_DETAIL_CHUNK_SIZE;
        let first = uniterm_proto::TaskDetailChunk {
            id: 1,
            offset: 0,
            total: bytes.len(),
            revision: 1,
            bytes: bytes[..split].to_vec(),
            compose: false,
        };
        view.receive_detail(&first);
        view.refresh_page(
            vec![entry(1, "changed", TaskStatus::Doing)],
            1,
            Default::default(),
            2,
        );
        assert!(view.requests().is_empty());
        assert!(view.detail_continuation(&first).is_some());
        let rest = uniterm_proto::TaskDetailChunk {
            id: 1,
            offset: split,
            total: bytes.len(),
            revision: 1,
            bytes: bytes[split..].to_vec(),
            compose: false,
        };
        view.receive_detail(&rest);
        assert_eq!(view.detail.as_ref(), Some(&detail));
        assert!(view.detail_continuation(&rest).is_none());
        assert!(matches!(
            view.requests().as_slice(),
            [uniterm_proto::ClientMessage::TaskDetail { id: 1, offset: 0 }]
        ));
        assert!(view.requests().is_empty());
    }
}
