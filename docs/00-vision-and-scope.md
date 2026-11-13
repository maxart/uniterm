# 00 - Vision and Scope

## The one-sentence pitch

A terminal multiplexer that does not eat your machine, and that treats AI agents as first-class, long-lived, observable citizens rather than as anonymous processes scrolling text.

## Why this exists

Uniterm CLI renders directly to the terminal so that terminal management and agent supervision can share a small, event-driven runtime.
It succeeds Uniterm Desktop, whose resource diagnostics motivated the footprint budgets and architecture recorded in [01-language-decision.md](01-language-decision.md).
The renderer has no DOM, browser engine, or timer-driven relayout, and emits no frames when nothing changes.

## Who it is for

Engineers who run many terminals and, increasingly, many AI coding agents at once.

The concrete user in the diagnostic had 23 workspaces, 45 tabs, 46 panes, and 21 live PTYs, several of them running Claude, Codex, and other agents in parallel.
That person cannot watch five agents work by bouncing between five terminals.
They need one place that tells them which agent is stuck, which is waiting for a permission, which is done, and which is quietly burning tokens on the wrong thing.

They also need the classic multiplexer to be excellent, because it is the substrate they live in all day.

## The two halves, and why one binary

The product is deliberately two programs fused into one binary.

The systems half is a real terminal multiplexer: PTY management, an escape-sequence parser, a grid model with scrollback, a damage-tracked renderer, a client-server split over a Unix socket, layouts, copy-mode, and a config and command system.
This half must be fast and quiet.
Input-to-pixel latency under 50 ms, zero frames when nothing changes, and near-zero CPU when occluded.

The agentic half is an agent-fleet supervisor: it detects agent state, queues the moments that need a human (permissions, questions), orchestrates multi-step workflows and agent-to-agent relays, tracks token cost, and learns across sessions (memory).

These belong in one binary because the agentic half is only cheap and only correct if it sits directly on top of the grid the systems half already owns.
Uniterm reads the grid it already owns, combines it with provider signals, and uses native process notifications for liveness.
Observation responds to output and lifecycle events without spawning a subprocess to capture terminal content on a timer.

## What good looks like (success criteria)

These carry over from the resource diagnostic and the language ADR, and they are the acceptance bar, not aspirations.

- Background or occluded CPU: under 0.5 percent of one core over five minutes.
- Foreground idle CPU, excluding child jobs: under 3 percent over five minutes.
- Frames rendered when nothing visibly changed: zero.
- Memory on the same 21-session workload: low tens of MiB steady state, an order of magnitude below the old ~821 MiB.
- Memory after closing panes: returns within about 10 percent of baseline after settling.
- Input-to-pixel latency: under 50 ms at p95.
- Agent-exit detection: under 500 ms, with no whole-system process scans.
- Git status update after a relevant change: under 1 second, with no periodic full scans.

Two costs from the old backend must be fixed here regardless of language, and they are treated as architecture requirements, not afterthoughts:
per-pane process-table polling for agent-exit detection is replaced with native exit notification (`pidfd`, `kqueue`/`EVFILT_PROC`), and redundant git worktree scans are replaced with a repository-root-keyed, event-driven, coalesced cache.
See [06-agentic-supervision.md](06-agentic-supervision.md) and [03-system-architecture.md](03-system-architecture.md).

## In scope for v1

- A client-server terminal multiplexer with sessions that survive client detach and server restart.
- Splits, windows, tabs, layouts, copy-mode with search, and directional pane navigation.
- Built-in session save/restore and continuous autosave, including scrollback and layout (the resurrect and continuum equivalents).
- Agent status detection (working, idle, tool, permission, question, error, exited) with zero polling in the common case.
- A waiting queue: the single place a human clears permissions and answers questions across the whole fleet.
- A built-in Observatory: fleet view, waiting queue, timeline, touched-files, and memory.
- Quick task capture (the New Task surface) to launch an agent or a workflow with one prompt.
- Deterministic multi-agent workflows and relay, with explicit completion contracts, stall timeouts, verifier gates, and git checkpoints.
- Per-agent and per-goal token and cost telemetry.
- A config file, a command language, and rebindable keys.
- Linux and macOS.

## Out of scope for v1 (explicit non-goals)

- The webview and everything that depended on it: browser panes, HTML/CSS theming, modal dialogs, native window chrome.
  These are replaced by terminal-native equivalents or dropped.
- A GUI app of any kind.
  The optional web and mobile companion for remote access is a post-v1 surface exposed over the control socket, not a bundled UI.
- Windows support in v1.
  The native exit-notification path differs there (registered waits on process handles) and the ecosystem story is weaker; it is a fast-follow, not a launch blocker.
- Being a general plugin platform on day one.
  Extensibility is designed for (the provider trait, the control protocol) but a full third-party plugin runtime is deferred.

## Product direction

Uniterm combines persistent client-server sessions, agent supervision, durable history, remote access, and a provider-neutral control API in one Rust binary.
Each capability must preserve the resource budgets above.
