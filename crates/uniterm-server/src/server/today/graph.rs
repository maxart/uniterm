//! Native agents and orchestration evidence over authoritative projections.

mod native;
mod presentation;

use super::*;
use std::collections::{BTreeMap, HashSet};
use uniterm_core::{RoleId, RunId};
use uniterm_proto::{ArtifactEntry, RunEntry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Row {
    run: RunId,
    role: Option<RoleId>,
    depth: usize,
    missing_parent: bool,
}

/// The current graph is bounded by the durable projection's retention policy.
/// Rendering never walks provider transcripts or invents completion from idle.
pub(in crate::server) struct Graph {
    dirty_panes: HashSet<PaneId>,
    agents_view: bool,
    agent_nodes: Vec<uniterm_core::timeline::AgentGraphNode>,
    agent_selected: usize,
    agent_scroll: usize,
    agent_day: TimelineDay,
    agent_times: BTreeMap<u64, String>,
    agent_date: String,
    agent_before: Option<u64>,
    live_agents: native::LiveAgents,
    source_hits: Vec<(Rect, bool)>,
    sequence: u64,
    pane_version: (u64, usize),
    runs: BTreeMap<RunId, RunEntry>,
    artifacts: Vec<ArtifactEntry>,
    projects: BTreeMap<ProjectId, (String, String)>,
    evidence_counts: BTreeMap<RunId, usize>,
    context_dirty: bool,
    live: HashSet<PaneId>,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
    details: bool,
    detail_scroll: usize,
    hits: Vec<(Rect, usize)>,
    history: Option<(RunId, Vec<String>)>,
    history_pending: bool,
    history_generation: u64,
}

impl Graph {
    /// Cancel pending response ownership so restoring a cached inspector can reload.
    pub(super) fn suspend(&mut self) {
        self.history_pending = false;
        self.history_generation = 0;
    }

    fn new() -> Self {
        Self {
            dirty_panes: HashSet::new(),
            agents_view: true,
            agent_nodes: Vec::new(),
            agent_selected: 0,
            agent_scroll: 0,
            agent_day: TimelineDay::default(),
            agent_times: BTreeMap::new(),
            agent_date: String::new(),
            agent_before: None,
            live_agents: BTreeMap::new(),
            source_hits: Vec::new(),
            sequence: 0,
            pane_version: (0, 0),
            runs: BTreeMap::new(),
            artifacts: Vec::new(),
            projects: BTreeMap::new(),
            evidence_counts: BTreeMap::new(),
            context_dirty: false,
            live: HashSet::new(),
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            details: false,
            detail_scroll: 0,
            hits: Vec::new(),
            history: None,
            history_pending: false,
            history_generation: 0,
        }
    }

    fn update(
        &mut self,
        sequence: u64,
        runs: Vec<RunEntry>,
        artifacts: Vec<ArtifactEntry>,
        live: HashSet<PaneId>,
    ) {
        let selected = self.rows.get(self.selected).copied();
        self.sequence = sequence;
        self.runs = runs.into_iter().map(|run| (run.id, run)).collect();
        self.evidence_counts.clear();
        for artifact in &artifacts {
            *self
                .evidence_counts
                .entry(artifact.producer_run)
                .or_default() += 1;
        }
        self.artifacts = artifacts;
        self.live = live;
        self.rows.clear();
        let mut children: BTreeMap<Option<RunId>, Vec<RunId>> = BTreeMap::new();
        for run in self.runs.values() {
            let parent = run.parent.filter(|parent| self.runs.contains_key(parent));
            children.entry(parent).or_default().push(run.id);
        }
        let mut stack: Vec<_> = children
            .get(&None)
            .into_iter()
            .flatten()
            .rev()
            .map(|id| (*id, 0))
            .collect();
        let mut seen = HashSet::new();
        while let Some((id, depth)) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let run = &self.runs[&id];
            self.rows.push(Row {
                run: id,
                role: None,
                depth,
                missing_parent: run
                    .parent
                    .is_some_and(|parent| !self.runs.contains_key(&parent)),
            });
            self.rows.extend(run.roles.iter().map(|role| Row {
                run: id,
                role: Some(role.id),
                depth: depth + 1,
                missing_parent: false,
            }));
            stack.extend(
                children
                    .get(&Some(id))
                    .into_iter()
                    .flatten()
                    .rev()
                    .map(|id| (*id, depth + 1)),
            );
        }
        self.selected = selected
            .and_then(|row| {
                self.rows
                    .iter()
                    .position(|candidate| candidate.run == row.run && candidate.role == row.role)
            })
            .unwrap_or(self.selected)
            .min(self.rows.len().saturating_sub(1));
    }

    fn target(&self) -> Option<(RunId, RoleId, PaneId)> {
        let row = self.rows.get(self.selected)?;
        let run = self.runs.get(&row.run)?;
        if run.status.terminal() {
            return None;
        }
        let role = if let Some(id) = row.role {
            run.roles.iter().find(|role| role.id == id)?
        } else {
            run.roles.iter().find(|role| {
                role.activation
                    .as_ref()
                    .is_some_and(|activation| activation.active)
            })?
        };
        self.live
            .contains(&role.pane)
            .then_some((run.id, role.id, role.pane))
    }

    fn detail(&self) -> String {
        let Some(row) = self.rows.get(self.selected) else {
            return "No retained runs. Start a workflow or relay to see its graph.".into();
        };
        let run = &self.runs[&row.run];
        let mut text = format!("Run #{} | {} | {}\n{}\nProject #{} | Task #{}\nParent: {}\nOutcome: {}\n\nRoles in orchestration order:\n", run.id.0, run.kind.label(), run.status.label(), run.title, run.project.0, run.task_id, run.parent.map_or_else(|| "root".into(), |id| format!("Run #{}", id.0)), run.outcome.as_deref().unwrap_or("No terminal verdict recorded"));
        text.push_str(&format!("{}\n", self.project_label(run)));
        for role in &run.roles {
            let activation = role.activation.as_ref().map_or_else(
                || "not activated".into(),
                |a| {
                    format!(
                        "latest activation #{} ({})",
                        a.id,
                        if a.active && !run.status.terminal() {
                            "current"
                        } else {
                            "handed off"
                        }
                    )
                },
            );
            text.push_str(&format!(
                "  {} -> {} | Pane #{} | {}{}\n",
                role.name,
                role.provider,
                role.pane.0,
                activation,
                if self.live.contains(&role.pane) {
                    ""
                } else {
                    " | unavailable"
                }
            ));
        }
        text.push_str("\nEvidence:\n");
        let mut count = 0;
        for artifact in self
            .artifacts
            .iter()
            .filter(|artifact| artifact.producer_run == run.id)
        {
            count += 1;
            text.push_str(&format!(
                "  Artifact #{} | {:?} | {:?}\n  {} | {} bytes\n  SHA-256 {}\n",
                artifact.id.0,
                artifact.kind,
                artifact.status,
                artifact.path,
                artifact.size,
                artifact.digest
            ));
        }
        if count == 0 {
            text.push_str("  No artifact evidence recorded\n");
        }
        text.push_str("\nAttempts / review gates:\n");
        if let Some((id, history)) = self.history.as_ref().filter(|(id, _)| *id == run.id) {
            let _ = id;
            for line in history {
                text.push_str(line);
                text.push('\n');
            }
        } else {
            text.push_str(if self.history_pending {
                "Loading authoritative history..."
            } else {
                "Press r to load authoritative history."
            });
        }
        text.push_str("\nA role handoff is not an approval; submitted verdicts and the terminal outcome record the completion contract.");
        text
    }

    fn project_event(&mut self, event: &crate::eventlog::LogEvent) {
        use crate::eventlog::LogEvent;
        let changed = match event {
            LogEvent::ProjectCreated {
                project,
                name,
                root,
            } => {
                if self
                    .projects
                    .get(&ProjectId(*project))
                    .is_some_and(|(old_name, old_root)| old_name == name && old_root == root)
                {
                    false
                } else {
                    self.projects
                        .insert(ProjectId(*project), (name.clone(), root.clone()));
                    true
                }
            }
            LogEvent::ProjectRenamed { project, name } => {
                let current = &mut self.projects.entry(ProjectId(*project)).or_default().0;
                if current == name {
                    false
                } else {
                    current.clone_from(name);
                    true
                }
            }
            LogEvent::ProjectDirectoryChanged { project, root } => {
                let current = &mut self.projects.entry(ProjectId(*project)).or_default().1;
                if current == root {
                    false
                } else {
                    current.clone_from(root);
                    true
                }
            }
            LogEvent::ProjectRemoved { project } => {
                self.projects.remove(&ProjectId(*project)).is_some()
            }
            _ => false,
        };
        self.context_dirty |= changed;
    }

    fn needs_history(&self) -> bool {
        !self.agents_view
            && self.details
            && !self.history_pending
            && self
                .rows
                .get(self.selected)
                .is_some_and(|row| self.history.as_ref().is_none_or(|(id, _)| *id != row.run))
    }

    fn project_label(&self, run: &RunEntry) -> String {
        self.projects.get(&run.project).map_or_else(
            || format!("Project #{} · path not retained", run.project.0),
            |(name, path)| format!("{name} · {path}"),
        )
    }

    // The same topology supplies rail geometry and row targets at every width.
    fn rail(&self, index: usize, max_depth: usize) -> String {
        native::ancestry_rail(
            self.rows[index].depth,
            self.rows[index + 1..].iter().map(|row| row.depth),
            max_depth,
        )
    }

    /// Draw inside Today's shared frame while consuming only event-backed dirt.
    fn compose_runs(&mut self, grid: &mut Grid, area: Rect, theme: &uniterm_core::Theme) {
        self.hits.clear();
        self.context_dirty = false;
        if area.w < 4 || area.h < 3 {
            return;
        }
        if self.details {
            panel(
                grid,
                area,
                " Attempts / reviews / evidence · v back ",
                theme.border,
            );
            let lines = detail_lines(&self.detail(), area.w.saturating_sub(4).max(1) as usize);
            let visible = area.h.saturating_sub(2) as usize;
            self.detail_scroll = self.detail_scroll.min(lines.len().saturating_sub(visible));
            for (offset, line) in lines
                .iter()
                .skip(self.detail_scroll)
                .take(visible)
                .enumerate()
            {
                row_text(
                    grid,
                    Rect::new(area.x + 2, area.y + 1 + offset as u16, area.w - 4, 1),
                    line,
                    theme.foreground,
                    Color::Default,
                );
            }
            return;
        }
        let (trace, dock) = presentation::regions(area);
        row_text(
            grid,
            Rect::new(trace.x, trace.y, trace.w, 1),
            &format!(
                "RUN GRAPH · {} runs · {} evidence items",
                self.runs.len(),
                self.artifacts.len()
            ),
            theme.muted,
            Color::Default,
        );
        let visible = usize::from(trace.h.saturating_sub(1));
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }
        if visible > 0 && self.selected >= self.scroll + visible {
            self.scroll = self.selected + 1 - visible;
        }
        if self.rows.is_empty() && visible > 0 {
            row_text(
                grid,
                Rect::new(trace.x, trace.y + 1, trace.w, 1),
                "No runs yet. Start a workflow or relay.",
                theme.muted,
                Color::Default,
            );
        }
        let depths: Vec<_> = self.rows.iter().map(|row| row.depth).collect();
        let path = presentation::selected_path(&depths, self.selected);
        for (offset, row) in self.rows.iter().skip(self.scroll).take(visible).enumerate() {
            let index = self.scroll + offset;
            let run = &self.runs[&row.run];
            let rect = Rect::new(trace.x, trace.y + 1 + offset as u16, trace.w, 1);
            let rail = self.rail(index, usize::from(rect.w.saturating_sub(36) / 2).max(1));
            let (glyph, color) = presentation::run_state(run.status, theme);
            let count = self
                .evidence_counts
                .get(&run.id)
                .copied()
                .unwrap_or_default();
            let evidence = if count > 0 && row.role.is_none() {
                format!("{count} art")
            } else {
                String::new()
            };
            let role = row
                .role
                .and_then(|id| run.roles.iter().find(|role| role.id == id));
            let identity = if role.is_some() {
                String::new()
            } else {
                format!("#{}", run.id.0)
            };
            let (label, context, status, glyph, color) = if let Some(role) = role {
                let active =
                    !run.status.terminal() && role.activation.as_ref().is_some_and(|a| a.active);
                let status = if !self.live.contains(&role.pane) && active {
                    "unavailable"
                } else if active {
                    "current"
                } else if role.activation.is_some() {
                    "handed off"
                } else {
                    "not started"
                };
                (
                    role.name.as_str(),
                    role.provider.as_str(),
                    status,
                    if active { "◎" } else { "○" },
                    if active { theme.accent } else { theme.muted },
                )
            } else {
                (
                    run.title.as_str(),
                    run.kind.label(),
                    if row.missing_parent {
                        "? parent"
                    } else {
                        run.status.label()
                    },
                    glyph,
                    color,
                )
            };
            presentation::trace_row(
                grid,
                rect,
                presentation::TraceRow {
                    rail: &rail,
                    depth: row.depth,
                    index,
                    path: &path,
                    selected: index == self.selected,
                    glyph,
                    identity: &identity,
                    label,
                    context,
                    status,
                    evidence: &evidence,
                    color,
                },
                theme,
            );
            self.hits.push((rect, index));
        }
        if let Some(row) = self.rows.get(self.selected) {
            let run = &self.runs[&row.run];
            let role = row
                .role
                .and_then(|id| run.roles.iter().find(|role| role.id == id));
            let (_, color) = presentation::run_state(run.status, theme);
            let identity = role.map_or_else(
                || format!("Run #{}", run.id.0),
                |role| format!("{} · Run #{}", role.name, run.id.0),
            );
            let facts = role.map_or_else(
                || {
                    format!(
                        "{} · Task #{} · {} roles · parent {}",
                        run.kind.label(),
                        run.task_id,
                        run.roles.len(),
                        run.parent
                            .map_or_else(|| "root".into(), |id| format!("#{}", id.0))
                    )
                },
                |role| {
                    format!(
                        "{} · Pane #{} · {}{}",
                        role.provider,
                        role.pane.0,
                        role.activation.as_ref().map_or_else(
                            || "not activated".into(),
                            |a| format!(
                                "activation #{} · {}",
                                a.id,
                                if a.active && !run.status.terminal() {
                                    "current"
                                } else {
                                    "handed off"
                                }
                            )
                        ),
                        if self.live.contains(&role.pane) && !run.status.terminal() {
                            ""
                        } else {
                            " · unavailable"
                        }
                    )
                },
            );
            let evidence = self
                .artifacts
                .iter()
                .find(|item| item.producer_run == run.id)
                .map_or_else(
                    || "No artifact evidence recorded".into(),
                    |item| format!("Artifact: {} · {}", item.path, item.status.label()),
                );
            let review = self
                .history
                .as_ref()
                .filter(|(id, _)| *id == run.id)
                .and_then(|(_, lines)| lines.last())
                .map_or("v attempts / reviews", String::as_str);
            presentation::inspector(
                grid,
                dock,
                presentation::Inspector {
                    identity: &identity,
                    status: run.status.label(),
                    lines: &[
                        run.title.clone(),
                        self.project_label(run),
                        format!(
                            "Outcome: {}",
                            run.outcome
                                .as_deref()
                                .unwrap_or("No terminal verdict recorded")
                        ),
                        format!("{evidence} · {review}"),
                    ],
                    footer: &facts,
                    color,
                },
                theme,
            );
        }
    }

    #[cfg(test)]
    fn compose(&mut self, grid: &mut Grid, theme: &uniterm_core::Theme, _message: &str) {
        let mut next = Grid::new(grid.width(), grid.height());
        self.compose_content(
            &mut next,
            Rect::new(0, 0, grid.width(), grid.height()),
            theme,
        );
        apply_grid(grid, &next);
    }
}

