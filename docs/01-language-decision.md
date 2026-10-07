# 01 - Language and Runtime Decision (ADR)

**Status:** Accepted.
**Decision:** The Uniterm CLI is written in Rust.
**Supersedes:** the draft `uniterm-cli-rust-decision-spec.md`, which this document finalizes and extends.

This ADR is the finalized version of the original draft.
The draft locked the language; this version keeps that decision, hardens the reasoning against the second memory report, and adds the runtime and concurrency decisions that the draft left open.

## Context

Uniterm today is a Tauri/WKWebView application: a Rust host driving a WebKit frontend that renders terminals inside a webview.
Two diagnostics measured it under a real workload on Apple Silicon.

The 2026-06-29 energy report, on a 21-session workload:

| Component | Physical footprint | CPU (% of one core) |
|---|---:|---:|
| Rust host process | 46 to 47 MiB | 9.6% |
| WebKit WebContent | 622 to 640 MiB (peak ~984) | 34.0% |
| WebKit GPU helper | ~142 to 147 MiB | 4.4% |
| **UI cohort total** | **~821 to 827 MiB** | **40 to 48%** |

The 2026-06-30 memory report, a day later, corroborated it: the WebKit content process alone was 674 MiB, the UI helpers about 874 MiB, and the whole coalition about 2.80 GiB.

The findings that drive the decision:

1. The webview is the cost.
   The overwhelming majority of both memory and CPU is WebKit, not Uniterm's own code.
2. The Rust host is already lean.
   At roughly 46 MiB (55 MiB in the next-day memory report) and ~9.6 percent CPU it is efficient; the problem is the layer above it, not the language.
3. The webview does work while invisible.
   Timer-driven style resolution, layout, font shaping, paint, and layer commits continued while Uniterm was not frontmost.
4. Terminal data is tiny.
   Persisted scrollback across all panes was 1.23 MiB on disk (45 scrollback files); the 622 MiB in WebContent is render and DOM retention, not terminal content.
5. Idle PTYs are cheap.
   All 21 PTY reader threads were blocked in `read`.
6. Two real costs live in Rust and are language-independent: per-pane process-table polling for agent-exit detection, and redundant libgit2 worktree scans.

The implication is decisive.
A CLI renders directly to the terminal and has no webview, so the entire WebKit cohort (the expensive part) disappears, and what remains (the Rust host) is the part that was already cheap.

## Decision and rationale

The CLI is written in Rust.
This is not momentum.
It follows from where the cost lives, what the CLI removes, and what the product sells.

**The rewrite targets the right layer, and Rust is already that layer.**
The CLI deletes WebContent, the GPU helper, the DOM, and all browser-engine retention.
What remains is PTY management, a grid model, an event loop, and a render-to-terminal path, which is work the current Rust host is already close to.
Choosing Rust reuses the lean, proven part rather than rewriting it.

**Resource and energy footprint is the differentiator, which makes GC the wrong tax.**
The product's reason to exist is low footprint.
A garbage-collected runtime would still beat WebKit decisively, but it reintroduces baseline heap overhead and periodic collection CPU on the exact axis the product is selling.
Rust gives deterministic memory, no GC pauses in the keystroke path, and a small static binary.

**The systems-plus-agentic split favors Rust specifically.**
The app is a systems half (PTYs, event loop, escape parsing, client-server over a socket, low input-to-pixel latency) and an agentic half (LLM and API calls, JSON, TLS, orchestrating many concurrent agent processes).
Rust covers both with mature crates.
Zig would mean hand-rolling or FFI for the agentic half, where its ecosystem is thin.
C would mean writing HTTP, TLS, JSON, and concurrent orchestration by hand in 2026, plus carrying the full memory-safety burden under heavy concurrency.

**The native exit and IPC primitives the carryover fixes require are first-class in Rust.**
`pidfd`, `kqueue` with `EVFILT_PROC` and `NOTE_EXIT`, and event-driven git invalidation are all directly available.
Switching languages buys nothing here, because the fix is the same everywhere.

## Alternatives considered

| Option | Verdict | Reasoning |
|---|---|---|
| **Rust** | **Chosen** | Reuses the already-lean host; no GC on the differentiating axis; covers both halves with mature crates; native exit and IPC primitives first-class. |
| **Go** | Rejected | Would beat WebKit and goroutines suit agent fan-out, but GC adds baseline heap and periodic CPU on the exact axis we sell, and it means rewriting the lean Rust host for no resource win that matters. |
| **Zig** | Rejected | Excellent for the terminal half (see Ghostty), but pre-1.0 with a thin ecosystem precisely in the agentic half (HTTP, JSON, TLS), where we would hand-roll or FFI. The friction lands on the differentiator. |
| **C** | Rejected | Systems half is fine (what real tmux uses), but writing the HTTP, TLS, JSON, and concurrency agentic layer in C in 2026, plus the full memory-safety burden under heavy concurrency, is not justified. |

## New in this finalized version: the runtime decisions

