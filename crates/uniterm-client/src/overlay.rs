//! Floating overlay windows (client-side compositing over the pane frame).
//!
//! A centered popup box with a title, a border, padded interior, and an ASCII
//! drop-shadow (offset one cell down-right), zellij/tmux-popup style. It is
//! drawn purely on the client on top of whatever the server sent, so pane
//! content on the server is untouched. This is the reusable substrate for New
//! Task (AG4), the Observatory (AG6), and task management (AG7).

/// A rectangle in 1-based terminal cell coordinates (as ANSI cursor moves use).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// A floating overlay: a title, content lines, and an optional action-bar
/// footer (keycap + caption pairs) drawn below a separator - the shared modal
/// structure, so every surface reads the same.
#[derive(Clone, Debug, Default)]
pub struct Overlay {
    pub title: String,
    pub lines: Vec<String>,
    pub footer: Vec<(String, String)>,
    row_styles: Vec<OverlayRowStyle>,
    /// Near-fullscreen ([`modal_rect`]) instead of content-sized. The big
    /// working surfaces (Observatory) use this; small inputs stay compact.
    full: bool,
}

/// Semantic styling for one plain-text overlay row. Keeping style metadata
/// separate from text preserves exact geometry and mouse hit-testing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum OverlayRowStyle {
    #[default]
    Plain,
    ComposerBorder,
    ComposerInput,
    Section,
    Action,
    Card,
    CardDetail,
    CardSelected,
    CardSelectedDetail,
    Error,
}

/// The shared near-fullscreen modal rectangle: most of the screen, but with a
/// clear gutter on every side (plus room for the drop shadow below-right) so
/// it still reads as a modal floating over the panes, not a screen takeover.
/// Never smaller than 48x10: the frame math (borders + separator + action
/// bar) subtracts up to 4 rows in `u16`, so a shrunken rect must stay above
/// that, and on a terminal tinier than the minimum the box overhangs - which
/// terminals clip harmlessly - rather than underflowing.
pub fn modal_rect(cols: u16, rows: u16) -> Rect {
    let w = cols.saturating_sub(10).max(48);
    let h = rows.saturating_sub(5).max(10);
    let x = (cols.saturating_sub(w) / 2).max(1) + 1;
    let y = (rows.saturating_sub(h) / 2).max(1) + 1;
    Rect { x, y, w, h }
}

static UI_THEME: std::sync::RwLock<uniterm_core::Theme> =
    std::sync::RwLock::new(uniterm_core::Theme::dark());

/// Update the semantic palette shared by every client-side surface. Client UI
/// is low frequency, so this lock is intentionally outside the pane hot path.
pub fn set_ui_theme(theme: uniterm_core::Theme) {
    if let Ok(mut current) = UI_THEME.write() {
        *current = theme;
    }
}

pub(crate) fn ui_theme() -> uniterm_core::Theme {
    UI_THEME
        .read()
        .map(|theme| *theme)
        .unwrap_or_else(|_| uniterm_core::Theme::dark())
}

pub(crate) fn shadow_style() -> String {
    format!("\x1b[{}m", ui_theme().background.sgr_bg())
}

pub(crate) fn panel_style() -> String {
    let theme = ui_theme();
    format!(
        "\x1b[0;{};{}m",
        theme.foreground.sgr_fg(),
        theme.surface.sgr_bg()
    )
}

pub(crate) fn panel_style_no_reset() -> String {
    let theme = ui_theme();
    format!(
        "\x1b[{};{}m",
        theme.foreground.sgr_fg(),
        theme.surface.sgr_bg()
    )
}

