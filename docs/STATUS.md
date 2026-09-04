# Uniterm implementation status

This is the single release-status record for every user-facing capability Uniterm claims.
It exists because design documents are allowed to be ambitious and release claims are not.

Status is determined by reading the code and the tests, never by reading the prose.

- **Shipped**: the end-to-end runtime path exists and at least one test exercises it.
- **Partial**: a useful path exists, but a claimed mechanism or its coverage is missing.
- **Missing**: no meaningful implementation path was found.
- **By design absent**: outside Uniterm's product boundary; it should not be added.

Entry point names a CLI command, a client surface, or a `crate::module::function`.
Evidence names a test function or test file, or `none`.

## The rule

A status update ships in the same change as the feature it describes, and it must name the feature's entry point and the test that exercises it.
A capability with no test is recorded here as `none` and cannot be described as shipped anywhere else in the documentation.

## Multiplexer and terminal

| Capability | Status | Entry point | Evidence | Notes |
|---|---|---|---|---|
| PTY-backed pane, VT parse into the grid | Shipped | `uniterm_server::terminal::Terminal::feed` | `parser_results_do_not_depend_on_pty_chunk_boundaries` (terminal.rs); 52 unit tests | PHASE1 M1 |
| Client-server attach, detach, reattach | Shipped | `uniterm serve` / `uniterm attach` | `client_receives_pane_output`, `server_survives_client_detach` (tests/m2_client_server.rs) | PHASE1 M2 |
| Splits, layout tree, directional focus, zoom | Shipped | `uniterm_core::layout::LayoutNode::compute` | `split_creates_a_divider` (tests/m3_layout.rs); 12 layout.rs unit tests | PHASE1 M3 |
| CLI front door (`new-session`, `ls`, `attach`, `kill`) | Shipped | `uniterm_cli::cmd_new_session`, `cmd_ls`, `cmd_kill` | `list_info_reflects_splits_and_windows_then_kill` (tests/m4_session.rs) | PHASE1 M4 |
| Ghostty-format config + status line | Shipped | `uniterm_core::config::Config::parse` | `status_line_shows_session_name` (tests/m5_status.rs); 20 config.rs unit tests | PHASE1 M5 |
| Copy-mode: scrollback, selection, incremental search | Shipped | `uniterm_server::copymode::CopyState::handle` | `copy_mode_indicator_and_osc52_yank` (tests/m6_copymode.rs); 9 copymode.rs unit tests | PHASE1 M6 |
| Pane resize by direction | Shipped | `uniterm_core::layout::LayoutNode::resize_pane` | `resize_adjusts_nearest_matching_split`, `resize_clamps_ratio` (layout.rs) | R1. Pure-layout coverage only; no socket-level resize regression |
| Chrome (status line, dividers) survives every pane operation | Partial | `uniterm_server::server::Server::draw_status`, `Server::draw_dividers` | `status_line_survives_pane_ops` (tests/r2_chrome.rs) | R2. One test; it stops reading at the first frame containing the session name, so the attach frame alone can satisfy it. No divider assertion, no resize, no heavy bottom-of-pane output |
| Alternate screen buffer | Shipped | `Terminal::enter_alt` / `Terminal::exit_alt` | `alt_screen_preserves_primary`, `resize_in_alt_screen_resizes_stashed_primary`, `shrink_in_alt_screen_clamps_restored_cursor` | A1 and BF1 |
| Unicode cell model (graphemes, combining, wide continuations) | Shipped | `uniterm_core::grid::Grid::set` + grapheme arena | `combining_and_emoji_sequences_live_in_the_arena`, `wide_grapheme_owns_a_continuation`, `cells_remain_compact_with_grapheme_and_hyperlink_handles` | DC2 |
| Logical lines and width reflow | Shipped | `uniterm_core::grid::Grid::resize_reflow` | `soft_wrapped_lines_reflow_and_cursor_follows_text`, `one_column_reflow_preserves_wide_text_for_later_growth`, `streaming_reflow_matches_the_reference_on_random_grids`, `height_only_resizes_keep_every_row_verbatim` (grid.rs) | DC3. Streams cells with no persisted intermediates; height-only changes never reflow; the removed implementation stays in the test module as the oracle |
| Resize coalescing (client settle timer, server batch filter, PTY size cache) | Shipped | `uniterm_client::resize::ResizeCoalescer`, `uniterm_server::server::io::coalesce_resizes`, `uniterm_server::pty::PtyProcess::resize` | `a_storm_settles_to_its_final_size_and_never_loses_it` (resize.rs), `a_resize_burst_in_one_batch_relayouts_once_at_the_final_size` (tests/resize_storm.rs), `resize_records_the_size_the_child_sees_and_skips_repeats` (pty.rs) | One relayout per settled size instead of one per intermediate geometry |
| Pane termination (TERM then KILL per process group, drained reap) | Shipped | `uniterm_server::pty::PtyProcess::finish_terminate` | `termination_escalates_for_the_whole_process_group`, `a_shell_with_unread_output_is_killed_and_reaped_promptly` (pty.rs) | Never blocks in `waitpid`: the master is drained while the shell exits, which on macOS is what lets a shell with unread output finish exiting |
| VT semantics: erase, edit, DEC modes, origin, tabs, repeat, SGR | Shipped | `Terminal::feed` | `private_mode_batches_origin_insert_and_cursor_visibility_work`, `dec_special_graphics_and_tab_controls_are_applied`, `osc_metadata_is_routed_and_never_drawn` | DC4 |
| Copy-mode grapheme and display-column correctness | Shipped | `copymode::line_text_range`, `copymode::search_column` | `mouse_selection_anchors_drags_and_yanks`, `selection_yields_text_and_copies`, `big_word_motions_use_whitespace_delimiters` | DC5. Cursor `left`/`right` step one column and do not skip a wide-glyph continuation |
| Width-aware damage renderer | Shipped | `uniterm_server::renderer::Renderer::render` | `damage_touching_a_wide_half_repaints_the_whole_glyph_span`, `full_and_incremental_output_match_an_independent_screen_oracle` | DC6 |
| Multi-client geometry and flow control | Shipped | `Server::recompute_client_geometry` | `smallest_attached_client_defines_shared_canvas`, `large_input_crosses_nonblocking_pty_without_truncation`, `slow_client_queue_is_bounded_and_disconnected` | DC7 |
| Differential viewport comparison with tmux | Partial | `Terminal::feed` compared to `tmux capture-pane` | `common_cli_stream_matches_tmux_viewport` (tests/vt_differential.rs) | DC8. `if Command::new("tmux").arg("-V").output().is_err() { return; }` makes the test assert nothing on a host without tmux, and it reports as passing. One hand-written fixture, not a corpus |
| Damage-only rendering, zero bytes when nothing changed | Shipped | `Renderer::render` | `clean_grid_emits_nothing`, `damage_and_idle_contract` (grid.rs), `idle_control_listener_does_not_wake_the_core` | Architectural invariant 3 and 4 |
| Mouse pane focus and status-line clicks | Shipped | `Server::on_mouse` | `mouse_scanner_extracts_events_and_keeps_keys`, `pane_menu_split_targets_the_pane_and_close_is_immediate` | FM1 |
| Clickable overlay rows and buttons | Shipped | `uniterm_client::overlay::row_at` | `row_at_maps_interior_only`, `clicks_select_rows_and_press_buttons` | FM2 |
| Mouse wheel scrollback and alt-screen arrow emulation | Shipped | `MouseKind::WheelUp` / `WheelDown` routing in server.rs | `wheel_scrolls_scrollback_and_returns_to_live`, `wheel_scrolls_history_emitted_by_an_inline_tui_region` | BF3 |
| Application mouse passthrough (DEC 1000/1002/1003/1006) | Shipped | per-pane private-mode tracking in `terminal.rs` | tests/bf4_mouse_passthrough.rs | BF4 |
| Always-on drag selection with OSC 52 yank | Shipped | `CopyState` mouse anchor path | `drag_selects_text_and_yanks_to_clipboard` (tests/s1_selection.rs) | S1 |
| Tab overview (zoom out) | Shipped | `Command::Overview`, `uniterm_core::layout` tile function | `overview_lists_tiles_and_click_switches`, `overview_tiles_fill_the_area_exactly` | S2 |
| Workspace switcher and kill | Shipped | `uniterm_client::sessions`, `ut workspace` | `stopped_workspaces_are_labeled_and_revived_on_enter`, `rename_session_moves_socket_and_updates_status` | S3 |
| Menu bar: model, drawing, dropdowns, window commands, rename input | Shipped | `uniterm_core::menu`, `uniterm_client::menu`, `Command::KillWindow` / `RenameWindow` | `workspace_menu_has_requested_groups_and_actions`, `item_hit_test_matches_rows`, `rename_shows_in_status_and_kill_window_closes_it` | MB1-MB5 |
| Quick-prompt key does not collide with PrevWindow | Shipped | `OVERLAY_KEY = b'N'` (uniterm-client/src/lib.rs) | `prefix_bindings_do_not_collide`, `prefix_then_p_toggles_overlay` | BF5 |
| Focus reporting, per-client repaint, `SIGCONT` recovery | Shipped | client `SIGCONT` handler; focus-report consumption | `focus_reports_are_consumed_and_focus_in_requests_repaint` | HD8 |
| Outer-terminal capability negotiation | Partial | attach reports outer-terminal facts in `ClientMessage::Attach` | none | No complete output-capability profile is selected |
| Kitty keyboard protocol | Missing | none | none | `ESC[?u` / `ESC[>u` are consumed and ignored in terminal.rs; no negotiation or encoding |
| Kitty graphics or Sixel image plane | Missing | none | none | No image storage, damage, or transport path exists |
| Native Windows (ConPTY, named pipes) | Missing | none | none | |

