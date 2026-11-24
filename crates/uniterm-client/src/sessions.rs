//! The Workspace switcher: merge live sockets with lightweight stopped
//! Workspace definitions, switch or revive one, or stop a running one.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use crate::overlay::{footer_width, search_composer, Overlay, OverlayRowStyle};
use crate::text_input::{decode_key, edit_line, line_with_cursor, LineKey};
use crate::workspace_request;
use uniterm_proto::{
    configured_default_workspace, merge_default_workspace, validate_workspace_name,
    workspace_name_from_catalog_key, ClientMessage, WorkspaceDefinition, WorkspaceInfo,
    WORKSPACE_CATALOG_DIR,
};

const FILTER_WIDTH: usize = 42;
const DEFAULT_BUTTON: &str = "[ Set selected as default ]";
const DEFAULT_BUTTON_INDENT: usize = 2;
const DEFAULT_ACTION_ROW: usize = 3;
const SESSION_LIST_START: usize = 5;

/// An action requested by the Manage Workspaces modal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionAction {
    None,
    Redraw,
    Close,
    Switch { name: String, path: PathBuf },
    Revive(String),
    SetDefault(String),
    KillCurrent,
    Kill { index: usize, path: PathBuf },
}

/// One running or stopped Workspace.
#[derive(Clone, Debug)]
pub struct SessionEntry {
    pub name: String,
    pub path: PathBuf,
    pub windows: u32,
    pub panes: u32,
    pub projects: u32,
    pub running: bool,
    /// Whether this is the session the client is attached to.
    pub current: bool,
}

/// One probed socket: its path and, when a server answered, its Tab, Pane,
/// and Project counts.
type Probe = (PathBuf, Option<(u32, u32, u32)>);

/// A right-aligned count with its unit pluralised and padded to the plural
/// width, so `1 tab` and `12 tabs` keep the following column in line.
pub(crate) fn count(value: u32, unit: &str, digits: usize) -> String {
    let width = unit.len() + 1;
    let label = if value == 1 {
        unit.to_string()
    } else {
        format!("{unit}s")
    };
    format!("{value:>digits$} {label:<width$}")
}

/// Column widths for the Workspace rows.
struct Columns {
    name: usize,
    projects: usize,
    tabs: usize,
    panes: usize,
}

/// The open session-switcher: the probed entries and the selected row.
pub struct SessionsState {
    pub entries: Vec<SessionEntry>,
    pub sel: usize,
    query: String,
    query_cursor: usize,
    action_pending: bool,
    default_workspace: String,
    default_error: Option<String>,
    remote: bool,
}

fn default_workspace() -> String {
    crate::tty::config_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| configured_default_workspace(&text))
        .unwrap_or_else(|| "default".into())
}

