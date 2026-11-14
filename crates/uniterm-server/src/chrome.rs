//! Pure geometry for the server-rendered Workspace chrome.
//!
//! Rendering and mouse handling both consume these layouts. Keeping the
//! viewport math here prevents the tab bar and sidebars from developing
//! separate ideas about which item occupies a cell.

use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use uniterm_core::{Rect, Theme};

pub(crate) const CARD_ROWS: u16 = 2;
pub(crate) const CARD_GAP: u16 = 1;
const PROJECT_CARD_ROWS: u16 = 3;
const NEW_TAB_WIDTH: u16 = 3;
const TAB_SCROLL_WIDTH: u16 = 3;

/// The painted one-cell sidebar edge is also its drag target. `leading` is
/// the right panel's left edge; the Project rail uses its trailing edge.
pub(crate) fn sidebar_divider(area: Rect, leading: bool) -> Option<uniterm_core::Divider> {
    (area.w > 0 && area.h > 0).then_some(uniterm_core::Divider {
        dir: uniterm_core::SplitDir::Horizontal,
        rect: Rect::new(
            if leading {
                area.x
            } else {
                area.right().saturating_sub(1)
            },
            area.y,
            1,
            area.h,
        ),
    })
}

/// One right-panel frame shared by headers, content, footer painting and input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RightPanelLayout {
    pub(crate) area: Rect,
    pub(crate) content: Rect,
    pub(crate) footer: Rect,
}

impl RightPanelLayout {
    /// Share the visible frame edge with capture and hit testing.
    pub(crate) fn divider(self) -> Option<uniterm_core::Divider> {
        sidebar_divider(self.area, true)
    }

    /// Reserve the established blank/header/blank rows and an optional footer.
    pub(crate) fn new(area: Rect, footer_rows: u16) -> Self {
        let footer_rows = footer_rows.min(area.h);
        let footer_y = area.bottom().saturating_sub(footer_rows);
        let content_y = area.y.saturating_add(3).min(footer_y);
        let x = area.x.saturating_add(u16::from(area.w > 0));
        let w = area.w.saturating_sub(1);
        Self {
            area,
            content: Rect::new(x, content_y, w, footer_y - content_y),
            footer: Rect::new(x, footer_y, w, footer_rows),
        }
    }

    /// A clipped header row must never take a footer or content cell.
    pub(crate) fn heading_row(self) -> Option<u16> {
        let y = self.area.y.saturating_add(1);
        (y < self.footer.y).then_some(y)
    }

    /// The divider owns the first footer row; buttons share the final row.
    pub(crate) fn footer_buttons(self, count: usize) -> Vec<Rect> {
        if self.footer.h < 2 || self.footer.w == 0 {
            return Vec::new();
        }
        equal_segments_with_gap(
            Rect::new(self.footer.x, self.footer.bottom() - 1, self.footer.w, 1),
            count,
            1,
        )
    }
}

/// One bounded horizontal viewport control, shared by painting and pointer input.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScrollbarLayout {
    pub(crate) area: Rect,
    pub(crate) track: Rect,
    pub(crate) thumb: Rect,
    pub(crate) max_scroll: usize,
}

impl ScrollbarLayout {
    pub(crate) fn new(area: Rect, total: usize, visible: usize, scroll: usize) -> Self {
        let track = area;
        let max_scroll = total.saturating_sub(visible);
        let thumb_width = if max_scroll == 0 {
            track.w
        } else {
            ((u128::from(track.w) * visible as u128 / total as u128) as u16)
                .max(1)
                .min(track.w)
        };
        let travel = track.w - thumb_width;
        let offset = if max_scroll == 0 {
            0
        } else {
            (u128::from(travel) * scroll.min(max_scroll) as u128 / max_scroll as u128) as u16
        };
        Self {
            area,
            track,
            thumb: Rect::new(track.x + offset, area.y, thumb_width, 1),
            max_scroll,
        }
    }

    /// Map a dragged thumb origin to a clamped column offset, including endpoints.
    pub(crate) fn scroll_at(self, x: u16, grab: u16) -> usize {
        let travel = self.track.w.saturating_sub(self.thumb.w);
        if travel == 0 {
            return 0;
        }
        let offset = x
            .saturating_sub(grab)
            .saturating_sub(self.track.x)
            .min(travel);
        ((u128::from(offset) * self.max_scroll as u128 + u128::from(travel) / 2)
            / u128::from(travel)) as usize
    }
}

/// The persistent views hosted by the right-hand Observatory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ObservatoryTab {
    #[default]
    Agents,
    Files,
    WebServers,
    Overwatch,
}

