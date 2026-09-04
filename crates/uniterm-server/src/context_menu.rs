//! Server-owned right-click menu state and rendering.
//!
//! Context menus target server-owned Pane and file-tree identities, so they
//! live beside those models instead of duplicating hit-testing in the client.
//! They are event-driven: a mouse or key event opens, moves, or closes a menu;
//! no timer wakes the core loop while one is visible.

use std::borrow::Cow;

use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};
use uniterm_core::{PaneId, Rect, Theme};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextTarget {
    Pane(PaneId),
    File {
        path: String,
        name: String,
        is_dir: bool,
        expanded: bool,
    },
    FileRoot {
        path: String,
        show_hidden: bool,
    },
    ConfirmFileDelete {
        path: String,
        is_dir: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextAction {
    SplitRight,
    SplitDown,
    Zoom,
    Overview,
    CopyMode,
    NewTab,
    /// Re-home the Pane in another Tab of the same Project (window index).
    MoveToTab(usize),
    /// Re-home the Pane in a fresh Tab of the same Project.
    MoveToNewTab,
    ClosePane,
    Open,
    CopyPath,
    CopyRelativePath,
    NewFile,
    NewFolder,
    Rename,
    Delete,
    ConfirmFileDelete,
    Refresh,
    ToggleHidden,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextItem {
    pub label: Cow<'static, str>,
    pub action: ContextAction,
}

impl ContextItem {
    const fn fixed(label: &'static str, action: ContextAction) -> Self {
        ContextItem {
            label: Cow::Borrowed(label),
            action,
        }
    }

    /// An item whose label is decided at open time, such as a target Tab.
    pub fn dynamic(label: String, action: ContextAction) -> Self {
        ContextItem {
            label: Cow::Owned(label),
            action,
        }
    }
}

const PANE_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Split right", ContextAction::SplitRight),
    ContextItem::fixed("Split down", ContextAction::SplitDown),
    ContextItem::fixed("Zoom pane", ContextAction::Zoom),
    ContextItem::fixed("All tabs overview", ContextAction::Overview),
    ContextItem::fixed("Scrollback / copy", ContextAction::CopyMode),
    ContextItem::fixed("New tab", ContextAction::NewTab),
    ContextItem::fixed("Close pane", ContextAction::ClosePane),
];

const FILE_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Open", ContextAction::Open),
    ContextItem::fixed("Copy path", ContextAction::CopyPath),
    ContextItem::fixed("Copy relative path", ContextAction::CopyRelativePath),
    ContextItem::fixed("New file...", ContextAction::NewFile),
    ContextItem::fixed("New folder...", ContextAction::NewFolder),
    ContextItem::fixed("Rename...", ContextAction::Rename),
    ContextItem::fixed("Delete...", ContextAction::Delete),
    ContextItem::fixed("Refresh", ContextAction::Refresh),
];

const FOLDER_COLLAPSED_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Expand", ContextAction::Open),
    ContextItem::fixed("Copy path", ContextAction::CopyPath),
    ContextItem::fixed("Copy relative path", ContextAction::CopyRelativePath),
    ContextItem::fixed("New file...", ContextAction::NewFile),
    ContextItem::fixed("New folder...", ContextAction::NewFolder),
    ContextItem::fixed("Rename...", ContextAction::Rename),
    ContextItem::fixed("Delete recursively...", ContextAction::Delete),
    ContextItem::fixed("Refresh", ContextAction::Refresh),
];

const FOLDER_EXPANDED_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Collapse", ContextAction::Open),
    ContextItem::fixed("Copy path", ContextAction::CopyPath),
    ContextItem::fixed("Copy relative path", ContextAction::CopyRelativePath),
    ContextItem::fixed("New file...", ContextAction::NewFile),
    ContextItem::fixed("New folder...", ContextAction::NewFolder),
    ContextItem::fixed("Rename...", ContextAction::Rename),
    ContextItem::fixed("Delete recursively...", ContextAction::Delete),
    ContextItem::fixed("Refresh", ContextAction::Refresh),
];

