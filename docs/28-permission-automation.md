# Workspace permission automation

**Settings > Agents > Auto-trust Project directories** (`auto-trust-projects = true`) defaults ON and is independent of tool Auto-approve and Pane delegation.
It accepts a complete recognized trust dialog only when its displayed absolute directory exactly matches the owning Project root.
The setting persists in machine configuration; changes apply immediately in the current Workspace and are shared by attached clients.
Paths with missing, ambiguous, truncated, parent, unrelated, relative, or differently spelled roots remain manual, including symlink aliases and pathless native dialogs.
No filesystem resolution is performed on the terminal output path.
It still requires a bundled provider identity, current invocation, kernel-confirmed PTY ownership, healthy event history, and an exact affirmative and negative dialog contract.

General permission automation is off by default and resets when the Workspace server restarts unless **Keep auto-approve on restart** is explicitly enabled in Workspace Settings.
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
Directory trust uses its own setting and Project scope even when Auto-approve is ON; tool permissions require Auto-approve, a live Pane delegation, or an explicit waiting action.
Explicit waiting approval can still accept a reviewed trust dialog when automatic trust is disabled or its directory cannot be established.
Enabling the mode also inspects already-visible dialogs, including agents that have not reached `Permission` status.
Manual `waiting approve` still targets an existing permission item and shares recognition and delivery with automatic approval.
Local detection manifests cannot grant input authority.

Provider contracts recognize permission headings, affirmative and negative choices, and interaction hints.
A common command picker with a recognized command heading, one-time affirmative choice, denial and confirmation hint is supported for every bundled provider and executable alias.
Codex 0.171 can fold a long command above its options or wrap its persistent-grant option over the denial row.
Uniterm checks the highlighted native selection, the command preview and footer in that layout, then confirms the one-time action.
A short Pane can show a scroll arrow in place of the denial row.
Uniterm can still confirm a highlighted one-time choice when the command is visible, including a wrapped shell redirection.
Codex folder trust recognizes **Trust and continue** with either **Quit** or **Back to Agent Command Center**, including in tall Panes where Codex parks the cursor below its startup picker.
Codex can draw the picker before its input handler is ready.
For the native Codex folder and command pickers Uniterm waits 150 ms, checks the visible selection, policy and foreground owner again, then sends one navigation or confirmation key.
Folder trust also rechecks the owning Project root.
Claude folder trust can default to **No, exit**; the recognizer moves down to **Yes, I trust this folder** before confirming.
The native startup dialog gets a 150 ms readiness check for each selection.
Claude's **Teach auto mode about your environment?** consent is accepted from its complete three-choice modal, including when Claude leaves an inactive composer visible below it.
Uniterm checks the ordered consent options, confirmation footer and focus on the selected row, and allows time for Claude to start accepting input.
The subsequent environment-scope picker is reported as a question for the human.
Claude 2.1.330 Bash dialogs from built-in subagents retain their provider title and one-time choice when enterprise-managed settings add an ask rule and risk notice.
The title must be a whole row, either `Bash command` or `Bash command · from <agent>`, and the numbered choices must count down to `1. Yes` directly above the `Esc to cancel` footer.
Prose or typed text that only begins with those words is neither a title nor a status signal, and a question picker, an unknown numbered choice or text after the footer stays manual.
The accepted choices are a one-time Yes, an optional `Yes, and don't ask again` grant (bare, or scoped with `for ...` and at most one indented wrapped row), and No.
A titled panel that does not meet this contract is never handed to the shared `Do you want to proceed?` matcher.
A later dialog with a heading of its own, such as an edit, plan, command or folder-trust prompt below a stale title, keeps its own contract.
A screen without the title costs one scan and no allocation.
When that picker is clipped against the Pane bottom, the title, the proceed question directly above, and the focused `1. Yes` on the last grid row form the conservative approval contract.
The clipped form must stay visible for 500 ms before Enter is sent, so a frame split across PTY reads is never answered early.
Both forms share one fingerprint, so revealing the remaining rows cannot deliver a second Enter.
Grok Build recognizes its older numbered radio layout and the 1.0.92 layout with the global grant last.
Wrapped Grok commands use continuation rows in the framed preview.
Both layouts require a framed command preview, the selected **Yes, proceed** row, rejection options, and the native navigation footer.
When the installed `agent` executable resolves to Cursor Agent, its shell picker uses the displayed **Run (once) (y)** shortcut.
Cursor and Grok command delivery adds no readiness delay.
OpenCode uses live cell styles to distinguish **Allow once** from other buttons, supporting both two-button and three-button layouts.
OpenCode 2.4 can use identical foregrounds on every label, so the selected background distinguishes Allow once even when the right edge clips the confirmation hint.
Codex and Claude tool contracts also cover file edits, network access, and existing terminal input where supported by the provider.
Kimi's complete numbered approval panel accepts the one-time `1` shortcut even when the session-wide option is highlighted.
Its inline feedback editor remains manual.
Pi project trust and Gemini one-time command choices use provider-specific or shared dialog contracts.
Other bundled providers retain the common complete-dialog matcher; custom providers and unmatched UI versions remain waiting.
Questions, arbitrary choices, persistent tool allow rules, and policy denials are not approved.
Prompts that require typing a literal `y` or `yes` without a recognized selection dialog are not supported.
Folder trust is an explicit exception to one-time tool permission: the provider may remember it on disk.

