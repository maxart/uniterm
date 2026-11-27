//! Server-owned right-click menu state and rendering.
//!
//! Context menus target server-owned Pane and file-tree identities, so they
//! live beside those models instead of duplicating hit-testing in the client.
//! They are event-driven: a mouse or key event opens, moves, or closes a menu;
//! no timer wakes the core loop while one is visible.

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;
use uniterm_core::{PaneId, Rect, Theme};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextTarget {
    Pane(PaneId),
    /// Keep actions bound to the selected invocation if the agent list changes.
    Agent {
        pane: PaneId,
        started_at: std::time::Instant,
        foreground_pid: Option<i32>,
    },
    /// Pin a server menu to the detected item even if the list changes.
    DevServer {
        pane: PaneId,
        port: u16,
        detected: u64,
    },
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
    /// Focus the selected agent's exact Tab and Pane.
    FocusAgent,
    /// Copy the selected agent Pane's current working directory.
    CopyAgentPath,
    /// Stop the selected agent through the shared single-agent stop action.
    StopAgent,
    /// Open the selected server's URL through the attached client.
    OpenServer,
    /// Copy the selected server's normalized URL to the attached client's clipboard.
    CopyServerAddress,
    /// Focus the exact Pane that announced the selected server.
    FocusServer,
    /// Stop the selected listener while retaining its source shell.
    StopServer,
    /// Choose the selected Pane's Tab arrangement.
    PaneLayout(uniterm_core::layout::PaneLayout),
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
    /// Reveal the nested list of Tabs the Pane can move to. Navigation only:
    /// the menu consumes it and never hands it to the dispatcher.
    OpenMoveSubmenu,
    /// Return from a nested list to its parent menu. Navigation only.
    Back,
    ClosePane,
    Open,
    /// Open a directory, or a file's parent, using the host desktop handler.
    OpenFileManager,
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
    /// A Pane that identified the destination when the menu opened. A Tab has
    /// no identity of its own, only a position that shifts when another Tab
    /// closes, so the dispatcher re-resolves a positional action through this
    /// Pane instead of trusting an index that went stale under an open menu.
    pub anchor: Option<PaneId>,
}

impl ContextItem {
    const fn fixed(label: &'static str, action: ContextAction) -> Self {
        ContextItem {
            label: Cow::Borrowed(label),
            action,
            anchor: None,
        }
    }

    /// An item whose label is decided at open time, such as a target Tab.
    pub fn dynamic(label: String, action: ContextAction) -> Self {
        ContextItem {
            label: Cow::Owned(label),
            action,
            anchor: None,
        }
    }

    /// Pin a positional destination to a Pane that lives in it.
    pub fn anchored(mut self, pane: PaneId) -> Self {
        self.anchor = Some(pane);
        self
    }
}

/// The single Pane-menu row that stands for every Tab destination.
const MOVE_ITEM: ContextItem = ContextItem::fixed("Move to tab...", ContextAction::OpenMoveSubmenu);
const BACK_ITEM: ContextItem = ContextItem::fixed("\u{25C2} Back", ContextAction::Back);
const MOVE_TITLE: &str = "Move to tab";
/// Most rows the nested list shows at once; a longer list scrolls inside
/// this viewport so the box keeps a stable size and position.
const SUBMENU_ROWS: usize = 8;

const PANE_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Split right", ContextAction::SplitRight),
    ContextItem::fixed("Split down", ContextAction::SplitDown),
    ContextItem::fixed("Zoom pane", ContextAction::Zoom),
    ContextItem::fixed("All tabs overview", ContextAction::Overview),
    ContextItem::fixed("Scrollback / copy", ContextAction::CopyMode),
    ContextItem::fixed("New tab", ContextAction::NewTab),
    ContextItem::fixed("Close pane", ContextAction::ClosePane),
    ContextItem::fixed(
        "Split layout",
        ContextAction::PaneLayout(uniterm_core::layout::PaneLayout::Split),
    ),
    ContextItem::fixed(
        "Dwindle layout",
        ContextAction::PaneLayout(uniterm_core::layout::PaneLayout::Dwindle),
    ),
    ContextItem::fixed(
        "Scrolling layout",
        ContextAction::PaneLayout(uniterm_core::layout::PaneLayout::Scrolling),
    ),
];

const SERVER_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Open in browser", ContextAction::OpenServer),
    ContextItem::fixed("Go to source pane", ContextAction::FocusServer),
    ContextItem::fixed("Kill server", ContextAction::StopServer),
    ContextItem::fixed("Copy server address", ContextAction::CopyServerAddress),
];

const AGENT_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Go to agent pane", ContextAction::FocusAgent),
    ContextItem::fixed("Copy working path", ContextAction::CopyAgentPath),
    ContextItem::fixed("Stop agent", ContextAction::StopAgent),
];

const FILE_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Open", ContextAction::Open),
    ContextItem::fixed("Open in file manager", ContextAction::OpenFileManager),
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
    ContextItem::fixed("Open in file manager", ContextAction::OpenFileManager),
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
    ContextItem::fixed("Open in file manager", ContextAction::OpenFileManager),
    ContextItem::fixed("Copy path", ContextAction::CopyPath),
    ContextItem::fixed("Copy relative path", ContextAction::CopyRelativePath),
    ContextItem::fixed("New file...", ContextAction::NewFile),
    ContextItem::fixed("New folder...", ContextAction::NewFolder),
    ContextItem::fixed("Rename...", ContextAction::Rename),
    ContextItem::fixed("Delete recursively...", ContextAction::Delete),
    ContextItem::fixed("Refresh", ContextAction::Refresh),
];

const ROOT_VISIBLE_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Open in file manager", ContextAction::OpenFileManager),
    ContextItem::fixed("Copy path", ContextAction::CopyPath),
    ContextItem::fixed("Copy relative path", ContextAction::CopyRelativePath),
    ContextItem::fixed("New file...", ContextAction::NewFile),
    ContextItem::fixed("New folder...", ContextAction::NewFolder),
    ContextItem::fixed("Show hidden files", ContextAction::ToggleHidden),
    ContextItem::fixed("Refresh", ContextAction::Refresh),
];

