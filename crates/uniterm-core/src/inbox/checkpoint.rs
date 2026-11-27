//! Inbox transfer between engines and to the protected durable copy. Restore
//! derived lookup structures from the bounded projection and retain
//! revocations even after journal rotation. The durable form never carries
//! claims, revocations, agent receipts or rate windows across a cold restart.

use super::*;

/// Transfer representation shared only by compatible live engines.
#[derive(Serialize, Deserialize)]
pub struct LiveBoard {
    journal: VecDeque<(u64, Event)>,
    entries: Vec<Entry>,
    paused: BTreeSet<ProjectId>,
    revoked: BTreeSet<Author>,
    sequence: u64,
    floor: u64,
    // An older engine reads the maximum as its conservative Workspace floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_floors: Option<BTreeMap<ProjectId, u64>>,
    changed: BTreeMap<ProjectId, u64>,
    rates: Vec<(Author, VecDeque<u64>)>,
    #[serde(default)]
    retention: Retention,
    #[serde(default)]
    prior: u64,
    #[serde(default)]
    legacy_ttl_through: u64,
}

impl LiveBoard {
    /// Schema 1 recorded a one-day TTL for both a default post and an explicitly
    /// one-day post. Allow an omitted TTL on retries of these original IDs only.
    pub fn mark_legacy_ttl_defaults(&mut self) {
        self.legacy_ttl_through = self.sequence;
    }
}

impl Board {
    /// Export the ephemeral journal and authorization state to the next
    /// engine of this live Workspace. A cold restart uses the durable projection.
    pub fn live_checkpoint(&self) -> LiveBoard {
        LiveBoard {
            journal: self.journal.clone(),
            entries: self.entries.values().cloned().collect(),
            paused: self.paused.clone(),
            revoked: self.revoked.clone(),
            sequence: self.sequence,
            floor: self.floors.values().copied().max().unwrap_or(0),
            project_floors: Some(self.floors.clone()),
            changed: self.changed.clone(),
            rates: self
                .rates
                .iter()
                .map(|(author, times)| (author.clone(), times.clone()))
                .collect(),
            retention: self.retention,
            prior: self.prior,
            legacy_ttl_through: self.legacy_ttl_through,
        }
    }

    /// The state a cold restart or Workspace rename may restore: posts, the
    /// human's receipts and pauses, and the retention they were projected
    /// under. Every sealed store write runs this, so it is built directly:
    /// the journal's payload copies, claims, revocations, agent receipts and
    /// rate windows are never cloned, and nothing conferring authority is
    /// ever present to be written.
    pub fn durable_checkpoint(&self) -> LiveBoard {
        LiveBoard {
            journal: VecDeque::new(),
            entries: self
                .entries
                .values()
                .map(|entry| Entry {
                    message: entry.message.clone(),
                    revision: entry.revision,
                    changed: entry.changed,
                    acknowledged: entry
                        .acknowledged
                        .get(&Author::Human)
                        .cloned()
                        .into_iter()
                        .collect(),
                    claim: None,
                })
                .collect(),
            paused: self.paused.clone(),
            revoked: BTreeSet::new(),
            sequence: self.sequence,
            floor: self.sequence,
            project_floors: None,
            changed: self.changed.clone(),
            rates: Vec::new(),
            retention: self.retention,
            prior: self.sequence,
            legacy_ttl_through: self.legacy_ttl_through,
        }
    }

