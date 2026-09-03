//! Copy-mode: scrollback navigation, selection, and incremental search.
//!
//! Entered with `prefix [`. While a pane is in copy-mode the server routes the
//! client's keystrokes here instead of to the PTY, renders a history-aware
//! viewport with the selection highlighted, and on yank emits an OSC 52
//! clipboard write to the client. This is the terminal-native equivalent of
//! tmux copy-mode (`docs/04`).

use std::fmt::Write as _;

use uniterm_core::{Attrs, Cell, Color, Grid, Rect};

const LATEST_BUTTON: &str = "[v Latest]";

/// The outcome of handling copy-mode input.
pub enum CopyAction {
    /// Nothing observable changed.
    None,
    /// State changed; the pane should be repainted.
    Redraw,
    /// Leave copy-mode.
    Exit,
    /// Copy this text to the client clipboard, then leave copy-mode.
    Copy(String),
}

/// Copy-mode state for one pane.
pub struct CopyState {
    /// Logical line index (into the combined history+screen buffer) of the top
    /// visible row.
    top: usize,
    /// Cursor within the viewport.
    cx: u16,
    cy: u16,
    width: u16,
    height: u16,
    /// Selection anchor as an absolute `(line, x)`, if a selection is active.
    anchor: Option<(usize, u16)>,
    /// The current search query and whether we are typing it.
    query: String,
    typing_search: bool,
    first_line_id: Option<u64>,
}

impl CopyState {
    /// Enter copy-mode at the bottom of the buffer (showing the live screen).
    pub fn new(grid: &Grid, rect: Rect) -> Self {
        let total = grid.total_lines();
        let height = rect.h.max(1);
        let top = total.saturating_sub(height as usize);
        CopyState {
            top,
            cx: 0,
            cy: height.saturating_sub(1),
            width: rect.w.max(1),
            height,
            anchor: None,
            query: String::new(),
            typing_search: false,
            first_line_id: grid.first_line_id(),
        }
    }

    /// Compensate viewport and selection coordinates when live output evicts
    /// old history while copy-mode is open.
    pub fn sync_history(&mut self, grid: &Grid) {
        let current = grid.first_line_id();
        if let (Some(old), Some(new)) = (self.first_line_id, current) {
            let evicted = new.saturating_sub(old) as usize;
            if evicted != 0 {
                self.top = self.top.saturating_sub(evicted);
                self.anchor = self
                    .anchor
                    .and_then(|(line, x)| (line >= evicted).then_some((line - evicted, x)));
            }
        }
        self.first_line_id = current;
        self.top = self.top.min(self.max_top(grid));
        self.cy = self.cy.min(self.height.saturating_sub(1));
    }

    /// Rebase copy-mode after pane reflow. Reflow changes physical row and
    /// column coordinates, so an in-progress cell selection is safely cleared.
    pub fn resize(&mut self, grid: &Grid, rect: Rect) {
        self.sync_history(grid);
        let at_bottom = self.top == self.max_top(grid);
        self.width = rect.w.max(1);
        self.height = rect.h.max(1);
        self.top = if at_bottom {
            self.max_top(grid)
        } else {
            self.top.min(self.max_top(grid))
        };
        self.cx = self.cx.min(self.width - 1);
        self.cy = self.cy.min(self.height - 1);
        self.anchor = None;
        self.first_line_id = grid.first_line_id();
    }

    fn max_top(&self, grid: &Grid) -> usize {
        grid.total_lines().saturating_sub(self.height as usize)
    }

    fn cursor_line(&self) -> usize {
        self.top + self.cy as usize
    }

    /// Absolute cursor position for the frame's final `CUP`.
    pub fn cursor_pos(&self, rect: Rect) -> (u16, u16) {
        (rect.x + self.cx, rect.y + self.cy)
    }

