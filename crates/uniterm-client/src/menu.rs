//! Client-side dropdown menus for server-rendered chrome.
//!
//! This module owns each open dropdown: an anchored box listing the menu's
//! items with their prefix shortcuts, keyboard navigation, and click
//! hit-testing.
//! The server owns the clickable Workspace, Tab, and Observatory geometry and
//! supplies an exact anchor. Prefix-driven menus open beside the screen edge
//! so keyboard access does not depend on mouse chrome.

use crate::overlay::{panel_style_no_reset, shadow_style, Rect};
use uniterm_core::menu::{Menu, MenuAction, MENUS, MENU_BAR_LEN};
use uniterm_core::ProjectId;

/// An open dropdown: which menu, and which item is selected.
#[derive(Clone, Debug)]
pub struct MenuState {
    pub idx: usize,
    pub sel: usize,
    anchor: Option<MenuAnchor>,
    project: Option<ProjectId>,
    scroll: std::cell::Cell<usize>,
    pub(crate) palette: Option<crate::palette::Palette>,
}

#[derive(Clone, Copy, Debug)]
struct MenuAnchor {
    x: u16,
    y: u16,
    width: u16,
    open_up: bool,
}

impl MenuState {
    pub fn open(idx: usize) -> Self {
        MenuState {
            idx: idx.min(MENUS.len() - 1),
            sel: 0,
            anchor: None,
            project: None,
            scroll: std::cell::Cell::new(0),
            palette: None,
        }
    }

    /// Open one menu at an exact 1-based server-rendered chrome cell.
    pub fn anchored(idx: usize, x: u16, y: u16, width: u16, open_up: bool) -> Self {
        MenuState {
            idx: idx.min(MENUS.len() - 1),
            sel: 0,
            anchor: Some(MenuAnchor {
                x,
                y,
                width,
                open_up,
            }),
            project: None,
            scroll: std::cell::Cell::new(0),
            palette: None,
        }
    }

    /// Open an anchored context menu for one server-resolved Project.
    pub fn anchored_project(
        idx: usize,
        project: ProjectId,
        x: u16,
        y: u16,
        width: u16,
        open_up: bool,
    ) -> Self {
        let mut state = Self::anchored(idx, x, y, width, open_up);
        state.project = Some(project);
        state
    }

    /// Search every non-contextual action from the configured prefix.
    pub fn quick_actions(bindings: &[uniterm_core::KeyBinding]) -> Self {
        let mut state = Self::open(0);
        state.palette = Some(crate::palette::Palette::new(bindings));
        state
    }

    /// Reuse native menu geometry and editing for a dedicated action surface.
    pub(crate) fn custom(title: String, rows: Vec<(String, MenuAction)>) -> Self {
        let mut state = Self::open(0);
        state.palette = Some(crate::palette::Palette::custom(title, rows));
        state
    }

    /// Whether live supervisor state may update this existing surface.
    pub(crate) fn is_overwatch(&self) -> bool {
        self.palette
            .as_ref()
            .is_some_and(|palette| palette.is_overwatch())
    }

    /// Catalog replies may update only the dedicated model/effort picker.
    pub(crate) fn is_overwatch_picker(&self) -> bool {
        self.palette.as_ref().is_some_and(|palette| {
            palette.purpose == crate::palette::PalettePurpose::OverwatchPicker
        })
    }

    /// Whether this surface owns an event-driven switcher subscription.
    pub(crate) fn is_switcher(&self) -> bool {
        self.palette
            .as_ref()
            .is_some_and(|palette| palette.switcher)
    }

    /// Resolve the same action for keyboard and mouse selection.
    pub fn action_at(&self, index: usize) -> MenuAction {
        if let Some(palette) = &self.palette {
            palette.items[index].action
        } else {
            self.menu().items[index].action
        }
    }

