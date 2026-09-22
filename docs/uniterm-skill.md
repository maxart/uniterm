# Uniterm agent control guide

Use Uniterm's stable Pane ids instead of scraping terminal processes.

Discover Panes with `ut pane list --json`.

Inside a Pane, use `current` or `$UNITERM_PANE_ID` and `$UNITERM_SOCKET` to retain caller-local targeting.

## Work in the background

First run `ut help background` to verify that the installed CLI supports these commands.
The CLI also checks the connected server's capability before sending background mutations and refuses an older server that could ignore the flag.
Always use `--background` for automation that creates a Project, Tab, split, or agent.
It preserves the human's active Project, Tab, Pane, and existing zoom instead of switching there and back.
Select explicit targets from `ut pane list --json`; `--pane PANE` anchors an agent launch in that Pane's Project and Tab.
Without `--background`, creation retains its interactive focus behavior.
A background split of the visible Tab still changes its layout; use a separate Tab when the human's terminal geometry must stay unchanged.
Closing the human's selected Pane or Tab necessarily chooses a replacement, so close only resources you own.

```sh
ut project add worker /absolute/project/path --background  # prints the new Project id
ut tab new worker --background                             # prints the new Tab ordinal
ut tab rename worker 2 "Review"                            # preserves focus
ut tab move worker Review left                            # preserves focus
ut pane split PANE --background                            # prints the new Pane id
ut agent start codex --pane PANE --tab --background         # prints the agent Pane id
ut pane close PANE
ut tab close worker Review
ut project remove worker                                  # explicitly closes every owned Pane
```

Creation does not attach a client or steal focus.
Reading, pasting, submitting prompts, renaming targeted Tabs, and closing other resources do not require a focus command.
Reserve `project switch`, `tab focus`, `pane focus`, and `agent attach` for an explicit request to change the human's view.
Workflows, relays, and worktree-open commands retain their existing focus behavior; `--background` is supported on the creation commands shown above.

## Paste text and submit prompts reliably

Prefer `ut agent prompt PANE TEXT` or `ut pane prompt PANE TEXT` for a complete prompt.
Both paste text and then press Enter once, without changing focus.
For a Tab, `ut tab prompt PROJECT TAB TEXT` sends to its remembered active Pane; use a stable Pane id when the Tab contains several agents.

```sh
ut agent prompt 12 "Run the focused tests and explain failures"
ut pane prompt 12 --stdin < prompt.txt      # multiline text without shell interpolation
ut tab prompt worker Review --stdin < prompt.txt
ut pane paste 12 "Draft text"               # insert text only
ut pane submit 12                            # Enter only, submit text already present
ut tab paste worker Review "Draft text"
ut tab submit worker Review
ut pane send-keys 12 "Raw input"            # exact bytes, no automatic Enter
ut pane send-keys 12 "Run tests" --enter     # the same paste-and-submit path as prompt
```

The server reads the destination terminal's bracketed-paste mode, wraps pasted text when enabled, and sends a carriage return (CR, byte 13) outside the paste as Enter.
This is terminal input, independent of the caller's agent provider and the host's keyboard shortcuts, on Uniterm's supported Linux, macOS, and Android Termux hosts.
Do not send the literal words `Enter`, `Return`, `C-m`, or `\n` as prompt text, and do not append a newline hoping it will submit an agent's input box.
`pane submit` is the unambiguous Enter command when text is already in the box.
For multiline input, wait until the destination application is ready and has enabled bracketed paste; a plain shell may execute each line separately.
`--stdin` reads UTF-8 text up to 256 KiB; quote inline text so your shell does not expand it.

A successful send means Uniterm accepted the bytes into the Pane's bounded input queue, not that the agent completed or even accepted the task.
After starting an agent, wait for its ready state with `ut agent wait PANE idle --timeout 60`, and inspect `ut pane read PANE --lines 80 --json` before sending.
After submission, use `ut pane wait-output PANE "expected text" --timeout 60 --json`, `ut agent wait`, or the task's explicit completion contract to check the result.
A timeout is an uncertain outcome; read the Pane before retrying so you do not submit twice.
Do not automatically send a second Enter: it can answer an unexpected permission dialog.
For a busy agent, prefer `ut instruction add PANE TEXT` for delivery at its next cooperative ready boundary.
Custom applications that bind submission to a different key need their documented input convention; Uniterm's submit operation always means the standard terminal Enter key.

Read bounded output with `ut pane read PANE --lines 200 --json`.

Send exact input with `ut pane send-keys PANE TEXT`, adding `--enter` only when submission is intended.
A non-zero exit means the Pane is missing or its input queue is full; never assume delivery.

Wait for literal output with `ut pane wait-output PANE TEXT --timeout 30 --json`.

Wait for reconciled agent state with `ut agent wait PANE idle --timeout 30`.

Start agents with `ut agent start NAME --pane PANE --tab --background` and prompt them with `ut agent prompt PANE TEXT`.
Use `ut agent attach PANE` only when asked to change the human's view.

Queue a human follow-up with `ut instruction add PANE TEXT` so it reaches the exact current invocation on its next cooperative ready event.
Use `ut instruction send-now ID` only for an explicit urgent bypass; heuristic idle never delivers queued direction.

Prefer explicit `uniterm workflow submit` and `uniterm relay submit` completion contracts over guessing completion from idle state.

Inspect durable orchestration ownership with `ut run list --json`; `--active` limits the response to live runs and `--project ID` keeps Project scope explicit.

In the New Task surface, use `@provider` as the workflow-wide fallback and `@role=provider` for explicit mixed-provider roles, for example `/workflow pair @claude @verifier=codex Ship it`.

Keep bulk actions scoped to the current Workspace.

Treat `truncated: true`, stale Pane ids, and timeouts as explicit outcomes rather than successful empty output.

Uniterm waits are event-driven and should be preferred over shell polling loops.