    /// Process a batch of input bytes; returns the resulting action.
    pub fn handle(&mut self, buf: &[u8], grid: &Grid) -> CopyAction {
        self.sync_history(grid);
        let mut changed = false;
        let mut i = 0;
        while i < buf.len() {
            let b = buf[i];
            if self.typing_search {
                match b {
                    0x0d | 0x0a => {
                        self.run_search(grid, true);
                        self.typing_search = false;
                    }
                    0x1b => {
                        self.typing_search = false;
                        self.query.clear();
                    }
                    0x7f | 0x08 => {
                        self.query.pop();
                    }
                    c if c.is_ascii_graphic() || c == b' ' => self.query.push(c as char),
                    _ => {}
                }
                changed = true;
                i += 1;
                continue;
            }

            // Cursor keys use CSI sequences. Modified variants include the
            // platform-native jump-to-live shortcuts: Ctrl+End on Linux and
            // Cmd+Down on macOS (when the terminal reports that modifier).
            if b == 0x1b {
                if buf.get(i + 1) == Some(&b'[') {
                    let Some(length) = csi_sequence_len(&buf[i + 2..]) else {
                        break;
                    };
                    let sequence = &buf[i + 2..i + 2 + length];
                    if matches!(sequence, b"1;5F" | b"1;9B") {
                        return CopyAction::Exit;
                    }
                    match sequence {
                        b"A" => self.up(),
                        b"B" => self.down(grid),
                        b"C" => self.right(),
                        b"D" => self.left(),
                        b"5~" => self.page(grid, true),
                        b"6~" => self.page(grid, false),
                        _ => {}
                    }
                    i += 2 + length;
                    changed = true;
                    continue;
                }
                return CopyAction::Exit; // lone ESC leaves copy-mode
            }

            match b {
                b'q' => return CopyAction::Exit,
                b'k' => self.up(),
                b'j' => self.down(grid),
                b'h' => self.left(),
                b'l' => self.right(),
                b'w' | b'W' => self.next_big_word(grid),
                b'b' | b'B' => self.previous_big_word(grid),
                b'e' | b'E' => self.end_big_word(grid),
                0x15 => self.page(grid, true),  // Ctrl-U
                0x04 => self.page(grid, false), // Ctrl-D
                b'g' => {
                    self.top = 0;
                    self.cy = 0;
                }
                b'G' => {
                    self.top = self.max_top(grid);
                    self.cy = self.height.saturating_sub(1);
                }
                b'v' | b' ' => {
                    self.anchor = if self.anchor.is_some() {
                        None
                    } else {
                        Some((self.cursor_line(), self.cx))
                    };
                }
                b'y' | 0x0d | 0x0a => {
                    let text = self.selected_text(grid);
                    return CopyAction::Copy(text);
                }
                b'/' => {
                    self.typing_search = true;
                    self.query.clear();
                }
                b'n' => self.run_search(grid, true),
                b'N' => self.run_search(grid, false),
                _ => {}
            }
            changed = true;
            i += 1;
        }
        if changed {
            CopyAction::Redraw
        } else {
            CopyAction::None
        }
    }

    fn up(&mut self) {
        if self.cy > 0 {
            self.cy -= 1;
        } else if self.top > 0 {
            self.top -= 1;
        }
    }

    fn down(&mut self, grid: &Grid) {
        let last = grid.total_lines().saturating_sub(1);
        if self.cursor_line() >= last {
            return;
        }
        if (self.cy as usize) < self.height as usize - 1 {
            self.cy += 1;
        } else if self.top < self.max_top(grid) {
            self.top += 1;
        }
    }

    fn left(&mut self) {
        self.cx = self.cx.saturating_sub(1);
    }

    fn right(&mut self) {
        if self.cx + 1 < self.width {
            self.cx += 1;
        }
    }

    fn is_space(&self, grid: &Grid, x: u16) -> bool {
        grid.line_cell(self.cursor_line(), x).ch.is_whitespace()
    }

    fn next_big_word(&mut self, grid: &Grid) {
        while self.cx + 1 < self.width && !self.is_space(grid, self.cx) {
            self.cx += 1;
        }
        while self.cx + 1 < self.width && self.is_space(grid, self.cx) {
            self.cx += 1;
        }
    }

    fn previous_big_word(&mut self, grid: &Grid) {
        self.cx = self.cx.saturating_sub(1);
        while self.cx > 0 && self.is_space(grid, self.cx) {
            self.cx -= 1;
        }
        while self.cx > 0 && !self.is_space(grid, self.cx - 1) {
            self.cx -= 1;
        }
    }