impl Today {
    /// Invalidate observed work labels only on a title/metadata event.
    /// Cached Graph state receives the same invalidation without idle work.
    pub(in crate::server) fn graph_agent_changed(&mut self, pane: PaneId) {
        if let Some(graph) = self.graph.as_mut() {
            graph.dirty_panes.insert(pane);
        }
        if let Some(graph) = self.cached_graph.as_mut() {
            // Reopening takes a fresh snapshot; retain no stale live target or
            // growing set of Pane IDs while the cached surface is hidden.
            graph.live_agents.remove(&pane);
        }
    }

    /// Project committed metadata changes without scanning Projects on idle flushes.
    pub(in crate::server) fn project_graph_event(&mut self, event: &crate::eventlog::LogEvent) {
        for graph in [&mut self.graph, &mut self.cached_graph]
            .into_iter()
            .flatten()
        {
            graph.project_event(event);
        }
    }

    fn accept_graph_history(
        &mut self,
        run: RunId,
        generation: u64,
        result: Result<Vec<String>, String>,
    ) -> bool {
        let Some(graph) = self
            .graph
            .as_mut()
            .filter(|graph| graph.history_generation == generation && graph.history_pending)
        else {
            return false;
        };
        graph.history_pending = false;
        graph.history = Some((
            run,
            result.unwrap_or_else(|error| {
                vec![format!("History unavailable: {error}. Press r to retry.")]
            }),
        ));
        true
    }
}

