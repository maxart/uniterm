# 03 - System Architecture

This document describes the overall shape of the binary: the process model, the two-runtime split, the component map, and how data flows from a keystroke to a pixel and from an agent event to a notification.

Detailed subsystem design lives in the documents that follow; this one is the map they hang off.

## Process model

The design is client-server, like tmux, for the same reasons and with the same payoff.

- A single long-lived **server** process owns everything durable: PTYs, grids, scrollback, sessions, windows, panes, agent state, the workflow and relay engines, and the event log.
- **Clients** are thin and ephemeral.
  A client attaches over a Unix domain socket, streams input to the server, and renders the frames the server sends it.
  When a client detaches or its terminal closes, the server and all sessions keep running.
- The server auto-starts on first client attach and can be told to stay resident so that agents keep working with no client attached at all.

This is the single most important structural decision, because it is simultaneously the multiplexer's persistence model and the agent fleet's "keep working while I close my laptop" model.
There is no separate daemon for agents; the multiplexer server is the daemon.

```
  terminal A ──┐
               │  Unix socket (framed, typed messages + fd passing)
  terminal B ──┼────────────►  uniterm server (one process)
               │                 owns: PTYs, grids, sessions, event log,
  control API ─┘                 agent supervisor, workflow/relay engine
  (web/mobile,
   scripts, agents)
```

## The two-runtime split (Decision R1)

Inside the server there are two clearly separated execution domains.
They never share the hot path and they communicate only by message passing.

### The core loop (systems half)

A single-threaded, `mio`-based event loop owns everything on the keystroke-to-pixel path.

Its responsibilities:

- Read the server socket: client input, resize, attach and detach, control commands.
- Read every PTY master fd into per-pane input buffers.
- Feed bytes to the VT parser, which mutates the per-pane grid and marks damage.
- Coalesce output so that a burst of PTY data produces at most one repaint per display frame.
- Render damaged regions of visible panes to attached clients, and nothing for occluded or inactive panes.

This loop has no async executor, no work-stealing scheduler, and no locks on the render path.
It is the tmux-shaped core, in Rust.
Everything it does is bounded and deterministic, which is what the sub-50 ms latency and zero-idle-frame budgets require.

### The agent runtime (agentic half)

A separate `tokio` runtime, on its own thread pool, owns everything that is naturally async, potentially slow, or network-bound.

Its responsibilities:

- The agent supervisor: process spawning, native exit notification (`pidfd`, `kqueue`), and the worker registry.
- The provider adapters: discovery, session-log tailing, and status heuristics.
- The workflow and relay engines and their timers (stall timeouts, retry backoff).
- HTTP and LLM API calls, TLS, and JSON.
- File watching (`notify`) for git and touched-files, debounced and repo-root-keyed.
- The control-protocol server that exposes state to scripts, agents, and the future web and mobile surfaces.
- Persistence: writing the event log and periodic snapshots.

### The boundary

The two halves exchange small, typed messages over channels.

- Core to agent runtime: "pane 7 emitted this OSC 777 event," "pane 7's PTY hit EOF," "the visible grid of pane 7 now looks like this" (only when a heuristic needs it, and only for panes under active detection).
- Agent runtime to core: "inject this text into pane 7 as a bracketed paste," "spawn a pane running this command in this layout," "pane 7's status is now `permission`, redraw its status badge," "show this waiting-queue item."

Because OSC 777 parsing happens in the core's VT parser (the bytes are already flowing through it), the common-case agent-state signal costs essentially nothing extra.
The agent runtime is woken by an event, not by a poll.
This is the concrete mechanism behind "observation is a read of state we already hold."

