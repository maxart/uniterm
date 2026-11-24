# Campaign provenance and limitations

This campaign measures Uniterm development commit `4274e426e22e9d9ec8e85fe8fe7a3ea55e8f6605`, Herdr stable v0.9.5 and tmux 3.8.
The Uniterm binary reports `uniterm 1.2.20-dev+g4274e426e`; it is not the previously published v1.2.20 binary.
The harness redacts development version suffixes in raw output; the full revision and binary SHA-256 remain in each raw report and provenance.json.
Herdr's latest stable release was rechecked against its official latest-release API on 2026-11-12: [v0.9.5](https://github.com/herdrdev/herdr/releases/tag/v0.9.5), published 2026-10-14.

## Build and execution

The unchanged `ut-compare 0.3.0` harness is commit `76026d6233515d1e568ba60f5837bbb529e626cb`.
All three measured source checkouts are clean; exact revisions, binary hashes and build details are in provenance.json.
The Uniterm native release build uses the repository's normal release profile and locked dependencies.
Uniterm and Herdr use Rust 1.96.0; Herdr's repository pins 1.96.1, so both products were explicitly built with the available common compiler.
Herdr uses Zig 0.16.0, ReleaseFast and its default SIMD-enabled terminal engine.
The tmux 3.8 build uses default -O2 optimization and static libevent 2.1.12 and ncurses 6.5 tinfow.
All 208 C/header files in its official release tarball match the clean source tag.
The comparator and harness binaries are identical to the [pre-change campaign](../2026-11-11/NOTES.md).

The host is native Bazzite 45 Linux x86_64, kernel 7.2.15, AMD Ryzen AI MAX+ 395, with 32 logical CPUs.
The observed CPU governor and platform profile were performance; no affinity or power-policy changes were made.
Desktop applications remained active, so this is not a dedicated laboratory host.
Runs were sequential under a sleep inhibitor, with no builds or agent timing experiments during collection.
No installed application binary or running user Workspace was replaced.

Every product receives private HOME/XDG directories, private sockets, `/bin/sh`, identical terminal dimensions and workloads.
Herdr update and manifest checks are disabled by the unchanged harness.
The marketing profile uses 160x50 geometry, 300-second idle windows, 20 startup samples, 50 control samples, 100 input samples, ten 50,000-line shell-generated bursts, 16 panes, 40 resizes and three clients.
Seeds 0, 1 and 2 rotate contender order.
All 324 metric records completed without errors or warnings; screen correctness, complete pane counts and process cleanup passed in each run.

## Interpretation

Values are medians of three run medians, without pooling samples.
Statistics JSON retains each run's median, p95 and sample count; ranges describe variation between run medians and are not confidence intervals.
The balanced index is the geometric mean of normalized performance ratios for the eight core metrics.
CPU is percent of one core, with a 0.1-percent scoring floor and one-percent ties.
Measured zero does not prove zero CPU work.
Input observations near the approximately 2 ms observer polling floor cannot establish sub-millisecond differences or physical display latency.

RSS covers the server, clients, shells and descendants, and retention defaults differ.
Uniterm keeps checkpoint and agent infrastructure; Herdr's optional pane-screen history is off, and tmux has no native disk restoration.
Disk-state size and restart readiness therefore describe different recovery behavior and are not persistence-quality rankings.
Render bytes and every other context metric are retained, including metrics where Uniterm loses.
No security or privacy assurance assessment was performed.

The same-host before/after comparison uses the same harness, compiler, profile and comparator binaries at different times on an active desktop.
It is not an interleaved A/B experiment.
The older v1.0.4 archive uses different hardware, compiler and product versions, so it cannot attribute changes to Uniterm alone.
Supplemental parser and fast-producer diagnostics guided implementation; they do not replace or enter these publication measurements.

## Correctness checks

Before this campaign, the integrated source passed workspace build, debug and release tests (1,207 passed and 11 ignored each), warning-free Clippy, formatting, connector tests and the public-export check.
Renderer, burst-rendering, parser, checkpoint and history budget examples also passed.
Tests used private HOME/XDG trees, and the real configuration remained unchanged.
Astra and Opus reviewed the integrated source and found no blocker or feature loss.
The changed PTY module also passed an Apple Silicon macOS target compile check; this is not a macOS runtime benchmark or a full macOS workspace test.

## Reproduction and evidence

Build the clean revisions with the compiler and release options recorded in provenance.json, then set the placeholder paths in comparison.toml.
The recorded hashes identify the measured artifacts; independent builds may have different hashes because of build paths and environment.
Run the pinned harness once per seed:

```sh
ut-compare run --config comparison.toml --profile marketing --seed 0 --output marketing-1.json --markdown marketing-1.md
ut-compare run --config comparison.toml --profile marketing --seed 1 --output marketing-2.json --markdown marketing-2.md
ut-compare run --config comparison.toml --profile marketing --seed 2 --output marketing-3.json --markdown marketing-3.md
ut-compare report --output report.md marketing-1.json marketing-2.json marketing-3.json
python3 summarize.py --baseline ../2026-11-11/statistics.json marketing-1.json marketing-2.json marketing-3.json
sha256sum -c SHA256SUMS
```

Raw reports are unchanged from the completed campaign.
Machine-local logs, process inventories and private build paths are excluded; the harness already omits host identities, absolute paths and terminal contents from its raw reports.
SHA256SUMS covers every file in this bundle except itself.

The derived CSV uses LF line endings for source control; its measurements are unchanged.

Public exports replace machine-local benchmark paths with `/path/to/uniterm-benchmark`; SHA256SUMS describes this sanitized copy.
