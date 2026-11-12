# Campaign provenance and limitations

This is the historical pre-change campaign, originally held from publication because of the confirmed startup regression.
It is retained alongside the later recovery campaign so the regression and before/after results remain visible.
The application repositories and benchmark harness source were left unchanged.

The selected releases are Uniterm v1.2.20, Herdr stable v0.9.5, and tmux 3.8.
Uniterm's release tag, local HEAD, and upstream HEAD resolved to the same commit at selection time.
Herdr's stable release was verified using GitHub's latest-release endpoint and its immutable v0.9.5 release page.
The local Herdr checkout was not updated; a clean temporary clone fetched the exact release tag.
The tmux binary was built from the official 3.8 release tarball; all 208 distributed C/header source files match the clean tag checkout.
Exact revisions and artifact hashes are in provenance.json and every raw run report.

The unchanged ut-compare 0.3.0 harness is commit 76026d6233515d1e568ba60f5837bbb529e626cb.
Its normal test suite passed 52 tests; two explicitly opt-in real-tmux tests were skipped.
The real-product smoke suite then completed all 36 metrics for all three contenders, with no recorded failures.
The smoke report has one path-only warning because the tmux build initially lived outside a directory named release.
Before the long campaign, the identical tmux binary was copied into a release directory; its hash did not change.

## Build reproduction

Uniterm and Herdr were built from clean source clones with cargo +stable build --release --locked.
Uniterm selected package uniterm-cli.
The compiler was Rust 1.96.0; Herdr used Zig 0.16.0 with its default ReleaseFast and SIMD-enabled terminal-engine build.
Herdr's repository toolchain pin is 1.96.1; the explicit +stable invocation used the available 1.96.0 compiler for both products.
No source or lockfile modifications were needed.
The tmux build used its default -O2 optimization and locally built static libevent 2.1.12 and ncurses 6.5 tinfow libraries.
The tmux sources were not modified.
The first ncurses install attempted the system terminfo path and was denied; installation was rerun with a temporary terminfo destination.
The tmux configure step was rerun with explicit LIBTINFOW_CFLAGS/LIBTINFOW_LIBS to find the local wide-character library.
All build trees and dependencies were temporary; no installed application binary was replaced.

## Execution

The host is native Bazzite 45 Linux x86_64, kernel 7.2.15, AMD Ryzen AI MAX+ 395, 32 logical CPUs.
The observed platform profile and CPU governor were performance.
No power-policy or CPU-affinity changes were made.
Desktop applications remained running; this is not a dedicated laboratory host.
The benchmark runs sequentially and a sleep inhibitor prevents suspend during collection.
No builds ran during measured trials.

Every product receives private HOME/XDG directories, private sockets, /bin/sh, identical terminal dimensions and workloads.
Herdr update and manifest network checks are disabled by the harness.
The current marketing profile uses 160x50 geometry, 300-second idle windows, 20 startup samples, 50 control samples, 100 latency samples, ten 50,000-line bursts, 16 panes, 40 resizes, and three attached clients in the multi-client scenario.
Seeds 0, 1, and 2 rotate contender order.
Screen correctness and complete pane counts gate valid measurements.
No security/privacy assessment was performed; performance results do not imply an assurance verdict.

The outer PTY observer checks for output at approximately 2 ms intervals.
Values clustered around 2 ms cannot establish sub-millisecond latency differences and do not measure physical display painting.
CPU percentages are percentages of one logical core, and observed zero does not prove zero work.
The balanced index floors CPU at 0.1 percent and treats values within one percent as ties.
The summary reports medians of independent run medians, preserving each run's range, p95 and count in statistics.json.
Ranges are not confidence intervals.

## Historical comparison

The archived baseline used Uniterm v1.0.4, Herdr v0.8.2, tmux 3.7c, the same harness commit and marketing workload, six logical CPUs, kernel 7.1.9, and Rust 1.97.1.
Hardware, compiler, OS and all three product versions differ.
Comparisons with that archive describe changes in relative standing, not a controlled attribution to Uniterm source changes.
The separate startup follow-up rebuilds Uniterm 1.0.4 with the same compiler and runs 20 alternating pairs through the unchanged AppAdapter::start implementation.
It confirms the startup regression on this host; it does not establish same-host changes in the other metrics.

## Report reproduction

Run the supplied config with ut-compare run --profile marketing and seeds 0, 1, and 2.
Generate report.md using ut-compare report --output report.md marketing-1.json marketing-2.json marketing-3.json.
The supplied summarize.py creates statistics.json, statistics.csv, and SUMMARY.md from the untouched raw reports.
Its balanced-index calculation was checked against all three harness-generated smoke scores.

## Retained evidence

The raw reports are unchanged from the completed three-run campaign.
Machine-local logs, process inventories, timestamps and private build paths are excluded from this public bundle.
The config uses placeholder paths; set them to the clean source checkouts and binaries matching provenance.json.
SHA256SUMS covers the files in this bundle.

The derived CSV uses LF line endings for source control; its measurements are unchanged.

## Supplemental startup reproduction

The original startup-followup.json and historical baseline-statistics.json are included unchanged.
The latter supports the historical-ratio section of SUMMARY.md; it is separate from the same-host before/after recovery comparison.
To reproduce the 20-pair follow-up, use a scratch directory with the pinned harness checkout named harness-src and a sibling startup-check directory.
Copy startup-check.rs to startup-check/src/main.rs, startup-check.Cargo.toml to startup-check/Cargo.toml and startup-check.Cargo.lock to startup-check/Cargo.lock.
Set the placeholder paths in startup.local.toml to the recorded Uniterm 1.0.4 and 1.2.20 builds, and create a reports directory alongside that config.
From that directory, run cargo run --release --locked --manifest-path startup-check/Cargo.toml using Rust 1.96.0 and an isolated benchmark environment with inherited UNITERM routing variables removed.
The unchanged adapter creates private HOME/XDG state; the follow-up writes reports/startup-followup.json.
These diagnostic samples do not enter the three-run campaign statistics.

Public exports replace machine-local benchmark paths with `/path/to/uniterm-benchmark`; SHA256SUMS describes this sanitized copy.
