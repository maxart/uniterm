# Approval fixtures

All ANSI captures use a 120-column, 40-row PTY inside a disposable Bubblewrap container.
They contain no credentials, real project contents, or model-generated responses.
The captures were made on 2026-10-22.

| Fixture | Origin | Validation |
| --- | --- | --- |
| `codex-live.ansi` | Codex 0.163.0, offline local provider, fresh git directory | Real folder trust dialog; standalone harness verifies the saved trust decision |
| `codex-trust.txt` | Transcription of the supplied screenshot | Preserves the Agent Command Center cancellation variant |
| `claude-trust.ansi` | Claude Code 2.1.302, fresh configuration and temporary credential copy | Real startup trust dialog with No selected initially; container check verifies saved trust after staged navigation |
| `opencode-live.ansi` | OpenCode 2.0.32, offline permission API | Real two-button dialog; harness verifies the pending request disappears after Enter |
| `grok-live.ansi` | Grok 1.0.62, offline deterministic fake model | Real numbered radio dialog; harness verifies a tool result arrives only after Enter |
| `grok-trust.txt` | Transcription of the supplied Grok startup screenshot | Both `grok` and `agent` send the advertised `y` shortcut; incomplete warnings, missing choices, policy denials, and newer prompts are rejected |
| `grok-trust-live.ansi` | Grok 1.0.67, offline startup with a harmless personal SessionStart hook | The hook marker is absent before `y` and appears afterward; no paid model call |
| `opencode-command.ansi` | Three-button layout from the local OpenCode permission component | Regression for explicit Allow once styling and rejection of missing/ambiguous styling |
| Other `.txt` files | Explicit dialog contracts, including the local Pi trust-selector source | Positive, incomplete, policy-denial, and question-screen regression cases |

The Grok capture is tested under both `grok` and `agent` identities.
The startup screenshot is a separate contract from the numbered command dialog.
Its shortcut is exercised through the actual Uniterm PTY path, including duplicate suppression and the live-grid latency budget.
The native startup fixture installs a harmless personal hook to trigger Grok's folder-trust requirement; an empty home without hooks can skip this dialog.
Pi and Gemini coverage is fixture-based, not a live authenticated session claim.
Other bundled providers use the shared complete-dialog contract; unfamiliar dialogs remain waiting.

From the repository root, regenerate and validate the offline captures with:

```sh
python3 scripts/test-agent-approval-containers.py codex
python3 scripts/test-agent-approval-containers.py opencode
python3 scripts/test-agent-approval-containers.py grok
# Optional authenticated startup only; no model prompt is submitted.
python3 scripts/test-agent-approval-containers.py claude --authenticated-startup
```

The harness requires Linux user namespaces, Bubblewrap, Python 3.11+, and the installed CLI.
It mounts only system libraries, the selected executable, this harness, and an output directory.
The first three checks isolate networking and contain no host credentials, preventing paid API calls.
The explicit Claude startup option shares networking for its connectivity check and copies only `.claude/.credentials.json` into its disposable home.
It accepts folder trust, verifies Claude's saved trust flag, and exits without submitting a model prompt.
Codex uses no model turn, OpenCode queues a permission through its local API, and Grok receives a hard-coded harmless Python print command from a capped local server.
The harness verifies the native UI action; the Rust replay and real-PTY tests separately verify Uniterm recognition and delivery.

Run the Rust regressions with:

```sh
cargo test -p uniterm-server --lib providers::approval -- --nocapture
cargo test -p uniterm-server --test auto_approval -- --nocapture
cargo test -p uniterm-core approval
```

The optimized `live_grid_approval_budget_includes_negative_screens` test includes extracting the actual grid and checks a five-millisecond p95 bound.
The integration matrix starts with Working status, proving delivery does not depend on Permission dwell.
Navigation sends one arrow, waits for an observed selection change, and only then submits Enter.
An unchanged repaint cannot resend either step, and a submitted dialog remains consumed until progress or a new invocation.
