//! The damage-tracked, dirty-cell diffing renderer (Decision R2).
//!
//! Given a [`Grid`] with damage, it emits the minimal escape sequences for the
//! cells that actually changed, and *nothing at all* when the grid is clean.
//! It caches cursor position and SGR state to avoid redundant sequences - the
//! `screen-write.c`/`tty.c` techniques from tmux, in Rust. See
//! `docs/04-multiplexer-core.md`.
//!
//! The server keeps one renderer cache per attached client and composites pane
//! damage at layout offsets. Grapheme text and display width come from the grid,
//! so a damaged half of a wide glyph always repaints the complete glyph span.

use std::fmt::Write as _;

use uniterm_core::{Attrs, Cell, Color, Grid, UnderlineStyle};

/// Renders grids to escape sequences, carrying cross-call caches so consecutive
/// renders stay minimal.
#[derive(Default)]
pub struct Renderer {
    // Cached terminal state so we only emit changes.
    last_style: Option<(Color, Color, Attrs, Color)>,
    /// OSC 8 target currently open in the host terminal, if any.
    last_hyperlink: Option<String>,
    cursor: Option<(u16, u16)>,
    /// When set, cells are emitted faint (SGR 2). The server turns this on for
    /// inactive panes so the focused pane is visibly brighter.
    dim: bool,
}

impl Renderer {
    pub fn new() -> Self {
        Renderer::default()
    }

    /// Reset cached state. Call when a client (re)attaches or the screen is
    /// known to have been clobbered, so the next render is unconditional.
    pub fn invalidate(&mut self) {
        self.last_style = None;
        self.last_hyperlink = None;
        self.cursor = None;
    }

    /// Set faint rendering for the next cells (used to dim inactive panes).
    /// Invalidates the style cache so the change takes effect immediately.
    pub fn set_dim(&mut self, dim: bool) {
        if self.dim != dim {
            self.dim = dim;
            self.last_style = None;
        }
    }

    /// Render the damaged cells of `grid` into `out`. Returns the number of
    /// bytes written. The caller is responsible for calling `grid.clear_damage()`
    /// afterwards (kept explicit so a render can target multiple clients).
    ///
    /// If the grid is clean this writes nothing and returns 0 - the literal
    /// implementation of the zero-frames-when-nothing-changes budget.
    pub fn render(&mut self, grid: &Grid, out: &mut Vec<u8>) -> usize {
        if !grid.is_dirty() {
            return 0;
        }
        if grid.pending_scroll_up() != 0 {
            let start = out.len();
            self.render_pane_full(grid, 0, 0, out);
            return out.len() - start;
        }
        let start = out.len();

        for y in 0..grid.height() {
            let Some((lo, hi)) = grid.row_damage(y) else {
                continue;
            };
            let (lo, hi) = expanded_damage(grid, y, lo, hi);
            // One cursor move per damaged run, then emit the run.
            self.move_cursor(lo, y, out);
            let mut x = lo;
            while x <= hi {
                let cell = grid.get(x, y);
                if cell.is_continuation() {
                    x += 1;
                    continue;
                }
                self.emit_cell(grid, cell, out);
                x = x.saturating_add(cell.display_width() as u16);
            }
            // The run advanced the cursor by (hi - lo + 1) columns.
            self.cursor = Some((hi + 1, y));
        }
        self.close_hyperlink(out);

        out.len() - start
    }

    /// Render a pane's damaged cells at window offset `(ox, oy)`. Like [`render`]
    /// but translated, for compositing several panes onto one screen (M3).
    pub fn render_pane_damage(
        &mut self,
        grid: &Grid,
        ox: u16,
        oy: u16,
        out: &mut Vec<u8>,
    ) -> usize {
        if !grid.is_dirty() {
            return 0;
        }
        if grid.pending_scroll_up() != 0 {
            let start = out.len();
            self.render_pane_full(grid, ox, oy, out);
            return out.len() - start;
        }
        self.render_pane_damage_precise(grid, ox, oy, out)
    }

