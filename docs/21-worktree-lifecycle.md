# Worktree Resource Lifecycle

## Purpose

A Uniterm worktree is a Git-owned linked worktree and a Workspace-owned Project.
Neither half is inferred from the other while rendering.
The lifecycle keeps both authorities aligned for parallel agent work without treating a path created by an arbitrary user as Uniterm-owned.

## Resource identity

Each registered worktree retains the owning Project ID and name, Git's canonical primary worktree, the canonical linked-worktree path, the local branch, and the commit checked out when creation completed.
This typed provenance appears in `ProjectInfo`, Workspace snapshots, worktree list results, and the Manage Projects and Projects rail details.
Related Projects expose the same repository label without running Git from either rendering surface.

Crash snapshots retain the provenance in reserved Project metadata.
Clean-stop Workspace definitions retain the typed registration so a later start creates fresh shell Panes at the same worktree and restores the lifecycle commands.
Ordinary Project metadata commands cannot overwrite the reserved `uniterm.worktree.*` keys.

## Commands

```text
ut project worktree list [-w Workspace]
ut project worktree add NAME REPO PATH [BASE] [-w Workspace]
ut project worktree open PROJECT [-w Workspace]
ut project worktree remove PROJECT [-w Workspace]
ut project worktree remove PROJECT --force --yes [-w Workspace]
ut project worktree cleanup PROJECT [-w Workspace]
```

`add` derives a local `uniterm/<name>` branch, records durable intent, asks Git to create it, verifies `git worktree list --porcelain -z`, and only then creates the Project and its first Pane.
`list` re-checks Git and reports active, missing, or prunable state, current branch and head, and whether the worktree is dirty.
Registrations from the same repository share one porcelain scan within a list request.
`open` re-checks the registration before switching to the Project.

## Removal and cleanup safety

Default removal runs `git status --porcelain` and refuses uncommitted or untracked changes.
Git remains authoritative and may apply stricter removal checks of its own.
The human CLI accepts destructive removal only when `--force` and `--yes` are both present.
The control API requires an explicit JSON `force: true` field.

An ordinary `ut project remove` refuses worktree Projects and points to the lifecycle command.
Project-surface removal is routed through the same clean-only server operation, so a UI action cannot bypass Git.

`cleanup` does not delete a live path.
It accepts only a worktree that Git reports missing or prunable, runs `git worktree prune`, verifies that the entry is absent, and then forgets the stale Project.

## Events and runtime ownership

Creation, removal, and cleanup append intent events before the Tokio runtime is allowed to mutate Git.
The runtime serializes worktree mutations, performs subprocess and filesystem work on a blocking worker, and returns a typed result without stalling the control dispatcher.
If the Workspace event stream is poisoned, mutating worktree commands fail before Git runs.

After Git confirms success, the mio core appends the result event before changing the Project projection.
The Tokio side never reads grids or mutates Projects, and the mio side never runs Git.
No timer, render-time scan, or idle wakeup is added.

## Automation

Binary protocol version 6 introduced the shared `WorktreeOperation` vocabulary, which remains available in current protocol version 13.
Control API version 1 exposes `worktree_list`, `worktree_add`, `worktree_open`, `worktree_remove`, and `worktree_cleanup` with the same Workspace-scoped semantic handler.

```json
{"version":1,"id":20,"workspace":"default","method":"worktree_list"}
{"version":1,"id":21,"workspace":"default","method":"worktree_add","params":{"name":"Review","repository":"/work/uniterm","path":"/work/uniterm-review","base":"main"}}
{"version":1,"id":22,"workspace":"default","method":"worktree_remove","params":{"project":2,"force":false}}
```

Results contain an `accepted` flag, an optional error, and freshly inspected resource entries.
Every Project ID is resolved only inside the named Workspace.
