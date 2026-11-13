//! Core-owned backlog for the runtime seam. Normal dispatch never waits;
//! only explicit shutdown may wait for channel readiness within its deadline.
//!
//! Replaceable observations move to the tail when superseded, so a newer
//! checkpoint can never overtake the event records it includes. Lifecycle
//! operations are barriers: a save must not move across a delete or rename.

use std::collections::VecDeque;

use crossbeam_channel::{Receiver, Sender, TrySendError};
use uniterm_proto::{AgentToCore, CoreToAgent};

const MAX_MESSAGES: usize = 512;
const MAX_BYTES: usize = 128 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Outbox {
    pending: VecDeque<(CoreToAgent, usize)>,
    bytes: usize,
    failure: Option<&'static str>,
}

impl Outbox {
    pub(super) fn push(&mut self, mut message: CoreToAgent) {
        if self.failure.is_some() {
            return;
        }
        let replace = self
            .pending
            .iter()
            .rposition(|(old, _)| same_projection(old, &message));
        if let Some(index) = replace.filter(|index| {
            self.pending
                .iter()
                .skip(index + 1)
                .all(|(item, _)| !barrier(item))
        }) {
            let (old, size) = self.pending.remove(index).expect("existing queue entry");
            self.bytes -= size;
            if let (
                CoreToAgent::PaneEvidence {
                    process_changed: previous,
                    ..
                },
                CoreToAgent::PaneEvidence {
                    process_changed, ..
                },
            ) = (old, &mut message)
            {
                *process_changed |= previous;
            }
        }
        let encoded_size = match &message {
            CoreToAgent::SnapshotSave { name, snapshot } => {
                Some(snapshot.retained_bytes().saturating_add(name.len() + 32) as u64)
            }
            CoreToAgent::EventAppend { name, line }
            | CoreToAgent::WorkspaceCatalogAppend { name, line } => {
                Some((name.len() + line.len() + 32) as u64)
            }
            CoreToAgent::PaneEvidence {
                tail,
                title,
                bound_agent,
                ..
            } => Some(
                (tail.len() + title.len() + bound_agent.as_ref().map_or(0, String::len) + 64)
                    as u64,
            ),
            CoreToAgent::DevServerEvidence { tail, .. } => Some((tail.len() + 32) as u64),
            _ => bincode::serialized_size(&message).ok(),
        };
        let size = encoded_size
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(usize::MAX);
        if self.pending.len() >= MAX_MESSAGES || size > MAX_BYTES.saturating_sub(self.bytes) {
            // Never continue past a missing ordered record and later publish
            // a checkpoint claiming it. The server exits with crash recovery
            // intact if a single dispatch exhausts the reserved headroom.
            self.failure = Some("agent runtime backlog exceeded its bounded capacity");
            return;
        }
        self.bytes += size;
        self.pending.push_back((message, size));
    }

    pub(super) fn flush(&mut self, sender: &Sender<CoreToAgent>) {
        while let Some((message, size)) = self.pending.pop_front() {
            match sender.try_send(message) {
                Ok(()) => self.bytes -= size,
                Err(TrySendError::Full(message)) => {
                    self.pending.push_front((message, size));
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.bytes -= size;
                    self.failure = Some("agent runtime disconnected with pending work");
                    break;
                }
            }
        }
    }

    /// Finish ordered work during shutdown without polling for reply traffic.
    /// Request capacity is progress even when the worker sends no reply.
    pub(super) fn flush_before_shutdown(
        &mut self,
        sender: &Sender<CoreToAgent>,
        replies: &Receiver<AgentToCore>,
        deadline: std::time::Instant,
    ) {
        let disconnected = crossbeam_channel::never();
        let mut replies = replies;
        while !self.is_empty() && std::time::Instant::now() < deadline {
            let (message, size) = self.pending.pop_front().expect("pending work");
            crossbeam_channel::select! {
                send(sender, message) -> result => {
                    self.bytes -= size;
                    if result.is_err() {
                        self.failure = Some("agent runtime disconnected with pending work");
                        break;
                    }
                }
                recv(replies) -> result => {
                    self.pending.push_front((message, size));
                    if result.is_err() {
                        // A disconnected receive is always ready. Exclude it
                        // so it cannot spin while request capacity is blocked.
                        replies = &disconnected;
                    }
                }
                default(deadline.saturating_duration_since(std::time::Instant::now())) => {
                    self.pending.push_front((message, size));
                    break;
                }
            }
        }
    }

