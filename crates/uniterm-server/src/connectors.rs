//! Agent connectors: install/remove/report the per-provider notify hook that
//! makes an agent announce its lifecycle over OSC 777 (docs/06). A hook prints
//! the envelope to the pane's `/dev/tty`, so the bytes arrive in the PTY
//! stream and the emulator parses them - no polling, no subprocess.
//!
//! The per-agent surfaces are ported from the Tauri app's plugin modules,
//! reduced to what this parser needs (`{agent, event}`; the Tauri scripts also
//! shipped token telemetry we do not consume). Everything agent-specific lives
//! behind this module's dispatch (invariant 8: no agent-id branch anywhere
//! else); the rest of the server only sees [`ConnectorStatus`].
//!
//! - Claude Code: hook groups in `~/.claude/settings.json`.
//! - Codex: hook groups in `~/.codex/hooks.json`, plus `[features] hooks =
//!   true` in `~/.codex/config.toml` (Codex ignores hooks.json without it).
//! - Gemini: hook groups in `~/.gemini/settings.json`.
//! - Grok: a dedicated registration file `~/.grok/hooks/uniterm-notify.json`
//!   (Grok merges every JSON in that directory at startup, so install/remove
//!   never touches shared config).
//! - Kiro: flat hook entries in `~/.kiro/agents/kiro_default.json`.
//! - OpenCode: a TypeScript plugin dropped into
//!   `~/.config/opencode/plugins/` (auto-discovered; no config merge).
//! - Cursor: flat command hooks in `~/.cursor/hooks.json`.
//! - Pi: a TypeScript extension dropped into
//!   `~/.pi/agent/extensions/` (auto-discovered; no config merge).

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use uniterm_proto::ConnectorStatus;

/// The marker every hook command carries, both the envelope URI the parser
/// expects and the tag that lets status/uninstall find exactly our entries.
/// Shared with the launch wrapper (`workflow::announce_wrapped`) so typed and
/// hooked envelopes are byte-identical.
pub(crate) const MARKER: &str = "uniterm://cli-agent";

/// A provider module's toggle entry point: flip toward installed/removed;
/// `None` when the config path is unresolvable (no `$HOME`).
type ToggleFn = fn(bool) -> Option<std::io::Result<()>>;

/// One provider's connector surface: where installed-ness is decided and how
/// to flip it. The single dispatch point - `status` and `toggle` resolve
/// through the same match, so they can never disagree about an agent's files.
struct Connector {
    /// The file whose content decides installed-ness (`None`: no `$HOME`).
    config: Option<PathBuf>,
    toggle: ToggleFn,
}

fn connector(agent: &str) -> Option<Connector> {
    let (config, toggle): (Option<PathBuf>, ToggleFn) = match agent {
        "claude" => (claude::settings_path(), claude::toggle),
        "codex" => (codex::hooks_path(), codex::toggle),
        "cursor" => (cursor::hooks_path(), cursor::toggle),
        "gemini" => (gemini::settings_path(), gemini::toggle),
        "grok" => (grok::registration_path(), grok::toggle),
        "kiro" => (kiro::agent_path(), kiro::toggle),
        "opencode" => (opencode::plugin_path(), opencode::toggle),
        "pi" => (pi::extension_path(), pi::toggle),
        _ => return None,
    };
    Some(Connector { config, toggle })
}

/// Whether this build has a first-party cooperative connector for a provider.
/// Detection manifests use this to distinguish connector-backed providers
/// from process-only recognition without reproducing the provider dispatch.
pub(crate) fn supports(agent: &str) -> bool {
    connector(agent).is_some()
}

/// The connector state for a provider id.
pub fn status(agent: &str) -> ConnectorStatus {
    match connector(agent) {
        None => ConnectorStatus::Unsupported,
        Some(c) => c
            .config
            .map(|p| marker_status(&p))
            .unwrap_or(ConnectorStatus::NotInstalled),
    }
}

