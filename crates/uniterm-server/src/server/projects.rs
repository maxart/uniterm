//! Project, Tab, and Workspace hierarchy management.
//!
//! Splitting, window and tab wiring, project switching, and Desktop import all
//! stay workspace-scoped, so a bulk action never reaches unrelated work.

use super::*;
use crate::context_menu::{ContextAction, ContextItem};

/// What detaching a Pane did to its source Tab.
enum Detached {
    TabKept,
    TabClosed,
}

impl Server {
    /// Wire a freshly spawned pane in as a split of the active pane (the New
    /// Task pattern): split in `dir`, focus it, drop any zoom.
    pub(super) fn split_active_pane(&mut self, new_id: PaneId, dir: SplitDir) {
        let wi = self.active_window;
        let active = self.windows[wi].active;
        self.windows[wi].layout.split(active, dir, new_id);
        self.last_active_pane = Some(active);
        self.windows[wi].active = new_id;
        self.windows[wi].zoomed = None;
    }

    /// Wire a freshly spawned pane in as a new window (tab) and switch to it.
    pub(super) fn push_window(&mut self, new_id: PaneId) {
        self.windows.push(Win {
            project: self.active_project,
            layout: LayoutNode::Leaf(new_id),
            active: new_id,
            zoomed: None,
            name: None,
        });
        self.active_window = self.windows.len() - 1;
        self.tab_scroll_follow_active = true;
    }

    /// "Move to" entries for a Pane's context menu: every other Tab of the
    /// same Project, numbered as the Tab bar numbers them, and a fresh Tab
    /// when the Pane is not already alone in its own.
    pub(super) fn pane_move_destinations(&self, pane: PaneId) -> Vec<ContextItem> {
        let Some(source) = self
            .windows
            .iter()
            .position(|tab| tab.layout.contains_pane(pane))
        else {
            return Vec::new();
        };
        let project = self.windows[source].project;
        let mut items = Vec::new();
        for (ordinal, index) in self.project_window_indices(project).into_iter().enumerate() {
            if index == source {
                continue;
            }
            let label = match &self.windows[index].name {
                Some(name) => format!("Move to tab {}: {name}", ordinal + 1),
                None => format!("Move to tab {}", ordinal + 1),
            };
            items.push(ContextItem::dynamic(label, ContextAction::MoveToTab(index)));
        }
        if self.windows[source].layout.pane_ids().len() > 1 {
            items.push(ContextItem::dynamic(
                "Move to new tab".into(),
                ContextAction::MoveToNewTab,
            ));
        }
        items
    }

    /// Re-home `pane` beside the active Pane of Tab `target`, splitting along
    /// that Pane's longer side. A source Tab left empty closes, exactly as it
    /// would when its last Pane exits, but the Pane and its process live on.
    pub(super) fn move_pane_to_window(
        &mut self,
        reg: &Registry,
        pane: PaneId,
        target: usize,
    ) -> bool {
        let Some(source) = self
            .windows
            .iter()
            .position(|tab| tab.layout.contains_pane(pane))
        else {
            return false;
        };
        if source == target
            || target >= self.windows.len()
            || self.windows[target].project != self.windows[source].project
        {
            return false;
        }
        let target = match self.detach_pane(source, pane) {
            Detached::TabKept => target,
            Detached::TabClosed if source < target => target - 1,
            Detached::TabClosed => target,
        };
        let (area, _) = self.chrome_area();
        let anchor = self.windows[target].active;
        let anchor_rect = self.windows[target]
            .layout
            .compute(area)
            .rect_of(anchor)
            .unwrap_or(area);
        // Prefer the split that leaves both Panes closest to square.
        let dir = if usize::from(anchor_rect.w) >= usize::from(anchor_rect.h) * 2 {
            SplitDir::Horizontal
        } else {
            SplitDir::Vertical
        };
        let tab = &mut self.windows[target];
        tab.layout.split(anchor, dir, pane);
        tab.zoomed = None;
        tab.active = pane;
        self.finish_pane_move(reg, target);
        true
    }

