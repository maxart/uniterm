# Workspace navigation and Overwatch

This is the implementation and acceptance record for the navigation and Overwatch features released in v1.2.16.
The application version stays unchanged until the human tests the result and authorizes a release.
Implementation status and test entry points live in [STATUS.md](STATUS.md).

## Product scope

1. Automatic Tab names are off by default.
   Explicit names always win; an agent Tab uses its current invocation's summary and an ordinary terminal uses its working directory or foreground command.
   Names update from existing events and never require a process-table scan or a model request.
2. Find Tabs and Agents is a separate opt-in search surface for Projects, Tabs, Panes, providers, paths and branches.
   It searches only the current Workspace and uses stable Pane targets, current status and a bounded terminal preview.
   Its subscription exists only while the surface is open.
3. Graph is an opt-in Today view of ordinary agents and their recorded relationships, with durable run relationships, role activations, review outcomes and artifact evidence when available.
   It distinguishes current liveness from historical results, queries historical attempts on demand and never fabricates a successful review from an idle status.
4. Overwatch owns one chosen provider invocation per Workspace, with explicit OFF/ON state, a native interactive chat, metadata access and an optional management grant.
   Uniterm owns lifecycle and recovery; the provider interprets questions and uses structured Workspace APIs for evidence.
5. Local and Servers selectors remain in the Project sidebar.
   Saved SSH profiles and explicit discovery expose remote Workspaces, preserve Local as the return destination and use the existing SSH bridge.
   See [saved Servers](32-saved-servers.md) for the connection contract.

## Native agent discovery

Graph is useful for ordinary detected agent sessions before a workflow or relay exists.
The shared process, screen and cooperative event paths work across providers; Uniterm-launched children additionally record explicit invocation ancestry.
Provider-specific native session readers supplement that shared path without making hooks a prerequisite on supported hosts.

Codex discovery follows explicit native thread ancestry across the session store, including already-created descendants under other dates.
Claude discovery validates conversation and subagent identities against native metadata and uses spawning metadata for immediate parent links.
Pi-family native headers can identify root sessions; providers without a supported native child format use explicit cooperative relationships instead.
Linux can associate a manually started root with its provider-owned writable transcript descriptor; other hosts can use cooperative session identity and path reporting.
A shared directory, recent file, message parent ID or idle screen never establishes an agent relationship or successful completion.

Known work labels come from explicit summary metadata and provider task titles, separately from current status.
Native child labels use only explicit provider metadata; a parent's task is not assigned to its child.
The index carries bounded work labels through status updates and history, clears them when the session changes, and does not retain arbitrary prompt or result content.
Missing task labels, parents, liveness and completion evidence remain unknown.

Graph's Agents and Runs/evidence modes share a compact tree renderer and a docked selected-item inspector.
One terminal row represents one observed agent, retained run or role, with continuous branches, state-colored markers and right-aligned status/evidence counts.
The selected ancestry is emphasized without turning launch/session relationships into task dependencies.
The inspector preserves Project/path, provider/work and invocation or activation facts; full history retains attempts, reviews and artifacts through `v`.
Artifact presence and role handoff never establish mechanical verification.
`server::today::graph` tests export real Grid fixtures at 140x40, 80x24 and the narrow, tall 52x48 viewport, and check exact click rows, Unicode clipping and zero bytes on unchanged redraws.

Filesystem discovery batches at most 128 directory entries per continuation, retains at most 1,024 unresolved headers and 4,096 cached paths, and drains its work queue before sleeping.
Newly reachable parents can trigger bounded reconciliation of evicted candidates; relevant descendants are not silently cut off at the unresolved-cache bound.
There is no periodic scan or idle wakeup timer.

## Overwatch contract

