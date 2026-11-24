# Benchmarks: Uniterm v1.0.4 vs Herdr stable v0.8.2 vs tmux 3.7c

All three binaries were built from clean clones of the official release tags.

Measured 2026-09-05.
Native Linux x86_64 with six logical CPUs, kernel 7.1.9 series, not WSL; the same host as the [v1.0.3 comparison](BENCHMARKS-v1.0.3.md), later the same day.
Every source revision was built in release mode with locked dependencies.

| Product | Source commit | Artifact SHA-256 |
| --- | --- | --- |
| Uniterm (ut) | `a961519c5ed965c91a9f354554bfdcaba154810a` | `c3c62e53cdd16be1f68dfff518cf4b33c2cf5bc6691ccd2b00344455aceb7b85` |
| Herdr | `9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c` | `85122e080e0a9d40e8e618bad2b5b491f6b58f1be1dfbb66b2898b74bf2464f3` |
| tmux | `e476c1230b958df0cb12977517d24b3dc931375b` | `365e1283883c342f6bbd3272d62a496f37fcc51680f0c845386873910152d517` |

## Setup and evidence

Three complete marketing trials with seeds 0, 1, and 2, using ut-compare 0.3.0 at harness commit `76026d6233515d1e568ba60f5837bbb529e626cb`.
160x50 PTY, POSIX /bin/sh, 300-second idle windows, 20 startup, 50 control and 100 latency trials, ten 50,000-line output bursts, 16 panes, 40 resizes, and two extra clients for the multi-client scenario.
Each contender completed all 36 metrics in every trial, every latency/output screen oracle passed, and all benchmark processes exited.
Each reported value is the median of the three run medians; the archived summary also lists the minimum and maximum run medians, and the statistics files carry per-trial p95 values and sample counts.

[Summary with ranges](benchmarks/1.0.4/SUMMARY.md), [generated report](benchmarks/1.0.4/report.md), [statistics JSON](benchmarks/1.0.4/statistics.json), [statistics CSV](benchmarks/1.0.4/statistics.csv), [run 1](benchmarks/1.0.4/marketing-1.json), [run 2](benchmarks/1.0.4/marketing-2.json), [run 3](benchmarks/1.0.4/marketing-3.json), [provenance and reproduction](benchmarks/1.0.4/NOTES.md), and [checksums](benchmarks/1.0.4/SHA256SUMS).
The per-trial Markdown reports sit beside the JSON in the same directory.

## Balanced index

| Contender | Median index | Trial range |
| --- | ---: | ---: |
| Uniterm 1.0.4 | 98.7 | 98.6 to 98.8 |
| tmux 3.7c | 85.8 | 85.3 to 86.6 |
| Herdr 0.8.2 | 36.1 | 35.7 to 36.6 |

The index is a geometric mean of ratios over the eight core metrics, with a one-percent tie rule and a CPU floor of 0.1 percent of one core.
The range is the spread of the three independent per-trial indices, not a confidence interval.

## Core performance

| Metric | Uniterm | Herdr | tmux | Result |
| --- | ---: | ---: | ---: | --- |
| Server start to ready | 8.23 ms | 44.85 ms | 15.49 ms | Uniterm |
| Control command round trip | 1.35 ms | 3.53 ms | 2.18 ms | Uniterm |
| Detached idle CPU (server and one shell) | 0.000 % core | 0.257 % core | 0.000 % core | tie (Uniterm, tmux) |
| Detached idle memory (server and one shell) | 11.2 MiB | 24.1 MiB | 10.6 MiB | tmux |
| Attached idle CPU (server, client, shell) | 0.000 % core | 0.427 % core | 0.003 % core | Uniterm |
| Attached idle memory (server, client, shell) | 15.8 MiB | 38.4 MiB | 17.0 MiB | Uniterm |
| Keystroke to visible | 2.08 ms | 8.31 ms | 2.09 ms | tie (Uniterm, tmux) |
| 50,000-line output burst to visible | 392.5 ms | 372.7 ms | 391.0 ms | Herdr |

Across the eight core metrics, Uniterm leads 4, tmux leads 1, Herdr leads 1, and 2 are Uniterm/tmux ties on these aggregated medians.
Inspect each row: the balanced index is a geometric mean of ratios, not a count of wins, which is why Uniterm and tmux are far apart on the index while close on several rows.
CPU is the percentage of one core, and RSS includes the server, clients, pane shells, and descendants.
Values within one percent are ties; the balanced index floors CPU at 0.1 percent of a core before forming ratios.
A zero reading does not establish that no CPU was used.

## Context measurements

| Metric | Uniterm | Herdr | tmux | Result |
| --- | ---: | ---: | ---: | --- |
| Bytes written to the outer terminal per burst | 55.5 KiB | 18.6 KiB | 2.24 MiB | Herdr |
| Output ingest rate | 8.75 MiB/s | 9.21 MiB/s | 8.78 MiB/s | Herdr |
| Idle memory with 16 panes attached | 82.5 MiB | 108.3 MiB | 81.5 MiB | tmux |
| Idle CPU with 16 panes attached | 0.000 % core | 0.993 % core | 0.003 % core | Uniterm |
| Memory per added pane | 4.44 MiB/pane | 4.71 MiB/pane | 4.30 MiB/pane | tmux |
| Memory returned after closing 15 panes | 96.1 % | 93.9 % | 99.9 % | tmux |
| Memory after closing the added panes | 18.1 MiB | 41.4 MiB | 16.9 MiB | tmux |
| Idle memory with 3 clients attached | 24.5 MiB | 65.5 MiB | 29.6 MiB | Uniterm |
| Keystroke to visible with 3 clients attached | 2.08 ms | 12.54 ms | 2.09 ms | tie (Uniterm, tmux) |
| Resize storm settle (40 resizes after output bursts) | 22.0 ms | 269.9 ms | 207.0 ms | Uniterm |
| Resize storm CPU | 60.0 ms CPU | 90.0 ms CPU | 10.0 ms CPU | tmux |
| Memory after the resize storm | 42.1 MiB | 48.9 MiB | 18.0 MiB | tmux |
| Graceful server shutdown | 45.1 ms | 305.4 ms | 25.5 ms | tmux |
| Restart to ready | 9.4 ms | 53.1 ms | 16.4 ms | context only |
| Binary size | 5.64 MiB | 23.40 MiB | 1.35 MiB | context only |
| State on disk after the live workloads | 1741.6 KiB | 4.7 KiB | 0 bytes | context only |

