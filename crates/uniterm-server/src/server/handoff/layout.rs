//! Live hierarchy and viewport ownership. A prepared engine restores the
//! geometry already on the screen without resizing a running foreground job.

use super::super::{Layout, LayoutNode, PaneId, Project, ProjectId, Rect, Server, Win};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io;

/// A private live layout retains identity allocators, Tab ancestry and the exact
/// viewport shown to attached clients through an engine upgrade.
#[derive(Serialize, Deserialize)]
pub struct WorkspaceLayout {
    projects: Vec<Project>,
    windows: Vec<Win>,
    active_project: ProjectId,
    active_window: usize,
    last_active_pane: Option<PaneId>,
    next_project_id: u64,
    next_pane_id: u64,
    canvas: (u16, u16),
    geometry: Layout,
    project_scroll: usize,
    tab_scroll: usize,
    stack_scroll: usize,
    follow_active: bool,
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "live Workspace layout does not match transferred Panes",
    )
}

impl WorkspaceLayout {
    /// Take a private checkpoint at the upgrade boundary. IDs already assigned
    /// to closed Panes remain reserved for the lifetime of this Workspace.
    pub fn capture(engine: &Server) -> Self {
        Self {
            projects: engine.projects.clone(),
            windows: engine.windows.clone(),
            active_project: engine.active_project,
            active_window: engine.active_window,
            last_active_pane: engine.last_active_pane,
            next_project_id: engine.next_project_id,
            next_pane_id: engine.next_pane_id,
            canvas: (engine.cols, engine.rows),
            geometry: engine.current_layout.clone(),
            project_scroll: engine.project_scroll,
            tab_scroll: engine.tab_scroll,
            stack_scroll: engine.stack_scroll,
            follow_active: engine.tab_scroll_follow_active,
        }
    }

