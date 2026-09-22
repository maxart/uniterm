# 08 - The Observatory

The Observatory is the answer to "I have eight agents running, where do I look?"

## Implementation status

The docked Agents, File manager, and Web servers tabs, the modal fleet view, and the workspace-scoped actionable waiting queue are implemented.
Waiting items can be focused, answered, or stopped; provider prompts can be dismissed; and paused orchestration runs can be resumed or rolled back through the same semantic server path used by the CLI.
The event timeline, detailed workflow and relay projections, changed-file detail, memory, and usage or cost telemetry described below are not yet complete.

In Uniterm it was a webview panel.
Here it is a persistent terminal-native right rail rendered by the server's damage-tracked chrome, fed by the agent-state model and event log, and also queryable over the control protocol so a future web or mobile companion can present the same data.

It is the single most visible agentic feature, and it is what turns the tool from "a multiplexer that can run agents" into "a place to command a fleet."

## What it is, structurally

- Not a separate app and not a webview.
  It is a docked server-rendered rail with Agents, File manager, and Web servers tabs.
- It renders from state the server already holds: the reconciled agent status per pane, the workflow and relay engine state, the event log, and the git and file watchers.
  It adds no general polling of its own; state changes damage and redraw only the affected chrome.
- Each tab owns an independent vertical scroll position, so changing tools does not discard the user's place.
- File-system watches are armed only while the File manager tab is visible and only for expanded directories.
- Everything it shows is **workspace-scoped**.
  This is the hard constraint from Uniterm: the fleet view, the waiting queue, and the workflows all filter by the active workspace, so a bulk action can never reach across into unrelated work.

## Observatory projections

The docked shell currently exposes Agents, File manager, and Web servers as its primary operational views.
The richer waiting, timeline, changed-files, and memory projections below remain part of the design and can join the same rail without returning to a modal architecture.

### 1. Fleet (the overview)

The primary dashboard: one row per active agent, plus the active goals and any running workflows.

Per-agent row: agent name and glyph, current task label, project (name, color, icon), token and cost telemetry with an approximate marker when estimated, and the last-event timestamp.

Rows are ordered by a status priority that puts the things needing a human first: permission and question, then error, then tool, then working, then idle, with recency breaking ties.
This ordering is deliberate: the top of the list is always "what needs you or what is stuck."
Rows are sortable by status, project, agent, or recency, and can be grouped.
The compact docked Agents rail uses a calmer projection: Project order first, then agent start time within each Project.
Its cards therefore keep their positions when live status changes, while the richer fleet and waiting projections retain attention-first ordering.

Ad-hoc agents (a pane running an agent that is not part of any goal or workflow) appear here too, so nothing is invisible; agents already accounted for inside a goal or workflow are not double-counted.

Alongside the agent rows: the active goals list (title, status badge, workflow name, current role) and, if any workflows are running, a compact flow visualization of their roles, gates, and last verdict, with a send-back affordance where allowed.

A token rollup aggregates cost across the workspace's active goals.
The dev-server monitor has its own Web servers tab beside the live Agents surface.
It recognizes loopback and explicit `localdev` host announce lines from Vite, Next.js, Rails, Django, Flask, Uvicorn, Gunicorn, Phoenix, Laravel, Hugo, Jekyll, Eleventy, Parcel, Bun, portless, Python `http.server`, webpack-dev-server, and generic HTTP servers.
Each row retains its Project, Tab, and Pane location, opens its URL in the user's desktop browser, and can focus the Pane that announced it.
Like Agents, Web servers defaults to the active Project and has a Project/Workspace scope control; Workspace scope includes the owning Project on every row.
Detection consumes only event-driven PTY screen-tail evidence.
The tokio runtime confirms each detected port on IPv4 and IPv6 loopback before listing it, checks it every five seconds while it remains tracked, requires three explicit connection refusals before removing it, and disarms the probe when the server or Pane goes away.
Timeouts, interruptions, unavailable address families, and runtime task failures are inconclusive, so a transient host-resume or resource error cannot remove an otherwise quiet server permanently.
No probe exists when no server has been detected.

### 2. Waiting queue (the supervision surface)

The most important view.
One list of everything, in this workspace, that needs a human, built on demand from the agent-state model and the engines:

