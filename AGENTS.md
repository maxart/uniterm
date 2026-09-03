# AGENTS.md - Working on Uniterm

This file is the operating manual for anyone (human or agent) writing code in this repository.
It encodes the architectural invariants, the performance discipline, and the concrete dos and don'ts that keep Uniterm true to why it exists.
Read it before you touch code.
`CLAUDE.md` contains only `@AGENTS.md`, so tooling that reads that file gets this one; do not put anything else in it.

## What Uniterm is

Uniterm is a terminal multiplexer written in Rust, built for agentic engineering.
It is one static binary that is both a complete tmux-class multiplexer (client-server, persistent sessions, splits, layouts, built-in session save/restore) and an agent-fleet supervisor (status detection, a waiting queue, multi-agent workflows and relay, a monitoring Observatory).
It succeeds Uniterm Desktop, the earlier GUI application, and is built around performance from the first line: no idle work, no polling, and a renderer that emits only what changed.
The resource budgets in the performance section below are the product, and every change is measured against them.
Uniterm Desktop is a separate codebase and is not part of this repository; the only place it appears here is the hierarchy importer (`ut migrate from-desktop`, `docs/13-desktop-migration.md`).
When this file says "the old app", it means Uniterm Desktop, and it is naming a pattern to avoid, not code to read.

The full design lives in `docs/`.
Start at `docs/README.md`.
If a decision here seems arbitrary, the reasoning is in the docs; read it before overriding.

## Repository layout

```
crates/
  uniterm-core/     Pure model + pure logic: grid and damage, layout tree, agent status,
                    the workflow/relay decision engines, run graph, artifact ledger,
                    guardrails, tasks, waiting and instruction queues. NO UI, NO async, NO I/O.
  uniterm-proto/    Wire + channel message types (the mio<->tokio seam, the client protocol,
                    the control API).
  uniterm-server/   The mio core loop (server.rs plus server/*: io, messages, chrome, mouse,
                    agents, projects, socket, and the projection modules), the damage-tracked
                    renderer, the VT parser, the tokio agent runtime, providers, persistence,
                    and the control server.
  uniterm-client/   The thin attach client, input decoding, the overlays and modals, and the
                    one-shot request helpers (the only crate that may use ratatui).
  uniterm-cli/      The `uniterm` binary front door (alias `ut`), remote attach, migration.
docs/               The design of record. docs/STATUS.md is the implementation-status record.
```

## The invariants (do not violate these)

These are the load-bearing rules.
Breaking one does not just make the code worse; it breaks the reason the product exists.

### 1. The no-UI-in-core boundary

`uniterm-core` must never depend on a UI toolkit, an async runtime, or an I/O crate (mio, tokio, crossterm, ratatui, termwiz, reqwest, rusqlite, nix, notify, and so on).
It is pure model and pure logic so that every front-end shares identical behaviour and the logic is exhaustively testable in isolation.
This is enforced by `crates/uniterm-core/tests/no_forbidden_deps.rs`; if you need one of those crates, your code belongs in `uniterm-server`, not `uniterm-core`.

### 2. Two runtimes, one seam (Decision R1)

The hot path (PTY reads, VT parse, grid update, render, client I/O) runs on the single-threaded `mio` core loop.
The agentic half (supervisor, provider adapters, workflow/relay engines, HTTP/LLM calls, file and git watching, the control server) runs on the `tokio` runtime on its own threads.
They communicate only through the typed channel messages in `uniterm-proto` (`CoreToAgent`, `AgentToCore`), never through shared mutable state.
Do not put `tokio`, `async`, or a lock on the keystroke-to-pixel path.
The agent runtime never touches a grid directly; it asks the core to act via a message.

### 3. Damage-tracked rendering, never full-frame (Decision R2)

The renderer emits only the cells that actually changed, and emits nothing when nothing changed.
Never redraw a whole pane or a whole frame on a timer.
`ratatui` is immediate-mode and is banned from the pane render path; it is allowed only in `uniterm-client` for the Observatory and dialogs, which are low-frequency surfaces.

### 4. No idle work

Nothing wakes the loop just because time passed.
There is no free-running timer that asks "should I redraw?"; that is the anti-pattern this design exists to avoid.
When idle, the core loop blocks in `poll` with no timeout and consumes no CPU.
Any tick (render coalescing, animation) must be damage-gated: armed by real work, disarmed when the work drains.

### 5. No polling for process state

Agent liveness and exit come from the kernel: `pidfd` on Linux, `kqueue` with `EVFILT_PROC`/`NOTE_EXIT` on macOS, registered once per tracked pid.
Never scan the process table per pane.
A per-pane process-table refresh, however parallel, is exactly what this rule forbids; do not introduce one.
If a fallback scan is ever unavoidable, it is one shared snapshot per interval across all watchers, with backoff for inactive panes, never per-pane.