    /// Re-home `pane` as the only Pane of a new Tab right after its current
    /// one. A Pane already alone in its Tab has nothing to move.
    pub(super) fn move_pane_to_new_window(&mut self, reg: &Registry, pane: PaneId) -> bool {
        let Some(source) = self
            .windows
            .iter()
            .position(|tab| tab.layout.contains_pane(pane))
        else {
            return false;
        };
        if self.windows[source].layout.pane_ids().len() < 2 {
            return false;
        }
        let project = self.windows[source].project;
        // The Tab keeps at least one other Pane (checked above), so the source
        // survives. The detach must run in every build: a `debug_assert!`
        // around it once compiled the call away in release and left the Pane
        // owned by two Tabs.
        let detached = self.detach_pane(source, pane);
        debug_assert!(matches!(detached, Detached::TabKept));
        let target = source + 1;
        self.windows.insert(
            target,
            Win {
                project,
                layout: LayoutNode::Leaf(pane),
                active: pane,
                zoomed: None,
                name: None,
            },
        );
        self.finish_pane_move(reg, target);
        true
    }

    fn detach_pane(&mut self, source: usize, pane: PaneId) -> Detached {
        match self.windows[source].layout.without(pane) {
            Some(layout) => {
                let tab = &mut self.windows[source];
                if tab.active == pane {
                    tab.active = layout.first_pane();
                }
                if tab.zoomed == Some(pane) {
                    tab.zoomed = None;
                }
                tab.layout = layout;
                Detached::TabKept
            }
            None => {
                self.windows.remove(source);
                if self.active_window > source {
                    self.active_window -= 1;
                } else if self.active_window == source {
                    self.active_window = source.min(self.windows.len().saturating_sub(1));
                }
                Detached::TabClosed
            }
        }
    }

    /// Every client must see the new Tab bar, the moved Pane, and no menu, so
    /// the move ends with a full repaint rather than relying on later damage.
    fn finish_pane_move(&mut self, reg: &Registry, target: usize) {
        self.last_active_pane = None;
        self.activate_window(target);
        self.tab_scroll_follow_active = true;
        self.relayout();
        self.full_repaint_all(reg);
        self.persist();
    }

