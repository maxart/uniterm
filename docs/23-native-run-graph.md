# Native Run Graph

## Purpose

The native run graph gives workflows, relays, and later forked work one provider-neutral ownership model.
It records which Project owns a run, which Task describes it, which stable roles own which Panes and providers, which role currently owns the live activation, and how child runs relate to parents.
It does not copy provider conversation history or create another orchestration runtime.

## Pure model and indexes

`uniterm-core` owns `RunId`, `RoleId`, `RunKind`, `RunStatus`, `RunRecord`, `RoleRecord`, `RunActivation`, and the pure `RunGraph` reducer.
The reducer has no I/O, async runtime, or UI dependency.
It maintains direct indexes for run to parent, run to children, run to Panes, Pane to active run and role, role to its latest activation, Project to runs, and Task to run.
Scalar relationship queries therefore do not scan the server's Pane map.

Run IDs, role IDs, and activation IDs are Workspace-local monotonic identities.
An activation ID is public inspection identity, not the private per-turn completion token embedded in an orchestration prompt.

## Event contract

Every lifecycle mutation is appended before the live graph projection changes.
The versioned event vocabulary contains creation, role declaration, activation, handoff, completion, failure, and cancellation.
Creation records the optional parent, Project, run kind, Task, and bounded title.
Role declaration records stable Pane and provider ownership.
Activation and handoff record public activation identities without exposing completion tokens.
Terminal events release the Pane-to-active-run index while retaining the bounded current graph for inspection.
The current projection retains at most 4,096 closed leaf runs plus live runs and their required parent chains.
Older closed subtrees remain in the append-only event log for later timeline or audit replay.

Ordinary workflow and relay launches create root runs.
`RunFork` creates a child only after Git authority confirms a linked worktree and the server registers its Project.

## Recovery and snapshots

Snapshot schema 11 checkpoints the bounded current graph and stores the exact event sequence reflected by it.
Startup streams only later run events through the same pure reducer.
An absent or older snapshot starts from an empty graph and replays the complete event stream.
A foreign Workspace event, missing relationship, duplicate identity, invalid handoff, or repeated terminal outcome fails recovery as invalid durable data.

The append-only event log remains authoritative.
The snapshot is only a bounded recovery accelerator and never edits or replaces history.

## Inspection surfaces

Binary wire protocol version 8 added `RunList` and the typed `Runs` projection, which remain available in current version 13.
Binary wire protocol version 12 adds `RunFork` and the typed `RunForked` result.
The CLI exposes the same semantic read as `ut run list [-w Workspace] [--project ID] [--active] [--json]`.
`ut run fork PARENT NAME PATH [BASE]` creates a worktree-backed child from an active native run.
Control API version 1 adds `run_list` with optional `project` and `active_only` parameters and advertises `run.list` through capability discovery.
Every control request still names its Workspace, so a socket cannot inspect a foreign graph.

The client Observatory detail shows the active Run and Role for a selected agent Pane.
The persistent server-rendered Agents rail shows the same ownership beside the active agent.
Both read the in-memory projection only while rendering and arm no timer, watcher, disk read, or grid poll.

## Current boundary

This slice establishes durable run relationships for the existing provider-neutral workflow and relay runtimes.
Per-role provider selection now records the resolved provider independently on every role declaration and is documented in [24-per-role-provider-selection.md](24-per-role-provider-selection.md).
The typed artifact ledger is implemented as the next ownership layer in [25-typed-artifact-ledger.md](25-typed-artifact-ledger.md).
Native Workspace guardrails are implemented separately in [26-native-workspace-guardrails.md](26-native-workspace-guardrails.md).
Worktree-backed child launch is implemented with inherited goal, provider assignments, and durable artifact references, but fresh Task, Pane, Role, activation, and completion-token identities.
Explicit artifact-consumption edges and richer timeline projections remain follow-on slices.
