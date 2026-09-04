# Benchmarks: Uniterm vs Herdr

Speed and footprint are the reason Uniterm exists, so they are measured rather than asserted.
This page records one comparison, Uniterm vs Herdr, the closest product in the same space, and states exactly what was measured, on what, and where the other product does better.
The [README](../README.md) carries the short form.

## Setup

| | |
| --- | --- |
| Harness | `ut-compare 0.2.0`, an open, product-neutral benchmark and review harness |
| Uniterm | commit `8167fd46440d` (version 1.0.0), built in release mode from a clean clone |
| Herdr | version 0.8.2, commit `45484aab8443`, built in release mode from a clean clone |
| Host | one Linux x86_64 laptop, kernel 7.1.9, 6 logical CPUs, native (not WSL) |
| Profile | `marketing`: 160x50 terminal, `/bin/sh` in every pane, 300 s idle windows, 20 startup, 50 command, and 100 latency samples, 10 output bursts of 50,000 lines, 16 panes, 40 resizes, 2 extra clients |
| Runs | three complete runs on 2026-09-03 with the contender order rotated between runs |
| Date | 2026-09-03 |

Fairness controls the harness enforces:

- Terminal latency and output completion go through a real PTY for both products, end to end: shell, server parse and render, client render, visible bytes.
- Both products get a private `HOME` and XDG tree; Herdr's update and manifest network checks are disabled during timing so the network cannot add variance.
- Startup is timed until an identical pane-listing probe succeeds; detached idle is sampled after one attach and detach so both servers hold exactly one pane shell.
- Herdr's headless render grid is pinned to the attached geometry.
- Every latency and output trial is checked against a modelled screen; a wrong or incomplete final screen fails the trial instead of counting as fast.
- CPU is the percentage of one core from cumulative process CPU time; cohort figures include the server, the attached clients, the pane shells, and their descendants.
- Values within one percent are ties.

## Core results

These eight metrics form the harness's balanced index.
Each value is the median of the three runs' medians.

| Metric | Uniterm | Herdr | Better |
| --- | ---: | ---: | --- |
| Server start to ready | 8.6 ms | 49.3 ms | Uniterm |
| Control command round trip | 1.4 ms | 3.2 ms | Uniterm |
| Detached idle CPU (server and one shell) | 0.00 % | 0.26 % | Uniterm |
| Detached idle memory (server and one shell) | 11.1 MiB | 24.3 MiB | Uniterm |
| Attached idle CPU (server, client, shell) | 0.00 % | 0.47 % | Uniterm |
| Attached idle memory (server, client, shell) | 15.8 MiB | 40.5 MiB | Uniterm |
| Keystroke to visible | 2.1 ms | 4.2 ms | Uniterm |
| 50,000-line output burst to visible | 391 ms | 370 ms | Herdr |

Herdr completes the output burst about five percent sooner and, as the context table shows, does so while writing far fewer bytes to the outer terminal.
Uniterm is faster or lighter on the other seven.

## Context results

Reported by the harness but not folded into its index, because the products persist and restore different things or because the scenario is a stress case.

| Scenario | Uniterm | Herdr | Better |
| --- | ---: | ---: | --- |
| Bytes written to the outer terminal per output burst | 72.6 KiB | 18.6 KiB | Herdr |
| Output ingest rate | 8.8 MiB/s | 9.3 MiB/s | Herdr |
| Idle memory with 16 panes attached | 85.6 MiB | 111.8 MiB | Uniterm |
| Idle CPU with 16 panes attached | 0.00 % | 1.03 % | Uniterm |
| Memory per added pane | 4.7 MiB | 4.7 MiB | tie |
| Memory returned after closing 15 panes | 96 % | 91 % | Uniterm |
| Idle memory with 3 clients attached | 24.6 MiB | 71.2 MiB | Uniterm |
| Keystroke to visible with 3 clients attached | 2.1 ms | 6.2 ms | Uniterm |
| Resize storm settle (40 resizes over 10,000 lines of scrollback) | 23 ms | 274 ms | Uniterm |
| Resize storm CPU | 40 ms | 110 ms | Uniterm |
| Memory after the resize storm | 39.4 MiB | 51.6 MiB | Uniterm |
| Graceful server shutdown | 45 ms | 305 ms | Uniterm |
| Restart to ready | 24 ms | 57 ms | not ranked |
| Binary size | 5.6 MiB | 23.2 MiB | Uniterm |
| State written to disk after the workloads | 1.7 MiB | 4 KiB | not ranked |

Two of those rows deserve a sentence each.
Uniterm writes about four times as many bytes to the outer terminal per output burst as Herdr does; its frames are correct and its input latency is lower, but Herdr's frame diffing is tighter, and that is a real advantage on slow links.
Uniterm also writes far more state to disk, because it persists scrollback, layout, and an event log so that a Workspace survives a crash with its history; Herdr persists session metadata only.
Restart to ready is not ranked for the same reason: the two products restore different things.

## How to read this

- These are measurements of one workload on one machine and one operating system.
  They say how the two products behaved there, not how they behave everywhere.
- Feature breadth is not part of the score.
  A faster result does not imply feature parity in either direction.
- Herdr's own background network features were disabled for timing, which removes a cost its users pay by default but which is not a rendering or multiplexing cost.
- Numbers change with every release of either product.
  This page names the exact commits; re-run before quoting anything newer.
- The raw JSON for the three runs, the merged report, and the harness configuration live in the benchmark repository; the harness itself is open so the methodology can be checked and the runs repeated.

## Reproducing

The harness builds both products from clean clones of recorded commits, runs its `doctor` check, then runs a profile and writes JSON and Markdown.
Its README documents the build steps, the profiles, the fairness rules, and the publication checklist; the `standard` profile is enough for engineering comparisons and the `marketing` profile is required for any public claim about idle CPU.
