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
const INDEX_VERSION: u32 = 6;

/// Lightweight search/summary metadata lets paging leave retained content on disk.
#[derive(Serialize, Deserialize)]
struct RowPointer {
    sequence: u64,
    timestamp_ms: u64,
    context: TimelineContext,
    provider: String,
    session_id: Option<String>,
    offset: u64,
    bytes: u64,
}

struct BucketWriter {
    rows: std::io::BufWriter<std::fs::File>,
    pointers: std::io::BufWriter<std::fs::File>,
    offset: u64,
}

impl BucketWriter {
    fn open(root: &std::path::Path, bucket: u64) -> std::io::Result<Self> {
        let rows = crate::persist::open_private_append(&root.join(format!("{bucket}.jsonl")))?;
        let offset = rows.metadata()?.len();
        let pointers = crate::persist::open_private_append(&root.join(format!("{bucket}.rows")))?;
        Ok(Self {
            rows: std::io::BufWriter::new(rows),
            pointers: std::io::BufWriter::new(pointers),
            offset,
        })
    }

    fn append(&mut self, name: &str, entry: TimelineEntry) -> std::io::Result<()> {
        let record = serde_json::to_string(&entry).map_err(std::io::Error::other)? + "\n";
        let record = crate::privacy::encode_line(name, &record)?;
        let pointer = RowPointer {
            sequence: entry.sequence,
            timestamp_ms: entry.timestamp_ms,
            context: entry.context,
            provider: entry.provider,
            session_id: entry.session_id,
            offset: self.offset,
            bytes: record.len() as u64,
        };
        self.rows.write_all(record.as_bytes())?;
        let metadata = serde_json::to_string(&pointer).map_err(std::io::Error::other)? + "\n";
        self.pointers
            .write_all(crate::privacy::encode_line(name, &metadata)?.as_bytes())?;
        self.offset += pointer.bytes;
        Ok(())
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.rows.flush()?;
        self.pointers.flush()?;
        self.rows.get_ref().sync_all()?;
        self.pointers.get_ref().sync_all()
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Invocation {
    sequence: u64,
    provider: String,
    session: Option<String>,
    #[serde(default)]
    last: Option<TimelineEntry>,
    #[serde(default)]
    exited: bool,
    #[serde(default)]
    parent_invocation: Option<u64>,
    #[serde(default)]
    process_group: Option<i32>,
    #[serde(default)]
    work_summary: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Projection {
    projects: HashMap<u64, TimelineContext>,
    panes: HashMap<u64, TimelineContext>,
    agents: HashMap<u64, Invocation>,
    #[serde(default)]
    children: HashMap<u64, HashMap<String, TimelineEntry>>,
    #[serde(default)]
    read_only_managers: HashSet<u64>,
}

impl Projection {
    fn project_structure(&mut self, state: &crate::eventlog::StructuralProjection) {
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
                if pane
                    .metadata
                    .iter()
                    .any(|(key, value)| key == "uniterm.manager.scope" && value == "read_only")
                {
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
        if let Some(pane) = &state.overwatch_pane {
            if pane
                .metadata
                .iter()
                .any(|(key, value)| key == "uniterm.manager.scope" && value == "read_only")
            {
                self.read_only_managers.insert(pane.id.0);
            }
            self.panes.insert(
                pane.id.0,
                TimelineContext {
                    pane: Some(pane.id),
                    root: pane.cwd.clone().unwrap_or_default(),
                    tab: "Overwatch".into(),
                    ..Default::default()
                },
            );
        }
    }

    fn observe(&mut self, envelope: &EventEnvelope) -> Option<TimelineEntry> {
        if let LogEvent::AgentResumed {
            structure: Some(state),
            ..
        } = &envelope.event
        {
            self.project_structure(state);
        }
        let mut entry = TimelineEntry {
            work_summary: None,
            sequence: envelope.sequence,
            timestamp_ms: envelope.timestamp_ms,
            context: TimelineContext::default(),
            invocation: None,
            provider: String::new(),
            session_id: None,
            parent_session_id: None,
            parent_invocation: None,
            kind: TimelineKind::Agent,
            summary: String::new(),
        };
        let pane = match &envelope.event {
            LogEvent::WorkspaceProjected { state } => {
                self.project_structure(state);
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
            LogEvent::AgentBound {
                pane,
                agent,
                invocation,
            } => {
                self.children.remove(pane);
                self.agents.insert(
                    *pane,
                    Invocation {
                        sequence: envelope.sequence,
                        provider: agent.clone(),
                        process_group: *invocation,
                        work_summary: None,
                        session: None,
                        last: None,
                        exited: false,
                        parent_invocation: None,
                    },
                );
                entry.summary = "Started".into();
                Some(*pane)
            }
            LogEvent::AgentResumed {
                pane,
                provider,
                session_id,
                predecessor,
                invocation,
                ..
            } => {
                self.children.remove(pane);
                self.agents.insert(
                    *pane,
                    Invocation {
                        sequence: envelope.sequence,
                        provider: provider.clone(),
                        process_group: *invocation,
                        work_summary: None,
                        session: session_id.clone(),
                        last: None,
                        exited: false,
                        parent_invocation: None,
                    },
                );
                entry.summary = predecessor.map_or_else(
                    || "Resumed saved session".into(),
                    |sequence| format!("Resumed from event #{sequence}"),
                );
                Some(*pane)
            }
            LogEvent::AgentDelegated {
                pane,
                invocation,
                parent_invocation,
                context,
            } => {
                let agent = self.agents.get_mut(pane)?;
                if agent.sequence != *invocation || invocation == parent_invocation {
                    return None;
                }
                agent.parent_invocation = Some(*parent_invocation);
                entry.context = context.clone();
                entry.summary = "Launched from agent Pane".into();
                Some(*pane)
            }
            LogEvent::AgentStatus { pane, status } => {
                if let Some(agent) = self.agents.get_mut(pane) {
                    agent.exited = *status == uniterm_core::AgentStatus::Exited;
                }
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
                    process_group: None,
                    work_summary: None,
                    session: None,
                    last: None,
                    exited: false,
                    parent_invocation: None,
                });
                if matches!((&agent.session, session_id), (Some(old), Some(new)) if old != new) {
                    self.children.remove(pane);
                    agent.work_summary = None;
                }
                if session_id.is_some() {
                    agent.session = session_id.clone();
                }
                entry.summary = "Session identified".into();
                Some(*pane)
            }
            LogEvent::AgentDetail {
                pane,
                invocation,
                detail: uniterm_core::AgentDetail::Summary { text },
            } => {
                let agent = self.agents.get_mut(pane)?;
                // Details are logged only after the live foreground/provider
                // checks accept them. A binding may predate PID discovery, so
                // its first accepted detail can refine that unknown identity
                // without changing the invocation's event sequence.
                if !agent.exited && agent.process_group.is_none() {
                    agent.process_group = *invocation;
                }
                if agent.exited || agent.process_group != *invocation {
                    return None;
                }
                let text = uniterm_core::agent_detail::bounded_text(
                    text.trim(),
                    uniterm_core::agent_detail::DETAIL_TEXT_LIMIT,
                );
                let summary = (!text.is_empty()).then_some(text);
                if summary == agent.work_summary {
                    return None;
                }
                agent.work_summary = summary;
                entry.summary = "Task summary updated".into();
                Some(*pane)
            }
            LogEvent::TaskObserved { task } => {
                let pane = task.context.pane?.0;
                let agent = self.agents.get_mut(&pane)?;
                if task.context.invocation != Some(agent.sequence)
                    || agent.exited
                    || task.context.observed.as_ref().is_some_and(|old| {
                        old.session_id.is_some() && old.session_id != agent.session
                    })
                {
                    return None;
                }
                agent.work_summary = Some(task.title.clone());
                entry.summary = format!("Task #{}: {}", task.id, task.status.display());
                Some(pane)
            }
            LogEvent::AgentSessionLinked {
                pane,
                provider,
                session_id,
                parent_session_id,
                work_summary,
            } => {
                entry.context = self.panes.get(pane).cloned().unwrap_or_default();
                // A harness child may share its parent's PTY. It is not the
                // Pane's root invocation and cannot inherit its resume target.
                entry.context.pane = Some(PaneId(*pane));
                entry.provider = provider.clone();
                entry.session_id = Some(session_id.clone());
                entry.parent_session_id = Some(parent_session_id.clone());
                entry.work_summary = match work_summary {
                    Some(text) => {
                        let text = uniterm_core::agent_detail::bounded_text(
                            text.trim(),
                            uniterm_core::agent_detail::DETAIL_TEXT_LIMIT,
                        );
                        (!text.is_empty()).then_some(text)
                    }
                    None => self
                        .children
                        .get(pane)
                        .and_then(|children| children.get(session_id))
                        .and_then(|previous| previous.work_summary.clone()),
                };
                entry.summary = "Child session observed".into();
                None
            }
            LogEvent::AgentUnbound { pane } | LogEvent::PaneClosed { pane } => {
                self.children.remove(pane);
                let agent = self.agents.remove(pane)?;
                entry.work_summary.clone_from(&agent.work_summary);
                entry.invocation = Some(agent.sequence);
                entry.context = agent
                    .last
                    .as_ref()
                    .map(|last| last.context.clone())
                    .unwrap_or_default();
                entry.parent_invocation = agent.parent_invocation;
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
                self.children.clear();
                entry.kind = TimelineKind::Boundary;
                entry.summary = reason.clone();
                None
            }
            _ => return None,
        };
        if let Some(pane) = pane {
            if !matches!(&envelope.event, LogEvent::AgentDelegated { .. }) {
                if let Some(context) = self.panes.get(&pane) {
                    entry.context = context.clone();
                } else if let Some(context) = self
                    .agents
                    .get(&pane)
                    .and_then(|agent| agent.last.as_ref())
                    .map(|last| &last.context)
                {
                    entry.context = context.clone();
                }
            }
            entry.context.pane = Some(PaneId(pane));
            if let Some(agent) = self.agents.get(&pane) {
                entry.invocation = Some(agent.sequence);
                entry.parent_invocation = agent.parent_invocation;
                entry.provider = agent.provider.clone();
                entry.session_id = agent.session.clone();
                entry.work_summary.clone_from(&agent.work_summary);
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
            limit(session, 512);
        }
        if let Some(session) = &mut entry.parent_session_id {
            limit(session, 512);
        }
        limit(&mut entry.summary, 4096);
        if let LogEvent::AgentSessionLinked { pane, .. } = &envelope.event {
            let children = self.children.entry(*pane).or_default();
            if let Some(session) = &entry.session_id {
                if children.len() < PAGE_SIZE || children.contains_key(session) {
                    children.insert(session.clone(), entry.clone());
                }
            }
        }
        if let Some(pane) = pane {
            if let Some(agent) = self.agents.get_mut(&pane) {
                agent.last = Some(entry.clone());
            }
        }
        Some(entry)
    }
}

/// Cache metadata is discarded after an interrupted update or a replaced log.
/// It holds only live context and byte positions, never lifetime observations.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Index {
    #[serde(default)]
    version: u32,
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
            || self.version != INDEX_VERSION
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
                version: INDEX_VERSION,
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
        let mut reader = crate::history::Reader::from_offset(name, file, self.offset)?;
        let mut line = String::new();
        let mut buckets: HashMap<u64, BucketWriter> = HashMap::new();
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
                let bucket = entry.timestamp_ms / DAY_MS;
                if !buckets.contains_key(&bucket) && buckets.len() >= 8 {
                    for writer in buckets.values_mut() {
                        writer.sync()?;
                    }
                    buckets.clear();
                }
                if let std::collections::hash_map::Entry::Vacant(slot) = buckets.entry(bucket) {
                    slot.insert(BucketWriter::open(&root, bucket)?);
                }
                buckets
                    .get_mut(&bucket)
                    .expect("opened bucket")
                    .append(name, entry)?;
            }
            self.sequence = envelope.sequence;
        }
        if let Some(offset) = reader.active_offset()? {
            self.offset = offset;
        } else {
            // A request whose cursor precedes the active checkpoint ended
            // inside an archive. Rebuild before reusing an inode byte offset.
            self.inode = 0;
            self.offset = 0;
        }
        for writer in buckets.values_mut() {
            writer.sync()?;
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
            let path = Self::root(name).join(format!("{bucket}.rows"));
            let file = match std::fs::File::open(path) {
                Ok(file) => file,
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        && !Self::root(name).join(format!("{bucket}.jsonl")).exists() =>
                {
                    continue
                }
                Err(e) => return Err(e),
            };
            for line in std::io::BufReader::new(file).lines() {
                let line = line?;
                let entry: RowPointer = serde_json::from_str(&crate::privacy::decode_line(&line)?)
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
        let mut entries = Vec::with_capacity(rows.len());
        let mut files = HashMap::new();
        for pointer in rows.into_values() {
            use std::io::Read;
            let bucket = pointer.timestamp_ms / DAY_MS;
            if let std::collections::hash_map::Entry::Vacant(slot) = files.entry(bucket) {
                slot.insert(std::fs::File::open(
                    Self::root(name).join(format!("{bucket}.jsonl")),
                )?);
            }
            let file = files.get_mut(&bucket).expect("opened bucket");
            file.seek(std::io::SeekFrom::Start(pointer.offset))?;
            if pointer.bytes > 128 * 1024 {
                return Err(std::io::Error::other("Oversized timeline row"));
            }
            let mut bytes = vec![0; pointer.bytes as usize];
            file.read_exact(&mut bytes)?;
            let record = std::str::from_utf8(&bytes).map_err(std::io::Error::other)?;
            let entry: TimelineEntry = serde_json::from_str(&crate::privacy::decode_line(record)?)
                .map_err(std::io::Error::other)?;
            if entry.sequence != pointer.sequence || entry.timestamp_ms != pointer.timestamp_ms {
                return Err(std::io::Error::other("Timeline row index mismatch"));
            }
            entries.push(entry);
        }
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
        // Open agents and their descendants remain visible when overnight
        // silence or a busy page hides their observations. Preserve the real
        // timestamp and cursor rather than manufacturing new activity.
        if before.is_none() && day.date == local_day("today")?.date {
            let visible: HashSet<_> = day.entries.iter().map(|entry| entry.sequence).collect();
            let mut active: Vec<_> = self
                .projection
                .agents
                .values()
                .filter(|agent| !agent.exited)
                .filter_map(|agent| agent.last.as_ref())
                .chain(
                    self.projection
                        .children
                        .iter()
                        .filter(|(pane, _)| {
                            self.projection
                                .agents
                                .get(pane)
                                .is_some_and(|agent| !agent.exited)
                        })
                        .flat_map(|(_, children)| children.values()),
                )
                .filter(|entry| !visible.contains(&entry.sequence))
                .map(|entry| {
                    let mut entry = entry.clone();
                    if entry.context.project.is_none() {
                        if let Some(context) = entry
                            .context
                            .pane
                            .and_then(|pane| self.projection.panes.get(&pane.0))
                        {
                            entry.context = context.clone();
                        }
                    }
                    entry
                })
                .filter(|entry| {
                    format!(
                        "{} {} {} {} {}",
                        entry.context.project_name,
                        entry.context.root,
                        entry.context.branch,
                        entry.provider,
                        entry.session_id.as_deref().unwrap_or("")
                    )
                    .to_lowercase()
                    .contains(&filter)
                })
                .collect();
            active.sort_by_key(|entry| entry.sequence);
            day.active_agents_truncated = active.len() > PAGE_SIZE;
            active.truncate(PAGE_SIZE);
            day.active_agents = active;
        }
        day.current_sequence = through;
        day.partial_history = true;
        // JSON escaping can make a legal 4096-byte prompt much larger on the
        // wire. Page by bytes as well as count so dense days remain readable.
        let mut wire_bytes = serde_json::to_vec(&day)
            .map_err(std::io::Error::other)?
            .len();
        let budget = uniterm_proto::CONTROL_MAX_FRAME_BYTES as usize - 8192;
        while wire_bytes > budget {
            if !day.active_agents.is_empty() {
                let entry = day.active_agents.pop().expect("length checked");
                day.active_agents_truncated = true;
                wire_bytes = wire_bytes.saturating_sub(
                    serde_json::to_vec(&entry)
                        .map_err(std::io::Error::other)?
                        .len()
                        + 1,
                );
            } else if day.entries.len() > 1 {
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
            LogEvent::AgentResumed {
                pane,
                provider,
                session_id,
                resume_command,
                ..
            } => {
                profiles.insert(
                    *pane,
                    (provider.clone(), session_id.clone(), resume_command.clone()),
                );
            }
            LogEvent::AgentSessionObserved {
                pane,
                provider,
                session_id,
                resume_command,
                ..
            } => {
                if profiles.get(pane).is_some_and(|(previous_provider, previous, _)| {
                    previous_provider != provider
                        || matches!((previous, session_id), (Some(old), Some(new)) if old != new)
                }) {
                    profiles.remove(pane);
                }
                // Session-only hooks must not replace explicit launch options.
                // Synthesize a provider default only without a usable profile
                // for this provider/session in the current invocation.
                let resume_command = if resume_command.is_empty() {
                    profiles
                        .get(pane)
                        .filter(|(_, _, argv)| !argv.is_empty())
                        .map(|(_, _, argv)| argv.clone())
                        .or_else(|| {
                            session_id.as_deref().and_then(|session| {
                                crate::providers::resume_argv(provider, session)
                            })
                        })
                        .unwrap_or_default()
                } else {
                    resume_command.clone()
                };
                if !resume_command.is_empty() {
                    let session_id = session_id.clone().or_else(|| {
                        profiles
                            .get(pane)
                            .and_then(|(_, previous, _)| previous.clone())
                    });
                    profiles.insert(*pane, (provider.clone(), session_id, resume_command));
                }
            }
            LogEvent::TimelineBoundary { .. } => profiles.clear(),
            _ => {}
        }
        if let Some(entry) = projection.observe(&envelope).filter(|entry| {
            entry.sequence == sequence
                && entry.kind == TimelineKind::Agent
                && entry.parent_session_id.is_none()
                && entry.invocation.is_some()
        }) {
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
            | LogEvent::AgentResumed { .. }
            | LogEvent::AgentStatus { .. }
            | LogEvent::AgentSessionObserved { .. }
            | LogEvent::AgentSessionLinked { .. }
            | LogEvent::AgentDetail {
                detail: uniterm_core::AgentDetail::Summary { .. },
                ..
            }
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

    fn resume_fixture(name: &str) {
        use crate::eventlog::{
            StructuralPane, StructuralProject, StructuralProjection, StructuralWindow,
        };
        crate::eventlog::delete(name).unwrap();
        append(
            name,
            1,
            1,
            LogEvent::WorkspaceProjected {
                state: StructuralProjection {
                    overwatch_pane: None,
                    active_window: 0,
                    next_pane_id: 8,
                    active_project: ProjectId(1),
                    next_project_id: 2,
                    projects: vec![StructuralProject {
                        id: ProjectId(1),
                        name: "resume-test".into(),
                        root: std::env::temp_dir().to_string_lossy().into_owned(),
                        active_pane: Some(PaneId(7)),
                        metadata: vec![],
                    }],
                    windows: vec![StructuralWindow {
                        stack_expanded: false,
                        stack_parent: None,
                        layout_mode: Default::default(),
                        project: ProjectId(1),
                        layout: uniterm_core::LayoutNode::Leaf(PaneId(7)),
                        active: PaneId(7),
                        zoomed: None,
                        name: None,
                        panes: vec![StructuralPane {
                            id: PaneId(7),
                            cwd: None,
                            metadata: vec![],
                            launch_args: vec![],
                            agent_launch: None,
                        }],
                    }],
                },
            },
        );
    }

    fn session_observation(provider: &str, session: Option<&str>, argv: Vec<String>) -> LogEvent {
        LogEvent::AgentSessionObserved {
            pane: 7,
            provider: provider.into(),
            session_id: session.map(str::to_owned),
            resume_command: argv,
            transcript_path: None,
        }
    }

    #[test]
    fn resume_preserves_explicit_options_across_session_only_observations() {
        for provider in ["codex", "claude", "pi"] {
            for resumed in [false, true] {
                let name = format!(
                    "timeline-options-{provider}-{resumed}-{}",
                    std::process::id()
                );
                resume_fixture(&name);
                let mut argv = crate::providers::resume_argv(provider, "session-a").unwrap();
                argv.extend(["--model".into(), "explicit-model".into()]);
                let initial = if resumed {
                    LogEvent::AgentResumed {
                        pane: 7,
                        provider: provider.into(),
                        session_id: Some("session-a".into()),
                        resume_command: argv.clone(),
                        previous_invocation: None,
                        invocation: Some(42),
                        predecessor: None,
                        structure: None,
                    }
                } else {
                    session_observation(provider, Some("session-a"), argv.clone())
                };
                append(&name, 2, 2, initial);
                for (sequence, event) in [
                    (3, session_observation(provider, Some("session-a"), vec![])),
                    (4, session_observation(provider, None, vec![])),
                    (5, session_observation(provider, Some("session-a"), vec![])),
                    (
                        6,
                        LogEvent::AgentStatus {
                            pane: 7,
                            status: uniterm_core::AgentStatus::Idle,
                        },
                    ),
                ] {
                    append(&name, sequence, sequence, event);
                    let profile = resume(&name, sequence).unwrap();
                    assert_eq!(profile.argv, argv);
                    assert_eq!(profile.provider, provider);
                    assert_eq!(profile.session_id.as_deref(), Some("session-a"));
                }
                // An authoritative nonempty update still replaces old options.
                argv.extend(["--new-option".into(), "explicit-value".into()]);
                append(
                    &name,
                    7,
                    7,
                    session_observation(provider, Some("session-a"), argv.clone()),
                );
                assert_eq!(resume(&name, 7).unwrap().argv, argv);
                crate::eventlog::delete(&name).unwrap();
            }
        }
    }

    #[test]
    fn resume_fallback_clears_options_on_session_provider_and_invocation_replacement() {
        let name = format!("timeline-resume-replacement-{}", std::process::id());
        resume_fixture(&name);
        let mut explicit = crate::providers::resume_argv("codex", "session-a").unwrap();
        explicit.extend(["--profile".into(), "explicit".into()]);
        append(
            &name,
            2,
            2,
            session_observation("codex", Some("session-a"), explicit.clone()),
        );
        append(
            &name,
            3,
            3,
            session_observation("codex", Some("session-b"), vec![]),
        );
        assert_eq!(
            resume(&name, 3).unwrap().argv,
            crate::providers::resume_argv("codex", "session-b").unwrap()
        );
        // Provider replacement invalidates options even if the session id matches.
        append(
            &name,
            4,
            4,
            session_observation("codex", Some("session-a"), explicit.clone()),
        );
        append(
            &name,
            5,
            5,
            session_observation("claude", Some("session-a"), vec![]),
        );
        let profile = resume(&name, 5).unwrap();
        assert_eq!(profile.provider, "claude");
        assert_eq!(
            profile.argv,
            crate::providers::resume_argv("claude", "session-a").unwrap()
        );
        append(
            &name,
            6,
            6,
            session_observation("codex", Some("session-a"), explicit),
        );
        append(
            &name,
            7,
            7,
            LogEvent::AgentBound {
                pane: 7,
                agent: "codex".into(),
                invocation: Some(43),
            },
        );
        assert!(resume(&name, 7).is_err());
        append(
            &name,
            8,
            8,
            session_observation("codex", Some("session-a"), vec![]),
        );
        assert_eq!(
            resume(&name, 8).unwrap().argv,
            crate::providers::resume_argv("codex", "session-a").unwrap()
        );
        // An unsupported replacement must not inherit any old provider's argv.
        append(
            &name,
            9,
            9,
            session_observation("unknown-provider", Some("session-a"), vec![]),
        );
        assert!(resume(&name, 9)
            .unwrap_err()
            .to_string()
            .contains("No provider resume profile"));
        crate::eventlog::delete(&name).unwrap();
    }

    #[test]
    fn explicit_work_metadata_survives_status_and_checkpoint_without_leaking_to_children() {
        for provider in ["codex", "claude", "grok", "opencode", "pi", "custom"] {
            let mut projection = Projection::default();
            let mut sequence = 0;
            let mut observe = |projection: &mut Projection, event| {
                sequence += 1;
                projection.observe(&EventEnvelope {
                    version: crate::eventlog::EVENT_VERSION,
                    sequence,
                    timestamp_ms: sequence,
                    workspace: "isolated-summary-projection".into(),
                    event,
                })
            };
            observe(
                &mut projection,
                LogEvent::AgentBound {
                    pane: 1,
                    agent: provider.into(),
                    invocation: Some(41),
                },
            );
            let detail = |invocation, text: &str| LogEvent::AgentDetail {
                pane: 1,
                invocation,
                detail: uniterm_core::AgentDetail::Summary { text: text.into() },
            };
            assert!(observe(&mut projection, detail(Some(40), "stale task")).is_none());
            let update =
                observe(&mut projection, detail(Some(41), "Repair auth\n界\u{001b}")).unwrap();
            assert_eq!(update.work_summary.as_deref(), Some("Repair auth 界"));
            assert_eq!(update.summary, "Task summary updated");
            assert!(
                observe(&mut projection, detail(Some(41), "Repair auth 界")).is_none(),
                "identical metadata is a no-op"
            );
            observe(
                &mut projection,
                LogEvent::AgentSessionObserved {
                    pane: 1,
                    provider: provider.into(),
                    session_id: Some("root".into()),
                    transcript_path: None,
                    resume_command: vec![],
                },
            );
            let status = observe(
                &mut projection,
                LogEvent::AgentStatus {
                    pane: 1,
                    status: uniterm_core::AgentStatus::Idle,
                },
            )
            .unwrap();
            assert_eq!(status.summary, "Idle");
            assert_eq!(status.work_summary.as_deref(), Some("Repair auth 界"));
            let encoded = serde_json::to_vec(&projection).unwrap();
            projection = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(
                projection.agents[&1].last.as_ref().unwrap().work_summary,
                status.work_summary
            );
            let child = observe(
                &mut projection,
                LogEvent::AgentSessionLinked {
                    work_summary: None,
                    pane: 1,
                    provider: provider.into(),
                    session_id: "child".into(),
                    parent_session_id: "root".into(),
                },
            )
            .unwrap();
            assert!(
                child.work_summary.is_none(),
                "the parent's task is not child task evidence"
            );
            let child_event = LogEvent::AgentSessionLinked {
                pane: 1,
                provider: provider.into(),
                session_id: "child".into(),
                parent_session_id: "root".into(),
                work_summary: Some("Review parser\n界\u{001b}".into()),
            };
            let mut legacy = serde_json::to_value(&child_event).unwrap();
            legacy["AgentSessionLinked"]
                .as_object_mut()
                .unwrap()
                .remove("work_summary");
            assert!(matches!(
                serde_json::from_value::<LogEvent>(legacy).unwrap(),
                LogEvent::AgentSessionLinked {
                    work_summary: None,
                    ..
                }
            ));
            let child_work = observe(&mut projection, child_event).unwrap();
            assert_eq!(child_work.work_summary.as_deref(), Some("Review parser 界"));
            assert_eq!(
                projection.agents[&1].work_summary.as_deref(),
                Some("Repair auth 界")
            );
            assert_eq!(
                projection.children[&1]["child"].work_summary,
                child_work.work_summary
            );
            let repeated = observe(
                &mut projection,
                LogEvent::AgentSessionLinked {
                    pane: 1,
                    provider: provider.into(),
                    session_id: "child".into(),
                    parent_session_id: "root".into(),
                    work_summary: None,
                },
            )
            .unwrap();
            assert_eq!(repeated.work_summary, child_work.work_summary);
            let cleared = observe(
                &mut projection,
                LogEvent::AgentSessionLinked {
                    pane: 1,
                    provider: provider.into(),
                    session_id: "child".into(),
                    parent_session_id: "root".into(),
                    work_summary: Some(String::new()),
                },
            )
            .unwrap();
            assert!(cleared.work_summary.is_none());
            let private = LogEvent::AgentDetail {
                pane: 1,
                invocation: Some(41),
                detail: uniterm_core::AgentDetail::PermissionAsked {
                    tool: "shell".into(),
                    preview: "private command".into(),
                },
            };
            assert!(!changes_day(&private));
            assert!(observe(&mut projection, private).is_none());
            let partial = observe(
                &mut projection,
                LogEvent::AgentSessionObserved {
                    pane: 1,
                    provider: provider.into(),
                    session_id: None,
                    transcript_path: Some("/tmp/isolated-transcript".into()),
                    resume_command: vec![],
                },
            )
            .unwrap();
            assert_eq!(partial.session_id.as_deref(), Some("root"));
            assert_eq!(partial.work_summary.as_deref(), Some("Repair auth 界"));
            assert!(projection.children[&1].contains_key("child"));
            projection = serde_json::from_slice(&serde_json::to_vec(&projection).unwrap()).unwrap();
            let reset = observe(
                &mut projection,
                LogEvent::AgentSessionObserved {
                    pane: 1,
                    provider: provider.into(),
                    session_id: Some("replacement".into()),
                    transcript_path: None,
                    resume_command: vec![],
                },
            )
            .unwrap();
            assert!(reset.work_summary.is_none());
            assert!(!projection.children.contains_key(&1));
            let long = "界".repeat(600);
            let bounded = observe(&mut projection, detail(Some(41), &long)).unwrap();
            assert_eq!(
                bounded.work_summary.unwrap().chars().count(),
                uniterm_core::agent_detail::DETAIL_TEXT_LIMIT
            );
            assert!(observe(&mut projection, detail(Some(41), ""))
                .unwrap()
                .work_summary
                .is_none());
            observe(&mut projection, detail(Some(41), "old task"));
            let rebound = observe(
                &mut projection,
                LogEvent::AgentBound {
                    pane: 1,
                    agent: provider.into(),
                    invocation: Some(42),
                },
            )
            .unwrap();
            assert!(rebound.work_summary.is_none());
            assert!(observe(&mut projection, detail(Some(41), "late old task")).is_none());
            observe(&mut projection, detail(Some(42), "Final recorded work"));
            let ended = observe(&mut projection, LogEvent::AgentUnbound { pane: 1 }).unwrap();
            assert_eq!(ended.work_summary.as_deref(), Some("Final recorded work"));
            assert!(observe(&mut projection, detail(Some(42), "after exit")).is_none());
        }
    }

    #[test]
    fn accepted_summary_refines_unknown_process_group_without_rebinding() {
        let mut projection = Projection::default();
        let observe = |projection: &mut Projection, sequence, event| {
            projection.observe(&EventEnvelope {
                version: crate::eventlog::EVENT_VERSION,
                sequence,
                timestamp_ms: sequence,
                workspace: "isolated-late-identification".into(),
                event,
            })
        };
        observe(
            &mut projection,
            1,
            LogEvent::AgentBound {
                pane: 1,
                agent: "custom".into(),
                invocation: None,
            },
        );
        let detail = |invocation, text: &str| LogEvent::AgentDetail {
            pane: 1,
            invocation,
            detail: uniterm_core::AgentDetail::Summary { text: text.into() },
        };
        let entry = observe(&mut projection, 2, detail(Some(41), "Identified work")).unwrap();
        assert_eq!(entry.work_summary.as_deref(), Some("Identified work"));
        assert_eq!(projection.agents[&1].sequence, 1);
        assert_eq!(projection.agents[&1].process_group, Some(41));
        projection = serde_json::from_slice(&serde_json::to_vec(&projection).unwrap()).unwrap();
        assert!(observe(&mut projection, 3, detail(Some(42), "Wrong process")).is_none());
        assert!(observe(&mut projection, 4, detail(None, "Unknown process")).is_none());
        assert!(observe(&mut projection, 5, detail(Some(41), ""))
            .unwrap()
            .work_summary
            .is_none());
        assert_eq!(projection.agents[&1].sequence, 1);
    }

    #[test]
    fn launch_ancestry_survives_projection_checkpoint_and_invocation_reuse() {
        let mut projection = Projection::default();
        let observe = |projection: &mut Projection, sequence, event| {
            projection.observe(&EventEnvelope {
                version: crate::eventlog::EVENT_VERSION,
                sequence,
                timestamp_ms: sequence,
                workspace: "isolated-pure-projection".into(),
                event,
            })
        };
        observe(
            &mut projection,
            1,
            LogEvent::AgentBound {
                pane: 1,
                agent: "claude".into(),
                invocation: None,
            },
        );
        observe(
            &mut projection,
            2,
            LogEvent::AgentBound {
                pane: 2,
                agent: "codex".into(),
                invocation: None,
            },
        );
        let link = observe(
            &mut projection,
            3,
            LogEvent::AgentDelegated {
                pane: 2,
                invocation: 2,
                parent_invocation: 1,
                context: TimelineContext {
                    project: Some(ProjectId(1)),
                    pane: Some(PaneId(2)),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        assert_eq!(link.parent_invocation, Some(1));
        assert_eq!(
            link.context.project,
            Some(ProjectId(1)),
            "launch context precedes the structural checkpoint"
        );
        let mut projection: Projection =
            serde_json::from_slice(&serde_json::to_vec(&projection).unwrap()).unwrap();
        let status = observe(
            &mut projection,
            4,
            LogEvent::AgentStatus {
                pane: 2,
                status: uniterm_core::AgentStatus::Working,
            },
        )
        .unwrap();
        assert_eq!(status.parent_invocation, Some(1));
        assert_eq!(status.context.project, Some(ProjectId(1)));
        let ended = observe(&mut projection, 5, LogEvent::AgentUnbound { pane: 2 }).unwrap();
        assert_eq!(ended.parent_invocation, Some(1));
        assert_eq!(ended.context.project, Some(ProjectId(1)));
        let new = observe(
            &mut projection,
            6,
            LogEvent::AgentBound {
                pane: 2,
                agent: "gemini".into(),
                invocation: None,
            },
        )
        .unwrap();
        assert_eq!(new.parent_invocation, None);
        assert!(observe(
            &mut projection,
            7,
            LogEvent::AgentDelegated {
                pane: 2,
                invocation: 2,
                parent_invocation: 1,
                context: TimelineContext::default(),
            }
        )
        .is_none());
    }

    #[test]
    fn paging_reads_only_visible_payloads_and_recovers_corrupt_older_rows() {
        use std::io::Read;
        let name = format!("timeline-lazy-{}", std::process::id());
        let day = local_day("2026-09-22").unwrap();
        for sequence in 1..=2048 {
            append(
                &name,
                sequence,
                day.start_ms + sequence,
                LogEvent::TimelineNote {
                    context: TimelineContext::default(),
                    text: "x".repeat(4096),
                },
            );
        }
        let mut index = Index::default();
        let start = std::time::Instant::now();
        let page = index.query(&name, &day.date, "", None, 2048).unwrap();
        let cold = start.elapsed();
        let start = std::time::Instant::now();
        assert_eq!(index.query(&name, &day.date, "", None, 2048).unwrap(), page);
        let warm = start.elapsed();
        let path = Index::root(&name).join(format!("{}.jsonl", day.start_ms / DAY_MS));
        let start = std::time::Instant::now();
        let mut count = 0;
        for line in std::io::BufReader::new(std::fs::File::open(&path).unwrap()).lines() {
            let entry: TimelineEntry =
                serde_json::from_str(&crate::privacy::decode_line(&line.unwrap()).unwrap())
                    .unwrap();
            std::hint::black_box(entry);
            count += 1;
        }
        let eager = start.elapsed();
        assert_eq!(count, 2048);
        eprintln!("timeline 2048 x 4KiB: cold={cold:?}, warm_page={warm:?}, eager_payload_decode={eager:?}, loaded={}/2048", page.entries.len());
        // Poison an off-page payload without changing byte offsets. Paging
        // must leave it unread; requesting that old page must rebuild it.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut first = [0];
        file.read_exact(&mut first).unwrap();
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        file.write_all(b"!").unwrap();
        assert_eq!(
            index.query_once(&name, &day.date, "", None, 2048).unwrap(),
            page
        );
        assert!(index
            .query_once(&name, &day.date, "", Some(2), 2048)
            .is_err());
        let repaired = index.query(&name, &day.date, "", Some(2), 2048).unwrap();
        assert_eq!(repaired.entries.len(), 1);
        assert_eq!(repaired.entries[0].sequence, 1);
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
        crate::history::compact(&name).unwrap();
        assert_eq!(index.query(&name, &day.date, "", None, 600).unwrap(), page);
        // At an empty active suffix, reaching the archived highwater must
        // still leave a reusable active offset. Repeated queries may not
        // silently rebuild all archived pages and replace the cache inode.
        use std::os::unix::fs::MetadataExt as _;
        assert_ne!(index.inode, 0);
        let cache_inode = std::fs::metadata(Index::root(&name).join("index.json"))
            .unwrap()
            .ino();
        assert_eq!(index.query(&name, &day.date, "", None, 600).unwrap(), page);
        assert_eq!(
            std::fs::metadata(Index::root(&name).join("index.json"))
                .unwrap()
                .ino(),
            cache_inode
        );
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
    fn overnight_agents_are_carried_without_inventing_activity_or_reusing_panes() {
        let name = format!("timeline-overnight-{}", std::process::id());
        let today = local_day("today").unwrap();
        let yesterday = today.start_ms - 1000;
        append(
            &name,
            1,
            yesterday,
            LogEvent::AgentBound {
                pane: 7,
                agent: "test".into(),
                invocation: Some(42),
            },
        );
        append(
            &name,
            2,
            yesterday,
            LogEvent::AgentSessionObserved {
                pane: 7,
                provider: "test".into(),
                session_id: Some("parent".into()),
                resume_command: vec!["test".into(), "resume".into(), "parent".into()],
                transcript_path: None,
            },
        );
        append(
            &name,
            3,
            yesterday,
            LogEvent::AgentSessionLinked {
                work_summary: None,
                pane: 7,
                provider: "test".into(),
                session_id: "child".into(),
                parent_session_id: "parent".into(),
            },
        );
        let mut index = Index::default();
        assert!(
            resume(&name, 3)
                .unwrap_err()
                .to_string()
                .contains("No provider resume profile"),
            "a child must never resume its parent's profile"
        );
        let day = index.query(&name, "today", "", None, 3).unwrap();
        assert!(day.entries.is_empty(), "silence is not activity");
        assert_eq!(day.active_agents.len(), 2);
        assert!(day
            .active_agents
            .iter()
            .all(|entry| entry.timestamp_ms == yesterday));
        assert_eq!(day.current_sequence, 3);
        assert!(day.next_before.is_none());
        let rows = uniterm_core::timeline::agent_tree(&day.active_agents);
        assert_eq!(rows[0].span.last.session_id.as_deref(), Some("parent"));
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].span.last.invocation, None);
        assert!(index
            .query(&name, &today.previous, "", None, 3)
            .unwrap()
            .active_agents
            .is_empty());
        assert!(index
            .query(&name, "today", "unrelated", None, 3)
            .unwrap()
            .active_agents
            .is_empty());
        assert!(index
            .query(&name, "today", "", Some(3), 3)
            .unwrap()
            .active_agents
            .is_empty());
        assert_eq!(
            Index::default().query(&name, "today", "", None, 3).unwrap(),
            day
        );
        // A pre-feature cache must rebuild so its missing last-observation
        // fields cannot silently hide running agents after upgrade.
        let path = Index::root(&name).join("index.json");
        let mut cached: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        cached["version"] = 0.into();
        std::fs::write(path, serde_json::to_vec(&cached).unwrap()).unwrap();
        assert_eq!(
            Index::default().query(&name, "today", "", None, 3).unwrap(),
            day
        );
        append(&name, 4, today.start_ms, LogEvent::AgentUnbound { pane: 7 });
        append(
            &name,
            5,
            today.start_ms + 1,
            LogEvent::AgentBound {
                pane: 7,
                agent: "test".into(),
                invocation: Some(43),
            },
        );
        let day = index.query(&name, "today", "", None, 5).unwrap();
        assert!(day.active_agents.is_empty());
        assert!(index.projection.children.is_empty());
        assert_eq!(day.entries.last().unwrap().invocation, Some(5));
        assert_eq!(day.entries.last().unwrap().session_id, None);
        std::fs::remove_dir_all(Index::root(&name)).unwrap();
        crate::eventlog::delete(&name).unwrap();
    }

    #[test]
    fn session_metadata_cannot_revive_an_exited_invocation() {
        let mut projection = Projection::default();
        for (sequence, event) in [
            (
                1,
                LogEvent::AgentBound {
                    pane: 7,
                    agent: "test".into(),
                    invocation: Some(42),
                },
            ),
            (
                2,
                LogEvent::AgentStatus {
                    pane: 7,
                    status: uniterm_core::AgentStatus::Exited,
                },
            ),
            (
                3,
                LogEvent::AgentSessionObserved {
                    pane: 7,
                    provider: "test".into(),
                    session_id: Some("late".into()),
                    resume_command: vec![],
                    transcript_path: None,
                },
            ),
        ] {
            projection.observe(&EventEnvelope {
                version: crate::eventlog::EVENT_VERSION,
                sequence,
                timestamp_ms: sequence,
                workspace: "test".into(),
                event,
            });
        }
        assert!(projection.agents[&7].exited);
        assert_eq!(
            projection.agents[&7]
                .last
                .as_ref()
                .unwrap()
                .session_id
                .as_deref(),
            Some("late")
        );
    }

    #[test]
    fn busy_day_paging_does_not_hide_known_agent_relationships() {
        let name = format!("timeline-busy-agents-{}", std::process::id());
        let day = local_day("today").unwrap();
        append(
            &name,
            1,
            day.start_ms,
            LogEvent::AgentBound {
                pane: 7,
                agent: "test".into(),
                invocation: Some(42),
            },
        );
        append(
            &name,
            2,
            day.start_ms + 1,
            LogEvent::AgentSessionObserved {
                pane: 7,
                provider: "test".into(),
                session_id: Some("parent".into()),
                resume_command: vec![],
                transcript_path: None,
            },
        );
        append(
            &name,
            3,
            day.start_ms + 2,
            LogEvent::AgentSessionLinked {
                work_summary: None,
                pane: 7,
                provider: "test".into(),
                session_id: "child".into(),
                parent_session_id: "parent".into(),
            },
        );
        for sequence in 4..=140 {
            append(
                &name,
                sequence,
                day.start_ms + sequence,
                LogEvent::TimelineNote {
                    context: TimelineContext::default(),
                    text: "Other activity".into(),
                },
            );
        }
        let day = Index::default()
            .query(&name, "today", "", None, 140)
            .unwrap();
        assert_eq!(day.entries.len(), PAGE_SIZE);
        assert_eq!(day.active_agents.len(), 2);
        let rows = uniterm_core::timeline::agent_tree(&day.active_agents);
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].span.last.session_id.as_deref(), Some("child"));
        assert_eq!(
            day.next_before,
            Some(13),
            "carryovers cannot move the paging cursor"
        );
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
                    invocation: None,
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
                    transcript_path: None,
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
                    invocation: None,
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