    /// Validate all ownership and geometry before installing any of it. A
    /// candidate may decline while the old owner retains the entire Workspace.
    pub fn install(self, engine: &mut Server) -> io::Result<()> {
        if !engine.agents.awaiting_handoff() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "layout import needs a prepared engine",
            ));
        }
        self.validate(engine.panes.keys().copied())?;
        engine.projects = self.projects;
        engine.windows = self.windows;
        engine.active_project = self.active_project;
        engine.active_window = self.active_window;
        engine.last_active_pane = self.last_active_pane;
        engine.next_project_id = self.next_project_id;
        engine.next_pane_id = self.next_pane_id;
        (engine.cols, engine.rows) = self.canvas;
        engine.current_layout = self.geometry;
        engine.tab_scroll = self.tab_scroll;
        engine.stack_scroll = self.stack_scroll;
        engine.project_scroll = self.project_scroll;
        engine.tab_scroll_follow_active = self.follow_active;
        Ok(())
    }

    pub(super) fn validate(&self, panes: impl Iterator<Item = PaneId>) -> io::Result<()> {
        let expected: HashSet<_> = panes.collect();
        let mut owners = HashMap::with_capacity(expected.len());
        let mut projects = HashSet::new();
        if self.next_pane_id == u64::MAX
            || self.next_project_id == u64::MAX
            || self.canvas.0 == 0
            || self.canvas.1 == 0
            || self
                .windows
                .get(self.active_window)
                .is_none_or(|w| w.project != self.active_project)
        {
            return Err(invalid());
        }
        for project in &self.projects {
            if project.id.0 == 0
                || project.id.0 >= self.next_project_id
                || !projects.insert(project.id)
            {
                return Err(invalid());
            }
        }
        for (index, window) in self.windows.iter().enumerate() {
            if !projects.contains(&window.project) {
                return Err(invalid());
            }
            let mut pending = vec![&window.layout];
            while let Some(node) = pending.pop() {
                match node {
                    LayoutNode::Leaf(id) => {
                        if !expected.contains(id)
                            || id.0 == 0
                            || id.0 >= self.next_pane_id
                            || owners.insert(*id, index).is_some()
                        {
                            return Err(invalid());
                        }
                    }
                    LayoutNode::Split {
                        ratio,
                        first,
                        second,
                        ..
                    } => {
                        if !(*ratio > 0.0 && *ratio < 1.0) {
                            return Err(invalid());
                        }
                        pending.extend([first.as_ref(), second.as_ref()]);
                    }
                }
            }
            if owners.get(&window.active) != Some(&index)
                || window
                    .zoomed
                    .is_some_and(|id| owners.get(&id) != Some(&index))
            {
                return Err(invalid());
            }
        }
        if owners.len() != expected.len() {
            return Err(invalid());
        }
        for project in &self.projects {
            if project.active_pane.is_some_and(|id| {
                owners
                    .get(&id)
                    .is_none_or(|i| self.windows[*i].project != project.id)
            }) {
                return Err(invalid());
            }
        }
        // A Tab has at most one parent. Mark complete paths so even a deep
        // stack validates in linear time.
        let mut done = vec![false; self.windows.len()];
        let mut visiting = vec![false; self.windows.len()];
        for start in 0..self.windows.len() {
            let mut current = start;
            let mut path = Vec::new();
            while !done[current] {
                if visiting[current] {
                    return Err(invalid());
                }
                visiting[current] = true;
                path.push(current);
                let window = &self.windows[current];
                let Some(parent) = window.stack_parent else {
                    break;
                };
                current = *owners.get(&parent).ok_or_else(invalid)?;
                if self.windows[current].project != window.project {
                    return Err(invalid());
                }
            }
            for i in path {
                done[i] = true;
            }
        }
        let fits = |r: Rect| {
            r.x.checked_add(r.w).is_some_and(|x| x <= self.canvas.0)
                && r.y.checked_add(r.h).is_some_and(|y| y <= self.canvas.1)
        };
        let mut visible = HashSet::new();
        if self.geometry.panes.iter().any(|(id, r)| {
            owners.get(id) != Some(&self.active_window) || !visible.insert(*id) || !fits(*r)
        }) || self.geometry.dividers.iter().any(|d| !fits(d.rect))
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{reaper::common, SplitDir};

    #[test]
    fn split_stacked_and_background_tabs_retain_selection_and_reserved_ids() {
        common::isolate_state();
        let root = common::temp_dir("live-layout");
        let path = root.join(format!("{}.sock", common::unique_workspace_name()));
        let (mut engine, mut poll) = Server::bind(&path, "/bin/cat", &[], 80, 24).unwrap();
        let original = engine.windows[0].active;
        let second = engine.spawn_pane(poll.registry(), &[]).unwrap();
        assert!(engine.windows[0]
            .layout
            .split(original, SplitDir::Horizontal, second));
        engine.windows[0].active = second;
        let third = engine
            .create_tab(poll.registry(), engine.active_project, true)
            .unwrap();
        assert!(engine.assign_tab_parent(third, Some(second)));
        engine.windows[0].zoomed = Some(second);
        engine.next_pane_id += 2;
        engine.relayout();

        let fresh = || {
            let encoded = serde_json::to_vec(&WorkspaceLayout::capture(&engine)).unwrap();
            serde_json::from_slice::<WorkspaceLayout>(&encoded).unwrap()
        };
        let saved = fresh();
        assert_eq!(saved.next_pane_id, engine.next_pane_id);
        assert_eq!(saved.windows[0].active, second);
        assert_eq!(saved.windows[1].stack_parent, Some(second));
        assert!(saved
            .validate([original, second, third].into_iter())
            .is_ok());
        assert!(saved.validate([original, second].into_iter()).is_err());
        let mut invalid = fresh();
        invalid.windows[0].stack_parent = Some(third);
        assert!(invalid.validate(engine.panes.keys().copied()).is_err());
        let mut invalid = fresh();
        invalid.next_pane_id = third.0;
        assert!(invalid.validate(engine.panes.keys().copied()).is_err());
        let mut invalid = fresh();
        invalid.geometry.panes[0].1.x = u16::MAX;
        assert!(invalid.validate(engine.panes.keys().copied()).is_err());
        let mut invalid = fresh();
        invalid.windows[1].layout = LayoutNode::Leaf(original);
        assert!(invalid.validate(engine.panes.keys().copied()).is_err());
        engine.retire_all_ptys(&mut poll);
    }
}