/// Row styles paint on the modal's own surface. List rows used to sit on a
/// darker blended strip inset from the frame, which read as a second box
/// inside the modal; selection is the only row that changes background.
fn overlay_row_style(style: OverlayRowStyle) -> String {
    let theme = ui_theme();
    match style {
        OverlayRowStyle::Plain => panel_style_no_reset(),
        OverlayRowStyle::ComposerBorder => format!(
            "\x1b[0;1;{};{}m",
            theme.accent.sgr_fg(),
            theme.background.sgr_bg()
        ),
        OverlayRowStyle::ComposerInput => format!(
            "\x1b[0;1;{};{}m",
            theme.foreground.sgr_fg(),
            theme.background.sgr_bg()
        ),
        OverlayRowStyle::Section => format!(
            "\x1b[0;1;{};{}m",
            theme.muted.sgr_fg(),
            theme.surface.sgr_bg()
        ),
        OverlayRowStyle::Action => format!(
            "\x1b[0;1;{};{}m",
            theme.status_active_fg.sgr_fg(),
            theme.accent_muted.sgr_bg()
        ),
        OverlayRowStyle::Card => format!(
            "\x1b[0;{};{}m",
            theme.foreground.sgr_fg(),
            theme.surface.sgr_bg()
        ),
        OverlayRowStyle::CardDetail => format!(
            "\x1b[0;{};{}m",
            theme.muted.sgr_fg(),
            theme.surface.sgr_bg()
        ),
        OverlayRowStyle::CardSelected => format!(
            "\x1b[0;1;{};{}m",
            theme.status_active_fg.sgr_fg(),
            theme.selection_bg.sgr_bg()
        ),
        OverlayRowStyle::CardSelectedDetail => format!(
            "\x1b[0;2;{};{}m",
            theme.status_active_fg.sgr_fg(),
            theme.selection_bg.sgr_bg()
        ),
        OverlayRowStyle::Error => format!(
            "\x1b[0;1;{};{}m",
            theme.error.sgr_fg(),
            theme.background.sgr_bg()
        ),
    }
}

/// Build a three-row search composer with an active border and a right-aligned
/// result count. `shown_query` must already be padded to `query_width` cells.
pub(crate) fn search_composer(
    label: &str,
    shown_query: &str,
    query_width: usize,
    visible: usize,
    total: usize,
) -> [String; 3] {
    let heading = format!("─ {label} ");
    let result = format!(" {visible}/{total} ");
    let inside = (query_width + 4)
        .max(heading.chars().count())
        .max(result.chars().count());
    let top = format!(
        "  \u{256D}{heading}{}\u{256E}",
        "─".repeat(inside.saturating_sub(heading.chars().count()))
    );
    let input = format!(" \u{203A} {shown_query}");
    let middle = format!(
        "  \u{2502}{input}{}\u{2502}",
        " ".repeat(inside.saturating_sub(input.chars().count()))
    );
    let bottom = format!(
        "  \u{2570}{}{}\u{256F}",
        "─".repeat(inside.saturating_sub(result.chars().count())),
        result
    );
    [top, middle, bottom]
}

/// The styled text of an action bar (`  key caption   key caption ...`),
/// padded to `width` visible cells. Layout mirrors [`footer_spans`].
pub(crate) fn footer_text(pairs: &[(&str, &str)], width: usize) -> String {
    let palette = ui_theme();
    let panel = panel_style_no_reset();
    let mut text = String::from("  ");
    let mut used = 2;
    for (key, caption) in pairs {
        text.push_str(&format!(
            "\x1b[1;{}m{key}\x1b[22;{}m {caption}{panel}   ",
            palette.accent.sgr_fg(),
            palette.muted.sgr_fg(),
        ));
        used += key.chars().count() + caption.chars().count() + 4;
    }
    text.push_str(&" ".repeat(width.saturating_sub(used)));
    text
}

/// The clickable interior-column span of each action-bar pair, mirroring
/// [`footer_text`]'s layout (2 leading cells, `key caption`, 3-cell gaps).
pub(crate) fn footer_spans(pairs: &[(&str, &str)]) -> Vec<(std::ops::Range<usize>, usize)> {
    let mut spans = Vec::new();
    let mut col = 2;
    for (i, (key, caption)) in pairs.iter().enumerate() {
        let len = key.chars().count() + 1 + caption.chars().count();
        spans.push((col..col + len, i));
        col += len + 3;
    }
    spans
}

