//! Human reader for the Workspace's Project messageboards.
use crate::overlay::{
    clip_text, finish_lines, footer_spans, footer_text, footer_width, modal_hit, modal_rect,
    modal_visible_rows, panel_style, render_list_modal_content, styled_line, ui_theme, ModalHit,
    Rect,
};
use crate::text_input::{decode_key, LineKey};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use unicode_width::UnicodeWidthStr;
use uniterm_core::ProjectId;
use uniterm_proto::{
    encode_frame, ClientMessage, InboxAuthor, InboxPageStatus, InboxPost, InboxReadCursor,
};

mod thread_layout;
use thread_layout::ThreadLayout;

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

fn next_request() -> u64 {
    let seq = NEXT_REQUEST
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
            Some(if id == uniterm_proto::INBOX_LIVE_REQUEST - 1 {
                1
            } else {
                id + 1
            })
        })
        .unwrap_or(1);
    seq | uniterm_proto::INBOX_LIVE_REQUEST
}
const CARD_ROWS: usize = 3;

#[derive(Default)]
pub(crate) struct InboxView {
    notice: Option<String>,
    request: u64,
    projects: Vec<(ProjectId, String)>,
    project: usize,
    projects_focused: bool,
    loading: bool,
    // Pages arrive in id order. The feed orders threads by their latest reply.
    threads: BTreeMap<u64, Vec<InboxPost>>,
    order: Vec<u64>,
    selected: usize,
    reading: bool,
    offset: usize,
    incoming: BTreeMap<u64, Vec<InboxPost>>,
    incoming_parents: BTreeMap<u64, u64>,
    post_threads: BTreeMap<u64, u64>,
    snapshot_valid: bool,
    // Keep only navigation identifiers while erased bodies are being reconciled.
    resume_after_erasure: Option<(u64, bool, usize)>,
    snapshot_retries: u8,
    layout: RefCell<ThreadLayout>,
    link: Option<(u64, u64)>,
    link_index: usize,
    parents: BTreeMap<u64, u64>,
    history: Vec<(u64, usize)>,
}

#[derive(Clone)]
enum CardLine {
    Top(String),
    Body(String),
    Note(String),
    Link { post: u64, target: u64 },
    Bottom(String),
}

// Two corners plus the leading rule and both spaces around a label.
fn card_label_width(card_width: usize) -> usize {
    card_width.saturating_sub(5)
}

// A clipped heading still signals that the full subject is available in the post.
fn elide(label: &str, available: usize) -> String {
    let clean = crate::overlay::plain_text(label);
    if clean.width() <= available {
        clean
    } else if available == 0 {
        String::new()
    } else {
        format!("{}…", clip_text(&clean, available - 1))
    }
}

fn card_border(width: usize, label: &str, top: bool, style: &str) -> (String, usize) {
    let (left, right) = if top { ("╭", "╮") } else { ("╰", "╯") };
    let inner = width.saturating_sub(2);
    let mut heading = String::new();
    let available = card_label_width(width);
    if !label.is_empty() && available > 0 {
        let title = elide(label, available);
        heading = format!("─ {title} ");
    }
    styled_line(&[(
        style,
        &format!(
            "{left}{heading}{}{right}",
            "─".repeat(inner - heading.width())
        ),
    )])
}

impl CardLine {
    fn link(&self) -> Option<(u64, u64)> {
        if let Self::Link { post, target } = self {
            Some((*post, *target))
        } else {
            None
        }
    }

    fn render(
        &self,
        width: usize,
        edge: &str,
        text: &str,
        muted: &str,
        target_subject: Option<&str>,
        selected_link: bool,
    ) -> (String, usize) {
        match self {
            Self::Top(label) => card_border(width, label, true, edge),
            Self::Bottom(label) => card_border(width, label, false, edge),
            _ => {
                let link;
                let (style, label) = match self {
                    Self::Body(label) => (text, label.as_str()),
                    Self::Note(label) => (muted, label.as_str()),
                    Self::Link { target, .. } => {
                        link = if let Some(subject) = target_subject {
                            let marker = if selected_link { "▸" } else { "↗" };
                            format!("{marker} thread:{target} · {subject}")
                        } else {
                            format!("thread:{target} (unavailable)")
                        };
                        (
                            if target_subject.is_some() {
                                edge
                            } else {
                                muted
                            },
                            link.as_str(),
                        )
                    }
                    _ => unreachable!(),
                };
                let inner = width.saturating_sub(4);
                let fitted = clip_text(label, inner);
                styled_line(&[
                    (muted, "│"),
                    (style, " "),
                    ("", &fitted),
                    ("", &" ".repeat(inner - fitted.width() + 1)),
                    (muted, "│"),
                ])
            }
        }
    }
}

struct Geometry {
    rect: Rect,
    rail: usize,
    detail: usize,
    visible: usize,
    cards: usize,
}

impl Geometry {
    fn new(cols: u16, rows: u16) -> Self {
        let normal = modal_rect(cols, rows);
        let rect = Rect {
            w: normal.w.min(cols).max(20),
            h: normal.h.min(rows).max(9),
            x: if normal.x.saturating_add(normal.w - 1) > cols {
                1
            } else {
                normal.x
            },
            y: if normal.y.saturating_add(normal.h - 1) > rows {
                1
            } else {
                normal.y
            },
        };
        let rail = (rect.w as usize / 5).clamp(6, 22);
        let visible = modal_visible_rows(rect.h);
        Self {
            rect,
            rail,
            detail: rect.w as usize - rail - 3,
            visible,
            cards: ((visible - 1) / CARD_ROWS).max(1),
        }
    }
}

impl InboxView {
    pub(crate) fn open(out: &mut Vec<u8>) -> Self {
        let mut view = Self::default();
        view.refresh(out);
        view
    }

    /// Input changes only focus, the current thread/line, or the request generation.
    /// Compare these before painting so an inert click or a list boundary emits no bytes.
    pub(crate) fn position(&self) -> (u64, bool, usize, bool, usize, Option<(u64, u64)>) {
        (
            self.request,
            self.projects_focused,
            self.selected,
            self.reading,
            self.offset,
            self.link,
        )
    }

    fn refresh(&mut self, out: &mut Vec<u8>) {
        self.snapshot_retries = 0;
        self.request = next_request();
        self.loading = true;
        out.extend(encode_frame(&ClientMessage::InboxProjects {
            request: self.request,
        }));
    }

    pub(crate) fn channels(
        &mut self,
        request: u64,
        active: ProjectId,
        projects: Vec<(ProjectId, String)>,
        out: &mut Vec<u8>,
    ) -> bool {
        let rail_only = request == uniterm_proto::INBOX_LIVE_REQUEST;
        if !rail_only && request != 0 && request != self.request {
            return false;
        }
        let previous = self
            .projects
            .get(self.project)
            .map(|p| p.0)
            .unwrap_or(active);
        let channels_changed = self.projects != projects;
        self.projects = projects;
        self.project = self
            .projects
            .iter()
            .position(|p| p.0 == previous)
            .or_else(|| self.projects.iter().position(|p| p.0 == active))
            .unwrap_or(0);
        let same = self
            .projects
            .get(self.project)
            .is_some_and(|p| p.0 == previous);
        if rail_only && same {
            return channels_changed;
        }
        if same && (!self.order.is_empty() || self.resume_after_erasure.is_some()) {
            // Keep the conversation readable while replacing its paginated snapshot.
            self.request = next_request();
            self.incoming.clear();
            self.incoming_parents.clear();
            self.loading = true;
            self.request_page(None, out);
            return channels_changed;
        }
        self.select_project(out);
        true
    }

