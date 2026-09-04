# 10 - Config, Commands, and Keybindings

This document covers the surfaces a user touches to configure and drive the tool: the config files, the command language, keybindings, and quick task capture.

A guiding principle from the prior-art study: tmux's biggest usability wart is its shell-like, non-POSIX config language, so we separate declarative configuration (structured, validated) from the runtime command language (for scripting and interactive commands).

## Configuration: one file, one schema

### Ghostty-style config, kept from Uniterm

Terminal appearance lives in a Ghostty-compatible key-value file, because it is already a clean plain-text format, it is what Uniterm used, and users may already have a Ghostty config to adopt.

On first launch, if `~/.config/ghostty/config` exists, offer to adopt it.

The implemented schema includes `theme`, `prefix`, `status`, `status-position`, `sidebar`, `sidebar-width`, `file-sidebar`, `file-sidebar-width`, `editor`, `editor.EXTENSION`, `notification-delivery`, `notify-completion`, `notification-sound`, `notification-sound-file`, `focus-follows-mouse`, `confirm-close`, `confirm-tab-close`, `scrollback-limit`, `restore`, and the CLI-owned `default-workspace`, plus semantic status color overrides.
`confirm-tab-close` defaults to on and can be disabled independently to close Tabs without opening the confirmation modal.
`ut workspace default NAME` atomically updates `default-workspace`, and every CLI command that omits a Workspace name resolves that preference before falling back to the literal `default` name.
The status and Tab bar defaults to the top, and its Workspace/Project segment matches the expanded sidebar width.
The sidebar itself uses terminal-default foreground and background colours; themes affect the top bar, Tabs, structural borders, and client-side surfaces.
The optional file sidebar is off by default, toggles with `prefix + f`, and watches only directories the user expands.
Its keyboard and right-click actions can copy either the absolute path or the path relative to the active Project root.
Opening a file uses `editor.EXTENSION` when one matches and otherwise uses the catch-all `editor` command, which defaults to `vi`.
The Settings surface accepts file-type mappings as a semicolon-separated list such as `md=glow; rs=nvim`; commands may contain arguments, and the tokio runtime verifies that each executable resolves on `PATH` before saving or launching it.
For a Git Project, its header shows compact colored `+N`, `-N`, and `?N` totals for inserted lines, deleted lines, and untracked files against `HEAD`.
The Git runtime canonicalizes Project paths to repository roots, shares one recursive watcher and cached summary per repository, and recomputes only after a debounced filesystem or Git-metadata event while the file sidebar is visible.
Filesystem event bursts keep one debounce worker per repository, and untracked paths are counted as a stream rather than buffered in memory.
If an OS watcher cannot be installed, the initial Git summary and the file manager remain available without live refresh.
If a later Git subprocess fails transiently, the watcher retains its last known good summary instead of projecting the failure as zero changes.
One directory listing retains at most 10,000 immediate children and stays browsable with a truncation notice instead of risking an out-of-memory exit.
Agent attention delivery can be `off`, `uniterm`, `terminal`, or `system`; completion notices are an independent opt-in.

Themes use semantic roles shared by server chrome and client surfaces: background, surface, foreground, muted, accent, success, warning, error, attention, selection, and active/inactive borders.
The bundled set includes Uniterm dark and light, Catppuccin, Tokyo Night, Dracula, Nord, Gruvbox dark and light, Solarized dark and light, Kanagawa, and Rose Pine.

The Settings modal is generated from the same typed values the server applies.
Its rail lists setting names under Appearance, Behaviour, Editors, Notifications, Guardrails, and Tools; the pane beside it shows the selected setting's help text and a control that fits its kind (a switch, a stepper with its range, a boxed editor, a button, or an option list).
Theme is a searchable list: moving through it previews the highlighted theme's palette, status bar, and Project row, and Enter applies it.
Changes take effect immediately, then the tokio runtime merges the Settings-owned keys into the existing file, writes a temporary file, and atomically renames it over the live file.
Comments, ordering, and advanced keys that are not owned by Settings remain intact.
The mio renderer never performs settings-file I/O.

## The command language

A runtime command language exists for scripting, for keybindings to invoke, and for interactive use through a command prompt.
It is the tmux muscle-memory surface, but it is not the persisted config.

- Commands are typed entries that resolve their target (Workspace, Project, Tab, Pane) from flags, following tmux's `cmd_find_state` model: `-t` selects a target.
- The historical tmux spellings remain compatibility aliases for scripts, while product UI and new CLI verbs use Workspace, Project, Tab, and Pane.
- A command prompt (invoked by a key) runs a single command interactively.
- Commands are also how the control protocol's structured API is exposed to humans, so a script and an interactive user drive the same verbs.
- Format strings (`#{session_name}`, `#{pane_pid}`, `#{agent_status}`, and so on) are available in status lines and in command output, including conditionals, because they are genuinely useful; they are not the config language.

The design intent: someone who knows tmux can drive this from day one, but nobody has to hand-write a config in a command dialect.

## Keybindings

Keybindings follow the key-table and prefix model from tmux, made rebindable through the structured config.

