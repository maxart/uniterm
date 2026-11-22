//! Bounded, ephemeral Project conversations. The runtime owns the event journal;
//! this pure reducer never performs I/O or schedules work (docs/29).
use crate::{PaneId, ProjectId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Explicit budgets keep collaboration from becoming an unbounded transcript.
pub const BODY_LIMIT: usize = 8192;
/// Per-Project live message count.
pub const PROJECT_LIMIT: usize = 1000;
/// Workspace payload budget, counting both journal and projection copies.
pub const BYTE_LIMIT: usize = 8 * 1024 * 1024;
/// Messages expire within one day even if nobody reads them.
pub const TTL_MS: u64 = 24 * 60 * 60 * 1000;

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
    pub expires_ms: u64,
    pub ttl_ms: u64,
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
#[derive(Clone, Debug)]
pub enum Event {
    Post(Message),
    Ack { id: u64, author: Author },
    Claim { id: u64, claim: Option<Claim> },
    Remove { id: u64 },
    Pause { project: ProjectId, paused: bool },
    Revoke { author: Author },
    Grant { author: Author },
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
        ttl_ms: u64,
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
}
impl Board {
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
            Event::Post(message) => {
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
                    self.bytes -= e.message.body.len() * 2;
                    if let Some(ids) = self.projects.get_mut(&e.message.project) {
                        ids.remove(&id);
                        if ids.is_empty() {
                            self.projects.remove(&e.message.project);
                        }
                    }
                    // Erase payload copies, not merely the visible projection.
                    self.journal
                        .retain(|(_, event)| !matches!(event, Event::Post(m) if m.id == id));
                    self.floor = self.sequence;
                }
            }
            Event::Pause { project, paused } => {
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
        while self.journal.len() > 4096 {
            if let Some((seq, _)) = self.journal.pop_front() {
                self.floor = self.floor.max(seq);
            }
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
    /// Structural deletion erases all payload copies and Project controls.
    pub fn retain_projects(&mut self, live: &BTreeSet<ProjectId>) {
        let gone: Vec<_> = self
            .changed
            .keys()
            .filter(|p| !live.contains(p))
            .copied()
            .collect();
        for project in gone {
            // Structural deletion already has its own authorization. Do not
            // manufacture a human confirmation for the inbox control command.
            let ids: Vec<_> = self
                .projects
                .get(&project)
                .into_iter()
                .flatten()
                .copied()
                .collect();
            for id in ids {
                self.record(Event::Remove { id });
            }
            self.changed.remove(&project);
            self.paused.remove(&project);
        }
    }
    /// One-shot deadline; an empty board has no idle wakeup.
    pub fn deadline(&self) -> Option<u64> {
        self.entries
            .values()
            .flat_map(|e| {
                [
                    Some(e.message.expires_ms),
                    e.claim.as_ref().map(|c| c.expires_ms),
                ]
            })
            .flatten()
            .min()
    }
    /// Expire bodies and leases on access or the earliest outstanding deadline.
    pub fn expire(&mut self, now: u64) {
        let remove: Vec<_> = self
            .entries
            .values()
            .filter(|e| e.message.expires_ms <= now)
            .map(|e| e.message.id)
            .collect();
        for id in remove {
            self.record(Event::Remove { id });
        }
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
                    || !(1000..=TTL_MS).contains(&ttl_ms)
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
                    if m.author == author && m.key == key {
                        return if m.kind == kind
                            && m.body == body
                            && m.reply_to == reply_to
                            && m.to == to
                            && m.task == task
                            && m.ttl_ms == ttl_ms
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
                        now.saturating_add(ttl_ms).min(parent.message.expires_ms),
                    )
                } else {
                    (id, now.saturating_add(ttl_ms))
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
                        expires_ms: now.saturating_add(lease_ms).min(e.message.expires_ms),
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
                for id in ids {
                    self.record(Event::Remove { id });
                }
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
            ttl_ms: 10_000,
        }
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
                *ttl_ms = TTL_MS;
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
                *ttl_ms = 1000;
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
