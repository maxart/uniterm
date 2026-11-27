//! Bounded Project conversations. The runtime owns the event journal and the
//! protected durable copy; this pure reducer never performs I/O or schedules
//! work (docs/29).
use crate::{PaneId, ProjectId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

mod checkpoint;
pub use checkpoint::LiveBoard;

/// Explicit budgets keep collaboration from becoming an unbounded transcript.
pub const BODY_LIMIT: usize = 8192;
/// Explicit links to other Project conversations. Punctuation may surround a
/// link, but an identifier containing the spelling is ordinary message text.
pub fn thread_mentions(body: &str) -> impl Iterator<Item = u64> + '_ {
    body.match_indices("thread:")
        .filter_map(move |(start, prefix)| {
            let word = |c: char| c.is_alphanumeric() || c == '_';
            if body[..start]
                .chars()
                .next_back()
                .is_some_and(|c| word(c) || c == '-')
            {
                return None;
            }
            let suffix = &body[start + prefix.len()..];
            let digits = suffix.bytes().take_while(u8::is_ascii_digit).count();
            if suffix[digits..].chars().next().is_some_and(word) {
                return None;
            }
            suffix[..digits].parse().ok().filter(|id| *id != 0)
        })
}

/// Maximum posts in one conversation, including its opening post.
pub const THREAD_LIMIT: usize = 100;
/// Bound the cloned bodies and receipts in each control response.
pub const PAGE_LIMIT: usize = 20;
/// One bounded page of changed visible rows. Larger bursts use ordinary snapshot pagination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReaderChanges {
    Posts(Vec<u64>),
    Reload,
}

impl ReaderChanges {
    /// A retained ID appears at most once even if its owner changes repeatedly.
    pub fn include(&mut self, id: u64) {
        if let Self::Posts(ids) = self {
            if !ids.contains(&id) {
                if ids.len() == PAGE_LIMIT {
                    *self = Self::Reload;
                } else {
                    ids.push(id);
                }
            }
        }
    }

    /// Fold changes received while a reader is still fetching its previous page.
    pub fn merge(&mut self, changes: &Self) {
        match changes {
            Self::Reload => *self = Self::Reload,
            Self::Posts(ids) => {
                for id in ids {
                    self.include(*id);
                }
            }
        }
    }
}

/// Per-Project live message count.
pub const PROJECT_LIMIT: usize = 1000;
/// Workspace payload budget, counting both journal and projection copies.
pub const BYTE_LIMIT: usize = 8 * 1024 * 1024;
/// Default retention: one day, unless the Workspace selects a longer one.
pub const TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// Longest explicit per-message lifetime, equal to the longest finite retention.
pub const MAX_TTL_MS: u64 = 30 * TTL_MS;

/// User-selected lifetime of Project inbox posts without an explicit shorter expiry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Retention {
    #[default]
    Day,
    Week,
    Month,
    Forever,
}

impl Retention {
    /// Ordering shared by the Settings control and configuration parser.
    pub const ALL: &'static [Self] = &[Self::Day, Self::Week, Self::Month, Self::Forever];

    /// Stable spelling in uniterm.conf.
    pub fn name(self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::Forever => "forever",
        }
    }

    /// Human-readable duration for Settings.
    pub fn label(self) -> &'static str {
        match self {
            Self::Day => "24 hours",
            Self::Week => "7 days",
            Self::Month => "30 days",
            Self::Forever => "Forever",
        }
    }

    /// Resolve a configuration value to a supported duration.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|r| r.name().eq_ignore_ascii_case(value))
    }

    /// Longest lifetime of any post, or `None` when only an explicit
    /// per-message lifetime, a clear or a Project removal ends it.
    pub fn ttl_ms(self) -> Option<u64> {
        match self {
            Self::Day => Some(TTL_MS),
            Self::Week => Some(7 * TTL_MS),
            Self::Month => Some(MAX_TTL_MS),
            Self::Forever => None,
        }
    }
}

/// The earlier of two deadlines, where `None` never arrives.
fn earlier(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Server-derived authorship; a reused Pane never inherits an old identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Author {
    Human,
    Agent { pane: PaneId, invocation: u64 },
}
/// Conversation intent helps agents distinguish evidence, disagreement and work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Plan,
    Proposal,
    Critique,
    Feedback,
    Question,
    Answer,
    Finding,
    Request,
    Handoff,
    Decision,
    Blocked,
}
/// Keep the ID an author cited and the conversation it resolved to at publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadReference {
    pub message: u64,
    pub thread: u64,
}

/// One immutable post; acknowledgement and claims live in subsequent events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub project: ProjectId,
    pub author: Author,
    pub kind: Kind,
    pub body: String,
    /// Explicit citations; older messages keep their ordinary body text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<ThreadReference>,
    pub reply_to: Option<u64>,
    pub thread: u64,
    pub to: Option<Author>,
    pub task: Option<u64>,
    pub key: String,
    pub created_ms: u64,
    /// Absolute deadline; `None` only under `Retention::Forever`.
    pub expires_ms: Option<u64>,
    /// Explicit lifetime requested by the author; `None` follows the
    /// Workspace retention, including later changes to it.
    pub ttl_ms: Option<u64>,
}
impl Message {
    fn payload_bytes(&self) -> usize {
        self.body.len() + self.references.len() * std::mem::size_of::<ThreadReference>()
    }
    // Explicit links are rendered as a final, backward-compatible paragraph.
    // Retry identity uses the author's original body and original cited IDs.
    fn authored_body(&self) -> &str {
        if self.references.is_empty() {
            &self.body
        } else {
            self.body.rsplit_once("\n\n").map_or("", |(body, _)| body)
        }
    }

    /// Whether the body must no longer be served or stored.
    pub fn expired(&self, now: u64) -> bool {
        self.expires_ms.is_some_and(|at| at <= now)
    }
}
/// A bounded lease is advisory work ownership, never permission to expand scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub owner: Author,
    pub revision: u64,
    pub expires_ms: u64,
}
/// Current view of a post, with revisions for atomic claims.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub message: Message,
    pub revision: u64,
    pub changed: u64,
    pub acknowledged: BTreeSet<Author>,
    pub claim: Option<Claim>,
}
/// All state changes are recorded before the view is projected.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    Post(Message),
    Ack {
        id: u64,
        author: Author,
    },
    Claim {
        id: u64,
        claim: Option<Claim>,
    },
    Remove {
        id: u64,
    },
    Pause {
        project: ProjectId,
        paused: bool,
    },
    Revoke {
        author: Author,
    },
    Grant {
        author: Author,
    },
    /// The human selected another lifetime for posts without a shorter one.
    Retention(Retention),
}
/// A post may carry explicit citations alongside its ordinary, free-form prose.
/// Its distinct wire operation lets older servers refuse it before publishing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostDraft {
    pub references: Vec<u64>,
    pub kind: Kind,
    pub body: String,
    pub reply_to: Option<u64>,
    pub to: Option<Author>,
    pub task: Option<u64>,
    pub key: String,
    pub ttl_ms: Option<u64>,
}
impl PostDraft {
    /// Version-gate explicit citations in the operation tag while keeping plain posts compatible.
    pub fn into_operation(self) -> Operation {
        if self.references.is_empty() {
            self.into_parts().0
        } else {
            Operation::Cite(self)
        }
    }

