# Benchmarks

Measured 2026-11-12 on native Bazzite 45 Linux x86_64, AMD Ryzen AI MAX+ 395 (32 logical CPUs), kernel 7.2.15.
Uniterm is the tested development commit `4274e426e`, compared with Herdr stable 0.9.5 and tmux 3.8.
It is not a measurement of the previously published Uniterm 1.2.20 binary.

| Metric | Uniterm dev | Herdr 0.9.5 | tmux 3.8 |
| --- | ---: | ---: | ---: |
| Startup ready | 4.576 ms | 22.402 ms | 6.329 ms |
| Control command | 0.382 ms | 0.823 ms | 0.530 ms |
| Detached idle CPU (one core) | 0.000% | 0.163% | 0.000% |
| Detached cohort RSS | 14.56 MiB | 30.03 MiB | 9.02 MiB |
| Attached idle CPU (one core) | 0.000% | 0.530% | 0.003% |
| Attached cohort RSS | 22.39 MiB | 51.10 MiB | 13.21 MiB |
| Input to visible output | 2.109 ms | 2.445 ms | 2.176 ms |
| 50,000-line burst | 163.254 ms | 173.232 ms | 225.691 ms |

On this same host, startup changed from 54.359 to 4.576 ms and 50,000-line bursts from 207.643 to 163.254 ms compared with Uniterm 1.2.20.
The 50,000-line burst used 5.8% less elapsed time than Herdr on this host.
Render output measured 87.06 KiB versus 59.68 KiB before and 28.42 KiB for Herdr; tmux used less memory.
Attached cohort RSS rose from 20.86 to 22.39 MiB; three-client RSS rose from 33.08 to 36.49 MiB.
Balanced index: uniterm 88.15/100, herdr 38.75/100, tmux 87.73/100.
Each score is the median of three per-run geometric-mean indices, normalized to the best result in each of the eight core metrics, with a 0.1%-of-one-core CPU floor and one-percent per-metric ties.
Uniterm and tmux have overlapping run ranges; their aggregate ordering does not establish a significant difference.
It is not a count of wins or a feature score.

## Evidence and method

Three unchanged `ut-compare 0.3.0` marketing runs, seeds 0/1/2: 160x50 PTY, `/bin/sh`, five-minute idle windows, 20 startups, 50 control requests, 100 input samples, ten 50,000-line bursts, 16 panes, 40 resizes and three clients.
Every contender completed all 36 metrics in every run, passed the screen oracles and exited cleanly.
Values are medians of three run medians; ranges are not confidence intervals.

[All metrics and run ranges](benchmarks/2026-11-12-recovery/SUMMARY.md), [raw runs and reproduction](benchmarks/2026-11-12-recovery/NOTES.md), [statistics](benchmarks/2026-11-12-recovery/statistics.json), [artifact provenance](benchmarks/2026-11-12-recovery/provenance.json) and [checksums](benchmarks/2026-11-12-recovery/SHA256SUMS).
The archive includes memory scaling, render bytes, resizing, shutdown, state size and every other measured result.
The [same-host pre-change campaign](benchmarks/2026-11-11/SUMMARY.md) retains the original Uniterm 1.2.20 results, including its startup regression.

All products use private HOME/XDG trees, identical shell workloads and default retention; Herdr's update/manifest checks are disabled during timing.
The exact source revisions, compiler, build options and binary hashes are archived.
No builds or agent timing experiments ran during collection; desktop applications remained active.
The before/after campaigns used the same host and comparator binaries at different times, not an interleaved A/B design.

CPU is percent of one core; measured zero does not prove zero work.
Input observations near the approximately 2 ms polling floor cannot establish sub-millisecond differences or physical display latency.
RSS includes the server, clients, shells and descendants; retention and feature differences matter.
Uniterm retains checkpoint/agent infrastructure; Herdr's optional pane-screen history was off, and tmux has no native disk restoration.
State size and restart are context, not equivalent recovery scores.
These measurements apply to this host and workload; assurance was not evaluated.

## Earlier results and local budgets

The [1.0.4 comparison](BENCHMARKS-v1.0.4.md) used a different host, compiler and product versions; it cannot attribute changes to Uniterm alone.
It also retains the earlier checkpoint and renderer subsystem measurements.
[1.0.3](BENCHMARKS-v1.0.3.md) and [1.0.0](BENCHMARKS-v1.0.0.md) retain their original evidence.

Release fixtures provide separate parser, rendering and persistence regression budgets:

```sh
cargo run --release --locked -p uniterm-server --example terminal_burst
cargo run --release --locked -p uniterm-server --example burst_render_budget
cargo run --release --locked -p uniterm-server --example render_spike
cargo run --release --locked -p uniterm-server --example checkpoint_spike
```
