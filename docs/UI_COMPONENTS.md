# UI components: instructions for agents

Read this guide before changing any Uniterm UI, alongside [AGENTS.md](../AGENTS.md).
Choose an existing component before editing a view.
Extend that component when a new host needs another capability, and update all affected hosts and their tests in the same change.
Do not copy a view's painting code and adjust its colors, spacing, or labels to create another implementation of the same component.
Keep this inventory current whenever a component or its reuse contract changes.
The code owns the implementation; [STATUS.md](STATUS.md) owns capability status and validation results.

## Choose the right host

| Surface being changed | Owner | Required approach |
|---|---|---|
| Pane content, main tabs, sidebars, native context menus, Today and Graph | `uniterm-server` | Synchronous native cells/ANSI, shared geometry, damage-driven output |
| Settings, command palettes, client menus and floating dialogs | `uniterm-client` | Existing modal, palette, menu and Settings components |
| Menu definitions, settings values and domain state | `uniterm-core` | Pure data and logic; no renderer, toolkit, async runtime or I/O |
| Settings, commands and snapshots crossing client/server or runtime boundaries | `uniterm-proto` | Existing typed vocabulary and compatibility discipline |

Do not introduce ratatui into server-rendered surfaces.
Client UI abstractions stay in the client; a server view must not import a client widget to get its appearance.
Use shared semantic theme values and the appropriate native rendering component instead.

## Component inventory

Paths below are repository-relative.
The two files named `chrome.rs` have different responsibilities: `uniterm-server/src/chrome.rs` owns reusable geometry and small rendering primitives; `uniterm-server/src/server/chrome.rs` connects them to Workspace state and shared panel painting.