    pub fn menu(&self) -> &'static Menu {
        &MENUS[self.idx]
    }

    /// The selected item's action.
    pub fn action(&self) -> MenuAction {
        self.action_at(self.sel)
    }

    /// Resolve the Project target for the clicked item or sidebar context menu.
    pub fn project_at(&self, index: usize) -> Option<ProjectId> {
        self.palette
            .as_ref()
            .and_then(|palette| palette.items.get(index))
            .and_then(|item| item.project)
            .or(self.project)
    }

    /// The Project target for the selected item or sidebar context menu.
    pub fn project(&self) -> Option<ProjectId> {
        self.project_at(self.sel)
    }

    /// Keep selection in the semantic-row viewport without moving rows on hover.
    /// Painting and pointer input both use this range, including separator rows.
    fn visible_rows(&self, height: u16) -> impl Iterator<Item = Option<usize>> + '_ {
        let menu = self.menu();
        let capacity = usize::from(height.saturating_sub(2));
        let total = menu.rows().count();
        let selected = menu
            .rows()
            .position(|row| row == Some(self.sel))
            .unwrap_or(0);
        let mut start = self.scroll.get().min(total.saturating_sub(capacity));
        if selected < start {
            start = selected;
        } else if selected >= start.saturating_add(capacity) {
            start = selected.saturating_add(1).saturating_sub(capacity);
        }
        self.scroll.set(start);
        menu.rows().skip(start).take(capacity)
    }

    pub fn next_item(&mut self) {
        self.sel = (self.sel + 1) % self.menu().items.len();
    }

    pub fn prev_item(&mut self) {
        let n = self.menu().items.len();
        self.sel = (self.sel + n - 1) % n;
    }

    pub fn next_menu(&mut self) {
        if self.anchor.is_some() {
            return;
        }
        self.idx = (self.idx + 1) % MENU_BAR_LEN;
        self.sel = 0;
        self.scroll.set(0);
    }

    pub fn prev_menu(&mut self) {
        if self.anchor.is_some() {
            return;
        }
        self.idx = (self.idx + MENU_BAR_LEN - 1) % MENU_BAR_LEN;
        self.sel = 0;
        self.scroll.set(0);
    }
}

/// A human label for the prefix byte (Ctrl-A = 0x01 -> `C-a`).
pub fn prefix_label(prefix: u8) -> String {
    if (0x01..=0x1a).contains(&prefix) {
        format!("C-{}", (b'a' + prefix - 1) as char)
    } else {
        format!("{}", prefix as char)
    }
}

/// One rendered line of the dropdown: label left, shortcut right.
fn item_line(label: &str, shortcut: &str, width: usize) -> String {
    let pad = width
        .saturating_sub(label.chars().count() + shortcut.chars().count())
        .max(1);
    format!("{label}{}{shortcut}", " ".repeat(pad))
}

/// The dropdown's box rectangle in 1-based cells, like [`Rect`]. It opens on
/// the requested side of its chrome anchor and remains clamped to the screen.
pub fn menu_rect(state: &MenuState, cols: u16, rows: u16, status_top: bool, prefix: u8) -> Rect {
    if let Some(palette) = &state.palette {
        return palette.overlay(state.sel, rows).geometry(cols, rows);
    }
    let menu = state.menu();
    let short = prefix_label(prefix);
    let content_w = menu
        .items
        .iter()
        .map(|it| it.label.chars().count() + short.chars().count() + it.key.chars().count() + 4)
        .max()
        .unwrap_or(10)
        .max(menu.title.chars().count() + 2) as u16;
    let minimum = state.anchor.map_or(0, |anchor| anchor.width);
    let w = (content_w + 4)
        .max(minimum)
        .min(cols.saturating_sub(2))
        .max(6);
    let separator_rows = u16::try_from(menu.separators_before().count()).unwrap_or(u16::MAX);
    // Prefer room for chrome and shadow, but retain one action on tiny screens.
    let available = rows.saturating_sub(2).max(3).min(rows);
    let h = (menu.items.len() as u16 + separator_rows + 2).min(available);
    let anchor_x = state.anchor.map_or(1, |anchor| anchor.x);
    // Keep the whole box (plus its 1-cell shadow) on screen.
    let x = anchor_x.min(cols.saturating_sub(w).max(1));
    let y = if let Some(anchor) = state.anchor {
        if anchor.open_up {
            anchor.y.saturating_sub(h)
        } else {
            anchor.y.saturating_add(1)
        }
    } else if status_top {
        2 // row 1 is the status line
    } else {
        rows.saturating_sub(h) // box bottom lands on the row above the status
    }
    .max(1)
    .min(rows.saturating_sub(h).max(1));
    Rect { x, y, w, h }
}

