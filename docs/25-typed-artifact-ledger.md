# Typed Artifact Ledger

## Purpose

The artifact ledger makes produced files durable, attributable resources instead of unstructured completion text.
Every retained Artifact has a stable Workspace-local `ArtifactId`, canonical Project ownership, an exact producer `RunId` and `RoleId`, a provider-neutral kind, a normalized Project-relative path, SHA-256 digest, observed byte size, lifecycle status, and an optional superseded identity.

The ledger does not copy file contents into Uniterm.
The Project filesystem remains authoritative for contents, while the Workspace event log is authoritative for observations and ownership history.

## Pure projection

`uniterm-core::artifact` contains the bounded reducer and no I/O, async runtime, or UI dependency.
The live projection retains at most 4,096 records and indexes Artifact by identity, Project, producer Run, producer Role, and current Project-relative path.
Scalar ownership reads use those indexes instead of scanning Panes.

The append-only lifecycle has three facts:

- `observed` creates one immutable producer identity with current filesystem facts.
- `refreshed` changes digest, size, and availability for the same identity after an event-driven re-observation.
- `missing` records that the current path no longer resolves to a bounded non-empty regular file.

A later `observed` event at the same Project path supersedes the previous current record atomically.
Superseded or missing records are the first candidates pruned at the projection cap.
A replacement remains possible when every retained record is available because the replaced current identity itself can leave the bounded projection.

## Validation and ownership

Workflow and relay completion accepts `--artifact [KIND=]PATH`.
Kinds are `file`, `plan`, `patch`, `report`, `test-evidence`, and `findings`.
Unprefixed values remain ordinary file paths.

The mio core validates the active completion token and resolves the active Run, Role, Pane, and Project.
It delegates filesystem work through `CoreToAgent::ArtifactValidate`.
The Tokio runtime canonicalizes the Project root and candidate, rejects paths and symlinks that escape the root, requires a non-empty regular file no larger than 256 MiB, bounds normalized UTF-8 path metadata, and streams SHA-256 in fixed-size chunks.
Only the resulting bounded facts cross back to the core.

The core stages the complete observation batch through a cloned pure ledger before appending any Artifact event.
It then appends every accepted lifecycle fact before publishing the projection and advancing the orchestration.
Ownership is captured before handoff, so the producer Role cannot race the next activation.

## Event-driven refresh

After an Artifact is observed, the core sends the runtime a complete typed watch set grouped by Project.
The runtime holds operating-system watches on artifact parent directories and arms no timer.
A matching filesystem event returns only stable Artifact identities to the core.
The core resolves current ownership and coalesces duplicate in-flight observations before asking the runtime to read and hash the file again.

Unchanged digest, size, and status are a no-op and append no event.
A content change appends `refreshed`.
Deletion appends `missing`, while the watch remains armed so recreation can restore availability.
Filesystem reads and hashes never run on the mio loop or touch a terminal grid.

## Recovery

Snapshot schema 12 checkpoints the bounded Artifact projection and its independently applied event cursor.
Startup restores that checkpoint, streams only the Artifact lifecycle suffix, then rebuilds the runtime watch set after Projects are restored.
Snapshot schema 11 migrates with its native run graph intact and an empty Artifact cursor, so older events and current files are never invented as observations.
Foreign Workspace envelopes, sequence gaps, future schemas, and invalid Artifact transitions fail recovery through the same event-log policy as other durable projections.

## Inspection

Binary wire protocol version 10 added `ArtifactList` and the typed `Artifacts` response, which remain available in current version 13.
The human command is:

```text
ut artifact list [-w Workspace] [--project ID] [--run ID] [--all] [--json]
```

The neutral control method is `artifact_list` with optional `project`, optional `run`, and `include_superseded` fields.
Both paths are Workspace-scoped server reads over the same projection.
Current available and missing records are returned by default, while `--all` or `include_superseded` exposes retained replacement history.

## Bounds and non-goals

This slice intentionally does not add Artifact contents, review annotations, line comments, a Timeline, workflow-detail rendering, general memory, or provider usage or cost.
Native Workspace guardrails are implemented separately in [26-native-workspace-guardrails.md](26-native-workspace-guardrails.md).
Those features can reference stable Artifact ownership later without changing this lifecycle contract.
