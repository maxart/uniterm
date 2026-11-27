//! Human reader for the Workspace's Project messageboards.
use crate::overlay::{
    clip_text, finish_lines, footer_spans, footer_text, footer_width, modal_hit, modal_rect,
    modal_visible_rows, panel_style, plain_text, render_list_modal_content, styled_line, ui_theme,
    ModalHit, Rect,
};
use crate::text_input::{decode_key, LineKey};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use uniterm_core::ProjectId;
use uniterm_proto::{
    encode_frame, ClientMessage, InboxAuthor, InboxPageStatus, InboxPost, InboxReadCursor,
};

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
const CARD_ROWS: usize = 4;

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
    resume_thread: Option<(u64, bool, usize)>,
    layout: RefCell<ThreadLayout>,
}

#[derive(Default)]
struct ThreadLayout {
    width: usize,
    lines: Vec<(bool, String)>,
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
        let rail = (rect.w as usize / 4).clamp(10, 27);
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
    pub(crate) fn position(&self) -> (u64, bool, usize, bool, usize) {
        (
            self.request,
            self.projects_focused,
            self.selected,
            self.reading,
            self.offset,
        )
    }

    fn refresh(&mut self, out: &mut Vec<u8>) {
        self.request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
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
        if request != self.request {
            return false;
        }
        let previous = self
            .projects
            .get(self.project)
            .map(|p| p.0)
            .unwrap_or(active);
        self.projects = projects;
        self.project = self
            .projects
            .iter()
            .position(|p| p.0 == previous)
            .or_else(|| self.projects.iter().position(|p| p.0 == active))
            .unwrap_or(0);
        let position = self
            .order
            .get(self.selected)
            .copied()
            .map(|id| (id, self.reading, self.offset));
        self.select_project(out);
        if self
            .projects
            .get(self.project)
            .is_some_and(|p| p.0 == previous)
        {
            self.resume_thread = position;
        }
        true
    }