/// Render the dropdown: shadow, box with the menu title, one line per item
/// (label + prefix shortcut), the selected line in reverse video.
pub fn render_menu(ms: &MenuState, cols: u16, rows: u16, status_top: bool, prefix: u8) -> Vec<u8> {
    if let Some(palette) = &ms.palette {
        return palette.overlay(ms.sel, rows).render(cols, rows);
    }
    let r = menu_rect(ms, cols, rows, status_top, prefix);
    if r.h < 3 {
        return Vec::new();
    }
    let menu = ms.menu();
    let short = prefix_label(prefix);
    let inner = r.w.saturating_sub(2) as usize;
    let mut o = Vec::new();

    // Drop-shadow (offset one cell down-right), matching the overlay style.
    let sh = shadow_style();
    for row in 0..r.h {
        let (sy, sx) = (r.y + 1 + row, r.x + 1);
        if sy > rows {
            break;
        }
        o.extend_from_slice(format!("\x1b[{sy};{sx}H{sh}").as_bytes());
        let fill = r.w.min(cols.saturating_sub(sx - 1)) as usize;
        o.extend(std::iter::repeat_n(b' ', fill));
    }

    let panel = panel_style_no_reset();
    o.extend_from_slice(panel.as_bytes());
    // Top border with the menu title.
    let title = format!(" {} ", menu.title);
    let mut top = String::from("\u{250C}\u{2500}");
    top.push_str(&title);
    for _ in 0..inner.saturating_sub(1 + title.chars().count()) {
        top.push('\u{2500}');
    }
    top.push('\u{2510}');
    o.extend_from_slice(format!("\x1b[{};{}H{}", r.y, r.x, top).as_bytes());

    // Items and optional grouping separators.
    let mut row_y = r.y + 1;
    for row in ms.visible_rows(r.h) {
        let Some(i) = row else {
            if row_y >= r.y + r.h - 1 {
                break;
            }
            let mut separator = String::from("\u{251C}");
            for _ in 0..inner {
                separator.push('\u{2500}');
            }
            separator.push('\u{2524}');
            o.extend_from_slice(format!("\x1b[{row_y};{}H{separator}", r.x).as_bytes());
            row_y += 1;
            continue;
        };
        let it = &menu.items[i];
        if row_y >= r.y + r.h - 1 {
            break; // clipped by a tiny screen
        }
        let shortcut = if it.key.is_empty() {
            String::new() // menu-only item, no keybinding to teach
        } else {
            format!("{short} {}", it.key)
        };
        let text = item_line(it.label, &shortcut, inner.saturating_sub(2));
        let sel = if i == ms.sel { "\x1b[7m" } else { "" };
        let unsel = if i == ms.sel { "\x1b[27m" } else { "" };
        o.extend_from_slice(
            format!(
                "\x1b[{};{}H\u{2502}{sel} {text} {unsel}\u{2502}",
                row_y, r.x
            )
            .as_bytes(),
        );
        row_y += 1;
    }

    // Bottom border.
    let mut bot = String::from("\u{2514}");
    for _ in 0..inner {
        bot.push('\u{2500}');
    }
    bot.push('\u{2518}');
    o.extend_from_slice(format!("\x1b[{};{}H{}", r.y + r.h - 1, r.x, bot).as_bytes());
    o.extend_from_slice(b"\x1b[0m");
    o
}

/// The 0-based item index under 1-based cell `(cx, cy)`, or `None` if the cell
/// is outside the dropdown's interior.
pub fn item_at(
    ms: &MenuState,
    cols: u16,
    rows: u16,
    status_top: bool,
    prefix: u8,
    cx: u16,
    cy: u16,
) -> Option<usize> {
    if let Some(palette) = &ms.palette {
        let row = palette.overlay(ms.sel, rows).row_at(cols, rows, cx, cy)?;
        let offset = row.checked_sub(2)?;
        let index = palette.start(ms.sel, rows) + offset;
        return (offset < palette.visible(rows) && index < palette.items.len()).then_some(index);
    }
    let r = menu_rect(ms, cols, rows, status_top, prefix);
    if r.h < 3 || cx <= r.x || cx >= r.x + r.w - 1 || cy <= r.y || cy >= r.y + r.h - 1 {
        return None;
    }
    let row = (cy - r.y - 1) as usize;
    ms.visible_rows(r.h).nth(row).flatten()
}

