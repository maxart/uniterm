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
