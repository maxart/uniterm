//! Pure geometry for the server-rendered Workspace chrome.
//!
//! Rendering and mouse handling both consume these layouts. Keeping the
//! viewport math here prevents the tab bar and sidebars from developing
//! separate ideas about which item occupies a cell.

use uniterm_core::Rect;

pub(crate) const CARD_ROWS: u16 = 2;
pub(crate) const CARD_GAP: u16 = 1;
const PROJECT_CARD_ROWS: u16 = 3;
const NEW_TAB_WIDTH: u16 = 3;
const TAB_SCROLL_WIDTH: u16 = 3;

/// The persistent views hosted by the right-hand Observatory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ObservatoryTab {
    #[default]
    Agents,
    Files,
    WebServers,
}

impl ObservatoryTab {
    pub(crate) const ALL: [ObservatoryTab; 3] = [
        ObservatoryTab::Agents,
        ObservatoryTab::Files,
        ObservatoryTab::WebServers,
    ];

    pub(crate) const fn index(self) -> usize {
        match self {
            ObservatoryTab::Agents => 0,
            ObservatoryTab::Files => 1,
            ObservatoryTab::WebServers => 2,
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
    tab_widths: &[u16],
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
    let all = layout_tabs(full_viewport, tab_widths, 0);
    let overflow = all.len() < tab_widths.len()
        || all
            .last()
            .is_some_and(|slot| slot.rect.w < tab_widths.get(slot.item).copied().unwrap_or(0));
    if !overflow {
        let trailing_x = all
            .last()
            .map_or(area.x, |slot| slot.rect.right())
            .min(new_tab.x);
        let trailing_new_tab = Rect::new(
            trailing_x,
            area.y,
            area.right().saturating_sub(trailing_x).min(NEW_TAB_WIDTH),
            1,
        );
        return TabBarLayout {
            scroll: 0,
            tabs: all,
            new_tab: Some(trailing_new_tab),
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
    let max_scroll = max_tab_scroll(viewport, tab_widths);
    let mut scroll = requested_scroll.min(max_scroll);
    if follow_active {
        if active < scroll {
            scroll = active;
        }
        while active < tab_widths.len()
            && !fully_visible(
                &layout_tabs(viewport, tab_widths, scroll),
                tab_widths,
                active,
            )
            && scroll < max_scroll
        {
            scroll += 1;
        }
    }
    let tabs = layout_tabs(viewport, tab_widths, scroll);
    let hidden_after = tabs
        .last()
        .is_none_or(|slot| slot.item + 1 < tab_widths.len())
        || tabs
            .last()
            .is_some_and(|slot| slot.rect.w < tab_widths.get(slot.item).copied().unwrap_or(0));
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

fn fully_visible(slots: &[TabSlot], widths: &[u16], item: usize) -> bool {
    slots.iter().any(|slot| {
        slot.item == item && slot.rect.w == widths.get(item).copied().unwrap_or_default()
    })
}

fn max_tab_scroll(area: Rect, widths: &[u16]) -> usize {
    if widths.is_empty() {
        return 0;
    }
    (0..widths.len())
        .find(|scroll| {
            fully_visible(
                &layout_tabs(area, widths, *scroll),
                widths,
                widths.len() - 1,
            )
        })
        .unwrap_or_else(|| widths.len().saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let layout = tab_bar_layout(area, &[8, 8, 8, 8, 8], 4, 0, true);
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
    fn fitting_tabs_do_not_pay_for_scroll_buttons() {
        let layout = tab_bar_layout(Rect::new(10, 0, 30, 1), &[8, 8], 0, 9, false);
        assert_eq!(layout.scroll, 0);
        assert!(layout.scroll_left.is_none());
        assert!(layout.scroll_right.is_none());
        assert_eq!(layout.new_tab, Some(Rect::new(26, 0, 3, 1)));
    }
}
