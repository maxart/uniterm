# Uniterm

Uniterm is a terminal multiplexer built for agentic engineering, written in Rust.
It is one static binary that is a complete tmux-class multiplexer and, in the same process, a supervisor for a fleet of AI coding agents.

It is fast and it is small.
On the measured Linux host, Uniterm v1.0.3 was ready in 8.7 ms, showed a keystroke in 2.08 ms, and used 15.6 MiB with one attached shell and 82.6 MiB with sixteen Panes.
Attached idle CPU measured 0.000 percent of one core over five-minute windows; a zero reading is limited by measurement resolution.
The native release binary measured 5.64 MiB.
Those are [measurements of one host and workload](#how-uniterm-compares); the renderer emits only changed cells and process liveness comes from the kernel rather than polling.

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

The latest comparison measures Uniterm **v1.0.3** (`2a6da6bf370d`) against Herdr **stable v0.8.2** (`9eb521456ac0`).
An open harness drove both release binaries through identical PTY workloads on one Linux x86_64 host on 2026-09-05.
Values below are medians of three rotated marketing-run medians; differences within one percent are ties.

| Metric | Uniterm | Herdr | Result |
| --- | ---: | ---: | --- |
| Server start to ready | 8.67 ms | 50.29 ms | Uniterm |
| Control command round trip | 1.24 ms | 3.22 ms | Uniterm |
| Detached idle CPU (server and one shell) | 0.000 % core | 0.293 % core | Uniterm |
| Detached idle memory (server and one shell) | 11.1 MiB | 24.3 MiB | Uniterm |
| Attached idle CPU (server, client, shell) | 0.000 % core | 0.477 % core | Uniterm |
| Attached idle memory (server, client, shell) | 15.6 MiB | 37.6 MiB | Uniterm |
| Keystroke to visible | 2.08 ms | 8.35 ms | Uniterm |
| 50,000-line output burst to visible | 400.1 ms | 386.8 ms | Herdr |
| Bytes written to the outer terminal per burst | 55.5 KiB | 18.8 KiB | Herdr |
| Idle memory with 16 panes attached | 82.6 MiB | 108.0 MiB | Uniterm |
| Idle memory with 3 clients attached | 24.4 MiB | 62.8 MiB | Uniterm |
| Resize storm settle (40 resizes after output bursts) | 22.7 ms | 267.3 ms | Uniterm |
| Binary size | 5.64 MiB | 21.99 MiB | context only |

Across the eight core metrics, Uniterm leads 7, Herdr leads 1, and 0 are ties on these aggregated medians.
Inspect each row: the balanced index is a geometric mean of ratios, not a count of wins.
Binary size and the extended scenarios are context, separate from the balanced index.
Herdr's background network checks were disabled during timing; feature breadth and assurance are not folded into performance.
A zero CPU reading does not establish zero CPU use.
These measurements apply to the named Linux host and workload, not every platform or workflow.

[Full setup, results, and downloadable raw evidence](docs/BENCHMARKS.md).
[Historical Uniterm v1.0.0 comparison](docs/BENCHMARKS-v1.0.0.md).
Both historical binaries came from development commits after their release tags, while reporting 1.0.0 and 0.8.2; this is not a controlled Uniterm-only version comparison.

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