    fn into_parts(self) -> (Operation, Vec<u64>) {
        let Self {
            kind,
            body,
            reply_to,
            to,
            task,
            key,
            ttl_ms,
            references,
        } = self;
        (
            Operation::Post {
                kind,
                body,
                reply_to,
                to,
                task,
                key,
                ttl_ms,
            },
            references,
        )
    }
}
/// Same vocabulary serves local control, CLI and subscriptions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    Discover,
    /// A kernel-verified launch may obtain an invocation-scoped bearer grant.
    Credential,
    List {
        after: u64,
        thread: Option<u64>,
        limit: usize,
    },
    Watch {
        after: u64,
        timeout_ms: u64,
    },
    Post {
        kind: Kind,
        body: String,
        reply_to: Option<u64>,
        to: Option<Author>,
        task: Option<u64>,
        key: String,
        /// Omitted to follow the Workspace retention.
        #[serde(default)]
        ttl_ms: Option<u64>,
    },
    Ack {
        id: u64,
    },
    Claim {
        id: u64,
        revision: u64,
        lease_ms: u64,
    },
    Release {
        id: u64,
        revision: u64,
    },
    Pause {
        paused: bool,
    },
    Clear {
        confirmed: bool,
    },
    Revoke {
        author: Author,
    },
    /// Page a conversation by immutable message IDs. Subsequent pages reuse
    /// its opening cutoff and verify that the previously read prefix survived.
    Read {
        id: u64,
        after_id: u64,
        through: Option<u64>,
        seen: usize,
    },
    Cite(PostDraft),
}
/// A page carries a safe reconnect cursor and explicit retention floor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub cursor: u64,
    pub floor: u64,
    pub more: bool,
    pub paused: bool,
}
/// Runtime-owned ephemeral journal and its projection, both bounded.
#[derive(Default)]
pub struct Board {
    // Coalesced visual changes for attached readers; never checkpointed.
    reader_changes: BTreeMap<ProjectId, ReaderChanges>,
    journal: VecDeque<(u64, Event)>,
    entries: BTreeMap<u64, Entry>,
    projects: BTreeMap<ProjectId, BTreeSet<u64>>,
    // Only conversations with replies need a second set; single-post threads
    // already have constant-cost lookup through entries.
    replies: BTreeMap<(ProjectId, u64), BTreeSet<u64>>,
    paused: BTreeSet<ProjectId>,
    revoked: BTreeSet<Author>,
    sequence: u64,
    floors: BTreeMap<ProjectId, u64>,
    bytes: usize,
    changed: BTreeMap<ProjectId, u64>,
    rates: BTreeMap<Author, VecDeque<u64>>,
    retention: Retention,
    /// Posts at or below this id were restored from an earlier epoch. An
    /// agent identity never matches their idempotency keys.
    prior: u64,
    /// Last id from a schema-1 handoff, whose CLI encoded the default TTL as one day.
    legacy_ttl_through: u64,
    /// Counts transitions that change the durable copy; never serialized.
    durable: u64,
    /// Counts transitions that must erase a body from the durable copy.
    erasures: u64,
}
impl Board {
    /// Lifetime applied to posts that do not ask for a shorter one.
    pub fn retention(&self) -> Retention {
        self.retention
    }
    /// Apply a new retention to every unexpired post. Explicit shorter
    /// lifetimes and reply boundaries keep limiting their posts; anything
    /// now past its deadline is erased immediately.
    pub fn set_retention(&mut self, retention: Retention, now: u64) {
        // A longer policy must never resurrect a post whose old deadline passed.
        self.expire(now);
        if self.retention != retention {
            self.record(Event::Retention(retention));
            self.expire(now);
        }
    }
    /// Changes whenever the protected durable copy is out of date.
    pub fn durable_revision(&self) -> u64 {
        self.durable
    }
    /// Changes whenever a stored body must be erased from the durable copy.
    pub fn erasures(&self) -> u64 {
        self.erasures
    }
    /// Nothing is worth keeping at rest: no post and no human pause.
    pub fn durably_empty(&self) -> bool {
        self.entries.is_empty() && self.paused.is_empty()
    }
    /// Deadline of a post created at `created` under the current retention.
    fn expiry(&self, created: u64, ttl_ms: Option<u64>) -> Option<u64> {
        earlier(ttl_ms, self.retention.ttl_ms()).map(|ttl| created.saturating_add(ttl))
    }
    /// Recompute every deadline in id order so a reply sees its parent's new one.
    fn reproject_expiry(&mut self) {
        let ids: Vec<_> = self.entries.keys().copied().collect();
        for id in ids {
            let message = &self.entries[&id].message;
            let own = self.expiry(message.created_ms, message.ttl_ms);
            let next = match message.reply_to {
                None => own,
                Some(parent) => match self.entries.get(&parent) {
                    Some(parent) => earlier(own, parent.message.expires_ms),
                    // Without its parent a reply may shrink but never grow.
                    None => earlier(own, message.expires_ms),
                },
            };
            if message.expires_ms != next {
                let entry = self.entries.get_mut(&id).expect("id from this board");
                entry.message.expires_ms = next;
                if let (Some(claim), Some(at)) = (&mut entry.claim, next) {
                    claim.expires_ms = claim.expires_ms.min(at);
                }
                // One logged policy change can update a whole Project. Give
                // each affected row its own cursor so even a one-row page can
                // continue through the batch without skipping a sibling.
                self.sequence += 1;
                entry.changed = self.sequence;
                self.changed.insert(entry.message.project, self.sequence);
            }
        }
        // The journal holds payload copies; keep them identical to the projection.
        for (_, event) in &mut self.journal {
            if let Event::Post(message) = event {
                if let Some(entry) = self.entries.get(&message.id) {
                    message.expires_ms = entry.message.expires_ms;
                }
            }
        }
    }
    /// Sequence changes only for real state transitions.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    /// Append first, then project. Retained history is bounded and advertises gaps.
    fn record(&mut self, event: Event) {
        self.sequence += 1;
        let project = match &event {
            Event::Post(m) => Some(m.project),
            Event::Ack { id, .. } | Event::Claim { id, .. } | Event::Remove { id } => {
                self.entries.get(id).map(|e| e.message.project)
            }
            Event::Pause { project, .. } => Some(*project),
            _ => None,
        };
        if let Some(project) = project {
            self.changed.insert(project, self.sequence);
            let visible = match &event {
                Event::Post(m) => Some(m.id),
                Event::Remove { id } => Some(*id),
                Event::Claim { id, claim }
                    if self
                        .entries
                        .get(id)
                        .and_then(|entry| entry.claim.as_ref())
                        .map(|claim| &claim.owner)
                        != claim.as_ref().map(|claim| &claim.owner) =>
                {
                    Some(*id)
                }
                _ => None,
            };
            if let Some(id) = visible {
                self.reader_changes
                    .entry(project)
                    .or_insert_with(|| ReaderChanges::Posts(Vec::new()))
                    .include(id);
            }
        }
        self.journal.push_back((self.sequence, event.clone()));
        match event {
            Event::Retention(retention) => {
                self.retention = retention;
                self.durable += 1;
                self.reproject_expiry();
            }
            Event::Post(message) => {
                self.durable += 1;
                self.rates
                    .entry(message.author.clone())
                    .or_default()
                    .push_back(message.created_ms);
                self.bytes += message.payload_bytes() * 2;
                self.projects
                    .entry(message.project)
                    .or_default()
                    .insert(message.id);
                self.index_reply(&message);
                self.entries.insert(
                    message.id,
                    Entry {
                        message,
                        revision: 0,
                        changed: self.sequence,
                        acknowledged: BTreeSet::new(),
                        claim: None,
                    },
                );
            }
            Event::Ack { id, author } => {
                if let Some(e) = self.entries.get_mut(&id) {
                    // Only the human's receipt outlives a restart.
                    self.durable += u64::from(author == Author::Human);
                    e.acknowledged.insert(author);
                    e.changed = self.sequence;
                }
            }
            Event::Claim { id, claim } => {
                if let Some(e) = self.entries.get_mut(&id) {
                    e.revision += 1;
                    e.changed = self.sequence;
                    e.claim = claim;
                }
            }
            Event::Remove { id } => {
                if let Some(e) = self.entries.remove(&id) {
                    self.durable += 1;
                    self.erasures += 1;
                    self.bytes -= e.message.payload_bytes() * 2;
                    if let Some(ids) = self.projects.get_mut(&e.message.project) {
                        ids.remove(&id);
                        if ids.is_empty() {
                            self.projects.remove(&e.message.project);
                        }
                    }
                    let thread = (e.message.project, e.message.thread);
                    if let Some(replies) = self.replies.get_mut(&thread) {
                        replies.remove(&id);
                        if replies.is_empty() {
                            self.replies.remove(&thread);
                        }
                    }
                    self.floors.insert(e.message.project, self.sequence);
                }
            }
            Event::Pause { project, paused } => {
                self.durable += 1;
                if paused {
                    self.paused.insert(project);
                } else {
                    self.paused.remove(&project);
                }
            }
            Event::Grant { .. } => {}
            Event::Revoke { author } => {
                self.revoked.insert(author);
            }
        }
        if self.journal.len() > 4096 {
            // List and watch read the projection. Rotating its backing journal
            // loses no visible state and does not create a reconciliation gap.
            self.journal.pop_front();
        }
    }
    /// Drain only Projects whose displayed posts or claim owners changed.
    pub fn take_reader_changes(&mut self) -> BTreeMap<ProjectId, ReaderChanges> {
        std::mem::take(&mut self.reader_changes)
    }

