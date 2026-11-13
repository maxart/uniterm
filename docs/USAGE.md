# Using Uniterm

Everything you can do inside a Workspace and from the `ut` command line.
Install first with the [README](../README.md) quick start or [INSTALL.md](INSTALL.md); configuration lives in [CONFIGURATION.md](CONFIGURATION.md).

## Today and local history

Choose **Workspace > Today / Timeline**, press **prefix D**, or run `ut today`.
The left sidebar is reserved for Projects.
Today occupies the main area, hides Project tabs, and leaves the Observatory available.
The bordered navigation controls switch days or open a date entry; activity labels and bars use the same provider colors as Config and the Agents pane.
Select an activity row to inspect its Project, worktree, provider, session ID, and event reference.
Press Enter for a scrollable, wrapped view of the complete note or retained prompt.
Bars join recorded observations; they do not measure continuous work or meeting time.
Coverage is explicitly partial, especially for older logs and providers that do not report session IDs.

Today offers **Activity** (`1`, also `a`), **Flow** (`2`), and optional **Graph** (`3`).
Activity combines timeline observations and agent relationships, with Project names, paths, session IDs and recorded status.
Edges come from provider-reported parent session IDs, including children sharing a parent's Pane.
A `?` marks a parent missing from the selected day/page or invalid ancestry; a shared directory never implies parenthood.
Activity and Flow retain session inspection and copy actions; Graph inspects run and role evidence.
Children without their own recorded resume profile cannot resume the parent's session.
Still-open invocations and their observed descendants last seen before today appear as carried over, with their original observation timestamps; they are not counted as new activity or continuous work, and child liveness remains unknown.
The first page also carries known relationships omitted by a busy day's observation limit, so unrelated activity cannot silently hide them.
The JSON day response exposes these separately in `active_agents` and reports `active_agents_truncated` when capped.

**Flow** draws regular Pane agents as provider-colored cards with branching launch paths.
Claude, Codex, Grok, Gemini, and other providers keep the same colors as the Observatory.
Paths blend the parent and worker colors, and new observations trigger a short traveling pulse that stops after settling.
Cards represent agents running in Tabs, split Panes, or tab stacks; harness-internal sub-agents have no separate Flow cards.
Links use recorded Uniterm launch invocation identities, including across providers; grouping Tabs alone does not invent a delegation link.
Older history without launch evidence shows independent cards, and a missing parent is marked `?`.
Use the arrow keys or mouse wheel to select and scroll cards, click a card to select it, Enter to inspect, and `g` to follow its exact live invocation.
Animation stops when the view is hidden and does no idle work after an update drains.

Use `[` / `]` for adjacent local days, `t` for today, `d` for a YYYY-MM-DD date, `/` to filter Project/path/branch/provider/session, and `j` / `k` to select rows.
`o` pages older observations without changing the selected day.
The Project count covers the whole day; the rows are a bounded page.
The view refreshes on events and interaction, including a midnight rollover when following today, without a ticking clock.

`n` records a breadcrumb now, using the selected historical Project context.
`g` visits an existing matching Project or live invocation; stale Pane IDs never silently navigate to another agent.
`c` copies the reported session ID through the terminal clipboard.
`r` asks you to type `resume` before opening a new background Tab, and requires a recorded, trusted provider resume profile and an available, unchanged worktree.
`Esc` returns to your remembered terminal selection.

```sh
ut today list --json
ut today list 2026-09-22 --filter frontend --json
ut today list --before 500 --json  # exclusive next_before cursor
ut today note 3 'Next: inspect retry test'
ut today focus 12 401             # Pane id + invocation start event
ut today resume 450               # recorded observation, new background Tab
ut today watch --after 500        # NDJSON subscription; reconnect after last sequence
ut today manager codex 3 --background
```

`m` starts a provider manager in a background Tab after you enter its provider name.
Starting a manager explicitly shares Project paths, provider/session IDs, and activity metadata with the chosen provider, including a cloud provider when applicable.
Its inherited API endpoint is read only and excludes notes, prompts, terminal text, and launch arguments.
The endpoint rejects mutations on the server, and that scope follows native resume and crash recovery.
`ut today manager PROVIDER PROJECT_ID --manage` explicitly grants the normal Workspace API, including private content access; destructive and bulk confirmations still apply.
This is an API boundary, not an OS sandbox against an agent with the same user privileges.

## History privacy and recovery

History stays in the local Workspace event stream and survives clean stops and crashes.
As new events accumulate, Uniterm automatically checkpoints recovery state and archives the original events so startup need not replay the entire history.
Timeline and subscription queries still include archived events.
Archives continue using disk space; this maintenance does not delete old history.
The new timeline adds metadata and deliberate notes, not terminal keystroke or transcript capture.
Existing snapshots and event records can already contain sensitive content.
Files use private directory/file permissions; they are unencrypted unless you enable protection.

