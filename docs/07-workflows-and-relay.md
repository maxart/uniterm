# 07 - Workflows and Relay

This is the multi-agent orchestration layer: how one prompt becomes a coordinated sequence of agents that plan, build, and verify, with a human in the loop only when needed.

## Implementation status

The pure workflow and relay decision functions and both live tokened runtimes are implemented.
The runtime delivers `needs_input` into the actionable waiting queue, validates artifacts outside the core loop, retries prompt delivery with individually armed bounded deadlines, creates and verifies Git checkpoints on the Tokio side, records transition events, and reconstructs active runs and outstanding tokens after restart.
Optional token and cost caps remain deferred until provider usage data has explicit provenance and exact versus estimated markers.
Per-role provider selection is implemented for bundled workflows and native relays through one global provider fallback plus explicit role overrides.
User-defined workflow templates remain future work.

It is a near-direct port of Uniterm's workflow and relay engines, which are the most mature part of that product.
The single most important design principle carried over: the decision logic is a set of **pure functions** from state to a next-action, with no I/O and no UI, tested exhaustively in `uniterm-core`, and only then wired to real panes in `uniterm-server`.

## Two orchestration models

- **Workflow**: a role-based sequence with a fixed shape (for example plan, then build, then verify), pane layout, artifact gates, and a verifier that can send work back.
  Best when the shape of the task is known.
- **Relay**: a turn-based async handoff between roles, where each turn opens, waits for an explicit submit, and decides the next turn.
  Best for longer, more open-ended back-and-forth, and it adds per-turn git checkpoints and rollback.

They share the same foundations (the completion contract, stall detection, escalation, the waiting queue) and differ in their control flow.

## The completion contract (the load-bearing idea)

An orchestrator must know when a role or turn is actually done.
Guessing from idle output is fragile: an agent can pause mid-thought, or finish without any clear signal.

The contract makes it explicit.
An agent signals completion through a CLI call that our control protocol handles:

```
uniterm workflow submit <role-token> --status done|needs_input|failed [--summary TEXT] [--artifact [KIND=]PATH ...]
uniterm relay submit    <turn-token> --status done|failed        [--summary TEXT] [--artifact [KIND=]PATH ...]
```

Artifact kinds are `file`, `plan`, `patch`, `report`, `test-evidence`, and `findings`.
An unprefixed path remains a `file`, and an unrecognized prefix leaves the complete value as the path so filenames containing `=` remain usable.
The runtime canonicalizes the Project root and artifact path, rejects escape through absolute paths or symlinks, verifies a bounded non-empty regular file, and computes SHA-256 plus observed size outside the mio loop before the orchestration advances.
The resulting Artifact is owned by the exact active Run and Role.
See [25-typed-artifact-ledger.md](25-typed-artifact-ledger.md).

Key properties, all from Uniterm:

- The orchestrator advances on this explicit signal, not on an idle heuristic.
  The idle heuristic remains only as a safety net (below), never as the primary trigger.
- A **per-activation token** is minted for each role or turn and embedded in the prompt injected into that pane.
  A role cannot forge a completion signal for another role, because it does not have the other role's token.
- `done` advances (honoring gates), `needs_input` pauses into the waiting queue, `failed` escalates with the agent's own summary.

Both `submit` calls arrive over the control protocol, are validated against the live token, append to the event log, and wake the relevant engine.

## The pure decision brains

Two pure functions, ported from `decideWorkflowNext` and `decideRelayNext`.

```
decide_workflow_next(state) -> Action
    where Action is one of:
      Inject { role }          - deliver a role's prompt
      AwaitReview              - wait for the verifier
      AdvanceToGrader          - move to the verifier role
      Complete
      Escalate { reason }      - pause into the waiting queue
      Hold                     - nothing to do yet

decide_relay_next(state) -> Action
    where Action is one of:
      OpenTurn { role }
      AwaitSubmit
      Escalate { reason }
      Complete
      Stop
```

They take the full orchestration state (roles, current index, phase, iteration, caps, last verdict, gate state, timers) and return a tagged next-action.
They perform no I/O.
The server side interprets the action: `Inject` triggers a real bracketed-paste injection, `Escalate` creates a waiting-queue item, and so on.

This separation is why the engine is trustworthy.
Every tricky case (a verdict that stalls, an iteration cap, a race between an explicit submit and an idle safety-net) is a table-testable transition, not a tangle of callbacks.

## Workflow execution model

