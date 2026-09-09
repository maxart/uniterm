# Neutral Control API

## Purpose

The control API is Uniterm's provider-neutral local automation contract.
It observes and mutates server-owned resources without pretending to be an interactive terminal client.
Terminal attach remains on the versioned binary protocol.

## Transport and discovery

Each running Workspace owns `<socket-dir>/<workspace>.control.sock` beside its ordinary `<workspace>.sock` attach socket.
The control socket is an owner-only Unix socket with mode `0600` inside an owner-only directory.
Every request and response is one UTF-8 JSON object followed by a newline.
Protocol version 1 accepts at most 1 MiB per request line or output frame, 128 concurrent connections, 64 pending requests across the Workspace, and 64 queued output frames per connection.
A connection that would exceed the bounded request intake is disconnected.
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

`project_remove` and `agent_stop_all` close every Pane they reach, so they require `"confirmed": true`; the server records the guardrail decision before the first Pane closes and answers an unconfirmed request with the `confirmation_required` error.

`ut tab new` and `ut tab rename` are thin CLI verbs over the `tab_create` and `tab_rename` methods, so a script and a control client see the same result.
The implemented surface exposes `capabilities`, Workspace and hierarchy snapshots, Project and Tab mutations, Pane read, send, and focus, Agent list, launch, focus, and stop, Task mutations, waiting-item actions, instruction operations, worktree lifecycle operations, native run-graph and artifact inspection, native orchestration launch and submission, worktree-backed child Run launch, and event subscriptions.
`pane_send` accepts a UTF-8 `text` string so hand-written JSON uses ordinary JSON escapes instead of an integer byte array.
Pane mutations use the same bounded pane-input function as the binary client protocol.

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
History and live delivery both require contiguous sequence numbers and suppress any live sequence already covered by catch-up.
Corrupt, gapped, or unavailable history ends that subscription with a structured `stream_error` frame while leaving the connection available for ordinary requests.

## Direct Pane attach

Interactive Pane attach remains on binary protocol version 13 rather than NDJSON.
`uniterm agent attach PANE` opens one Pane with an initial full terminal snapshot followed by ordinary damage and cursor operations.
`--observe` makes the stream read-only, the default controller role claims input only when no controller exists, and `--takeover` explicitly revokes the previous controller to observer status.
Controller input uses the same bounded Pane input function as every other semantic input path.
Pane geometry stays server-owned; a direct client's resize signal requests a full repaint and never resizes the PTY.
Direct render output uses the existing bounded, supersedable client queue, does not receive Workspace chrome, titles derived from another Pane, or OSC 52 clipboard requests, and exits when its Pane closes.
The same binary frames pass unchanged through `uniterm remote HOST WORKSPACE --pane PANE` and the existing SSH byte bridge.

## Runtime boundary

The blocking acceptor, connection readers, bounded writers, and event-log catch-up live on the agent-runtime side.
Requests cross to the mio core as `AgentToCore::ControlRequest`, and responses cross back as `CoreToAgent::ControlResponse`.
Only the core reads grids or mutates pane state.
Connection state is never shared with the core, and every idle worker blocks on a socket or channel without a timer.