    fn select_project(&mut self, out: &mut Vec<u8>) {
        self.request = next_request();
        self.incoming.clear();
        self.incoming_parents.clear();
        self.parents.clear();
        self.post_threads.clear();
        self.snapshot_valid = false;
        self.snapshot_retries = 0;
        self.resume_after_erasure = None;
        self.link = None;
        self.history.clear();
        self.notice = None;
        self.threads.clear();
        self.order.clear();
        self.reading = false;
        self.offset = 0;
        self.selected = 0;
        *self.layout.get_mut() = ThreadLayout::default();
        self.loading = self.projects.get(self.project).is_some();
        self.request_page(None, out);
    }

    fn request_page(&self, cursor: Option<InboxReadCursor>, out: &mut Vec<u8>) {
        if let Some((project, _)) = self.projects.get(self.project) {
            out.extend(encode_frame(&ClientMessage::InboxRead {
                request: self.request,
                project: *project,
                cursor,
            }));
        }
    }

    pub(crate) fn project_removed(&mut self, project: ProjectId, out: &mut Vec<u8>) -> bool {
        let Some(index) = self.projects.iter().position(|p| p.0 == project) else {
            return false;
        };
        self.projects.remove(index);
        if index == self.project {
            self.project = self.project.min(self.projects.len().saturating_sub(1));
            self.select_project(out);
            if self.projects.is_empty() {
                // Another attachment may have created the replacement since our last refresh.
                self.refresh(out);
            }
        } else if index < self.project {
            self.project -= 1;
        }
        true
    }

    pub(crate) fn page_parents(
        &mut self,
        request: u64,
        project: ProjectId,
        parents: Vec<(u64, u64)>,
    ) {
        if request == self.request
            && self.projects.get(self.project).map(|p| p.0) == Some(project)
            && self.loading
        {
            self.incoming_parents.extend(parents);
        }
    }

    pub(crate) fn page(
        &mut self,
        request: u64,
        project: ProjectId,
        posts: Vec<InboxPost>,
        status: InboxPageStatus,
        notice: Option<String>,
        out: &mut Vec<u8>,
    ) -> bool {
        if request != self.request
            || self.projects.get(self.project).map(|p| p.0) != Some(project)
            || !self.loading
        {
            return false;
        }
        for post in posts {
            self.incoming.entry(post.thread).or_default().push(post);
        }
        let retained = match status {
            InboxPageStatus::More(cursor) => {
                self.request_page(Some(cursor), out);
                return false;
            }
            InboxPageStatus::Complete { retained } => retained,
        };
        let previous = self.order.get(self.selected).copied();
        self.threads = std::mem::take(&mut self.incoming);
        self.parents = std::mem::take(&mut self.incoming_parents);
        *self.layout.get_mut() = ThreadLayout::default();
        self.notice = notice;
        self.post_threads = self
            .threads
            .values()
            .flatten()
            .map(|post| (post.id, post.thread))
            .collect();
        self.snapshot_valid = self.post_threads.len() == retained;
        if !self.snapshot_valid {
            self.post_threads.clear();
            self.parents.clear();
            self.threads.clear();
            self.order.clear();
            if let Some(id) = previous {
                self.resume_after_erasure = Some((id, self.reading, self.offset));
            }
            self.selected = 0;
            self.reading = false;
            self.offset = 0;
            if self.snapshot_retries < 2 {
                self.snapshot_retries += 1;
                self.notice = Some("Messages were removed while loading. Updating Inbox…".into());
                self.request = next_request();
                self.request_page(None, out);
            } else {
                self.loading = false;
                self.notice =
                    Some("Messages were removed while loading. Refresh to try again.".into());
            }
            return true;
        }
        self.order = self.threads.keys().copied().collect();
        self.order
            .sort_unstable_by_key(|id| std::cmp::Reverse(self.threads[id].last().map(|p| p.id)));
        self.snapshot_retries = 0;
        let previous = if let Some((id, reading, offset)) = self.resume_after_erasure.take() {
            self.reading = reading;
            self.offset = offset;
            Some(id)
        } else {
            previous
        };
        self.restore_selection(previous);
        self.loading = false;
        true
    }

    fn restore_selection(&mut self, previous: Option<u64>) {
        if let Some(index) = self.order.iter().position(|id| Some(*id) == previous) {
            self.selected = index;
            self.history.retain(|(id, _)| self.threads.contains_key(id));
            if let Some((post, target)) = self.link {
                if !self.threads.contains_key(&target)
                    || !self.threads[&self.order[index]]
                        .iter()
                        .any(|entry| entry.id == post)
                {
                    self.link = None;
                }
            }
        } else {
            self.selected = 0;
            self.reading = false;
            self.offset = 0;
            self.history.clear();
            self.link = None;
        }
    }

    pub(crate) fn delta(
        &mut self,
        request: u64,
        project: ProjectId,
        delta: uniterm_proto::InboxDelta,
        notice: Option<String>,
        _out: &mut Vec<u8>,
    ) -> bool {
        if request != self.request
            || self.projects.get(self.project).map(|p| p.0) != Some(project)
            || self.loading
        {
            return false;
        }
        if !self.snapshot_valid {
            return false;
        }
        let selected = self.order.get(self.selected).copied();
        let notice_changed = self.notice != notice;
        self.notice = notice;
        let mut changed = BTreeSet::new();
        let mut linked_target_changed = false;
        for id in delta.removed {
            if let Some(thread) = self.post_threads.remove(&id) {
                self.parents.remove(&id);
                if let Some(posts) = self.threads.get_mut(&thread) {
                    if let Ok(index) = posts.binary_search_by_key(&id, |p| p.id) {
                        posts.remove(index);
                    }
                    if posts.is_empty() {
                        self.threads.remove(&thread);
                    }
                }
                linked_target_changed |= id == thread && self.layout.get_mut().references(thread);
                changed.insert(thread);
            }
        }
        self.parents.extend(delta.parents);
        for post in delta.posts {
            let thread = post.thread;
            let posts = self.threads.entry(thread).or_default();
            match posts.binary_search_by_key(&post.id, |p| p.id) {
                Ok(index) if posts[index] == post => continue,
                Ok(index) => posts[index] = post,
                Err(index) => {
                    linked_target_changed |=
                        post.id == thread && self.layout.get_mut().references(thread);
                    self.post_threads.insert(post.id, thread);
                    posts.insert(index, post);
                }
            }
            changed.insert(thread);
        }
        if changed.is_empty() {
            return notice_changed;
        }
        let selected_changed = selected.is_some_and(|id| changed.contains(&id));
        if selected_changed || linked_target_changed {
            *self.layout.get_mut() = ThreadLayout::default();
        }
        // Only affected roots can move in latest-reply order.
        self.order.retain(|id| !changed.contains(id));
        for id in changed {
            if let Some(latest) = self.threads.get(&id).and_then(|posts| posts.last()) {
                let index = self.order.partition_point(|root| {
                    self.threads[root]
                        .last()
                        .is_some_and(|post| post.id > latest.id)
                });
                self.order.insert(index, id);
            }
        }
        self.restore_selection(selected);
        notice_changed || !self.reading || selected_changed || linked_target_changed
    }

