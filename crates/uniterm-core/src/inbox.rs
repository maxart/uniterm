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
/// One immutable post; acknowledgement and claims live in subsequent events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub project: ProjectId,
    pub author: Author,
    pub kind: Kind,
    pub body: String,
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
    journal: VecDeque<(u64, Event)>,
    entries: BTreeMap<u64, Entry>,
    projects: BTreeMap<ProjectId, BTreeSet<u64>>,
    paused: BTreeSet<ProjectId>,
    revoked: BTreeSet<Author>,
    sequence: u64,
    floor: u64,
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
                self.bytes += message.body.len() * 2;
                self.projects
                    .entry(message.project)
                    .or_default()
                    .insert(message.id);
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
                    self.bytes -= e.message.body.len() * 2;
                    if let Some(ids) = self.projects.get_mut(&e.message.project) {
                        ids.remove(&id);
                        if ids.is_empty() {
                            self.projects.remove(&e.message.project);
                        }
                    }
                    self.floor = self.sequence;
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
        for &id in ids {
            self.record(Event::Remove { id });
        }
        self.journal.retain(|(_, event)| match event {
            Event::Post(message) => self.entries.contains_key(&message.id),
            _ => true,
        });
        self.refresh_cursors_below_floor();
    }

    /// A removal invalidates old incremental views. Give retained rows fresh
    /// cursors in their previous order so a full relist can page past the gap.
    fn refresh_cursors_below_floor(&mut self) {
        let mut older: Vec<_> = self
            .entries
            .values_mut()
            .filter(|entry| entry.changed < self.floor)
            .collect();
        older.sort_unstable_by_key(|entry| entry.changed);
        for entry in older {
            self.sequence += 1;
            entry.changed = self.sequence;
            self.changed.insert(entry.message.project, self.sequence);
        }
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
    pub fn validate_cursor(&self, after: u64) -> Result<(), &'static str> {
        if after > self.sequence || (after != 0 && after < self.floor) {
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

    /// Project-indexed pagination; message contents are untrusted attributed data.
    pub fn list(
        &self,
        project: ProjectId,
        after: u64,
        thread: Option<u64>,
        limit: usize,
    ) -> Result<Page, &'static str> {
        self.validate_cursor(after)?;
        let mut entries: Vec<_> = self
            .projects
            .get(&project)
            .into_iter()
            .flatten()
            .filter_map(|id| self.entries.get(id))
            .filter(|e| e.changed > after && thread.is_none_or(|t| e.message.thread == t))
            .collect();
        entries.sort_by_key(|e| e.changed);
        let limit = limit.clamp(1, 20);
        let more = entries.len() > limit;
        entries.truncate(limit);
        let cursor = if more {
            entries.last().map_or(after, |e| e.changed)
        } else {
            self.sequence
        };
        Ok(Page {
            entries: entries.into_iter().cloned().collect(),
            cursor,
            floor: self.floor,
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
        match op {
            Operation::Credential if !human => {
                self.record(Event::Grant { author });
                Ok(None)
            }
            Operation::Post {
                kind,
                body,
                reply_to,
                to,
                task,
                key,
                ttl_ms,
            } => {
                if body.trim().is_empty()
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
                            && m.body == body
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
                if self.projects.get(&project).map_or(0, BTreeSet::len) >= PROJECT_LIMIT
                    || self.bytes + body.len() * 2 > BYTE_LIMIT
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
                        .projects
                        .get(&project)
                        .into_iter()
                        .flatten()
                        .filter_map(|id| self.entries.get(id))
                        .filter(|e| e.message.thread == parent.message.thread)
                        .count()
                        >= 100
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
