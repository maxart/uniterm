# Omarchy theme following

Implemented for v1.2.24, opt-in and scoped to the Workspace host.
The setting is `follow-omarchy-theme = true`, also available below the preview in Settings > Theme on an eligible host.
The default is off.

## Platform and persistence contract

Only Linux builds contain the native watcher.
Eligibility requires the exact `ID=omarchy` identity in `/etc/os-release`; ordinary Arch, other Linux distributions, macOS and Android cannot activate it.
Native Windows has no watcher implementation.
Copied settings or theme directories alone cannot enable the feature on another distribution.
A Workspace requests capability once at startup; Linux then performs a single bounded eligibility read.
Bare runtimes perform no Omarchy discovery or unsolicited capability publication.
Ineligible hosts create no theme watcher or recurring work.

The server owns the effective palette, so every client attached to an opted-in Omarchy Workspace inherits it, including remote clients on other operating systems.
This is intentional; Workspaces hosted on other systems retain their existing behavior.
Wire protocol 36 carries the full semantic palette, including the initial Settings snapshot and subsequent `ThemeChanged` messages.
Clients and servers must support the same wire protocol.
Existing Workspace processes keep their previous executable and protocol until restarted; upgrade clients and Workspace hosts together.

The configured manual theme and explicit status-color overrides remain the durable source of preferences.
Observed colors never replace the manual theme in the saved config.
Manual theme selection turns following off, and disabling following restores the manual palette.
Config reloads retain the observed palette while following remains enabled.
Invalid combined Settings patches change nothing.

## Native observation

Omarchy stages its palette and replaces `~/.local/state/omarchy/current/theme`, then writes `theme.name`.
These behaviors were checked against the installed `/usr/share/omarchy/bin/omarchy-theme-set` and the upstream [activation flow](https://github.com/omacom/omarchy/blob/quattro/docs/theming.md#theme-activation-flow).
The legacy `~/.config/omarchy/current` location is selected only when the newer location is absent and the legacy directory exists.
No desktop configuration, hooks or scripts are modified or executed.

`runtime::omarchy::Service` uses the existing notify dependency's Linux `INotifyWatcher`, with no polling fallback.
It watches the stable `current` directory and the active theme directory nonrecursively, registers before reading, and rearms the replaced directory.
A bounded event queue coalesces bursts, and a 75 ms debounce is armed only by a real relevant filesystem event.
Fatal watcher state survives queue coalescing.
One per-service permit stays with the native watcher through blocking reads, so canceled generations cannot accumulate overlapping reads or watchers on stalled storage.
Access notifications and unrelated background changes are ignored.
Reads run on the agent runtime's blocking pool, never the mio core loop.
A read accepts at most 64 KiB from a regular `colors.toml` file and rejects FIFOs, devices and file symlinks.
The pure parser converts supported hex color roles using the same mapping as the bundled Omarchy catalog.

Directory identity and event epochs reject observations taken across a replacement.
Watch generations reject late messages after disable/re-enable.
Disabling also clears the cached observation, so a later invalid source and config reload cannot revive an old palette.
Unchanged parsed palettes produce no core message or repaint.
Missing, oversized or malformed palettes retain the last valid colors and surface a Settings error; a later relevant filesystem event can recover.
Watcher setup or backend failure is reported without periodic retries; turn following off and on to retry registration.

## Rendering and performance

`Server::apply_effective_theme` updates terminal defaults and changed native chrome, then sends the full palette to attached clients.
It does not relayout, resize PTYs, save config, refresh file lists or schedule idle redraws.
Detached Workspaces retain the palette without drawing.
Duplicate effective palettes return before touching terminals or client output.
Terminal applications requesting color-change notifications still receive their requested terminal response.
An actual color change necessarily has bounded parsing and rendering cost; the performance contract is no recurring idle work and no material input-latency regression.

Client overlays repaint through the existing overlay compositor without rebuilding dialog state or issuing Refresh requests.
Open text edits, selection and cursor position survive a palette update.
With no overlay open, a palette message itself emits no client terminal bytes.

## Regression and qualification coverage

- `config::omarchy_live::tests`: bounded parsing, role mapping, explicit overrides, config round trips and bundled catalog equivalence.
- `runtime::omarchy::tests`: native initial publication, no-op/access filtering, directory replacement, malformed/oversized/FIFO recovery, missing-theme startup, disable/re-enable and watcher teardown.
- `server::theme_follow::tests::omarchy_updates_are_generation_scoped_idle_safe_and_preserve_manual_config`: stale messages, first attach, manual fallback, config reload, atomic validation and silent duplicates.
- `uniterm-cli/tests/theme_picker.rs`: real PTY first-attach palette, live overlay synchronization, preserved edits, duplicate silence and hidden controls on ineligible hosts.
- `omarchy_following_five_minute_idle_budget` (explicit ignored test): simultaneous isolated off/on processes, CPU, core wakeups/messages, resident memory, threads and file descriptors over 300 seconds.
- `omarchy_24_pane_input_and_theme_switch_latency` (explicit ignored test): manual versus a changed palette on every input in a 24-Pane Workspace, with p95 below 50 ms and at most 5 ms additional latency.

The latency fixture measures input through rendered terminal cells, not physical display presentation.
The idle fixture isolates the watcher/runtime cost; it does not claim to benchmark every workload in a full Workspace.
## Local qualification results

Measured on Linux x86_64 / Omarchy 4.5.0, Rust release profile, on 2026-11-13.
The 24-Pane input-to-render fixture measured p95 of 0.783 ms with a manual palette and 0.610 ms while changing the palette on every input, both below the 50 ms budget.
This single-run difference does not establish a latency improvement.
The fixture collects 200 samples after 10 warmup inputs for each mode.
The five-minute isolated watcher/runtime comparison passed with zero core wakeups and zero palette messages in both processes.

| Measurement | Following off | Following on |
|---|---:|---:|
| Sample duration | 300.099 s | 300.100 s |
| Process CPU | 0.000044% | 0.000092% |
| CPU time over sample | 0.131 ms | 0.276 ms |
| Core wakeups / palette messages | 0 / 0 | 0 / 0 |
| Resident memory | 6,140 KiB | 7,068 KiB |
| Threads | 4 | 5 |
| File descriptors | 12 | 15 |

The observed resource increment in this run is 928 KiB resident memory, one sleeping native watcher thread and three descriptors.
Resident memory can vary between runs.
It is small but not literally zero overhead.
No polling, periodic palette reads or idle rendering was introduced.
Both idle CPU values are far below the 0.5% background budget.
The separate final release renderer spike also passed: the idle grid emitted zero bytes, and worst render p95 was 7.138 microseconds across its 100,000-frame benchmark.

Reproduce the explicit qualifications with:

```sh
cargo test -p uniterm-server --release --lib omarchy_following_five_minute_idle_budget -- --ignored --nocapture
cargo test -p uniterm-server --release --lib omarchy_24_pane_input_and_theme_switch_latency -- --ignored --nocapture
cargo run --release -p uniterm-server --example render_spike
```


