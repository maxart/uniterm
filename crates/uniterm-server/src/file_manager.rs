//! Event-driven state for the optional Project file manager.
//!
//! The core owns only the visible tree and input state. Directory reads,
//! mutations, and OS watches cross the typed runtime seam in `uniterm-proto`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use uniterm_core::ProjectId;
use uniterm_proto::{FileEntry, FileOperation, GitChangeStats, FILE_LISTING_LIMIT};

#[derive(Clone, Debug)]
pub struct FileRow {
    pub path: String,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub expanded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptKind {
    File,
    Directory,
    Rename,
}

#[derive(Clone, Debug)]
struct Prompt {
    kind: PromptKind,
    buffer: String,
    /// Creation parent for new entries, or the source path for a rename.
    target: String,
}

#[derive(Clone, Debug)]
pub enum FileAction {
    None,
    Redraw,
    Refresh(Vec<String>),
    Mutate(FileOperation),
    Open(String),
    Copy(String),
    Blur,
}

pub struct FileManager {
    pub project: ProjectId,
    pub root: String,
    pub focused: bool,
    pub selected: usize,
    pub show_hidden: bool,
    pub error: Option<String>,
    viewport_first: usize,
    git_stats: Option<GitChangeStats>,
    listings: HashMap<String, Vec<FileEntry>>,
    expanded: HashSet<String>,
    pending: HashSet<String>,
    prompt: Option<Prompt>,
    confirm_delete: bool,
}

impl FileManager {
    pub fn new(project: ProjectId, root: String) -> Self {
        let mut expanded = HashSet::new();
        expanded.insert(root.clone());
        FileManager {
            project,
            root,
            focused: false,
            selected: 0,
            show_hidden: false,
            error: None,
            viewport_first: 0,
            git_stats: None,
            listings: HashMap::new(),
            expanded,
            pending: HashSet::new(),
            prompt: None,
            confirm_delete: false,
        }
    }

    pub fn reset(&mut self, project: ProjectId, root: String, focus: bool) {
        *self = Self::new(project, root);
        self.focused = focus;
    }

    pub fn request(&mut self, directory: &str) -> bool {
        self.pending.insert(directory.to_string())
    }

    pub fn finish_listing(
        &mut self,
        project: ProjectId,
        directory: String,
        entries: Vec<FileEntry>,
        truncated: bool,
        error: Option<String>,
    ) -> bool {
        if project != self.project {
            return false;
        }
        self.pending.remove(&directory);
        if let Some(error) = error {
            self.error = Some(error);
        } else {
            self.listings.insert(directory, entries);
            self.error =
                truncated.then(|| format!("Showing the first {FILE_LISTING_LIMIT} entries"));
        }
        self.clamp_selection();
        true
    }

    pub fn finish_mutation(&mut self, project: ProjectId, error: Option<String>) -> bool {
        if project != self.project {
            return false;
        }
        self.error = error;
        true
    }

    /// Apply a repository summary only when it belongs to the visible Project.
    pub fn finish_git_stats(&mut self, project: ProjectId, stats: Option<GitChangeStats>) -> bool {
        if project != self.project || self.git_stats == stats {
            return false;
        }
        self.git_stats = stats;
        true
    }

    /// Return the visible Project's cached repository summary.
    pub fn git_stats(&self) -> Option<&GitChangeStats> {
        self.git_stats.as_ref()
    }

    pub fn watched_directories(&self) -> Vec<String> {
        let mut watched: Vec<String> = self.expanded.iter().cloned().collect();
        watched.sort();
        watched
    }

    pub fn rows(&self) -> Vec<FileRow> {
        let mut rows = Vec::new();
        self.append_rows(&self.root, 0, &mut rows);
        rows
    }

    /// Return the stable first visible row for a viewport of `capacity` rows.
    pub fn first_visible(&self, capacity: usize) -> usize {
        self.viewport_first
            .min(self.rows().len().saturating_sub(capacity))
    }

    /// Keep the selected row visible without moving a viewport that already
    /// contains it. Mouse selection therefore never makes the tree jump.
    pub fn sync_viewport(&mut self, capacity: usize) {
        if capacity == 0 {
            self.viewport_first = 0;
            return;
        }
        let len = self.rows().len();
        self.viewport_first = self.viewport_first.min(len.saturating_sub(capacity));
        if self.selected < self.viewport_first {
            self.viewport_first = self.selected;
        } else if self.selected >= self.viewport_first.saturating_add(capacity) {
            self.viewport_first = self.selected.saturating_add(1).saturating_sub(capacity);
        }
    }

