# Uniterm

Uniterm is a terminal multiplexer built for agentic engineering, written in Rust.

It is one static binary that is at once:

- a complete tmux-class multiplexer (client-server, persistent Workspaces, Projects, Tabs, splits, resize, zoom, copy-mode, alternate screen, and built-in save/restore), and
- an agent-fleet supervisor (per-agent status detection and colouring, a New Task launcher, durable deterministic workflows and relays, actionable waiting and instruction queues, durable tasks, and an Observatory for agents, files, and development servers).

It succeeds Uniterm Desktop, the earlier GUI application, as a native terminal program built around performance from the first line: the renderer emits only the cells that actually change, nothing wakes the loop just because time passed, and process and agent liveness comes from the kernel rather than polling.
Idle Workspaces cost nothing, keystrokes reach the screen without waiting on anything else, and a fleet of agents fits in a few tens of megabytes.

The full design of record lives in [`docs/`](docs/README.md).

## Features

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
- Agent attention notifications can appear as a clickable Uniterm toast, a host-terminal notification, or a native macOS/Linux notification.
- Durable tasks and a task-management view.

## Install

Prebuilt binaries are published on the GitHub Releases page; until the first release is published, build from source as described below.
Once a release exists, install it with one command:

```sh
curl -fsSL https://uniterm.dev/install.sh | sh
```

The installer downloads `uniterm` and its recommended `ut` alias from the latest GitHub release, verifies both against the release's SHA-256 manifest, and installs them in `/usr/local/bin` when possible.
It falls back to `~/.local/bin` when privilege escalation is unavailable and uses `$PREFIX/bin` on Termux.
Set `UNITERM_INSTALL_DIR` to choose another destination or `UNITERM_VERSION=v1.0.0` to pin a release.

Prebuilt releases support Apple Silicon macOS, glibc Linux on x86-64 and ARM64, x86-64 or ARM64 WSL, and AArch64 Android Termux.
Windows users should run the command inside WSL.
Intel macOS and native Windows are not supported.

```sh
curl -fsSL https://uniterm.dev/install.sh | UNITERM_INSTALL_DIR="$HOME/.local/bin" sh
```

Until the domain endpoint is deployed, the same versioned installer is available from the public repository:

```sh
curl -fsSL https://raw.githubusercontent.com/maxart/uniterm/main/install.sh | sh
```

### Build from source

Building requires Rust 1.96 or newer and a C toolchain: Xcode Command Line Tools on macOS or `build-essential`/`gcc` on Linux.

```sh
git clone https://github.com/maxart/uniterm.git
cd uniterm
cargo build --release --workspace
cargo install --path crates/uniterm-cli --bins
```

`cargo install` places the `uniterm` and `ut` binaries in `~/.cargo/bin`, which should be on your `PATH` (rustup adds it).
`ut` is a real second binary, not a shell alias, and is the recommended way to invoke Uniterm.

Note: if you previously had Uniterm Desktop installed, its `uniterm` executable may shadow this one on your `PATH`.
Prefer `ut`, or remove the old binary, so you are running this build.

State is stored under the XDG directories: Workspace snapshots and event logs live in `$XDG_STATE_HOME/uniterm/` (default `~/.local/state/uniterm/`), and config is read from `$XDG_CONFIG_HOME/uniterm/uniterm.conf` (default `~/.config/uniterm/uniterm.conf`).
Runtime and state directories are owner-only (0700), and Workspace sockets and state files are owner-only (0600).
Snapshots and event logs can contain terminal output and project metadata, so treat the state directory as sensitive local data even though it is not sent anywhere or encrypted at rest.

### Reproducible release builds

Cross-platform release binaries always go under `target/dist/<os>-<arch>/`.
Each platform folder contains only the executable `uniterm` and `ut` binaries, with no archives, versioned folders, checksums, or build caches.
Use the canonical builder instead of invoking `cargo zigbuild` directly:

```sh
scripts/build-dist.sh macos-arm64
scripts/build-dist.sh ubuntu-x86_64
scripts/build-dist.sh ubuntu-aarch64
scripts/build-dist.sh arch-x86_64
scripts/build-dist.sh fedora-x86_64
scripts/build-dist.sh android-aarch64
```

The macOS target is Apple Silicon only.
Intel and universal macOS builds are intentionally unsupported.
The Android target is AArch64 Linux for Termux and uses API level 24 by default.
Native Termux builds use Cargo directly.
Cross-builds accept `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT`, or the legacy `ANDROID_NDK` variable; set `ANDROID_API_LEVEL` to override the minimum API level.
The legacy NDK r10e package automatically falls back to API 21 when no API level is explicitly requested.