/// Flip the connector: install it when absent, remove it when present.
/// Returns the resulting state; I/O failures (including a config file we
/// could not parse) leave the file untouched, and the caller re-reads
/// reality rather than trusting intent.
pub fn toggle(agent: &str) -> ConnectorStatus {
    let Some(c) = connector(agent) else {
        return ConnectorStatus::Unsupported;
    };
    let install = status(agent) != ConnectorStatus::Installed;
    let _ = (c.toggle)(install);
    status(agent)
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Installed = the config file mentions our marker; only entries this module
/// wrote ever carry it, so a plain content probe is exact for every shape.
fn marker_status(path: &Path) -> ConnectorStatus {
    match std::fs::read_to_string(path) {
        Ok(t) if t.contains(MARKER) => ConnectorStatus::Installed,
        _ => ConnectorStatus::NotInstalled,
    }
}

/// A shell `printf` emitting one OSC 777 lifecycle envelope. The one
/// definition of the envelope bytes: connector hooks and launch wrappers
/// (`workflow::announce_wrapped`) both build on it, so the parser sees the
/// same shape from either source.
pub(crate) fn envelope_printf(agent: &str, event: &str) -> String {
    format!(
        "printf '\\033]777;notify;{MARKER};{{\"agent\":\"{agent}\",\"event\":\"{event}\"}}\\007'"
    )
}

/// The inline hook command for one lifecycle event: print the OSC 777
/// envelope to the pane's tty, guarded on `$UNITERM` so agent runs outside a
/// uniterm pane stay silent.
fn hook_command(agent: &str, event: &str) -> String {
    format!(
        "[ -n \"$UNITERM\" ] && {} > /dev/tty || true",
        envelope_printf(agent, event)
    )
}

/// Read a JSON config file. A missing file reads as `{}` (the first install
/// starts from nothing); an unreadable or unparseable one is an error, so a
/// toggle aborts instead of rewriting - and thereby destroying - a file it
/// could not understand.
fn read_json(path: &Path) -> std::io::Result<Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(e) => return Err(e),
    };
    serde_json::from_str(&text).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {e}", path.display()),
        )
    })
}

/// Atomic write (temp + rename), the repo-wide persistence rule.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("uniterm-tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

fn write_json(path: &Path, v: &Value) -> std::io::Result<()> {
    write_atomic(path, &serde_json::to_string_pretty(v).unwrap_or_default())
}

/// The nested hook-group shape shared by Claude Code, Codex, and Gemini:
/// `hooks.<Event> = [{ "hooks": [{ "type": "command", "command": ... }] }]`,
/// merged into a JSON config file without touching anything else in it.
mod nested {
    use super::{hook_command, read_json, write_json, MARKER};
    use serde_json::{json, Value};
    use std::path::Path;

    /// Add our hook group to every listed event, leaving everything else in
    /// the file (and any user hooks) untouched. Idempotent.
    pub(super) fn install(
        path: &Path,
        agent: &str,
        events: &[(&str, &str)],
    ) -> std::io::Result<()> {
        let mut v = read_json(path)?;
        if !v.is_object() {
            v = json!({});
        }
        let hooks = v
            .as_object_mut()
            .expect("settings root is an object")
            .entry("hooks")
            .or_insert_with(|| json!({}));
        if !hooks.is_object() {
            *hooks = json!({});
        }
        let hooks = hooks.as_object_mut().expect("hooks is an object");
        for (name, event) in events {
            let groups = hooks.entry(*name).or_insert_with(|| json!([]));
            if !groups.is_array() {
                *groups = json!([]);
            }
            let arr = groups.as_array_mut().expect("event groups are an array");
            let cmd = hook_command(agent, event);
            let present = arr.iter().any(|g| {
                g.get("hooks").and_then(Value::as_array).is_some_and(|hs| {
                    hs.iter()
                        .any(|h| h.get("command").and_then(Value::as_str) == Some(cmd.as_str()))
                })
            });
            if !present {
                arr.push(json!({ "hooks": [{ "type": "command", "command": cmd }] }));
            }
        }
        write_json(path, &v)
    }

    /// Remove every hook entry carrying our marker; empty structures left
    /// behind are pruned so uninstall leaves no residue.
    pub(super) fn uninstall(path: &Path) -> std::io::Result<()> {
        let mut v = read_json(path)?;
        if let Some(hooks) = v.get_mut("hooks").and_then(Value::as_object_mut) {
            for groups in hooks.values_mut() {
                if let Some(arr) = groups.as_array_mut() {
                    for g in arr.iter_mut() {
                        if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                            hs.retain(|h| {
                                !h.get("command")
                                    .and_then(Value::as_str)
                                    .is_some_and(|c| c.contains(MARKER))
                            });
                        }
                    }
                    arr.retain(|g| {
                        g.get("hooks")
                            .and_then(Value::as_array)
                            .is_none_or(|hs| !hs.is_empty())
                    });
                }
            }
            hooks.retain(|_, groups| groups.as_array().is_none_or(|a| !a.is_empty()));
            if hooks.is_empty() {
                v.as_object_mut().expect("settings root").remove("hooks");
            }
        }
        write_json(path, &v)
    }
}

