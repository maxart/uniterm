//! Terminal grid, scrollback, grapheme storage, and damage tracking.
//!
//! Cells stay small and `Copy`: ordinary single-codepoint glyphs live directly
//! in the cell, while multi-codepoint graphemes use a handle into a per-grid
//! arena. Wide glyphs own a lead cell plus an explicit continuation cell. This
//! keeps renderer cursor accounting exact and makes erase/edit operations able
//! to invalidate both halves of a wide glyph.

use std::collections::{HashMap, HashSet, VecDeque};

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

/// A cell colour: the terminal default, an xterm-256 palette index, or 24-bit
/// true colour.
#[derive(
    Clone, Copy, PartialEq, Eq, Debug, Default, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Color {
    /// The terminal's own default foreground/background.
    #[default]
    Default,
    /// An xterm-256 palette index.
    Idx(u8),
    /// 24-bit RGB.
    Rgb(u8, u8, u8),
}

impl Color {
    pub const DEFAULT: Color = Color::Default;

    /// SGR parameters selecting this colour as foreground.
    pub fn sgr_fg(&self) -> String {
        match self {
            Color::Default => "39".into(),
            Color::Idx(n) => format!("38;5;{n}"),
            Color::Rgb(r, g, b) => format!("38;2;{r};{g};{b}"),
        }
    }

    /// SGR parameters selecting this colour as background.
    pub fn sgr_bg(&self) -> String {
        match self {
            Color::Default => "49".into(),
            Color::Idx(n) => format!("48;5;{n}"),
            Color::Rgb(r, g, b) => format!("48;2;{r};{g};{b}"),
        }
    }
}

/// Cell attributes packed into two bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Attrs(pub u16);

/// The underline shape selected by `SGR 4` or `SGR 4:<style>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum UnderlineStyle {
    /// No underline.
    #[default]
    None,
    /// A single straight underline (`SGR 4` or `SGR 4:1`).
    Single,
    /// A double underline (`SGR 4:2`).
    Double,
    /// A curly underline (`SGR 4:3`).
    Curly,
    /// A dotted underline (`SGR 4:4`).
    Dotted,
    /// A dashed underline (`SGR 4:5`).
    Dashed,
}

impl Attrs {
    pub const NONE: Attrs = Attrs(0);
    pub const BOLD: Attrs = Attrs(1 << 0);
    pub const UNDERLINE: Attrs = Attrs(1 << 1);
    pub const REVERSE: Attrs = Attrs(1 << 2);
    pub const DIM: Attrs = Attrs(1 << 3);
    pub const ITALIC: Attrs = Attrs(1 << 4);
    pub const BLINK: Attrs = Attrs(1 << 5);
    pub const INVISIBLE: Attrs = Attrs(1 << 6);
    pub const STRIKE: Attrs = Attrs(1 << 7);

    const UNDERLINE_STYLE_MASK: u16 = (1 << 1) | (1 << 8) | (1 << 9);

    /// Decode the extended underline style packed into the attribute bits.
    pub fn underline_style(self) -> UnderlineStyle {
        let encoded = ((self.0 >> 1) & 1) | ((self.0 >> 7) & 2) | ((self.0 >> 7) & 4);
        match encoded {
            1 => UnderlineStyle::Single,
            2 => UnderlineStyle::Double,
            3 => UnderlineStyle::Curly,
            4 => UnderlineStyle::Dotted,
            5 => UnderlineStyle::Dashed,
            _ => UnderlineStyle::None,
        }
    }

    /// Replace only the underline style, preserving every other attribute.
    pub fn set_underline_style(&mut self, style: UnderlineStyle) {
        let encoded = style as u16;
        self.0 &= !Self::UNDERLINE_STYLE_MASK;
        self.0 |= (encoded & 1) << 1;
        self.0 |= (encoded & 2) << 7;
        self.0 |= (encoded & 4) << 7;
    }
}

/// One compact terminal cell.
///
/// `width` is 1 or 2 on a glyph lead and 0 on a continuation. `cluster == 0`
/// means `ch` is the complete glyph; other values index the owning grid's
/// grapheme arena. Underline colours also use a compact grid-local handle.
/// Handles never cross grids without being resolved first.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    pub width: u8,
    /// Grid-local underline-colour handle, or zero for the terminal default.
    pub underline_color: u8,
    pub cluster: u32,
    /// Grid-local OSC 8 hyperlink handle, or zero for ordinary text.
    pub hyperlink: u32,
}

impl Cell {
    /// Whether this is the trailing cell of a wide grapheme.
    #[inline]
    pub fn is_continuation(self) -> bool {
        self.width == 0
    }

    /// Display width of a glyph lead, or zero for a continuation.
    #[inline]
    pub fn display_width(self) -> u8 {
        if self.is_continuation() {
            0
        } else {
            self.width.max(1)
        }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            fg: Color::DEFAULT,
            bg: Color::DEFAULT,
            attrs: Attrs::NONE,
            width: 1,
            underline_color: 0,
            cluster: 0,
            hyperlink: 0,
        }
    }
}

/// A self-contained persisted cell. Grapheme text is resolved rather than
/// serializing a grid-local arena handle.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoredCell {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    /// Resolved underline colour, independent of the source grid's arena.
    pub underline_color: Color,
    pub width: u8,
    pub continuation: bool,
}

/// One persisted physical line, including whether it soft-wraps into the next.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoredLine {
    pub cells: Vec<StoredCell>,
    pub wrapped: bool,
}

#[derive(Clone)]
struct Line {
    cells: Box<[Cell]>,
    /// This physical line continues into the next without a hard newline.
    wrapped: bool,
    /// Stable while the line remains retained, allowing copy-mode to compensate
    /// when old scrollback lines are evicted under live output.
    id: u64,
}

