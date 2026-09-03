# Contributing to Uniterm

Thank you for helping.
Read [AGENTS.md](AGENTS.md) first: it is the operating manual for anyone, human or agent, writing code here, and it lists the invariants that a change must not break.

## Before you open a pull request

Run the four gates; all must pass and clippy must be warning-free:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

Run the test suite in release mode as well when you touch the server or the CLI, because `debug_assert!` and overflow checks behave differently there:

```sh
cargo test --workspace --release
```

Integration tests must isolate their state: call `isolate_state()` and use `unique_workspace_name()` from `tests/common/mod.rs` in any test that binds or spawns a server, so a test run can never touch a real Workspace.

## What a good change looks like

- One concern per pull request, with a commit message that says what changed and why.
- New logic in `uniterm-core` comes with a unit test in the same file; cross-crate behaviour comes with an integration test.
- Anything on the hot path is measured; anything that could run while idle is shown not to.
- A new agent is one provider module and a manifest; nothing else branches on an agent id.
- Documentation changes ship with the feature, and [docs/STATUS.md](docs/STATUS.md) is updated in the same change with the entry point and the test.

## Style

- Match the surrounding code.
- No em dashes or en dashes in prose; use a plain hyphen.
- In long Markdown, one sentence per line.