const ROOT_HIDDEN_ITEMS: &[ContextItem] = &[
    ContextItem::fixed("Open in file manager", ContextAction::OpenFileManager),
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

/// The nested destination list hanging from one row of its parent menu.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Submenu {
    /// Index of the parent item this list was opened from.
    parent: usize,
    selected: usize,
    /// First visible row; the list scrolls inside a bounded viewport.
    first: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextMenu {
    pub target: ContextTarget,
    pub x: u16,
    pub y: u16,
    pub selected: usize,
    items: Vec<ContextItem>,
    active_layout: Option<uniterm_core::layout::PaneLayout>,
    /// First visible row of the top-level list on a screen too short for it.
    first: usize,
    /// Rows shared by rendering, navigation and destination anchor repair.
    destinations: Vec<ContextItem>,
    /// All labels are fixed at open time, including their maximum display width.
    destination_width: u16,
    submenu: Option<Submenu>,
}

fn fixed_items(target: &ContextTarget) -> &'static [ContextItem] {
    match target {
        ContextTarget::Pane(_) => PANE_ITEMS,
        ContextTarget::Agent { .. } => AGENT_ITEMS,
        ContextTarget::DevServer { .. } => SERVER_ITEMS,
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

/// Rows a list occupies: one per item plus its separator, if it has one.
fn row_count(len: usize, separator: Option<usize>) -> usize {
    len + usize::from(separator.is_some())
}

fn item_for_row(len: usize, separator: Option<usize>, row: usize) -> Option<usize> {
    let item = match separator {
        Some(separator) if row == separator => return None,
        Some(separator) if row > separator => row - 1,
        _ => row,
    };
    (item < len).then_some(item)
}

fn row_for_item(separator: Option<usize>, item: usize) -> usize {
    item + usize::from(separator.is_some_and(|separator| item >= separator))
}

/// The scroll offset that keeps `row` inside a viewport of `visible` rows,
/// moving it no further than needed and never past the end of the list.
fn reveal(first: usize, row: usize, visible: usize, total: usize) -> usize {
    let first = if row < first {
        row
    } else if visible > 0 && row >= first + visible {
        row + 1 - visible
    } else {
        first
    };
    first.min(total.saturating_sub(visible))
}

/// What one screen cell means to a menu box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    Outside,
    /// A border, separator, or empty cell: inside the box, bound to nothing.
    Inert,
    Item(usize),
    ScrollUp,
    ScrollDown,
}

/// One bordered list. The top-level menu and the nested list are both drawn
/// and hit-tested through this, so their rows can never disagree.
struct Panel<'a> {
    rect: Rect,
    title: &'a str,
    items: &'a [ContextItem],
    separator: Option<usize>,
    first: usize,
    selected: usize,
    active_layout: Option<uniterm_core::layout::PaneLayout>,
}