/// The visible width an action bar needs (before padding).
pub(crate) fn footer_width(pairs: &[(&str, &str)]) -> usize {
    footer_spans(pairs).last().map(|(r, _)| r.end).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The shared list+detail modal (task manager, Manage Agents): one definition
// of the frame, the scrolling list column, and the hit-testing, so the two
// surfaces cannot drift apart.
// ---------------------------------------------------------------------------

/// List rows visible inside a `modal_rect` of height `h` (interior minus the
/// separator and action-bar rows).
pub(crate) fn modal_visible_rows(h: u16) -> usize {
    h.saturating_sub(4) as usize
}

/// Move a list selection up/down and keep it scrolled into view.
pub(crate) fn nav_list(
    sel: &mut usize,
    scroll: &mut usize,
    down: bool,
    len: usize,
    visible: usize,
) {
    if len == 0 {
        return;
    }
    *sel = if down {
        (*sel + 1).min(len - 1)
    } else {
        sel.saturating_sub(1)
    };
    if *sel < *scroll {
        *scroll = *sel;
    }
    if *sel >= *scroll + visible {
        *scroll = *sel + 1 - visible;
    }
}

/// Where a click at 1-based `(cx, cy)` lands inside a list modal.
pub(crate) enum ModalHit {
    Outside,
    /// The action bar; the payload is the interior column (for [`footer_spans`]).
    Bar(usize),
    /// A visible list slot (scroll not yet applied).
    ListRow(usize),
    None,
}

/// Shared hit-testing for the list modal's frame, mirroring
/// [`render_list_modal`]'s layout exactly.
pub(crate) fn modal_hit(r: Rect, list_w: u16, cx: u16, cy: u16) -> ModalHit {
    if cx < r.x || cx >= r.x + r.w || cy < r.y || cy >= r.y + r.h {
        return ModalHit::Outside;
    }
    if cy == r.y + r.h - 2 {
        return ModalHit::Bar(cx.saturating_sub(r.x + 1) as usize);
    }
    // List rows: interior starts one row below the top border.
    if cx < r.x + list_w && cy > r.y && cy < r.y + r.h - 3 {
        return ModalHit::ListRow((cy - r.y - 1) as usize);
    }
    ModalHit::None
}

/// One styled detail line: concatenated `(style, text)` parts plus the
/// visible width, tracked so the trailing pad is exact by construction -
/// hand-counted paddings previously came up short and let the pane bleed
/// through the modal.
pub(crate) fn styled_line(parts: &[(&str, &str)]) -> (String, usize) {
    let mut text = String::new();
    let mut vis = 0;
    for (style, s) in parts {
        text.push_str(style);
        text.push_str(s);
        vis += s.chars().count();
    }
    (text, vis)
}

/// Pad every styled detail line to exactly `width` visible cells (clipping
/// nothing: builders keep content within width), prefix the panel colours,
/// and fill/truncate to `count` rows.
pub(crate) fn finish_lines(
    mut lines: Vec<(String, usize)>,
    panel: &str,
    width: usize,
    count: usize,
) -> Vec<String> {
    while lines.len() < count {
        lines.push((String::new(), 0));
    }
    lines.truncate(count);
    lines
        .into_iter()
        .map(|(text, vis)| format!("{panel}{text}{}", " ".repeat(width.saturating_sub(vis))))
        .collect()
}

/// Paint the shared two-column modal: drop shadow, bordered box with a title,
/// a fixed-width list column, a divider, the detail pane, a separator, and
/// the action bar.
///
/// `list_cell(slot)` returns the styled cell for visible list slot `slot`
/// (the caller applies its scroll offset), padded to EXACTLY `list_w` visible
/// cells - a short cell shifts the divider left and lets the pane underneath
/// bleed through. `detail` lines come padded from [`finish_lines`]; `bar`
/// comes padded from [`footer_text`].
pub(crate) fn render_list_modal(
    cols: u16,
    rows: u16,
    title: &str,
    list_w: usize,
    list_cell: impl Fn(usize) -> Option<String>,
    detail: &[String],
    bar: &str,
) -> Vec<u8> {
    let r = modal_rect(cols, rows);
    let mut o = Vec::new();
    // Drop shadow, matching the other overlays.
    let sh = shadow_style();
    for row in 0..r.h {
        let (sy, sx) = (r.y + 1 + row, r.x + 1);
        if sy > rows {
            break;
        }
        o.extend_from_slice(format!("\x1b[{sy};{sx}H{sh}").as_bytes());
        o.extend(std::iter::repeat_n(
            b' ',
            r.w.min(cols.saturating_sub(sx - 1)) as usize,
        ));
    }
    let panel = panel_style();
    let inner = r.w.saturating_sub(2) as usize;
    // Top border with the title.
    let mut top = String::from("\u{250C}\u{2500}");
    top.push_str(title);
    for _ in 0..inner.saturating_sub(1 + title.chars().count()) {
        top.push('\u{2500}');
    }
    top.push('\u{2510}');
    o.extend_from_slice(panel.as_bytes());
    o.extend_from_slice(format!("\x1b[{};{}H{}", r.y, r.x, top).as_bytes());
    // Interior: list cell, divider, detail cell.
    for row in 0..modal_visible_rows(r.h) {
        let y = r.y + 1 + row as u16;
        o.extend_from_slice(format!("\x1b[{};{}H{panel}\u{2502}", y, r.x).as_bytes());
        match list_cell(row) {
            Some(cell) => o.extend_from_slice(cell.as_bytes()),
            None => o.extend(std::iter::repeat_n(b' ', list_w)),
        }
        o.extend_from_slice("\u{2502}".as_bytes());
        let line = detail.get(row).map(String::as_str).unwrap_or_default();
        o.extend_from_slice(line.as_bytes());
        o.extend_from_slice(format!("{panel}\u{2502}").as_bytes());
    }
    // Separator row above the action bar.
    let mut sep = String::from("\u{251C}");
    for i in 0..inner {
        sep.push(if i == list_w { '\u{2534}' } else { '\u{2500}' });
    }
    sep.push('\u{2524}');
    o.extend_from_slice(format!("\x1b[{};{}H{panel}{}", r.y + r.h - 3, r.x, sep).as_bytes());
    // Action bar.
    o.extend_from_slice(
        format!(
            "\x1b[{};{}H{panel}\u{2502}{bar}\u{2502}",
            r.y + r.h - 2,
            r.x
        )
        .as_bytes(),
    );
    // Bottom border.
    let mut bot = String::from("\u{2514}");
    for _ in 0..inner {
        bot.push('\u{2500}');
    }
    bot.push('\u{2518}');
    o.extend_from_slice(format!("\x1b[{};{}H{panel}{}\x1b[0m", r.y + r.h - 1, r.x, bot).as_bytes());
    o
}

/// Parse render output into (row, col, visible-char-count) segments: a CUP
/// (`ESC[r;cH`) starts a segment; other escapes are skipped; every printable
/// char counts one cell. Shared by the modals' anti-bleed-through tests.
#[cfg(test)]
pub(crate) fn render_segments(bytes: &[u8]) -> Vec<(u16, u16, usize)> {
    let s = String::from_utf8_lossy(bytes);
    let mut out: Vec<(u16, u16, usize)> = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                let mut body = String::new();
                for c2 in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c2) {
                        if c2 == 'H' {
                            let mut it = body.split(';');
                            let row: u16 = it.next().unwrap_or("1").parse().unwrap_or(1);
                            let col: u16 = it.next().unwrap_or("1").parse().unwrap_or(1);
                            out.push((row, col, 0));
                        }
                        break;
                    }
                    body.push(c2);
                }
            }
            continue;
        }
        if let Some(last) = out.last_mut() {
            last.2 += 1;
        }
    }
    out
}

