//! Runtime-owned, rebuildable day index over the authoritative event log.
//! Index I/O never enters the renderer; only a bounded day crosses the seam.

use crate::eventlog::{EventEnvelope, LogEvent};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, Seek, Write};
use std::path::PathBuf;
use uniterm_core::{
    timeline::{TimelineContext, TimelineDay, TimelineEntry, TimelineKind},
    PaneId, ProjectId,
};

const DAY_MS: u64 = 86_400_000;
const PAGE_SIZE: usize = 128;

#[derive(Clone, Default, Serialize, Deserialize)]
struct Invocation {
    sequence: u64,
    provider: String,
    session: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Projection {
    projects: HashMap<u64, TimelineContext>,
    panes: HashMap<u64, TimelineContext>,
    agents: HashMap<u64, Invocation>,
    #[serde(default)]
    read_only_managers: HashSet<u64>,
}

impl Projection {
    fn observe(&mut self, envelope: &EventEnvelope) -> Option<TimelineEntry> {
        let mut entry = TimelineEntry {
            sequence: envelope.sequence,
            timestamp_ms: envelope.timestamp_ms,
            context: TimelineContext::default(),
            invocation: None,
            provider: String::new(),
            session_id: None,
            kind: TimelineKind::Agent,
            summary: String::new(),
        };
        let pane = match &envelope.event {
            LogEvent::WorkspaceProjected { state } => {
                self.projects = state
                    .projects
                    .iter()
                    .map(|p| {
                        (
                            p.id.0,
                            TimelineContext {
                                project: Some(p.id),
                                project_name: p.name.clone(),
                                root: p.root.clone(),
                                branch: p
                                    .metadata
                                    .iter()
                                    .find(|(key, _)| key == "uniterm.worktree.branch")
                                    .map(|(_, value)| value.clone())
                                    .unwrap_or_default(),
                                ..Default::default()
                            },
                        )
                    })
                    .collect();
                self.panes.clear();
                self.read_only_managers.clear();
                for window in &state.windows {
                    for pane in &window.panes {
                        if pane.metadata.iter().any(|(key, value)| {
                            key == "uniterm.manager.scope" && value == "read_only"
                        }) {
                            self.read_only_managers.insert(pane.id.0);
                        }
                        let mut context = self
                            .projects
                            .get(&window.project.0)
                            .cloned()
                            .unwrap_or_default();
                        context.pane = Some(pane.id);
                        context.tab = window.name.clone().unwrap_or_default();
                        self.panes.insert(pane.id.0, context);
                    }
                }
                return None;
            }
            LogEvent::ProjectSelected { project } => {
                entry.context =
                    self.projects
                        .get(project)
                        .cloned()
                        .unwrap_or_else(|| TimelineContext {
                            project: Some(ProjectId(*project)),
                            project_name: format!("Project {project}"),
                            ..Default::default()
                        });
                entry.kind = TimelineKind::Visit;
                entry.summary = "Visited Project".into();
                None
            }
            LogEvent::AgentBound { pane, agent } => {
                self.agents.insert(
                    *pane,
                    Invocation {
                        sequence: envelope.sequence,
                        provider: agent.clone(),
                        session: None,
                    },
                );
                entry.summary = "Started".into();
                Some(*pane)
            }
            LogEvent::AgentStatus { pane, status } => {
                entry.summary = format!("{status:?}");
                Some(*pane)
            }
            LogEvent::AgentSessionObserved {
                pane,
                provider,
                session_id,
                ..
            } => {
                let agent = self.agents.entry(*pane).or_insert_with(|| Invocation {
                    sequence: envelope.sequence,
                    provider: provider.clone(),
                    session: None,
                });
                agent.session = session_id.clone();
                entry.summary = "Session identified".into();
                Some(*pane)
            }
            LogEvent::AgentUnbound { pane } | LogEvent::PaneClosed { pane } => {
                let agent = self.agents.remove(pane)?;
                entry.invocation = Some(agent.sequence);
                entry.provider = agent.provider;
                entry.session_id = agent.session;
                entry.summary = "Ended".into();
                Some(*pane)
            }
            LogEvent::TimelineNote { context, text } => {
                entry.context = context.clone();
                entry.kind = TimelineKind::Note;
                entry.summary = text.clone();
                None
            }
            LogEvent::TimelinePrompt { context, text } => {
                entry.context = context.clone();
                entry.kind = TimelineKind::Prompt;
                entry.summary = text.clone();
                None
            }
            LogEvent::TimelineResumed { predecessor, pane } => {
                entry.summary = format!("Resumed from event #{predecessor}");
                Some(*pane)
            }
            LogEvent::TimelineBoundary { reason } => {
                self.agents.clear();
                entry.kind = TimelineKind::Boundary;
                entry.summary = reason.clone();
                None
            }
            _ => return None,
        };
        if let Some(pane) = pane {
            entry.context = self.panes.get(&pane).cloned().unwrap_or_default();
            entry.context.pane = Some(PaneId(pane));
            if let Some(agent) = self.agents.get(&pane) {
                entry.invocation = Some(agent.sequence);
                entry.provider = agent.provider.clone();
                entry.session_id = agent.session.clone();
            }
        }
        // Wire frames and index pages stay bounded even when child-supplied
        // session metadata or historical labels are unusually large.
        fn limit(text: &mut String, max: usize) {
            if text.len() > max {
                let mut end = max;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
        }
        limit(&mut entry.context.project_name, 128);
        limit(&mut entry.context.root, 1024);
        limit(&mut entry.context.branch, 128);
        limit(&mut entry.context.tab, 128);
        limit(&mut entry.provider, 64);
        if let Some(session) = &mut entry.session_id {
            limit(session, 256);
        }
        limit(&mut entry.summary, 4096);
        Some(entry)
    }
}

/// Cache metadata is discarded after an interrupted update or a replaced log.
/// It holds only live context and byte positions, never lifetime observations.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Index {
    #[serde(skip)]
    loaded: Option<String>,
    offset: u64,
    sequence: u64,
    inode: u64,
    projection: Projection,
}

impl Index {
    fn root(name: &str) -> PathBuf {
        crate::persist::snapshot_path(name).with_extension("timeline")
    }