/// Claude Code: lifecycle hooks in `settings.json` (`~/.claude`, or
/// `$CLAUDE_CONFIG_DIR`).
mod claude {
    use std::path::PathBuf;

    /// Claude Code hook name -> the OSC 777 event it reports (the names
    /// `AgentStatus::from_event` understands).
    pub(super) const EVENTS: &[(&str, &str)] = &[
        ("SessionStart", "session_start"),
        ("UserPromptSubmit", "prompt_submit"),
        ("PreToolUse", "tool_start"),
        ("PostToolUse", "tool_end"),
        ("Notification", "permission_request"),
        ("Stop", "idle"),
        ("SessionEnd", "session_end"),
    ];

    pub(super) fn settings_path() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
            return Some(PathBuf::from(dir).join("settings.json"));
        }
        Some(super::home()?.join(".claude/settings.json"))
    }

    pub(super) fn toggle(install: bool) -> Option<std::io::Result<()>> {
        let p = settings_path()?;
        Some(if install {
            super::nested::install(&p, "claude", EVENTS)
        } else {
            super::nested::uninstall(&p)
        })
    }
}

/// Codex: the same nested shape in `~/.codex/hooks.json` (or `$CODEX_HOME`),
/// but Codex only reads it when `[features] hooks = true` in `config.toml`,
/// so install flips that flag too. Uninstall leaves the flag alone (harmless,
/// and it may not be ours), matching the Tauri app.
mod codex {
    use std::path::{Path, PathBuf};

    pub(super) const EVENTS: &[(&str, &str)] = &[
        ("SessionStart", "session_start"),
        ("UserPromptSubmit", "prompt_submit"),
        ("PreToolUse", "tool_start"),
        ("PermissionRequest", "permission_request"),
        ("PostToolUse", "tool_end"),
        ("Stop", "idle"),
    ];

    fn codex_home() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("CODEX_HOME") {
            return Some(PathBuf::from(dir));
        }
        Some(super::home()?.join(".codex"))
    }

    pub(super) fn hooks_path() -> Option<PathBuf> {
        Some(codex_home()?.join("hooks.json"))
    }

    pub(super) fn toggle(install: bool) -> Option<std::io::Result<()>> {
        let p = hooks_path()?;
        let config = codex_home()?.join("config.toml");
        Some(if install {
            super::nested::install(&p, "codex", EVENTS).and_then(|()| ensure_hooks_flag(&config))
        } else {
            super::nested::uninstall(&p)
        })
    }

    /// Set `hooks = true` under `[features]` in `config.toml`, preserving the
    /// rest of the file byte-for-byte (a line edit, not a re-serialization, so
    /// comments and ordering survive). Creates file/section as needed.
    pub(super) fn ensure_hooks_flag(path: &Path) -> std::io::Result<()> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        let mut in_features = false;
        let mut features_header = None;
        let mut hooks_at = None;
        for (i, l) in lines.iter().enumerate() {
            let t = l.trim();
            if t.starts_with('[') {
                in_features = t == "[features]";
                if in_features && features_header.is_none() {
                    features_header = Some(i);
                }
                continue;
            }
            if in_features && t.split('=').next().map(str::trim) == Some("hooks") {
                hooks_at = Some(i);
            }
        }
        match (hooks_at, features_header) {
            (Some(i), _) => {
                // Read the current value with any inline comment stripped, so
                // `hooks=true` or `hooks = true  # why` count as already right
                // and the file is not rewritten.
                let after_eq = lines[i].split_once('=').map(|x| x.1).unwrap_or("");
                let (value, comment) = match after_eq.find('#') {
                    Some(h) => (after_eq[..h].trim(), Some(after_eq[h..].trim_end())),
                    None => (after_eq.trim(), None),
                };
                if value == "true" {
                    return Ok(());
                }
                // Flip the value in place, keeping indentation and the user's
                // inline comment - a line edit, not a line replacement.
                let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
                lines[i] = match comment {
                    Some(c) => format!("{indent}hooks = true  {c}"),
                    None => format!("{indent}hooks = true"),
                };
            }
            (None, Some(h)) => lines.insert(h + 1, "hooks = true".into()),
            (None, None) => {
                if lines.last().is_some_and(|l| !l.is_empty()) {
                    lines.push(String::new());
                }
                lines.push("[features]".into());
                lines.push("hooks = true".into());
            }
        }
        super::write_atomic(path, &(lines.join("\n") + "\n"))
    }
}

