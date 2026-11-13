# Neutral Control API

## Purpose

The control API is Uniterm's provider-neutral local automation contract.
It observes and mutates server-owned resources without pretending to be an interactive terminal client.
Terminal attach remains on the versioned binary protocol.

## Transport and discovery

Each running Workspace owns `<socket-dir>/<workspace>.control.sock` beside its ordinary `<workspace>.sock` attach socket.
The control socket is an owner-only Unix socket with mode `0600` inside an owner-only directory.
Every request and response is one UTF-8 JSON object followed by a newline.
Protocol version 1 accepts at most 1 MiB per request line or output frame, 64 pending requests across the Workspace, and 64 queued output frames per connection.
There is no fixed connection-count cutoff; the compatible numeric `max_connections` field advertises `4294967295`, with actual capacity determined by OS resources.
Each connection owns one socket and an async task on the transport reactor, with no descriptor clone or dedicated reader/writer threads.
A request that would exceed bounded intake receives `server_busy` before dispatch and may be retried later.
Malformed or unsupported requests receive `invalid_request`, correlated to their numeric id when available, or id zero otherwise.
An oversized response receives `response_too_large` instead of silently closing the connection.
Lost replies and transport timeouts do not imply that mutations were rolled back; clients do not replay mutations automatically and must inspect the target before retrying an unconfirmed operation.
A connection that does not drain its output is disconnected instead of retaining unbounded event history.

## Request envelope

Every request contains `version`, a caller-chosen numeric `id`, the exact `workspace` name, and a tagged `method` with optional `params`.
The server rejects a mismatched Workspace even though the socket pathname already scopes the connection.

```json
{"version":1,"id":1,"workspace":"default","method":"capabilities"}
{"version":1,"id":2,"workspace":"default","method":"pane_send","params":{"pane":1,"text":"hello\n"}}
{"version":1,"id":3,"workspace":"default","method":"subscribe","params":{"after_sequence":42}}
{"version":1,"id":4,"workspace":"default","method":"instruction_add","params":{"pane":1,"text":"Run the focused regression next"}}
{"version":1,"id":5,"workspace":"default","method":"worktree_list"}
{"version":1,"id":6,"workspace":"default","method":"run_list","params":{"project":1,"active_only":true}}
{"version":1,"id":7,"workspace":"default","method":"orchestration_start","params":{"launch":{"kind":"workflow","template":"pair","goal":"Ship it","provider":"claude","role_providers":[{"role":"verifier","provider":"codex"}],"project":null}}}
{"version":1,"id":8,"workspace":"default","method":"artifact_list","params":{"project":1,"run":3,"include_superseded":false}}
{"version":1,"id":9,"workspace":"default","method":"run_fork","params":{"fork":{"parent":3,"name":"alternative","path":"/work/alternative","base":null}}}
{"version":1,"id":10,"workspace":"default","method":"task_list"}
{"version":1,"id":11,"workspace":"default","method":"project_remove","params":{"project":2,"confirmed":true}}
{"version":1,"id":12,"workspace":"default","method":"agent_stop_all","params":{"scope":{"project":2},"confirmed":true}}
```

`project_directory` accepts `{"project":1,"root":"/work/new-root"}` and uses the same host-validated path as the sidebar and `ut project directory`.
The capability is `project.directory`; success returns an accepted Project mutation, including a canonical-path no-op.
Rejections return `project_directory_rejected` without changing the root.
Use absolute paths or `~/` on the Workspace host.
Existing Panes retain their working directories; new launches use the changed root.
Registered worktrees and Projects with an active workflow or relay reject directory changes.
The binary wire version is 33 and the event-envelope schema is 11; older binaries refuse the new durable schema instead of treating an unknown directory-change or delegation event as corruption.

`project_remove` and `agent_stop_all` close every Pane they reach, so they require `"confirmed": true`; the server records the guardrail decision before the first Pane closes and answers an unconfirmed request with the `confirmation_required` error.

