//! The task manager (AG7's management surface, v2): a modal with a
//! colour-coded task list on the left, a detail pane on the right, and an
//! action bar - edit (inline), cycle status, delete (two-step confirm).
//!
//! Pure state + rendering + hit-testing, like the other client surfaces: the
//! attach loop feeds keys/mouse in and sends the returned ops to the server;
//! the server answers every mutation with a fresh snapshot, so the modal is
//! always a projection of server truth.

use crate::overlay::{
    finish_lines, footer_spans, footer_text, modal_hit, modal_rect, modal_visible_rows, nav_list,
    panel_style, panel_style_no_reset, render_list_modal, styled_line, ui_theme, ModalHit, Rect,
};
use crate::text_input::{decode_key, edit_line, line_with_cursor, LineKey};
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
}

/// The modal's input mode.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    Browse,
    /// Inline title editing in the detail pane.
    Edit {
        buf: String,
        cursor: usize,
    },
    /// `x` pressed once; a second `x` deletes, anything else cancels.
    ConfirmDelete,
}

/// The open task manager.
pub struct TaskView {
    pub items: Vec<TaskEntry>,
    pub sel: usize,
    scroll: usize,
    mode: Mode,
}

/// The action bar's pairs (shared overlay styling); index 0 is the
/// non-clickable navigation hint.
const BUTTONS: &[(&str, &str)] = &[
    ("\u{2191}\u{2193}", "select"),
    ("e", "edit"),
    ("spc", "status"),
    ("x", "delete"),
    ("esc", "close"),
];

/// Width of the list column (incl. its padding), fixed for a stable layout.
const LIST_W: u16 = 28;

impl TaskView {
    pub fn new(items: Vec<TaskEntry>) -> Self {
        TaskView {
            items,
            sel: 0,
            scroll: 0,
            mode: Mode::Browse,
        }
    }

    /// Replace the items with a fresh server snapshot, keeping the selection
    /// stable by task id where possible.
    pub fn refresh(&mut self, items: Vec<TaskEntry>) {
        let keep = self.items.get(self.sel).map(|t| t.id);
        self.items = items;
        self.sel = keep
            .and_then(|id| self.items.iter().position(|t| t.id == id))
            .unwrap_or(0)
            .min(self.items.len().saturating_sub(1));
        if !matches!(self.mode, Mode::Edit { .. }) {
            self.mode = Mode::Browse;
        }
    }

    /// The modal's box rectangle: the shared near-fullscreen geometry.
    pub fn rect(cols: u16, rows: u16) -> Rect {
        modal_rect(cols, rows)
    }

