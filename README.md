# Uniterm

Uniterm is a terminal multiplexer built for agentic engineering, written in Rust.
It is one static binary that is a complete tmux-class multiplexer and, in the same process, a supervisor for a fleet of AI coding agents.

Run a dozen agents, keep every one of them in view, and stop wondering which is working, which is stuck, and which has been waiting on you for the last twenty minutes.
Uniterm puts that answer in a rail beside your terminals, queues the moments that need a human, and lets you hand direction to a busy agent without racing its input.
Underneath, it is the multiplexer you already live in all day: persistent Workspaces with Projects, Tabs, and Panes, splits, zoom, copy-mode, and a save and restore that brings back layout, working directories, and scrollback after a detach, a crash, or a reboot.

Uniterm updates only changed terminal cells and avoids periodic idle work.
Use it locally or over SSH.

Its core design:

- **One binary, nothing bolted on.**
  No webview, no embedded terminal engine, no libghostty, no plugin runtime.
  Nothing third-party will ever run inside your multiplexer, so it cannot be slowed down, broken, or compromised by something you did not install.
- **It owns the grid.**
  Every byte an agent prints goes through Uniterm's own terminal model, so knowing whether an agent is working, idle, or asking for permission is a read of state it already holds, never a scrape of a pane on a timer.
- **The kernel does the watching.**
  Process exits arrive as kernel events instead of polling, and unchanged terminal cells require no redraw.
- **Persistence that tmux plugins cannot match.**
  Layout and scrollback are snapshotted atomically and continuously, and a clean stop keeps the run history and audit trail, because a multiplexer for agents has to survive the agents.
- **Any agent, no lock-in.**
  Claude Code, Codex, Cursor, Gemini, and OpenCode work out of the box, and a new agent is one provider module.
- **The fleet can run itself.**
  Everything you can do at the keyboard, an agent inside a Pane can do with `ut`: start and stop other agents, read their screens, send them text, queue direction, answer the waiting queue, and organise Tabs and Projects.
  A harness-neutral skill teaches any agent how, so one lead agent can dispatch and supervise the rest while you watch the rail.

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
- **A daily activity timeline.**
  Workspace > Today shows Projects, worktrees, agents, and session IDs on a live, navigable timeline backed by durable history.
  Add notes, inspect earlier days, resume a recorded session, or start a manager with read-only access by default.
- **Optional encrypted local history.**
  Protect Workspace state with an external key; prompt retention requires protected storage and explicit opt-in for each prompt.
- **Agent status you can trust.**
  Detection reconciles cooperative signals, provider logs, anchored screen rules, foreground process identity, and kernel exit events; working is a positive match, never output volume, and `ut agent explain` shows the evidence.
- **Orchestration with a completion contract.**
  Workflows and relays advance when a role submits a token, verifiers alone pass verdicts, iterations are capped, and every run has a durable identity you can inspect.
- **An Observatory beside the terminal.**
  Agents and detected web servers across the Workspace by default, plus a sandboxed file manager, in a rail that never covers your content.
- **Notifications where you are.**
  A toast, a host-terminal or native notification, and a bell, a synthesized chime, or your own sound, played on the machine you are sitting at even when the Workspace is remote.
- **Drivable by agents.**
  Reorder Tabs by dragging, keyboard, menu, or background automation.
  A harness-neutral skill teaches any AI agent to run the fleet through `ut`; see below.

The full list, with the test behind each claim, is in [docs/FEATURES.md](docs/FEATURES.md) and [docs/STATUS.md](docs/STATUS.md).

## How Uniterm compares

Measured on 2026-11-12: Uniterm development commit `4274e426e`, Herdr stable 0.9.5 and tmux 3.8 on the same Linux host.
Three unchanged marketing runs used five-minute idle windows and identical PTY workloads; these are medians of run medians.

| Metric | Uniterm dev | Herdr 0.9.5 | tmux 3.8 |
| --- | ---: | ---: | ---: |
| Startup ready | 4.576 ms | 22.402 ms | 6.329 ms |
| Control command | 0.382 ms | 0.823 ms | 0.530 ms |
| Detached idle CPU (one core) | 0.000% | 0.163% | 0.000% |
| Detached cohort RSS | 14.56 MiB | 30.03 MiB | 9.02 MiB |
| Attached idle CPU (one core) | 0.000% | 0.530% | 0.003% |
| Attached cohort RSS | 22.39 MiB | 51.10 MiB | 13.21 MiB |
| Input to visible output | 2.109 ms | 2.445 ms | 2.176 ms |
| 50,000-line burst | 163.254 ms | 173.232 ms | 225.691 ms |

On this same host, startup changed from 54.359 to 4.576 ms and 50,000-line bursts from 207.643 to 163.254 ms compared with Uniterm 1.2.20.
The 50,000-line burst used 5.8% less elapsed time than Herdr on this host.
Render output measured 87.06 KiB versus 59.68 KiB before and 28.42 KiB for Herdr; tmux used less memory.
Attached cohort RSS rose from 20.86 to 22.39 MiB; three-client RSS rose from 33.08 to 36.49 MiB.
Input values near 2 ms reach the observer's polling floor; measured zero CPU does not prove zero work.
[All 36 metrics, run ranges, source and binary hashes, reproduction, limitations and earlier results](docs/BENCHMARKS.md).

## Driving Uniterm from an AI agent

[`skills/manage-uniterm/SKILL.md`](skills/manage-uniterm/SKILL.md) is a harness-neutral skill (the plain `SKILL.md` format read by Claude Code, Codex, OpenCode, Cursor, and similar tools) that teaches an agent to find and monitor other agents across Panes, Tabs, and Projects, read and send Pane text, focus and organise the hierarchy, queue direction, and handle the waiting queue, all through `ut`.
Point your harness's skill discovery at the `skills/` directory, or copy the folder into its skills location; `ut --skill` prints a shorter version for agents that cannot load skills.

## Notes

- **Your data stays local.**
  Snapshots and event logs live under `~/.local/state/uniterm/` with owner-only permissions and are never sent anywhere; they can contain terminal output, so treat the directory as sensitive.
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