The server sends Enter for a selected affirmative choice.
When navigation is needed, it sends one arrow and waits for the changed screen before deciding the next step; it never batches navigation with confirmation.
An explicit manual approval also completes its pending navigation with auto mode off; disabling auto mode stops pending automatic navigation.
If a native picker with a readiness delay or a recognized return-to-composer state remains visible after an automatic delivery, an explicit `ut waiting approve ID` can try that choice once more.
Queued user input takes precedence.
The existing arbitrary-text `waiting answer` command remains available for an explicitly reviewed response.

The last delivered prompt fingerprint is scoped to an invocation, rather than a reusable Pane id.
An unchanged repaint or off/on toggle does not repeat the key.
A changed request can be handled even if status remains `Permission` between prompts.
A second identical request needs observable progress between prompts.
Uniterm recognizes the return to the native Codex composer with a one-time approval receipt, Kimi’s normal input and toolbar, or the native Cursor, Grok and OpenCode composers, even when agent status did not change.
An unfinished synchronized terminal update cannot confirm dismissal or trigger another key.
The waiting item remains until provider progress is observed: queued input is not proof that approval succeeded.
Terminal text is heuristic evidence, not an authenticated approval API, so provider UI changes and misleading screen content remain limitations.

## Runtime, audit, and recovery

Pure opt-in and deduplication state lives in `uniterm-core::approval`.
Provider recognition lives under `uniterm-server::providers`; the core server revalidates the PTY and delivers input.
A provider that initially ignores keyboard input can request one deadline for a visible, authorized choice.
The server checks the live dialog again at that deadline and disarms on delivery or invalidation.
Recognition uses no periodic polling, external scraper, subprocess, or idle wakeup.
With general mode disabled, actual output is inspected for trust only when Auto-trust Project directories is ON, or to continue explicitly requested manual navigation.
When both settings are OFF and no manual navigation is pending, approval recognition does no work.
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

`providers::approval::tests` replays captured Codex, Claude, OpenCode, and Grok ANSI through the real VT parser, alongside Claude's auto mode consent, the Codex screenshot transcription, and Pi/Gemini dialog fixtures.
`tests/auto_approval.rs` exercises the real PTY-to-input path across provider identities while status is still `Working`, plus off mode, deduplication, stale ownership, restart revocation, and Workspace scope.
Its Codex case ends a 256-KiB output burst with folder trust, covering recognition after bounded reads drain.
`tests/auto_approval_persistence.rs` exercises the config/Settings/protocol/CLI path through real isolated server processes, clean restarts, SIGKILL crashes, ON/OFF compaction, Workspace scope, default revocation, opt-out, and independent terminal restore settings.
It reports end-to-end delivery latency; the matcher budget test includes grid extraction and negative screens.
The idle budget test runs with auto-approval enabled and requires no terminal output while idle.
The Workspace PTY test `workspace_auto_approve_delivers_one_time_command_choices_for_every_bundled_provider` checks delivery under all 17 bundled provider identities.
Native captures and executable checks distinguish each tested CLI layout from the common permission contract.

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

On 2026-11-26, authenticated tests ran a scratch shell script in isolated Workspaces through Cursor Agent (`agent` 2026.11.22-e9042b2), Grok Build 1.0.92, Claude Code 2.1.332, Codex 0.172.0, and OpenCode 2.4.2.
With tool Auto-approve OFF, the five native pickers held the command and the marker file was absent.
Enabling it produced the marker in each Workspace, and a second request for the identical command produced a second marker in the same invocation.
Elapsed time from starting the enable CLI to observing the first file was 68 ms, 39 ms, 185 ms, 178 ms, and 29 ms respectively, using local debug builds.
These single observations include the CLI, provider and shell execution.
The v1.2.28 verification checked two accepted delivery events in one invocation and both marker files for every CLI.
With the wrapped pickers already visible, Cursor and Grok passed the exact 93-byte argument to the scratch script in 59 ms and 31 ms respectively.
