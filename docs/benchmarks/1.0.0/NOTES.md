# Uniterm v1.0.0 comparison evidence

This preserves the September 3 baseline.
Both binaries came from development commits after their release tags while reporting versions 1.0.0 and 0.8.2.
These are historical builds, not measurements of those exact release tags.
Historical Linux x86_64 laptop run, six logical CPUs, kernel 7.1.9-arch1-2.
Measured 2026-09-03 to 2026-09-04.

| Product | Source commit | Artifact SHA-256 |
| --- | --- | --- |
| Uniterm (ut) | `8167fd46440db34f6e1c084362063ffb36cd79ff` | `a16fbb64abe9dfae1b45700325863133fd896220fac33ac2ee20ee8bafafe7d8` |
| Herdr | `45484aab84430ac2b18c7bbf44aba15f2b039677` | `32bdd68b6b89c1df873fed6288a4319d9bef4cec487a6de51a464f0f8aaf3563` |

## Host and operation

The original raw JSON records the host and compiler.
AC/battery status, governor, thermal readings, other foreground load, and exact harness commit were not retained with the historical reports and cannot be reconstructed reliably.
Do not assume they match the new run.

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

The Uniterm v1.0.0 tag points to 1b0d9e7413fc34e1e25f48b6dd6c4a37a7113803, whereas the measured historical build came from 8167fd46440db34f6e1c084362063ffb36cd79ff.
Historical summary values are regenerated from the original JSON with consistent precision; the original raw samples remain unchanged.

Public exports replace machine-local benchmark paths with `/path/to/uniterm-benchmark`; SHA256SUMS describes this sanitized copy.
