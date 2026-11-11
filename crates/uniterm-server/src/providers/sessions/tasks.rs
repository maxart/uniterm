//! Incremental task observations for already-owned native root transcripts.
//! Registration starts at EOF: older private history is never imported.

use super::Target;
use crossbeam_channel::Sender;
use mio::Waker;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use uniterm_core::tasks::TaskObservation;
use uniterm_proto::AgentToCore;

const READ_BUDGET: usize = 128 * 1024;
const LINE_LIMIT: usize = 128 * 1024;

pub(super) struct Tail {
    targets: Vec<Target>,
    provider: String,
    session: String,
    identity: (u64, u64),
    offset: u64,
    partial: Vec<u8>,
    discarded: bool,
    turn: Option<String>,
    last_prompt: Option<TaskObservation>,
}

fn open(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

impl Tail {
    pub(super) fn new(target: Target, session: String, path: &Path) -> Option<Self> {
        let declared = super::read_identity(&target.provider, path)?;
        if declared.id != session || declared.parent.is_some() {
            return None;
        }
        let file = open(path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() {
            return None;
        }
        Some(Self {
            provider: target.provider.clone(),
            targets: vec![target],
            session,
            identity: (metadata.dev(), metadata.ino()),
            offset: metadata.len(),
            partial: Vec::new(),
            discarded: false,
            turn: None,
            last_prompt: None,
        })
    }

    pub(super) fn add(&mut self, target: Target, session: &str) {
        if self.session == session
            && self.provider == target.provider
            && !self
                .targets
                .iter()
                .any(|old| old.pane == target.pane && old.pid == target.pid)
        {
            self.targets.push(target);
        }
    }

    pub(super) fn forget(&mut self, pane: uniterm_proto::PaneId) -> bool {
        self.targets.retain(|target| target.pane != pane);
        !self.targets.is_empty()
    }

    /// Returns true only while bytes from this concrete write remain unread.
    pub(super) fn drain(
        &mut self,
        path: &Path,
        output: &Sender<AgentToCore>,
        waker: &Waker,
    ) -> bool {
        let Ok(mut file) = open(path) else {
            return false;
        };
        let Ok(metadata) = file.metadata() else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        let identity = (metadata.dev(), metadata.ino());
        if identity != self.identity || metadata.len() < self.offset {
            // Rotation/replacement cannot lend a different file the old
            // session's authority. Revalidate, then skip its existing history.
            if super::read_identity(&self.provider, path)
                .is_some_and(|declared| declared.id == self.session && declared.parent.is_none())
            {
                self.identity = identity;
                self.offset = metadata.len();
                self.partial.clear();
                self.discarded = false;
                self.turn = None;
                self.last_prompt = None;
            }
            return false;
        }
        if metadata.len() == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut bytes = Vec::with_capacity(READ_BUDGET);
        if file
            .take(READ_BUDGET as u64)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return false;
        }
        self.offset += bytes.len() as u64;
        for byte in bytes {
            if byte != b'\n' {
                if self.partial.len() < LINE_LIMIT && !self.discarded {
                    self.partial.push(byte);
                } else {
                    self.partial.clear();
                    self.discarded = true;
                }
                continue;
            }
            if !self.discarded {
                if let Ok(line) = std::str::from_utf8(&self.partial) {
                    if let Some(observation) =
                        super::super::session_task_observation(&self.provider, line, &mut self.turn)
                    {
                        let duplicate = matches!(&observation, TaskObservation::Prompt { .. })
                            && self.last_prompt.as_ref() == Some(&observation);
                        if !duplicate {
                            self.last_prompt = match &observation {
                                TaskObservation::Prompt { .. } => Some(observation.clone()),
                                _ => None,
                            };
                            for target in &self.targets {
                                if output
                                    .send(AgentToCore::AgentTaskObserved {
                                        pane: target.pane,
                                        foreground_pid: target.pid,
                                        provider: self.provider.clone(),
                                        session_id: self.session.clone(),
                                        observation: observation.clone(),
                                    })
                                    .is_ok()
                                {
                                    let _ = waker.wake();
                                }
                            }
                        }
                    }
                }
            }
            self.partial.clear();
            self.discarded = false;
        }
        self.offset < metadata.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn native_task_tail_is_incremental_bounded_deduplicated_and_quiet() {
        let root = std::env::temp_dir().join(format!(
            "uniterm-task-tail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("root.jsonl");
        std::fs::write(&path, b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"root\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"OLD PRIVATE HISTORY\"}}\n").unwrap();
        let poll = mio::Poll::new().unwrap();
        let waker = Waker::new(poll.registry(), mio::Token(10)).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let target = Target {
            pane: uniterm_proto::PaneId(7),
            pid: Some(700),
            provider: "codex".into(),
        };
        let mut tail = Tail::new(target, "root".into(), &path).unwrap();
        assert!(!tail.drain(&path, &tx, &waker));
        assert!(rx.try_recv().is_err());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Repair login\"}}").unwrap();
        assert!(!tail.drain(&path, &tx, &waker));
        assert!(
            rx.try_recv().is_err(),
            "partial writes are not complete messages"
        );
        file.write_all(b"\n{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Repair login\"}]}}\n").unwrap();
        assert!(!tail.drain(&path, &tx, &waker));
        assert!(
            matches!(rx.try_recv().unwrap(), AgentToCore::AgentTaskObserved { pane: uniterm_proto::PaneId(7), foreground_pid: Some(700), session_id, observation: TaskObservation::Prompt { id: Some(id), text }, .. } if session_id == "root" && id == "turn-1" && text == "Repair login")
        );
        assert!(
            rx.try_recv().is_err(),
            "response_item and event_msg describe one accepted prompt"
        );
        file.write_all(&vec![b'x'; READ_BUDGET * 3]).unwrap();
        file.write_all(b"\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-1\"}}\n").unwrap();
        assert!(tail.drain(&path, &tx, &waker));
        assert!(tail.partial.len() <= LINE_LIMIT);
        while tail.drain(&path, &tx, &waker) {}
        assert!(
            matches!(rx.try_recv().unwrap(), AgentToCore::AgentTaskObserved { observation: TaskObservation::Finished { id: Some(id) }, .. } if id == "turn-1")
        );
        assert!(!tail.drain(&path, &tx, &waker));
        assert!(rx.try_recv().is_err());
        let replacement = root.join("replacement.jsonl");
        std::fs::write(
            &replacement,
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"foreign\"}}\n",
        )
        .unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(!tail.drain(&path, &tx, &waker));
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"foreign work\"}}\n").unwrap();
        assert!(!tail.drain(&path, &tx, &waker));
        assert!(
            rx.try_recv().is_err(),
            "replaced files need the exact declared session"
        );
        assert!(!tail.forget(uniterm_proto::PaneId(7)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_task_parsers_exclude_context_tools_children_and_heuristic_completion() {
        let parse = |provider: &str, value: serde_json::Value| {
            super::super::super::session_task_observation(
                provider,
                &value.to_string(),
                &mut Some("turn".into()),
            )
        };
        use serde_json::json;
        for text in [
            "# AGENTS.md instructions for repo",
            "<environment_context>private</environment_context>",
            "<turn_aborted>cancelled</turn_aborted>",
        ] {
            assert_eq!(
                parse(
                    "codex",
                    json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}})
                ),
                None
            );
        }
        assert_eq!(
            parse(
                "codex",
                json!({"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"secret"}]}})
            ),
            None
        );
        assert_eq!(
            parse(
                "claude",
                json!({"type":"user","uuid":"u","message":{"content":[{"type":"tool_result","content":"not a prompt"}]}})
            ),
            None
        );
        assert_eq!(
            parse(
                "claude",
                json!({"type":"user","uuid":"u","isSidechain":true,"message":{"content":"child work"}})
            ),
            None
        );
        assert_eq!(
            parse(
                "claude",
                json!({"type":"assistant","message":{"stop_reason":"tool_use"}})
            ),
            None
        );
        assert_eq!(
            parse(
                "claude",
                json!({"type":"assistant","isApiErrorMessage":true,"message":{"stop_reason":"end_turn"}})
            ),
            None
        );
        assert!(
            matches!(parse("claude", json!({"type":"user","uuid":"u","message":{"content":"Fix keyboard"}})), Some(TaskObservation::Prompt { id: Some(id), text }) if id == "u" && text == "Fix keyboard")
        );
        assert!(matches!(
            parse(
                "claude",
                json!({"type":"assistant","message":{"stop_reason":"end_turn"}})
            ),
            Some(TaskObservation::Finished { .. })
        ));
        assert!(matches!(
            parse(
                "pi",
                json!({"type":"message","id":"u","message":{"role":"user","content":[{"type":"text","text":"Fix keyboard"}]}})
            ),
            Some(TaskObservation::Prompt { .. })
        ));
        assert_eq!(
            parse(
                "pi",
                json!({"type":"message","id":"a","message":{"role":"assistant","stopReason":"stop"}})
            ),
            None
        );
    }
}
