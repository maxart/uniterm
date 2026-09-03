# 06 - Agentic Supervision

This is the foundational agentic layer: how the tool knows what every agent is doing, how agents are represented, launched, and reaped, and how new agent types plug in.

Everything above it (workflows, relay, the Observatory) is a consumer of this layer.
If this layer is right, the rest is legible; if it is wrong, the rest is guesswork.

One term is used throughout this and the following documents and is worth defining up front.
A **goal** (also called a task) is the durable, first-class unit of agentic work: a title, a description, optional success criteria, a workspace and project, a status, the panes and agents assigned to it, its artifacts, and its own event log.
Goals persist across restarts on the event log, and a workflow or relay run is how a goal gets executed.
The full data model for goals is in [09-uniterm-feature-port.md](09-uniterm-feature-port.md) (tier 1, item 4); here it is enough to know a goal is the thing an agent is working toward.

## The agent-state model

Every agent-bearing pane has an agent state, held in `uniterm-core` and updated by the agent runtime.

The status enum, the set that every agent supervisor converges on:

- `unknown`: not yet observed.
- `starting`: session opening.
- `working`: actively producing output or thinking.
- `tool`: running a tool call.
- `permission`: blocked, waiting for a human to approve or deny an action.
- `question`: blocked, waiting for a human to answer.
- `idle`: turn finished, waiting for the next prompt.
- `error`: failed.
- `exited`: the process ended.

`permission` and `question` are the two states that mean "a human is the bottleneck," and they are what the waiting queue is built from.
The distinction between `idle` (done, healthy, waiting for you) and `permission`/`question` (blocked, needs you now) is the single most important signal the product surfaces.

Alongside the status, each agent pane carries: the agent id, the current task label, token counts and estimated cost with a source flag (reported vs estimated), the last tool name, the last-working-at timestamp, and any pending question or permission detail.

## The detection stack: primary, then fallbacks

We combine every signal the four projects use, in priority order, because they have different reliability and cost profiles and they reinforce each other.

### 1. OSC 777, the primary and cooperative channel

The best signal is the agent telling us directly.
Uniterm's OSC 777 protocol is the model: the agent prints a structured escape-sequence envelope carrying lifecycle events and metadata, which our VT parser already sees and extracts for free.

The envelope carries events (session start, prompt submit, tool start and end, permission request, permission reply, question, idle, error, session end) plus metadata (protocol version, session id, cwd, project, goal id, workflow id, run id, role, current task, prompt/completion/total tokens, estimated cost, telemetry source, sandbox mode, approval mode, artifacts, and memory proposals).

Why it is primary:

- It is unambiguous.
  There is no heuristic guessing whether a spinner means working; the agent says so.
- It is nearly free.
  The bytes already pass through the core VT parser; recognizing the envelope and routing it costs a branch, not a subprocess.
- It carries data no heuristic can recover: token counts, cost, the exact tool name, the exact permission being requested, memory proposals.

