//! Lossless event-log maintenance on the ordered persistence worker.
//!
//! A checkpoint is a projection of committed events, never a second source
//! of truth. Its previous active stream is durable and immutable before the
//! checkpoint replaces that stream. Historical readers pin the active inode
//! and follow archive links; recovery reads only the checkpoint and suffix.

use crate::eventlog::{self, EventEnvelope, LogEvent, StructuralProjection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, BufRead, Read, Seek, Write};
use std::path::PathBuf;
use uniterm_core::{ArtifactLedger, InstructionQueue, RunGraph, TaskList, WaitingQueue};

const CHECKPOINT_VERSION: u32 = 1;
/// New events must add this much active data before maintenance is worthwhile.
pub(crate) const COMPACT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PINNED_ARCHIVES: usize = 8;

/// Every recoverable projection at one committed event cursor, including
/// monotonic allocators that must outlive deleted tasks and resolved requests.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RecoveryCheckpoint {
    version: u32,
    pub(crate) through: u64,
    archive: Archive,
    pub(crate) structure: Option<StructuralProjection>,
    pub(crate) tasks: TaskList,
    pub(crate) waiting: WaitingQueue,
    pub(crate) instructions: InstructionQueue,
    #[serde(with = "binary_projection")]
    pub(crate) run_graph: RunGraph,
    #[serde(with = "binary_projection")]
    pub(crate) artifacts: ArtifactLedger,
    pub(crate) runs: Vec<eventlog::DurableOrchestration>,
    pub(crate) invocations: std::collections::HashMap<uniterm_core::PaneId, u64>,
}

// The existing snapshot projection types contain tuple-keyed indexes. JSON
// object keys cannot represent those; use the existing binary serializer for
// these two fields, inside the explicit versioned recovery envelope.
mod binary_projection {
    use bincode::Options as _;
    use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};
    pub(super) fn serialize<T: Serialize, S: Serializer>(
        value: &T,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let bytes = bincode::serialize(value).map_err(serde::ser::Error::custom)?;
        let mut text = String::with_capacity(bytes.len() * 2);
        const HEX: &[u8] = b"0123456789abcdef";
        for byte in bytes {
            text.push(HEX[(byte >> 4) as usize] as char);
            text.push(HEX[(byte & 15) as usize] as char);
        }
        serializer.serialize_str(&text)
    }
    pub(super) fn deserialize<'de, T: DeserializeOwned, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<T, D::Error> {
        let text = String::deserialize(deserializer)?;
        if !text.len().is_multiple_of(2) {
            return Err(serde::de::Error::custom("invalid binary projection length"));
        }
        let bytes: Result<Vec<u8>, D::Error> = text
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let high = (pair[0] as char)
                    .to_digit(16)
                    .ok_or_else(|| serde::de::Error::custom("invalid binary projection"))?;
                let low = (pair[1] as char)
                    .to_digit(16)
                    .ok_or_else(|| serde::de::Error::custom("invalid binary projection"))?;
                Ok(((high << 4) | low) as u8)
            })
            .collect();
        let bytes = bytes?;
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(bytes.len() as u64)
            .deserialize(&bytes)
            .map_err(serde::de::Error::custom)
    }
}

/// Archive names contain only an event cursor, never a user path; deterministic
/// names let interrupted maintenance reuse the already published archive.
/// The hash covers decoded JSON lines so later encryption preserves identity.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Archive {
    file: String,
    sha256: String,
}

impl RecoveryCheckpoint {
    pub(crate) fn validate(&self, sequence: Option<u64>) -> io::Result<()> {
        if self.version > CHECKPOINT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported recovery checkpoint version",
            ));
        }
        if self.version != CHECKPOINT_VERSION
            || self.through == 0
            || sequence != Some(self.through)
            || !self.archive.file.starts_with("events-")
            || !self.archive.file.ends_with(".log")
            || !self
                .archive
                .file
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
            || self.archive.sha256.len() != 64
            || !self
                .archive
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid("invalid recovery checkpoint or archive link"));
        }
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn archive_dir(name: &str) -> PathBuf {
    crate::persist::snapshot_path(name).with_extension("history")
}

pub(crate) fn checkpoint_header(
    reader: &mut io::BufReader<std::fs::File>,
) -> io::Result<Option<RecoveryCheckpoint>> {
    checkpoint_header_with_owner(reader).map(|(checkpoint, _)| checkpoint)
}

fn checkpoint_header_with_owner(
    reader: &mut io::BufReader<std::fs::File>,
) -> io::Result<(Option<RecoveryCheckpoint>, Option<String>)> {
    let mut line = String::new();
    let length = reader.read_line(&mut line)?;
    let decoded = crate::privacy::decode_line(&line)?;
    let envelope = serde_json::from_str::<EventEnvelope>(&decoded).ok();
    let owner = envelope.as_ref().map(|event| event.workspace.clone());
    if let Some(EventEnvelope {
        version,
        sequence,
        event: LogEvent::RecoveryCheckpoint { state },
        ..
    }) = envelope
    {
        if !(1..=eventlog::EVENT_VERSION).contains(&version) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported event version",
            ));
        }
        state.validate(Some(sequence))?;
        Ok((Some(*state), owner))
    } else {
        reader.seek(io::SeekFrom::Current(-(length as i64)))?;
        Ok((None, owner))
    }
}

