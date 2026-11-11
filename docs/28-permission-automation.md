# Workspace permission automation

The mode is off by default and resets when the Workspace server restarts unless **Keep auto-approve on restart** is explicitly enabled in Workspace Settings.
Enable it only when the human authorizes automatic acceptance of recognized permission requests across that Workspace, including requests with destructive effects.

## Controls

Open **Workspace > Settings > Connectors** under Agents, **Agents > Config...**, or the Agents rail's **Config** button.
Workspace Settings has its own provider status list and **Install all connectors** action.
Each provider row shows its connector status: `ready`, `missing`, `update`, or `n/a`.
The detail pane retains CLI availability, running count, launch actions, and connector installation or removal.
Press `I` or click **install all** to install or refresh supported connectors for installed CLIs.
Global configuration files remain in the **Global files** tab.

Press `A` to toggle auto-approval immediately.
Enabling and disabling require no second confirmation.
Both Settings and Agents Config include a dedicated **Auto-approve** item in the left list.
Its page shows an authoritative **on / off** selector, separated by blank space from behavior and scope details.
Selecting **on** enables the mode immediately; selecting **off** disables it, and selecting **off** also revokes per-pane delegation even if Workspace mode is already off.
The dialog title shows the mode; the Agents rail's button always remains **Config**.
Other attached clients receive mode changes, and a newly opened Config dialog reads the authoritative mode.

```sh
ut agent auto-approve status -w work
ut agent auto-approve on -w work
ut agent auto-approve off -w work
ut waiting approve 17 -w work
```

The explicit `on` CLI command and API `enabled: true` request both enable the mode directly.
The legacy `confirmed` wire field is accepted but ignored for this setting.
Use the `agent.auto_approve` and `waiting.approve` control capabilities to discover support.
The status response includes `enabled` and `persist`; CLI status explains the corresponding restart behavior.
The Settings preference `persist-auto-approve` defaults to false and is stored in machine configuration.
It never changes the live ON/OFF mode by itself.
When true, ON stays on indefinitely until explicitly turned OFF, and OFF also survives restart.
When false, the next server start records and applies OFF.
Each Workspace owns its mode independently, including Workspaces sharing a config file.
The preference applies independently of terminal `restore`.

```json
{"version":1,"id":1,"workspace":"work","method":"auto_approve","params":{"enabled":null}}
{"version":1,"id":2,"workspace":"work","method":"auto_approve","params":{"enabled":true}}
{"version":1,"id":3,"workspace":"work","method":"auto_approve","params":{"enabled":false}}
{"version":1,"id":4,"workspace":"work","method":"waiting_act","params":{"id":17,"action":"Approve"}}
```

## Per-pane delegation

`ut delegate arm PANE` authorizes only that current invocation, using this same approval pipeline.
It captures `delegation-max-answers-per-hour` (default 30, maximum 240), counts accepted submissions, and revokes itself immediately on exhaustion.
A Pane arm's budget applies even with Workspace mode on.
Explicit disarm and budget exhaustion exclude that invocation from Workspace automation until re-arm or replacement.
Manual reviewed approval remains available.
Explicit **off** in either UI, CLI, or API revokes all Pane arms, even if Workspace mode was already off.
Selecting **on** does not override a Pane's explicit exclusion.
There is no separate persistent Yolo setting and no automatic re-arm after restart.
The Observatory marks armed Panes, and `ut delegate status` / `ut delegate explain` expose scope and reasons.

## Recognition and delivery

Automation acts only on a known current agent invocation with kernel-confirmed ownership of the PTY.
It recognizes complete permission dialogs directly on output or identity discovery, without waiting for the status dwell used by notifications and the waiting queue.
Enabling the mode also inspects already-visible dialogs, including agents that have not reached `Permission` status.
Manual `waiting approve` still targets an existing permission item and shares recognition and delivery with automatic approval.
Local detection manifests cannot grant input authority.

Provider contracts recognize permission headings, affirmative and negative choices, and interaction hints.
Codex folder trust includes **Trust and continue** and **Back to Agent Command Center**, matching the supplied screenshot.
Claude folder trust can default to **No, exit**; the recognizer moves down to **Yes, I trust this folder** before confirming.
Grok/Agent's numbered radio layout accepts the selected **Yes, proceed** option, without enabling its persistent grants.
OpenCode uses live cell styles to distinguish **Allow once** from other buttons, supporting both two-button and three-button layouts.
Codex and Claude tool contracts also cover file edits, network access, and existing terminal input where supported by the provider.
Pi project trust and Gemini one-time command choices use provider-specific or shared dialog contracts.
Other bundled providers retain the common complete-dialog matcher; custom providers and unmatched UI versions remain waiting.
Questions, arbitrary choices, persistent tool allow rules, and policy denials are not approved.
Prompts that require typing a literal `y` or `yes` without a recognized selection dialog are not supported.
Folder trust is an explicit exception to one-time tool permission: the provider may remember it on disk.

