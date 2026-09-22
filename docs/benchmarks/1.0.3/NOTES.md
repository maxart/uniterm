# Uniterm v1.0.3 comparison evidence

Both binaries were built from the official release tags.
Intel Core 5 320, six logical CPUs, Omarchy 4.0.2, kernel 7.1.9-arch1-2; native Linux, not WSL.
Measured 2026-09-05.

| Product | Source commit | Artifact SHA-256 |
| --- | --- | --- |
| Uniterm (ut) | `2a6da6bf370d7ab33f164460817b46086ba96314` | `2fb92ae4ac3e97ef7ee5946a4b98430af052fb9880ac33a012c72899f727a097` |
| Herdr | `9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c` | `b9ba0c3a056313022281192b3379b4439f07690b9188e956085fd38cbf586fa4` |

## Host and operation

At the start: AC connected, battery charging, CPU governor powersave, platform profile balanced.
Settings were observed, not changed.
The desktop and existing user applications remained running.
No compilation ran concurrently with measurement.
The harness consumed a real PTY directly, with TERM=xterm-256color; a graphical terminal emulator was not in the measured rendering path.
Host samples before and after each run are in host-observations.jsonl.

## Reproduce

Use [ut-compare](https://github.com/maxart/uniterm-benchmark), version 0.2.0, with the source commits above.
Build native release binaries from clean clones with locked dependencies; Herdr requires Zig 0.15.2.
Use the marketing profile without changing workload settings: 160x50 PTY, /bin/sh, 300 s idle windows, 20 startup, 50 control, 100 latency, ten 50,000-line output bursts, 16 panes, 40 resizes, and two extra clients.
Herdr's network checks and onboarding are disabled; its headless geometry matches the PTY.

```sh
ut-compare report --output report.md marketing-1.json marketing-2.json marketing-3.json
```

The human summary uses the median of the three run medians.
The generated report retains each run separately.
All reported latency/output screen checks passed and both products completed every recorded metric with no contender errors.
A zero CPU reading is resolution-limited.
These results apply only to this Linux host and workload; they establish neither macOS nor WSL performance.
Binary size, state size, feature breadth, and assurance are separate from the eight-metric performance index.
The two versions use different Herdr revisions and run dates, so they are not a controlled Uniterm-only regression comparison.

The exact harness commit and executable hash are recorded in provenance.json, along with the build commands and product compilers (repository-pinned Rust 1.96.0 for Uniterm and 1.96.1 for Herdr).
The raw host.rustc field records the comparator compiler, Rust 1.97.1, not the product compilers.
The checked-in configuration changed only assurance evidence; benchmark source and profile workloads remained unchanged.
The archived configuration uses paths relative to the benchmark repository root; copy it there as comparison.local.toml or adjust the paths before running.
Seeds: 20260905, 20260906, 20260907.
Smoke and standard validation also completed successfully before marketing collection.

Public exports replace machine-local benchmark paths with `/path/to/uniterm-benchmark`; SHA256SUMS describes this sanitized copy.