## Persistence and recovery

| Capability | Status | Entry point | Evidence | Notes |
|---|---|---|---|---|
| Structural snapshot and atomic restore on start | Shipped | `uniterm_server::server::Server::recover_workspace`; `persist::save` (temp + rename) | `server_restores_structure_from_snapshot` (tests/p2_persist.rs), `snapshot_round_trips` | P2-1 |
| Grid and scrollback content persistence | Shipped | `Grid::export_lines` into `persist::PaneSnap` | `persisted_graphemes_resolve_across_grids`, `pty_output_advances_the_crash_snapshot_without_a_structural_change` | P2-2 |
| Append-only event log | Shipped | `uniterm_server::eventlog` | `append_projects_and_persists`, `structural_log_recovers_without_a_snapshot`, `envelope_sequences_are_monotonic_and_workspace_scoped` | P2-3 |
| Autosave on by default | Shipped | `uniterm_core::config::Config.restore` (default `true`); dirty-triggered snapshots | `restore_defaults_on_and_is_configurable`, tests/dirty_snapshot_cadence.rs | P2-4 |
| Restore-on-start prompting | Missing | `Config.restore` toggle plus `Field::Restore` in the Settings dialog | none | P2-4. Restore is applied silently when the flag is on; no prompt, dialog, or overlay exists in any crate |
| Snapshot schema migration | Shipped | `persist::migrate` compatibility structures | `v9_snapshot_migrates_with_an_empty_event_cursor`, `v10_snapshot_retains_its_structural_cursor_and_starts_an_empty_graph`, `v11_snapshot_retains_its_graph_and_starts_an_empty_artifact_ledger` | |
| Checkpoint-cursor and suffix-only replay | Shipped | `eventlog` streaming reducers | `structural_suffix_overrides_checkpoint_and_keeps_grid_content`, `artifact_replay_advances_only_the_suffix_after_a_checkpoint` | |
| Corrupt-tail diagnosis and repair to the last consistent prefix | Shipped | `eventlog::repair` | `repair_keeps_the_prefix_before_a_partial_failed_append`, `repair_discards_a_gapped_suffix_but_never_a_future_schema`, `truncated_final_record_does_not_hide_the_last_complete_projection` | |
| Append-failure poisoning (no false checkpoint) | Shipped | runtime event writer | `failed_event_append_freezes_followups_and_snapshots` | |
| Workspace advisory lock | Shipped | sidecar `flock` in `persist` | `workspace_lock_outlives_the_socket_path` | |
| Continuous dirty-output crash checkpoints | Shipped | two-second armed snapshot deadline in server.rs | `pty_output_advances_the_crash_snapshot_without_a_structural_change` | P1K |
| Workspace catalog and clean-stop restoration | Shipped | `uniterm_server::workspace_catalog` | `clean_stop_rebuilds_projects_tabs_and_split_geometry_without_runtime_state`, `list_includes_stopped_workspaces_and_forget_removes_them` | |