- **Permission requests**: the action, sandbox mode, and approval mode, with approve and deny.
- **Questions**: the agent's question text, with an answer action that injects the response back into the pane.
- **Paused workflows and relays**: the pause or escalation reason, with resume, skip-role, roll-back, and stop.
- **Close-tab prompts**: a pane with running work the user tried to close, with confirm-close or keep.

The Fleet view shows a waiting badge (a count with a red marker), and when the count changes the Observatory can auto-focus the waiting queue, because clearing it is usually the highest-priority action.
Every item resolves to a focus target (Pane, then Tab, then Project, then Workspace) so the human can jump straight to the context.

This is where human-and-fleet coordination happens.
The design intent, straight from Uniterm's lesson, is that there is exactly one queue, it is scoped, and it replaces scattered modal prompts.

### 3. Timeline

A chronological, filterable event log for the workspace: goals created, workflows started, agents launched, prompts injected, tools started and finished, permissions and questions asked and answered, files changed, verifier verdicts, human approvals and send-backs, goals completed or abandoned.

It is a direct projection of the event log (see [05-session-persistence.md](05-session-persistence.md)), filterable by event type, goal, and agent, and it renders artifacts and evidence (verdict findings, test results, diffs) inline where available.
Because the log is durable, the timeline survives restarts and is the audit trail.

### 4. Files

The recently touched files across the workspace, from the git and filesystem watchers: a live tree of changed files with per-file insertions and deletions, updated on git-change events (not a timer), with reveal-in-file-manager and open-in-configured-editor actions.

This is the "what did the agents actually change" view, and it reuses the coalesced, repo-root-keyed git cache from [03-system-architecture.md](03-system-architecture.md), so it costs nothing when nothing changed.

### 5. Memory

The learning surface, ported from Uniterm:

- The project memory file (`.uniterm/memory/memory.md`) rendered read-only, with a start-file action if absent.
- Pending memory proposals from agents (via OSC 777 `memory_proposal` events): an agent-authored title, body, and rationale, with accept (promote into the memory file) and dismiss.
  Agents never write memory directly; a human curates.
- Recent session mirrors (from the provider session-sync adapters), labeled by agent and time.
- Curate and dream actions: curate synthesizes proposals from the most recent sessions, dream synthesizes across many sessions, both running locally and writing candidate files under `.uniterm/memory/` for review.

## Interaction model

- Fully keyboard-driven: select a row and act on it, with a consistent set of verbs (focus, approve, deny, answer, resume, skip, roll back, stop).
- Selecting an agent row focuses its pane directly, so the Observatory is a launchpad into the terminal, not a walled garden.
- Selecting a web-server row opens its HTTP(S) URL in the local desktop browser; `p` focuses its source Pane instead.
- Visible HTTP(S) URLs in ordinary Panes also open on a plain click-release when the child application has not claimed mouse input.
- OSC 8 hyperlink labels are retained in the grid and forwarded to the outer terminal, so both labeled and visibly printed HTTP(S) links keep their targets.
- Observatory is docked on the right; `prefix + o` toggles the rail without changing its selected tab, while `prefix + f` reveals and selects File manager.
- Its top tabs switch tools without obscuring the active Pane, and the coloured `Manage...` footer button opens agent actions upward.
- Fuzzy pickers (`nucleo-matcher`) for choosing among agents, projects, and workflows.
- The waiting queue can be surfaced without opening the full Observatory, for example as a status-line badge and a single keybind that jumps to the next item that needs you.

## Over the wire

Everything the Observatory renders is available over the control protocol as structured queries and subscriptions.
This is deliberate and is what makes a remote surface a later addition rather than a rewrite: a web or mobile client speaks the same control protocol, subscribes to the same agent-state and event-log streams, and renders the same fleet, waiting queue, and timeline.
v1 ships the terminal Observatory; the protocol keeps the remote door open without bundling a web server into the binary.

## Performance posture

The Observatory obeys the same budgets as everything else.
It is part of the server's damage-tracked chrome and repaints on state-change notifications and user input, not on a timer.
When it is hidden the UI costs nothing, and when it is visible and idle it renders zero frames because nothing is damaged.
File watches are disarmed outside the visible File manager tab.
An armed dev-server liveness probe is the narrow exception: it exists only for ports announced by real PTY output and ends after the port goes down.
Detection never scans the process table or every Pane on a timer; it reads the bounded screen tail already produced by a PTY-output event.
