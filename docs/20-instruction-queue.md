# Instruction Queue and Steering

## Purpose

The instruction queue stores human direction that should reach an agent after its current work.
It is distinct from the waiting queue, which stores agent requests that need a human decision.
Queued direction is never inferred from screen text and never injected because a heuristic detector guessed that the Pane was idle.

## Model

Each item has a monotonic instruction ID, stable Pane ID, exact foreground process-group identity, author source, creation event sequence, delivery policy, state, and bounded UTF-8 text.
The active projection retains at most 1,024 instructions per Workspace and 64 per invocation.
One instruction retains at most 16,384 Unicode scalar values.
An add that has no active agent invocation, exceeds a bound, or contains only whitespace is rejected.

The append-only event stream records queued, replaced, canceled, and delivery-attempt events.
Replacement creates a fresh instruction ID and resolves the old identity without editing prior history.
Every delivery attempt has a monotonic delivery ID, its permitting boundary, and the authoritative Pane input acceptance result.
An accepted attempt is recorded before the item leaves the in-memory active projection.
A rejected Pane input attempt remains queued and can be retried by a later cooperative ready event or explicit send-now.

## Delivery boundaries

The default `next_ready` policy delivers at most one instruction when the owning invocation emits a cooperative OSC 777 `idle` event.
The parser carries this readiness fact separately from the reconciled `AgentStatus::Idle` value.
Log-tail, grid, quiescence, and process evidence may update status and user-visible diagnostics, but none of those paths call instruction delivery.

`send-now` is the only human bypass.
It still validates Pane and invocation ownership and uses the same bracketed-paste, submit-tail, and bounded Pane input path as cooperative delivery.

## Invocation and recovery safety

An instruction belongs to the recorded process group, not merely to a reusable Pane ID.
A process-group change, cooperative session end, Pane close, or server restart cancels stale items with a durable reason.
Replaying the event log rebuilds the pending projection and advances instruction and delivery allocators before stale invocation reconciliation runs.
Direction from a dead invocation is never inherited by a replacement agent in the same Pane.

## Commands

```text
ut instruction list [-w Workspace] [--json]
ut instruction add PANE TEXT... [-w Workspace] [--json]
ut instruction replace ID TEXT... [-w Workspace] [--json]
ut instruction cancel ID [-w Workspace] [--json]
ut instruction send-now ID [-w Workspace] [--json]
```

`ut steer` is an alias for `ut instruction`.
The binary client protocol and NDJSON control API route these operations through the same mio-owned semantic handler.
Control API callers use `instruction_list`, `instruction_add`, `instruction_replace`, `instruction_cancel`, and `instruction_send_now`.

## Runtime and idle behavior

The pure queue and lifecycle types live in `uniterm-core`.
Only the mio core validates invocation ownership and writes to a PTY.
The Tokio runtime persists event records and transports control requests without accessing Pane state.
The feature adds no periodic timer, process scan, grid scan, or idle wakeup.
