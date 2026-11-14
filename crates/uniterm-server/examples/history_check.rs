//! Read-only compatibility validation for a copied Workspace before installation.
//! XDG_STATE_HOME is required so this diagnostic never defaults to live state.
use std::io;
use uniterm_core::{ArtifactLedger, InstructionQueue, RunGraph, TaskList, WaitingQueue};
use uniterm_server::eventlog;

fn main() -> io::Result<()> {
    if std::env::var_os("XDG_STATE_HOME")
        .is_none_or(|value| !std::path::Path::new(&value).is_absolute())
    {
        return Err(io::Error::other(
            "Set XDG_STATE_HOME to an absolute path containing a private copy of the Workspace state",
        ));
    }
    let name = std::env::args()
        .nth(1)
        .ok_or_else(|| io::Error::other("Workspace name required"))?;
    uniterm_proto::validate_workspace_name(&name).map_err(io::Error::other)?;
    if !eventlog::exists(&name) {
        return Err(io::Error::other(
            "No copied Workspace history exists; nothing was validated",
        ));
    }
    let snapshot_path = uniterm_server::persist::snapshot_path(&name);
    let checkpoint = uniterm_server::persist::load(&name);
    if snapshot_path.exists() && checkpoint.is_none() {
        return Err(io::Error::other(
            "Copied snapshot is unreadable or incompatible",
        ));
    }
    let mut count = 0;
    let mut cursor = 0;
    eventlog::visit_after(&name, 0, |event| {
        count += 1;
        cursor = event.sequence;
        Ok(())
    })?;
    let mut tasks = TaskList::new();
    eventlog::replay_tasks(&name, &mut tasks)?;
    eventlog::replay_waiting(&name, &mut WaitingQueue::new())?;
    eventlog::replay_instructions(&name, &mut InstructionQueue::new())?;
    eventlog::replay_run_graph(&name, 0, &mut RunGraph::new())?;
    eventlog::replay_artifacts(&name, 0, &mut ArtifactLedger::new())?;
    eventlog::replay_orchestrations(&name)?;
    eventlog::replay_invocations(&name, cursor)?;
    let snapshot = eventlog::recover_snapshot(&name, checkpoint)?;
    let projects = snapshot.as_ref().map_or(0, |s| s.projects.len());
    let tabs = snapshot.as_ref().map_or(0, |s| s.windows.len());
    println!("Validated {count} events through {cursor}; recovered {projects} Projects, {tabs} Tabs, {} Tasks; supported event schema {}", tasks.len(), eventlog::EVENT_VERSION);
    Ok(())
}