Protection is an explicit migration of a **stopped** Workspace and covers its event stream and immutable archives, snapshot, catalog, timeline indexes, and recovery copies.
Store the key outside the Uniterm state directory and keep a separate safe backup; losing it makes protected state unreadable.
For example, after choosing a key location and stopping the intended Workspace:

```sh
ut privacy keygen /path/outside-uniterm-state/work.key
ut privacy protect work /path/outside-uniterm-state/work.key
UNITERM_STATE_KEY_FILE=/path/outside-uniterm-state/work.key ut workspace new -d work
ut privacy status work
```

Supply `UNITERM_STATE_KEY_FILE` when starting the server, including after a machine restart.
A missing or incorrect key locks startup; Uniterm does not interpret ciphertext as corruption or fall back to plaintext.
An interrupted migration must be retried with the same key before startup.
Protection uses authenticated XChaCha20-Poly1305 through RustCrypto; it does not protect an unlocked process, provider-side copies, or against another process with equivalent filesystem access.
Workspace names and file sizes remain visible.

Optional prompt retention is explicit **for each submission**, rather than a persistent Project switch that could capture a later sensitive prompt unexpectedly:

```sh
ut today prompt 12 --stdin --retain < prompt.txt
```

This sends a semantic prompt to a live agent and records it only in a healthy, unlocked, protected Workspace.
The limit is 4096 UTF-8 bytes.
Ordinary typing and `ut agent prompt` do not opt into timeline prompt retention.
The inspector identifies missing content as not captured; read-only managers cannot read retained prompts.
An accepted mutation is queued; a subsequent successful timeline query through its event sequence observes the synced note/prompt.
Durability failures appear in the view and reject further explicit retention.

History is retained until explicit Workspace deletion; there is no automatic expiration or silent pruning of recovery evidence.
`ut workspace forget NAME` removes a stopped Workspace's logs and archives, snapshot, catalog, local recovery copies, and derived timeline cache; it retains your external key.
Copies in backups, filesystem snapshots, exports, and provider storage are outside that deletion, and deletion is not a claim of physical secure erasure.
`ut today list --json > chosen-file.json` is an explicit plaintext export of one page, including any retained prompts/notes visible to that caller; follow `next_before` for earlier pages and protect exported files yourself.

## The command line

