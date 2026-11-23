# 05 - Session Persistence

Uniterm owns the terminal grid and layout, so it can capture them directly for recovery.
Persistence is built into the server and preserves structured state through an event log, atomic snapshots, and a Workspace catalog.
Crash recovery and intentional stops have different retention guarantees, described below.

## Three mechanisms, one subsystem

Persistence is one subsystem with three write paths, all living in the agent runtime so they never touch the render loop.

1. **The event log** (the continuous record).
   An append-only log of everything structurally meaningful, written as it happens.
2. **Snapshots** (the point-in-time capture).
   Periodic and event-triggered binary captures of the full tree plus grid content, from which a restore is reconstructed.
3. **The Workspace catalog** (the clean-stop structural record).
   Append-only lightweight definitions containing only Workspace identity, ordered Projects and their roots, ordered Tabs and names, and pane-free split geometry.

The event log is the source of truth for history and for agentic state; snapshots are the fast-restore artifact for terminal content and layout.
The Workspace catalog has a narrower promise: an intentional stop can discard runtime state while retaining the setup needed to start working again.

## Clean stop and lightweight recovery

`ut workspace stop NAME` stops the server but retains its latest Workspace catalog definition.
`ut workspace stop --all` does the same for every running Workspace and waits for their final catalog writes to finish.
The stopped Workspace remains visible beside running Workspaces in `ut workspace list` and in the Manage Workspaces modal.
Selecting a stopped Workspace starts a new server and recreates every Project at its remembered path with one fresh shell Pane per remembered split leaf.
The active Project, Project ordering, Tab ordering, Tab names, split directions, split ratios, and the Agents and Web Servers Project-or-Workspace scope choices are restored.

This lightweight path retains only anonymous split geometry.
It deliberately does not retain Pane identities, terminal content, scrollback, running commands, agents, web servers, or other process state.
An intentional stop records the catalog definition and deletes the crash snapshot, which is the crash marker.
The event stream is retained across an intentional stop, because Tasks, the run graph, the artifact ledger, and the guardrail audit trail are projections of it and must outlive a stop.
Without a snapshot the next start rebuilds structure from the catalog and replays only those agentic projections; waiting items and instructions bound to an invocation that no longer runs are resolved as closed before fresh shells can reuse a Pane id.
`ut workspace forget NAME` permanently removes a stopped Workspace and its remaining recovery artifacts; it refuses to remove a running Workspace.
`ut workspace forget --all` permanently removes every stopped Workspace and orphaned recovery artifact.
For safety, the bulk forget refuses the whole operation while any Workspace is running; run `ut workspace stop --all` first.

## Recovery guarantees

Damaged durable state is repaired or quarantined so it does not keep a Workspace from starting.
A stream written by a newer schema remains fatal, because a newer binary can still read it and an older one must not touch it.
Operating-system resource exhaustion can also prevent startup; that failure preserves the recovery checkpoint instead of replacing it with an empty Workspace.

- The first record of a stream establishes its origin, which may be any sequence.
  Imported streams can begin above one, and lossless maintenance keeps a recovery checkpoint followed by the active suffix, so a stream that begins mid-history is contiguous, not damaged.
  Only a gap or repeat after the origin is damage, and it is repaired by truncating to the last consistent prefix with the original preserved as `NAME.log.corrupt-<nanos>`.
- Every projection (structure, tasks, waiting queue, instructions, run graph, artifact ledger, orchestrations) is replayed into scratch state before any Pane is spawned, so a rejected stream cannot leave half-restored state behind.
- A stream or snapshot that still cannot be replayed (foreign ownership, an invalid rename record, an impossible run-graph transition) is quarantined: both files are renamed to `.corrupt-<nanos>` siblings, a diagnostic names them on stderr, and the Workspace starts from its catalog definition.
- A running server holds two claims for its lifetime: a `flock` beside the socket and a POSIX record lock on `locks/NAME.lock` inside the state directory.
  The runtime directory differs between a desktop login, an SSH session, a systemd user unit, and a test harness, while the state directory does not; the second claim guarantees that no other process, including `ut workspace forget` and `ut workspace rename`, can ever operate on the durable files of a live Workspace.
- A Workspace rename holds both old and new claims until its ordered durability worker acknowledges the exact request and names.
  A failed or unrelated acknowledgment cannot release the old claim, and repeated renames retain each outstanding claim independently.
  If shutdown exceeds its bounded wait, the existing join thread keeps these claims until the worker actually exits.