`delegation_arm` accepts `{"pane":1,"confirmed":true}` and arms one Pane for delegated answering, approving only complete recognized permission dialogs through the shared Auto-approve pipeline (see [26-native-workspace-guardrails.md](26-native-workspace-guardrails.md)).
Arming hands the provider's future approval keystrokes to the server, so the same confirmation contract applies: an unconfirmed request is refused with `confirmation_required` and nothing changes, and a confirmed arm appends `DelegationArmed` before the first verdict runs.
`delegation_disarm` takes `{"pane":1}` and records `DelegationDisarmed`; `delegation_status` takes an optional `pane` and returns every matching arm with its hourly answers used and remaining; `delegation_explain` takes `{"pane":1}` and returns the verdict the arm would reach for the Pane's live recognized dialog without changing anything.
Explicit disarm excludes the current invocation from Workspace automation too; explicit Auto-approve off clears all arms.
An arm can consume its final budget immediately on an already-visible dialog, so a successful arm response may contain no remaining active entry.
The capabilities are `delegation.arm`, `delegation.status`, and `delegation.explain`.

`ut tab new` and `ut tab rename` are thin CLI verbs over the `tab_create` and `tab_rename` methods, so a script and a control client see the same result.
The implemented surface exposes `capabilities`, Workspace and hierarchy snapshots, Project and Tab mutations, Pane read, send, and focus, Agent list, launch, focus, and stop, Task mutations, waiting-item actions, delegation arm, disarm, status, and explain, instruction operations, worktree lifecycle operations, native run-graph and artifact inspection, native orchestration launch and submission, worktree-backed child Run launch, and event subscriptions.
`pane_send` accepts a UTF-8 `text` string so hand-written JSON uses ordinary JSON escapes instead of an integer byte array.
Pane mutations use the same bounded pane-input function as the binary client protocol.

## Background automation and text submission

`project_create`, `tab_create`, `pane_split`, and `agent_launch` accept `"background": true` to preserve the human's selection.
The default is false for compatibility with interactive creation.
Before using these flags, direct NDJSON clients must check `capabilities` for `automation.background`; older v1 servers can ignore unknown fields.
The Rust control client and CLI perform this check on the same connection before sending a background mutation or targeted Tab edit.
`agent_launch` accepts an optional stable `pane` anchor so automation can launch in another Tab or Project without selecting it.
`pane_split` requires a stable `pane` plus `axis` (`"LeftRight"` or `"TopBottom"`).
`tab_rename` and `tab_move` always operate on their target without selecting it.
`pane_close` and `tab_close` close explicit targets; closing the selected resource uses the ordinary replacement selection, and closing the last Pane stops the Workspace.
Creation, launch, and split responses report the created identity; a Tab id is its current 1-based ordinal within its Project.
Agent, Task, orchestration, and child Run launches resolve providers asynchronously; wait for the request's response before issuing a dependent query.
A changed target or executable search path returns `launch_refused`, and a disconnected requester cannot complete a pending launch.
Provider discovery has bounded admission; a busy response can be retried after outstanding requests complete.

```json
{"version":1,"id":20,"workspace":"default","method":"tab_create","params":{"project":2,"background":true}}
{"version":1,"id":21,"workspace":"default","method":"agent_launch","params":{"agent":"codex","target":"NewWindow","pane":7,"background":true}}
{"version":1,"id":22,"workspace":"default","method":"pane_paste","params":{"pane":8,"text":"First line\nSecond line","submit":true}}
{"version":1,"id":23,"workspace":"default","method":"pane_paste","params":{"pane":8,"text":"","submit":true}}
{"version":1,"id":24,"workspace":"default","method":"tab_paste","params":{"project":2,"tab":3,"text":"Run tests","submit":true}}
```