impl Overlay {
    pub fn new(title: impl Into<String>, lines: Vec<String>) -> Self {
        Overlay {
            title: title.into(),
            lines,
            footer: Vec::new(),
            row_styles: Vec::new(),
            full: false,
        }
    }

    /// An overlay with the shared action-bar footer.
    pub fn with_footer(
        title: impl Into<String>,
        lines: Vec<String>,
        footer: &[(&str, &str)],
    ) -> Self {
        Overlay {
            title: title.into(),
            lines,
            footer: footer
                .iter()
                .map(|(k, c)| (k.to_string(), c.to_string()))
                .collect(),
            row_styles: Vec::new(),
            full: false,
        }
    }

    /// Apply semantic styles by content-row index. Missing entries remain
    /// plain, which keeps existing overlays source-compatible.
    pub(crate) fn with_row_styles(mut self, styles: Vec<OverlayRowStyle>) -> Self {
        self.row_styles = styles;
        self
    }

    #[cfg(test)]
    pub(crate) fn row_style_at(&self, row: usize) -> OverlayRowStyle {
        self.row_styles.get(row).copied().unwrap_or_default()
    }

    /// Make this overlay near-fullscreen (the shared large-modal geometry).
    pub fn fullscreen(mut self) -> Self {
        self.full = true;
        self
    }

