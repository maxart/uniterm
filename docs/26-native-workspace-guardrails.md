# Native Workspace automation guardrails

## Purpose

Uniterm can enforce policy over native actions whose ownership and side effects it controls.
This boundary adds provider-neutral launch, capacity, iteration, elapsed-time, Project, and confirmed rollback decisions without claiming to sandbox arbitrary commands executed inside a provider CLI.

The pure policy is in `uniterm-core`.
The mio server supplies stable Workspace facts, appends every decision before the corresponding side effect, and routes elapsed-time asks through the existing waiting queue.
No policy check reads a terminal grid, touches the filesystem, polls a process, or branches on a provider id.

## Configuration

The Ghostty-style configuration accepts these bounded keys:

```text
guardrail-max-active-runs = 8
guardrail-max-role-panes = 16
guardrail-max-iterations = 3
guardrail-max-elapsed-minutes = 120
guardrail-allowed-project = api
guardrail-allowed-project = /work/web
```

The same five values are editable in the built-in Settings surface.
The four numeric rows support left and right adjustment or exact entry with Enter.
Allowed Projects uses a semicolon-separated one-line editor, and clearing it restores the allow-all Workspace default.

The hard parser bounds are 64 active runs, 256 role Panes, 100 iterations, seven days elapsed, and 64 Project selectors.
Every numeric value must be at least one.
Invalid values are diagnosed and the forgiving runtime parser retains its safe default.

Allowed-Project values are exact matches against a Project's name or stored canonical root.
An empty selector list allows every Project already owned by the Workspace.
A non-empty list that matches no owned Project denies every native orchestration launch.
The request Workspace remains the outer authority, so a selector can never reach a Project in another Workspace.

## Launch contract

Interactive New Task and control `orchestration_start` already share the same workflow and relay launch functions.
Those functions now resolve the target Project and provider assignments, evaluate the complete requested role set, and append a `GuardrailDecision` before the first role Pane is spawned.

Paused runs still count toward active-run and role-Pane limits because they retain their Panes and durable ownership.
A rejected launch creates no role Pane, Task, Run, or partial layout.
A successful launch creates every role Pane at the resolved Project root and records that same stable `ProjectId` in the Run graph.

Each run captures its launch-time limits.
A later config reload therefore cannot silently rewrite an active run's iteration or elapsed-time contract.
Workflow and relay state machines use the captured iteration limit instead of a hard-coded template-specific value.

## Elapsed-time boundary

Elapsed-time enforcement is event-armed.
An `Instant` deadline exists only while a workflow or relay is in the awaiting phase, and it participates in the mio poll timeout that already serves concrete orchestration work.
Paused and terminal runs add no elapsed wakeup.

At the exact boundary the server appends an `ask` decision, clears competing idle and stall deadlines, sends a provider-neutral guard event through the pure state machine, and creates the ordinary Workspace-scoped waiting item.
An explicit waiting-queue resume is the human override and does not arm the same elapsed guard again.

Durable orchestration projections retain the launch epoch, captured limits, and whether the elapsed guard has fired.
Legacy projections start their elapsed contract at recovery with safe defaults while preserving the state machine's original iteration cap.
Invalid durable limits reject that recovered run instead of being silently clamped.

## Confirmation and audit

The pure vocabulary distinguishes ordinary semantic operations from destructive or bulk operations that require explicit confirmation.
The existing relay rollback is already reachable only from a relay waiting item.
That confirmed path now records an `allow` decision before the Tokio runtime receives the Git rollback request.

Project removal, the bulk agent stop, and Workspace stop carry the human's confirmation on the wire instead of assuming it.
The thin client sets `confirmed` only after its own confirm step (the Projects and Agents views' two-step confirm, or the close-confirmation overlay for the context menu), an explicit `ut project remove` or `ut workspace stop` is the operator's confirmation, and a control request must pass `"confirmed": true`.
The server evaluates the pure decision and appends it before the first Pane closes; an unconfirmed request is refused with a `confirmation_required` control error or a Guardrail toast, and that `ask` is itself in the audit trail.

Every launch allow or denial, elapsed ask, and confirmed rollback is an append-only `GuardrailDecision` in the Workspace event stream.
The record carries stable Project and Run ownership when those identities exist, the evaluated action, and the pure outcome.
Waiting resolution remains the separate durable record of what the human chose.

## Deliberate limits

This is an automation contract, not a security boundary around child processes.
It does not intercept provider network calls, filesystem operations, shell commands, credentials, or model APIs.
It adds no credential proxy, inference gateway, cloud runner, or third-party protocol bridge.

Token and cost budgets remain disabled.
Uniterm does not yet own authoritative invocation-scoped usage facts, and screen estimates are not acceptable enforcement inputs.
Provider-sourced usage with provenance and exact-versus-estimated markers remains a separate Priority 2 ledger.

The semantic command vocabulary leaves room for consistent policy on bulk waiting actions, Project removal, and Workspace stop.
Those commands retain their existing local confirmation paths until they are exposed through a shared unattended semantic surface.

## Verification

Pure tests cover allow, deny, capacity, Project restriction, elapsed boundary, confirmation, and no-op decisions.
Config tests cover bounds, diagnostics, duplicate elimination, and canonical round trips.
The socket integration test proves allowed Project ownership, active-run denial without partial Panes, foreign selector rejection, append-only allow, deny, and ask events, and elapsed escalation into the waiting queue.
The orchestration deadline test proves fully disarmed state has no wakeup.
Settings tests cover bounded numeric edits, exact Project-list edits, protocol transport, and repeated config-key preservation and clearing.
