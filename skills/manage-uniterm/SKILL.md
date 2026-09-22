---
name: manage-uniterm
description: Control, monitor, and act on a running Uniterm Workspace from any AI agent or harness. Use when asked to find or watch other agents in other Panes, Tabs, or Projects; read what another Pane shows; send text or a prompt to another Pane; copy output from one Pane into another; focus or activate a Project, Tab, or Pane; create, rename, or reorder Tabs and Projects; queue direction for a busy agent; handle the waiting queue; or start and coordinate agents inside Uniterm.
---

# Manage Uniterm

Uniterm is a terminal multiplexer built for agentic engineering.
One Workspace is one server; it owns Projects, each Project owns Tabs, each Tab owns Panes, and each Pane is one PTY with a stable numeric id.
Everything below goes through the `ut` command, which talks to the running server over its socket, so it works from inside a Uniterm Pane, from a plain shell on the same machine, or from a script.
Nothing here scrapes screens with `tmux capture-pane` or polls processes; Uniterm owns the grid and the process tree and answers directly.

## Orient yourself first

```sh
ut pane list --json            # every live Pane: id, project, project_name, tab, tab_name, pane, active
ut agent list --json           # every running agent: pane_id, agent, status, project_name, tab, evidence
ut project list                # Projects in the Workspace, with the active one marked
ut workspace list              # running and stopped Workspaces on this machine
```

If you are running inside a Uniterm Pane, `$UNITERM_PANE_ID` is your own Pane and `$UNITERM_SOCKET` is your Workspace; `ut` uses them automatically.
Add `-w NAME` to any command to address another Workspace.

Identify things by stable ids where you can: Pane ids never change for the life of the Pane, Project ids never change for the life of the Project.
Project selectors accept an id or a case-insensitive name; Tab selectors accept a 1-based ordinal within the Project or a case-insensitive Tab name; hierarchy Pane selectors use the 1-based ordinal that `ut pane list --json` reports.

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

## Find and monitor other agents

```sh
ut agent list --json                   # every running agent with status, authority, and evidence
ut agent explain PANE                  # why Uniterm believes one Pane's status (omit PANE for your own)
ut agent wait PANE idle --timeout 120  # block until the agent reaches a status (event-driven, no polling)
ut pane read PANE --lines 200 --json   # what that Pane currently shows, bounded and marked truncated when cut
ut pane wait-output PANE "tests passed" --timeout 60 --json
```

Statuses are `starting`, `working`, `tool`, `permission`, `question`, `idle`, `error`, and `exited`.
`permission` and `question` mean a human decision is needed; `working` and `tool` mean leave it alone.
Uniterm decides status from the agent's own cooperative signal first, then from anchored screen evidence, and it never treats output volume as activity, so a quiet `working` is real.

Prefer `ut agent wait` and `ut pane wait-output` over sleep loops; both return as soon as the server sees the change and fail clearly on timeout.
Treat a timeout, a missing Pane, or `truncated: true` as a real outcome, never as empty success.

## Send things to other Panes

```sh
ut pane send-keys PANE "cargo test"            # types the text, does not submit
ut pane send-keys PANE "cargo test" --enter    # types and presses Enter
ut agent prompt PANE "Summarise the failing tests"   # a prompt for an agent's input box, submitted
```

`send-keys` writes exactly what you give it into the Pane's input, so quote carefully and add `--enter` only when you mean to submit.
A non-zero exit means the Pane is gone or its input queue is full; never assume delivery.
To hand a busy agent direction that must reach exactly its current invocation, use the instruction queue below instead of typing over it.

## Copy from one Pane to another

Read, then send: there is no clipboard step.

```sh
ut pane read 3 --lines 40 --json | jq -r '.text' > /tmp/from-3.txt
ut pane send-keys 7 "$(cat /tmp/from-3.txt)"
```

For a single line, pipe directly: `ut pane send-keys 7 "$(ut pane read 3 --lines 1 --json | jq -r .text)"`.
Check `truncated` in the JSON when copying long output; ask for more lines rather than pasting a cut tail.

## Activate Projects, Tabs, and Panes

```sh
ut project switch api            # by name or id; the sidebar and Tab bar follow
ut tab focus api 2               # Project, then Tab ordinal or name
ut pane focus 12                 # by stable Pane id
ut pane focus api 2 1            # by hierarchy ordinals
ut agent attach 12               # stream that Pane directly (add --observe to watch without control)
```

Focusing changes what attached humans see, so do it when the human asked to look, not as a side effect of reading.
Reading and sending never require focus.

## Organise the Workspace

```sh
ut tab new --background          # a Tab in the active Project; prints its ordinal
ut tab new api --background      # a Tab in a named Project
ut tab rename api 2 "Review"     # name a Tab by Project and ordinal or current name
ut tab move left|right           # reorder the active Tab within its Project
ut project add web ~/work/web --background # a Project with its first Tab
ut project rename web "Web app"
ut project move web up|down      # reorder the left sidebar
ut project remove web            # closes every Pane it owns; confirm with the human first
ut pane metadata PANE task "fixing flaky tests" --ttl 600   # a note shown under the agent in the sidebar
```

`pane metadata` keys the sidebar understands are `task`, `model`, `branch`, `title`, and `cwd`; an empty value removes the key and `--ttl SECONDS` lets it expire on its own.

## Direct a busy agent without interrupting it

```sh
ut instruction add PANE "When the tests pass, also run clippy"   # delivered at the agent's next ready point
ut instruction list --json
ut instruction replace ID "New wording"
ut instruction cancel ID
ut instruction send-now ID       # explicit bypass, only for an urgent interruption
```

Queued instructions reach exactly the invocation that was running when you queued them; if that agent exits, the instruction is cancelled rather than delivered to a stranger.

## Handle the waiting queue

```sh
ut waiting list --json           # Panes waiting on a human, most urgent first
ut waiting focus ID
ut waiting answer ID "yes"       # answers a question or a permission prompt in that Pane
ut waiting dismiss ID
ut waiting stop ID               # stops that agent
ut waiting resume ID             # resumes a paused orchestration
```

Answer only what the human has delegated to you; a permission prompt is a decision, not a formality.

## Start and coordinate agents

```sh
ut agent start claude --pane PANE --background       # a split beside an explicit Pane
ut agent start claude --pane PANE --tab --background # a new Tab in that Pane's Project
ut agent start codex --pane PANE --current --background # use that Pane's shell
ut run list --active --json      # native workflows and relays with their Roles and Panes
ut artifact list --run ID --json # what a run produced
```

Workflows and relays finish through the explicit completion contract (`uniterm workflow submit` and `uniterm relay submit` with the token Uniterm injected into the prompt), never through idle guessing.
Bulk actions stay inside the current Workspace by design; a stop-all or remove is scoped and requires confirmation.

## Rules of the road

- Use ids from `ut pane list --json`; do not parse the status line or guess ordinals from memory.
- Read before you send: know what is in the Pane and whether an agent is `working` before typing into it.
- Never type into a Pane whose agent is `permission` or `question` unless the human delegated that decision; use `ut waiting answer` so the decision is recorded.
- Keep your own work in your own Pane; open a new Tab (`ut tab new --background`) for anything that needs a terminal of its own.
- Every command exits non-zero on failure and says why on stderr; check exit codes rather than assuming.
- `ut --skill` prints Uniterm's own short control guide and `ut help TOPIC` searches command help when you need a flag you do not remember.