    pub(super) fn project_window_indices(&self, project: ProjectId) -> Vec<usize> {
        self.windows
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| (tab.project == project).then_some(index))
            .collect()
    }

    pub(super) fn activate_window(&mut self, index: usize) -> bool {
        if let Some(current) = self.windows.get(self.active_window) {
            if let Some(project) = self
                .projects
                .iter_mut()
                .find(|project| project.id == current.project)
            {
                project.active_pane = Some(current.active);
            }
        }
        let Some(tab) = self.windows.get(index) else {
            return false;
        };
        let project = tab.project;
        let project_changed = self.active_project != project;
        let changed = self.active_window != index || self.active_project != project;
        self.active_window = index;
        self.active_project = project;
        if changed {
            self.tab_scroll_follow_active = true;
        }
        if project_changed {
            self.tab_scroll = 0;
            self.project_scroll = self
                .projects
                .iter()
                .position(|item| item.id == project)
                .unwrap_or(0);
        }
        if let Some(project) = self.projects.iter_mut().find(|item| item.id == project) {
            project.active_pane = Some(self.windows[index].active);
        }
        if project_changed && self.file_manager_visible() {
            let keep_focus = self.files.focused;
            self.sync_file_manager(keep_focus);
        }
        changed
    }

    /// Reorder the active Tab within its Project. The durable event is
    /// appended before the in-memory projection changes so a failed append
    /// never leaves an unrecorded ordering behind.
    pub(super) fn move_active_tab(&mut self, direction: TabMoveDirection) -> bool {
        let tabs = self.project_window_indices(self.active_project);
        if tabs.len() < 2 {
            return false;
        }
        let Some(from) = tabs.iter().position(|index| *index == self.active_window) else {
            return false;
        };
        let Some(to) = tab_move_target(tabs.len(), from, direction) else {
            return false;
        };
        self.append_event(crate::eventlog::LogEvent::TabMoved {
            project: self.active_project.0,
            from: from as u32,
            to: to as u32,
        });
        self.windows.swap(tabs[from], tabs[to]);
        self.active_window = tabs[to];
        self.tab_scroll_follow_active = true;
        // The event log records the move, but restore reads the snapshot, so
        // persist on every real move regardless of the caller (key binding,
        // CLI, automation).
        self.persist();
        true
    }

    pub(super) fn switch_project(&mut self, reg: &Registry, project: ProjectId) {
        if project == self.active_project {
            return;
        }
        let preferred = self
            .projects
            .iter()
            .find(|item| item.id == project)
            .and_then(|item| item.active_pane);
        let Some(index) = self
            .project_window_indices(project)
            .into_iter()
            .find(|index| {
                preferred.is_some_and(|pane| self.windows[*index].layout.contains_pane(pane))
            })
            .or_else(|| self.project_window_indices(project).into_iter().next())
        else {
            return;
        };
        self.append_event(crate::eventlog::LogEvent::ProjectSelected { project: project.0 });
        self.activate_window(index);
        self.overview = None;
        self.relayout();
        self.full_repaint_all(reg);
        self.persist();
    }

    pub(super) fn create_project(
        &mut self,
        reg: &Registry,
        name: &str,
        root: &str,
    ) -> Result<(), String> {
        let name = name.trim();
        let root = root.trim();
        if name.is_empty() || root.is_empty() {
            return Err("Project name and folder are required".into());
        }
        let root = if root == "~" {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| "The host has no HOME folder".to_string())?
        } else if let Some(rest) = root.strip_prefix("~/") {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| "The host has no HOME folder".to_string())?
                .join(rest)
        } else {
            PathBuf::from(root)
        };
        let stored_root = root.to_string_lossy().into_owned();
        let id = ProjectId(self.next_project_id);
        let pane = self
            .spawn_pane_at(reg, &[], Some(&root))
            .map_err(|error| format!("Could not open {}: {error}", root.display()))?;
        self.next_project_id += 1;
        self.append_event(crate::eventlog::LogEvent::ProjectCreated {
            project: id.0,
            name: name.to_string(),
            root: stored_root.clone(),
        });
        self.projects.push(Project {
            id,
            name: name.to_string(),
            root: stored_root,
            active_pane: Some(pane),
            metadata: HashMap::new(),
        });
        self.windows.push(Win {
            project: id,
            layout: LayoutNode::Leaf(pane),
            active: pane,
            zoomed: None,
            name: None,
        });
        self.activate_window(self.windows.len() - 1);
        self.relayout();
        self.full_repaint_all(reg);
        self.persist();
        Ok(())
    }

    /// Apply a hierarchy-only Desktop import. All PTYs are staged before the
    /// Project and Tab projection changes, so a spawn failure leaves the
    /// existing Workspace untouched.
    pub(super) fn import_workspace(
        &mut self,
        reg: &Registry,
        workspace: &uniterm_proto::ImportedWorkspace,
        mode: uniterm_proto::WorkspaceImportMode,
    ) -> Result<(u32, u32, u32), String> {
        const MAX_PROJECTS: usize = 256;
        const MAX_TABS_PER_PROJECT: usize = 256;
        if workspace.projects.is_empty() {
            return Err("the imported Workspace has no usable Projects".into());
        }
        if workspace.projects.len() > MAX_PROJECTS {
            return Err(format!(
                "the imported Workspace exceeds {MAX_PROJECTS} Projects"
            ));
        }
        for project in &workspace.projects {
            if project.name.trim().is_empty() || project.name.len() > 256 {
                return Err("an imported Project has an invalid name".into());
            }
            if project.root.len() > 4096 || !Path::new(&project.root).is_dir() {
                return Err(format!("Project path is unavailable: {}", project.root));
            }
            if project.tabs.is_empty() || project.tabs.len() > MAX_TABS_PER_PROJECT {
                return Err(format!(
                    "Project '{}' has an invalid Tab count",
                    project.name
                ));
            }
            if project
                .tabs
                .iter()
                .filter_map(|tab| tab.name.as_ref())
                .any(|name| name.len() > 256)
            {
                return Err(format!(
                    "Project '{}' has an invalid Tab name",
                    project.name
                ));
            }
        }
        match mode {
            uniterm_proto::WorkspaceImportMode::Fresh => {
                self.import_fresh_workspace(reg, workspace)
            }
            uniterm_proto::WorkspaceImportMode::Merge => {
                self.merge_imported_workspace(reg, workspace)
            }
        }
    }

    pub(super) fn import_fresh_workspace(
        &mut self,
        reg: &Registry,
        workspace: &uniterm_proto::ImportedWorkspace,
    ) -> Result<(u32, u32, u32), String> {
        if self.projects.len() != 1 || self.windows.len() != 1 || self.panes.len() != 1 {
            return Err("fresh import requires a newly-created Workspace".into());
        }
        let first = &workspace.projects[0];
        if self.projects[0].root != first.root {
            return Err("fresh Workspace was not started at the first Project path".into());
        }

        let mut staged = Vec::new();
        for (project_index, project) in workspace.projects.iter().enumerate() {
            let first_tab = usize::from(project_index == 0);
            for tab_index in first_tab..project.tabs.len() {
                match self.spawn_pane_at(reg, &[], Some(Path::new(&project.root))) {
                    Ok(pane) => staged.push((project_index, tab_index, pane)),
                    Err(error) => {
                        let panes: Vec<_> = staged.iter().map(|(_, _, pane)| *pane).collect();
                        self.discard_staged_panes(reg, &panes);
                        return Err(format!(
                            "could not create a Tab for '{}': {error}",
                            project.name
                        ));
                    }
                }
            }
        }

        self.append_event(crate::eventlog::LogEvent::ProjectRenamed {
            project: 1,
            name: first.name.clone(),
        });
        self.append_event(crate::eventlog::LogEvent::ProjectMetadataSet {
            project: 1,
            key: "desktop.source_id".into(),
            value: first.source_id.clone(),
        });
        self.projects[0].name = first.name.clone();
        self.projects[0]
            .metadata
            .insert("desktop.source_id".into(), first.source_id.clone());
        self.windows[0].name = first.tabs[0].name.clone();
        if let Some(name) = &self.windows[0].name {
            self.append_event(crate::eventlog::LogEvent::WindowRenamed {
                window: 0,
                name: name.clone(),
            });
        }

        let mut project_ids = vec![ProjectId(1)];
        for project in workspace.projects.iter().skip(1) {
            let id = ProjectId(self.next_project_id);
            self.next_project_id += 1;
            self.append_event(crate::eventlog::LogEvent::ProjectCreated {
                project: id.0,
                name: project.name.clone(),
                root: project.root.clone(),
            });
            self.append_event(crate::eventlog::LogEvent::ProjectMetadataSet {
                project: id.0,
                key: "desktop.source_id".into(),
                value: project.source_id.clone(),
            });
            self.projects.push(Project {
                id,
                name: project.name.clone(),
                root: project.root.clone(),
                active_pane: None,
                metadata: HashMap::from([("desktop.source_id".into(), project.source_id.clone())]),
            });
            project_ids.push(id);
        }

        for (project_index, tab_index, pane) in staged {
            let project = project_ids[project_index];
            let name = workspace.projects[project_index].tabs[tab_index]
                .name
                .clone();
            let window = self.windows.len() as u64;
            self.append_event(crate::eventlog::LogEvent::WindowNew);
            if let Some(name) = &name {
                self.append_event(crate::eventlog::LogEvent::WindowRenamed {
                    window,
                    name: name.clone(),
                });
            }
            self.windows.push(Win {
                project,
                layout: LayoutNode::Leaf(pane),
                active: pane,
                zoomed: None,
                name,
            });
            if let Some(project) = self.projects.iter_mut().find(|item| item.id == project) {
                project.active_pane.get_or_insert(pane);
            }
        }
        self.active_window = 0;
        self.active_project = ProjectId(1);
        self.projects[0].active_pane = Some(self.windows[0].active);
        self.files
            .reset(ProjectId(1), first.root.clone(), self.files.focused);
        self.relayout();
        self.full_repaint_all(reg);
        self.persist();
        Ok((
            workspace.projects.len() as u32,
            workspace
                .projects
                .iter()
                .map(|project| project.tabs.len() as u32)
                .sum(),
            0,
        ))
    }

    pub(super) fn merge_imported_workspace(
        &mut self,
        reg: &Registry,
        workspace: &uniterm_proto::ImportedWorkspace,
    ) -> Result<(u32, u32, u32), String> {
        struct PendingTab {
            source_project: usize,
            project: Option<ProjectId>,
            tab: usize,
            pane: PaneId,
        }

        let mut staged: Vec<PendingTab> = Vec::new();
        let mut merged = 0u32;
        for (source_project, project) in workspace.projects.iter().enumerate() {
            let existing = self
                .projects
                .iter()
                .find(|item| item.root == project.root)
                .map(|item| item.id);
            let have = existing.map_or(0, |id| self.project_window_indices(id).len());
            if existing.is_some() {
                merged += 1;
            }
            for tab in have..project.tabs.len() {
                match self.spawn_pane_at(reg, &[], Some(Path::new(&project.root))) {
                    Ok(pane) => staged.push(PendingTab {
                        source_project,
                        project: existing,
                        tab,
                        pane,
                    }),
                    Err(error) => {
                        let panes: Vec<_> = staged.iter().map(|pending| pending.pane).collect();
                        self.discard_staged_panes(reg, &panes);
                        return Err(format!(
                            "could not create a Tab for '{}': {error}",
                            project.name
                        ));
                    }
                }
            }
        }

        let mut added_projects = 0u32;
        let mut added_tabs = 0u32;
        for source_project in 0..workspace.projects.len() {
            let source = &workspace.projects[source_project];
            let existing = self
                .projects
                .iter()
                .find(|item| item.root == source.root)
                .map(|item| item.id);
            let project = if let Some(project) = existing {
                project
            } else {
                let id = ProjectId(self.next_project_id);
                self.next_project_id += 1;
                self.append_event(crate::eventlog::LogEvent::ProjectCreated {
                    project: id.0,
                    name: source.name.clone(),
                    root: source.root.clone(),
                });
                self.append_event(crate::eventlog::LogEvent::ProjectMetadataSet {
                    project: id.0,
                    key: "desktop.source_id".into(),
                    value: source.source_id.clone(),
                });
                self.projects.push(Project {
                    id,
                    name: source.name.clone(),
                    root: source.root.clone(),
                    active_pane: None,
                    metadata: HashMap::from([(
                        "desktop.source_id".into(),
                        source.source_id.clone(),
                    )]),
                });
                added_projects += 1;
                id
            };
            for pending in staged
                .iter_mut()
                .filter(|pending| pending.source_project == source_project)
            {
                pending.project = Some(project);
            }
        }

        for pending in staged {
            let project = pending.project.expect("import Project was assigned");
            let name = workspace.projects[pending.source_project].tabs[pending.tab]
                .name
                .clone();
            let window = self.windows.len() as u64;
            self.append_event(crate::eventlog::LogEvent::WindowNew);
            if let Some(name) = &name {
                self.append_event(crate::eventlog::LogEvent::WindowRenamed {
                    window,
                    name: name.clone(),
                });
            }
            self.windows.push(Win {
                project,
                layout: LayoutNode::Leaf(pending.pane),
                active: pending.pane,
                zoomed: None,
                name,
            });
            if let Some(project) = self.projects.iter_mut().find(|item| item.id == project) {
                project.active_pane.get_or_insert(pending.pane);
            }
            added_tabs += 1;
        }
        if added_projects > 0 || added_tabs > 0 {
            self.relayout();
            self.full_repaint_all(reg);
            self.persist();
        }
        Ok((added_projects, added_tabs, merged))
    }

    pub(super) fn discard_staged_panes(&mut self, reg: &Registry, panes: &[PaneId]) {
        self.terminate_panes(panes);
        for pane in panes {
            if let Some(mut staged) = self.panes.remove(pane) {
                let _ = reg.deregister(&mut SourceFd(&staged.pty.raw_fd()));
                self.pane_tokens.remove(&staged.token);
                let _ = staged.pty.kill();
                self.append_event(crate::eventlog::LogEvent::PaneClosed { pane: pane.0 });
            }
        }
    }

    pub(super) fn rename_project(&mut self, reg: &Registry, project: ProjectId, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let Some(index) = self.projects.iter().position(|item| item.id == project) else {
            return;
        };
        self.append_event(crate::eventlog::LogEvent::ProjectRenamed {
            project: project.0,
            name: name.to_string(),
        });
        self.projects[index].name = name.to_string();
        self.full_repaint_all(reg);
        self.persist();
    }

    pub(super) fn move_project(
        &mut self,
        reg: &Registry,
        project: ProjectId,
        direction: uniterm_proto::ProjectMoveDirection,
    ) {
        let Some(from) = self.projects.iter().position(|item| item.id == project) else {
            return;
        };
        let to = match direction {
            uniterm_proto::ProjectMoveDirection::Up => from.checked_sub(1),
            uniterm_proto::ProjectMoveDirection::Down => {
                (from + 1 < self.projects.len()).then_some(from + 1)
            }
        };
        let Some(to) = to else {
            return;
        };
        let mut order: Vec<u64> = self.projects.iter().map(|item| item.id.0).collect();
        order.swap(from, to);
        self.append_event(crate::eventlog::LogEvent::ProjectReordered { projects: order });
        self.projects.swap(from, to);
        self.full_repaint_all(reg);
        self.persist();
    }

    pub(super) fn remove_project(&mut self, reg: &Registry, project: ProjectId) {
        if self.projects.len() <= 1 || !self.projects.iter().any(|item| item.id == project) {
            return;
        }
        let panes: Vec<PaneId> = self
            .windows
            .iter()
            .filter(|tab| tab.project == project)
            .flat_map(|tab| tab.layout.pane_ids())
            .collect();
        self.append_event(crate::eventlog::LogEvent::ProjectRemoved { project: project.0 });
        self.projects.retain(|item| item.id != project);
        self.sync_artifact_watches();
        if self.active_project == project {
            let replacement = self.projects[0].id;
            if let Some(index) = self.project_window_indices(replacement).into_iter().next() {
                self.activate_window(index);
            }
        }
        self.terminate_panes(&panes);
        for pane in panes {
            self.close_pane(reg, pane);
            if !self.running {
                return;
            }
        }
        self.persist();
    }

    pub(super) fn workspace_snapshot(&self) -> Vec<uniterm_proto::ProjectInfo> {
        self.projects
            .iter()
            .map(|project| {
                let tabs: Vec<&Win> = self
                    .windows
                    .iter()
                    .filter(|tab| tab.project == project.id)
                    .collect();
                let panes = tabs
                    .iter()
                    .map(|tab| tab.layout.pane_ids().len() as u32)
                    .sum();
                let attention = self.project_attention(project.id);
                uniterm_proto::ProjectInfo {
                    id: project.id,
                    name: project.name.clone(),
                    root: project.root.clone(),
                    tabs: tabs.len() as u32,
                    panes,
                    active: project.id == self.active_project,
                    attention,
                    worktree: Self::worktree_registration(project),
                }
            })
            .collect()
    }

    pub(super) fn project_attention(&self, project: ProjectId) -> u32 {
        self.windows
            .iter()
            .filter(|tab| tab.project == project)
            .flat_map(|tab| tab.layout.pane_ids())
            .filter(|pane| {
                self.panes
                    .get(pane)
                    .and_then(|pane| pane.agent.as_ref())
                    .is_some_and(|agent| agent.status.needs_human())
            })
            .count() as u32
    }

    pub(super) fn reply_workspace(&mut self, reg: &Registry, token: Token) {
        let msg = ServerMessage::Workspace {
            name: self.name.clone(),
            active_project: self.active_project,
            projects: self.workspace_snapshot(),
        };
        if let Some(client) = self.clients.get_mut(&token) {
            client.queue(&encode_frame(&msg));
            client.flush();
            let _ = set_interest(reg, client, token);
        }
    }

    /// Distinct project names seen in this session's task history (titles of
    /// the form `# project NAME: ...`), for the New Task `/project`
    /// autocomplete. Sorted, deduped.
    pub(super) fn project_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tasks
            .ordered()
            .iter()
            .filter_map(|t| {
                let rest = t.title.strip_prefix("# project ")?;
                let (name, _) = rest.split_once(':')?;
                let name = name.trim();
                (!name.is_empty()).then(|| name.to_string())
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }

    pub(super) fn project_root_for_pane(&self, pane: PaneId) -> Option<String> {
        let project = self
            .windows
            .iter()
            .find(|window| window.layout.contains_pane(pane))?
            .project;
        self.projects
            .iter()
            .find(|item| item.id == project)
            .map(|item| item.root.clone())
    }
}

