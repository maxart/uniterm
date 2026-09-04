# Using Uniterm

Everything you can do inside a Workspace and from the `ut` command line.
Install first with the [README](../README.md) quick start or [INSTALL.md](INSTALL.md); configuration lives in [CONFIGURATION.md](CONFIGURATION.md).

## The command line

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

## Remote Workspaces over SSH

Install the same Uniterm build on the local and remote machines, make sure `uniterm` or `ut` is available on the remote non-interactive `PATH`, then run `ut remote HOST [Workspace]`.
Use `~/.ssh/config` for ports, identities, jump hosts, and stable aliases.
Uniterm verifies wire compatibility before entering raw terminal mode, reuses the authenticated OpenSSH connection, enables keepalives, and connects the normal thin client through a private local proxy.
The remote server auto-starts when needed and survives detach, SSH loss, or closing the local terminal.

Remote attach uses local keybindings and client-side overlays, while remote Projects, Panes, persistence, and agent integrations remain owned by the remote server.
Detach and run another `ut remote HOST NAME` command to change remote Workspaces.
See [`14-ssh-remote-sessions.md`](14-ssh-remote-sessions.md) for the transport, failure, and security design.


## Keyboard

Inside a Workspace, commands are triggered with a prefix key, `Ctrl-A` by default (configurable).

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

## Context and command menus

Uniterm keeps its status line focused on navigation instead of permanent menu titles.
Right-click a Pane or Tab to open the menu for that exact target, click the coloured Workspace or Agents button for its anchored menu, or press `Ctrl-A m` to open the command menu.
Every item shows its keyboard shortcut, so the menus double as a cheat sheet.
While a menu is open: arrows (or `h j k l`) navigate, `Enter` or a click runs the item, `Esc`/`q` or a click elsewhere closes.
The Workspace menu groups Project creation and management, Workspace management and rename, `Projects` and `Observatory` visibility toggles, Settings, then Detach and Close this Workspace.

## Mouse

Mouse support is on while attached; selection and scrolling work out of the box, no toggles.

- **Select text by dragging**, exactly like a plain terminal: the selection highlights as you drag and is copied to your system clipboard on release (via OSC 52).
  Apps that ask for mouse tracking (vim with `mouse=a`, htop) receive the mouse instead and handle selection themselves; `Shift`+drag always gives you the terminal's native selection.
  Turn on `Freeze on select` in Settings (`freeze-on-select = true`) and the Pane's screen holds still from the first drag until release, so an agent that keeps printing or repainting cannot move the text you are selecting.
  The same switch makes a plain drag uniterm's selection even in apps that take the mouse, such as an agent UI on the alternate screen or vim with `mouse=a`; those apps still receive clicks, the wheel, and pointer motion, but never a left-button drag.
  `Copy on select` (`copy-on-select`, on by default) copies the selection on release and resumes live output; turned off, the selection stays highlighted until `y` or `Enter` copies it, or `Esc`, `q`, or a plain click dismisses it.
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

## The task manager

`Ctrl-A t` opens the task manager: a two-pane modal with the task list on the left (colour-coded status dots: blue planned, amber running, red blocked, green finished) and the selected task's details on the right.
Arrows or the wheel move the selection (clicking a row selects it too); `e` edits the title inline, `Space` cycles the status, `x` deletes (press `x` again to confirm), `Esc` closes.
The action bar at the bottom shows every key, and each action round-trips through the server so the list always reflects Workspace truth.

## The Observatory

The right-hand Observatory is a persistent dock with Agents, File manager, and Web servers tabs.
The Agents tab orders live agents by urgency and supports active-Project or whole-Workspace scope; click a card to jump to its exact Pane and use the coloured `Manage...` footer button for agent actions.
The Web servers tab lists event-driven, loopback-verified server detections and opens a server's URL on click.
Each Observatory tab keeps an independent vertical scroll position.

## The file manager

`Ctrl-A f` reveals the Observatory's File manager tab and gives it keyboard focus.
It uses a Midnight Commander-style tree and reserves terminal geometry instead of painting over child applications.
Use arrows or `h j k l` to browse, `Enter` to expand a folder or open a file in its configured editor, `n` for a file, `N` for a folder, `R` to rename, `d` twice to delete, `y` to copy the absolute path, `Y` to copy the Project-relative path, `.` to show hidden entries, and `r` to refresh.
Right-click any file or folder for the same actions in a context menu; right-click empty file-manager space to create at the Project root or copy its path.
Press `Esc` to return keyboard focus to the terminal while leaving the tree visible.
Directory reads, file mutations, and non-recursive OS watches run outside the terminal hot path, and every operation is sandboxed to the active Project root.

## Workspaces and Projects

`Ctrl-A s` opens Manage Workspaces: type any part of a Workspace name to narrow the list instantly.
Use the arrows to select a match and `Enter` or a click to attach to it in place.
`Ctrl-G x` stops the selected Workspace.
`Ctrl-A $` renames the current Workspace in a prefilled input; the socket, snapshot, and status line follow.

`Ctrl-A P` opens Manage Projects for the active Workspace.
`Ctrl-A A` opens the New Project modal directly; once the folder is confirmed, every modal closes and you land in the new Project's first Tab, ready to work. `Ctrl-A b` toggles the left sidebar.
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

## Settings and themes

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
Each notice can also be heard: `notification-sound` is the terminal bell by default, `chime` plays a short two-tone clip that Uniterm synthesizes in code (no audio file to find or ship), and `file` plays your own audio file named by `notification-sound-file`.
The sound is chosen by the Workspace but played by the attached client, so a Workspace on a remote host chimes on the machine you are sitting at.
Attention always sounds; a completion stays quiet when the agent's Pane is the one you are already looking at in a focused terminal.

## Driving Uniterm from an AI agent

[`skills/manage-uniterm/SKILL.md`](../skills/manage-uniterm/SKILL.md) is a harness-neutral skill (the plain `SKILL.md` format read by Claude Code, Codex, OpenCode, Cursor, and similar tools) that teaches an agent to find and monitor other agents across Panes, Tabs, and Projects, read and send Pane text, copy between Panes, focus and organise the hierarchy, queue direction, and handle the waiting queue, all through `ut`.
Point your harness's skill discovery at the `skills/` directory, or copy the folder into its skills location.
`ut --skill` prints a shorter version for agents that cannot load skills.

