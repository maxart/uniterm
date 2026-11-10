//! Ordered durability operations and their acknowledged state.
//!
//! The dispatcher awaits one operation at a time: event append, sync, capture
//! expansion, checkpoint publication, and clean-stop deletion cannot overtake
//! each other. Failure state belongs here, not to unrelated runtime services.

use std::collections::{HashMap, HashSet};
use uniterm_proto::AgentToCore;

#[derive(Default)]
pub(super) struct Persistence {
    poisoned_event_logs: HashSet<String>,
    failed_catalogs: HashSet<String>,
    saved_catalogs: HashMap<String, String>,
    uncompacted_event_bytes: HashMap<String, u64>,
}

impl Persistence {
    pub(super) fn events_failed(&self, workspace: &str) -> bool {
        self.poisoned_event_logs.contains(workspace)
    }
    pub(super) async fn save_snapshot(
        &mut self,
        name: String,
        snapshot: Box<uniterm_proto::checkpoint::Snapshot<uniterm_core::GridCapture>>,
    ) -> Option<AgentToCore> {
        if self.poisoned_event_logs.contains(&name) {
            Some(AgentToCore::DurabilityError {
                workspace: name,
                operation: "snapshot skipped after event-log failure".into(),
                error: "the event stream has an unrecorded sequence".into(),
            })
        } else {
            let workspace = name.clone();
            let result = tokio::task::spawn_blocking(move || {
                // The structural event is authoritative and was queued
                // before this checkpoint. Flush it first so a hard
                // power loss cannot leave a snapshot claiming a
                // sequence that never reached stable storage.
                crate::eventlog::sync(&name)?;
                crate::persist::save_capture(&name, snapshot.as_ref())
            })
            .await;
            match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(AgentToCore::DurabilityError {
                    workspace,
                    operation: "snapshot save".into(),
                    error: error.to_string(),
                }),
                Err(error) => Some(AgentToCore::DurabilityError {
                    workspace,
                    operation: "snapshot worker".into(),
                    error: error.to_string(),
                }),
            }
        }
    }
    pub(super) async fn delete_snapshot(&mut self, name: String) -> Option<AgentToCore> {
        if self.failed_catalogs.contains(&name) {
            Some(AgentToCore::DurabilityError {
                workspace: name,
                operation: "clean-stop checkpoint retained".into(),
                error: "the latest Workspace catalog write failed".into(),
            })
        } else {
            let _ = tokio::task::spawn_blocking(move || crate::persist::delete(&name)).await;
            None
        }
    }
    pub(super) async fn append_event(
        &mut self,
        name: String,
        line: String,
    ) -> (Option<AgentToCore>, Option<crate::eventlog::EventEnvelope>) {
        let reply = {
            let workspace = name.clone();
            if self.poisoned_event_logs.contains(&workspace) {
                Some(AgentToCore::DurabilityError {
                    workspace,
                    operation: "event append skipped after prior failure".into(),
                    error: "the durable stream is frozen at its last consistent prefix".into(),
                })
            } else {
                let stream_line = line.clone();
                let envelope = serde_json::from_str::<crate::eventlog::EventEnvelope>(&stream_line)
                    .ok()
                    .filter(|envelope| envelope.workspace == workspace);
                let sync = envelope.as_ref().is_some_and(|envelope| {
                    matches!(
                        envelope.event,
                        crate::eventlog::LogEvent::TimelineNote { .. }
                            | crate::eventlog::LogEvent::TimelinePrompt { .. }
                            | crate::eventlog::LogEvent::TimelineBoundary { .. }
                    )
                });
                // Check an existing stream on the first genuine append,
                // then only after another threshold of newly committed bytes.
                // No timer and no metadata lookup are added to idle work.
                let bytes = self
                    .uncompacted_event_bytes
                    .entry(workspace.clone())
                    .or_insert(crate::history::COMPACT_BYTES);
                *bytes = bytes.saturating_add(line.len() as u64);
                let maintenance = *bytes >= crate::history::COMPACT_BYTES
                    && !envelope.as_ref().is_some_and(|event| {
                        matches!(
                            event.event,
                            crate::eventlog::LogEvent::WorkspaceRenamed { .. }
                        )
                    });
                if maintenance {
                    *bytes = 0;
                }
                let result = tokio::task::spawn_blocking(move || {
                    crate::eventlog::append_line(&name, &line)?;
                    if sync {
                        crate::eventlog::sync(&name)?;
                    }
                    let maintenance_error = maintenance
                        .then(|| crate::history::compact_if_needed(&name).err())
                        .flatten();
                    Ok::<_, std::io::Error>(maintenance_error)
                })
                .await;
                match result {
                    Ok(Ok(maintenance_error)) => {
                        if maintenance_error.is_some() {
                            self.uncompacted_event_bytes
                                .insert(workspace.clone(), crate::history::COMPACT_BYTES);
                        }
                        let diagnostic =
                            maintenance_error.map(|error| AgentToCore::DurabilityError {
                                workspace,
                                operation: "history compaction".into(),
                                error: error.to_string(),
                            });
                        // Optional maintenance cannot revoke an acknowledged
                        // append or freeze later writes to an intact stream.
                        return (diagnostic, envelope);
                    }
                    Ok(Err(error)) => {
                        self.poisoned_event_logs.insert(workspace.clone());
                        Some(AgentToCore::DurabilityError {
                            workspace,
                            operation: "event append".into(),
                            error: error.to_string(),
                        })
                    }
                    Err(error) => {
                        self.poisoned_event_logs.insert(workspace.clone());
                        Some(AgentToCore::DurabilityError {
                            workspace,
                            operation: "event writer".into(),
                            error: error.to_string(),
                        })
                    }
                }
            }
        };
        (reply, None)
    }
    pub(super) async fn rename_events(
        &mut self,
        request: u64,
        old: String,
        new: String,
    ) -> Option<AgentToCore> {
        if let Some(bytes) = self.uncompacted_event_bytes.remove(&old) {
            self.uncompacted_event_bytes.insert(new.clone(), bytes);
        }
        let poisoned = self.poisoned_event_logs.remove(&old);
        let old_name = old.clone();
        let new_name = new.clone();
        let result = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            wait_rename_gate(&old);
            crate::eventlog::rename(&old, &new)
        })
        .await;
        let error = match result {
            Ok(Ok(())) => {
                if poisoned {
                    self.poisoned_event_logs.insert(new_name.clone());
                }
                None
            }
            Ok(Err(error)) => Some(error.to_string()),
            Err(error) => Some(format!("rename worker failed: {error}")),
        };
        if error.is_some() {
            self.poisoned_event_logs.insert(new_name.clone());
            if poisoned {
                self.poisoned_event_logs.insert(old_name.clone());
            }
        }
        Some(AgentToCore::EventRenamed {
            request,
            old: old_name,
            new: new_name,
            error,
        })
    }
    pub(super) async fn delete_events(&mut self, name: String) -> Option<AgentToCore> {
        self.poisoned_event_logs.remove(&name);
        self.uncompacted_event_bytes.remove(&name);
        let _ = tokio::task::spawn_blocking(move || crate::eventlog::delete(&name)).await;
        None
    }
    pub(super) async fn append_catalog(
        &mut self,
        name: String,
        line: String,
    ) -> Option<AgentToCore> {
        let workspace = name.clone();
        let saved = line.clone();
        let result = if self.saved_catalogs.get(&name) == Some(&line) {
            Ok(Ok(()))
        } else {
            tokio::task::spawn_blocking(move || {
                crate::workspace_catalog::append_line(&workspace, &saved)
            })
            .await
        };
        let error = match result {
            Ok(result) => result.err().map(|error| error.to_string()),
            Err(error) => Some(error.to_string()),
        };
        if error.is_some() {
            self.failed_catalogs.insert(name.clone());
        } else {
            self.failed_catalogs.remove(&name);
            self.saved_catalogs.insert(name.clone(), line.clone());
        }
        Some(AgentToCore::WorkspaceCatalogSaved { name, line, error })
    }
    pub(super) async fn rename_catalog(&mut self, old: String, new: String) -> Option<AgentToCore> {
        self.saved_catalogs.remove(&old);
        self.saved_catalogs.remove(&new);
        if self.failed_catalogs.remove(&old) {
            self.failed_catalogs.insert(new.clone());
        }
        let workspace = new.clone();
        let result =
            tokio::task::spawn_blocking(move || crate::workspace_catalog::rename(&old, &new)).await;
        let error = match result {
            Ok(result) => result.err().map(|error| error.to_string()),
            Err(error) => Some(error.to_string()),
        };
        error.map(|error| {
            self.failed_catalogs.insert(workspace.clone());
            AgentToCore::DurabilityError {
                workspace,
                operation: "Workspace catalog rename".into(),
                error,
            }
        })
    }
}