## Agent supervision and detection

| Capability | Status | Entry point | Evidence | Notes |
|---|---|---|---|---|
| OSC 777 parsing and agent registry | Shipped | `Terminal::feed` OSC routing; `uniterm_core::agent` registry | `parses_osc777_agent_event`, `registry_colors_and_names`, `every_provider_has_a_banner` | AG1 |
| Evidence-based status reconciliation with dwell smoothing | Shipped | `uniterm_core::agent` reconciliation; `providers::Catalog` rules | `permission_rule_outranks_idle_rule`, `stale_fallback_cannot_override_newer_agent_evidence`, `agent_evidence_authority_matches_reconciliation_order` | AG1, HD3 |
| Native process-exit detection (pidfd / kqueue) | Shipped | `uniterm_server::process_watch` | `kernel_notifies_once_when_the_process_exits`, `exit_event_maps_to_exited_status` | Invariant 5 |
| Agent unbind on exit | Shipped | `pane.agent.take()` on `session_end` / exit in server.rs | `exit_event_maps_to_exited_status` | BF2. No direct assertion that the sidebar entry disappears |
| `ut agent explain` provenance reporting | Shipped | `ut agent explain PANE` | `local_manifest_reload_reclassifies_current_grid_without_pty_activity`, `versioned_rules_preserve_explanation_hints` | HD3 |
| Provider connectors (notify hooks) for 8 providers | Shipped | `uniterm_server::connectors` | `every_registry_provider_has_a_connector`, `nested_install_uninstall_round_trip`, `codex_flag_created_updated_and_left_intact` | AG8 |
| Fleet sidebar with provider branding | Shipped | `Server::draw_sidebar` (server.rs) | `agent_uses_branded_sidebar_without_a_pane_frame`, `sidebar_agents_group_by_project_and_keep_start_order_when_status_changes` | HD2, AG2 |
| Manage Agents modal (install state, connectors, launch, stop-all) | Shipped | Agents menu > Setup...; `uniterm_client::agents` | `agents_snapshot_lists_every_provider`, `launched_agent_is_bound_and_listed_without_a_connector`, `stop_all_needs_a_second_x` | AG8 |
| Attention notifications with stale-cancel | Shipped | `Server::notify_agent_transition`, `runtime::system_notification` | `notification_and_file_sidebar_settings_round_trip` | Platform-dependent; no end-to-end delivery test |
| Notification sound (bell, synthesized chime, custom file) | Shipped | `Server::deliver_agent_notification` emits `ServerMessage::Chime`; `uniterm-client/src/chime.rs` synthesizes and plays it | `a_permission_prompt_chimes_the_client_and_a_quiet_idle_does_not`, `chime::tests` | Playback needs a local player (`pw-play`, `paplay`, `aplay`, `ffplay`, `afplay`); falls back to the bell |
| Versioned local detection manifests with precedence and reload | Shipped | `providers::Catalog::load_from_paths`, `ManifestWatcher::start` | `local_cache_last_good_and_bundled_precedence_is_explicit`, `local_manifest_reload_reclassifies_current_grid_without_pty_activity` | P1D |
| Offline manifest validation | Shipped | `ut agent manifests validate PATH` | `manifest_validation_is_offline_and_rejects_control_patterns` (uniterm-cli), `validation_rejects_broad_control_and_unbounded_data` | P1D |
| Publisher-authenticated manifest distribution | Missing | none | none | P1D. No HTTP client, no signature verification, and nothing in the workspace writes the "verified cache"; `read_verified` only compares a local SHA-256 sidecar |
| Launch-profile and native-resume safety | Shipped | invocation-scoped launch profile in server.rs | `native_resume_rejects_forged_or_foreign_argv`, `resume_argv_is_checked_against_the_trusted_builtin_boundary`, `osc777_retains_provider_owned_native_resume_identity` | |

