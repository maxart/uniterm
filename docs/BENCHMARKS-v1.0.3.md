# Benchmarks: Uniterm v1.0.3 vs Herdr stable v0.8.2

This preserves the first release-tag comparison, measured the morning of 2026-09-05 with ut-compare 0.2.0.
It is a two-product comparison; the [latest comparison](BENCHMARKS.md) adds tmux and measures Uniterm v1.0.4.
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
The v1.0.4 comparison ran later the same day on the same host with the same Herdr artifact, but with ut-compare 0.3.0 and a third contender; it is not a controlled Uniterm-only version comparison.

[Latest Uniterm v1.0.4 comparison](BENCHMARKS.md).
[Historical Uniterm v1.0.0 comparison](BENCHMARKS-v1.0.0.md).