| Need | Component and source | Reuse contract |
|---|---|---|
| Main Project Tab bar, overflow and plus button | `TabBarLayout`, `tab_bar_layout` in [server chrome primitives](../crates/uniterm-server/src/chrome.rs); `Server::tab_bar_layout`, `draw_status` in [Workspace chrome](../crates/uniterm-server/src/server/chrome.rs) | Paint and click the same `TabSlot` rectangles; preserve scroll controls, stable Tab targets and the plus slot |
| Root and stack/sub-tab label alignment | `project_tab_label` in [server chrome primitives](../crates/uniterm-server/src/chrome.rs); [stack tabs](../crates/uniterm-server/src/server/tab_stacks.rs) | Both use `Config::tab_label_alignment`; Center is the default; fitting changes text placement inside the existing rectangle |
| View selectors: Local/Servers, right-panel tabs, Today, Agents/Runs | `tab_strip_slots`, `tab_strip_cell`, `tab_strip_style`, `tab_strip_label`, `render_tab_strip` in [server chrome primitives](../crates/uniterm-server/src/chrome.rs) | One geometry and style vocabulary; ANSI hosts use the renderer, Grid hosts use its cell styles and label fitting; selectors keep centered labels independently of Project Tab alignment |
| Finite marquee for a clipped selector label | `TabLabelReveal` in [server chrome primitives](../crates/uniterm-server/src/chrome.rs); attachment host in [sidebar labels](../crates/uniterm-server/src/server/sidebar_labels.rs) | Reuse `new`, `deadline`, `advance`, `offset`, `text` and `finish`; hosts supply identity, unchanged rectangles, attention, cancellation and damage delivery, never a second timer policy |
| Right-panel frame, heading and content bounds | `RightPanelLayout` in [server chrome primitives](../crates/uniterm-server/src/chrome.rs); `RightPanelChrome` and `Server::right_panel_layout` in [Workspace chrome](../crates/uniterm-server/src/server/chrome.rs) | Agents, Files, Web and Overwatch share blank/header/blank spacing, heading style and vertical border; footer height determines the content rectangle |
| Right-panel footer divider and buttons | `RightPanelLayout::footer_buttons`, `RightPanelChrome::footer_divider`, `footer_button` | Reuse footer rectangles for both rendering and clicks; use the existing centered button fitting, foreground, background and bold attributes |
| Resizable Project and right-panel edges | `sidebar_divider`, `RightPanelLayout::divider` in [server chrome primitives](../crates/uniterm-server/src/chrome.rs); [sidebar resize capture](../crates/uniterm-server/src/server/sidebar_resize.rs) | Paint and hit-test the same one-cell edge; capture the initiating attachment across content boundaries, apply Settings width semantics, and save once on release or cancellation |
| Project cards and fractional vertical padding | `project_card_slots`, `CardSlot`, `card_at` in [server chrome primitives](../crates/uniterm-server/src/chrome.rs); `draw_sidebar` in [Workspace chrome](../crates/uniterm-server/src/server/chrome.rs) | Item padding participates in scrolling and hits, including the final item; preserve half-block glyphs and effective background fills |
| Agent and Web list cards | `card_slots`, `CardSlot`, `card_at`; `observatory_agent_slots`, `observatory_web_slots` | Layout from the shared panel content bounds; never recalculate item row numbers in mouse handlers |
| Scrolling-layout indicator | `ScrollbarLayout` in [server chrome primitives](../crates/uniterm-server/src/chrome.rs) | Rendering, dragging and content reservation use the same track/thumb rectangles |
| Activity, Flow and Graph content | `row_text`, `put`, `panel`, `detail_lines`, `apply_grid` in [Today](../crates/uniterm-server/src/server/today.rs) | Shared sanitized, display-cell-aware composition and grid comparison; unchanged content produces no damage |
| Compact Graph rows and selected-item dock | `regions`, `TraceRow`, `trace_row`, `Inspector`, `inspector` in [Graph presentation](../crates/uniterm-server/src/server/today/graph/presentation.rs) | Agents and Runs share density, ancestry rails, state markers, clipping, selected-path styling and fixed inspector anchors |
| Workspace-owned terminal in a panel | `overwatch_rect`, `visible_pane_rect`, `input_pane`, `resize_overwatch` in [Overwatch](../crates/uniterm-server/src/server/overwatch.rs) | Use the shared panel content rectangle for PTY size, output, cursor, copy and mouse; the service Pane has no fabricated Project or Tab |
| Command dropdowns and their sections | `Menu`, `MenuItem`, `MenuAction`, `Menu::rows` in [core menus](../crates/uniterm-core/src/menu.rs); `MenuState` in [client menus](../crates/uniterm-client/src/menu.rs) | Sections are identified by semantic actions; height, separators, scrolling, keyboard navigation and hits consume the same row model |
| Server-owned context menus | [context menu renderer](../crates/uniterm-server/src/context_menu.rs), [agent menu integration](../crates/uniterm-server/src/server/agent_menu.rs) | Extend the existing menu model and semantic dispatch; keep anchored geometry and invocation-safe targets |
| Searchable action or entity picker | `Palette`, `MenuState::custom` in [palettes](../crates/uniterm-client/src/palette.rs) and [menus](../crates/uniterm-client/src/menu.rs) | Reuse filtering, selection, scrolling, empty states and keyboard/pointer behavior |
| Floating dialog and two-column list modal | `Overlay`, `OverlayRowStyle`, `modal_rect`, `render_list_modal`, `render_tabbed_list_modal`, `modal_hit` in [overlays](../crates/uniterm-client/src/overlay.rs) | Use shared borders, padding, shadows, semantic rows, list viewport and hit geometry |
| Search field, modal list and command footer | `search_composer`, `nav_list`, `footer_text`, `footer_spans` in [overlays](../crates/uniterm-client/src/overlay.rs) | Keep key hints, click spans and visible text consistent; do not embed custom ANSI in domain labels |
| Persistent user preference | `SettingsView`, `Field`, `Kind`, `SECTIONS` in [Settings](../crates/uniterm-client/src/settings.rs); [Config](../crates/uniterm-core/src/config.rs); `SettingsPatch` / `SettingsSnapshot` in [protocol](../crates/uniterm-proto/src/lib.rs); `Server::apply_settings` | Use the existing choice/toggle/input control and authoritative save/reply path; synchronize initial attach and live replies |
| Theme and semantic colors | `Theme` in [configuration](../crates/uniterm-core/src/config.rs), `ui_theme` / `OverlayRowStyle` in [overlays](../crates/uniterm-client/src/overlay.rs) | Select semantic foreground, muted, divider, surface, accent, success, warning and error roles; add a reusable semantic role if needed |