`pane_paste` and `tab_paste` honor the destination's bracketed-paste mode; `submit: true` appends terminal Enter (CR) outside the paste envelope.
Empty text with `submit: true` sends only Enter, and `submit: false` inserts text without submitting.
`tab_paste` resolves the Tab's remembered active Pane without changing selection.
`pane_send` remains the exact-byte UTF-8 operation for callers that intentionally supply terminal control sequences.
An accepted input response acknowledges queueing, not task completion; use output waits or explicit completion contracts before taking the next action.
Capability discovery advertises `automation.background`, `pane.split`, `pane.close`, `tab.close`, `pane.paste`, and `tab.paste`.

## Instruction queue

`instruction_add` binds bounded human direction to the Pane's current agent invocation and defaults to delivery at the next cooperative ready boundary.
`instruction_replace` creates a fresh durable instruction identity while resolving the superseded item.
`instruction_cancel` removes an item without writing to its Pane.
`instruction_send_now` is the explicit bypass for urgent direction.
All four mutations and `instruction_list` use the same server semantic path as the binary client and CLI commands.
Heuristic idle evidence never delivers an instruction.
See [20-instruction-queue.md](20-instruction-queue.md) for lifecycle, recovery, and failure behavior.

## Worktree resources

`worktree_add`, `worktree_list`, `worktree_open`, `worktree_remove`, and `worktree_cleanup` use the same semantic path as the binary client and CLI commands.
Git work runs on a serialized blocking worker while the control dispatcher remains available.
Mutations require a healthy Workspace event stream and update Project state only after Git authority confirms the result.
See [21-worktree-lifecycle.md](21-worktree-lifecycle.md) for provenance, restore, and destructive-action safety.

## Run graph resource

`run_list` returns stable Run and Role identities, parent and child links, Project and Task ownership, Pane and provider ownership, current public activation identity, lifecycle status, and terminal outcome.
Optional `project` and `active_only` parameters filter the already Workspace-scoped projection.
The CLI and binary protocol use the same semantic read.
See [23-native-run-graph.md](23-native-run-graph.md) for event ordering, recovery, and index ownership.

## Artifact resource

`artifact_list` returns stable Artifact identity, Project ownership, producer Run and Role, kind, normalized Project-relative path, SHA-256 digest, size, status, and optional superseded identity.
Optional `project` and `run` parameters filter the Workspace-owned projection through its direct indexes.
Superseded records are hidden unless `include_superseded` is true, while missing current records remain visible.
The CLI and binary protocol use the same semantic read.
See [25-typed-artifact-ledger.md](25-typed-artifact-ledger.md) for lifecycle and observation behavior.

## Orchestration launch

`orchestration_start` launches a bundled workflow or the native two-role relay through the same server functions used by the interactive New Task surface.
The request carries one optional global provider and explicit role-to-provider overrides.
The server validates role names and capabilities, resolves every executable, and rejects the entire request before creating Panes if any assignment is invalid.
A successful result returns the stable Run id.
See [24-per-role-provider-selection.md](24-per-role-provider-selection.md) for selection order, recovery, and the provider-ownership boundary.

`run_fork` accepts one active parent Run plus a new Project name, absolute worktree path, and optional Git base.
The server derives the repository, goal, provider assignments, and durable artifact references from the parent.
It asks Git authority to create the worktree, registers a Project only after Git confirms it, and launches a child through the ordinary guarded workflow or relay path.
The child receives fresh Task, Pane, Role, activation, and completion-token identities.
If launch fails after Git creation, Uniterm removes the transient Project and asks Git authority to roll back the worktree.

## Responses and errors

Each response frame echoes the request `id` and contains either a typed `result` or a structured `error` with `code` and `message`.
`confirmation_required` means a destructive or bulk method was called without `"confirmed": true` and nothing changed.
Capability discovery returns the protocol version and stable capability strings.
Workspace snapshots include the current event sequence so automation can take a snapshot and then subscribe without an observation gap.

## Event subscriptions