fn digest(mut reader: impl BufRead) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if !line.ends_with('\n') {
            return Err(invalid("unterminated event archive record"));
        }
        let plain = crate::privacy::decode_line(line.trim_end_matches(['\r', '\n']))?;
        hash.update(plain.trim_end_matches(['\r', '\n']).as_bytes());
        hash.update(b"\n");
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Pin the active stream and expose originals in sequence order, omitting
/// recovery-only checkpoint records. Archive traversal is iterative and each
/// link must strictly decrease the cursor, so cycles cannot consume the stack.
pub(crate) struct Reader {
    reader: io::BufReader<std::fs::File>,
    pending: Vec<ArchiveSegment>,
    spill: Option<std::fs::File>,
    remaining: Option<u64>,
    active: Option<io::BufReader<std::fs::File>>,
    pub(crate) origin: u64,
}

enum ArchiveSegment {
    Pinned(io::BufReader<std::fs::File>),
    Spilled { offset: u64, length: u64 },
}

impl ArchiveSegment {
    fn open(
        self,
        spill: Option<&std::fs::File>,
    ) -> io::Result<(io::BufReader<std::fs::File>, Option<u64>)> {
        match self {
            Self::Pinned(reader) => Ok((reader, None)),
            Self::Spilled { offset, length } => {
                let mut reader = io::BufReader::new(
                    spill
                        .ok_or_else(|| invalid("missing archive spill"))?
                        .try_clone()?,
                );
                reader.seek(io::SeekFrom::Start(offset))?;
                Ok((reader, Some(length)))
            }
        }
    }
}

/// Preserve rename/delete-safe readers without keeping a descriptor per
/// archive or copying lifetime history into memory. Short chains stay zero-copy.
/// Spill bytes remain in their original form, including encryption, and the
/// file is unlinked before use so exit or a crash cannot leave private history.
fn archive_spill() -> io::Result<std::fs::File> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let path = std::env::temp_dir().join(format!(
            "uniterm-history-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                std::fs::remove_file(path)?;
                return Ok(file);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

fn spill_segment(
    mut reader: io::BufReader<std::fs::File>,
    spill: &mut std::fs::File,
) -> io::Result<ArchiveSegment> {
    let offset = spill.stream_position()?;
    let length = io::copy(&mut reader, spill)?;
    Ok(ArchiveSegment::Spilled { offset, length })
}

impl Reader {
    pub(crate) fn new(name: &str, file: std::fs::File, after: u64) -> io::Result<Self> {
        let mut active = io::BufReader::new(file);
        let (mut checkpoint, owner) = checkpoint_header_with_owner(&mut active)?;
        if owner.as_deref().is_some_and(|owner| owner != name) {
            // A renamed stream can begin under its old owner. Prove the
            // complete authoritative rename chain before serving even an
            // early historical cursor, rather than accepting a foreign file.
            let offset = active.stream_position()?;
            active.rewind()?;
            eventlog::validate_owner(&mut active, name)?;
            active.seek(io::SeekFrom::Start(offset))?;
        }
        let mut pending = Vec::new();
        let mut spill = None;
        while checkpoint
            .as_ref()
            .is_some_and(|state| state.through > after)
        {
            let state = checkpoint.take().expect("checked checkpoint");
            let path = archive_dir(name).join(&state.archive.file);
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            let mut reader = io::BufReader::new(file);
            if digest(&mut reader)? != state.archive.sha256 {
                return Err(invalid("event archive checksum mismatch"));
            }
            reader.rewind()?;
            checkpoint = checkpoint_header(&mut reader)?;
            if checkpoint
                .as_ref()
                .is_some_and(|previous| previous.through >= state.through)
            {
                return Err(invalid("cyclic or out-of-order event archive link"));
            }
            if pending.len() == MAX_PINNED_ARCHIVES && spill.is_none() {
                let mut file = archive_spill()?;
                for segment in &mut pending {
                    // Replace each pin as it is copied, closing its descriptor
                    // before opening the next archive in the chain.
                    if let ArchiveSegment::Pinned(reader) = std::mem::replace(
                        segment,
                        ArchiveSegment::Spilled {
                            offset: 0,
                            length: 0,
                        },
                    ) {
                        *segment = spill_segment(reader, &mut file)?;
                    }
                }
                spill = Some(file);
            }
            pending.push(match spill.as_mut() {
                Some(file) => spill_segment(reader, file)?,
                None => ArchiveSegment::Pinned(reader),
            });
        }
        let origin = checkpoint.map_or(0, |state| state.through);
        if let Some(segment) = pending.pop() {
            let (reader, remaining) = segment.open(spill.as_ref())?;
            Ok(Self {
                reader,
                pending,
                spill,
                remaining,
                active: Some(active),
                origin,
            })
        } else {
            Ok(Self {
                reader: active,
                pending,
                spill,
                remaining: None,
                active: None,
                origin,
            })
        }
    }

    pub(crate) fn from_offset(name: &str, file: std::fs::File, offset: u64) -> io::Result<Self> {
        if offset == 0 {
            return Self::new(name, file, 0);
        }
        let mut reader = io::BufReader::new(file);
        reader.seek(io::SeekFrom::Start(offset))?;
        Ok(Self {
            reader,
            pending: Vec::new(),
            spill: None,
            remaining: None,
            active: None,
            origin: 0,
        })
    }

    /// Byte position is reusable only once the reader reaches the pinned
    /// active inode. Earlier history positions belong to immutable archives.
    pub(crate) fn active_offset(&mut self) -> io::Result<Option<u64>> {
        // A query can stop exactly at the last archived record without
        // another read. Advance exhausted segments before deciding whether
        // the saved position belongs to the active inode.
        self.fill_buf()?;
        if self.active.is_some() {
            Ok(None)
        } else {
            self.reader.stream_position().map(Some)
        }
    }
}

use std::os::unix::fs::OpenOptionsExt;

impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let bytes = self.fill_buf()?;
        let length = bytes.len().min(buffer.len());
        buffer[..length].copy_from_slice(&bytes[..length]);
        self.consume(length);
        Ok(length)
    }
}

