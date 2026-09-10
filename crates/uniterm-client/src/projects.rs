//! Workspace Project manager. It is a projection of server truth and keeps
//! hierarchy operations available without leaving the terminal UI.

use crate::overlay::{footer_width, search_composer, Overlay, OverlayRowStyle};
use crate::text_input::{decode_key, edit_line, line_with_cursor, LineKey};
use std::path::{Path, PathBuf};
use uniterm_core::ProjectId;
use uniterm_proto::{ProjectInfo, ProjectMoveDirection};

const PATH_SUGGESTION_ROWS: usize = 5;
const PATH_WIDTH: usize = 58;
const FILTER_WIDTH: usize = 48;
const PROJECT_ACTION_ROW: usize = 4;
const PROJECT_LIST_START: usize = 6;
const MOVE_BUTTON_INDENT: usize = 2;
const MOVE_BUTTON_GAP: usize = 2;
const MOVE_UP_BUTTON: &str = "[ ↑ Move up ]";
const MOVE_DOWN_BUTTON: &str = "[ ↓ Move down ]";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectAction {
    None,
    Redraw,
    Close,
    Switch(ProjectId),
    Create,
    Rename(ProjectId, String),
    Move(ProjectId, ProjectMoveDirection),
    Remove(ProjectId),
}