    fn update(&mut self, name: &str, through: u64) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let root = Self::root(name);
        crate::persist::ensure_private_dir(&root)?;
        let path = crate::persist::snapshot_path(name).with_extension("log");
        let file = std::fs::File::open(&path)?;
        let meta = file.metadata()?;
        if self.loaded.as_deref() != Some(name) {
            *self = std::fs::read(root.join("index.json"))
                .ok()
                .and_then(|bytes| {
                    crate::privacy::decode(&bytes)
                        .ok()
                        .and_then(|plain| serde_json::from_slice(&plain).ok())
                })
                .unwrap_or_default();
            self.loaded = Some(name.to_owned());
        }
        if root.join("dirty").exists()
            || self.inode != meta.ino()
            || self.offset > meta.len()
            || self.sequence > through
        {
            for item in std::fs::read_dir(&root)? {
                let path = item?.path();
                if path.is_file() {
                    std::fs::remove_file(path)?;
                }
            }
            *self = Self {
                loaded: Some(name.into()),
                inode: meta.ino(),
                ..Default::default()
            };
        }
        if self.sequence >= through {
            return Ok(());
        }
        crate::persist::open_private_append(&root.join("dirty"))?.sync_all()?;
        crate::persist::sync_parent_directory(&root.join("dirty"))?;
        let mut reader = std::io::BufReader::new(file);
        reader.seek(std::io::SeekFrom::Start(self.offset))?;
        let mut line = String::new();
        let mut touched = HashSet::new();
        while self.sequence < through {
            line.clear();
            if reader.read_line(&mut line)? == 0 || !line.ends_with('\n') {
                return Err(std::io::Error::other(
                    "Timeline source has not reached its committed cursor",
                ));
            }
            let decoded = crate::privacy::decode_line(&line)?;
            let envelope: EventEnvelope = match serde_json::from_str(&decoded) {
                Ok(envelope) => envelope,
                Err(_) => {
                    // Legacy records have no reliable wall-clock time. They
                    // seed context but do not invent activity at the Unix epoch.
                    let event: LogEvent =
                        serde_json::from_str(&decoded).map_err(std::io::Error::other)?;
                    EventEnvelope {
                        version: 1,
                        sequence: self.sequence + 1,
                        timestamp_ms: 0,
                        workspace: name.into(),
                        event,
                    }
                }
            };
            if envelope.version > crate::eventlog::EVENT_VERSION
                || (self.sequence != 0 && envelope.sequence != self.sequence + 1)
            {
                return Err(std::io::Error::other(
                    "Timeline source sequence or schema mismatch",
                ));
            }
            if let Some(entry) = self
                .projection
                .observe(&envelope)
                .filter(|entry| entry.timestamp_ms != 0)
            {
                let path = root.join(format!("{}.jsonl", entry.timestamp_ms / DAY_MS));
                let mut file = crate::persist::open_private_append(&path)?;
                let record = serde_json::to_string(&entry).map_err(std::io::Error::other)? + "\n";
                file.write_all(crate::privacy::encode_line(name, &record)?.as_bytes())?;
                touched.insert(path);
            }
            self.sequence = envelope.sequence;
            self.offset = reader.stream_position()?;
        }
        for path in touched {
            std::fs::File::open(path)?.sync_all()?;
        }
        let tmp = root.join("index.tmp");
        let mut file = crate::persist::open_private_append(&tmp)?;
        file.set_len(0)?;
        let bytes = serde_json::to_vec(&self).map_err(std::io::Error::other)?;
        file.write_all(&crate::privacy::encode(name, &bytes)?)?;
        file.sync_all()?;
        std::fs::rename(tmp, root.join("index.json"))?;
        std::fs::remove_file(root.join("dirty"))?;
        crate::persist::sync_parent_directory(&root.join("index.json"))?;
        Ok(())
    }