    /// Direct post lookup avoids pagination-dependent mutation responses.
    pub fn entry(&self, id: u64) -> Option<&Entry> {
        self.entries.get(&id)
    }
    /// Project-local revisions wake only interested subscribers.
    pub fn changed(&self, project: ProjectId) -> u64 {
        self.changed.get(&project).copied().unwrap_or(0)
    }

    /// Erase the journal's payload copies in one pass for the entire batch.
    fn remove_posts(&mut self, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        let mut affected = BTreeSet::new();
        for &id in ids {
            if let Some(entry) = self.entries.get(&id) {
                affected.insert(entry.message.project);
            }
            self.record(Event::Remove { id });
        }
        self.journal.retain(|(_, event)| match event {
            Event::Post(message) => self.entries.contains_key(&message.id),
            _ => true,
        });
        for project in affected {
            self.refresh_cursors_below_floor(project);
        }
    }

    /// A removal invalidates old incremental views. Give retained rows fresh
    /// cursors in their previous order so a full relist can page past the gap.
    fn refresh_cursors_below_floor(&mut self, project: ProjectId) {
        let floor = self.floor(project);
        if floor == 0 {
            return;
        }
        let mut older: Vec<_> = self
            .projects
            .get(&project)
            .into_iter()
            .flatten()
            .filter_map(|id| self.entries.get(id))
            .filter(|entry| entry.changed < floor)
            .map(|entry| (entry.changed, entry.message.id))
            .collect();
        older.sort_unstable();
        for (_, id) in older {
            self.sequence += 1;
            self.entries.get_mut(&id).expect("indexed post").changed = self.sequence;
            self.changed.insert(project, self.sequence);
        }
    }

    fn floor(&self, project: ProjectId) -> u64 {
        self.floors.get(&project).copied().unwrap_or(0)
    }