/// A horizontal border run whose last cell may carry a scroll marker.
fn push_rule(line: &mut String, width: usize, marker: Option<char>) {
    match marker {
        Some(marker) if width > 0 => {
            line.extend(std::iter::repeat_n('\u{2500}', width - 1));
            line.push(marker);
        }
        _ => line.extend(std::iter::repeat_n('\u{2500}', width)),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Escape,
    Quit,
    Other,
}

impl ContextMenu {
    pub fn new(target: ContextTarget, x: u16, y: u16) -> Self {
        let items = fixed_items(&target).to_vec();
        ContextMenu {
            active_layout: matches!(target, ContextTarget::Pane(_))
                .then_some(uniterm_core::layout::PaneLayout::Split),
            target,
            x,
            y,
            selected: 0,
            items,
            first: 0,
            destinations: Vec::new(),
            destination_width: 0,
            submenu: None,
        }
    }

    /// A Pane menu whose fixed actions gain one "Move to tab..." row when the
    /// server resolved any destination at open time. The destinations live in
    /// a nested list, so the top-level rows keep the same positions however
    /// many Tabs the Project has. Destinations are decided by the server so
    /// the menu never guesses which Tabs share the Project.
    pub fn pane(
        pane: PaneId,
        x: u16,
        y: u16,
        mut destinations: Vec<ContextItem>,
        layout: uniterm_core::layout::PaneLayout,
    ) -> Self {
        let mut items = PANE_ITEMS.to_vec();
        if !destinations.is_empty() {
            let close = items
                .iter()
                .position(|item| item.action == ContextAction::ClosePane)
                .unwrap_or(items.len());
            items.insert(close, MOVE_ITEM);
            destinations.insert(0, BACK_ITEM);
        }
        let widest = destinations
            .iter()
            .map(|item| item.label.width())
            .max()
            .unwrap_or(0);
        let destination_width =
            u16::try_from(widest.max(MOVE_TITLE.width() + 2).saturating_add(4)).unwrap_or(u16::MAX);
        ContextMenu {
            target: ContextTarget::Pane(pane),
            x,
            y,
            selected: 0,
            items,
            active_layout: Some(layout),
            first: 0,
            destinations,
            destination_width,
            submenu: None,
        }
    }

    fn separator_index(&self) -> Option<usize> {
        self.items.iter().position(|item| {
            matches!(
                item.action,
                ContextAction::PaneLayout(uniterm_core::layout::PaneLayout::Split)
            )
        })
    }

    pub fn title(&self) -> &'static str {
        match self.target {
            ContextTarget::Pane(_) => "Pane",
            ContextTarget::Agent { .. } => "Agent",
            ContextTarget::DevServer { .. } => "Server",
            ContextTarget::File { is_dir: true, .. } | ContextTarget::FileRoot { .. } => "Folder",
            ContextTarget::File { is_dir: false, .. } => "File",
            ContextTarget::ConfirmFileDelete { .. } => "Confirm delete",
        }
    }

    pub fn items(&self) -> &[ContextItem] {
        &self.items
    }

    /// Rows of the open nested list, Back first; empty while it is closed.
    #[cfg(test)]
    fn submenu_items(&self) -> &[ContextItem] {
        self.submenu
            .as_ref()
            .map_or(&[], |_| self.destinations.as_slice())
    }

    /// The Pane that identified `action`'s destination when the menu opened.
    pub fn anchor_for(&self, action: ContextAction) -> Option<PaneId> {
        self.destinations
            .iter()
            .find(|item| item.action == action)
            .and_then(|item| item.anchor)
    }

    /// Keep a still-live destination Tab identifiable when its anchor Pane exits.
    pub fn replace_destination_anchor(&mut self, exiting: PaneId, survivor: PaneId) {
        if let Some(item) = self
            .destinations
            .iter_mut()
            .find(|item| item.anchor == Some(exiting))
        {
            item.anchor = Some(survivor);
        }
    }

    pub fn rect(&self, cols: u16, rows: u16) -> Rect {
        let content_width = self
            .items()
            .iter()
            .map(|item| {
                item.label.width()
                    + if matches!(
                        item.action,
                        ContextAction::PaneLayout(_) | ContextAction::OpenMoveSubmenu
                    ) {
                        2
                    } else {
                        0
                    }
            })
            .chain(std::iter::once(self.title().width() + 2))
            .max()
            .unwrap_or(8);
        let width = u16::try_from(content_width.saturating_add(4))
            .unwrap_or(u16::MAX)
            .min(cols.max(1));
        let height =
            u16::try_from(row_count(self.items.len(), self.separator_index()).saturating_add(2))
                .unwrap_or(u16::MAX)
                .min(rows.max(1));
        let x = self.x.min(cols.saturating_sub(width));
        let y = self.y.min(rows.saturating_sub(height));
        Rect::new(x, y, width, height)
    }

    /// Where the nested list is drawn while open. Its first row sits on the
    /// parent row. It opens to the right of the menu, then to the left, and
    /// only covers the menu when the screen has room for neither.
    pub fn submenu_rect(&self, cols: u16, rows: u16) -> Option<Rect> {
        let sub = self.submenu.as_ref()?;
        let main = self.main_panel(cols, rows);
        let width = self.destination_width.min(cols.max(1));
        let height = u16::try_from(self.destinations.len().min(SUBMENU_ROWS) + 2)
            .unwrap_or(u16::MAX)
            .min(rows.max(1));
        let parent_row = row_for_item(main.separator, sub.parent)
            .saturating_sub(main.first)
            .min(main.visible().saturating_sub(1));
        let parent_y = main
            .rect
            .y
            .saturating_add(1)
            .saturating_add(u16::try_from(parent_row).unwrap_or(u16::MAX));
        let y = parent_y.saturating_sub(1).min(rows.saturating_sub(height));
        let x = if u32::from(main.rect.right()) + u32::from(width) <= u32::from(cols) {
            main.rect.right()
        } else if main.rect.x >= width {
            main.rect.x - width
        } else {
            cols.saturating_sub(width)
        };
        Some(Rect::new(x, y, width, height))
    }

    fn main_panel(&self, cols: u16, rows: u16) -> Panel<'_> {
        let rect = self.rect(cols, rows);
        let separator = self.separator_index();
        let visible = usize::from(rect.h.saturating_sub(2));
        Panel {
            rect,
            title: self.title(),
            items: &self.items,
            separator,
            first: reveal(
                self.first,
                row_for_item(separator, self.selected),
                visible,
                row_count(self.items.len(), separator),
            ),
            selected: self.selected,
            active_layout: self.active_layout,
        }
    }

    fn submenu_panel(&self, cols: u16, rows: u16) -> Option<Panel<'_>> {
        let sub = self.submenu.as_ref()?;
        let rect = self.submenu_rect(cols, rows)?;
        let visible = usize::from(rect.h.saturating_sub(2));
        Some(Panel {
            rect,
            title: MOVE_TITLE,
            items: &self.destinations,
            separator: None,
            first: reveal(sub.first, sub.selected, visible, self.destinations.len()),
            selected: sub.selected,
            active_layout: None,
        })
    }

    /// Preserve each visible selection across terminal resizes and subsequent pointer hits.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.first = self.main_panel(cols, rows).first;
        let first = self.submenu_panel(cols, rows).map(|panel| panel.first);
        if let (Some(sub), Some(first)) = (&mut self.submenu, first) {
            sub.first = first;
        }
    }

    /// Right-clicks on painted cells must not reach the surface underneath.
    pub fn contains(&self, cols: u16, rows: u16, x: u16, y: u16) -> bool {
        self.rect(cols, rows).contains(x, y)
            || self
                .submenu_rect(cols, rows)
                .is_some_and(|rect| rect.contains(x, y))
    }

    fn submenu_hit(&self, cols: u16, rows: u16, x: u16, y: u16) -> Hit {
        self.submenu_panel(cols, rows)
            .map_or(Hit::Outside, |panel| panel.hit(x, y))
    }

    /// The action bound to a cell, in whichever list is on top there.
    #[cfg(test)]
    pub fn action_at(&self, cols: u16, rows: u16, x: u16, y: u16) -> Option<ContextAction> {
        let item = match self.submenu_hit(cols, rows, x, y) {
            Hit::Item(index) => self.destinations.get(index),
            Hit::Outside => match self.main_panel(cols, rows).hit(x, y) {
                Hit::Item(index) => self.items.get(index),
                _ => None,
            },
            _ => None,
        };
        item.map(|item| item.action)
    }

    /// Follow the pointer. While the nested list is open only its rows react:
    /// travelling diagonally towards a destination crosses other parent rows,
    /// and that must not close the list or arm a different action.
    pub fn hover(&mut self, cols: u16, rows: u16, x: u16, y: u16) -> bool {
        if self.submenu.is_some() {
            let Hit::Item(index) = self.submenu_hit(cols, rows, x, y) else {
                return false;
            };
            return self
                .submenu
                .as_mut()
                .is_some_and(|sub| std::mem::replace(&mut sub.selected, index) != index);
        }
        let Hit::Item(index) = self.main_panel(cols, rows).hit(x, y) else {
            return false;
        };
        std::mem::replace(&mut self.selected, index) != index
    }

    /// A primary click, including a bare release from a release-only terminal.
    /// The attachment consumes the rest of a captured gesture. Navigation never runs
    /// an action, borders and separators are inert, and only a cell outside
    /// every box dismisses the menu.
    pub fn click(&mut self, cols: u16, rows: u16, x: u16, y: u16) -> ContextInput {
        if let Some(parent) = self.submenu.as_ref().map(|sub| sub.parent) {
            match self.submenu_hit(cols, rows, x, y) {
                Hit::Item(index) => {
                    let action = self.destinations.get(index).map(|item| item.action);
                    return match action {
                        Some(ContextAction::Back) => {
                            self.submenu = None;
                            ContextInput::Redraw
                        }
                        Some(action) => ContextInput::Run(action),
                        None => ContextInput::None,
                    };
                }
                hit @ (Hit::ScrollUp | Hit::ScrollDown) => {
                    return if self.scroll(cols, rows, hit == Hit::ScrollDown) {
                        ContextInput::Redraw
                    } else {
                        ContextInput::None
                    };
                }
                Hit::Inert => return ContextInput::None,
                Hit::Outside => {}
            }
            // The parent menu behind an open list is a way back, not a second
            // set of live actions: its highlight does not follow the pointer
            // there, so a press must not run a row the user never saw armed.
            return match self.main_panel(cols, rows).hit(x, y) {
                Hit::Outside => ContextInput::Close,
                Hit::Item(index) if index == parent => ContextInput::None,
                hit => {
                    self.submenu = None;
                    if let Hit::Item(index) = hit {
                        self.selected = index;
                    }
                    ContextInput::Redraw
                }
            };
        }
        match self.main_panel(cols, rows).hit(x, y) {
            Hit::Outside => ContextInput::Close,
            Hit::Inert => ContextInput::None,
            hit @ (Hit::ScrollUp | Hit::ScrollDown) => {
                if self.scroll(cols, rows, hit == Hit::ScrollDown) {
                    ContextInput::Redraw
                } else {
                    ContextInput::None
                }
            }
            Hit::Item(index) => {
                self.selected = index;
                match self.items.get(index).map(|item| item.action) {
                    Some(ContextAction::OpenMoveSubmenu) => {
                        // Opened by pointer: the highlight starts on Back, so
                        // no destination is armed until the pointer picks one.
                        self.open_submenu(index, false);
                        ContextInput::Redraw
                    }
                    Some(action) => ContextInput::Run(action),
                    None => ContextInput::None,
                }
            }
        }
    }

    /// Wheel input. A list longer than its viewport scrolls, stopping at
    /// either end; a list that fits moves its selection as the keys do.
    /// Returns whether anything visible changed.
    pub fn wheel(&mut self, cols: u16, rows: u16, up: bool) -> bool {
        let overflows = match self.submenu_panel(cols, rows) {
            Some(panel) => panel.total() > panel.visible(),
            None => {
                let panel = self.main_panel(cols, rows);
                panel.total() > panel.visible()
            }
        };
        if overflows {
            self.scroll(cols, rows, !up)
        } else {
            self.step(cols, rows, !up);
            true
        }
    }

    fn open_submenu(&mut self, parent: usize, keyboard: bool) {
        self.selected = parent;
        self.submenu = Some(Submenu {
            parent,
            selected: usize::from(keyboard && self.destinations.len() > 1),
            first: 0,
        });
    }

    /// Move the active list's selection by one item, wrapping at the ends and
    /// scrolling just far enough to keep it visible.
    fn step(&mut self, cols: u16, rows: u16, forward: bool) {
        let next = |selected: usize, len: usize| {
            if forward {
                (selected + 1) % len
            } else {
                (selected + len - 1) % len
            }
        };
        if self.submenu.is_some() {
            let visible = self
                .submenu_panel(cols, rows)
                .map_or(0, |panel| panel.visible());
            if let Some(sub) = self.submenu.as_mut() {
                sub.selected = next(sub.selected, self.destinations.len());
                sub.first = reveal(sub.first, sub.selected, visible, self.destinations.len());
            }
            return;
        }
        if self.items.is_empty() {
            return;
        }
        self.selected = next(self.selected, self.items.len());
        let panel = self.main_panel(cols, rows);
        let first = reveal(
            panel.first,
            row_for_item(panel.separator, self.selected),
            panel.visible(),
            panel.total(),
        );
        self.first = first;
    }

    /// Scroll the active list's viewport by one row without wrapping, keeping
    /// the selection on a visible item. Returns false at either end.
    fn scroll(&mut self, cols: u16, rows: u16, down: bool) -> bool {
        let (first, visible, total, separator) = match self.submenu_panel(cols, rows) {
            Some(panel) => (panel.first, panel.visible(), panel.total(), None),
            None => {
                let panel = self.main_panel(cols, rows);
                (panel.first, panel.visible(), panel.total(), panel.separator)
            }
        };
        let mut next = if down {
            (first + 1).min(total.saturating_sub(visible))
        } else {
            first.saturating_sub(1)
        };
        if visible == 1 && separator == Some(next) {
            next = if down { next + 1 } else { next - 1 };
        }
        if next == first || visible == 0 {
            return false;
        }
        let last = next + visible - 1;
        if let Some(sub) = self.submenu.as_mut() {
            sub.first = next;
            sub.selected = sub.selected.clamp(next, last);
            return true;
        }
        self.first = next;
        let len = self.items.len();
        let row = row_for_item(separator, self.selected).clamp(next, last);
        // A viewport edge can rest on the separator; take the item just
        // inside it so the selection is always something actionable.
        let inside = if down { row + 1 } else { row.saturating_sub(1) };
        if let Some(item) =
            item_for_row(len, separator, row).or_else(|| item_for_row(len, separator, inside))
        {
            self.selected = item;
        }
        true
    }

    /// Keyboard input. Arrows and `hjkl` navigate; Enter or Right on the
    /// "Move to tab..." row opens the nested list; Left, Backspace or Escape
    /// steps back out of it; Escape at the top level, `q` or Ctrl-C dismisses.
    pub fn handle(&mut self, cols: u16, rows: u16, input: &[u8]) -> ContextInput {
        let mut changed = false;
        let mut index = 0;
        while index < input.len() {
            let (key, used) = match &input[index..] {
                [0x1b, b'[' | b'O', b'A', ..] => (Key::Up, 3),
                [0x1b, b'[' | b'O', b'B', ..] => (Key::Down, 3),
                [0x1b, b'[' | b'O', b'C', ..] => (Key::Right, 3),
                [0x1b, b'[' | b'O', b'D', ..] => (Key::Left, 3),
                [0x1b, ..] => (Key::Escape, 1),
                [b'k', ..] => (Key::Up, 1),
                [b'j', ..] => (Key::Down, 1),
                [b'l', ..] => (Key::Right, 1),
                [b'h' | 0x7f | 0x08, ..] => (Key::Left, 1),
                [b'\r' | b'\n', ..] => (Key::Enter, 1),
                [0x03 | b'q', ..] => (Key::Quit, 1),
                _ => (Key::Other, 1),
            };
            index += used;
            match key {
                Key::Up | Key::Down => {
                    self.step(cols, rows, key == Key::Down);
                    changed = true;
                }
                Key::Quit => return ContextInput::Close,
                Key::Escape | Key::Left if self.submenu.is_some() => {
                    self.submenu = None;
                    changed = true;
                }
                Key::Escape => return ContextInput::Close,
                Key::Right | Key::Enter if rows <= 2 || cols <= 2 => {}
                Key::Right | Key::Enter => {
                    if let Some(sub) = &self.submenu {
                        if key == Key::Enter {
                            match self.destinations.get(sub.selected).map(|item| item.action) {
                                Some(ContextAction::Back) => {
                                    self.submenu = None;
                                    changed = true;
                                }
                                Some(action) => return ContextInput::Run(action),
                                None => {}
                            }
                        }
                        continue;
                    }
                    match self.items.get(self.selected).map(|item| item.action) {
                        Some(ContextAction::OpenMoveSubmenu) => {
                            // Opened by key: start on the first destination,
                            // since Left and Escape already lead back.
                            self.open_submenu(self.selected, true);
                            changed = true;
                        }
                        Some(action) if key == Key::Enter => return ContextInput::Run(action),
                        _ => {}
                    }
                }
                Key::Left | Key::Other => {}
            }
        }
        if changed {
            ContextInput::Redraw
        } else {
            ContextInput::None
        }
    }

    pub fn render(&self, theme: &Theme, cols: u16, rows: u16, out: &mut Vec<u8>) {
        self.main_panel(cols, rows).render(theme, out);
        if let Some(panel) = self.submenu_panel(cols, rows) {
            panel.render(theme, out);
        }
    }
}

