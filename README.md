# Uniterm

Uniterm is a terminal multiplexer built for agentic engineering, written in Rust.
It is one static binary that is a complete tmux-class multiplexer and, in the same process, a supervisor for a fleet of AI coding agents.

It is fast and it is small.
The server is ready in 9 ms, a keystroke is on screen in 2 ms, an idle Workspace uses no CPU at all, one attached Workspace with a shell is 16 MiB of memory and sixteen Panes are 86 MiB, and the whole program is a 5.6 MiB binary with no runtime, no webview, and no dependencies to install.
Those are [measured numbers](#how-uniterm-compares), not adjectives: the renderer emits only the cells that changed, nothing wakes the loop just because time passed, and process liveness comes from the kernel rather than polling.

You get persistent Workspaces with Projects, Tabs, and Panes that survive a detach or a client crash, splits and zoom and copy-mode, and built-in save and restore that keeps scrollback and layout.
Because Uniterm owns every terminal grid, it can also tell which agent is working, idle, waiting on a permission prompt, or asking a question, at no extra cost, and it puts that in a monitoring rail beside your terminals, runs deterministic multi-agent workflows, and lets you queue direction for a busy agent without racing its input.

## Set up

Install the latest release; the script verifies the download against the release's SHA-256 manifest:

```sh
curl -fsSL https://raw.githubusercontent.com/maxart/uniterm/main/install.sh | sh
```

Prebuilt binaries cover Apple Silicon macOS, glibc Linux on x86-64 and ARM64, WSL, and Android Termux.
Building from source needs Rust 1.96 and a C toolchain; see [docs/INSTALL.md](docs/INSTALL.md) for that, for install locations, and for where Uniterm keeps its state.

Then, thirty seconds to a working fleet:

```sh
ut                            # attach to your default Workspace, creating it on first run
# Ctrl-A %                    split the Pane left/right; Ctrl-A " splits top/bottom
ut agent start claude --tab   # or codex, cursor, gemini, opencode: a new Tab running that agent
# Ctrl-A o                    toggle the Observatory: agents, files, and web servers beside the terminal
```

From there, `Ctrl-A N` opens the New Task prompt, `Ctrl-A s` switches Workspaces, `Ctrl-A m` opens a command menu that doubles as a cheat sheet, and `ut remote HOST` attaches to a Workspace on another machine over your own SSH connection.
[docs/USAGE.md](docs/USAGE.md) has every key, mouse gesture, command, and surface; [docs/CONFIGURATION.md](docs/CONFIGURATION.md) has the config file and themes.

## What you get

- **A complete multiplexer.**
  Client-server over a Unix socket, the Workspace > Project > Tab > Pane hierarchy, splits, directional focus, resize by key or by dragging a divider, zoom, an overview of every Tab as a live miniature, copy-mode with search and OSC 52 clipboard, alternate-screen apps, and mouse selection and scrolling that just work.
- **Save and restore that tmux plugins cannot match.**
  Projects, Tabs, layouts, working directories, and scrollback are snapshotted atomically and come back after a crash; a clean stop keeps the event stream so run history and the audit trail outlive it.
- **Agent status you can trust.**
  Detection reconciles cooperative signals, provider logs, anchored screen rules, foreground process identity, and kernel exit events; working is a positive match, never output volume, and `ut agent explain` shows the evidence.
- **Orchestration with a completion contract.**
  Workflows and relays advance when a role submits a token, verifiers alone pass verdicts, iterations are capped, and every run has a durable identity you can inspect.
- **An Observatory beside the terminal.**
  Agents by urgency, a sandboxed file manager, and detected web servers, in a rail that never covers your content.
- **Notifications where you are.**
  A toast, a host-terminal or native notification, and a bell, a synthesized chime, or your own sound, played on the machine you are sitting at even when the Workspace is remote.
- **Drivable by agents.**
  A harness-neutral skill teaches any AI agent to run the fleet through `ut`; see below.

The full list, with the test behind each claim, is in [docs/FEATURES.md](docs/FEATURES.md) and [docs/STATUS.md](docs/STATUS.md).

## How Uniterm compares

Herdr is the closest product in the same space, so it is the one Uniterm is benchmarked against.
The comparison below comes from an open harness that builds both products from clean clones, runs them through a real PTY with identical geometry and workload, checks every trial's final screen, and runs the whole profile three times with the order rotated.
Values are medians across the three runs; ties within one percent are called ties.

| Measured on one Linux x86_64 laptop, 2026-09-03 | Uniterm 1.0.0 | Herdr 0.8.2 |
| --- | ---: | ---: |
| Server start to ready | 8.6 ms | 49.3 ms |
| Keystroke to visible | 2.1 ms | 4.2 ms |
| 50,000-line output burst to visible | 391 ms | **370 ms** |
| Bytes written to the terminal per burst | 72.6 KiB | **18.6 KiB** |
| Idle CPU, one Pane attached | 0.00 % | 0.47 % |
| Idle memory, one Pane attached | 15.8 MiB | 40.5 MiB |
| Idle memory, 16 Panes attached | 85.6 MiB | 111.8 MiB |
| Idle memory, 3 clients attached | 24.6 MiB | 71.2 MiB |
| 40 rapid resizes over 10,000 lines of scrollback, settle | 23 ms | 274 ms |
| Graceful shutdown | 45 ms | 305 ms |
| Binary size | 5.6 MiB | 23.2 MiB |

Herdr wins the two rows in bold: it finishes a large output burst about five percent sooner and writes a quarter of the bytes to the outer terminal while doing it, which matters on slow links.
Uniterm is faster or lighter everywhere else, and writes more state to disk because it persists scrollback and an event log that Herdr does not.

Read these as what happened on one machine with one workload, not as universal constants.
Feature breadth is not scored, Herdr's background network checks were disabled for timing, and the numbers move with every release of either product.
[docs/BENCHMARKS.md](docs/BENCHMARKS.md) has the full tables, the exact commits, the profile, and every fairness control.

## Driving Uniterm from an AI agent

[`skills/manage-uniterm/SKILL.md`](skills/manage-uniterm/SKILL.md) is a harness-neutral skill (the plain `SKILL.md` format read by Claude Code, Codex, OpenCode, Cursor, and similar tools) that teaches an agent to find and monitor other agents across Panes, Tabs, and Projects, read and send Pane text, focus and organise the hierarchy, queue direction, and handle the waiting queue, all through `ut`.
Point your harness's skill discovery at the `skills/` directory, or copy the folder into its skills location; `ut --skill` prints a shorter version for agents that cannot load skills.

## Notes

- **Your data stays local.**
  Snapshots and event logs live under `~/.local/state/uniterm/` with owner-only permissions and are never sent anywhere; they can contain terminal output, so treat the directory as sensitive.
  Uniterm has no telemetry and no update checks; its only network use is your own SSH connection for remote Workspaces and a loopback probe that confirms a detected development server is listening.
- **Platforms.**
  Apple Silicon macOS, glibc Linux, WSL, and Android Termux are supported; Intel macOS and native Windows are not.
- **Uniterm Desktop.**
  Uniterm succeeds the earlier GUI application and imports its hierarchy with `ut migrate from-desktop`; if the old `uniterm` executable is still on your `PATH`, prefer `ut`.

## Documentation

- [docs/INSTALL.md](docs/INSTALL.md): releases, building from source, state locations, reproducible release builds.
- [docs/USAGE.md](docs/USAGE.md): the command line, keys, mouse, menus, the Observatory, file manager, Workspaces and Projects, Settings.
- [docs/CONFIGURATION.md](docs/CONFIGURATION.md): the config file, themes, and provider manifests.
- [docs/FEATURES.md](docs/FEATURES.md): the complete feature list.
- [docs/BENCHMARKS.md](docs/BENCHMARKS.md): the comparison above in full.
- [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md): building, testing, and the repository layout.
- [docs/README.md](docs/README.md): the design of record, from vision to each subsystem.

## Contributing

See [AGENTS.md](AGENTS.md) for the architectural invariants, [CONTRIBUTING.md](CONTRIBUTING.md) for the gates every change must pass, and [SECURITY.md](SECURITY.md) for how to report a vulnerability.

## License

MIT.
