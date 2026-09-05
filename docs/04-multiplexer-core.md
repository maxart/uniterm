# 04 - Multiplexer Core

This is the systems half: the tmux-class multiplexer.
It must be complete enough that a tmux user feels at home, and fast enough to hit the resource budgets.

Everything here lives in `uniterm-core` (the model) and `uniterm-server` (the loop, PTY, and renderer).

## Object model

The Pane and Tab mechanics follow tmux, while the ownership hierarchy matches Uniterm Desktop.

```
Server process         one durable Workspace, addressed by its socket
 └─ Workspace          named, persistent, the safety scope for agentic actions
     └─ Project        named repository/root with stable identity and metadata
         └─ Tab        one screenful and one structured Pane layout tree
             └─ Pane   one PTY, grid, process state, agent state, and metadata
```

- **Server process / Workspace**: the durable top-level context, socket, event log, Project catalog, and agent supervisor.
  The historical `session` CLI spelling remains a compatibility alias, but product UI calls this a Workspace.
- **Project**: a stable id, name, root, metadata, and an ordered set of Tabs.
  Agent queries and bulk actions can scope to a Project without relying on mutable Tab indices.
- **Tab**: one screenful containing a layout tree of Panes.
  The implementation retains the internal `window` vocabulary where it helps tmux compatibility, but user-facing surfaces call these Tabs.
- **Pane**: the atom.
  One PTY, one grid with scrollback, the child process state, and (when a pane is running an agent) its agent state.

This hierarchy is **Workspace > Project > Tab > Pane**.
Every Workspace starts with a default Project derived from its launch directory, so a pure-multiplexer user can ignore Projects until they are useful.
The responsive left sidebar contains Projects only, uses two-row cards separated by one blank non-clickable row, scrolls vertically, and collapses automatically on narrow terminals.
Right-clicking a Project card opens actions for that stable Project id, while right-clicking the heading, a separator, or other empty rail space opens Project creation and management actions.
The active Project receives a theme-backed selection background in addition to its marker, so the current context remains unmistakable at a glance.
A theme-coloured Workspace button labels the active Workspace on the left, keeps an ASCII `v` dropdown marker on the right, and anchors its management menu at the top of the rail, with the rail divider continued through the status row beside it.
The right sidebar is the persistent Observatory, with vertically scrollable Agents, File manager, and Web servers tabs.
Its divider also continues through the status row, separating the center Tab bar from the Observatory tabs.
Every rail view uses the same blank top row, heading row, and blank heading-to-content row as the Projects rail.
The Agents view defaults to the active Project and can show every agent in the Workspace, with each Workspace-scope card naming its owning Project.
Agent cards stay grouped in Project order and remain in start-time order within each Project, so status changes do not make the rail jump.
Outside active selections, rail foregrounds and backgrounds follow the host terminal, while exact provider brand colours identify agents and the configured theme remains limited to Uniterm chrome on the main canvas.
The center status bar is a horizontally scrollable Tab viewport with an always-visible new-Tab button and overflow controls when they are needed.
Persistent buttons use each theme's muted secondary accent, while the active center Tab and active Project keep the full-strength accent.
It never changes child application colors or geometry to communicate focus or agent presence.

## The grid and scrollback

The grid is the memory model that made 46 panes cost 1.23 MiB of scrollback, and we reproduce its properties.

- A grid is a 2D array of cells plus a scrollback history.
- Visible rows and retained scrollback rows use compact boxed cell arrays with a bounded line ring.
- A cell is small and `Copy`: an ordinary codepoint stays inline, while a multi-codepoint grapheme uses a handle into a compacting per-grid arena.
  Double-width graphemes own an explicit continuation cell, so every erase, insert, delete, copy, persistence, and rendering operation can repair both halves exactly.
- Scrollback is a ring buffer with a configurable line limit.
  Lines above the visible region are history; the visible region is a window into the grid.
- Each pane keeps a primary and an alternate screen (for full-screen programs), following the standard model.
- Line flags record wrapping (a soft newline for reflow), extension, and scrolled-off state.