`subscribe` accepts an `after_sequence` cursor owned by the named Workspace.
The server first returns a successful subscription response, then streams every durable event with a greater sequence, followed by new events after their append succeeds.
Event frames contain the subscription id, sequence, timestamp, Workspace, and typed event JSON.
A cursor newer than the server projection is rejected as `invalid_cursor`.
One connection may own one subscription; another request on that connection is rejected as `already_subscribed`.
Catch-up captures the response's high-water cursor, streams only through that cursor on a bounded blocking worker, and queues later live events behind it.
The agent-runtime dispatcher remains available for event appends, watchers, and other control responses while catch-up runs.
Catch-up does not retain Workspace lifetime history in memory.
Automatic recovery checkpoints preserve the original sequence space: subscriptions transparently traverse immutable archives and never expose recovery-only checkpoint records.
Historical cursors remain valid after maintenance, and archive checksum or availability failures use the same `stream_error` contract as active-log failures.
History and live delivery both require contiguous sequence numbers and suppress any live sequence already covered by catch-up.
Corrupt, gapped, or unavailable history ends that subscription with a structured `stream_error` frame while leaving the connection available for ordinary requests.

## Direct Pane attach

Interactive Pane attach remains on binary protocol version 19 rather than NDJSON.
`uniterm agent attach PANE` opens one Pane with an initial full terminal snapshot followed by ordinary damage and cursor operations.
`--observe` makes the stream read-only, the default controller role claims input only when no controller exists, and `--takeover` explicitly revokes the previous controller to observer status.
Controller input uses the same bounded Pane input function as every other semantic input path.
Pane geometry stays server-owned; a direct client's resize signal requests a full repaint and never resizes the PTY.
Direct render output uses the existing bounded, supersedable client queue, does not receive Workspace chrome, titles derived from another Pane, or OSC 52 clipboard requests, and exits when its Pane closes.
The same binary frames pass unchanged through `uniterm remote HOST WORKSPACE --pane PANE` and the existing SSH byte bridge.

## Runtime boundary

The asynchronous socket reactor, bounded writers, and event-log catch-up live on the agent-runtime side.
Requests cross to the mio core as `AgentToCore::ControlRequest`, and responses cross back as `CoreToAgent::ControlResponse`.
Only the core reads grids or mutates pane state.
Connection state is never shared with the core, and every idle worker blocks on a socket or channel without a timer.
Accept failures arm a short recovery backoff only while the failure persists; exhausting descriptors does not permanently stop the acceptor.

## Today and restricted managers

`timeline_query` accepts `date` (`today` or YYYY-MM-DD), optional `filter`, and exclusive `before` sequence.
Its `timeline` result contains at most 128 observations, a whole-day Project summary (capped at 256 with an explicit truncation flag), local UTC bounds/time labels, `current_sequence`, `next_before`, and partial-coverage metadata.
`parent_invocation` on an agent observation links a regular Pane agent to its explicit Uniterm launch anchor, using durable invocation sequences across providers.
It is distinct from provider-reported `parent_session_id`, which may describe a harness-internal child sharing a Pane.
Today Flow draws cards only for observations with their own Pane invocation and uses these launch links; old history without links remains unconnected.
`AgentDelegated` events carry `pane`, `invocation`, `parent_invocation`, and the captured launch `context` and are available on metadata-only subscriptions.
`timeline_note` records deliberate Project annotations; `timeline_prompt` is explicit semantic submission and retention, available only in protected storage.
`timeline_focus` requires a Pane and its invocation start sequence; `timeline_resume` resolves an observation's provider resume profile and starts a new background Tab after validation.
`timeline_open` shows the high-level view.
`timeline_manager` names a provider and Project, with explicit `manage` and `background` flags.

