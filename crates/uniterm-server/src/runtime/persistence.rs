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
                let result =
                    tokio::task::spawn_blocking(move || crate::eventlog::append_line(&name, &line))
                        .await;
                match result {
                    Ok(Ok(())) => {
                        let envelope =
                            serde_json::from_str::<crate::eventlog::EventEnvelope>(&stream_line)
                                .ok()
                                .filter(|envelope| envelope.workspace == workspace);
                        return (None, envelope);
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
    pub(super) async fn rename_events(&mut self, old: String, new: String) -> Option<AgentToCore> {
        let poisoned = self.poisoned_event_logs.remove(&old);
        let old_name = old.clone();
        let new_name = new.clone();
        let result = tokio::task::spawn_blocking(move || crate::eventlog::rename(&old, &new)).await;
        match result {
            Ok(Ok(())) => {
                if poisoned {
                    self.poisoned_event_logs.insert(new_name);
                }
                None
            }
            Ok(Err(error)) => {
                self.poisoned_event_logs.insert(new_name);
                if poisoned {
                    self.poisoned_event_logs.insert(old_name.clone());
                }
                Some(AgentToCore::DurabilityError {
                    workspace: old_name,
                    operation: "event-log rename".into(),
                    error: error.to_string(),
                })
            }
            Err(error) => {
                self.poisoned_event_logs.insert(new_name);
                if poisoned {
                    self.poisoned_event_logs.insert(old_name.clone());
                }
                Some(AgentToCore::DurabilityError {
                    workspace: old_name,
                    operation: "event-log rename worker".into(),
                    error: error.to_string(),
                })
            }
        }
    }
    pub(super) async fn delete_events(&mut self, name: String) -> Option<AgentToCore> {
        self.poisoned_event_logs.remove(&name);
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
