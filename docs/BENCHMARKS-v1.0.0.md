# Benchmarks: Uniterm v1.0.0 vs Herdr

This preserves the September 3 baseline.
Both binaries came from development commits after their release tags while reporting versions 1.0.0 and 0.8.2.
These are historical builds, not measurements of those exact release tags.

Measured 2026-09-03 to 2026-09-04.
Historical Linux x86_64 laptop run, six logical CPUs, kernel 7.1.9-arch1-2.
Both source revisions were built in release mode from clean clones.

| Product | Source commit | Artifact SHA-256 |
| --- | --- | --- |
| Uniterm (ut) | `8167fd46440db34f6e1c084362063ffb36cd79ff` | `a16fbb64abe9dfae1b45700325863133fd896220fac33ac2ee20ee8bafafe7d8` |
| Herdr | `45484aab84430ac2b18c7bbf44aba15f2b039677` | `32bdd68b6b89c1df873fed6288a4319d9bef4cec487a6de51a464f0f8aaf3563` |

## Setup and evidence

Three complete marketing runs with rotated contender order, using ut-compare 0.2.0.
160x50 PTY, /bin/sh, 300-second idle windows, 20 startup, 50 control and 100 latency trials, ten 50,000-line output bursts, 16 panes, 40 resizes, and three clients for the multi-client scenario.
Each reported value is the median of the three run medians.
All metrics completed with no contender errors, and every latency/output screen oracle passed.

[Generated report](benchmarks/1.0.0/report.md), [run 1](benchmarks/1.0.0/marketing-1.json), [run 2](benchmarks/1.0.0/marketing-2.json), [run 3](benchmarks/1.0.0/marketing-3.json), and [host notes and reproduction](benchmarks/1.0.0/NOTES.md).

## Core performance

| Metric | Uniterm | Herdr | Result |
| --- | ---: | ---: | --- |
| Server start to ready | 8.55 ms | 49.26 ms | Uniterm |
| Control command round trip | 1.36 ms | 3.19 ms | Uniterm |
| Detached idle CPU (server and one shell) | 0.000 % core | 0.257 % core | Uniterm |
| Detached idle memory (server and one shell) | 11.1 MiB | 24.3 MiB | Uniterm |
| Attached idle CPU (server, client, shell) | 0.000 % core | 0.470 % core | Uniterm |
| Attached idle memory (server, client, shell) | 15.8 MiB | 40.5 MiB | Uniterm |
| Keystroke to visible | 2.09 ms | 4.15 ms | Uniterm |
| 50,000-line output burst to visible | 391.5 ms | 370.0 ms | Herdr |

Across the eight core metrics, Uniterm leads 7, Herdr leads 1, and 0 are ties on these aggregated medians.
Inspect each row: the balanced index is a geometric mean of ratios, not a count of wins.
CPU is the percentage of one core, and RSS includes the server, clients, pane shells, and descendants.
Values within one percent are ties; the balanced index floors CPU at 0.1 percent of a core before forming ratios.
A zero reading does not establish that no CPU was used.

## Context measurements

| Metric | Uniterm | Herdr | Result |
| --- | ---: | ---: | --- |
| Bytes written to the outer terminal per burst | 72.6 KiB | 18.6 KiB | Herdr |
| Output ingest rate | 8.77 MiB/s | 9.28 MiB/s | Herdr |
| Idle memory with 16 panes attached | 85.6 MiB | 111.8 MiB | Uniterm |
| Idle CPU with 16 panes attached | 0.000 % core | 1.030 % core | Uniterm |
| Memory per added pane | 4.67 MiB/pane | 4.73 MiB/pane | Uniterm |
| Memory returned after closing 15 panes | 96.3 % | 91.1 % | Uniterm |
| Memory after closing the added panes | 18.1 MiB | 45.7 MiB | Uniterm |
| Idle memory with 3 clients attached | 24.7 MiB | 71.2 MiB | Uniterm |
| Keystroke to visible with 3 clients attached | 2.09 ms | 6.24 ms | Uniterm |
| Resize storm settle (40 resizes after output bursts) | 22.7 ms | 274.4 ms | Uniterm |
| Resize storm CPU | 40.0 ms CPU | 110.0 ms CPU | Uniterm |
| Memory after the resize storm | 39.3 MiB | 51.6 MiB | Uniterm |
| Graceful server shutdown | 44.6 ms | 305.2 ms | Uniterm |
| Restart to ready | 24.3 ms | 57.2 ms | context only |
| Binary size | 5.58 MiB | 23.15 MiB | context only |
| State on disk after the live workloads | 1741.6 KiB | 4.0 KiB | context only |

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

[Latest Uniterm v1.0.3 comparison](BENCHMARKS.md).
