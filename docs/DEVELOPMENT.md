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

## Shared UI components

Native UI changes extend shared components before adding host-specific painting or hit testing.
The renderer and pointer handler must consume the same rectangles and semantic rows.

The [AI UI component guide](UI_COMPONENTS.md) is the canonical inventory of components, source locations and reuse contracts.
Read it before changing UI and update it when a component contract changes.

### Shared UI checks

`RightPanelLayout::content` excludes the three header rows and the host's reserved footer rows; Files adds its optional Git rows inside this range.
`RightPanelChrome::frame` preserves live terminal content on chrome-only updates; it must never clear or redraw the embedded PTY.
Overwatch uses that same content rectangle for visible rendering, cursor, mouse and copy, and derives hidden launch/resize dimensions from the same layout.
`right_panel_layout_bounds_headers_content_and_footer_at_every_size` checks clipped geometry; `right_panel_chrome_matches_agents_and_preserves_live_content_and_settings_targets` compares actual rendered cells and SGR styles across top/bottom bars, narrow widths and focus, then exercises first attach and Settings pointer bounds.
`hidden_overwatch_launch_uses_content_size_before_exec` checks the child's initial `stty size` and unchanged resize damage.
`export_overwatch_native_terminal_fixtures` writes full 140x30 and 100x24 Overwatch and Agents ANSI, text and styled cell captures when `UNITERM_OVERWATCH_FIXTURE_DIR` is set.
Project Tab alignment regressions live in `uniterm-server/tests/tab_label_alignment.rs`, covering Settings saves, first attach, unchanged outside cells and styles, fixed click and plus targets, Unicode clipping, and zero render bytes for a repeated choice.
`config::tests::tab_label_alignment_defaults_roundtrips_and_validates`, `chrome::tests::project_tab_labels_align_sanitized_graphemes_and_clipped_ellipsis`, and the client Settings choice test cover parsing, fitting and keyboard/pointer agreement.
The fitting test also reports a bounded 10,000-label measurement; production fitting runs only on damaged chrome.

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

See [`AGENTS.md`](../AGENTS.md) (`CLAUDE.md` forwards tools to it) for the architectural invariants and contribution guide.

### Selector label reveals

Native view selectors share padded, grapheme-safe label fitting in `chrome::tab_strip_label`.
The full finite animation state is reusable as `chrome::TabLabelReveal`, with `new`, `deadline`, `advance`, `offset`, `text` and `finish` APIs.
It owns delays, grapheme windows, the final pause and permanent disarming; a host owns target identity, attachment scope, visibility, input cancellation and render delivery.
The right-panel host in `server::sidebar_labels` keeps the effect attachment-local and sends only the affected label rectangle through the existing render queue.
`sidebar_label_reveal_is_attachment_local_padded_finite_and_cancellable` covers exact cells, cursor preservation, another attachment, normal chrome repaint, no-op hover, completion, focus loss, overlays, resize and hidden geometry.

Sidebar drag resizing lives in `server::sidebar_resize` and shares the painted `chrome::sidebar_divider` rectangles and Settings width application path.
`sidebar_resize_captures_owner_crosses_content_saves_once_and_is_idle_when_unchanged` exercises two attachments, cross-panel movement, bounds and exactly one config save, and times 10,000 unchanged drags with zero output or requests.
`sidebar_resize_cancellation_and_capped_noop_preserve_preferences` covers focus, overlay, detach, input, peer viewport changes and capped width no-ops.
`sidebar_resize_shared_borders_keep_narrow_center_today_and_overwatch_usable` checks actual ANSI-decoded border cells, narrow/wide layouts, top/bottom bars, Today and detached Overwatch terminal resizing.
These paths run only on pointer or geometry events and arm no idle timer.
`chrome::tests::reveal_has_slow_bounded_steps_and_never_rearms_after_finishing` checks the reusable lifecycle, Unicode labels, explicit cancellation and terminal state without sleeping.
`label_tick_preserves_renderer_cache_and_next_pty_damage_is_one_byte` proves that a label tick preserves exact cursor/SGR caches: the next one-cell PTY update emits one byte, then an unchanged update emits zero.
