# Combined release validation

The next candidate is v1.2.14, combining the Project inbox, compatibility diagnostic, release instructions, and cross-provider engineering guidance with v1.2.9.
Publication depends on the checks below and the observed live-agent results.

## Release gates

- Review every pending change, including new files, for scope, invocation identity, bounded retention, no-op behavior, and event-driven operation.
- Run the complete workspace build, debug tests, production-profile tests, warning-free Clippy, formatting, shell checks, export tests, and publisher tests.
- Run isolated inbox transport, competing claims, expiry, restart, credential revocation, restricted endpoint, and cross-Project tests.
- Validate history compatibility against copied fixtures, including old and unsupported schemas; prove the diagnostic does not modify its input.
- Run native binaries and integration executables in the existing disposable Omarchy VM and verify its identity and candidate hashes.
- Measure populated-board idle CPU/output and approval, rendering, and inbox performance.
- Check the final source snapshot, protocol/version consistency, public export, and platform release assets before publication.

## Live-agent evaluation

Use disposable homes, private Uniterm state, a synthetic project, and temporary copies of only the selected authentication material.
Never expose live Workspace sockets, the host home directory, or unrelated project files.
Destroy child processes on exit, including timeout and failure.
Keep rotated credentials only in an owner-only temporary cache between sequential runs; resolve newer login state before removing that cache.
Never overwrite a newer host login with an older test copy.
Network access is for the selected providers; prompts contain only the synthetic test project.

Run Codex, Claude, Grok/Agent, and OpenCode with real model prompts.
Start with at most eight short sessions and a fixed per-session deadline; use native turn and spending limits where available.
Do not retry failed authentication or capacity errors in a loop.
Record available token usage and spending separately from estimates; a time limit is not a hard token limit.

Compare two-worker tasks with and without the inbox using the same task, provider pair, deadline, and completion checks.
Use normal Uniterm discovery guidance for the inbox condition, without scripting the agents' inbox actions or feeding them expected command sequences.
Record whether each agent discovers the board, reads shared work, claims before editing, respects another claim, posts evidence, and hands work off.
Check work products independently instead of trusting an agent's completion statement.
Measure completed unique assignments, duplicate work, correctness, tool calls, elapsed time, usage, and human nudges.
Include an independent task where collaboration is unnecessary to detect coordination overhead.

A small paired sample can establish interoperability and expose workflow problems.
It cannot establish a statistically reliable productivity improvement.
Report successful cases, failures, and confounders separately; revise discovery or instructions when observed behavior warrants it, then rerun only the affected cases.

## Results

The evaluation ran on 2026-10-22 using a synthetic two-file Python project and an independent one-file control.
Every final work product passed its own external test.
No human nudges were sent during a measured run.
Raw logs remain outside the public repository; only sanitized results and reusable fixtures belong in the release.

### Observed collaboration

| Provider pair / scenario | Condition | Elapsed | Tool calls | Correct assignments |
| --- | --- | --- | --- | --- |
| Codex + Claude | Inbox | 24.74 s | 5 + 3 | 2/2 |
| Codex + Claude | Files baseline | 21.33 s | 5 + 3 | 2/2 |
| Grok + OpenCode | Inbox, concise guidance | 24.34 s | 5 + 6 | 2/2 |
| Grok + OpenCode | Files baseline | 48.66 s | 9 + 8 | 2/2 |
| Grok, independent | Inbox available | 15.25 s | 3 | 1/1 |
| Grok, independent | Files baseline | 15.16 s | 7 | 1/1 |

All four providers discovered the board, claimed work before editing, and posted completion evidence.
These runs used normal Uniterm Task-launch guidance; they do not prove that every agent launched independently outside that path discovers the inbox unaided.
An earlier Grok/OpenCode trial rejected a competing claim with `revision_conflict`; the losing agent selected the other assignment.
The final measured pairs did not collide.
In the measured inbox run, Grok initially supplied `--task unique` where a numeric durable Task ID was required, then removed the invalid argument and posted its handoff without a human nudge.
Without the board, Grok and OpenCode initially invented different file-claim conventions and spent tool calls reconciling them.
The independent Grok run made no inbox calls or posts.
The original independent-run report also checked the unassigned second file; that out-of-scope failure is excluded above, and the reusable runner now checks only the assigned task.

The board gives agents a usable shared ownership and handoff mechanism.
These results do not establish a general speedup: Codex/Claude were slightly faster without it, and model latency, caching, run order, unpinned native CLI versions, and nondeterminism are confounders.
Use the inbox for shared assignments and dependency handoffs; independent work should proceed without board traffic.
A longer randomized evaluation would be needed to measure planning efficiency, and was deliberately avoided to limit model usage.

### Reported model usage

Input below excludes cache reads and includes cache creation.
Counts are summed across the turn's requests, not unique prompt lengths.
OpenCode uses a temporary Codex access token; its reported zero cost is not treated as proof that subscription usage is free.