Because we own this structure in plain Rust memory, two things follow that tmux and the external observers cannot do.
We can serialize a pane's grid and scrollback to disk for real persistence (see [05-session-persistence.md](05-session-persistence.md)), and we can run agent-state heuristics against the actual cells with no `capture-pane` subprocess.

## The VT parser

Escape-sequence parsing turns PTY bytes into grid mutations.

- The parser is driven from the core loop as bytes arrive; it is a state machine over CSI, OSC, and DCS sequences.
- We validate `vte` (Alacritty's parser) as the base state machine and layer our cell model and damage marking on top.
- The parser also recognizes the semantic and agentic escape sequences we care about and routes them instead of drawing them:
  - OSC 7 (working directory) updates the pane's cwd.
  - OSC 133 (semantic prompt zones) marks command boundaries, which powers copy-mode command navigation and "jump to last prompt."
  - OSC 52 (clipboard) integrates with the system clipboard, including over SSH.
  - OSC 777 (agent metadata) is extracted as a structured event and handed to the agent runtime; this is the primary agent-state channel and it is nearly free because the bytes already flow through here.

Every mutation marks the affected cells as damaged.
Nothing about "the screen changed" is inferred on a timer; it is known exactly, at the moment it happens.

## The renderer (Decision R2)

The renderer is the mechanism that delivers the zero-idle-frame budget.
It is an explicit dirty-cell diffing renderer over the grid, in the spirit of tmux's `screen-write.c` and `tty.c`, not an immediate-mode full-frame TUI.

A note on "frame," since a program writing escape sequences to a host terminal has no vsync and owns no framebuffer.
The real budget is not literally frames; it is zero bytes written and zero wakeups when nothing visible changed.
The "frame boundary" below is a coalescing tick that is **damage-gated**: it is armed only when there is pending damage on a visible pane, and it disarms itself when damage drains.
There is no free-running timer waking the loop to ask whether it should redraw, because a free-running idle timer is exactly the webview anti-pattern this project exists to escape.
When idle, the loop blocks in `mio` waiting for input or PTY data, and nothing renders because nothing is armed.

The pipeline:

1. Panes accumulate **damage** as the parser mutates their grids.
   Inactive and occluded panes accumulate damage in their model and draw nothing.
2. Output is **coalesced per PTY readiness drain**.
   A burst is parsed up to a fairness budget and its combined damage is painted once before the loop returns to other ready sources.
3. On repaint, for each visible pane, changes are **collected per line and merged** across adjacent cells, then emitted as the minimal escape sequences.
4. **Cursor movement is minimized** by caching the last cursor position and emitting the shortest move.
5. **Region operations** are used where they win: erase-line and erase-display for clears, and scroll sequences instead of rewriting every line when content scrolls.
6. A **cached last-cell** avoids redundant SGR (color and attribute) sequences when consecutive cells share styling.
   Extended underline shapes and SGR 58 colours are part of that cached style, so modern TUIs retain curly, dotted, dashed, and coloured diagnostics when rendered through Uniterm.
7. If nothing visible changed, **no bytes are written and no frame is produced**.
   This is the literal implementation of the budget.

Multiple clients viewing the same Workspace each keep independent cursor and style caches.
The smallest attached viewport defines the shared canvas, so no attached client receives a layout larger than its terminal.

`ratatui` is deliberately not used here.
It is immediate-mode and redraw-oriented, which is the opposite of what this path needs.
`ratatui` is used only in `uniterm-client` for dialogs and other low-frequency modal surfaces where immediate-mode is fine.
The docked Observatory uses the same damage-tracked server renderer as the rest of the persistent chrome.

## PTY layer

- Each pane owns one PTY.
  On Unix we validate `portable-pty` (from WezTerm) or direct `nix`.
- PTY masters are read non-blocking from the core loop's `mio` registration; there are no per-pane blocking reader threads competing on the hot path.
  The diagnostic confirmed idle PTYs are cheap when their readers block; here they are cheaper still because reads are edge-triggered off the event loop rather than a thread per pane.
- PTY reads and writes have per-readiness fairness budgets, and child input is retained in a bounded queue until writable readiness.
- Client output is also bounded.
  A client that remains too slow is detached instead of blocking pane progress or allowing memory to grow without limit.
- Child processes are spawned into their own process group with `setsid`, so the whole descendant tree can be signalled and reaped as a unit with an escalating SIGTERM then SIGKILL.
  Shutdown sends SIGTERM to every target group together, grants one short collective grace period, then sends SIGKILL to survivors and reaps each pane shell.
  This matters for agents that spawn subprocesses.

## Tabs, layouts, and resizing

- A Tab's Panes are arranged in a **layout tree** whose nodes are left-right splits, top-bottom splits, and leaves (Panes), following tmux's structure.
  We persist this as structured data, not tmux's opaque checksum strings.
- Built-in layouts: even-horizontal, even-vertical, main-pane (one large plus a stack), and tiled, plus arbitrary user splits.
- Named layout templates (from Uniterm: for example a claude-plus-shell pair, or a planner-builder-verifier triad) are first-class and can be applied by name or from the command language.
- Resizing propagates from the smallest attached client down the layout tree, redistributing space and marking affected panes damaged.
- Width changes reflow visible content and scrollback from retained soft-wrap metadata.
  Grapheme boundaries, double-width cells, the active cursor, the alternate buffer, and the stashed primary buffer remain coherent through the transformation.
  The reflow streams cells row to row with no persisted intermediate, keeps the grid's arenas, and stores only content for hard-newline history; height-only changes move rows between the viewport and history without reflowing at all.
- Resizes are coalesced at both ends: the client reports the settled size after a short quiet period, the server applies only the last size in a batch, and a pane's child is signalled once per rectangle that actually changed.
  Everything in a drag therefore costs one relayout, one reflow, and one repaint.
- Zoom (temporarily maximize one pane) and directional focus navigation (move focus by geometry, not just cycle order) are supported.

## Copy-mode

- A vi-like and an emacs-like key table for scrollback navigation, selection, and search, following tmux's copy-mode.
- Search with incremental highlighting and result navigation.
- Selection to the system clipboard (OSC 52 aware, so it works over SSH).
- Live scrollback eviction rebases a frozen copy viewport, and pane resize safely clamps its cursor and clears coordinates invalidated by reflow.
- The viewport addresses logical lines, so scrolling output never moves it; the optional `freeze-on-select` setting also copies the screen rows the moment a selection starts (a drag, or `v`), so an application that repaints in place or owns the alternate screen cannot change the text being selected until the selection ends.
- An application that asks for mouse tracking owns the mouse, so a drag is normally its own; with `freeze-on-select` uniterm keeps left-button drags for its selection everywhere and withholds the press until the release shows it was a plain click, which the application then receives whole.
- `copy-on-select` (default on) yanks a mouse selection on release; off, the selection stays highlighted in the frozen pane until `y` or Enter copies it, or Esc, `q`, or a plain click dismisses it.
- Semantic navigation using OSC 133 marks: jump to the previous or next shell prompt, and select the output of the last command.
  This is more useful in agent panes than line-by-line scrolling.
- A viewport more than one page behind live output shows a clickable Latest button beside the retained current-line/total-lines indicator.
  Ctrl+End on Linux and Cmd+Down on macOS return to live output when the terminal reports the modified key.

## Mouse support

Mouse support is in scope for v1, because it is a top-tier multiplexer feature and its absence is immediately felt.

- **Mouse mode is negotiated and configurable** (`mouse = true` in the behavior config, see [10-config-commands-keybindings.md](10-config-commands-keybindings.md)).
- **Passthrough first.**
  When the focused pane's child application has requested mouse reporting (it enabled an SGR mouse mode), we forward mouse events to it, so full-screen programs (editors, pagers, other TUIs) get their clicks and drags.
  We use SGR extended mouse encoding, not the legacy coordinate-limited encoding, so wide terminals work.
- **Multiplexer gestures when the child is not consuming the mouse:**
  click a Pane to focus it, right-click a Pane, Tab, or file-manager row for a target-specific context menu, drag a split divider to resize, click or horizontally scroll the Tab bar, click its fixed new-Tab button, click a Project in the left rail to switch it, switch Observatory tabs in the right rail, and vertically scroll either rail.
- **Copy-mode selection**: drag to select, with the selection landing on the system clipboard (OSC 52 aware).
- Mouse events arrive as input on the client, are framed to the server like keystrokes, and are resolved against the layout to decide whether they are a gesture or passthrough.
  Like every other input, they touch only the core loop, never an async runtime.

## Workspaces surviving detach and restart

- Detach and restart-survival across a client disconnect is inherent to the client-server model: the server keeps running.
- Survival across a **server restart** (crash, upgrade, or reboot) is the persistence layer's job and is covered in full in [05-session-persistence.md](05-session-persistence.md).
  The short version: PTYs cannot survive a server process exit, but the event log and grid-and-layout snapshots restore the full Workspace > Project > Tab > Pane tree, layouts, scrollback, focus, metadata, and working directories atomically.

## The control protocol

The control protocol is how every non-human client reaches the server: scripts, an agent's own CLI calls, and the future web and mobile companion.
It is a deliberate superset of tmux control mode.

Two layers on one socket:

1. **A line-based `%`-directive stream**, source-compatible in spirit with tmux control mode, for streaming output and lifecycle notifications.
   `%output`, `%pane-mode`, `%layout-change`, `%session-changed`, and so on, plus subscriptions to specific state and a pause mechanism for slow consumers.
   This keeps existing control-mode tooling patterns familiar.
2. **A structured request-response API** (JSON-RPC framed on the same socket) for everything richer than tmux ever exposed: query fleet status, list waiting-queue items and resolve them, submit a workflow or relay turn, launch a task, subscribe to agent-state changes, stream the event log.

The structured layer is what the agentic features, the Observatory-over-the-wire, and the remote surfaces are built on.
It is the reason the control protocol is called out as a first-class deliverable rather than an afterthought: it is the seam that keeps the whole product extensible and remotely reachable without bloating the binary.

Design rules for the control protocol:

- The local runtime directory is mode 0700 and each Workspace socket is mode 0600; Uniterm refuses to replace a live socket or a non-socket path.
- Each server also holds a process-lifetime advisory lock keyed by Workspace name, so unlinking a live listener path cannot admit a second server for the same durable state.
- Client health probes never unlink sockets.
  Only server startup may remove a socket, after the lifetime lock is acquired and the kernel returns a definitive connection-refused or not-found error.
- Client control frames are capped at 2 MiB, buffered bytes never exceed that cap, and abusive connection or per-readiness message counts are rejected.
- Listener accepts are processed in bounded batches, with other ready PTYs and clients serviced between batches while a real backlog remains; once the backlog drains, polling returns to its blocking idle state.
- Ordering: `%output` blocks preserve pane byte order; structured notifications are sequenced so a consumer can track state without races (an improvement on tmux, whose control mode guarantees ordering for output but not for notifications).
- Subscriptions are stateful and explicit, so a reconnecting client can resync.
- Every mutating call is authenticated by a token (mode 0600 on disk) so an arbitrary local process cannot drive the fleet.

## Options and configuration

- Options are hierarchical (global, Workspace, Project, Tab, Pane) with inheritance.
- Unlike tmux, the persisted config is a structured file, not a shell-like command script; the command language exists for runtime and scripting.
  See [10-config-commands-keybindings.md](10-config-commands-keybindings.md).
- Format strings (`#{...}`-style) are supported for status lines and templated output, because they are genuinely useful, but they are not the config language.

## What "mostly complete tmux alternative" includes for v1

- Workspaces, Projects, Tabs, Panes, splits, layouts, zoom, directional navigation.
- Copy-mode with search and semantic navigation.
- Mouse support: gestures for focus, resize, and scroll, plus SGR passthrough to child applications.
- Status line, configurable and format-string driven.
- A command language and rebindable keys with a prefix model.
- The control protocol (superset of control mode).
- Built-in Workspace save, restore, and continuous autosave, including scrollback and layout.
- Client-server with detach, multi-client attach to the same Workspace, and server-restart survival.

Deferred past v1: remote and SSH-nested edge cases beyond basic attach, a full third-party plugin runtime, and Windows.