    fn end_big_word(&mut self, grid: &Grid) {
        while self.cx + 1 < self.width && self.is_space(grid, self.cx) {
            self.cx += 1;
        }
        while self.cx + 1 < self.width && !self.is_space(grid, self.cx + 1) {
            self.cx += 1;
        }
    }

    /// Clamp a 0-based screen cell into pane-local viewport coordinates.
    fn cell_in(&self, rect: Rect, sx: u16, sy: u16) -> (u16, u16) {
        (
            sx.saturating_sub(rect.x).min(self.width.saturating_sub(1)),
            sy.saturating_sub(rect.y).min(self.height.saturating_sub(1)),
        )
    }

    /// Anchor a mouse selection at 0-based screen cell `(sx, sy)` inside
    /// `rect`: the anchor and the cursor both move there.
    pub fn mouse_anchor(&mut self, rect: Rect, sx: u16, sy: u16) {
        let (x, y) = self.cell_in(rect, sx, sy);
        self.cx = x;
        self.cy = y;
        self.anchor = Some((self.cursor_line(), x));
    }

    /// Extend the mouse selection to screen cell `(sx, sy)` (clamped into the
    /// pane, so dragging past an edge selects to that edge).
    pub fn mouse_drag(&mut self, rect: Rect, sx: u16, sy: u16) {
        let (x, y) = self.cell_in(rect, sx, sy);
        self.cx = x;
        self.cy = y;
    }

    /// The selected text (the cursor's line when no anchor is set), as `y`
    /// yields it - used by the mouse-release yank.
    pub fn yank(&self, grid: &Grid) -> String {
        self.selected_text(grid)
    }

    /// Scroll the viewport by `lines` (negative = up into history), used by the
    /// mouse wheel. The cursor rides along with the viewport. Returns whether
    /// the view ends at the live bottom (so wheel-down can leave copy-mode).
    pub fn scroll(&mut self, grid: &Grid, lines: i32) -> bool {
        let max = self.max_top(grid);
        self.top = if lines < 0 {
            self.top.saturating_sub(lines.unsigned_abs() as usize)
        } else {
            (self.top + lines as usize).min(max)
        };
        self.top == max
    }

    /// The clickable jump-to-live control, once the viewport is more than one
    /// full pane-height behind the current screen.
    pub fn latest_button_rect(&self, grid: &Grid, rect: Rect) -> Option<Rect> {
        let pages_behind = self.max_top(grid).saturating_sub(self.top);
        if pages_behind <= self.height as usize {
            return None;
        }
        let indicator_width = self.indicator_label(grid).len() as u16;
        let indicator_x = rect.x + rect.w.saturating_sub(indicator_width + 1);
        let button_width = LATEST_BUTTON.len() as u16;
        let button_x = indicator_x.checked_sub(button_width + 1)?;
        (button_x >= rect.x).then_some(Rect::new(button_x, rect.y, button_width, 1))
    }

    fn page(&mut self, grid: &Grid, up: bool) {
        let half = (self.height / 2).max(1) as usize;
        if up {
            self.top = self.top.saturating_sub(half);
        } else {
            self.top = (self.top + half).min(self.max_top(grid));
        }
    }

    /// The ordered selection endpoints `(start, end)`, if a selection exists.
    fn selection(&self) -> Option<((usize, u16), (usize, u16))> {
        let anchor = self.anchor?;
        let cursor = (self.cursor_line(), self.cx);
        let a = anchor.0 * self.width as usize + anchor.1 as usize;
        let c = cursor.0 * self.width as usize + cursor.1 as usize;
        Some(if a <= c {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        })
    }

    /// Whether `(line, x)` falls within the current selection (linear order).
    fn is_selected(&self, line: usize, x: u16) -> bool {
        let Some((s, e)) = self.selection() else {
            return false;
        };
        let pos = line * self.width as usize + x as usize;
        let lo = s.0 * self.width as usize + s.1 as usize;
        let hi = e.0 * self.width as usize + e.1 as usize;
        pos >= lo && pos <= hi
    }