/// A state transition requested by the folder-first New Project overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewProjectAction {
    /// Keep the overlay open without repainting it.
    None,
    /// Repaint after input, selection, validation, or a step change.
    Redraw,
    /// Cancel Project creation and reveal the terminal again.
    Close,
    /// Create the Project only after both steps have been validated.
    Submit { name: String, root: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PathSuggestion {
    label: String,
    completed: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NewProjectStep {
    Path,
    Name,
}

/// Folder-first Project creation. The name is derived from the selected
/// directory, leaving the only required decision - the Project root - as a
/// shell-like path input with case-insensitive Tab completion.
pub struct NewProjectView {
    path: String,
    path_cursor: usize,
    name: String,
    name_cursor: usize,
    home: PathBuf,
    root: Option<PathBuf>,
    step: NewProjectStep,
    suggestions: Vec<PathSuggestion>,
    sel: usize,
    error: Option<String>,
    remote: bool,
    submitting: bool,
}

impl Default for NewProjectView {
    fn default() -> Self {
        Self::new()
    }
}

impl NewProjectView {
    /// Start at the user's home directory, the useful default for discovery.
    pub fn new() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        Self::with_home(home)
    }

    /// Build a Project flow whose folder belongs to the remote Workspace
    /// host. Local filesystem suggestions and canonicalization would turn a
    /// valid remote `~/...` path into an unrelated client-machine path.
    pub fn for_remote() -> Self {
        let mut view = Self::with_home(PathBuf::from("~"));
        view.remote = true;
        view.suggestions.clear();
        view
    }

    fn with_home(home: PathBuf) -> Self {
        let mut view = NewProjectView {
            path: "~/".into(),
            path_cursor: 2,
            name: String::new(),
            name_cursor: 0,
            home,
            root: None,
            step: NewProjectStep::Path,
            suggestions: Vec::new(),
            sel: 0,
            error: None,
            remote: false,
            submitting: false,
        };
        view.refresh_suggestions();
        view
    }

    fn refresh_suggestions(&mut self) {
        if self.remote {
            self.suggestions.clear();
            self.sel = 0;
            self.error = None;
            return;
        }
        self.suggestions = path_suggestions(&self.path, &self.home);
        self.sel = self.sel.min(self.suggestions.len().saturating_sub(1));
        self.error = None;
    }

    fn move_selection(&mut self, down: bool) {
        if self.suggestions.is_empty() {
            return;
        }
        self.sel = if down {
            (self.sel + 1) % self.suggestions.len()
        } else {
            (self.sel + self.suggestions.len() - 1) % self.suggestions.len()
        };
    }

    fn complete(&mut self) {
        let Some(suggestion) = self.suggestions.get(self.sel) else {
            return;
        };
        self.path.clone_from(&suggestion.completed);
        self.path_cursor = self.path.len();
        self.sel = 0;
        self.refresh_suggestions();
    }

    fn continue_from_path(&mut self) -> NewProjectAction {
        if self.remote {
            let path = self.path.trim();
            if path.is_empty() {
                self.error = Some("Enter a folder path on the remote host".into());
                return NewProjectAction::Redraw;
            }
            self.name = Path::new(path.trim_end_matches('/'))
                .file_name()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .unwrap_or("Project")
                .to_string();
            self.name_cursor = self.name.len();
            self.root = Some(PathBuf::from(path));
            self.step = NewProjectStep::Name;
            self.suggestions.clear();
            self.sel = 0;
            self.error = None;
            return NewProjectAction::Redraw;
        }
        let Some(expanded) = expand_path(&self.path, &self.home) else {
            self.error = Some("Enter a folder path".into());
            return NewProjectAction::Redraw;
        };
        let selected_name = expanded
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let Ok(root) = std::fs::canonicalize(&expanded) else {
            self.error = Some("That folder does not exist".into());
            return NewProjectAction::Redraw;
        };
        if !root.is_dir() {
            self.error = Some("The path must be a folder".into());
            return NewProjectAction::Redraw;
        }
        self.name = selected_name.unwrap_or_else(|| {
            root.file_name()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .unwrap_or("Project")
                .to_string()
        });
        self.name_cursor = self.name.len();
        self.root = Some(root);
        self.step = NewProjectStep::Name;
        self.suggestions.clear();
        self.sel = 0;
        self.error = None;
        NewProjectAction::Redraw
    }

    fn submit(&mut self) -> NewProjectAction {
        let name = self.name.trim();
        if name.is_empty() {
            self.error = Some("Enter a Project name".into());
            return NewProjectAction::Redraw;
        }
        let Some(root) = &self.root else {
            self.step = NewProjectStep::Path;
            self.error = Some("Choose the Project folder again".into());
            return NewProjectAction::Redraw;
        };
        self.submitting = true;
        NewProjectAction::Submit {
            name: name.to_string(),
            root: root.to_string_lossy().into_owned(),
        }
    }

    /// Keep a rejected create visible and editable using the server's
    /// authoritative host-side failure.
    pub fn reject(&mut self, error: String) {
        self.submitting = false;
        self.error = Some(error);
    }

    /// Apply raw terminal input and return the client-loop action it implies.
    pub fn handle(&mut self, input: &[u8]) -> NewProjectAction {
        let mut index = 0;
        while index < input.len() {
            let (key, used) = decode_key(input, index);
            index += used.max(1);
            if self.submitting {
                if matches!(key, LineKey::Escape | LineKey::Cancel) {
                    return NewProjectAction::Close;
                }
                continue;
            }
            match key {
                LineKey::Escape | LineKey::Cancel => return NewProjectAction::Close,
                LineKey::Enter => {
                    return if self.step == NewProjectStep::Path {
                        self.continue_from_path()
                    } else {
                        self.submit()
                    };
                }
                LineKey::Tab if self.step == NewProjectStep::Path && !self.remote => {
                    self.complete()
                }
                LineKey::Up if self.step == NewProjectStep::Path && !self.remote => {
                    self.move_selection(false)
                }
                LineKey::Down if self.step == NewProjectStep::Path && !self.remote => {
                    self.move_selection(true)
                }
                key => {
                    let changed = if self.step == NewProjectStep::Path {
                        edit_line(&mut self.path, &mut self.path_cursor, key)
                    } else {
                        edit_line(&mut self.name, &mut self.name_cursor, key)
                    };
                    if changed && self.step == NewProjectStep::Path {
                        self.root = None;
                        self.sel = 0;
                        self.refresh_suggestions();
                    } else if changed {
                        self.error = None;
                    }
                }
            }
        }
        NewProjectAction::Redraw
    }

    /// Render a stable two-step overlay with live folder matches and guidance.
    pub fn overlay(&self) -> Overlay {
        let shown_path = if self.step == NewProjectStep::Path {
            line_with_cursor(&self.path, self.path_cursor, PATH_WIDTH.saturating_sub(3))
        } else {
            tail_chars(&self.path, PATH_WIDTH.saturating_sub(3))
        };
        let mut lines = if self.step == NewProjectStep::Path {
            vec![
                " Folder".into(),
                format!(" > {shown_path}"),
                String::new(),
                if self.remote {
                    " Remote host folder".into()
                } else {
                    " Matching folders".into()
                },
            ]
        } else {
            let shown_name =
                line_with_cursor(&self.name, self.name_cursor, PATH_WIDTH.saturating_sub(3));
            vec![
                " Folder".into(),
                format!("   {shown_path}"),
                String::new(),
                " Project name".into(),
                format!(" > {shown_name}"),
            ]
        };
        if self.step == NewProjectStep::Path && !self.remote {
            let first = self
                .sel
                .saturating_sub(PATH_SUGGESTION_ROWS.saturating_sub(1));
            for index in first..first + PATH_SUGGESTION_ROWS {
                let line = self
                    .suggestions
                    .get(index)
                    .map(|suggestion| {
                        let marker = if index == self.sel { '\u{25B8}' } else { ' ' };
                        let label: String = suggestion
                            .label
                            .chars()
                            .take(PATH_WIDTH.saturating_sub(4))
                            .collect();
                        format!(" {marker} {label}")
                    })
                    .unwrap_or_default();
                lines.push(line);
            }
        } else {
            while lines.len() < 4 + PATH_SUGGESTION_ROWS {
                lines.push(String::new());
            }
        }
        lines.push(String::new());
        let summary = self.error.clone().unwrap_or_else(|| {
            if self.submitting {
                return "Adding Project on the Workspace host...".into();
            }
            if self.step == NewProjectStep::Path {
                project_name_preview(&self.path, &self.home)
                    .map(|name| format!("Next: name will start as {name}"))
                    .unwrap_or_else(|| {
                        if self.remote {
                            "Enter a folder path on the remote host".into()
                        } else {
                            "Choose a folder, then confirm its name".into()
                        }
                    })
            } else {
                "Edit the suggested name or accept it".into()
            }
        });
        lines.push(format!(" {summary}"));
        let footer = if self.step == NewProjectStep::Path && !self.remote {
            vec![
                ("enter", "continue"),
                ("tab", "complete"),
                ("\u{2191}\u{2193}", "choose"),
                ("esc", "cancel"),
            ]
        } else if self.step == NewProjectStep::Path {
            vec![("enter", "continue"), ("esc", "cancel")]
        } else {
            vec![("enter", "add Project"), ("esc", "cancel")]
        };
        Overlay::with_footer("New Project", lines, &footer)
    }
}

fn path_suggestions(input: &str, home: &Path) -> Vec<PathSuggestion> {
    let (display_parent, partial) = input
        .rsplit_once('/')
        .map(|(parent, partial)| (format!("{parent}/"), partial))
        .unwrap_or_else(|| (String::new(), input));
    let expanded_parent = if display_parent.is_empty() {
        std::env::current_dir().ok()
    } else {
        expand_path(&display_parent, home)
    };
    let Some(parent) = expanded_parent else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let partial_lower = partial.to_lowercase();
    let mut matches: Vec<(bool, PathSuggestion)> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| partial.starts_with('.') || !name.starts_with('.'))
        .filter(|name| name.to_lowercase().starts_with(&partial_lower))
        .map(|name| {
            let exact_case = name.starts_with(partial);
            (
                exact_case,
                PathSuggestion {
                    label: format!("{name}/"),
                    completed: format!("{display_parent}{name}/"),
                },
            )
        })
        .collect();
    matches.sort_by(|(a_exact, a), (b_exact, b)| {
        b_exact
            .cmp(a_exact)
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
            .then_with(|| a.label.cmp(&b.label))
    });
    matches
        .into_iter()
        .map(|(_, suggestion)| suggestion)
        .collect()
}