    fn footer_pairs(&self) -> Vec<(&str, &str)> {
        self.footer
            .iter()
            .map(|(k, c)| (k.as_str(), c.as_str()))
            .collect()
    }

    /// The box rectangle, centered in a `cols` x `rows` screen, sized to fit its
    /// content but always leaving a 2-cell margin (so the shadow has room).
    /// Fullscreen overlays take the shared near-fullscreen rect instead.
    pub fn geometry(&self, cols: u16, rows: u16) -> Rect {
        if self.full {
            return modal_rect(cols, rows);
        }
        let content_w = self
            .lines
            .iter()
            .map(|l| l.chars().count())
            .chain(std::iter::once(self.title.chars().count() + 2))
            .chain(std::iter::once(footer_width(&self.footer_pairs())))
            .max()
            .unwrap_or(10)
            .max(10) as u16;
        // border (2) + padding (2)
        let w = (content_w + 4).min(cols.saturating_sub(4)).max(6);
        // The footer adds a separator row + the bar row.
        let footer_rows = if self.footer.is_empty() { 0 } else { 2 };
        let h = (self.lines.len() as u16 + 2 + footer_rows)
            .min(rows.saturating_sub(4))
            .max(3);
        // Center, then shift up-left by half the shadow so the whole thing
        // (box + 1-cell shadow) stays visually centered. 1-based coordinates.
        let x = (cols.saturating_sub(w) / 2).max(1) + 1;
        let y = (rows.saturating_sub(h) / 2).max(1) + 1;
        Rect { x, y, w, h }
    }

    /// Whether the 1-based cell `(cx, cy)` is inside the overlay box.
    pub fn contains(&self, cols: u16, rows: u16, cx: u16, cy: u16) -> bool {
        let r = self.geometry(cols, rows);
        cx >= r.x && cx < r.x + r.w && cy >= r.y && cy < r.y + r.h
    }

    /// The 0-based content-line index under the 1-based cell `(cx, cy)`, or
    /// `None` if the cell is on the title/border or outside the box. Line `i`
    /// corresponds to `self.lines[i]` as drawn by [`Overlay::render`].
    pub fn row_at(&self, cols: u16, rows: u16, cx: u16, cy: u16) -> Option<usize> {
        let r = self.geometry(cols, rows);
        if cx < r.x || cx >= r.x + r.w {
            return None; // outside horizontally
        }
        if cy <= r.y || cy >= r.y + r.h - 1 {
            return None; // title row, bottom border, or outside vertically
        }
        Some((cy - r.y - 1) as usize)
    }