    fn select_project(&mut self, out: &mut Vec<u8>) {
        self.request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        self.resume_thread = None;
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
            self.threads.entry(post.thread).or_default().push(post);
        }
        let retained = match status {
            InboxPageStatus::More(cursor) => {
                self.request_page(Some(cursor), out);
                return false;
            }
            InboxPageStatus::Complete { retained } => retained,
        };
        self.notice = notice;
        if self.threads.values().map(Vec::len).sum::<usize>() != retained {
            self.threads.clear();
            self.notice = Some(
                "Messages were removed while loading. Refresh to read the current Inbox.".into(),
            );
        }
        self.order = self.threads.keys().copied().collect();
        self.order
            .sort_unstable_by_key(|id| std::cmp::Reverse(self.threads[id].last().map(|p| p.id)));
        if let Some((id, reading, offset)) = self.resume_thread.take() {
            if let Some(i) = self.order.iter().position(|thread| *thread == id) {
                self.selected = i;
                self.reading = reading;
                self.offset = offset;
            }
        }
        self.loading = false;
        true
    }

    fn open_thread(&mut self) {
        if self.order.get(self.selected).is_some() {
            self.reading = true;
            self.projects_focused = false;
            self.offset = 0;
            *self.layout.get_mut() = ThreadLayout::default();
        }
    }

    fn thread_lines(&self, width: usize) -> std::cell::Ref<'_, ThreadLayout> {
        if self.layout.borrow().width != width {
            let mut lines = Vec::new();
            if let Some(posts) = self
                .order
                .get(self.selected)
                .and_then(|id| self.threads.get(id))
            {
                for post in posts {
                    lines.extend(
                        crate::overlay::wrap_text(&metadata(post), width)
                            .into_iter()
                            .map(|line| (true, line)),
                    );
                    let mut context = Vec::new();
                    if let Some(to) = post.to {
                        context.push(format!("To {}", author_label(to)));
                    }
                    if let Some(task) = post.task {
                        context.push(format!("Task #{task}"));
                    }
                    if let Some(owner) = post.claimed_by {
                        context.push(format!("Claimed by {}", author_label(owner)));
                    }
                    if !context.is_empty() {
                        lines.extend(
                            crate::overlay::wrap_text(&context.join("  ·  "), width)
                                .into_iter()
                                .map(|line| (false, line)),
                        );
                    }
                    for paragraph in post.body.split('\n') {
                        let safe = plain_text(&paragraph.replace('\t', "    "));
                        let mut line = String::new();
                        let mut used = 0;
                        for g in safe.graphemes(true) {
                            if used + g.width() > width && !line.is_empty() {
                                lines.push((false, std::mem::take(&mut line)));
                                used = 0;
                            }
                            line.push_str(g);
                            used += g.width();
                        }
                        lines.push((false, line));
                    }
                    lines.push((false, String::new()));
                }
            }
            *self.layout.borrow_mut() = ThreadLayout { width, lines };
        }
        self.layout.borrow()
    }

    fn navigate(&mut self, down: bool, amount: usize, g: &Geometry, out: &mut Vec<u8>) {
        let (value, max) = if self.projects_focused {
            (self.project, self.projects.len().saturating_sub(1))
        } else if self.reading {
            (
                self.offset,
                self.thread_lines(g.detail - 2)
                    .lines
                    .len()
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
                LineKey::Escape | LineKey::Left if self.reading => {
                    self.reading = false;
                    self.projects_focused = false;
                }
                LineKey::Escape | LineKey::Cancel | LineKey::Char('q') => return true,
                LineKey::Left => self.projects_focused = true,
                LineKey::Tab | LineKey::BackTab => self.projects_focused = !self.projects_focused,
                LineKey::Right | LineKey::Enter if self.projects_focused => {
                    self.projects_focused = false
                }
                LineKey::Right | LineKey::Enter if !self.reading => self.open_thread(),
                LineKey::Up | LineKey::Char('k') => self.navigate(false, 1, &g, out),
                LineKey::Down | LineKey::Char('j') => self.navigate(true, 1, &g, out),
                LineKey::Home => self.navigate(false, usize::MAX, &g, out),
                LineKey::End => self.navigate(true, usize::MAX, &g, out),
                LineKey::Char('r') => self.refresh(out),
                _ => {}
            }
        }
        false
    }

    fn footer(&self) -> [(&'static str, &'static str); 4] {
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
                ("Enter", "Feed")
            } else if self.reading {
                ("PgDn", "Scroll")
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
                let actions = self.footer();
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
                if !self.reading {
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
        let muted = format!("\x1b[22;{}m", theme.muted.sgr_fg());
        let selected = format!(
            "\x1b[1;{};{}m",
            theme.status_active_bg.sgr_bg(),
            theme.status_active_fg.sgr_fg()
        );
        let mut rows_out = Vec::new();
        let name = self
            .projects
            .get(self.project)
            .map(|p| p.1.as_str())
            .unwrap_or("");
        let heading = if self.reading {
            format!(" # {name} / Thread")
        } else {
            format!(" # {name}   {}", count(self.order.len(), "thread"))
        };
        rows_out.push(
            if let Some(notice) = self.notice.as_ref().filter(|_| !self.order.is_empty()) {
                styled_line(&[(&muted, notice)])
            } else {
                styled_line(&[(&accent, &heading)])
            },
        );
        if self.loading || self.order.is_empty() {
            let status = if self.loading {
                " Loading inbox..."
            } else if let Some(notice) = &self.notice {
                notice.as_str()
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
        } else if self.reading {
            let layout = self.thread_lines(g.detail - 2);
            let from = self
                .offset
                .min(layout.lines.len().saturating_sub(g.visible - 1));
            for (header, text) in layout.lines.iter().skip(from).take(g.visible - 1) {
                rows_out.push(styled_line(&[(
                    if *header { &accent } else { &panel },
                    &format!(" {text}"),
                )]));
            }
        } else {
            let from = first(self.selected, g.cards);
            for (i, thread) in self.order.iter().enumerate().skip(from).take(g.cards) {
                let posts = &self.threads[thread];
                let post = &posts[0];
                let style = if i == self.selected && !self.projects_focused {
                    &selected
                } else {
                    &accent
                };
                rows_out.push(styled_line(&[(style, &format!(" {}", metadata(post)))]));
                let snippet = clip_text(&post.body, g.detail * 2);
                let preview = crate::overlay::wrap_text(&snippet, g.detail - 2);
                for row in 0..2 {
                    let text = preview.get(row).map(String::as_str).unwrap_or("");
                    rows_out.push(styled_line(&[(&panel, &format!(" {text}"))]));
                }
                let replies = posts.len() - usize::from(post.id == *thread);
                rows_out.push(styled_line(&[(
                    &muted,
                    &format!(
                        " {}{}",
                        count(replies, "reply"),
                        if post.claimed_by.is_some() {
                            "  ·  Claimed"
                        } else {
                            ""
                        }
                    ),
                )]));
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
                        format!(" # {name}"),
                    )
                };
                let safe = clip_text(&label, g.rail);
                Some(format!(
                    "{style}{safe}{}{panel}",
                    " ".repeat(g.rail - safe.width())
                ))
            },
            &detail,
            &footer_text(
                fitted_footer(&self.footer(), usize::from(g.rect.w - 2)),
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

fn author_label(author: InboxAuthor) -> String {
    match author {
        InboxAuthor::Human => "Human".into(),
        InboxAuthor::Agent { pane, invocation } => format!("Agent {}.{}", pane.0, invocation),
    }
}

fn metadata(post: &InboxPost) -> String {
    let seconds = (post.created_ms / 1000) as libc::time_t;
    let mut date = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: gmtime_r initializes the supplied tm on success and retains neither pointer.
    let timestamp = unsafe {
        if libc::gmtime_r(&seconds, date.as_mut_ptr()).is_null() {
            String::from("Unknown date")
        } else {
            let t = date.assume_init();
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02} UTC",
                i64::from(t.tm_year) + 1900,
                t.tm_mon + 1,
                t.tm_mday,
                t.tm_hour,
                t.tm_min
            )
        }
    };
    format!(
        "{}  {:?}  {timestamp}",
        author_label(post.author),
        post.kind
    )
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
                let actions = v.footer();
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