    fn append_rows(&self, directory: &str, depth: usize, rows: &mut Vec<FileRow>) {
        let Some(entries) = self.listings.get(directory) else {
            return;
        };
        for entry in entries {
            if !self.show_hidden && entry.name.starts_with('.') {
                continue;
            }
            let expanded = entry.is_dir && self.expanded.contains(&entry.path);
            rows.push(FileRow {
                path: entry.path.clone(),
                name: entry.name.clone(),
                depth,
                is_dir: entry.is_dir,
                is_symlink: entry.is_symlink,
                expanded,
            });
            if expanded {
                self.append_rows(&entry.path, depth + 1, rows);
            }
        }
    }

    pub fn prompt_label(&self) -> Option<(&'static str, &str)> {
        self.prompt.as_ref().map(|prompt| {
            let label = match prompt.kind {
                PromptKind::File => "New file",
                PromptKind::Directory => "New folder",
                PromptKind::Rename => "Rename",
            };
            (label, prompt.buffer.as_str())
        })
    }

    pub fn status_line(&self) -> String {
        if self.confirm_delete {
            return "Delete recursively? Enter confirms".into();
        }
        if let Some(error) = &self.error {
            return error.clone();
        }
        let rows = self.rows();
        let Some(row) = rows.get(self.selected) else {
            return "Empty folder".into();
        };
        if row.is_dir {
            "Folder".into()
        } else {
            row.path
                .strip_prefix(&self.root)
                .unwrap_or(&row.path)
                .trim_start_matches('/')
                .to_string()
        }
    }

    pub fn handle(&mut self, input: &[u8]) -> FileAction {
        if self.prompt.is_some() {
            return self.handle_prompt(input);
        }
        if self.confirm_delete {
            self.confirm_delete = false;
            if matches!(input.first(), Some(b'\r' | b'\n' | b'd' | b'D')) {
                if let Some(row) = self.rows().get(self.selected) {
                    return FileAction::Mutate(FileOperation::Delete {
                        path: row.path.clone(),
                    });
                }
            }
            return FileAction::Redraw;
        }
        let rows = self.rows();
        let key = decode_key(input);
        match key {
            Key::Escape | Key::Char('q') => {
                self.focused = false;
                FileAction::Blur
            }
            Key::Up | Key::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                FileAction::Redraw
            }
            Key::Down | Key::Char('j') => {
                if self.selected + 1 < rows.len() {
                    self.selected += 1;
                }
                FileAction::Redraw
            }
            Key::Left | Key::Char('h') => {
                let Some(row) = rows.get(self.selected) else {
                    return FileAction::None;
                };
                if row.is_dir && row.expanded {
                    self.collapse(&row.path);
                } else if let Some(parent) = Path::new(&row.path).parent() {
                    let parent = parent.to_string_lossy();
                    if let Some(index) = rows.iter().position(|item| item.path == parent) {
                        self.selected = index;
                    }
                }
                FileAction::Redraw
            }
            Key::Right | Key::Char('l') => self.open_selected(&rows, false),
            Key::Enter | Key::Char('o') => self.open_selected(&rows, true),
            Key::Char('r') => FileAction::Refresh(self.watched_directories()),
            Key::Char('.') => {
                self.show_hidden = !self.show_hidden;
                self.clamp_selection();
                FileAction::Redraw
            }
            Key::Char('n') => {
                self.prompt = Some(Prompt {
                    kind: PromptKind::File,
                    buffer: String::new(),
                    target: self.creation_parent(&rows),
                });
                FileAction::Redraw
            }
            Key::Char('N') => {
                self.prompt = Some(Prompt {
                    kind: PromptKind::Directory,
                    buffer: String::new(),
                    target: self.creation_parent(&rows),
                });
                FileAction::Redraw
            }
            Key::Char('R') => {
                if let Some(row) = rows.get(self.selected) {
                    self.prompt = Some(Prompt {
                        kind: PromptKind::Rename,
                        buffer: row.name.clone(),
                        target: row.path.clone(),
                    });
                }
                FileAction::Redraw
            }
            Key::Char('d') => {
                if !rows.is_empty() {
                    self.confirm_delete = true;
                }
                FileAction::Redraw
            }
            Key::Char('y') => rows
                .get(self.selected)
                .map(|row| FileAction::Copy(row.path.clone()))
                .unwrap_or(FileAction::None),
            Key::Char('Y') => rows
                .get(self.selected)
                .map(|row| FileAction::Copy(self.relative_path(&row.path)))
                .unwrap_or(FileAction::None),
            _ => FileAction::None,
        }
    }

    fn creation_parent(&self, rows: &[FileRow]) -> String {
        rows.get(self.selected)
            .filter(|row| row.is_dir)
            .map(|row| row.path.clone())
            .or_else(|| {
                rows.get(self.selected).and_then(|row| {
                    Path::new(&row.path)
                        .parent()
                        .map(|path| path.to_string_lossy().into_owned())
                })
            })
            .unwrap_or_else(|| self.root.clone())
    }