## Orchestration: workflows, relay, waiting queue, instructions

| Capability | Status | Entry point | Evidence | Notes |
|---|---|---|---|---|
| Pure orchestration decision engine | Shipped | `uniterm_core::orchestrate` (`decide_relay_next`, workflow transitions) | 15 orchestrate.rs unit tests | AG5 |
| Live workflow launch with per-role panes and tokens | Shipped | `/workflow <name>` in New Task; `Server::launch_workflow` | `pair_workflow_advances_on_submits_and_completes` (tests/w2_workflow.rs) | W2 |
| `uniterm workflow submit` / `relay submit` completion contract | Shipped | `ut workflow submit`, `ut relay submit` (`uniterm_cli::cmd_submit`) | `pair_workflow_advances_on_submits_and_completes`, `relay_needs_input_can_be_answered_and_then_completes` | Neither subcommand appears in `ut --help` |
| Live relay runtime across two panes | Shipped | `uniterm_server::server::orchestration::ActiveRelay`, `Server::launch_relay` | `relay_needs_input_can_be_answered_and_then_completes` (tests/orchestration_waiting.rs) | AG5. Genuine multi-turn test: token mint, `needs_input`, cross-token rejection, answer, handoff to role two, done |
| Verifier-only verdicts, iteration caps, stall detection | Shipped | `uniterm_core::orchestrate` | orchestrate.rs unit tests; `idle_fallback_never_bypasses_human_or_artifact_gates` | |
| Artifact gates through evented file observation | Shipped | `runtime` artifact gate + watch | `artifact_gate_requires_nonempty_files_inside_the_project`, `artifact_watch_reobserves_once_then_stays_idle_without_a_timer` | |
| Bounded prompt-delivery retry | Shipped | `server::orchestration::schedule_prompt_retry`, `flush_prompt_deliveries_due` | none | Three event-armed attempts then a waiting item; no test drives the ladder |
| Stall escalation on identical verdicts and silent roles | Shipped | `uniterm_core::orchestrate` stall detection; `flush_orchestration_stalls` | `two_identical_fix_verdicts_stall_and_abort`, `failure_and_stall_and_stop_escalate` (orchestrate.rs) | Core-only coverage; the ten-minute server timeout has no integration test |
| Git checkpoint and rollback confirmed by Git before caching | Shipped | Tokio-side `CoreToAgent::RelayCheckpointCreate` handler | `relay_checkpoint_is_cached_only_after_git_can_restore_it`, `relay_checkpoint_failure_returns_no_false_reference` | |
| Restart recovery of active workflows and relays | Shipped | `server::orchestration` recovery projection | `recovered_runs_accept_only_restartable_phases_and_valid_shapes`, `recovery_preserves_mixed_providers_and_migrates_legacy_scalar_ownership`, `orchestration_projection_replay_keeps_only_active_runs` | Unit-level projection tests; no end-to-end kill-and-restart integration test |
| Workspace-scoped waiting queue with focus, answer, dismiss, stop, resume, rollback | Shipped | `ut waiting`, Observatory waiting rows, `server::waiting` | `relay_needs_input_can_be_answered_and_then_completes`, `waiting_answer_uses_the_semantic_action_and_preserves_utf8`, `rollback_is_only_offered_for_a_relay_wait` | |
| Instruction queue and steering | Shipped | `ut instruction list/add/replace/cancel/send-now` | `instructions_wait_for_cooperative_ready_and_stay_invocation_scoped` (tests/instruction_queue.rs); 5 instruction.rs unit tests | P1B |
| Agent selection and real agent launch from New Task | Shipped | `@agent` token in New Task; `workflow::launch_invocation` | `suggests_installed_agents_after_at`, `launch_invocation_quotes_custom_executable_paths`, `announce_wrapper_emits_a_parseable_envelope` | W1 |
| Per-role provider selection | Shipped | `@role=provider` in New Task; `orchestration_start` control launch | `role_provider_resolution_applies_explicit_choices_over_global_fallback`, `role_provider_resolution_reports_the_role_with_a_missing_cli`, `new_task_preserves_explicit_role_provider_selections` | P1F |
| Native run graph (runs, roles, parents, handoff) | Shipped | `ut run list`; `uniterm_core::run_graph` | `indexes_parent_project_roles_panes_and_activations`, `run_graph_replay_advances_a_checkpoint_through_handoff_and_completion` | P1E |
| Worktree-backed child runs (`ut run fork`) | Shipped | `ut run fork PARENT NAME PATH [BASE]` | `active_run_forks_into_fresh_worktree_owned_identities` (tests/child_run_fork.rs) | P1I |
| Typed artifact ledger (identity, ownership, lifecycle, recovery) | Shipped | `uniterm_core::artifact::ArtifactLedger::apply`; `server::artifact` | `indexes_ownership_and_supersedes_one_current_path`, `refresh_and_missing_preserve_identity_and_reject_stale_updates`, `artifact_replay_advances_only_the_suffix_after_a_checkpoint` | P1G core |
| Artifact inspection CLI (`ut artifact list`) | Shipped | `ut artifact list [--project] [--run] [--all] [--json]` | `artifact_inspection_round_trips_on_binary_and_handwritten_json` (proto) | P1G. No test invokes the CLI path, so argument parsing and output formatting are untested |
| Artifact review annotations | Missing | none | none | P1G. `ArtifactEvent` has only `Observed`, `Refreshed`, `Missing`; no annotation type, event, or wire message anywhere |
| Artifact UI projections | Missing | none | none | P1G. `uniterm-client` matches `ServerMessage::Artifacts` into an empty arm and renders nothing |
| Guardrails: active-run, role-pane, iteration, elapsed, exact Project allow-list | Shipped | `uniterm_core::evaluate_launch` / `evaluate_elapsed` via `Server::prepare_orchestration_launch` | `control_launch_obeys_project_capacity_and_elapsed_guards_without_partial_panes` (tests/guardrail_contract.rs), `launch_allows_owned_capacity_and_denies_each_boundary` | P1H |
| Guardrail Settings controls | Shipped | Settings dialog guardrail rows | `guardrail_rows_emit_bounded_numeric_and_exact_project_patches`, `guardrail_limits_and_exact_project_selectors_round_trip` | P1H |
| Guardrail semantic-confirmation gate | Shipped | `Server::guard_semantic` for `ClientMessage::ProjectRemove`, `AgentsStopAll`, `KillServer`, and the `project_remove` and `agent_stop_all` control methods | `destructive_control_methods_require_explicit_confirmation` (tests/control_api.rs), `destructive_and_bulk_commands_ask_until_confirmed` (guardrail.rs) | P1H. Confirmation is the human choice carried on the wire; unconfirmed control requests get `confirmation_required` |
| Guardrail token and cost budgets | Missing | none | none | P1H. `GuardLimits` has four fields, none of them a budget; blocked on authoritative invocation-scoped usage facts |
| Durable task manager | Shipped | `uniterm_core::tasks`; Tasks modal | `add_get_and_status_transitions`, `navigation_and_status_cycle`, `inline_edit_round_trip`, `large_task_history_replays_without_collecting_the_event_stream` | AG7 |
| New Task floating prompt with slash commands | Partial | `prefix N`; `uniterm_client::task::TaskInput` | `recognizes_slash_commands`, `suggests_workflow_templates_and_project_names`, `selection_wraps_and_tab_uses_it` | AG4. The overlay, `/workflow`, `/relay`, `/project`, `/save`, `@provider` and `@role=provider` all work. The claimed `ut` command does not exist; there is no `ut task` or `ut new-task` in the CLI dispatch, so the only non-TUI launch path is control `orchestration_start` |
| Floating overlay infrastructure with drop shadow | Shipped | `uniterm_client::overlay` | `box_is_centered_within_bounds`, `shadow_is_offset_and_present`, `long_content_is_clipped_to_interior` | AG3 |