/// Remove volatile Pane identities from a live layout before it enters the
/// lightweight Workspace catalog. Ratios are quantized far more finely than
/// the interactive five-percent resize step.
pub(super) fn workspace_layout_definition(
    layout: &LayoutNode,
) -> uniterm_proto::WorkspaceLayoutDefinition {
    match layout {
        LayoutNode::Leaf(_) => uniterm_proto::WorkspaceLayoutDefinition::Pane,
        LayoutNode::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let ratio = if ratio.is_finite() {
                ratio.clamp(0.0001, 0.9999)
            } else {
                0.5
            };
            uniterm_proto::WorkspaceLayoutDefinition::Split {
                dir: *dir,
                first_ratio: (ratio * 10_000.0).round() as u16,
                first: Box::new(workspace_layout_definition(first)),
                second: Box::new(workspace_layout_definition(second)),
            }
        }
    }
}

/// Assign fresh Pane identities to an anonymous catalog layout.
pub(super) fn workspace_layout_with_panes(
    definition: &uniterm_proto::WorkspaceLayoutDefinition,
    panes: &mut impl Iterator<Item = PaneId>,
) -> Option<LayoutNode> {
    match definition {
        uniterm_proto::WorkspaceLayoutDefinition::Pane => Some(LayoutNode::Leaf(panes.next()?)),
        uniterm_proto::WorkspaceLayoutDefinition::Split {
            dir,
            first_ratio,
            first,
            second,
        } => Some(LayoutNode::Split {
            dir: *dir,
            ratio: (f32::from(*first_ratio) / 10_000.0).clamp(0.0001, 0.9999),
            first: Box::new(workspace_layout_with_panes(first, panes)?),
            second: Box::new(workspace_layout_with_panes(second, panes)?),
        }),
    }
}