The server sends Enter for a selected affirmative choice.
When navigation is needed, it sends one arrow and waits for the changed screen before deciding the next step; it never batches navigation with confirmation.
An explicit manual approval also completes its pending navigation with auto mode off; disabling auto mode stops pending automatic navigation.
It does not send a blind fleet-wide keystroke.
Queued user input takes precedence.
The existing arbitrary-text `waiting answer` command remains available for an explicitly reviewed response.

The last delivered prompt fingerprint is scoped to an invocation, rather than a reusable Pane id.
An unchanged repaint or off/on toggle does not repeat the key.
A changed request can be handled even if status remains `Permission` between prompts.
An identical successive request needs observable progress between prompts; without that evidence, it stays waiting instead of risking a duplicate submission.
The waiting item remains until provider progress is observed: queued input is not proof that approval succeeded.
Terminal text is heuristic evidence, not an authenticated approval API, so provider UI changes and misleading screen content remain limitations.

## Runtime, audit, and recovery

Pure opt-in and deduplication state lives in `uniterm-core::approval`.
Provider recognition lives under `uniterm-server::providers`; the core server revalidates the PTY and delivers input.
There is no polling, external scraper, retry timer, subprocess, or idle wakeup.
Disabled mode checks only whether an explicitly requested manual navigation is pending on actual output events.
Status changes inspect a live dialog only when a previous delivery needs progress validation.
Enabled mode samples a known agent only on output, identity/status transition, or explicit enable.

Event schema 10 records `AutoApprovalChanged`, `PermissionApprovalAttempt`, and `PermissionApprovalDelivery` before projecting authorization and after queue acceptance respectively.
Approval audit entries contain invocation and Pane identity, not another copy of command text.
The ordered persistence worker syncs each mode-change event to stable storage; no I/O is added to permission recognition or idle processing.
Mode commands change live behavior immediately; their replies do not acknowledge a completed storage sync.
Crash recovery uses the last committed mode-change event.
Existing waiting and terminal history retention still applies.
Each startup records its effective mode: OFF by default, or the latest Workspace mode when `persist-auto-approve` is true.
Schema 11 recovery checkpoints retain the latest `AutoApprovalChanged` value, so compaction, clean shutdown (which removes terminal snapshots), and crash recovery preserve both ON and OFF.
Older checkpoints without this field default to OFF.
Recovery reads only the active checkpoint and event suffix; it does not scan archived history.
Delivery fingerprints and pending navigation remain invocation-local and are never restored.
The preference adds no timer or idle work.
Known durability failure suppresses automatic and manual recognized approval.

## Local verification

`providers::approval::tests` replays captured Codex, Claude, OpenCode, and Grok ANSI through the real VT parser, alongside the Codex screenshot transcription and Pi/Gemini dialog fixtures.
`tests/auto_approval.rs` exercises the real PTY-to-input path across provider identities while status is still `Working`, plus off mode, deduplication, stale ownership, restart revocation, and Workspace scope.
Its Codex case ends a 256-KiB output burst with folder trust, covering recognition after bounded reads drain.
`tests/auto_approval_persistence.rs` exercises the config/Settings/protocol/CLI path through real isolated server processes, clean restarts, SIGKILL crashes, ON/OFF compaction, Workspace scope, default revocation, opt-out, and independent terminal restore settings.
It reports end-to-end delivery latency; the matcher budget test includes grid extraction and negative screens.
The idle budget test runs with auto-approval enabled and requires no terminal output while idle.

On the local 2026-10-22 optimized benchmark, extracting and recognizing each 120-by-40 captured screen took 0.025-0.031 ms p95, including the negative-screen case.
The seven-provider PTY matrix delivered input in 11-36 ms during a concurrent build; these are individual delivery measurements, not a latency percentile.
The 315.8-second idle audit emitted zero terminal bytes, used 0.0020% CPU, and held 10,612 KiB RSS.
These local measurements complement the enforced five-millisecond recognition budget; they are not guarantees for every machine or future provider UI.
The original 2026-10-22 verification passed Workspace build, debug tests, optimized release tests, warning-denied Clippy, and formatting checks.
See [STATUS.md](STATUS.md) for combined validation of restart persistence.
The full optimized suite used `CARGO_PROFILE_RELEASE_LTO=false cargo test --workspace --release` to reduce test-binary link time; the recognition measurements above used the standard release profile.

Real agent checks run inside unprivileged Bubblewrap containers, with disposable homes and projects.
Claude's startup capture and saved-trust acceptance check used a temporary credential copy and no model prompt.
OpenCode's permission API and Grok's deterministic local fake model exercise their real dialogs without external network access, credentials, or token usage.
Codex folder trust is captured and accepted offline using a dummy local provider; the test checks its saved trust decision.
The supplied screenshot is covered separately to retain the Agent Command Center variant.
See `tests/fixtures/approval/README.md` for capture provenance and the reproducible offline harness.