## Right-panel rules

Use `Server::right_panel_layout(footer_rows)` to derive the outer panel, content and footer rectangles.
Agents and Overwatch use a two-row footer; Files reserves its existing three-row status/footer region; Web does not reserve a footer.
Use `RightPanelChrome::frame`, `heading`, `row`, `footer_divider` and `footer_button` to paint those parts.
Header controls use `heading_row()` rather than a second copy of the heading offset.
Footer actions use `footer_buttons()` rather than a separate last-row click rule.
The Overwatch tab appears only while Overwatch is ON, including startup, recovery and attention states.
Turning it OFF removes the tab and returns an active Overwatch panel to Agents; Files and Web selections stay selected.
Derive selector labels, geometry, selected position and clicks from the same visible-tab list; hidden state updates must not repaint another panel.

An embedded terminal owns its content cells.
On a chrome-only update, preserve those cells and paint only the frame, heading and footer.
Do not erase the terminal and replay its whole contents merely to repaint a heading or button.
Size hidden terminal services from the same content geometry before launch so their first output is not lost to a later shrink.
Unchanged dimensions must not send another PTY resize.

Both sidebar borders are mouse drag targets over their full visible height, excluding the main tab bar.
`sidebar_divider` supplies the trailing Project edge and leading right-panel edge to both painters and capture.
Route captured movement before sidebar, Today, Overwatch, scrolling and central-pane mouse handling so crossing a content boundary cannot become application input.
Only the initiating attachment may move or release the capture; a shared viewport change cancels it regardless of attachment.
Focus loss, overlays, detach, input and semantic commands also end capture, keeping its last visible width.
Use `apply_sidebar_width_settings` for Settings updates and live drag previews; save only once at gesture completion.
An unchanged effective width produces no repaint, PTY resize, settings message or save, including a plain click on a responsive, capped width.
Unavailable-Pane diagnostics use the native `detail_lines` fitting through `Pane::resize_terminal`, so resizing keeps the error and final retry hint visible without changing the retained original output/profile.
An unchanged size must leave that diagnostic grid undamaged too.
Keep configured widths within 16..40 cells for Projects and 22..52 for the right panel, while reserving 40 central cells when both panels are visible or 48 without the right panel.
Responsive fitting must preserve the saved preference until the user moves the border to a different visible width.
Render padding and borders outside the terminal rectangle so terminal scrolling, cursor positioning, copying and mouse reporting cannot consume chrome cells.

Do not create a Project Tab for a Workspace service to obtain a terminal renderer.
Reuse the existing detached terminal integration and retain explicit Workspace ownership in queries and persistence.

## Tab rules

Project Tabs represent terminals and stacks; selector tabs choose views.
Keep those meanings and their settings separate.
`tab-label-alignment` applies to root and stack Project Tabs only, with Left, Center and Right choices and Center as the default.
It must not change Local/Servers, Observatory, Today, Graph mode selectors, the Workspace button or footer buttons.
It must not move a Tab's hit rectangle, plus button or overflow controls.