pub(super) fn tab_move_target(
    count: usize,
    current: usize,
    direction: TabMoveDirection,
) -> Option<usize> {
    if count < 2 || current >= count {
        return None;
    }
    Some(match direction {
        TabMoveDirection::Previous => (current + count - 1) % count,
        TabMoveDirection::Next => (current + 1) % count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_move_target_is_a_no_op_for_one_tab_and_wraps() {
        assert_eq!(tab_move_target(1, 0, TabMoveDirection::Next), None);
        assert_eq!(tab_move_target(3, 0, TabMoveDirection::Previous), Some(2));
        assert_eq!(tab_move_target(3, 2, TabMoveDirection::Next), Some(0));
    }

    #[test]
    fn workspace_layout_round_trip_replaces_only_pane_identities() {
        let layout = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratio: 0.55,
            first: Box::new(LayoutNode::Leaf(PaneId(1))),
            second: Box::new(LayoutNode::Split {
                dir: SplitDir::Vertical,
                ratio: 0.35,
                first: Box::new(LayoutNode::Leaf(PaneId(2))),
                second: Box::new(LayoutNode::Leaf(PaneId(3))),
            }),
        };
        let definition = workspace_layout_definition(&layout);
        assert_eq!(definition.pane_count(), Some(3));

        let mut panes = [PaneId(10), PaneId(11), PaneId(12)].into_iter();
        let restored = workspace_layout_with_panes(&definition, &mut panes).unwrap();
        assert_eq!(panes.next(), None);
        let geometry = restored.compute(Rect::new(0, 0, 101, 101));
        assert_eq!(geometry.rect_of(PaneId(10)).unwrap().w, 55);
        assert_eq!(geometry.rect_of(PaneId(11)).unwrap().h, 35);
        assert_eq!(geometry.rect_of(PaneId(12)).unwrap().h, 65);
        assert!(!restored.contains_pane(PaneId(1)));
    }
}