```sh
skill/uniterm-release-build/scripts/release-uniterm.sh --repo .
```

`.agents/skills/uniterm-release-build` points to the same committed skill folder so Codex-compatible repository discovery uses the canonical copy.

## Usage

```sh
ut                                  # attach to or create the "default" Workspace
ut work                             # attach to or create the "work" Workspace
ut workspace list                  # list running Workspaces
ut workspace new -d work           # create a detached Workspace
ut project add frontend ~/src/web  # add a Project and its first Tab
ut project list -w work            # list Projects in a Workspace
ut pane list -w work --json        # list live Panes with stable ids
ut pane focus 3 -w work            # focus Pane 3 from another process
ut tab focus frontend 2 -w work    # focus Project frontend's second Tab
ut pane focus frontend 2 1 -w work # focus its first Pane by hierarchy
ut agent explain                   # explain the active Pane's detection state
ut run list -w work --json         # inspect durable Run, Role, Pane, and parent links
ut migrate from-desktop --dry-run  # preview a hierarchy import from Desktop
ut remote workbox                  # attach to the remote default Workspace over SSH
ut remote workbox agents           # attach to the remote "agents" Workspace
ut --remote workbox agents         # equivalent flag spelling
```

### Remote Workspaces over SSH

Install the same Uniterm build on the local and remote machines, make sure `uniterm` or `ut` is available on the remote non-interactive `PATH`, then run `ut remote HOST [Workspace]`.
Use `~/.ssh/config` for ports, identities, jump hosts, and stable aliases.
Uniterm verifies wire compatibility before entering raw terminal mode, reuses the authenticated OpenSSH connection, enables keepalives, and connects the normal thin client through a private local proxy.
The remote server auto-starts when needed and survives detach, SSH loss, or closing the local terminal.

Remote attach uses local keybindings and client-side overlays, while remote Projects, Panes, persistence, and agent integrations remain owned by the remote server.
Detach and run another `ut remote HOST NAME` command to change remote Workspaces.
See [`docs/14-ssh-remote-sessions.md`](docs/14-ssh-remote-sessions.md) for the transport, failure, and security design.

Inside a Workspace, commands are triggered with a prefix key, `Ctrl-A` by default (configurable).

### Keyboard

| Keys | Action |
| --- | --- |
| `Ctrl-A` then `d` | detach (the Workspace keeps running) |
| `Ctrl-A` then `%` | split left/right |
| `Ctrl-A` then `"` | split top/bottom |
| `Ctrl-A` then `h` `j` `k` `l` | move focus between panes |
| `Ctrl-A` then `Shift-H/J/K/L` | resize the focused pane |
| `Ctrl-A` then `z` | zoom in on the focused pane (toggle) |
| `Ctrl-A` then `w` | zoom out: every Tab in the Project as a live miniature |
| `Ctrl-A` then `x` | close the focused pane |
| `Ctrl-A` then `c` | new Tab in the active Project |
| `Ctrl-A` then `,` | rename the current Tab |
| `Ctrl-A` then `&` | close the current Tab and all its Panes |
| `Ctrl-A` then `n` / `p` | next / previous Tab in the Project |
| `Ctrl-A` then `0`..`9` | select Tab by number |
| `Ctrl-A` then `[` | enter copy-mode (scroll, select, search) |
| `Ctrl-A` then `m` | open the command menu |
| `Ctrl-A` then `A` | open the New Project modal |
| `Ctrl-A` then `P` | Manage Projects (find, switch, add, rename, remove) |
| `Ctrl-A` then `b` | toggle the left-hand Workspace sidebar |
| `Ctrl-A` then `g` | Settings (theme, sidebar, behavior, restore) |
| `Ctrl-A` then `f` | reveal and focus the Observatory file manager |
| `Ctrl-A` then `s` | Manage Workspaces (find, switch, close) |
| `Ctrl-A` then `$` | rename the current Workspace |
| `Ctrl-A` then `Q` | close this Workspace and all its Panes |
| `Ctrl-A` then `N` | New Task (floating prompt) |
| `Ctrl-A` then `o` | toggle the right-hand Observatory |
| `Ctrl-A` then `t` | task manager (list + details, edit/status/delete) |
| `Ctrl-A` then `Ctrl-A` | send a literal `Ctrl-A` |

In the New Task window you can type a plain prompt (it spawns a pane and runs it) or an inline slash-command: `/relay`, `/workflow <name>`, `/project <name>`, or `/save <title>`.
The window autocompletes as you type: an empty input lists every slash command with what it does, `/workflow ` suggests the bundled templates (`solo`, `pair`, `triad`) with their role lineups, `/project ` suggests project names you have used before, and an `@` word suggests the installed providers.
Arrows move the selection, `Tab` completes.