- Key tables: `root` (no prefix), `prefix` (after the prefix key), `copy-mode`, `copy-mode-vi`, and mode-specific tables.
- A configurable prefix key (default `C-a`) enters the `prefix` table.
- Bindings map a key in a table to a command (or a command list).
- Defaults cover the full multiplexer verb set (new window, split, navigate, resize, zoom, copy-mode, detach) plus the agentic verbs (open the waiting queue, jump to next item needing attention, toggle the Observatory, quick task).

The future rebindable key-table schema will validate duplicate bindings as config errors.
We keep the Uniterm convenience bindings that make sense in a terminal: index-based window selection and directional pane focus.

Agentic default bindings, ported from Uniterm's hotkey set (adapted to the prefix model):

- quick task / new task (the New Task surface),
- toggle Observatory,
- jump to next waiting-queue item,
- open command palette / command prompt.

## Mouse

Every mouse action resolves to the same semantic command a key or the CLI would issue.

- A left click focuses a Pane; a drag inside a Pane selects text unless the application owns the mouse.
- Pressing on a divider and dragging moves that divider to the pointer, resizing the split; the ratio persists with the layout.
  The keyboard equivalent is the `resize-*` bindings (prefix and Shift-H/J/K/L by default), which step the active Pane's nearest matching split by five percent.
- A right click on a Pane opens its context menu: split right or down, zoom, the all-Tabs overview, scrollback, a new Tab, one "Move to tab N" entry per other Tab of the same Project (named when the Tab has a name), "Move to new tab" when the Pane shares its Tab, and close.
  Moving a Pane keeps its process and scrollback; it is split beside the destination Tab's active Pane along that Pane's longer side, and a source Tab left empty closes.

## Quick task capture (the New Task surface)

The fastest path from intent to a running agent, ported from Uniterm's Quick Task dialog, rendered terminal-native (a prompt overlay, not a modal).

- Invoked by a keybind or a command.
- A single input: "What do you want to accomplish?"
- Inline slash-commands mirror Uniterm: `/workflow <name>` selects a template, `/project <name>` selects a project, `/save` saves the goal as a draft without launching, `/worktree` toggles worktree isolation, `/skip-permissions` runs in auto-approval mode.
- Remembered defaults: last-used workflow (tri-state, so "no workflow" is distinct from "unset"), last-used agent preferences, and the active project.
- On submit: if a workflow is selected, resolve agents, build the context pack (goal, project memory, success criteria), spawn the pane layout, and inject the first role's prompt deterministically; otherwise, launch a single agent with the prompt.
- A fuller task editor (success criteria, description, per-role agent overrides, a scratch terminal to run setup commands before launch) is available for the cases that need it, but the one-line capture is the default because most goals are quick.

Failures are never silent: an unresolved variable or a missing agent surfaces an error and falls back to interactive collection, exactly as Uniterm does.

## The `.uniterm/` project convention

Per-project agentic state lives under `.uniterm/` in the repo, git-ignorable and transparent (kept from Uniterm):

- `.uniterm/memory/memory.md` and memory candidates,
- workflow artifacts and verifier verdicts,
- git checkpoints,
- session mirrors from the provider adapters.

Machine-scoped multiplexer state (the event log, snapshots, the worker registry, the control-socket token) lives in the XDG state and data directories, not in the repo.
See [05-session-persistence.md](05-session-persistence.md).

## Command-line front door

The `uniterm` binary (alias `ut`) is the entry point for humans and agents alike.

Human-facing, tmux-like:

- `ut work` attaches to or creates the `work` Workspace.
- `ut workspace list|new|switch|rename|stop|forget|default` manages running and stopped Workspaces and the default selected by a bare `ut` invocation.
- `ut workspace stop --all` stops every running Workspace while retaining its lightweight definition.
- `ut workspace forget --all` permanently removes every stopped Workspace and refuses to run until all Workspaces are stopped.
- Workspace names are 1 to 64 bytes and contain only ASCII letters, digits, dots, hyphens, and underscores; separators, whitespace, `.` and `..` are rejected before any socket or state path is built.
- `ut project list|add|switch|rename|move|remove|metadata` manages Projects inside a Workspace.
- `ut pane list [-w Workspace] [--json]` lists stable Pane ids with their Project and Tab locations.
- `ut pane focus <pane-id> [-w Workspace]` focuses a live Pane across Project and Tab boundaries.
- `ut tab focus <project> <tab> [-w Workspace]` focuses a Tab by Project id/name and Tab ordinal/name while preserving its remembered active Pane.
- `ut pane focus <project> <tab> <pane> [-w Workspace]` focuses the 1-based Pane ordinal at an explicit hierarchy location.
- `ut pane metadata` publishes configurable sidebar context, optionally with a one-shot TTL.
- `ut agent explain [pane-id]` reports detection authority and evidence.

Agent-facing (over the control protocol):

- `uniterm workflow submit <token> --status ...`
- `uniterm relay submit <token> --status ...`

Scripting and remote:

- `uniterm control` speaks the control protocol for scripts and the future remote surface.
- structured subcommands to query fleet status, list and resolve waiting-queue items, and launch tasks.

The design keeps one binary, one socket, one command vocabulary, whether the caller is a person at a prompt, a keybinding, an agent reporting completion, or a remote client.
