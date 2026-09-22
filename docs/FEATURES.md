# Features

The complete list of what Uniterm does today, in the words of the design record.
[STATUS.md](STATUS.md) maps each capability to its entry point and its test; [USAGE.md](USAGE.md) explains how to reach each one.

Multiplexer:

- Client-server over a Unix socket.
- The product hierarchy is **Workspace > Project > Tab > Pane**, the same model as Uniterm Desktop.
- A Workspace is one durable server and safety scope, a Project owns a root and metadata, a Tab owns a layout tree, and a Pane owns one PTY.
- The server owns all state; clients are disposable, so detaching or a client crash never loses a Workspace.
- A responsive, vertically scrollable Projects sidebar uses spaced two-row cards, a clear active-Project background, compact shell-prompt paths, and automatic narrow-terminal collapse.
- A theme-coloured Workspace button anchors that sidebar and opens Workspace management in place, with the rail divider continued beside it through the top bar.
- Project roots use compact shell-prompt paths such as `~/W/uniterm` instead of consuming a full row.
- A persistent, vertically scrollable Observatory rail on the right switches among Agents, File manager, and Web servers without covering terminal content.
- Its event-driven file manager watches only expanded directories while visible and supports browsing, opening, creating, renaming, deleting, and copying paths.
- Splits (horizontal and vertical), directional focus, pane resize by keyboard or by dragging a divider, zoom, and multiple Tabs.
- Right-click a Pane to move it to another Tab of the same Project or to a fresh Tab; its process and scrollback come along.
- A damage-tracked renderer that writes zero bytes when nothing visible changed.
- Grapheme-aware Unicode rendering with combining sequences, emoji clusters, double-width cells, and exact wide-glyph erasure.
- Width-change reflow for visible content and scrollback, with logical wrap metadata and cursor remapping.
- Copy-mode with keyboard navigation, selection, search, and OSC 52 clipboard.
- Alternate-screen support, so full-screen apps (vim, less, htop, man, git log) restore the prior screen on exit.
- A roomy top status and Tab bar that stays visible across every operation, scrolls horizontally when needed, keeps its new-Tab button reachable, and adapts to live terminal resizes.
- Built-in resurrect/continuum: Projects, Tabs, Pane trees, layouts, focus, working directories, metadata, and scrollback are snapshotted atomically and restored after a crash.
- A clean stop keeps the Workspace's event stream, so tasks, run history, and the audit trail survive an intentional stop; a damaged stream is repaired or quarantined rather than refusing to start.
- One Workspace has exactly one server: a second server under a different runtime directory is refused instead of sharing the durable files.
- Focus-in and `SIGCONT` recovery repaint only the affected client, eliminating stranded characters after suspend/resume or a foreground app exits.
- Deterministic multi-client sizing uses the smallest attached viewport, while bounded non-blocking I/O prevents a stalled client or child PTY from growing memory without limit.
- SSH remote attach keeps the UI client local while the persistent Workspace server and PTYs stay on the remote host.

Agentic layer:

- Agent detection reconciles cooperative OSC 777, provider-native logs, anchored screen rules over the live grid and window title, foreground process identity, and native kernel exit events.
- Working is a positive match (a spinner or an anchored activity line), never output volume, so typing a prompt or a repainting footer cannot mark an idle agent busy.
- Built-in provider rules cover Claude Code, Codex, OpenCode, Gemini, Grok, Kiro, and Cursor Agent; versioned local and verified-cache manifests add or override providers without branching in the core.
- `ut agent explain` reports the winning authority, source, manifest version, rule, precedence, invocation, confidence, dwell hint, timestamp, and exact evidence instead of hiding heuristic guesses.
- Provider-branded agent entries in the Observatory's Agents tab, without shrinking, recolouring, or framing agent Panes; an agent working in a Git worktree Project shows its branch beside its status.
- Destructive and bulk actions (removing a Project, stopping every agent, stopping a Workspace) carry an explicit confirmation and are recorded as guardrail decisions before anything closes.
- A floating "New Task" window (with an ASCII drop-shadow) to launch a prompt or an orchestration inline.
- Live tokened workflow and relay runtimes backed by pure, exhaustively-tested decision engines, bounded delivery retry, artifact gates, Git checkpoints, restart recovery, and an actionable waiting queue.
- Per-role provider selection lets one native workflow use different installed CLIs for planning, building, and verification while preserving provider-owned login and resume behavior.
- A native event-backed run graph gives every orchestration stable Run and Role identities, parent links, Project, Pane, and provider ownership, checkpoint recovery, CLI and control inspection, and active Observatory context.
- An event-backed instruction queue lets humans add, replace, cancel, or explicitly send follow-up direction without racing a busy agent's terminal input.
- A docked Observatory keeps agents, Project file access, and detected web servers beside the terminal, with direct Pane focus and workspace-safe actions.
- Agent attention notifications can appear as a clickable Uniterm toast, a host-terminal notification, or a native macOS/Linux notification, with an optional bell, built-in chime, or custom sound played where you are attached.
- Durable tasks and a task-management view.
