//! Process-private inbox transfer. Restore derived lookup structures from
//! the bounded projection and retain revocations even after journal rotation.

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
    changed: BTreeMap<ProjectId, u64>,
    rates: Vec<(Author, VecDeque<u64>)>,
}

impl Board {
    /// Export the ephemeral journal and authorization state to the next
    /// engine of this live Workspace. Restart recovery still starts empty.
    pub fn live_checkpoint(&self) -> LiveBoard {
        LiveBoard {
            journal: self.journal.clone(),
            entries: self.entries.values().cloned().collect(),
            paused: self.paused.clone(),
            revoked: self.revoked.clone(),
            sequence: self.sequence,
            floor: self.floor,
            changed: self.changed.clone(),
            rates: self
                .rates
                .iter()
                .map(|(author, times)| (author.clone(), times.clone()))
                .collect(),
        }
    }

    /// Rebuild indexes and payload accounting before the replacement may
    /// answer an inbox request. Keep original message and lease deadlines.
    pub fn from_live_checkpoint(saved: LiveBoard) -> Result<Self, &'static str> {
        if saved.sequence == u64::MAX
            || saved.floor > saved.sequence
            || saved.journal.len() > 4096
            || saved.entries.len() > 4000
            || saved.rates.len() > 1024
            || saved.revoked.len() > 1024
            || saved.revoked.contains(&Author::Human)
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
            floor: saved.floor,
            changed: saved.changed,
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
                || !(1000..=TTL_MS).contains(&message.ttl_ms)
                || message.expires_ms > message.created_ms.saturating_add(message.ttl_ms)
                || message.body.is_empty()
                || message.body.len() > BODY_LIMIT
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
            board.bytes += 2 * message.body.len();
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
            ttl_ms: 20_000,
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
}