State (ported from Uniterm's `ActiveWorkflow`): id, task id, template id, status, phase, iteration, the ordered roles (each with pane, status, prompt, submit token, timestamps), the current role index, verified count, last verdict, per-role artifact-gate state, and the caps (max iterations, max tokens, max wall-clock).

The role lifecycle:

1. **Waiting**: pane spawned, PTY live, no prompt yet.
2. **Running**: the role's prompt is injected as a bracketed paste, gated on readiness, with delivery retry (2 s, 4 s, 8 s, jittered, capped, up to three tries before escalation).
3. **Completion**: normally an explicit `workflow submit`; as a safety net, an idle-hold detector (the pane goes from working-ish to idle and stays there past a debounce) plus a slower background poll catches roles that finish without submitting.
4. **Evidence-of-work check**: if a role never showed a working status since it started, it is paused for a grace period for the human to resume, rather than being treated as instantly done.
5. **Artifact gate**: if the role is expected to produce an artifact, the engine advances only once that file exists and is non-empty.
6. **Verifier gate**: the verifier role reads its verdict artifact and the engine routes on it.

## The verifier and verification-first

One role, the verifier, owns the verdict; every other role trusts it, so there is no circular verification.

The verifier writes a structured verdict (verdict is one of approved, fix, or replan, plus findings, an iteration number, and next actions).
The engine routes:

- `approved`: the workflow completes.
- `fix`: send back to the builder role.
- `replan`: send back to the planner role.

Send-back resolves its target from the template's loopback tag, increments the iteration counter, and mints a fresh token for the new activation.

The crucial reliability detail, from Uniterm: **verdict-stall detection**.
If two consecutive `fix` verdicts carry identical findings, the engine concludes the loop is not converging and escalates to the waiting queue instead of looping forever.
Combined with the max-iteration cap, this makes infinite fix loops impossible.

## Relay execution model

State (ported from `RelayRun`): id, task id, phase, roles, the list of turns, current turn index, iteration, escalation reason, a git checkpoint ref, caps, and a per-run turn-stall timeout (default around ten minutes, configurable).

Each turn (ported from `RelayTurn`) has an id, a role, a status (pending, running, submitted, error, cancelled), the submitted status and summary, artifacts, timestamps, and its own checkpoint ref.

The turn lifecycle:

1. **Open**: arm prompt delivery (with the same retry-with-backoff loop) and a stall timer.
2. **Await submit**: the agent works and calls `relay submit`; the arriving event clears the stall and delivery timers.
3. **Decide**: `done` opens the next role's turn if any remain; `failed` escalates immediately, because a reported failure is terminal, not transient.
4. **Escalate**: on stall, failure, cap exceeded, or manual stop, pause into the waiting queue with a rendered summary (outcome, rounds versus cap, elapsed, per-turn statuses, artifacts, token total), and offer rollback.

## Git checkpoints and rollback (relay)

Before a builder turn, relay takes a best-effort git checkpoint and stores the ref on the turn.
If that turn goes wrong, the escalation surface offers "roll back this turn," which restores the checkpoint.
This is low-complexity and high-trust: the human can undo a bad agent turn with one action.
Checkpoints live under `.uniterm/` in the project.

## Stall, retry, and stop

These are the properties that keep a long-running fleet from hanging or thrashing, all ported from Uniterm.

- **Stall timeouts**: a turn or role that never submits within its window escalates rather than waiting forever.
- **Delivery retry**: prompt injection that fails retries with bounded jittered backoff (2 s, 4 s, 8 s, capped around 30 s) up to three times before escalating.
- **Graceful then force stop**: the first stop lets in-flight work finish and arms no new work; a second stop within a grace window force-aborts and marks the run cancelled.
- **Caps**: optional max-tokens and max-wall-clock per run, sourced from OSC 777 telemetry, that pause the run when exceeded, with an approximate marker when the number is estimated.

## Escalation and the waiting queue

Every non-happy path ends in the same place: an item in the waiting queue (see [08-observatory.md](08-observatory.md)), scoped to the workspace, with the reason and the available actions (resume, skip role, roll back, stop).
This is deliberate.
There is exactly one surface a human checks to find everything that needs them, whether it is a permission request from a single ad-hoc agent or a stalled verifier in a five-role workflow.

## Templates

Workflows are defined by templates, ported from Uniterm's TOML format.

A template specifies: the ordered roles, the pane layout to spawn, per-role prompts with variable interpolation (the goal, the project, context paths, artifact names), which roles expect artifacts, the verifier and its loopback targets, and the caps.
Templates are agent-agnostic: a role is a slot, and any provider can fill it, with the concrete agent chosen per role by override, saved preference, template default, or first-installed.

Templates are bundled (a solo agent, a planner-builder-verifier triad, a pair) and user-definable, and can be applied by name from quick task capture or the command language.

Bundled roles declare provider-neutral capabilities and currently require `interactive_cli`.
The New Task syntax accepts `@provider` as the global fallback and `@role=provider` as an explicit override, while control API `orchestration_start` uses the same typed selection vocabulary and server launch path.
Resolution and PATH checks happen once before any role Pane is created.
Each role's resolved provider and command are projected durably and restored without rerunning provider selection.
See [24-per-role-provider-selection.md](24-per-role-provider-selection.md).

## Persistence and recovery

Because every workflow and relay transition is appended to the event log, the engines are recoverable.
After a server restart, an in-flight orchestration is reconstructed from the log: roles and turns, their statuses, the current position, and the outstanding tokens, so a workflow that was mid-build resumes rather than being lost.
A run that cannot be safely resumed is surfaced as cancelled with its full history preserved, exactly as Uniterm does, rather than silently dropped.

## Why this is the differentiator

Plenty of tools can fan out prompts to several agents.
The hard part, and the part Uniterm actually solved, is doing it deterministically: knowing when a role is truly done, refusing to loop forever, checkpointing so mistakes are reversible, and funneling every exception to one human-facing queue.
Porting the pure decision brains verbatim, with their exhaustive transition tests, is how we inherit that reliability instead of re-learning it.