/// Gemini: the nested shape in `~/.gemini/settings.json`. Gemini has no
/// permission hook; permission states come from the fallback detectors.
mod gemini {
    use std::path::PathBuf;

    pub(super) const EVENTS: &[(&str, &str)] = &[
        ("SessionStart", "session_start"),
        ("BeforeAgent", "prompt_submit"),
        ("BeforeTool", "tool_start"),
        ("AfterTool", "tool_end"),
        ("AfterAgent", "idle"),
        ("SessionEnd", "session_end"),
    ];

    pub(super) fn settings_path() -> Option<PathBuf> {
        Some(super::home()?.join(".gemini/settings.json"))
    }

    pub(super) fn toggle(install: bool) -> Option<std::io::Result<()>> {
        let p = settings_path()?;
        Some(if install {
            super::nested::install(&p, "gemini", EVENTS)
        } else {
            super::nested::uninstall(&p)
        })
    }
}

/// Grok: a registration file of our own in `~/.grok/hooks/` (Grok merges
/// every `*.json` there at startup), so install writes one file and uninstall
/// deletes it - no shared-config surgery at all. Entries carry Grok's
/// required `timeout` and sit under a wrapping `hooks` object.
mod grok {
    use serde_json::json;
    use std::path::PathBuf;

    pub(super) const EVENTS: &[(&str, &str)] = &[
        ("SessionStart", "session_start"),
        ("UserPromptSubmit", "prompt_submit"),
        ("PreToolUse", "tool_start"),
        ("PostToolUse", "tool_end"),
        ("PostToolUseFailure", "tool_end"),
        ("Stop", "idle"),
        ("SessionEnd", "session_end"),
        ("Notification", "permission_request"),
    ];

    pub(super) fn registration_path() -> Option<PathBuf> {
        Some(super::home()?.join(".grok/hooks/uniterm-notify.json"))
    }

    pub(super) fn toggle(install: bool) -> Option<std::io::Result<()>> {
        let p = registration_path()?;
        Some(if install {
            let mut hooks = serde_json::Map::new();
            for (name, event) in EVENTS {
                hooks.insert(
                    name.to_string(),
                    json!([{ "hooks": [{
                        "type": "command",
                        "command": super::hook_command("grok", event),
                        "timeout": 5,
                    }] }]),
                );
            }
            super::write_json(&p, &json!({ "hooks": hooks }))
        } else {
            std::fs::remove_file(&p)
        })
    }
}

/// Kiro: flat hook entries (`{command, matcher?}`, no nested wrapper) inside
/// the per-agent file `~/.kiro/agents/kiro_default.json`; a minimal agent
/// file is created when none exists. No permission or session-end hook.
mod kiro {
    use serde_json::{json, Value};
    use std::path::PathBuf;

    /// (hook key, event, wants a `matcher: "*"`). Tool hooks are matched.
    pub(super) const EVENTS: &[(&str, &str, bool)] = &[
        ("agentSpawn", "session_start", false),
        ("userPromptSubmit", "prompt_submit", false),
        ("preToolUse", "tool_start", true),
        ("postToolUse", "tool_end", true),
        ("stop", "idle", false),
    ];

    pub(super) fn agent_path() -> Option<PathBuf> {
        Some(super::home()?.join(".kiro/agents/kiro_default.json"))
    }

    pub(super) fn toggle(install: bool) -> Option<std::io::Result<()>> {
        let p = agent_path()?;
        Some(apply(&p, install))
    }

    fn apply(p: &std::path::Path, install: bool) -> std::io::Result<()> {
        let mut v = super::read_json(p)?;
        if !v.is_object() {
            v = json!({});
        }
        let root = v.as_object_mut().expect("agent root is an object");
        if install {
            if !root.contains_key("name") {
                root.insert("name".into(), json!("kiro_default"));
            }
            let hooks = root.entry("hooks").or_insert_with(|| json!({}));
            if !hooks.is_object() {
                *hooks = json!({});
            }
            let hooks = hooks.as_object_mut().expect("hooks is an object");
            for (name, event, matched) in EVENTS {
                let arr = hooks.entry(*name).or_insert_with(|| json!([]));
                if !arr.is_array() {
                    *arr = json!([]);
                }
                let arr = arr.as_array_mut().expect("hook entries are an array");
                let cmd = super::hook_command("kiro", event);
                if !arr
                    .iter()
                    .any(|e| e.get("command").and_then(Value::as_str) == Some(cmd.as_str()))
                {
                    let mut entry = json!({ "command": cmd });
                    if *matched {
                        entry["matcher"] = json!("*");
                    }
                    arr.push(entry);
                }
            }
        } else if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
            for arr in hooks.values_mut() {
                if let Some(a) = arr.as_array_mut() {
                    a.retain(|e| {
                        !e.get("command")
                            .and_then(Value::as_str)
                            .is_some_and(|c| c.contains(super::MARKER))
                    });
                }
            }
            hooks.retain(|_, a| a.as_array().is_none_or(|a| !a.is_empty()));
            if hooks.is_empty() {
                root.remove("hooks");
            }
        }
        super::write_json(p, &v)
    }
}

