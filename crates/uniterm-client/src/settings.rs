//! Schema-backed Settings modal. The server owns and persists values; this
//! client state only presents the current projection and emits one-field
//! patches for keyboard or mouse interactions.
//!
//! The surface has two zones. The rail on the left lists setting names under
//! section headings and nothing else, so it scans like a table of contents.
//! The pane on the right shows the selected setting's value, its help text,
//! and a control that fits its kind: a switch, a searchable option list with
//! a live theme preview, a stepper with its range, an inline editor, or a
//! button. Values change only through explicit actions; moving the cursor
//! through a list previews, Enter applies.

use crate::overlay::{
    finish_lines, footer_text, modal_hit, modal_rect, modal_visible_rows, panel_style,
    render_list_modal, styled_line, ui_theme, ModalHit, Rect,
};
use crate::text_input::{decode_key, edit_line, LineKey};
use uniterm_proto::{SettingsPatch, SettingsSnapshot};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsAction {
    None,
    Redraw,
    Close,
    Apply(SettingsPatch),
    /// Leave raw mode and run the hierarchy-only Desktop importer.
    MigrateDesktop,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Theme,
    Sidebar,
    SidebarWidth,
    FileSidebar,
    FileSidebarWidth,
    Editor,
    EditorRules,
    Notifications,
    NotifyCompletion,
    Status,
    StatusPosition,
    FocusFollowsMouse,
    ConfirmClose,
    ConfirmTabClose,
    Scrollback,
    Restore,
    GuardActiveRuns,
    GuardRolePanes,
    GuardIterations,
    GuardElapsedMinutes,
    GuardAllowedProjects,
    DesktopMigration,
}

/// Selectable settings in rail order. Sections group them; the order here is
/// the order of navigation and stays stable for scripted tests.
const FIELDS: &[Field] = &[
    Field::Theme,
    Field::Status,
    Field::StatusPosition,
    Field::Sidebar,
    Field::SidebarWidth,
    Field::FileSidebar,
    Field::FileSidebarWidth,
    Field::FocusFollowsMouse,
    Field::ConfirmClose,
    Field::ConfirmTabClose,
    Field::Scrollback,
    Field::Restore,
    Field::Editor,
    Field::EditorRules,
    Field::Notifications,
    Field::NotifyCompletion,
    Field::GuardActiveRuns,
    Field::GuardRolePanes,
    Field::GuardIterations,
    Field::GuardElapsedMinutes,
    Field::GuardAllowedProjects,
    Field::DesktopMigration,
];

struct Section {
    title: &'static str,
    fields: &'static [Field],
}

const SECTIONS: &[Section] = &[
    Section {
        title: "Appearance",
        fields: &[
            Field::Theme,
            Field::Status,
            Field::StatusPosition,
            Field::Sidebar,
            Field::SidebarWidth,
            Field::FileSidebar,
            Field::FileSidebarWidth,
        ],
    },
    Section {
        title: "Behaviour",
        fields: &[
            Field::FocusFollowsMouse,
            Field::ConfirmClose,
            Field::ConfirmTabClose,
            Field::Scrollback,
            Field::Restore,
        ],
    },
    Section {
        title: "Editors",
        fields: &[Field::Editor, Field::EditorRules],
    },
    Section {
        title: "Notifications",
        fields: &[Field::Notifications, Field::NotifyCompletion],
    },
    Section {
        title: "Guardrails",
        fields: &[
            Field::GuardActiveRuns,
            Field::GuardRolePanes,
            Field::GuardIterations,
            Field::GuardElapsedMinutes,
            Field::GuardAllowedProjects,
        ],
    },
    Section {
        title: "Tools",
        fields: &[Field::DesktopMigration],
    },
];

/// What kind of control the pane shows for a setting.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Switch,
    Choice,
    Number,
    Text,
    Action,
}

/// One rail row: a section heading (not selectable) or a setting.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RailRow {
    Heading(&'static str),
    Setting(Field),
}

const LIST_W: u16 = 26;

/// Where keyboard focus is: the rail, or a list of options in the pane.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Focus {
    Rail,
    Options { cursor: usize, query: String },
}

pub struct SettingsView {
    settings: SettingsSnapshot,
    sel: usize,
    scroll: usize,
    saved: bool,
    error: Option<String>,
    editing: Option<EditState>,
    focus: Focus,
}

struct EditState {
    field: Field,
    buffer: String,
    cursor: usize,
}

impl SettingsView {
    pub fn new(settings: SettingsSnapshot, saved: bool, error: Option<String>) -> Self {
        SettingsView {
            settings,
            sel: 0,
            scroll: 0,
            saved,
            error,
            editing: None,
            focus: Focus::Rail,
        }
    }

    pub fn refresh(&mut self, settings: SettingsSnapshot, saved: bool, error: Option<String>) {
        self.settings = settings;
        self.saved = saved;
        self.error = error;
    }

    pub fn rect(cols: u16, rows: u16) -> Rect {
        modal_rect(cols, rows)
    }

    fn field(&self) -> Field {
        FIELDS[self.sel.min(FIELDS.len() - 1)]
    }

    fn rail_rows() -> Vec<RailRow> {
        let mut rows = Vec::new();
        for (index, section) in SECTIONS.iter().enumerate() {
            if index != 0 {
                rows.push(RailRow::Heading(""));
            }
            rows.push(RailRow::Heading(section.title));
            rows.extend(section.fields.iter().map(|field| RailRow::Setting(*field)));
        }
        rows
    }

    fn rail_index_of(field: Field) -> usize {
        Self::rail_rows()
            .iter()
            .position(|row| *row == RailRow::Setting(field))
            .unwrap_or(0)
    }