| Run / provider | Input | Cache reads | Output | Reported USD |
| --- | ---: | ---: | ---: | ---: |
| Inbox / Codex | 5,291 | 89,984 | 574 | Unavailable |
| Baseline / Codex | 6,711 | 83,456 | 493 | Unavailable |
| Inbox / Claude | 6,983 | 65,310 | 679 | 0.082474 |
| Baseline / Claude | 5,318 | 61,986 | 761 | 0.070129 |
| Inbox / Grok | 21,371 | 79,232 | 946 | 0.029932 |
| Baseline / Grok | 40,061 | 48,768 | 975 | 0.037521 |
| Inbox / OpenCode | 7,235 | 35,200 | 474 | Unavailable |
| Baseline / OpenCode | 3,719 | 51,328 | 1,059 | Unavailable |
| Independent inbox available / Grok | 14,997 | 44,800 | 936 | 0.019723 |
| Independent baseline / Grok | 27,849 | 41,984 | 854 | 0.027817 |

The initial eight-session target was exceeded while fixing the evaluator and retesting affected cases.
Early trials exposed a container DNS mount omission, a missing Codex code-mode helper, an evaluator version-probe bug, and revoked Claude OAuth credentials.
Those trials are excluded from the comparison, but their reported usage still counts toward total testing cost.
Across 17 sessions with nonzero reported usage, the logs contain 238,017 input tokens excluding cache reads, 1,074,264 cache-read tokens, and 12,363 output tokens.
Claude and Grok reported a combined USD 0.450402; Codex/OpenCode subscription usage has no reliable dollar figure here, so that amount is not a total bill.
The wrapper now passes version/help probes straight to the native executable, and an offline regression prevents them from becoming model prompts or false completion markers.
Temporary credentials are serialized through a private cache lock so concurrent containers cannot race rotating refresh tokens.
Claude and Grok use native turn limits; Claude also has a per-session dollar cap.
Codex and OpenCode have bounded wall time and requested tool-call limits, which are not hard token caps.
No further paid trials are needed for the release gates.

### Approval coverage

The previous Grok native fixture covered numbered tool approval, not the startup trust dialog.
The candidate adds the supplied screenshot and an authentic Grok 1.0.67 startup capture for both `grok` and `agent`.
A disposable offline home contains a harmless SessionStart hook: its marker is absent before approval and appears after the advertised `y` shortcut.
This verifies native acceptance with no paid model calls.
A final native-container rerun also passed Codex and Claude folder trust, OpenCode permission, and both Grok startup and command approval.
Uniterm tests separately replay the capture through the VT parser and real PTY delivery, reject incomplete/stale/policy-denied screens, suppress repeat delivery, and measure recognition latency.
Codex, Claude, OpenCode, Pi, and Gemini regressions remain in the shared PTY matrix; Pi and Gemini coverage is fixture-based.
Headless inbox runs do not by themselves prove interactive approval handling.

### Engineering gates

The complete debug workspace suite passed 936 tests; the separate required tmux differential corpus also passed.
The workspace build, warning-free Clippy, formatting, shell lint, installer tests, public-export checks, and isolated publisher tests passed.
The copied-history diagnostic passed all 13 compatibility/refusal cases with byte-identical inputs.
Four offline evaluator tests cover explicit live opt-in, native version/help probes, failure completion ordering, and usage accounting.
Authenticated live runs are opt-in and never run by CI; CI runs the offline evaluator and history checks.

Omarchy 4.0.6 x86-64 ran 90 compiled Rust test executables plus the diagnostic and evaluator scripts in disposable offline namespaces.
All 936 Rust tests passed after correcting one harness assumption: the workflow artifact test needs a writable disposable current project, while source and executables remain read-only.
The additional native tmux differential test passed.
A later review added an explicit owning-Project check for linked Tasks, with a real-transport test that refuses a foreign Task and accepts it in its own Project.
The rebuilt inbox executable passed a further Omarchy VM run after that correction.
All 91 transferred test/CLI executable hashes matched the host copies, and the native CLI reported candidate 1.2.10 with protocol 30.
The guest test directory was removed and the VM returned to its original stopped state.

With a retained inbox post and auto-approval enabled, the optimized Today and Flow checks emitted no idle output for 300.9 and 331.8 seconds.
Each reported 0.0001% process CPU and 10,688 KiB RSS, below the 0.5% idle budget.
These are idle-view measurements, not a claim about a 20-agent workload.

Release-tool validation exposed a detached Git maintenance process recreating the temporary export's object metadata during cleanup.
The publisher now disables automatic maintenance only in its disposable export repository; the local mirror/tag test passed afterward.

The initial optimized run encountered connector-lock contention and a missing rustdoc artifact while a subsequent build was replacing artifacts.
The final optimized server library passed all 396 tests in a separate offline namespace.
Full-grid recognition p95 ranged from 0.043 to 0.055 ms across the captured dialogs, including 0.055 ms for Grok startup; the one-time text recognizer measured 0.002 ms.
The final sequential production-profile workspace run passed all 936 tests with no failures.
Its targeted approval rerun measured 0.033-0.043 ms p95 across captured dialogs and 0.043 ms for Grok startup, below the five-millisecond bound.
The renderer measured 0.002535 ms worst p95; the Tasks PTY latency check measured 0.155 ms p95 with 128 retained 256-KiB prompts, two attached renderers, and two subscriptions.
The 1,000-post inbox budget completed in 11.57 ms with 200,000 bytes of charged payload.
The compact checkpoint capture measured 14.47 ms p95 for the 45-pane, 1,500-history-line workload.
The history budget retained all 100,000 events and reduced active-history recovery from 292.13 ms to 0.123 ms after maintenance.

