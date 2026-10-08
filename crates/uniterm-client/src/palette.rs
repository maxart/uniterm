//! Event-driven action search, sharing the menus' semantic dispatch.
use crate::input::MenuKeys;
use crate::overlay::{Overlay, OverlayRowStyle};
use crate::text_input::{decode_key, edit_line, line_with_cursor, LineKey};
use uniterm_core::menu::{MenuAction, MenuItem, MENUS, QUICK_ACTIONS};
use uniterm_core::ProjectId;
use uniterm_proto::ProjectInfo;

#[derive(Clone, Debug)]
pub(crate) struct PaletteItem {
    label: String,
    key: &'static str,
    pub(crate) action: MenuAction,
    pub(crate) project: Option<ProjectId>,
}

impl From<MenuItem> for PaletteItem {
    fn from(item: MenuItem) -> Self {
        Self {
            label: item.label.into(),
            key: item.key,
            action: item.action,
            project: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Palette {
    query: String,
    searching: bool,
    cursor: usize,
    scroll: std::cell::Cell<usize>,
    catalog: Vec<PaletteItem>,
    pub(crate) items: Vec<PaletteItem>,
}

impl Palette {
    pub(crate) fn new(bindings: &[uniterm_core::KeyBinding]) -> Self {
        let mut catalog = Vec::<PaletteItem>::new();
        for item in MENUS
            .iter()
            .flat_map(|menu| menu.items)
            .chain(QUICK_ACTIONS)
        {
            if matches!(
                item.action,
                MenuAction::SwitchProject
                    | MenuAction::NewProjectTab
                    | MenuAction::RenameProject
                    | MenuAction::MoveProjectUp
                    | MenuAction::MoveProjectDown
                    | MenuAction::MoveProjectTop
                    | MenuAction::MoveProjectBottom
                    | MenuAction::CloseProject
            ) || catalog
                .iter()
                .any(|existing| existing.action == item.action)
            {
                continue;
            }
            let mut item = *item;
            if bindings
                .iter()
                .any(|binding| item.key.as_bytes() == [binding.key])
            {
                item.key = "";
            }
            catalog.push(item.into());
        }
        Self {
            searching: false,
            query: String::new(),
            cursor: 0,
            scroll: std::cell::Cell::new(0),
            items: catalog.clone(),
            catalog,
        }
    }

    /// Merge the requested Workspace projection without losing a typed query.
    pub(crate) fn refresh_projects(&mut self, projects: &[ProjectInfo], selected: &mut usize) {
        let previous = self
            .items
            .get(*selected)
            .map(|item| (item.action, item.project));
        self.catalog.retain(|item| item.project.is_none());
        self.catalog
            .extend(projects.iter().map(|project| PaletteItem {
                label: format!("Go to project: {}", project.name),
                key: "",
                action: MenuAction::SwitchProject,
                project: Some(project.id),
            }));
        self.filter();
        *selected = previous
            .and_then(|previous| {
                self.items
                    .iter()
                    .position(|item| (item.action, item.project) == previous)
            })
            .unwrap_or(0);
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        self.items = self
            .catalog
            .iter()
            .filter(|item| fuzzy_match(&query, &item.label))
            .cloned()
            .collect();
    }

    /// Preserve the prefix table until the user explicitly enters search.
    pub(crate) fn prepare_keys<'a>(
        &mut self,
        bytes: &'a [u8],
        bindings: &[uniterm_core::KeyBinding],
    ) -> Option<&'a [u8]> {
        if self.searching || bytes.is_empty() {
            return Some(bytes);
        }
        if bindings.iter().any(|binding| binding.key == bytes[0]) {
            return None;
        }
        if matches!(bytes[0], b' ' | b'/') {
            self.searching = true;
            return Some(&bytes[1..]);
        }
        if matches!(bytes[0], 0x0e | 0x10)
            || bytes.starts_with(b"\x1b[Z")
            || matches!(
                decode_key(bytes, 0).0,
                LineKey::Up
                    | LineKey::Down
                    | LineKey::Tab
                    | LineKey::Enter
                    | LineKey::Escape
                    | LineKey::Cancel
            )
        {
            self.searching = true;
            return Some(bytes);
        }
        None
    }

    pub(crate) fn searching(&self) -> bool {
        self.searching
    }

    pub(crate) fn visible(&self, rows: u16) -> usize {
        usize::from(rows.saturating_sub(10)).clamp(1, 12)
    }

    pub(crate) fn start(&self, selected: usize, rows: u16) -> usize {
        let visible = self.visible(rows);
        let mut start = self
            .scroll
            .get()
            .min(self.items.len().saturating_sub(visible));
        if selected < start {
            start = selected;
        }
        if selected >= start + visible {
            start = selected + 1 - visible;
        }
        self.scroll.set(start);
        start
    }

    pub(crate) fn overlay(&self, selected: usize, rows: u16) -> Overlay {
        let mut lines = vec![
            if self.searching {
                format!("> {}", line_with_cursor(&self.query, self.cursor, 48))
            } else {
                "Space or /: search actions and projects".into()
            },
            format!("{} results", self.items.len()),
        ];
        let mut styles = vec![OverlayRowStyle::ComposerInput, OverlayRowStyle::Section];
        let start = self.start(selected, rows);
        for index in start..start + self.visible(rows) {
            lines.push(self.items.get(index).map_or(String::new(), |item| {
                let label = crate::tty::sanitize_terminal_title(&item.label);
                if item.key.is_empty() {
                    format!("  {label}")
                } else {
                    format!("  {label:<30} {}", item.key)
                }
            }));
            styles.push(if index == selected && index < self.items.len() {
                OverlayRowStyle::CardSelected
            } else {
                OverlayRowStyle::Plain
            });
        }
        if self.items.is_empty() {
            lines[2] = "No matching actions".into();
        }
        Overlay::with_footer(
            "Quick actions",
            lines,
            &[
                (if self.searching { "Type" } else { "Space /" }, "search"),
                ("Up/Down/Tab", "select"),
                ("Enter", "run"),
                ("Esc", "close"),
            ],
        )
        .with_row_styles(styles)
    }

    pub(crate) fn handle(&mut self, bytes: &[u8], selected: &mut usize) -> MenuKeys {
        let mut at = 0;
        let mut changed = false;
        while at < bytes.len() {
            let (key, used) = if bytes[at..].starts_with(b"\x1b[Z") {
                (LineKey::Up, 3)
            } else if bytes[at] == 0x0e {
                (LineKey::Down, 1)
            } else if bytes[at] == 0x10 {
                (LineKey::Up, 1)
            } else {
                decode_key(bytes, at)
            };
            at += used.max(1);
            match key {
                LineKey::Escape | LineKey::Cancel => return MenuKeys::Close,
                LineKey::Enter if !self.items.is_empty() => return MenuKeys::Run { consumed: at },
                LineKey::Enter => {}
                LineKey::Down | LineKey::Tab if !self.items.is_empty() => {
                    *selected = (*selected + 1) % self.items.len();
                    changed = true;
                }
                LineKey::Up if !self.items.is_empty() => {
                    *selected = (*selected + self.items.len() - 1) % self.items.len();
                    changed = true;
                }
                key => {
                    if edit_line(&mut self.query, &mut self.cursor, key) {
                        self.filter();
                        *selected = 0;
                        changed = true;
                    }
                }
            }
        }
        // The box can change width when editing; repaint the exposed background.
        if changed {
            MenuKeys::Switched
        } else {
            MenuKeys::None
        }
    }
}

fn fuzzy_match(query: &str, label: &str) -> bool {
    let label = label.to_lowercase();
    let mut chars = label.chars();
    query
        .chars()
        .filter(|c| !c.is_whitespace())
        .all(|needle| chars.by_ref().any(|c| c == needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: u64, name: &str) -> ProjectInfo {
        ProjectInfo {
            id: ProjectId(id),
            name: name.into(),
            root: "/tmp".into(),
            tabs: 1,
            panes: 1,
            active: false,
            attention: 0,
            worktree: None,
        }
    }

    #[test]
    fn project_reply_preserves_query_and_refreshes_stable_targets() {
        let mut state = crate::menu::MenuState::quick_actions(&[]);
        let palette = state.palette.as_mut().unwrap();
        palette.handle("CAFÉ".as_bytes(), &mut state.sel);
        assert!(palette.items.is_empty());
        palette.refresh_projects(&[project(7, "café"), project(9, "café")], &mut state.sel);
        assert_eq!(palette.items.len(), 2);
        assert!(matches!(
            palette.handle(b"\t\r", &mut state.sel),
            MenuKeys::Run { .. }
        ));
        assert_eq!(state.action(), MenuAction::SwitchProject);
        assert_eq!(state.project(), Some(ProjectId(9)));
        // A click resolves its own row, even if keyboard selection is elsewhere.
        assert_eq!(state.project_at(0), Some(ProjectId(7)));
        let palette = state.palette.as_mut().unwrap();
        palette.refresh_projects(&[project(9, "café"), project(7, "renamed")], &mut state.sel);
        assert_eq!(palette.items.len(), 1);
        assert_eq!(state.sel, 0);
        palette.refresh_projects(&[], &mut state.sel);
        assert!(matches!(
            palette.handle(b"\r", &mut state.sel),
            MenuKeys::None
        ));
    }

    #[test]
    fn project_names_are_sanitized_at_the_render_boundary() {
        let mut palette = Palette::new(&[]);
        let mut selected = 0;
        palette.refresh_projects(&[project(7, "unique\x1b]52;c;payload\x07")], &mut selected);
        palette.handle(b"unique", &mut selected);
        let rendered = palette.overlay(selected, 24).render(80, 24);
        assert!(!rendered.windows(5).any(|bytes| bytes == b"\x1b]52;"));
        assert!(!rendered.contains(&7));
    }

    #[test]
    fn hovering_a_visible_row_keeps_its_scrolled_action_under_the_pointer() {
        let mut state = crate::menu::MenuState::quick_actions(&[]);
        state.sel = state.palette.as_ref().unwrap().items.len() - 1;
        let rect = crate::menu::menu_rect(&state, 80, 24, true, 1);
        let index = crate::menu::item_at(&state, 80, 24, true, 1, rect.x + 2, rect.y + 3).unwrap();
        let action = state.action_at(index);
        state.sel = index;
        crate::menu::render_menu(&state, 80, 24, true, 1);
        let clicked =
            crate::menu::item_at(&state, 80, 24, true, 1, rect.x + 2, rect.y + 3).unwrap();
        assert_eq!(state.action_at(clicked), action);
    }

    #[test]
    fn search_is_explicit_and_configured_shortcuts_keep_precedence() {
        let mut palette = Palette::new(&[]);
        assert_eq!(palette.prepare_keys(b"c", &[]), None);
        assert_eq!(palette.prepare_keys(b"/new", &[]), Some(&b"new"[..]));
        assert_eq!(palette.prepare_keys(b"c", &[]), Some(&b"c"[..]));
        let bindings = uniterm_core::Config::parse("bind.space = new-tab\n").bindings;
        let mut palette = Palette::new(&bindings);
        assert_eq!(palette.prepare_keys(b" ", &bindings), None);
        assert_eq!(palette.prepare_keys(b"/", &bindings), Some(&b""[..]));
    }

    #[test]
    fn scrolled_mouse_rows_match_the_visible_actions() {
        let mut state = crate::menu::MenuState::quick_actions(&[]);
        let palette = state.palette.as_ref().unwrap();
        state.sel = palette.items.len() - 1;
        for rows in [12, 24, 40] {
            let palette = state.palette.as_ref().unwrap();
            let rect = palette.overlay(state.sel, rows).geometry(80, rows);
            let start = palette.start(state.sel, rows);
            for offset in 0..palette.visible(rows) {
                assert_eq!(
                    crate::menu::item_at(
                        &state,
                        80,
                        rows,
                        true,
                        1,
                        rect.x + 2,
                        rect.y + 3 + offset as u16
                    ),
                    Some(start + offset)
                );
            }
            assert_eq!(
                crate::menu::item_at(&state, 80, rows, true, 1, rect.x + 2, rect.y + 1),
                None
            );
        }
    }

    #[test]
    fn typing_filters_and_enter_dispatches_menu_action() {
        let mut palette = Palette::new(&[]);
        let mut selected = 0;
        assert!(matches!(
            palette.handle(b"splt r\r", &mut selected),
            MenuKeys::Run { .. }
        ));
        assert_eq!(palette.items[selected].action, MenuAction::SplitRight);
    }
    #[test]
    fn no_match_enter_is_noop_and_editing_recovers() {
        let mut palette = Palette::new(&[]);
        let mut selected = 0;
        palette.handle(b"zzzz", &mut selected);
        assert!(palette.items.is_empty());
        assert!(matches!(
            palette.handle(b"\r", &mut selected),
            MenuKeys::None
        ));
        palette.handle(b"\x15about", &mut selected);
        assert_eq!(palette.items[selected].action, MenuAction::About);
    }
    #[test]
    fn navigation_scrolls_and_unknown_keys_do_not_redraw() {
        let mut palette = Palette::new(&[]);
        let mut selected = 0;
        palette.handle(b"\x1b[A", &mut selected);
        assert_eq!(selected, palette.items.len() - 1);
        assert!(palette.start(selected, 24) > 0);
        assert!(matches!(
            palette.handle(b"\x00", &mut selected),
            MenuKeys::None
        ));
        palette.handle(b"\t", &mut selected);
        assert_eq!(selected, 0);
    }
}