fn expand_path(input: &str, home: &Path) -> Option<PathBuf> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if input == "~" {
        return Some(home.to_path_buf());
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return Some(home.join(rest));
    }
    Some(PathBuf::from(input))
}

fn project_name_preview(input: &str, home: &Path) -> Option<String> {
    let path = expand_path(input.trim_end_matches('/'), home)?;
    path.file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn tail_chars(value: &str, width: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    chars[chars.len().saturating_sub(width)..].iter().collect()
}

pub struct ProjectsView {
    pub workspace: String,
    pub items: Vec<ProjectInfo>,
    pub sel: usize,
    query: String,
    query_cursor: usize,
    action_pending: bool,
    confirm_remove: bool,
}

impl ProjectsView {
    pub fn new(workspace: String, items: Vec<ProjectInfo>) -> Self {
        let sel = items.iter().position(|project| project.active).unwrap_or(0);
        ProjectsView {
            workspace,
            items,
            sel,
            query: String::new(),
            query_cursor: 0,
            action_pending: false,
            confirm_remove: false,
        }
    }

    pub fn refresh(&mut self, workspace: String, items: Vec<ProjectInfo>) {
        let keep = self.items.get(self.sel).map(|project| project.id);
        self.workspace = workspace;
        self.items = items;
        self.sel = keep
            .and_then(|id| self.items.iter().position(|project| project.id == id))
            .or_else(|| self.items.iter().position(|project| project.active))
            .unwrap_or(0)
            .min(self.items.len().saturating_sub(1));
        self.keep_selection_visible();
        self.confirm_remove = false;
    }

    fn visible_indices(&self) -> Vec<usize> {
        let needle = self.query.trim().to_lowercase();
        if needle.is_empty() {
            return (0..self.items.len()).collect();
        }
        let names: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, project)| {
                project
                    .name
                    .to_lowercase()
                    .contains(&needle)
                    .then_some(index)
            })
            .collect();
        if !names.is_empty() {
            return names;
        }
        let folders: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, project)| {
                Path::new(&project.root)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.to_lowercase().contains(&needle))
                    .then_some(index)
            })
            .collect();
        if !folders.is_empty() {
            return folders;
        }
        let worktrees: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, project)| {
                project
                    .worktree
                    .as_ref()
                    .is_some_and(|worktree| {
                        worktree.branch.to_lowercase().contains(&needle)
                            || worktree.repository.to_lowercase().contains(&needle)
                    })
                    .then_some(index)
            })
            .collect();
        if !worktrees.is_empty() {
            return worktrees;
        }
        self.items
            .iter()
            .enumerate()
            .filter_map(|(index, project)| {
                project
                    .root
                    .to_lowercase()
                    .contains(&needle)
                    .then_some(index)
            })
            .collect()
    }

    fn keep_selection_visible(&mut self) {
        let visible = self.visible_indices();
        if !visible.contains(&self.sel) {
            self.sel = visible.first().copied().unwrap_or(0);
        }
    }

    fn selected(&self) -> Option<&ProjectInfo> {
        if self.visible_indices().contains(&self.sel) {
            self.items.get(self.sel)
        } else {
            None
        }
    }

    fn nav(&mut self, down: bool) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let position = visible
            .iter()
            .position(|index| *index == self.sel)
            .unwrap_or(0);
        let next = if down {
            (position + 1) % visible.len()
        } else {
            (position + visible.len() - 1) % visible.len()
        };
        self.sel = visible[next];
        self.confirm_remove = false;
    }

    fn move_selected(&mut self, direction: ProjectMoveDirection) -> ProjectAction {
        if self.selected().is_none() {
            return ProjectAction::Redraw;
        }
        let from = self.sel;
        let to = match direction {
            ProjectMoveDirection::Up => from.checked_sub(1),
            ProjectMoveDirection::Down => (from + 1 < self.items.len()).then_some(from + 1),
        };
        let Some(to) = to else {
            return ProjectAction::Redraw;
        };
        let project = self.items[from].id;
        self.items.swap(from, to);
        self.sel = to;
        self.confirm_remove = false;
        ProjectAction::Move(project, direction)
    }

    pub fn handle(&mut self, input: &[u8]) -> ProjectAction {
        let mut index = 0;
        let mut redraw = false;
        while index < input.len() {
            if input[index] == 0x07 {
                self.action_pending = true;
                index += 1;
                continue;
            }
            let (key, used) = decode_key(input, index);
            index += used.max(1);
            if self.confirm_remove {
                self.confirm_remove = false;
                return if key == LineKey::Char('X') {
                    self.selected()
                        .map(|project| ProjectAction::Remove(project.id))
                        .unwrap_or(ProjectAction::None)
                } else {
                    ProjectAction::Redraw
                };
            }
            let action_key = std::mem::take(&mut self.action_pending);
            match key {
                LineKey::Escape | LineKey::Cancel => return ProjectAction::Close,
                // Find is always focused. Tab is intentionally inert so it
                // cannot strand the modal in a mode where typing is ignored.
                LineKey::Tab => {}
                LineKey::Down => {
                    self.nav(true);
                    redraw = true;
                }
                LineKey::Up => {
                    self.nav(false);
                    redraw = true;
                }
                LineKey::Enter => {
                    if let Some(project) = self.selected() {
                        return ProjectAction::Switch(project.id);
                    }
                }
                LineKey::Char('j' | 'J') if action_key => {
                    return self.move_selected(ProjectMoveDirection::Down)
                }
                LineKey::Char('k' | 'K') if action_key => {
                    return self.move_selected(ProjectMoveDirection::Up)
                }
                LineKey::Char('n' | 'N') if action_key => return ProjectAction::Create,
                LineKey::Char('r' | 'R') if action_key => {
                    if let Some(project) = self.selected() {
                        return ProjectAction::Rename(project.id, project.name.clone());
                    }
                }
                LineKey::Char('x' | 'X')
                    if action_key && self.items.len() > 1 && self.selected().is_some() =>
                {
                    self.confirm_remove = true;
                    return ProjectAction::Redraw;
                }
                LineKey::Char('q' | 'Q') if action_key => return ProjectAction::Close,
                key => {
                    if edit_line(&mut self.query, &mut self.query_cursor, key) {
                        self.keep_selection_visible();
                        self.confirm_remove = false;
                        redraw = true;
                    }
                }
            }
        }
        if redraw {
            ProjectAction::Redraw
        } else {
            ProjectAction::None
        }
    }

    pub fn overlay(&self) -> Overlay {
        let mut shown_query = line_with_cursor(&self.query, self.query_cursor, FILTER_WIDTH);
        shown_query.extend(std::iter::repeat_n(
            ' ',
            FILTER_WIDTH.saturating_sub(shown_query.chars().count()),
        ));
        let visible = self.visible_indices();
        let composer = search_composer(
            "Search projects",
            &shown_query,
            FILTER_WIDTH,
            visible.len(),
            self.items.len(),
        );
        let mut lines = vec![format!("  WORKSPACE  \u{00B7}  {}", self.workspace)];
        lines.extend(composer);
        lines.push(move_button_line());
        lines.push(format!("  PROJECTS  \u{00B7}  {} shown", visible.len()));
        let mut styles = vec![
            OverlayRowStyle::Section,
            OverlayRowStyle::ComposerBorder,
            OverlayRowStyle::ComposerInput,
            OverlayRowStyle::ComposerBorder,
            OverlayRowStyle::Action,
            OverlayRowStyle::Section,
        ];
        let list_start = lines.len();
        let columns = project_columns(&self.items);
        for &index in &visible {
            let project = &self.items[index];
            let selected = if index == self.sel { '\u{25B8}' } else { ' ' };
            lines.push(format!(" {selected}  {}", project_title(project, &columns)));
            lines.push(format!("     {}", project_detail(project, &columns)));
            if index == self.sel {
                styles.push(OverlayRowStyle::CardSelected);
                styles.push(OverlayRowStyle::CardSelectedDetail);
            } else {
                styles.push(OverlayRowStyle::Card);
                styles.push(OverlayRowStyle::CardDetail);
            }
        }
        if visible.is_empty() {
            lines.push("  No Projects match. Keep typing or clear the search.".into());
            lines.push(String::new());
            styles.push(OverlayRowStyle::Card);
            styles.push(OverlayRowStyle::CardDetail);
        }
        let stable_list_rows = self.items.len().max(1) * 2;
        while lines.len() - list_start < stable_list_rows {
            lines.push(String::new());
            styles.push(OverlayRowStyle::Plain);
        }
        let normal_footer = [
            ("enter", "switch"),
            ("\u{2191}\u{2193}", "select"),
            ("^G+n/r/K/J/X", "manage"),
            ("esc", "close"),
        ];
        let footer = if self.confirm_remove {
            vec![("X", "confirm remove"), ("any key", "cancel")]
        } else {
            normal_footer.to_vec()
        };
        let mut stable_width = footer_width(&normal_footer);
        for project in &self.items {
            stable_width = stable_width
                .max(4 + project_title(project, &columns).chars().count())
                .max(5 + project_detail(project, &columns).chars().count());
        }
        for line in &lines[..PROJECT_LIST_START] {
            stable_width = stable_width.max(line.chars().count());
        }
        for line in &mut lines {
            line.extend(std::iter::repeat_n(
                ' ',
                stable_width.saturating_sub(line.chars().count()),
            ));
        }
        Overlay::with_footer("Manage Projects", lines, &footer).with_row_styles(styles)
    }

    pub fn click(&mut self, cols: u16, rows: u16, x: u16, y: u16) -> ProjectAction {
        self.action_pending = false;
        let overlay = self.overlay();
        let Some(row) = overlay.row_at(cols, rows, x, y) else {
            return ProjectAction::Close;
        };
        if row == PROJECT_ACTION_ROW {
            let rect = overlay.geometry(cols, rows);
            let Some(column) = x.checked_sub(rect.x + 2).map(usize::from) else {
                return ProjectAction::None;
            };
            if let Some(direction) = move_button_at(column) {
                return self.move_selected(direction);
            }
            return ProjectAction::None;
        }
        let Some(relative) = row.checked_sub(PROJECT_LIST_START) else {
            return ProjectAction::None;
        };
        let visible = self.visible_indices();
        let Some(&index) = visible.get(relative / 2) else {
            return ProjectAction::None;
        };
        if self.sel == index {
            ProjectAction::Switch(self.items[index].id)
        } else {
            self.sel = index;
            ProjectAction::Redraw
        }
    }
}