The tagged Release workflow remains the publication gate for native macOS tests, packaged descriptor-pressure qualification, and all four public platform artifact pairs.
It builds through the canonical distribution script and verifies inventory, checksums, binary formats, platform baselines, version, and protocol before publishing.
Existing Workspace servers are not stopped or replaced by publishing a release.

### Native macOS launch correction

The v1.2.10 tag failed native macOS qualification before any release assets were published.
Both workflow and relay bootstrap commands exceeded macOS canonical terminal input limits after inbox guidance was added.
The tag is retained; v1.2.11 is the corrected combined candidate.
First-launch delivery now disables canonical input and echo before queueing the command, escapes embedded control bytes into one physical line, and restores canonical input and echo before the provider starts.
Running-agent delivery is unchanged.
The existing workflow integration now verifies an over-9-KiB goal with newlines, UTF-8, quotes, backslashes, and a control byte reaches the provider intact, alongside inbox discovery guidance.
Both the workflow and relay regressions passed locally after the correction.
A disposable offline packaged-binary smoke harness also passed inbox discovery/post/claim/watch/clear, toggles without confirmation, and the captured Grok startup dialog consuming `y`.
The replay uses the fixture's terminal dimensions; replaying its absolute cursor positions into a smaller terminal correctly leaves its clipped choices unapproved.

The corrected candidate passed the complete native macOS CI suite and both targeted launch tests in the Omarchy VM.
The guest was returned to its stopped state after removing the disposable test files.
Release qualification repeats both native macOS launch regressions five times, in addition to the full suite and ten packaged descriptor-pressure runs.

Publication also waits for successful CI on the exact exported commit, including production-profile tests and platform gates; absent or failed CI refuses publication.
The guard was exercised offline with successful, missing, and failed CI responses.

### Hosted Linux disk qualification

The v1.2.11 development release runner exhausted disk space while linking debug tests; the public build reached publication, which remained blocked on CI.
Publication was canceled so the combined release could include a repeatable infrastructure correction as v1.2.12.
Linux CI now removes an explicit allow-list of unused hosted-runner SDKs and requires 25 GiB free before linking.
It preserves Rust, Android/NDK, source files and caches; a five-case offline guard test refuses developer/macOS environments and insufficient or unreadable free space without deleting real files.
The Linux release job also removes completed debug build output before cross-compilation.
The production optimization profile is unchanged.
A direct POSIX-reader regression additionally checks launch encoding for empty, Unicode, quoted, backslash and control-character input; the full long-prompt PTY test remains the cross-platform behavior check.
The v1.2.11 runtime source passed all 936 local debug and optimized tests before this infrastructure correction; final v1.2.12 results include the additional reader unit test.

### Connector lock lifetime qualification

The v1.2.12 disk preflight recovered 14 GiB on the development release runner, but its concurrent library suite exposed connector-lock contention again.
No v1.2.12 assets were published.
Closing a locked descriptor alone can retain the lock when a concurrently forked child inherits the same open file description.
The connector guard now explicitly unlocks before closing; a deterministic regression duplicates the descriptor and verifies both immediate reacquisition and exclusion while the next owner holds it.
Environment-sensitive connector fixtures now run in separate test processes rather than temporarily changing the parallel library process's HOME.
This also removes the shared fixture mutex whose poisoning cascaded into four unrelated failures.
Both native macOS and Linux release qualification repeat the entire server library five times alongside its concurrent PTY tests.
The corrected local server library passed ten consecutive runs with normal parallelism, totaling 3,980 tests, including the inherited-descriptor regression.
The next combined candidate is v1.2.13; the earlier failed candidate tags are retained without published releases.

### Relay fixture interruption qualification

The v1.2.13 local debug suite passed 938 tests, warning-free Clippy, formatting, and the workspace build.
The connector fixture suite also passed all 28 tests in the offline Omarchy VM; its executable hash matched the host and the guest returned to its stopped state.
Both public platform debug jobs passed, but the development release's third repeated relay test returned without its first token after about 0.12 seconds, before the three-second response deadline.
Publication was canceled.
The fixture had discarded unexpected socket errors; it now reports premature closure and transport/server errors explicitly.
Twenty subsequent native macOS workflow/relay repetitions passed with these diagnostics, so the original error was not reproduced.
A separate timed Unix-socket probe did reproduce EINTR despite SA_RESTART, exposing a missing retry case in this test reader.
The reader now retries interrupted reads under the original deadline, with a deterministic regression for both recovery and deadline expiry.
This is a confirmed fixture defect, not proof of the original intermittent failure's cause.
CI now repeats native orchestration qualification twenty times, in addition to the release job's five repetitions.
The next combined candidate is v1.2.14.
