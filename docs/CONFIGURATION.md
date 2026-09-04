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
scrollback-limit = 10000
theme = uniterm-dark       # select any bundled semantic theme
sidebar = true
sidebar-width = 24         # 16..40; responsive at runtime
file-sidebar = true        # legacy name: show the right Observatory rail
file-sidebar-width = 36    # Observatory width, 22..52
notification-delivery = uniterm  # off | uniterm | terminal | system
notify-completion = false  # also notify when an agent becomes idle
notification-sound = bell  # off | bell | chime | file
notification-sound-file =  # audio file used when notification-sound = file
focus-follows-mouse = false
confirm-close = true
restore = true             # resurrect a saved Workspace on start (alias: autosave)
```

Bundled semantic themes are Uniterm dark/light, Catppuccin, Tokyo Night, Dracula, Nord, Gruvbox dark/light, Solarized dark/light, Kanagawa, and Rose Pine.
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