    /// Return the selected row for a right-click without treating a repeated
    /// click as an open request.
    pub fn select_at(&mut self, row: usize, first: usize, count: usize) -> Option<FileRow> {
        if row >= count {
            return None;
        }
        let selected = first + row;
        let item = self.rows().get(selected).cloned()?;
        self.focused = true;
        self.selected = selected;
        Some(item)
    }

    /// Expand/collapse a folder or open a file selected by a context menu.
    pub fn open_path(&mut self, path: &str, is_dir: bool) -> FileAction {
        if !is_dir {
            return FileAction::Open(path.to_string());
        }
        if self.expanded.contains(path) {
            self.collapse(path);
            FileAction::Redraw
        } else {
            self.expanded.insert(path.to_string());
            FileAction::Refresh(vec![path.to_string()])
        }
    }

    /// Start a create prompt with a stable parent captured when the menu opens.
    pub fn begin_create(&mut self, path: &str, target_is_dir: bool, directory: bool) -> FileAction {
        let parent = if target_is_dir {
            path.to_string()
        } else {
            Path::new(path)
                .parent()
                .unwrap_or_else(|| Path::new(&self.root))
                .to_string_lossy()
                .into_owned()
        };
        self.focused = true;
        self.prompt = Some(Prompt {
            kind: if directory {
                PromptKind::Directory
            } else {
                PromptKind::File
            },
            buffer: String::new(),
            target: parent,
        });
        FileAction::Redraw
    }

    /// Start a rename prompt for the exact row that was right-clicked.
    pub fn begin_rename(&mut self, path: &str, name: &str) -> FileAction {
        self.focused = true;
        self.prompt = Some(Prompt {
            kind: PromptKind::Rename,
            buffer: name.to_string(),
            target: path.to_string(),
        });
        FileAction::Redraw
    }

    /// Express a file-tree path relative to the active Project root.
    pub fn relative_path(&self, path: &str) -> String {
        Path::new(path)
            .strip_prefix(&self.root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(|relative| relative.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                if path == self.root {
                    ".".into()
                } else {
                    path.to_string()
                }
            })
    }

    /// Toggle hidden rows from the file-manager context menu.
    pub fn toggle_hidden(&mut self) -> FileAction {
        self.show_hidden = !self.show_hidden;
        self.clamp_selection();
        FileAction::Redraw
    }

    fn open_selected(&mut self, rows: &[FileRow], open_file: bool) -> FileAction {
        let Some(row) = rows.get(self.selected) else {
            return FileAction::None;
        };
        if !row.is_dir {
            return if open_file {
                FileAction::Open(row.path.clone())
            } else {
                FileAction::None
            };
        }
        if row.expanded {
            self.collapse(&row.path);
            FileAction::Redraw
        } else {
            self.expanded.insert(row.path.clone());
            FileAction::Refresh(vec![row.path.clone()])
        }
    }

    fn collapse(&mut self, path: &str) {
        let prefix = format!("{path}/");
        self.expanded
            .retain(|expanded| expanded != path && !expanded.starts_with(&prefix));
        self.clamp_selection();
    }

    fn handle_prompt(&mut self, input: &[u8]) -> FileAction {
        let Some(prompt) = self.prompt.as_mut() else {
            return FileAction::None;
        };
        if input == b"\x1b" || input == b"\x03" {
            self.prompt = None;
            return FileAction::Redraw;
        }
        if matches!(input.first(), Some(b'\r' | b'\n')) {
            let prompt = self.prompt.take().unwrap();
            let operation = match prompt.kind {
                PromptKind::File => FileOperation::CreateFile {
                    parent: prompt.target,
                    name: prompt.buffer,
                },
                PromptKind::Directory => FileOperation::CreateDirectory {
                    parent: prompt.target,
                    name: prompt.buffer,
                },
                PromptKind::Rename => FileOperation::Rename {
                    path: prompt.target,
                    name: prompt.buffer,
                },
            };
            return FileAction::Mutate(operation);
        }
        if input.starts_with(b"\x1b\x7f") || input.starts_with(b"\x1b\x08") || input == b"\x17" {
            delete_word(&mut prompt.buffer);
        } else {
            for &byte in input {
                match byte {
                    0x7f | 0x08 => {
                        prompt.buffer.pop();
                    }
                    0x15 => prompt.buffer.clear(),
                    value if (0x20..0x7f).contains(&value) => prompt.buffer.push(value as char),
                    _ => {}
                }
            }
        }
        FileAction::Redraw
    }