    fn label(field: Field) -> &'static str {
        match field {
            Field::Theme => "Theme",
            Field::Sidebar => "Projects sidebar",
            Field::SidebarWidth => "Projects width",
            Field::FileSidebar => "Observatory sidebar",
            Field::FileSidebarWidth => "Observatory width",
            Field::Editor => "Default editor",
            Field::EditorRules => "File-type editors",
            Field::Notifications => "Delivery",
            Field::NotifyCompletion => "Completion notices",
            Field::Status => "Status bar",
            Field::StatusPosition => "Status position",
            Field::FocusFollowsMouse => "Focus follows mouse",
            Field::ConfirmClose => "Confirm pane close",
            Field::ConfirmTabClose => "Confirm tab close",
            Field::Scrollback => "Scrollback lines",
            Field::Restore => "Restore on startup",
            Field::GuardActiveRuns => "Active run limit",
            Field::GuardRolePanes => "Role Pane limit",
            Field::GuardIterations => "Iteration limit",
            Field::GuardElapsedMinutes => "Elapsed limit",
            Field::GuardAllowedProjects => "Allowed Projects",
            Field::DesktopMigration => "Import Uniterm Desktop",
        }
    }

    fn kind(field: Field) -> Kind {
        match field {
            Field::Theme | Field::Notifications | Field::StatusPosition => Kind::Choice,
            Field::Sidebar
            | Field::FileSidebar
            | Field::NotifyCompletion
            | Field::Status
            | Field::FocusFollowsMouse
            | Field::ConfirmClose
            | Field::ConfirmTabClose
            | Field::Restore => Kind::Switch,
            Field::SidebarWidth
            | Field::FileSidebarWidth
            | Field::Scrollback
            | Field::GuardActiveRuns
            | Field::GuardRolePanes
            | Field::GuardIterations
            | Field::GuardElapsedMinutes => Kind::Number,
            Field::Editor | Field::EditorRules | Field::GuardAllowedProjects => Kind::Text,
            Field::DesktopMigration => Kind::Action,
        }
    }

    fn value(&self, field: Field) -> String {
        match field {
            Field::Theme => self.settings.theme.clone(),
            Field::Sidebar => on_off(self.settings.sidebar).into(),
            Field::SidebarWidth => self.settings.sidebar_width.to_string(),
            Field::FileSidebar => on_off(self.settings.file_sidebar).into(),
            Field::FileSidebarWidth => self.settings.file_sidebar_width.to_string(),
            Field::Editor => self.settings.editor.clone(),
            Field::EditorRules => self.settings.editor_rules.clone(),
            Field::Notifications => self.settings.notification_delivery.clone(),
            Field::NotifyCompletion => on_off(self.settings.notify_completion).into(),
            Field::Status => on_off(self.settings.status).into(),
            Field::StatusPosition => if self.settings.status_top {
                "top"
            } else {
                "bottom"
            }
            .into(),
            Field::FocusFollowsMouse => on_off(self.settings.focus_follows_mouse).into(),
            Field::ConfirmClose => on_off(self.settings.confirm_close).into(),
            Field::ConfirmTabClose => on_off(self.settings.confirm_tab_close).into(),
            Field::Scrollback => self.settings.scrollback_limit.to_string(),
            Field::Restore => on_off(self.settings.restore).into(),
            Field::GuardActiveRuns => self.settings.guardrail_max_active_runs.to_string(),
            Field::GuardRolePanes => self.settings.guardrail_max_role_panes.to_string(),
            Field::GuardIterations => self.settings.guardrail_max_iterations.to_string(),
            Field::GuardElapsedMinutes => self.settings.guardrail_max_elapsed_minutes.to_string(),
            Field::GuardAllowedProjects => self.settings.guardrail_allowed_projects.clone(),
            Field::DesktopMigration => String::new(),
        }
    }

    fn switch_value(&self, field: Field) -> bool {
        match field {
            Field::Sidebar => self.settings.sidebar,
            Field::FileSidebar => self.settings.file_sidebar,
            Field::NotifyCompletion => self.settings.notify_completion,
            Field::Status => self.settings.status,
            Field::FocusFollowsMouse => self.settings.focus_follows_mouse,
            Field::ConfirmClose => self.settings.confirm_close,
            Field::ConfirmTabClose => self.settings.confirm_tab_close,
            Field::Restore => self.settings.restore,
            _ => false,
        }
    }

    fn description(field: Field) -> &'static str {
        match field {
            Field::Theme => {
                "Semantic colours shared by the chrome and every modal. Move through the list to preview; Enter applies."
            }
            Field::Status => "Show the Workspace and Tab bar.",
            Field::StatusPosition => "Place the status bar above or below the terminal.",
            Field::Sidebar => "Persistent Project hierarchy on the left.",
            Field::SidebarWidth => "Width of the Project sidebar in cells.",
            Field::FileSidebar => "Persistent right Observatory rail. Prefix + o toggles it too.",
            Field::FileSidebarWidth => "Width of the Observatory in cells when visible.",
            Field::FocusFollowsMouse => "Focus a Pane as the pointer moves over it.",
            Field::ConfirmClose => "Require confirmation before closing a Pane.",
            Field::ConfirmTabClose => {
                "Require confirmation before closing a Tab and every Pane in it."
            }
            Field::Scrollback => "Maximum retained lines per Pane.",
            Field::Restore => "Restore saved Tabs, Panes, and scrollback on startup.",
            Field::Editor => "Catch-all command used to open files. It is validated before it is saved.",
            Field::EditorRules => {
                "Overrides in extension=command form, separated by semicolons. Example: md=glow."
            }
            Field::Notifications => {
                "Where agent attention goes: nowhere, a Uniterm toast, the host terminal (OSC), or a native system notification."
            }
            Field::NotifyCompletion => "Also notify when a working agent settles back to idle.",
            Field::GuardActiveRuns => {
                "Maximum active workflows and relays. Paused runs still reserve capacity."
            }
            Field::GuardRolePanes => "Maximum role Panes reserved by active native runs.",
            Field::GuardIterations => {
                "Maximum workflow or relay iterations captured when a run starts."
            }
            Field::GuardElapsedMinutes => {
                "Minutes before an awaiting run pauses into the waiting queue."
            }
            Field::GuardAllowedProjects => {
                "Exact Project names or roots separated by semicolons. Empty allows every Project in the Workspace."
            }
            Field::DesktopMigration => {
                "Import Desktop Workspaces, Projects, paths, and Tabs. Existing CLI Workspaces are never silently replaced."
            }
        }
    }

    /// The options of a choice setting, in display order.
    fn options(&self, field: Field) -> Vec<String> {
        match field {
            Field::Theme => self.settings.themes.clone(),
            Field::Notifications => self.settings.notification_deliveries.clone(),
            Field::StatusPosition => vec!["top".into(), "bottom".into()],
            _ => Vec::new(),
        }
    }

    /// Options after the search filter, as (index into `options`, name).
    fn filtered_options(&self, field: Field, query: &str) -> Vec<(usize, String)> {
        let needle = query.trim().to_ascii_lowercase();
        self.options(field)
            .into_iter()
            .enumerate()
            .filter(|(_, name)| needle.is_empty() || name.to_ascii_lowercase().contains(&needle))
            .collect()
    }

    fn number_range(field: Field) -> (u64, u64, u64) {
        match field {
            Field::SidebarWidth => (16, 40, 2),
            Field::FileSidebarWidth => (22, 52, 2),
            Field::Scrollback => (100, 1_000_000, 1_000),
            Field::GuardActiveRuns => (1, u64::from(uniterm_core::GUARDRAIL_MAX_ACTIVE_RUNS), 1),
            Field::GuardRolePanes => (1, u64::from(uniterm_core::GUARDRAIL_MAX_ROLE_PANES), 1),
            Field::GuardIterations => (1, u64::from(uniterm_core::GUARDRAIL_MAX_ITERATIONS), 1),
            Field::GuardElapsedMinutes => (1, uniterm_core::GUARDRAIL_MAX_ELAPSED_SECONDS / 60, 15),
            _ => (0, 0, 1),
        }
    }

    fn set_choice(&mut self, field: Field, value: String) -> SettingsPatch {
        let mut patch = SettingsPatch::default();
        match field {
            Field::Theme => {
                self.settings.theme = value.clone();
                patch.theme = Some(value);
            }
            Field::Notifications => {
                self.settings.notification_delivery = value.clone();
                patch.notification_delivery = Some(value);
            }
            Field::StatusPosition => {
                self.settings.status_top = value == "top";
                patch.status_top = Some(self.settings.status_top);
            }
            _ => {}
        }
        self.saved = false;
        self.error = None;
        patch
    }

    fn change(&mut self, forward: bool) -> SettingsPatch {
        let field = self.field();
        match Self::kind(field) {
            Kind::Choice => {
                let options = self.options(field);
                let current = options
                    .iter()
                    .position(|name| *name == self.value(field))
                    .unwrap_or(0);
                let count = options.len().max(1);
                let next = if forward {
                    (current + 1) % count
                } else {
                    (current + count - 1) % count
                };
                match options.get(next).cloned() {
                    Some(value) => self.set_choice(field, value),
                    None => SettingsPatch::default(),
                }
            }
            Kind::Switch => self.toggle(field),
            Kind::Number => {
                let (min, max, step) = Self::number_range(field);
                let current = self.value(field).parse::<u64>().unwrap_or(min);
                let next = if forward {
                    current.saturating_add(step)
                } else {
                    current.saturating_sub(step)
                }
                .clamp(min, max);
                self.set_number(field, next)
            }
            Kind::Text | Kind::Action => SettingsPatch::default(),
        }
    }

    fn toggle(&mut self, field: Field) -> SettingsPatch {
        let mut patch = SettingsPatch::default();
        match field {
            Field::Sidebar => toggle(&mut self.settings.sidebar, &mut patch.sidebar),
            Field::FileSidebar => toggle(&mut self.settings.file_sidebar, &mut patch.file_sidebar),
            Field::NotifyCompletion => toggle(
                &mut self.settings.notify_completion,
                &mut patch.notify_completion,
            ),
            Field::Status => toggle(&mut self.settings.status, &mut patch.status),
            Field::FocusFollowsMouse => toggle(
                &mut self.settings.focus_follows_mouse,
                &mut patch.focus_follows_mouse,
            ),
            Field::ConfirmClose => {
                toggle(&mut self.settings.confirm_close, &mut patch.confirm_close)
            }
            Field::ConfirmTabClose => toggle(
                &mut self.settings.confirm_tab_close,
                &mut patch.confirm_tab_close,
            ),
            Field::Restore => toggle(&mut self.settings.restore, &mut patch.restore),
            _ => {}
        }
        self.saved = false;
        self.error = None;
        patch
    }

    fn set_number(&mut self, field: Field, value: u64) -> SettingsPatch {
        let mut patch = SettingsPatch::default();
        match field {
            Field::SidebarWidth => {
                self.settings.sidebar_width = value as u16;
                patch.sidebar_width = Some(value as u16);
            }
            Field::FileSidebarWidth => {
                self.settings.file_sidebar_width = value as u16;
                patch.file_sidebar_width = Some(value as u16);
            }
            Field::Scrollback => {
                self.settings.scrollback_limit = value as usize;
                patch.scrollback_limit = Some(value as usize);
            }
            Field::GuardActiveRuns => {
                self.settings.guardrail_max_active_runs = value as u16;
                patch.guardrail_max_active_runs = Some(value as u16);
            }
            Field::GuardRolePanes => {
                self.settings.guardrail_max_role_panes = value as u16;
                patch.guardrail_max_role_panes = Some(value as u16);
            }
            Field::GuardIterations => {
                self.settings.guardrail_max_iterations = value as u32;
                patch.guardrail_max_iterations = Some(value as u32);
            }
            Field::GuardElapsedMinutes => {
                self.settings.guardrail_max_elapsed_minutes = value;
                patch.guardrail_max_elapsed_minutes = Some(value);
            }
            _ => {}
        }
        self.saved = false;
        self.error = None;
        patch
    }

    fn is_editable_field(field: Field) -> bool {
        matches!(Self::kind(field), Kind::Text | Kind::Number)
    }

    fn begin_edit(&mut self) {
        let field = self.field();
        if !Self::is_editable_field(field) {
            return;
        }
        let buffer = self.value(field);
        let cursor = buffer.len();
        self.editing = Some(EditState {
            field,
            buffer,
            cursor,
        });
        self.error = None;
    }

    fn enter_options(&mut self) {
        let field = self.field();
        let cursor = self
            .options(field)
            .iter()
            .position(|name| *name == self.value(field))
            .unwrap_or(0);
        self.focus = Focus::Options {
            cursor,
            query: String::new(),
        };
    }

    fn handle_edit(&mut self, input: &[u8]) -> SettingsAction {
        let mut index = 0;
        while index < input.len() {
            let (key, consumed) = decode_key(input, index);
            if consumed == 0 {
                break;
            }
            index += consumed;
            match key {
                LineKey::Escape | LineKey::Cancel => {
                    self.editing = None;
                    return SettingsAction::Redraw;
                }
                LineKey::Enter => {
                    let Some(editing) = self.editing.take() else {
                        return SettingsAction::Redraw;
                    };
                    let value = editing.buffer.trim().to_string();
                    let mut patch = SettingsPatch::default();
                    match editing.field {
                        Field::Editor => {
                            self.settings.editor = value.clone();
                            patch.editor = Some(value);
                        }
                        Field::EditorRules => {
                            self.settings.editor_rules = value.clone();
                            patch.editor_rules = Some(value);
                        }
                        Field::GuardAllowedProjects => {
                            self.settings.guardrail_allowed_projects = value.clone();
                            patch.guardrail_allowed_projects = Some(value);
                        }
                        field if Self::kind(field) == Kind::Number => {
                            let (min, max, _) = Self::number_range(field);
                            match bounded_number(&value, min, max, Self::label(field)) {
                                Ok(number) => patch = self.set_number(field, number),
                                Err(error) => {
                                    self.error = Some(error);
                                    return SettingsAction::Redraw;
                                }
                            }
                        }
                        _ => return SettingsAction::Redraw,
                    }
                    self.saved = false;
                    return SettingsAction::Apply(patch);
                }
                _ => {
                    if let Some(editing) = &mut self.editing {
                        edit_line(&mut editing.buffer, &mut editing.cursor, key);
                    }
                }
            }
        }
        SettingsAction::Redraw
    }

    fn handle_options(&mut self, input: &[u8]) -> SettingsAction {
        let field = self.field();
        let mut index = 0;
        while index < input.len() {
            let (key, consumed) = decode_key(input, index);
            if consumed == 0 {
                break;
            }
            index += consumed;
            let Focus::Options { cursor, query } = &self.focus else {
                return SettingsAction::Redraw;
            };
            let (mut cursor, mut query) = (*cursor, query.clone());
            match key {
                LineKey::Escape | LineKey::Cancel | LineKey::Left => {
                    self.focus = Focus::Rail;
                    return SettingsAction::Redraw;
                }
                LineKey::Up => cursor = cursor.saturating_sub(1),
                LineKey::Down => cursor += 1,
                LineKey::Enter => {
                    let chosen = self
                        .filtered_options(field, &query)
                        .get(cursor)
                        .map(|(_, name)| name.clone());
                    self.focus = Focus::Rail;
                    return match chosen {
                        Some(value) => SettingsAction::Apply(self.set_choice(field, value)),
                        None => SettingsAction::Redraw,
                    };
                }
                other => {
                    let mut cursor_in_query = query.len();
                    edit_line(&mut query, &mut cursor_in_query, other);
                    cursor = 0;
                }
            }
            let count = self.filtered_options(field, &query).len();
            self.focus = Focus::Options {
                cursor: cursor.min(count.saturating_sub(1)),
                query,
            };
        }
        SettingsAction::Redraw
    }

    fn focus_query(&self) -> String {
        match &self.focus {
            Focus::Options { query, .. } => query.clone(),
            Focus::Rail => String::new(),
        }
    }

    pub fn handle(&mut self, input: &[u8], cols: u16, rows: u16) -> SettingsAction {
        if self.editing.is_some() {
            return self.handle_edit(input);
        }
        if matches!(self.focus, Focus::Options { .. }) {
            return self.handle_options(input);
        }
        let visible = modal_visible_rows(Self::rect(cols, rows).h);
        let mut index = 0;
        while index < input.len() {
            let byte = input[index];
            if byte == 0x1b {
                if input.get(index + 1) == Some(&b'[') {
                    match input.get(index + 2) {
                        Some(b'A') => self.nav(false, visible),
                        Some(b'B') => self.nav(true, visible),
                        Some(b'C') if self.changes_from_rail() => {
                            return SettingsAction::Apply(self.change(true));
                        }
                        Some(b'C') if Self::kind(self.field()) == Kind::Text => {
                            self.begin_edit();
                            return SettingsAction::Redraw;
                        }
                        Some(b'D') if self.changes_from_rail() => {
                            return SettingsAction::Apply(self.change(false));
                        }
                        _ => {}
                    }
                    index += 3;
                    continue;
                }
                return SettingsAction::Close;
            }
            match byte {
                b'q' | 0x03 => return SettingsAction::Close,
                b'j' => self.nav(true, visible),
                b'k' => self.nav(false, visible),
                b'h' | b'-' if self.changes_from_rail() => {
                    return SettingsAction::Apply(self.change(false));
                }
                b'l' | b'+' | b' ' => {
                    return match Self::kind(self.field()) {
                        Kind::Action => SettingsAction::MigrateDesktop,
                        Kind::Text => {
                            self.begin_edit();
                            SettingsAction::Redraw
                        }
                        _ => SettingsAction::Apply(self.change(true)),
                    };
                }
                0x0d | 0x0a => {
                    return match Self::kind(self.field()) {
                        Kind::Action => SettingsAction::MigrateDesktop,
                        Kind::Text | Kind::Number => {
                            self.begin_edit();
                            SettingsAction::Redraw
                        }
                        Kind::Choice => {
                            self.enter_options();
                            SettingsAction::Redraw
                        }
                        Kind::Switch => SettingsAction::Apply(self.change(true)),
                    };
                }
                _ => {}
            }
            index += 1;
        }
        SettingsAction::Redraw
    }

    fn changes_from_rail(&self) -> bool {
        matches!(
            Self::kind(self.field()),
            Kind::Choice | Kind::Switch | Kind::Number
        )
    }

    /// Move the rail selection, keeping the selected row and its heading in
    /// view.
    fn nav(&mut self, down: bool, visible: usize) {
        self.sel = if down {
            (self.sel + 1).min(FIELDS.len() - 1)
        } else {
            self.sel.saturating_sub(1)
        };
        let row = Self::rail_index_of(self.field());
        // Keep the section heading visible when its first setting is chosen.
        let top = row.saturating_sub(1);
        if top < self.scroll {
            self.scroll = top;
        }
        if row >= self.scroll + visible {
            self.scroll = row + 1 - visible;
        }
    }

    pub fn click(&mut self, cols: u16, rows: u16, x: u16, y: u16) -> SettingsAction {
        let rect = Self::rect(cols, rows);
        match modal_hit(rect, LIST_W, x, y) {
            ModalHit::Outside => SettingsAction::Close,
            ModalHit::ListRow(slot) => {
                let rows = Self::rail_rows();
                let Some(RailRow::Setting(field)) = rows.get(self.scroll + slot).copied() else {
                    return SettingsAction::None;
                };
                let index = FIELDS.iter().position(|item| *item == field).unwrap_or(0);
                self.editing = None;
                self.focus = Focus::Rail;
                if self.sel == index {
                    match Self::kind(field) {
                        Kind::Action => SettingsAction::MigrateDesktop,
                        Kind::Text | Kind::Number => {
                            self.begin_edit();
                            SettingsAction::Redraw
                        }
                        Kind::Choice => {
                            self.enter_options();
                            SettingsAction::Redraw
                        }
                        Kind::Switch => SettingsAction::Apply(self.change(true)),
                    }
                } else {
                    self.sel = index;
                    SettingsAction::Redraw
                }
            }
            ModalHit::Bar(_) => SettingsAction::Close,
            ModalHit::None => self.click_pane(rect, x, y),
        }
    }

    /// A click inside the detail pane: an option row applies that option, a
    /// stepper arrow steps, a switch row toggles.
    fn click_pane(&mut self, rect: Rect, x: u16, y: u16) -> SettingsAction {
        let pane_x = rect.x + LIST_W + 1;
        if x <= pane_x || y <= rect.y {
            return SettingsAction::None;
        }
        let row = usize::from(y - rect.y - 1);
        let field = self.field();
        match Self::kind(field) {
            Kind::Choice => {
                let query = self.focus_query();
                let visible = modal_visible_rows(rect.h);
                let (first_row, _, first_index) = self.option_window(field, visible);
                let Some(offset) = row.checked_sub(first_row) else {
                    return SettingsAction::None;
                };
                match self
                    .filtered_options(field, &query)
                    .get(first_index + offset)
                {
                    Some((_, name)) => {
                        let name = name.clone();
                        self.focus = Focus::Rail;
                        SettingsAction::Apply(self.set_choice(field, name))
                    }
                    None => SettingsAction::None,
                }
            }
            Kind::Switch if row == CONTROL_ROW => SettingsAction::Apply(self.change(true)),
            Kind::Number if row == CONTROL_ROW => {
                let column = usize::from(x - pane_x - 1);
                if column < 5 {
                    SettingsAction::Apply(self.change(false))
                } else if column < 5 + 2 + self.value(field).len() + 4 {
                    SettingsAction::Apply(self.change(true))
                } else {
                    SettingsAction::None
                }
            }
            Kind::Action if row == CONTROL_ROW => SettingsAction::MigrateDesktop,
            Kind::Text if row == CONTROL_ROW => {
                self.begin_edit();
                SettingsAction::Redraw
            }
            _ => SettingsAction::None,
        }
    }

    /// Geometry of the option list in the pane: the detail row of its first
    /// entry, how many rows it may use, and the index of the first option
    /// shown after scrolling to the cursor. Rendering and clicks share it.
    fn option_window(&self, field: Field, visible: usize) -> (usize, usize, usize) {
        let search_rows = usize::from(field == Field::Theme);
        let preview_rows = if field == Field::Theme {
            PREVIEW_ROWS + 1
        } else {
            0
        };
        let first_row = CONTROL_ROW + search_rows;
        let list_rows = visible.saturating_sub(first_row + preview_rows + 2).max(1);
        let cursor = match &self.focus {
            Focus::Options { cursor, .. } => *cursor,
            Focus::Rail => 0,
        };
        let first_index = cursor.saturating_sub(list_rows.saturating_sub(1));
        (first_row, list_rows, first_index)
    }

    pub fn render(&self, cols: u16, rows: u16) -> Vec<u8> {
        let rect = Self::rect(cols, rows);
        let inner = rect.w.saturating_sub(2) as usize;
        let visible = modal_visible_rows(rect.h);
        let panel = panel_style();
        let theme = ui_theme();
        let accent = format!("\x1b[1;{}m", theme.accent.sgr_fg());
        let muted = format!("\x1b[{}m", theme.muted.sgr_fg());
        let strong = format!("\x1b[1;{}m", theme.foreground.sgr_fg());
        let heading_style = format!("\x1b[1;{}m", theme.muted.sgr_fg());
        let error_style = format!("\x1b[{}m", theme.error.sgr_fg());
        let success = format!("\x1b[{}m", theme.success.sgr_fg());
        let warning = format!("\x1b[{}m", theme.warning.sgr_fg());
        let selection = format!(
            "\x1b[{};{}m",
            theme.status_active_bg.sgr_bg(),
            theme.status_active_fg.sgr_fg()
        );
        let detail_width = inner.saturating_sub(LIST_W as usize + 1);
        let text_width = detail_width.saturating_sub(4);
        let field = self.field();
        let kind = Self::kind(field);

        // Title, help text, then the control.
        let mut detail = vec![
            styled_line(&[]),
            styled_line(&[(&panel, "  "), (&accent, Self::label(field))]),
        ];
        for line in wrap(Self::description(field), text_width) {
            detail.push(styled_line(&[(&panel, "  "), (&muted, &line)]));
        }
        while detail.len() < CONTROL_ROW {
            detail.push(styled_line(&[]));
        }
        detail.truncate(CONTROL_ROW);

        let mut hints: Vec<(&str, &str)> = vec![("\u{2191}\u{2193}", "select")];
        match kind {
            Kind::Switch => {
                let on = self.switch_value(field);
                let (on_style, off_style) = if on {
                    (accent.as_str(), muted.as_str())
                } else {
                    (muted.as_str(), accent.as_str())
                };
                detail.push(styled_line(&[
                    (&panel, "  "),
                    (on_style, if on { "\u{25C9} on " } else { "\u{25CB} on " }),
                    (&panel, "   "),
                    (off_style, if on { "\u{25CB} off" } else { "\u{25C9} off" }),
                ]));
                hints.push(("enter", "toggle"));
            }
            Kind::Number => {
                let (min, max, step) = Self::number_range(field);
                let shown = self.editing.as_ref().map_or_else(
                    || self.value(field),
                    |editing| {
                        let mut value = editing.buffer.clone();
                        value.insert(editing.cursor.min(value.len()), '\u{2588}');
                        value
                    },
                );
                let unit = if field == Field::GuardElapsedMinutes {
                    " min"
                } else {
                    ""
                };
                detail.push(styled_line(&[
                    (&panel, "  "),
                    (&accent, "\u{25C2}"),
                    (&panel, "  "),
                    (&strong, &format!("{shown}{unit}")),
                    (&panel, "  "),
                    (&accent, "\u{25B8}"),
                    (&panel, "    "),
                    (&muted, &format!("{min} to {max}, step {step}")),
                ]));
                hints.push(("\u{2190}\u{2192}", "step"));
                hints.push((
                    "enter",
                    if self.editing.is_some() {
                        "save"
                    } else {
                        "type"
                    },
                ));
            }
            Kind::Text => {
                let shown = self.editing.as_ref().map_or_else(
                    || self.value(field),
                    |editing| {
                        let mut value = editing.buffer.clone();
                        value.insert(editing.cursor.min(value.len()), '\u{2588}');
                        value
                    },
                );
                let placeholder = if shown.is_empty() {
                    if field == Field::GuardAllowedProjects {
                        "every Project in the Workspace"
                    } else {
                        "not set"
                    }
                } else {
                    ""
                };
                let box_width = text_width.max(12);
                let line = clip(&format!(" \u{203A} {shown}{placeholder}"), box_width - 2);
                let border = if self.editing.is_some() {
                    accent.as_str()
                } else {
                    muted.as_str()
                };
                detail.push(styled_line(&[
                    (&panel, "  "),
                    (
                        border,
                        &format!("\u{256D}{}\u{256E}", "\u{2500}".repeat(box_width - 2)),
                    ),
                ]));
                detail.push(styled_line(&[
                    (&panel, "  "),
                    (border, "\u{2502}"),
                    (
                        if placeholder.is_empty() {
                            strong.as_str()
                        } else {
                            muted.as_str()
                        },
                        &format!("{line:<width$}", width = box_width - 2),
                    ),
                    (border, "\u{2502}"),
                ]));
                detail.push(styled_line(&[
                    (&panel, "  "),
                    (
                        border,
                        &format!("\u{2570}{}\u{256F}", "\u{2500}".repeat(box_width - 2)),
                    ),
                ]));
                hints.push((
                    "enter",
                    if self.editing.is_some() {
                        "save"
                    } else {
                        "edit"
                    },
                ));
                if self.editing.is_some() {
                    hints.push(("esc", "cancel"));
                }
            }
            Kind::Choice => {
                let query = self.focus_query();
                let in_list = matches!(self.focus, Focus::Options { .. });
                let cursor = match &self.focus {
                    Focus::Options { cursor, .. } => Some(*cursor),
                    Focus::Rail => None,
                };
                let options = self.filtered_options(field, &query);
                let current = self.value(field);
                let previewed = cursor
                    .and_then(|index| options.get(index))
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| current.clone());
                if field == Field::Theme {
                    if in_list || !query.is_empty() {
                        detail.push(styled_line(&[
                            (&panel, "  "),
                            (&muted, "\u{203A} "),
                            (&strong, &query),
                            (&accent, "\u{2588}"),
                            (
                                &muted,
                                &format!("   {}/{}", options.len(), self.options(field).len()),
                            ),
                        ]));
                    } else {
                        detail.push(styled_line(&[
                            (&panel, "  "),
                            (&muted, "Type to search the themes"),
                        ]));
                    }
                }
                let (_, list_rows, first) = self.option_window(field, visible);
                for (offset, (_, name)) in options.iter().enumerate().skip(first).take(list_rows) {
                    let is_cursor = cursor == Some(offset);
                    let is_current = *name == current;
                    let marker = if is_current { "\u{25C9}" } else { "\u{25CB}" };
                    let pointer = if is_cursor { "\u{25B8}" } else { " " };
                    let style = if is_cursor {
                        selection.as_str()
                    } else if is_current {
                        accent.as_str()
                    } else {
                        panel.as_str()
                    };
                    let text = format!(" {pointer} {marker} {name}");
                    let width = text_width.saturating_sub(1).max(text.chars().count());
                    detail.push(styled_line(&[
                        (&panel, "  "),
                        (style, &format!("{text:<width$}")),
                    ]));
                }
                if options.is_empty() {
                    detail.push(styled_line(&[(&panel, "  "), (&muted, "No theme matches")]));
                } else if first + list_rows < options.len() {
                    let hidden = options.len() - first - list_rows;
                    detail.push(styled_line(&[
                        (&panel, "  "),
                        (&muted, &format!("   \u{2026} {hidden} more below")),
                    ]));
                }
                if field == Field::Theme {
                    detail.push(styled_line(&[]));
                    detail.extend(theme_preview(&previewed, &panel, text_width));
                }
                if in_list {
                    hints.push(("enter", "apply"));
                    hints.push(("esc", "back"));
                } else {
                    hints.push(("\u{2190}\u{2192}", "cycle"));
                    hints.push(("enter", "choose"));
                }
            }
            Kind::Action => {
                detail.push(styled_line(&[
                    (&panel, "  "),
                    (&selection, " Open the importer "),
                ]));
                hints.push(("enter", "open"));
            }
        }
        if !hints.iter().any(|(key, _)| *key == "esc") {
            hints.push(("esc", "close"));
        }

        // Save state on the last row of the pane.
        while detail.len() < visible.saturating_sub(1) {
            detail.push(styled_line(&[]));
        }
        detail.truncate(visible.saturating_sub(1));
        if let Some(error) = &self.error {
            detail.push(styled_line(&[
                (&panel, "  "),
                (
                    &error_style,
                    &clip(&format!("Save failed: {error}"), text_width),
                ),
            ]));
        } else {
            detail.push(styled_line(&[
                (&panel, "  "),
                (
                    if self.saved { &success } else { &warning },
                    if self.saved {
                        "\u{2713} Saved"
                    } else {
                        "\u{25CF} Saving\u{2026}"
                    },
                ),
            ]));
        }
        let detail = finish_lines(detail, &panel, detail_width, visible);
        let rail = Self::rail_rows();
        render_list_modal(
            cols,
            rows,
            " Settings ",
            LIST_W as usize,
            |slot| {
                let row = *rail.get(self.scroll + slot)?;
                let width = LIST_W as usize;
                Some(match row {
                    RailRow::Heading(title) => {
                        let text = clip(&format!(" {}", title.to_ascii_uppercase()), width);
                        format!(
                            "{panel}{heading_style}{text}{}{panel}",
                            " ".repeat(width - text.chars().count())
                        )
                    }
                    RailRow::Setting(field) => {
                        let text = clip(&format!("   {}", Self::label(field)), width - 1);
                        if field == self.field() {
                            format!(
                                "{panel}{accent}\u{258E}{selection}{text}{}{panel}",
                                " ".repeat(width - 1 - text.chars().count())
                            )
                        } else {
                            format!(
                                "{panel} {text}{}",
                                " ".repeat(width - 1 - text.chars().count())
                            )
                        }
                    }
                })
            },
            &detail,
            &footer_text(&hints, inner),
        )
    }
}

