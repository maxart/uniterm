# 05 - Session Persistence (built-in resurrect and continuum)

One of the explicit goals is to ship the equivalents of `tmux-resurrect` and `tmux-continuum` built in by default, and to do them better than the plugins can.

This document specifies that subsystem.

## Why we can do better than the plugins

`tmux-resurrect` and `tmux-continuum` are excellent given their constraint, which is that they live outside tmux and can only talk to it through commands.

That constraint means they can capture, per pane, only the running command, the working directory, and the position in the session-window-pane tree, plus window and session names.
They cannot capture scrollback, because tmux never persists pane content.
They cannot capture true layout geometry except by round-tripping tmux's opaque layout strings.
They restore by replaying `new-session`, `new-window`, and `send-keys` commands, which is slow and fragile, and they cannot make the restore atomic.
Continuum's autosave runs on a coarse timer (about every 15 minutes) because each save shells out through the command interface.

We are inside the server and own the grid, so none of those constraints apply.
We persist scrollback and layout as first-class data, we snapshot cheaply and often, and we restore atomically.

## Three mechanisms, one subsystem

Persistence is one subsystem with three write paths, all living in the agent runtime so they never touch the render loop.

1. **The event log** (the continuum-like continuous record).
   An append-only log of everything structurally meaningful, written as it happens.
2. **Snapshots** (the resurrect-like point-in-time capture).
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

A damaged or unexpected durable state must never keep a Workspace from starting; the only fatal recovery error is a stream written by a newer schema, because a newer binary can still read it and an older one must not touch it.

- The first record of a stream establishes its origin, which may be any sequence.
  A clean stop deletes the stream while a still-live writer keeps its counter, and a future compaction keeps only the suffix after a checkpoint, so a stream that begins mid-history is contiguous, not damaged.
  Only a gap or repeat after the origin is damage, and it is repaired by truncating to the last consistent prefix with the original preserved as `NAME.log.corrupt-<nanos>`.
- Every projection (structure, tasks, waiting queue, instructions, run graph, artifact ledger, orchestrations) is replayed into scratch state before any Pane is spawned, so a rejected stream cannot leave half-restored state behind.
- A stream or snapshot that still cannot be replayed (foreign ownership, an invalid rename record, an impossible run-graph transition) is quarantined: both files are renamed to `.corrupt-<nanos>` siblings, a diagnostic names them on stderr, and the Workspace starts from its catalog definition.
- A running server holds two claims for its lifetime: a `flock` beside the socket and a POSIX record lock on `locks/NAME.lock` inside the state directory.
  The runtime directory differs between a desktop login, an SSH session, a systemd user unit, and a test harness, while the state directory does not; the second claim guarantees that no other process, including `ut workspace forget` and `ut workspace rename`, can ever operate on the durable files of a live Workspace.
- A clean stop freezes event and checkpoint writes before it queues the snapshot deletion, so nothing that drains later in the same poll batch can recreate the crash marker.
- Only the latest catalog line is ever read, so identical consecutive definitions are not appended and a server folds any longer catalog to its latest valid line, atomically, when it starts.

## The event log

An append-only log is the ground truth, and every view (the Observatory timeline, the workflow engine's recovery, the restore path) is a projection of it.

- Append-only, so concurrent writers are safe and crash recovery is a replay, not a repair.
- One logical stream per Workspace, with retention policies so it does not grow without bound.
- Entry kinds include: Project, Tab, and Pane created or destroyed; layout changed; Pane command started or exited; working directory changed; metadata changed; and the full set of agentic events.
- Storage: `rusqlite` with a bundled SQLite is the leading candidate for the log because it gives indexed queries (the timeline filters by type, goal, and agent) and safe concurrent reads for free; a custom append-only file is the fallback if SQLite proves heavier than the budget allows.
  This is validated in the persistence spike.

The event log is what makes the Observatory timeline, cost rollups, and audit trails free, and it is what lets the workflow engine reconstruct in-flight orchestration after a restart.

The custom log suppresses identical adjacent records because they contain no timestamp or distinct state information, and crash recovery streams task events instead of collecting the lifetime log in memory.
Hard truncation is intentionally deferred until every recoverable projection has an explicit checkpoint event; deleting old records before that would trade disk space for silent recovery loss.
The 100,000-event replay test exercises growth and recovery assumptions so a future retention checkpoint can be introduced from measurements rather than an arbitrary limit.
The opt-in long-duration reliability workload separately exercises connection churn and development-server detection.

## Snapshots

Snapshots capture what the event log does not replay cheaply: the actual terminal content and the exact layout at a moment.

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
Older snapshots with no Pane directory, relative values, or a nested directory that disappeared while the machine was down restore the Pane at its owning Project root instead of inheriting the server's home directory or dropping the Pane.

Clean stop is different from crash recovery: it records the lightweight Workspace definition, then removes the full snapshot and runtime event stream.

## Restore

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

## Summary of the improvement over the plugins

| Capability | tmux-resurrect + continuum | Uniterm CLI |
|---|---|---|
| Workspace, Project, Tab, Pane tree | Partial | Yes |
| Running command and cwd | Yes | Yes |
| Layout | Via opaque strings | Structured, exact |
| Scrollback content | No | Yes |
| Restore style | Replay send-keys, non-atomic | Reconstruct in memory, atomic |
| Autosave cadence | ~15 min (shell-out cost) | Seconds, dirty-triggered, in-process |
| Agent re-adoption | N/A | Yes, via the worker registry |
| Built in by default | No (two plugins) | Yes |
