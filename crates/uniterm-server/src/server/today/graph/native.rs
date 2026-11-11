//! Ordinary Pane invocations and observed native children share Graph's rails,
//! tabs, inspector and final terminal sanitization with durable run evidence.
use super::*;
use uniterm_core::timeline::{agent_graph, AgentGraphNode};

pub(in crate::server::today) type LiveAgents = BTreeMap<PaneId, LiveAgent>;

#[derive(PartialEq, Eq)]
pub(in crate::server::today) struct LiveAgent {
    invocation: u64,
    session: Option<String>,
    status: AgentStatus,
    work: Option<String>,
}

impl Graph {
    pub(in crate::server::today) fn coverage(&self) -> Option<String> {
        if !self.agents_view || self.details {
            return None;
        }
        let mut labels = Vec::new();
        if self.agent_day.next_before.is_some() {
            labels.push("Older (o)");
        }
        if self.agent_day.active_agents_truncated {
            labels.push("Truncated agents");
        }
        if self.agent_day.partial_history {
            labels.push("Partial history");
        }
        (!labels.is_empty()).then(|| labels.join(" · "))
    }

    pub(in crate::server::today) fn key_hint(&self) -> &'static str {
        if self.agents_view {
            "↑↓ Enter focus  v history  n view  [] day  t today  o older  r refresh  Esc"
        } else {
            "↑↓ select / scroll  v inspect  n Agents / Runs  r refresh  Enter go  Esc back"
        }
    }

    pub(in crate::server::today) fn query(&self) -> (String, String, Option<u64>) {
        (self.agent_date.clone(), String::new(), self.agent_before)
    }

    pub(in crate::server::today) fn agent_sequence(&self) -> u64 {
        self.agent_day.current_sequence
    }

    pub(in crate::server::today) fn accept_agents(&mut self, day: TimelineDay, live: LiveAgents) {
        let selected = self
            .agent_nodes
            .get(self.agent_selected)
            .map(|node| node.identity.clone());
        let entries: Vec<_> = day
            .entries
            .iter()
            .chain(&day.active_agents)
            .cloned()
            .collect();
        self.agent_nodes = agent_graph(&entries);
        self.agent_selected = selected
            .and_then(|id| {
                self.agent_nodes
                    .iter()
                    .position(|node| node.identity == id)
                    .or_else(|| {
                        // Session discovery enriches the same selected invocation;
                        // it cannot transfer selection to an unrelated row inserted
                        // before it or arbitrarily choose among replaced sessions.
                        if id.invocation.is_none() || id.session.is_some() {
                            return None;
                        }
                        let mut candidates =
                            self.agent_nodes.iter().enumerate().filter(|(_, node)| {
                                node.identity.invocation == id.invocation
                                    && node.identity.provider == id.provider
                                    && node.identity.project == id.project
                                    && node.identity.pane == id.pane
                            });
                        let (index, _) = candidates.next()?;
                        candidates.next().is_none().then_some(index)
                    })
            })
            .unwrap_or(self.agent_selected)
            .min(self.agent_nodes.len().saturating_sub(1));
        self.agent_times = day
            .entries
            .iter()
            .zip(&day.times)
            .map(|(entry, time)| (entry.sequence, time.clone()))
            .collect();
        self.agent_day = day;
        self.live_agents = live;
        self.dirty_panes.clear();
    }

    fn live_agent(&self, node: &AgentGraphNode) -> Option<&LiveAgent> {
        self.live_agents.get(&node.identity.pane?).filter(|live| {
            Some(live.invocation) == node.identity.invocation
                && live.status != AgentStatus::Exited
                && node
                    .identity
                    .session
                    .as_ref()
                    .is_none_or(|id| live.session.as_ref() == Some(id))
        })
    }

    fn agent_target(&self) -> Option<(PaneId, u64, Option<String>)> {
        let node = self.agent_nodes.get(self.agent_selected)?;
        self.live_agent(node)?;
        Some((
            node.identity.pane?,
            node.identity.invocation?,
            node.identity.session.clone(),
        ))
    }

    fn agent_work<'a>(&'a self, node: &'a AgentGraphNode) -> &'a str {
        self.live_agent(node)
            .and_then(|live| live.work.as_deref())
            .or(node.span.last.work_summary.as_deref())
            .unwrap_or("Work not reported")
    }

    fn agent_liveness(&self, node: &AgentGraphNode) -> String {
        if node.identity.invocation.is_none() {
            return "child session · liveness unknown".into();
        }
        if let Some(live) = self.live_agent(node) {
            return format!("live Pane · {:?}", live.status);
        }
        format!(
            "historical · observed {}",
            node.status.as_deref().unwrap_or(&node.span.last.summary)
        )
    }

    fn agent_detail(&self, node: &AgentGraphNode) -> String {
        let entry = &node.span.last;
        let mut detail = format!(
            "{} · {}\nProject: {}\nPath: {}\nBranch: {} · Tab: {}\nInvocation: {} · Pane: {}\nSession: {}\nParent invocation: {}\nParent session: {}{}\n\nObserved history (event sequence / Unix ms):\n",
            entry.provider, self.agent_liveness(node), entry.context.project_name,
            entry.context.root, entry.context.branch, entry.context.tab,
            entry.invocation.map_or_else(|| "none (native child)".into(), |id| format!("#{id}")),
            entry.context.pane.map_or_else(|| "unavailable".into(), |id| format!("#{}", id.0)),
            entry.session_id.as_deref().unwrap_or("not reported"),
            entry.parent_invocation.map_or_else(|| "not reported".into(), |id| format!("#{id}")),
            entry.parent_session_id.as_deref().unwrap_or("not reported"),
            if node.parent_missing { " · unresolved in this page (missing or ambiguous parent / cycle)" } else { "" },
        );
        detail.push_str(&format!("Work: {}\n", self.agent_work(node)));
        let mut work = None;
        for event in &node.evidence {
            detail.push_str(&format!(
                "Event #{} · {} · {}\n",
                event.sequence, event.timestamp_ms, event.summary
            ));
            if event.work_summary.as_deref() != work {
                work = event.work_summary.as_deref();
                detail.push_str(&format!(
                    "  Observed work: {}\n",
                    work.unwrap_or("not reported")
                ));
            }
        }
        detail.push_str("\nEdges record ancestry only. Idle, exit and silence do not establish task completion. Native child liveness is unknown without its own live invocation.");
        detail
    }

    /// Shared selectors use the same cell style and rectangles as Today tabs.
    pub(in crate::server::today) fn compose_content(
        &mut self,
        grid: &mut Grid,
        area: Rect,
        theme: &uniterm_core::Theme,
    ) {
        self.hits.clear();
        self.source_hits.clear();
        self.context_dirty = false;
        if area.h < 3 || area.w < 4 {
            return;
        }
        for (rect, (label, agents)) in
            crate::chrome::tab_strip_slots(Rect::new(area.x, area.y, area.w, 1), 2)
                .into_iter()
                .zip([("Agents", true), ("Runs / evidence", false)])
        {
            let style = crate::chrome::tab_strip_cell(theme, self.agents_view == agents);
            let label = crate::chrome::tab_strip_label(label, rect.w as usize);
            put(grid, rect.x, rect.y, &label, style.fg, style.bg);
            for x in rect.x..rect.right() {
                let mut cell = grid.get(x, rect.y);
                cell.attrs = style.attrs;
                grid.set(x, rect.y, cell);
            }
            self.source_hits.push((rect, agents));
        }
        let content = Rect::new(area.x, area.y + 2, area.w, area.h - 2);
        if self.agents_view {
            self.compose_agents(grid, content, theme);
        } else {
            self.compose_runs(grid, content, theme);
        }
    }

    fn compose_agents(&mut self, grid: &mut Grid, area: Rect, theme: &uniterm_core::Theme) {
        if area.h < 3 || area.w < 4 {
            return;
        }
        if self.details {
            panel(
                grid,
                area,
                " Observed agent history · v back ",
                theme.border,
            );
            let text = self.agent_nodes.get(self.agent_selected).map_or_else(
                || {
                    "No observed agents in this day/page. Launch any supported agent in a Pane."
                        .into()
                },
                |node| self.agent_detail(node),
            );
            let lines = detail_lines(&text, usize::from(area.w.saturating_sub(4)).max(1));
            let visible = usize::from(area.h - 2);
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
                "AGENT GRAPH · {} agents · {}",
                self.agent_nodes.len(),
                self.agent_day.date
            ),
            theme.muted,
            Color::Default,
        );
        let visible = usize::from(trace.h.saturating_sub(1));
        if self.agent_selected < self.agent_scroll {
            self.agent_scroll = self.agent_selected;
        }
        if visible > 0 && self.agent_selected >= self.agent_scroll + visible {
            self.agent_scroll = self.agent_selected + 1 - visible;
        }
        if self.agent_nodes.is_empty() && visible > 0 {
            row_text(
                grid,
                Rect::new(trace.x, trace.y + 1, trace.w, 1),
                "No observed agents yet. Launch an agent in any Pane.",
                theme.muted,
                Color::Default,
            );
        }
        let depths: Vec<_> = self.agent_nodes.iter().map(|node| node.depth).collect();
        let path = presentation::selected_path(&depths, self.agent_selected);
        for (offset, node) in self
            .agent_nodes
            .iter()
            .skip(self.agent_scroll)
            .take(visible)
            .enumerate()
        {
            let index = self.agent_scroll + offset;
            let rect = Rect::new(trace.x, trace.y + 1 + offset as u16, trace.w, 1);
            let live = self.live_agent(node);
            let status = live.map_or(
                if node.identity.invocation.is_none() {
                    "unknown"
                } else {
                    "historical"
                },
                |live| live.status.label(),
            );
            let (glyph, color) = presentation::agent_state(
                live.map_or(AgentStatus::Unknown, |live| live.status),
                theme,
            );
            let rail = ancestry_rail(
                node.depth,
                self.agent_nodes[index + 1..].iter().map(|node| node.depth),
                usize::from(rect.w.saturating_sub(36) / 2).max(1),
            );
            presentation::trace_row(
                grid,
                rect,
                presentation::TraceRow {
                    rail: &rail,
                    depth: node.depth,
                    index,
                    path: &path,
                    selected: index == self.agent_selected,
                    glyph,
                    identity: "",
                    label: self.agent_work(node),
                    context: &node.span.last.provider,
                    status,
                    evidence: if node.parent_missing { "?" } else { "" },
                    color,
                },
                theme,
            );
            self.hits.push((rect, index));
        }
        if let Some(node) = self.agent_nodes.get(self.agent_selected) {
            let entry = &node.span.last;
            let (_, color) = presentation::agent_state(
                self.live_agent(node)
                    .map_or(AgentStatus::Unknown, |live| live.status),
                theme,
            );
            let time = self
                .agent_times
                .get(&entry.sequence)
                .cloned()
                .unwrap_or_else(|| {
                    if entry.timestamp_ms < self.agent_day.start_ms {
                        "carried over".into()
                    } else {
                        format!(
                            "observed {:02}:{:02} UTC",
                            (entry.timestamp_ms / 3_600_000) % 24,
                            (entry.timestamp_ms / 60_000) % 60
                        )
                    }
                });
            let (project, root) = self
                .live_agent(node)
                .and_then(|_| node.identity.project.and_then(|id| self.projects.get(&id)))
                .map_or(
                    (
                        entry.context.project_name.as_str(),
                        entry.context.root.as_str(),
                    ),
                    |(name, root)| (name.as_str(), root.as_str()),
                );
            let status = self.live_agent(node).map_or(
                if node.identity.invocation.is_none() {
                    "unknown"
                } else {
                    "historical"
                },
                |live| live.status.label(),
            );
            presentation::inspector(
                grid,
                dock,
                presentation::Inspector {
                    identity: &entry.provider,
                    status,
                    lines: &[
                        self.agent_work(node).into(),
                        format!(
                            "Project: {}",
                            if project.is_empty() {
                                "Workspace"
                            } else {
                                project
                            }
                        ),
                        if root.is_empty() {
                            "Path not recorded".into()
                        } else {
                            root.into()
                        },
                        format!(
                            "{} · {} observation{}{}",
                            self.agent_liveness(node),
                            node.span.observations,
                            if node.span.observations == 1 { "" } else { "s" },
                            if node.parent_missing {
                                " · ? parent unresolved"
                            } else {
                                ""
                            }
                        ),
                    ],
                    footer: &if entry.context.tab.is_empty() {
                        time
                    } else {
                        format!("{time} · Tab {}", entry.context.tab)
                    },
                    color,
                },
                theme,
            );
        }
    }
}