### 6. Git status is repo-keyed, cached, and event-driven

Canonicalize every path to its repository root, cache by that root, coalesce concurrent requests, and invalidate from filesystem and git events with a short debounce.
Never run a periodic full scan per project or per pane.
Defer expensive diff stats until a view that needs them is visible.

### 7. The event log is the ground truth

Durable state is an append-only event log; every view (the Observatory, the timeline, restore, recovery) is a projection of it.
Add new state to the log first and project it into views, never the reverse.
This is what makes history, restore, and crash recovery free.

### 8. Agent-agnosticism

No feature may branch on a specific agent id.
Everything agent-specific (discovery, spawn, status heuristics, session parsing, pricing) goes behind the provider trait, one module per agent.
`if agent == "claude"` in the core is a bug.

### 9. Workspace scoping

Every agentic query (fleet view, waiting queue, workflows) is filtered by the active workspace.
This is a safety property: a bulk action like "approve all pending" must never reach into unrelated work.
Thread the workspace scope through from day one; retrofitting it is a migration.

## Build, test, lint

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets   # must be warning-free
cargo fmt --all --check                  # must be clean
```

Phase 0 spikes:

```sh
cargo run --release -p uniterm-server --example render_spike   # renderer budget check
cargo run -p uniterm-server --example runtime_demo             # mio<->tokio boundary
```

All four gates must pass before a change is done.
Clippy warnings are errors here.
Run `cargo test --workspace --release` as well when you touch the server or the CLI: `debug_assert!` bodies and overflow checks are compiled out in release, and a bug hid there once.

Integration tests must never touch a real Workspace.
Every test that binds or spawns a server calls `isolate_state()` and uses `unique_workspace_name()` from `tests/common/mod.rs`, and every spawned `ut` gets `XDG_STATE_HOME` and `XDG_RUNTIME_DIR` through `.env`.
A test run must leave `~/.local/state/uniterm` byte-identical.

## Cross-platform distribution builds

All cross-platform release artifacts have one canonical location and naming scheme:

```text
target/dist/
  macos-arm64/
    uniterm
    ut
  ubuntu-x86_64/
    uniterm
    ut
  ubuntu-aarch64/
    uniterm
    ut
  arch-x86_64/
    uniterm
    ut
  fedora-x86_64/
    uniterm
    ut
  android-aarch64/
    uniterm
    ut