```sh
ut                                  # attach to or create the "default" Workspace
ut work                             # attach to or create the "work" Workspace
ut att work                         # attach to an existing Workspace (alias for attach)
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

`ut attach [NAME]`, `ut att [NAME]`, and `ut a [NAME]` are equivalent.
Omitting `NAME` selects the configured default Workspace; unlike a bare `ut NAME`, these commands do not create a missing Workspace.

## Updating Uniterm

Use `ut update --check` to check the latest public release, `ut update` to install, or `ut update --version v1.2.1` to choose a stable version.
About > Update and Settings > Update Uniterm offer the same check with an install confirmation, then return to the Workspace.
Updates verify both binaries and preserve running Workspaces; existing servers use their old executable until you restart them.
See [installation and updates](INSTALL.md#update-an-existing-installation).

## Monitoring an agent

`ut agent watch PANE` prints a current snapshot followed by NDJSON events from the Workspace event log.
Each line carries its event sequence, session identity, current status, last notification, pending permission, loop state, and waiting id when available.
The stream survives an agent restart in the same Pane; a new invocation or session clears old details.
`ut agent explain PANE --json` and `ut agent list --json` expose the same current details.

```sh
ut agent watch 9
ut agent watch 9 --after 1234
ut agent wait 9 permission --timeout 120
ut agent wait 9 --event loop-stopped --timeout 3600
ut agent wait 9 --event notification --after 1234 --timeout 120
ut waiting answer 17 yes
```

A notification is an event, not a permission request or proof that the task finished.
Idle includes pauses between loop iterations.
A `loop-stopped` event means the provider reported a successful explicit loop-stop tool call.
Event waits observe new events; `--after` also replays events following that cursor.
Reconnect silently rebuilds context from the retained log before emitting events after the cursor, with memory bounded to the watched Pane.
Only answer a current waiting id after reviewing the permission detail; `waiting answer` remains guarded terminal input.
Upgrade the Claude connector with `ut agent connector install claude` and start a new Claude session to load the updated hooks.
Claude reports explicit permission requests, questions, failed turns, and tool completion, while child-agent lifecycle hooks cannot mark the parent busy or idle.
Gemini reports typed tool-permission notifications, Grok distinguishes permission notifications from idle reminders and task completion, and Kiro installs both its V2 profile hooks and V3 global hooks.
Claude and Cursor hooks stay silent when Grok imports them; Grok uses its own connector to preserve provider identity.
Connectors install and update automatically by default at startup and after upgrading Uniterm.
Use `ut agent connector install all` for an immediate manual refresh, then restart the affected agent sessions.
OpenCode registers `./uniterm-notify` in its effective global `plugin` list, preserving user entries, and installs a separate local v2 TUI hook.
Ordinary setup supports both majors without a CLI version probe; immutable configs retain version-specific setup, refreshed when the config or executable changes.
OpenCode 2 status comes from the local TUI's selected session and pending prompts, including blocked children, because its shared server does not own the Pane terminal.

Today session identity comes from provider metadata, never from a guessed working-directory match.
Refresh an existing integration with `ut agent connector install AGENT`, then start or resume the harness to load its updated hooks.
Hooks normally report through the Pane's terminal; detached hooks without a controlling terminal use the inherited Workspace socket instead.
The socket fallback checks the reporting process's ancestry and the current Pane invocation, uses a bounded deadline, and cannot write through a read-only manager endpoint.
Codex runs command hooks without a controlling terminal, so its session reporting requires a running Uniterm server with the `agent.hook.report` capability as well as the updated helper binary.
Codex also requires its non-managed hooks to be trusted in `/hooks`; installing a connector does not bypass that review.
Codex permission hooks run before native automatic review, so they preserve identity without claiming a human wait; its visible approval prompt supplies the permission status.
The Claude, Codex, Gemini, Cursor, Grok, and Kiro helpers preserve session IDs when their hook payload reports one; Pi and OpenCode extensions report their native identities too.
Codex also discovers explicit parent/child relationships from native transcript metadata after the owning session is identified, including children that share the same Pane.
Linux can recover Codex, Pi, and OMP identity from an exact native transcript writer without hooks.
Providers without an exposed identity remain explicitly unidentified; Uniterm never assigns another session merely because it shares a Project.

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
Pressing the prefix opens a floating Quick actions window immediately.
Existing prefix shortcuts still run directly; press Space or `/` to enter fuzzy search instead, then type part of an action or Project name, select with Up/Down or Tab/Shift-Tab, and press Enter.
Project results are labeled `Go to project: <name>` and switch directly to that Project in the current Workspace.
Ctrl-N/Ctrl-P and the mouse wheel also move the selection; click an action to run it.
Escape or Ctrl-C dismisses the window, and repeating the prefix sends its literal byte to the Pane.
Configured bindings take precedence over the search keys, and the outer picker in a nested Uniterm uses the doubled prefix.
The picker shares commands and close confirmations with the existing menus, and does no work while idle.
About > Docs opens <https://uniterm.dev/docs>.

Shift+Enter is preserved as a distinct key for TUIs, including Codex, that enable Kitty keyboard disambiguation.
The outer terminal must support that protocol and deliver the modified key; Uniterm cannot recover Shift once a terminal has encoded both Enter and Shift+Enter as the same carriage return.
Uniterm requests disambiguation while attached, restores the host keyboard mode on detach, and keeps separate bounded negotiation stacks for each Pane's main and alternate screens.
Legacy shells and Uniterm menus receive their usual control keys, including Ctrl-C and Escape.
Only the disambiguation flag is supported; key-release events, alternate-key reports, and reporting all keys as escape sequences are not advertised.
The TUI decides what Shift+Enter does; Uniterm preserves the modifier instead of assigning a universal newline binding.

| Keys | Action |
| --- | --- |
| `Ctrl-A` then `d` | detach (the Workspace keeps running) |
| `Ctrl-A` then `%` | split left/right |
| `Ctrl-A` then `"` | split top/bottom |
| `Ctrl-A` then `h` `j` `k` `l` | move focus between panes |
| `Ctrl-A` then `Shift-H/J/K/L` | resize the focused pane |
| `Ctrl-A` then `z` | zoom in on the focused pane (toggle); directional focus to another Pane restores the split view |
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
The window autocompletes as you type: an empty input lists every slash command with what it does, `/workflow ` suggests the bundled templates (`solo`, `pair`, `triad`) with their role lineups, `/project ` suggests actual Projects in this Workspace, and an `@` word suggests the installed providers.
Arrows move the selection, `Tab` completes.

Press `Ctrl-O` for launch options: Project, agent, single-agent/workflow/relay/save mode, pane/tab placement, and per-role provider choices.
Use up/down or Tab to select an option and left/right to change it; type into supported model/profile fields.
Enter or Escape returns to the prompt, and Enter there submits.
The displayed directory is the actual launch root, including registered worktree Projects.
Workflow and relay launches create their own Tab with role Panes; model/profile overrides are currently single-agent options, and unset options preserve the agent's native defaults.
OpenCode uses the prompt flag shared by versions 1 and 2; model/agent TUI flags removed in OpenCode 2 are deliberately not offered.
Unavailable agents and invalid Projects leave the draft open with an error and create no Task or Pane.

`Ctrl-J` inserts a newline; bracketed paste preserves multiline text without submitting it.
When options are open, paste targets Model or Profile / agent and ignores noneditable choices; line breaks remain exclusive to the prompt.
Whitespace and file mentions such as `@README.md` remain prompt text.
Use `-- ` before literal text that looks like Uniterm launch syntax, for example `-- /workflow is a directory name`.
Opening options separates recognized inline choices from the prompt, so subsequent edits are literal.
Quote Project names containing spaces, for example `/project "Project B" fix the tests`.