/// Cursor CLI: versioned, flat command hook entries in
/// `~/.cursor/hooks.json`. Cursor has no dedicated permission-request event,
/// so approvals remain covered by the provider's grid rules. Tool hooks still
/// provide unambiguous active-tool state without any polling.
mod cursor {
    use serde_json::{json, Value};
    use std::path::PathBuf;

    pub(super) const EVENTS: &[(&str, &str)] = &[
        ("sessionStart", "session_start"),
        ("beforeSubmitPrompt", "prompt_submit"),
        ("preToolUse", "tool_start"),
        ("postToolUse", "tool_end"),
        ("postToolUseFailure", "tool_end"),
        ("stop", "idle"),
        ("sessionEnd", "session_end"),
    ];

    pub(super) fn hooks_path() -> Option<PathBuf> {
        Some(super::home()?.join(".cursor/hooks.json"))
    }

    pub(super) fn toggle(install: bool) -> Option<std::io::Result<()>> {
        let path = hooks_path()?;
        Some(apply(&path, install))
    }

    pub(super) fn apply(path: &std::path::Path, install: bool) -> std::io::Result<()> {
        let mut value = super::read_json(path)?;
        if !value.is_object() {
            value = json!({});
        }
        let root = value.as_object_mut().expect("hooks root is an object");
        if install {
            root.entry("version").or_insert_with(|| json!(1));
            let hooks = root.entry("hooks").or_insert_with(|| json!({}));
            if !hooks.is_object() {
                *hooks = json!({});
            }
            let hooks = hooks.as_object_mut().expect("hooks is an object");
            for (name, event) in EVENTS {
                let entries = hooks.entry(*name).or_insert_with(|| json!([]));
                if !entries.is_array() {
                    *entries = json!([]);
                }
                let entries = entries.as_array_mut().expect("hook entries are an array");
                let command = super::hook_command("cursor", event);
                if !entries.iter().any(|entry| {
                    entry.get("command").and_then(Value::as_str) == Some(command.as_str())
                }) {
                    entries.push(json!({ "command": command }));
                }
            }
        } else if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
            for entries in hooks.values_mut() {
                if let Some(entries) = entries.as_array_mut() {
                    entries.retain(|entry| {
                        !entry
                            .get("command")
                            .and_then(Value::as_str)
                            .is_some_and(|command| command.contains(super::MARKER))
                    });
                }
            }
            hooks.retain(|_, entries| entries.as_array().is_none_or(|entries| !entries.is_empty()));
            if hooks.is_empty() {
                root.remove("hooks");
            }
        }
        super::write_json(path, &value)
    }
}

/// Pi: extensions in `$PI_CODING_AGENT_DIR/extensions/`, which defaults to
/// `~/.pi/agent/extensions/`. Pi auto-discovers these TypeScript modules, so
/// install and uninstall never need to rewrite shared settings.
mod pi {
    use std::path::PathBuf;

    const EXTENSION: &str = r#"// uniterm connector: reports Pi lifecycle over OSC 777.
// Installed by uniterm (Agents > Setup...); safe to delete.
import * as fs from "node:fs"
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent"

const emit = (event: string) => {
  if (!process.env.UNITERM) return
  try {
    fs.writeFileSync(
      "/dev/tty",
      `\x1b]777;notify;uniterm://cli-agent;{"agent":"pi","event":"${event}"}\x07`,
    )
  } catch {}
}

export default function unitermNotify(pi: ExtensionAPI) {
  pi.on("session_start", () => emit("session_start"))
  pi.on("agent_start", () => emit("prompt_submit"))
  pi.on("tool_execution_start", () => emit("tool_start"))
  pi.on("tool_execution_end", () => emit("tool_end"))
  pi.on("agent_settled", () => emit("idle"))
  pi.on("session_shutdown", (event) => {
    if (event.reason === "quit") emit("session_end")
  })
}
"#;