#[cfg(test)]
type RenameGateChannels = (
    crossbeam_channel::Sender<()>,
    crossbeam_channel::Receiver<()>,
);

#[cfg(test)]
fn rename_gates() -> &'static std::sync::Mutex<HashMap<String, RenameGateChannels>> {
    static GATES: std::sync::OnceLock<std::sync::Mutex<HashMap<String, RenameGateChannels>>> =
        std::sync::OnceLock::new();
    GATES.get_or_init(Default::default)
}

#[cfg(test)]
pub(crate) struct RenameGate {
    old: String,
    pub(crate) entered: crossbeam_channel::Receiver<()>,
    release: crossbeam_channel::Sender<()>,
}

#[cfg(test)]
impl RenameGate {
    pub(crate) fn release(&self) {
        let _ = self.release.try_send(());
    }
}

#[cfg(test)]
impl Drop for RenameGate {
    fn drop(&mut self) {
        self.release();
        rename_gates().lock().unwrap().remove(&self.old);
    }
}

#[cfg(test)]
pub(crate) fn hold_rename(old: &str) -> RenameGate {
    let (entered_tx, entered) = crossbeam_channel::bounded(1);
    let (release, release_rx) = crossbeam_channel::bounded(1);
    rename_gates()
        .lock()
        .unwrap()
        .insert(old.to_owned(), (entered_tx, release_rx));
    RenameGate {
        old: old.to_owned(),
        entered,
        release,
    }
}

