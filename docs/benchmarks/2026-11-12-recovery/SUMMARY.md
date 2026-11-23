# Comparative benchmark results

3 marketing trials on one host; median of run medians, with no pooled samples.
Ranges describe the spread of run medians, not confidence intervals.

| Contender | Balanced index | Run range |
| --- | ---: | ---: |
| uniterm | 88.15 | 88.09 to 88.35 |
| tmux | 87.73 | 87.23 to 88.47 |
| herdr | 38.75 | 37.62 to 40.39 |

Each cell shows the median and the minimum-to-maximum run-median range.

| Metric | Uniterm | Herdr | tmux |
| --- | ---: | ---: | ---: |
| server_startup_ready | 4.576 ms (4.489 ms to 4.606 ms) | 22.402 ms (22.319 ms to 22.553 ms) | 6.329 ms (6.296 ms to 6.392 ms) |
| control_command_latency | 0.382 ms (0.373 ms to 0.391 ms) | 0.823 ms (0.809 ms to 0.887 ms) | 0.530 ms (0.526 ms to 0.542 ms) |
| daemon_idle_cohort_cpu | 0.000 % core (0.000 % core to 0.000 % core) | 0.163 % core (0.163 % core to 0.190 % core) | 0.000 % core (0.000 % core to 0.000 % core) |
| daemon_idle_cohort_rss | 14.56 MiB (14.31 MiB to 14.72 MiB) | 30.03 MiB (29.85 MiB to 30.11 MiB) | 9.02 MiB (8.95 MiB to 9.15 MiB) |
| foreground_idle_cohort_cpu | 0.000 % core (0.000 % core to 0.000 % core) | 0.530 % core (0.530 % core to 0.640 % core) | 0.003 % core (0.000 % core to 0.003 % core) |
| foreground_idle_cohort_rss | 22.39 MiB (22.30 MiB to 22.84 MiB) | 51.10 MiB (51.06 MiB to 53.09 MiB) | 13.21 MiB (13.14 MiB to 13.39 MiB) |
| terminal_input_to_visible | 2.109 ms (2.106 ms to 2.117 ms) | 2.445 ms (2.152 ms to 4.142 ms) | 2.176 ms (2.146 ms to 2.393 ms) |
| terminal_output_completion | 163.254 ms (161.473 ms to 167.093 ms) | 173.232 ms (173.099 ms to 174.948 ms) | 225.691 ms (224.197 ms to 227.585 ms) |
| binary_size | 11106.29 KiB (11106.29 KiB to 11106.29 KiB) | 30342.92 KiB (30342.92 KiB to 30342.92 KiB) | 2275.01 KiB (2275.01 KiB to 2275.01 KiB) |
| client_render_output | 87.06 KiB (87.06 KiB to 87.07 KiB) | 28.42 KiB (28.10 KiB to 28.53 KiB) | 806.70 KiB (749.93 KiB to 832.70 KiB) |
| daemon_idle_root_cpu | 0.000 % core (0.000 % core to 0.000 % core) | 0.163 % core (0.163 % core to 0.190 % core) | 0.000 % core (0.000 % core to 0.000 % core) |
| daemon_idle_root_rss | 10.45 MiB (10.27 MiB to 10.46 MiB) | 25.89 MiB (25.75 MiB to 25.93 MiB) | 4.97 MiB (4.97 MiB to 5.11 MiB) |
| foreground_idle_root_cpu | 0.000 % core (0.000 % core to 0.000 % core) | 0.530 % core (0.530 % core to 0.640 % core) | 0.003 % core (0.000 % core to 0.003 % core) |
| foreground_idle_root_rss | 18.26 MiB (18.23 MiB to 18.53 MiB) | 46.97 MiB (46.93 MiB to 48.85 MiB) | 9.19 MiB (9.07 MiB to 9.32 MiB) |
| isolated_state_size | 1765.32 KiB (1765.32 KiB to 1765.32 KiB) | 5.42 KiB (5.42 KiB to 5.42 KiB) | 0.00 KiB (0.00 KiB to 0.00 KiB) |
| live_suite_shutdown | 37.613 ms (31.958 ms to 38.768 ms) | 308.167 ms (307.937 ms to 309.625 ms) | 16.649 ms (16.196 ms to 17.846 ms) |
| multiclient_3_idle_cohort_cpu | 0.000 % core (0.000 % core to 0.000 % core) | 1.277 % core (1.120 % core to 1.297 % core) | 0.003 % core (0.003 % core to 0.007 % core) |
| multiclient_3_idle_cohort_rss | 36.49 MiB (36.33 MiB to 36.73 MiB) | 94.21 MiB (93.78 MiB to 99.21 MiB) | 21.82 MiB (21.80 MiB to 22.04 MiB) |
| multiclient_3_idle_root_cpu | 0.000 % core (0.000 % core to 0.000 % core) | 1.277 % core (1.120 % core to 1.297 % core) | 0.003 % core (0.003 % core to 0.007 % core) |
| multiclient_3_idle_root_rss | 32.32 MiB (32.23 MiB to 32.48 MiB) | 90.20 MiB (89.77 MiB to 95.13 MiB) | 17.86 MiB (17.70 MiB to 17.96 MiB) |
| multiclient_3_input_to_visible | 2.117 ms (2.110 ms to 2.117 ms) | 4.197 ms (2.150 ms to 4.208 ms) | 2.166 ms (2.138 ms to 2.176 ms) |
| multipane_16_idle_cohort_cpu | 0.000 % core (0.000 % core to 0.000 % core) | 1.210 % core (1.200 % core to 1.443 % core) | 0.003 % core (0.000 % core to 0.003 % core) |
| multipane_16_idle_cohort_rss | 85.30 MiB (85.10 MiB to 85.52 MiB) | 117.51 MiB (117.01 MiB to 117.96 MiB) | 74.52 MiB (74.51 MiB to 74.66 MiB) |
| multipane_16_idle_root_cpu | 0.000 % core (0.000 % core to 0.000 % core) | 1.210 % core (1.200 % core to 1.443 % core) | 0.003 % core (0.000 % core to 0.003 % core) |
| multipane_16_idle_root_rss | 20.35 MiB (19.79 MiB to 20.44 MiB) | 52.08 MiB (51.81 MiB to 52.20 MiB) | 9.24 MiB (9.19 MiB to 9.37 MiB) |
| multipane_process_count | 18.000 processes (18.000 processes to 18.000 processes) | 18.000 processes (18.000 processes to 18.000 processes) | 18.000 processes (18.000 processes to 18.000 processes) |
| multipane_suite_shutdown | 37.083 ms (33.964 ms to 37.866 ms) | 308.717 ms (308.215 ms to 309.197 ms) | 16.012 ms (12.860 ms to 16.370 ms) |
| pane_close_recovery | 95.924 % (95.786 % to 96.406 %) | 93.499 % (93.393 % to 93.605 %) | 99.847 % (99.841 % to 99.848 %) |
| pane_close_rss | 24.35 MiB (23.80 MiB to 24.43 MiB) | 54.59 MiB (54.34 MiB to 54.61 MiB) | 13.22 MiB (13.11 MiB to 13.35 MiB) |
| pane_memory_slope | 4331.000 KiB/pane (4327.082 KiB/pane to 4373.971 KiB/pane) | 4463.988 KiB/pane (4424.929 KiB/pane to 4468.103 KiB/pane) | 4195.200 KiB/pane (4177.835 KiB/pane to 4208.271 KiB/pane) |
| resize_storm_cpu | 20.000 ms CPU (20.000 ms CPU to 30.000 ms CPU) | 50.000 ms CPU (30.000 ms CPU to 50.000 ms CPU) | 10.000 ms CPU (0.000 ms CPU to 10.000 ms CPU) |
| resize_storm_rss | 46.20 MiB (46.10 MiB to 46.56 MiB) | 61.61 MiB (61.61 MiB to 62.91 MiB) | 14.15 MiB (14.04 MiB to 14.35 MiB) |
| resize_storm_settle | 10.969 ms (9.601 ms to 11.139 ms) | 212.939 ms (212.237 ms to 214.888 ms) | 209.519 ms (205.654 ms to 210.579 ms) |
| restart_ready | 5.309 ms (4.913 ms to 5.420 ms) | 22.395 ms (22.102 ms to 22.436 ms) | 7.498 ms (6.982 ms to 7.655 ms) |
| server_shutdown | 30.921 ms (29.309 ms to 32.088 ms) | 306.182 ms (305.901 ms to 308.022 ms) | 12.199 ms (10.675 ms to 12.341 ms) |
| terminal_output_ingest_rate | 21.030 MiB/s (20.552 MiB/s to 21.262 MiB/s) | 19.819 MiB/s (19.624 MiB/s to 19.834 MiB/s) | 15.212 MiB/s (15.086 MiB/s to 15.314 MiB/s) |