impl Server {
    pub(in crate::server) fn open_run_graph(&mut self, reg: &Registry) {
        if !self.config.run_graph {
            self.show_guardrail_toast(reg, "Graph is off. Enable it in Settings.");
            return;
        }
        let focus_changed = self.focus_terminal_input();
        if self.today.as_ref().is_some_and(|view| view.graph.is_some()) {
            self.flush_run_graph(reg);
            if focus_changed {
                self.repaint_chrome_all(reg);
            }
            return;
        }
        let runs = self.run_snapshot(None, false);
        let artifacts = self.artifact_snapshot(None, None, true);
        let live = self.panes.keys().copied().collect();
        let live_agents = self.graph_live_agents();
        if self.today.is_none() {
            self.today = Some(Today::new(self.chrome_area().0));
        }
        let view = self.today.as_mut().unwrap();
        let mut graph = view
            .graph
            .take()
            .or_else(|| view.cached_graph.take())
            .unwrap_or_else(Graph::new);
        graph.pane_version = (self.next_pane_id, self.panes.len());
        graph.live_agents = live_agents;
        graph.dirty_panes.clear();
        graph.projects = self
            .projects
            .iter()
            .map(|p| (p.id, (p.name.clone(), p.root.clone())))
            .collect();
        graph.update(
            self.run_graph_sequence.max(self.artifact_sequence),
            runs,
            artifacts,
            live,
        );
        view.graph = Some(graph);
        view.graph_enabled = true;
        view.mode = TodayMode::Graph;
        view.expanded = false;
        view.input = None;
        view.animation_due = None;
        view.pulse = 0;
        view.message.clear();
        // In-flight day results must not replace Graph's message or selection.
        self.today_generation = self.today_generation.saturating_add(1);
        view.generation = self.today_generation;
        view.pending = false;
        self.agents
            .send(CoreToAgent::TimelineWatch { enabled: true });
        self.request_today(None);
        self.overview = None;
        self.tab_drag = None;
        self.request_graph_history();
        self.full_repaint_all(reg);
    }

