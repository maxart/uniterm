# Developing Uniterm

How to build, test, and find your way around the repository.
[AGENTS.md](../AGENTS.md) is the operating manual with the architectural invariants, and [CONTRIBUTING.md](../CONTRIBUTING.md) lists what a change must pass.

## Building and testing

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets   # warning-free
cargo fmt --all --check                  # formatted
```

All four gates are expected to pass on every change, and CI runs them on Linux and macOS for every push and pull request, plus the whole suite in release mode and the installer's own test.

An opt-in reliability soak repeatedly attaches short-lived control clients while keeping a detected development server live.
For an eight-hour run:

```sh
UNITERM_SOAK_SECONDS=28800 cargo test --release -p uniterm-server --test reliability_soak -- --ignored --nocapture
```

## Focused performance checks

```sh
cargo run --release -p uniterm-server --example render_spike
cargo test -p uniterm-server --release --lib paging_reads_only_visible_payloads_and_recovers_corrupt_older_rows -- --nocapture
cargo test -p uniterm-server --release --test today_idle -- --ignored --nocapture
```

The layout spike measures Dwindle and Scrolling arrangements for 45 Panes, alongside the existing damage and idle-render checks.
The timeline fixture uses 2,048 entries with 4 KiB payloads, measures a warm 128-entry query against decoding every payload, and proves that an off-page corrupt payload is not read until requested.
On the local Linux run on 2026-10-07, the warm query took 2.417 ms versus 7.579 ms for decoding all payloads; building the index and returning the first page took 19.206 ms.
This is an index microbenchmark, not an end-to-end UI latency measurement.
Dwindle and Scrolling arrangement p95 were 0.574 and 0.278 microseconds; renderer p95 peaked at 2.560 microseconds, with zero bytes for an unchanged grid.
The ignored Today check separately measures actual attached idle CPU and render traffic for five minutes.
The same local run observed no idle socket bytes over 326.9 seconds, 0.0008 percent process CPU, and 9,008 KiB resident memory for that Today fixture.
The feature batch passed 781 tests in both debug and release mode, with 5 opt-in tests ignored by each default suite; the Today idle test was also run explicitly.
Workspace build, warning-free Clippy across all targets, formatting, and whitespace checks passed.

## Repository layout

```
crates/
  uniterm-core/     Pure model + logic (grid + damage, layout tree, agent status,
                    orchestration brains, tasks). No UI, no async, no I/O.
  uniterm-proto/    Wire and channel message types.
  uniterm-server/   The mio core loop, damage-tracked renderer, PTYs, persistence,
                    event log, and the agentic surfaces' server side.
  uniterm-client/   The thin attach client, mouse handling, and overlays.
  uniterm-cli/      The `uniterm` binary front door (alias `ut`).
docs/               The design of record.
```

See [`AGENTS.md`](../AGENTS.md) (`CLAUDE.md` is a symlink to it) for the architectural invariants and contribution guide.