    /// Restore posts under a new epoch. Original deadlines apply, including
    /// any that passed while the Workspace was stopped.
    pub fn from_durable_checkpoint(mut saved: LiveBoard, now: u64) -> Result<Self, &'static str> {
        // Stored bytes are not trusted to be in durable form. Drop everything
        // a cold restart must not revive before validating, so a stored lease,
        // revocation, agent receipt or rate window can neither take effect nor
        // decide whether the posts beside it load.
        saved.journal.clear();
        saved.revoked.clear();
        saved.rates.clear();
        // The caller starts a new epoch, so no previous cursor is valid.
        saved.floor = 0;
        saved.project_floors = None;
        saved.prior = saved.sequence;
        for entry in &mut saved.entries {
            entry.claim = None;
            entry.acknowledged.retain(|author| *author == Author::Human);
        }
        let mut board = Self::from_live_checkpoint(saved)?;
        board.expire(now);
        // Cold readers obtain a new epoch and have no incremental history.
        // Their initial 20-row list must still be pageable through older posts.
        board.floors.clear();
        Ok(board)
    }

    /// Rebuild indexes and payload accounting before the replacement may
    /// answer an inbox request. Keep original message and lease deadlines.
    pub fn from_live_checkpoint(saved: LiveBoard) -> Result<Self, &'static str> {
        if saved.sequence == u64::MAX
            || saved.floor > saved.sequence
            || saved.project_floors.as_ref().is_some_and(|floors| {
                floors.iter().any(|(project, &floor)| {
                    floor == 0 || floor > saved.floor || !saved.changed.contains_key(project)
                }) || floors.values().copied().max().unwrap_or(0) != saved.floor
            })
            || saved.journal.len() > 4096
            || saved.entries.len() > 4000
            || saved.rates.len() > 1024
            || saved.revoked.len() > 1024
            || saved.revoked.contains(&Author::Human)
            || saved.prior > saved.sequence
            || saved.legacy_ttl_through > saved.sequence
            || saved
                .changed
                .values()
                .any(|&n| n == 0 || n > saved.sequence)
        {
            return Err("invalid inbox checkpoint");
        }
        let mut last = 0;
        for (seq, _) in &saved.journal {
            if *seq <= last || *seq > saved.sequence {
                return Err("invalid inbox journal cursor");
            }
            last = *seq;
        }
        let mut board = Self {
            journal: saved.journal,
            paused: saved.paused,
            revoked: saved.revoked,
            sequence: saved.sequence,
            floors: saved.project_floors.unwrap_or_else(|| {
                if saved.floor == 0 {
                    BTreeMap::new()
                } else {
                    saved
                        .changed
                        .keys()
                        .map(|&project| (project, saved.floor))
                        .collect()
                }
            }),
            changed: saved.changed,
            retention: saved.retention,
            prior: saved.prior,
            legacy_ttl_through: saved.legacy_ttl_through,
            ..Self::default()
        };
        let mut acknowledgements = 0;
        for entry in saved.entries {
            let message = &entry.message;
            if message.id == 0
                || message.id > entry.changed
                || entry.changed > board.changed(message.project)
                || entry.revision > entry.changed
                || message.thread == 0
                || message.thread > message.id
                || message
                    .ttl_ms
                    .is_some_and(|ttl| !(1000..=MAX_TTL_MS).contains(&ttl))
                || board
                    .expiry(message.created_ms, message.ttl_ms)
                    .is_some_and(|limit| message.expires_ms.is_none_or(|at| at > limit))
                || message.body.is_empty()
                || message.payload_bytes() > BODY_LIMIT
                || message
                    .references
                    .iter()
                    .any(|r| r.thread == 0 || r.thread > r.message || r.message >= message.id)
                || message.key.is_empty()
                || message.key.len() > 128
                || entry.acknowledged.len() > 64
                || entry.claim.as_ref().is_some_and(|claim| {
                    claim.revision != entry.revision || board.authorize(&claim.owner).is_err()
                })
            {
                return Err("invalid live inbox entry");
            }
            acknowledgements += entry.acknowledged.len();
            board.bytes += 2 * message.payload_bytes();
            board.index_reply(message);
            let ids = board.projects.entry(message.project).or_default();
            if acknowledgements > 4096
                || board.bytes > BYTE_LIMIT
                || ids.len() >= PROJECT_LIMIT
                || !ids.insert(message.id)
                || board.entries.insert(message.id, entry).is_some()
            {
                return Err("invalid live inbox capacity or duplicate message");
            }
        }
        for (seq, event) in &board.journal {
            if let Event::Post(message) = event {
                if message.id != *seq
                    || !board
                        .entries
                        .get(seq)
                        .is_some_and(|entry| &entry.message == message)
                {
                    return Err("inbox journal does not match its projection");
                }
            }
        }
        for (author, times) in saved.rates {
            if times.len() > 30 || board.rates.insert(author, times).is_some() {
                return Err("invalid live inbox rate limit");
            }
        }
        // Earlier engines could hand off rows older than their pagination
        // floor. Preserve the ability to page through a fresh listing.
        for project in board.floors.keys().copied().collect::<Vec<_>>() {
            board.refresh_cursors_below_floor(project);
        }
        Ok(board)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn post(key: String) -> Operation {
        Operation::Post {
            kind: Kind::Request,
            body: "build".into(),
            reply_to: None,
            to: None,
            task: None,
            key,
            ttl_ms: Some(20_000),
        }
    }

    #[test]
    fn live_board_keeps_lease_revision_rate_limits_revocations_and_retention() {
        let project = ProjectId(3);
        let writer = Author::Agent {
            pane: PaneId(2),
            invocation: 4,
        };
        let revoked = Author::Agent {
            pane: PaneId(5),
            invocation: 7,
        };
        let mut source = Board::default();
        for key in 0..30 {
            source
                .apply(project, writer.clone(), post(key.to_string()), 1000)
                .unwrap();
        }
        source
            .apply(
                project,
                writer.clone(),
                Operation::Claim {
                    id: 1,
                    revision: 0,
                    lease_ms: 5000,
                },
                1100,
            )
            .unwrap();
        source
            .apply(
                project,
                Author::Human,
                Operation::Revoke {
                    author: revoked.clone(),
                },
                1200,
            )
            .unwrap();
        let saved = serde_json::to_vec(&source.live_checkpoint()).unwrap();
        let mut resumed =
            Board::from_live_checkpoint(serde_json::from_slice(&saved).unwrap()).unwrap();
        assert_eq!(
            resumed.list(project, 0, None, 40),
            source.list(project, 0, None, 40)
        );
        assert_eq!(resumed.deadline(), Some(6100));
        assert_eq!(resumed.authorize(&revoked), Err("revoked"));
        assert_eq!(
            resumed.apply(project, writer.clone(), post("duplicate".into()), 1250),
            Err("rate_limited")
        );
        assert_eq!(
            resumed.apply(project, writer, post("0".into()), 1250),
            Ok(Some(1))
        );
        resumed.expire(6100);
        assert!(resumed.entry(1).unwrap().claim.is_none());
        assert_eq!(resumed.entry(1).unwrap().revision, 2);
        resumed.expire(21_000);
        let mut saved = resumed.live_checkpoint();
        assert_eq!(saved.floor, resumed.sequence());
        assert!(saved.entries.is_empty());
        assert!(!saved
            .journal
            .iter()
            .any(|(_, e)| matches!(e, Event::Post(_))));
        saved.floor += 1;
        assert!(Board::from_live_checkpoint(saved).is_err());
    }

    #[test]
    fn finite_retention_bounds_restored_deadlines_even_without_a_shorter_explicit_ttl() {
        for ttl in [None, Some(MAX_TTL_MS)] {
            let mut board = Board::default();
            let mut operation = post("retained".into());
            if let Operation::Post { ttl_ms, .. } = &mut operation {
                *ttl_ms = ttl;
            }
            board
                .apply(ProjectId(1), Author::Human, operation, 1000)
                .unwrap();
            let mut cold = board.durable_checkpoint();
            cold.entries[0].message.expires_ms = Some(1000 + TTL_MS + 1);
            assert!(Board::from_durable_checkpoint(cold, 1001).is_err());
            let mut live = board.live_checkpoint();
            // Keep the journal consistent, leaving only the policy violation.
            live.entries[0].message.expires_ms = Some(1000 + TTL_MS + 1);
            if let Event::Post(message) = &mut live.journal[0].1 {
                message.expires_ms = live.entries[0].message.expires_ms;
            }
            assert!(Board::from_live_checkpoint(live).is_err());
        }
    }

    #[test]
    fn durable_board_restores_posts_without_authority_or_expired_bodies() {
        let project = ProjectId(3);
        let writer = Author::Agent {
            pane: PaneId(2),
            invocation: 4,
        };
        let revoked = Author::Agent {
            pane: PaneId(5),
            invocation: 7,
        };
        let follow = |key: &str, ttl_ms| {
            let mut op = post(key.into());
            if let Operation::Post { ttl_ms: ttl, .. } = &mut op {
                *ttl = ttl_ms;
            }
            op
        };
        let mut source = Board::default();
        source.set_retention(Retention::Week, 0);
        let kept = source
            .apply(project, writer.clone(), follow("kept", None), 1000)
            .unwrap()
            .unwrap();
        let brief = source
            .apply(project, writer.clone(), follow("brief", Some(20_000)), 1000)
            .unwrap()
            .unwrap();
        source
            .apply(
                project,
                writer.clone(),
                Operation::Claim {
                    id: kept,
                    revision: 0,
                    lease_ms: 3_600_000,
                },
                1100,
            )
            .unwrap();
        for author in [writer.clone(), Author::Human] {
            source
                .apply(project, author, Operation::Ack { id: kept }, 1100)
                .unwrap();
        }
        source
            .apply(
                project,
                Author::Human,
                Operation::Revoke {
                    author: revoked.clone(),
                },
                1200,
            )
            .unwrap();
        source
            .apply(
                ProjectId(9),
                Author::Human,
                Operation::Pause { paused: true },
                1200,
            )
            .unwrap();

        // The live form carries the journal and every authority-bearing field;
        // the durable form is built without them, not stripped afterwards.
        let live = source.live_checkpoint();
        assert!(live
            .journal
            .iter()
            .any(|(_, e)| matches!(e, Event::Post(_))));
        assert!(live.revoked.contains(&revoked) && !live.rates.is_empty());
        assert!(live.prior == 0 && live.floor == 0);
        let durable = source.durable_checkpoint();
        assert!(durable.journal.is_empty() && durable.revoked.is_empty());
        assert!(durable.rates.is_empty());
        assert_eq!(durable.floor, source.sequence());
        assert_eq!(durable.prior, source.sequence());
        assert_eq!(durable.sequence, live.sequence);
        assert_eq!(durable.changed, live.changed);
        assert_eq!(durable.paused, live.paused);
        assert_eq!(durable.entries.len(), live.entries.len());
        for (slim, full) in durable.entries.iter().zip(&live.entries) {
            assert_eq!(slim.message, full.message);
            assert_eq!((slim.revision, slim.changed), (full.revision, full.changed));
            assert!(slim.claim.is_none() && slim.acknowledged.len() <= 1);
        }
        assert!(live.entries.iter().any(|e| e.acknowledged.len() == 2));
        let stored = serde_json::to_string(&durable).unwrap();
        // Nothing that confers authority is written, even before it is read.
        assert!(!stored.contains("\"owner\""));
        assert!(!stored.contains("\"invocation\":7"));
        // Writing the durable form and reading the live form cold agree.
        assert_eq!(
            serde_json::to_string(
                &Board::from_durable_checkpoint(source.live_checkpoint(), 1300)
                    .unwrap()
                    .durable_checkpoint()
            )
            .unwrap(),
            stored
        );
        let reopen =
            |at| Board::from_durable_checkpoint(serde_json::from_str(&stored).unwrap(), at);
        let mut resumed = reopen(1300).unwrap();
        assert_eq!(resumed.retention(), Retention::Week);
        let entry = resumed.entry(kept).unwrap();
        assert_eq!(entry.message, source.entry(kept).unwrap().message);
        assert_eq!(entry.message.expires_ms, Some(1000 + 7 * TTL_MS));
        assert!(entry.claim.is_none());
        assert_eq!(entry.acknowledged, BTreeSet::from([Author::Human]));
        assert!(resumed.entry(brief).is_some());
        assert_eq!(resumed.authorize(&revoked), Ok(()));
        assert!(resumed.list(ProjectId(9), 0, None, 1).unwrap().paused);
        // The runtime rejects requests carrying the old epoch. Within the new
        // epoch an initial listing can continue through every retained post.
        let first_page = resumed.list(project, 0, None, 1).unwrap();
        assert!(first_page.more);
        assert_eq!(
            resumed
                .list(project, first_page.cursor, None, 1)
                .unwrap()
                .entries
                .len(),
            1
        );
        assert_eq!(resumed.list(project, 0, None, 20).unwrap().entries.len(), 2);
        // New ids continue after the restored sequence and cannot collide.
        let fresh = resumed
            .apply(project, writer.clone(), follow("kept", None), 1300)
            .unwrap()
            .unwrap();
        assert!(fresh > source.sequence());
        // The human keeps idempotent retries; a reused agent identity does not.
        let mine = source
            .apply(project, Author::Human, follow("mine", None), 1300)
            .unwrap();
        let again = Board::from_durable_checkpoint(source.durable_checkpoint(), 1300)
            .unwrap()
            .apply(project, Author::Human, follow("mine", None), 1400);
        assert_eq!(again, Ok(mine));
        // The lease could be claimed again, by anyone, at the stored revision.
        let revision = resumed.entry(kept).unwrap().revision;
        resumed
            .apply(
                project,
                revoked,
                Operation::Claim {
                    id: kept,
                    revision,
                    lease_ms: 1000,
                },
                1300,
            )
            .unwrap();

        // Deadlines that pass while stopped erase bodies before any reply.
        let later = reopen(21_000).unwrap();
        assert!(later.entry(brief).is_none() && later.entry(kept).is_some());
        let gone = reopen(1000 + 7 * TTL_MS).unwrap();
        assert!(gone.entry(kept).is_none());
        assert!(!gone.durably_empty(), "the human pause is still stored");

        // A stored lease or receipt is dropped on reading, never trusted.
        let mut forged = source.live_checkpoint();
        assert!(forged.entries.iter().any(|entry| entry.claim.is_some()));
        forged.revoked.clear();
        let forged = Board::from_durable_checkpoint(forged, 1300).unwrap();
        assert!(forged.entry(kept).unwrap().claim.is_none());
        assert_eq!(
            forged.entry(kept).unwrap().acknowledged,
            BTreeSet::from([Author::Human])
        );
        // Stored fields a live engine would refuse outright, or would honour,
        // are discarded before validation instead of failing or reviving.
        let mut hostile = source.durable_checkpoint();
        hostile.revoked = BTreeSet::from([Author::Human, writer.clone()]);
        hostile.floor = u64::MAX;
        hostile.prior = u64::MAX;
        hostile.journal = VecDeque::from([(u64::MAX, Event::Remove { id: kept })]);
        hostile.rates = vec![(writer.clone(), VecDeque::from(vec![1250; 31]))];
        for entry in &mut hostile.entries {
            entry.claim = Some(Claim {
                owner: writer.clone(),
                revision: entry.revision + 9,
                expires_ms: u64::MAX,
            });
            entry.acknowledged = (0..65)
                .map(|n| Author::Agent {
                    pane: PaneId(n),
                    invocation: n,
                })
                .collect();
        }
        let hostile = serde_json::to_string(&hostile).unwrap();
        assert!(Board::from_live_checkpoint(serde_json::from_str(&hostile).unwrap()).is_err());
        let mut cold =
            Board::from_durable_checkpoint(serde_json::from_str(&hostile).unwrap(), 1300).unwrap();
        assert!(cold.entry(kept).unwrap().claim.is_none());
        assert!(cold.entry(kept).unwrap().acknowledged.is_empty());
        assert_eq!(cold.authorize(&writer), Ok(()));
        assert_eq!(cold.deadline(), Some(21_000));
        assert_eq!(cold.list(project, 0, None, 20).unwrap().entries.len(), 3);
        // The stored rate window is gone, and so is the stored lease.
        assert!(cold
            .apply(project, writer.clone(), follow("after", None), 1300)
            .is_ok());
        let revision = cold.entry(kept).unwrap().revision;
        let stranger = Author::Agent {
            pane: PaneId(8),
            invocation: 1,
        };
        let claim = Operation::Claim {
            id: kept,
            revision,
            lease_ms: 1000,
        };
        assert_eq!(cold.apply(project, stranger, claim, 1300), Ok(Some(kept)));
        // Damage to what is kept still refuses the whole copy.
        let mut damaged = source.durable_checkpoint();
        damaged.entries[0].revision = damaged.entries[0].changed + 1;
        assert!(Board::from_durable_checkpoint(damaged, 1300).is_err());
        // A deadline-free post is valid only under the retention that allows it.
        let mut unbounded = source.durable_checkpoint();
        unbounded.entries[0].message.expires_ms = None;
        assert!(Board::from_durable_checkpoint(unbounded, 1300).is_err());
        let mut forever = Board::default();
        forever.set_retention(Retention::Forever, 0);
        forever
            .apply(project, writer, follow("always", None), 1000)
            .unwrap();
        let forever = Board::from_durable_checkpoint(forever.durable_checkpoint(), u64::MAX);
        assert_eq!(
            forever
                .unwrap()
                .list(project, 0, None, 5)
                .unwrap()
                .entries
                .len(),
            1
        );
    }
}