## Observatory and projections

| Capability | Status | Entry point | Evidence | Notes |
|---|---|---|---|---|
| Docked Agents surface with attention-first sorting | Shipped | `chrome::ObservatoryTab::Agents` (server-drawn dock) | `agent_uses_branded_sidebar_without_a_pane_frame`, `blocked_states_sort_before_healthy_ones`, `fleet_sorts_blocked_first_and_filters_waiting` | AG6 partial |
| Observatory modal with agents, servers, waiting queue, filters | Shipped | `prefix o`; `uniterm_client::observatory::ObservatoryView` | `filters_and_jumps_by_stable_pane_id`, `waiting_answer_uses_the_semantic_action_and_preserves_utf8`, `rollback_is_only_offered_for_a_relay_wait` | AG6 partial. One list plus a detail pane; it has no tabs |
| Docked Files surface with cached Git summaries | Shipped | `chrome::ObservatoryTab::Files`; `uniterm_server::file_manager`, `git_status` | `git_summary_does_not_shift_file_clicks_to_the_next_row`, `git_stats_are_scoped_to_the_visible_project`, `counts_tracked_and_untracked_changes` | AG6 partial |
| Docked Web servers surface with loopback verification | Shipped | `chrome::ObservatoryTab::WebServers`; `uniterm_server::dev_server` | `observatory_lists_announced_server_and_plain_url_click_opens_it`, `web_server_probe_is_armed_by_evidence_and_disarms_when_down`, `hidden_web_server_surface_does_not_run_liveness_ticks` | AG6 partial |
| Workflow and relay detail projections in the Observatory | Missing | none | none | AG6. The dock is exactly `[Agents, Files, WebServers]`; `ObservatoryView` holds no run, workflow, or relay field |
| Timeline | Missing | none | none | The event log carries the data; no view projects it |
| Agent memory proposals | Missing | none | none | No type, event, or surface exists |
| Token and cost telemetry | Missing | none | none | No usage normalization, no pricing data file |

