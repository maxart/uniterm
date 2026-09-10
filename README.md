# Uniterm

Uniterm is a terminal multiplexer built for agentic engineering, written in Rust.
It is one static binary that is a complete tmux-class multiplexer and, in the same process, a supervisor for a fleet of AI coding agents.

Run a dozen agents, keep every one of them in view, and stop wondering which is working, which is stuck, and which has been waiting on you for the last twenty minutes.
Uniterm puts that answer in a rail beside your terminals, queues the moments that need a human, and lets you hand direction to a busy agent without racing its input.
Underneath, it is the multiplexer you already live in all day: persistent Workspaces with Projects, Tabs, and Panes, splits, zoom, copy-mode, and a save and restore that brings back layout, working directories, and scrollback after a detach, a crash, or a reboot.

It is fast and it is small.
A Workspace is ready before you have finished reaching for the next key, and what you type lands on screen as fast as your terminal can draw it.
Detach for the night and Uniterm sits perfectly still, so your laptop stays cool and quiet while a whole fleet of Panes waits for you to come back.
It fits on the machine in front of you and on the one at the other end of an SSH connection, and it leaves the memory to your agents.

That is a result of what Uniterm refuses to be, not of tuning:

- **One binary, nothing bolted on.**
  No webview, no embedded terminal engine, no libghostty, no plugin runtime.
  Nothing third-party will ever run inside your multiplexer, so it cannot be slowed down, broken, or compromised by something you did not install.
- **It owns the grid.**
  Every byte an agent prints goes through Uniterm's own terminal model, so knowing whether an agent is working, idle, or asking for permission is a read of state it already holds, never a scrape of a pane on a timer.
- **The kernel does the watching.**
  Process exits arrive as kernel events instead of polling, and the renderer emits only the cells that changed, so an idle fleet draws nothing and costs nothing.
- **Persistence that tmux plugins cannot match.**
  Layout and scrollback are snapshotted atomically and continuously, and a clean stop keeps the run history and audit trail, because a multiplexer for agents has to survive the agents.
- **Any agent, no lock-in.**
  Claude Code, Codex, Cursor, Gemini, and OpenCode work out of the box, and a new agent is one provider module.
- **The fleet can run itself.**
  Everything you can do at the keyboard, an agent inside a Pane can do with `ut`: start and stop other agents, read their screens, send them text, queue direction, answer the waiting queue, and organise Tabs and Projects.
  A harness-neutral skill teaches any agent how, so one lead agent can dispatch and supervise the rest while you watch the rail.
- **Your data never leaves your machine.**
  No telemetry, no update checks, no accounts; the only network use is your own SSH connection.

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

The latest comparison measures Uniterm **v1.0.4** (`a961519c5ed9`) against Herdr **stable v0.8.2** (`9eb521456ac0`) and **tmux 3.7c** (`e476c1230b95`).
An open harness drove all three release binaries through identical PTY workloads on one Linux x86_64 host on 2026-09-05.
Values below are medians of three rotated marketing-run medians; differences within one percent are ties.

| Metric | Uniterm | Herdr | tmux | Result |
| --- | ---: | ---: | ---: | --- |
| Server start to ready | 8.23 ms | 44.85 ms | 15.49 ms | Uniterm |
| Control command round trip | 1.35 ms | 3.53 ms | 2.18 ms | Uniterm |
| Detached idle CPU (server and one shell) | 0.000 % core | 0.257 % core | 0.000 % core | tie (Uniterm, tmux) |
| Detached idle memory (server and one shell) | 11.2 MiB | 24.1 MiB | 10.6 MiB | tmux |
| Attached idle CPU (server, client, shell) | 0.000 % core | 0.427 % core | 0.003 % core | Uniterm |
| Attached idle memory (server, client, shell) | 15.8 MiB | 38.4 MiB | 17.0 MiB | Uniterm |
| Keystroke to visible | 2.08 ms | 8.31 ms | 2.09 ms | tie (Uniterm, tmux) |
| 50,000-line output burst to visible | 392.5 ms | 372.7 ms | 391.0 ms | Herdr |
| Bytes written to the outer terminal per burst | 55.5 KiB | 18.6 KiB | 2.24 MiB | Herdr |
| Idle memory with 16 panes attached | 82.5 MiB | 108.3 MiB | 81.5 MiB | tmux |
| Idle memory with 3 clients attached | 24.5 MiB | 65.5 MiB | 29.6 MiB | Uniterm |
| Resize storm settle (40 resizes after output bursts) | 22.0 ms | 269.9 ms | 207.0 ms | Uniterm |
| Binary size | 5.64 MiB | 23.40 MiB | 1.35 MiB | context only |

Across the eight core metrics, Uniterm leads 4, tmux leads 1, Herdr leads 1, and 2 are Uniterm/tmux ties on these aggregated medians.
On the balanced index, a geometric mean of ratios rather than a count of wins, Uniterm scored 98.7, tmux 85.8, and Herdr 36.1.
Binary size and the extended scenarios are context, separate from the balanced index.
Herdr's background network checks were disabled during timing; tmux ran with a private socket and configuration; feature breadth and assurance are not folded into performance.
A zero CPU reading does not establish zero CPU use.
These measurements apply to the named Linux host and workload, not every platform or workflow.

[Full setup, results, and downloadable raw evidence](docs/BENCHMARKS.md).
[Historical Uniterm v1.0.3 comparison](docs/BENCHMARKS-v1.0.3.md) and [historical Uniterm v1.0.0 comparison](docs/BENCHMARKS-v1.0.0.md), both against Herdr alone.
The v1.0.0 binaries came from development commits after their release tags; none of the three campaigns is a controlled Uniterm-only version comparison.

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