impl Panel<'_> {
    fn visible(&self) -> usize {
        usize::from(self.rect.h.saturating_sub(2))
    }

    fn total(&self) -> usize {
        row_count(self.items.len(), self.separator)
    }

    fn more_above(&self) -> bool {
        self.first > 0
    }

    fn more_below(&self) -> bool {
        self.first + self.visible() < self.total()
    }

    fn hit(&self, x: u16, y: u16) -> Hit {
        let rect = self.rect;
        if !rect.contains(x, y) {
            return Hit::Outside;
        }
        if y == rect.y {
            return if self.more_above() {
                Hit::ScrollUp
            } else {
                Hit::Inert
            };
        }
        if y + 1 == rect.bottom() {
            return if self.more_below() {
                Hit::ScrollDown
            } else {
                Hit::Inert
            };
        }
        if x == rect.x || x + 1 == rect.right() {
            return Hit::Inert;
        }
        let row = self.first + usize::from(y - rect.y - 1);
        item_for_row(self.items.len(), self.separator, row).map_or(Hit::Inert, Hit::Item)
    }

    fn render(&self, theme: &Theme, out: &mut Vec<u8>) {
        let rect = self.rect;
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

        let title = clip_cells(&format!(" {} ", self.title), inner);
        let mut top = String::from("\u{250C}");
        top.push_str(&title);
        push_rule(
            &mut top,
            inner.saturating_sub(title.width()),
            self.more_above().then_some('\u{25B2}'),
        );
        top.push('\u{2510}');
        out.extend_from_slice(
            format!("\x1b[{};{}H{border}{top}", rect.y + 1, rect.x + 1).as_bytes(),
        );

        for slot in 0..self.visible() {
            let row = self.first + slot;
            let y = rect.y + slot as u16 + 2;
            if self.separator == Some(row) {
                out.extend_from_slice(
                    format!("\x1b[{y};{}H{border}├{}┤", rect.x + 1, "─".repeat(inner)).as_bytes(),
                );
                continue;
            }
            let Some(index) = item_for_row(self.items.len(), self.separator, row) else {
                break;
            };
            let item = &self.items[index];
            let style = if index == self.selected {
                selected.as_str()
            } else {
                base.as_str()
            };
            let label = match item.action {
                ContextAction::PaneLayout(mode) => fit_cells(
                    &format!(
                        " {} {}",
                        if self.active_layout == Some(mode) {
                            "●"
                        } else {
                            " "
                        },
                        item.label
                    ),
                    inner,
                ),
                // The marker keeps its own trailing cells so a long label is
                // clipped before it instead of pushing it out of the box.
                ContextAction::OpenMoveSubmenu if inner > 2 => {
                    let mut label = fit_cells(&format!(" {}", item.label), inner - 2);
                    label.push_str("\u{25B8} ");
                    label
                }
                _ => fit_cells(&format!(" {}", item.label), inner),
            };
            out.extend_from_slice(
                format!(
                    "\x1b[{y};{}H{border}\u{2502}{style}{label}{border}\u{2502}",
                    rect.x + 1
                )
                .as_bytes(),
            );
        }

        let mut bottom = String::from("\u{2514}");
        push_rule(&mut bottom, inner, self.more_below().then_some('\u{25BC}'));
        bottom.push('\u{2518}');
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
    for grapheme in text.graphemes(true) {
        if grapheme.chars().any(char::is_control) {
            continue;
        }
        let cells = grapheme.width();
        if used.saturating_add(cells) > width {
            break;
        }
        clipped.push_str(grapheme);
        used += cells;
    }
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_menu_shares_visible_geometry_with_mouse_and_keyboard_actions() {
        let mut menu = ContextMenu::new(
            ContextTarget::Agent {
                pane: PaneId(7),
                started_at: std::time::Instant::now(),
                foreground_pid: Some(42),
            },
            95,
            8,
        );
        assert_eq!(menu.title(), "Agent");
        assert_eq!(
            menu.items()
                .iter()
                .map(|item| item.label.as_ref())
                .collect::<Vec<_>>(),
            ["Go to agent pane", "Copy working path", "Stop agent"]
        );
        let rect = menu.rect(100, 30);
        assert_eq!(rect, Rect::new(79, 8, 21, 5));
        assert_eq!(
            menu.action_at(100, 30, 80, 9),
            Some(ContextAction::FocusAgent)
        );
        assert_eq!(
            menu.action_at(100, 30, 80, 10),
            Some(ContextAction::CopyAgentPath)
        );
        assert_eq!(
            menu.action_at(100, 30, 80, 11),
            Some(ContextAction::StopAgent)
        );
        assert_eq!(
            menu.handle(100, 30, b"j\r"),
            ContextInput::Run(ContextAction::CopyAgentPath)
        );
        assert_eq!(
            menu.handle(100, 30, b"j\r"),
            ContextInput::Run(ContextAction::StopAgent)
        );
    }

    #[test]
    fn server_menu_keeps_all_four_actions_on_screen() {
        let mut menu = ContextMenu::new(
            ContextTarget::DevServer {
                pane: PaneId(7),
                port: 3000,
                detected: 12,
            },
            99,
            29,
        );
        let rect = menu.rect(100, 30);
        assert!(rect.right() <= 100 && rect.bottom() <= 30);
        assert_eq!(
            menu.items()
                .iter()
                .map(|item| item.label.as_ref())
                .collect::<Vec<_>>(),
            [
                "Open in browser",
                "Go to source pane",
                "Kill server",
                "Copy server address"
            ]
        );
        assert_eq!(
            menu.action_at(100, 30, rect.x + 1, rect.y + 1),
            Some(ContextAction::OpenServer)
        );
        assert_eq!(
            menu.handle(100, 30, b"j\r"),
            ContextInput::Run(ContextAction::FocusServer)
        );
        assert_eq!(
            menu.handle(100, 30, b"j\r"),
            ContextInput::Run(ContextAction::StopServer)
        );
    }

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
                "Split layout",
                "Dwindle layout",
                "Scrolling layout",
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
        assert_eq!(menu.handle(100, 30, b"k"), ContextInput::Redraw);
        assert_eq!(menu.selected, menu.items().len() - 1);
        assert_eq!(
            menu.handle(100, 30, b"\r"),
            ContextInput::Run(ContextAction::PaneLayout(
                uniterm_core::layout::PaneLayout::Scrolling
            ))
        );
        assert_eq!(menu.handle(100, 30, b"\x1b"), ContextInput::Close);

        let mut menu = ContextMenu::new(ContextTarget::Pane(PaneId(1)), 0, 0);
        assert_eq!(
            menu.handle(100, 30, b"jj\r"),
            ContextInput::Run(ContextAction::Zoom)
        );
    }

    #[test]
    fn pane_layout_group_has_a_non_action_separator_and_one_active_dot() {
        use uniterm_core::layout::PaneLayout;
        for layout in [
            PaneLayout::Split,
            PaneLayout::Dwindle,
            PaneLayout::Scrolling,
        ] {
            let mut menu = ContextMenu::pane(
                PaneId(1),
                0,
                0,
                vec![ContextItem::dynamic(
                    "Move to tab 2".into(),
                    ContextAction::MoveToTab(1),
                )],
                layout,
            );
            let rect = menu.rect(80, 24);
            let index = menu.separator_index().unwrap();
            let row = rect.y + 1 + index as u16;
            assert_eq!(menu.action_at(80, 24, rect.x + 2, row), None);
            assert!(!menu.hover(80, 24, rect.x + 2, row));
            assert_eq!(
                menu.action_at(80, 24, rect.x + 2, row + 1),
                Some(ContextAction::PaneLayout(PaneLayout::Split))
            );
            menu.selected = index - 1;
            assert_eq!(
                menu.handle(100, 30, b"j\r"),
                ContextInput::Run(ContextAction::PaneLayout(PaneLayout::Split))
            );
            let mut output = Vec::new();
            menu.render(&Theme::dark(), 80, 24, &mut output);
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains(&format!("├{}┤", "─".repeat(usize::from(rect.w - 2)))));
            let label = match layout {
                PaneLayout::Split => "Split",
                PaneLayout::Dwindle => "Dwindle",
                PaneLayout::Scrolling => "Scrolling",
            };
            assert!(output.contains(&format!("● {label} layout")));
            assert_eq!(output.matches('●').count(), 1);
        }
    }

    const COLS: u16 = 80;
    const ROWS: u16 = 24;

    fn destinations(tabs: usize, new_tab: bool) -> Vec<ContextItem> {
        let mut items: Vec<ContextItem> = (0..tabs)
            .map(|tab| {
                ContextItem::dynamic(
                    format!("Tab {}", tab + 2),
                    ContextAction::MoveToTab(tab + 1),
                )
                .anchored(PaneId(100 + tab as u64))
            })
            .collect();
        if new_tab {
            items.push(ContextItem::dynamic(
                "New tab".into(),
                ContextAction::MoveToNewTab,
            ));
        }
        items
    }

    fn pane_menu(x: u16, y: u16, tabs: usize, new_tab: bool) -> ContextMenu {
        ContextMenu::pane(
            PaneId(1),
            x,
            y,
            destinations(tabs, new_tab),
            uniterm_core::layout::PaneLayout::Split,
        )
    }

    fn move_row(menu: &ContextMenu) -> usize {
        menu.items()
            .iter()
            .position(|item| item.action == ContextAction::OpenMoveSubmenu)
            .unwrap()
    }

    /// The cell of the "Move to tab..." row's first label column.
    fn move_cell(menu: &ContextMenu, cols: u16, rows: u16) -> (u16, u16) {
        let rect = menu.rect(cols, rows);
        (rect.x + 1, rect.y + 1 + move_row(menu) as u16)
    }

    fn rendered(menu: &ContextMenu, cols: u16, rows: u16) -> String {
        let mut output = Vec::new();
        menu.render(&Theme::dark(), cols, rows, &mut output);
        String::from_utf8(output).unwrap()
    }

    /// The text last painted starting at zero-based cell `(x, y)`, with
    /// styling removed, up to the next cursor move.
    fn painted_at(output: &str, x: u16, y: u16) -> Option<String> {
        let marker = format!("\x1b[{};{}H", y + 1, x + 1);
        let start = output.rfind(&marker)? + marker.len();
        let mut text = String::new();
        let mut chars = output[start..].chars();
        while let Some(ch) = chars.next() {
            if ch != '\x1b' {
                text.push(ch);
                continue;
            }
            match chars.find(|ch| ch.is_ascii_alphabetic()) {
                Some('m') => {}
                _ => break,
            }
        }
        Some(text)
    }

    fn painted_label(output: &str, x: u16, y: u16) -> String {
        painted_at(output, x, y)
            .unwrap()
            .trim_matches(['│', ' '])
            .to_string()
    }

    #[test]
    fn pane_menu_collapses_every_destination_under_one_row() {
        let menu = pane_menu(0, 0, 3, true);
        let labels: Vec<&str> = menu
            .items()
            .iter()
            .map(|item| item.label.as_ref())
            .collect();
        assert_eq!(
            labels,
            [
                "Split right",
                "Split down",
                "Zoom pane",
                "All tabs overview",
                "Scrollback / copy",
                "New tab",
                "Move to tab...",
                "Close pane",
                "Split layout",
                "Dwindle layout",
                "Scrolling layout",
            ]
        );
        assert!(menu.submenu_items().is_empty());
        assert!(!menu.items().iter().any(|item| matches!(
            item.action,
            ContextAction::MoveToTab(_) | ContextAction::MoveToNewTab
        )));
        // The top-level rows do not depend on how many Tabs exist.
        assert_eq!(
            pane_menu(0, 0, 40, true).rect(COLS, ROWS),
            pane_menu(0, 0, 1, false).rect(COLS, ROWS)
        );
        // Nothing to move to: no row at all.
        let lone = pane_menu(0, 0, 0, false);
        assert!(lone
            .items()
            .iter()
            .all(|item| item.action != ContextAction::OpenMoveSubmenu));
        let output = rendered(&menu, COLS, ROWS);
        let (x, y) = move_cell(&menu, COLS, ROWS);
        let row = painted_at(&output, x - 1, y).unwrap();
        assert!(
            row.contains("Move to tab...") && row.contains('▸'),
            "{row:?}"
        );
    }

    #[test]
    fn keyboard_opens_runs_and_backs_out_of_the_move_submenu() {
        let mut menu = pane_menu(0, 0, 2, true);
        menu.selected = move_row(&menu);
        // Enter on the parent row navigates; it never reaches the dispatcher.
        assert_eq!(menu.handle(COLS, ROWS, b"\r"), ContextInput::Redraw);
        assert_eq!(
            menu.submenu_items()
                .iter()
                .map(|item| item.label.as_ref())
                .collect::<Vec<_>>(),
            ["◂ Back", "Tab 2", "Tab 3", "New tab"]
        );
        // The first destination is selected; Enter runs its semantic action.
        assert_eq!(
            menu.clone().handle(COLS, ROWS, b"\r"),
            ContextInput::Run(ContextAction::MoveToTab(1))
        );
        assert_eq!(
            menu.clone().handle(COLS, ROWS, b"j\x1b[B\r"),
            ContextInput::Run(ContextAction::MoveToNewTab)
        );
        // Selection wraps through Back, which only returns to the parent.
        let mut back = menu.clone();
        assert_eq!(back.handle(COLS, ROWS, b"k\r"), ContextInput::Redraw);
        assert!(back.submenu_items().is_empty());
        assert_eq!(back.selected, move_row(&back));

        for leave in [&b"\x1b[D"[..], b"\x1bOD", b"h", b"\x7f", b"\x1b"] {
            let mut menu = menu.clone();
            assert_eq!(menu.handle(COLS, ROWS, leave), ContextInput::Redraw);
            assert!(menu.submenu_items().is_empty(), "{leave:?}");
            assert_eq!(menu.selected, move_row(&menu));
            // A second Escape dismisses the menu itself.
            assert_eq!(menu.handle(COLS, ROWS, b"\x1b"), ContextInput::Close);
        }
        for quit in [&b"q"[..], b"\x03"] {
            assert_eq!(menu.clone().handle(COLS, ROWS, quit), ContextInput::Close);
        }

        // Right and `l` open it too, and only on the parent row.
        for open in [&b"\x1b[C"[..], b"\x1bOC", b"l"] {
            let mut menu = pane_menu(0, 0, 2, true);
            assert_eq!(menu.handle(COLS, ROWS, open), ContextInput::None);
            assert!(menu.submenu_items().is_empty());
            menu.selected = move_row(&menu);
            assert_eq!(menu.handle(COLS, ROWS, open), ContextInput::Redraw);
            assert_eq!(menu.submenu_items().len(), 4);
        }
        // One burst of input can walk the whole path.
        let mut menu = pane_menu(0, 0, 2, true);
        assert_eq!(
            menu.handle(COLS, ROWS, b"jjjjjj\rj\r"),
            ContextInput::Run(ContextAction::MoveToTab(2))
        );
        assert_eq!(
            menu.anchor_for(ContextAction::MoveToTab(2)),
            Some(PaneId(101))
        );
        assert_eq!(menu.anchor_for(ContextAction::MoveToNewTab), None);
    }

    #[test]
    fn pointer_opens_the_submenu_beside_its_row_and_shares_painted_rows() {
        let mut menu = pane_menu(9, 4, 2, true);
        let main = menu.rect(COLS, ROWS);
        let (x, y) = move_cell(&menu, COLS, ROWS);
        assert_eq!(
            menu.action_at(COLS, ROWS, x, y),
            Some(ContextAction::OpenMoveSubmenu)
        );
        assert_eq!(menu.click(COLS, ROWS, x, y), ContextInput::Redraw);
        // Pressing the parent row again leaves the list open.
        assert_eq!(menu.click(COLS, ROWS, x, y), ContextInput::None);

        // The first row, Back, is level with the pointer that opened it.
        let sub = menu.submenu_rect(COLS, ROWS).unwrap();
        assert_eq!((sub.x, sub.y, sub.h), (main.right(), y - 1, 6));
        let output = rendered(&menu, COLS, ROWS);
        for (slot, item) in menu.submenu_items().iter().enumerate() {
            let row = sub.y + 1 + slot as u16;
            assert_eq!(painted_label(&output, sub.x, row), item.label);
            for cell in [sub.x + 1, sub.right() - 2] {
                assert_eq!(menu.action_at(COLS, ROWS, cell, row), Some(item.action));
            }
            // Borders are part of the box but bound to nothing.
            for cell in [sub.x, sub.right() - 1] {
                assert_eq!(menu.action_at(COLS, ROWS, cell, row), None);
                assert_eq!(
                    menu.clone().click(COLS, ROWS, cell, row),
                    ContextInput::None
                );
            }
        }
        assert!(painted_at(&output, sub.x, sub.y)
            .unwrap()
            .starts_with("┌ Move to tab ─"));

        // Hover follows the pointer inside the list, repaints only on change,
        // and ignores the parent rows behind it.
        assert!(menu.hover(COLS, ROWS, sub.x + 1, sub.y + 2));
        assert!(!menu.hover(COLS, ROWS, sub.x + 1, sub.y + 2));
        assert!(!menu.hover(COLS, ROWS, x, y + 1));
        assert_eq!(menu.selected, move_row(&menu));
        assert_eq!(
            menu.clone().click(COLS, ROWS, sub.x + 1, sub.y + 2),
            ContextInput::Run(ContextAction::MoveToTab(1))
        );
        assert_eq!(
            menu.clone().click(COLS, ROWS, sub.x + 1, sub.y + 4),
            ContextInput::Run(ContextAction::MoveToNewTab)
        );

        // Back, and any other parent row, return without running anything.
        let mut back = menu.clone();
        assert_eq!(
            back.click(COLS, ROWS, sub.x + 1, sub.y + 1),
            ContextInput::Redraw
        );
        assert!(back.submenu_items().is_empty());
        let mut parent = menu.clone();
        assert_eq!(
            parent.action_at(COLS, ROWS, x, y + 1),
            Some(ContextAction::ClosePane)
        );
        assert_eq!(parent.click(COLS, ROWS, x, y + 1), ContextInput::Redraw);
        assert!(parent.submenu_items().is_empty());
        assert_eq!(parent.selected, move_row(&parent) + 1);
        // Only now is Close pane a live, highlighted target.
        assert_eq!(
            parent.click(COLS, ROWS, x, y + 1),
            ContextInput::Run(ContextAction::ClosePane)
        );

        // A cell outside both boxes dismisses everything.
        assert_eq!(menu.clone().click(COLS, ROWS, 70, 20), ContextInput::Close);
    }

    #[test]
    fn top_level_borders_and_separator_are_inert_and_outside_closes() {
        let mut menu = pane_menu(9, 4, 1, true);
        let rect = menu.rect(COLS, ROWS);
        let separator = rect.y + 1 + menu.separator_index().unwrap() as u16;
        for (x, y) in [
            (rect.x, rect.y),
            (rect.x + 3, rect.y),
            (rect.x, rect.y + 2),
            (rect.right() - 1, rect.y + 2),
            (rect.x + 3, rect.bottom() - 1),
            (rect.x + 3, separator),
        ] {
            assert_eq!(menu.click(COLS, ROWS, x, y), ContextInput::None);
        }
        assert_eq!(
            menu.click(COLS, ROWS, rect.right(), rect.y + 2),
            ContextInput::Close
        );
        assert_eq!(
            menu.click(COLS, ROWS, rect.x + 1, rect.y + 1),
            ContextInput::Run(ContextAction::SplitRight)
        );
    }

    #[test]
    fn a_long_destination_list_scrolls_inside_a_bounded_viewport() {
        let mut menu = pane_menu(9, 4, 30, true);
        let (x, y) = move_cell(&menu, COLS, ROWS);
        assert_eq!(menu.click(COLS, ROWS, x, y), ContextInput::Redraw);
        let total = menu.submenu_items().len();
        assert_eq!(total, 32);
        let sub = menu.submenu_rect(COLS, ROWS).unwrap();
        assert_eq!(usize::from(sub.h), SUBMENU_ROWS + 2);
        assert!(sub.bottom() <= ROWS && sub.right() <= COLS);
        let first_row = sub.y + 1;
        let last_row = sub.bottom() - 2;
        let marker = sub.right() - 2;

        // At the top: nothing above, more below, and scrolling up is a no-op.
        let output = rendered(&menu, COLS, ROWS);
        assert!(!painted_at(&output, sub.x, sub.y).unwrap().contains('▲'));
        assert!(painted_at(&output, sub.x, sub.bottom() - 1)
            .unwrap()
            .ends_with("▼┘"));
        assert!(!menu.wheel(COLS, ROWS, true));
        assert_eq!(menu.click(COLS, ROWS, marker, sub.y), ContextInput::None);
        assert_eq!(
            menu.action_at(COLS, ROWS, sub.x + 1, first_row),
            Some(ContextAction::Back)
        );

        // One wheel step moves the rows by exactly one and the box not at all.
        assert!(menu.wheel(COLS, ROWS, false));
        assert_eq!(menu.submenu_rect(COLS, ROWS), Some(sub));
        assert_eq!(
            menu.action_at(COLS, ROWS, sub.x + 1, first_row),
            Some(ContextAction::MoveToTab(1))
        );
        let output = rendered(&menu, COLS, ROWS);
        assert!(painted_at(&output, sub.x, sub.y).unwrap().ends_with("▲┐"));
        assert_eq!(painted_label(&output, sub.x, first_row), "Tab 2");
        // The border markers scroll for pointers that have no wheel.
        assert_eq!(
            menu.click(COLS, ROWS, marker, sub.bottom() - 1),
            ContextInput::Redraw
        );
        assert_eq!(
            menu.action_at(COLS, ROWS, sub.x + 1, first_row),
            Some(ContextAction::MoveToTab(2))
        );

        // Scrolling stops at the end with the last destination on the last row.
        let mut steps = 0;
        while menu.wheel(COLS, ROWS, false) {
            steps += 1;
            assert!(steps <= total);
        }
        assert_eq!(steps, total - SUBMENU_ROWS - 2);
        assert_eq!(
            menu.action_at(COLS, ROWS, sub.x + 1, last_row),
            Some(ContextAction::MoveToNewTab)
        );
        let output = rendered(&menu, COLS, ROWS);
        assert!(!painted_at(&output, sub.x, sub.bottom() - 1)
            .unwrap()
            .contains('▼'));
        assert_eq!(painted_label(&output, sub.x, last_row), "New tab");
        // The selection was carried into view, so Enter acts on a visible row.
        assert_eq!(
            menu.clone().handle(COLS, ROWS, b"\r"),
            ContextInput::Run(menu.action_at(COLS, ROWS, sub.x + 1, first_row).unwrap())
        );

        // Keys wrap, and the viewport follows the selection both ways.
        assert_eq!(menu.handle(COLS, ROWS, b"\x1b"), ContextInput::Redraw);
        assert_eq!(menu.handle(COLS, ROWS, b"\r"), ContextInput::Redraw);
        assert_eq!(
            menu.action_at(COLS, ROWS, sub.x + 1, first_row),
            Some(ContextAction::Back)
        );
        assert_eq!(menu.handle(COLS, ROWS, b"kk"), ContextInput::Redraw);
        assert_eq!(
            menu.action_at(COLS, ROWS, sub.x + 1, last_row),
            Some(ContextAction::MoveToNewTab)
        );
        assert_eq!(
            menu.clone().handle(COLS, ROWS, b"\r"),
            ContextInput::Run(ContextAction::MoveToNewTab)
        );
        assert_eq!(menu.handle(COLS, ROWS, b"jj"), ContextInput::Redraw);
        assert_eq!(
            menu.handle(COLS, ROWS, b"\r"),
            ContextInput::Run(ContextAction::MoveToTab(1))
        );
    }

    #[test]
    fn submenu_stays_on_screen_at_narrow_widths_and_edges() {
        // Wide: room on the right. Right edge: flips to the left. Narrow:
        // neither side fits, so it covers the menu but stays on screen.
        for (cols, rows, x, y) in [
            (80, 24, 9, 4),
            (80, 24, 79, 23),
            (30, 24, 3, 0),
            (20, 10, 0, 9),
        ] {
            let mut menu = ContextMenu::pane(
                PaneId(1),
                x,
                y,
                vec![
                    ContextItem::dynamic(
                        "Tab 2: 日本語のタブ名 build".into(),
                        ContextAction::MoveToTab(1),
                    ),
                    ContextItem::dynamic(
                        "Tab 3: \x1b]0;evil\x07ok".into(),
                        ContextAction::MoveToTab(2),
                    ),
                ],
                uniterm_core::layout::PaneLayout::Split,
            );
            menu.selected = move_row(&menu);
            assert_eq!(menu.handle(cols, rows, b"l"), ContextInput::Redraw);
            let main = menu.rect(cols, rows);
            let sub = menu.submenu_rect(cols, rows).unwrap();
            assert!(sub.right() <= cols && sub.bottom() <= rows, "{sub:?}");
            if cols == 80 {
                assert!(sub.x == main.right() || sub.right() == main.x, "{sub:?}");
            }
            let output = rendered(&menu, cols, rows);
            // A Tab name cannot smuggle a control sequence into the frame.
            assert!(!output.contains("\x1b]") && !output.contains('\x07'));
            for slot in 0..3u16 {
                let row = painted_at(&output, sub.x, sub.y + 1 + slot).unwrap();
                assert_eq!(row.width(), usize::from(sub.w), "{row:?}");
            }
            // Keyboard and pointer agree on every destination row.
            for (slot, action) in [
                (1, ContextAction::MoveToTab(1)),
                (2, ContextAction::MoveToTab(2)),
            ] {
                assert_eq!(
                    menu.action_at(cols, rows, sub.x + 1, sub.y + 1 + slot),
                    Some(action)
                );
                assert_eq!(
                    menu.clone().click(cols, rows, sub.x + 1, sub.y + 1 + slot),
                    ContextInput::Run(action)
                );
            }
            assert_eq!(
                menu.handle(cols, rows, b"j\r"),
                ContextInput::Run(ContextAction::MoveToTab(2))
            );
        }
    }

    #[test]
    fn resized_panels_keep_the_selected_row_painted_and_clickable() {
        let mut menu = pane_menu(0, 1, 12, true);
        assert_eq!(menu.handle(COLS, ROWS, b"jjjjjj"), ContextInput::Redraw);
        menu.resize(COLS, 5);
        let main = menu.rect(COLS, 5);
        let (x, y) = (main.x + 1, main.bottom() - 2);
        assert_eq!(
            menu.action_at(COLS, 5, x, y),
            Some(ContextAction::OpenMoveSubmenu)
        );
        assert_eq!(menu.handle(COLS, ROWS, b"ljjjjjj"), ContextInput::Redraw);
        menu.resize(COLS, 5);
        let sub = menu.submenu_rect(COLS, 5).unwrap();
        let (x, y) = (sub.x + 1, sub.bottom() - 2);
        assert_eq!(painted_label(&rendered(&menu, COLS, 5), sub.x, y), "Tab 8");
        assert_eq!(
            menu.clone().click(COLS, 5, x, y),
            ContextInput::Run(ContextAction::MoveToTab(7))
        );
        assert_eq!(
            menu.clone().handle(COLS, 5, b"\r"),
            ContextInput::Run(ContextAction::MoveToTab(7))
        );
        assert!(menu.hover(COLS, 5, x, y - 1));
        assert_eq!(
            menu.action_at(COLS, 5, x, y - 1),
            Some(ContextAction::MoveToTab(6))
        );
        assert_eq!(menu.handle(COLS, 2, b"\r"), ContextInput::None);
    }

    #[test]
    fn destination_names_keep_whole_graphemes_when_fitted() {
        let name = "Tab 2: 👩‍💻👩‍💻👩‍💻👩‍💻👩‍💻";
        let mut menu = ContextMenu::pane(
            PaneId(1),
            1,
            2,
            vec![ContextItem::dynamic(
                name.to_owned(),
                ContextAction::MoveToTab(1),
            )],
            uniterm_core::layout::PaneLayout::Split,
        );
        menu.handle(COLS, ROWS, b"jjjjjjl");
        for (cols, expected) in [(80, name), (13, "Tab 2: 👩‍💻")] {
            let sub = menu.submenu_rect(cols, ROWS).unwrap();
            assert_eq!(
                painted_label(&rendered(&menu, cols, ROWS), sub.x, sub.y + 2),
                expected
            );
        }
    }

    #[test]
    fn a_menu_taller_than_the_screen_keeps_every_row_reachable() {
        let rows = 8;
        let mut menu = pane_menu(0, 0, 2, true);
        let rect = menu.rect(COLS, rows);
        assert_eq!(rect.h, rows);
        let visible = rect.h - 2;
        for _ in 0..menu.items().len() {
            let output = rendered(&menu, COLS, rows);
            let item = menu.items()[menu.selected].clone();
            // The selected item is painted, and the pointer finds it there.
            let row = (0..visible)
                .map(|slot| rect.y + 1 + slot)
                .find(|row| {
                    painted_at(&output, rect.x, *row)
                        .is_some_and(|text| text.contains(item.label.as_ref()))
                })
                .unwrap_or_else(|| panic!("{} is not visible", item.label));
            assert_eq!(
                menu.action_at(COLS, rows, rect.x + 1, row),
                Some(item.action)
            );
            assert_eq!(menu.handle(COLS, rows, b"j"), ContextInput::Redraw);
        }
        assert_eq!(menu.selected, 0);

        // The wheel scrolls without wrapping and never rests on the separator.
        assert!(!menu.wheel(COLS, rows, true));
        let mut steps = 0;
        while menu.wheel(COLS, rows, false) {
            steps += 1;
            let selected = Some(menu.items()[menu.selected].action);
            assert!(
                (0..2).any(
                    |slot| menu.action_at(COLS, rows, rect.x + 1, rect.y + 1 + slot) == selected
                ),
                "selection left the top of the viewport"
            );
        }
        assert_eq!(steps, 12 - usize::from(visible));
        assert_eq!(
            menu.action_at(COLS, rows, rect.x + 1, rect.bottom() - 2),
            Some(ContextAction::PaneLayout(
                uniterm_core::layout::PaneLayout::Scrolling
            ))
        );
        // The nested list opens fully on screen from a scrolled parent.
        let mut menu = pane_menu(0, 0, 2, true);
        assert_eq!(menu.handle(COLS, rows, b"jjjjjjl"), ContextInput::Redraw);
        let sub = menu.submenu_rect(COLS, rows).unwrap();
        assert!(sub.bottom() <= rows);
        assert_eq!(
            menu.handle(COLS, rows, b"\r"),
            ContextInput::Run(ContextAction::MoveToTab(1))
        );
    }
}