    fn selected_text(&self, grid: &Grid) -> String {
        let Some((s, e)) = self.selection() else {
            // No explicit selection: yank the cursor's line.
            return line_text(grid, self.cursor_line(), self.width);
        };
        let mut out = String::new();
        for line in s.0..=e.0 {
            let from = if line == s.0 { s.1 } else { 0 };
            let to = if line == e.0 {
                e.1.saturating_add(1).min(self.width)
            } else {
                self.width
            };
            let selected = line_text_range(grid, line, from, to);
            if to == self.width {
                out.push_str(selected.trim_end());
            } else {
                out.push_str(&selected);
            }
            if line != e.0 && !grid.line_wrapped(line) {
                out.push('\n');
            }
        }
        out
    }

    fn run_search(&mut self, grid: &Grid, forward: bool) {
        if self.query.is_empty() {
            return;
        }
        let total = grid.total_lines();
        let start = self.cursor_line();
        // Scan every line once, starting just past the cursor.
        for step in 1..=total {
            let line = if forward {
                (start + step) % total
            } else {
                (start + total - step) % total
            };
            let text = line_text(grid, line, self.width);
            if let Some(byte) = text.find(&self.query) {
                let col = search_column(grid, line, self.width, byte);
                self.top = line.min(self.max_top(grid));
                self.cy = (line - self.top) as u16;
                self.cx = col.min(self.width.saturating_sub(1));
                return;
            }
        }
    }

    /// Render the viewport (history-aware), selection highlight, and indicator.
    pub fn render(&self, grid: &Grid, rect: Rect, ops: &mut Vec<u8>) {
        for row in 0..rect.h {
            let line = self.top + row as usize;
            ops.extend_from_slice(format!("\x1b[{};{}H", rect.y + row + 1, rect.x + 1).as_bytes());
            let mut x = 0;
            while x < rect.w {
                let cell = grid.line_cell(line, x);
                if cell.is_continuation() {
                    x += 1;
                    continue;
                }
                let selected =
                    self.is_selected(line, x) || (cell.width == 2 && self.is_selected(line, x + 1));
                emit_copy_style(
                    cell,
                    grid.underline_color(cell.underline_color),
                    selected,
                    ops,
                );
                grid.write_cell_display_text(cell, ops);
                x = x.saturating_add(cell.display_width() as u16);
            }
            ops.extend_from_slice(b"\x1b[0m");
        }
        // Indicator at the top-right of the pane.
        let label = self.indicator_label(grid);
        let lx = rect.x + rect.w.saturating_sub(label.len() as u16 + 1);
        ops.extend_from_slice(format!("\x1b[{};{}H\x1b[7m", rect.y + 1, lx + 1).as_bytes());
        ops.extend_from_slice(label.as_bytes());
        ops.extend_from_slice(b"\x1b[0m");
        if let Some(button) = self.latest_button_rect(grid, rect) {
            ops.extend_from_slice(
                format!("\x1b[{};{}H\x1b[7m", button.y + 1, button.x + 1).as_bytes(),
            );
            ops.extend_from_slice(LATEST_BUTTON.as_bytes());
            ops.extend_from_slice(b"\x1b[0m");
        }
    }

    fn indicator_label(&self, grid: &Grid) -> String {
        if self.typing_search {
            format!("[search: {}]", self.query)
        } else {
            format!("[COPY {}/{}]", self.cursor_line() + 1, grid.total_lines())
        }
    }
}

fn csi_sequence_len(input: &[u8]) -> Option<usize> {
    input
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))
        .map(|index| index + 1)
}

fn line_text(grid: &Grid, line: usize, width: u16) -> String {
    line_text_range(grid, line, 0, width).trim_end().to_string()
}

fn line_text_range(grid: &Grid, line: usize, from: u16, to: u16) -> String {
    let mut out = String::new();
    let mut x = from.min(grid.width());
    if x > 0 && grid.line_cell(line, x).is_continuation() {
        x -= 1;
    }
    while x < to.min(grid.width()) {
        let cell = grid.line_cell(line, x);
        if !cell.is_continuation() {
            out.push_str(&grid.cell_text_owned(cell));
        }
        x += cell.display_width().max(1) as u16;
    }
    out
}

