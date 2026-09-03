//! Read-only adapter for Uniterm Desktop's hierarchy persistence.
//!
//! The adapter intentionally keeps only Workspaces, Projects with paths, and
//! Tabs. Pane processes, split trees, scrollback, and agent state are runtime
//! details and are never imported.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use uniterm_proto::{ImportedProject, ImportedTab, ImportedWorkspace};

const APP_DIR: &str = "com.uniterm.app";

#[derive(Clone, Debug)]
pub struct DesktopMigration {
    pub data_dir: PathBuf,
    pub workspaces: Vec<DesktopWorkspace>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct DesktopWorkspace {
    pub source_id: String,
    pub name: String,
    pub projects: Vec<DesktopProject>,
}

#[derive(Clone, Debug)]
pub struct DesktopProject {
    pub source_id: String,
    pub name: String,
    pub path: String,
    pub tabs: Vec<ImportedTab>,
}

impl DesktopWorkspace {
    pub fn imported(&self, projects: Vec<ImportedProject>) -> ImportedWorkspace {
        ImportedWorkspace {
            source_id: self.source_id.clone(),
            projects,
        }
    }
}

#[derive(Deserialize)]
struct WorkspaceRecord {
    id: String,
    name: String,
    #[serde(default)]
    sort_order: i32,
}

#[derive(Deserialize)]
struct ProjectRecord {
    id: String,
    name: String,
    path: String,
    #[serde(default)]
    sort_order: i32,
    #[serde(default = "default_workspace_id")]
    workspace_id: String,
}

#[derive(Deserialize)]
struct ProjectSnapshot {
    project_id: String,
    #[serde(default)]
    tabs: Vec<TabSnapshot>,
}

#[derive(Deserialize)]
struct TabSnapshot {
    #[serde(default)]
    custom_title: Option<String>,
}

fn default_workspace_id() -> String {
    "default".into()
}

/// Locate Desktop data using the exact OS directories its Rust host uses.
/// `UNITERM_DESKTOP_DATA_DIR` is checked first for portable or unusual
/// installations, followed by the native platform candidates.
pub fn detect_data_dir(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return validate_data_dir(path);
    }
    let candidates = candidate_data_dirs_for(std::env::consts::OS, |key| std::env::var_os(key));
    for candidate in &candidates {
        if is_desktop_data_dir(candidate) {
            return Ok(candidate.clone());
        }
    }
    let searched = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(if searched.is_empty() {
        "Uniterm Desktop data was not found; set UNITERM_DESKTOP_DATA_DIR".into()
    } else {
        format!("Uniterm Desktop data was not found (searched: {searched})")
    })
}

fn validate_data_dir(path: &Path) -> Result<PathBuf, String> {
    if is_desktop_data_dir(path) {
        Ok(path.to_path_buf())
    } else {
        Err(format!(
            "{} is not a Uniterm Desktop data directory (projects.json is missing)",
            path.display()
        ))
    }
}