impl Line {
    fn blank(width: u16, cell: Cell, id: u64) -> Self {
        Line {
            cells: vec![cell; width as usize].into_boxed_slice(),
            wrapped: false,
            id,
        }
    }

    fn trim_default_tail(&mut self) {
        if self.wrapped {
            return;
        }
        let keep = self
            .cells
            .iter()
            .rposition(|cell| *cell != Cell::default())
            .map_or(0, |index| index + 1);
        if keep < self.cells.len() {
            let mut cells = std::mem::take(&mut self.cells).into_vec();
            cells.truncate(keep);
            self.cells = cells.into_boxed_slice();
        }
    }
}

/// Per-row damage: the inclusive column range changed since the last render.
#[derive(Clone, Copy, Debug)]
struct RowDamage {
    dirty: bool,
    lo: u16,
    hi: u16,
}

impl RowDamage {
    const fn clean() -> Self {
        RowDamage {
            dirty: false,
            lo: 0,
            hi: 0,
        }
    }
}

/// A terminal grid with a bounded scrollback ring and exact damage tracking.
pub struct Grid {
    width: u16,
    height: u16,
    lines: Vec<Line>,
    damage: Vec<RowDamage>,
    any_damage: bool,
    /// Whole-grid upward scrolls not yet reflected in attached clients.
    pending_scroll_up: u16,
    scrollback: VecDeque<Line>,
    scrollback_limit: usize,
    clusters: Vec<String>,
    cluster_gc_at: usize,
    hyperlinks: HashMap<u32, String>,
    hyperlink_gc_at: usize,
    next_hyperlink: u32,
    underline_colors: Vec<Color>,
    next_line_id: u64,
}

impl Grid {
    pub fn new(width: u16, height: u16) -> Self {
        let mut next_line_id = 1;
        let lines = (0..height)
            .map(|_| {
                let line = Line::blank(width, Cell::default(), next_line_id);
                next_line_id += 1;
                line
            })
            .collect();
        Grid {
            width,
            height,
            lines,
            damage: vec![RowDamage::clean(); height as usize],
            any_damage: false,
            pending_scroll_up: 0,
            scrollback: VecDeque::new(),
            scrollback_limit: 10_000,
            clusters: vec![String::new()],
            cluster_gc_at: 4096,
            hyperlinks: HashMap::new(),
            hyperlink_gc_at: 4096,
            next_hyperlink: 1,
            underline_colors: vec![Color::Default],
            next_line_id,
        }
    }

    pub fn set_scrollback_limit(&mut self, limit: usize) {
        self.scrollback_limit = limit;
        while self.scrollback.len() > limit {
            self.scrollback.pop_front();
        }
    }