## Before and after the Uniterm changes

The earlier campaign used the same host, compiler, harness, profile and comparator binaries.
The campaigns ran at different times on an active desktop; this is not an interleaved A/B experiment.
Zero CPU readings and observer-floor input times should not be interpreted as precise speedups.

| Core metric | Previous Uniterm | Current Uniterm | Previous Herdr/Uniterm | Current Herdr/Uniterm |
| --- | ---: | ---: | ---: | ---: |
| server_startup_ready | 54.359 ms | 4.576 ms | 0.417 | 4.896 |
| control_command_latency | 0.409 ms | 0.382 ms | 2.140 | 2.154 |
| daemon_idle_cohort_cpu | 0.000 % core | 0.000 % core | not interpreted | not interpreted |
| daemon_idle_cohort_rss | 13.83 MiB | 14.56 MiB | 2.061 | 2.063 |
| foreground_idle_cohort_cpu | 0.000 % core | 0.000 % core | not interpreted | not interpreted |
| foreground_idle_cohort_rss | 20.86 MiB | 22.39 MiB | 2.382 | 2.282 |
| terminal_input_to_visible | 2.119 ms | 2.109 ms | 1.179 | 1.159 |
| terminal_output_completion | 207.643 ms | 163.254 ms | 0.839 | 1.061 |

For these lower-is-better metrics, Herdr/Uniterm ratios above 1 favor Uniterm.

CPU scoring applies the harness 0.1%-of-one-core floor and one-percent tie rule.
Observed zero CPU is not proof of zero CPU use; the terminal observer polls at about 2 ms.
Desktop applications remained running. Claims apply to this host and workload.
The generated report, raw JSON, statistics JSON/CSV and provenance accompany this summary.