For a new selector host, extend the shared strip's geometry or styles if necessary and exercise every existing host that uses the changed behavior.
Do not create bracketed text links, custom tab colors or independent centering arithmetic in the host.
Use the tab list's single source of truth for labels, count, ordering and pointer targets.
Selector labels reserve one display cell of padding on each side when their slot is at least three cells wide.
Truncation uses a single ellipsis inside that padded interior and never splits a grapheme.
`chrome::TabLabelReveal` owns the reusable marquee lifecycle and label fitting, independently of any panel or tab identity.
`new(label, width, now)` returns no animation for a fitting label; `deadline()` is the sole wakeup source; `advance(now)` reports a changed window; `text()` gives the padded display text; `offset()` distinguishes a reveal window from the static label; `finish()` cancels and restores the static label.
The right-panel selector uses this primitive through `server::sidebar_labels` to reveal clipped labels on hover or selection.
Each attachment gets one delayed, slow pass, a final pause, then the ordinary clipped label; movement inside the same slot does not restart it.
Only that label is repainted, using the same slot, style and fitting helpers, and normal frames preserve its current window.
Leaving, losing terminal focus, opening an overlay, detaching, resizing or hiding the selector cancels its deadline.
No deadline exists after the finite reveal completes or when the label fits.
Another selector host must retain its own target identity and attachment scope, reject stale labels or geometry, and remove the state when attention or visibility ends.
Keep a completed instance until attention leaves the target so movement within the same slot cannot rearm it.
Include its current text in ordinary repaint frames and send changed label cells through the existing supersedable render queue.
Label-only ANSI output saves and restores cursor and rendition; it must not invalidate the Pane renderer caches.
Hover reporting is independent of the focus-follows-mouse setting; the server retains that setting as the authority for changing Pane focus.

## Menu and Settings rules

Declare menu section starts by `MenuAction`, never by an item index or a count of preceding features.
Workspace About has its own section.
Detach and Close share the final session section.
Adding another feature must preserve these groups without adjusting separator offsets.
Long menus must remain reachable through the shared viewport and scrolling logic.

A new preference needs a default, validated input, Config parsing/serialization, a typed Settings patch/snapshot, the standard control, and a saved authoritative reply.
Choose the existing `Kind::Choice`, toggle, text or number behavior before designing a new interaction.
Reuse the shared Settings save path and update other attachments through existing messages.
For a label-only change, repaint affected chrome without resizing terminal content.
An unchanged value must not trigger new visible work.
Auto-trust Project directories uses the shared explicit on/off hit regions (`approval_choice`) at its standard Settings control row; clicking the selected value is a no-op.
Keep incompatible wire changes and existing enum discriminants consistent with [AGENTS.md](../AGENTS.md).

## Text, geometry and meaning

Server/Grid `uniterm_core::Rect` coordinates are zero-based; client `overlay::Rect` coordinates are one-based.
Follow the existing mouse-coordinate conversion boundary instead of applying another offset in a widget.
Measure terminal text in display cells and preserve whole grapheme clusters.
Use the existing fitting/sanitization boundary for the host, including titles, paths, agent labels and externally supplied text.
Do not use byte length or character count as a substitute for display width.

Use terminal-default foreground/background for unfilled native regions.
Preserve explicit semantic fills in the effective background channel, including half-block padding on transparent terminals.
Do not recolor PTY output to express selection or focus; those states belong to chrome and cursor handling.

Styling must preserve the meaning of data.
An agent's idle state is not a successful outcome; an artifact's presence is not mechanical verification; a child relationship is not a task dependency.
Keep unknown, historical, missing-parent and truncated-history states visible.
Only render a folding control when it actually folds content.
Graph status and artifact information keep their reserved space at narrow widths; optional labels and context shorten first.
The selected-item dock stays fixed while the trace scrolls, and full details remain available through the existing inspector action.

Clicks, key bindings, menus, CLI actions and automation must converge on the existing semantic command path.
Do not implement a second mutation directly in a renderer or mouse handler.
Use stable identities and the owning indexes when resolving targets.

## Change and review procedure

1. Identify the host and list the existing components the change will use.
2. Read their implementations and an existing consumer, including geometry and input routing.
3. Extend a shared component when required, then migrate consumers together.
4. Test actual rendered cells and actions, including at least one existing sibling view when shared chrome changes.
5. Inspect a real Grid or terminal export at a narrow and a wide width; a hand-drawn mockup alone does not validate the implementation.
6. Update this guide when the component contract changes, and update [STATUS.md](STATUS.md) with entry points and regression tests.
7. Run the required workspace gates and the performance checks appropriate to the change.