    pub(super) fn backpressured(&self) -> bool {
        self.pending.len() >= MAX_MESSAGES / 2 || self.bytes >= MAX_BYTES / 2
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(super) fn check_health(&self) -> std::io::Result<()> {
        self.failure
            .map_or(Ok(()), |error| Err(std::io::Error::other(error)))
    }
}

fn same_projection(old: &CoreToAgent, new: &CoreToAgent) -> bool {
    match (old, new) {
        (CoreToAgent::SnapshotSave { name: a, .. }, CoreToAgent::SnapshotSave { name: b, .. })
        | (
            CoreToAgent::WorkspaceCatalogAppend { name: a, .. },
            CoreToAgent::WorkspaceCatalogAppend { name: b, .. },
        ) => a == b,
        (CoreToAgent::PaneEvidence { pane: a, .. }, CoreToAgent::PaneEvidence { pane: b, .. })
        | (
            CoreToAgent::DevServerEvidence { pane: a, .. },
            CoreToAgent::DevServerEvidence { pane: b, .. },
        ) => a == b,
        _ => false,
    }
}

fn barrier(message: &CoreToAgent) -> bool {
    !matches!(
        message,
        CoreToAgent::SnapshotSave { .. }
            | CoreToAgent::WorkspaceCatalogAppend { .. }
            | CoreToAgent::EventAppend { .. }
            | CoreToAgent::PaneEvidence { .. }
            | CoreToAgent::DevServerEvidence { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_flush_uses_request_readiness_without_reply_acknowledgements() {
        for replies_open in [true, false] {
            let (sender, incoming) = crossbeam_channel::bounded(0);
            let (reply_sender, replies) = crossbeam_channel::bounded(1);
            let reply_sender = replies_open.then_some(reply_sender);
            let mut outbox = Outbox::default();
            for pane in 0..400 {
                outbox.push(CoreToAgent::OscAgentEvent {
                    pane: uniterm_proto::PaneId(pane),
                    payload: String::new(),
                });
            }
            let worker = std::thread::spawn(move || {
                incoming
                    .iter()
                    .map(|message| match message {
                        CoreToAgent::OscAgentEvent { pane, .. } => pane,
                        _ => panic!("unexpected fixture request"),
                    })
                    .collect::<Vec<_>>()
            });
            let started = std::time::Instant::now();
            outbox.flush_before_shutdown(
                &sender,
                &replies,
                started + std::time::Duration::from_secs(2),
            );
            drop(sender);
            let processed = worker.join().unwrap();
            assert_eq!(
                processed,
                (0..400).map(uniterm_proto::PaneId).collect::<Vec<_>>()
            );
            assert!(outbox.is_empty());
            assert_eq!(outbox.bytes, 0);
            assert!(outbox.check_health().is_ok());
            eprintln!(
                "shutdown without replies (open={replies_open}): {:?}",
                started.elapsed()
            );
            drop(reply_sender);
        }
    }

    #[test]
    fn shutdown_deadline_preserves_unsent_work_and_disconnect_reports_failure() {
        let (sender, incoming) = crossbeam_channel::bounded(0);
        let replies = crossbeam_channel::never();
        let mut outbox = Outbox::default();
        for pane in 0..2 {
            outbox.push(CoreToAgent::OscAgentEvent {
                pane: uniterm_proto::PaneId(pane),
                payload: String::new(),
            });
        }
        let bytes = outbox.bytes;
        outbox.flush_before_shutdown(&sender, &replies, std::time::Instant::now());
        assert_eq!(outbox.pending.len(), 2);
        assert_eq!(outbox.bytes, bytes);
        assert!(outbox.check_health().is_ok());
        // A live but full request channel must wait only until the same
        // absolute deadline and preserve the front message for retry.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(10);
        outbox.flush_before_shutdown(&sender, &replies, deadline);
        assert!(std::time::Instant::now() >= deadline);
        assert_eq!(outbox.pending.len(), 2);
        assert_eq!(outbox.bytes, bytes);
        assert!(outbox.check_health().is_ok());
        drop(incoming);
        outbox.flush_before_shutdown(
            &sender,
            &replies,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
        );
        assert!(outbox.check_health().is_err());
        assert!(matches!(&outbox.pending.front().unwrap().0,
            CoreToAgent::OscAgentEvent { pane, .. } if *pane == uniterm_proto::PaneId(1)));
    }

    fn snapshot(byte: u8) -> CoreToAgent {
        CoreToAgent::SnapshotSave {
            name: "test".into(),
            snapshot: Box::new(uniterm_proto::checkpoint::Snapshot::new(
                0,
                u64::from(byte),
                uniterm_core::ProjectId(1),
                2,
                vec![],
                vec![],
            )),
        }
    }

    #[test]
    fn superseded_checkpoints_follow_every_ordered_event() {
        let mut outbox = Outbox::default();
        for sequence in 0..100 {
            outbox.push(CoreToAgent::EventAppend {
                name: "test".into(),
                line: sequence.to_string(),
            });
            outbox.push(snapshot(sequence));
        }
        assert_eq!(outbox.pending.len(), 101);
        for sequence in 0..100 {
            assert!(matches!(&outbox.pending[sequence].0,
                CoreToAgent::EventAppend { line, .. } if line == &sequence.to_string()));
        }
        assert!(
            matches!(&outbox.pending[100].0, CoreToAgent::SnapshotSave { snapshot, .. } if snapshot.next_pane_id == 99)
        );
    }

    #[test]
    fn lifecycle_barriers_preserve_save_delete_and_rename_order() {
        let mut outbox = Outbox::default();
        outbox.push(snapshot(1));
        outbox.push(CoreToAgent::SnapshotDelete {
            name: "test".into(),
        });
        outbox.push(snapshot(2));
        assert_eq!(outbox.pending.len(), 3);
    }

    #[test]
    fn full_transport_retains_a_bounded_ordered_prefix_without_blocking() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut outbox = Outbox::default();
        for sequence in 0..MAX_MESSAGES + 10 {
            outbox.push(CoreToAgent::EventAppend {
                name: "test".into(),
                line: sequence.to_string(),
            });
            outbox.flush(&tx);
        }
        assert!(outbox.backpressured());
        assert!(outbox.check_health().is_err());
        assert_eq!(outbox.pending.len(), MAX_MESSAGES);
        for sequence in 0..=MAX_MESSAGES {
            assert!(
                matches!(rx.try_recv().unwrap(), CoreToAgent::EventAppend { line, .. } if line == sequence.to_string())
            );
            outbox.flush(&tx);
        }
        assert!(outbox.is_empty());
    }

    #[test]
    fn idle_and_repeated_observations_retain_no_unbounded_work() {
        let mut outbox = Outbox::default();
        let (tx, rx) = crossbeam_channel::bounded(1);
        outbox.flush(&tx);
        assert!(rx.is_empty());
        for _ in 0..10_000 {
            outbox.push(CoreToAgent::DevServerEvidence {
                pane: uniterm_proto::PaneId(1),
                tail: "latest".into(),
            });
        }
        assert_eq!(outbox.pending.len(), 1);
        assert!(!outbox.backpressured());
    }

    #[test]
    fn coalesced_evidence_keeps_latest_invocation_and_discovery_requirement() {
        let mut outbox = Outbox::default();
        for (pid, changed, title) in [(10, true, "old"), (20, false, "latest")] {
            outbox.push(CoreToAgent::PaneEvidence {
                pane: uniterm_proto::PaneId(1),
                foreground_pid: Some(pid),
                process_changed: changed,
                tail: title.into(),
                title: title.into(),
                bound_agent: None,
            });
        }
        assert_eq!(outbox.pending.len(), 1);
        assert!(matches!(&outbox.pending[0].0, CoreToAgent::PaneEvidence {
            foreground_pid: Some(20), process_changed: true, title, ..
        } if title == "latest"));
    }
}