    fn open_thread(&mut self) {
        if self.order.get(self.selected).is_some() {
            self.reading = true;
            self.projects_focused = false;
            self.offset = 0;
            self.link = None;
            *self.layout.get_mut() = ThreadLayout::default();
        }
    }

    fn thread_lines(&self, width: usize) -> std::cell::Ref<'_, ThreadLayout> {
        if self.layout.borrow().width != width {
            let posts = self
                .order
                .get(self.selected)
                .and_then(|id| self.threads.get(id));
            *self.layout.borrow_mut() = ThreadLayout::build(
                width,
                posts.map_or(&[], Vec::as_slice),
                &self.parents,
                &self.threads,
            );
        }
        self.layout.borrow()
    }

    fn select_link(&mut self, forward: bool, g: &Geometry) {
        let (index, offset, link) = {
            let layout = self.thread_lines(g.detail - 4);
            let links = &layout.links;
            if links.is_empty() {
                return;
            }
            // The ordinal is valid between navigations. A resize or a live
            // update can move it, so fall back to the stable post/target pair.
            let current = self.link.and_then(|link| {
                if links
                    .get(self.link_index)
                    .is_some_and(|entry| entry.1 == link)
                {
                    Some(self.link_index)
                } else {
                    links.iter().position(|entry| entry.1 == link)
                }
            });
            let viewport = g.visible - 1;
            let offset = self.offset.min(layout.rows.saturating_sub(viewport));
            let index = match (current, forward) {
                (Some(i), true) => (i + 1) % links.len(),
                (Some(i), false) => i.checked_sub(1).unwrap_or(links.len() - 1),
                (None, true) => links.partition_point(|(row, _)| *row < offset) % links.len(),
                (None, false) => links
                    .partition_point(|(row, _)| *row < offset + viewport)
                    .checked_sub(1)
                    .unwrap_or(links.len() - 1),
            };
            let offset = links[index]
                .0
                .saturating_sub(viewport / 2)
                .min(layout.rows.saturating_sub(viewport));
            (index, offset, links[index].1)
        };
        self.link_index = index;
        self.link = Some(link);
        self.offset = offset;
    }

    fn follow(&mut self, id: u64) {
        if let Some(i) = self.order.iter().position(|root| *root == id) {
            if i == self.selected {
                return;
            }
            let current = self.order[self.selected];
            if let Some(previous) = self.history.iter().position(|(root, _)| *root == current) {
                self.history.truncate(previous);
            }
            self.history.push((current, self.offset));
            self.selected = i;
            self.open_thread();
        }
    }

    fn back(&mut self) {
        if let Some((id, offset)) = self.history.pop() {
            if let Some(i) = self.order.iter().position(|root| *root == id) {
                self.selected = i;
                self.open_thread();
                self.offset = offset;
                return;
            }
        }
        self.reading = false;
        self.projects_focused = false;
        self.link = None;
    }

    fn navigate(&mut self, down: bool, amount: usize, g: &Geometry, out: &mut Vec<u8>) {
        if self.reading && !self.projects_focused && self.offset == usize::MAX && down {
            return;
        }
        let (value, max) = if self.projects_focused {
            (self.project, self.projects.len().saturating_sub(1))
        } else if self.reading {
            (
                self.offset,
                self.thread_lines(g.detail - 4)
                    .rows
                    .saturating_sub(g.visible - 1),
            )
        } else {
            (self.selected, self.order.len().saturating_sub(1))
        };
        let value = value.min(max);
        let next = if down {
            value.saturating_add(amount).min(max)
        } else {
            value.saturating_sub(amount)
        };
        if self.projects_focused && self.project != next {
            self.project = next;
            self.select_project(out);
        } else if !self.projects_focused && self.reading {
            if self.offset != next {
                self.link = None;
            }
            self.offset = next;
        } else if !self.projects_focused {
            self.selected = next;
        }
    }

    // True dismisses the overlay and lets the attach loop restore the pane.
    pub(crate) fn key(&mut self, chunk: &[u8], cols: u16, rows: u16, out: &mut Vec<u8>) -> bool {
        let g = Geometry::new(cols, rows);
        let mut at = 0;
        while at < chunk.len() {
            let page = chunk[at..].starts_with(b"\x1b[5~") || chunk[at..].starts_with(b"\x1b[6~");
            if page {
                self.navigate(
                    chunk[at + 2] == b'6',
                    if self.reading { g.visible - 1 } else { g.cards },
                    &g,
                    out,
                );
                at += 4;
                continue;
            }
            let (key, used) = decode_key(chunk, at);
            at += used.max(1);
            match key {
                LineKey::Escape if cols < 20 || rows < 9 => return true,
                LineKey::Left if self.projects_focused => (),
                LineKey::Escape | LineKey::Left if self.reading => {
                    self.back();
                }
                LineKey::Escape | LineKey::Cancel | LineKey::Char('q') => return true,
                LineKey::Left => self.projects_focused = true,
                LineKey::Tab | LineKey::BackTab => self.projects_focused = !self.projects_focused,
                LineKey::Right | LineKey::Enter if self.projects_focused => {
                    self.projects_focused = false
                }
                LineKey::Right | LineKey::Enter if !self.reading => self.open_thread(),
                LineKey::Char(']') | LineKey::Char('[')
                    if self.reading && !self.projects_focused =>
                {
                    self.select_link(matches!(key, LineKey::Char(']')), &g);
                }
                LineKey::Enter if self.reading => {
                    if let Some((_, id)) = self.link {
                        self.follow(id);
                    }
                }
                LineKey::Up | LineKey::Char('k') => self.navigate(false, 1, &g, out),
                LineKey::Down | LineKey::Char('j') => self.navigate(true, 1, &g, out),
                LineKey::Home => self.navigate(false, usize::MAX, &g, out),
                LineKey::End if self.reading && !self.projects_focused => {
                    // Resolve against the current layout so new replies and resizes
                    // keep the latest row visible until the reader navigates away.
                    self.offset = usize::MAX;
                    self.link = None;
                }
                LineKey::End => self.navigate(true, usize::MAX, &g, out),
                LineKey::Char('r') => self.refresh(out),
                _ => {}
            }
        }
        false
    }

    fn footer(&self, g: &Geometry) -> [(&'static str, &'static str); 4] {
        [
            (
                "Tab",
                if self.projects_focused {
                    "Threads"
                } else {
                    "Projects"
                },
            ),
            if self.projects_focused {
                ("Enter", if self.reading { "Thread" } else { "Feed" })
            } else if self.reading {
                if self.link.is_some() {
                    ("Enter", "Open link")
                } else if !self.thread_lines(g.detail - 4).links.is_empty() {
                    ("]", "Next link")
                } else {
                    ("PgDn", "Scroll")
                }
            } else {
                ("Enter", "Read")
            },
            ("r", "Refresh"),
            ("Esc", if self.reading { "Back" } else { "Close" }),
        ]
    }

    pub(crate) fn pointer(
        &mut self,
        size: (u16, u16),
        x: u16,
        y: u16,
        wheel: Option<bool>,
        out: &mut Vec<u8>,
    ) -> bool {
        let (cols, rows) = size;
        if cols < 20 || rows < 9 {
            return wheel.is_none();
        }
        let g = Geometry::new(cols, rows);
        let r = g.rect;
        let hit = modal_hit(r, g.rail as u16 + 1, x, y);
        if let Some(down) = wheel {
            if matches!(hit, ModalHit::Outside | ModalHit::Bar(_)) {
                return false;
            }
            self.projects_focused = matches!(hit, ModalHit::ListRow(_));
            self.navigate(down, 1, &g, out);
            return false;
        }
        match hit {
            ModalHit::Outside => return true,
            ModalHit::ListRow(slot) => {
                if let Some(idx) = slot
                    .checked_sub(1)
                    .map(|s| s + first(self.project, g.visible - 1))
                    .filter(|&i| i < self.projects.len())
                {
                    self.projects_focused = true;
                    if idx != self.project {
                        self.project = idx;
                        self.select_project(out);
                    }
                }
            }
            ModalHit::Bar(col) => {
                let actions = self.footer(&g);
                let visible = fitted_footer(&actions, usize::from(r.w - 2));
                if let Some((_, index)) = footer_spans(visible)
                    .into_iter()
                    .find(|(span, _)| span.contains(&col))
                    .filter(|_| x > r.x && x < r.x + r.w - 1)
                {
                    return self.key(
                        match visible[index].0 {
                            "Tab" => b"\t",
                            "Enter" => b"\r",
                            "]" => b"]",
                            "PgDn" => b"\x1b[6~",
                            "r" => b"r",
                            "Esc" => b"\x1b",
                            _ => return false,
                        },
                        cols,
                        rows,
                        out,
                    );
                }
            }
            ModalHit::None
                if x > r.x + g.rail as u16 + 1
                    && x < r.x + r.w - 1
                    && y > r.y + 1
                    && y < r.y + r.h - 3 =>
            {
                self.projects_focused = false;
                if self.reading {
                    let link = {
                        let layout = self.thread_lines(g.detail - 4);
                        let from = self.offset.min(layout.rows.saturating_sub(g.visible - 1));
                        layout.link_at(from + usize::from(y - r.y - 2))
                    };
                    if let Some((_, id)) = link {
                        self.follow(id);
                    }
                } else {
                    let slot = usize::from(y - r.y - 2) / CARD_ROWS;
                    let idx = first(self.selected, g.cards) + slot;
                    if slot < g.cards && idx < self.order.len() {
                        self.selected = idx;
                        self.open_thread();
                    }
                }
            }
            _ => {}
        }
        false
    }

    pub(crate) fn render(&self, cols: u16, rows: u16) -> Vec<u8> {
        if cols < 20 || rows < 9 {
            return format!(
                "\x1b[?25l\x1b[H{}",
                clip_text("Close Inbox (Esc)", cols.into())
            )
            .into_bytes();
        }
        let g = Geometry::new(cols, rows);
        let panel = panel_style();
        let theme = ui_theme();
        let accent = format!("\x1b[1;{}m", theme.accent.sgr_fg());
        let muted = format!(
            "\x1b[0;{};{}m",
            theme.muted.sgr_fg(),
            theme.surface.sgr_bg()
        );
        let selected = format!(
            "\x1b[1;{};{}m",
            theme.status_active_bg.sgr_bg(),
            theme.status_active_fg.sgr_fg()
        );
        let mut rows_out = Vec::new();
        let viewport = g.visible - 1;
        let layout = self.reading.then(|| self.thread_lines(g.detail - 4));
        let from = layout
            .as_ref()
            .map_or(0, |l| self.offset.min(l.rows.saturating_sub(viewport)));
        let scroll = layout
            .as_ref()
            .filter(|l| l.rows > viewport)
            .map(|l| {
                let end = from + viewport;
                let direction = if from == 0 {
                    '↓'
                } else if end == l.rows {
                    '↑'
                } else {
                    '↕'
                };
                if g.detail >= 45 {
                    format!(" {direction} {}-{end}/{}", from + 1, l.rows)
                } else {
                    format!(" {direction}")
                }
            })
            .unwrap_or_default();
        let heading_width = g.detail - scroll.width();
        let name = self
            .projects
            .get(self.project)
            .map(|p| p.1.as_str())
            .unwrap_or("");
        let heading = if self.reading {
            let thread = format!("thread:{}", self.order[self.selected]);
            let name_width = heading_width.saturating_sub(thread.width() + 6);
            if name_width < 3 {
                elide(&format!(" {thread}"), heading_width)
            } else {
                format!(" # {} / {thread}", elide(name, name_width))
            }
        } else {
            format!(" # {name}   {}", count(self.order.len(), "thread"))
        };
        rows_out.push(
            if let Some(notice) = self.notice.as_ref().filter(|_| !self.order.is_empty()) {
                styled_line(&[(&muted, " "), ("", notice)])
            } else if self.loading && !self.order.is_empty() {
                styled_line(&[(&muted, " Refreshing…"), (&accent, &heading)])
            } else {
                let mut line = styled_line(&[(&accent, &heading)]);
                if !scroll.is_empty() {
                    line.0.push_str(&" ".repeat(heading_width - line.1));
                    line.0.push_str(&muted);
                    line.0.push_str(&scroll);
                    line.1 = g.detail;
                }
                line
            },
        );
        if self.order.is_empty() {
            let status = if let Some(notice) = &self.notice {
                notice.as_str()
            } else if self.loading {
                " Loading inbox..."
            } else if self.projects.is_empty() {
                " No Projects in this Workspace."
            } else {
                " No messages in this Project."
            };
            rows_out.extend(
                crate::overlay::wrap_text(status, g.detail - 2)
                    .into_iter()
                    .map(|s| styled_line(&[(&muted, &format!(" {s}"))])),
            );
        } else if let Some(layout) = layout {
            for line in layout.visible(from, viewport) {
                let link = line.link();
                let edge = if link.is_some() && link == self.link {
                    &selected
                } else if link.is_some() {
                    &accent
                } else {
                    &muted
                };
                rows_out.push(
                    line.render(
                        g.detail,
                        edge,
                        &panel,
                        &muted,
                        link.and_then(|(_, id)| self.threads.get(&id)?.first())
                            .map(subject),
                        link.is_some() && link == self.link,
                    ),
                );
            }
        } else {
            let from = first(self.selected, g.cards);
            for (i, thread) in self.order.iter().enumerate().skip(from).take(g.cards) {
                let posts = &self.threads[thread];
                let post = &posts[0];
                let focused = i == self.selected && !self.projects_focused;
                let edge = if focused { &accent } else { &muted };
                let title = if focused {
                    format!("▸ {}", subject(post))
                } else {
                    subject(post).to_owned()
                };
                rows_out.push(card_border(g.detail, &title, true, edge));
                let replies = posts.len() - usize::from(post.id == *thread);
                let mut byline = format!(
                    "{} · {}",
                    count(replies, "reply"),
                    author_label(post.author)
                );
                if let Some(last) = posts.last().filter(|last| last.author != post.author) {
                    byline.push_str(&format!(" → {}", author_label(last.author)));
                }
                rows_out.push(
                    CardLine::Note(byline).render(g.detail, edge, &panel, &muted, None, false),
                );
                let reference = format!("thread:{thread} · ");
                let claim = if post.claimed_by.is_some() {
                    " · Claimed"
                } else {
                    ""
                };
                let budget =
                    card_label_width(g.detail).saturating_sub(reference.width() + claim.width());
                let label = format!("{reference}{}{claim}", metadata(post, budget));
                rows_out.push(card_border(g.detail, &label, false, &muted));
            }
        }
        let detail = finish_lines(rows_out, &panel, g.detail, g.visible);
        render_list_modal_content(
            (cols, rows),
            g.rect,
            (" Inbox ", None),
            g.rail,
            |slot| {
                let (style, label) = if slot == 0 {
                    (&muted, " Projects".to_string())
                } else {
                    let i = first(self.project, g.visible - 1) + slot - 1;
                    let (_, name) = self.projects.get(i)?;
                    (
                        if i == self.project {
                            if self.projects_focused {
                                &selected
                            } else {
                                &accent
                            }
                        } else {
                            &panel
                        },
                        if self.projects_focused && i == self.project {
                            format!(" ▸ {name}")
                        } else if i == self.project {
                            format!(" # {name}")
                        } else {
                            format!("   {name}")
                        },
                    )
                };
                let safe = elide(&label, g.rail);
                Some(format!(
                    "{style}{safe}{}{panel}",
                    " ".repeat(g.rail - safe.width())
                ))
            },
            &detail,
            &footer_text(
                fitted_footer(&self.footer(&g), usize::from(g.rect.w - 2)),
                usize::from(g.rect.w - 2),
            ),
        )
    }
}