    pub fn click(&mut self, row: usize, first: usize, count: usize) -> FileAction {
        if row >= count {
            return FileAction::None;
        }
        let selected = first + row;
        if selected >= self.rows().len() {
            return FileAction::None;
        }
        self.focused = true;
        if self.selected == selected {
            let rows = self.rows();
            self.open_selected(&rows, true)
        } else {
            self.selected = selected;
            FileAction::Redraw
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.rows().len();
        self.selected = self.selected.min(len.saturating_sub(1));
        self.viewport_first = self.viewport_first.min(len.saturating_sub(1));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Escape,
    Unknown,
}

fn decode_key(input: &[u8]) -> Key {
    match input {
        b"\x1b[A" => Key::Up,
        b"\x1b[B" => Key::Down,
        b"\x1b[C" => Key::Right,
        b"\x1b[D" => Key::Left,
        b"\x1b" | b"\x03" => Key::Escape,
        b"\r" | b"\n" => Key::Enter,
        [value] if (0x20..0x7f).contains(value) => Key::Char(*value as char),
        _ => Key::Unknown,
    }
}

fn delete_word(value: &mut String) {
    while value.ends_with(|ch: char| ch.is_whitespace() || matches!(ch, '/' | '-' | '_' | '.')) {
        value.pop();
    }
    while value.ends_with(|ch: char| !ch.is_whitespace() && !matches!(ch, '/' | '-' | '_' | '.')) {
        value.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_only_descends_into_expanded_directories() {
        let mut manager = FileManager::new(ProjectId(1), "/work".into());
        manager.finish_listing(
            ProjectId(1),
            "/work".into(),
            vec![FileEntry {
                name: "src".into(),
                path: "/work/src".into(),
                is_dir: true,
                is_symlink: false,
                size: 0,
            }],
            false,
            None,
        );
        manager.finish_listing(
            ProjectId(1),
            "/work/src".into(),
            vec![FileEntry {
                name: "main.rs".into(),
                path: "/work/src/main.rs".into(),
                is_dir: false,
                is_symlink: false,
                size: 10,
            }],
            false,
            None,
        );
        assert_eq!(manager.rows().len(), 1);
        assert!(matches!(manager.handle(b"\r"), FileAction::Refresh(_)));
        assert_eq!(manager.rows().len(), 2);
    }

    #[test]
    fn option_delete_edits_prompt_instead_of_blurring_sidebar() {
        let mut manager = FileManager::new(ProjectId(1), "/work".into());
        manager.focused = true;
        manager.handle(b"n");
        manager.handle(b"hello world");
        manager.handle(b"\x1b\x7f");
        assert_eq!(manager.prompt_label(), Some(("New file", "hello ")));
        assert!(manager.focused);
    }

    #[test]
    fn copies_files_and_folders_relative_to_the_project_root() {
        let manager = FileManager::new(ProjectId(1), "/work".into());
        assert_eq!(manager.relative_path("/work/src"), "src");
        assert_eq!(manager.relative_path("/work/src/main.rs"), "src/main.rs");
        assert_eq!(manager.relative_path("/work"), ".");
    }

    #[test]
    fn context_create_prompt_keeps_the_clicked_parent() {
        let mut manager = FileManager::new(ProjectId(1), "/work".into());
        assert!(matches!(
            manager.begin_create("/work/src/main.rs", false, false),
            FileAction::Redraw
        ));
        manager.handle(b"notes.txt");
        assert!(matches!(
            manager.handle(b"\r"),
            FileAction::Mutate(FileOperation::CreateFile { parent, name })
                if parent == "/work/src" && name == "notes.txt"
        ));
    }

    #[test]
    fn git_stats_are_scoped_to_the_visible_project() {
        let mut manager = FileManager::new(ProjectId(1), "/work".into());
        let stats = GitChangeStats {
            files_changed: 1,
            insertions: 4,
            deletions: 2,
            untracked: 1,
        };
        assert!(!manager.finish_git_stats(ProjectId(2), Some(stats.clone())));
        assert!(manager.git_stats().is_none());
        assert!(manager.finish_git_stats(ProjectId(1), Some(stats.clone())));
        assert_eq!(manager.git_stats(), Some(&stats));
        assert!(!manager.finish_git_stats(ProjectId(1), Some(stats)));
        manager.reset(ProjectId(2), "/other".into(), false);
        assert!(manager.git_stats().is_none());
    }

    #[test]
    fn a_truncated_directory_remains_browsable_and_reports_the_limit() {
        let mut manager = FileManager::new(ProjectId(1), "/work".into());
        manager.finish_listing(
            ProjectId(1),
            "/work".into(),
            vec![FileEntry {
                name: "visible.txt".into(),
                path: "/work/visible.txt".into(),
                is_dir: false,
                is_symlink: false,
                size: 1,
            }],
            true,
            None,
        );
        assert_eq!(manager.rows().len(), 1);
        assert_eq!(
            manager.status_line(),
            format!("Showing the first {FILE_LISTING_LIMIT} entries")
        );
    }
}