    /// Remove all saved lines, implementing ED 3 without touching the viewport.
    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
    }

    #[inline]
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    #[inline]
    pub fn total_lines(&self) -> usize {
        self.scrollback.len() + self.height as usize
    }

    /// Actual cells retained across history and the live screen.
    ///
    /// Hard-newline history may omit terminal-default trailing cells, so this
    /// diagnostic is more meaningful than `total_lines * width` for resource
    /// budget checks.
    pub fn retained_cell_count(&self) -> usize {
        self.scrollback
            .iter()
            .chain(&self.lines)
            .map(|line| line.cells.len())
            .sum()
    }

    #[inline]
    pub fn width(&self) -> u16 {
        self.width
    }

    #[inline]
    pub fn height(&self) -> u16 {
        self.height
    }

    /// Stable id of the oldest retained line, used by copy-mode during eviction.
    pub fn first_line_id(&self) -> Option<u64> {
        self.scrollback
            .front()
            .or_else(|| self.lines.first())
            .map(|line| line.id)
    }

    /// Stable id of a combined history+screen line.
    pub fn line_id(&self, line: usize) -> Option<u64> {
        self.line(line).map(|line| line.id)
    }

    /// Whether a physical line soft-wraps into the following line.
    pub fn line_wrapped(&self, line: usize) -> bool {
        self.line(line).is_some_and(|line| line.wrapped)
    }

    /// Set the visible row's soft-wrap flag.
    pub fn set_row_wrapped(&mut self, y: u16, wrapped: bool) {
        if let Some(line) = self.lines.get_mut(y as usize) {
            line.wrapped = wrapped;
        }
    }

    fn line(&self, line: usize) -> Option<&Line> {
        if line < self.scrollback.len() {
            self.scrollback.get(line)
        } else {
            self.lines.get(line - self.scrollback.len())
        }
    }

    pub fn line_cell(&self, line: usize, x: u16) -> Cell {
        self.line(line)
            .and_then(|line| line.cells.get(x as usize))
            .copied()
            .unwrap_or_default()
    }

    #[inline]
    pub fn get(&self, x: u16, y: u16) -> Cell {
        self.lines[y as usize].cells[x as usize]
    }

    /// Append the resolved grapheme bytes to a buffer without allocating for
    /// the overwhelmingly common single-codepoint case.
    pub fn write_cell_text(&self, cell: Cell, out: &mut Vec<u8>) {
        if cell.is_continuation() {
            return;
        }
        if cell.cluster != 0 {
            if let Some(text) = self.clusters.get(cell.cluster as usize) {
                out.extend_from_slice(text.as_bytes());
            }
        } else {
            let mut bytes = [0; 4];
            out.extend_from_slice(cell.ch.encode_utf8(&mut bytes).as_bytes());
        }
    }

    /// Append text safe for the cell's modeled display width. A wide grapheme
    /// retained through a one-column resize is rendered as a placeholder until
    /// the pane widens again, while its original text remains available for
    /// reflow, persistence, and copy-mode extraction.
    pub fn write_cell_display_text(&self, cell: Cell, out: &mut Vec<u8>) {
        if cell.is_continuation() {
            return;
        }
        if cell.cluster != 0 {
            let text = self
                .clusters
                .get(cell.cluster as usize)
                .map(String::as_str)
                .unwrap_or("�");
            if cell.width == 1 && text.width() > 1 {
                out.extend_from_slice("�".as_bytes());
            } else {
                out.extend_from_slice(text.as_bytes());
            }
        } else if cell.width == 1 && cell.ch.width().unwrap_or(0) > 1 {
            out.extend_from_slice("�".as_bytes());
        } else {
            let mut bytes = [0; 4];
            out.extend_from_slice(cell.ch.encode_utf8(&mut bytes).as_bytes());
        }
    }

    /// Resolve a cell to owned text for persistence, copy-mode, or tests.
    pub fn cell_text_owned(&self, cell: Cell) -> String {
        if cell.is_continuation() {
            String::new()
        } else if cell.cluster != 0 {
            self.clusters
                .get(cell.cluster as usize)
                .cloned()
                .unwrap_or_else(|| "�".into())
        } else {
            cell.ch.to_string()
        }
    }

    /// Intern an OSC 8 target without allocating per linked cell.
    pub fn intern_hyperlink(&mut self, uri: &str) -> u32 {
        if uri.is_empty() || uri.len() > 8192 || uri.chars().any(char::is_control) {
            return 0;
        }
        if let Some((handle, _)) = self
            .hyperlinks
            .iter()
            .find(|(_, value)| value.as_str() == uri)
        {
            return *handle;
        }
        if self.hyperlinks.len() >= self.hyperlink_gc_at {
            self.compact_hyperlinks();
        }
        let handle = loop {
            let candidate = self.next_hyperlink.max(1);
            self.next_hyperlink = candidate.wrapping_add(1).max(1);
            if !self.hyperlinks.contains_key(&candidate) {
                break candidate;
            }
        };
        self.hyperlinks.insert(handle, uri.into());
        handle
    }

    /// Resolve a grid-local OSC 8 handle for rendering or click activation.
    pub fn hyperlink(&self, handle: u32) -> Option<&str> {
        (handle != 0)
            .then(|| self.hyperlinks.get(&handle).map(String::as_str))
            .flatten()
    }

    /// Intern an underline colour without increasing every cell beyond the
    /// 24-byte hot-path budget. Pathological screens with more than 255 live
    /// underline colours degrade new colours to the terminal default.
    pub fn intern_underline_color(&mut self, color: Color) -> u8 {
        if color == Color::Default {
            return 0;
        }
        if let Some(handle) = self
            .underline_colors
            .iter()
            .position(|entry| *entry == color)
        {
            return handle as u8;
        }
        if self.underline_colors.len() == u8::MAX as usize + 1 {
            self.compact_underline_colors();
            if let Some(handle) = self
                .underline_colors
                .iter()
                .position(|entry| *entry == color)
            {
                return handle as u8;
            }
        }
        if self.underline_colors.len() == u8::MAX as usize + 1 {
            return 0;
        }
        self.underline_colors.push(color);
        (self.underline_colors.len() - 1) as u8
    }

    /// Resolve one cell's grid-local underline colour handle.
    pub fn underline_color(&self, handle: u8) -> Color {
        self.underline_colors
            .get(handle as usize)
            .copied()
            .unwrap_or_default()
    }

    fn compact_underline_colors(&mut self) {
        let old = std::mem::replace(&mut self.underline_colors, vec![Color::Default]);
        let mut handles = HashMap::new();
        for line in self.scrollback.iter_mut().chain(self.lines.iter_mut()) {
            for cell in &mut line.cells {
                if cell.underline_color == 0 {
                    continue;
                }
                let Some(color) = old.get(cell.underline_color as usize).copied() else {
                    cell.underline_color = 0;
                    continue;
                };
                let handle = *handles.entry(color).or_insert_with(|| {
                    self.underline_colors.push(color);
                    (self.underline_colors.len() - 1) as u8
                });
                cell.underline_color = handle;
            }
        }
    }

    fn compact_hyperlinks(&mut self) {
        let used: HashSet<u32> = self
            .scrollback
            .iter()
            .chain(&self.lines)
            .flat_map(|line| line.cells.iter().map(|cell| cell.hyperlink))
            .filter(|handle| *handle != 0)
            .collect();
        self.hyperlinks.retain(|handle, _| used.contains(handle));
        self.hyperlink_gc_at = self.hyperlinks.len().saturating_mul(2).max(4096);
    }

    fn intern(&mut self, text: &str) -> u32 {
        if let Some((idx, _)) = self
            .clusters
            .iter()
            .enumerate()
            .rev()
            .take(64)
            .find(|(_, existing)| existing.as_str() == text)
        {
            return idx as u32;
        }
        if self.clusters.len() >= self.cluster_gc_at {
            self.compact_clusters();
        }
        self.clusters.push(text.to_string());
        (self.clusters.len() - 1) as u32
    }

    fn compact_clusters(&mut self) {
        let old = std::mem::replace(&mut self.clusters, vec![String::new()]);
        let mut handles: HashMap<String, u32> = HashMap::new();
        for line in self.scrollback.iter_mut().chain(self.lines.iter_mut()) {
            for cell in &mut line.cells {
                if cell.cluster == 0 {
                    continue;
                }
                let Some(text) = old.get(cell.cluster as usize) else {
                    cell.cluster = 0;
                    cell.ch = '�';
                    continue;
                };
                let handle = if let Some(handle) = handles.get(text) {
                    *handle
                } else {
                    self.clusters.push(text.clone());
                    let handle = (self.clusters.len() - 1) as u32;
                    handles.insert(text.clone(), handle);
                    handle
                };
                cell.cluster = handle;
            }
        }
        self.cluster_gc_at = self.clusters.len().saturating_mul(2).max(4096);
    }

    fn mark_damage(&mut self, x: u16, y: u16) {
        let row = &mut self.damage[y as usize];
        if row.dirty {
            row.lo = row.lo.min(x);
            row.hi = row.hi.max(x);
        } else {
            row.dirty = true;
            row.lo = x;
            row.hi = x;
        }
        self.any_damage = true;
    }

    /// Low-level cell assignment for moves within this same grid.
    pub fn set(&mut self, x: u16, y: u16, cell: Cell) {
        if x >= self.width || y >= self.height {
            return;
        }
        if self.lines[y as usize].cells[x as usize] == cell {
            return;
        }
        self.lines[y as usize].cells[x as usize] = cell;
        self.mark_damage(x, y);
    }

    /// Clear a cell and the other half of any wide glyph occupying it.
    pub fn clear_glyph_at(&mut self, x: u16, y: u16, blank: Cell) {
        if x >= self.width || y >= self.height {
            return;
        }
        let cell = self.get(x, y);
        if cell.is_continuation() && x > 0 {
            self.set(x - 1, y, blank);
            self.set(x, y, blank);
        } else {
            self.set(x, y, blank);
            if cell.width == 2 && x + 1 < self.width {
                self.set(x + 1, y, blank);
            }
        }
    }

    /// Erase a half-open range, expanding across either half of a wide glyph.
    pub fn erase_cells(&mut self, y: u16, from: u16, to: u16, blank: Cell) {
        if y >= self.height {
            return;
        }
        let mut x = from.min(self.width);
        let to = to.min(self.width);
        while x < to {
            let width = self.get(x, y).display_width().max(1) as u16;
            self.clear_glyph_at(x, y, blank);
            x += width;
        }
    }

    /// Delete display columns and repair any wide glyph cut by the boundary.
    pub fn delete_cells(&mut self, y: u16, from: u16, count: u16, blank: Cell) {
        if y >= self.height || from >= self.width || count == 0 {
            return;
        }
        let count = count.min(self.width - from);
        for x in from..self.width {
            let source = x + count;
            let cell = if source < self.width {
                self.get(source, y)
            } else {
                blank
            };
            self.set(x, y, cell);
        }
        self.repair_wide_row(y, blank);
    }

    /// Insert display columns and repair any wide glyph cut by the boundary.
    pub fn insert_cells(&mut self, y: u16, from: u16, count: u16, blank: Cell) {
        if y >= self.height || from >= self.width || count == 0 {
            return;
        }
        let count = count.min(self.width - from);
        let mut x = self.width;
        while x > from {
            x -= 1;
            let cell = if x >= from + count {
                self.get(x - count, y)
            } else {
                blank
            };
            self.set(x, y, cell);
        }
        self.repair_wide_row(y, blank);
    }

    fn repair_wide_row(&mut self, y: u16, blank: Cell) {
        for x in 0..self.width {
            let cell = self.get(x, y);
            if cell.is_continuation() {
                let valid = x > 0 && self.get(x - 1, y).width == 2;
                if !valid {
                    self.set(x, y, blank);
                }
            } else if cell.width == 2 {
                let valid = x + 1 < self.width && self.get(x + 1, y).is_continuation();
                if !valid {
                    self.set(x, y, blank);
                }
            }
        }
    }

    /// Write one complete grapheme with a display width of one or two cells.
    pub fn set_grapheme(&mut self, x: u16, y: u16, text: &str, mut cell: Cell, width: u8) {
        if x >= self.width || y >= self.height {
            return;
        }
        let width = width.clamp(1, 2);
        if width == 2 && x + 1 >= self.width {
            return;
        }
        let blank = Cell {
            fg: cell.fg,
            bg: cell.bg,
            ..Cell::default()
        };
        self.clear_glyph_at(x, y, blank);
        if width == 2 {
            self.clear_glyph_at(x + 1, y, blank);
        }
        let mut chars = text.chars();
        cell.ch = chars.next().unwrap_or(' ');
        cell.width = width;
        cell.cluster = if chars.next().is_some() {
            self.intern(text)
        } else {
            0
        };
        self.set(x, y, cell);
        if width == 2 {
            self.set(
                x + 1,
                y,
                Cell {
                    ch: ' ',
                    fg: cell.fg,
                    bg: cell.bg,
                    attrs: cell.attrs,
                    width: 0,
                    underline_color: cell.underline_color,
                    cluster: 0,
                    hyperlink: cell.hyperlink,
                },
            );
        }
    }

    /// Whether appending `ch` to this lead cell remains a single grapheme.
    pub fn grapheme_extends(&self, cell: Cell, ch: char) -> bool {
        if cell.is_continuation() {
            return false;
        }
        let mut text = self.cell_text_owned(cell);
        text.push(ch);
        text.graphemes(true).count() == 1
    }

    /// Extend a previously written grapheme and repair its continuation width.
    pub fn append_to_grapheme(&mut self, x: u16, y: u16, ch: char) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        let mut lead_x = x;
        let mut cell = self.get(lead_x, y);
        if cell.is_continuation() && lead_x > 0 {
            lead_x -= 1;
            cell = self.get(lead_x, y);
        }
        if !self.grapheme_extends(cell, ch) {
            return false;
        }
        let mut text = self.cell_text_owned(cell);
        text.push(ch);
        let width = text.as_str().width().clamp(1, 2) as u8;
        self.set_grapheme(lead_x, y, &text, cell, width);
        true
    }

    /// Scroll the whole screen up, preserving the departing line in history.
    pub fn scroll_up(&mut self, blank: Cell) {
        if self.height == 0 {
            return;
        }
        let replacement = Line::blank(self.width, blank, self.next_line_id);
        self.next_line_id += 1;
        let mut departing = self.lines.remove(0);
        departing.trim_default_tail();
        self.push_scrollback(departing);
        self.lines.push(replacement);
        self.damage.rotate_left(1);
        if let Some(row) = self.damage.last_mut() {
            *row = RowDamage::clean();
        }
        self.pending_scroll_up = self.pending_scroll_up.saturating_add(1).min(self.height);
        self.mark_rows_damaged(self.height - 1, self.height - 1);
    }

    /// Scroll a vertical region upward. A region anchored at the screen top
    /// preserves its departing line in history, matching DECSTBM behavior used
    /// by inline terminal applications.
    pub fn scroll_region_up(&mut self, top: u16, bottom: u16, blank: Cell) {
        self.scroll_region_up_inner(top, bottom, blank, top == 0);
    }

    /// Scroll a vertical region upward without preserving the departing line.
    ///
    /// Terminal line-editing commands use this path because deleting a line is
    /// a viewport edit, even when the edit begins at the top of the screen.
    pub fn scroll_region_up_discarding(&mut self, top: u16, bottom: u16, blank: Cell) {
        self.scroll_region_up_inner(top, bottom, blank, false);
    }

    fn scroll_region_up_inner(
        &mut self,
        top: u16,
        bottom: u16,
        blank: Cell,
        preserve_history: bool,
    ) {
        if top >= self.height || bottom >= self.height || top > bottom {
            return;
        }
        if preserve_history && top == 0 && bottom == self.height - 1 {
            self.scroll_up(blank);
            return;
        }
        let departing = self.lines.remove(top as usize);
        if preserve_history {
            self.push_scrollback(departing);
        }
        let id = self.alloc_line_id();
        self.lines
            .insert(bottom as usize, Line::blank(self.width, blank, id));
        self.pending_scroll_up = 0;
        self.mark_rows_damaged(top, bottom);
    }

    fn push_scrollback(&mut self, departing: Line) {
        if self.scrollback_limit == 0 {
            return;
        }
        self.scrollback.push_back(departing);
        while self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
        }
    }

    /// Scroll a vertical region downward, blanking its top row.
    pub fn scroll_region_down(&mut self, top: u16, bottom: u16, blank: Cell) {
        if top >= self.height || bottom >= self.height || top > bottom {
            return;
        }
        self.lines.remove(bottom as usize);
        let id = self.alloc_line_id();
        self.lines
            .insert(top as usize, Line::blank(self.width, blank, id));
        self.pending_scroll_up = 0;
        self.mark_rows_damaged(top, bottom);
    }

    fn mark_rows_damaged(&mut self, top: u16, bottom: u16) {
        if self.width == 0 {
            return;
        }
        for y in top..=bottom {
            let row = &mut self.damage[y as usize];
            row.dirty = true;
            row.lo = 0;
            row.hi = self.width - 1;
        }
        self.any_damage = true;
    }

    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.any_damage
    }

    #[inline]
    pub fn row_damage(&self, y: u16) -> Option<(u16, u16)> {
        let row = self.damage[y as usize];
        row.dirty.then_some((row.lo, row.hi))
    }

    /// Number of whole-grid upward scrolls pending in the damage projection.
    /// Renderers that cannot safely express a pane-local scroll must repaint
    /// the full pane instead.
    #[inline]
    pub fn pending_scroll_up(&self) -> u16 {
        self.pending_scroll_up
    }

    /// Export recent physical lines with resolved graphemes and wrap metadata.
    pub fn export_lines(&self, max: usize) -> Vec<StoredLine> {
        let start = self.total_lines().saturating_sub(max);
        (start..self.total_lines())
            .map(|line| self.store_line(line))
            .collect()
    }

    fn store_line(&self, line: usize) -> StoredLine {
        let source = self.line(line);
        let wrapped = source.is_some_and(|line| line.wrapped);
        let mut cells: Vec<StoredCell> = (0..self.width)
            .map(|x| {
                let cell = self.line_cell(line, x);
                StoredCell {
                    text: self.cell_text_owned(cell),
                    fg: cell.fg,
                    bg: cell.bg,
                    attrs: cell.attrs,
                    underline_color: self.underline_color(cell.underline_color),
                    width: cell.width,
                    continuation: cell.is_continuation(),
                }
            })
            .collect();
        if !wrapped {
            while cells.last().is_some_and(stored_cell_is_default_blank) {
                cells.pop();
            }
        }
        StoredLine { cells, wrapped }
    }

    /// Resize the normal terminal buffer with logical-line reflow. Historical
    /// and visible soft-wrapped rows are joined, then wrapped to the new width.
    /// A taller viewport extends below the live screen instead of pulling
    /// history above it, matching tmux and keeping a fresh prompt in place.
    /// Returns the cursor mapped into the resized viewport.
    pub fn resize_reflow(
        &mut self,
        width: u16,
        height: u16,
        blank: Cell,
        cursor: (u16, u16),
        cursor_pending: bool,
    ) -> (u16, u16, bool) {
        let (width, height) = (width.max(1), height.max(1));
        if width == self.width && height == self.height {
            return (cursor.0, cursor.1, cursor_pending);
        }

        // Grow vertically before doing any width reflow. This preserves the
        // live viewport's top row and cursor position. A combined width and
        // height change must go through reflow below so the added height can
        // hold newly wrapped rows instead of becoming premature blank lines.
        if height > self.height && width == self.width {
            while self.lines.len() < height as usize {
                let line_id = self.alloc_line_id();
                self.lines.push(Line::blank(width, blank, line_id));
            }
            self.height = height;
            self.damage = vec![RowDamage::clean(); height as usize];
            self.mark_all_damaged();
            let cursor_x = if cursor_pending {
                cursor.0.saturating_sub(1)
            } else {
                cursor.0
            };
            return (
                cursor_x.min(width - 1),
                cursor.1.min(height - 1),
                cursor_pending,
            );
        }

        let old_cursor_line = self.scrollback.len() + cursor.1 as usize;
        let old_screen_line = self.scrollback.len();
        let stored: Vec<StoredLine> = (0..self.total_lines())
            .map(|line| self.store_line(line))
            .collect();
        let blank_saved = StoredCell {
            text: " ".into(),
            fg: blank.fg,
            bg: blank.bg,
            attrs: blank.attrs,
            underline_color: self.underline_color(blank.underline_color),
            width: 1,
            continuation: false,
        };

        let mut logical: Vec<Vec<StoredCell>> = Vec::new();
        let mut current = Vec::new();
        let mut current_width = 0usize;
        let mut cursor_logical = 0usize;
        let mut cursor_offset = 0usize;
        let mut screen_logical = 0usize;
        let mut screen_offset = 0usize;
        for (line_idx, line) in stored.iter().enumerate() {
            if line_idx == old_cursor_line {
                cursor_logical = logical.len();
                cursor_offset = current_width + cursor.0 as usize;
            }
            if line_idx == old_screen_line {
                screen_logical = logical.len();
                screen_offset = current_width;
            }
            let mut leads: Vec<StoredCell> = line
                .cells
                .iter()
                .filter(|cell| !cell.continuation)
                .cloned()
                .collect();
            if !line.wrapped {
                while leads.last().is_some_and(stored_cell_is_default_blank) {
                    leads.pop();
                }
            }
            current_width += leads
                .iter()
                .map(|cell| cell.width.max(1) as usize)
                .sum::<usize>();
            current.extend(leads);
            if !line.wrapped {
                logical.push(std::mem::take(&mut current));
                current_width = 0;
            }
        }
        if !current.is_empty() {
            logical.push(current);
        }
        if logical.is_empty() {
            logical.push(Vec::new());
        }

        let mut physical = Vec::new();
        let mut logical_starts = Vec::with_capacity(logical.len());
        let mut logical_counts = Vec::with_capacity(logical.len());
        for glyphs in &logical {
            logical_starts.push(physical.len());
            let before = physical.len();
            let mut row = stored_blank_line(width, &blank_saved);
            let mut x = 0u16;
            for glyph in glyphs {
                let natural_width = glyph.text.as_str().width().clamp(1, 2) as u16;
                let glyph_width = natural_width.min(width);
                if x > 0 && x + glyph_width > width {
                    row.wrapped = true;
                    physical.push(row);
                    row = stored_blank_line(width, &blank_saved);
                    x = 0;
                }
                row.cells[x as usize] = glyph.clone();
                row.cells[x as usize].width = glyph_width as u8;
                row.cells[x as usize].continuation = false;
                if glyph_width == 2 {
                    row.cells[x as usize + 1] = StoredCell {
                        text: String::new(),
                        fg: glyph.fg,
                        bg: glyph.bg,
                        attrs: glyph.attrs,
                        underline_color: glyph.underline_color,
                        width: 0,
                        continuation: true,
                    };
                }
                x += glyph_width;
            }
            physical.push(row);
            logical_counts.push(physical.len() - before);
        }

        let cursor_logical = cursor_logical.min(logical_starts.len() - 1);
        let logical_count = logical_counts[cursor_logical].max(1);
        let mut cursor_row_in_logical = (cursor_offset / width as usize).min(logical_count - 1);
        let mut cursor_global = logical_starts[cursor_logical] + cursor_row_in_logical;
        let mut cursor_x = (cursor_offset % width as usize) as u16;
        let mut mapped_pending = false;
        if cursor_pending && cursor_x == 0 && cursor_offset != 0 && cursor_row_in_logical != 0 {
            cursor_row_in_logical -= 1;
            cursor_global = logical_starts[cursor_logical] + cursor_row_in_logical;
            cursor_x = width - 1;
            mapped_pending = true;
        }

        let screen_logical = screen_logical.min(logical_starts.len() - 1);
        let reflowed_screen_start = logical_starts[screen_logical]
            + (screen_offset / width as usize).min(logical_counts[screen_logical] - 1);
        let screen_start = if height > self.height {
            // Preserve the former viewport top across growth. If narrower
            // reflow puts the cursor below the enlarged viewport, advance only
            // far enough to keep the cursor visible.
            let cursor_start = cursor_global
                .saturating_add(1)
                .saturating_sub(height as usize);
            let start = reflowed_screen_start.max(cursor_start);
            while physical.len() < start + height as usize {
                physical.push(stored_blank_line(width, &blank_saved));
            }
            start
        } else {
            while physical.len() < height as usize {
                physical.insert(0, stored_blank_line(width, &blank_saved));
                cursor_global += 1;
            }
            physical.len().saturating_sub(height as usize)
        };
        let history_start = screen_start.saturating_sub(self.scrollback_limit);
        cursor_global = cursor_global.saturating_sub(history_start);
        let retained: Vec<StoredLine> = physical.into_iter().skip(history_start).collect();
        let retained_screen_start = screen_start - history_start;

        let mut replacement = Grid::new(width, height);
        replacement.set_scrollback_limit(self.scrollback_limit);
        replacement.scrollback.clear();
        replacement.lines.clear();
        for (idx, stored) in retained.iter().enumerate() {
            let line = replacement.line_from_stored(stored);
            if idx < retained_screen_start {
                replacement.scrollback.push_back(line);
            } else {
                replacement.lines.push(line);
            }
        }
        while replacement.lines.len() < height as usize {
            let line_id = replacement.alloc_line_id();
            replacement
                .lines
                .insert(0, Line::blank(width, blank, line_id));
        }
        replacement.damage = vec![RowDamage::clean(); height as usize];
        replacement.mark_all_damaged();
        let cursor_y = cursor_global
            .saturating_sub(retained_screen_start)
            .min(height as usize - 1) as u16;
        *self = replacement;
        (cursor_x.min(width - 1), cursor_y, mapped_pending)
    }

    /// Load saved content into history, leaving the fresh viewport blank.
    pub fn load_scrollback(&mut self, lines: &[StoredLine]) {
        self.scrollback.clear();
        for stored in lines {
            let mut line = self.line_from_stored(stored);
            line.trim_default_tail();
            self.scrollback.push_back(line);
        }
        while self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
        }
    }

    fn alloc_line_id(&mut self) -> u64 {
        let id = self.next_line_id;
        self.next_line_id += 1;
        id
    }

    fn line_from_stored(&mut self, stored: &StoredLine) -> Line {
        let mut line = Line::blank(self.width, Cell::default(), self.alloc_line_id());
        line.wrapped = stored.wrapped;
        for (x, saved) in stored.cells.iter().take(self.width as usize).enumerate() {
            if saved.continuation {
                continue;
            }
            let cell = Cell {
                ch: saved.text.chars().next().unwrap_or(' '),
                fg: saved.fg,
                bg: saved.bg,
                attrs: saved.attrs,
                underline_color: self.intern_underline_color(saved.underline_color),
                width: saved.width.max(1),
                cluster: 0,
                hyperlink: 0,
            };
            self.set_stored_on_line(&mut line, x as u16, saved, cell);
        }
        line
    }

    fn set_stored_on_line(&mut self, line: &mut Line, x: u16, saved: &StoredCell, mut cell: Cell) {
        if x >= self.width || saved.text.is_empty() {
            return;
        }
        cell.cluster = if saved.text.chars().count() > 1 {
            self.intern(&saved.text)
        } else {
            0
        };
        line.cells[x as usize] = cell;
        if cell.width == 2 && x + 1 < self.width {
            line.cells[x as usize + 1] = Cell {
                ch: ' ',
                fg: cell.fg,
                bg: cell.bg,
                attrs: cell.attrs,
                underline_color: cell.underline_color,
                width: 0,
                cluster: 0,
                hyperlink: cell.hyperlink,
            };
        }
    }

    pub fn mark_all_damaged(&mut self) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        self.pending_scroll_up = 0;
        for row in &mut self.damage {
            row.dirty = true;
            row.lo = 0;
            row.hi = self.width - 1;
        }
        self.any_damage = true;
    }

    pub fn clear_damage(&mut self) {
        if self.any_damage {
            for row in &mut self.damage {
                row.dirty = false;
            }
            self.any_damage = false;
        }
        self.pending_scroll_up = 0;
    }
}

