# Benchmarks: Uniterm v1.0.3 vs Herdr stable v0.8.2

Both binaries were built from the official release tags.

Measured 2026-09-05.
Intel Core 5 320, six logical CPUs, Omarchy 4.0.2, kernel 7.1.9-arch1-2; native Linux, not WSL.
Both source revisions were built in release mode from clean clones.

| Product | Source commit | Artifact SHA-256 |
| --- | --- | --- |
| Uniterm (ut) | `2a6da6bf370d7ab33f164460817b46086ba96314` | `2fb92ae4ac3e97ef7ee5946a4b98430af052fb9880ac33a012c72899f727a097` |
| Herdr | `9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c` | `b9ba0c3a056313022281192b3379b4439f07690b9188e956085fd38cbf586fa4` |

## Setup and evidence

Three complete marketing runs with rotated contender order, using ut-compare 0.2.0.
160x50 PTY, /bin/sh, 300-second idle windows, 20 startup, 50 control and 100 latency trials, ten 50,000-line output bursts, 16 panes, 40 resizes, and three clients for the multi-client scenario.
Each reported value is the median of the three run medians.
All metrics completed with no contender errors, and every latency/output screen oracle passed.

[Generated report](benchmarks/1.0.3/report.md), [run 1](benchmarks/1.0.3/marketing-1.json), [run 2](benchmarks/1.0.3/marketing-2.json), [run 3](benchmarks/1.0.3/marketing-3.json), and [host notes and reproduction](benchmarks/1.0.3/NOTES.md).

## Core performance

| Metric | Uniterm | Herdr | Result |
| --- | ---: | ---: | --- |
| Server start to ready | 8.67 ms | 50.29 ms | Uniterm |
| Control command round trip | 1.24 ms | 3.22 ms | Uniterm |
| Detached idle CPU (server and one shell) | 0.000 % core | 0.293 % core | Uniterm |
| Detached idle memory (server and one shell) | 11.1 MiB | 24.3 MiB | Uniterm |
| Attached idle CPU (server, client, shell) | 0.000 % core | 0.477 % core | Uniterm |
| Attached idle memory (server, client, shell) | 15.6 MiB | 37.6 MiB | Uniterm |
| Keystroke to visible | 2.08 ms | 8.35 ms | Uniterm |
| 50,000-line output burst to visible | 400.1 ms | 386.8 ms | Herdr |

Across the eight core metrics, Uniterm leads 7, Herdr leads 1, and 0 are ties on these aggregated medians.
Inspect each row: the balanced index is a geometric mean of ratios, not a count of wins.
CPU is the percentage of one core, and RSS includes the server, clients, pane shells, and descendants.
Values within one percent are ties; the balanced index floors CPU at 0.1 percent of a core before forming ratios.
A zero reading does not establish that no CPU was used.

## Context measurements

| Metric | Uniterm | Herdr | Result |
| --- | ---: | ---: | --- |
| Bytes written to the outer terminal per burst | 55.5 KiB | 18.8 KiB | Herdr |
| Output ingest rate | 8.58 MiB/s | 8.88 MiB/s | Herdr |
| Idle memory with 16 panes attached | 82.6 MiB | 108.0 MiB | Uniterm |
| Idle CPU with 16 panes attached | 0.000 % core | 1.040 % core | Uniterm |
| Memory per added pane | 4.46 MiB/pane | 4.73 MiB/pane | Uniterm |
| Memory returned after closing 15 panes | 96.3 % | 93.7 % | Uniterm |
| Memory after closing the added panes | 18.1 MiB | 41.3 MiB | Uniterm |
| Idle memory with 3 clients attached | 24.4 MiB | 62.8 MiB | Uniterm |
| Keystroke to visible with 3 clients attached | 2.09 ms | 14.48 ms | Uniterm |
| Resize storm settle (40 resizes after output bursts) | 22.7 ms | 267.3 ms | Uniterm |
| Resize storm CPU | 60.0 ms CPU | 90.0 ms CPU | Uniterm |
| Memory after the resize storm | 40.3 MiB | 48.4 MiB | Uniterm |
| Graceful server shutdown | 46.4 ms | 343.8 ms | Uniterm |
| Restart to ready | 9.5 ms | 55.2 ms | context only |
| Binary size | 5.64 MiB | 21.99 MiB | context only |
| State on disk after the live workloads | 1742.1 KiB | 4.7 KiB | context only |

These rows do not enter the balanced performance index.
Binary size, persistence, and restart are context, not performance-quality rankings.
The resize storm runs after all ten output bursts; each product retains scrollback according to its defaults.
Neither product restored the prior-output marker after a graceful stop and restart.
Herdr supports opt-in pane-screen history, but it was disabled in this comparison, so the state sizes do not represent equivalent recovery behavior.

## Fairness and limitations

Both products receive private HOME and XDG trees with owner-only runtime directories, identical shell, locale, geometry, payloads, settling periods, and sampling windows.
Herdr's version and manifest network checks are disabled during timing and reviewed separately in the assurance rubric.
Readiness and control use the same semantic operation: a fresh CLI listing panes over the product socket.
Detached idle is sampled after one attach/detach so both servers hold one shell; Herdr's headless grid is pinned to the profile geometry.
Latency and output travel through the attached PTY and must pass the final-screen oracle.
Incorrect or missing terminal state is a failed measurement.
Feature breadth and security/privacy scores are separate from performance.
These runs establish results only for this host and workload, not all Linux machines, macOS, or WSL.
The historical and new comparisons use different Herdr revisions and run dates; differences cannot be attributed solely to Uniterm's version.

## Historical comparison

[Uniterm v1.0.0 vs the Herdr development revision reporting 0.8.2](BENCHMARKS-v1.0.0.md) retains the September 3 measurements and raw evidence.
The website offers a version toggle for the two independent comparisons.

## Adding tmux

Tmux would be a useful baseline for common multiplexer workloads: startup, control latency, idle resources, input/output, pane scaling, resize, and multiple clients.
It has not yet been measured by this harness.
A tmux adapter should use an isolated socket and configuration and the same PTY workload, shell, geometry, cohort accounting, and screen oracle.
Product-specific features should be documented separately with N/A where they are outside tmux's scope.
An unsupported feature must not turn into a zero, a timeout, or a performance penalty, and a failed shared workload must remain a failure.
Only equivalent measured core metrics may enter a common index.
See the [official tmux documentation](https://github.com/tmux/tmux/wiki) for its multiplexer model.

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