Tasks updates while open from Workspace events, including changes made by other clients and completed runs.
Use `f` for all/active/planned/completed, `p` for workspace/project scope, `r` for status/recency order, and `/` to search titles, notes, prompts, and saved provider names.
The list loads 32 summaries at a time; moving beyond either end loads the adjacent page, and filtering searches the entire retained history.
Selected details load on demand, preserving full notes and prompts for editing and launch drafts.
A native provider restart keeps the Task attached to its resumed invocation; unrelated reuse of the same Pane does not capture old Tasks.
Enter goes to the selected Task's waiting request or active agent Pane.
`l` opens a launch draft from the selected Task; launching creates a new attempt linked to its source and preserves earlier outcomes.
`e` edits the title, `n` edits notes, Space changes manual progress, and `x` twice deletes the Task record without stopping its agents or run.
`a` answers an existing waiting request and `u` requests resume through the same waiting controls as the Observatory; the server validates whether each action is applicable.
Use Page Up/Page Down to read longer execution details, outcomes, and artifact summaries.
Manual progress, native run outcome, and live agent activity are separate facts: idle never automatically completes a Task.

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
- Drag a Tab onto another Tab to reorder it within its Project, or use the Tab menu's Move tab left/right actions and `Ctrl-A <` / `Ctrl-A >`.
  Reordering keeps the selected Tab and Pane and persists the new order.
  Agents can use `ut tab move PROJECT TAB left|right --background` to reorder another Project without selecting it; targeted moves also preserve focus when the flag is omitted.
- Press on a divider and drag: the divider follows the pointer and the new split ratio persists with the layout.
- Drag the right edge of the Project sidebar or the left edge of the Observatory/Files/Web/Overwatch panel to resize it.
  The panel follows the pointer across content boundaries and saves its width when you release; the same widths appear in Settings.
  Project widths range from 16 to 40 cells and right-panel widths from 22 to 52, with responsive limits that leave room for the central panes and Today.
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
Agents and Servers default to the whole active Workspace; use each scope control to switch to the active Project.
Saved scope selections are restored when reopening a Workspace.
The Agents tab groups live agents by Project and start time; click a card to jump to its exact Pane.
Right-click an agent for `Go to agent pane`, `Copy working path`, or `Stop agent`.
Stopping an agent closes its Pane, just like `ut agent stop`; a menu for an invocation that has ended cannot stop its replacement.
The Web servers tab lists event-driven, loopback-verified server detections and opens a server's URL on click.
Right-click a server for `Open in browser`, `Go to source pane`, or `Kill server`.
Source navigation focuses its exact Project, Tab, and Pane; killing targets the selected port's listeners in that Pane's process session and preserves other servers and the shell.
Uniterm reports a stop error if it cannot verify ownership or confirm that the port closed.
Each Observatory tab keeps an independent vertical scroll position.

## The file manager

Click **By name** in the FILES header to switch to **By recency**, with the newest modification times first.
Both orders keep folders above files; name order is the default, and the Workspace remembers your choice across restarts.
The equivalent command is `ut files sort name|recency [--workspace NAME]`; custom bindings can use `files-by-name` and `files-by-recency`.

`Ctrl-A f` reveals the Observatory's File manager tab and gives it keyboard focus.
It uses a Midnight Commander-style tree and reserves terminal geometry instead of painting over child applications.
Use arrows or `h j k l` to browse, `Enter` to expand a folder or open a file in its configured editor, `n` for a file, `N` for a folder, `R` to rename, `d` twice to delete, `y` to copy the absolute path, `Y` to copy the Project-relative path, `.` to show hidden entries, and `r` to refresh.
A single click expands a folder or opens a file, including a row that was not previously selected.
Opening a file again focuses its existing terminal editor Pane in the same Project, including when reached through a symlink.
After the editor exits or its Pane closes, opening the file starts a fresh editor Tab.
External editor launchers such as `code` retain their own file and window behavior and never create a Uniterm Tab.
Git summary discovery runs independently so that slow repositories do not delay file opening.
File watches ignore read notifications so directory listings do not continually refresh themselves.
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
Right-click a Project in the left sidebar and choose **Change Directory...** to edit its root with the existing folder-completion modal.
The modal starts with the current directory and keeps host validation errors editable.
`ut project directory <project> <absolute-path|~/path> [-w workspace]` performs the same operation by name or id.
The host validates and canonicalizes the directory; remote paths belong to the Workspace host.
New Tabs, splits, and agent launches use the new root, while running Panes retain their working directories and the selected Project stays selected.
Registered worktree roots remain Git-managed, and active workflows or relays must finish or stop before their Project root can change.