impl ObservatoryTab {
    pub(crate) const LABELS: [&'static str; 4] = ["Agents", "Files", "Web", "Overwatch"];
    pub(crate) const ALL: [ObservatoryTab; 4] = [
        ObservatoryTab::Agents,
        ObservatoryTab::Files,
        ObservatoryTab::WebServers,
        ObservatoryTab::Overwatch,
    ];

    pub(crate) const fn index(self) -> usize {
        match self {
            ObservatoryTab::Agents => 0,
            ObservatoryTab::Files => 1,
            ObservatoryTab::WebServers => 2,
            ObservatoryTab::Overwatch => 3,
        }
    }
}

/// One item card in a vertically scrollable sidebar viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CardSlot {
    pub(crate) item: usize,
    pub(crate) rect: Rect,
}

/// Lay out complete two-row cards, leaving a non-clickable row between them.
pub(crate) fn card_slots(
    start_row: u16,
    end_row: u16,
    total: usize,
    scroll: usize,
) -> Vec<CardSlot> {
    card_slots_with_geometry(start_row, end_row, total, scroll, CARD_ROWS, CARD_GAP)
}

/// Lay out adjacent Project cards with an owned trailing transition row. Its
/// upper half is bottom padding and its lower half is the next card's top
/// padding. The final visible item retains its upper half at the list end.
pub(crate) fn project_card_slots(
    start_row: u16,
    end_row: u16,
    total: usize,
    scroll: usize,
) -> Vec<CardSlot> {
    card_slots_with_geometry(start_row, end_row, total, scroll, PROJECT_CARD_ROWS, 0)
}

fn card_slots_with_geometry(
    start_row: u16,
    end_row: u16,
    total: usize,
    scroll: usize,
    rows: u16,
    gap: u16,
) -> Vec<CardSlot> {
    let height = end_row.saturating_sub(start_row);
    let stride = rows.saturating_add(gap);
    let capacity = if height < rows {
        0
    } else {
        usize::from(height.saturating_add(gap) / stride)
    };
    let first = scroll.min(total.saturating_sub(capacity));
    (0..capacity.min(total.saturating_sub(first)))
        .map(|slot| CardSlot {
            item: first + slot,
            rect: Rect::new(
                0,
                start_row.saturating_add(u16::try_from(slot).unwrap_or(u16::MAX) * stride),
                0,
                rows,
            ),
        })
        .collect()
}

/// Return the item under `row`, excluding the deliberate card gaps.
pub(crate) fn card_at(slots: &[CardSlot], row: u16) -> Option<usize> {
    slots
        .iter()
        .find(|slot| row >= slot.rect.y && row < slot.rect.bottom())
        .map(|slot| slot.item)
}

/// Divide `area` into adjacent, equal-width controls that consume every cell.
/// Any remainder is assigned from left to right, matching flex-grow behavior.
pub(crate) fn equal_segments(area: Rect, count: usize) -> Vec<Rect> {
    let Ok(count) = u16::try_from(count) else {
        return Vec::new();
    };
    if area.w == 0 || area.h == 0 || count == 0 {
        return Vec::new();
    }
    let base = area.w / count;
    let remainder = area.w % count;
    let mut x = area.x;
    (0..count)
        .map(|index| {
            let width = base + u16::from(index < remainder);
            let rect = Rect::new(x, area.y, width, area.h);
            x = rect.right();
            rect
        })
        .collect()
}

/// Shared one-row geometry for Observatory, navigation, and Today view tabs.
/// Hidden zero-width tabs have no painted cells or mouse targets.
pub(crate) fn tab_strip_slots(area: Rect, count: usize) -> Vec<Rect> {
    if area.h == 0 {
        return Vec::new();
    }
    equal_segments(Rect::new(area.x, area.y, area.w, 1), count)
        .into_iter()
        .filter(|rect| rect.w > 0)
        .collect()
}

/// A grid cell with the shared tab attributes, for surfaces that paint into a Grid.
pub(crate) fn tab_strip_cell(theme: &Theme, selected: bool) -> uniterm_core::Cell {
    uniterm_core::Cell {
        fg: if selected {
            theme.foreground
        } else {
            theme.status_fg
        },
        bg: if selected {
            theme.accent_muted
        } else {
            theme.status_bg
        },
        attrs: if selected {
            uniterm_core::Attrs::BOLD
        } else {
            uniterm_core::Attrs::NONE
        },
        ..uniterm_core::Cell::default()
    }
}

/// Reset inherited attributes so every host uses the Observatory tab appearance.
pub(crate) fn tab_strip_style(theme: &Theme, selected: bool) -> String {
    let cell = tab_strip_cell(theme, selected);
    format!(
        "\x1b[0;{}{};{}m",
        if selected { "1;" } else { "" },
        cell.bg.sgr_bg(),
        cell.fg.sgr_fg()
    )
}

/// Keep selector labels centered with one cell of breathing room on each side.
/// Tiny slots keep one usable cell rather than losing the label to padding.
pub(crate) fn tab_strip_label(text: &str, width: usize) -> String {
    let padding = usize::from(width >= 3);
    let inner = width.saturating_sub(padding * 2);
    let (label, _) = clipped_tab_label(text, inner);
    format!(
        "{}{}{}",
        " ".repeat(padding),
        pad_tab_label(&label, inner, uniterm_core::TabLabelAlignment::Center),
        " ".repeat(padding)
    )
}