    fn selected(&self) -> Option<&TaskEntry> {
        self.items.get(self.sel)
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

    /// Drive the modal from raw key bytes. Only the first action-producing
    /// key of a chunk is honoured (modal semantics).
    pub fn handle(&mut self, chunk: &[u8], cols: u16, rows: u16) -> TaskAction {
        let visible = modal_visible_rows(Self::rect(cols, rows).h);
        let mut redraw = false;
        let mut i = 0;
        while i < chunk.len() {
            let b = chunk[i];
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
                        let title = match &self.mode {
                            Mode::Edit { buf, .. } => buf.trim().to_string(),
                            _ => String::new(),
                        };
                        let id = self.selected().map(|task| task.id);
                        self.mode = Mode::Browse;
                        if let (Some(id), false) = (id, title.is_empty()) {
                            return TaskAction::Retitle(id, title);
                        }
                        redraw = true;
                    }
                    key => {
                        if let Mode::Edit { buf, cursor } = &mut self.mode {
                            redraw |= edit_line(buf, cursor, key);
                        }
                    }
                }
                continue;
            }
            if self.mode == Mode::ConfirmDelete {
                self.mode = Mode::Browse;
                if b == b'x' {
                    if let Some(t) = self.selected() {
                        return TaskAction::Delete(t.id);
                    }
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
                b'e' => {
                    if let Some(t) = self.selected() {
                        self.mode = Mode::Edit {
                            buf: t.title.clone(),
                            cursor: t.title.len(),
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
                    self.mode = Mode::ConfirmDelete;
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
                for (span, key) in bar_spans() {
                    if span.contains(&rel) {
                        return match key {
                            "e" => self.handle(b"e", cols, rows),
                            "spc" => self.handle(b" ", cols, rows),
                            "x" => self.handle(b"x", cols, rows),
                            _ => TaskAction::Close,
                        };
                    }
                }
                TaskAction::None
            }
            ModalHit::ListRow(slot) => {
                let row = slot + self.scroll;
                if row < self.items.len() && row < self.scroll + modal_visible_rows(r.h) {
                    self.sel = row;
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
        let detail = self.detail_lines(inner.saturating_sub(list_w + 1), visible);
        render_list_modal(
            cols,
            rows,
            &format!(" Tasks ({}) ", self.items.len()),
            list_w,
            |slot| {
                let idx = self.scroll + slot;
                let t = self.items.get(idx)?;
                let selected = idx == self.sel;
                let dot = task_status_color(t.status).sgr_fg();
                let name: String = t.title.chars().take(list_w - 5).collect();
                // The cell is ` ● name…` filled to EXACTLY list_w cells:
                // 3 fixed cells (space, dot, space) + name + fill.
                let fill = " ".repeat(list_w - 3 - name.chars().count());
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
        let pairs: &[(&str, &str)] = match &self.mode {
            Mode::Edit { .. } => &[("enter", "save"), ("esc", "cancel")],
            Mode::ConfirmDelete => &[("x", "confirm delete"), ("any key", "cancels")],
            Mode::Browse => BUTTONS,
        };
        footer_text(pairs, width)
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
            return finish_lines(out, &panel, width, count);
        };
        out.push(mk(&[]));
        // Title, wrapped over up to 3 rows (or the edit buffer).
        if let Mode::Edit { buf, cursor } = &self.mode {
            let shown = line_with_cursor(buf, *cursor, width.saturating_sub(6));
            out.push(mk(&[(&panel, "  "), (&strong, "> "), (&strong, &shown)]));
        } else {
            for line in wrap(&t.title, width.saturating_sub(4)).into_iter().take(3) {
                out.push(mk(&[(&panel, "  "), (&strong, &line)]));
            }
        }
        out.push(mk(&[]));
        let dot_style = format!("\x1b[{}m", task_status_color(t.status).sgr_fg());
        let status_text = format!("\u{25CF} {}", t.status.display());
        out.push(mk(&[
            (&panel, "  "),
            (&dim, "status"),
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
        if self.mode == Mode::ConfirmDelete {
            out.push(mk(&[
                (&panel, "  "),
                (&error, "delete this task? press x again to confirm"),
            ]));
        } else if t.notes.is_empty() {
            out.push(mk(&[(&panel, "  "), (&dim, "notes"), (&panel, "   -")]));
        } else {
            out.push(mk(&[(&panel, "  "), (&dim, "notes")]));
            for line in wrap(&t.notes, width.saturating_sub(4)).into_iter().take(4) {
                out.push(mk(&[(&panel, "  "), (&panel, &line)]));
            }
        }
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
fn bar_spans() -> Vec<(std::ops::Range<usize>, &'static str)> {
    footer_spans(BUTTONS)
        .into_iter()
        .filter(|(_, i)| *i > 0)
        .map(|(range, i)| (range, BUTTONS[i].0))
        .collect()
}

/// Greedy word wrap (words longer than `width` are hard-split).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        while word.chars().count() > width {
            let head: String = word.chars().take(width).collect();
            let tail: String = word.chars().skip(width).collect();
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            lines.push(head);
            word = tail;
        }
        if cur.is_empty() {
            cur = word;
        } else if cur.chars().count() + 1 + word.chars().count() <= width {
            cur.push(' ');
            cur.push_str(&word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = word;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
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
}
