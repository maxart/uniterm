# Local benchmark results

Publication held: Uniterm retains memory and idle-CPU advantages over Herdr, but its relative standing is weaker and the same-host follow-up confirms a startup regression.

3 marketing trials; medians of run medians, with no pooled samples.

| Contender | Balanced index | Run range |
| --- | ---: | ---: |
| tmux | 91.51 | 91.39 to 93.03 |
| uniterm | 66.76 | 66.69 to 68.03 |
| herdr | 42.82 | 39.25 to 43.41 |

| Metric | Unit | Uniterm | Herdr | tmux |
| --- | --- | ---: | ---: | ---: |
| server_startup_ready | ms | 54.359 | 22.681 | 6.536 |
| control_command_latency | ms | 0.409 | 0.875 | 0.547 |
| daemon_idle_cohort_cpu | % core | 0.000 | 0.167 | 0.000 |
| daemon_idle_cohort_rss | KiB | 13.83 MiB | 28.50 MiB | 9.08 MiB |
| foreground_idle_cohort_cpu | % core | 0.000 | 0.537 | 0.003 |
| foreground_idle_cohort_rss | KiB | 20.86 MiB | 49.69 MiB | 13.26 MiB |
| terminal_input_to_visible | ms | 2.119 | 2.498 | 2.364 |
| terminal_output_completion | ms | 207.643 | 174.222 | 231.847 |
| binary_size | bytes | 10895.47 KiB | 30342.92 KiB | 2275.01 KiB |
| client_render_output | bytes | 59.68 KiB | 28.56 KiB | 889.06 KiB |
| daemon_idle_root_cpu | % core | 0.000 | 0.167 | 0.000 |
| daemon_idle_root_rss | KiB | 9.84 MiB | 24.40 MiB | 5.03 MiB |
| foreground_idle_root_cpu | % core | 0.000 | 0.537 | 0.003 |
| foreground_idle_root_rss | KiB | 16.63 MiB | 45.74 MiB | 9.13 MiB |
| isolated_state_size | bytes | 1762.29 KiB | 5.42 KiB | 0.00 KiB |
| live_suite_shutdown | ms | 35.818 | 306.601 | 15.343 |
| multiclient_3_idle_cohort_cpu | % core | 0.000 | 1.133 | 0.007 |
| multiclient_3_idle_cohort_rss | KiB | 33.08 MiB | 88.07 MiB | 21.89 MiB |
| multiclient_3_idle_root_cpu | % core | 0.000 | 1.133 | 0.007 |
| multiclient_3_idle_root_rss | KiB | 29.05 MiB | 84.00 MiB | 17.86 MiB |
| multiclient_3_input_to_visible | ms | 2.113 | 2.110 | 2.196 |
| multipane_16_idle_cohort_cpu | % core | 0.000 | 1.207 | 0.003 |
| multipane_16_idle_cohort_rss | KiB | 84.57 MiB | 114.32 MiB | 74.31 MiB |
| multipane_16_idle_root_cpu | % core | 0.000 | 1.207 | 0.003 |
| multipane_16_idle_root_rss | KiB | 19.06 MiB | 49.11 MiB | 9.23 MiB |
| multipane_process_count | processes | 18.000 | 18.000 | 18.000 |
| multipane_suite_shutdown | ms | 30.892 | 308.487 | 16.473 |
| pane_close_recovery | % | 95.700 | 92.873 | 99.836 |
| pane_close_rss | KiB | 23.22 MiB | 51.97 MiB | 13.38 MiB |
| pane_memory_slope | KiB/pane | 4357.300 | 4427.871 | 4162.888 |
| resize_storm_cpu | ms CPU | 30.000 | 30.000 | 10.000 |
| resize_storm_rss | KiB | 44.52 MiB | 59.29 MiB | 14.23 MiB |
| resize_storm_settle | ms | 11.017 | 211.366 | 208.955 |
| restart_ready | ms | 5.226 | 22.729 | 6.898 |
| server_shutdown | ms | 30.952 | 306.293 | 11.929 |
| terminal_output_ingest_rate | MiB/s | 16.535 | 19.706 | 14.808 |

## Change in standing against Herdr

Historical results used different hardware, kernel, compiler, and application versions.
These ratios describe relative standing in two campaigns; they do not isolate a Uniterm code regression.

Herdr/Uniterm ratios above 1 favor Uniterm for these lower-is-better metrics.

| Metric | Historical Herdr/Uniterm | Current Herdr/Uniterm |
| --- | ---: | ---: |
| server_startup_ready | 5.448 | 0.417 |
| control_command_latency | 2.616 | 2.140 |
| daemon_idle_cohort_rss | 2.152 | 2.061 |
| foreground_idle_cohort_rss | 2.438 | 2.382 |
| terminal_input_to_visible | 3.991 | 1.179 |
| terminal_output_completion | 0.949 | 0.839 |

CPU ranking applies the harness 0.1%-of-one-core floor; observed zero is not proof of no CPU use.
Desktop applications remained running. Claims apply to this host and workload.
See raw JSON, generated report, per-trial statistics, and provenance.json for evidence.

## Same-host startup regression check

Twenty paired trials, alternating version order, using the unchanged harness startup adapter, clean release builds and Rust 1.96.0 on this host.

| Version | Median startup | Sample range |
| --- | ---: | ---: |
| Uniterm 1.0.4 | 3.773 ms | 3.450 to 4.323 ms |
| Uniterm 1.2.20 | 53.255 ms | 47.956 to 60.222 ms |

Current startup is 14.11 times slower in this controlled follow-up.
This confirms a startup regression; it does not isolate the responsible code change or establish same-host regressions for the other metrics.
Raw evidence: startup-followup.json; build identity: provenance.json; reproduction source: startup-check.rs and its Cargo files; see NOTES.md.