    pub(in crate::server) fn flush_run_graph(&mut self, reg: &Registry) {
        let Some(graph) = self.today.as_ref().and_then(|view| view.graph.as_ref()) else {
            return;
        };
        let refresh_live = !graph.dirty_panes.is_empty();
        let projection_changed = graph.sequence
            != self.run_graph_sequence.max(self.artifact_sequence)
            || graph.pane_version != (self.next_pane_id, self.panes.len());
        let context_changed = graph.context_dirty;
        let mut live_changed = false;
        if refresh_live {
            let mut panes = self
                .today
                .as_mut()
                .and_then(|view| view.graph.as_mut())
                .map(|graph| std::mem::take(&mut graph.dirty_panes))
                .unwrap_or_default();
            for pane in panes.drain() {
                let live = self.graph_live_agent(pane);
                if let Some(graph) = self.today.as_mut().and_then(|view| view.graph.as_mut()) {
                    if graph.live_agents.get(&pane) != live.as_ref() {
                        if let Some(live) = live {
                            graph.live_agents.insert(pane, live);
                        } else {
                            graph.live_agents.remove(&pane);
                        }
                        live_changed = true;
                    }
                }
            }
            if let Some(graph) = self.today.as_mut().and_then(|view| view.graph.as_mut()) {
                graph.dirty_panes = panes;
            }
        }
        if !projection_changed {
            if context_changed || live_changed {
                self.paint_today(reg);
            }
            return;
        }
        let runs = self.run_snapshot(None, false);
        let artifacts = self.artifact_snapshot(None, None, true);
        let live = self.panes.keys().copied().collect();
        if let Some(graph) = self.today.as_mut().and_then(|view| view.graph.as_mut()) {
            graph.pane_version = (self.next_pane_id, self.panes.len());
            graph.projects = self
                .projects
                .iter()
                .map(|p| (p.id, (p.name.clone(), p.root.clone())))
                .collect();
            graph.update(
                self.run_graph_sequence.max(self.artifact_sequence),
                runs,
                artifacts,
                live,
            );
        }
        self.paint_today(reg);
    }

