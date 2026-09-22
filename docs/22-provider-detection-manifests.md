# Versioned Provider Detection Manifests

This document defines the provider detection data contract implemented by Uniterm.
The contract expands agent recognition without adding provider branches to the reconciler or filesystem work to the mio core.

## Source order

Uniterm compiles one immutable catalog snapshot from these sources, in winning order:

1. `$XDG_CONFIG_HOME/uniterm/providers.json`, the explicit local override.
2. `$XDG_CACHE_HOME/uniterm/providers.json`, accepted only when `providers.json.sha256` matches its exact bytes.
3. `$XDG_STATE_HOME/uniterm/providers.last-good.json`, an atomic envelope retained from the last valid cache.
4. Rules bundled into the current binary.

A higher source replaces the complete provider definition with the same provider id.
Executable alias collisions between different provider ids also resolve by source precedence.
Local and cached data are detection data only and never gain authority to execute a persisted native resume command.

The cache digest detects corruption and incomplete updates.
It does not authenticate a publisher.
A future network updater must add publisher signature verification before it writes this cache, and must not add a periodic runtime fetch.

## Schema version 1

Every file is one document rather than an unversioned array.

```json
{
  "schema_version": 1,
  "manifest_version": "my-team-2026.08.1",
  "providers": [
    {
      "id": "my-agent",
      "executable_aliases": ["my-agent"],
      "capabilities": ["process", "screen", "log"],
      "log_path": "~/.my-agent/events.log",
      "rules": [
        {
          "id": "screen.permission",
          "evidence": "screen",
          "status": "permission",
          "pattern": "approval required",
          "confidence": 95,
          "dwell_ms": 5000
        },
        {
          "id": "log.idle",
          "evidence": "log",
          "status": "idle",
          "pattern": "turn complete",
          "confidence": 98,
          "dwell_ms": 2000
        }
      ]
    }
  ]
}
```

`capabilities` is declarative and must agree with the data in the provider definition.
`process` requires at least one bare executable alias.
`screen` requires at least one screen rule.
`log` requires a log path and at least one log rule.
`connector` must exactly match whether this build backs the provider id with a first-party connector.
This keeps process-only recognition distinct from cooperative connector support.

Rule status values are `starting`, `working`, `tool`, `permission`, `question`, `idle`, `error`, and `exited`.
Confidence is an integer from 1 through 100.
Dwell hints are bounded at 60 seconds and replace the built-in smoothing interval for that winning rule.

## Validation and activation

Run offline validation before installation:

```sh
ut agent manifests validate ./providers.json
```

Validation requires schema version 1, unique bounded provider and rule ids, a maximum 1 MiB document, at most 128 providers, at most 512 rules, and at most 64 executable aliases per provider.
Patterns are literal bounded substrings.
Patterns with fewer than three letters or digits, patterns shorter than four bytes, patterns longer than 512 bytes, control characters, unknown fields, inconsistent capabilities, invalid confidence, and excessive dwell hints are rejected.
A rule may add `region` (`bottom`, the default, or `title`), `anchor` (`anywhere`, the default, `line_start`, or `spinner_line`), and `priority` (default 50; the highest matching rule wins).
Only a `spinner_line` rule may carry an empty pattern, meaning any line that opens with a spinner glyph.
Working and tool rules should be anchored so typed prompt text cannot match them; when no screen rule matches, a known provider is reported idle.

One invalid provider rejects its entire source.
The previous catalog remains active until a complete replacement validates.
An invalid cache falls back to the atomically stored last-known-good envelope, then to bundled rules.

## Reload behavior

The Tokio runtime watches only the local manifest, cache manifest, and cache digest parent directories.
Directory watches preserve atomic rename visibility.
Notification bursts enter a one-item channel, catalog parsing runs in a blocking worker, and a change received during a load schedules exactly one follow-up load.

Successful load replaces the runtime-owned `Arc<Catalog>` and clears foreground-process detection cache entries.
The runtime then sends one typed reload result across the seam.
The mio core resubmits its current bounded pane evidence, so a rule change can reclassify a quiet pane without waiting for PTY output.
There is no polling timer, network fetch, grid access from Tokio, or shared mutable catalog across the seam.

## Explanation contract

`ut agent explain [PANE]` reports the winning authority and evidence plus:

- source and numeric precedence;
- manifest version and matched rule;
- confidence and dwell hint;
- declared process, screen, log, and connector capabilities;
- foreground process group and invocation pid;
- evidence timestamp.

Cooperative OSC 777, launch, terminal activity, and kernel exit observations use direct provenance rather than pretending to come from a manifest.
Manifest ids, versions, rule ids, paths, and patterns reject control characters, and CLI explanation text is sanitized again at the terminal output boundary.

Binary wire protocol version 7 introduced the structured explanation fields, which remain available in current version 13.