// A compact screen still exposes the complete Back/Close button. Include
// earlier shortcuts as space permits; the rail and cards remain direct targets.
fn fitted_footer<'a>(actions: &'a [(&str, &str)], width: usize) -> &'a [(&'a str, &'a str)] {
    for i in 0..actions.len() {
        if footer_width(&actions[i..]) <= width {
            return &actions[i..];
        }
    }
    &[]
}

fn first(selected: usize, visible: usize) -> usize {
    (selected + 1).saturating_sub(visible)
}
fn count(n: usize, noun: &str) -> String {
    match (n, noun) {
        (1, _) => format!("1 {noun}"),
        (_, "reply") => format!("{n} replies"),
        _ => format!("{n} {noun}s"),
    }
}

fn subject(post: &InboxPost) -> &str {
    post.body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

fn author_label(author: InboxAuthor) -> String {
    match author {
        InboxAuthor::Human => "Human".into(),
        InboxAuthor::Agent { pane, invocation } => format!("Agent {}.{}", pane.0, invocation),
    }
}

fn metadata(post: &InboxPost, available: usize) -> String {
    let kind = format!("{:?}", post.kind);
    let room = available.saturating_sub(kind.width() + " · ".width());
    if room < 6 {
        return kind;
    }
    let Ok(seconds) = libc::time_t::try_from(post.created_ms / 1000) else {
        return kind;
    };
    let mut date = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: gmtime_r initializes the supplied tm on success and retains neither pointer.
    let t = unsafe {
        if libc::gmtime_r(&seconds, date.as_mut_ptr()).is_null() {
            return kind;
        }
        date.assume_init()
    };
    let year = i64::from(t.tm_year) + 1900;
    let (month, day, hour, min) = (t.tm_mon + 1, t.tm_mday, t.tm_hour, t.tm_min);
    if room >= 20 && (0..=9999).contains(&year) {
        format!("{kind} · {year:04}-{month:02}-{day:02} {hour:02}:{min:02} UTC")
    } else if room >= 12 {
        format!("{kind} · {month:02}-{day:02} {hour:02}:{min:02}Z")
    } else {
        format!("{kind} · {hour:02}:{min:02}Z")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniterm_core::{inbox::Kind, PaneId};

    fn post(id: u64, thread: u64, body: &str) -> InboxPost {
        InboxPost {
            id,
            thread,
            body: body.into(),
            kind: Kind::Proposal,
            created_ms: 61_200_000,
            author: InboxAuthor::Agent {
                pane: PaneId(2),
                invocation: 1,
            },
            to: None,
            task: None,
            claimed_by: None,
        }
    }

    fn board(out: &mut Vec<u8>) -> InboxView {
        let mut view = InboxView::open(out);
        assert!(view.channels(
            view.request,
            ProjectId(7),
            vec![
                (ProjectId(3), "Other".into()),
                (ProjectId(7), "構築".into())
            ],
            out
        ));
        view
    }

    #[test]
    fn focused_link_uses_panel_borders_and_project_updates_preserve_the_reader() {
        let link = CardLine::Link { post: 3, target: 7 };
        let edge = "\x1b[1;44;37m";
        let muted = "\x1b[0;90;40m";
        let (line, cells) = link.render(20, edge, "", muted, Some("X"), true);
        assert_eq!(cells, 20);
        assert_eq!(line, format!("{muted}│{edge} ▸ thread:7 · X   {muted}│"));
        let mut out = Vec::new();
        let mut view = board(&mut out);
        let project = ProjectId(7);
        view.page(
            view.request,
            project,
            vec![post(1, 1, "root")],
            InboxPageStatus::Complete { retained: 1 },
            None,
            &mut out,
        );
        view.open_thread();
        view.offset = 2;
        let request = view.request;
        out.clear();
        assert!(view.channels(
            uniterm_proto::INBOX_LIVE_REQUEST,
            ProjectId(3),
            vec![(project, "Renamed".into()), (ProjectId(3), "Other".into())],
            &mut out
        ));
        assert!(view.reading && out.is_empty());
        assert_eq!((view.project, view.request, view.offset), (0, request, 2));
        view.key(b"\t", 100, 24, &mut out);
        view.key(b"\x1b[D", 100, 24, &mut out);
        assert!(view.projects_focused && view.reading);
        let screen = String::from_utf8(view.render(100, 24)).unwrap();
        assert!(screen.contains("▸ Renamed") && screen.contains("   Other"));
    }

    #[test]
    fn card_footer_fits_complete_utc_timestamps_at_each_width() {
        let posts = [post(1, 1, "Test")];
        for (available, expected) in [
            (31, "Proposal · 1970-01-01 17:00 UTC"),
            (30, "Proposal · 01-01 17:00Z"),
            (23, "Proposal · 01-01 17:00Z"),
            (22, "Proposal · 17:00Z"),
            (17, "Proposal · 17:00Z"),
            (16, "Proposal"),
        ] {
            let layout =
                ThreadLayout::build(available + 1, &posts, &BTreeMap::new(), &BTreeMap::new());
            let footer = layout.visible(layout.rows - 1, 1).next().unwrap();
            let (line, cells) = footer.render(available + 5, "", "", "", None, false);
            assert_eq!(cells, available + 5);
            assert_eq!(
                line,
                format!(
                    "╰─ {expected} {}╯",
                    "─".repeat(available - expected.width())
                )
            );
        }
    }

    #[test]
    fn clipped_card_labels_keep_whole_graphemes_and_signal_hidden_text() {
        for (label, expected) in [
            ("abc界", "ab…"),
            ("e\u{301}ab界", "e\u{301}a…"),
            ("界a", "界a"),
            ("a\x07b", "a b"),
        ] {
            let (line, cells) = card_border(8, label, true, "");
            assert_eq!(cells, 8);
            assert_eq!(line, format!("╭─ {expected} ╮"));
        }
    }

    #[test]
    fn thread_scroll_hint_tracks_the_viewport_and_clipped_project_names() {
        for cols in [20, 48, 120] {
            let rows = 21;
            let mut out = Vec::new();
            let mut v = board(&mut out);
            let name = "Release engineering and build infrastructure";
            v.projects[v.project].1 = name.into();
            let body = "The release coordinator reviewed the updated instructions.\n".repeat(40);
            v.page(
                v.request,
                ProjectId(7),
                vec![post(1, 1, &body)],
                InboxPageStatus::Complete { retained: 1 },
                None,
                &mut out,
            );
            let g = Geometry::new(cols, rows);
            let project_label = elide(&format!(" # {name}"), g.rail);
            assert!(project_label.ends_with('…'));
            let render = |view: &InboxView| String::from_utf8(view.render(cols, rows)).unwrap();
            assert!(render(&v).contains(&project_label));
            v.key(b"\r", cols, rows, &mut out);
            let total = v.thread_lines(g.detail - 4).rows;
            let viewport = g.visible - 1;
            for (key, arrow, start) in [
                (&b""[..], '↓', 1),
                (&b"j"[..], '↕', 2),
                (&b"\x1b[F"[..], '↑', total - viewport + 1),
            ] {
                v.key(key, cols, rows, &mut out);
                let screen = render(&v);
                if cols == 120 {
                    assert!(screen
                        .contains(&format!("{arrow} {start}-{}/{total}", start + viewport - 1)));
                } else {
                    assert!(screen.contains(&format!(" {arrow}")));
                }
                assert!(screen.contains(&project_label));
            }
        }
    }

    #[test]
    fn inbox_row_updates_keep_unrelated_threads_quiet_and_remove_retired_bodies() {
        use uniterm_proto::InboxDelta;
        let mut out = Vec::new();
        let mut v = board(&mut out);
        let project = ProjectId(7);
        let request = v.request;
        assert_ne!(request & uniterm_proto::INBOX_LIVE_REQUEST, 0);
        v.page(
            request,
            project,
            vec![post(1, 1, "first"), post(2, 2, "second")],
            InboxPageStatus::Complete { retained: 2 },
            None,
            &mut out,
        );
        v.open_thread();
        assert_eq!(v.order[v.selected], 2);
        let update = InboxDelta {
            posts: vec![post(3, 1, "private comment")],
            parents: vec![(3, 1)],
            removed: vec![],
        };
        assert!(!v.delta(request, ProjectId(9), update.clone(), None, &mut out));
        assert!(!v.delta(request - 1, project, update.clone(), None, &mut out));
        assert!(!v.delta(request, project, update.clone(), None, &mut out));
        assert_eq!(v.order, [1, 2]);
        assert_eq!(v.order[v.selected], 2);
        assert_eq!(v.parents[&3], 1);
        assert!(!v.delta(request, project, update, None, &mut out));
        let mut changed = post(2, 2, "second");
        changed.claimed_by = Some(InboxAuthor::Human);
        let claim = InboxDelta {
            posts: vec![changed],
            ..Default::default()
        };
        assert!(v.delta(request, project, claim.clone(), None, &mut out));
        assert!(!v.delta(request, project, claim, None, &mut out));
        assert!(!v.delta(
            request,
            project,
            InboxDelta {
                removed: vec![1, 3],
                ..Default::default()
            },
            None,
            &mut out
        ));
        assert_eq!(v.order, [2]);
        assert!(!v.parents.contains_key(&3));
        assert!(!v.threads.contains_key(&1));
        assert!(v.delta(
            request,
            project,
            InboxDelta {
                removed: vec![2],
                ..Default::default()
            },
            None,
            &mut out
        ));
        assert!(!v.reading);
        assert!(v.threads.is_empty());
    }

    #[test]
    fn expiry_during_snapshot_reconciles_erased_bodies_and_restores_thread_position() {
        let mut out = Vec::new();
        let mut v = board(&mut out);
        let project = ProjectId(7);
        v.page(
            v.request,
            project,
            vec![
                post(1, 1, "retired body"),
                post(2, 2, "prior"),
                post(3, 3, "selected"),
            ],
            InboxPageStatus::Complete { retained: 3 },
            None,
            &mut out,
        );
        v.open_thread();
        v.history.push((2, 8));
        v.offset = 3;
        v.refresh(&mut out);
        v.channels(v.request, project, v.projects.clone(), &mut out);
        let old = v.request;
        v.page(
            old,
            project,
            vec![post(1, 1, "retired body")],
            InboxPageStatus::More(InboxReadCursor {
                after: 1,
                through: 3,
            }),
            None,
            &mut out,
        );
        out.clear();
        v.page(
            old,
            project,
            vec![post(2, 2, "prior"), post(3, 3, "selected")],
            InboxPageStatus::Complete { retained: 2 },
            None,
            &mut out,
        );
        assert!(v.threads.is_empty());
        assert!(!String::from_utf8(v.render(80, 24))
            .unwrap()
            .contains("retired body"));
        assert_ne!(v.request, old);
        assert_eq!(
            out,
            encode_frame(&ClientMessage::InboxRead {
                request: v.request,
                project,
                cursor: None
            })
        );
        v.page(
            v.request,
            project,
            vec![post(2, 2, "prior"), post(3, 3, "selected")],
            InboxPageStatus::Complete { retained: 2 },
            None,
            &mut out,
        );
        assert!(v.reading);
        assert_eq!(v.order[v.selected], 3);
        assert_eq!(v.offset, 3);
        assert_eq!(v.history, [(2, 8)]);
        // A continuously changing board stops after two retries and waits for refresh.
        v.refresh(&mut out);
        v.channels(v.request, project, v.projects.clone(), &mut out);
        for _ in 0..3 {
            out.clear();
            v.page(
                v.request,
                project,
                vec![post(9, 9, "expired")],
                InboxPageStatus::Complete { retained: 0 },
                None,
                &mut out,
            );
        }
        assert!(out.is_empty() && !v.loading && v.threads.is_empty());
        assert!(v.notice.as_ref().unwrap().contains("Refresh"));
    }

    #[test]
    fn scoped_pages_and_switching_never_reopen_a_stale_thread() {
        let mut out = vec![];
        let mut v = board(&mut out);
        let first = v.request;
        out.clear();
        assert!(!v.page(
            first,
            ProjectId(7),
            vec![post(1, 1, "first")],
            InboxPageStatus::More(InboxReadCursor {
                after: 1,
                through: 3
            }),
            None,
            &mut out
        ));
        assert_eq!(
            out,
            encode_frame(&ClientMessage::InboxRead {
                request: first,
                project: ProjectId(7),
                cursor: Some(InboxReadCursor {
                    after: 1,
                    through: 3
                })
            })
        );
        assert!(v.page(
            first,
            ProjectId(7),
            vec![post(2, 2, "second"), post(3, 1, "reply")],
            InboxPageStatus::Complete { retained: 3 },
            None,
            &mut out
        ));
        assert_eq!(v.order, [1, 2]);
        v.key(b"\t\x1b[A", 80, 24, &mut out);
        assert_eq!(v.project, 0);
        assert!(!v.page(
            first,
            ProjectId(7),
            vec![post(4, 4, "late")],
            InboxPageStatus::Complete { retained: 1 },
            None,
            &mut out
        ));
        assert!(v.order.is_empty());
        assert!(v.page(
            v.request,
            ProjectId(3),
            vec![],
            InboxPageStatus::Complete { retained: 0 },
            None,
            &mut out
        ));
        assert!(String::from_utf8(v.render(80, 24))
            .unwrap()
            .contains("No messages"));
    }

    #[test]
    fn removing_a_project_discards_partial_pages_before_they_can_be_displayed() {
        let mut out = Vec::new();
        let mut v = board(&mut out);
        let request = v.request;
        let project = ProjectId(7);
        let posts = (1..=20).map(|id| post(id, id, "removed body")).collect();
        assert!(!v.page(
            request,
            project,
            posts,
            InboxPageStatus::More(InboxReadCursor {
                after: 20,
                through: 21
            }),
            None,
            &mut out
        ));
        assert!(!v.project_removed(ProjectId(u64::MAX), &mut out));
        assert!(v.project_removed(project, &mut out));
        assert!(v.threads.is_empty());
        assert!(!v.page(
            request,
            project,
            vec![post(21, 21, "late")],
            InboxPageStatus::Complete { retained: 1 },
            None,
            &mut out
        ));
        assert!(!v.project_removed(project, &mut out));
        assert_eq!(v.projects[v.project].0, ProjectId(3));
        assert!(v.page(
            v.request,
            ProjectId(3),
            vec![],
            InboxPageStatus::Complete { retained: 0 },
            None,
            &mut out
        ));
        let screen = String::from_utf8(v.render(80, 24)).unwrap();
        assert!(screen.contains("No messages"));
        assert!(!screen.contains("removed body"));
    }

    #[test]
    fn clear_during_pagination_discards_earlier_bodies() {
        let mut out = Vec::new();
        let mut v = board(&mut out);
        let request = v.request;
        assert!(!v.page(
            request,
            ProjectId(7),
            vec![post(1, 1, "cleared body")],
            InboxPageStatus::More(InboxReadCursor {
                after: 1,
                through: 2
            }),
            None,
            &mut out
        ));
        assert!(v.page(
            request,
            ProjectId(7),
            vec![],
            InboxPageStatus::Complete { retained: 0 },
            None,
            &mut out
        ));
        assert!(v.threads.is_empty());
        let screen = String::from_utf8(v.render(80, 24)).unwrap();
        assert!(screen.contains("Messages were removed"));
        assert!(!screen.contains("cleared body"));
    }

    #[test]
    fn compact_footers_keep_a_complete_back_and_close_target() {
        for (cols, rows) in [(20, 9), (23, 10), (40, 10), (80, 24)] {
            let mut out = Vec::new();
            let mut v = board(&mut out);
            v.page(
                v.request,
                ProjectId(7),
                vec![post(1, 1, "body")],
                InboxPageStatus::Complete { retained: 1 },
                None,
                &mut out,
            );
            v.key(b"\r", cols, rows, &mut out);
            let rect = Geometry::new(cols, rows).rect;
            for caption in ["Back", "Close"] {
                let actions = v.footer(&Geometry::new(cols, rows));
                let width = usize::from(rect.w - 2);
                let visible = fitted_footer(&actions, width);
                assert!(footer_width(visible) <= width);
                assert_eq!(visible.last(), Some(&("Esc", caption)));
                let screen = v.render(cols, rows);
                let footer = footer_text(visible, width);
                assert!(screen.windows(footer.len()).any(|s| s == footer.as_bytes()));
                let (span, _) = footer_spans(visible).pop().unwrap();
                let clicked = v.pointer(
                    (cols, rows),
                    rect.x + span.end as u16,
                    rect.y + rect.h - 2,
                    None,
                    &mut out,
                );
                assert_eq!(clicked, caption == "Close");
                assert!(!v.reading);
            }
        }
        let mut out = Vec::new();
        let mut v = board(&mut out);
        assert!(String::from_utf8(v.render(16, 8))
            .unwrap()
            .contains("Close Inbox"));
        assert!(v.pointer((16, 8), 1, 1, None, &mut out));
    }

    #[test]
    fn navigation_skips_self_and_unavailable_targets_and_shows_reply_parent() {
        let mut out = Vec::new();
        let mut view = board(&mut out);
        let posts = vec![
            post(1, 1, "First topic"),
            post(2, 2, "Second topic"),
            post(3, 3, "Third topic"),
            post(4, 1, "thread:2"),
            post(5, 1, &format!("{}thread:3", "context\n".repeat(40))),
            post(6, 1, "thread:1 thread:42"),
        ];
        view.page_parents(view.request, ProjectId(7), vec![(4, 1), (5, 4), (6, 5)]);
        let retained = posts.len();
        view.page(
            view.request,
            ProjectId(7),
            posts,
            InboxPageStatus::Complete { retained },
            None,
            &mut out,
        );
        view.open_thread();
        let g = Geometry::new(100, 24);
        let second_row = {
            let layout = view.thread_lines(g.detail - 4);
            assert_eq!(
                layout.links.iter().map(|entry| entry.1).collect::<Vec<_>>(),
                [(4, 2), (5, 3)]
            );
            assert!(layout.references(42));
            assert!(!layout.references(1));
            assert!(layout.visible(0, layout.rows).any(|line|
                matches!(line.as_ref(), CardLine::Top(title) if title.contains("#5 ·") && title.contains("reply to #4"))));
            assert!(layout.visible(0, layout.rows).any(|line|
                matches!(line.as_ref(), CardLine::Note(text) if text == "thread:1 · this discussion")));
            layout.links[1].0
        };
        view.offset = second_row - 1;
        view.select_link(true, &g);
        assert_eq!(view.link, Some((5, 3)));
        view.select_link(true, &g);
        assert_eq!(view.link, Some((4, 2)));
        view.select_link(false, &g);
        assert_eq!(view.link, Some((5, 3)));
    }

    #[test]
    #[ignore = "measures the layout and warm navigation of a full thread"]
    fn full_thread_layout_and_navigation_budget() {
        let mut out = Vec::new();
        let mut view = board(&mut out);
        let body = format!("{}thread:101", "a\n".repeat(4090));
        let mut posts: Vec<_> = (1..=100).map(|id| post(id, 1, &body)).collect();
        posts.push(post(101, 101, "Referenced conversation"));
        view.page(
            view.request,
            ProjectId(7),
            posts,
            InboxPageStatus::Complete { retained: 101 },
            None,
            &mut out,
        );
        view.selected = view.order.iter().position(|id| *id == 1).unwrap();
        view.open_thread();
        let g = Geometry::new(80, 24);
        let start = std::time::Instant::now();
        std::hint::black_box(view.thread_lines(g.detail - 4));
        let cold = start.elapsed();
        let start = std::time::Instant::now();
        for _ in 0..100 {
            view.select_link(true, &g);
            std::hint::black_box(view.footer(&g));
        }
        eprintln!(
            "full 100-post alternating-line Inbox: layout {cold:?}, 100 link/footers {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn compact_feed_marks_selection_and_end_follows_new_replies() {
        let mut out = Vec::new();
        let mut view = board(&mut out);
        view.page(
            view.request,
            ProjectId(7),
            vec![
                post(1, 1, "First subject"),
                post(
                    2,
                    2,
                    "A second subject that will be clipped at narrow widths",
                ),
            ],
            InboxPageStatus::Complete { retained: 2 },
            None,
            &mut out,
        );
        let (cols, rows) = (48, 20);
        let feed = String::from_utf8(view.render(cols, rows)).unwrap();
        assert!(feed.contains("▸ A second") && feed.contains("…"));
        assert!(feed.contains("thread:2 · Proposal · 17:00Z "));
        let wide = String::from_utf8(view.render(128, rows)).unwrap();
        assert!(wide.contains("thread:2 · Proposal · 1970-01-01 17:00 UTC "));
        let g = Geometry::new(cols, rows);
        assert!(g.cards >= 2);
        view.pointer(
            (cols, rows),
            g.rect.x + g.rail as u16 + 3,
            g.rect.y + 2 + CARD_ROWS as u16,
            None,
            &mut out,
        );
        assert!(view.reading);
        assert_eq!(view.order[view.selected], 1);
        view.key(b"\x1b[F", cols, rows, &mut out);
        assert_eq!(view.offset, usize::MAX);
        let tail = view.position();
        view.key(b"j\x1b[6~", cols, rows, &mut out);
        assert_eq!(view.position(), tail);
        view.channels(view.request, ProjectId(7), view.projects.clone(), &mut out);
        let mut posts = vec![post(1, 1, "First subject")];
        posts.extend((3..=12).map(|id| post(id, 1, &format!("Reply {id}"))));
        view.page(
            view.request,
            ProjectId(7),
            posts,
            InboxPageStatus::Complete { retained: 11 },
            None,
            &mut out,
        );
        assert!(String::from_utf8(view.render(cols, rows))
            .unwrap()
            .contains("Reply 12"));
        view.key(b"k", cols, rows, &mut out);
        assert_ne!(view.offset, usize::MAX);
    }

    #[test]
    fn linked_message_cards_support_keyboard_pointer_and_back() {
        for (cols, rows) in [(48, 19), (128, 40)] {
            let mut out = Vec::new();
            let mut v = board(&mut out);
            v.page(
                v.request,
                ProjectId(7),
                vec![
                    post(1, 1, "Build evidence"),
                    post(2, 2, "Design review"),
                    post(3, 2, "See thread:1 and thread:1"),
                    post(4, 2, "Another reply"),
                ],
                InboxPageStatus::Complete { retained: 4 },
                None,
                &mut out,
            );
            let g = Geometry::new(cols, rows);
            let feed = String::from_utf8(v.render(cols, rows)).unwrap();
            for token in ["╭", "╮", "╰", "╯", "Design review", "2 replies"] {
                assert!(feed.contains(token));
            }
            assert!(!feed.contains("Another reply"));
            v.key(b"\r]", cols, rows, &mut out);
            assert_eq!(v.link, Some((3, 1)));
            let offset = v.offset;
            let row = {
                let layout = v.thread_lines(g.detail - 4);
                assert_eq!(layout.links.len(), 1);
                let link = layout.links[0].0;
                link - offset.min(layout.rows.saturating_sub(g.visible - 1))
            };
            v.key(b"\r", cols, rows, &mut out);
            assert_eq!(v.order[v.selected], 1);
            assert!(String::from_utf8(v.render(cols, rows))
                .unwrap()
                .contains("Build evidence"));
            v.key(b"\x1b", cols, rows, &mut out);
            assert_eq!(v.order[v.selected], 2);
            assert_eq!(v.offset, offset);
            v.pointer(
                (cols, rows),
                g.rect.x + g.rail as u16 + 4,
                g.rect.y + 2 + row as u16,
                None,
                &mut out,
            );
            assert_eq!(v.order[v.selected], 1);
            assert!(v.reading);
        }
    }

    #[test]
    fn thread_reading_preserves_text_and_shares_keyboard_and_mouse_geometry() {
        for (cols, rows) in [(48, 21), (60, 19), (128, 40)] {
            let mut out = vec![];
            let mut v = board(&mut out);
            v.page(
                v.request,
                ProjectId(7),
                vec![
                    post(1, 1, "hello\n   indented 界 value\x1b]52;x\x07"),
                    post(3, 1, "second line"),
                ],
                InboxPageStatus::Complete { retained: 2 },
                None,
                &mut out,
            );
            let feed = v.render(cols, rows);
            assert_eq!(feed, v.render(cols, rows));
            let screen = String::from_utf8(feed).unwrap();
            assert!(screen.contains("1 reply"));
            let g = Geometry::new(cols, rows);
            let r = g.rect;
            v.pointer(
                (cols, rows),
                r.x + g.rail as u16 + 3,
                r.y + 2,
                None,
                &mut out,
            );
            assert!(v.reading);
            let detail = String::from_utf8(v.render(cols, rows)).unwrap();
            assert!(detail.contains("   indented 界 value"));
            assert!(detail.contains("second line"));
            assert!(!detail.contains("\x1b]52"));
            assert!(!detail.contains('\x07'));
            v.key(b"\x1b", cols, rows, &mut out);
            assert!(!v.reading);
            v.key(b"\r", cols, rows, &mut out);
            assert_eq!(String::from_utf8(v.render(cols, rows)).unwrap(), detail);
            assert!(v.key(b"q", cols, rows, &mut out));
        }
    }
}