No permission step is implied by this procedure.
Follow the user's existing authorization and complete the work.
If a genuinely different UI shape requires a new primitive, keep it in the appropriate shared component module, document why the existing primitive does not fit, and add it to this inventory.

## Regression checks

| Change | Required evidence |
|---|---|
| Shared panel or tab styling | Exact row positions, glyphs, foreground/background and attributes compared with an existing sibling host |
| Geometry, padding or scrolling | Painting and pointer actions agree at item edges, padding, final visible items, overflow controls and viewport boundaries |
| Root or stack labels | All three alignments, unchanged rectangles, clipped Unicode and combining/ZWJ sequences, plus buttons and second attach |
| Settings or menus | Keyboard and pointer behavior, persistence/authoritative replies, invalid values, long-menu reachability and semantic sections |
| Embedded terminal | First output at the correct size, unchanged-size no-op, header/footer exclusion, native mouse/cursor/copy behavior, content preserved through chrome repaint |
| Cached or event-driven view | First attach, hidden/reopened view, no-op update and zero render bytes when nothing visible changes |
| Graph | Dense and small fixtures, continuous/clipped rails, selected-path styles, dock anchors, honest status/evidence, safe focus and unchanged damage |

Use narrow and wide layouts, top and bottom status-bar positions where applicable, and a viewport where a sidebar becomes hidden.
Test state and subprocess environments must be isolated as required by AGENTS.md; never use a real Workspace as a fixture.
Useful existing starting points include [Observatory chrome tests](../crates/uniterm-server/tests/observatory_chrome.rs), [shared navigation tests](../crates/uniterm-server/tests/machines_navigation.rs), [Tab alignment tests](../crates/uniterm-server/tests/tab_label_alignment.rs), [stack tests](../crates/uniterm-server/tests/tab_stacks.rs), [Overwatch tests](../crates/uniterm-server/src/server/overwatch.rs), [Graph tests](../crates/uniterm-server/src/server/today/graph.rs), [menu tests](../crates/uniterm-core/src/menu.rs) and the [real-terminal Settings test](../crates/uniterm-cli/tests/theme_picker.rs).

The required completion gates remain:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
git diff --check
```

Also run `cargo test --workspace --release` when touching the server or CLI.
Measure new hot-path work and prove newly affected idle surfaces remain quiet; see [performance discipline](../AGENTS.md#performance-discipline) and [development checks](DEVELOPMENT.md).

## Native model and effort pickers

Overwatch model settings use `MenuState` with `Palette::custom_with_details`, sharing filtering, row previews, scrolling and pointer rectangles.
`custom_with_details` accepts a title, a source/capability notice and rows containing display text, searchable aliases, preview text and a semantic action.
Use `PalettePurpose` to identify the host explicitly rather than matching visible title text.
The picker owns its pending model/effort selection and applies one semantic `Selection` operation only after the final choice.
Explicit palette identity distinguishes settings from model and effort stages; live status broadcasts must not replace a pending picker.
Correlate metadata replies with the active request and harness, and ignore replies after cancellation or a harness change.
Render friendly labels alongside canonical identifiers; pass only identifiers to the server.
Show native supported effort choices and harness default separately, with unavailable/unsupported explanations and an explicit advanced custom fallback.
Never add per-frame discovery, a polling timer, or a second keyboard/mouse geometry implementation.

## Grouped searchable controls

`Palette::custom_grouped` assigns a semantic section to each action before filtering.
Overwatch groups lifecycle actions, current provider/model/effort/recovery/access settings, and alternative providers in that order.
`visible_rows` supplies a cached mapping of physical action and divider rows to rendering, scrolling and pointer input; keyboard selection visits only actions.
The row mapping is rebuilt only when filtering or catalog refresh changes the list, keeping scrolling and pointer lookup independent of catalog size.
Clicks on dividers and interior blank rows leave the menu open; outside clicks close it.
Filtering preserves group identity and adds one divider between surviving groups, with no leading or trailing divider.
`OverlayRowStyle::Separator` paints a full-width theme divider without embedding terminal escapes in labels.
Live refresh preserves the query and selected action, including the Access action while ON.