fn save_default_workspace(name: &str) -> Result<(), String> {
    validate_workspace_name(name)
        .map_err(|error| format!("invalid Workspace name '{name}': {error}"))?;
    let path = crate::tty::config_path()
        .ok_or_else(|| "HOME and XDG_CONFIG_HOME are unset".to_string())?;
    let parent = path
        .parent()
        .ok_or_else(|| "the config path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let merged = merge_default_workspace(&existing, name);
    let temporary = path.with_extension(format!("conf.{}.tmp", std::process::id()));
    let write = (|| -> std::io::Result<()> {
        use std::io::Write as _;

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(merged.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write.map_err(|error| error.to_string())
}

fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("uniterm");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local").join("state").join("uniterm")
}

fn list_definitions() -> Vec<(String, WorkspaceDefinition)> {
    let Ok(entries) = std::fs::read_dir(state_dir().join(WORKSPACE_CATALOG_DIR)) else {
        return Vec::new();
    };
    let mut definitions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(workspace_name_from_catalog_key)
        else {
            continue;
        };
        let Some(definition) = std::fs::read_to_string(path).ok().and_then(|contents| {
            contents.lines().rev().find_map(|line| {
                serde_json::from_str::<WorkspaceDefinition>(line)
                    .ok()
                    .filter(WorkspaceDefinition::is_valid)
            })
        }) else {
            continue;
        };
        definitions.push((name, definition));
    }
    definitions
}

/// Probe live sibling sockets and merge them over remembered definitions.
pub fn list_sessions(current: &Path) -> Vec<SessionEntry> {
    let Some(dir) = current.parent() else {
        return Vec::new();
    };
    let mut entries: BTreeMap<String, SessionEntry> = list_definitions()
        .into_iter()
        .map(|(name, definition)| {
            let path = dir.join(format!("{name}.sock"));
            (
                name.clone(),
                SessionEntry {
                    name,
                    path,
                    windows: definition.tab_count() as u32,
                    panes: 0,
                    projects: definition.projects.len() as u32,
                    running: false,
                    current: false,
                },
            )
        })
        .collect();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return entries.into_values().collect();
    };
    let sockets: Vec<PathBuf> = rd
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("sock"))
        .collect();
    // One Workspace projection per socket answers everything the row shows,
    // and the probes run side by side so the modal waits for the slowest
    // sibling once rather than for every sibling in turn.
    let probed: Vec<Probe> = std::thread::scope(|scope| {
        let handles: Vec<_> = sockets
            .iter()
            .map(|path| {
                scope.spawn(move || {
                    let counts = workspace_request(path, ClientMessage::WorkspaceState)
                        .ok()
                        .map(|(_, _, projects)| {
                            (
                                projects.iter().map(|project| project.tabs).sum(),
                                projects.iter().map(|project| project.panes).sum(),
                                projects.len() as u32,
                            )
                        });
                    (path.clone(), counts)
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    });
    for (path, counts) in probed {
        let Some((windows, panes, projects)) = counts else {
            continue; // dead or foreign socket
        };
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        entries.insert(
            name.clone(),
            SessionEntry {
                current: path == current,
                name,
                path,
                windows,
                panes,
                projects,
                running: true,
            },
        );
    }
    entries.into_values().collect()
}

impl SessionsState {
    /// Probe and open, selecting the current session's row.
    pub fn open(current: &Path) -> Self {
        Self::with_entries_and_default(list_sessions(current), default_workspace())
    }

    #[cfg(test)]
    fn with_entries(entries: Vec<SessionEntry>) -> Self {
        Self::with_entries_and_default(entries, "default".into())
    }

    fn with_entries_and_default(entries: Vec<SessionEntry>, default_workspace: String) -> Self {
        let sel = entries.iter().position(|e| e.current).unwrap_or(0);
        SessionsState {
            entries,
            sel,
            query: String::new(),
            query_cursor: 0,
            action_pending: false,
            default_workspace,
            default_error: None,
            remote: false,
        }
    }

    /// Open from a server-authoritative remote-host catalog. Paths are kept as
    /// names because `ut remote` reconnects its SSH bridge instead of opening
    /// a client-machine Unix socket.
    pub fn from_remote(entries: Vec<WorkspaceInfo>, current: &str) -> Self {
        let mut state = Self::with_entries_and_default(
            entries
                .into_iter()
                .map(|entry| SessionEntry {
                    current: entry.name == current,
                    path: PathBuf::from(&entry.name),
                    name: entry.name,
                    windows: entry.windows,
                    panes: entry.panes,
                    projects: entry.projects,
                    running: entry.running,
                })
                .collect(),
            current.to_string(),
        );
        state.remote = true;
        state
    }

    fn visible_indices(&self) -> Vec<usize> {
        let needle = self.query.trim().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                (needle.is_empty() || entry.name.to_lowercase().contains(&needle)).then_some(index)
            })
            .collect()
    }

    fn keep_selection_visible(&mut self) {
        let visible = self.visible_indices();
        if !visible.contains(&self.sel) {
            self.sel = visible.first().copied().unwrap_or(0);
        }
    }

    pub fn next(&mut self) {
        self.default_error = None;
        let visible = self.visible_indices();
        if !visible.is_empty() {
            let position = visible
                .iter()
                .position(|index| *index == self.sel)
                .unwrap_or(0);
            self.sel = visible[(position + 1) % visible.len()];
        }
    }

    pub fn prev(&mut self) {
        self.default_error = None;
        let visible = self.visible_indices();
        if !visible.is_empty() {
            let position = visible
                .iter()
                .position(|index| *index == self.sel)
                .unwrap_or(0);
            self.sel = visible[(position + visible.len() - 1) % visible.len()];
        }
    }

    pub fn selected(&self) -> Option<&SessionEntry> {
        if self.visible_indices().contains(&self.sel) {
            self.entries.get(self.sel)
        } else {
            None
        }
    }

    /// Convert a stopped live row into its dormant catalog representation.
    pub fn mark_stopped(&mut self, idx: usize) {
        if let Some(entry) = self.entries.get_mut(idx) {
            entry.running = false;
            entry.current = false;
            entry.panes = 0;
        }
        self.keep_selection_visible();
    }

    /// Persist `name` as the CLI fallback without switching the attached
    /// Workspace. Any write failure remains visible in the open modal.
    pub fn set_default(&mut self, name: &str) {
        match save_default_workspace(name) {
            Ok(()) => {
                self.default_workspace = name.to_string();
                self.default_error = None;
            }
            Err(error) => self.default_error = Some(error),
        }
    }

    /// Apply raw terminal input to the filter and selected Workspace.
    pub fn handle(&mut self, input: &[u8]) -> SessionAction {
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
            let action_key = std::mem::take(&mut self.action_pending);
            match key {
                LineKey::Escape | LineKey::Cancel => return SessionAction::Close,
                // Find is always focused. Tab stays inert instead of changing
                // the interpretation of subsequent printable input.
                LineKey::Tab => {}
                LineKey::Up => {
                    self.prev();
                    redraw = true;
                }
                LineKey::Down => {
                    self.next();
                    redraw = true;
                }
                LineKey::Enter => {
                    return self.selected().map_or(SessionAction::None, |entry| {
                        if entry.current {
                            SessionAction::Close
                        } else if !entry.running {
                            SessionAction::Revive(entry.name.clone())
                        } else {
                            SessionAction::Switch {
                                name: entry.name.clone(),
                                path: entry.path.clone(),
                            }
                        }
                    });
                }
                LineKey::Char('x' | 'X') if action_key && !self.remote => {
                    return self.selected().map_or(SessionAction::None, |entry| {
                        if entry.current {
                            SessionAction::KillCurrent
                        } else if entry.running {
                            SessionAction::Kill {
                                index: self.sel,
                                path: entry.path.clone(),
                            }
                        } else {
                            SessionAction::None
                        }
                    });
                }
                LineKey::Char('d' | 'D') if action_key && !self.remote => {
                    return self.selected().map_or(SessionAction::None, |entry| {
                        SessionAction::SetDefault(entry.name.clone())
                    });
                }
                LineKey::Char('q' | 'Q') if action_key => return SessionAction::Close,
                key => {
                    if edit_line(&mut self.query, &mut self.query_cursor, key) {
                        self.keep_selection_visible();
                        redraw = true;
                    }
                }
            }
        }
        if redraw {
            SessionAction::Redraw
        } else {
            SessionAction::None
        }
    }

    /// The modal overlay: one row per Workspace, the selection marked.
    pub fn overlay(&self) -> Overlay {
        let mut shown_query = line_with_cursor(&self.query, self.query_cursor, FILTER_WIDTH);
        shown_query.extend(std::iter::repeat_n(
            ' ',
            FILTER_WIDTH.saturating_sub(shown_query.chars().count()),
        ));
        let visible = self.visible_indices();
        let composer = search_composer(
            "Search workspaces",
            &shown_query,
            FILTER_WIDTH,
            visible.len(),
            self.entries.len(),
        );
        let default_status = self.default_error.as_ref().map_or_else(
            || format!("DEFAULT  \u{00B7}  {}", self.default_workspace),
            |error| format!("Could not save default: {error}"),
        );
        let mut lines = Vec::from(composer);
        lines.push(if self.remote {
            "  REMOTE HOST  \u{00B7}  select a Workspace to reconnect".into()
        } else {
            format!(
                "{}{DEFAULT_BUTTON}   {default_status}",
                " ".repeat(DEFAULT_BUTTON_INDENT)
            )
        });
        lines.push(format!("  WORKSPACES  \u{00B7}  {} shown", visible.len()));
        let mut styles = vec![
            OverlayRowStyle::ComposerBorder,
            OverlayRowStyle::ComposerInput,
            OverlayRowStyle::ComposerBorder,
            if self.remote {
                OverlayRowStyle::Section
            } else if self.default_error.is_some() {
                OverlayRowStyle::Error
            } else {
                OverlayRowStyle::Action
            },
            OverlayRowStyle::Section,
        ];
        let list_start = SESSION_LIST_START;
        if self.entries.is_empty() {
            lines.push("  No Workspaces found.".into());
            lines.push("  Start one with: ut new-workspace NAME".into());
            styles.push(OverlayRowStyle::Card);
            styles.push(OverlayRowStyle::CardDetail);
        } else {
            let columns = self.columns();
            for &i in &visible {
                let e = &self.entries[i];
                let mark = if i == self.sel { '\u{25B8}' } else { ' ' };
                lines.push(format!(" {mark}  {}", self.title_row(e, &columns)));
                lines.push(format!("     {}", self.detail_row(e, &columns)));
                if i == self.sel {
                    styles.push(OverlayRowStyle::CardSelected);
                    styles.push(OverlayRowStyle::CardSelectedDetail);
                } else {
                    styles.push(OverlayRowStyle::Card);
                    styles.push(OverlayRowStyle::CardDetail);
                }
            }
            if visible.is_empty() {
                lines.push("  No Workspaces match. Keep typing or clear the search.".into());
                lines.push(String::new());
                styles.push(OverlayRowStyle::Card);
                styles.push(OverlayRowStyle::CardDetail);
            }
        }
        let stable_list_rows = self.entries.len().max(1) * 2;
        while lines.len().saturating_sub(list_start) < stable_list_rows {
            lines.push(String::new());
            styles.push(OverlayRowStyle::Plain);
        }
        let footer = if self.remote {
            vec![
                ("enter", "switch"),
                ("\u{2191}\u{2193}", "select"),
                ("esc", "close"),
            ]
        } else {
            vec![
                ("enter", "switch"),
                ("\u{2191}\u{2193}", "select"),
                ("^G+d/x", "default/stop"),
                ("esc", "close"),
            ]
        };
        let mut stable_width = footer_width(&footer)
            .max(lines[0].chars().count())
            .max(lines[1].chars().count())
            .max(lines[2].chars().count())
            .max(lines[3].chars().count())
            .max(lines[4].chars().count());
        let columns = self.columns();
        for entry in &self.entries {
            stable_width = stable_width
                .max(4 + self.title_row(entry, &columns).chars().count())
                .max(5 + self.detail_row(entry, &columns).chars().count());
        }
        for line in &mut lines {
            line.extend(std::iter::repeat_n(
                ' ',
                stable_width.saturating_sub(line.chars().count()),
            ));
        }
        Overlay::with_footer("Manage Workspaces", lines, &footer).with_row_styles(styles)
    }

    /// Column widths shared by every row so names, states, and counts line
    /// up down the list regardless of which rows the search keeps.
    fn columns(&self) -> Columns {
        let digits = |value: u32| value.to_string().len();
        Columns {
            name: self
                .entries
                .iter()
                .map(|entry| entry.name.chars().count())
                .max()
                .unwrap_or(0)
                .max(8),
            projects: self
                .entries
                .iter()
                .map(|e| digits(e.projects))
                .max()
                .unwrap_or(1),
            tabs: self
                .entries
                .iter()
                .map(|e| digits(e.windows))
                .max()
                .unwrap_or(1),
            panes: self
                .entries
                .iter()
                .map(|e| digits(e.panes))
                .max()
                .unwrap_or(1),
        }
    }

    fn title_row(&self, entry: &SessionEntry, columns: &Columns) -> String {
        let state = if entry.current {
            "ATTACHED"
        } else if entry.running {
            "RUNNING"
        } else {
            "STOPPED"
        };
        let default = if !self.remote && entry.name == self.default_workspace {
            "  DEFAULT"
        } else {
            ""
        };
        format!(
            "{:<name$}   {state:<8}{default}",
            entry.name,
            name = columns.name
        )
    }

    fn detail_row(&self, entry: &SessionEntry, columns: &Columns) -> String {
        let panes = if entry.running {
            format!("  \u{00B7}  {}", count(entry.panes, "pane", columns.panes))
        } else {
            String::new()
        };
        format!(
            "{}  \u{00B7}  {}{panes}",
            count(entry.projects, "project", columns.projects),
            count(entry.windows, "tab", columns.tabs)
        )
    }

    /// The entry index under an overlay content-line index. Search, action,
    /// and section rows precede two styled rows for each Workspace.
    pub fn entry_at(&self, content_line: usize) -> Option<usize> {
        let row = content_line.checked_sub(SESSION_LIST_START)?;
        self.visible_indices().get(row / 2).copied()
    }

    /// Resolve a click to the same actions exposed by the keyboard.
    pub fn click(&mut self, cols: u16, rows: u16, x: u16, y: u16) -> SessionAction {
        self.action_pending = false;
        let overlay = self.overlay();
        let Some(row) = overlay.row_at(cols, rows, x, y) else {
            return SessionAction::Close;
        };
        if row == DEFAULT_ACTION_ROW && !self.remote {
            let rect = overlay.geometry(cols, rows);
            let Some(column) = x.checked_sub(rect.x + 2).map(usize::from) else {
                return SessionAction::None;
            };
            let end = DEFAULT_BUTTON_INDENT + DEFAULT_BUTTON.chars().count();
            if (DEFAULT_BUTTON_INDENT..end).contains(&column) {
                return self.selected().map_or(SessionAction::None, |entry| {
                    SessionAction::SetDefault(entry.name.clone())
                });
            }
            return SessionAction::None;
        }
        self.entry_at(row)
            .and_then(|index| self.entries.get(index))
            .map_or(SessionAction::None, |entry| {
                if entry.current {
                    SessionAction::Close
                } else if entry.running {
                    SessionAction::Switch {
                        name: entry.name.clone(),
                        path: entry.path.clone(),
                    }
                } else {
                    SessionAction::Revive(entry.name.clone())
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, current: bool) -> SessionEntry {
        SessionEntry {
            name: name.into(),
            path: PathBuf::from(format!("/tmp/{name}.sock")),
            windows: 1,
            panes: 2,
            projects: 1,
            running: true,
            current,
        }
    }

    #[test]
    fn selection_wraps_and_rows_map_to_entries() {
        let mut st = SessionsState::with_entries(vec![entry("alpha", false), entry("beta", true)]);
        st.next();
        assert_eq!(st.sel, 0);
        st.prev();
        assert_eq!(st.sel, 1);
        // Composer, default action, and section header precede two-row cards.
        assert_eq!(st.entry_at(0), None);
        assert_eq!(st.entry_at(4), None);
        assert_eq!(st.entry_at(5), Some(0));
        assert_eq!(st.entry_at(6), Some(0));
        assert_eq!(st.entry_at(7), Some(1));
        assert_eq!(st.entry_at(8), Some(1));
        assert_eq!(st.entry_at(9), None);
    }

    #[test]
    fn overlay_lists_and_marks() {
        let mut st = SessionsState::with_entries(vec![entry("alpha", false), entry("beta", true)]);
        st.sel = 0;
        let ov = st.overlay();
        let text = ov.lines.join("\n");
        assert!(text.contains("alpha"));
        assert!(text.contains("beta"));
        assert!(text.contains("ATTACHED"));
        assert!(ov.lines[5].starts_with(" \u{25B8}")); // selection marker
        assert_eq!(ov.row_style_at(1), OverlayRowStyle::ComposerInput);
        assert_eq!(ov.row_style_at(5), OverlayRowStyle::CardSelected);
        assert_eq!(ov.row_style_at(6), OverlayRowStyle::CardSelectedDetail);
        assert_eq!(ov.row_style_at(7), OverlayRowStyle::Card);
        assert_eq!(ov.row_style_at(8), OverlayRowStyle::CardDetail);
    }

    #[test]
    fn stopping_keeps_a_dormant_row() {
        let mut st = SessionsState::with_entries(vec![entry("a", false), entry("b", false)]);
        st.sel = 1;
        st.mark_stopped(1);
        assert_eq!(st.sel, 1);
        assert_eq!(st.entries.len(), 2);
        assert!(!st.entries[1].running);
        assert_eq!(st.entries[1].panes, 0);
    }

    #[test]
    fn typing_filters_workspace_names_and_enter_switches() {
        let mut st = SessionsState::with_entries(vec![
            entry("personal", true),
            entry("xterm-release", false),
            entry("website", false),
        ]);
        assert_eq!(st.handle(b"XTERM-REL"), SessionAction::Redraw);
        assert_eq!(st.sel, 1);
        assert_eq!(st.entry_at(5), Some(1));
        assert_eq!(st.entry_at(7), None);
        let text = st.overlay().lines.join("\n");
        assert!(text.contains("xterm-release"));
        assert!(!text.contains("personal"));
        assert!(!text.contains("website"));
        assert_eq!(
            st.handle(b"\r"),
            SessionAction::Switch {
                name: "xterm-release".into(),
                path: PathBuf::from("/tmp/xterm-release.sock"),
            }
        );

        st.handle(b"\x15missing");
        assert_eq!(st.handle(b"\r"), SessionAction::None);
        assert_eq!(st.handle(b"\t"), SessionAction::None);
        assert_eq!(st.handle(b"x"), SessionAction::Redraw);
    }

    #[test]
    fn remote_catalog_switches_by_host_workspace_name() {
        let mut state = SessionsState::from_remote(
            vec![
                WorkspaceInfo {
                    name: "Personal".into(),
                    windows: 1,
                    panes: 1,
                    projects: 1,
                    running: true,
                },
                WorkspaceInfo {
                    name: "Work".into(),
                    windows: 3,
                    panes: 5,
                    projects: 2,
                    running: true,
                },
            ],
            "Personal",
        );
        state.next();
        assert_eq!(
            state.handle(b"\r"),
            SessionAction::Switch {
                name: "Work".into(),
                path: PathBuf::from("Work"),
            }
        );
        assert!(state.overlay().lines[3].contains("REMOTE HOST"));
    }

    #[test]
    fn workspace_filter_is_case_insensitive() {
        let mut st = SessionsState::with_entries(vec![
            entry("Personal", true),
            entry("Work", false),
            entry("Release", false),
        ]);
        assert_eq!(st.handle(b"work"), SessionAction::Redraw);
        assert_eq!(st.visible_indices(), [1]);
        assert_eq!(st.sel, 1);
    }

    #[test]
    fn stopped_workspaces_are_labeled_and_revived_on_enter() {
        let mut stopped = entry("remembered", false);
        stopped.running = false;
        stopped.panes = 0;
        stopped.windows = 3;
        stopped.projects = 2;
        let mut state = SessionsState::with_entries(vec![entry("live", true), stopped]);
        state.sel = 1;
        let rendered = state.overlay().lines.join("\n");
        assert!(rendered.contains("remembered"));
        assert!(rendered.contains("STOPPED"));
        assert!(rendered.contains("2 projects  \u{00B7}  3 tabs"));
        assert_eq!(
            state.handle(b"\r"),
            SessionAction::Revive("remembered".into())
        );
        assert_eq!(state.handle(&[0x07, b'x']), SessionAction::None);
    }

    #[test]
    fn tab_is_inert_and_ctrl_g_x_kills() {
        let mut st =
            SessionsState::with_entries(vec![entry("personal", true), entry("release", false)]);
        st.handle(b"\x1b[B");
        assert_eq!(st.handle(b"\t"), SessionAction::None);
        assert_eq!(
            st.handle(b"\x07X"),
            SessionAction::Kill {
                index: 1,
                path: PathBuf::from("/tmp/release.sock"),
            }
        );
    }

    #[test]
    fn filtering_and_tab_keep_modal_geometry_stable() {
        let mut st = SessionsState::with_entries(vec![
            entry("personal", true),
            entry("release", false),
            entry("website", false),
        ]);
        let before = st.overlay().geometry(120, 40);
        assert_eq!(st.handle(b"release"), SessionAction::Redraw);
        assert_eq!(st.overlay().geometry(120, 40), before);
        assert_eq!(st.handle(b"\t"), SessionAction::None);
        assert_eq!(st.overlay().geometry(120, 40), before);
    }

    #[test]
    fn default_workspace_config_preserves_other_settings() {
        let existing = "theme = nord\ndefault-workspace = old\nsidebar = true\n";
        let merged = merge_default_workspace(existing, "Work");
        assert!(merged.contains("theme = nord\n"));
        assert!(merged.contains("sidebar = true\n"));
        assert_eq!(
            configured_default_workspace(&merged).as_deref(),
            Some("Work")
        );
        assert_eq!(
            merged.matches(uniterm_proto::DEFAULT_WORKSPACE_KEY).count(),
            1
        );
    }

    #[test]
    fn modal_marks_default_and_exposes_keyboard_action() {
        let mut state = SessionsState::with_entries_and_default(
            vec![entry("personal", true), entry("Work", false)],
            "Work".into(),
        );
        state.sel = 1;
        let text = state.overlay().lines.join("\n");
        assert!(text.contains("DEFAULT  \u{00B7}  Work"));
        let work_row = text
            .lines()
            .find(|line| line.contains("Work") && line.contains("RUNNING"))
            .expect("Work row");
        assert!(work_row.ends_with("DEFAULT") || work_row.contains("DEFAULT "));
        assert_eq!(
            state.handle(&[0x07, b'd']),
            SessionAction::SetDefault("Work".into())
        );
    }

    #[test]
    fn default_button_is_clickable_without_switching_workspace() {
        let mut state = SessionsState::with_entries(vec![entry("personal", true)]);
        let overlay = state.overlay();
        let rect = overlay.geometry(120, 40);
        assert_eq!(
            state.click(
                120,
                40,
                rect.x + 2 + DEFAULT_BUTTON_INDENT as u16,
                rect.y + 1 + DEFAULT_ACTION_ROW as u16,
            ),
            SessionAction::SetDefault("personal".into())
        );
    }
}