The core-to-runtime transport has one channel slot and a core-owned outbox capped at 512 messages and a 128 MiB payload budget.
Checkpoint accounting uses compact capture allocation capacities and conservative metadata overhead rather than serializing terminal content on the core loop.
Queued Pane evidence, development-server evidence, catalogs, and checkpoints are replaced by their latest values without moving a newer checkpoint ahead of its event records or across a lifecycle operation.
At half the outbox limit the core defers readable PTYs and client sockets until the runtime consumes queued work and wakes it; socket writes and runtime replies remain serviceable.
A single dispatch that exhausts the remaining reserve fails explicitly and preserves the accepted event prefix and crash checkpoint instead of silently dropping a record and publishing an inconsistent checkpoint.
The reply channel holds at most 128 messages and applies backpressure to runtime producers.
Shutdown drains replies while flushing the outbox so bounded channels cannot deadlock the final persistence writes.

Checkpoint DTOs live in `uniterm-proto::checkpoint`.
The core captures owned compact cells and arena text, then `runtime::persistence::Persistence` serializes them directly into the existing durable schema on a blocking worker.
No live grid, lock, or shared mutable state crosses that boundary.
Capturing still copies retained cells synchronously; `checkpoint_spike` measures that remaining cost at 20 and 45 Panes and enforces a 50 ms p95 capture budget.
The persistence service owns append-failure poisoning and acknowledged catalog state, keeping their ordering rules separate from control, provider, and filesystem work.
Project filesystem operations, artifact validation, and event-driven watches live in `runtime::files`, with their ownership checks and bounds alongside them.

Closing a Pane immediately removes its UI ownership and transfers its PTY to the core's small teardown state machine.
It retains the shell pid through a 15 ms TERM grace, sends KILL, drains remaining terminal output in bounded batches, and reaps on native exit readiness.
These deadlines exist only while teardown work is pending and disappear when it drains.

## Component map

```
                          ┌──────────────────────────── uniterm server ───────────────────────────┐
                          │                                                                        │
  clients ◄──socket──►    │  ┌── core loop (mio, single thread) ──┐   ┌── agent runtime (tokio) ─┐ │
  (tui render + input)    │  │  socket I/O                        │   │  supervisor (spawn/exit) │ │
                          │  │  pty readers ─► vt parser ─► grid  │   │  provider adapters       │ │
  control API ◄──socket─► │  │        │                  (damage) │◄─►│  workflow engine         │ │
  (web, mobile, scripts,  │  │  renderer (dirty-cell diff)        │ ch│  relay engine            │ │
   agents' cli calls)     │  │  layout / windows / sessions       │   │  control server          │ │
                          │  └────────────────────────────────────┘   │  git watcher (coalesced) │ │
                          │                                            │  file watcher            │ │
                          │  ┌── persistence ──────────────────────────┴──────────────────────┐  │ │
                          │  │  append-only event log  +  periodic binary snapshots            │  │ │
                          │  └─────────────────────────────────────────────────────────────────┘ │ │
                          └────────────────────────────────────────────────────────────────────────┘
```

## Crate layout

Enforcing the discipline of a core that cannot import a UI type as an actual crate boundary, not a convention.

- `uniterm-core`: sessions, windows, panes, grid, VT parsing, layout, the pure decision brains for workflows and relay, the agent-state model.
  No rendering, no async, no I/O beyond traits.
  This is the crate with the exhaustive unit tests.
- `uniterm-server`: the mio core loop, the PTY layer, the renderer, the tokio agent runtime, the supervisor, the persistence layer, and the control server.
  Depends on `uniterm-core`.
- `uniterm-client`: the thin attach client and low-frequency dialog surfaces (this is where `ratatui` lives).
  Talks to the server over the socket.
- `uniterm-cli`: the `uniterm` and `ut` binary front door, argument parsing, and the subcommands agents call (`uniterm workflow submit`, `uniterm relay submit`, `uniterm attach`, and so on).
- `uniterm-proto`: the wire types for both the client render protocol and the control protocol, shared by all of the above.

The renderer choice from Decision R2 (damage-tracked custom grid, not immediate-mode) lives in `uniterm-server`.
`ratatui` is confined to `uniterm-client` and only for dialog and management surfaces, never the Pane grid or persistent Observatory.