    /// Apply a pending whole-pane upward scroll through terminal margins, then
    /// paint only cells changed independently of that row movement.
    ///
    /// The caller must use this only when the pane spans the entire terminal
    /// width. ANSI vertical margins affect complete rows, so using them for a
    /// side-by-side pane would corrupt its neighbours.
    pub fn render_pane_damage_with_scroll(
        &mut self,
        grid: &Grid,
        ox: u16,
        oy: u16,
        out: &mut Vec<u8>,
    ) -> usize {
        if !grid.is_dirty() {
            return 0;
        }
        let start = out.len();
        let scroll = grid.pending_scroll_up();
        if scroll != 0 {
            self.close_hyperlink(out);
            let mut sequence = String::new();
            let _ = write!(
                sequence,
                "\x1b[{};{}r\x1b[{}S\x1b[r",
                oy + 1,
                oy + grid.height(),
                scroll
            );
            out.extend_from_slice(sequence.as_bytes());
            // Setting DECSTBM commonly homes the cursor. Preserve style caches,
            // but force the next cell run to position itself absolutely.
            self.cursor = None;
        }
        self.render_pane_damage_precise(grid, ox, oy, out);
        out.len() - start
    }

    fn render_pane_damage_precise(
        &mut self,
        grid: &Grid,
        ox: u16,
        oy: u16,
        out: &mut Vec<u8>,
    ) -> usize {
        let start = out.len();
        for y in 0..grid.height() {
            let Some((lo, hi)) = grid.row_damage(y) else {
                continue;
            };
            let (lo, hi) = expanded_damage(grid, y, lo, hi);
            self.move_cursor(ox + lo, oy + y, out);
            let mut x = lo;
            while x <= hi {
                let cell = grid.get(x, y);
                if cell.is_continuation() {
                    x += 1;
                    continue;
                }
                self.emit_cell(grid, cell, out);
                x = x.saturating_add(cell.display_width() as u16);
            }
            self.cursor = Some((ox + hi + 1, oy + y));
        }
        self.close_hyperlink(out);
        out.len() - start
    }

    /// Render every cell of a pane's grid at window offset `(ox, oy)`, ignoring
    /// damage. Used for the attach baseline and after a layout change. Unlike
    /// [`render_full`] it emits no clear-screen (the server clears once for the
    /// whole window, then paints each pane).
    pub fn render_pane_full(&mut self, grid: &Grid, ox: u16, oy: u16, out: &mut Vec<u8>) {
        for y in 0..grid.height() {
            self.move_cursor(ox, oy + y, out);
            let mut x = 0;
            while x < grid.width() {
                let cell = grid.get(x, y);
                if cell.is_continuation() {
                    x += 1;
                    continue;
                }
                self.emit_cell(grid, cell, out);
                x = x.saturating_add(cell.display_width() as u16);
            }
            self.cursor = Some((ox + grid.width(), oy + y));
        }
        self.close_hyperlink(out);
    }

    /// Render the entire grid unconditionally (a full repaint), ignoring damage.
    /// Used when a client attaches mid-stream and needs a complete baseline
    /// before it can consume incremental damage diffs. Emits a clear-screen
    /// first so the client's terminal starts from a known state.
    pub fn render_full(&mut self, grid: &Grid, out: &mut Vec<u8>) {
        self.invalidate();
        out.extend_from_slice(b"\x1b[2J"); // erase display
        for y in 0..grid.height() {
            self.move_cursor(0, y, out);
            let mut x = 0;
            while x < grid.width() {
                let cell = grid.get(x, y);
                if cell.is_continuation() {
                    x += 1;
                    continue;
                }
                self.emit_cell(grid, cell, out);
                x = x.saturating_add(cell.display_width() as u16);
            }
            self.cursor = Some((grid.width(), y));
        }
        self.close_hyperlink(out);
    }