const ROOT_VISIBLE_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Copy path", ContextAction::CopyPath),
    ContextItem::fixed("Copy relative path", ContextAction::CopyRelativePath),
    ContextItem::fixed("New file...", ContextAction::NewFile),
    ContextItem::fixed("New folder...", ContextAction::NewFolder),
    ContextItem::fixed("Show hidden files", ContextAction::ToggleHidden),
    ContextItem::fixed("Refresh", ContextAction::Refresh),
];

const ROOT_HIDDEN_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Copy path", ContextAction::CopyPath),
    ContextItem::fixed("Copy relative path", ContextAction::CopyRelativePath),
    ContextItem::fixed("New file...", ContextAction::NewFile),
    ContextItem::fixed("New folder...", ContextAction::NewFolder),
    ContextItem::fixed("Hide hidden files", ContextAction::ToggleHidden),
    ContextItem::fixed("Refresh", ContextAction::Refresh),
];

const CONFIRM_FILE_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Delete file", ContextAction::ConfirmFileDelete),
    ContextItem::fixed("Cancel", ContextAction::Cancel),
];

const CONFIRM_FOLDER_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Delete recursively", ContextAction::ConfirmFileDelete),
    ContextItem::fixed("Cancel", ContextAction::Cancel),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextInput {
    None,
    Redraw,
    Close,
    Run(ContextAction),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextMenu {
    pub target: ContextTarget,
    pub x: u16,
    pub y: u16,
    pub selected: usize,
    items: Vec<ContextItem>,
}

fn fixed_items(target: &ContextTarget) -> &'static [ContextItem] {
    match target {
        ContextTarget::Pane(_) => PANE_ITEMS,
        ContextTarget::File { is_dir: false, .. } => FILE_ITEMS,
        ContextTarget::File {
            is_dir: true,
            expanded: false,
            ..
        } => FOLDER_COLLAPSED_ITEMS,
        ContextTarget::File {
            is_dir: true,
            expanded: true,
            ..
        } => FOLDER_EXPANDED_ITEMS,
        ContextTarget::FileRoot {
            show_hidden: false, ..
        } => ROOT_VISIBLE_ITEMS,
        ContextTarget::FileRoot {
            show_hidden: true, ..
        } => ROOT_HIDDEN_ITEMS,
        ContextTarget::ConfirmFileDelete { is_dir: false, .. } => CONFIRM_FILE_ITEMS,
        ContextTarget::ConfirmFileDelete { is_dir: true, .. } => CONFIRM_FOLDER_ITEMS,
    }
}

impl ContextMenu {
    pub fn new(target: ContextTarget, x: u16, y: u16) -> Self {
        let items = fixed_items(&target).to_vec();
        ContextMenu {
            target,
            x,
            y,
            selected: 0,
            items,
        }
    }

    /// A Pane menu whose fixed actions are followed by one "Move to" entry per
    /// destination the server resolved at open time. Destinations are decided
    /// by the server so the menu never guesses which Tabs share the Project.
    pub fn pane(pane: PaneId, x: u16, y: u16, destinations: Vec<ContextItem>) -> Self {
        let mut items = PANE_ITEMS.to_vec();
        let close = items.pop();
        items.extend(destinations);
        items.extend(close);
        ContextMenu {
            target: ContextTarget::Pane(pane),
            x,
            y,
            selected: 0,
            items,
        }
    }