Pick the global provider with an `@` word anywhere in the line: `@claude fix the tests` launches Claude Code in a new Pane with that prompt (with no `@` word the first installed provider is used).
For a workflow or relay, add explicit role overrides such as `/workflow pair @claude @verifier=codex Ship the feature` or `/relay @builder=codex @reviewer=claude Audit the parser`.

`/workflow <template> [@provider] [@role=provider ...] <goal>` runs a real multi-agent workflow: a new Tab opens with one Pane per role, each role's selected provider receives a ported role prompt with the goal and a per-turn completion token, and the run advances when a role finishes with `uniterm workflow submit <token>` (the injected prompt spells out the exact command).
The verifier alone passes verdict: `--verdict approved` completes the run (the Tab title flips to `wf:<template>: done`), `--verdict fix --summary "..."` loops the findings back to the builder, with iteration caps and stall detection so runs cannot loop forever.
Inspect the Workspace-native relationships with `ut run list [--project ID] [--active] [--json]`.
The same `run_list` projection and provider-neutral `orchestration_start` launch are available on the local control API, and the Observatory shows the active Run and Role for each live turn.

Queue a follow-up for a busy agent with `ut instruction add PANE TEXT`.
The direction is bound to that exact invocation and reaches it one item at a time only when the agent emits a cooperative ready event.
Use `ut instruction list`, `replace`, or `cancel` to manage the queue, and `ut instruction send-now ID` for an explicit urgent bypass.
Heuristic idle detection never injects queued text.

Create an isolated Project with `ut project worktree add NAME REPO PATH [BASE]`.
Use `ut project worktree list` or `open PROJECT` to inspect and enter registered worktrees.
Removal is clean-only by default; a dirty worktree requires the separately confirmed `remove PROJECT --force --yes`, while `cleanup PROJECT` only forgets a path Git proves is already absent or prunable.
The same lifecycle is available through the local control API and survives Workspace restart.

### Context and command menus

Uniterm keeps its status line focused on navigation instead of permanent menu titles.
Right-click a Pane or Tab to open the menu for that exact target, click the coloured Workspace or Agents button for its anchored menu, or press `Ctrl-A m` to open the command menu.
Every item shows its keyboard shortcut, so the menus double as a cheat sheet.
While a menu is open: arrows (or `h j k l`) navigate, `Enter` or a click runs the item, `Esc`/`q` or a click elsewhere closes.
The Workspace menu groups Project creation and management, Workspace management and rename, `Projects` and `Observatory` visibility toggles, Settings, then Detach and Close this Workspace.

### Mouse

Mouse support is on while attached; selection and scrolling work out of the box, no toggles.

- **Select text by dragging**, exactly like a plain terminal: the selection highlights as you drag and is copied to your system clipboard on release (via OSC 52).
  Apps that ask for mouse tracking (vim with `mouse=a`, htop) receive the mouse instead and handle selection themselves; `Shift`+drag always gives you the terminal's native selection.
- **Scroll wheel just works**: it scrolls back through a pane's history, moves either sidebar vertically, and moves an overflowing Tab bar horizontally.
  Full-screen apps get wheel-as-arrow-keys, and apps that ask for mouse tracking get the real events.
- Click a Pane to focus it. Optional focus-follows-mouse can be enabled in Settings.
- Right-click a Pane for split right/down, zoom, overview, copy-mode, new Tab, "Move to tab N" for every other Tab of the same Project, "Move to new tab", and close.
- Press on a divider and drag: the divider follows the pointer and the new split ratio persists with the layout.
- Right-click any Tab for its Tab menu, and click the always-visible `+` button to create a Tab immediately to its left.
- Right-click a file-manager row for open/expand, absolute or Project-relative path copy, create, rename, delete, and refresh actions.
- Clicks and drags are forwarded to apps that asked for mouse tracking, translated to pane coordinates.
- Click a Tab in the status line to switch to it; overflow arrows keep every Tab reachable.
- Click a Project in the left sidebar or an agent in the right Observatory to jump to it.
- Click the Workspace and Agents buttons to open their anchored menus.
- Switch the Observatory among Agents, File manager, and Web servers from its top tabs; click a detected server to open its URL.
- Click a tile in the zoom-out overview to switch Tabs.
  Each overview tile is a true miniature of its Tab: the real split layout scaled down, every Pane's content sampled into place with its colours, the selected tile at full brightness.
- Click outside an open overlay to dismiss it.

### The task manager