    /// Emit a cursor move to `(x, y)` through the position cache. The server
    /// uses this to park the visible cursor on the active pane after a damage
    /// batch: a raw CUP appended behind the renderer's back would leave the
    /// cache pointing at the end of the last damaged run while the real cursor
    /// sits elsewhere, and every later short-circuited move would then paint
    /// its run at the wrong position (stale cells until a full repaint).
    pub fn place_cursor(&mut self, x: u16, y: u16, out: &mut Vec<u8>) {
        self.move_cursor(x, y, out);
    }

    fn move_cursor(&mut self, x: u16, y: u16, out: &mut Vec<u8>) {
        if self.cursor == Some((x, y)) {
            return; // already there - a cheap, common win on contiguous writes
        }
        // CSI row ; col H  (1-based)
        let mut buf = String::new();
        let _ = write!(buf, "\x1b[{};{}H", y + 1, x + 1);
        out.extend_from_slice(buf.as_bytes());
        self.cursor = Some((x, y));
    }

    fn emit_cell(&mut self, grid: &Grid, cell: Cell, out: &mut Vec<u8>) {
        self.set_hyperlink(grid.hyperlink(cell.hyperlink), out);
        let underline_color = grid.underline_color(cell.underline_color);
        let style = (cell.fg, cell.bg, cell.attrs, underline_color);
        if self.last_style != Some(style) {
            self.emit_sgr(&cell, underline_color, out);
            self.last_style = Some(style);
        }
        grid.write_cell_display_text(cell, out);
    }

    fn set_hyperlink(&mut self, target: Option<&str>, out: &mut Vec<u8>) {
        if self.last_hyperlink.as_deref() == target {
            return;
        }
        self.close_hyperlink(out);
        if let Some(target) = target {
            out.extend_from_slice(b"\x1b]8;;");
            out.extend_from_slice(target.as_bytes());
            out.extend_from_slice(b"\x1b\\");
            self.last_hyperlink = Some(target.into());
        }
    }

    fn close_hyperlink(&mut self, out: &mut Vec<u8>) {
        if self.last_hyperlink.take().is_some() {
            out.extend_from_slice(b"\x1b]8;;\x1b\\");
        }
    }

    fn emit_sgr(&self, cell: &Cell, underline_color: Color, out: &mut Vec<u8>) {
        let mut buf = String::from("\x1b[0"); // reset, then rebuild
        if self.dim {
            buf.push_str(";2"); // faint: dims inactive panes
        }
        if cell.attrs.0 & Attrs::BOLD.0 != 0 {
            buf.push_str(";1");
        }
        if cell.attrs.0 & Attrs::DIM.0 != 0 {
            buf.push_str(";2");
        }
        if cell.attrs.0 & Attrs::ITALIC.0 != 0 {
            buf.push_str(";3");
        }
        match cell.attrs.underline_style() {
            UnderlineStyle::None => {}
            UnderlineStyle::Single => buf.push_str(";4"),
            UnderlineStyle::Double => buf.push_str(";4:2"),
            UnderlineStyle::Curly => buf.push_str(";4:3"),
            UnderlineStyle::Dotted => buf.push_str(";4:4"),
            UnderlineStyle::Dashed => buf.push_str(";4:5"),
        }
        if cell.attrs.0 & Attrs::BLINK.0 != 0 {
            buf.push_str(";5");
        }
        if cell.attrs.0 & Attrs::REVERSE.0 != 0 {
            buf.push_str(";7");
        }
        if cell.attrs.0 & Attrs::INVISIBLE.0 != 0 {
            buf.push_str(";8");
        }
        if cell.attrs.0 & Attrs::STRIKE.0 != 0 {
            buf.push_str(";9");
        }
        // Written in place (no per-style String): colourful apps change style
        // almost every cell, and this sits on the render hot path.
        match cell.fg {
            Color::Default => {}
            Color::Idx(n) => {
                let _ = write!(buf, ";38;5;{n}");
            }
            Color::Rgb(r, g, b) => {
                let _ = write!(buf, ";38;2;{r};{g};{b}");
            }
        }
        match cell.bg {
            Color::Default => {}
            Color::Idx(n) => {
                let _ = write!(buf, ";48;5;{n}");
            }
            Color::Rgb(r, g, b) => {
                let _ = write!(buf, ";48;2;{r};{g};{b}");
            }
        }
        match underline_color {
            Color::Default => {}
            Color::Idx(n) => {
                let _ = write!(buf, ";58;5;{n}");
            }
            Color::Rgb(r, g, b) => {
                let _ = write!(buf, ";58;2;{r};{g};{b}");
            }
        }
        buf.push('m');
        out.extend_from_slice(buf.as_bytes());
    }
}