Enabling it is a per-agent concern.
Agents that support a notify hook (Claude Code, OpenCode, Codex, Gemini, Kiro, Grok, Cursor Agent, and Pi in Uniterm's implementation) emit these events when launched through us; the provider adapter knows how to turn the hook on.
Cursor does not expose a dedicated permission-request hook, so that state uses its grid fallback while the connector supplies the rest of its lifecycle.
Pi's extension uses `agent_settled` for idle, after automatic retries, compaction, and queued follow-ups have drained, and uses its project-trust screen as a permission fallback.
This is the normal graceful-degradation path for any state an agent cannot announce over OSC 777.

The emission mechanism is the load-bearing detail that makes "nearly free" true, so it is stated explicitly.
The envelope must land in the pane's PTY output stream, because that is the byte stream our VT parser already reads; it is not a side channel or a socket.
Concretely, the agent's notify hook writes the escape sequence to its controlling terminal (`/dev/tty`), the kernel delivers those bytes on the PTY master exactly like any other output, and our parser recognizes the OSC 777 envelope and routes it to the supervisor instead of drawing it.
No extra file descriptor, no subprocess, and no polling are involved; the cost is one branch in a parser the bytes were already flowing through.

The envelope is an OSC sequence with a `777` code, a `notify` selector, our URI scheme as the source, and a JSON payload, terminated by BEL:

```
ESC ] 777 ; notify ; uniterm://cli-agent ; {"v":1,"event":"permission_request","session_id":"...","role":"builder","tool":"Bash","approval_mode":"ask"} BEL
```

The provider adapter's job at spawn time is to install the hook that writes sequences of this shape to `/dev/tty` on each lifecycle transition.
The full payload schema is frozen and published as a small versioned spec (see [12-open-questions.md](12-open-questions.md) Q6), so third-party agents can target a stable contract; the `v` field carries the protocol version.

### 2. Log-tail detection, the second fallback

When an agent does not emit OSC 777 but does write a structured session log, we tail it.
Reading the last few kilobytes of a JSONL transcript and matching the last meaningful entry gives status cheaply and offline:

- `stop_reason: end_turn` on the last assistant message means `idle` (done, awaiting input).
- `stop_reason: tool_use` means `permission` (blocked on a tool approval).
- a queued or enqueue marker means `working` (processing the next prompt).
- a system error means `error`.

This is O(1) in the log size (tail read), needs no process introspection, and works for any agent whose log format an adapter understands.

### 3. Grid-pattern heuristics, the last fallback, made cheap

When there is neither OSC 777 nor a parseable log, we fall back to pattern-matching the agent's output, with a structural advantage over external supervisors: they scrape `tmux capture-pane` on a timer, whereas we already hold the grid, so we match against cells we own with no subprocess and no polling.

The fallback receives an independent plain-text snapshot of the live bottom rows.
It does not depend on copy-mode position, scrollback position, or how a client has scrolled its own display.
Provider-owned manifests contain executable aliases, declared capabilities, versioned screen and log rules, confidence, and dwell hints.
`$XDG_CONFIG_HOME/uniterm/providers.json` overrides verified cache and bundled definitions without branching on an id in the reconciler.
Each manifest may declare a `log_path`; the runtime reads only the final 64 KiB after pane activity, never from the mio loop and never on an idle timer.

```json
{
  "schema_version": 1,
  "manifest_version": "my-team-1",
  "providers": [{
    "id": "my-agent",
    "executable_aliases": ["my-agent"],
    "capabilities": ["process", "screen"],
    "rules": [{
      "id": "screen.permission",
      "evidence": "screen",
      "status": "permission",
      "pattern": "approval required",
      "confidence": 95,
      "dwell_ms": 5000
    }]
  }]
}
```

See [22-provider-detection-manifests.md](22-provider-detection-manifests.md) for precedence, cache verification, reload, validation, and explanation details.

The heuristics are a registry of per-agent detectors, learned from real agent behavior, evaluated by one provider-neutral matcher:

- Every rule names a region (`bottom`, the live bottom rows, or `title`, the OSC 0/2 window title), an anchor (`anywhere`, `line_start`, or `spinner_line`), and a priority; the highest matching priority wins.
- `working` and `tool` rules are anchored: a spinner glyph (braille, quarter or half circles, hexagons, or the star set) or a line-start phrase must open the line, so text the user types into the indented input box can never impersonate an activity line.
- Most agent TUIs animate their spinner and blocked markers in the window title, so title rules carry the highest priorities.
- An approval-prompt shape indicates `permission` and outranks a running signal, because the spinner can still be on screen beneath the prompt.
- When no rule matches, a known agent is `idle`. Raw output volume is never evidence: keyboard echo, a repainting footer, and a resize are all output, and treating bytes as `working` is what made idle agents look busy while the user typed.
- Because `idle` rests on a positive or default verdict rather than on silence, its dwell only has to outlast one redraw flicker (600 ms), while `permission` keeps its 5 s dwell.
- A cooperative `working` whose idle hook never fires may be replaced by a persistent screen `idle` after 30 s, and a cooperative `session_start` (`starting`) yields to the first real verdict from any source.

Detectors run only for panes under active detection (an agent pane without a better signal), and only when that pane's grid actually changed, so they never become a background poll.
Foreground process identity is sampled only after PTY output and cached until the process group changes.

### 4. Process liveness and exit, the ground truth for "is it even running"

Two OS-level signals underpin the others.

- **Foreground process group** (Uniterm's `foreground_pid`): `tcgetpgrp` on the pane's PTY master tells us whether the foreground group is the agent or the shell.
  This cheaply distinguishes "agent executing" from "shell sitting at a prompt," and it is used to sanity-check the higher-level status and to decide whether closing a pane needs a confirmation.
- **Native exit notification**: the moment a child exits we learn it from the kernel, via `pidfd` on Linux or `kqueue` with `EVFILT_PROC`/`NOTE_EXIT` on macOS, registered once per tracked pid.
  This replaces the old per-pane `sysinfo` scan entirely and satisfies the sub-500 ms exit-detection budget with no whole-system scan.

### Reconciliation

The signals are combined, not used in isolation, because in practice they disagree.

- A cooperative OSC 777 event wins, except that a grid-detected permission prompt can downgrade a stale `working` to `permission` (the agent's hook fired "running" but a prompt is on screen).
- Dwell thresholds smooth flicker: a status change does not fire a notification until it holds (about 5 s for waiting states, about 2 s for terminal states), because agents flicker between waiting and running for around 100 ms during prompts.
- A pane whose process has exited overrides any other status with `exited`, and a workflow waiting on a dead pane escalates rather than hanging.
- `ut agent explain [pane-id]` reports the provider, reconciled status, authority, foreground pid, and exact evidence that won.

## The provider trait

Agent types are pluggable behind one trait, and adding an agent is one module, with no agent-specific branching in the core.

The trait (shape, not final signature):

- `id` and display metadata (name, color).
- `discover(snapshot)`: given a shared process-and-session snapshot, find running instances of this agent that were not launched by us.
- `spawn(spec)`: how to launch this agent (command, flags, how to enable its notify hook or tracing).
- `can_embed`: whether it can run directly in our PTY (all of ours can, since we own the terminal; this exists for parity and future remote cases).
- `status_from_grid(cells)` and `status_from_log(tail)`: the fallback detectors for this agent.
- `parse_session(path)`: read this agent's native session log and render clean role-tagged Markdown (the session-sync adapter).
- `pricing(model)`: model-aware token pricing for cost estimation.

Providers we ship, following Uniterm's registry: Claude Code, Codex, OpenCode, Gemini, Grok, Kiro, Cursor Agent, and Pi, with room for more.
Discovery uses a single shared `ps`-and-session snapshot handed to every provider, never one scan per provider.

The core never says `if agent == "claude"`.
It calls the trait.
This is what keeps the tool current as the agent ecosystem churns.

## Launching and steering agents

- **Launch**: an agent is launched into a pane through its provider's `spawn`, which sets up the notify hook so OSC 777 flows from the start.
  Launch can be ad hoc (open a pane, run an agent), from quick task capture (see [10-config-commands-keybindings.md](10-config-commands-keybindings.md)), or as a role inside a workflow.
- **Steer**: guidance is injected into a pane as a bracketed paste gated on the agent being ready, following Uniterm's injection model (bracketed paste, an OSC 777 readiness gate, and a submit tail), with delivery retry on failure.
  Steering is how a workflow hands a role its prompt and how a human nudges a running agent.
  Human follow-ups can be stored in the event-backed instruction queue and delivered one at a time by a cooperative `idle` event, or bypassed by an explicit send-now command.
  Heuristic idle evidence never injects queued direction.
- **Answer and approve**: resolving a `question` or `permission` from the waiting queue injects the answer or the approve/deny back into the pane (or, for structured agents, over their protocol).
- **Reap**: stopping an agent signals its whole process group with SIGTERM, waits a short grace, then SIGKILL, so no descendants are orphaned.

## Discovery of external agents

Because of the provider `discover` path, the tool can adopt agents started outside it: a Claude running in a plain shell we did not spawn is still discovered, its state detected, and it appears in the fleet view.
This matters because people do not start every agent through the tool, and a fleet view that only shows agents we launched would be lying about the fleet.

## What this layer guarantees to the layers above

- A single, reconciled status per agent pane, updated in near real time, with no polling in the common case.
- A durable event stream (into the event log) of every agent transition, so history and recovery are free.
- A uniform representation regardless of agent type, so workflows, the waiting queue, and the Observatory are written once against the model, not per agent.
- Cost and token telemetry per agent.
- Exit and liveness that meet the resource budgets by construction.
