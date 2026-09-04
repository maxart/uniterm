# Uniterm CLI - Design Documentation

This folder is the design record for the Uniterm CLI: a terminal multiplexer written in Rust, built for the age of agentic engineering.

It is a tmux-class multiplexer that renders directly to the terminal (no webview), plus a first-class supervisor for fleets of AI coding agents.

The old Tauri/WKWebView Uniterm is being scrapped.
This is its replacement, and it keeps only the part of the old system that was ever cheap: the lean Rust host.

## What this is

One statically-linked Rust binary that is two things at once:

1. A (mostly) complete tmux alternative.
   Client-server, persistent Workspaces, Projects, Tabs, splits, layouts, copy-mode, scripting, and a config system.
   It ships with the equivalents of `tmux-resurrect` and `tmux-continuum` built in by default, and improves on them by persisting scrollback and layout that those plugins cannot.

2. An agent-fleet supervisor.
   Because it owns the terminal grid, it can see what every agent is doing with zero extra cost, detect when an agent is working, idle, blocked on a permission prompt, or asking a question, and surface all of that in a built-in monitoring view (the successor to Uniterm's Observatory).
   It orchestrates multi-agent workflows and relays with deterministic, well-tested state machines.

The north-star property is the one the old app failed: near-zero footprint when nothing visibly changes.

## User guides

These pages describe the product as shipped, for people using it rather than designing it.

| Document | What it covers |
|---|---|
| [INSTALL.md](INSTALL.md) | Releases, building from source, state locations, reproducible release builds |
| [USAGE.md](USAGE.md) | The command line, keys, mouse, menus, the Observatory, file manager, Workspaces and Projects, Settings |
| [CONFIGURATION.md](CONFIGURATION.md) | The config file, themes, and provider detection manifests |
| [FEATURES.md](FEATURES.md) | The complete feature list |
| [BENCHMARKS.md](BENCHMARKS.md) | The measured comparison, Uniterm vs Herdr, with its setup and limits |
| [DEVELOPMENT.md](DEVELOPMENT.md) | Building, testing, and the repository layout |

## Reading order

Start at the top and go down.
Each document assumes you have read the ones before it.

| # | Document | What it covers |
|---|---|---|
| 00 | [00-vision-and-scope.md](00-vision-and-scope.md) | What we are building, for whom, and the non-goals |
| 01 | [01-language-decision.md](01-language-decision.md) | The finalized ADR: why Rust, grounded in the resource diagnostic |
| 03 | [03-system-architecture.md](03-system-architecture.md) | Process model, the two-runtime split, component map, threading |
| 04 | [04-multiplexer-core.md](04-multiplexer-core.md) | Workspace/Project/Tab/Pane hierarchy, grid, damage renderer, control protocol |
| 05 | [05-session-persistence.md](05-session-persistence.md) | Built-in resurrect + continuum: event-sourced state, snapshots, atomic restore |
| 06 | [06-agentic-supervision.md](06-agentic-supervision.md) | Agent status detection, the OSC 777 protocol, the provider trait, exit/liveness |
| 07 | [07-workflows-and-relay.md](07-workflows-and-relay.md) | Deterministic multi-agent orchestration: workflows, relay, verifier gates, checkpoints |
| 08 | [08-observatory.md](08-observatory.md) | The built-in monitoring surface: fleet view, waiting queue, timeline, files, memory |
| 10 | [10-config-commands-keybindings.md](10-config-commands-keybindings.md) | Config format, the command language, keybindings, quick-prompt |
| 13 | [13-desktop-migration.md](13-desktop-migration.md) | Safe hierarchy-only migration from Uniterm Desktop |
| 14 | [14-ssh-remote-sessions.md](14-ssh-remote-sessions.md) | Thin-client remote Workspaces over an SSH stdio bridge |
| 19 | [19-control-api.md](19-control-api.md) | Versioned local NDJSON automation contract, resource snapshots, mutations, and event subscriptions |
| 20 | [20-instruction-queue.md](20-instruction-queue.md) | Event-backed human direction, cooperative delivery, explicit send-now, and invocation safety |
| 21 | [21-worktree-lifecycle.md](21-worktree-lifecycle.md) | Git-authoritative worktree provenance, safe removal, stale cleanup, and restoration |
| 22 | [22-provider-detection-manifests.md](22-provider-detection-manifests.md) | Versioned detection schema, precedence, verified cache, reload, validation, and explain provenance |
| 23 | [23-native-run-graph.md](23-native-run-graph.md) | Stable run and role relationships, lifecycle events, indexed recovery, inspection, and Observatory ownership |
| 24 | [24-per-role-provider-selection.md](24-per-role-provider-selection.md) | Provider-neutral role requirements, explicit mixed-provider launch, durable ownership, and shared automation syntax |
| 25 | [25-typed-artifact-ledger.md](25-typed-artifact-ledger.md) | Stable artifact identity, producer ownership, lifecycle events, recovery, filesystem observation, and inspection |
| 26 | [26-native-workspace-guardrails.md](26-native-workspace-guardrails.md) | Pure Workspace launch policy, bounded runs and roles, captured iteration and elapsed limits, exact Project allow-lists, waiting escalation, and durable decisions |


## Status

[STATUS.md](STATUS.md) is the single record of implementation status.
It lists every user-facing capability Uniterm claims with its status, its entry point, and the test that exercises it, so any claim can be checked against the code rather than against prose.
Do not restate implementation status here or in any other document; link to that table instead.

A status update ships in the same change as the feature it describes, and it must name the feature's entry point and its test.
A capability with no test is recorded there as `none` and must not be described as shipped anywhere.

At the last audit the table held 93 shipped, 5 partial, 15 missing, and 4 deliberately absent capabilities.
The numbered design documents above stay ambitious on purpose and are the design of record, not a release claim.

The resource budgets and architectural invariants remain release gates for every follow-on change.

The 2026-07-30 remote-session release added repaint supersession under backpressure, provider-neutral stranded-screen recovery, and protocol-versioned SSH thin-client attach.

## Naming note

The project is named **Uniterm**.
The binary is `uniterm` with a short alias `ut`, per-project state lives under `.uniterm/`, and the agent-metadata URI scheme is `uniterm://`.
The name is carried forward deliberately from the app it replaces: the webview was the problem, not the identity.