## Hierarchy, projects, and workspaces

| Capability | Status | Entry point | Evidence | Notes |
|---|---|---|---|---|
| Workspace > Project > Tab > Pane hierarchy with durable selection | Shipped | `ut project`, `ut tab`, `ut pane`; `Server` hierarchy state | `projects_own_tabs_and_switch_as_one_scope`, `every_new_tab_and_pane_starts_at_the_project_root` | HD1 |
| Project management (add, rename, reorder, switch, remove, metadata) | Shipped | `ut project add/rename/move/switch/remove/metadata` | `switch_and_remove_are_explicit`, `move_reorders_the_modal_immediately_and_keeps_selection` | HD1, HD7 |
| Fleet listing from the CLI | Shipped | `ut agent list [--json]` (control `agent_list`) | `tabs_can_be_created_and_named_by_hierarchy_position` (uniterm-cli/tests/tab_cli.rs) | |
| Tab creation and renaming from the CLI | Shipped | `ut tab new [project]`, `ut tab rename <project> <tab> <name>` (control `tab_create`, `tab_rename`) | `tabs_can_be_created_and_named_by_hierarchy_position` (uniterm-cli/tests/tab_cli.rs) | |
| Scriptable hierarchy focus | Shipped | `ut tab focus <project> <tab>`, `ut pane focus <project> <tab> <pane>` | `pane_list_json_and_focus_are_scriptable_across_projects` (uniterm-cli) | |
| Workspace management (list, create detached, attach, rename, stop, forget) | Shipped | `ut workspace list/new/switch/rename/stop/forget` | `list_includes_stopped_workspaces_and_forget_removes_them`, `rename_session_moves_socket_and_updates_status` | |
| Desktop hierarchy migration (dry-run and interactive) | Shipped | `ut migrate` | `desktop_hierarchy_import_creates_fresh_projects_and_tabs`, `detection_uses_native_linux_macos_and_windows_locations`, `joins_workspace_project_and_tab_records` | |
| Pane and Project metadata with optional one-shot TTL | Shipped | `ut pane metadata`, `ut project metadata` | `workspace_layout_round_trip_replaces_only_pane_identities` | HD7. No test asserts TTL expiry damage-gating |
| Semantic themes and live Settings application | Shipped | `uniterm_core::config` semantic colours; Settings modal | `semantic_presets_and_settings_round_trip`, `theme_cycles_and_boolean_toggles`, `settings_merge_preserves_comments_and_unknown_keys` | HD4, HD5 |
| Shared modal geometry across menus, Settings, Projects, Tasks, Agents | Shipped | `chrome::modal_rect`; `uniterm_client::overlay` | `every_box_row_paints_exactly_its_full_width` (agents.rs and taskview.rs), `semantic_styles_do_not_change_composer_geometry` | HD6 |
| File manager mutations confined to the project root | Shipped | `uniterm_server::file_manager` | `file_operations_stay_inside_the_project_root`, `copies_files_and_folders_relative_to_the_project_root`, `a_truncated_directory_remains_browsable_and_reports_the_limit` | |
| Worktree lifecycle (create, list, open, remove, forced remove, cleanup) | Shipped | `ut project worktree add/list/open/remove` | `create_open_list_dirty_remove_and_stale_cleanup_share_git_authority`, `cli_requires_separate_force_confirmation_and_restores_provenance` | P1C |