#[cfg(test)]
fn wait_rename_gate(old: &str) {
    let gate = rename_gates().lock().unwrap().remove(old);
    if let Some((entered, release)) = gate {
        let _ = entered.send(());
        let _ = release.recv();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compaction_failure_keeps_committed_delivery_and_later_appends() {
        let name = format!("compaction-failure-{}", std::process::id());
        let mut log = crate::eventlog::EventLog::open(&name);
        let (workspace, line) = log
            .record(crate::eventlog::LogEvent::TimelineNote {
                context: Default::default(),
                text: "x".repeat(crate::history::COMPACT_BYTES as usize),
            })
            .unwrap();
        crate::persist::ensure_private_dir(&crate::persist::state_dir()).unwrap();
        let archives = crate::history::archive_dir(&name);
        // A file where the archive directory must go injects an ordinary
        // maintenance I/O failure after the authoritative append succeeded.
        std::fs::write(&archives, b"obstruction").unwrap();
        let mut persistence = Persistence::default();
        let (error, committed) = persistence.append_event(workspace, line).await;
        assert!(
            matches!(error, Some(AgentToCore::DurabilityError { operation, .. }) if operation == "history compaction")
        );
        assert_eq!(committed.unwrap().sequence, 1);
        assert!(!persistence.events_failed(&name));
        std::fs::remove_file(&archives).unwrap();
        let (workspace, line) = log
            .record(crate::eventlog::LogEvent::TaskCreated {
                id: 1,
                title: "later".into(),
                status: uniterm_core::TaskStatus::Todo,
            })
            .unwrap();
        let (error, committed) = persistence.append_event(workspace, line).await;
        assert!(error.is_none());
        assert_eq!(committed.unwrap().sequence, 2);
        assert!(
            archives.is_dir(),
            "maintenance failure stayed eligible for retry"
        );
        let mut tasks = uniterm_core::TaskList::new();
        crate::eventlog::replay_tasks(&name, &mut tasks).unwrap();
        assert_eq!(tasks.get(1).unwrap().title, "later");
        crate::eventlog::delete(&name).unwrap();
    }
}