Open **Workspace > Overwatch**, **Settings > Overwatch**, the configured `overwatch` binding, or prefix-O.
Select a harness, then choose **Model** to search its native model catalog by friendly name or exact identifier.
Choose a model, then choose its advertised reasoning effort or variant; **Harness default** leaves that choice to the harness.
For example, choosing **GPT-6-Luna**, then **medium**, sends Codex the canonical `gpt-6-luna` identifier and a separate `model_reasoning_effort` override.
The **Effort** settings row edits the current model's effort directly.
The picker applies both choices together; cancelling either stage changes nothing.
**Harness default model** clears both overrides, and **Custom model ID** remains an advanced fallback when a catalog is unavailable.
Cached catalogs show their source and timestamp; **Refresh** requests a new metadata snapshot, with no background polling.
Discovery runs outside the terminal render loop, with bounded processes and output, and sends no user prompt or billable model turn.
Native harness startup may maintain its own local caches.

| Harness | Model and effort source | Launch selection |
| --- | --- | --- |
| Codex | Visible native cached models, or app-server `model/list`; per-model reasoning levels | Canonical model ID plus `model_reasoning_effort` |
| Claude Code | Native SDK initialization model metadata and supported effort levels | Model alias/ID plus `--effort` |
| Grok / Grok's `agent` alias | Whitelisted native cached model and effort metadata, or native model list | Model ID plus `--reasoning-effort` |
| Cursor Agent | Native `--list-models` / `models`, preserving exact variant IDs | Native model ID; separate effort is unavailable |
| Pi | Native RPC available models and per-model thinking maps | `provider/model` plus `--thinking`; extension-only discovery is excluded |
| OpenCode | Version-aware native model catalog and variants | v1 model/variant flags; v2 `mini` interactive interface with `provider/model#variant` |