    pub(super) fn graph_input(&mut self, reg: &Registry, bytes: &[u8]) {
        let Some(view) = self.today.as_mut() else {
            return;
        };
        let Some(graph) = view.graph.as_mut() else {
            return;
        };
        if bytes == b"n" {
            graph.agents_view = !graph.agents_view;
            graph.details = false;
            view.message.clear();
            self.paint_today(reg);
            return;
        }
        if graph.agents_view {
            self.native_graph_input(reg, bytes);
            return;
        }
        match bytes {
            b"\x1b" | b"q" if !graph.details => {
                self.close_today();
                self.full_repaint_all(reg);
                return;
            }
            b"\x1b" | b"q" | b"v" if graph.details => graph.details = false,
            b"v" => {
                graph.details = true;
                graph.detail_scroll = 0;
            }
            b"r" if graph.details => graph.history = None,
            b"j" | b"\x1b[B" if graph.details => {
                graph.detail_scroll = graph.detail_scroll.saturating_add(1)
            }
            b"k" | b"\x1b[A" if graph.details => {
                graph.detail_scroll = graph.detail_scroll.saturating_sub(1)
            }
            b"j" | b"\x1b[B" => {
                graph.selected = (graph.selected + 1).min(graph.rows.len().saturating_sub(1))
            }
            b"k" | b"\x1b[A" => graph.selected = graph.selected.saturating_sub(1),
            b"\r" | b"g" => {
                let target = graph.target();
                if let Some((run, role, pane)) = target.filter(|(run, role, pane)| {
                    self.run_graph.active_for_pane(*pane) == Some((*run, *role))
                }) {
                    let _ = (run, role);
                    self.close_today();
                    self.focus_pane_target(reg, pane);
                    return;
                }
                view.message =
                    "No matching live role. Historical evidence remains available with v.".into();
            }
            _ => {}
        }
        self.request_graph_history();
        self.paint_today(reg);
    }

    fn request_graph_history(&mut self) {
        let Some(graph) = self.today.as_mut().and_then(|view| view.graph.as_mut()) else {
            return;
        };
        if !graph.needs_history() {
            return;
        }
        let Some(row) = graph.rows.get(graph.selected) else {
            return;
        };
        let run = &graph.runs[&row.run];
        self.today_generation = self.today_generation.saturating_add(1);
        graph.history_generation = self.today_generation;
        graph.history_pending = true;
        self.agents.send(CoreToAgent::RunGraphHistory {
            name: self.name.clone(),
            run: run.id,
            task_id: run.task_id,
            through: self.log.current_sequence(),
            generation: graph.history_generation,
        });
    }

    pub(in crate::server) fn graph_history_loaded(
        &mut self,
        reg: &Registry,
        run: RunId,
        generation: u64,
        result: Result<Vec<String>, String>,
    ) {
        if !self
            .today
            .as_mut()
            .is_some_and(|view| view.accept_graph_history(run, generation, result))
        {
            return;
        }
        self.request_graph_history();
        self.paint_today(reg);
    }

