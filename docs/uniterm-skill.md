# Uniterm agent control guide

Use Uniterm's stable Pane ids instead of scraping terminal processes.

Discover Panes with `ut pane list --json`.

Inside a Pane, use `current` or `$UNITERM_PANE_ID` and `$UNITERM_SOCKET` to retain caller-local targeting.

Read bounded output with `ut pane read PANE --lines 200 --json`.

Send exact input with `ut pane send-keys PANE TEXT`, adding `--enter` only when submission is intended.
A non-zero exit means the Pane is missing or its input queue is full; never assume delivery.

Wait for literal output with `ut pane wait-output PANE TEXT --timeout 30 --json`.

Wait for reconciled agent state with `ut agent wait PANE idle --timeout 30`.

Start agents with `ut agent start NAME`, prompt them with `ut agent prompt PANE TEXT`, and focus them with `ut agent attach PANE`.

Queue a human follow-up with `ut instruction add PANE TEXT` so it reaches the exact current invocation on its next cooperative ready event.
Use `ut instruction send-now ID` only for an explicit urgent bypass; heuristic idle never delivers queued direction.

Prefer explicit `uniterm workflow submit` and `uniterm relay submit` completion contracts over guessing completion from idle state.

Inspect durable orchestration ownership with `ut run list --json`; `--active` limits the response to live runs and `--project ID` keeps Project scope explicit.

In the New Task surface, use `@provider` as the workflow-wide fallback and `@role=provider` for explicit mixed-provider roles, for example `/workflow pair @claude @verifier=codex Ship it`.

Keep bulk actions scoped to the current Workspace.

Treat `truncated: true`, stale Pane ids, and timeouts as explicit outcomes rather than successful empty output.

Uniterm waits are event-driven and should be preferred over shell polling loops.