A missing capability is shown as unknown, never guessed from the model name.
Unsupported separate effort controls explain that the native model variant or harness default controls the behavior.
Executable discovery distinguishes Grok's and Cursor's shared `agent` basename by executable identity.
Model lists are bounded to 256 entries; credentials and raw provider configurations never enter the picker or event log.
Adapter contracts were checked against installed harnesses and the official [Codex model-list protocol](https://learn.chatgpt.com/docs/app-server#models), [Claude SDK model metadata](https://code.claude.com/docs/en/agent-sdk/typescript), [Grok CLI reference](https://docs.x.ai/build/cli/reference), [Cursor CLI parameters](https://cursor.com/docs/cli/reference/parameters), [Pi RPC implementation](https://github.com/earendil-works/pi/tree/main/packages/coding-agent/src/modes/rpc), and [OpenCode models](https://opencode.ai/docs/models).

Harness/provider and Model remain editable in Overwatch settings while ON.
Applying a changed choice interrupts the current response and starts a new chat, rebuilding context from durable Workspace facts rather than transferring the old conversation.
The fourth tab stays visible and the selected Project, ordinary terminal and access grant stay unchanged.
Selecting the current harness or model does not restart or append an event; choosing a different harness clears the previous model override to its default.
Invalid model values are rejected before stopping the current chat.
If the selected harness cannot start, normal bounded recovery and attention status explain the failure; choose another harness or Retry recovery.
The chosen model, effort and provider launch capability survive exit recovery, native session restore, clean restart and event-log compaction.
The agent runs in a Workspace-owned terminal inside the fourth right-panel tab, Overwatch.
The Overwatch tab appears only while Overwatch is ON, including startup, recovery and attention states.
Turning it OFF removes the tab and returns an active Overwatch panel to Agents; Files and Web selections stay selected.
It does not create a Project Tab or change the selected Project or terminal.
The management grant can change only while OFF.
Selecting a provider shares Project paths, agent identity and Workspace activity metadata with that provider.
Read-only mode excludes raw terminal content, retained prompts and notes from its inherited Uniterm endpoint.
This endpoint is a capability restriction, not an operating-system sandbox around the provider process.
Management enables the normal Workspace API, including private content access; existing destructive and bulk confirmations still apply.

```sh
ut overwatch configure PROVIDER PROJECT_ID --model MODEL
ut overwatch model MODEL
ut overwatch model "" # provider default; restarts chat if ON and changed
ut overwatch select codex gpt-6-luna --effort medium # selected harness must match
ut overwatch on
ut overwatch status --json
ut overwatch chat
ut overwatch context --json
ut overwatch off
ut overwatch configure PROVIDER PROJECT_ID --manage
ut overwatch retry
```

The API operation vocabulary is shared by CLI, menus and keyboard controls.
The selected provider uses `ut overwatch context`, `ut agent list`, paged `ut today list`, and cursor-based `ut today watch` for facts.
Current inventory carries a sequence; historical claims cite event sequences and preserve missing-coverage indicators.
The inventory returns at most 64 typed metadata records per page: Projects, Tabs, Panes, agents, Tasks, Runs, Roles, Artifacts and waiting items.
Follow `next_offset` with `ut overwatch context --offset N --sequence SEQUENCE --json`; `--limit N` requests a smaller page.
A changed Workspace returns `stale_context`, requiring a new first page instead of mixing records from different event cursors.
`truncated_text` explicitly marks bounded labels and paths, and `private_content_excluded` marks the exclusion of prompts, notes, terminal evidence, transcripts, question text and private turn tokens.
An open Overwatch menu receives event-driven status updates while preserving its query and selection.
Updates never reopen a closed menu or open another client's menu; an invalid action returns a transient notice without changing durable supervisor state.
Project visits answer navigation questions separately from observed agent activity.
Untrusted historical text cannot authorize management actions.

Lifecycle state, provider choice, permissions and recovery attempts are events in the owning Workspace log.
Compaction retains this projection and a clean stop does not discard it.
A saved PID or Pane alone never proves a live invocation survived a restart.
Restore reuses the existing native-resume path when a saved provider session is available.
Unavailable restored chats remain inspectable and report repair instructions.
After an observed exit, recovery uses delays of 1, 4 and 16 seconds, then stops with Attention.
The retry budget survives server restart and resets only through explicit enable/retry.
No timer remains armed for an OFF or healthy idle assistant.
A startup that remains Starting for 60 seconds reports Attention so authentication and permission prompts remain under human control.
Recovery does not answer permission prompts or change providers automatically.
Fresh replacement conversations rebuild Workspace facts; they must not claim to retain conversation text that the provider cannot restore.

## Engineering boundaries

Pure policy lives in uniterm-core, wire vocabulary in uniterm-proto, provider discovery and disk/network work in the runtime, and semantic mutations in the mio-owned server.
All new rendered content uses existing theme, geometry, clipping and terminal sanitization boundaries.
Changed cells drive rendering; unchanged snapshots produce no terminal output.
The binary wire protocol advances to 34, the local event schema to 13, and the snapshot schema to 15 so incompatible older readers refuse new state.
Schema-14 snapshots migrate the former Project-owned assistant into the detached Workspace terminal.
This is not an application release or a request to restart the user's Workspace.

## Acceptance gates

- Defaults preserve existing behavior, with explicit settings and empty states.
- Manual names and invocation replacement never inherit a stale automatic summary.
- Search targets remain stable across Project/Tab reorder and closure.
- Graph history is bounded, cited and separate from current liveness.
- Overwatch duplicate enables are idempotent, OFF cancels delayed work, ownership changes cannot close an unrelated invocation, and read-only endpoints deny mutations.
- Overwatch recovery, clean restart and compaction retain configuration and bounded retry policy.
- Remote disconnects preserve sessions; reconnect and return Local use explicit actions and preserve SSH host trust.
- First attach, narrow terminals, Unicode, click targets, theme, no-op rendering and isolated Workspace state are covered.
- Workspace build, debug and release tests, warning-free Clippy, formatting and relevant performance checks pass before the features are handed over for testing.

## Removing Projects without recorded activity today

A management-enabled Overwatch can act on a request such as "remove all Projects we have not worked on today".
It uses the deterministic cleanup preview rather than guessing from terminal text or a partial history page.

```sh
ut overwatch prune --preview --json
ut overwatch prune --apply SEQUENCE --date YYYY-MM-DD --projects ID,ID --confirm --json
```

The preview reports exact stable Project IDs, names, Pane counts, retained reasons, the local date and the evidence cursor.
"Worked on" means a recorded Uniterm Project visit, note or agent observation today; work outside Uniterm is not observed.
The current Project, active or uncertain agents and managed worktrees are retained.
Overwatch has no owning Project; its optional launch-context Project can be removed without stopping the assistant.
The day-wide Project summary is independent of timeline observation pagination.
A truncated summary or unhealthy event log refuses cleanup.
Before applying, the server reads the day again and requires the same date, cursor and complete candidate set.
Any intervening Workspace event requires a fresh preview.
The control caller supplies the human's explicit bulk confirmation; the server records guardrail decisions before closing the first Pane.
Removal closes the candidate Projects' Panes and removes their navigation entries while preserving their source directories and history.
Read-only Overwatch can preview this action but cannot apply it.

Settings > Agents > Overwatch shows ON/OFF, lifecycle state, provider, model, effort, access and recovery attempts.
The status updates from Workspace events while Settings is open.
The Open Overwatch button opens the shared controls.
The Project sidebar contains only the shared Local/Servers tab strip at its bottom.
With session restore disabled, saved Overwatch configuration remains available but automatic activation is reset to OFF.

## Local preview

From the repository root, launch the freshly built binary with separate preview state and configuration:

```sh
preview_root=$(mktemp -d /tmp/uniterm-preview.XXXXXX)
mkdir -p "$preview_root/state" "$preview_root/runtime" "$preview_root/config/uniterm"
cat > "$preview_root/config/uniterm/uniterm.conf" <<'EOF'
auto-install-connectors = false
auto-update-connectors = false
auto-tab-names = true
switcher-enabled = true
run-graph = true
EOF
env -u UNITERM_SOCKET -u UNITERM_PANE_ID \
  XDG_STATE_HOME="$preview_root/state" \
  XDG_RUNTIME_DIR="$preview_root/runtime" \
  XDG_CONFIG_HOME="$preview_root/config" \
  ./target/debug/ut next-preview
```

This enables the three optional navigation features in the preview configuration.
Use prefix-S for Find Tabs and Agents, prefix-G for Graph, and prefix-O for Overwatch controls.
The sidebar's Servers control opens saved SSH profiles; a remote host must run this same wire-protocol build.
Configure an installed provider in Overwatch, enable it, and open its chat to exercise Workspace questions and management.
Settings > Agents > Keep auto-approve on restart controls persistence separately from the Auto-approve ON/OFF choice.
Keep the preview root path to restart the same preview Workspace with the same environment and test saved settings.

## Direct work in Tasks

**Settings > Agents > Auto-track tasks** is ON by default (`auto-track-tasks = true`).
An accepted direct user prompt creates a normal durable Task for its Project, Pane, invocation and native session.
Follow-up prompts join unfinished work; a prompt after an explicitly finished response creates a new Task.
Hooks and native transcript reports are deduplicated, including delayed reports from an older turn.
A New Task launch is adopted without making a second record, and workflow/relay role prompts keep their existing orchestration ownership.
Overwatch's own Workspace-service chat does not create Tasks for ordinary management questions.

Running/tool activity marks tracked work running, and permission/question/error/exit observations can mark it blocked.
Idle, silence and process exit never mark a Task finished.
An explicit provider response-end event can mark it finished; that records the end of a response, not proof that the requested objective succeeded or was verified.
Tasks explains this distinction in its details.
Manual status edits take precedence over later automatic updates.
Task titles and notes remain editable through the existing UI and API.

Tasks retains submitted text and bounded follow-up history locally, using the same event log and protected-storage behavior as existing Tasks.
Each captured prompt is bounded to 16 KiB and combined retained history to 64 KiB, with a visible truncation indicator.
Native discovery starts at the owned transcript's current end and does not import historical conversations.
If discovery happens after the first prompt, that prompt may be unavailable; an observed work summary or supported task title still creates a labelled Task without inventing its prompt.
See [provider observation coverage](06-agentic-supervision.md) for the hook and transcript paths.
Disabling Auto-track tasks stops new capture and automatic changes; existing Tasks remain available.
After an observation gap or recovery, unidentified old work is kept separate from a newly identified conversation instead of guessing its owner.

Graph consumes the same invocation identity and durable Task observations, so direct work labels and Task references appear in its history without requiring a workflow.
Overwatch's metadata context includes these Tasks and their statuses through the existing paged inventory.
A management-enabled Overwatch uses the normal `task_page`, `task_detail`, `task_set_status`, `task_retitle`, `task_notes` and `task_delete` operations; read-only mode retains its existing restrictions and excludes full prompts and notes.