The CLI exposes the same model through `ut project list|add|switch|rename|directory|move|remove|metadata`, plus `ut pane list [--json]`, `ut agent list [--json]`, `ut tab focus <project> <tab>`, `ut tab new [project]`, `ut tab rename <project> <tab> <name>`, and both stable-id and hierarchy forms of `ut pane focus`.
Project selectors accept a stable id or case-insensitive name, Tab selectors accept a 1-based ordinal or case-insensitive name, and hierarchy Pane selectors use the 1-based ordinal reported by `ut pane list --json`.

The left sidebar is the persistent Project overview for this hierarchy.
It begins directly with Projects, uses the host terminal's default foreground and background, and gives every Project two rows of context.
It displays Project attention counts, supports clicks and vertical scrolling, uses a compact width on medium terminals, and disappears below the safe content threshold.
Project roots are abbreviated like a shell prompt, with `~` for home and compact parent folders.
Agents keep their provider colours in the right Observatory rail, where they stay grouped by Project and start time, can be scoped to the active Project or whole Workspace, and show the Git branch when their Project is a worktree.
Manage Workspaces and Manage Projects lay their rows out in aligned columns (name, state, counts) and Manage Workspaces probes sibling servers in parallel, so it opens in one round trip.

## Settings and themes

`Ctrl-A g` opens Settings: a rail of setting names grouped under Appearance, Agents, Behaviour, Editors, Notifications, Guardrails, and Tools, and a pane with the selected setting's help text and a control that fits it (an on/off switch, a stepper that shows its range, a boxed editor, an option list, or a button).
Theme opens a dedicated browser with a visible search field, readable names and source labels, a scrollable list with a position count and clickable scrollbar, and a separate color preview.
Type to filter by name or collection; use the mouse wheel, arrows, Tab/Shift-Tab, Page Up/Down, or Home/End to browse.
Clicking a theme previews it; Enter or the Apply button saves it, and Escape returns to Settings without applying.
Search and preview stay separate from the list viewport, so short terminals can browse the entire catalog.
The currently applied theme is marked with an asterisk and is scrolled into view when the browser opens.
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
Per-pane approval uses `ut delegate arm PANE` and the existing Auto-approve pipeline, with a captured hourly budget.
`ut delegate disarm PANE` excludes that invocation from automatic approval, even when Workspace Auto-approve is on.
Selecting Auto-approve **off** clears every Pane arm; Pane delegation always resets on restart.
Workspace Auto-approve also resets unless Keep auto-approve on restart is explicitly enabled.
Use `ut delegate status --json` and `ut delegate explain PANE` to inspect scope and withheld requests.
Each notice can also be heard: `notification-sound` is the terminal bell by default, `chime` plays a short two-tone clip that Uniterm synthesizes in code (no audio file to find or ship), and `file` plays your own audio file named by `notification-sound-file`.
The sound is chosen by the Workspace but played by the attached client, so a Workspace on a remote host chimes on the machine you are sitting at.
Attention always sounds; a completion stays quiet when the agent's Pane is the one you are already looking at in a focused terminal.

## Connector overview and automatic permissions

Open **Workspace > Settings > Connectors** under Agents, **Agents > Config...**, or the Agents rail's **Config** button to see connector status for each provider.
The Settings page includes an **Install all connectors** button.
Press `I` or click **install all** to install or refresh connectors for installed CLIs.
The existing **Global files** tab still previews and edits agent configuration files.

Select **Auto-update connectors** or **Auto-install connectors** in either Workspace Settings or Agents Config.
Each page has its own **on / off** selector, separated from its description and config details.
Both machine-wide preferences default to **on**; explicit `false` settings remain respected independently.
The equivalent settings in `$XDG_CONFIG_HOME/uniterm/uniterm.conf` (normally `~/.config/uniterm/uniterm.conf`) is:

```ini
auto-update-connectors = true
auto-install-connectors = true
```

After `ut update`, the new binary installs missing connectors and refreshes outdated connectors once, according to the two preferences.
Starting or attaching Uniterm also checks once, covering package-manager and manual binary replacements.
Current connectors are not rewritten, and missing agent CLIs are skipped.
Turn off **Auto-install connectors** if you want a manually removed connector to stay removed; auto-update alone does not reinstall it.
Only Uniterm-managed hooks are replaced, and failures are reported and retried at the next startup.
There is no background polling, and already-running agents load changed hooks when they next start.
`ut agent connector auto-update` runs the same check and honors both saved preferences.
**Install all connectors** remains available for an immediate manual install or refresh.