    /// Read at most one page. `before` is exclusive and stable across appends.
    pub(crate) fn query(
        &mut self,
        name: &str,
        date: &str,
        filter: &str,
        before: Option<u64>,
        through: u64,
    ) -> std::io::Result<TimelineDay> {
        local_day(date)?;
        match self.query_once(name, date, filter, before, through) {
            Ok(day) => Ok(day),
            Err(_) => {
                // Derived files are disposable. One rebuild handles a corrupt
                // cache; source errors still propagate without repairing it here.
                let root = Self::root(name);
                if root.exists() {
                    std::fs::remove_dir_all(root)?;
                }
                *self = Self::default();
                self.query_once(name, date, filter, before, through)
            }
        }
    }

    fn query_once(
        &mut self,
        name: &str,
        date: &str,
        filter: &str,
        before: Option<u64>,
        through: u64,
    ) -> std::io::Result<TimelineDay> {
        let mut day = local_day(date)?;
        self.update(name, through)?;
        let mut rows = BTreeMap::new();
        let mut more = false;
        let filter = filter.to_lowercase();
        let mut projects = BTreeMap::new();
        for bucket in day.start_ms / DAY_MS..=day.end_ms.saturating_sub(1) / DAY_MS {
            let path = Self::root(name).join(format!("{bucket}.jsonl"));
            let file = match std::fs::File::open(path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            for line in std::io::BufReader::new(file).lines() {
                let line = line?;
                let entry: TimelineEntry =
                    serde_json::from_str(&crate::privacy::decode_line(&line)?)
                        .map_err(std::io::Error::other)?;
                if entry.timestamp_ms >= day.start_ms
                    && entry.timestamp_ms < day.end_ms
                    && entry.sequence <= through
                {
                    if let Some(project) = entry.context.project {
                        if projects.len() < 256 || projects.contains_key(&project.0) {
                            projects.insert(project.0, entry.context.clone());
                        } else {
                            day.projects_truncated = true;
                        }
                    }
                    if before.is_some_and(|before| entry.sequence >= before)
                        || !format!(
                            "{} {} {} {} {}",
                            entry.context.project_name,
                            entry.context.root,
                            entry.context.branch,
                            entry.provider,
                            entry.session_id.as_deref().unwrap_or("")
                        )
                        .to_lowercase()
                        .contains(&filter)
                    {
                        continue;
                    }
                    rows.insert(entry.sequence, entry);
                    if rows.len() > PAGE_SIZE {
                        rows.pop_first();
                        more = true;
                    }
                }
            }
        }
        // Clock rollback can move sequence order across UTC buckets.
        let mut entries: Vec<_> = rows.into_values().collect();
        for entry in &mut entries {
            if entry.context.project.is_none() {
                if let Some(pane) = entry.context.pane {
                    if self
                        .projection
                        .agents
                        .get(&pane.0)
                        .is_some_and(|agent| Some(agent.sequence) == entry.invocation)
                    {
                        if let Some(context) = self.projection.panes.get(&pane.0) {
                            entry.context = context.clone();
                        }
                    }
                }
            }
        }
        day.next_before = more.then(|| entries[0].sequence);
        day.projects = projects.into_values().collect();
        day.times = entries.iter().map(|e| local_time(e.timestamp_ms)).collect();
        day.axis = (0..=4)
            .map(|i| local_time(day.start_ms + (day.end_ms - day.start_ms) * i / 4))
            .collect();
        day.entries = entries;
        day.current_sequence = through;
        day.partial_history = true;
        // JSON escaping can make a legal 4096-byte prompt much larger on the
        // wire. Page by bytes as well as count so dense days remain readable.
        let mut wire_bytes = serde_json::to_vec(&day)
            .map_err(std::io::Error::other)?
            .len();
        let budget = uniterm_proto::CONTROL_MAX_FRAME_BYTES as usize - 8192;
        while wire_bytes > budget {
            if day.entries.len() > 1 {
                let entry = day.entries.remove(0);
                let time = day.times.remove(0);
                wire_bytes = wire_bytes.saturating_sub(
                    serde_json::to_vec(&entry)
                        .map_err(std::io::Error::other)?
                        .len()
                        + serde_json::to_vec(&time)
                            .map_err(std::io::Error::other)?
                            .len()
                        + 2,
                );
                day.next_before = day.entries.first().map(|entry| entry.sequence);
            } else if day.projects.len() > 1 {
                let project = day.projects.pop().expect("length checked");
                wire_bytes = wire_bytes.saturating_sub(
                    serde_json::to_vec(&project)
                        .map_err(std::io::Error::other)?
                        .len()
                        + 1,
                );
                day.projects_truncated = true;
            } else {
                return Err(std::io::Error::other(
                    "Timeline observation exceeds the control frame budget",
                ));
            }
        }
        Ok(day)
    }
}

/// Replay a bounded live set of provider profiles, never execute history text.
pub(crate) fn resume(
    name: &str,
    sequence: u64,
) -> std::io::Result<uniterm_proto::TimelineResumeData> {
    let mut projection = Projection::default();
    let mut profiles = HashMap::new();
    let mut result = None;
    crate::eventlog::visit_through(name, 0, sequence, |envelope| {
        match &envelope.event {
            LogEvent::AgentBound { pane, .. } => {
                profiles.remove(pane);
            }
            LogEvent::AgentSessionObserved {
                pane,
                provider,
                session_id,
                resume_command,
            } => {
                if !resume_command.is_empty() {
                    profiles.insert(
                        *pane,
                        (provider.clone(), session_id.clone(), resume_command.clone()),
                    );
                }
            }
            LogEvent::TimelineBoundary { .. } => profiles.clear(),
            _ => {}
        }
        if let Some(entry) = projection
            .observe(&envelope)
            .filter(|entry| entry.sequence == sequence && entry.kind == TimelineKind::Agent)
        {
            if let Some((provider, session_id, argv)) =
                entry.context.pane.and_then(|p| profiles.get(&p.0))
            {
                result = Some(uniterm_proto::TimelineResumeData {
                    manager_read_only: entry
                        .context
                        .pane
                        .is_some_and(|p| projection.read_only_managers.contains(&p.0)),
                    context: entry.context,
                    predecessor: sequence,
                    provider: provider.clone(),
                    session_id: session_id.clone(),
                    argv: argv.clone(),
                });
            }
        }
        if let LogEvent::AgentUnbound { pane } | LogEvent::PaneClosed { pane } = envelope.event {
            profiles.remove(&pane);
        }
        Ok(())
    })?;
    let result = result.ok_or_else(|| std::io::Error::other("No provider resume profile was recorded for this observation; copy its session ID instead"))?;
    if !std::path::Path::new(&result.context.root).is_dir() {
        return Err(std::io::Error::other(
            "The recorded worktree is unavailable; restore it explicitly before resuming",
        ));
    }
    Ok(result)
}

fn local_time(timestamp_ms: u64) -> String {
    let time = (timestamp_ms / 1000) as libc::time_t;
    // SAFETY: initialized tm storage and a valid pointer to time_t.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return "??:??".into();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// Local civil dates are converted on the runtime side with the OS timezone.
/// `tm_isdst = -1` lets the OS resolve 23/25-hour days instead of adding 24h.
pub(crate) fn local_day(date: &str) -> std::io::Result<TimelineDay> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    // SAFETY: libc receives initialized, writable tm storage and a valid time_t.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return Err(std::io::Error::other("local timezone unavailable"));
    }
    if !date.is_empty() && date != "today" {
        let parts: Vec<_> = date
            .split('-')
            .filter_map(|p| p.parse::<i32>().ok())
            .collect();
        if date.len() != 10
            || date.as_bytes().get(4) != Some(&b'-')
            || date.as_bytes().get(7) != Some(&b'-')
            || parts.len() != 3
            || !(1970..=9999).contains(&parts[0])
            || !(1..=12).contains(&parts[1])
            || !(1..=31).contains(&parts[2])
        {
            return Err(std::io::Error::other("date must be today or YYYY-MM-DD"));
        }
        tm.tm_year = parts[0] - 1900;
        tm.tm_mon = parts[1] - 1;
        tm.tm_mday = parts[2];
    }
    tm.tm_hour = 0;
    tm.tm_min = 0;
    tm.tm_sec = 0;
    tm.tm_isdst = -1;
    let requested = (tm.tm_year, tm.tm_mon, tm.tm_mday);
    // SAFETY: mktime normalizes a valid writable tm in the process timezone.
    let start = unsafe { libc::mktime(&mut tm) };
    if start < 0 || requested != (tm.tm_year, tm.tm_mon, tm.tm_mday) {
        return Err(std::io::Error::other("invalid calendar date"));
    }
    let format = |tm: &libc::tm| {
        format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        )
    };
    let date = format(&tm);
    let timezone = format!(
        "UTC{}{:02}:{:02}",
        if tm.tm_gmtoff < 0 { "-" } else { "+" },
        tm.tm_gmtoff.unsigned_abs() / 3600,
        (tm.tm_gmtoff.unsigned_abs() / 60) % 60
    );
    let mut previous = tm;
    previous.tm_mday -= 1;
    previous.tm_isdst = -1;
    unsafe {
        libc::mktime(&mut previous);
    }
    tm.tm_mday += 1;
    tm.tm_isdst = -1;
    let end = unsafe { libc::mktime(&mut tm) };
    Ok(TimelineDay {
        date,
        previous: format(&previous),
        next: format(&tm),
        timezone,
        start_ms: start as u64 * 1000,
        end_ms: end as u64 * 1000,
        ..Default::default()
    })
}