    /// Structural deletion erases all payload copies and Project controls.
    pub fn retain_projects(&mut self, live: &BTreeSet<ProjectId>) {
        let gone: Vec<_> = self
            .changed
            .keys()
            .filter(|p| !live.contains(p))
            .copied()
            .collect();
        // Structural deletion already has its own authorization. Erase and
        // reconcile once even when several Projects disappear together.
        let ids: Vec<_> = gone
            .iter()
            .filter_map(|project| self.projects.get(project))
            .flatten()
            .copied()
            .collect();
        self.remove_posts(&ids);
        for project in gone {
            if self.paused.contains(&project) {
                self.record(Event::Pause {
                    project,
                    paused: false,
                });
            }
            self.changed.remove(&project);
            self.floors.remove(&project);
        }
    }
    /// One-shot deadline; an empty board has no idle wakeup.
    pub fn deadline(&self) -> Option<u64> {
        self.entries
            .values()
            .flat_map(|e| [e.message.expires_ms, e.claim.as_ref().map(|c| c.expires_ms)])
            .flatten()
            .min()
    }
    /// Expire bodies and leases on access or the earliest outstanding deadline.
    pub fn expire(&mut self, now: u64) {
        let remove: Vec<_> = self
            .entries
            .values()
            .filter(|e| e.message.expired(now))
            .map(|e| e.message.id)
            .collect();
        self.remove_posts(&remove);
        let release: Vec<_> = self
            .entries
            .values()
            .filter(|e| e.claim.as_ref().is_some_and(|c| c.expires_ms <= now))
            .map(|e| e.message.id)
            .collect();
        for id in release {
            self.record(Event::Claim { id, claim: None });
        }
    }
    /// Invocation exit releases work immediately without giving a successor ownership.
    pub fn forget_pane(&mut self, pane: PaneId) {
        let ids: Vec<_> = self
            .entries
            .values()
            .filter(|e| {
                e.claim
                    .as_ref()
                    .is_some_and(|c| matches!(c.owner, Author::Agent { pane: p, .. } if p == pane))
            })
            .map(|e| e.message.id)
            .collect();
        for id in ids {
            self.record(Event::Claim { id, claim: None });
        }
        self.revoked
            .retain(|a| !matches!(a, Author::Agent { pane: p, .. } if *p == pane));
    }
    /// Membership and revocation are checked even on reads and watch wakeups.
    pub fn authorize(&self, author: &Author) -> Result<(), &'static str> {
        if self.revoked.contains(author) {
            Err("revoked")
        } else {
            Ok(())
        }
    }
    /// Reject stale incremental cursors without allocating a page.
    pub fn validate_cursor(&self, project: ProjectId, after: u64) -> Result<(), &'static str> {
        if after > self.sequence || (after != 0 && after < self.floor(project)) {
            Err("cursor_expired")
        } else {
            Ok(())
        }
    }
    /// Browse retained posts in creation order without changing acknowledgements or leases.
    /// An id cursor stays valid when an earlier post expires or gets a new revision.
    pub fn reader_entries(
        &self,
        project: ProjectId,
        after: u64,
        now: u64,
    ) -> impl Iterator<Item = &Entry> {
        use std::ops::Bound::{Excluded, Unbounded};
        self.projects
            .get(&project)
            .into_iter()
            .flat_map(move |ids| ids.range((Excluded(after), Unbounded)))
            .filter_map(|id| self.entries.get(id))
            .filter(move |entry| !entry.message.expired(now))
    }

    fn index_reply(&mut self, message: &Message) {
        if message.id != message.thread {
            self.replies
                .entry((message.project, message.thread))
                .or_default()
                .insert(message.id);
        }
    }

    fn thread_ids(&self, project: ProjectId, root: u64) -> impl Iterator<Item = u64> + '_ {
        let root_entry = self
            .entries
            .get(&root)
            .filter(|e| e.message.project == project && e.message.thread == root);
        root_entry.map(|e| e.message.id).into_iter().chain(
            self.replies
                .get(&(project, root))
                .into_iter()
                .flatten()
                .copied(),
        )
    }

    /// Read one bounded slice of a conversation. A receipt, lease, or
    /// unrelated expiry cannot reorder its pages. A missing previously
    /// fetched post invalidates the partial result before the caller emits it.
    pub fn read_thread(
        &self,
        project: ProjectId,
        id: u64,
        after_id: u64,
        through: Option<u64>,
        seen: usize,
    ) -> Result<Page, &'static str> {
        let root = self
            .entries
            .get(&id)
            .filter(|e| e.message.project == project)
            .ok_or("not_found")?
            .message
            .thread;
        let cutoff = through.unwrap_or(self.sequence);
        if cutoff > self.sequence
            || after_id > cutoff
            || id > cutoff
            || seen > THREAD_LIMIT
            || (through.is_none() && (after_id != 0 || seen != 0))
        {
            return Err("invalid_thread_cursor");
        }
        let mut ids = self
            .thread_ids(project, root)
            .take_while(|id| *id <= cutoff)
            .peekable();
        let mut retained = 0;
        while ids.next_if(|id| *id <= after_id).is_some() {
            retained += 1;
        }
        if retained != seen {
            return Err("thread_changed");
        }
        let entries = ids
            .by_ref()
            .take(PAGE_LIMIT)
            .map(|id| self.entries[&id].clone())
            .collect();
        Ok(Page {
            entries,
            cursor: cutoff,
            floor: self.floor(project),
            more: ids.next().is_some(),
            paused: self.paused.contains(&project),
        })
    }

    /// A mutation receipt carries its affected row and a cursor for subsequent changes.
    pub fn mutation_page(&self, project: ProjectId, id: Option<u64>) -> Page {
        Page {
            entries: id
                .and_then(|id| self.entries.get(&id))
                .filter(|entry| entry.message.project == project)
                .cloned()
                .into_iter()
                .collect(),
            cursor: self.sequence,
            floor: self.floor(project),
            more: false,
            paused: self.paused.contains(&project),
        }
    }

    /// Project-indexed pagination; message contents are untrusted attributed data.
    pub fn list(
        &self,
        project: ProjectId,
        after: u64,
        thread: Option<u64>,
        limit: usize,
    ) -> Result<Page, &'static str> {
        self.validate_cursor(project, after)?;
        // Opening a reply opens its conversation. A missing or foreign ID
        // preserves the existing empty-page behavior of a filtered listing.
        let thread = thread.map(|id| {
            self.entries
                .get(&id)
                .filter(|entry| entry.message.project == project)
                .map_or(id, |entry| entry.message.thread)
        });
        let mut entries: Vec<_> = if let Some(root) = thread {
            self.thread_ids(project, root)
                .filter_map(|id| self.entries.get(&id))
                .filter(|e| e.changed > after)
                .collect()
        } else {
            self.projects
                .get(&project)
                .into_iter()
                .flatten()
                .filter_map(|id| self.entries.get(id))
                .filter(|e| e.changed > after)
                .collect()
        };
        let limit = limit.clamp(1, PAGE_LIMIT);
        let more = entries.len() > limit;
        if more {
            entries.select_nth_unstable_by_key(limit, |e| e.changed);
            entries.truncate(limit);
        }
        entries.sort_unstable_by_key(|e| e.changed);
        let cursor = if more {
            entries.last().map_or(after, |e| e.changed)
        } else {
            self.sequence
        };
        Ok(Page {
            entries: entries.into_iter().cloned().collect(),
            cursor,
            floor: self.floor(project),
            more,
            paused: self.paused.contains(&project),
        })
    }
    /// Validate, append an event, and return the changed post id. Retried posts
    /// return their original id only if their full content is identical.
    pub fn apply(
        &mut self,
        project: ProjectId,
        author: Author,
        op: Operation,
        now: u64,
    ) -> Result<Option<u64>, &'static str> {
        self.expire(now);
        self.rates.retain(|_, times| {
            while times
                .front()
                .is_some_and(|t| now.saturating_sub(*t) >= 60_000)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
        self.authorize(&author)?;
        let human = author == Author::Human;
        if self.paused.contains(&project) && !human {
            return Err("paused");
        }
        let (op, cited) = match op {
            Operation::Cite(draft) => draft.into_parts(),
            other => (other, Vec::new()),
        };
        match op {
            Operation::Credential if !human => {
                self.record(Event::Grant { author });
                Ok(None)
            }
            Operation::Post {
                kind,
                mut body,
                reply_to,
                to,
                task,
                key,
                ttl_ms,
            } => {
                if (body.trim().is_empty() && cited.is_empty())
                    || cited.len() > BODY_LIMIT / std::mem::size_of::<ThreadReference>()
                    || body.len() > BODY_LIMIT
                    || body
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\t')
                    || key.is_empty()
                    || key.len() > 128
                    || ttl_ms.is_some_and(|ttl| !(1000..=MAX_TTL_MS).contains(&ttl))
                {
                    return Err("invalid_post");
                }
                for e in self
                    .projects
                    .get(&project)
                    .into_iter()
                    .flatten()
                    .filter_map(|id| self.entries.get(id))
                {
                    let m = &e.message;
                    // A restart may reuse an agent identity; only the human's
                    // retries reach posts restored from an earlier epoch.
                    if (m.id > self.prior || human) && m.author == author && m.key == key {
                        return if m.kind == kind
                            && m.authored_body() == body
                            && m.references
                                .iter()
                                .map(|r| r.message)
                                .eq(cited.iter().copied())
                            && m.reply_to == reply_to
                            && m.to == to
                            && m.task == task
                            && (m.ttl_ms == ttl_ms
                                || (m.id <= self.legacy_ttl_through
                                    && ttl_ms.is_none()
                                    && m.ttl_ms == Some(TTL_MS)))
                        {
                            Ok(Some(m.id))
                        } else {
                            Err("idempotency_conflict")
                        };
                    }
                }
                let mut references = Vec::with_capacity(cited.len());
                for message in cited {
                    let thread = self
                        .entries
                        .get(&message)
                        .filter(|e| e.message.project == project)
                        .ok_or("thread_not_found")?
                        .message
                        .thread;
                    if !references.is_empty() {
                        body.push(' ');
                    } else if !body.is_empty() {
                        body.push_str("\n\n");
                    }
                    use std::fmt::Write as _;
                    write!(body, "thread:{thread}").expect("writing to a String cannot fail");
                    if body.len() > BODY_LIMIT {
                        return Err("invalid_post");
                    }
                    references.push(ThreadReference { message, thread });
                }
                let payload_bytes =
                    body.len() + references.len() * std::mem::size_of::<ThreadReference>();
                // Keep a full page of richly cited posts within one control frame.
                if payload_bytes > BODY_LIMIT {
                    return Err("invalid_post");
                }
                if self.projects.get(&project).map_or(0, BTreeSet::len) >= PROJECT_LIMIT
                    || self.bytes + payload_bytes * 2 > BYTE_LIMIT
                    || self.entries.len() >= 4000
                {
                    return Err("capacity");
                }
                if self
                    .rates
                    .get(&author)
                    .is_some_and(|times| times.len() >= 30)
                {
                    return Err("rate_limited");
                }
                if !self.rates.contains_key(&author) && self.rates.len() >= 1024 {
                    return Err("capacity");
                }
                let id = self.sequence + 1;
                let (thread, expires_ms) = if let Some(parent) = reply_to {
                    let parent = self
                        .entries
                        .get(&parent)
                        .filter(|e| e.message.project == project)
                        .ok_or("not_found")?;
                    if self
                        .replies
                        .get(&(project, parent.message.thread))
                        .map_or(0, BTreeSet::len)
                        >= THREAD_LIMIT - 1
                    {
                        return Err("thread_budget");
                    }
                    (
                        parent.message.thread,
                        earlier(self.expiry(now, ttl_ms), parent.message.expires_ms),
                    )
                } else {
                    (id, self.expiry(now, ttl_ms))
                };
                self.record(Event::Post(Message {
                    id,
                    project,
                    author,
                    kind,
                    body,
                    references,
                    reply_to,
                    thread,
                    to,
                    task,
                    key,
                    created_ms: now,
                    expires_ms,
                    ttl_ms,
                }));
                Ok(Some(id))
            }
            Operation::Ack { id } => {
                let e = self
                    .entries
                    .get(&id)
                    .filter(|e| e.message.project == project)
                    .ok_or("not_found")?;
                if !e.acknowledged.contains(&author) {
                    if e.acknowledged.len() >= 64
                        || self
                            .entries
                            .values()
                            .map(|e| e.acknowledged.len())
                            .sum::<usize>()
                            >= 4096
                    {
                        return Err("capacity");
                    }
                    self.record(Event::Ack { id, author });
                }
                Ok(Some(id))
            }
            Operation::Claim {
                id,
                revision,
                lease_ms,
            } => {
                let e = self
                    .entries
                    .get(&id)
                    .filter(|e| e.message.project == project)
                    .ok_or("not_found")?;
                if e.message.thread != id {
                    return Err("claim_thread_root");
                }
                if e.revision != revision {
                    return Err("revision_conflict");
                }
                if e.claim.as_ref().is_some_and(|c| c.owner != author) {
                    return Err("already_claimed");
                }
                if e.message.task.is_some_and(|task| {
                    self.entries.values().any(|other| {
                        other.message.id != id
                            && other.message.task == Some(task)
                            && other.claim.is_some()
                    })
                }) {
                    return Err("task_already_claimed");
                }
                if !(1000..=3_600_000).contains(&lease_ms) {
                    return Err("invalid_lease");
                }
                self.record(Event::Claim {
                    id,
                    claim: Some(Claim {
                        owner: author,
                        revision: revision + 1,
                        expires_ms: earlier(
                            Some(now.saturating_add(lease_ms)),
                            e.message.expires_ms,
                        )
                        .unwrap_or(u64::MAX),
                    }),
                });
                Ok(Some(id))
            }
            Operation::Release { id, revision } => {
                let e = self
                    .entries
                    .get(&id)
                    .filter(|e| e.message.project == project)
                    .ok_or("not_found")?;
                if e.revision != revision {
                    return Err("revision_conflict");
                }
                if !human && e.claim.as_ref().is_none_or(|c| c.owner != author) {
                    return Err("not_owner");
                }
                if e.claim.is_some() {
                    self.record(Event::Claim { id, claim: None });
                }
                Ok(Some(id))
            }
            Operation::Pause { paused } if human => {
                if self.paused.contains(&project) != paused {
                    self.record(Event::Pause { project, paused });
                }
                Ok(None)
            }
            Operation::Clear { confirmed: true } if human => {
                let ids: Vec<_> = self
                    .projects
                    .get(&project)
                    .into_iter()
                    .flatten()
                    .copied()
                    .collect();
                self.remove_posts(&ids);
                Ok(None)
            }
            Operation::Revoke { author } if human => {
                if author == Author::Human {
                    return Err("invalid_recipient");
                }
                if self.revoked.contains(&author) {
                    return Ok(None);
                }
                if self.revoked.len() >= 1024 {
                    return Err("capacity");
                }
                self.record(Event::Revoke {
                    author: author.clone(),
                });
                let ids: Vec<_> = self
                    .entries
                    .values()
                    .filter(|e| e.claim.as_ref().is_some_and(|c| c.owner == author))
                    .map(|e| e.message.id)
                    .collect();
                for id in ids {
                    self.record(Event::Claim { id, claim: None });
                }
                Ok(None)
            }
            _ => Err("forbidden"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn worker(n: u64) -> Author {
        Author::Agent {
            pane: PaneId(n),
            invocation: n,
        }
    }
    fn post(key: &str) -> Operation {
        Operation::Post {
            kind: Kind::Proposal,
            body: "Test evidence and alternatives".into(),
            reply_to: None,
            to: None,
            task: None,
            key: key.into(),
            ttl_ms: Some(10_000),
        }
    }
    #[test]
    fn inbox_reader_bursts_are_bounded_and_duplicate_rows_coalesce() {
        let mut changes = ReaderChanges::Posts(Vec::new());
        for id in 1..=20 {
            changes.merge(&ReaderChanges::Posts(vec![id, id]));
        }
        assert_eq!(changes, ReaderChanges::Posts((1..=20).collect()));
        changes.include(21);
        changes.include(22);
        assert_eq!(changes, ReaderChanges::Reload);
    }

    #[test]
    fn reader_cursor_respects_project_expiry_and_leaves_the_board_untouched() {
        let mut board = Board::default();
        let project = ProjectId(1);
        let first = board
            .apply(project, worker(1), post("1"), 0)
            .unwrap()
            .unwrap();
        board.apply(ProjectId(2), worker(2), post("2"), 1).unwrap();
        let last = board
            .apply(project, worker(1), post("3"), 2)
            .unwrap()
            .unwrap();
        let seq = board.sequence();
        for (cursor, time) in [(first, 2), (0, 10_000)] {
            let entries: Vec<_> = board.reader_entries(project, cursor, time).collect();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].message.id, last);
            assert!(entries[0].acknowledged.is_empty());
        }
        assert_eq!(board.sequence(), seq);
        assert!(board.reader_entries(project, u64::MAX, 2).next().is_none());
    }

    fn defaulted(key: &str, reply: Option<u64>, ttl: Option<u64>) -> Operation {
        let mut op = post(key);
        if let Operation::Post {
            reply_to, ttl_ms, ..
        } = &mut op
        {
            *reply_to = reply;
            *ttl_ms = ttl;
        }
        op
    }
    #[test]
    fn read_pages_survive_unrelated_expiry_and_concurrent_replies_but_never_return_erased_posts() {
        let mut board = Board::default();
        let project = ProjectId(1);
        let other = ProjectId(2);
        let root = board
            .apply(project, worker(1), defaulted("root", None, None), 0)
            .unwrap()
            .unwrap();
        let mut ids = vec![root];
        for n in 1..45 {
            let id = board
                .apply(
                    project,
                    worker(n / 20 + 2),
                    defaulted(&n.to_string(), Some(root), (n == 2).then_some(2000)),
                    0,
                )
                .unwrap()
                .unwrap();
            ids.push(id);
        }
        board
            .apply(
                other,
                worker(9),
                defaulted("unrelated", None, Some(1000)),
                0,
            )
            .unwrap();
        assert_eq!(board.read_thread(other, root, 0, None, 0), Err("not_found"));
        let mut board = Board::from_live_checkpoint(board.live_checkpoint()).unwrap();
        let mut page = board.read_thread(project, ids[41], 0, None, 0).unwrap();
        let cutoff = page.cursor;
        let mut seen: Vec<_> = page.entries.iter().map(|e| e.message.id).collect();
        assert_eq!(seen, ids[..PAGE_LIMIT]);
        assert!(page.more);
        board.expire(1000);
        board
            .apply(project, worker(9), Operation::Ack { id: ids[44] }, 1000)
            .unwrap();
        board
            .apply(
                project,
                worker(9),
                defaulted("late", Some(root), None),
                1000,
            )
            .unwrap();
        while page.more {
            page = board
                .read_thread(
                    project,
                    ids[41],
                    *seen.last().unwrap(),
                    Some(cutoff),
                    seen.len(),
                )
                .unwrap();
            assert_eq!(page.cursor, cutoff);
            assert_eq!(page.floor, 0);
            seen.extend(page.entries.iter().map(|e| e.message.id));
        }
        assert_eq!(seen, ids);
        board.expire(2000);
        assert_eq!(
            board.read_thread(project, root, ids[19], Some(cutoff), 20),
            Err("thread_changed")
        );
        let restored = Board::from_durable_checkpoint(board.durable_checkpoint(), 2000).unwrap();
        assert_eq!(
            restored
                .read_thread(project, root, 0, None, 0)
                .unwrap()
                .entries[2]
                .message
                .id,
            ids[3]
        );
    }

    #[test]
    fn participants_can_reply_to_comments_and_cite_project_threads() {
        let mut board = Board::default();
        let project = ProjectId(7);
        let other = ProjectId(8);
        let related = board
            .apply(project, worker(1), defaulted("logs", None, Some(1000)), 0)
            .unwrap()
            .unwrap();
        let foreign = board
            .apply(other, worker(5), post("foreign"), 0)
            .unwrap()
            .unwrap();
        let root = board
            .apply(project, worker(2), post("design"), 0)
            .unwrap()
            .unwrap();
        let reply = PostDraft {
            kind: Kind::Feedback,
            body: "Review these logs.".into(),
            reply_to: Some(root),
            to: None,
            task: None,
            key: "reply".into(),
            ttl_ms: None,
            references: vec![related],
        }
        .into_operation();
        let second = board
            .apply(project, worker(3), reply.clone(), 0)
            .unwrap()
            .unwrap();
        let third = board
            .apply(
                project,
                worker(4),
                defaulted("nested", Some(second), None),
                0,
            )
            .unwrap()
            .unwrap();
        let entries = board.list(project, 0, Some(third), 20).unwrap().entries;
        assert_eq!(
            entries
                .iter()
                .map(|e| e.message.author.clone())
                .collect::<Vec<_>>(),
            [worker(2), worker(3), worker(4)]
        );
        assert_eq!(entries[2].message.reply_to, Some(second));
        assert_eq!(entries[2].message.thread, root);
        for invalid in [foreign, 999] {
            let op = PostDraft {
                kind: Kind::Feedback,
                body: "bad".into(),
                reply_to: None,
                to: None,
                task: None,
                key: "bad-ref".into(),
                ttl_ms: None,
                references: vec![invalid],
            }
            .into_operation();
            let before = board.sequence();
            assert_eq!(
                board.apply(project, worker(2), op, 0),
                Err("thread_not_found")
            );
            assert_eq!(board.sequence(), before);
        }
        assert_eq!(
            board
                .take_reader_changes()
                .into_keys()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([project, other])
        );
        board
            .apply(project, worker(4), Operation::Ack { id: second }, 0)
            .unwrap();
        assert!(board.take_reader_changes().is_empty());
        board.expire(1001);
        assert_eq!(
            board.take_reader_changes(),
            BTreeMap::from([(project, ReaderChanges::Posts(vec![related]))])
        );
        let before = board.sequence();
        assert_eq!(
            board.apply(project, worker(3), reply, 1001),
            Ok(Some(second))
        );
        assert_eq!(board.sequence(), before);
        let mut same_text = defaulted("reply", Some(root), None);
        if let Operation::Post { kind, body, .. } = &mut same_text {
            *kind = Kind::Feedback;
            *body = format!("Review these logs.\n\nthread:{related}");
        }
        assert_eq!(
            board.apply(project, worker(3), same_text, 1001),
            Err("idempotency_conflict")
        );
        // Plain posts may quote expired or foreign IDs, including pasted logs.
        let mut logs = post("log-output");
        if let Operation::Post { body, .. } = &mut logs {
            *body = format!("worker-thread:{foreign} ERR; [thread:999]");
        }
        board.apply(project, worker(2), logs, 1001).unwrap();
        let cite = PostDraft {
            kind: Kind::Feedback,
            body: "Follow-up".into(),
            reply_to: None,
            to: None,
            task: None,
            key: "cite-comment".into(),
            ttl_ms: None,
            references: vec![third],
        }
        .into_operation();
        let linked = board
            .apply(project, worker(2), cite.clone(), 1001)
            .unwrap()
            .unwrap();
        let message = board.entry(linked).unwrap().message.clone();
        assert_eq!(
            message.references,
            [ThreadReference {
                message: third,
                thread: root
            }]
        );
        assert_eq!(message.body, format!("Follow-up\n\nthread:{root}"));
        // Both the rendered footer and the structured IDs count toward the post quota.
        let mut oversized = cite.clone();
        if let Operation::Cite(draft) = &mut oversized {
            draft.key = "large".into();
            draft.references = vec![root; BODY_LIMIT / std::mem::size_of::<ThreadReference>()];
        }
        assert_eq!(
            board.apply(project, worker(2), oversized, 1001),
            Err("invalid_post")
        );
        let mut restored =
            Board::from_durable_checkpoint(board.durable_checkpoint(), 1001).unwrap();
        assert_eq!(
            restored.entry(linked).unwrap().message.references,
            message.references
        );
        // Durable restore deliberately gives agent identities a new idempotency scope.
        assert_ne!(
            restored.apply(project, worker(2), cite, 1001).unwrap(),
            Some(linked)
        );
        assert_eq!(
            thread_mentions("mythread:2 worker-thread:7 thread:3a thread:0 (thread:5), thread:42.")
                .collect::<Vec<_>>(),
            [5, 42]
        );
    }

    #[test]
    fn retention_names_labels_and_durations_are_stable() {
        let expected = [
            (Retention::Day, "24h", "24 hours", Some(TTL_MS)),
            (Retention::Week, "7d", "7 days", Some(7 * TTL_MS)),
            (Retention::Month, "30d", "30 days", Some(30 * TTL_MS)),
            (Retention::Forever, "forever", "Forever", None),
        ];
        assert_eq!(Retention::ALL.len(), expected.len());
        for (index, (retention, name, label, ttl)) in expected.into_iter().enumerate() {
            assert_eq!(Retention::ALL[index], retention);
            assert_eq!(retention.name(), name);
            assert_eq!(retention.label(), label);
            assert_eq!(retention.ttl_ms(), ttl);
            assert_eq!(Retention::parse(name), Some(retention));
        }
        assert_eq!(Retention::default(), Retention::Day);
        assert_eq!(Retention::parse("FOREVER"), Some(Retention::Forever));
        assert_eq!(Retention::parse("1y"), None);
        assert_eq!(Retention::parse(""), None);
    }

    #[test]
    fn one_hundred_agent_identities_can_share_a_nested_conversation() {
        let mut board = Board::default();
        let project = ProjectId(7);
        let mut parent = None;
        for author in 1..=THREAD_LIMIT as u64 {
            parent = board
                .apply(
                    project,
                    worker(author),
                    defaulted("contribution", parent, None),
                    0,
                )
                .unwrap();
        }
        let newest = parent.unwrap();
        let mut after = 0;
        let mut seen = 0;
        let mut cutoff = None;
        loop {
            let page = board
                .read_thread(project, newest, after, cutoff, seen)
                .unwrap();
            cutoff = Some(page.cursor);
            for entry in page.entries {
                seen += 1;
                assert_eq!(entry.message.author, worker(seen as u64));
                assert_eq!(entry.message.reply_to, (after != 0).then_some(after));
                assert_eq!(entry.message.thread, 1);
                after = entry.message.id;
            }
            if !page.more {
                break;
            }
        }
        assert_eq!(seen, THREAD_LIMIT);
        assert_eq!(
            board.apply(project, worker(101), defaulted("another", parent, None), 0),
            Err("thread_budget")
        );
    }

    #[test]
    fn expiry_invalidates_only_its_projects_incremental_readers_and_watches() {
        let mut board = Board::default();
        let (a, b) = (ProjectId(1), ProjectId(2));
        board.apply(b, worker(2), post("stay"), 0).unwrap();
        board
            .apply(a, worker(1), defaulted("short", None, Some(1000)), 0)
            .unwrap();
        let cursor = board.sequence();
        let changed = board.changed(b);
        board.expire(1000);
        for board in [
            &board,
            &Board::from_live_checkpoint(board.live_checkpoint()).unwrap(),
        ] {
            assert_eq!(board.validate_cursor(a, cursor), Err("cursor_expired"));
            assert_eq!(board.changed(b), changed);
            assert!(board.list(b, cursor, None, 20).unwrap().entries.is_empty());
        }
        let cold = Board::from_durable_checkpoint(board.durable_checkpoint(), 1000).unwrap();
        assert_eq!(cold.list(b, 0, None, 20).unwrap().entries.len(), 1);
        assert!(cold.list(a, 0, None, 20).unwrap().entries.is_empty());
    }

    #[test]
    fn retention_changes_are_pageable_and_only_wake_affected_projects() {
        let mut board = Board::default();
        let project = ProjectId(1);
        let other = ProjectId(2);
        let ids: Vec<_> = (0..23)
            .map(|i| {
                board
                    .apply(project, worker(1), defaulted(&i.to_string(), None, None), 0)
                    .unwrap()
                    .unwrap()
            })
            .collect();
        board.apply(other, worker(2), post("short"), 0).unwrap();
        let cursor = board.sequence();
        let unchanged = board.changed(other);
        board.set_retention(Retention::Week, 1);

        assert!(board.changed(project) > cursor);
        assert_eq!(board.changed(other), unchanged);
        let mut cursor = cursor;
        let mut received = Vec::new();
        loop {
            let page = board.list(project, cursor, None, 3).unwrap();
            assert!(page.cursor > cursor);
            for entry in page.entries {
                assert_eq!(entry.message.expires_ms, Some(7 * TTL_MS));
                received.push(entry.message.id);
            }
            cursor = page.cursor;
            if !page.more {
                break;
            }
        }
        assert_eq!(received, ids);
        assert!(board
            .list(project, cursor, None, 3)
            .unwrap()
            .entries
            .is_empty());
    }

    #[test]
    fn removing_a_paused_project_marks_its_saved_pause_for_removal() {
        let mut board = Board::default();
        let project = ProjectId(3);
        board
            .apply(project, Author::Human, Operation::Pause { paused: true }, 0)
            .unwrap();
        let revision = board.durable_revision();
        board.retain_projects(&BTreeSet::new());
        assert!(board.durably_empty());
        assert!(board.durable_revision() > revision);
        assert_eq!(board.changed(project), 0);
    }

    #[test]
    fn a_full_relist_after_expiry_can_page_through_the_retained_posts() {
        let project = ProjectId(1);
        let mut board = Board::default();
        let retained: Vec<_> = (0..25)
            .map(|i| {
                board
                    .apply(
                        project,
                        Author::Human,
                        defaulted(&i.to_string(), None, None),
                        0,
                    )
                    .unwrap()
                    .unwrap()
            })
            .collect();
        board
            .apply(
                project,
                Author::Human,
                defaulted("short", None, Some(1000)),
                0,
            )
            .unwrap();
        let interrupted = board.list(project, 0, None, 20).unwrap();
        assert!(interrupted.more);
        board.expire(1000);
        assert_eq!(
            board.list(project, interrupted.cursor, None, 20),
            Err("cursor_expired")
        );
        let first = board.list(project, 0, None, 20).unwrap();
        let second = board.list(project, first.cursor, None, 20).unwrap();
        assert!(first.more && !second.more);
        let ids: Vec<_> = first
            .entries
            .iter()
            .chain(&second.entries)
            .map(|entry| entry.message.id)
            .collect();
        assert_eq!(ids, retained);

        // Rotating the event journal during a fresh scan cannot hide a
        // retained post. Its current value still lives in the projection.
        for n in 0..4100 {
            board
                .apply(
                    ProjectId(2),
                    Author::Human,
                    Operation::Pause { paused: n % 2 == 0 },
                    1100,
                )
                .unwrap();
        }
        assert_eq!(
            board.list(project, first.cursor, None, 20).unwrap().entries,
            second.entries
        );
        let restored = Board::from_live_checkpoint(board.live_checkpoint()).unwrap();
        assert_eq!(
            restored
                .list(project, first.cursor, None, 20)
                .unwrap()
                .entries,
            second.entries
        );
    }

    #[test]
    fn retention_bounds_defaulted_posts_and_keeps_explicit_shorter_expiry() {
        let mut b = Board::default();
        let p = ProjectId(1);
        // The default is one day; an omitted lifetime follows the Workspace.
        let root = b
            .apply(p, worker(1), defaulted("root", None, None), 0)
            .unwrap()
            .unwrap();
        let short = b
            .apply(p, worker(1), defaulted("short", None, Some(5000)), 0)
            .unwrap()
            .unwrap();
        let reply = b
            .apply(p, worker(2), defaulted("reply", Some(root), None), 1000)
            .unwrap()
            .unwrap();
        let brief_reply = b
            .apply(
                p,
                worker(2),
                defaulted("brief", Some(root), Some(2000)),
                1000,
            )
            .unwrap()
            .unwrap();
        let expiry = |b: &Board, id| b.entry(id).unwrap().message.expires_ms;
        assert_eq!(expiry(&b, root), Some(TTL_MS));
        assert_eq!(expiry(&b, short), Some(5000));
        assert_eq!(expiry(&b, reply), Some(TTL_MS));
        assert_eq!(expiry(&b, brief_reply), Some(3000));
        assert_eq!(
            b.apply(
                p,
                worker(1),
                defaulted("long", None, Some(MAX_TTL_MS + 1)),
                0
            ),
            Err("invalid_post")
        );
        // An explicit lifetime longer than the retention cannot outlive it.
        let capped = b
            .apply(p, worker(1), defaulted("capped", None, Some(7 * TTL_MS)), 0)
            .unwrap()
            .unwrap();
        assert_eq!(expiry(&b, capped), Some(TTL_MS));
        b.apply(
            p,
            worker(1),
            Operation::Claim {
                id: root,
                revision: 0,
                lease_ms: 3_600_000,
            },
            0,
        )
        .unwrap();

        // Forever removes only the time limit of posts without a shorter one.
        let sequence = b.sequence();
        let durable = b.durable_revision();
        b.set_retention(Retention::Forever, 1500);
        assert_eq!(b.retention(), Retention::Forever);
        assert!(b.sequence() > sequence);
        assert!(b.durable_revision() > durable);
        assert_eq!(expiry(&b, root), None);
        assert_eq!(expiry(&b, reply), None);
        assert_eq!(expiry(&b, short), Some(5000));
        assert_eq!(expiry(&b, brief_reply), Some(3000));
        assert_eq!(expiry(&b, capped), Some(7 * TTL_MS));
        assert_eq!(
            b.entry(root).unwrap().claim.as_ref().unwrap().expires_ms,
            3_600_000
        );
        assert_eq!(b.deadline(), Some(3000));
        // The journal payload copies carry the same deadlines as the projection.
        for (_, event) in &b.journal {
            if let Event::Post(message) = event {
                assert_eq!(message, &b.entry(message.id).unwrap().message);
            }
        }
        // Selecting the same value again is not a transition.
        let sequence = b.sequence();
        b.set_retention(Retention::Forever, 1500);
        assert_eq!(b.sequence(), sequence);
        let forever = b
            .apply(p, worker(3), defaulted("forever", None, None), 2000)
            .unwrap()
            .unwrap();
        assert_eq!(expiry(&b, forever), None);
        b.expire(u64::MAX);
        assert!(b.entry(root).is_some() && b.entry(reply).is_some());
        assert!(b.entry(forever).is_some());
        assert!(b.entry(short).is_none() && b.entry(brief_reply).is_none());
        assert!(b.entry(capped).is_none());
        assert_eq!(b.deadline(), None);
        assert!(b.reader_entries(p, 0, u64::MAX).count() == 3);

        // Shortening applies from each post's creation time and erases at once.
        let erasures = b.erasures();
        b.set_retention(Retention::Week, 7 * TTL_MS + 1500);
        assert!(b.entry(root).is_none() && b.entry(reply).is_none());
        assert_eq!(expiry(&b, forever), Some(7 * TTL_MS + 2000));
        assert_eq!(b.erasures(), erasures + 2);
        assert!(!b
            .journal
            .iter()
            .any(|(_, e)| matches!(e, Event::Post(m) if m.id == root || m.id == reply)));
        // An expired post is never revived by a later, longer retention.
        b.set_retention(Retention::Forever, 7 * TTL_MS + 2000);
        assert!(b.entry(forever).is_none());
        assert!(b.durably_empty());
    }
    #[test]
    fn idempotent_retry_distinguishes_default_and_explicit_lifetime() {
        let mut b = Board::default();
        let p = ProjectId(1);
        let id = b
            .apply(p, worker(1), defaulted("k", None, None), 0)
            .unwrap();
        assert_eq!(b.apply(p, worker(1), defaulted("k", None, None), 5), Ok(id));
        assert_eq!(
            b.apply(p, worker(1), defaulted("k", None, Some(TTL_MS)), 5),
            Err("idempotency_conflict")
        );
        // An older client that omits the field in JSON follows the retention.
        let op: Operation = serde_json::from_str(
            r#"{"op":"post","kind":"finding","body":"b","reply_to":null,"to":null,"task":null,"key":"k"}"#,
        )
        .unwrap();
        assert!(matches!(op, Operation::Post { ttl_ms: None, .. }));
        let op: Operation = serde_json::from_str(
            r#"{"op":"post","kind":"finding","body":"b","reply_to":null,"to":null,"task":null,"key":"k","ttl_ms":86400000}"#,
        )
        .unwrap();
        assert!(matches!(
            op,
            Operation::Post {
                ttl_ms: Some(TTL_MS),
                ..
            }
        ));
    }
    #[test]
    fn idempotency_scope_claims_expiration_and_erasure() {
        let mut b = Board::default();
        let p = ProjectId(1);
        let id = b.apply(p, worker(1), post("a"), 0).unwrap().unwrap();
        assert_eq!(b.apply(p, worker(1), post("a"), 1), Ok(Some(id)));
        assert!(b
            .list(ProjectId(2), 0, None, 50)
            .unwrap()
            .entries
            .is_empty());
        assert_eq!(
            b.apply(ProjectId(2), worker(2), Operation::Ack { id }, 0),
            Err("not_found")
        );
        b.apply(
            p,
            worker(1),
            Operation::Claim {
                id,
                revision: 0,
                lease_ms: 2000,
            },
            0,
        )
        .unwrap();
        assert_eq!(
            b.apply(
                p,
                worker(2),
                Operation::Claim {
                    id,
                    revision: 0,
                    lease_ms: 2000
                },
                0
            ),
            Err("revision_conflict")
        );
        b.forget_pane(PaneId(1));
        assert!(b.entries[&id].claim.is_none());
        assert_eq!(b.entries[&id].revision, 2);
        b.expire(10_000);
        assert!(b.entries.is_empty());
        assert_eq!(b.bytes, 0);
        assert_eq!(b.deadline(), None);
        assert!(!b.journal.iter().any(|(_, e)| matches!(e, Event::Post(_))));
        assert_eq!(b.list(p, 1, None, 50), Err("cursor_expired"));
    }
    #[test]
    fn pause_revocation_limits_and_noop_ack() {
        let mut b = Board::default();
        let p = ProjectId(1);
        let id = b.apply(p, worker(1), post("a"), 0).unwrap().unwrap();
        b.apply(p, worker(2), Operation::Ack { id }, 0).unwrap();
        let seq = b.sequence();
        b.apply(p, worker(2), Operation::Ack { id }, 0).unwrap();
        assert_eq!(seq, b.sequence());
        b.apply(p, Author::Human, Operation::Pause { paused: true }, 0)
            .unwrap();
        assert_eq!(b.apply(p, worker(1), post("b"), 0), Err("paused"));
        assert!(b
            .apply(p, worker(1), Operation::Clear { confirmed: true }, 0)
            .is_err());
        b.apply(p, Author::Human, Operation::Revoke { author: worker(1) }, 0)
            .unwrap();
        assert_eq!(b.authorize(&worker(1)), Err("revoked"));
        b.apply(p, Author::Human, Operation::Clear { confirmed: true }, 0)
            .unwrap();
        assert_eq!(b.bytes, 0);
    }
    #[test]
    fn retries_of_capped_replies_and_task_claims_are_safe() {
        let mut b = Board::default();
        let p = ProjectId(1);
        let id = b.apply(p, worker(1), post("root"), 0).unwrap().unwrap();
        let mut reply = post("reply");
        if let Operation::Post { reply_to, .. } = &mut reply {
            *reply_to = Some(id);
        }
        let rid = b.apply(p, worker(2), reply.clone(), 5000).unwrap();
        assert_eq!(b.apply(p, worker(2), reply, 6000), Ok(rid));
        let mut offer = post("task-a");
        if let Operation::Post { task, .. } = &mut offer {
            *task = Some(42);
        }
        let a = b.apply(p, worker(1), offer.clone(), 0).unwrap().unwrap();
        if let Operation::Post { key, .. } = &mut offer {
            *key = "task-b".into();
        }
        let other = b.apply(p, worker(2), offer, 0).unwrap().unwrap();
        b.apply(
            p,
            worker(1),
            Operation::Claim {
                id: a,
                revision: 0,
                lease_ms: 1000,
            },
            0,
        )
        .unwrap();
        assert_eq!(
            b.apply(
                p,
                worker(2),
                Operation::Claim {
                    id: other,
                    revision: 0,
                    lease_ms: 1000
                },
                0
            ),
            Err("task_already_claimed")
        );
        b.expire(1000);
        b.apply(
            p,
            worker(2),
            Operation::Claim {
                id: other,
                revision: 0,
                lease_ms: 1000,
            },
            1000,
        )
        .unwrap();
        b.retain_projects(&BTreeSet::new());
        assert!(b.entries.is_empty());
        assert_eq!(b.bytes, 0);
    }
    #[test]
    fn quota_pagination_and_real_work_budget() {
        let mut b = Board::default();
        let p = ProjectId(1);
        let begin = std::time::Instant::now();
        for n in 0..1000 {
            let mut op = post(&n.to_string());
            if let Operation::Post { body, ttl_ms, .. } = &mut op {
                *body = "x".repeat(100);
                *ttl_ms = Some(TTL_MS);
            }
            b.apply(p, worker(n / 30 + 1), op, n * 1000).unwrap();
        }
        assert_eq!(
            b.apply(p, worker(100), post("full"), 1_000_000),
            Err("capacity")
        );
        let first = b.list(p, 0, None, usize::MAX).unwrap();
        assert_eq!(first.entries.len(), 20);
        assert!(first.more);
        let second = b.list(p, first.cursor, None, 20).unwrap();
        assert!(second.entries[0].changed > first.cursor);
        eprintln!(
            "inbox: 1000 bounded posts in {:?}, payload accounting {} bytes",
            begin.elapsed(),
            b.bytes
        );
        assert!(begin.elapsed().as_secs() < 5);
        assert!(Board::default().deadline().is_none());
    }
    #[test]
    fn short_lived_posts_cannot_evade_sender_rate_limit() {
        let mut b = Board::default();
        let p = ProjectId(1);
        for n in 0..30 {
            let mut op = post(&n.to_string());
            if let Operation::Post { ttl_ms, .. } = &mut op {
                *ttl_ms = Some(1000);
            }
            b.apply(p, worker(1), op, n * 1000).unwrap();
        }
        assert_eq!(
            b.apply(p, worker(1), post("too-fast"), 30_000),
            Err("rate_limited")
        );
        b.apply(p, worker(1), post("next-window"), 60_000).unwrap();
    }

    #[test]
    fn repeated_administration_is_idle_and_project_removal_preserves_peers() {
        let mut b = Board::default();
        let project = ProjectId(1);
        let peer = ProjectId(2);
        let removed = b
            .apply(project, worker(1), post("removed"), 0)
            .unwrap()
            .unwrap();
        let kept = b.apply(peer, worker(2), post("kept"), 0).unwrap().unwrap();
        for paused in [false, true, false] {
            b.apply(project, Author::Human, Operation::Pause { paused }, 0)
                .unwrap();
            let sequence = b.sequence();
            b.apply(project, Author::Human, Operation::Pause { paused }, 0)
                .unwrap();
            assert_eq!(b.sequence(), sequence);
        }
        let revoke = Operation::Revoke { author: worker(1) };
        b.apply(project, Author::Human, revoke.clone(), 0).unwrap();
        let sequence = b.sequence();
        b.apply(project, Author::Human, revoke, 0).unwrap();
        assert_eq!(b.sequence(), sequence);
        b.retain_projects(&BTreeSet::from([peer]));
        assert!(b.entry(removed).is_none());
        assert!(b.entry(kept).is_some());
        assert!(!b
            .journal
            .iter()
            .any(|(_, e)| matches!(e, Event::Post(m) if m.project == project)));
        let sequence = b.sequence();
        b.retain_projects(&BTreeSet::from([peer]));
        assert_eq!(b.sequence(), sequence);
    }
}