## Data-flow walkthroughs

### A keystroke

1. The client reads a key, frames it, and writes it to the socket.
2. The core loop wakes on the socket fd, decodes the input message, and writes the bytes to the focused pane's PTY master.
3. The PTY child processes it and writes output.
4. The core loop wakes on the PTY fd, reads the bytes, feeds them to the VT parser, which mutates the grid and marks the changed cells as damaged.
5. At the next frame boundary the renderer diffs the damaged cells and sends only those escape sequences to every client currently viewing that pane.

No async runtime, no allocation storm, no full-frame redraw.
This is the path the 50 ms p95 budget protects.

### An agent going idle

1. The agent finishes a turn and prints its OSC 777 idle event.
2. Those bytes arrive on the PTY fd and flow through the same VT parser, which recognizes the OSC 777 envelope and extracts the structured event instead of drawing it.
3. The core loop hands the event to the agent runtime over the channel.
4. The supervisor updates that pane's agent state to `idle`, stamps `last_working_at`, and appends the event to the log.
5. If a workflow owns that pane, the workflow engine's decision brain runs and may inject the next role's prompt, advance a gate, or escalate.
6. The status badge for that pane is marked damaged so the next frame reflects the new state, and if a dwell threshold is crossed the notifier fires.

The only work that happened on the core loop was recognizing an escape sequence it was already parsing.

### An agent exiting

1. The child process exits.
2. On Linux the `pidfd` becomes readable; on macOS the `kqueue` `EVFILT_PROC`/`NOTE_EXIT` fires.
   Either way the agent runtime is notified by the kernel, with no scan.
3. The supervisor reaps the process group, updates state, and appends an exit event.
4. If a workflow was waiting on that pane, the engine treats a dead pane as an error and escalates rather than hanging.

There is no `sysinfo` refresh and no per-pane poll anywhere in this path.

## How the carryover costs are designed out

The two language-independent costs from the diagnostic are addressed structurally here, not patched later.

**Process-exit detection.**
The only mechanism is native kernel notification (`pidfd` on Linux, `kqueue` `NOTE_EXIT` on macOS), registered once per tracked agent pid in the agent runtime.
There is no periodic process-table scan.
If a future platform forces a fallback, the rule is one shared snapshot per interval across all watchers, backoff for inactive panes, and never per-pane.

**Git status.**
A single git watcher in the agent runtime canonicalizes each project path to its repository root, keys a cache by that root, coalesces concurrent requests, and invalidates on filesystem and git-metadata events with a short debounce.
Cheap dirty-or-clean state is computed for visible badges; expensive diff stats are deferred until a view that needs them is visible.
There is no per-project or per-pane periodic scan.

## Concurrency and safety notes

- Cross-process safety (two `uniterm` invocations, or an agent's CLI call racing the server) uses advisory `flock` on sidecar lock files with a short polling backoff rather than a database for simple serialization.
- The event log is append-only, which sidesteps most concurrent-writer hazards and makes crash recovery a replay rather than a repair.
- Secrets (any provider env, control-socket tokens) are written with mode 0600.
- The core loop owns all grid state exclusively; the agent runtime never touches a grid directly, only through messages, which keeps the render path lock-free.

## Where the surfaces plug in

- The **attach client** and the **Observatory** are both `uniterm-client` render surfaces over the socket.
  The Observatory is not a separate app; it is a client view (a special window) fed by the same state.
- The **control protocol** is how everything non-human reaches the server: an agent's `uniterm workflow submit`, a script querying fleet status, and the future web and mobile companion.
  It is a superset of tmux control mode; see [04-multiplexer-core.md](04-multiplexer-core.md).
- **Remote terminal attach** uses the normal client protocol through an SSH stdio bridge, with no public listener or second server implementation.
  See [14-ssh-remote-sessions.md](14-ssh-remote-sessions.md).
- A future web or mobile companion remains a separate post-v1 control-protocol surface.