/// Whether 1-based `(cx, cy)` falls anywhere inside the dropdown box.
pub fn contains(
    ms: &MenuState,
    cols: u16,
    rows: u16,
    status_top: bool,
    prefix: u8,
    cx: u16,
    cy: u16,
) -> bool {
    let r = menu_rect(ms, cols, rows, status_top, prefix);
    cx >= r.x && cx < r.x + r.w && cy >= r.y && cy < r.y + r.h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_wraps() {
        let mut ms = MenuState::open(0);
        let n = ms.menu().items.len();
        ms.prev_item();
        assert_eq!(ms.sel, n - 1);
        ms.next_item();
        assert_eq!(ms.sel, 0);
        ms.prev_menu();
        assert_eq!(ms.idx, MENU_BAR_LEN - 1);
        ms.next_menu();
        assert_eq!(ms.idx, 0);
    }

    #[test]
    fn rect_sits_above_a_bottom_status_line() {
        let state = MenuState::open(0);
        let r = menu_rect(&state, 100, 30, false, 0x01);
        assert_eq!(r.y + r.h - 1, 29); // bottom border on the row above status
        assert!(r.x + r.w <= 100);
    }

    #[test]
    fn rect_sits_below_a_top_status_line() {
        let state = MenuState::open(1);
        let r = menu_rect(&state, 100, 30, true, 0x01);
        assert_eq!(r.y, 2);
    }

    #[test]
    fn item_hit_test_matches_rows() {
        let ms = MenuState::open(1); // Tabs
        let r = menu_rect(&ms, 100, 30, false, 0x01);
        assert_eq!(
            item_at(&ms, 100, 30, false, 0x01, r.x + 2, r.y + 1),
            Some(0)
        );
        assert_eq!(
            item_at(&ms, 100, 30, false, 0x01, r.x + 2, r.y + 2),
            Some(1)
        );
        // The title row is not an item; outside is nothing.
        assert_eq!(item_at(&ms, 100, 30, false, 0x01, r.x + 2, r.y), None);
        assert_eq!(item_at(&ms, 100, 30, false, 0x01, 1, 1), None);
        assert!(contains(&ms, 100, 30, false, 0x01, r.x, r.y));
        assert!(!contains(&ms, 100, 30, false, 0x01, 1, 1));
    }

    #[test]
    fn render_lists_labels_and_shortcuts() {
        let ms = MenuState::open(1); // Tabs
        let bytes = render_menu(&ms, 100, 30, false, 0x01);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("New tab"));
        assert!(s.contains("Rename tab"));
        assert!(s.contains("C-a c"));
        assert!(s.contains("\x1b[7m")); // a selected row exists
    }

    #[test]
    fn workspace_groups_hit_test_across_multiple_separators() {
        let idx = MENUS
            .iter()
            .position(|menu| menu.title == "Workspace")
            .unwrap();
        let ms = MenuState::open(idx);
        let r = menu_rect(&ms, 120, 30, true, 0x01);
        for (offset, separator) in ms.menu().separators_before().enumerate() {
            let row = separator + offset;
            assert_eq!(
                item_at(&ms, 120, 30, true, 0x01, r.x + 2, r.y + 1 + row as u16,),
                None,
            );
        }
        assert_eq!(
            item_at(
                &ms,
                120,
                30,
                true,
                0x01,
                r.x + 2,
                r.y + ms.menu().rows().count() as u16
            ),
            Some(ms.menu().items.len() - 1),
        );
        let bytes = render_menu(&ms, 120, 30, true, 0x01);
        let rendered = String::from_utf8_lossy(&bytes);
        assert!(rendered.contains("About Uniterm"));
        assert!(rendered.contains("Close this Workspace"));
        assert!(rendered.contains('\u{251C}'));
    }

    fn assert_selected_row_is_painted_and_clickable(
        ms: &MenuState,
        cols: u16,
        rows: u16,
        top: bool,
    ) {
        let rect = menu_rect(ms, cols, rows, top, 1);
        assert!(rect.y + rect.h - 1 <= rows);
        let bytes = render_menu(ms, cols, rows, top, 1);
        let rendered = String::from_utf8(bytes).unwrap();
        let mut selected_rows = 0;
        for y in rect.y + 1..rect.y + rect.h - 1 {
            let hit = item_at(ms, cols, rows, top, 1, rect.x + 2, y);
            if let Some(index) = hit {
                let item = ms.menu().items[index];
                let selected = if index == ms.sel { "\x1b[7m" } else { "" };
                assert!(
                    rendered.contains(&format!("\x1b[{y};{}H│{selected} {}", rect.x, item.label)),
                    "missing painted item {} at row {y}",
                    item.label
                );
                if index == ms.sel {
                    selected_rows += 1;
                    assert_eq!(ms.action_at(index), ms.action());
                }
            } else {
                assert!(
                    rendered.contains(&format!("\x1b[{y};{}H├", rect.x)),
                    "non-action interior row must retain its semantic separator"
                );
            }
        }
        assert_eq!(
            selected_rows,
            1,
            "selected {:?} is not visible at {cols}x{rows}",
            ms.action()
        );
        assert_eq!(rendered.matches("\x1b[7m").count(), 1);
        for y in [rect.y, rect.y + rect.h - 1] {
            assert_eq!(item_at(ms, cols, rows, top, 1, rect.x + 2, y), None);
        }
    }

    #[test]
    fn short_workspace_menu_follows_every_up_down_selection_and_shared_hit_row() {
        let index = MENUS
            .iter()
            .position(|menu| menu.title == "Workspace")
            .unwrap();
        for cols in [80, 40] {
            for rows in [20, 12, 8, 6, 5, 4, 3] {
                for top in [true, false] {
                    for anchored in [false, true] {
                        let mut state = if anchored {
                            MenuState::anchored(index, 1, if top { 1 } else { rows }, 1, !top)
                        } else {
                            MenuState::open(index)
                        };
                        assert_selected_row_is_painted_and_clickable(&state, cols, rows, top);
                        // Up wraps directly to Close; it must be visible before Enter can act.
                        for key in [b"\x1b[A", b"\x1b[B"] {
                            for _ in 0..state.menu().items.len() + 1 {
                                crate::input::handle_menu_keys(key, &mut state);
                                assert_selected_row_is_painted_and_clickable(
                                    &state, cols, rows, top,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn menu_viewport_preserves_hover_positions_and_follows_resize() {
        let index = MENUS
            .iter()
            .position(|menu| menu.title == "Workspace")
            .unwrap();
        let mut state = MenuState::open(index);
        state.prev_item();
        assert_selected_row_is_painted_and_clickable(&state, 80, 8, true);
        let rect = menu_rect(&state, 80, 8, true, 1);
        let start = state.scroll.get();
        let (y, item) = (rect.y + 1..rect.y + rect.h - 1)
            .find_map(|y| item_at(&state, 80, 8, true, 1, rect.x + 2, y).map(|item| (y, item)))
            .unwrap();
        // Client hover changes selection directly; it must not scroll under the mouse.
        state.sel = item;
        assert_selected_row_is_painted_and_clickable(&state, 80, 8, true);
        assert_eq!(state.scroll.get(), start);
        assert_eq!(item_at(&state, 80, 8, true, 1, rect.x + 2, y), Some(item));
        state.sel = state.menu().items.len() - 1;
        for rows in [30, 20, 5, 12, 30] {
            assert_selected_row_is_painted_and_clickable(&state, 80, rows, true);
        }
        assert_eq!(state.scroll.get(), 0);
        state.next_menu();
        assert_eq!(state.scroll.get(), 0);
        assert_selected_row_is_painted_and_clickable(&state, 80, 8, true);
    }

    #[test]
    fn anchored_menu_viewport_stays_on_screen_and_impossible_heights_are_inert() {
        let index = MENUS
            .iter()
            .position(|menu| menu.title == "Workspace")
            .unwrap();
        let mut state = MenuState::anchored(index, 1, 19, 1, false);
        state.prev_item();
        assert_selected_row_is_painted_and_clickable(&state, 80, 20, true);
        for rows in 0..3 {
            assert!(render_menu(&state, 80, rows, true, 1).is_empty());
            assert_eq!(item_at(&state, 80, rows, true, 1, 2, 2), None);
        }
    }

    #[test]
    fn prefix_labels() {
        assert_eq!(prefix_label(0x01), "C-a");
        assert_eq!(prefix_label(0x02), "C-b");
    }

    #[test]
    fn server_anchor_controls_side_and_minimum_width() {
        let state = MenuState::anchored(2, 90, 28, 28, true);
        let rect = menu_rect(&state, 120, 30, true, 0x01);
        assert_eq!(rect.w, 28);
        assert!(rect.y + rect.h <= 28);
        assert!(rect.x + rect.w <= 121);
    }

    #[test]
    fn project_anchor_retains_the_server_resolved_target() {
        let index = MENUS
            .iter()
            .position(|menu| menu.title == "Project")
            .unwrap();
        let state = MenuState::anchored_project(index, ProjectId(42), 1, 4, 23, false);
        assert_eq!(state.project(), Some(ProjectId(42)));
        assert_eq!(state.menu().title, "Project");
        let item = state
            .menu()
            .items
            .iter()
            .position(|item| item.action == uniterm_core::menu::MenuAction::ChangeProjectDirectory)
            .unwrap();
        let rect = menu_rect(&state, 100, 40, false, 1);
        let separators = state
            .menu()
            .separators_before()
            .filter(|index| *index <= item)
            .count();
        assert_eq!(
            item_at(
                &state,
                100,
                40,
                false,
                1,
                rect.x + 1,
                rect.y + 1 + (item + separators) as u16
            ),
            Some(item)
        );
    }
}
