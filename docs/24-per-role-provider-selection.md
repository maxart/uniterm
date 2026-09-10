# Per-role Provider Selection

## Purpose

Native workflows and relays can assign a different installed agent CLI to each role without making the orchestration engine provider-specific.
Uniterm still owns the run graph, Panes, completion tokens, handoffs, recovery, and event history.
Each provider still owns its executable, login, model selection, conversation state, and cooperative native resume command.

## Selection contract

The interactive New Task surface accepts one global `@provider` and any number of explicit `@role=provider` overrides.

```text
/workflow triad @claude @planner=gemini @builder=codex @verifier=claude Ship the feature
/relay @builder=codex @reviewer=claude Audit and repair the parser
```

The resolution order for every role is:

1. An explicit `@role=provider` selection.
2. The global `@provider` selection.
3. The first installed provider in the built-in registry.

Workflow role names come from the selected template.
The built-in relay roles are `builder` and `reviewer`.
An empty selection, unknown role, duplicate role assignment, missing executable, or unsatisfied capability rejects the launch before any role Pane or run is created.

`uniterm-core` owns only provider-neutral requirements and selection validation.
Bundled roles currently require the stable `interactive_cli` capability.
The server resolves provider ids through the existing registry and performs the one-shot PATH check at launch time.
An executable name or absolute executable path not in the built-in registry remains accepted as a custom provider, preserving the existing local harness and test contract.
No background provider polling is added.

## Shared automation shape

Control API version 1 exposes `orchestration_start` and advertises `orchestration.start`.
It reaches the same server launch functions as the interactive New Task surface.

```json
{"version":1,"id":7,"workspace":"default","method":"orchestration_start","params":{"launch":{"kind":"workflow","template":"pair","goal":"Ship the feature","provider":"claude","role_providers":[{"role":"verifier","provider":"codex"}],"project":null}}}
```

Success returns an `orchestration_started` result containing the stable Run id.
Validation failures return `invalid_orchestration_launch` with an actionable role or provider message and do not create a partial run.
Relay launches use `"kind":"relay"` and require `template` to be `null`.

Binary wire protocol version 9 added the role-provider selection vector to `NewTask`, and it remains available in current version 13.
The selection vocabulary itself is shared from `uniterm-proto`, so automation and the interactive client do not grow separate provider assignment languages.

## Durable ownership and recovery

Live workflow and relay state retains one resolved provider id and exact launch command per ordered role.
The first activation of each role starts that command, emits lifecycle envelopes under that provider id, and binds the Pane to the same provider.
Later turns reuse the already running provider process through ordinary bounded Pane input.

Each run-graph role declaration records its own provider instead of copying a run-wide provider.
The orchestration projection stores the aligned provider vector in the append-only event stream.
Recovery restores the exact mapping rather than rerunning first-installed selection, and legacy single-provider records expand their old scalar ownership across every role.
Malformed or incomplete mappings fail recovery explicitly.

Provider-owned native resume remains invocation-scoped.
When a selected CLI reports its session id and complete resume argv through the cooperative envelope, the existing Pane structural projection records and validates it at the trusted provider boundary.
Per-role selection never invents a resume flag or takes ownership of credentials.

## Current boundary

This slice applies to bundled workflows and the native two-role relay.
The typed artifact ledger is implemented separately in [25-typed-artifact-ledger.md](25-typed-artifact-ledger.md).
Native Workspace guardrails are implemented separately in [26-native-workspace-guardrails.md](26-native-workspace-guardrails.md).
User-defined workflow templates, saved role preferences, capability negotiation beyond `interactive_cli`, and usage or cost projections remain separate work.