impl BufRead for Reader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        while self.remaining == Some(0) || self.reader.fill_buf()?.is_empty() {
            if let Some(segment) = self.pending.pop() {
                let (reader, remaining) = segment.open(self.spill.as_ref())?;
                self.reader = reader;
                self.remaining = remaining;
            } else if let Some(active) = self.active.take() {
                self.reader = active;
                self.remaining = None;
                self.spill = None;
            } else {
                break;
            }
        }
        let bytes = self.reader.fill_buf()?;
        Ok(&bytes[..self
            .remaining
            .map_or(bytes.len(), |left| left.min(bytes.len() as u64) as usize)])
    }

    fn consume(&mut self, amount: usize) {
        self.reader.consume(amount);
        if let Some(remaining) = &mut self.remaining {
            *remaining = remaining.saturating_sub(amount as u64);
        }
    }
}

/// Stage immutable archives before publishing the renamed active stream.
/// Keeping the old links until publication makes every interruption readable
/// under at least the old name; a retry can reuse identical staged hardlinks.
pub(crate) fn stage_rename(old: &str, new: &str) -> io::Result<()> {
    let source = archive_dir(old);
    if !source.exists() {
        return Ok(());
    }
    let destination = archive_dir(new);
    crate::persist::ensure_private_dir(&destination)?;
    for item in std::fs::read_dir(&source)? {
        let item = item?;
        if !item.file_type()?.is_file() {
            return Err(invalid("unexpected event archive entry"));
        }
        let target = destination.join(item.file_name());
        match std::fs::hard_link(item.path(), &target) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if digest(io::BufReader::new(std::fs::File::open(item.path())?))?
                    != digest(io::BufReader::new(std::fs::File::open(&target)?))?
                {
                    return Err(invalid(
                        "renamed event archive already exists with different content",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    crate::persist::sync_parent_directory(&destination.join("archive"))?;
    crate::persist::sync_parent_directory(&destination)
}

/// Verify the directly referenced archive cheaply during startup. Complete
/// plaintext hashes are checked only when historical readers traverse it.
pub(crate) fn validate_archive(name: &str) -> io::Result<()> {
    let file = match std::fs::File::open(eventlog::EventLog::path(name)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if let Some(state) = checkpoint_header(&mut io::BufReader::new(file))? {
        let path = archive_dir(name).join(state.archive.file);
        let archive = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        if !archive.metadata()?.is_file() {
            return Err(invalid("event archive is not a regular file"));
        }
    }
    Ok(())
}

/// Compact only after real append work has grown the active suffix. Called
/// on the ordered worker, never from an idle timer or the terminal I/O loop.
pub(crate) fn compact_if_needed(name: &str) -> io::Result<bool> {
    let file = match std::fs::File::open(eventlog::EventLog::path(name)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let length = file.metadata()?.len();
    if length < COMPACT_BYTES {
        return Ok(false);
    }
    let mut reader = io::BufReader::new(file);
    checkpoint_header(&mut reader)?;
    // A large live checkpoint cannot repeatedly compact itself. Only bytes
    // appended after its header count toward the next maintenance threshold.
    if length.saturating_sub(reader.stream_position()?) < COMPACT_BYTES {
        return Ok(false);
    }
    compact(name)
}

/// Archive committed events and atomically publish their complete recovery
/// projection. The caller must hold Workspace ownership and serialize this
/// operation with appends, snapshot writes, renames, and clean shutdown.
/// Returns false for an empty log or an already checkpointed highwater.
pub fn compact(name: &str) -> io::Result<bool> {
    compact_at(name, |_| Ok(()))
}

fn compact_at(name: &str, mut boundary: impl FnMut(&str) -> io::Result<()>) -> io::Result<bool> {
    crate::privacy::verify(name)?;
    validate_archive(name)?;
    let path = eventlog::EventLog::path(name);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let length = file.metadata()?.len();
    let mut reader = io::BufReader::new(file);
    let consistent = eventlog::consistent_prefix_len(&mut reader, name)?;
    if consistent as u64 != length {
        return Err(invalid("repair the event stream before compacting it"));
    }
    reader.rewind()?;
    let prior = checkpoint_header(&mut reader)?;
    let through = eventlog::EventLog::open(name).current_sequence();
    if through == 0 || prior.as_ref().is_some_and(|prior| prior.through == through) {
        return Ok(false);
    }
    // Every reducer reads the same committed stream while appends are paused.
    // These are the exact recovery reducers, including all ID allocators.
    let mut tasks = TaskList::new();
    eventlog::replay_tasks(name, &mut tasks)?;
    let mut waiting = WaitingQueue::new();
    eventlog::replay_waiting(name, &mut waiting)?;
    let mut instructions = InstructionQueue::new();
    eventlog::replay_instructions(name, &mut instructions)?;
    let mut run_graph = RunGraph::new();
    eventlog::replay_run_graph(name, 0, &mut run_graph)?;
    let mut artifacts = ArtifactLedger::new();
    eventlog::replay_artifacts(name, 0, &mut artifacts)?;
    let runs = eventlog::replay_orchestrations(name)?;
    let invocations = eventlog::replay_invocations(name, through)?;
    let structure = eventlog::recover_snapshot(name, None)?
        .as_ref()
        .map(StructuralProjection::from_snapshot);
    eventlog::sync(name)?;
    let directory = archive_dir(name);
    crate::persist::ensure_private_dir(&directory)?;
    let archive = directory.join(format!("events-{through}.log"));
    let sha256 = digest(io::BufReader::new(std::fs::File::open(&path)?))?;
    if archive.exists() {
        if digest(io::BufReader::new(std::fs::File::open(&archive)?))? != sha256 {
            return Err(invalid(
                "existing event archive does not match active stream",
            ));
        }
    } else {
        let temporary = archive.with_extension("tmp");
        let mut archived = crate::persist::open_private_append(&temporary)?;
        archived.set_len(0)?;
        let mut source = std::fs::File::open(&path)?;
        io::copy(&mut source, &mut archived)?;
        crate::persist::sync_file_for_crash(&archived)?;
        std::fs::rename(temporary, &archive)?;
        crate::persist::sync_parent_directory(&archive)?;
    }
    crate::persist::sync_parent_directory(&directory)?;
    boundary("archive")?;
    let state = RecoveryCheckpoint {
        version: CHECKPOINT_VERSION,
        through,
        archive: Archive {
            file: archive.file_name().unwrap().to_string_lossy().into_owned(),
            sha256,
        },
        structure,
        tasks,
        waiting,
        instructions,
        run_graph,
        artifacts,
        runs,
        invocations,
    };
    let envelope = EventEnvelope {
        version: eventlog::EVENT_VERSION,
        sequence: through,
        timestamp_ms: 0,
        workspace: name.into(),
        event: LogEvent::RecoveryCheckpoint {
            state: Box::new(state),
        },
    };
    let line = serde_json::to_string(&envelope).map_err(io::Error::other)? + "\n";
    let temporary = path.with_extension("log.compact.tmp");
    let mut output = crate::persist::open_private_append(&temporary)?;
    output.set_len(0)?;
    output.write_all(crate::privacy::encode_line(name, &line)?.as_bytes())?;
    crate::persist::sync_file_for_crash(&output)?;
    boundary("checkpoint")?;
    std::fs::rename(temporary, &path)?;
    crate::persist::sync_parent_directory(&path)?;
    boundary("published")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use uniterm_core::{PaneId, ProjectId, TaskStatus};

    fn name() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        format!(
            "history-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn append(log: &mut eventlog::EventLog, event: LogEvent) {
        let (name, line) = log.record(event).unwrap();
        eventlog::append_line(&name, &line).unwrap();
    }

    fn history(name: &str, through: u64) -> Vec<EventEnvelope> {
        let mut result = Vec::new();
        eventlog::visit_through(name, 0, through, |event| {
            result.push(event);
            Ok(())
        })
        .unwrap();
        result
    }

    fn projections(name: &str) -> RecoveryCheckpoint {
        let mut tasks = TaskList::new();
        eventlog::replay_tasks(name, &mut tasks).unwrap();
        let mut waiting = WaitingQueue::new();
        eventlog::replay_waiting(name, &mut waiting).unwrap();
        let mut instructions = InstructionQueue::new();
        eventlog::replay_instructions(name, &mut instructions).unwrap();
        let mut graph = RunGraph::new();
        eventlog::replay_run_graph(name, 0, &mut graph).unwrap();
        let mut artifacts = ArtifactLedger::new();
        eventlog::replay_artifacts(name, 0, &mut artifacts).unwrap();
        let runs = eventlog::replay_orchestrations(name).unwrap();
        let structure = eventlog::recover_snapshot(name, None)
            .unwrap()
            .as_ref()
            .map(StructuralProjection::from_snapshot);
        let invocations =
            eventlog::replay_invocations(name, eventlog::EventLog::open(name).current_sequence())
                .unwrap();
        RecoveryCheckpoint {
            version: CHECKPOINT_VERSION,
            through: 0,
            archive: Archive {
                file: String::new(),
                sha256: String::new(),
            },
            structure,
            tasks,
            waiting,
            instructions,
            run_graph: graph,
            artifacts,
            runs,
            invocations,
        }
    }

    fn seed(name: &str) -> eventlog::EventLog {
        use uniterm_core::{
            ArtifactEvent, ArtifactId, ArtifactKind, RoleId, RunGraphEvent, RunId, RunKind,
        };
        let mut log = eventlog::EventLog::open(name);
        append(
            &mut log,
            LogEvent::WorkspaceProjected {
                state: StructuralProjection {
                    active_window: 0,
                    next_pane_id: 2,
                    active_project: ProjectId(1),
                    next_project_id: 2,
                    projects: vec![eventlog::StructuralProject {
                        id: ProjectId(1),
                        name: "project".into(),
                        root: "/tmp".into(),
                        active_pane: Some(PaneId(1)),
                        metadata: vec![],
                    }],
                    windows: vec![eventlog::StructuralWindow {
                        stack_expanded: false,
                        stack_parent: None,
                        layout_mode: Default::default(),
                        project: ProjectId(1),
                        layout: uniterm_core::LayoutNode::Leaf(PaneId(1)),
                        active: PaneId(1),
                        zoomed: None,
                        name: None,
                        panes: vec![eventlog::StructuralPane {
                            id: PaneId(1),
                            cwd: Some("/tmp".into()),
                            metadata: vec![],
                            launch_args: vec!["--model".into(), "test".into()],
                            agent_launch: None,
                        }],
                    }],
                },
            },
        );
        append(
            &mut log,
            LogEvent::AgentBound {
                pane: 1,
                agent: "fake".into(),
                invocation: Some(44),
            },
        );
        append(
            &mut log,
            LogEvent::TaskCreated {
                id: 3,
                title: "retained".into(),
                status: TaskStatus::Doing,
            },
        );
        append(
            &mut log,
            LogEvent::TaskDetailsUpdated {
                id: 3,
                notes: "full notes".into(),
                context: uniterm_core::tasks::TaskContext {
                    prompt: Some("private prompt".into()),
                    ..Default::default()
                },
            },
        );
        append(
            &mut log,
            LogEvent::TaskCreated {
                id: 19,
                title: "deleted".into(),
                status: TaskStatus::Done,
            },
        );
        append(&mut log, LogEvent::TaskDeleted { id: 19 });
        for id in [5, 18] {
            append(
                &mut log,
                LogEvent::WaitingCreated {
                    item: uniterm_core::WaitingItem {
                        id,
                        pane: PaneId(id),
                        invocation: Some(44),
                        kind: uniterm_core::WaitingKind::Permission,
                        summary: "permission".into(),
                    },
                },
            );
        }
        append(
            &mut log,
            LogEvent::WaitingResolved {
                id: 18,
                resolution: uniterm_core::WaitingResolution::Answered,
            },
        );
        let mut queue = InstructionQueue::new();
        for id in 0..2 {
            let item = queue.allocate(
                PaneId(1),
                44,
                uniterm_core::InstructionAuthor::Cli,
                id,
                "remember",
            );
            append(&mut log, LogEvent::InstructionQueued { item });
        }
        append(
            &mut log,
            LogEvent::InstructionDelivery {
                id: 2,
                delivery_id: 71,
                accepted: true,
                boundary: uniterm_core::InstructionBoundary::SendNow,
            },
        );
        for change in [
            RunGraphEvent::Created {
                run: RunId(1),
                parent: None,
                project: ProjectId(1),
                kind: RunKind::Workflow,
                task_id: 3,
                title: "run".into(),
            },
            RunGraphEvent::RoleDeclared {
                run: RunId(1),
                role: RoleId(1),
                name: "worker".into(),
                pane: PaneId(1),
                provider: "fake".into(),
            },
            RunGraphEvent::Activated {
                run: RunId(1),
                role: RoleId(1),
                activation: 1,
            },
        ] {
            append(&mut log, LogEvent::RunGraph { change });
        }
        append(
            &mut log,
            LogEvent::ArtifactLedger {
                change: ArtifactEvent::Observed {
                    artifact: ArtifactId(1),
                    project: ProjectId(1),
                    producer_run: RunId(1),
                    producer_role: RoleId(1),
                    kind: ArtifactKind::Plan,
                    path: "plan.md".into(),
                    digest: "a".repeat(64),
                    size: 42,
                },
            },
        );
        let state = uniterm_core::orchestrate::State::new(
            vec![uniterm_core::orchestrate::Role::new("worker", false)],
            1,
        );
        append(
            &mut log,
            LogEvent::OrchestrationProjected {
                run: eventlog::DurableOrchestration {
                    kind: uniterm_proto::OrchestrationKind::Workflow,
                    task_id: 3,
                    template: Some("solo".into()),
                    goal: "finish".into(),
                    role_providers: vec![eventlog::DurableRoleProvider {
                        provider: "fake".into(),
                        command: "fake".into(),
                    }],
                    agent_id: "fake".into(),
                    agent_cmd: "fake".into(),
                    role_panes: vec![PaneId(1)],
                    started: vec![true],
                    state,
                    checkpoints: vec![(1, "hash".into())],
                    guardrail: Box::new(eventlog::DurableGuardrail {
                        started_at_ms: 123,
                        elapsed_triggered: true,
                        ..Default::default()
                    }),
                },
            },
        );
        append(
            &mut log,
            LogEvent::GuardrailDecision {
                record: uniterm_core::GuardrailRecord {
                    project: Some(ProjectId(1)),
                    run: Some(RunId(1)),
                    action: uniterm_core::GuardAction::ElapsedLimit {
                        elapsed_seconds: 61,
                        limit_seconds: 60,
                    },
                    decision: uniterm_core::GuardDecision::Ask {
                        reason: "review elapsed run".into(),
                    },
                },
            },
        );
        append(
            &mut log,
            LogEvent::TimelineNote {
                context: Default::default(),
                text: "history must remain".into(),
            },
        );
        log
    }

    #[test]
    fn long_archive_chain_reads_with_bounded_descriptors_after_rename_and_delete() {
        const CHILD: &str = "UNITERM_ARCHIVE_FD_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "history::tests::long_archive_chain_reads_with_bounded_descriptors_after_rename_and_delete", "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let old = name();
        let new = name();
        let mut log = seed(&old);
        for i in 0..80 {
            append(
                &mut log,
                LogEvent::TimelineNote {
                    context: Default::default(),
                    text: format!("archive {i}"),
                },
            );
            assert!(compact(&old).unwrap());
        }
        append(
            &mut log,
            LogEvent::WorkspaceRenamed {
                old: old.clone(),
                new: new.clone(),
            },
        );
        let expected = history(&old, log.current_sequence());
        // This subprocess contains no concurrent tests. The chain is longer
        // than its descriptor allowance; never alter the parent test runner.
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: valid pointers to a native rlimit; changes only this child.
        unsafe {
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
            limit.rlim_cur = limit.rlim_cur.min(32);
            assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
        }
        let mut pinned = Reader::new(
            &old,
            std::fs::File::open(eventlog::EventLog::path(&old)).unwrap(),
            0,
        )
        .unwrap();
        assert!(pinned.spill.is_some());
        assert!(pinned
            .pending
            .iter()
            .all(|segment| matches!(segment, ArchiveSegment::Spilled { .. })));
        assert!(pinned.active_offset().unwrap().is_none());
        eventlog::rename(&old, &new).unwrap();
        eventlog::delete(&new).unwrap();
        let actual = pinned
            .lines()
            .map(|line| serde_json::from_str::<EventEnvelope>(&line.unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn checkpoint_preserves_every_projection_allocator_and_historical_cursor() {
        let name = name();
        let mut log = seed(&name);
        let before = projections(&name);
        let events = history(&name, log.current_sequence());
        let pinned = std::fs::File::open(eventlog::EventLog::path(&name)).unwrap();
        assert!(compact(&name).unwrap());
        assert_eq!(before, projections(&name));
        assert_eq!(events, history(&name, log.current_sequence()));
        let pinned_lines = Reader::new(&name, pinned, 0)
            .unwrap()
            .lines()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(pinned_lines.len(), events.len());
        let active = std::fs::read(eventlog::EventLog::path(&name)).unwrap();
        assert!(!compact(&name).unwrap());
        assert_eq!(
            active,
            std::fs::read(eventlog::EventLog::path(&name)).unwrap()
        );
        for after in 0..=log.current_sequence() {
            let mut found = Vec::new();
            eventlog::visit_through(&name, after, log.current_sequence(), |event| {
                found.push(event);
                Ok(())
            })
            .unwrap();
            assert_eq!(
                found,
                events
                    .iter()
                    .filter(|event| event.sequence > after)
                    .cloned()
                    .collect::<Vec<_>>()
            );
        }
        append(
            &mut log,
            LogEvent::TaskRetitled {
                id: 3,
                title: "after checkpoint".into(),
            },
        );
        let second = projections(&name);
        let history_second = history(&name, log.current_sequence());
        assert!(compact(&name).unwrap());
        assert_eq!(second, projections(&name));
        assert_eq!(history_second, history(&name, log.current_sequence()));
        assert_eq!(
            eventlog::EventLog::open(&name).current_sequence(),
            log.current_sequence()
        );
        eventlog::delete(&name).unwrap();
    }

    #[test]
    fn interrupted_publication_never_loses_events_and_reuses_the_archive() {
        for point in ["archive", "checkpoint", "published"] {
            let name = name();
            let log = seed(&name);
            let before = projections(&name);
            let events = history(&name, log.current_sequence());
            assert!(compact_at(&name, |boundary| if boundary == point {
                Err(io::Error::other("injected crash"))
            } else {
                Ok(())
            })
            .is_err());
            assert_eq!(before, projections(&name));
            assert_eq!(events, history(&name, log.current_sequence()));
            assert_eq!(compact(&name).unwrap(), point != "published");
            assert_eq!(std::fs::read_dir(archive_dir(&name)).unwrap().count(), 1);
            eventlog::delete(&name).unwrap();
        }
    }

    #[test]
    fn archived_corruption_is_reported_and_does_not_slow_active_recovery() {
        let name = name();
        let log = seed(&name);
        let before = projections(&name);
        compact(&name).unwrap();
        let path = std::fs::read_dir(archive_dir(&name))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(path, b"corrupt\n").unwrap();
        assert_eq!(before, projections(&name));
        let error =
            eventlog::visit_through(&name, 0, log.current_sequence(), |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("checksum"));
        eventlog::delete(&name).unwrap();
    }

    #[test]
    fn compacted_rename_forget_and_non_one_origin_preserve_history() {
        let original = name();
        let renamed = name();
        let mut log = seed(&original);
        compact(&original).unwrap();
        let before = projections(&original);
        append(
            &mut log,
            LogEvent::WorkspaceRenamed {
                old: original.clone(),
                new: renamed.clone(),
            },
        );
        log.rename_projection(&renamed);
        eventlog::rename(&original, &renamed).unwrap();
        assert_eq!(before, projections(&renamed));
        assert_eq!(
            history(&renamed, log.current_sequence())
                .last()
                .unwrap()
                .event,
            LogEvent::WorkspaceRenamed {
                old: original,
                new: renamed.clone()
            }
        );
        compact(&renamed).unwrap();
        assert_eq!(before, projections(&renamed));
        eventlog::delete(&renamed).unwrap();
        assert!(!archive_dir(&renamed).exists());

        let name = name();
        let line = serde_json::to_string(&EventEnvelope {
            version: eventlog::EVENT_VERSION,
            sequence: 901,
            timestamp_ms: 7,
            workspace: name.clone(),
            event: LogEvent::TaskCreated {
                id: 2,
                title: "later origin".into(),
                status: TaskStatus::Todo,
            },
        })
        .unwrap()
            + "\n";
        eventlog::append_line(&name, &line).unwrap();
        let before = history(&name, 901);
        compact(&name).unwrap();
        assert_eq!(before, history(&name, 901));
        eventlog::delete(&name).unwrap();
    }
    #[test]
    fn protected_archives_remain_readable_after_migration_and_further_compaction() {
        const CHILD: &str = "UNITERM_HISTORY_PRIVACY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let key = std::env::temp_dir().join(format!("{}-key", name()));
            crate::privacy::generate_key(&key).unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "history::tests::protected_archives_remain_readable_after_migration_and_further_compaction"])
                .env(CHILD, "1").env("UNITERM_STATE_KEY_FILE", &key).status().unwrap();
            std::fs::remove_file(key).unwrap();
            assert!(status.success());
            return;
        }
        let name = name();
        let mut log = seed(&name);
        let before = projections(&name);
        let events = history(&name, log.current_sequence());
        compact(&name).unwrap();
        let key = PathBuf::from(std::env::var_os("UNITERM_STATE_KEY_FILE").unwrap());
        crate::privacy::protect(&name, &key).unwrap();
        crate::privacy::protect(&name, &key).unwrap();
        assert_eq!(before, projections(&name));
        assert_eq!(events, history(&name, log.current_sequence()));
        append(
            &mut log,
            LogEvent::TaskRetitled {
                id: 3,
                title: "private next task".into(),
            },
        );
        let after = projections(&name);
        compact(&name).unwrap();
        assert_eq!(after, projections(&name));
        assert_eq!(
            history(&name, log.current_sequence()).len(),
            events.len() + 1
        );
        for index in 0..MAX_PINNED_ARCHIVES {
            append(
                &mut log,
                LogEvent::TimelineNote {
                    context: Default::default(),
                    text: format!("private spilled note {index}"),
                },
            );
            compact(&name).unwrap();
        }
        let pinned = Reader::new(
            &name,
            std::fs::File::open(eventlog::EventLog::path(&name)).unwrap(),
            0,
        )
        .unwrap();
        let spill = pinned.spill.as_ref().expect("long protected chain spills");
        use std::os::unix::fs::{FileExt, MetadataExt};
        assert_eq!(spill.metadata().unwrap().nlink(), 0);
        let mut bytes = vec![0; spill.metadata().unwrap().len() as usize];
        spill.read_exact_at(&mut bytes, 0).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("private"));
        assert_eq!(
            pinned
                .lines()
                .collect::<io::Result<Vec<_>>>()
                .unwrap()
                .len(),
            events.len() + 1 + MAX_PINNED_ARCHIVES
        );
        for path in std::fs::read_dir(archive_dir(&name))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .chain([eventlog::EventLog::path(&name)])
        {
            let bytes = std::fs::read_to_string(path).unwrap();
            assert!(!bytes.contains("private prompt"));
            assert!(!bytes.contains("private next task"));
        }
        eventlog::delete(&name).unwrap();
        crate::privacy::forget(&name).unwrap();
    }

    #[test]
    fn archive_links_reject_traversal_future_schema_and_missing_files() {
        let name = name();
        seed(&name);
        compact(&name).unwrap();
        let path = eventlog::EventLog::path(&name);
        let original = std::fs::read_to_string(&path).unwrap();
        let mut envelope: EventEnvelope = serde_json::from_str(&original).unwrap();
        let LogEvent::RecoveryCheckpoint { state } = &mut envelope.event else {
            panic!("checkpoint");
        };
        state.archive.file = "events-../outside.log".into();
        std::fs::write(&path, serde_json::to_string(&envelope).unwrap() + "\n").unwrap();
        assert!(eventlog::repair_consistent_prefix(&name).is_err());
        let mut envelope: EventEnvelope = serde_json::from_str(&original).unwrap();
        envelope.version = eventlog::EVENT_VERSION + 1;
        std::fs::write(&path, serde_json::to_string(&envelope).unwrap() + "\n").unwrap();
        let error = eventlog::repair_consistent_prefix(&name).err().unwrap();
        assert!(eventlog::is_future_schema_error(&error));
        std::fs::write(&path, &original).unwrap();
        std::fs::remove_dir_all(archive_dir(&name)).unwrap();
        assert!(eventlog::repair_consistent_prefix(&name).is_err());
        // Validation must not rewrite the authoritative checkpoint.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        eventlog::delete(&name).unwrap();
    }

    #[test]
    fn staging_a_rename_keeps_both_archive_names_readable_until_publication() {
        let old = name();
        let new = name();
        let mut log = seed(&old);
        compact(&old).unwrap();
        append(
            &mut log,
            LogEvent::WorkspaceRenamed {
                old: old.clone(),
                new: new.clone(),
            },
        );
        let events = history(&old, log.current_sequence());
        let pinned = Reader::new(
            &old,
            std::fs::File::open(eventlog::EventLog::path(&old)).unwrap(),
            0,
        )
        .unwrap();
        stage_rename(&old, &new).unwrap();
        assert_eq!(history(&old, log.current_sequence()), events);
        stage_rename(&old, &new).unwrap();
        // Emulate a crash immediately after log publication, before old
        // archive links are cleaned. Both namespaces retain all the bytes.
        std::fs::rename(
            eventlog::EventLog::path(&old),
            eventlog::EventLog::path(&new),
        )
        .unwrap();
        assert_eq!(history(&new, log.current_sequence()), events);
        assert!(archive_dir(&old).exists());
        eventlog::delete(&old).unwrap();
        assert_eq!(
            pinned.lines().collect::<Result<Vec<_>, _>>().unwrap().len(),
            events.len()
        );
        assert_eq!(history(&new, log.current_sequence()), events);
        eventlog::delete(&new).unwrap();
    }
    #[test]
    fn checkpoint_binary_fields_reject_corrupt_allocation_lengths() {
        let name = name();
        seed(&name);
        compact(&name).unwrap();
        let path = eventlog::EventLog::path(&name);
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["event"]["RecoveryCheckpoint"]["state"]["run_graph"] =
            serde_json::Value::String("ff".repeat(8));
        assert!(serde_json::from_value::<EventEnvelope>(value).is_err());
        eventlog::delete(&name).unwrap();
    }

    #[test]
    fn exact_binding_replay_uses_archives_only_for_an_older_snapshot() {
        let name = name();
        let mut log = seed(&name);
        let first = log.current_sequence();
        let bindings = eventlog::replay_invocations(&name, first).unwrap();
        append(
            &mut log,
            LogEvent::AgentBound {
                pane: 1,
                agent: "fake".into(),
                invocation: Some(99),
            },
        );
        let second = log.current_sequence();
        compact(&name).unwrap();
        assert_eq!(
            eventlog::replay_invocations(&name, first).unwrap(),
            bindings
        );
        assert_eq!(
            eventlog::replay_invocations(&name, second).unwrap()[&PaneId(1)],
            second
        );
        // Corrupted historical bytes must not force a full scan for a current
        // snapshot's exact invocation identity.
        let archive = std::fs::read_dir(archive_dir(&name))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(archive, b"damaged history\n").unwrap();
        assert_eq!(
            eventlog::replay_invocations(&name, second).unwrap()[&PaneId(1)],
            second
        );
        assert!(eventlog::replay_invocations(&name, first).is_err());
        eventlog::delete(&name).unwrap();
    }

    #[test]
    fn automatic_maintenance_is_size_gated_and_does_not_rearchive_its_checkpoint() {
        let name = name();
        let mut log = seed(&name);
        assert!(!compact_if_needed(&name).unwrap());
        append(
            &mut log,
            LogEvent::TimelineNote {
                context: Default::default(),
                text: "x".repeat(COMPACT_BYTES as usize),
            },
        );
        assert!(compact_if_needed(&name).unwrap());
        let bytes = std::fs::read(eventlog::EventLog::path(&name)).unwrap();
        assert!(!compact_if_needed(&name).unwrap());
        assert_eq!(
            bytes,
            std::fs::read(eventlog::EventLog::path(&name)).unwrap()
        );
        eventlog::delete(&name).unwrap();
    }
    #[test]
    fn cyclic_links_and_foreign_history_are_rejected_without_rewriting_data() {
        let name = name();
        let log = seed(&name);
        compact(&name).unwrap();
        let path = eventlog::EventLog::path(&name);
        let original = std::fs::read(&path).unwrap();
        let mut envelope: EventEnvelope = serde_json::from_slice(&original).unwrap();
        let LogEvent::RecoveryCheckpoint { state } = &mut envelope.event else {
            panic!("checkpoint");
        };
        let archive = archive_dir(&name).join(&state.archive.file);
        std::fs::write(&archive, &original).unwrap();
        state.archive.sha256 =
            digest(io::BufReader::new(std::fs::File::open(&archive).unwrap())).unwrap();
        std::fs::write(&path, serde_json::to_string(&envelope).unwrap() + "\n").unwrap();
        assert!(
            eventlog::visit_through(&name, 0, log.current_sequence(), |_| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("cyclic")
        );
        let foreign = "another-workspace";
        envelope.workspace = foreign.into();
        std::fs::write(&path, serde_json::to_string(&envelope).unwrap() + "\n").unwrap();
        assert!(eventlog::visit_through(&name, 0, 1, |_| Ok(())).is_err());
        eventlog::delete(&name).unwrap();
    }

    #[test]
    fn maintenance_leaves_empty_or_damaged_active_streams_unchanged() {
        let name = name();
        assert!(!compact(&name).unwrap());
        assert!(!archive_dir(&name).exists());
        seed(&name);
        let path = eventlog::EventLog::path(&name);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"partial record");
        std::fs::write(&path, &bytes).unwrap();
        assert!(compact(&name).is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert!(!archive_dir(&name).exists());
        eventlog::delete(&name).unwrap();
    }
    #[test]
    fn clean_stop_retains_checkpoints_and_quarantine_preserves_all_archive_bytes() {
        let name = name();
        let log = seed(&name);
        let events = history(&name, log.current_sequence());
        let snapshot = eventlog::recover_snapshot(&name, None).unwrap().unwrap();
        crate::persist::save(&name, &snapshot).unwrap();
        compact(&name).unwrap();
        let before = projections(&name);
        crate::persist::delete(&name);
        assert!(!crate::persist::exists(&name));
        assert_eq!(before, projections(&name));
        let original = std::fs::read(eventlog::EventLog::path(&name)).unwrap();
        let backup = eventlog::quarantine(&name).unwrap().unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), original);
        let archive_backup = std::fs::read_dir(crate::persist::state_dir())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&format!("{name}.history.corrupt-"))
            })
            .unwrap();
        assert!(!eventlog::exists(&name));
        std::fs::rename(backup, eventlog::EventLog::path(&name)).unwrap();
        std::fs::rename(archive_backup, archive_dir(&name)).unwrap();
        assert_eq!(events, history(&name, log.current_sequence()));
        eventlog::delete(&name).unwrap();
    }
}