`Ctrl-A t` opens the task manager: a two-pane modal with the task list on the left (colour-coded status dots: blue planned, amber running, red blocked, green finished) and the selected task's details on the right.
Arrows or the wheel move the selection (clicking a row selects it too); `e` edits the title inline, `Space` cycles the status, `x` deletes (press `x` again to confirm), `Esc` closes.
The action bar at the bottom shows every key, and each action round-trips through the server so the list always reflects Workspace truth.

### The Observatory

The right-hand Observatory is a persistent dock with Agents, File manager, and Web servers tabs.
The Agents tab orders live agents by urgency and supports active-Project or whole-Workspace scope; click a card to jump to its exact Pane and use the coloured `Manage...` footer button for agent actions.
The Web servers tab lists event-driven, loopback-verified server detections and opens a server's URL on click.
Each Observatory tab keeps an independent vertical scroll position.

### The file manager

`Ctrl-A f` reveals the Observatory's File manager tab and gives it keyboard focus.
It uses a Midnight Commander-style tree and reserves terminal geometry instead of painting over child applications.
Use arrows or `h j k l` to browse, `Enter` to expand a folder or open a file in its configured editor, `n` for a file, `N` for a folder, `R` to rename, `d` twice to delete, `y` to copy the absolute path, `Y` to copy the Project-relative path, `.` to show hidden entries, and `r` to refresh.
Right-click any file or folder for the same actions in a context menu; right-click empty file-manager space to create at the Project root or copy its path.
Press `Esc` to return keyboard focus to the terminal while leaving the tree visible.
Directory reads, file mutations, and non-recursive OS watches run outside the terminal hot path, and every operation is sandboxed to the active Project root.

### Workspaces and Projects

`Ctrl-A s` opens Manage Workspaces: type any part of a Workspace name to narrow the list instantly.
Use the arrows to select a match and `Enter` or a click to attach to it in place.
`Ctrl-G x` stops the selected Workspace.
`Ctrl-A $` renames the current Workspace in a prefilled input; the socket, snapshot, and status line follow.

`Ctrl-A P` opens Manage Projects for the active Workspace.
`Ctrl-A A` opens the New Project modal directly, and `Ctrl-A b` toggles the left sidebar.
Projects have stable ids, names, roots, last-focused Tabs and Panes, and durable metadata.
Type any part of a Project name or folder to narrow the list, then press `Enter` to switch.
Matching is case-insensitive; Project-name matches take precedence over root-folder and full-path matches, so a shared parent folder does not swamp a name search.
Use `Ctrl-G` followed by `n` to add, `r` to rename, `K`/`J` to move the selected Project up/down, or `X` twice to remove.
Reordering updates the modal and left sidebar immediately and persists with the Workspace.
The CLI exposes the same model through `ut project list|add|switch|rename|move|remove|metadata`, plus `ut pane list [--json]`, `ut agent list [--json]`, `ut tab focus <project> <tab>`, `ut tab new [project]`, `ut tab rename <project> <tab> <name>`, and both stable-id and hierarchy forms of `ut pane focus`.
Project selectors accept a stable id or case-insensitive name, Tab selectors accept a 1-based ordinal or case-insensitive name, and hierarchy Pane selectors use the 1-based ordinal reported by `ut pane list --json`.

The left sidebar is the persistent Project overview for this hierarchy.
It begins directly with Projects, uses the host terminal's default foreground and background, and gives every Project two rows of context.
It displays Project attention counts, supports clicks and vertical scrolling, uses a compact width on medium terminals, and disappears below the safe content threshold.
Project roots are abbreviated like a shell prompt, with `~` for home and compact parent folders.
Agents keep their provider colours in the right Observatory rail, where they stay grouped by Project and start time, can be scoped to the active Project or whole Workspace, and show the Git branch when their Project is a worktree.
Manage Workspaces and Manage Projects lay their rows out in aligned columns (name, state, counts) and Manage Workspaces probes sibling servers in parallel, so it opens in one round trip.

### Settings and themes