    pub fn title(&self) -> &'static str {
        match self.target {
            ContextTarget::Pane(_) => "Pane",
            ContextTarget::File { is_dir: true, .. } | ContextTarget::FileRoot { .. } => "Folder",
            ContextTarget::File { is_dir: false, .. } => "File",
            ContextTarget::ConfirmFileDelete { .. } => "Confirm delete",
        }
    }

    pub fn items(&self) -> &[ContextItem] {
        &self.items
    }

    pub fn rect(&self, cols: u16, rows: u16) -> Rect {
        let content_width = self
            .items()
            .iter()
            .map(|item| item.label.width())
            .chain(std::iter::once(self.title().width() + 2))
            .max()
            .unwrap_or(8);
        let width = u16::try_from(content_width.saturating_add(4))
            .unwrap_or(u16::MAX)
            .min(cols.max(1));
        let height = u16::try_from(self.items().len().saturating_add(2))
            .unwrap_or(u16::MAX)
            .min(rows.max(1));
        let x = self.x.min(cols.saturating_sub(width));
        let y = self.y.min(rows.saturating_sub(height));
        Rect::new(x, y, width, height)
    }

    pub fn item_at(&self, cols: u16, rows: u16, x: u16, y: u16) -> Option<usize> {
        let rect = self.rect(cols, rows);
        if x <= rect.x
            || x >= rect.right().saturating_sub(1)
            || y <= rect.y
            || y >= rect.bottom().saturating_sub(1)
        {
            return None;
        }
        let item = usize::from(y - rect.y - 1);
        (item < self.items().len()).then_some(item)
    }

    pub fn action_at(&self, cols: u16, rows: u16, x: u16, y: u16) -> Option<ContextAction> {
        self.item_at(cols, rows, x, y)
            .and_then(|index| self.items().get(index))
            .map(|item| item.action)
    }

    pub fn hover(&mut self, cols: u16, rows: u16, x: u16, y: u16) -> bool {
        let Some(selected) = self.item_at(cols, rows, x, y) else {
            return false;
        };
        if self.selected == selected {
            return false;
        }
        self.selected = selected;
        true
    }

    pub fn handle(&mut self, input: &[u8]) -> ContextInput {
        let mut changed = false;
        let mut index = 0;
        while index < input.len() {
            if input[index..].starts_with(b"\x1b[A") {
                let len = self.items().len();
                self.selected = (self.selected + len - 1) % len;
                changed = true;
                index += 3;
                continue;
            }
            if input[index..].starts_with(b"\x1b[B") {
                self.selected = (self.selected + 1) % self.items().len();
                changed = true;
                index += 3;
                continue;
            }
            match input[index] {
                0x1b | 0x03 | b'q' => return ContextInput::Close,
                b'\r' | b'\n' => {
                    return self
                        .items()
                        .get(self.selected)
                        .map(|item| ContextInput::Run(item.action))
                        .unwrap_or(ContextInput::None);
                }
                b'k' => {
                    let len = self.items().len();
                    self.selected = (self.selected + len - 1) % len;
                    changed = true;
                }
                b'j' => {
                    self.selected = (self.selected + 1) % self.items().len();
                    changed = true;
                }
                _ => {}
            }
            index += 1;
        }
        if changed {
            ContextInput::Redraw
        } else {
            ContextInput::None
        }
    }

    pub fn render(&self, theme: &Theme, cols: u16, rows: u16, out: &mut Vec<u8>) {
        let rect = self.rect(cols, rows);
        if rect.w < 2 || rect.h < 2 {
            return;
        }
        let inner = usize::from(rect.w - 2);
        let border = format!(
            "\x1b[0;{};{}m",
            theme.border_active.sgr_fg(),
            theme.surface.sgr_bg()
        );
        let base = format!(
            "\x1b[0;{};{}m",
            theme.foreground.sgr_fg(),
            theme.surface.sgr_bg()
        );
        let selected = format!(
            "\x1b[0;1;{};{}m",
            theme.status_active_fg.sgr_fg(),
            theme.status_active_bg.sgr_bg()
        );

        let title = clip_cells(&format!(" {} ", self.title()), inner);
        let mut top = String::from("\u{250C}");
        top.push_str(&title);
        top.extend(std::iter::repeat_n(
            '\u{2500}',
            inner.saturating_sub(title.width()),
        ));
        top.push('\u{2510}');
        out.extend_from_slice(
            format!("\x1b[{};{}H{border}{top}", rect.y + 1, rect.x + 1).as_bytes(),
        );

        for row in 0..usize::from(rect.h.saturating_sub(2)) {
            let Some(item) = self.items().get(row) else {
                break;
            };
            let style = if row == self.selected {
                selected.as_str()
            } else {
                base.as_str()
            };
            let label = fit_cells(&format!(" {}", item.label), inner);
            out.extend_from_slice(
                format!(
                    "\x1b[{};{}H{border}\u{2502}{style}{label}{border}\u{2502}",
                    rect.y + row as u16 + 2,
                    rect.x + 1
                )
                .as_bytes(),
            );
        }

        let bottom = format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(inner));
        out.extend_from_slice(
            format!(
                "\x1b[{};{}H{border}{bottom}\x1b[0m",
                rect.bottom(),
                rect.x + 1
            )
            .as_bytes(),
        );
    }
}