fn search_column(grid: &Grid, line: usize, width: u16, byte_offset: usize) -> u16 {
    let mut bytes = 0;
    let mut x = 0;
    while x < width.min(grid.width()) {
        let cell = grid.line_cell(line, x);
        if !cell.is_continuation() {
            let text = grid.cell_text_owned(cell);
            if bytes >= byte_offset || bytes + text.len() > byte_offset {
                return x;
            }
            bytes += text.len();
        }
        x += cell.display_width().max(1) as u16;
    }
    width.saturating_sub(1)
}

fn emit_copy_style(cell: Cell, underline_color: Color, selected: bool, out: &mut Vec<u8>) {
    let mut sgr = String::from("\x1b[0");
    for (attr, code) in [
        (Attrs::BOLD, 1),
        (Attrs::DIM, 2),
        (Attrs::ITALIC, 3),
        (Attrs::BLINK, 5),
        (Attrs::REVERSE, 7),
        (Attrs::INVISIBLE, 8),
        (Attrs::STRIKE, 9),
    ] {
        if cell.attrs.0 & attr.0 != 0 {
            let _ = write!(sgr, ";{code}");
        }
    }
    match cell.attrs.underline_style() {
        uniterm_core::UnderlineStyle::None => {}
        uniterm_core::UnderlineStyle::Single => sgr.push_str(";4"),
        uniterm_core::UnderlineStyle::Double => sgr.push_str(";4:2"),
        uniterm_core::UnderlineStyle::Curly => sgr.push_str(";4:3"),
        uniterm_core::UnderlineStyle::Dotted => sgr.push_str(";4:4"),
        uniterm_core::UnderlineStyle::Dashed => sgr.push_str(";4:5"),
    }
    if selected {
        sgr.push_str(";7");
    }
    match cell.fg {
        Color::Default => {}
        Color::Idx(n) => {
            let _ = write!(sgr, ";38;5;{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(sgr, ";38;2;{r};{g};{b}");
        }
    }
    match cell.bg {
        Color::Default => {}
        Color::Idx(n) => {
            let _ = write!(sgr, ";48;5;{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(sgr, ";48;2;{r};{g};{b}");
        }
    }
    match underline_color {
        Color::Default => {}
        Color::Idx(n) => {
            let _ = write!(sgr, ";58;5;{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(sgr, ";58;2;{r};{g};{b}");
        }
    }
    sgr.push('m');
    out.extend_from_slice(sgr.as_bytes());
}

/// Encode bytes as standard base64 (for the OSC 52 clipboard payload).
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x3) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(((b1 & 0xf) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Build an OSC 52 clipboard-set sequence for `text`.
pub fn osc52(text: &str) -> Vec<u8> {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes())).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn osc52_wraps_base64() {
        let seq = osc52("hi");
        let s = String::from_utf8(seq).unwrap();
        assert!(s.starts_with("\x1b]52;c;"));
        assert!(s.ends_with('\x07'));
        assert!(s.contains("aGk=")); // base64("hi")
    }

    #[test]
    fn enter_and_navigate_scrollback() {
        let mut g = Grid::new(10, 3);
        // Fill history: write a line and scroll, a few times.
        for i in 0..5u8 {
            g.set(
                0,
                2,
                uniterm_core::Cell {
                    ch: (b'a' + i) as char,
                    ..Default::default()
                },
            );
            g.scroll_up(uniterm_core::Cell::default());
        }
        let rect = Rect::new(0, 0, 10, 3);
        let mut cs = CopyState::new(&g, rect);
        let top_before = cs.top;
        cs.handle(b"k", &g); // move up
        cs.handle(b"g", &g); // jump to top
        assert_eq!(cs.top, 0);
        assert!(cs.top <= top_before);
    }

    #[test]
    fn big_word_motions_use_whitespace_delimiters() {
        let mut grid = Grid::new(20, 3);
        for (column, character) in "one  two-three".chars().enumerate() {
            grid.set(
                column as u16,
                2,
                Cell {
                    ch: character,
                    ..Default::default()
                },
            );
        }
        let mut state = CopyState::new(&grid, Rect::new(0, 0, 20, 3));
        state.handle(b"w", &grid);
        assert_eq!(state.cx, 5);
        state.handle(b"e", &grid);
        assert_eq!(state.cx, 13);
        state.handle(b"b", &grid);
        assert_eq!(state.cx, 5);
    }

    #[test]
    fn mouse_selection_anchors_drags_and_yanks() {
        let mut g = Grid::new(12, 3);
        for (i, ch) in "HELLOWORLD".chars().enumerate() {
            g.set(
                i as u16,
                0,
                uniterm_core::Cell {
                    ch,
                    ..Default::default()
                },
            );
        }
        // The pane sits at an offset; screen cells must translate.
        let rect = Rect::new(2, 1, 12, 3);
        let mut cs = CopyState::new(&g, rect);
        cs.mouse_anchor(rect, 2, 1); // pane cell (0, 0)
        cs.mouse_drag(rect, 6, 1); // pane cell (4, 0)
        assert_eq!(cs.yank(&g), "HELLO");
        // Dragging past the right edge clamps to the pane.
        cs.mouse_drag(rect, 99, 1);
        assert_eq!(cs.yank(&g), "HELLOWORLD");
    }

    #[test]
    fn wheel_scroll_moves_viewport_and_reports_bottom() {
        let mut g = Grid::new(10, 3);
        for i in 0..8u8 {
            g.set(
                0,
                2,
                uniterm_core::Cell {
                    ch: (b'a' + i) as char,
                    ..Default::default()
                },
            );
            g.scroll_up(uniterm_core::Cell::default());
        }
        let rect = Rect::new(0, 0, 10, 3);
        let mut cs = CopyState::new(&g, rect);
        // Opens at the bottom; scrolling up leaves it, scrolling down returns.
        assert!(cs.scroll(&g, 0));
        assert!(!cs.scroll(&g, -3));
        let top_after_up = cs.top;
        assert!(top_after_up < g.total_lines() - 3);
        assert!(cs.scroll(&g, 100)); // clamped to the bottom
        assert!(!cs.scroll(&g, -100)); // clamped to the top
        assert_eq!(cs.top, 0);
    }

    #[test]
    fn latest_button_only_appears_more_than_one_page_behind() {
        let mut g = Grid::new(40, 4);
        for _ in 0..12 {
            g.scroll_up(Cell::default());
        }
        let rect = Rect::new(3, 2, 40, 4);
        let mut cs = CopyState::new(&g, rect);
        assert_eq!(cs.latest_button_rect(&g, rect), None);

        cs.scroll(&g, -4);
        assert_eq!(cs.latest_button_rect(&g, rect), None);
        cs.scroll(&g, -1);
        let button = cs.latest_button_rect(&g, rect).expect("latest button");
        assert_eq!(button.y, rect.y);
        assert_eq!(button.w, LATEST_BUTTON.len() as u16);
        assert!(button.x >= rect.x);

        let mut rendered = Vec::new();
        cs.render(&g, rect, &mut rendered);
        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.contains(LATEST_BUTTON));
        assert!(rendered.contains("[COPY"));
    }

    #[test]
    fn native_bottom_shortcuts_resume_live_output() {
        let g = Grid::new(10, 3);
        let rect = Rect::new(0, 0, 10, 3);
        for shortcut in [b"\x1b[1;5F".as_slice(), b"\x1b[1;9B"] {
            let mut cs = CopyState::new(&g, rect);
            assert!(matches!(cs.handle(shortcut, &g), CopyAction::Exit));
        }
    }

    #[test]
    fn selection_yields_text_and_copies() {
        let mut g = Grid::new(6, 2);
        for (i, ch) in "hello".chars().enumerate() {
            g.set(
                i as u16,
                0,
                uniterm_core::Cell {
                    ch,
                    ..Default::default()
                },
            );
        }
        let rect = Rect::new(0, 0, 6, 2);
        let mut cs = CopyState::new(&g, rect);
        // Put cursor at row 0, start selection, extend right across "hello".
        cs.top = 0;
        cs.cy = 0;
        cs.cx = 0;
        cs.handle(b"v", &g); // anchor
        for _ in 0..4 {
            cs.handle(b"l", &g);
        }
        match cs.handle(b"y", &g) {
            CopyAction::Copy(text) => assert_eq!(text, "hello"),
            _ => panic!("expected Copy"),
        }
    }
}