**Settings > Agents > Auto-trust Project directories** defaults ON (`auto-trust-projects = true`).
It accepts complete recognized trust dialogs only when the displayed absolute directory exactly matches the owning Project root.
Missing, ambiguous, parent, unrelated, truncated or differently spelled paths stay manual.
Every attached client shares this independent setting; disabling it prevents automatic directory trust even when Auto-approve is ON.
Ordinary permission automation is off by default.
Select **Auto-approve** in either Settings or Agents Config.
The page places an explicit **on / off** selector above a separate explanation.
Select **on** to enable it immediately for this Workspace; select **off** to disable it.
Neither direction asks for confirmation.
The Config shortcut `A` and the explicit command `ut agent auto-approve on -w NAME` remain available.
Use `ut agent auto-approve off` to disable it and `ut agent auto-approve status` to inspect it.
It resets when the server restarts by default; the rail button always remains **Config**.
To retain your choice, enable **Workspace > Settings > Agents > Keep auto-approve on restart** (`persist-auto-approve = true`).
This preference alone never enables Auto-approve.
Once explicitly ON, it stays ON across clean restarts and crashes until turned OFF; OFF is also saved per Workspace.
Turning the preference off leaves the live mode unchanged and resets it OFF at the next restart.
Recognized choices are accepted on live output, without waiting for status notification dwell.
Project trust follows its own setting; destructive command requests and other tool permissions still require explicit permission automation or a reviewed waiting action.
Questions and unknown dialogs remain waiting.
`ut waiting approve ID` uses the same recognition path for one reviewed permission item.
See [permission automation](28-permission-automation.md) for dialog coverage, deduplication, audit, and limitations.

## Driving Uniterm from an AI agent

[`skills/manage-uniterm/SKILL.md`](../skills/manage-uniterm/SKILL.md) is a harness-neutral skill (the plain `SKILL.md` format read by Claude Code, Codex, OpenCode, Cursor, and similar tools) that teaches an agent to find and monitor other agents across Panes, Tabs, and Projects, read and send Pane text, copy between Panes, focus and organise the hierarchy, queue direction, and handle the waiting queue, all through `ut`.
Point your harness's skill discovery at the `skills/` directory, or copy the folder into its skills location.
`ut --skill` prints a shorter version for agents that cannot load skills.

## Background automation

Agents and scripts can manage Projects, Tabs, and Panes without changing the attached human's selection.
Use `--background` with `ut project add`, `ut tab new`, `ut pane split`, and `ut agent start`; anchor an agent launch with `--pane PANE`.
`ut pane close PANE` and `ut tab close PROJECT TAB` target resources directly.
Tabs expand to fill their strip, shrink equally as more Tabs are added, and scroll after reaching eight cells each.

Use `ut agent prompt PANE TEXT` to paste and submit, `ut pane paste PANE TEXT` to insert only, and `ut pane submit PANE` to press Enter on text already present.
`ut tab prompt PROJECT TAB TEXT` targets that Tab's remembered active Pane without switching to it.
Replace TEXT with `--stdin` to read a multiline prompt from a file.
Run `ut --skill` or `ut help prompt` for targeting, readiness, submission, and completion guidance.

### Return from Files to a terminal

Press Escape while navigating Files to return keyboard input to the active Pane and keep the sidebar open.
Clicking a terminal Pane or Tab also returns keyboard input immediately, even when it is already active.
Pane and Tab navigation shortcuts also return keyboard input to the terminal, including directional focus, last Pane, splits, zoom, and Tab switches.
Escape inside a Files create or rename prompt cancels that prompt first; press Escape again to leave Files navigation.

## Pane layouts and tab stacks (sub-tabs)

Choose **Split layout**, **Dwindle layout**, or **Scrolling layout** from a Pane's context menu.
A separator groups these choices, and a dot marks the active layout.
Dwindle recursively divides the remaining space along its longer visual axis.
Scrolling shows full-height columns on a horizontal tape, with the focused Pane kept visible; narrow terminals show one column.
A flat, borderless bottom scrollbar inside the main content shows the visible range.
Click the empty track, drag its thumb, or use the wheel over it to scroll; focus moves only when needed to keep the active Pane visible.
Zoom hides the scrollbar until the columns are visible again.
The manual split tree is retained when changing display modes.
Directional Pane focus moves through the tape; divider resizing applies to Split layout.
Focus on hover applies to every layout and updates all attached clients immediately when changed in Settings.
Use `ut pane layout PANE split|dwindle|scrolling` for the same operation from automation.

The Tabs menu offers **New child tab** and **Move out of stack**.
Parent Tabs stay on the main strip; the selected stack shows its root and child Tabs on a second strip.
Both strips support wheel scrolling and drag reordering.
Every stack member, including the original Tab, keeps its plain label in the secondary row.
The row remains when only that original Tab is left, including after restart, and disappears when the final member closes or explicitly moves out of the stack.
Closing the original member first transfers the group to a surviving member.
The child strip occupies only the main content area; the left Project rail and right Agents, Files, and Servers columns keep their full height.
Tab stacks are also called sub-tabs, subtabs, child tabs, or nested tabs.
Prefer stacks for additional delegated agents so related workers stay with their parent.
Use `ut agent start NAME --pane PARENT --stack --background` to start an agent beneath another Tab without changing the human's selection.
`ut pane stack CHILD PARENT|none` groups or ungroups an existing Tab, scoped to one Project and rejecting ancestry cycles.
A parent's last Pane closing promotes its children to the next ancestor, or keeps them grouped beneath a surviving member.
When one Tab remains, the sub-tab row disappears and the survivor is a regular top-level Tab.
This also applies to old checkpoints that retained a singleton stack row.
Layout modes and ancestry persist through checkpoints and clean restarts.

