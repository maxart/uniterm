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
#[derive(Clone, Copy, Debug)]
pub struct MenuState {
    pub idx: usize,
    pub sel: usize,
    anchor: Option<MenuAnchor>,
    project: Option<ProjectId>,
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

    pub fn menu(&self) -> &'static Menu {
        &MENUS[self.idx]
    }

    /// The selected item's action.
    pub fn action(&self) -> MenuAction {
        self.menu().items[self.sel].action
    }

    /// The Project resolved by the server for a sidebar context menu.
    pub fn project(&self) -> Option<ProjectId> {
        self.project
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
    }

    pub fn prev_menu(&mut self) {
        if self.anchor.is_some() {
            return;
        }
        self.idx = (self.idx + MENU_BAR_LEN - 1) % MENU_BAR_LEN;
        self.sel = 0;
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
    let separator_rows = u16::try_from(menu.separators_before.len()).unwrap_or(u16::MAX);
    let h = (menu.items.len() as u16 + separator_rows + 2).min(rows.saturating_sub(2));
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
    .max(1);
    Rect { x, y, w, h }
}

/// Render the dropdown: shadow, box with the menu title, one line per item
/// (label + prefix shortcut), the selected line in reverse video.
pub fn render_menu(ms: &MenuState, cols: u16, rows: u16, status_top: bool, prefix: u8) -> Vec<u8> {
    let r = menu_rect(ms, cols, rows, status_top, prefix);
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
    for (i, it) in menu.items.iter().enumerate() {
        if menu.separators_before.contains(&i) {
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
        }
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
    let r = menu_rect(ms, cols, rows, status_top, prefix);
    if cx <= r.x || cx >= r.x + r.w - 1 || cy <= r.y || cy >= r.y + r.h - 1 {
        return None;
    }
    let row = (cy - r.y - 1) as usize;
    let mut separators_above = 0usize;
    for &separator in ms.menu().separators_before {
        let separator_row = separator + separators_above;
        if row == separator_row {
            return None;
        }
        if row < separator_row {
            break;
        }
        separators_above += 1;
    }
    let idx = row.saturating_sub(separators_above);
    (idx < ms.menu().items.len()).then_some(idx)
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
        assert_eq!(ms.menu().separators_before, &[2, 4, 7, 8]);
        for (offset, separator) in ms.menu().separators_before.iter().enumerate() {
            let row = separator + offset;
            assert_eq!(
                item_at(&ms, 120, 30, true, 0x01, r.x + 2, r.y + 1 + row as u16,),
                None,
            );
        }
        assert_eq!(
            item_at(&ms, 120, 30, true, 0x01, r.x + 2, r.y + 14),
            Some(ms.menu().items.len() - 1),
        );
        let bytes = render_menu(&ms, 120, 30, true, 0x01);
        let rendered = String::from_utf8_lossy(&bytes);
        assert!(rendered.contains("About Uniterm"));
        assert!(rendered.contains("Close this Workspace"));
        assert!(rendered.contains('\u{251C}'));
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
    }
}