/// Preserve continuous ancestry and the local branch when deep rails are clipped.
pub(super) fn ancestry_rail(
    depth: usize,
    following: impl Iterator<Item = usize> + Clone,
    max_depth: usize,
) -> String {
    let shown = depth.min(max_depth);
    let mut rail = String::new();
    if depth > shown {
        rail.push('…');
    }
    for level in depth - shown..depth {
        let continues = following
            .clone()
            .take_while(|next| *next > level)
            .any(|next| next == level + 1);
        rail.push_str(if level + 1 == depth {
            if continues {
                "├─"
            } else {
                "└─"
            }
        } else if continues {
            "│ "
        } else {
            "  "
        });
    }
    rail
}

impl Server {
    pub(super) fn graph_live_agent(&self, pane: PaneId) -> Option<LiveAgent> {
        let invocation = *self.timeline_invocations.get(&pane)?;
        let agent = self.panes.get(&pane)?.agent.as_ref()?;
        Some(LiveAgent {
            invocation,
            session: agent.session_id.clone(),
            status: agent.status,
            work: self.automatic_pane_name(pane),
        })
    }

    pub(in crate::server::today) fn graph_live_agents(&self) -> LiveAgents {
        self.timeline_invocations
            .keys()
            .filter_map(|pane| self.graph_live_agent(*pane).map(|live| (*pane, live)))
            .collect()
    }