fn is_desktop_data_dir(path: &Path) -> bool {
    path.join("projects.json").is_file() || path.join("projects.json.bak").is_file()
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn candidate_data_dirs_for(os: &str, env: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = env("UNITERM_DESKTOP_DATA_DIR") {
        push_unique(&mut paths, PathBuf::from(path));
    }
    match os {
        "linux" | "freebsd" | "openbsd" | "netbsd" | "dragonfly" => {
            if let Some(base) = env("XDG_DATA_HOME") {
                push_unique(&mut paths, PathBuf::from(base).join(APP_DIR));
            }
            if let Some(home) = env("HOME") {
                let home = PathBuf::from(home);
                push_unique(&mut paths, home.join(".local/share").join(APP_DIR));
                // Flatpak exposes XDG data below the application sandbox.
                push_unique(
                    &mut paths,
                    home.join(".var/app")
                        .join(APP_DIR)
                        .join("data")
                        .join(APP_DIR),
                );
            }
        }
        "macos" => {
            if let Some(home) = env("HOME") {
                push_unique(
                    &mut paths,
                    PathBuf::from(home)
                        .join("Library/Application Support")
                        .join(APP_DIR),
                );
            }
        }
        "windows" => {
            if let Some(base) = env("APPDATA") {
                push_unique(&mut paths, PathBuf::from(base).join(APP_DIR));
            }
            // The released app uses roaming data, but keep local data as a
            // fallback for older or repackaged installations.
            if let Some(base) = env("LOCALAPPDATA") {
                push_unique(&mut paths, PathBuf::from(base).join(APP_DIR));
            }
        }
        _ => {}
    }
    paths
}

/// Load the three Desktop hierarchy projections and join them by their stable
/// ids. Backup files are accepted when the live atomic file is unavailable.
pub fn load(data_dir: PathBuf) -> Result<DesktopMigration, String> {
    let mut warnings = Vec::new();
    let mut workspaces: Vec<WorkspaceRecord> =
        read_list_with_backup(&data_dir.join("project_workspaces.json"), false)?;
    let mut projects: Vec<ProjectRecord> =
        read_list_with_backup(&data_dir.join("projects.json"), true)?;
    let snapshots: Vec<ProjectSnapshot> =
        read_list_with_backup(&data_dir.join("workspaces_v3.json"), false)?;

    workspaces.sort_by_key(|workspace| workspace.sort_order);
    projects.sort_by_key(|project| project.sort_order);
    if workspaces.is_empty() && !projects.is_empty() {
        workspaces.push(WorkspaceRecord {
            id: "default".into(),
            name: "Default".into(),
            sort_order: 0,
        });
        warnings.push("Desktop workspace catalog was missing; using Default".into());
    }

    let snapshots: HashMap<String, ProjectSnapshot> = snapshots
        .into_iter()
        .map(|snapshot| (snapshot.project_id.clone(), snapshot))
        .collect();
    let known: HashSet<String> = workspaces
        .iter()
        .map(|workspace| workspace.id.clone())
        .collect();
    for missing in projects
        .iter()
        .map(|project| project.workspace_id.clone())
        .filter(|id| !known.contains(id))
        .collect::<HashSet<_>>()
    {
        workspaces.push(WorkspaceRecord {
            name: format!("Recovered {missing}"),
            id: missing,
            sort_order: i32::MAX,
        });
        warnings.push("A Project referenced a missing Desktop Workspace; recovered it".into());
    }

    let mut joined = Vec::new();
    for workspace in workspaces {
        let mut children = Vec::new();
        for project in projects
            .iter()
            .filter(|project| project.workspace_id == workspace.id)
        {
            let tabs = snapshots
                .get(&project.id)
                .map(|snapshot| {
                    snapshot
                        .tabs
                        .iter()
                        .map(|tab| ImportedTab {
                            name: tab
                                .custom_title
                                .as_deref()
                                .map(str::trim)
                                .filter(|name| !name.is_empty())
                                .map(str::to_string),
                        })
                        .collect::<Vec<_>>()
                })
                .filter(|tabs| !tabs.is_empty())
                .unwrap_or_else(|| vec![ImportedTab { name: None }]);
            children.push(DesktopProject {
                source_id: project.id.clone(),
                name: project.name.trim().to_string(),
                path: project.path.clone(),
                tabs,
            });
        }
        if !children.is_empty() {
            joined.push(DesktopWorkspace {
                source_id: workspace.id,
                name: workspace.name.trim().to_string(),
                projects: children,
            });
        }
    }
    Ok(DesktopMigration {
        data_dir,
        workspaces: joined,
        warnings,
    })
}

fn read_list_with_backup<T: DeserializeOwned>(
    path: &Path,
    required: bool,
) -> Result<Vec<T>, String> {
    let backup = PathBuf::from(format!("{}.bak", path.display()));
    let mut errors = Vec::new();
    for candidate in [path, backup.as_path()] {
        match std::fs::read(candidate) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => return Ok(value),
                Err(error) => errors.push(format!("{}: {error}", candidate.display())),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => errors.push(format!("{}: {error}", candidate.display())),
        }
    }
    if required || !errors.is_empty() {
        Err(format!(
            "could not load {}{}",
            path.display(),
            if errors.is_empty() {
                String::new()
            } else {
                format!(" ({})", errors.join("; "))
            }
        ))
    } else {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn vars<'a>(values: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            values
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| OsStr::new(value).to_os_string())
        }
    }

    #[test]
    fn detection_uses_native_linux_macos_and_windows_locations() {
        let linux = candidate_data_dirs_for("linux", vars(&[("HOME", "/home/dev")]));
        assert_eq!(
            linux[0],
            PathBuf::from("/home/dev/.local/share/com.uniterm.app")
        );

        let mac = candidate_data_dirs_for("macos", vars(&[("HOME", "/Users/max")]));
        assert_eq!(
            mac[0],
            PathBuf::from("/Users/max/Library/Application Support/com.uniterm.app")
        );

        let windows = candidate_data_dirs_for(
            "windows",
            vars(&[("APPDATA", r"C:\Users\max\AppData\Roaming")]),
        );
        assert!(windows[0].to_string_lossy().ends_with(APP_DIR));
    }

    #[test]
    fn explicit_override_is_first_and_deduplicated() {
        let paths = candidate_data_dirs_for(
            "linux",
            vars(&[
                ("UNITERM_DESKTOP_DATA_DIR", "/portable/uniterm"),
                ("XDG_DATA_HOME", "/portable"),
            ]),
        );
        assert_eq!(paths[0], PathBuf::from("/portable/uniterm"));
    }

    #[test]
    fn joins_workspace_project_and_tab_records() {
        let dir =
            std::env::temp_dir().join(format!("uniterm-desktop-migration-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("project_workspaces.json"),
            r#"[{"id":"work","name":"Work","sort_order":0}]"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("projects.json"),
            r#"[{"id":"p1","name":"Uniterm","path":"/tmp","sort_order":0,"workspace_id":"work"}]"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("workspaces_v3.json"),
            r#"[{"project_id":"p1","tabs":[{"custom_title":"Build"},{"custom_title":null}]}]"#,
        )
        .unwrap();

        let migration = load(dir.clone()).unwrap();
        assert_eq!(migration.workspaces.len(), 1);
        assert_eq!(migration.workspaces[0].projects[0].tabs.len(), 2);
        assert_eq!(
            migration.workspaces[0].projects[0].tabs[0].name.as_deref(),
            Some("Build")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