## Automation and remote

| Capability | Status | Entry point | Evidence | Notes |
|---|---|---|---|---|
| Owner-only NDJSON control socket with capability discovery | Shipped | `uniterm_server::control_api`; `server::control` dispatch | `snapshots_pane_io_and_cursored_events_share_one_private_stream` (tests/control_api.rs), `control_path_conflict_fails_startup_without_replacing_user_data` | P1A |
| Bounded intake and lagging-subscriber handling | Shipped | `control_api` queues | `full_request_intake_disconnects_the_flooding_reader`, `lagging_connection_never_grows_past_its_bounded_queue` | |
| Cursored durable-event subscriptions | Shipped | `control_api` subscribe with `after_sequence` | `subscription_catch_up_precedes_queued_live_events_exactly_once`, `large_subscription_catch_up_does_not_stall_control_dispatch`, `failed_subscription_catch_up_returns_a_structured_stream_error` | |
| Broad semantic control vocabulary (Project, Tab, Pane, Agent, Task, waiting, workflow, relay, Run, Artifact, worktree) | Shipped | `server::control` | `broader_control_resources_keep_human_writable_json_shapes`, `orchestration_launch_has_one_human_writable_control_shape` | P1J. Coverage is wire-shape round-trips plus the control_api integration test; not every method has a behavioural test |
| Direct Pane attach with observer, controller, takeover | Shipped | `ut agent attach` / `ut pane` attach path | `direct_attach_streams_one_pane_and_enforces_controller_takeover` (tests/pane_attach.rs), `direct_pane_attach_roles_round_trip_on_the_binary_stream` | |
| Pane automation (list, focus, read, send, wait-output, metadata) | Shipped | `ut pane list/focus/read/send-keys/wait-output/metadata` | `pane_control_waits_on_output_events_and_reads_the_server_grid`, `pane_list_json_and_focus_are_scriptable_across_projects` | |
| Agent automation (start, prompt, send, read, wait, attach, explain) | Shipped | `ut agent start/prompt/read/wait/attach/explain` | `launched_agent_is_bound_and_listed_without_a_connector`, `launched_agent_settles_from_starting_to_idle_without_polling` | |
| SSH thin-client remote attach | Shipped | `ut remote HOST [Workspace]`, `ut remote-bridge`, `ut remote-check` | 12 remote.rs unit tests including `bridge_rejects_protocol_mismatch`, `handshake_discards_bounded_login_noise`, `remote_bridge_publishes_its_search_path_before_attach_bytes` | No end-to-end integration test over a real SSH hop |
| Repaint supersession under backpressure | Shipped | `Server` per-client repaint state | `backpressured_render_bursts_collapse_to_one_repaint`, `protocol_output_before_first_render_does_not_defer_the_frame` | |
| Automatic remote bootstrap | Missing | none | none | `ut remote` exits with "Uniterm is not installed on the remote host"; no probe, download, verify, or install path |
| Live process handoff on server upgrade | Missing | none | none | |
| Out-of-process plugin runtime | Missing | none | none | Deliberately deferred until the control API is stable |
| Browser or multi-user collaboration client | By design absent | none | none | Non-goal (docs/00) |
| Model catalogs, credentials, or inference routing | By design absent | none | none | Non-goal (docs/00) |
| Managed cloud sandbox control plane | By design absent | none | none | Non-goal (docs/00) |
| Interception of arbitrary provider tool calls | By design absent | none | none | Non-goal; Uniterm guards only what Uniterm owns |

## Counts

| Status | Count |
|---|---|
| Shipped | 95 |
| Partial | 5 |
| Missing | 15 |
| By design absent | 4 |
| Total | 119 |