    fn agent_dir() -> Option<PathBuf> {
        let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR") else {
            return Some(super::home()?.join(".pi/agent"));
        };
        let dir = dir.to_string_lossy();
        if dir == "~" {
            return super::home();
        }
        if let Some(rest) = dir.strip_prefix("~/") {
            return Some(super::home()?.join(rest));
        }
        Some(PathBuf::from(dir.as_ref()))
    }

    pub(super) fn extension_path() -> Option<PathBuf> {
        Some(agent_dir()?.join("extensions/uniterm-notify.ts"))
    }

    pub(super) fn toggle(install: bool) -> Option<std::io::Result<()>> {
        let path = extension_path()?;
        Some(if install {
            super::write_atomic(&path, EXTENSION)
        } else {
            std::fs::remove_file(&path)
        })
    }
}

/// OpenCode: hooks are in-process plugins, not shell commands, so this drops
/// a TypeScript module into `~/.config/opencode/plugins/` (OpenCode's
/// xdg-basedir convention on every platform; auto-discovered, no config
/// merge). Uninstall deletes the file.
mod opencode {
    use std::path::PathBuf;

    /// The plugin source. Kept dependency-free and untyped so it loads under
    /// OpenCode's bundled runtime as-is; the marker URI doubles as the
    /// installed-ness probe.
    const PLUGIN: &str = r#"// uniterm connector: reports OpenCode lifecycle over OSC 777.
// Installed by uniterm (Agents > Setup...); safe to delete.
import * as fs from "node:fs"

const emit = (event) => {
  if (!process.env.UNITERM) return
  try {
    fs.writeFileSync(
      "/dev/tty",
      `\x1b]777;notify;uniterm://cli-agent;{"agent":"opencode","event":"${event}"}\x07`,
    )
  } catch {}
}

export const UnitermNotify = async () => {
  emit("session_start")
  let ended = false
  const end = () => {
    if (!ended) {
      ended = true
      emit("session_end")
    }
  }
  // session.deleted does not fire on /exit or ^D; catch the process end too.
  for (const sig of ["exit", "SIGINT", "SIGTERM", "SIGHUP"]) process.on(sig, end)
  return {
    event: async ({ event }) => {
      const t = event?.type
      if (t === "session.status") emit(event.properties?.type === "busy" ? "prompt_submit" : "idle")
      else if (t === "session.idle") emit("idle")
      else if (t === "session.error") emit("error")
      else if (t === "session.deleted") end()
      else if (t === "permission.asked") emit("permission_request")
    },
    "chat.message": async () => emit("prompt_submit"),
    "tool.execute.before": async () => emit("tool_start"),
    "tool.execute.after": async () => emit("tool_end"),
  }
}
"#;