These rows do not enter the balanced performance index.
Binary size, persistence, and restart are context, not performance-quality rankings.
tmux has no native disk restoration, so its restart row measures a fresh session and its state row is zero by design; Uniterm's state is the checkpoint that restores layout and scrollback after a crash.
The resize storm runs after all ten output bursts; each product retains scrollback according to its defaults, which is why tmux holds far less memory after the storm.
tmux writes about forty times more bytes to the outer terminal per burst than Uniterm; Uniterm's renderer emits only changed cells.
Neither Uniterm nor Herdr restored the prior-output marker after a graceful stop and restart.
Herdr supports opt-in pane-screen history, but it was disabled in this comparison, so the state sizes do not represent equivalent recovery behavior.
Uniterm's memory-related context rows carry the checkpoint and agent-detection machinery that tmux does not have; the ranking reports what was measured, not feature parity.

## Fairness and limitations

All products receive private HOME and XDG trees with owner-only runtime directories and sockets, identical shell, locale, geometry, payloads, settling periods, and sampling windows.
Herdr's version and manifest network checks are disabled during timing and reviewed separately in the assurance rubric.
tmux uses a private socket and configuration, a non-login shell, and tiled prefix-key splits.
Readiness and control use the same semantic operation: a fresh CLI listing panes over the product socket.
Detached idle is sampled after one attach/detach so every server holds one shell; Herdr's headless grid is pinned to the profile geometry.
Startup order reverses and suite order rotates by seed.
Latency and output travel through the attached PTY and must pass the final-screen oracle.
Incorrect or missing terminal state is a failed measurement.
Only the eight core metrics enter the index; artifact size, state size, process count, extended scenarios, and assurance do not.
Assurance is unreviewed and remains unknown for all three contenders.
These runs establish results only for this host and workload, not all Linux machines, macOS, or WSL.
They are not a controlled comparison with the earlier campaigns: the harness version, the number of contenders, and the time of day all changed.

## Historical comparisons

[Uniterm v1.0.3 vs Herdr stable v0.8.2](BENCHMARKS-v1.0.3.md) retains the two-product release-tag comparison from earlier on 2026-09-05.
[Uniterm v1.0.0 vs the Herdr development revision reporting 0.8.2](BENCHMARKS-v1.0.0.md) retains the September 3 measurements.
Both keep their raw evidence, and the website offers a version toggle for all three independent comparisons.

## Checkpoint and renderer hardening

The repository also contains focused release-mode budgets that run in CI:

```sh
cargo run --release --locked -p uniterm-server --example checkpoint_spike
cargo run --release --locked -p uniterm-server --example render_spike
```

These are subsystem measurements, separate from the whole-product comparison above.
The checkpoint workload uses 20 and 45 owned terminals at 200x50, each with 1,500 short output lines, capturing the latest 1,000 lines.
A counting allocator measures requested live heap bytes and allocations; its instrumentation affects timing, so capture p95 is a regression budget rather than end-to-end input latency.
The retained resolved-export path supplies the baseline in the same executable.

| Checkpoint measurement | 20 Panes | 45 Panes |
|---|---:|---:|
| Terminal-model heap before capture | 27.16 MiB | 61.12 MiB |
| Previous resolved capture + serialization, transient peak | 169.22 MiB | 380.75 MiB |
| Compact capture, transient peak | 18.80 MiB | 42.30 MiB |
| Compact capture + worker serialization, transient peak | 34.22 MiB | 76.99 MiB |
| Previous allocations per checkpoint | 4,020,022 | 9,045,047 |
| Compact capture allocations | 41 | 91 |
| Compact capture + serialization allocations | 42 | 92 |

With the compiler idle, the local Linux run measured baseline capture-and-serialize p95 at 189.8 ms for 20 Panes and 418.8 ms for 45 Panes.
Compact capture p95 was 3.4 ms and 13.4 ms respectively; capture plus worker serialization was 16.1 ms and 34.3 ms.
These allocator-instrumented timings describe this fixture, not the complete server input path.

The spike enforces capture p95 below 50 ms, compact transient memory below half the baseline, fewer than one percent of the baseline capture allocations, and a 32 MiB terminal-model heap budget for this 20-Pane fixture.
Model heap excludes shells, clients, allocator metadata, and the rest of the server; it does not establish a whole-process RSS budget for every scrollback workload.
Capturing remains synchronous and proportional to retained cells, while serialization and storage run on the persistence worker.

For the existing 200x50 renderer scroll fixture, default-blank tail erasure reduces the semantic scroll update from 236 to 62 bytes and its fallback repaint from 10,360 to 680 bytes.
The dense full-frame fixture stays at 10,352 bytes, its single-cell update stays at 20 bytes, and an unchanged grid still emits zero bytes.
Erasure is bounded to the Pane's columns and regression-tested against neighbouring content, styles, and cursor placement.
The renderer spike reports sampled p95 in fractional microseconds instead of rounding average times down to zero.
These results identify concrete allocation and output improvements without attributing the whole-product comparison to one subsystem or claiming macOS performance.