`Ctrl-A g` opens Settings: a rail of setting names grouped under Appearance, Behaviour, Editors, Notifications, Guardrails, and Tools, and a pane with the selected setting's help text and a control that fits it (an on/off switch, a stepper that shows its range, a boxed editor, an option list, or a button).
Theme is a searchable list with a live preview of the highlighted theme's palette, status bar, and Project row; moving previews, Enter applies, Esc backs out.
Changes apply immediately and are saved atomically through the background runtime.
The `Import Uniterm Desktop` row opens the same interactive hierarchy importer as `ut migrate from-desktop` after restoring normal terminal mode.
After success, Uniterm attaches to an imported Workspace and opens the Workspace picker when several were imported.
It imports only Workspaces, Projects with canonical paths, and Tabs, and it never silently overwrites an existing CLI Workspace.
The writer changes only Settings-owned keys, preserving comments and advanced hand-written options.
The default editor accepts a command such as `nano`, `vim`, or `nvim --clean`; optional file-type rules use entries such as `md=glow; rs=nvim` and take precedence for matching extensions.
Uniterm validates each executable on the runtime's `PATH` before saving the setting or opening a file, and reports an error in the Settings or Files surface when it cannot be resolved.
When `confirm-close` is enabled, closing a Pane or Tab opens a shared confirmation surface before terminating its process.
Agent attention notifications wait for a stable status before delivery and are canceled if the agent moves on first.
The `system` channel prefers `terminal-notifier` and falls back to built-in AppleScript on macOS; on Linux it uses `notify-send` when a graphical session and the helper are available.

## Driving Uniterm from an AI agent

[`skills/manage-uniterm/SKILL.md`](skills/manage-uniterm/SKILL.md) is a harness-neutral skill (the plain `SKILL.md` format read by Claude Code, Codex, OpenCode, Cursor, and similar tools) that teaches an agent to find and monitor other agents across Panes, Tabs, and Projects, read and send Pane text, copy between Panes, focus and organise the hierarchy, queue direction, and handle the waiting queue, all through `ut`.
Point your harness's skill discovery at the `skills/` directory, or copy the folder into its skills location.
`ut --skill` prints a shorter version for agents that cannot load skills.

## Configuration

Uniterm reads a Ghostty-style `key = value` config from `~/.config/uniterm/uniterm.conf`.
Unknown keys are ignored, so it is forgiving.

```ini
# ~/.config/uniterm/uniterm.conf
prefix = C-a               # the prefix key (e.g. C-a, C-b)
status = on                # show the status line
status-position = top      # top | bottom
scrollback-limit = 10000
theme = uniterm-dark       # select any bundled semantic theme
sidebar = true
sidebar-width = 24         # 16..40; responsive at runtime
file-sidebar = true        # legacy name: show the right Observatory rail
file-sidebar-width = 36    # Observatory width, 22..52
notification-delivery = uniterm  # off | uniterm | terminal | system
notify-completion = false  # also notify when an agent becomes idle
focus-follows-mouse = false
confirm-close = true
restore = true             # resurrect a saved Workspace on start (alias: autosave)
```

Bundled semantic themes are Uniterm dark/light, Catppuccin, Tokyo Night, Dracula, Nord, Gruvbox dark/light, Solarized dark/light, Kanagawa, and Rose Pine.
Theme roles style the top bar, Tabs, borders, and client dialogs, including a surface-blended secondary accent for buttons that keeps the active Tab and Project visually dominant.
The sidebar and application canvas retain terminal-native colours, and child application colours are never dimmed or recoloured for focus.

Local agent providers can be added or overridden without rebuilding, and a valid atomic replacement hot-reloads on the Tokio runtime:

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

Save that document as `$XDG_CONFIG_HOME/uniterm/providers.json` (normally `~/.config/uniterm/providers.json`).
Validate it offline with `ut agent manifests validate PATH`.
The complete schema, source precedence, verified cache, last-known-good, and reload contract is in [`docs/22-provider-detection-manifests.md`](docs/22-provider-detection-manifests.md).

## Building and testing

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets   # warning-free
cargo fmt --all --check                  # formatted
```

All four gates are expected to pass on every change.

An opt-in reliability soak repeatedly attaches short-lived control clients while keeping a detected development server live.
For an eight-hour run:

```sh
UNITERM_SOAK_SECONDS=28800 cargo test --release -p uniterm-server --test reliability_soak -- --ignored --nocapture
```

## Repository layout

```
crates/
  uniterm-core/     Pure model + logic (grid + damage, layout tree, agent status,
                    orchestration brains, tasks). No UI, no async, no I/O.
  uniterm-proto/    Wire and channel message types.
  uniterm-server/   The mio core loop, damage-tracked renderer, PTYs, persistence,
                    event log, and the agentic surfaces' server side.
  uniterm-client/   The thin attach client, mouse handling, and overlays.
  uniterm-cli/      The `uniterm` binary front door (alias `ut`).
docs/               The design of record.
```

See [`AGENTS.md`](AGENTS.md) (`CLAUDE.md` is a symlink to it) for the architectural invariants and contribution guide.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the gates every change must pass and [SECURITY.md](SECURITY.md) for how to report a vulnerability.

## License

MIT.
