# Uniterm 1.0.4, Herdr 0.8.2, and tmux 3.7c

Three complete marketing trials on one native Linux x86-64 host, with seeds 0, 1, and 2.
Each contender completed all 36 metrics in every trial; all benchmark processes exited.
Cells show the median of the three run medians, followed by their minimum and maximum.
The range describes observed trial variation, not a confidence interval.
Per-trial p95 values and sample counts are in statistics.json; full samples are in the raw JSON.

## Balanced index

These are medians and ranges of the three independent per-trial indices.
The eight core metrics, one-percent tie rule, and 0.1-percent-of-one-core CPU floor are unchanged.

| Contender | Median index | Trial range | Trials 1 / 2 / 3 |
| --- | ---: | ---: | --- |
| Uniterm 1.0.4 | 98.7 | 98.6 to 98.8 | 98.7 / 98.6 / 98.8 |
| Herdr 0.8.2 | 36.1 | 35.7 to 36.6 | 36.6 / 36.1 / 35.7 |
| tmux 3.7c | 85.8 | 85.3 to 86.6 | 86.6 / 85.8 / 85.3 |

## Core metrics

| Metric | Uniterm 1.0.4 | Herdr 0.8.2 | tmux 3.7c |
| --- | ---: | ---: | ---: |
| `server_startup_ready` | 8.23 ms (8.09 ms to 8.76 ms) | 44.85 ms (44.43 ms to 47.88 ms) | 15.49 ms (14.87 ms to 16.11 ms) |
| `control_command_latency` | 1.35 ms (1.26 ms to 1.43 ms) | 3.53 ms (3.41 ms to 3.54 ms) | 2.18 ms (2.09 ms to 2.23 ms) |
| `daemon_idle_cohort_cpu` | 0.000 % core (0.000 % core to 0.000 % core) | 0.257 % core (0.250 % core to 0.263 % core) | 0.000 % core (0.000 % core to 0.000 % core) |
| `daemon_idle_cohort_rss` | 11.19 MiB (11.04 MiB to 11.26 MiB) | 24.09 MiB (23.93 MiB to 24.28 MiB) | 10.58 MiB (10.57 MiB to 10.66 MiB) |
| `foreground_idle_cohort_cpu` | 0.000 % core (0.000 % core to 0.000 % core) | 0.427 % core (0.427 % core to 0.440 % core) | 0.003 % core (0.003 % core to 0.007 % core) |
| `foreground_idle_cohort_rss` | 15.77 MiB (15.63 MiB to 15.79 MiB) | 38.45 MiB (38.34 MiB to 38.48 MiB) | 16.96 MiB (16.91 MiB to 17.11 MiB) |
| `terminal_input_to_visible` | 2.08 ms (2.08 ms to 2.09 ms) | 8.31 ms (8.31 ms to 8.32 ms) | 2.09 ms (2.09 ms to 2.09 ms) |
| `terminal_output_completion` | 392.52 ms (390.58 ms to 398.39 ms) | 372.65 ms (371.63 ms to 373.80 ms) | 391.01 ms (382.76 ms to 395.76 ms) |

## Context metrics