A second socket at `manager/<workspace>.control.sock` permits only `timeline_query`, `agent_list`, `capabilities`, and `subscribe`.
It excludes private note/prompt text, detection evidence, terminal output, and resume arguments.
Subscription entries excluded from metadata sharing become `{ "redacted": true }` while retaining their original sequence, timestamp, and Workspace envelope.
The inherited virtual `manager/<workspace>.sock` path lets ordinary control CLI commands select this endpoint without exposing binary input commands there.
The ordinary OS user can still access other sockets and files; this is a server capability boundary, not an operating-system sandbox.
See [Today and local history](USAGE.md#today-and-local-history) and [History privacy and recovery](USAGE.md#history-privacy-and-recovery) for explicit provider sharing, storage protection, and deletion semantics.

## Tab stacks and pane display modes

Servers advertise `tab.stack`, `pane.layout`, and `agent.launch.stack` for these additions.
`file_sort` takes `sort` (`name` or `recency`) and changes the Workspace's Files ordering through the same command as the header toggle and `ut files sort`.
Both orders keep folders first; the Workspace definition records the choice, and repeated selection is a no-op.

`pane_layout` takes a stable `pane` id and `mode` (`split`, `dwindle`, or `scrolling`).
`tab_stack` takes a child `pane` id and nullable `parent` Pane anchor.
Both act on the owning Tab, preserving the human's selection; invalid parents, cross-Project links, and cycles return `accepted: false`.
`agent_launch` accepts `target: "NewStack"`, an explicit `pane` anchor, and `background: true`.
Pane snapshots include `stack_parent` and `layout_mode`.
Project `direction` also accepts `Top` and `Bottom`, retaining the other Projects' relative order.

## Task launch and management

`task_start` shares New Task's validation, provider-specific interactive arguments, Project selection, and durable attempt ownership.
A successful mutation response contains the new Task id; a refusal has code `launch_refused` and creates no Task or Pane.
Use `options.save` to record a planned task without starting an agent.
`options.source_task` links a new attempt to an existing Task without overwriting its run outcome.

```json
{"version":1,"id":20,"workspace":"default","method":"task_start","params":{"launch":{"prompt":"Review the parser","agent":"codex","project":"Project B","options":{"target":"NewWindow"}}}}
{"version":1,"id":21,"workspace":"default","method":"task_notes","params":{"id":1,"notes":"Acceptance criteria: preserve existing behavior"}}
```

Optional `workflow`, `relay`, and `role_providers` fields select the existing guarded orchestration paths.
The optional `model` and `profile` fields inside `options` are single-agent overrides and are rejected when the selected provider does not support them.
Native approval behavior is never changed by task launch.
`task_list` preserves exact durable launch context, recency sequence, Project, linked run, live agents, waiting items, and artifact summaries for small histories.
If its encoded task payload exceeds 256 KiB it returns `result_too_large`; use `task_page` and `task_detail` instead of retrying the unbounded request.
`task_page` accepts a `query` object with `offset`, `search`, `filter` (0 all, 1 active, 2 planned, 3 finished), `project_only`, and `recent`.
Its `items` are summaries with at most 256 characters per text field, without full launch prompts or execution details; `total` counts matches before paging, and `query.offset` identifies the returned page.
All filters apply across retained history before the fixed 32-item page is selected.
`task_detail` accepts `id`, byte `offset`, and optional `revision`; its `chunk` contains at most 32 KiB of JSON-encoded TaskEntry bytes plus the total byte length and revision.
Start at offset zero, concatenate each `bytes` array, and pass the first reply's revision on subsequent requests until the total is reached.
A `stale_revision` response requires restarting at zero; no durable text is truncated.
The detail payload includes the same exact fields as a small `task_list` entry.

```json
{"version":1,"id":22,"workspace":"default","method":"task_page","params":{"query":{"offset":32,"search":"parser"}}}
{"version":1,"id":23,"workspace":"default","method":"task_detail","params":{"id":1,"offset":0}}
```

Attach wire version 25 uses bounded summary pages plus demand-driven immutable detail chunks, sharing encoded detail snapshots between readers.
A view finishes an in-flight detail transfer before refreshing to a newer event revision, so active agents cannot starve large transfers over SSH.
The interactive Tasks subscription is event-driven and stops when the view closes; control clients can continue using the existing event subscription contract.

## Permission automation

`auto_approve` queries or changes a Workspace's session-only permission automation.
Omit `enabled` or use null to query, set false to disable, or set true to enable immediately.
The legacy `confirmed` field is accepted for compatibility and has no effect on this toggle.
The response is `AutoApproval` with `enabled` and an optional `error`.
`waiting_act` also accepts `action: "Approve"` for one recognized permission dialog.
Both automatic and manual recognition revalidate the current invocation and PTY owner, and leave unknown dialogs waiting.
Capabilities are `agent.auto_approve` and `waiting.approve`; see [permission automation](28-permission-automation.md).

## Ephemeral Project inbox

Capability `inbox.ephemeral.v1` exposes the `inbox` method.
Parameters are `project` (optional Project id), `identity` (optional `{pane, invocation, foreground_pid}`), `epoch` (optional generation), and tagged `operation` (`{op: ...}`).
Operations are `discover`, `list`, `watch`, `post`, `ack`, `claim`, `release`, `pause`, `clear`, `revoke`, and `credential`.
`clear` carries `confirmed: true` from its caller; an unconfirmed request changes nothing.
An operation referencing existing IDs or a nonzero cursor must include the epoch from discovery/list.
A response has type `inbox`; its data contains epoch, Project, authenticated author, page, optional mutation id and guidance.
The page exposes entries, cursor, retention floor, more and paused.

```json
{"version":1,"id":1,"workspace":"work","method":"inbox","params":{"project":1,"identity":null,"epoch":null,"operation":{"op":"discover"}}}
{"version":1,"id":2,"workspace":"work","method":"inbox","params":{"project":1,"identity":null,"epoch":null,"operation":{"op":"post","kind":"proposal","body":"Compare both approaches using the latency benchmark.","reply_to":null,"to":null,"task":null,"key":"approach-v1","ttl_ms":86400000}}}
```

The separate owner-only `inbox/<workspace>.control.sock` endpoint requires a current invocation and allows only inbox operations and `agent_hook_context`.
Normally the socket peer must belong to the foreground process tree.
A kernel-verified invocation can request `credential`, receiving an `inbox_credential` result containing `project` and a secret `token`.
A trusted detached backend may supply that token as `identity.capability` together with `identity.capability_project`; the runtime verifies the grant and the core still checks current invocation and Project ownership.
`ut inbox credential` emits the complete secret identity JSON for `UNITERM_INBOX_IDENTITY`; it must never be included in model context or board messages.
Grants are memory-only and cannot survive revocation, invocation replacement, Project removal, or restart.
The full control socket permits human administration without an invocation; the metadata-only manager endpoint denies inbox access.
Watch is one bounded long poll per connection (maximum 300 seconds), not the durable Workspace subscription stream.
Explicit errors include `epoch_required`, `epoch_mismatch`, `cursor_expired`, `idempotency_conflict`, `revision_conflict`, `already_claimed`, `task_already_claimed`, `revoked`, `paused`, `capacity`, `thread_budget`, and `rate_limited`.
See [the inbox contract](29-project-inbox-proposal.md) for all fields and retention semantics.

## Pane moves

`pane_move` (capability `pane.move`) accepts `pane`, `project`, and an optional 1-based `tab`.
An absent or null `tab` creates a new Tab from a split Pane.
`ut pane move PANE PROJECT TAB` and `ut pane move PANE --new-tab` share the context menu's move implementation.
Moves preserve the PTY process, focus the destination and close an emptied source Tab.
Both source and destination must belong to the supplied Project; detached Workspace services cannot move.
Same-Tab moves, solitary-Pane new-Tab moves and stale targets return `accepted: false` without modifying the layout.
The read-only manager endpoint rejects this mutation, including for Overwatch.