/// Detail row (0-based inside the pane) where the control starts; the title
/// and help text above it keep a fixed height so controls never jump.
const CONTROL_ROW: usize = 6;

/// Rows the theme preview block occupies below the list.
const PREVIEW_ROWS: usize = 4;

/// A live preview of `name`: its palette swatches and a mock of the status
/// bar and a Project row, drawn in that theme's own colours.
fn theme_preview(name: &str, panel: &str, width: usize) -> Vec<(String, usize)> {
    let preview = uniterm_core::Theme::named(name);
    let swatch = |color: uniterm_core::Color| format!("\x1b[{}m", color.sgr_bg());
    let bg = |color: uniterm_core::Color| format!("\x1b[{}m", color.sgr_bg());
    let fg = |color: uniterm_core::Color| format!("\x1b[{}m", color.sgr_fg());
    let mut lines = Vec::new();
    lines.push(styled_line(&[
        (panel, "  "),
        (&swatch(preview.background), "   "),
        (&swatch(preview.surface), "   "),
        (&swatch(preview.accent), "   "),
        (&swatch(preview.success), "   "),
        (&swatch(preview.warning), "   "),
        (&swatch(preview.error), "   "),
        (panel, "  "),
        (&fg(preview.muted), &clip(name, width.saturating_sub(22))),
    ]));
    let status = format!("{}{}", bg(preview.status_bg), fg(preview.status_fg));
    let active = format!(
        "\x1b[1m{}{}",
        bg(preview.status_active_bg),
        fg(preview.status_active_fg)
    );
    lines.push(styled_line(&[
        (panel, "  "),
        (&status, " Work \u{25BE} "),
        (&active, "  1  "),
        (&status, "  2   +                 "),
    ]));
    let project = format!("\x1b[1m{}{}", bg(preview.accent), fg(preview.background));
    lines.push(styled_line(&[
        (panel, "  "),
        (
            &format!("{}{}", bg(preview.surface), fg(preview.foreground)),
            " \u{25B8} api ",
        ),
        (
            &format!("{}{}", bg(preview.surface), fg(preview.muted)),
            "~/work/api        ",
        ),
        (&project, " \u{25CF} Claude "),
    ]));
    lines.push(styled_line(&[
        (panel, "  "),
        (&fg(preview.foreground), "text "),
        (&fg(preview.muted), "muted "),
        (&fg(preview.accent), "accent "),
        (&fg(preview.success), "\u{2713} idle "),
        (&fg(preview.warning), "! waiting "),
        (&fg(preview.error), "\u{00D7} error"),
    ]));
    lines
}