    pub(super) fn native_graph_input(&mut self, reg: &Registry, bytes: &[u8]) {
        let Some(view) = self.today.as_mut() else {
            return;
        };
        let Some(graph) = view.graph.as_mut() else {
            return;
        };
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
            b"j" | b"\x1b[B" if graph.details => {
                graph.detail_scroll = graph.detail_scroll.saturating_add(1)
            }
            b"k" | b"\x1b[A" if graph.details => {
                graph.detail_scroll = graph.detail_scroll.saturating_sub(1)
            }
            b"j" | b"\x1b[B" => {
                graph.agent_selected =
                    (graph.agent_selected + 1).min(graph.agent_nodes.len().saturating_sub(1))
            }
            b"k" | b"\x1b[A" => graph.agent_selected = graph.agent_selected.saturating_sub(1),
            b"\r" | b"g" => {
                if let Some((pane, invocation, session)) =
                    graph.agent_target().filter(|(pane, invocation, session)| {
                        self.timeline_invocations.get(pane) == Some(invocation)
                            && self
                                .panes
                                .get(pane)
                                .and_then(|pane| pane.agent.as_ref())
                                .is_some_and(|agent| {
                                    agent.status != AgentStatus::Exited
                                        && session
                                            .as_ref()
                                            .is_none_or(|id| agent.session_id.as_ref() == Some(id))
                                })
                    })
                {
                    let _ = (invocation, session);
                    self.close_today();
                    self.focus_pane_target(reg, pane);
                    return;
                }
                view.message = "No matching live invocation. Native child liveness is unknown; v shows recorded history.".into();
            }
            b"r" | b"o" | b"[" | b"]" | b"t" => {
                match bytes {
                    b"o" => {
                        if graph.agent_day.next_before.is_none() {
                            return;
                        }
                        graph.agent_before = graph.agent_day.next_before;
                    }
                    b"[" => {
                        graph.agent_date.clone_from(&graph.agent_day.previous);
                        graph.agent_before = None;
                    }
                    b"]" => {
                        graph.agent_date.clone_from(&graph.agent_day.next);
                        graph.agent_before = None;
                    }
                    b"t" => {
                        graph.agent_date.clear();
                        graph.agent_before = None;
                    }
                    _ => {}
                }
                self.request_today(None);
            }
            _ => {}
        }
        self.paint_today(reg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniterm_core::timeline::{TimelineContext, TimelineEntry, TimelineKind};

    fn observation(sequence: u64, invocation: Option<u64>, session: &str) -> TimelineEntry {
        TimelineEntry {
            sequence,
            timestamp_ms: 1000 * sequence,
            invocation,
            context: TimelineContext {
                project: Some(ProjectId(1)),
                project_name: "工程 fixture".into(),
                root: "/tmp/graph-fixture".into(),
                pane: Some(PaneId(7)),
                ..TimelineContext::default()
            },
            provider: "custom-provider".into(),
            session_id: Some(session.into()),
            parent_invocation: None,
            parent_session_id: None,
            kind: TimelineKind::Agent,
            work_summary: Some("Validate input handling".into()),
            summary: "Idle".into(),
        }
    }

    fn live(invocation: u64, session: &str, status: AgentStatus) -> LiveAgent {
        LiveAgent {
            invocation,
            session: Some(session.into()),
            status,
            work: None,
        }
    }

    #[test]
    fn ordinary_graph_focus_requires_exact_invocation_and_session_and_never_targets_children() {
        let root = observation(10, Some(10), "root");
        let mut child = observation(11, None, "child");
        child.parent_session_id = Some("root".into());
        let mut graph = Graph::new();
        graph.accept_agents(
            TimelineDay {
                entries: vec![root.clone(), child],
                ..TimelineDay::default()
            },
            BTreeMap::from([(PaneId(7), live(10, "root", AgentStatus::Idle))]),
        );
        assert_eq!(
            graph.agent_target(),
            Some((PaneId(7), 10, Some("root".into())))
        );
        assert_eq!(graph.agent_nodes[1].parent, Some(0));
        graph.agent_selected = 1;
        assert_eq!(graph.agent_target(), None);
        assert!(graph
            .agent_detail(&graph.agent_nodes[1])
            .contains("liveness unknown"));
        graph.agent_selected = 0;
        graph
            .live_agents
            .insert(PaneId(7), live(12, "root", AgentStatus::Idle));
        assert_eq!(
            graph.agent_target(),
            None,
            "same Pane/session but a resumed invocation"
        );
        graph
            .live_agents
            .insert(PaneId(7), live(10, "new-session", AgentStatus::Idle));
        assert_eq!(
            graph.agent_target(),
            None,
            "native session replaced inside invocation"
        );
        graph
            .live_agents
            .insert(PaneId(7), live(10, "root", AgentStatus::Exited));
        assert_eq!(
            graph.agent_target(),
            None,
            "exited agent is not a live focus target"
        );
        assert!(!graph
            .agent_detail(&graph.agent_nodes[0])
            .contains("completed"));
    }

    #[test]
    fn ordinary_graph_first_attach_narrow_unicode_clicks_and_noop_are_damage_tracked() {
        let root = observation(10, Some(10), "root");
        let mut child = observation(11, None, "child\x1b]52;c;bad\x07");
        child.parent_session_id = Some("root".into());
        let mut graph = Graph::new();
        graph.accept_agents(
            TimelineDay {
                date: "2026-11-10".into(),
                entries: vec![root, child],
                active_agents_truncated: true,
                partial_history: true,
                next_before: Some(10),
                ..TimelineDay::default()
            },
            BTreeMap::new(),
        );
        for (width, height) in [(140, 40), (70, 25), (30, 15), (12, 10), (4, 4), (1, 1)] {
            let mut grid = Grid::new(width, height);
            graph.compose(&mut grid, &uniterm_core::Theme::dark(), "");
            let text = fixture_text(&grid);
            assert!(!text.contains('\x1b'));
            assert!(!text.contains('\x07'));
            assert!(graph
                .hits
                .iter()
                .all(|(rect, _)| rect.right() <= width && rect.bottom() <= height));
            assert!(graph
                .source_hits
                .iter()
                .all(|(rect, _)| rect.right() <= width && rect.bottom() <= height));
            if width >= 70 {
                assert_eq!(graph.hits[0], (Rect::new(0, 3, width, 1), 0));
                assert_eq!(graph.hits[1], (Rect::new(0, 4, width, 1), 1));
                assert!(text.contains("工程 fixture"));
                assert!(text.contains("/tmp/graph-fixture"));
                assert!(text.contains("Validate input handling"));
            }
            if width >= 140 {
                assert!(text.contains("unknown"));
                let coverage = graph.coverage().unwrap();
                assert!(coverage.contains("Truncated agents"));
                assert!(coverage.contains("Partial history"));
            }
            grid.clear_damage();
            graph.compose(&mut grid, &uniterm_core::Theme::dark(), "");
            assert!(!grid.is_dirty(), "unchanged graph at {width}x{height}");
            let mut bytes = Vec::new();
            Renderer::new().render_pane_damage(&grid, 0, 0, &mut bytes);
            assert!(bytes.is_empty());
        }
    }

    #[test]
    fn session_discovery_preserves_selection_when_earlier_rows_arrive() {
        let mut unknown = observation(10, Some(10), "session");
        unknown.session_id = None;
        let mut graph = Graph::new();
        graph.accept_agents(
            TimelineDay {
                entries: vec![unknown.clone()],
                ..TimelineDay::default()
            },
            BTreeMap::new(),
        );
        let earlier = observation(5, Some(5), "earlier");
        let identified = observation(11, Some(10), "session");
        graph.accept_agents(
            TimelineDay {
                entries: vec![earlier, unknown, identified],
                ..TimelineDay::default()
            },
            BTreeMap::new(),
        );
        assert_eq!(graph.agent_selected, 1);
        assert_eq!(
            graph.agent_nodes[graph.agent_selected].identity.invocation,
            Some(10)
        );
    }

    #[test]
    fn small_native_graph_has_compact_continuous_rails_and_docked_context() {
        let mut root = observation(10, Some(10), "root-session");
        root.provider = "codex".into();
        root.summary = "Working".into();
        root.work_summary = Some("Validate request handling".into());
        let mut child = observation(11, None, "review-session");
        child.provider = "codex".into();
        child.parent_session_id = root.session_id.clone();
        child.summary = "Child session observed".into();
        child.work_summary = Some("Review error cases".into());
        let mut sibling = child.clone();
        sibling.sequence = 12;
        sibling.session_id = Some("checks-session".into());
        sibling.work_summary = Some("Check Unicode input".into());
        let mut historical = observation(13, Some(5), "earlier-session");
        historical.provider = "claude".into();
        historical.summary = "Ended".into();
        historical.work_summary = Some("Inspect validation boundaries".into());
        let mut graph = Graph::new();
        graph.accept_agents(
            TimelineDay {
                date: "2026-11-10".into(),
                entries: vec![root, child, sibling, historical],
                times: vec![
                    "09:40:00".into(),
                    "09:41:00".into(),
                    "09:42:00".into(),
                    "09:43:00".into(),
                ],
                ..TimelineDay::default()
            },
            BTreeMap::from([(PaneId(7), live(10, "root-session", AgentStatus::Working))]),
        );
        let mut view = Today::new(Rect::new(0, 0, 140, 30));
        view.graph = Some(graph);
        view.mode = TodayMode::Graph;
        view.graph_enabled = true;
        view.message.clear();
        let theme = uniterm_core::Theme::dark();
        for (width, height) in [(140, 40), (80, 24), (52, 48)] {
            view.grid = Grid::new(width, height);
            view.compose(&theme);
            let graph = view.graph.as_ref().unwrap();
            assert_eq!(
                graph.hits.len(),
                4,
                "all four nodes fit at {width}x{height}"
            );
            let text = fixture_text(&view.grid);
            assert!(text.contains("1 observation"));
            assert!(!text.contains("1 observations"));
            assert_eq!(text.matches("Enter focus").count(), 1);
            assert!(!text.contains("Recorded ancestry"));
            assert!(!text.contains("Ancestry is observed"));
            let child = graph.hits[1].0;
            assert_eq!(child.h, 1);
            assert_eq!(view.grid.get(child.x + 1, child.y).ch, '├');
            let sibling = graph.hits[2].0;
            assert_eq!(sibling.y, child.y + 1);
            assert_eq!(view.grid.get(sibling.x + 1, sibling.y).ch, '└');
            assert!(text.contains("/tmp/graph-fixture"));
            export_fixture(
                &format!("today-native-graph-small-{width}x{height}"),
                &view.grid,
                &theme,
            );
            view.grid.clear_damage();
            view.compose(&theme);
            assert!(!view.grid.is_dirty());
        }
    }

    #[test]
    fn ordinary_graph_selection_survives_updates_and_pages_report_limits() {
        let root = observation(10, Some(10), "root");
        let mut child = observation(11, None, "child");
        child.parent_session_id = Some("root".into());
        let mut graph = Graph::new();
        graph.accept_agents(
            TimelineDay {
                entries: vec![root.clone(), child.clone()],
                ..TimelineDay::default()
            },
            BTreeMap::new(),
        );
        graph.agent_selected = 1;
        let mut newer = root.clone();
        newer.sequence = 15;
        newer.summary = "Working".into();
        graph.accept_agents(
            TimelineDay {
                entries: vec![newer, root],
                active_agents: vec![child],
                ..TimelineDay::default()
            },
            BTreeMap::new(),
        );
        assert_eq!(
            graph.agent_nodes[graph.agent_selected]
                .identity
                .session
                .as_deref(),
            Some("child")
        );
        assert_eq!(graph.agent_nodes[0].status.as_deref(), Some("Working"));
        assert_eq!(graph.agent_nodes[0].evidence.len(), 2);
        assert!(graph
            .agent_detail(&graph.agent_nodes[0])
            .contains("Event #15"));
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use uniterm_core::timeline::{TimelineContext, TimelineEntry, TimelineKind};

    #[test]
    fn dense_native_graph_projection_composition_budget_and_export() {
        let entries: Vec<_> = (1..=512)
            .map(|id| TimelineEntry {
                sequence: id,
                timestamp_ms: id * 1000,
                invocation: Some(id),
                context: TimelineContext {
                    project: Some(ProjectId(1)),
                    project_name: "Validation fixture".into(),
                    root: "/tmp/native-graph-fixture".into(),
                    pane: Some(PaneId(id)),
                    ..TimelineContext::default()
                },
                provider: if id % 2 == 0 { "codex" } else { "claude" }.into(),
                session_id: Some(format!("session-{id}")),
                parent_invocation: (id > 1).then_some(id / 2),
                parent_session_id: None,
                kind: TimelineKind::Agent,
                summary: "Idle".into(),
                work_summary: Some(format!("Validate request handling {id}")),
            })
            .collect();
        let children: Vec<_> = (513..=1024)
            .map(|id| TimelineEntry {
                sequence: id,
                timestamp_ms: id * 1000,
                invocation: None,
                context: entries[0].context.clone(),
                provider: "claude".into(),
                session_id: Some(format!("child-{id}")),
                parent_invocation: None,
                parent_session_id: Some("session-1".into()),
                kind: TimelineKind::Agent,
                summary: "Child session observed".into(),
                work_summary: None,
            })
            .collect();
        let day = TimelineDay {
            date: "2026-11-10".into(),
            current_sequence: 1024,
            entries,
            active_agents: children,
            next_before: Some(1),
            active_agents_truncated: true,
            ..TimelineDay::default()
        };
        let mut graph = Graph::new();
        let started = std::time::Instant::now();
        let live = (1..=512)
            .map(|id| {
                (
                    PaneId(id),
                    LiveAgent {
                        invocation: id,
                        session: Some(format!("session-{id}")),
                        status: match id % 6 {
                            0 => AgentStatus::Working,
                            1 => AgentStatus::Permission,
                            2 => AgentStatus::Idle,
                            3 => AgentStatus::Tool,
                            4 => AgentStatus::Question,
                            _ => AgentStatus::Error,
                        },
                        work: None,
                    },
                )
            })
            .collect();
        graph.accept_agents(day, live);
        let projection = started.elapsed();
        assert_eq!(graph.agent_nodes.len(), 1024);
        let mut view = Today::new(Rect::new(0, 0, 140, 40));
        view.graph = Some(graph);
        view.mode = TodayMode::Graph;
        view.graph_enabled = true;
        view.message.clear();
        let theme = uniterm_core::Theme::dark();
        for (width, height) in [(140, 40), (80, 24), (52, 48)] {
            view.grid = Grid::new(width, height);
            view.graph.as_mut().unwrap().agent_selected = 15;
            view.graph.as_mut().unwrap().agent_scroll = 0;
            view.compose(&theme);
            export_fixture(
                &format!("today-native-graph-{width}x{height}"),
                &view.grid,
                &theme,
            );
        }
        view.grid = Grid::new(140, 40);
        view.compose(&theme);
        view.grid.clear_damage();
        let start = std::time::Instant::now();
        for _ in 0..100 {
            view.compose(&theme);
        }
        let mean = start.elapsed() / 100;
        eprintln!("native graph 512-day + 512-carryover projection: {projection:?}; 140x40 composition mean: {mean:?}");
        assert!(
            projection < std::time::Duration::from_millis(500),
            "bounded debug projection budget"
        );
        assert!(
            mean < std::time::Duration::from_millis(50),
            "input-to-pixel composition budget"
        );
        assert!(!view.grid.is_dirty());
        let mut bytes = Vec::new();
        Renderer::new().render_pane_damage(&view.grid, 0, 0, &mut bytes);
        assert!(
            bytes.is_empty(),
            "unchanged dense graph emits no terminal bytes"
        );
        // Export the same Grid rendered for the inspector, with actual history.
        view.graph.as_mut().unwrap().details = true;
        view.compose(&theme);
        export_fixture("today-native-graph-history-140x40", &view.grid, &theme);
    }
}