/// Column widths shared by every Project row.
struct ProjectColumns {
    name: usize,
    path: usize,
    tabs: usize,
    panes: usize,
}

fn project_columns(items: &[ProjectInfo]) -> ProjectColumns {
    let digits = |value: u32| value.to_string().len();
    ProjectColumns {
        name: items
            .iter()
            .map(|project| project.name.chars().count())
            .max()
            .unwrap_or(0)
            .max(8),
        path: items
            .iter()
            .map(|project| display_path(&project.root).chars().count())
            .max()
            .unwrap_or(0),
        tabs: items.iter().map(|p| digits(p.tabs)).max().unwrap_or(1),
        panes: items.iter().map(|p| digits(p.panes)).max().unwrap_or(1),
    }
}

/// The name padded to the name column, then the state flags.
fn project_title(project: &ProjectInfo, columns: &ProjectColumns) -> String {
    let active = if project.active { "  ACTIVE" } else { "" };
    let attention = if project.attention > 0 {
        format!("  !{} ATTENTION", project.attention)
    } else {
        String::new()
    };
    format!(
        "{:<name$}{active}{attention}",
        project.name,
        name = columns.name
    )
}

/// The path column, the counts right-aligned, then worktree provenance.
fn project_detail(project: &ProjectInfo, columns: &ProjectColumns) -> String {
    let mut detail = format!(
        "{:<path$}  \u{00B7}  {}  \u{00B7}  {}",
        display_path(&project.root),
        crate::sessions::count(project.tabs, "tab", columns.tabs),
        crate::sessions::count(project.panes, "pane", columns.panes),
        path = columns.path
    );
    if let Some(worktree) = &project.worktree {
        let repository = Path::new(&worktree.repository)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&worktree.repository);
        detail.push_str(&format!(
            "  \u{00B7}  \u{2387} {}  \u{00B7}  worktree of {repository}",
            worktree.branch
        ));
    }
    detail
}