/// Reveal a clipped selector using whole graphemes while preserving its padding.
/// `offset` counts graphemes, never bytes or terminal cells.
pub(crate) fn tab_strip_revealed_label(text: &str, width: usize, offset: usize) -> String {
    let padding = usize::from(width >= 3);
    let inner = width.saturating_sub(padding * 2);
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    let mut visible = String::new();
    let mut used = 0;
    for grapheme in clean.graphemes(true).skip(offset) {
        if used + grapheme.width() > inner {
            break;
        }
        visible.push_str(grapheme);
        used += grapheme.width();
    }
    format!(
        "{}{}{}",
        " ".repeat(padding),
        pad_tab_label(&visible, inner, uniterm_core::TabLabelAlignment::Left),
        " ".repeat(padding)
    )
}

/// Last reveal offset needed to show the end, or `None` for an uncut label.
pub(crate) fn tab_strip_reveal_end(text: &str, width: usize) -> Option<usize> {
    let inner = width.saturating_sub(usize::from(width >= 3) * 2);
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if inner == 0 || clean.width() <= inner {
        return None;
    }
    let mut remaining = clean.width();
    for (index, grapheme) in clean.graphemes(true).enumerate() {
        remaining = remaining.saturating_sub(grapheme.width());
        if remaining <= inner {
            return Some(index + 1);
        }
    }
    None
}

/// Reusable finite marquee for any selector host, independent of target identity.
/// Hosts own focus, geometry, attachment scope and cancellation; this state owns
/// fitting and the complete timing policy. See `docs/UI_COMPONENTS.md`.
pub(crate) struct TabLabelReveal {
    label: String,
    width: usize,
    end: usize,
    offset: Option<usize>,
    due: Option<Instant>,
}

impl TabLabelReveal {
    const START_PAUSE: Duration = Duration::from_millis(900);
    const STEP_PAUSE: Duration = Duration::from_millis(450);
    const END_PAUSE: Duration = Duration::from_millis(1200);

    /// Arm once for real attention to a clipped label; fitting labels do no work.
    /// Replace the instance if the label or slot width changes.
    pub(crate) fn new(label: &str, width: usize, now: Instant) -> Option<Self> {
        Some(Self {
            label: label.chars().filter(|c| !c.is_control()).collect(),
            width,
            end: tab_strip_reveal_end(label, width)?,
            offset: None,
            due: Some(now + Self::START_PAUSE),
        })
    }

    /// The only wakeup this effect needs; `None` means it has drained forever.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.due
    }

    /// The current grapheme window, or `None` for the ordinary clipped label.
    pub(crate) fn offset(&self) -> Option<usize> {
        self.offset
    }

    /// Advance at most one visible step and report whether the label changed.
    /// Late servicing never bursts through intermediate frames or restarts a pass.
    pub(crate) fn advance(&mut self, now: Instant) -> bool {
        if self.due.is_none_or(|due| now < due) {
            return false;
        }
        let previous = self.text();
        match self.offset {
            None => {
                self.offset = Some(0);
                self.due = Some(now + Self::STEP_PAUSE);
            }
            Some(offset) if offset < self.end => {
                self.offset = Some(offset + 1);
                self.due = Some(
                    now + if offset + 1 == self.end {
                        Self::END_PAUSE
                    } else {
                        Self::STEP_PAUSE
                    },
                );
            }
            Some(_) => self.finish(),
        }
        self.text() != previous
    }

    /// Cancel pending work and restore the ordinary label, without rearming it.
    pub(crate) fn finish(&mut self) {
        self.offset = None;
        self.due = None;
    }

    /// The same padded, sanitized fitting used by static and animated selectors.
    pub(crate) fn text(&self) -> String {
        self.offset.map_or_else(
            || tab_strip_label(&self.label, self.width),
            |offset| tab_strip_revealed_label(&self.label, self.width, offset),
        )
    }
}

/// Fit Project root and stack labels inside their unchanged display-cell rectangles.
pub(crate) fn project_tab_label(
    text: &str,
    width: usize,
    alignment: uniterm_core::TabLabelAlignment,
) -> String {
    let (label, _) = clipped_tab_label(text, width);
    pad_tab_label(&label, width, alignment)
}

fn pad_tab_label(text: &str, width: usize, alignment: uniterm_core::TabLabelAlignment) -> String {
    let remaining = width.saturating_sub(text.width());
    let left = match alignment {
        uniterm_core::TabLabelAlignment::Left => 0,
        uniterm_core::TabLabelAlignment::Center => remaining / 2,
        uniterm_core::TabLabelAlignment::Right => remaining,
    };
    format!(
        "{}{}{}",
        " ".repeat(left),
        text,
        " ".repeat(remaining - left)
    )
}