| Metric | Uniterm 1.0.4 | Herdr 0.8.2 | tmux 3.7c |
| --- | ---: | ---: | ---: |
| `binary_size` | 5.64 MiB (5.64 MiB to 5.64 MiB) | 23.40 MiB (23.40 MiB to 23.40 MiB) | 1.35 MiB (1.35 MiB to 1.35 MiB) |
| `client_render_output` | 55.49 KiB (55.46 KiB to 55.51 KiB) | 18.62 KiB (18.43 KiB to 18.78 KiB) | 2.24 MiB (2.16 MiB to 2.27 MiB) |
| `daemon_idle_root_cpu` | 0.000 % core (0.000 % core to 0.000 % core) | 0.257 % core (0.250 % core to 0.263 % core) | 0.000 % core (0.000 % core to 0.000 % core) |
| `daemon_idle_root_rss` | 6.94 MiB (6.75 MiB to 6.96 MiB) | 19.78 MiB (19.57 MiB to 19.90 MiB) | 6.28 MiB (6.27 MiB to 6.30 MiB) |
| `foreground_idle_root_cpu` | 0.000 % core (0.000 % core to 0.000 % core) | 0.427 % core (0.427 % core to 0.440 % core) | 0.003 % core (0.003 % core to 0.007 % core) |
| `foreground_idle_root_rss` | 11.45 MiB (11.31 MiB to 11.45 MiB) | 34.05 MiB (33.95 MiB to 34.08 MiB) | 12.64 MiB (12.51 MiB to 12.75 MiB) |
| `isolated_state_size` | 1.70 MiB (1.70 MiB to 1.70 MiB) | 4.70 KiB (4.70 KiB to 4.83 KiB) | 0 bytes (0 bytes to 0 bytes) |
| `live_suite_shutdown` | 45.18 ms (43.05 ms to 46.11 ms) | 79.14 ms (79.10 ms to 79.21 ms) | 27.09 ms (26.83 ms to 28.51 ms) |
| `multiclient_3_idle_cohort_cpu` | 0.000 % core (0.000 % core to 0.000 % core) | 0.720 % core (0.717 % core to 0.730 % core) | 0.003 % core (0.003 % core to 0.007 % core) |
| `multiclient_3_idle_cohort_rss` | 24.54 MiB (24.37 MiB to 24.56 MiB) | 65.50 MiB (65.24 MiB to 65.60 MiB) | 29.62 MiB (29.58 MiB to 29.68 MiB) |
| `multiclient_3_idle_root_cpu` | 0.000 % core (0.000 % core to 0.000 % core) | 0.720 % core (0.717 % core to 0.730 % core) | 0.003 % core (0.003 % core to 0.007 % core) |
| `multiclient_3_idle_root_rss` | 20.20 MiB (20.03 MiB to 20.22 MiB) | 61.20 MiB (60.92 MiB to 61.25 MiB) | 25.29 MiB (25.26 MiB to 25.31 MiB) |
| `multiclient_3_input_to_visible` | 2.08 ms (2.08 ms to 2.09 ms) | 12.54 ms (12.43 ms to 14.46 ms) | 2.09 ms (2.09 ms to 2.19 ms) |
| `multipane_16_idle_cohort_cpu` | 0.000 % core (0.000 % core to 0.000 % core) | 0.993 % core (0.980 % core to 1.003 % core) | 0.003 % core (0.003 % core to 0.007 % core) |
| `multipane_16_idle_cohort_rss` | 82.46 MiB (82.22 MiB to 82.70 MiB) | 108.30 MiB (108.13 MiB to 108.54 MiB) | 81.49 MiB (81.33 MiB to 81.53 MiB) |
| `multipane_16_idle_root_cpu` | 0.000 % core (0.000 % core to 0.000 % core) | 0.993 % core (0.980 % core to 1.003 % core) | 0.003 % core (0.003 % core to 0.007 % core) |
| `multipane_16_idle_root_rss` | 13.78 MiB (13.72 MiB to 13.79 MiB) | 39.50 MiB (39.30 MiB to 39.52 MiB) | 12.65 MiB (12.57 MiB to 12.67 MiB) |
| `multipane_process_count` | 18 (18 to 18) | 18 (18 to 18) | 18 (18 to 18) |
| `multipane_suite_shutdown` | 44.69 ms (44.16 ms to 46.15 ms) | 79.73 ms (79.65 ms to 80.00 ms) | 24.55 ms (24.03 ms to 24.57 ms) |
| `pane_close_recovery` | 96.13 % (96.04 % to 96.29 %) | 93.90 % (93.32 % to 94.01 %) | 99.86 % (99.86 % to 99.87 %) |
| `pane_close_rss` | 18.07 MiB (18.06 MiB to 18.11 MiB) | 41.36 MiB (41.05 MiB to 42.06 MiB) | 16.95 MiB (16.81 MiB to 17.00 MiB) |
| `pane_memory_slope` | 4.44 MiB/pane (4.44 MiB/pane to 4.47 MiB/pane) | 4.71 MiB/pane (4.69 MiB/pane to 4.71 MiB/pane) | 4.30 MiB/pane (4.29 MiB/pane to 4.31 MiB/pane) |
| `resize_storm_cpu` | 60.00 ms CPU (50.00 ms CPU to 60.00 ms CPU) | 90.00 ms CPU (90.00 ms CPU to 100.00 ms CPU) | 10.00 ms CPU (0.00 ms CPU to 20.00 ms CPU) |
| `resize_storm_rss` | 42.11 MiB (41.98 MiB to 42.18 MiB) | 48.95 MiB (47.52 MiB to 49.02 MiB) | 17.96 MiB (17.91 MiB to 18.14 MiB) |
| `resize_storm_settle` | 22.02 ms (13.55 ms to 22.81 ms) | 269.94 ms (265.93 ms to 275.70 ms) | 207.05 ms (206.94 ms to 209.27 ms) |
| `restart_ready` | 9.37 ms (8.83 ms to 9.47 ms) | 53.13 ms (52.47 ms to 53.88 ms) | 16.39 ms (16.10 ms to 16.72 ms) |
| `server_shutdown` | 45.05 ms (44.04 ms to 47.55 ms) | 305.42 ms (305.29 ms to 305.50 ms) | 25.50 ms (25.49 ms to 27.41 ms) |
| `terminal_output_ingest_rate` | 8.75 MiB/s (8.62 MiB/s to 8.79 MiB/s) | 9.21 MiB/s (9.18 MiB/s to 9.24 MiB/s) | 8.78 MiB/s (8.68 MiB/s to 8.97 MiB/s) |

## Interpretation and evidence

A zero CPU reading does not establish zero CPU consumption.
RSS covers roots and their transitive descendants where the metric says cohort.
Only the eight core metrics enter the index; artifact size, state size, process count, extended scenarios, and assurance do not.
tmux restart readiness measures a fresh session; native disk restoration is N/A.
Assurance is unreviewed and remains unknown for all three contenders.
These observations establish neither macOS nor WSL performance and are not a controlled comparison with older campaigns.

[Per-trial report](report.md), [run 1](marketing-1.json), [run 2](marketing-2.json), [run 3](marketing-3.json), [CSV](statistics.csv), [provenance and reproduction](NOTES.md).