    pub(super) fn plugin_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| Some(super::home()?.join(".config")))?;
        Some(base.join("opencode/plugins/uniterm-notify.ts"))
    }

    pub(super) fn toggle(install: bool) -> Option<std::io::Result<()>> {
        let p = plugin_path()?;
        Some(if install {
            super::write_atomic(&p, PLUGIN)
        } else {
            std::fs::remove_file(&p)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn temp_file(tag: &str, name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("uniterm-conn-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn nested_install_uninstall_round_trip() {
        let path = temp_file("roundtrip", "settings.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(marker_status(&path), ConnectorStatus::NotInstalled);
        nested::install(&path, "claude", claude::EVENTS).unwrap();
        assert_eq!(marker_status(&path), ConnectorStatus::Installed);
        // Idempotent: a second install adds nothing.
        nested::install(&path, "claude", claude::EVENTS).unwrap();
        let v = read(&path);
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "duplicate hook group after reinstall");
        // The envelope carries the marker URI + the event name the parser maps.
        let cmd = stop[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains(MARKER) && cmd.contains("\"event\":\"idle\""));
        nested::uninstall(&path).unwrap();
        assert_eq!(marker_status(&path), ConnectorStatus::NotInstalled);
        // Uninstall leaves no empty hooks residue behind.
        assert!(read(&path).get("hooks").is_none());
    }

    #[test]
    fn nested_preserves_unrelated_settings_and_user_hooks() {
        let path = temp_file("preserve", "settings.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "model": "opus",
                "hooks": {
                    "Stop": [
                        { "hooks": [{ "type": "command", "command": "say done" }] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        nested::install(&path, "claude", claude::EVENTS).unwrap();
        nested::uninstall(&path).unwrap();
        let v = read(&path);
        assert_eq!(v["model"], "opus");
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "say done");
    }

    #[test]
    fn codex_flag_created_updated_and_left_intact() {
        // No file: created with the section.
        let path = temp_file("codex-new", "config.toml");
        let _ = std::fs::remove_file(&path);
        codex::ensure_hooks_flag(&path).unwrap();
        let t = std::fs::read_to_string(&path).unwrap();
        assert!(t.contains("[features]") && t.contains("hooks = true"));
        // Existing content and comments survive; a false flag is flipped.
        let path = temp_file("codex-flip", "config.toml");
        std::fs::write(
            &path,
            "# my config\nmodel = \"o3\"\n\n[features]\n# flag\nhooks = false\n\n[other]\nx = 1\n",
        )
        .unwrap();
        codex::ensure_hooks_flag(&path).unwrap();
        let t = std::fs::read_to_string(&path).unwrap();
        assert!(t.contains("# my config") && t.contains("# flag") && t.contains("x = 1"));
        assert!(t.contains("hooks = true") && !t.contains("hooks = false"));
        // Already true: the file is not rewritten (no spurious churn).
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        codex::ensure_hooks_flag(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
    }

    #[test]
    fn kiro_flat_entries_round_trip() {
        let home = temp_file("kiro", "x").parent().unwrap().join("kiro-home");
        std::fs::create_dir_all(&home).unwrap();
        temp_env(&home, || {
            // Install must create the minimal agent file, flat entries,
            // matchers on tool hooks only.
            assert_eq!(toggle("kiro"), ConnectorStatus::Installed);
            let path = home.join(".kiro/agents/kiro_default.json");
            let v = read(&path);
            assert_eq!(v["name"], "kiro_default");
            let pre = &v["hooks"]["preToolUse"][0];
            assert_eq!(pre["matcher"], "*");
            assert!(pre["command"].as_str().unwrap().contains(MARKER));
            assert!(v["hooks"]["stop"][0].get("matcher").is_none());
            assert!(
                v["hooks"]["stop"][0].get("hooks").is_none(),
                "flat, not nested"
            );
            // Uninstall removes only our entries and prunes empties.
            assert_eq!(toggle("kiro"), ConnectorStatus::NotInstalled);
            assert!(read(&path).get("hooks").is_none());
        });
    }

    #[test]
    fn cursor_flat_hooks_round_trip_and_preserve_user_entries() {
        let path = temp_file("cursor", "hooks.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "version": 1,
                "hooks": {
                    "stop": [{ "command": "notify-send done" }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        cursor::apply(&path, true).unwrap();
        cursor::apply(&path, true).unwrap();
        let value = read(&path);
        assert_eq!(value["version"], 1);
        assert_eq!(value["hooks"]["stop"].as_array().unwrap().len(), 2);
        let pre_tool = &value["hooks"]["preToolUse"][0];
        assert!(pre_tool["command"].as_str().unwrap().contains(MARKER));
        assert!(pre_tool.get("hooks").is_none(), "Cursor hooks are flat");

        cursor::apply(&path, false).unwrap();
        let value = read(&path);
        assert_eq!(value["hooks"]["stop"][0]["command"], "notify-send done");
        assert!(value["hooks"].get("preToolUse").is_none());
        assert_eq!(marker_status(&path), ConnectorStatus::NotInstalled);
    }

    /// Run `f` with `$HOME` temporarily overridden (serialized by a lock so
    /// parallel tests cannot race the process-global env).
    fn temp_env(home: &Path, f: impl FnOnce()) {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap();
        let old = std::env::var_os("HOME");
        std::env::set_var("HOME", home);
        f();
        match old {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn grok_registration_file_round_trip() {
        let home = temp_file("grok", "x").parent().unwrap().join("grok-home");
        std::fs::create_dir_all(&home).unwrap();
        temp_env(&home, || {
            assert_eq!(status("grok"), ConnectorStatus::NotInstalled);
            assert_eq!(toggle("grok"), ConnectorStatus::Installed);
            let reg = home.join(".grok/hooks/uniterm-notify.json");
            let v: Value = serde_json::from_str(&std::fs::read_to_string(&reg).unwrap()).unwrap();
            // Grok entries need the wrapping hooks object + a timeout.
            let entry = &v["hooks"]["Notification"][0]["hooks"][0];
            assert_eq!(entry["timeout"], 5);
            assert!(entry["command"]
                .as_str()
                .unwrap()
                .contains("permission_request"));
            assert_eq!(toggle("grok"), ConnectorStatus::NotInstalled);
            assert!(!reg.exists());
        });
    }

    #[test]
    fn opencode_plugin_file_round_trip() {
        let home = temp_file("oc", "x").parent().unwrap().join("oc-home");
        std::fs::create_dir_all(&home).unwrap();
        temp_env(&home, || {
            // XDG_CONFIG_HOME must not leak in from the host environment.
            let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
            std::env::remove_var("XDG_CONFIG_HOME");
            assert_eq!(toggle("opencode"), ConnectorStatus::Installed);
            let plug = home.join(".config/opencode/plugins/uniterm-notify.ts");
            let t = std::fs::read_to_string(&plug).unwrap();
            assert!(t.contains(MARKER) && t.contains("tool.execute.before"));
            assert_eq!(toggle("opencode"), ConnectorStatus::NotInstalled);
            assert!(!plug.exists());
            if let Some(v) = old_xdg {
                std::env::set_var("XDG_CONFIG_HOME", v);
            }
        });
    }

    #[test]
    fn pi_extension_file_round_trip_and_honors_agent_dir() {
        let home = temp_file("pi", "x").parent().unwrap().join("pi-home");
        let agent_dir = home.join("custom-agent-dir");
        std::fs::create_dir_all(&home).unwrap();
        temp_env(&home, || {
            let old_dir = std::env::var_os("PI_CODING_AGENT_DIR");
            std::env::set_var("PI_CODING_AGENT_DIR", &agent_dir);

            assert_eq!(status("pi"), ConnectorStatus::NotInstalled);
            assert_eq!(toggle("pi"), ConnectorStatus::Installed);
            let extension = agent_dir.join("extensions/uniterm-notify.ts");
            let text = std::fs::read_to_string(&extension).unwrap();
            assert!(text.contains(MARKER));
            assert!(text.contains("agent_settled"));
            assert!(text.contains("tool_execution_start"));
            assert!(text.contains("event.reason === \"quit\""));

            assert_eq!(toggle("pi"), ConnectorStatus::NotInstalled);
            assert!(!extension.exists());
            match old_dir {
                Some(value) => std::env::set_var("PI_CODING_AGENT_DIR", value),
                None => std::env::remove_var("PI_CODING_AGENT_DIR"),
            }
        });
    }

    #[test]
    fn every_registry_provider_has_a_connector() {
        // The Tauri app shipped a connector for every agent; the port must
        // not silently drop one when a provider is added. `connector` is the
        // single dispatch point, so this covers status and toggle alike.
        for p in uniterm_core::agent::PROVIDERS {
            assert!(
                connector(p.id).is_some(),
                "provider {} has no connector arm",
                p.id
            );
        }
        assert_eq!(status("nonsense"), ConnectorStatus::Unsupported);
        assert_eq!(toggle("nonsense"), ConnectorStatus::Unsupported);
    }

    #[test]
    fn unparseable_settings_abort_the_toggle_untouched() {
        // The data-loss guard: a file the JSON parser rejects (trailing
        // comma, comment, corruption) must fail the toggle and keep its
        // bytes, never be replaced with only our hooks.
        let path = temp_file("corrupt", "settings.json");
        let before = "{ \"model\": \"opus\", }"; // trailing comma
        std::fs::write(&path, before).unwrap();
        assert!(nested::install(&path, "claude", claude::EVENTS).is_err());
        assert!(nested::uninstall(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn codex_flag_spellings_and_comments_survive() {
        // `hooks=true` and a commented `hooks = true  # why` are already
        // right: no rewrite (mtime unchanged is asserted by the base test;
        // here content identity is enough).
        for already in [
            "[features]\nhooks=true\n",
            "[features]\nhooks = true # on\n",
        ] {
            let path = temp_file("codex-asis", "config.toml");
            std::fs::write(&path, already).unwrap();
            codex::ensure_hooks_flag(&path).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), already);
        }
        // Flipping a false flag keeps the user's inline comment and indent.
        let path = temp_file("codex-comment", "config.toml");
        std::fs::write(&path, "[features]\n  hooks = false # keep off at work\n").unwrap();
        codex::ensure_hooks_flag(&path).unwrap();
        let t = std::fs::read_to_string(&path).unwrap();
        assert!(
            t.contains("  hooks = true  # keep off at work"),
            "comment/indent lost: {t:?}"
        );
    }
}
