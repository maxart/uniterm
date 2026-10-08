# 13 - Uniterm Desktop hierarchy migration

This document records the migration contract between Uniterm Desktop and Uniterm CLI.
The first version moves only durable organization, not terminal runtime state.

## Scope

The importer preserves:

- Workspace names and ordering from the source selection.
- Project names, ordering, and canonical filesystem paths.
- Tab count, ordering, and user-given Tab names.

Every imported Tab starts with one fresh shell Pane at its Project path.
The importer deliberately ignores Pane layouts, scrollback, terminal contents, running processes, shell arguments, browser state, and agent state.
Those values are either runtime-specific or do not have a faithful representation in both applications.

## Desktop discovery

`ut migrate from-desktop` checks `UNITERM_DESKTOP_DATA_DIR` first, then the native data directory used by Uniterm Desktop:

- Linux and BSD: `$XDG_DATA_HOME/com.uniterm.app`, falling back to `~/.local/share/com.uniterm.app`.
- Linux Flatpak: `~/.var/app/com.uniterm.app/data/com.uniterm.app`.
- macOS: `~/Library/Application Support/com.uniterm.app`.
- Windows: `%APPDATA%\com.uniterm.app`, with `%LOCALAPPDATA%\com.uniterm.app` as a compatibility fallback.

The directory is considered used when `projects.json` or its recovery backup exists.
The importer reads `project_workspaces.json`, `projects.json`, and `workspaces_v3.json` without modifying them.
It accepts the Desktop atomic writer's `.bak` recovery files when a live file is missing or invalid.

## Safety and conflicts

The importer canonicalizes every Project path before creating anything.
Projects whose paths are unavailable are reported and skipped.
Workspace names become safe socket keys containing only ASCII letters, digits, dots, underscores, and hyphens.

Existing CLI Workspaces are never silently overwritten or removed.
An interactive conflict offers these choices:

- Import under a unique suffixed name.
- Merge only missing Projects and Tabs.
- Archive the existing Workspace under a timestamped name, then import.
- Skip the conflicting Workspace.
- Cancel the remaining migration.

Archive is the replacement workflow.
It preserves the complete existing Workspace and never deletes it.
Merge matches Projects by canonical path and does not rename or remove existing Projects or Tabs.

`--dry-run` performs discovery, parsing, canonicalization, counting, and conflict reporting without changing CLI state.
Automation must select an explicit `--on-conflict` policy when prompting is unavailable.

## Entry points

The command-line entry point is:

```sh
ut migrate from-desktop --dry-run
ut migrate from-desktop
ut migrate from-desktop --workspace Work
ut migrate from-desktop --on-conflict rename --yes
```

The in-app Settings surface contains `Import Uniterm Desktop`.
Activating it detaches the thin client, restores normal terminal mode, and runs the same interactive importer.
After success it attaches to an imported Workspace, and when several were imported it opens a freshly probed Workspace switcher.
There is one parser and one conflict implementation for both entry points.

## Persistence and runtime boundaries

Desktop files are read only from the CLI process.
The CLI sends a typed hierarchy import to the target Workspace server.
The server creates fresh PTYs, appends structural events, updates its in-memory projection, and writes its normal atomic snapshot.
No migration script writes bincode snapshots or bypasses the server's domain operations.

Migration is explicit user work and creates no watcher, polling loop, background timer, or idle wakeup.

## Reverse migration

The same hierarchy can later be exported through a versioned portable manifest and imported by Uniterm Desktop.
That reverse path should use a Desktop-owned command or authenticated RPC so Uniterm CLI never writes another application's live persistence files.
Live two-way synchronization is out of scope.