## Agent setup and global files

In Agents config, **I** installs Uniterm integration connectors for all installed, supported agent CLIs.
It preserves other settings, skips missing CLIs, and reports installation errors.
It does not download agent programs.
The equivalent CLI action is `ut agent connector install all`.
Cursor accepts `cursor` or `cursor-agent` when launching.
The shared `agent` alias follows its installed executable target, including Grok's `agent` symlink; it is not assumed to belong to Cursor.

The **Providers** and **Global files** tabs use the same active colors and centered layout as the main tab bar.
Press **Tab** or **Shift-Tab** to switch tabs, or click anywhere in a tab.
Select **Global files** to preview global instructions and configuration.
The list includes Claude's user `CLAUDE.md` and `settings.json`, its OS-wide `CLAUDE.md`, and Codex's `AGENTS.md`, `AGENTS.override.md`, and `config.toml`.
User paths honor `CLAUDE_CONFIG_DIR` and `CODEX_HOME`.
Previews load only when requested and are limited to 64 KiB; **e** opens the configured editor, creating a missing file first without overwriting existing content.
System files use normal OS permissions; an inaccessible file displays the error.

Server cards offer **Copy server address** in their right-click menu.
Agent tool-completion and ready events also trigger descendant listener discovery, so redirected server output can still produce a card.
Linux/Android use descendant process file descriptors; macOS uses libproc child discovery and a scoped `lsof` query.
Discovery requires a live descendant and an HTTP response on loopback; fully detached services whose parent has exited and HTTPS-only listeners still need a recognized terminal announcement.
The Project context menu offers **Move to top** and **Move to bottom**, also available as `ut project move PROJECT top|bottom`.

Today loads at most 128 entry payloads per page, using a separate lightweight index for filtering and whole-day Project summaries.
Older payloads remain on disk until their page is requested; existing caches rebuild once for the new index format.
The full monitoring skill is bundled as `ut --skill monitoring`.

## Retrying an unavailable restored Pane

If a saved directory or executable is unavailable, the Pane stays in its original layout and displays a recovery diagnostic.
Its saved output remains available in copy mode, and no replacement command runs in another directory.
Restore the directory or executable, then press Enter in that Pane or run `ut pane submit PANE` to retry.
Other input is rejected while recovery is unavailable, so typed commands cannot unexpectedly run after a retry succeeds.
Stopping the Workspace preserves unavailable Panes for the next start; other Panes still restart as fresh shells.

## Project agent inbox

`ut inbox discover` introduces the Project message board, and `ut inbox help` lists its commands.
Use `ut inbox list --json` to inspect plans, proposals, critiques, feedback, questions, findings and handoffs without scraping agent terminals.
Inside a recognized agent Pane, the CLI derives its current invocation and Project; human administration inside a Pane uses `--human` explicitly.
Post with `ut inbox post --type proposal --key design-v1 --stdin < proposal.txt` and reply with `--reply-to MESSAGE --epoch EPOCH`.
Claims use `ut inbox claim MESSAGE --revision REVISION --epoch EPOCH`; use `ut inbox watch --after CURSOR --epoch EPOCH --timeout 300 --json` for event-driven waiting.
The epoch and cursor come from discovery/list responses.
Human controls are `pause`, `resume`, `revoke PANE INVOCATION`, and `clear --confirm`, with explicit `--project ID` and an epoch for revoke/clear.
Bodies expire within 24 hours and disappear on server restart or Workspace rename; keep lasting decisions in Tasks, artifacts or project documentation.
Read [the inbox contract](29-project-inbox-proposal.md) for scope, budgets, discovery coverage and privacy limits, and `ut --skill monitoring` for the complete collaboration workflow.

## Graph

Today combines timeline and agent history in Activity, with Project names and paths.
Use its tabs or keys `1` (Activity), `2` (Flow) and `3` (Graph, when enabled).

Enable **Graph** in Settings, then open **Today > Graph**, press prefix-G, or run `ut run graph`.
Graph opens on ordinary agents, including native descendants and explicit launch relationships, without workflow or relay setup.
Agent rows show known work, Project name and path, provider and observed status; unreported work and unknown child liveness remain labeled.
Use arrows or click a row, `v` for source observations, and Enter to focus only the matching live invocation and session.
Select **Runs / evidence** or press `n` to inspect run and role relationships, attempts, review outcomes and artifact evidence.
Enter returns to a selected live role only if it still owns that Pane.
In details, `r` refreshes historical evidence and Escape returns to the graph; Escape from the graph restores the terminal.
The feature defaults off and remains idle when the Workspace does not change.