- A clean stop freezes event and checkpoint writes before it queues the snapshot deletion, so nothing that drains later in the same poll batch can recreate the crash marker.
- Only the latest catalog line is ever read, so identical consecutive definitions are not appended and a server folds any longer catalog to its latest valid line, atomically, when it starts.

## The event log

An append-only log is the ground truth, and every view (the Observatory timeline, the workflow engine's recovery, the restore path) is a projection of it.

- Append-only, so concurrent writers are safe and crash recovery is a replay, not a repair.
- One logical stream per Workspace, with a bounded active suffix and immutable archives retaining every historical event.
- Entry kinds include: Project, Tab, and Pane created or destroyed; layout changed; Pane command started or exited; working directory changed; metadata changed; and the full set of agentic events.
- Storage: `rusqlite` with a bundled SQLite is the leading candidate for the log because it gives indexed queries (the timeline filters by type, goal, and agent) and safe concurrent reads for free; a custom append-only file is the fallback if SQLite proves heavier than the budget allows.
  This is validated in the persistence spike.

The event log is what makes the Observatory timeline, cost rollups, and audit trails free, and it is what lets the workflow engine reconstruct in-flight orchestration after a restart.

The custom log suppresses identical adjacent records because they contain no timestamp or distinct state information.
The ordered persistence worker checks maintenance on the first real append and after each additional 16 MiB of prepared event data.
Once the active suffix exceeds 16 MiB, `history::compact` projects its committed highwater into a versioned recovery checkpoint and retains its original bytes in an immutable archive under `NAME.history/`.
The checkpoint contains structure, Tasks, waiting items, instructions, run graph, artifact ledger, active orchestrations and their captured guardrail limits, exact invocation identities, and every monotonic allocator.
It is a projection of committed events, with its own event-envelope cursor; it never consumes a new historical event sequence.
The archive file, its directory, and the parent state directory are synced before the checkpoint atomically replaces the active log.
An interrupted publication leaves either the original stream or a complete checkpoint with its archived originals; retrying an unpublished highwater reuses its archive.
Maintenance failures are reported independently from successful event appends and do not suppress their live delivery.

Recovery validates the active file and directly referenced archive, then replays only the checkpoint and its suffix.
It does not hash or scan the archived lifetime history during ordinary startup.
A terminal snapshot older than the checkpoint can still prove its original invocation identities from the archives.
The full timeline and subscription readers transparently follow archived original records, check their decoded SHA-256 identities and sequence continuity, and hide recovery-only checkpoint records.
Readers retain at most eight archive descriptors; longer chains copy their requested archive suffixes to one private, immediately unlinked temporary file, retaining the original encryption.
This bounds descriptors and memory while preserving an in-progress read across Workspace rename or deletion.
Long-chain reads require temporary disk space proportional to the requested archived data; short chains retain the direct-read path.
Archive links remain within the Workspace's history directory and strictly decrease their cursor, preventing traversal and cycles.
Already opened historical segments remain pinned across rename or deletion.

This bounds active replay work by current projection size plus new events, not total lifetime event count.
It is not a disk-retention policy: immutable archives and their historical checkpoint copies continue growing, and live Tasks and other retained projections can also grow.
No historical event is deleted by maintenance.
Clean stop keeps the checkpoint and archives, while Workspace forget removes them along with the active stream.
Protection migration encrypts archived records as well as active state without changing their decoded identities.
The `history_spike` release workload measures 100,000 events through the actual file-opening, validation, cursor-discovery and recovery reducers before and after maintenance, and verifies that every original event remains queryable.
The opt-in long-duration reliability workload separately exercises connection churn and development-server detection.

## Snapshots

Snapshots capture what the event log does not replay cheaply: the actual terminal content and the exact layout at a moment.

The mio loop captures recent compact cells and their required arenas into owned `GridCapture` values, without allocating a String for each cell.
The ordered persistence worker serializes those captures directly into schema v12, borrowing grapheme text and using stack buffers for ordinary characters.
This avoids both core-thread serialization and a worker-side expanded fleet of cell Strings.
The live grid may resize, compact its arenas, or disappear after capture without changing the checkpoint.
`tests/checkpoint_capture.rs` verifies exact byte compatibility with resolved snapshots, including Unicode, styles, source mutation, and event cursors.
Capture remains synchronous and proportional to retained content; the release `checkpoint_spike` measures its latency and allocation budget separately from worker serialization.

A snapshot contains, for the whole server:

- The tree: Workspace, Projects, Tabs, Panes, and their stable identities, roots, names, and metadata.
- Per Tab: its owning Project and the structured layout tree (real geometry, not an opaque string).
- Per pane: the command that was running, the working directory, the environment needed to relaunch, the shell, and any agent binding (which agent, which role, which goal or workflow).
- Per pane: the **grid and scrollback content**, serialized compactly (the sparse cell model serializes well; this is the thing the plugins cannot do).
- The focus state: active Workspace, Project, Tab, and Pane.

Snapshots are written:

- On a two-second damage-armed cadence when terminal state is dirty, because a snapshot is a cheap in-process serialization rather than a shell-out storm.
  The first dirty batch fixes the deadline so continuous output cannot postpone recovery indefinitely, and a completed checkpoint disarms it until new PTY output arrives.
- On **significant events**: a pane opens or closes, a layout changes, or a workflow advances.
- **Atomically**: written to a temp file and renamed, so a crash mid-write never corrupts the current snapshot.

Per-Pane working-directory recovery is cross-platform and event-driven.
Every Pane caches the directory it was launched in, then advances that cache from validated OSC 7 `file:` reports when the shell changes directory.
Linux may additionally resolve the foreground process directory through `/proc`, but restore correctness never depends on `/proc`.
A directory change immediately writes an event-first snapshot, and the runtime flushes the event stream before atomically replacing and syncing that checkpoint.
macOS adds `F_FULLFSYNC` to the portable `sync_all` boundary so a forced power loss receives the strongest local-filesystem durability guarantee available.

Workspace catalog writes acknowledge their exact Workspace and definition after a successful flush.
Only an acknowledged definition enters the core's deduplication cache, and a different in-flight definition prevents a return to an older layout from being incorrectly suppressed.
Failures remain eligible for the next checkpoint or clean-stop save, and are surfaced to the core.
If the latest catalog save still fails at clean stop, the runtime retains the crash checkpoint so the next start can recover the layout from the snapshot and event stream.
A retry after a partial catalog append starts a fresh record boundary.
Older snapshots with no Pane directory, relative values, or a nested directory that disappeared while the machine was down restore the Pane at its owning Project root instead of inheriting the server's home directory or dropping the Pane.
If both directories or the executable are unavailable, the Pane remains in its original layout with its saved history and a visible retry diagnostic.
It owns no child process, does not claim a live agent, and does not run an old command in an unrelated directory.
Enter, including `ut pane submit PANE`, retries the original saved launch profile after the directory or executable becomes available.
Native provider executable checks run on bounded blocking workers, including explicit Today resume, so slow PATH directories do not block terminal input.
Existing restored Panes wait for worker capacity without requiring a manual retry, while excess interactive requests receive a busy response.
The worker resolves the exact saved provider executable and checks its provider identity before spawning; the original bare command and explicit arguments remain in the durable profile for future restarts.
A pending check leaves restored content in a childless Pane; closing the Pane, moving it to another Project, changing its Project root or PATH, or disconnecting a requesting control client invalidates the continuation.
Only a successful native bind transfers Tasks from the exact saved invocation to the new invocation.
One version-7 `AgentResumed` record publishes the native profile and binding together with that exact Task transfer, so a crash cannot commit a new binding without its resumable arguments or ownership.
Today resume also includes its new Pane structure and predecessor in that record, including read-only manager scope.
An incomplete record repairs to the previous binding; ordinary fresh launches still clear the old native profile.
Repeated checkpoints and failed retries retain the original content and profile without persisting diagnostics over them.
An intentional stop keeps the checkpoint while any Pane remains unavailable; live Panes in that checkpoint have their commands, agent bindings, metadata, and terminal content cleared so they restart as fresh shells.
If the operating system cannot allocate even a childless recovery terminal, startup fails explicitly, retains the original checkpoint, and reaps any processes already started during recovery.

Clean stop is different from crash recovery: it records the lightweight Workspace definition and removes the full snapshot.
It retains the event stream so Tasks, the run graph, the artifact ledger, and the audit trail survive an intentional stop.

## Restore

Ordinary `ut` and `uniterm` startup waits for an explicit server readiness signal after recovery completes.
There is no fixed startup deadline: a large event log or slow disk does not cause the launcher to kill a recovering server.
After one second the launcher shows a restoring message; it blocks on startup pipes rather than repeatedly querying the socket.
A server that fails before readiness reports its startup error, with diagnostics drained continuously and a bounded tail retained.
Log-prefix validation streams one record at a time and preserves the original log before repairing a damaged suffix.


After an unclean server exit, a later start can restore from the richer crash snapshot.
After an intentional stop, startup uses the lightweight Workspace catalog path described above.

The restore is **atomic and reconstructive**, not a replay of send-keys:

1. Load the latest good snapshot and the tail of the event log.
2. Rebuild the full tree in memory: Workspace, Projects, Tabs, Panes, and their structured layouts, all at once.
3. For each pane, replay its serialized grid and scrollback into a fresh grid so the history is actually there, rendered dim with a restore marker so the user can see the boundary between restored and live output (the Uniterm scrollback-restore UX).
4. Re-spawn each pane's PTY and re-run its command in its working directory, since PTYs themselves cannot survive a process exit.
5. Re-bind agent state from the event log: a pane that was an agent under a workflow is reattached to that workflow, its role, its goal, and its last known status.
6. Restore focus.

Because agents are separate long-lived processes, an agent that was still running in its own process group during the restart can, where the platform allows, be re-adopted rather than restarted, using the worker registry (below).
Where re-adoption is not possible, the agent is relaunched with its session context, and its native session log (via the provider adapter) is used to resume continuity.

Restore is configurable: automatic on start, prompt-on-start, or manual via a command, defaulting to prompt-on-start so the user is never surprised.

## The worker registry

This is what makes agent re-adoption possible and what keeps the fleet consistent across restarts.

- Each running agent has a small registry file (pid, process-group id, the pane it belongs to, its socket path if it speaks a structured protocol, and created-at), mode 0600.
- On server start, the registry is scanned and each pid is probed for liveness (`kill(pid, 0)` or a `pidfd` open).
- Live agents are re-adopted and reconnected to their panes; dead entries are cleaned up and their panes restored from snapshot instead.

This is why closing the client, or even restarting the server, does not stop the agents: they are tracked in durable files, not only in memory.

## On-disk layout

State is split between a per-machine location and a per-project location, following the Uniterm `.uniterm/` convention where it belongs to the project.

- Per-machine (XDG state and data dirs):
  - the event-log database,
  - the snapshot files (current plus a small ring of recent ones for safety),
  - append-only Workspace catalog definitions under `workspaces/`,
  - the worker registry,
  - the control-socket token.
- Per-project (`.uniterm/` in the repo, git-ignorable):
  - workflow artifacts and verifier verdicts,
  - git checkpoints (see [07-workflows-and-relay.md](07-workflows-and-relay.md)),
  - project memory (`.uniterm/memory/memory.md`) and session mirrors.

Keeping project-scoped agentic artifacts in the repo makes them portable, transparent, and reviewable, exactly as Uniterm did.
Keeping machine-scoped multiplexer state in XDG dirs keeps the repo clean.

## Failure and safety properties

- A crash mid-write never corrupts state, because snapshots are written atomically and the log is append-only.
- A restore never silently loses a pane; a pane that cannot be restored (its command is gone, its cwd no longer exists) is surfaced with its last state rather than dropped.
- Scrollback restore is bounded (a configurable line and byte cap per pane) so a runaway history cannot blow up the snapshot.
- Continuous autosave respects the resource budget: it is dirty-triggered, content is captured only for changed panes, and the whole thing runs on the tokio side so it never adds latency to a keystroke.

## Retention summary

| State | Crash recovery from a snapshot | After an intentional stop |
|---|---|---|
| Workspace, Project, and Tab hierarchy | Restored | Recreated from the catalog |
| Split layout | Structured geometry restored | Anonymous split geometry recreated |
| Terminal content and scrollback | Restored from the snapshot | Discarded |
| Commands and agent processes | Relaunched or re-adopted where supported | Replaced with fresh shells |
| Tasks, run graph, artifacts, and audit history | Replayed from durable history | Retained and replayed |

Snapshots are dirty-triggered and written atomically on the persistence worker.
See [STATUS.md](STATUS.md) for implementation coverage and its tests.