/// Sanitize before measuring and never split a grapheme when reserving the ellipsis.
fn clipped_tab_label(text: &str, width: usize) -> (String, bool) {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if clean.width() <= width {
        return (clean, false);
    }
    if width == 0 {
        return (String::new(), true);
    }
    let mut fitted = String::new();
    let mut used = 0usize;
    for grapheme in clean.graphemes(true) {
        let grapheme_width = grapheme.width();
        if used.saturating_add(grapheme_width) > width - 1 {
            break;
        }
        fitted.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    fitted.truncate(fitted.trim_end_matches(' ').len());
    fitted.push('…');
    (fitted, true)
}

/// Paint the same slots used by pointer input, without touching adjacent cells.
/// Callers invoke this only when their visible chrome is damaged.
pub(crate) fn render_tab_strip(
    ops: &mut Vec<u8>,
    area: Rect,
    labels: &[&str],
    selected: usize,
    theme: &Theme,
) {
    let slots = tab_strip_slots(area, labels.len());
    if slots.is_empty() {
        return;
    }
    for (index, (rect, label)) in slots.iter().zip(labels).enumerate() {
        ops.extend_from_slice(
            format!(
                "\x1b[{};{}H{}{}",
                rect.y + 1,
                rect.x + 1,
                tab_strip_style(theme, index == selected),
                tab_strip_label(label, usize::from(rect.w))
            )
            .as_bytes(),
        );
    }
    ops.extend_from_slice(b"\x1b[0m");
}

/// Divide `area` into equal controls separated by a fixed-width gap.
/// The gap is dropped only when retaining it would leave a control empty.
pub(crate) fn equal_segments_with_gap(area: Rect, count: usize, gap: u16) -> Vec<Rect> {
    let Ok(count_cells) = u16::try_from(count) else {
        return Vec::new();
    };
    let gap_cells = gap.saturating_mul(count_cells.saturating_sub(1));
    let gap = if area.w >= count_cells.saturating_add(gap_cells) {
        gap
    } else {
        0
    };
    let segment_area = Rect::new(
        area.x,
        area.y,
        area.w
            .saturating_sub(gap.saturating_mul(count_cells.saturating_sub(1))),
        area.h,
    );
    equal_segments(segment_area, count)
        .into_iter()
        .enumerate()
        .map(|(index, mut rect)| {
            rect.x = rect
                .x
                .saturating_add(u16::try_from(index).unwrap_or(u16::MAX).saturating_mul(gap));
            rect
        })
        .collect()
}

/// One visible Tab segment in the horizontally scrollable status bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TabSlot {
    pub(crate) item: usize,
    pub(crate) rect: Rect,
}

/// Complete tab-bar geometry, including fixed trailing creation controls.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TabBarLayout {
    pub(crate) scroll: usize,
    pub(crate) tabs: Vec<TabSlot>,
    pub(crate) scroll_left: Option<Rect>,
    pub(crate) scroll_right: Option<Rect>,
    pub(crate) new_tab: Option<Rect>,
    pub(crate) hidden_before: bool,
    pub(crate) hidden_after: bool,
}

/// Compute the tab viewport: `+` is always fixed at the far right,
/// while overflow introduces compact left/right controls around the viewport.
pub(crate) fn tab_bar_layout(
    area: Rect,
    count: usize,
    active: usize,
    requested_scroll: usize,
    follow_active: bool,
) -> TabBarLayout {
    if area.w == 0 || area.h == 0 {
        return TabBarLayout::default();
    }

    let new_width = NEW_TAB_WIDTH.min(area.w);
    let new_tab = Rect::new(area.right().saturating_sub(new_width), area.y, new_width, 1);
    let full_viewport = Rect::new(area.x, area.y, area.w.saturating_sub(new_width), 1);
    // Equal shares consume the entire strip. Eight cells retain an ordinal
    // and a useful shortened title; only then do we introduce scrolling.
    const MIN_TAB_WIDTH: u16 = 8;
    if count == 0 || count <= usize::from(full_viewport.w / MIN_TAB_WIDTH) {
        let widths: Vec<u16> = (0..count)
            .map(|index| {
                (usize::from(full_viewport.w) / count
                    + usize::from(index < usize::from(full_viewport.w) % count))
                    as u16
            })
            .collect();
        return TabBarLayout {
            scroll: 0,
            tabs: layout_tabs(full_viewport, &widths, 0),
            new_tab: Some(new_tab),
            ..TabBarLayout::default()
        };
    }

    let left_width = TAB_SCROLL_WIDTH.min(full_viewport.w);
    let left = Rect::new(full_viewport.x, area.y, left_width, 1);
    let right_width = TAB_SCROLL_WIDTH.min(full_viewport.w.saturating_sub(left_width));
    let right = Rect::new(
        new_tab.x.saturating_sub(right_width),
        area.y,
        right_width,
        1,
    );
    let viewport = Rect::new(
        left.right(),
        area.y,
        right.x.saturating_sub(left.right()),
        1,
    );
    let visible = usize::from(viewport.w / MIN_TAB_WIDTH).max(1);
    let max_scroll = count.saturating_sub(visible);
    let mut scroll = requested_scroll.min(max_scroll);
    if follow_active && active < count {
        if active < scroll {
            scroll = active;
        } else if active >= scroll.saturating_add(visible) {
            scroll = active
                .saturating_add(1)
                .saturating_sub(visible)
                .min(max_scroll);
        }
    }
    // Only allocate visible slots, even for a Workspace with many hidden Tabs.
    let shown = usize::from(viewport.w.div_ceil(MIN_TAB_WIDTH)).min(count - scroll);
    let tabs: Vec<_> = (0..shown)
        .map(|offset| {
            let x = viewport.x + offset as u16 * MIN_TAB_WIDTH;
            TabSlot {
                item: scroll + offset,
                rect: Rect::new(x, area.y, MIN_TAB_WIDTH.min(viewport.right() - x), 1),
            }
        })
        .collect();
    let hidden_after = tabs
        .last()
        .is_none_or(|slot| slot.item + 1 < count || slot.rect.w < MIN_TAB_WIDTH);
    TabBarLayout {
        scroll,
        tabs,
        scroll_left: Some(left),
        scroll_right: Some(right),
        new_tab: Some(new_tab),
        hidden_before: scroll > 0,
        hidden_after,
    }
}

