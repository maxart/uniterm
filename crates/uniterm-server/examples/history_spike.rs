//! Measure startup-equivalent replay before and after lossless maintenance.
//! A child process has private XDG directories; no user's Workspace is opened.

use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::time::Instant;
use uniterm_core::{
    ArtifactLedger, InstructionQueue, RunGraph, TaskList, TaskStatus, WaitingQueue,
};
use uniterm_server::{eventlog, history, persist};

const EVENTS: u64 = 100_000;
const NAME: &str = "replay-budget";

fn recover() -> io::Result<(TaskList, u64)> {
    eventlog::repair_consistent_prefix(NAME)?;
    let cursor = eventlog::EventLog::open(NAME).current_sequence();
    let mut tasks = TaskList::new();
    eventlog::replay_tasks(NAME, &mut tasks)?;
    eventlog::replay_waiting(NAME, &mut WaitingQueue::new())?;
    eventlog::replay_instructions(NAME, &mut InstructionQueue::new())?;
    eventlog::replay_run_graph(NAME, 0, &mut RunGraph::new())?;
    eventlog::replay_artifacts(NAME, 0, &mut ArtifactLedger::new())?;
    eventlog::replay_orchestrations(NAME)?;
    eventlog::recover_snapshot(NAME, None)?;
    eventlog::replay_invocations(NAME, cursor)?;
    Ok((tasks, cursor))
}

fn workload() -> io::Result<()> {
    let path = persist::snapshot_path(NAME).with_extension("log");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let mut output = io::BufWriter::new(file);
    for sequence in 1..=EVENTS {
        let event = if sequence == 1 {
            eventlog::LogEvent::TaskCreated {
                id: 1,
                title: "long-lived task".into(),
                status: TaskStatus::Todo,
            }
        } else {
            eventlog::LogEvent::TaskStatusChanged {
                id: 1,
                status: if sequence % 2 == 0 {
                    TaskStatus::Doing
                } else {
                    TaskStatus::Blocked
                },
            }
        };
        serde_json::to_writer(
            &mut output,
            &eventlog::EventEnvelope {
                version: eventlog::EVENT_VERSION,
                sequence,
                timestamp_ms: sequence,
                workspace: NAME.into(),
                event,
            },
        )?;
        output.write_all(b"\n")?;
    }
    output.flush()?;
    output.get_ref().sync_all()?;
    let before_bytes = std::fs::metadata(&path)?.len();
    let started = Instant::now();
    let expected = recover()?;
    let before = started.elapsed();
    let started = Instant::now();
    assert!(history::compact(NAME)?);
    let maintenance = started.elapsed();
    let started = Instant::now();
    assert_eq!(recover()?, expected);
    let after = started.elapsed();
    let after_bytes = std::fs::metadata(&path)?.len();
    let mut count = 0;
    eventlog::visit_through(NAME, 0, EVENTS, |_| {
        count += 1;
        Ok(())
    })?;
    assert_eq!(count, EVENTS);
    assert!(
        after_bytes < before_bytes / 100,
        "active recovery file did not compact"
    );
    assert!(
        after < before / 10,
        "checkpoint recovery did not improve substantially"
    );
    println!("events={EVENTS} recovery_before_ms={:.3} recovery_after_ms={:.3} maintenance_ms={:.3} active_before_bytes={before_bytes} active_after_bytes={after_bytes} retained_events={count}", before.as_secs_f64() * 1000.0, after.as_secs_f64() * 1000.0, maintenance.as_secs_f64() * 1000.0);
    println!("Measures disk open, consistency validation, cursor discovery and every recovery reducer; excludes PTY/process startup. Archives retain all events and grow with history.");
    Ok(())
}

fn main() -> io::Result<()> {
    if std::env::var_os("UNITERM_HISTORY_SPIKE_CHILD").is_some() {
        return workload();
    }
    let root = std::env::temp_dir().join(format!(
        "uniterm-history-spike-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root)?;
    let status = std::process::Command::new(std::env::current_exe()?)
        .env("UNITERM_HISTORY_SPIKE_CHILD", "1")
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .status();
    std::fs::remove_dir_all(root)?;
    if status?.success() {
        Ok(())
    } else {
        Err(io::Error::other("history budget failed"))
    }
}