fn stored_cell_is_default_blank(cell: &StoredCell) -> bool {
    cell.text == " "
        && cell.fg == Color::Default
        && cell.bg == Color::Default
        && cell.attrs == Attrs::NONE
        && cell.underline_color == Color::Default
        && !cell.continuation
}

fn stored_blank_line(width: u16, blank: &StoredCell) -> StoredLine {
    StoredLine {
        cells: vec![blank.clone(); width as usize],
        wrapped: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_remain_compact_with_grapheme_and_hyperlink_handles() {
        assert!(
            std::mem::size_of::<Cell>() <= 24,
            "Cell grew to {} bytes",
            std::mem::size_of::<Cell>()
        );
    }

    #[test]
    fn underline_styles_pack_without_disturbing_other_attributes() {
        for style in [
            UnderlineStyle::None,
            UnderlineStyle::Single,
            UnderlineStyle::Double,
            UnderlineStyle::Curly,
            UnderlineStyle::Dotted,
            UnderlineStyle::Dashed,
        ] {
            let mut attrs = Attrs::BOLD;
            attrs.set_underline_style(style);
            assert_eq!(attrs.underline_style(), style);
            assert_ne!(attrs.0 & Attrs::BOLD.0, 0);
        }
    }

    #[test]
    fn underline_colours_are_interned_without_growing_cells() {
        let mut grid = Grid::new(2, 1);
        let red = grid.intern_underline_color(Color::Rgb(255, 0, 0));
        assert_ne!(red, 0);
        assert_eq!(grid.intern_underline_color(Color::Rgb(255, 0, 0)), red);
        assert_eq!(grid.underline_color(red), Color::Rgb(255, 0, 0));
        assert_eq!(grid.intern_underline_color(Color::Default), 0);
    }

    fn cell(ch: char) -> Cell {
        Cell {
            ch,
            ..Cell::default()
        }
    }

    #[test]
    fn damage_and_idle_contract() {
        let mut grid = Grid::new(10, 4);
        assert!(!grid.is_dirty());
        grid.set(3, 1, cell('x'));
        assert_eq!(grid.row_damage(1), Some((3, 3)));
        grid.clear_damage();
        grid.set(3, 1, cell('x'));
        assert!(!grid.is_dirty());
    }

    #[test]
    fn wide_grapheme_owns_a_continuation() {
        let mut grid = Grid::new(6, 2);
        grid.set_grapheme(1, 0, "界", cell('界'), 2);
        assert_eq!(grid.get(1, 0).display_width(), 2);
        assert!(grid.get(2, 0).is_continuation());
        grid.clear_glyph_at(2, 0, Cell::default());
        assert_eq!(grid.get(1, 0).ch, ' ');
        assert_eq!(grid.get(2, 0).ch, ' ');
    }

    #[test]
    fn combining_and_emoji_sequences_live_in_the_arena() {
        let mut grid = Grid::new(8, 2);
        grid.set_grapheme(0, 0, "e", cell('e'), 1);
        assert!(grid.append_to_grapheme(0, 0, '\u{301}'));
        assert_eq!(grid.cell_text_owned(grid.get(0, 0)), "e\u{301}");

        grid.set_grapheme(2, 0, "👩", cell('👩'), 2);
        assert!(grid.append_to_grapheme(2, 0, '\u{200d}'));
        assert!(grid.append_to_grapheme(2, 0, '💻'));
        assert_eq!(grid.cell_text_owned(grid.get(2, 0)), "👩\u{200d}💻");
        assert!(grid.get(3, 0).is_continuation());
    }

    #[test]
    fn scrollback_keeps_wrap_flags_and_stable_ids() {
        let mut grid = Grid::new(4, 2);
        let first = grid.line_id(0).unwrap();
        grid.set_row_wrapped(0, true);
        grid.scroll_up(Cell::default());
        assert_eq!(grid.line_id(0), Some(first));
        assert!(grid.line_wrapped(0));
        assert_eq!(grid.scrollback_len(), 1);
    }

    #[test]
    fn hard_newline_history_drops_unused_trailing_cells() {
        let mut grid = Grid::new(200, 2);
        for (x, ch) in "short log line".chars().enumerate() {
            grid.set(x as u16, 0, cell(ch));
        }
        grid.scroll_up(Cell::default());

        assert_eq!(grid.scrollback[0].cells.len(), "short log line".len());
        assert_eq!(grid.line_cell(0, 199), Cell::default());
        let exported = grid.export_lines(3);
        assert_eq!(exported[0].cells.len(), "short log line".len());
    }

    #[test]
    fn whole_grid_scroll_tracks_only_the_exposed_row_as_cell_damage() {
        let mut grid = Grid::new(80, 24);
        grid.clear_damage();
        grid.scroll_up(Cell::default());

        assert_eq!(grid.pending_scroll_up(), 1);
        assert_eq!(grid.row_damage(0), None);
        assert_eq!(grid.row_damage(23), Some((0, 79)));
        grid.clear_damage();
        assert_eq!(grid.pending_scroll_up(), 0);
    }

    #[test]
    fn top_anchored_scroll_region_preserves_history_and_lower_rows() {
        let mut grid = Grid::new(4, 4);
        for (row, ch) in ['A', 'B', 'C', 'D'].into_iter().enumerate() {
            grid.set(0, row as u16, cell(ch));
        }
        grid.clear_damage();

        grid.scroll_region_up(0, 2, Cell::default());

        assert_eq!(grid.scrollback_len(), 1);
        assert_eq!(grid.line_cell(0, 0).ch, 'A');
        assert_eq!(grid.get(0, 0).ch, 'B');
        assert_eq!(grid.get(0, 1).ch, 'C');
        assert_eq!(grid.get(0, 2).ch, ' ');
        assert_eq!(grid.get(0, 3).ch, 'D');
        assert_eq!(grid.row_damage(0), Some((0, 3)));
        assert_eq!(grid.row_damage(2), Some((0, 3)));
        assert_eq!(grid.row_damage(3), None);
    }

    #[test]
    fn persisted_graphemes_resolve_across_grids() {
        let mut source = Grid::new(6, 2);
        source.set_grapheme(0, 0, "e\u{301}", cell('e'), 1);
        source.set_row_wrapped(0, true);
        let lines = source.export_lines(2);
        let mut restored = Grid::new(6, 2);
        restored.load_scrollback(&lines);
        assert_eq!(
            restored.cell_text_owned(restored.line_cell(0, 0)),
            "e\u{301}"
        );
        assert!(restored.line_wrapped(0));
    }

    #[test]
    fn ed3_support_clears_only_history() {
        let mut grid = Grid::new(3, 1);
        grid.set(0, 0, cell('x'));
        grid.scroll_up(Cell::default());
        grid.set(0, 0, cell('y'));
        grid.clear_scrollback();
        assert_eq!(grid.scrollback_len(), 0);
        assert_eq!(grid.get(0, 0).ch, 'y');
    }

    #[test]
    fn growing_height_keeps_the_live_viewport_at_the_top() {
        for width in [8, 12] {
            let mut grid = Grid::new(8, 3);
            for _ in 0..31 {
                grid.set(0, 0, cell('h'));
                grid.scroll_up(Cell::default());
            }
            grid.set(0, 0, cell('$'));

            let cursor = grid.resize_reflow(width, 34, Cell::default(), (1, 0), false);

            assert_eq!(cursor, (1, 0, false), "target width {width}");
            assert_eq!(grid.get(0, 0).ch, '$', "target width {width}");
            assert_eq!(grid.scrollback_len(), 31, "target width {width}");
        }
    }

    #[test]
    fn grapheme_arena_compacts_unreferenced_overwrites() {
        let mut grid = Grid::new(2, 1);
        for idx in 0..5000u32 {
            let mark = char::from_u32(0x300 + idx % 0x300).unwrap();
            let text = format!("x{mark}{idx}");
            grid.set_grapheme(0, 0, &text, cell('x'), 1);
        }
        assert!(grid.clusters.len() < 2000, "arena did not compact");
        assert!(grid.cell_text_owned(grid.get(0, 0)).starts_with('x'));
    }

    #[test]
    fn one_column_reflow_preserves_wide_text_for_later_growth() {
        let mut grid = Grid::new(2, 1);
        grid.set_grapheme(0, 0, "界", cell('界'), 2);
        grid.resize_reflow(1, 1, Cell::default(), (2, 0), true);
        assert_eq!(grid.cell_text_owned(grid.get(0, 0)), "界");
        assert_eq!(grid.get(0, 0).width, 1);
        let mut display = Vec::new();
        grid.write_cell_display_text(grid.get(0, 0), &mut display);
        assert_eq!(String::from_utf8(display).unwrap(), "�");
        grid.resize_reflow(2, 1, Cell::default(), (1, 0), false);
        assert_eq!(grid.cell_text_owned(grid.get(0, 0)), "界");
        assert_eq!(grid.get(0, 0).width, 2);
        assert!(grid.get(1, 0).is_continuation());
    }
}