## Saved SSH Servers

Use the **Local / Servers** controls at the bottom of the Project sidebar, **Workspace > Servers**, or `ut servers` to open saved SSH connections.
In Servers, `a` adds a profile, `n` edits it, `e` enables/disables it, `r` discovers remote Workspaces, `Tab` selects a discovered Workspace, and `Enter` connects.
Press `l` or choose Local in the sidebar to return to your original local Workspace.
A disconnected remote connection returns to Local with its diagnostic; Enter reconnects explicitly.
Remote sessions and agents keep running when you switch away.

`ut machine add "Build" user@build agents` saves a target and default Workspace.
`ut machine list --json`, `ut machine status Build --json`, and `ut machine sessions Build --json` support automation.
The remaining verbs are `rename ID LABEL`, `enable ID`, `disable ID`, `remove ID`, `connect ID`, and `reconnect ID`.
Discovery is explicit and bounded; the dialog labels its results as snapshots.
See [Saved SSH Servers](32-saved-servers.md) for storage, SSH trust, recovery, and scope.

## Automatic Tab names and Find Tabs and Agents

Both features are independent and default off under Settings > Behaviour.
**Auto-name tabs** follows an agent's invocation-scoped summary, explicit `summary`/`task`/`title` Pane metadata, or an observed provider task title.
Activity spinners, generic provider labels, and inherited shell titles are excluded.
Without a task summary, an agent Tab keeps its numbered fallback.
Regular terminals use their cooperatively reported current directory and observed foreground command.
Accepted shell submissions trigger one bounded foreground observation, so silent commands can be named without periodic process polling.
Custom names always win, survive restart, and can be cleared to resume automatic naming.

Enable **Find Tabs and Agents**, then use Workspace > Find Tabs and Agents, prefix-S, or `ut switcher`.
Type to fuzzy-match Project and Tab names, Pane ordinals, provider, status, directory, or cached branch.
Use Up/Down or Tab to select and Enter to focus the exact Pane; Escape closes the surface.
The preview shows bounded recent terminal output, without a transcript copy or model request.
Updates preserve the typed query and stable Pane selection, and stop when the surface closes.
The projection is bounded to 512 live Panes in the current Workspace.

### Workspace Overwatch

Open Settings > Agents > Overwatch to see its status and open its controls.
Workspace > Overwatch and prefix-O open the same controls.
Choose a provider while OFF, then turn ON and open its native chat in the right panel's fourth tab, Overwatch.
The assistant belongs to the Workspace and does not create a Project Tab.
The chosen provider receives Workspace metadata; management and private content access are an explicit separate grant in the Access row.
Access can change while ON, restarting chat with the new endpoint.
The controls separate ON/OFF and Open chat, current provider/model/effort/recovery/access settings, and alternative providers with divider lines.
Questions about current agents, earlier Project visits and today's work use paged structured evidence from the owning Workspace.
The assistant has bounded exit recovery and an explicit retry action; authentication prompts remain visible in chat.
`ut overwatch status --json` reports its lifecycle, and `ut overwatch off` stops its owned invocation.

With management enabled, requests to remove Projects not worked on today use `ut overwatch prune --preview --json` and then an exact confirmed apply with the returned sequence, local date and candidate IDs.
The operation removes Workspace entries and closes their Panes, without deleting directories.
Incomplete evidence, a changed preview or uncertain live work prevents removal.
See the [Overwatch contract](31-workspace-navigation-and-overwatch.md) for commands, recovery and privacy boundaries.

### Tasks from direct agent prompts

Prompting an agent directly can now populate Tasks without using New Task.
**Settings > Agents > Auto-track tasks** controls this behavior and defaults ON.
Open Tasks to inspect the submitted work, follow-ups, status and owning agent; edit titles, notes and statuses with the existing controls.
Follow-ups share unfinished work, while the next prompt after an explicit response completion starts a new Task.
Finished means the provider ended its response, not that Uniterm verified the requested outcome.
Discovery may provide only a work label when prompt capture was unavailable.
Overwatch can see the Task metadata and, with management enabled, use the existing Task operations.

Overwatch's settings let you change its harness/provider and model while OFF or ON; leaving the model blank uses the provider default.
A changed selection while ON restarts the assistant with a new chat and interrupts the current response, while preserving Workspace history, Project focus and access permissions.
Selecting the same value does nothing; switching harnesses resets the model to that harness's default.
Management access still changes only while OFF.

Overwatch settings include searchable native **Model** and **Effort** pickers.
Choose the harness, select a friendly model name, then select its advertised effort or leave the harness default.
Both choices apply together; changing them while ON starts one fresh chat and interrupts its current response.
Custom model identifiers remain available as an advanced fallback, and cached or unavailable catalogs are labeled.
See [the model selection contracts](31-workspace-navigation-and-overwatch.md) for provider details.