    /// Render the overlay to escape bytes: the drop-shadow first, then the box
    /// with title, then the centered content. Coordinates are absolute, so the
    /// caller can append this after the pane frame to composite on top.
    pub fn render(&self, cols: u16, rows: u16) -> Vec<u8> {
        let r = self.geometry(cols, rows);
        let mut o = Vec::new();

        // Drop-shadow: a filled dark rectangle offset one cell down-right. The
        // box drawn next covers all but the offset edge, leaving an L-shadow.
        let sh = shadow_style();
        for row in 0..r.h {
            let sy = r.y + 1 + row;
            let sx = r.x + 1;
            if sy > rows {
                break;
            }
            o.extend_from_slice(format!("\x1b[{sy};{sx}H{sh}").as_bytes());
            let fill = r.w.min(cols.saturating_sub(sx - 1)) as usize;
            o.extend(std::iter::repeat_n(b' ', fill));
        }

        // Box.
        // Reset every attribute before painting the frame. Detail rows use
        // dim text, and restoring only the panel colors would leave that
        // attribute active on the right border and the next row's left one.
        let panel = panel_style();
        let inner = r.w.saturating_sub(2) as usize;
        // Top border with an embedded title: "┌─ Title ─…─┐".
        let mut top = String::from("\u{250C}");
        let title = if self.title.is_empty() {
            String::new()
        } else {
            format!(" {} ", self.title)
        };
        let tlen = title.chars().count();
        top.push('\u{2500}');
        top.push_str(&title);
        for _ in 0..inner.saturating_sub(1 + tlen) {
            top.push('\u{2500}');
        }
        top.push('\u{2510}');
        o.extend_from_slice(format!("\x1b[{};{}H{panel}{top}", r.y, r.x).as_bytes());

        // Interior rows (minus the footer's separator + bar rows).
        let footer_rows = if self.footer.is_empty() { 0 } else { 2 };
        for i in 0..r.h.saturating_sub(2 + footer_rows) {
            let row_y = r.y + 1 + i;
            let text = self.lines.get(i as usize).cloned().unwrap_or_default();
            let mut clipped: String = text.chars().take(inner.saturating_sub(2)).collect();
            let used = clipped.chars().count();
            for _ in 0..inner.saturating_sub(2).saturating_sub(used) {
                clipped.push(' ');
            }
            let row_style = self.row_styles.get(i as usize).copied().unwrap_or_default();
            let styled = overlay_row_style(row_style);
            // The one-cell padding on each side takes the row's style too, so
            // a selected row is highlighted edge to edge instead of leaving a
            // panel-coloured gutter around it.
            o.extend_from_slice(
                format!(
                    "\x1b[{};{}H{panel}\u{2502}{styled} {clipped} {panel}\u{2502}",
                    row_y, r.x
                )
                .as_bytes(),
            );
        }

        // Optional action-bar footer: a separator row, then the keycap bar.
        if !self.footer.is_empty() {
            let mut sep = String::from("\u{251C}");
            for _ in 0..inner {
                sep.push('\u{2500}');
            }
            sep.push('\u{2524}');
            o.extend_from_slice(format!("\x1b[{};{}H{panel}{sep}", r.y + r.h - 3, r.x).as_bytes());
            o.extend_from_slice(
                format!(
                    "\x1b[{};{}H{panel}\u{2502}{}{panel}\u{2502}",
                    r.y + r.h - 2,
                    r.x,
                    footer_text(&self.footer_pairs(), inner)
                )
                .as_bytes(),
            );
        }
        // Bottom border.
        let mut bot = String::from("\u{2514}");
        for _ in 0..inner {
            bot.push('\u{2500}');
        }
        bot.push('\u{2518}');
        o.extend_from_slice(format!("\x1b[{};{}H{panel}{bot}", r.y + r.h - 1, r.x).as_bytes());

        o.extend_from_slice(b"\x1b[0m");
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_is_centered_within_bounds() {
        let ov = Overlay::new("New Task", vec!["one".into(), "two".into()]);
        let r = ov.geometry(80, 24);
        // Fits with margin, and the box + shadow stay on-screen.
        assert!(r.x >= 1 && r.y >= 1);
        assert!(r.x + r.w <= 80);
        assert!(r.y + r.h <= 24);
        // Roughly centered.
        let cx = r.x + r.w / 2;
        assert!((cx as i32 - 40).abs() <= 3);
    }

    #[test]
    fn shadow_is_offset_and_present() {
        let ov = Overlay::new("T", vec!["hello".into()]);
        let bytes = ov.render(80, 24);
        let s = String::from_utf8_lossy(&bytes);
        // Shadow uses the shadow bg colour, box uses the panel bg + border char.
        assert!(s.contains(&shadow_style()));
        assert!(s.contains(&panel_style_no_reset()));
        assert!(s.contains('\u{250C}')); // top-left corner
        assert!(s.contains("T")); // title
    }

    #[test]
    fn row_at_maps_interior_only() {
        let ov = Overlay::new("T", vec!["l0".into(), "l1".into(), "l2".into()]);
        let r = ov.geometry(80, 24);
        // Interior line 0 is at box row r.y + 1.
        assert_eq!(ov.row_at(80, 24, r.x + 2, r.y + 1), Some(0));
        assert_eq!(ov.row_at(80, 24, r.x + 2, r.y + 2), Some(1));
        // Title row and bottom border are not content lines.
        assert_eq!(ov.row_at(80, 24, r.x + 2, r.y), None);
        assert_eq!(ov.row_at(80, 24, r.x + 2, r.y + r.h - 1), None);
        // Outside the box horizontally -> None; and contains() agrees.
        assert_eq!(ov.row_at(80, 24, 1, r.y + 1), None);
        assert!(ov.contains(80, 24, r.x, r.y));
        assert!(!ov.contains(80, 24, 1, 1));
    }

    #[test]
    fn semantic_styles_do_not_change_composer_geometry() {
        let composer = search_composer("Search projects", "abc█      ", 10, 2, 7);
        assert_eq!(composer[0].chars().count(), composer[1].chars().count());
        assert_eq!(composer[1].chars().count(), composer[2].chars().count());

        let plain = Overlay::new("Search", composer.to_vec());
        let styled = Overlay::new("Search", composer.to_vec()).with_row_styles(vec![
            OverlayRowStyle::ComposerBorder,
            OverlayRowStyle::ComposerInput,
            OverlayRowStyle::ComposerBorder,
        ]);
        assert_eq!(plain.geometry(80, 24), styled.geometry(80, 24));
        let rendered = String::from_utf8_lossy(&styled.render(80, 24)).into_owned();
        assert!(rendered.contains(&overlay_row_style(OverlayRowStyle::ComposerBorder)));
        assert!(rendered.contains(&overlay_row_style(OverlayRowStyle::ComposerInput)));
    }

    #[test]
    fn styled_rows_reassert_panel_colors_at_both_vertical_borders() {
        let overlay = Overlay::with_footer(
            "Cards",
            vec!["detail".into(), "plain".into()],
            &[("esc", "close")],
        )
        .with_row_styles(vec![OverlayRowStyle::CardDetail, OverlayRowStyle::Card]);
        let rect = overlay.geometry(80, 24);
        let rendered = String::from_utf8_lossy(&overlay.render(80, 24)).into_owned();
        let panel = panel_style();
        assert!(panel.starts_with("\x1b[0;"));

        assert!(rendered.contains(&format!("\x1b[{};{}H{panel}\u{250C}", rect.y, rect.x)));
        for row in 0..2 {
            let prefix = format!("\x1b[{};{}H{panel}\u{2502}", rect.y + 1 + row, rect.x);
            assert!(rendered.contains(&prefix));
        }
        assert!(rendered.contains(&format!(
            "\x1b[{};{}H{panel}\u{251C}",
            rect.y + rect.h - 3,
            rect.x
        )));
        assert!(rendered.contains(&format!(
            "\x1b[{};{}H{panel}\u{2502}",
            rect.y + rect.h - 2,
            rect.x
        )));
        assert!(rendered.contains(&format!(
            "\x1b[{};{}H{panel}\u{2514}",
            rect.y + rect.h - 1,
            rect.x
        )));
        assert!(rendered.matches(&format!(" {panel}\u{2502}")).count() >= 2);
        assert!(rendered.contains(&format!("{panel}\u{2502}\x1b[")));
    }

    #[test]
    fn long_content_is_clipped_to_interior() {
        let ov = Overlay::new("t", vec!["x".repeat(500)]);
        let bytes = ov.render(40, 12);
        let r = ov.geometry(40, 12);
        // No interior line may exceed the inner width; the box fits the screen.
        assert!(r.x + r.w <= 40);
        assert!(!String::from_utf8_lossy(&bytes).is_empty());
    }
}