/// A root shown the way a prompt would: the home directory becomes `~`.
fn display_path(root: &str) -> String {
    match std::env::var_os("HOME").map(|home| home.to_string_lossy().into_owned()) {
        Some(home) if !home.is_empty() && root == home => "~".into(),
        Some(home) if !home.is_empty() && root.starts_with(&format!("{home}/")) => {
            format!("~{}", &root[home.len()..])
        }
        _ => root.to_string(),
    }
}

fn move_button_line() -> String {
    format!(
        "{}{MOVE_UP_BUTTON}{}{MOVE_DOWN_BUTTON}",
        " ".repeat(MOVE_BUTTON_INDENT),
        " ".repeat(MOVE_BUTTON_GAP)
    )
}

fn move_button_at(column: usize) -> Option<ProjectMoveDirection> {
    let up_end = MOVE_BUTTON_INDENT + MOVE_UP_BUTTON.chars().count();
    if (MOVE_BUTTON_INDENT..up_end).contains(&column) {
        return Some(ProjectMoveDirection::Up);
    }
    let down_start = up_end + MOVE_BUTTON_GAP;
    let down_end = down_start + MOVE_DOWN_BUTTON.chars().count();
    (down_start..down_end)
        .contains(&column)
        .then_some(ProjectMoveDirection::Down)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: u64, active: bool) -> ProjectInfo {
        ProjectInfo {
            id: ProjectId(id),
            name: format!("Project {id}"),
            root: format!("/tmp/{id}"),
            tabs: 1,
            panes: 1,
            active,
            attention: 0,
            worktree: None,
        }
    }

    #[test]
    fn switch_and_remove_are_explicit() {
        let mut view = ProjectsView::new("Work".into(), vec![project(1, true), project(2, false)]);
        view.handle(b"\x1b[B");
        assert_eq!(view.handle(b"\r"), ProjectAction::Switch(ProjectId(2)));
        assert_eq!(view.handle(b"\x07X"), ProjectAction::Redraw);
        assert_eq!(view.handle(b"X"), ProjectAction::Remove(ProjectId(2)));
    }

    #[test]
    fn move_reorders_the_modal_immediately_and_keeps_selection() {
        let mut view = ProjectsView::new(
            "Work".into(),
            vec![project(1, true), project(2, false), project(3, false)],
        );
        view.handle(b"\x1b[B");
        assert_eq!(
            view.handle(b"\x07K"),
            ProjectAction::Move(ProjectId(2), ProjectMoveDirection::Up)
        );
        assert_eq!(
            view.items
                .iter()
                .map(|project| project.id)
                .collect::<Vec<_>>(),
            [ProjectId(2), ProjectId(1), ProjectId(3)]
        );
        assert_eq!(view.sel, 0);
        assert_eq!(
            view.handle(b"\x07J"),
            ProjectAction::Move(ProjectId(2), ProjectMoveDirection::Down)
        );
        assert_eq!(view.sel, 1);
    }

    #[test]
    fn move_buttons_are_visible_and_clickable() {
        let mut view = ProjectsView::new(
            "Work".into(),
            vec![project(1, true), project(2, false), project(3, false)],
        );
        view.handle(b"\x1b[B");
        let overlay = view.overlay();
        let rect = overlay.geometry(120, 40);
        let button_y = rect.y + 1 + PROJECT_ACTION_ROW as u16;
        let up_x = rect.x + 2 + MOVE_BUTTON_INDENT as u16 + 2;

        assert!(overlay.lines[PROJECT_ACTION_ROW].contains(MOVE_UP_BUTTON));
        assert!(overlay.lines[PROJECT_ACTION_ROW].contains(MOVE_DOWN_BUTTON));
        assert_eq!(
            view.click(120, 40, up_x, button_y),
            ProjectAction::Move(ProjectId(2), ProjectMoveDirection::Up)
        );
        assert_eq!(view.sel, 0);

        let overlay = view.overlay();
        let rect = overlay.geometry(120, 40);
        let down_start = MOVE_BUTTON_INDENT + MOVE_UP_BUTTON.chars().count() + MOVE_BUTTON_GAP;
        let down_x = rect.x + 2 + down_start as u16 + 2;
        assert_eq!(
            view.click(120, 40, down_x, button_y),
            ProjectAction::Move(ProjectId(2), ProjectMoveDirection::Down)
        );
        assert_eq!(view.sel, 1);
    }

    #[test]
    fn project_rows_remain_clickable_below_move_buttons() {
        let mut view = ProjectsView::new("Work".into(), vec![project(1, true), project(2, false)]);
        let overlay = view.overlay();
        let rect = overlay.geometry(120, 40);
        let second_project_y = rect.y + 1 + PROJECT_LIST_START as u16 + 2;

        assert_eq!(
            view.click(120, 40, rect.x + 4, second_project_y),
            ProjectAction::Redraw
        );
        assert_eq!(view.sel, 1);
    }

    #[test]
    fn project_cards_and_search_composer_have_semantic_styles() {
        let mut view = ProjectsView::new(
            "Work".into(),
            vec![project(1, true), project(2, false), project(3, false)],
        );
        view.sel = 1;
        let overlay = view.overlay();
        assert!(overlay.lines[1].contains("Search projects"));
        assert!(overlay.lines[2].contains('\u{203A}'));
        assert_eq!(overlay.row_style_at(1), OverlayRowStyle::ComposerBorder);
        assert_eq!(overlay.row_style_at(2), OverlayRowStyle::ComposerInput);
        assert_eq!(
            overlay.row_style_at(PROJECT_LIST_START),
            OverlayRowStyle::Card
        );
        assert_eq!(
            overlay.row_style_at(PROJECT_LIST_START + 2),
            OverlayRowStyle::CardSelected
        );
        assert_eq!(
            overlay.row_style_at(PROJECT_LIST_START + 4),
            OverlayRowStyle::Card
        );
        assert_eq!(
            overlay.row_style_at(PROJECT_LIST_START + 5),
            OverlayRowStyle::CardDetail
        );
    }

    #[test]
    fn typing_filters_projects_by_name_or_folder_and_enter_switches() {
        let mut frontend = project(1, true);
        frontend.name = "Frontend".into();
        frontend.root = "/work/apps/web-client".into();
        let mut backend = project(2, false);
        backend.name = "Payments API".into();
        backend.root = "/work/services/payments".into();

        let mut by_name = ProjectsView::new("Work".into(), vec![frontend.clone(), backend.clone()]);
        assert_eq!(by_name.handle(b"PAYMENTS API"), ProjectAction::Redraw);
        assert_eq!(by_name.sel, 1);
        assert_eq!(by_name.handle(b"\r"), ProjectAction::Switch(ProjectId(2)));
        let text = by_name.overlay().lines.join("\n");
        assert!(text.contains("Payments API"));
        assert!(!text.contains("Frontend"));

        let mut by_folder = ProjectsView::new("Work".into(), vec![frontend, backend]);
        by_folder.handle(b"web-cli");
        assert_eq!(by_folder.sel, 0);
        assert_eq!(by_folder.handle(b"\r"), ProjectAction::Switch(ProjectId(1)));

        by_folder.handle(b"\x15missing");
        assert_eq!(by_folder.handle(b"\r"), ProjectAction::None);
        assert_eq!(by_folder.handle(b"\t"), ProjectAction::None);
        assert_eq!(by_folder.handle(b"X"), ProjectAction::Redraw);
    }

    #[test]
    fn worktree_cards_show_and_search_repository_provenance() {
        let primary = project(1, true);
        let mut review = project(2, false);
        review.name = "Review".into();
        review.root = "/work/review".into();
        review.worktree = Some(uniterm_proto::WorktreeRegistration {
            project: review.id,
            project_name: review.name.clone(),
            repository: "/work/uniterm".into(),
            path: review.root.clone(),
            branch: "uniterm/review".into(),
            created_head: "0123456789abcdef".into(),
        });
        let mut view = ProjectsView::new("Work".into(), vec![primary, review]);
        view.handle(b"uniterm/review");
        assert_eq!(view.sel, 1);
        let rendered = view.overlay().lines.join("\n");
        assert!(rendered.contains("worktree of uniterm"));
        assert!(rendered.contains("uniterm/review"));
        assert!(!rendered.contains("Project 1"));
    }

    #[test]
    fn lowercase_name_match_outranks_a_common_parent_folder() {
        let mut work = project(1, true);
        work.name = "Work".into();
        work.root = "/home/example/Work/client".into();
        let mut personal = project(2, false);
        personal.name = "Personal".into();
        personal.root = "/home/example/Work/personal".into();
        let mut view = ProjectsView::new("Main".into(), vec![work, personal]);

        assert_eq!(view.handle(b"work"), ProjectAction::Redraw);
        assert_eq!(view.visible_indices(), [0]);
        assert_eq!(view.sel, 0);
    }

    #[test]
    fn filtering_and_tab_keep_modal_geometry_stable() {
        let mut view = ProjectsView::new(
            "Work".into(),
            vec![project(1, true), project(2, false), project(3, false)],
        );
        let before = view.overlay().geometry(120, 40);
        assert_eq!(view.handle(b"project 2"), ProjectAction::Redraw);
        assert_eq!(view.overlay().geometry(120, 40), before);
        assert_eq!(view.handle(b"\t"), ProjectAction::None);
        assert_eq!(view.overlay().geometry(120, 40), before);
    }

    #[test]
    fn new_project_starts_at_home_and_completes_case_insensitively() {
        let home = std::env::temp_dir().join(format!(
            "uniterm-new-project-complete-{}",
            std::process::id()
        ));
        let work = home.join("Work");
        std::fs::create_dir_all(&work).unwrap();
        let mut view = NewProjectView::with_home(home.clone());
        assert_eq!(view.path, "~/");

        view.handle(b"work");
        assert_eq!(view.handle(b"\t"), NewProjectAction::Redraw);
        assert_eq!(view.path, "~/Work/");
        assert_eq!(view.handle(b"\r"), NewProjectAction::Redraw);
        assert_eq!(view.name, "Work");
        assert_eq!(
            view.handle(b"\r"),
            NewProjectAction::Submit {
                name: "Work".into(),
                root: std::fs::canonicalize(&work)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            }
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn new_project_rejects_a_missing_folder_without_closing() {
        let home = std::env::temp_dir().join(format!(
            "uniterm-new-project-missing-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let mut view = NewProjectView::with_home(home.clone());
        view.path = "~/missing".into();
        assert_eq!(view.handle(b"\r"), NewProjectAction::Redraw);
        assert!(view
            .overlay()
            .lines
            .iter()
            .any(|line| line.contains("does not exist")));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn remote_new_project_preserves_the_host_owned_home_path() {
        let mut view = NewProjectView::for_remote();
        view.path = "~/Work/uniterm".into();
        view.path_cursor = view.path.len();
        assert_eq!(view.handle(b"\r"), NewProjectAction::Redraw);
        assert_eq!(view.name, "uniterm");
        assert_eq!(
            view.handle(b"\r"),
            NewProjectAction::Submit {
                name: "uniterm".into(),
                root: "~/Work/uniterm".into(),
            }
        );
        assert!(view.submitting);
        view.reject("remote folder does not exist".into());
        assert!(!view.submitting);
        assert!(view
            .overlay()
            .lines
            .iter()
            .any(|line| line.contains("remote folder does not exist")));
    }

    #[test]
    fn option_delete_edits_path_and_name_without_closing() {
        let mut view = NewProjectView::with_home(PathBuf::from("/tmp"));
        view.path = "~/Work/Uniterm CLI".into();
        view.path_cursor = view.path.len();
        assert_eq!(view.handle(b"\x1b\x7f"), NewProjectAction::Redraw);
        assert_eq!(view.path, "~/Work/Uniterm ");

        view.step = NewProjectStep::Name;
        view.name = "Uniterm CLI".into();
        view.name_cursor = view.name.len();
        assert_eq!(view.handle(b"\x17"), NewProjectAction::Redraw);
        assert_eq!(view.name, "Uniterm ");
    }
}