    pub(super) fn graph_mouse(&mut self, reg: &Registry, cx: u16, cy: u16, kind: MouseKind) {
        let area = self.chrome_area().0;
        if matches!(kind, MouseKind::WheelUp | MouseKind::WheelDown) {
            self.graph_input(
                reg,
                if kind == MouseKind::WheelUp {
                    b"k"
                } else {
                    b"j"
                },
            );
            return;
        }
        if kind != MouseKind::Click {
            return;
        }
        if let Some(graph) = self.today.as_mut().and_then(|view| view.graph.as_mut()) {
            if let Some((_, agents)) = graph.source_hits.iter().find(|(rect, _)| {
                rect.contains(cx.saturating_sub(area.x), cy.saturating_sub(area.y))
            }) {
                if graph.agents_view != *agents {
                    self.graph_input(reg, b"n");
                }
                return;
            }
            if let Some((_, index)) = graph.hits.iter().find(|(rect, _)| {
                rect.contains(cx.saturating_sub(area.x), cy.saturating_sub(area.y))
            }) {
                if graph.agents_view {
                    graph.agent_selected = *index;
                } else {
                    graph.selected = *index;
                }
            }
        }
        self.paint_today(reg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniterm_core::{ProjectId, RunActivation, RunKind, RunStatus};
    use uniterm_proto::RunRoleEntry;

    fn run(id: u64, parent: Option<u64>) -> RunEntry {
        RunEntry {
            id: RunId(id),
            parent: parent.map(RunId),
            children: vec![],
            project: ProjectId(1),
            kind: RunKind::Workflow,
            task_id: id,
            title: format!("修復 task {id}"),
            status: RunStatus::Active,
            outcome: None,
            panes: vec![PaneId(id)],
            roles: vec![RunRoleEntry {
                id: RoleId(id),
                name: "reviewer".into(),
                pane: PaneId(id),
                provider: "provider-neutral".into(),
                activation: Some(RunActivation {
                    id,
                    pane: PaneId(id),
                    provider: "provider-neutral".into(),
                    active: true,
                }),
            }],
        }
    }

    #[test]
    fn cached_graph_rejects_stale_history_and_preserves_day_cursor() {
        let mut graph = Graph::new();
        graph.agents_view = false;
        graph.update(1, vec![run(1, None)], vec![], HashSet::new());
        graph.details = true;
        graph.history_pending = true;
        graph.history_generation = 7;
        let mut view = Today::new(Rect::new(0, 0, 100, 30));
        view.date = "2026-10-31".into();
        view.before = Some(123);
        view.scroll = 4;
        view.mode = TodayMode::Graph;
        view.graph = Some(graph);
        assert!(view.select_day_mode(TodayMode::Activity));
        assert_eq!(view.date, "2026-10-31");
        assert_eq!(view.before, Some(123));
        assert_eq!(view.scroll, 4);
        assert!(!view.accept_graph_history(RunId(1), 7, Ok(vec!["stale history".into()])));
        view.graph = view.cached_graph.take();
        let graph = view.graph.as_mut().unwrap();
        assert!(
            graph.needs_history(),
            "restoring details must issue a fresh request"
        );
        graph.history_pending = true;
        graph.history_generation = 8;
        assert!(!view.accept_graph_history(RunId(1), 7, Ok(vec!["late reply".into()])));
        assert!(view.accept_graph_history(RunId(1), 8, Ok(vec!["fresh history".into()])));
        assert!(!view.graph.as_ref().unwrap().needs_history());
        assert_eq!(
            view.graph.as_ref().unwrap().history.as_ref().unwrap().1,
            vec!["fresh history"]
        );
    }

    #[test]
    fn project_events_update_active_and_cached_graph_context() {
        use crate::eventlog::LogEvent;
        let mut graph = Graph::new();
        graph.agents_view = false;
        graph.update(1, vec![run(1, None)], vec![], HashSet::new());
        let mut view = Today::new(Rect::new(0, 0, 100, 30));
        view.graph = Some(graph);
        view.project_graph_event(&LogEvent::ProjectCreated {
            project: 1,
            name: "Original".into(),
            root: "/old".into(),
        });
        view.project_graph_event(&LogEvent::ProjectRenamed {
            project: 1,
            name: "Renamed".into(),
        });
        assert_eq!(
            view.graph.as_ref().unwrap().project_label(&run(1, None)),
            "Renamed · /old"
        );
        view.mode = TodayMode::Graph;
        view.compose(&uniterm_core::Theme::dark());
        view.grid.clear_damage();
        assert!(!view.graph.as_ref().unwrap().context_dirty);
        view.project_graph_event(&LogEvent::ProjectDirectoryChanged {
            project: 1,
            root: "/visible".into(),
        });
        assert!(view.graph.as_ref().unwrap().context_dirty);
        view.compose(&uniterm_core::Theme::dark());
        assert!(fixture_text(&view.grid).contains("/visible"));
        assert!(!view.graph.as_ref().unwrap().context_dirty);
        view.grid.clear_damage();
        view.project_graph_event(&LogEvent::ProjectDirectoryChanged {
            project: 1,
            root: "/visible".into(),
        });
        assert!(
            !view.graph.as_ref().unwrap().context_dirty,
            "identical directory must not arm a repaint"
        );
        view.compose(&uniterm_core::Theme::dark());
        assert!(!view.grid.is_dirty());
        view.select_day_mode(TodayMode::Activity);
        view.project_graph_event(&LogEvent::ProjectDirectoryChanged {
            project: 1,
            root: "/new".into(),
        });
        assert_eq!(
            view.cached_graph
                .as_ref()
                .unwrap()
                .project_label(&run(1, None)),
            "Renamed · /new"
        );
        view.project_graph_event(&LogEvent::ProjectRemoved { project: 1 });
        assert!(view.cached_graph.as_ref().unwrap().projects.is_empty());
    }

    #[test]
    fn integrated_graph_fixtures_show_relationships_attempts_reviews_and_evidence() {
        let mut graph = Graph::new();
        graph.agents_view = false;
        let mut root = run(1, None);
        root.title = "Ship request validation".into();
        root.roles[0].name = "builder".into();
        root.roles[0].provider = "codex".into();
        root.roles.push(RunRoleEntry {
            id: RoleId(10),
            name: "reviewer".into(),
            pane: PaneId(10),
            provider: "claude".into(),
            activation: None,
        });
        let mut child = run(2, Some(1));
        child.title = "Validate error responses".into();
        child.roles[0].provider = "opencode".into();
        let mut done = run(3, None);
        done.title = "Document the response contract".into();
        done.status = RunStatus::Completed;
        done.outcome = Some("pass: contract reviewed".into());
        graph.update(
            7,
            vec![root, child, done],
            vec![ArtifactEntry {
                id: uniterm_core::ArtifactId(1),
                project: ProjectId(1),
                producer_run: RunId(1),
                producer_role: RoleId(1),
                kind: uniterm_core::ArtifactKind::TestEvidence,
                path: "evidence/validation-tests.txt".into(),
                digest: "a".repeat(64),
                size: 4096,
                status: uniterm_core::ArtifactStatus::Available,
                supersedes: None,
            }],
            HashSet::from([PaneId(1), PaneId(2)]),
        );
        graph.projects.insert(
            ProjectId(1),
            ("Orchard API".into(), "/work/orchard/api".into()),
        );
        graph.history = Some((
            RunId(1),
            vec![
                "Event #3 | Attempt #1: builder activated".into(),
                "Event #4 | Submission done | verdict: fix | add edge-case coverage".into(),
                "Event #5 | Attempt #2: edge-case coverage -> reviewer".into(),
            ],
        ));
        assert_eq!(graph.rail(1, 10), "├─");
        assert_eq!(graph.rail(3, 10), "└─");
        let mut view = Today::new(Rect::new(0, 0, 140, 44));
        view.graph = Some(graph);
        view.mode = TodayMode::Graph;
        view.graph_enabled = true;
        view.message.clear();
        for (width, height) in [(140, 40), (80, 24), (52, 48), (40, 20), (20, 12), (1, 1)] {
            view.grid = Grid::new(width, height);
            view.compose(&uniterm_core::Theme::dark());
            assert!(view
                .hits
                .iter()
                .all(|(rect, _)| rect.right() <= width && rect.bottom() <= height));
            assert!(view
                .graph
                .as_ref()
                .unwrap()
                .hits
                .iter()
                .all(|(rect, _)| rect.right() <= width && rect.bottom() <= height));
            let text = fixture_text(&view.grid);
            if width >= 90 {
                assert!(text.contains("TODAY"));
                assert!(text.contains("3 Graph"));
                assert!(text.contains("Orchard API"));
                assert!(text.contains("/work/orchard/api"));
                assert!(text.contains("Task #1"));
                assert!(text.contains("No terminal verdict recorded"));
            }
            if width >= 140 {
                assert!(text.contains("validation-tests.txt"));
                assert!(
                    text.contains("edge-case coverage"),
                    "compact dock retains recorded review history"
                );
            }
            export_fixture(
                &format!("today-graph-{width}x{height}"),
                &view.grid,
                &uniterm_core::Theme::dark(),
            );
            view.grid.clear_damage();
            view.compose(&uniterm_core::Theme::dark());
            assert!(!view.grid.is_dirty());
        }
        view.grid = Grid::new(120, 40);
        view.graph.as_mut().unwrap().details = true;
        view.compose(&uniterm_core::Theme::dark());
        let text = fixture_text(&view.grid);
        assert!(text.contains("Attempt #1"));
        assert!(text.contains("verdict: fix"));
        assert!(text.contains("validation-tests.txt"));
        export_fixture(
            "today-graph-evidence-120x40",
            &view.grid,
            &uniterm_core::Theme::dark(),
        );
        // Retained external labels may not inject terminal control strings.
        view.graph
            .as_mut()
            .unwrap()
            .runs
            .get_mut(&RunId(1))
            .unwrap()
            .title = "title\x1b]52;c;secret\x07日本語".into();
        view.compose(&uniterm_core::Theme::light());
        assert!(!fixture_text(&view.grid).contains('\x1b'));
        assert!(!fixture_text(&view.grid).contains('\x07'));
    }

    #[test]
    fn dense_run_trace_exports_recorded_outcomes_roles_and_review_evidence() {
        let mut graph = Graph::new();
        graph.agents_view = false;
        let runs = (1..=36)
            .map(|id| {
                let mut item = run(id, (id > 1).then_some((id + 1) / 3));
                item.title = format!(
                    "{} {id}",
                    match id % 5 {
                        0 => "Check Unicode input",
                        1 => "Validate request handling",
                        2 => "Review error paths",
                        3 => "Document response contract",
                        _ => "Test recovery boundaries",
                    }
                );
                item.status = match id % 5 {
                    0 => RunStatus::Active,
                    1 => RunStatus::Completed,
                    2 => RunStatus::Failed,
                    3 => RunStatus::Created,
                    _ => RunStatus::Canceled,
                };
                item.outcome = match item.status {
                    RunStatus::Completed => Some("pass: review accepted".into()),
                    RunStatus::Failed => Some("fix: missing boundary coverage".into()),
                    _ => None,
                };
                item.roles[0].name = if id % 2 == 0 { "builder" } else { "reviewer" }.into();
                item.roles[0].provider = if id % 2 == 0 { "codex" } else { "claude" }.into();
                item.roles[0].activation.as_mut().unwrap().active =
                    item.status == RunStatus::Active;
                if item.status == RunStatus::Created {
                    item.roles[0].activation = None;
                }
                item
            })
            .collect();
        let artifacts = (1..=36)
            .filter(|id| id % 5 == 1)
            .map(|id| ArtifactEntry {
                id: uniterm_core::ArtifactId(id),
                project: ProjectId(1),
                producer_run: RunId(id),
                producer_role: RoleId(id),
                kind: uniterm_core::ArtifactKind::TestEvidence,
                path: format!("evidence/check-{id}.txt"),
                digest: "a".repeat(64),
                size: 1024,
                status: uniterm_core::ArtifactStatus::Available,
                supersedes: None,
            })
            .collect();
        graph.update(100, runs, artifacts, (1..=36).map(PaneId).collect());
        graph.projects.insert(
            ProjectId(1),
            ("Request validation".into(), "/work/api".into()),
        );
        graph.history = Some((
            RunId(1),
            vec![
                "Event #96 | Attempt #1: builder activated".into(),
                "Event #97 | Submission | verdict: fix | add boundary coverage".into(),
                "Event #98 | Attempt #2: reviewer activated".into(),
                "Event #99 | Review verdict: pass | boundary checks accepted".into(),
            ],
        ));
        let mut view = Today::new(Rect::new(0, 0, 140, 40));
        view.graph = Some(graph);
        view.mode = TodayMode::Graph;
        view.graph_enabled = true;
        view.message.clear();
        let theme = uniterm_core::Theme::dark();
        for (width, height) in [(140, 40), (80, 24), (52, 48)] {
            view.grid = Grid::new(width, height);
            view.compose(&theme);
            let graph = view.graph.as_ref().unwrap();
            assert!(graph.hits.len() >= 8);
            assert!(graph
                .hits
                .windows(2)
                .all(|pair| pair[1].0.y == pair[0].0.y + 1));
            assert!(graph
                .hits
                .iter()
                .all(|(rect, _)| rect.h == 1 && rect.bottom() < height - 5));
            let text = fixture_text(&view.grid);
            assert!(text.contains("1 art"));
            assert!(text.contains("pass: review accepted"));
            assert!(!text.contains("verified"));
            export_fixture(
                &format!("today-graph-runs-dense-{width}x{height}"),
                &view.grid,
                &theme,
            );
            view.grid.clear_damage();
            view.compose(&theme);
            assert!(!view.grid.is_dirty());
        }
        view.grid = Grid::new(140, 40);
        view.graph.as_mut().unwrap().details = true;
        view.compose(&theme);
        let text = fixture_text(&view.grid);
        assert!(text.contains("Attempt #1"));
        assert!(text.contains("verdict: fix"));
        assert!(text.contains("Review verdict: pass"));
        assert!(text.contains("check-1.txt"));
        export_fixture("today-graph-runs-dense-history-140x40", &view.grid, &theme);
    }

    #[test]
    fn graph_topology_keeps_identity_selection_and_marks_missing_parent() {
        let mut graph = Graph::new();
        graph.agents_view = false;
        graph.update(
            1,
            vec![run(3, Some(99)), run(2, Some(1)), run(1, None)],
            vec![],
            HashSet::from([PaneId(1)]),
        );
        assert_eq!(
            graph
                .rows
                .iter()
                .map(|row| (row.run.0, row.role.is_some(), row.depth))
                .collect::<Vec<_>>(),
            vec![
                (1, false, 0),
                (1, true, 1),
                (2, false, 1),
                (2, true, 2),
                (3, false, 0),
                (3, true, 1)
            ]
        );
        assert!(graph.rows[4].missing_parent);
        graph.selected = 3;
        graph.update(2, vec![run(2, None), run(3, None)], vec![], HashSet::new());
        assert_eq!(graph.rows[graph.selected].role, Some(RoleId(2)));
        assert_eq!(graph.target(), None);
    }

    #[test]
    fn graph_first_compose_click_geometry_and_unchanged_damage() {
        for (width, height) in [(100, 30), (30, 12), (2, 2)] {
            let mut graph = Graph::new();
            graph.agents_view = false;
            graph.update(1, vec![run(1, None)], vec![], HashSet::from([PaneId(1)]));
            let mut grid = Grid::new(width, height);
            let theme = uniterm_core::Theme::named("uniterm-dark");
            graph.compose(&mut grid, &theme, "");
            if height > 4 {
                assert_eq!(grid.get(0, 3).ch, '▎');
                assert_eq!(graph.hits[0], (Rect::new(0, 3, width, 1), 0));
                assert_eq!(graph.hits[1].0.y, 4);
                assert_eq!(graph.target(), Some((RunId(1), RoleId(1), PaneId(1))));
            }
            grid.clear_damage();
            graph.compose(&mut grid, &theme, "");
            assert!(!grid.is_dirty());
            let mut bytes = vec![];
            Renderer::new().render_pane_damage(&grid, 0, 0, &mut bytes);
            assert!(bytes.is_empty());
        }
    }

    #[test]
    fn graph_history_never_focuses_reused_panes_or_claims_a_verdict() {
        let mut graph = Graph::new();
        graph.agents_view = false;
        let mut completed = run(1, None);
        completed.status = RunStatus::Completed;
        graph.update(1, vec![completed], vec![], HashSet::from([PaneId(1)]));
        assert_eq!(graph.target(), None);
        assert!(graph.detail().contains("No terminal verdict recorded"));
        assert!(graph
            .detail()
            .contains("Press r to load authoritative history"));
    }

    #[test]
    fn graph_composition_budget_and_idle() {
        let mut graph = Graph::new();
        graph.agents_view = false;
        graph.update(
            1,
            (1..=256).map(|id| run(id, None)).collect(),
            vec![],
            HashSet::new(),
        );
        let mut grid = Grid::new(100, 30);
        let theme = uniterm_core::Theme::named("uniterm-dark");
        graph.compose(&mut grid, &theme, "");
        grid.clear_damage();
        let start = std::time::Instant::now();
        for _ in 0..100 {
            graph.compose(&mut grid, &theme, "");
        }
        eprintln!(
            "graph 256-run composition mean: {:?}",
            start.elapsed() / 100
        );
        assert!(!grid.is_dirty());
    }
}