```

- ALWAYS use `scripts/build-dist.sh <platform>` for cross-platform releases.
- ALWAYS name platform folders `<os>-<arch>` in lowercase, with no version, commit, date, or `-dirty` suffix.
- ALWAYS leave exactly the two executable files `uniterm` and `ut` inside each platform folder.
- NEVER create tarballs, zip files, checksums, manifests, nested version folders, or cross-build intermediates under `target/dist/`.
- NEVER create cross-build target or Zig cache directories elsewhere in this repository.
  The script builds in an isolated directory under `/tmp`, copies only the two binaries into `target/dist/<platform>/`, and removes the temporary build tree.
- NEVER build Uniterm for Intel macOS or as a universal macOS binary.
  Apple Silicon `macos-arm64`, mapped to `aarch64-apple-darwin`, is the only permitted Mac artifact.
- Ubuntu releases use `ubuntu-x86_64`, mapped to `x86_64-unknown-linux-gnu.2.17`, so the binary retains a glibc 2.17 compatibility baseline.
- Ubuntu ARM releases use `ubuntu-aarch64`, mapped to `aarch64-unknown-linux-gnu.2.17`, and are the generic Linux ARM64 and WSL ARM64 release source.
- Arch Linux and Omarchy releases use `arch-x86_64`, mapped to `x86_64-unknown-linux-gnu.2.17`.
- Fedora releases use `fedora-x86_64`, mapped to `x86_64-unknown-linux-gnu.2.28`.
- Android Termux releases use `android-aarch64`, mapped to `aarch64-linux-android`, with Android API level 24 by default.
  Native Termux builds use Cargo directly; cross-builds use the NDK because Zig does not provide Android's Bionic system libraries.
  Modern NDKs use their unified AArch64 Clang linker; legacy r10e automatically falls back to its API 21 GCC linker when no API level is explicitly requested.
- When the user requests another platform, add one explicit mapping to `scripts/build-dist.sh` using the same `<os>-<arch>` folder convention.
  Do not bypass the script or invent another artifact location.
- Rebuilding a platform replaces the two binaries in its stable folder.
  Old versions are not retained in `target/dist/`; source control provides version history.

Build the current supported artifacts with:

```sh
scripts/build-dist.sh macos-arm64
scripts/build-dist.sh ubuntu-x86_64
scripts/build-dist.sh ubuntu-aarch64
scripts/build-dist.sh arch-x86_64
scripts/build-dist.sh fedora-x86_64
scripts/build-dist.sh android-aarch64
```

Agent-facing skills live under `skills/` in the plain `SKILL.md` format; `skills/manage-uniterm/` teaches any harness to drive a running Workspace through `ut`, and it must be updated in the same change as any `ut` verb it documents.

## Performance discipline

The resource budgets are the product, not a nicety.
They are in `docs/00-vision-and-scope.md` and `docs/01-language-decision.md`.
The ones you must not regress:

- Background/occluded CPU under 0.5 percent over five minutes.
- Foreground idle CPU under 3 percent over five minutes.
- Zero frames (zero bytes written, zero wakeups) when nothing visible changed.
- Input-to-pixel latency under 50 ms p95.
- Agent-exit detection under 500 ms with no whole-system scan.
- Memory in low tens of MiB steady state on a 20-plus session workload.

## Engineering review rules

These rules capture recurring review lessons and apply across Uniterm.

- Cache an external side effect only after the authoritative system confirms that it happened.
- Scope observed agent state to one invocation and process group, never just a reusable Pane id.
- Preserve explicit launch overrides through handoff and restore, but clear unreadable or absent values when a new invocation starts.
- Measure terminal strings by display-cell width, never byte length or Unicode scalar count.
- Sanitize every value that can reach OSC, SGR, a title, notification, or other terminal chrome channel at the final output boundary.
- Route clicks, key bindings, CLI controls, and automation through the same semantic command path.
- Add regression coverage for the no-op case and the first-attach case, not only the steady-state success path.
- Resolve scalar facts from their owning index or map; do not scan every Pane to answer a single-Pane question.

When you add anything on the hot path, measure it.
When you add anything that could run while idle, prove it does not.
The renderer spike is the template for a budget check; grow it into CI rather than trusting a vibe.

## Dos and don'ts

### Hot path (the mio core loop and renderer)

- DO keep it synchronous, allocation-light, and lock-free.
- DO mark damage precisely and emit only changed cells.
- DO recognize OSC 7, OSC 133, OSC 52, and OSC 777 in the parser and route them instead of drawing them.
- DON'T call `.await`, spawn a task, or take a lock here.
- DON'T allocate per cell or per frame in steady state; reuse buffers.
- DON'T add a periodic timer.

### Consistent TUI item geometry

- DO derive rendering, scrolling, and hit testing from the same item rectangles; padding belongs to the item, including the final visible item.
- DO split sub-row vertical padding across a shared terminal cell with upper/lower half-block glyphs, retaining the outer half above the first item and below the last item.
- DO keep semantic fills in the effective background channel. If a half-block needs the terminal-default colour on one side, use reverse video so transparent terminals do not render the fill as a lighter foreground border.
- DO use terminal-default foreground/background (`39`/`49`) for unfilled items, and add integration assertions for exact row positions, SGR styles, glyph halves, viewport edges, and clicks.
- DON'T fake fractional spacing by alternating full-row gaps or by attaching a differently rendered foreground strip to only some items.

### The agent runtime (tokio side)

- DO put all network, disk, subprocess, and watcher work here.
- DO send results back to the core as `AgentToCore` messages; let the core mutate grids and state.
- DO spawn agents into their own process group (`setsid`) and reap with escalating SIGTERM then SIGKILL, so descendants are never orphaned.
- DON'T reach across the seam into grid or session state directly.
- DON'T block the render loop waiting on the agent runtime; a failed channel send is dropped, not awaited (the durable record is the event log).

### Agent status detection

- DO treat OSC 777 as the primary, cooperative signal (the agent's notify hook writes the envelope to its `/dev/tty`, so the bytes arrive in the PTY stream).
- DO fall back to log-tail, then the provider's screen rules over the cursor-to-bottom region and the window title, then reconcile by authority.
- DO require a positive, anchored match for `working` and `tool` (a spinner glyph or a line-start phrase; a title rule outranks grid text); when no rule matches, a known agent is idle.
- DO use dwell thresholds: about 5 s for permission and question, 600 ms for idle, 2 s for error and exit; agents flicker for about 100 ms during prompts.
- DO let a detected permission prompt outrank a stale "working" signal, let any real verdict replace a cooperative `starting`, and let a persistent screen idle replace a cooperative `working` only after 30 s.
- DON'T treat output volume as evidence: keyboard echo, a repainting footer, and a resize are all output, and treating bytes as "working" marked idle agents busy while the user typed.
- DON'T write a bare substring rule such as `thinking` or `running` that typed prompt text can match.
- DON'T scrape `tmux capture-pane` or spawn a subprocess to read output; we own the grid, read it directly.
- DON'T fire a notification on an unsmoothed status change.

### Persistence

- DO append to the event log first, then project.
- DO write snapshots atomically (temp file then rename) and often (dirty-triggered, seconds not minutes).
- DO persist grid/scrollback and structured layout - the thing tmux-resurrect cannot.
- DO use advisory `flock` on a sidecar lock for cross-process safety, with a short backoff; a running server holds one beside its socket and a POSIX record lock under the state directory, so no process can share a live Workspace's durable files through a different runtime directory.
- DO treat the snapshot as the crash marker: a clean stop deletes only the snapshot and keeps the event stream, so Tasks, the run graph, the artifact ledger, and the audit trail outlive an intentional stop.
- DO accept an event stream whose first sequence is above 1 as contiguous, repair damage after the origin, and quarantine anything unreadable to a `.corrupt-*` sibling; only a future-schema stream may refuse to start.
- DON'T store an opaque layout string; store structured layout.
- DON'T let a crash mid-write corrupt state; that is what atomic writes prevent.
- DON'T put machine-scoped state in the repo; project-scoped agentic artifacts go under `.uniterm/`, machine state goes in XDG dirs.

### Extensibility

- DO add a new agent as one module implementing the provider trait.
- DO source model pricing from a data file, updatable without a rebuild, and mark estimated costs as estimated.
- DON'T special-case an agent id anywhere outside its provider module.
- DON'T bake a plugin runtime into the binary; if we add one it is out-of-process behind the control protocol, so plugin versions never couple to ours.

### Orchestration (workflows and relay)

- DO write the decision logic as pure functions in `uniterm-core` and test every transition before wiring it to panes.
- DO advance on the explicit completion contract (`uniterm workflow submit` / `uniterm relay submit`), not on an idle guess; idle is only a safety net.
- DO mint a per-role/per-turn token embedded in the injected prompt so a role cannot forge another's completion.
- DO cap iterations and detect a stalled verdict (two identical `fix` verdicts) so loops cannot run forever.
- DON'T make idle detection the primary completion trigger.
- DON'T let any role but the verifier produce a verdict.

### Guardrails and confirmation

- DO carry the human's confirmation on the wire for destructive and bulk commands (Project removal, the bulk agent stop, Workspace stop) and record the guardrail decision before the first Pane closes.
- DO answer an unconfirmed control request with `confirmation_required` and change nothing.
- DON'T pass a literal `confirmed: true` from server code; the client's confirm step, an explicit CLI command, or the control caller is the only source of that flag.

## Rust conventions

- Match the surrounding code: comment density, naming, and idiom.
- Prefer small `Copy` types on the hot path (see `Cell`); prefer newtypes for ids (`PaneId`) so kinds cannot be confused.
- Handle errors explicitly; the hot path must never `panic!` on bad input from a child process (clamp and continue).
- Keep `unsafe` out of `uniterm-core` entirely; isolate any platform `unsafe` (pidfd, kqueue, PTY) behind a small, documented, tested wrapper in `uniterm-server`.
- Every public item gets a doc comment that says why, not just what, and links the relevant `docs/` file where useful.
- Add a unit test with the code, in the same file, for pure logic; add an integration test for cross-crate behaviour.
- Do not add a dependency without cause; each one is a footprint and a supply-chain surface, and this project sells low footprint.

## Style (repo-wide, matches the docs)

- No em dashes and no en dashes; use a plain hyphen.
- In long Markdown, put each full sentence on its own physical line (see any file in `docs/`).
- There is no changelog file; release notes are generated from commit messages, so write each message for a reader who was not there.
- Commit messages describe the change and its reason; do not add an agent as co-author.
- Ship the status update with the feature: `docs/STATUS.md` names the entry point and the test in the same change.

## How to make common changes

- Adding a hot-path feature: put the model in `uniterm-core`, the loop/render/IO in `uniterm-server`, and any new message on the seam in `uniterm-proto`.
- Adding an agent provider: one module in `uniterm-server` implementing the provider trait; nothing else changes.
- Adding a command: it resolves its target (session/window/pane) from flags and is exposed both to humans and over the control protocol (one vocabulary).
- Adding persisted state: define the event(s), append them, and write the projection; do not add a side store the log does not know about.

## When in doubt

Ask: does this add work while idle, put async on the hot path, poll for process or git state, branch on an agent id, or bypass the event log?
If yes to any, stop and reconsider - one of the invariants above is telling you the design is wrong for this product.
