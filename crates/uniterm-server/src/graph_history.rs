//! Selected-run history reads stay off the render loop and retain bounded evidence.

use crate::eventlog::{visit_through, LogEvent};
use std::collections::VecDeque;
use uniterm_core::{RunGraphEvent, RunId};

/// Read authoritative attempts and review outcomes without exposing private turn tokens.
pub(crate) fn read(name: &str, run: RunId, task: u64, through: u64) -> Result<Vec<String>, String> {
    let mut rows = VecDeque::new();
    let mut omitted = 0usize;
    visit_through(name, 0, through, |entry| {
        let description = match entry.event {
            LogEvent::RunGraph { change } => match change {
                RunGraphEvent::Activated {
                    run: owner,
                    role,
                    activation,
                } if owner == run => {
                    Some(format!("Attempt #{activation}: role #{} activated", role.0))
                }
                RunGraphEvent::Handoff {
                    run: owner,
                    from,
                    to,
                    activation,
                } if owner == run => Some(format!(
                    "Attempt #{activation}: role #{} -> role #{}",
                    from.0, to.0
                )),
                RunGraphEvent::Completed {
                    run: owner,
                    outcome,
                } if owner == run => Some(format!("Completed: {outcome}")),
                RunGraphEvent::Failed {
                    run: owner,
                    outcome,
                } if owner == run => Some(format!("Failed: {outcome}")),
                RunGraphEvent::Canceled {
                    run: owner,
                    outcome,
                } if owner == run => Some(format!("Canceled: {outcome}")),
                _ => None,
            },
            LogEvent::OrchestrationSubmitted {
                task_id,
                status,
                verdict,
                summary,
                ..
            } if task_id == task => Some(format!(
                "Submission {status:?} | verdict: {} | {summary}",
                verdict.as_deref().unwrap_or("none")
            )),
            LogEvent::OrchestrationArtifactsValidated {
                task_id, artifacts, ..
            } if task_id == task => Some(format!(
                "Artifact gate accepted {} recorded paths",
                artifacts.len()
            )),
            LogEvent::OrchestrationDelivery {
                task_id, accepted, ..
            } if task_id == task => Some(format!(
                "Prompt delivery {}",
                if accepted { "accepted" } else { "failed" }
            )),
            LogEvent::OrchestrationFinished {
                task_id, outcome, ..
            } if task_id == task => Some(format!("Orchestration outcome: {outcome}")),
            _ => None,
        };
        if let Some(description) = description {
            if rows.len() == 128 {
                rows.pop_front();
                omitted += 1;
            }
            let text: String = description
                .chars()
                .filter(|c| !c.is_control())
                .take(768)
                .collect();
            rows.push_back(format!(
                "Event #{} | {} ms UTC | {text}",
                entry.sequence, entry.timestamp_ms
            ));
        }
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    if omitted > 0 {
        rows.push_front(format!(
            "{omitted} older entries omitted; retained in Workspace history."
        ));
    }
    if rows.is_empty() {
        rows.push_back("No attempt or review events recorded at this cursor.".into());
    }
    Ok(rows.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eventlog::{append_line, EventLog};
    use uniterm_core::RoleId;
    use uniterm_proto::{OrchestrationKind, SubmissionStatus};

    #[test]
    fn selected_run_history_is_bounded_scoped_and_excludes_turn_tokens() {
        let name = format!("graph-evidence-{}", std::process::id());
        crate::eventlog::delete(&name).unwrap();
        let mut log = EventLog::open(&name);
        for activation in 1..=130 {
            let (_, line) = log
                .record(LogEvent::RunGraph {
                    change: RunGraphEvent::Activated {
                        run: RunId(1),
                        role: RoleId(1),
                        activation,
                    },
                })
                .unwrap();
            append_line(&name, &line).unwrap();
        }
        let (_, line) = log
            .record(LogEvent::OrchestrationSubmitted {
                kind: OrchestrationKind::Workflow,
                task_id: 9,
                token: 987654321,
                status: SubmissionStatus::Done,
                verdict: Some("pass".into()),
                summary: "Reviewed".into(),
                artifacts: vec![],
            })
            .unwrap();
        append_line(&name, &line).unwrap();
        let (_, line) = log
            .record(LogEvent::RunGraph {
                change: RunGraphEvent::Completed {
                    run: RunId(2),
                    outcome: "foreign run".into(),
                },
            })
            .unwrap();
        append_line(&name, &line).unwrap();
        let rows = read(&name, RunId(1), 9, log.current_sequence()).unwrap();
        assert_eq!(rows.len(), 129);
        let text = rows.join("\n");
        assert!(text.contains("3 older entries omitted"));
        assert!(text.contains("verdict: pass"));
        assert!(text.contains("Attempt #130"));
        assert!(!text.contains("987654321"));
        assert!(!text.contains("foreign run"));
        assert!(read(&name, RunId(1), 9, log.current_sequence() + 1).is_err());
        crate::eventlog::delete(&name).unwrap();
    }
}
