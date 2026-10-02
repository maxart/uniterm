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