fn fit_cells(text: &str, width: usize) -> String {
    let mut fitted = clip_cells(text, width);
    let used = fitted.width();
    fitted.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    fitted
}

fn clip_cells(text: &str, width: usize) -> String {
    let mut clipped = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let char_width = ch.width().unwrap_or(0);
        if used.saturating_add(char_width) > width {
            break;
        }
        clipped.push(ch);
        used = used.saturating_add(char_width);
    }
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_menu_has_the_expected_mouse_first_actions() {
        let menu = ContextMenu::new(ContextTarget::Pane(PaneId(7)), 30, 20);
        let labels: Vec<&str> = menu
            .items()
            .iter()
            .map(|item| item.label.as_ref())
            .collect();
        assert_eq!(
            labels,
            vec![
                "Split right",
                "Split down",
                "Zoom pane",
                "All tabs overview",
                "Scrollback / copy",
                "New tab",
                "Close pane",
            ]
        );
    }

    #[test]
    fn file_menu_exposes_both_path_forms_and_mutations() {
        let menu = ContextMenu::new(
            ContextTarget::File {
                path: "/work/src/lib.rs".into(),
                name: "lib.rs".into(),
                is_dir: false,
                expanded: false,
            },
            70,
            20,
        );
        let labels: Vec<&str> = menu
            .items()
            .iter()
            .map(|item| item.label.as_ref())
            .collect();
        assert!(labels.contains(&"Copy path"));
        assert!(labels.contains(&"Copy relative path"));
        assert!(labels.contains(&"New file..."));
        assert!(labels.contains(&"New folder..."));
        assert!(labels.contains(&"Rename..."));
        assert!(labels.contains(&"Delete..."));
    }

    #[test]
    fn menu_clamps_to_screen_and_hit_tests_visible_rows() {
        let mut menu = ContextMenu::new(ContextTarget::Pane(PaneId(1)), 78, 23);
        let rect = menu.rect(80, 24);
        assert!(rect.right() <= 80);
        assert!(rect.bottom() <= 24);
        let x = rect.x + 1;
        let y = rect.y + 2;
        assert_eq!(menu.action_at(80, 24, x, y), Some(ContextAction::SplitDown));
        assert!(menu.hover(80, 24, x, y));
        assert_eq!(menu.selected, 1);
    }

    #[test]
    fn keyboard_navigation_wraps_and_submits() {
        let mut menu = ContextMenu::new(ContextTarget::Pane(PaneId(1)), 0, 0);
        assert_eq!(menu.handle(b"k"), ContextInput::Redraw);
        assert_eq!(menu.selected, menu.items().len() - 1);
        assert_eq!(
            menu.handle(b"\r"),
            ContextInput::Run(ContextAction::ClosePane)
        );
        assert_eq!(menu.handle(b"\x1b"), ContextInput::Close);

        let mut menu = ContextMenu::new(ContextTarget::Pane(PaneId(1)), 0, 0);
        assert_eq!(menu.handle(b"jj\r"), ContextInput::Run(ContextAction::Zoom));
    }
}