fn expanded_damage(grid: &Grid, y: u16, mut lo: u16, mut hi: u16) -> (u16, u16) {
    if grid.get(lo, y).is_continuation() && lo > 0 {
        lo -= 1;
    }
    let cell = grid.get(hi, y);
    if !cell.is_continuation() && cell.width == 2 && hi + 1 < grid.width() {
        hi += 1;
    }
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthChar as _;

    struct Oracle {
        cells: Vec<Vec<String>>,
        x: usize,
        y: usize,
        scroll_top: usize,
        scroll_bottom: usize,
    }

    impl Oracle {
        fn new(width: usize, height: usize) -> Self {
            Oracle {
                cells: vec![vec![" ".into(); width]; height],
                x: 0,
                y: 0,
                scroll_top: 0,
                scroll_bottom: height.saturating_sub(1),
            }
        }

        fn apply(&mut self, bytes: &[u8]) {
            let text = std::str::from_utf8(bytes).unwrap();
            let mut at = 0;
            while at < text.len() {
                if text.as_bytes()[at] == 0x1b && text.as_bytes().get(at + 1) == Some(&b'[') {
                    let start = at + 2;
                    let Some(end) = text.as_bytes()[start..]
                        .iter()
                        .position(|byte| (0x40..=0x7e).contains(byte))
                        .map(|offset| start + offset)
                    else {
                        break;
                    };
                    let final_byte = text.as_bytes()[end];
                    let body = &text[start..end];
                    match final_byte {
                        b'H' => {
                            let mut parts = body.split(';');
                            self.y = parts
                                .next()
                                .and_then(|v| v.parse::<usize>().ok())
                                .unwrap_or(1)
                                - 1;
                            self.x = parts
                                .next()
                                .and_then(|v| v.parse::<usize>().ok())
                                .unwrap_or(1)
                                - 1;
                        }
                        b'J' if body == "2" => {
                            for row in &mut self.cells {
                                row.fill(" ".into());
                            }
                        }
                        b'r' => {
                            if body.is_empty() {
                                self.scroll_top = 0;
                                self.scroll_bottom = self.cells.len().saturating_sub(1);
                            } else {
                                let mut parts = body.split(';');
                                self.scroll_top = parts
                                    .next()
                                    .and_then(|v| v.parse::<usize>().ok())
                                    .unwrap_or(1)
                                    .saturating_sub(1);
                                self.scroll_bottom = parts
                                    .next()
                                    .and_then(|v| v.parse::<usize>().ok())
                                    .unwrap_or(self.cells.len())
                                    .saturating_sub(1)
                                    .min(self.cells.len().saturating_sub(1));
                            }
                        }
                        b'S' => {
                            let count = body.parse::<usize>().unwrap_or(1);
                            for _ in 0..count.min(
                                self.scroll_bottom
                                    .saturating_sub(self.scroll_top)
                                    .saturating_add(1),
                            ) {
                                self.cells.remove(self.scroll_top);
                                self.cells.insert(
                                    self.scroll_bottom,
                                    vec![" ".into(); self.cells[0].len()],
                                );
                            }
                        }
                        _ => {}
                    }
                    at = end + 1;
                    continue;
                }
                let ch = text[at..].chars().next().unwrap();
                at += ch.len_utf8();
                let width = ch.width().unwrap_or(0).min(2);
                if width == 0 {
                    if self.y < self.cells.len() && self.x != 0 {
                        let mut px = self.x - 1;
                        while px != 0 && self.cells[self.y][px].is_empty() {
                            px -= 1;
                        }
                        self.cells[self.y][px].push(ch);
                    }
                    continue;
                }
                if self.y >= self.cells.len() || self.x >= self.cells[self.y].len() {
                    continue;
                }
                self.cells[self.y][self.x] = ch.to_string();
                if width == 2 && self.x + 1 < self.cells[self.y].len() {
                    self.cells[self.y][self.x + 1].clear();
                }
                self.x += width;
            }
        }

        fn assert_matches(&self, grid: &Grid) {
            for y in 0..grid.height() {
                for x in 0..grid.width() {
                    let cell = grid.get(x, y);
                    let expected = grid.cell_text_owned(cell);
                    assert_eq!(
                        self.cells[y as usize][x as usize], expected,
                        "render mismatch at ({x},{y})"
                    );
                }
            }
        }
    }

    fn plain(ch: char) -> Cell {
        Cell {
            ch,
            ..Cell::default()
        }
    }

    #[test]
    fn clean_grid_emits_nothing() {
        // The budget property: no damage => zero bytes.
        let grid = Grid::new(80, 24);
        let mut r = Renderer::new();
        let mut out = Vec::new();
        assert_eq!(r.render(&grid, &mut out), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn single_cell_change_is_small() {
        let mut grid = Grid::new(80, 24);
        grid.set(5, 2, plain('A'));
        let mut r = Renderer::new();
        let mut out = Vec::new();
        let n = r.render(&grid, &mut out);
        // A cursor move plus an SGR reset plus one glyph - far less than a
        // full-screen redraw of 80*24 cells.
        assert!(n > 0 && n < 32, "expected a tiny diff, got {n} bytes");
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains('A'));
        assert!(s.contains("\x1b[3;6H")); // row 3, col 6 (1-based)
    }

    #[test]
    fn contiguous_run_uses_one_cursor_move() {
        let mut grid = Grid::new(80, 24);
        for (i, ch) in "hello".chars().enumerate() {
            grid.set(i as u16, 0, plain(ch));
        }
        let mut r = Renderer::new();
        let mut out = Vec::new();
        r.render(&grid, &mut out);
        let s = String::from_utf8(out).unwrap();
        // Exactly one cursor-position sequence for the whole contiguous run.
        assert_eq!(s.matches("\x1b[1;1H").count(), 1);
        assert!(s.contains("hello"));
    }

    #[test]
    fn full_width_scroll_uses_margins_and_only_repaints_the_exposed_row() {
        let mut grid = Grid::new(120, 30);
        for (x, ch) in "new bottom".chars().enumerate() {
            grid.set(x as u16, 29, plain(ch));
        }
        grid.clear_damage();
        grid.scroll_up(Cell::default());
        for (x, ch) in "next prompt".chars().enumerate() {
            grid.set(x as u16, 29, plain(ch));
        }

        let mut renderer = Renderer::new();
        let mut optimized = Vec::new();
        renderer.render_pane_damage_with_scroll(&grid, 0, 0, &mut optimized);
        let optimized = String::from_utf8(optimized).unwrap();
        assert!(optimized.contains("\x1b[1;30r\x1b[1S\x1b[r"));
        assert!(optimized.contains("next prompt"));
        assert!(
            optimized.len() < 200,
            "scroll diff was {} bytes",
            optimized.len()
        );

        let mut fallback = Vec::new();
        Renderer::new().render_pane_damage(&grid, 0, 0, &mut fallback);
        assert!(fallback.len() > 3_000, "fallback unexpectedly small");
    }

    #[test]
    fn semantic_scroll_matches_the_grid_projection() {
        let mut grid = Grid::new(12, 4);
        for (row, ch) in ['A', 'B', 'C', 'D'].into_iter().enumerate() {
            for x in 0..12 {
                grid.set(x, row as u16, plain(ch));
            }
        }
        let mut renderer = Renderer::new();
        let mut oracle = Oracle::new(12, 4);
        let mut output = Vec::new();
        renderer.render_pane_full(&grid, 0, 0, &mut output);
        oracle.apply(&output);
        grid.clear_damage();

        grid.scroll_up(Cell::default());
        for x in 0..12 {
            grid.set(x, 3, plain('E'));
        }
        output.clear();
        renderer.render_pane_damage_with_scroll(&grid, 0, 0, &mut output);
        oracle.apply(&output);
        oracle.assert_matches(&grid);
    }

    #[test]
    fn parked_cursor_goes_through_the_cache() {
        // The server parks the visible cursor after every damage batch. If
        // that move bypassed the cache, a later run starting where a previous
        // run ended would be short-circuited to the parked position and paint
        // in the wrong place (the stale-cells-after-clear bug).
        let mut grid = Grid::new(80, 24);
        let mut r = Renderer::new();
        let mut out = Vec::new();
        grid.set(0, 5, plain('a'));
        r.render(&grid, &mut out);
        grid.clear_damage();
        // Park the cursor somewhere else, as the server does after a batch.
        r.place_cursor(0, 10, &mut out);
        out.clear();
        // The next run starts exactly where the previous one ended (1,5); the
        // renderer must know the cursor was parked away and emit a real move.
        grid.set(1, 5, plain('b'));
        r.render(&grid, &mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("\x1b[6;2H"),
            "expected an explicit cursor move, got {s:?}"
        );
        // Parking where the cursor already is stays free.
        let mut park = Vec::new();
        r.place_cursor(2, 5, &mut park);
        assert!(park.is_empty(), "redundant park should emit nothing");
    }

    #[test]
    fn truecolor_cells_emit_rgb_sgr() {
        let mut grid = Grid::new(20, 3);
        grid.set(
            0,
            0,
            Cell {
                ch: 'x',
                fg: Color::Rgb(10, 20, 30),
                bg: Color::Rgb(40, 50, 60),
                ..Cell::default()
            },
        );
        let mut r = Renderer::new();
        let mut out = Vec::new();
        r.render(&grid, &mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("38;2;10;20;30"), "fg rgb missing: {s:?}");
        assert!(s.contains("48;2;40;50;60"), "bg rgb missing: {s:?}");
    }

    #[test]
    fn extended_underlines_survive_terminal_to_host_rendering() {
        let mut terminal = crate::Terminal::new(20, 3);
        terminal.feed(b"\x1b[4:3;58:2::10:20:30mA\x1b[4:2;58;5;123mB");
        let mut renderer = Renderer::new();
        let mut output = Vec::new();
        renderer.render(terminal.grid(), &mut output);
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains("4:3;58;2;10;20;30"),
            "curly underline missing: {output:?}"
        );
        assert!(
            output.contains("4:2;58;5;123"),
            "double underline missing: {output:?}"
        );
    }

    #[test]
    fn shared_style_emits_sgr_once() {
        let mut grid = Grid::new(80, 24);
        let styled = Cell {
            ch: 'x',
            fg: Color::Idx(9),
            ..Cell::default()
        };
        for x in 0..3u16 {
            let mut c = styled;
            c.ch = (b'a' + x as u8) as char;
            grid.set(x, 0, c);
        }
        let mut r = Renderer::new();
        let mut out = Vec::new();
        r.render(&grid, &mut out);
        let s = String::from_utf8(out).unwrap();
        // The three cells share a style, so the SGR (with 38;5;9) appears once.
        assert_eq!(s.matches("38;5;9").count(), 1);
    }

    #[test]
    fn osc8_links_are_forwarded_and_closed_around_linked_cells() {
        let mut grid = Grid::new(20, 2);
        let hyperlink = grid.intern_hyperlink("https://example.com/docs");
        for (x, ch) in "docs".chars().enumerate() {
            grid.set(
                x as u16,
                0,
                Cell {
                    ch,
                    hyperlink,
                    ..Cell::default()
                },
            );
        }
        let mut renderer = Renderer::new();
        let mut output = Vec::new();
        renderer.render(&grid, &mut output);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\x1b]8;;https://example.com/docs\x1b\\"));
        assert!(output.contains("docs"));
        assert!(output.ends_with("\x1b]8;;\x1b\\"));
    }

    #[test]
    fn full_and_incremental_output_match_an_independent_screen_oracle() {
        let mut terminal = crate::Terminal::new(12, 3);
        terminal.feed("A界e\u{301} tail".as_bytes());
        terminal.feed(b"\x1b[2;3H\x1b[1;38;2;4;5;6mXYZ\x1b[0m");

        let mut renderer = Renderer::new();
        let mut oracle = Oracle::new(12, 3);
        let mut full = Vec::new();
        renderer.render_full(terminal.grid(), &mut full);
        oracle.apply(&full);
        oracle.assert_matches(terminal.grid());
        terminal.grid_mut().clear_damage();

        terminal.feed(b"\x1b[1;2H\x1b[3XQ\x1b[2;4H\x1b[P");
        let mut incremental = Vec::new();
        renderer.render(terminal.grid(), &mut incremental);
        oracle.apply(&incremental);
        oracle.assert_matches(terminal.grid());
    }

    #[test]
    fn right_margin_shell_erase_is_visible_in_the_incremental_frame() {
        let mut terminal = crate::Terminal::new(5, 2);
        terminal.feed(b"ABCDE");
        let mut renderer = Renderer::new();
        let mut oracle = Oracle::new(5, 2);
        let mut frame = Vec::new();
        renderer.render_full(terminal.grid(), &mut frame);
        oracle.apply(&frame);
        terminal.grid_mut().clear_damage();

        // Readline-style cleanup at the deferred-wrap margin must both paint
        // the erased cell and leave the next glyph on this row.
        terminal.feed(b"\x1b[X");
        frame.clear();
        renderer.render(terminal.grid(), &mut frame);
        assert!(frame.ends_with(b" "), "erase diff omitted its blank cell");
        oracle.apply(&frame);
        oracle.assert_matches(terminal.grid());
        terminal.grid_mut().clear_damage();

        terminal.feed(b"X");
        frame.clear();
        renderer.render(terminal.grid(), &mut frame);
        oracle.apply(&frame);
        oracle.assert_matches(terminal.grid());
        assert_eq!(terminal.grid().get(4, 0).ch, 'X');
        assert_eq!(terminal.grid().get(0, 1).ch, ' ');
    }

    #[test]
    fn damage_touching_a_wide_half_repaints_the_whole_glyph_span() {
        let mut grid = Grid::new(6, 1);
        grid.set_grapheme(2, 0, "界", plain('界'), 2);
        grid.clear_damage();
        grid.clear_glyph_at(3, 0, Cell::default());
        let mut renderer = Renderer::new();
        let mut out = Vec::new();
        renderer.render(&grid, &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\x1b[1;3H"));
        assert!(text.ends_with("  "));
    }

    #[test]
    fn adversarial_4k_viewport_fits_the_wire_frame_budget() {
        // A 4K display at an 8x16 cell size is 480x135. Alternate true-colour
        // cells defeat style-run compression and approximate the largest
        // realistic full repaint that an application can force.
        let mut grid = Grid::new(480, 135);
        for y in 0..grid.height() {
            for x in 0..grid.width() {
                grid.set(
                    x,
                    y,
                    Cell {
                        ch: if (x + y) % 2 == 0 { 'X' } else { 'Y' },
                        fg: Color::Rgb(x as u8, y as u8, (x ^ y) as u8),
                        ..Cell::default()
                    },
                );
            }
        }
        let mut output = Vec::new();
        Renderer::new().render_full(&grid, &mut output);
        let encoded = uniterm_proto::encode_frame(&uniterm_proto::ServerMessage::RenderOps(output));
        eprintln!("adversarial 480x135 repaint: {} wire bytes", encoded.len());
        assert!(
            encoded.len() <= uniterm_proto::MAX_SERVER_FRAME as usize + 4,
            "4K full repaint is {} bytes, above the wire limit",
            encoded.len()
        );
    }
}