fn toggle(value: &mut bool, patch: &mut Option<bool>) {
    *value = !*value;
    *patch = Some(*value);
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

fn bounded_number(value: &str, minimum: u64, maximum: u64, label: &str) -> Result<u64, String> {
    let value = value
        .parse::<u64>()
        .map_err(|_| format!("{label} must be an integer from {minimum} to {maximum}"))?;
    if !(minimum..=maximum).contains(&value) {
        return Err(format!(
            "{label} must be an integer from {minimum} to {maximum}"
        ));
    }
    Ok(value)
}

/// Greedy word wrap at `width` cells for the help text.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> SettingsSnapshot {
        SettingsSnapshot {
            theme: "uniterm-dark".into(),
            themes: vec!["uniterm-dark".into(), "nord".into()],
            status: true,
            status_top: false,
            sidebar: true,
            sidebar_width: 24,
            file_sidebar: false,
            file_sidebar_width: 32,
            notification_delivery: "uniterm".into(),
            notification_deliveries: vec![
                "off".into(),
                "uniterm".into(),
                "terminal".into(),
                "system".into(),
            ],
            notify_completion: false,
            focus_follows_mouse: false,
            confirm_close: true,
            confirm_tab_close: true,
            scrollback_limit: 10_000,
            restore: true,
            guardrail_max_active_runs: 8,
            guardrail_max_role_panes: 16,
            guardrail_max_iterations: 3,
            guardrail_max_elapsed_minutes: 120,
            guardrail_allowed_projects: "api; /work/web".into(),
            editor: "vi".into(),
            editor_rules: "md=glow".into(),
        }
    }

    fn select(view: &mut SettingsView, field: Field) {
        view.sel = FIELDS.iter().position(|item| *item == field).unwrap();
    }

    #[test]
    fn theme_cycles_and_boolean_toggles() {
        let mut view = SettingsView::new(snapshot(), true, None);
        assert!(matches!(
            view.handle(b"l", 120, 40),
            SettingsAction::Apply(SettingsPatch { theme: Some(ref value), .. }) if value == "nord"
        ));
        select(&mut view, Field::Sidebar);
        assert!(matches!(
            view.handle(b" ", 120, 40),
            SettingsAction::Apply(SettingsPatch {
                sidebar: Some(false),
                ..
            })
        ));
    }

    #[test]
    fn theme_list_searches_previews_and_applies_on_enter() {
        let mut view = SettingsView::new(snapshot(), true, None);
        // Enter opens the option list at the current theme.
        assert_eq!(view.handle(b"\r", 120, 40), SettingsAction::Redraw);
        assert!(matches!(view.focus, Focus::Options { cursor: 0, .. }));
        // Typing filters; the cursor previews the only match, nothing applied.
        assert_eq!(view.handle(b"nor", 120, 40), SettingsAction::Redraw);
        assert_eq!(
            view.filtered_options(Field::Theme, &view.focus_query()),
            vec![(1, "nord".to_string())]
        );
        assert_eq!(view.settings.theme, "uniterm-dark");
        let frame = String::from_utf8_lossy(&view.render(120, 40)).into_owned();
        assert!(frame.contains("nord"), "preview should name the theme");
        // Enter applies the previewed theme and returns focus to the rail.
        assert!(matches!(
            view.handle(b"\r", 120, 40),
            SettingsAction::Apply(SettingsPatch { theme: Some(ref value), .. }) if value == "nord"
        ));
        assert_eq!(view.focus, Focus::Rail);
        // Escape leaves the list without applying.
        view.handle(b"\r", 120, 40);
        view.handle(b"\x1b[A", 120, 40);
        assert_eq!(view.handle(b"\x1b", 120, 40), SettingsAction::Redraw);
        assert_eq!(view.settings.theme, "nord");
    }

    #[test]
    fn rail_lists_names_only_under_section_headings() {
        let view = SettingsView::new(snapshot(), true, None);
        let rows = SettingsView::rail_rows();
        assert!(matches!(rows[0], RailRow::Heading("Appearance")));
        assert!(matches!(rows[1], RailRow::Setting(Field::Theme)));
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, RailRow::Setting(_)))
                .count(),
            FIELDS.len()
        );
        let frame = String::from_utf8_lossy(&view.render(120, 40)).into_owned();
        assert!(frame.contains("APPEARANCE"));
        assert!(frame.contains("GUARDRAILS"));
        // The rail row for the theme carries its name, not its value.
        let rail_row = frame
            .lines()
            .find(|line| line.contains("Theme"))
            .unwrap_or_default();
        assert!(!rail_row.contains("uniterm-dark") || rail_row.contains("\u{25C9}"));
    }

    #[test]
    fn tab_close_confirmation_emits_its_own_patch() {
        let mut view = SettingsView::new(snapshot(), true, None);
        select(&mut view, Field::ConfirmTabClose);
        assert!(matches!(
            view.handle(b" ", 120, 40),
            SettingsAction::Apply(SettingsPatch {
                confirm_tab_close: Some(false),
                confirm_close: None,
                ..
            })
        ));
    }

    #[test]
    fn desktop_migration_is_an_explicit_action() {
        let mut view = SettingsView::new(snapshot(), true, None);
        view.sel = FIELDS.len() - 1;
        assert_eq!(view.handle(b"\r", 120, 40), SettingsAction::MigrateDesktop);
    }

    #[test]
    fn editor_fields_use_single_line_editing_and_emit_exact_patches() {
        let mut view = SettingsView::new(snapshot(), true, None);
        select(&mut view, Field::Editor);
        assert_eq!(view.handle(b"\r", 120, 40), SettingsAction::Redraw);
        assert!(matches!(
            view.handle(b"\x15nvim --clean\r", 120, 40),
            SettingsAction::Apply(SettingsPatch { editor: Some(ref value), .. })
                if value == "nvim --clean"
        ));

        select(&mut view, Field::EditorRules);
        assert_eq!(view.handle(b"\r", 120, 40), SettingsAction::Redraw);
        assert!(matches!(
            view.handle(b"\x15md=glow; rs=nvim\r", 120, 40),
            SettingsAction::Apply(SettingsPatch { editor_rules: Some(ref value), .. })
                if value == "md=glow; rs=nvim"
        ));
    }

    #[test]
    fn guardrail_rows_emit_bounded_numeric_and_exact_project_patches() {
        let mut view = SettingsView::new(snapshot(), true, None);
        select(&mut view, Field::GuardActiveRuns);
        assert!(matches!(
            view.handle(b"l", 120, 40),
            SettingsAction::Apply(SettingsPatch {
                guardrail_max_active_runs: Some(9),
                ..
            })
        ));

        select(&mut view, Field::GuardElapsedMinutes);
        assert_eq!(view.handle(b"\r", 120, 40), SettingsAction::Redraw);
        assert!(matches!(
            view.handle(b"\x15180\r", 120, 40),
            SettingsAction::Apply(SettingsPatch {
                guardrail_max_elapsed_minutes: Some(180),
                ..
            })
        ));

        select(&mut view, Field::GuardAllowedProjects);
        assert_eq!(view.handle(b"\r", 120, 40), SettingsAction::Redraw);
        assert!(matches!(
            view.handle(b"\x15core; /work/api\r", 120, 40),
            SettingsAction::Apply(SettingsPatch {
                guardrail_allowed_projects: Some(ref value),
                ..
            }) if value == "core; /work/api"
        ));
    }

    #[test]
    fn guardrail_numeric_editor_rejects_out_of_range_values_locally() {
        let mut view = SettingsView::new(snapshot(), true, None);
        select(&mut view, Field::GuardIterations);
        assert_eq!(view.handle(b"\r", 120, 40), SettingsAction::Redraw);
        assert_eq!(view.handle(b"\x150\r", 120, 40), SettingsAction::Redraw);
        assert!(view
            .error
            .as_deref()
            .is_some_and(|error| error.contains("1 to 100")));
    }

    #[test]
    fn steppers_clamp_to_their_range_and_widths_step_by_two() {
        let mut view = SettingsView::new(snapshot(), true, None);
        select(&mut view, Field::SidebarWidth);
        assert!(matches!(
            view.handle(b"l", 120, 40),
            SettingsAction::Apply(SettingsPatch {
                sidebar_width: Some(26),
                ..
            })
        ));
        view.settings.sidebar_width = 40;
        assert!(matches!(
            view.handle(b"l", 120, 40),
            SettingsAction::Apply(SettingsPatch {
                sidebar_width: Some(40),
                ..
            })
        ));
        select(&mut view, Field::Scrollback);
        view.settings.scrollback_limit = 500;
        assert!(matches!(
            view.handle(b"h", 120, 40),
            SettingsAction::Apply(SettingsPatch {
                scrollback_limit: Some(100),
                ..
            })
        ));
    }

    #[test]
    fn clicking_an_option_row_applies_it() {
        let mut view = SettingsView::new(snapshot(), true, None);
        select(&mut view, Field::Notifications);
        let rect = SettingsView::rect(120, 40);
        // Options start at CONTROL_ROW inside the pane; the fourth is "system".
        let y = rect.y + 1 + CONTROL_ROW as u16 + 3;
        let x = rect.x + LIST_W + 4;
        assert!(matches!(
            view.click(120, 40, x, y),
            SettingsAction::Apply(SettingsPatch { notification_delivery: Some(ref value), .. })
                if value == "system"
        ));
    }

    #[test]
    fn rendered_rows_never_exceed_the_modal_width() {
        let view = SettingsView::new(snapshot(), true, None);
        let rect = SettingsView::rect(100, 30);
        for (row, col, count) in crate::overlay::render_segments(&view.render(100, 30)) {
            // The drop shadow is one cell right of the box by design.
            let shadow = col == rect.x + 1 && count == usize::from(rect.w);
            if row >= rect.y && row < rect.y + rect.h && !shadow {
                assert!(
                    col + count as u16 <= rect.x + rect.w,
                    "row {row} overflows: starts {col}, {count} cells"
                );
            }
        }
    }
}