fn layout_tabs(area: Rect, widths: &[u16], scroll: usize) -> Vec<TabSlot> {
    let mut slots = Vec::new();
    let mut x = area.x;
    for (item, desired) in widths.iter().copied().enumerate().skip(scroll) {
        let remaining = area.right().saturating_sub(x);
        if remaining == 0 {
            break;
        }
        let width = desired.min(remaining).max(1);
        slots.push(TabSlot {
            item,
            rect: Rect::new(x, area.y, width, 1),
        });
        x = x.saturating_add(width);
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_panel_layout_bounds_headers_content_and_footer_at_every_size() {
        for y in [0, 1] {
            for w in [0, 1, 2, 22, 36, 52] {
                for h in 0..32 {
                    for footer_rows in [0, 2, 3] {
                        let panel = RightPanelLayout::new(Rect::new(10, y, w, h), footer_rows);
                        assert!(panel.content.y >= y);
                        assert!(panel.content.bottom() <= panel.footer.y);
                        assert_eq!(panel.footer.bottom(), y + h);
                        if h >= 6 {
                            assert_eq!(panel.content.y, y + 3);
                        }
                        if let Some(header) = panel.heading_row() {
                            assert_eq!(header, y + 1);
                            assert!(header < panel.footer.y);
                        }
                        for count in [1, 3] {
                            for rect in panel.footer_buttons(count) {
                                assert_eq!(rect.y, y + h - 1);
                                assert!(rect.x >= panel.content.x);
                                assert!(rect.right() <= panel.area.right());
                                assert!(!panel.content.contains(rect.x, rect.y));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn shared_tab_strip_has_exact_styles_and_resets_inherited_attributes() {
        let theme = uniterm_core::Config::default().theme;
        let mut ops = Vec::new();
        render_tab_strip(
            &mut ops,
            Rect::new(2, 3, 17, 1),
            &["Local", "Servers"],
            0,
            &theme,
        );
        assert_eq!(
            String::from_utf8(ops.clone()).unwrap(),
            format!(
                "\x1b[4;3H\x1b[0;1;{};{}m  Local  \x1b[4;12H\x1b[0;{};{}m Serve… \x1b[0m",
                theme.accent_muted.sgr_bg(),
                theme.foreground.sgr_fg(),
                theme.status_bg.sgr_bg(),
                theme.status_fg.sgr_fg()
            )
        );
        let mut terminal = crate::Terminal::new(24, 6);
        terminal.feed(b"\x1b[2;7m");
        terminal.feed(&ops);
        for x in 2..19 {
            let cell = terminal.grid().get(x, 3);
            assert_eq!(
                cell.bg,
                if x < 11 {
                    theme.accent_muted
                } else {
                    theme.status_bg
                }
            );
            assert_eq!(
                cell.fg,
                if x < 11 {
                    theme.foreground
                } else {
                    theme.status_fg
                }
            );
            assert_eq!(
                cell.attrs,
                if x < 11 {
                    uniterm_core::Attrs::BOLD
                } else {
                    uniterm_core::Attrs::default()
                }
            );
        }
    }

    #[test]
    fn shared_tab_strip_narrow_geometry_matches_every_painted_and_clickable_cell() {
        let theme = uniterm_core::Config::default().theme;
        for width in 0..24 {
            for labels in [
                &["Local", "Servers"][..],
                &["Agents", "Files", "Web"][..],
                &["Agents", "Files", "Web", "Overwatch"][..],
                &["Timeline", "Agents", "Flow", "Runs"][..],
            ] {
                let area = Rect::new(2, 1, width, 1);
                let slots = tab_strip_slots(area, labels.len());
                assert!(slots.iter().all(|rect| rect.w > 0 && rect.h == 1));
                assert_eq!(slots.iter().map(|rect| rect.w).sum::<u16>(), width);
                let mut ops = Vec::new();
                render_tab_strip(&mut ops, area, labels, 0, &theme);
                let mut terminal = crate::Terminal::new(30, 3);
                terminal.feed(&ops);
                for x in 0..30 {
                    let hit = slots.iter().position(|rect| rect.contains(x, 1));
                    assert_eq!(hit.is_some(), area.contains(x, 1));
                    if let Some(index) = hit {
                        let cell = terminal.grid().get(x, 1);
                        assert_eq!(
                            cell.bg,
                            if index == 0 {
                                theme.accent_muted
                            } else {
                                theme.status_bg
                            }
                        );
                    } else {
                        assert_eq!(terminal.grid().get(x, 1), uniterm_core::Cell::default());
                    }
                }
            }
        }
        let mut ops = b"unchanged".to_vec();
        for area in [Rect::new(0, 0, 0, 1), Rect::new(0, 0, 10, 0)] {
            render_tab_strip(&mut ops, area, &["Local", "Servers"], 0, &theme);
        }
        render_tab_strip(&mut ops, Rect::new(0, 0, 10, 1), &[], 0, &theme);
        assert_eq!(ops, b"unchanged");
    }

    #[test]
    fn shared_tab_rendering_work_is_bounded_by_visible_width() {
        let theme = uniterm_core::Config::default().theme;
        let mut ops = Vec::with_capacity(512);
        let started = std::time::Instant::now();
        for _ in 0..10_000 {
            ops.clear();
            render_tab_strip(
                &mut ops,
                Rect::new(0, 0, 39, 1),
                &["Local", "Servers"],
                0,
                &theme,
            );
            std::hint::black_box(&ops);
        }
        eprintln!(
            "10,000 shared tab strip renders (39 cells): {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn project_tab_labels_align_sanitized_graphemes_and_clipped_ellipsis() {
        use uniterm_core::TabLabelAlignment::{Center, Left, Right};
        for (alignment, expected, clipped) in [
            (Left, "1:界e\u{301}     ", "1:… "),
            (Center, "  1:界e\u{301}   ", "1:… "),
            (Right, "     1:界e\u{301}", " 1:…"),
        ] {
            assert_eq!(
                project_tab_label("1:界e\u{301}\n\x1b\x07", 10, alignment),
                expected
            );
            assert_eq!(project_tab_label("1:👩‍💻long", 4, alignment), clipped);
            assert_eq!(project_tab_label("界界", 3, alignment), "界…");
            assert_eq!(project_tab_label("界", 1, alignment), "…");
            assert_eq!(project_tab_label("anything", 0, alignment), "");
            for width in 0..20 {
                let label = project_tab_label("1:👩‍💻界e\u{301} long label", width, alignment);
                assert_eq!(label.width(), width);
                assert!(!label.chars().any(char::is_control));
            }
        }
        // Selector strips reserve padding separately from Project label alignment.
        assert_eq!(tab_strip_label("a    very long", 6), "  a…  ");
        let started = std::time::Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(project_tab_label("1:👩‍💻界e\u{301} long label", 12, Right));
        }
        eprintln!("10,000 Project Tab fits: {:?}", started.elapsed());
    }

    #[test]
    fn shared_tab_labels_use_display_cells_and_sanitize_terminal_controls() {
        assert_eq!(tab_strip_label("Local", 8), " Local  ");
        assert_eq!(tab_strip_label("Servers", 4), " S… ");
        assert_eq!(tab_strip_label("界界", 3), " … ");
        assert_eq!(tab_strip_label("👩‍💻x", 2), "… ");
        assert_eq!(tab_strip_label("A\n\x1bB", 4), " AB ");
        assert_eq!(tab_strip_label("Local", 0), "");
        for width in 0..20 {
            let text = "A👩‍💻界e\u{301} long";
            let label = tab_strip_label(text, width);
            assert_eq!(label.width(), width);
            if width >= 3 {
                assert!(label.starts_with(' ') && label.ends_with(' '));
            }
            if let Some(end) = tab_strip_reveal_end(text, width) {
                for offset in 0..=end {
                    let visible = tab_strip_revealed_label(text, width, offset);
                    assert_eq!(visible.width(), width);
                    assert!(!visible.starts_with('\u{200d}'));
                    assert!(!visible.chars().any(char::is_control));
                    if width >= 3 {
                        assert!(visible.starts_with(' ') && visible.ends_with(' '));
                    }
                }
            }
        }
        assert_eq!(tab_strip_revealed_label("A👩‍💻界e\u{301}", 6, 1), " 👩‍💻界 ");
    }

    #[test]
    fn reveal_has_slow_bounded_steps_and_never_rearms_after_finishing() {
        let start = Instant::now();
        let mut reveal = TabLabelReveal::new("Overwatch", 8, start).unwrap();
        assert_eq!(reveal.text(), " Overw… ");
        assert!(!reveal.advance(start + TabLabelReveal::START_PAUSE - Duration::from_millis(1)));
        let mut now = start + TabLabelReveal::START_PAUSE;
        let mut texts = Vec::new();
        while reveal.deadline().is_some() {
            assert!(reveal.advance(now));
            texts.push(reveal.text());
            now = reveal.deadline().unwrap_or(now);
            assert!(texts.len() < 10);
        }
        assert_eq!(
            texts,
            [" Overwa ", " verwat ", " erwatc ", " rwatch ", " Overw… "]
        );
        assert!(!reveal.advance(now + Duration::from_secs(3600)));
        assert!(TabLabelReveal::new("Agents", 8, start).is_none());

        // No host identity or fixed label set is required. The same lifecycle
        // reveals a dynamic Unicode label and can be cancelled by any host.
        let mut custom = TabLabelReveal::new("A👩‍💻界e\u{301} long\x1b", 6, start).unwrap();
        let mut steps = 0;
        while let Some(due) = custom.deadline() {
            assert!(custom.advance(due));
            assert_eq!(custom.text().width(), 6);
            assert!(!custom.text().chars().any(char::is_control));
            steps += 1;
            assert!(steps < 15);
        }
        let mut cancelled = TabLabelReveal::new("Long custom view", 6, start).unwrap();
        assert!(cancelled.advance(start + TabLabelReveal::START_PAUSE));
        cancelled.finish();
        assert!(cancelled.deadline().is_none());
        assert_eq!(cancelled.offset(), None);
        assert_eq!(cancelled.text(), tab_strip_label("Long custom view", 6));
        assert!(!cancelled.advance(start + Duration::from_secs(3600)));

        let mut repeated = TabLabelReveal::new("AAAAAAAAA", 6, start).unwrap();
        let mut frames = 0;
        while let Some(due) = repeated.deadline() {
            frames += usize::from(repeated.advance(due));
        }
        assert_eq!(frames, 2, "identical windows need no repaint");

        let started = Instant::now();
        for _ in 0..10_000 {
            let mut reveal = TabLabelReveal::new("Overwatch", 8, start).unwrap();
            while let Some(due) = reveal.deadline() {
                std::hint::black_box(reveal.advance(due));
            }
        }
        eprintln!("10,000 finite selector reveals: {:?}", started.elapsed());
    }

    #[test]
    fn scrollbar_thumb_tracks_columns_and_clamps_drag_endpoints() {
        let area = Rect::new(24, 23, 60, 1);
        let first = ScrollbarLayout::new(area, 4, 2, 0);
        assert_eq!(first.track, area);
        assert_eq!(first.thumb, Rect::new(24, 23, 30, 1));
        let last = ScrollbarLayout::new(area, 4, 2, usize::MAX);
        assert_eq!(last.thumb.right(), last.track.right());
        assert_eq!(last.scroll_at(0, 10), 0);
        assert_eq!(last.scroll_at(u16::MAX, 10), 2);
        for total in [0, 1, 2] {
            let fits = ScrollbarLayout::new(area, total, 2, 50);
            assert_eq!(fits.thumb, fits.track);
            assert_eq!(fits.scroll_at(u16::MAX, 0), 0);
        }
        let huge = ScrollbarLayout::new(area, usize::MAX, 1, usize::MAX);
        assert_eq!(huge.thumb.w, 1);
        assert_eq!(huge.scroll_at(u16::MAX, 0), usize::MAX - 1);
        let started = std::time::Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(ScrollbarLayout::new(area, 1_000_000, 2, 900_000));
        }
        eprintln!(
            "10,000 Scrolling scrollbar layouts: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn card_viewport_never_makes_gap_rows_clickable() {
        let slots = card_slots(3, 12, 8, 2);
        assert_eq!(
            slots.iter().map(|slot| slot.item).collect::<Vec<_>>(),
            [2, 3, 4]
        );
        assert_eq!(card_at(&slots, 3), Some(2));
        assert_eq!(card_at(&slots, 4), Some(2));
        assert_eq!(card_at(&slots, 5), None);
        assert_eq!(card_at(&slots, 6), Some(3));
    }

    #[test]
    fn project_cards_include_bottom_padding_on_the_last_item() {
        let slots = project_card_slots(3, 14, 8, 2);
        assert_eq!(
            slots.iter().map(|slot| slot.item).collect::<Vec<_>>(),
            [2, 3, 4]
        );
        assert_eq!(slots[0].rect, Rect::new(0, 3, 0, 3));
        assert_eq!(slots[1].rect, Rect::new(0, 6, 0, 3));
        assert_eq!(slots[2].rect, Rect::new(0, 9, 0, 3));
        assert_eq!(card_at(&slots, 5), Some(2));
        assert_eq!(card_at(&slots, 6), Some(3));
        assert_eq!(card_at(&slots, 8), Some(3));
        assert_eq!(card_at(&slots, 9), Some(4));
        assert_eq!(card_at(&slots, 11), Some(4));
        assert_eq!(card_at(&slots, 12), None);
    }

    #[test]
    fn equal_segments_fill_the_area_and_share_remainder() {
        assert_eq!(
            equal_segments(Rect::new(85, 19, 35, 1), 3),
            [
                Rect::new(85, 19, 12, 1),
                Rect::new(97, 19, 12, 1),
                Rect::new(109, 19, 11, 1),
            ]
        );
    }

    #[test]
    fn equal_segments_with_gap_leave_non_clickable_cells_between_controls() {
        assert_eq!(
            equal_segments_with_gap(Rect::new(85, 19, 35, 1), 3, 1),
            [
                Rect::new(85, 19, 11, 1),
                Rect::new(97, 19, 11, 1),
                Rect::new(109, 19, 11, 1),
            ]
        );
        assert_eq!(
            equal_segments_with_gap(Rect::new(0, 0, 3, 1), 3, 1),
            [
                Rect::new(0, 0, 1, 1),
                Rect::new(1, 0, 1, 1),
                Rect::new(2, 0, 1, 1),
            ]
        );
    }

    #[test]
    fn tab_overflow_keeps_creation_control_fixed_and_follows_active() {
        let area = Rect::new(20, 0, 35, 1);
        let layout = tab_bar_layout(area, 5, 4, 0, true);
        assert_eq!(layout.new_tab, Some(Rect::new(52, 0, 3, 1)));
        assert!(layout.scroll_left.is_some());
        assert!(layout.scroll_right.is_some());
        assert!(layout
            .tabs
            .iter()
            .any(|slot| slot.item == 4 && slot.rect.w == 8));
        assert!(layout.hidden_before);
        assert!(!layout.hidden_after);
    }

    #[test]
    fn tabs_fill_shrink_then_scroll_at_eight_cells() {
        let area = Rect::new(10, 2, 36, 1);
        let one = tab_bar_layout(area, 1, 0, 0, false);
        assert_eq!(one.tabs[0].rect, Rect::new(10, 2, 33, 1));
        let two = tab_bar_layout(area, 2, 0, 0, false);
        assert_eq!(
            two.tabs.iter().map(|slot| slot.rect.w).collect::<Vec<_>>(),
            [17, 16]
        );
        assert_eq!(two.tabs[1].rect.right(), two.new_tab.unwrap().x);
        let four = tab_bar_layout(area, 4, 0, 0, false);
        assert_eq!(
            four.tabs.iter().map(|slot| slot.rect.w).collect::<Vec<_>>(),
            [9, 8, 8, 8]
        );
        assert!(four.scroll_left.is_none());
        let five = tab_bar_layout(area, 5, 4, 0, true);
        assert!(five.scroll_left.is_some());
        assert!(five
            .tabs
            .iter()
            .any(|slot| slot.item == 4 && slot.rect.w == 8));
        assert!(tab_bar_layout(Rect::new(0, 0, 0, 1), 1, 0, 0, true)
            .tabs
            .is_empty());
        assert!(tab_bar_layout(Rect::new(0, 0, 2, 1), 1, 0, 0, true)
            .tabs
            .is_empty());
        assert!(tab_bar_layout(area, 0, 0, 0, false).tabs.is_empty());
    }

    #[test]
    fn tab_layout_work_is_bounded_by_visible_slots() {
        let started = std::time::Instant::now();
        for _ in 0..10_000 {
            let layout = std::hint::black_box(tab_bar_layout(
                Rect::new(0, 0, 120, 1),
                1_000_000,
                999_999,
                0,
                true,
            ));
            assert!(layout.tabs.len() <= 14);
            assert_eq!(layout.tabs.last().unwrap().item, 999_999);
            assert!(!layout.hidden_after);
        }
        eprintln!("10,000 overflowing Tab layouts: {:?}", started.elapsed());
    }

    #[test]
    fn fitting_tabs_do_not_pay_for_scroll_buttons() {
        let layout = tab_bar_layout(Rect::new(10, 0, 30, 1), 2, 0, 9, false);
        assert_eq!(layout.scroll, 0);
        assert!(layout.scroll_left.is_none());
        assert!(layout.scroll_right.is_none());
        assert_eq!(layout.new_tab, Some(Rect::new(37, 0, 3, 1)));
    }
}
