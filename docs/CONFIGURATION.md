# Configuring Uniterm

The config file, the bundled themes, and the provider detection manifests.
The Settings modal (`Ctrl-A g`, see [USAGE.md](USAGE.md)) edits the same file and preserves anything it does not own.

Uniterm reads a Ghostty-style `key = value` config from `~/.config/uniterm/uniterm.conf`.
Unknown keys are ignored, so it is forgiving.

```ini
# ~/.config/uniterm/uniterm.conf
prefix = C-a               # the prefix key (e.g. C-a, C-b)
status = on                # show the status line
status-position = top      # top | bottom
tab-label-alignment = center # left | center | right; main Project Tabs and stack tabs
scrollback-limit = 10000
theme = uniterm-dark       # select any bundled semantic theme
sidebar = true
sidebar-width = 24         # 16..40; responsive at runtime
file-sidebar = true        # legacy name: show the right Observatory rail
file-sidebar-width = 36    # Observatory/Files/Web/Overwatch width, 22..52; drag its left border
notification-delivery = uniterm  # off | uniterm | terminal | system
notify-completion = false  # also notify when an agent becomes idle
notification-sound = bell  # off | bell | chime | file
notification-sound-file =  # audio file used when notification-sound = file
focus-follows-mouse = false
freeze-on-select = false   # hold the screen still while selecting; a drag selects even in apps that take the mouse
copy-on-select = true      # copy a drag selection to the clipboard on release
confirm-close = true
restore = true             # resurrect a saved Workspace on start (alias: autosave)
persist-auto-approve = false # opt in to keeping each Workspace's last ON/OFF choice
```

Bundled semantic themes are Uniterm dark/light, Catppuccin, Tokyo Night, Dracula, Nord, Gruvbox dark/light, Solarized dark/light, Kanagawa, and Rose Pine.
The same Settings control also includes 178 [Omarchy palettes](../themes/omarchy/README.md): all 22 upstream themes, 145 of 146 community gallery themes, and 11 local additions captured on 2026-10-01.
Use `omarchy-*`, `omarchy-community-*`, or `omarchy-local-*` names from the [full catalog](../themes/omarchy/CATALOG.md), for example `theme = omarchy-local-minimal` for the user's Felix-derived Minimal theme.
Gruvu and the local Aether theme are intentionally excluded; theme selection uses bundled data without network access or desktop configuration changes.
Theme roles style the top bar, Tabs, borders, and client dialogs, including a surface-blended secondary accent for buttons that keeps the active Tab and Project visually dominant.
The sidebar and application canvas retain terminal-native colours, and child application colours are never dimmed or recoloured for focus.

Local agent providers can be added or overridden without rebuilding, and a valid atomic replacement hot-reloads on the Tokio runtime:

```json
{
  "schema_version": 1,
  "manifest_version": "my-team-1",
  "providers": [{
    "id": "my-agent",
    "executable_aliases": ["my-agent"],
    "capabilities": ["process", "screen"],
    "rules": [{
      "id": "screen.permission",
      "evidence": "screen",
      "status": "permission",
      "pattern": "approval required",
      "confidence": 95,
      "dwell_ms": 5000
    }]
  }]
}
```

Save that document as `$XDG_CONFIG_HOME/uniterm/providers.json` (normally `~/.config/uniterm/providers.json`).
Validate it offline with `ut agent manifests validate PATH`.
The complete schema, source precedence, verified cache, last-known-good, and reload contract is in [`22-provider-detection-manifests.md`](22-provider-detection-manifests.md).

## Delegated answering

`ut delegate arm PANE` adds per-invocation scope to the existing [Auto-approve pipeline](28-permission-automation.md).
The arm captures this budget at authorization time:

```ini
delegation-max-answers-per-hour = 30 # 1..240, default 30
```

Only accepted submissions count; navigation and failed writes do not.
The final allowed submission revokes the arm, and explicit re-arm is required to resume it.
Workspace Auto-approve does not bypass a Pane arm's budget or an explicit Pane disarm.
Selecting Auto-approve **off** clears all arms, including when the Workspace switch was already off.
Pane arms never persist across restart.
Workspace mode resets unless the explicit Keep auto-approve on restart preference is enabled.
The unreleased `delegation-auto-arm` and retired `delegation-allow` keys are diagnosed as unknown keys and cannot enable automation.

### Graph

`run-graph = false` keeps the native agent and run graph disabled by default.
Ordinary agents and recorded native children appear without configuring a workflow or relay.
Enable it in Workspace Settings or set `run-graph = true` to use Today > Graph, prefix-G, or `ut run graph`.
The surface refreshes on real Workspace changes and loads native agent history for the selected day.
Run-attempt and artifact evidence is queried when its inspector is requested.

### Project Tab label alignment

Choose **Left**, **Center**, or **Right** under **Settings > Appearance > Tab label alignment**.
`tab-label-alignment = center` is the default for the main center-area Project Tabs and their stack/sub-tabs.
The choice is saved in the same config file and applies immediately to attached clients.
Labels use terminal display-cell widths, preserve whole graphemes, and end clipped text with an ellipsis.
Tab sizes, click targets, and plus buttons stay fixed.
Local/Servers, Observatory and Today selector labels, and the Workspace button keep their existing presentation.

### Tab names and Find Tabs and Agents

`auto-tab-names = false` keeps automatic naming off by default.
Enable **Auto-name tabs** under Settings > Behaviour to follow an agent's observed task summary, or the directory and foreground command of a regular terminal.
A custom Tab name always takes precedence; clearing it resumes automatic naming.
`switcher-enabled = false` independently keeps the Find Tabs and Agents view off.
Enable **Find Tabs and Agents** under Settings > Behaviour, then open Workspace > Find Tabs and Agents, prefix-S, or `ut switcher`.
The configurable binding action is `switcher`.
Neither option starts a periodic process or filesystem scan.

## Auto-approve restart preference

**Workspace > Settings > Agents > Keep auto-approve on restart** saves `persist-auto-approve` in the machine configuration.
It is off by default, so each Workspace resets Auto-approve to OFF when its server restarts.
With this preference on, each Workspace restores its own last ON or OFF choice from its durable event log, including clean restarts, crashes, and history compaction.
ON stays on until explicitly turned off; OFF stays off.
Changing this preference does not enable or disable the current mode.
Turning the preference off restores the default OFF behavior at the next restart.
It is independent of `restore`, which controls terminal session recovery.
The preference is machine-wide; authorization is Workspace-specific and is never copied to a new or different Workspace.
`ut agent auto-approve status` reports both the current Workspace mode and restart behavior.

### Automatic direct Tasks

`auto-track-tasks = true` enables capture of accepted direct agent prompts in Tasks; it is ON by default and appears under Settings > Agents.
Follow-ups join unfinished work, and explicit response-end observations finish a Task without asserting verified success.
Idle never finishes work; manual status edits take precedence.
Prompt capture is bounded and uses the existing Workspace event log.
Turn this off to stop capture and automatic status changes while keeping existing records.
See [direct work in Tasks](31-workspace-navigation-and-overwatch.md#direct-work-in-tasks) for discovery and retention limits.