The draft left the concurrency model and renderer open.
This section closes them.
Detail lives in [03-system-architecture.md](03-system-architecture.md) and [04-multiplexer-core.md](04-multiplexer-core.md); the decisions are recorded here so the ADR is self-contained.

### Decision R1: two runtimes, not one

The hot terminal path (PTY reads, parse, grid update, render, client I/O) runs on a single-threaded, `mio`-based event loop, in the spirit of tmux's single-threaded libevent core.
No async executor sits between a keystroke and a pixel.

The agentic subsystems (agent process supervision, HTTP and LLM calls, file watching, the workflow and relay engine, the control-socket server) run on a separate `tokio` runtime on its own threads.

The two communicate by message passing over channels.
This keeps scheduler jitter, work-stealing, and future non-determinism out of the latency-critical path, while still giving the agentic half the mature async ecosystem it needs.
This directly serves the sub-50 ms input-to-pixel and zero-idle-frame budgets.

### Decision R2: a damage-tracked custom grid renderer

The draft called the renderer "the highest-leverage open technical decision."
It is resolved in favor of an explicit dirty-cell diffing renderer over a custom grid, not an immediate-mode full-frame TUI.

Immediate-mode TUIs (redraw the whole frame, diff against the last) are fine at small scale but redraw-oriented by nature, and the budget here is literally zero frames when nothing changes, at 45-plus panes.
tmux already proves the alternative: collect changes per line, merge adjacent cells, and emit only the escape sequences for cells that actually changed, tracking damage with per-pane redraw flags.
We build that model in Rust.
Inactive panes update their grid and draw nothing until shown.

### Decision R3: crates to validate in a spike, not final

| Concern | Candidate(s) |
|---|---|
| PTY | `portable-pty` (from WezTerm), or direct `nix` |
| Escape-sequence parsing | `vte` (from Alacritty) |
| Terminal cell model / TUI primitives | custom grid; `ratatui` only for dialogs, not the Pane or persistent Observatory render path |
| Terminal capability output | `termwiz` or `terminfo` via `crossterm`, wrapped behind our own damage tracker |
| Event loop (hot path) | `mio` |
| Async runtime (agentic half) | `tokio` |
| HTTP / LLM APIs | `reqwest` with streaming |
| Serialization | `serde`, `serde_json`, `toml` |
| Git status | `gitoxide` preferred, `git2` as fallback |
| Process exit events | `nix` for `kqueue`; raw `pidfd` on Linux |
| File watching | `notify` |
| Fuzzy matching (pickers) | `nucleo-matcher` |
| Embedded event store | `rusqlite` (bundled) or a custom append-only log; see [05-session-persistence.md](05-session-persistence.md) |

Note the deliberate choice not to use `ratatui` for the per-pane terminal render path.
`ratatui` is immediate-mode and excellent for the dashboard surfaces, but the pane grid needs the damage-tracked path of R2.

## Costs that carry over (must fix regardless of language)

The CLI does not automatically fix these; they are design issues in the current Rust backend and will follow the rewrite unless addressed.
They are elevated here to architecture requirements.

1. **Agent-exit detection via process-table polling.**
   The old `is_pane_descendant_alive` triggered full `sysinfo` refreshes with Rayon fan-out per pane.
   Fix: native exit notification (`kqueue` plus `EVFILT_PROC`/`NOTE_EXIT` on macOS/BSD, `pidfd` on Linux).
   Any residual fallback scan shares one cached snapshot per interval across all watchers, backs off for inactive panes, and never scans per pane.

2. **Redundant git worktree scans.**
   libgit2 stats were recomputed across duplicate project records.
   Fix: canonicalize to repository root, cache keyed by that root, coalesce concurrent requests, invalidate from filesystem and git events with a short debounce, and defer expensive diff stats until the relevant view is visible.

## Success criteria

Initial targets, derived from the diagnostic budgets; validate against supported hardware.

- Background or occluded CPU: under 0.5 percent over five minutes.
- Foreground idle CPU, excluding child jobs: under 3 percent over five minutes.
- Frames with no visible change: zero.
- Memory, same 21-session workload: low tens of MiB steady state, an order-of-magnitude reduction from ~821 MiB.
- Memory after closing panes: within ~10 percent of baseline after settling.
- Input-to-pixel latency: under 50 ms p95.
- Agent-exit detection: under 500 ms, no whole-system scans.
- Git badge update after a relevant change: under 1 second, no periodic full scans.

## Decision log

| Date | Decision | Note |
|---|---|---|
| 2026-06-30 | Language = Rust for the CLI | Grounded in the 2026-06-29 and 2026-06-30 resource diagnostics. |
| 2026-06-30 | R1: two-runtime split (mio hot path, tokio agentic) | Keeps async out of the keystroke-to-pixel path. |
| 2026-06-30 | R2: damage-tracked custom grid renderer | Required for the zero-idle-frame budget at scale. |
| 2026-06-30 | R3: crate shortlist to validate in a spike | Notably, no ratatui on the pane render path. |