/// Only meaningful evidence refreshes an open Today view.
pub(crate) fn changes_day(event: &LogEvent) -> bool {
    matches!(
        event,
        LogEvent::ProjectSelected { .. }
            | LogEvent::AgentBound { .. }
            | LogEvent::AgentStatus { .. }
            | LogEvent::AgentSessionObserved { .. }
            | LogEvent::AgentUnbound { .. }
            | LogEvent::PaneClosed { .. }
            | LogEvent::TimelineNote { .. }
            | LogEvent::TimelinePrompt { .. }
            | LogEvent::TimelineResumed { .. }
            | LogEvent::TimelineBoundary { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append(name: &str, sequence: u64, timestamp_ms: u64, event: LogEvent) {
        let mut line = serde_json::to_string(&EventEnvelope {
            version: crate::eventlog::EVENT_VERSION,
            sequence,
            timestamp_ms,
            workspace: name.into(),
            event,
        })
        .unwrap();
        line.push('\n');
        crate::eventlog::append_line(name, &line).unwrap();
    }

    #[test]
    fn indexed_days_page_rebuild_and_survive_restart_without_duplicate_rows() {
        let name = format!("timeline-index-{}", std::process::id());
        let day = local_day("2026-09-22").unwrap();
        for sequence in 1..=600 {
            append(
                &name,
                sequence,
                day.start_ms + sequence * 1000,
                LogEvent::TimelineNote {
                    context: TimelineContext {
                        project_name: "example".into(),
                        ..Default::default()
                    },
                    text: format!("note {sequence}"),
                },
            );
        }
        let mut index = Index::default();
        let page = index.query(&name, &day.date, "", None, 600).unwrap();
        assert_eq!(page.entries.len(), PAGE_SIZE);
        assert_eq!(page.entries.last().unwrap().sequence, 600);
        let older = index
            .query(&name, &day.date, "", page.next_before, 600)
            .unwrap();
        assert_eq!(
            older.entries.last().unwrap().sequence,
            page.entries[0].sequence - 1
        );
        assert!(index
            .query(&name, &day.previous, "", None, 600)
            .unwrap()
            .entries
            .is_empty());
        let mut reopened = Index::default();
        assert_eq!(
            reopened.query(&name, &day.date, "", None, 600).unwrap(),
            page
        );
        std::fs::write(Index::root(&name).join("dirty"), b"interrupted").unwrap();
        assert_eq!(
            reopened.query(&name, &day.date, "", None, 600).unwrap(),
            page
        );
        append(
            &name,
            601,
            day.start_ms + 700_000,
            LogEvent::TimelineNote {
                context: TimelineContext::default(),
                text: "after restart".into(),
            },
        );
        let live = reopened.query(&name, &day.date, "", None, 601).unwrap();
        assert_eq!(live.entries.last().unwrap().summary, "after restart");
        std::fs::remove_dir_all(Index::root(&name)).unwrap();
        crate::eventlog::delete(&name).unwrap();
    }

    #[test]
    fn escaped_prompt_pages_fit_the_control_transport() {
        let name = format!("timeline-frame-{}", std::process::id());
        let day = local_day("2026-09-22").unwrap();
        for sequence in 1..=128 {
            append(
                &name,
                sequence,
                day.start_ms + sequence,
                LogEvent::TimelinePrompt {
                    context: TimelineContext {
                        root: "\n".repeat(1024),
                        ..Default::default()
                    },
                    text: "x".to_owned() + &"\n".repeat(4095),
                },
            );
        }
        let mut index = Index::default();
        let day = index.query(&name, &day.date, "", None, 128).unwrap();
        assert!(
            serde_json::to_vec(&day).unwrap().len()
                < uniterm_proto::CONTROL_MAX_FRAME_BYTES as usize - 4096
        );
        assert!(day.entries.len() < 128);
        assert_eq!(day.entries.last().unwrap().sequence, 128);
        assert_eq!(day.next_before, Some(day.entries[0].sequence));
        std::fs::remove_dir_all(Index::root(&name)).unwrap();
        crate::eventlog::delete(&name).unwrap();
    }

    #[test]
    fn invocation_identity_is_not_reused_and_prompts_are_not_projected() {
        let mut projection = Projection::default();
        let make = |sequence, event| EventEnvelope {
            version: 2,
            sequence,
            timestamp_ms: sequence,
            workspace: "test".into(),
            event,
        };
        let first = projection
            .observe(&make(
                1,
                LogEvent::AgentBound {
                    pane: 7,
                    agent: "provider".into(),
                },
            ))
            .unwrap();
        let observed = projection
            .observe(&make(
                2,
                LogEvent::AgentSessionObserved {
                    pane: 7,
                    provider: "provider".into(),
                    session_id: Some("session-a".into()),
                    resume_command: vec!["secret prompt must not appear".into()],
                },
            ))
            .unwrap();
        assert_eq!(observed.invocation, first.invocation);
        assert_eq!(observed.session_id.as_deref(), Some("session-a"));
        assert!(!serde_json::to_string(&observed).unwrap().contains("secret"));
        projection.observe(&make(3, LogEvent::AgentUnbound { pane: 7 }));
        let second = projection
            .observe(&make(
                4,
                LogEvent::AgentBound {
                    pane: 7,
                    agent: "provider".into(),
                },
            ))
            .unwrap();
        assert_ne!(first.invocation, second.invocation);
        assert_eq!(second.session_id, None);
        assert!(projection
            .observe(&make(
                5,
                LogEvent::PaneLaunchProfile {
                    pane: 7,
                    args: vec!["secret".into()]
                }
            ))
            .is_none());
    }

    #[test]
    fn civil_day_timezone_cases_in_isolated_processes() {
        for timezone in ["America/Los_Angeles", "Asia/Kolkata"] {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "timeline::tests::timezone_child"])
                .env("UNITERM_TIMELINE_TZ_TEST", timezone)
                .env("TZ", timezone)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stdout)
            );
        }
    }

    #[test]
    fn timezone_child() {
        let Ok(zone) = std::env::var("UNITERM_TIMELINE_TZ_TEST") else {
            return;
        };
        if zone == "America/Los_Angeles" {
            let spring = local_day("2026-03-08").unwrap();
            let fall = local_day("2026-11-01").unwrap();
            assert_eq!(spring.end_ms - spring.start_ms, 23 * 3_600_000);
            assert_eq!(fall.end_ms - fall.start_ms, 25 * 3_600_000);
            assert_eq!(local_time(spring.start_ms), "00:00");
            assert_eq!(local_time(spring.end_ms), "00:00");
            assert_eq!(spring.previous, "2026-03-07");
            assert_eq!(spring.next, "2026-03-09");
        } else {
            assert_eq!(local_day("2026-09-22").unwrap().timezone, "UTC+05:30");
        }
    }

    #[test]
    fn invalid_dates_do_not_roll_into_another_month() {
        assert!(local_day("2026-02-30").is_err());
        assert!(local_day("2026-13-01").is_err());
        let leap = local_day("2024-02-29").unwrap();
        assert_eq!(leap.previous, "2024-02-28");
        assert_eq!(leap.next, "2024-03-01");
        assert!(leap.end_ms > leap.start_ms);
    }
}
