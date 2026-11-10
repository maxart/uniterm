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
    /// The nested hook entries this build writes, for connectors whose
    /// installed entries can be compared (`Outdated` detection).
    expected: Option<fn() -> Vec<HookEntry>>,
}

fn connector(agent: &str) -> Option<Connector> {
    let expected: Option<fn() -> Vec<HookEntry>> = match agent {
        "claude" => Some(claude::entries),
        "codex" => Some(codex::entries),
        "gemini" => Some(gemini::entries),
        "grok" => Some(grok::entries),
        _ => None,
    };
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
    Some(Connector {
        config,
        toggle,
        expected,
    })
}

/// Whether this build has a first-party cooperative connector for a provider.
/// Detection manifests use this to distinguish connector-backed providers
/// from process-only recognition without reproducing the provider dispatch.
pub(crate) fn supports(agent: &str) -> bool {
    connector(agent).is_some()
}

/// The connector state for a provider id, read from disk. A marked file
/// whose entries differ from what this build writes is `Outdated`.
pub fn status(agent: &str) -> ConnectorStatus {
    if agent == "opencode" {
        return opencode::status();
    }
    match connector(agent) {
        None => ConnectorStatus::Unsupported,
        Some(c) => {
            let Some(path) = c.config else {
                return ConnectorStatus::NotInstalled;
            };
            let status = match agent {
                "cursor" => flat_status(&path, &cursor::entries()),
                "kiro" => flat_status(&path, &kiro::entries()),
                "pi" => match std::fs::read_to_string(&path) {
                    Ok(text) if text == pi::EXTENSION => ConnectorStatus::Installed,
                    Ok(text) if text.contains(MARKER) => ConnectorStatus::Outdated,
                    _ => ConnectorStatus::NotInstalled,
                },
                _ => path_status(&path, c.expected.map(|expected| expected()).as_deref()),
            };
            if agent == "kiro" {
                if status == ConnectorStatus::Installed && kiro::current_global_hooks() {
                    status
                } else if status == ConnectorStatus::Installed || kiro::has_global_hooks() {
                    ConnectorStatus::Outdated
                } else {
                    status
                }
            } else {
                status
            }
        }
    }
}

/// Installed-ness of one config file, compared with the entries this build
/// writes when the connector's shape allows it.
fn path_status(path: &Path, expected: Option<&[HookEntry]>) -> ConnectorStatus {
    match (marker_status(path), expected) {
        (ConnectorStatus::Installed, Some(expected))
            if nested::marked_entries(path) != entry_set(expected) =>
        {
            ConnectorStatus::Outdated
        }
        (status, _) => status,
    }
}

/// Flip the connector: install it when absent or outdated, remove it when
/// current. Returns the resulting state; I/O failures (including a config
/// file we could not parse) leave the file untouched, and the caller re-reads
/// reality rather than trusting intent.
pub fn toggle(agent: &str) -> ConnectorStatus {
    if status(agent) == ConnectorStatus::Installed {
        remove(agent).1
    } else {
        install(agent).1
    }
}

/// Install the connector, or upgrade one this build considers outdated by
/// rewriting only the entries that carry our marker. Automatic refresh is
/// controlled through `auto-update-connectors`. The returned status
/// is re-read from disk, so it reports what actually happened.
pub fn install(agent: &str) -> (std::io::Result<()>, ConnectorStatus) {
    let _lock = match connector_lock() {
        Ok(lock) => lock,
        Err(error) => return (Err(error), status(agent)),
    };
    install_unlocked(agent)
}

fn install_unlocked(agent: &str) -> (std::io::Result<()>, ConnectorStatus) {
    let Some(c) = connector(agent) else {
        return (Ok(()), ConnectorStatus::Unsupported);
    };
    // Each provider merges its managed entries atomically and completes any
    // prerequisite config write before making the connector look current.
    let result = (c.toggle)(true).unwrap_or_else(|| Err(no_home()));
    (result, status(agent))
}

/// Remove every entry carrying our marker. The status is re-read from disk.
pub fn remove(agent: &str) -> (std::io::Result<()>, ConnectorStatus) {
    let _lock = match connector_lock() {
        Ok(lock) => lock,
        Err(error) => return (Err(error), status(agent)),
    };
    let Some(c) = connector(agent) else {
        return (Ok(()), ConnectorStatus::Unsupported);
    };
    if status(agent) == ConnectorStatus::NotInstalled {
        return (Ok(()), ConnectorStatus::NotInstalled);
    }
    let result = match (c.toggle)(false) {
        Some(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Some(result) => result,
        None => Err(no_home()),
    };
    (result, status(agent))
}

/// Apply independent install and update policies for locally available providers.
/// Runs once outside the core loop, with no work when both policies are off.
/// Current connectors cause no writes; failures are retried on the next
/// invocation because status always comes from disk.
pub fn auto_update(update: bool, install: bool) -> std::io::Result<()> {
    if !update && !install {
        return Ok(());
    }
    let _lock = match connector_lock() {
        Ok(lock) => lock,
        // Another client or installer owns the same machine-wide refresh.
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut errors = Vec::new();
    for id in crate::workflow::installed_agents() {
        match status(&id) {
            ConnectorStatus::Outdated if update => {}
            ConnectorStatus::NotInstalled if install => {}
            _ => continue,
        }
        match install_unlocked(&id) {
            (Ok(()), ConnectorStatus::Installed) => {}
            (Err(error), _) => errors.push(format!("{id}: {error}")),
            (Ok(()), _) => errors.push(format!("{id}: connector is not current")),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(std::io::Error::other(errors.join("; ")))
    }
}

// All connector writers share a short-lived, machine-scoped advisory lock.
// Explicitly unlocking on drop releases it even if a concurrently forked child
// inherited the open file description before exec. Never wait on a server's
// render loop, and never leave a timestamp claiming an unsuccessful upgrade.
struct ConnectorLock(std::fs::File);

impl Drop for ConnectorLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: the owned descriptor is valid until the field is dropped.
        // Closing alone can leave the flock held by a child awaiting exec.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn connector_lock() -> std::io::Result<ConnectorLock> {
    let path = crate::server::config_path()
        .ok_or_else(no_home)?
        .with_file_name("connectors.lock");
    connector_lock_at(&path)
}

fn connector_lock_at(path: &Path) -> std::io::Result<ConnectorLock> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(path.parent().expect("config has a parent"))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    // SAFETY: the owned file descriptor remains valid for the lifetime of the lock.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(ConnectorLock(lock))
}

fn flat_status(path: &Path, expected: &[HookEntry]) -> ConnectorStatus {
    if marker_status(path) != ConnectorStatus::Installed {
        return ConnectorStatus::NotInstalled;
    }
    let mut entries = std::collections::BTreeSet::new();
    if let Ok(value) = read_json(path) {
        if let Some(hooks) = value.get("hooks").and_then(Value::as_object) {
            for (event, list) in hooks {
                for entry in list.as_array().into_iter().flatten() {
                    if let Some(command) = entry
                        .get("command")
                        .and_then(Value::as_str)
                        .filter(|command| command.contains(MARKER))
                    {
                        entries.insert(HookEntry {
                            event: event.clone(),
                            matcher: entry
                                .get("matcher")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            command: command.into(),
                        });
                    }
                }
            }
        }
    }
    if entries == entry_set(expected) {
        ConnectorStatus::Installed
    } else {
        ConnectorStatus::Outdated
    }
}

// Strip only managed flat hooks before replacing the current event set. Old
// event names must disappear too; unrelated user hooks retain their order.
fn strip_flat_hooks(root: &mut serde_json::Map<String, Value>) {
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        for list in hooks.values_mut() {
            if let Some(entries) = list.as_array_mut() {
                entries.retain(|entry| {
                    !entry
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|command| command.contains(MARKER))
                });
            }
        }
        hooks.retain(|_, list| list.as_array().is_none_or(|entries| !entries.is_empty()));
    }
}

fn no_home() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "the connector config path is unresolvable (no $HOME)",
    )
}

/// Translate one provider hook invocation (its JSON on stdin) into a single
/// bounded OSC 777 envelope. `None` means the input was not understood or
/// carried nothing reportable: the caller stays silent, and a hook never
/// synthesizes a status it did not observe. See `ut agent hook`.
pub fn hook_envelope(agent: &str, input: &[u8]) -> Option<String> {
    // Compatibility payloads must use their host's semantics, including
    // notification types and child lifecycle. Older installed hook commands
    // can still invoke the helper with the imported vendor's name.
    let agent = if grok::owns_hook(input) {
        "grok"
    } else {
        agent
    };
    let fields = match agent {
        "claude" => claude::hook_fields(input)?,
        "codex" => codex::hook_fields(input)?,
        "gemini" => gemini::hook_fields(input)?,
        "grok" => grok::hook_fields(input)?,
        "cursor" => cursor::hook_fields(input)?,
        "kiro" => kiro::hook_fields(input)?,
        _ => return None,
    };
    Some(envelope(agent, fields))
}

/// Preserve provider session identity on every supported lifecycle hook,
/// including the first prompt after an integration was installed mid-session.
fn lifecycle_fields(
    input: &[u8],
    events: &[(&str, &str)],
) -> Option<serde_json::Map<String, Value>> {
    let hook: Value = serde_json::from_slice(input).ok()?;
    let fallback = std::env::var("UNITERM_HOOK_EVENT").ok();
    let name = hook
        .get("hook_event_name")
        .or_else(|| hook.get("event"))
        .and_then(Value::as_str)
        .or(fallback.as_deref())?;
    let event = events
        .iter()
        .find(|(key, event)| *key == name || *event == name)?
        .1;
    let mut fields = serde_json::Map::new();
    fields.insert("event".into(), Value::String(event.into()));
    for (target, aliases) in [
        (
            "session_id",
            &["session_id", "sessionId", "conversation_id"][..],
        ),
        (
            "transcript_path",
            &["transcript_path", "transcriptPath"][..],
        ),
        (
            "parent_session_id",
            &[
                "parent_session_id",
                "parent_thread_id",
                "parent_conversation_id",
            ][..],
        ),
    ] {
        if let Some(value) = aliases
            .iter()
            .find_map(|key| hook.get(*key).and_then(Value::as_str))
        {
            let limit = if target == "transcript_path" {
                4096
            } else {
                512
            };
            if !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control) {
                fields.insert(target.into(), Value::String(value.into()));
            }
        }
    }
    Some(fields)
}

fn lifecycle_entries(agent: &str, events: &[(&str, &str)]) -> Vec<HookEntry> {
    events
        .iter()
        .map(|(name, event)| HookEntry {
            event: (*name).into(),
            matcher: None,
            command: helper_command(agent, Some(event)),
        })
        .collect()
}

/// Encode an envelope whose JSON can pass any terminal byte filter: every
/// `;` (which vte would split into more than its sixteen OSC parameters)
/// and every non-ASCII or control character becomes a JSON `\u` escape.
/// The parser decodes the same JSON, so the round trip is exact.
fn envelope(agent: &str, mut fields: serde_json::Map<String, Value>) -> String {
    fields.insert("agent".into(), Value::String(agent.into()));
    let json = serde_json::to_string(&Value::Object(fields)).unwrap_or_default();
    let mut escaped = String::with_capacity(json.len());
    for c in json.chars() {
        if c == ';' || !c.is_ascii() || c.is_ascii_control() {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                escaped.push_str(&format!("\\u{unit:04x}"));
            }
        } else {
            escaped.push(c);
        }
    }
    format!("\x1b]777;notify;{MARKER};{escaped}\x07")
}

/// One nested hook group entry: the event key, an optional tool or type
/// matcher, and the command.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct HookEntry {
    event: String,
    matcher: Option<String>,
    command: String,
}

fn entry_set(entries: &[HookEntry]) -> std::collections::BTreeSet<HookEntry> {
    entries.iter().cloned().collect()
}

#[cfg(test)]
fn printf_entries(agent: &str, events: &[(&str, &str)]) -> Vec<HookEntry> {
    events
        .iter()
        .map(|(name, event)| HookEntry {
            event: (*name).into(),
            matcher: None,
            command: hook_command(agent, event),
        })
        .collect()
}

/// A hook that hands its stdin to `ut agent hook AGENT`, which writes the
/// enriched envelope to the tty itself. The binary is the one that started
/// this Pane's server (`UNITERM_BIN`), else `ut` on `PATH`. When the helper is
/// missing or fails, `fallback` (a plain envelope) is printed instead; with no
/// fallback the hook stays silent, so a malformed or unknown input can never
/// fabricate a permission prompt.
fn helper_command(agent: &str, fallback: Option<&str>) -> String {
    let event_environment = fallback
        .map(|event| format!("UNITERM_HOOK_EVENT={event} "))
        .unwrap_or_default();
    let fallback = fallback.map_or_else(
        || "true".to_string(),
        |event| format!("{} > /dev/tty", envelope_printf(agent, event)),
    );
    // Grok imports Claude and Cursor hook files by default. Only its own
    // connector may report those invocations, or one process gets two ids.
    let guard = if matches!(agent, "claude" | "cursor") {
        "[ -z \"$GROK_HOOK_EVENT\" ] && "
    } else {
        ""
    };
    format!(
        "{guard}[ -n \"$UNITERM\" ] && {{ {event_environment}\"${{UNITERM_BIN:-ut}}\" agent hook {agent} >/dev/null 2>&1 || {fallback}; }} || true # {MARKER}"
    )
}

/// Provider-owned discovery output uses the hook's documented context channel.
/// No messages or credentials are embedded, and unsupported providers stay silent.
pub fn inbox_context(agent: &str, input: &[u8]) -> Option<Value> {
    let input: Value = serde_json::from_slice(input).ok()?;
    if input.get("hook_event_name")?.as_str()? != "SessionStart" {
        return None;
    }
    match agent {
        "claude" | "gemini" => Some(
            serde_json::json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": crate::runtime::inbox::GUIDANCE}}),
        ),
        _ => None,
    }
}

fn inbox_context_entry(agent: &str) -> HookEntry {
    HookEntry { event: "SessionStart".into(), matcher: None, command: format!("[ -n \"$UNITERM\" ] && \"${{UNITERM_BIN:-ut}}\" agent inbox-context {agent} 2>/dev/null || true # {MARKER}") }
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
#[cfg(test)]
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
    use super::{read_json, write_json, HookEntry, MARKER};
    use serde_json::{json, Value};
    use std::collections::BTreeSet;
    use std::path::Path;

    /// Add our hook groups, leaving everything else in the file (and any
    /// user hooks) untouched. Idempotent: a group with the same matcher and
    /// command is never added twice.
    #[cfg(test)]
    pub(super) fn install(path: &Path, entries: &[HookEntry]) -> std::io::Result<()> {
        let mut v = read_json(path)?;
        add_entries(&mut v, entries);
        write_json(path, &v)
    }

    /// Replace every entry carrying our marker with `entries` in one atomic
    /// write, so a failed upgrade leaves the previous connector intact.
    pub(super) fn upgrade(path: &Path, entries: &[HookEntry]) -> std::io::Result<()> {
        let mut v = read_json(path)?;
        strip_marked(&mut v);
        add_entries(&mut v, entries);
        write_json(path, &v)
    }

    pub(super) fn add_entries(v: &mut Value, entries: &[HookEntry]) {
        if !v.is_object() {
            *v = json!({});
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
        for entry in entries {
            let groups = hooks
                .entry(entry.event.as_str())
                .or_insert_with(|| json!([]));
            if !groups.is_array() {
                *groups = json!([]);
            }
            let arr = groups.as_array_mut().expect("event groups are an array");
            let present = arr.iter().any(|g| {
                g.get("matcher").and_then(Value::as_str) == entry.matcher.as_deref()
                    && g.get("hooks").and_then(Value::as_array).is_some_and(|hs| {
                        hs.iter().any(|h| {
                            h.get("command").and_then(Value::as_str) == Some(entry.command.as_str())
                        })
                    })
            });
            if !present {
                let mut group = json!({
                    "hooks": [{ "type": "command", "command": entry.command }]
                });
                if let Some(matcher) = &entry.matcher {
                    group["matcher"] = Value::String(matcher.clone());
                }
                arr.push(group);
            }
        }
    }

    /// Every entry in the file that carries our marker, for comparing an
    /// installed connector with what this build would write.
    pub(super) fn marked_entries(path: &Path) -> BTreeSet<HookEntry> {
        let mut out = BTreeSet::new();
        let Ok(v) = read_json(path) else {
            return out;
        };
        let Some(hooks) = v.get("hooks").and_then(Value::as_object) else {
            return out;
        };
        for (event, groups) in hooks {
            for group in groups.as_array().into_iter().flatten() {
                let matcher = group
                    .get("matcher")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let commands = group.get("hooks").and_then(Value::as_array);
                for hook in commands.into_iter().flatten() {
                    if let Some(command) = hook
                        .get("command")
                        .and_then(Value::as_str)
                        .filter(|command| command.contains(MARKER))
                    {
                        out.insert(HookEntry {
                            event: event.clone(),
                            matcher: matcher.clone(),
                            command: command.to_string(),
                        });
                    }
                }
            }
        }
        out
    }

    /// Remove every hook entry carrying our marker; empty structures left
    /// behind are pruned so uninstall leaves no residue.
    pub(super) fn uninstall(path: &Path) -> std::io::Result<()> {
        let mut v = read_json(path)?;
        strip_marked(&mut v);
        write_json(path, &v)
    }

    pub(super) fn strip_marked(v: &mut Value) {
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
    }
}

/// Claude Code: lifecycle hooks in `settings.json` (`~/.claude`, or
/// `$CLAUDE_CONFIG_DIR`).
///
/// Every hook runs `ut agent hook claude` without a fixed fallback so child
/// lifecycle events can be ignored. Hook details (identity, notification
/// type, permission tool, and loop wakeup) are translated in
/// [`hook_fields`]. A Claude `Notification` is a permission prompt only when
/// its `notification_type` says so; every other type, including the idle
/// reminder and `PushNotification`, is an event that leaves status alone.
mod claude {
    use super::HookEntry;
    use serde_json::{Map, Value};
    use std::path::PathBuf;
    use uniterm_core::agent_detail::{bounded_text, DETAIL_ID_LIMIT, DETAIL_TEXT_LIMIT};

    /// Claude Code hook name -> the OSC 777 event a plain hook reports (the
    /// names `AgentStatus::from_event` understands).
    pub(super) const EVENTS: &[(&str, &str)] = &[
        ("UserPromptSubmit", "prompt_submit"),
        ("PreToolUse", "tool_start"),
        ("PostToolUse", "tool_end"),
        ("PostToolUseFailure", "tool_end"),
        ("Stop", "idle"),
        ("StopFailure", "error"),
        ("SessionEnd", "session_end"),
    ];

    /// Hooks routed through the stdin helper: (hook name, matcher, plain
    /// fallback). No fixed fallback may undo a deliberate child-event filter.
    const HELPER_EVENTS: &[(&str, Option<&str>, Option<&str>)] = &[
        ("SessionStart", None, None),
        ("Notification", None, None),
        ("PermissionRequest", None, None),
        ("Elicitation", None, None),
        ("ElicitationResult", None, None),
        ("PostToolUse", Some("ScheduleWakeup"), None),
    ];

    /// Every entry this build writes, in install order.
    pub(super) fn entries() -> Vec<HookEntry> {
        let mut entries: Vec<_> = EVENTS
            .iter()
            .map(|(event, _)| HookEntry {
                event: (*event).into(),
                matcher: None,
                command: super::helper_command("claude", None),
            })
            .collect();
        entries.extend(
            HELPER_EVENTS
                .iter()
                .map(|(event, matcher, fallback)| HookEntry {
                    event: (*event).into(),
                    matcher: matcher.map(str::to_string),
                    command: super::helper_command("claude", *fallback),
                }),
        );
        entries.push(super::inbox_context_entry("claude"));
        entries
    }

    pub(super) fn settings_path() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
            return Some(PathBuf::from(dir).join("settings.json"));
        }
        Some(super::home()?.join(".claude/settings.json"))
    }

    pub(super) fn toggle(install: bool) -> Option<std::io::Result<()>> {
        let p = settings_path()?;
        Some(if install {
            super::nested::upgrade(&p, &entries())
        } else {
            super::nested::uninstall(&p)
        })
    }

    fn text(value: &Value, key: &str, limit: usize) -> Option<Value> {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| Value::String(bounded_text(text, limit)))
    }

    /// The most identifying argument of a tool call, for a one-line preview:
    /// a shell command, a path, a URL, or a pattern, else the compact input.
    fn preview(input: &Value) -> String {
        for key in [
            "command",
            "file_path",
            "notebook_path",
            "path",
            "url",
            "pattern",
            "query",
            "description",
        ] {
            if let Some(text) = input.get(key).and_then(Value::as_str) {
                return bounded_text(text, DETAIL_TEXT_LIMIT);
            }
        }
        match input {
            Value::Null => String::new(),
            other => {
                let compact = other.to_string();
                bounded_text(&compact, DETAIL_TEXT_LIMIT)
            }
        }
    }

    /// Translate one hook's stdin JSON into envelope fields, or `None` when
    /// it is malformed or carries nothing reportable.
    pub(super) fn hook_fields(input: &[u8]) -> Option<Map<String, Value>> {
        let hook: Value = serde_json::from_slice(input).ok()?;
        let mut out = Map::new();
        let event = match hook.get("hook_event_name").and_then(Value::as_str)? {
            "SessionStart" => "session_start",
            "Notification" => {
                let kind = hook
                    .get("notification_type")
                    .and_then(Value::as_str)
                    .filter(|kind| !kind.is_empty());
                if let Some(kind) = kind {
                    out.insert(
                        "notification_kind".into(),
                        Value::String(bounded_text(kind, DETAIL_TEXT_LIMIT)),
                    );
                }
                if let Some(message) = text(&hook, "message", DETAIL_TEXT_LIMIT) {
                    out.insert("message".into(), message);
                }
                // The provider's own type decides; free text never does.
                match kind {
                    Some("permission_prompt") => "permission_request",
                    Some("elicitation_dialog") => "question",
                    _ => "notification",
                }
            }
            "Elicitation" => "question",
            "ElicitationResult" => "tool_end",
            "PreToolUse"
                if hook.get("tool_name").and_then(Value::as_str) == Some("AskUserQuestion") =>
            {
                "question"
            }
            "PermissionRequest" => {
                let tool = text(&hook, "tool_name", DETAIL_TEXT_LIMIT)?;
                out.insert("tool".into(), tool);
                let input = hook.get("tool_input").unwrap_or(&Value::Null);
                out.insert("preview".into(), Value::String(preview(input)));
                "permission_request"
            }
            // PostToolUse fires only after the tool completed; a failed call
            // reports PostToolUseFailure, which never reaches this branch.
            "PostToolUse" => {
                if hook.get("tool_name").and_then(Value::as_str) != Some("ScheduleWakeup") {
                    if hook
                        .get("agent_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| !id.is_empty())
                    {
                        return None;
                    }
                    return super::lifecycle_fields(input, EVENTS);
                }
                let input = hook.get("tool_input")?;
                if input.get("stop").and_then(Value::as_bool) == Some(true) {
                    out.insert("loop_state".into(), Value::String("stopped".into()));
                } else {
                    let delay = input
                        .get("delaySeconds")
                        .and_then(Value::as_u64)
                        .and_then(|delay| u32::try_from(delay).ok())?;
                    out.insert("loop_state".into(), Value::String("scheduled".into()));
                    out.insert("delay_seconds".into(), Value::from(delay));
                }
                "loop"
            }
            name => EVENTS.iter().find(|(key, _)| *key == name)?.1,
        };
        // Child tools share the parent's terminal/session id. Surface their
        // human blockers, but never let child activity or Stop finish the root.
        if hook
            .get("agent_id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
            && !matches!(event, "permission_request" | "question")
        {
            return None;
        }
        out.insert("event".into(), Value::String(event.into()));
        for key in ["session_id", "transcript_path"] {
            if let Some(value) = text(&hook, key, DETAIL_ID_LIMIT) {
                out.insert(key.into(), value);
            }
        }
        Some(out)
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
        // This runs before native auto-review, so it does not prove that
        // a human is needed. The visible approval UI supplies that verdict.
        ("PermissionRequest", "notification"),
        ("PostToolUse", "tool_end"),
        ("Stop", "idle"),
        ("Interrupt", "idle"),
    ];

    pub(super) fn entries() -> Vec<super::HookEntry> {
        super::lifecycle_entries("codex", EVENTS)
    }

    pub(super) fn hook_fields(input: &[u8]) -> Option<serde_json::Map<String, serde_json::Value>> {
        let mut fields = super::lifecycle_fields(input, EVENTS)?;
        // A nested Codex inherits its parent's thread identity. Preserve that
        // edge, but never let its hooks change the owning Pane's lifecycle.
        if let Ok(parent) = std::env::var("CODEX_THREAD_ID") {
            if fields
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| id != parent)
                && !parent.is_empty()
                && parent.len() <= 512
            {
                fields.insert(
                    "parent_session_id".into(),
                    serde_json::Value::String(parent),
                );
            }
        }
        Some(fields)
    }

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
            ensure_hooks_flag(&config).and_then(|()| super::nested::upgrade(&p, &entries()))
        } else {
            super::nested::uninstall(&p)
        })
    }

    /// Set `hooks = true` under `[features]` in `config.toml`, preserving the
    /// rest of the file byte-for-byte (a line edit, not a re-serialization, so
    /// comments and ordering survive). Creates file/section as needed.
    pub(super) fn ensure_hooks_flag(path: &Path) -> std::io::Result<()> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error),
        };
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

/// Gemini: lifecycle and typed ToolPermission alerts in its nested hooks.
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

    pub(super) fn entries() -> Vec<super::HookEntry> {
        let mut entries = super::lifecycle_entries("gemini", EVENTS);
        entries.push(super::HookEntry {
            event: "Notification".into(),
            matcher: Some("ToolPermission".into()),
            command: super::helper_command("gemini", None),
        });
        entries.push(super::inbox_context_entry("gemini"));
        entries
    }

    pub(super) fn hook_fields(input: &[u8]) -> Option<serde_json::Map<String, serde_json::Value>> {
        let hook: serde_json::Value = serde_json::from_slice(input).ok()?;
        if hook
            .get("hook_event_name")
            .and_then(serde_json::Value::as_str)
            == Some("Notification")
        {
            if hook
                .get("notification_type")
                .and_then(serde_json::Value::as_str)
                != Some("ToolPermission")
            {
                return None;
            }
            return super::lifecycle_fields(input, &[("Notification", "permission_request")]);
        }
        super::lifecycle_fields(input, EVENTS)
    }

    pub(super) fn settings_path() -> Option<PathBuf> {
        Some(super::home()?.join(".gemini/settings.json"))
    }

    pub(super) fn toggle(install: bool) -> Option<std::io::Result<()>> {
        let p = settings_path()?;
        Some(if install {
            super::nested::upgrade(&p, &entries())
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
    use super::HookEntry;
    use serde_json::{json, Value};
    use std::path::PathBuf;

    pub(super) const EVENTS: &[(&str, &str)] = &[
        ("SessionStart", "session_start"),
        ("UserPromptSubmit", "prompt_submit"),
        ("PreToolUse", "tool_start"),
        ("PostToolUse", "tool_end"),
        ("PostToolUseFailure", "tool_end"),
        ("Stop", "idle"),
        ("StopCancelled", "idle"),
        ("StopFailure", "error"),
        ("SessionEnd", "session_end"),
        ("Notification", "notification"),
    ];

    pub(super) fn entries() -> Vec<HookEntry> {
        EVENTS
            .iter()
            .map(|(event, _)| HookEntry {
                event: (*event).into(),
                matcher: None,
                command: super::helper_command("grok", None),
            })
            .collect()
    }

    pub(super) fn owns_hook(input: &[u8]) -> bool {
        let Ok(hook) = serde_json::from_slice::<Value>(input) else {
            return false;
        };
        // Grok adds its native snake_case event alongside the Claude alias.
        hook.get("hookEventName")
            .and_then(Value::as_str)
            .is_some_and(|event| event.contains('_') || matches!(event, "stop" | "notification"))
            && hook.get("sessionId").and_then(Value::as_str).is_some()
    }

    pub(super) fn hook_fields(input: &[u8]) -> Option<serde_json::Map<String, Value>> {
        let hook: Value = serde_json::from_slice(input).ok()?;
        let mut fields = super::lifecycle_fields(input, EVENTS)?;
        if fields.get("event").and_then(Value::as_str) == Some("notification") {
            let kind = hook
                .get("notificationType")
                .or_else(|| hook.get("notification_type"))
                .and_then(Value::as_str);
            if kind == Some("permission_prompt") {
                fields.insert("event".into(), json!("permission_request"));
            }
        }
        // Child teardown shares the terminal and must not end its owner.
        if hook
            .get("subagentType")
            .or_else(|| hook.get("subagent_type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| !kind.is_empty())
        {
            return None;
        }
        Some(fields)
    }

    pub(super) fn registration_path() -> Option<PathBuf> {
        Some(
            std::env::var_os("GROK_HOME")
                .map(PathBuf::from)
                .or_else(|| Some(super::home()?.join(".grok")))?
                .join("hooks/uniterm-notify.json"),
        )
    }

    pub(super) fn toggle(install: bool) -> Option<std::io::Result<()>> {
        let p = registration_path()?;
        Some(if install {
            (|| {
                let mut value = super::read_json(&p)?;
                super::nested::strip_marked(&mut value);
                super::nested::add_entries(&mut value, &entries());
                if let Some(hooks) = value.get_mut("hooks").and_then(Value::as_object_mut) {
                    for groups in hooks.values_mut() {
                        for group in groups.as_array_mut().into_iter().flatten() {
                            for hook in group
                                .get_mut("hooks")
                                .and_then(Value::as_array_mut)
                                .into_iter()
                                .flatten()
                            {
                                if hook
                                    .get("command")
                                    .and_then(Value::as_str)
                                    .is_some_and(|command| command.contains(super::MARKER))
                                {
                                    hook["timeout"] = json!(5);
                                }
                            }
                        }
                    }
                }
                super::write_json(&p, &value)
            })()
        } else {
            std::fs::remove_file(&p)
        })
    }
}

/// Kiro: flat hook entries (`{command, matcher?}`, no nested wrapper) inside
/// the per-agent file `~/.kiro/agents/kiro_default.json`; a minimal agent
/// file is created when none exists. V3 also uses a separate global hooks
/// file, including SessionEnd. Permission prompts use the screen fallback.
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

    const V3_EVENTS: &[(&str, &str)] = &[
        ("SessionStart", "session_start"),
        ("UserPromptSubmit", "prompt_submit"),
        ("PreToolUse", "tool_start"),
        ("PostToolUse", "tool_end"),
        ("Stop", "idle"),
        ("SessionEnd", "session_end"),
    ];

    pub(super) fn entries() -> Vec<super::HookEntry> {
        EVENTS
            .iter()
            .map(|(name, event, matched)| super::HookEntry {
                event: (*name).into(),
                matcher: matched.then(|| "*".into()),
                command: super::helper_command("kiro", Some(event)),
            })
            .collect()
    }

    pub(super) fn hook_fields(input: &[u8]) -> Option<serde_json::Map<String, Value>> {
        super::lifecycle_fields(input, V3_EVENTS).or_else(|| {
            super::lifecycle_fields(
                input,
                &EVENTS
                    .iter()
                    .map(|(name, event, _)| (*name, *event))
                    .collect::<Vec<_>>(),
            )
        })
    }

    fn global_path() -> Option<PathBuf> {
        Some(super::home()?.join(".kiro/hooks/uniterm-notify.json"))
    }

    fn global_hooks() -> Value {
        json!({"version": "v1", "hooks": V3_EVENTS.iter().map(|(name, event)| json!({
            "name": format!("Uniterm {name}"), "trigger": name,
            "action": {"type": "command", "command": super::helper_command("kiro", Some(event))},
            "timeout": 5,
        })).collect::<Vec<_>>()})
    }

    pub(super) fn has_global_hooks() -> bool {
        global_path()
            .is_some_and(|path| super::marker_status(&path) == super::ConnectorStatus::Installed)
    }

    pub(super) fn current_global_hooks() -> bool {
        global_path()
            .and_then(|path| super::read_json(&path).ok())
            .is_some_and(|value| value == global_hooks())
    }

    pub(super) fn agent_path() -> Option<PathBuf> {
        Some(super::home()?.join(".kiro/agents/kiro_default.json"))
    }

    pub(super) fn toggle(install: bool) -> Option<std::io::Result<()>> {
        let p = agent_path()?;
        let global = global_path()?;
        Some(apply(&p, install).and_then(|()| {
            if install {
                super::write_json(&global, &global_hooks())
            } else {
                match std::fs::remove_file(global) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    result => result,
                }
            }
        }))
    }

    fn apply(p: &std::path::Path, install: bool) -> std::io::Result<()> {
        let mut v = super::read_json(p)?;
        if !v.is_object() {
            v = json!({});
        }
        let root = v.as_object_mut().expect("agent root is an object");
        if install {
            super::strip_flat_hooks(root);
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
                let cmd = super::helper_command("kiro", Some(event));
                arr.retain(|entry| {
                    entry
                        .get("command")
                        .and_then(Value::as_str)
                        .is_none_or(|command| !command.contains(super::MARKER) || command == cmd)
                });
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

    pub(super) fn entries() -> Vec<super::HookEntry> {
        EVENTS
            .iter()
            .map(|(name, event)| super::HookEntry {
                event: (*name).into(),
                matcher: None,
                command: super::helper_command("cursor", Some(event)),
            })
            .collect()
    }

    pub(super) fn hook_fields(input: &[u8]) -> Option<serde_json::Map<String, Value>> {
        let hook: Value = serde_json::from_slice(input).ok()?;
        let mut fields = super::lifecycle_fields(input, EVENTS)?;
        if fields.get("event").and_then(Value::as_str) == Some("idle")
            && hook.get("status").and_then(Value::as_str) == Some("error")
        {
            fields.insert("event".into(), Value::String("error".into()));
        }
        Some(fields)
    }

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
            super::strip_flat_hooks(root);
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
                let command = super::helper_command("cursor", Some(event));
                entries.retain(|entry| {
                    entry
                        .get("command")
                        .and_then(Value::as_str)
                        .is_none_or(|old| !old.contains(super::MARKER) || old == command)
                });
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

    pub(super) const EXTENSION: &str = r#"// uniterm connector: reports Pi lifecycle over OSC 777.
// Installed by uniterm (Agents > Setup...); safe to delete.
import * as fs from "node:fs"
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent"

const emit = (event: string, context?: any) => {
  if (!process.env.UNITERM) return
  try {
    fs.writeFileSync(
      "/dev/tty",
      "\x1b]777;notify;uniterm://cli-agent;" + JSON.stringify({
        agent: "pi", event,
        session_id: context?.sessionManager?.getSessionId?.(),
        transcript_path: context?.sessionManager?.getSessionFile?.(),
      }).replace(/;/g, "\\u003b") + "\x07",
    )
  } catch {}
}

export default function unitermNotify(pi: ExtensionAPI) {
  pi.on("session_start", (_event, context) => emit("session_start", context))
  pi.on("agent_start", (_event, context) => emit("prompt_submit", context))
  pi.on("tool_execution_start", (_event, context) => emit("tool_start", context))
  pi.on("tool_execution_end", (_event, context) => emit("tool_end", context))
  pi.on("agent_settled", (_event, context) => emit("idle", context))
  pi.on("session_shutdown", (event, context) => {
    if (event.reason === "quit") emit("session_end", context)
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
/// version-specific auto-discovered plugins under the XDG config directory.
/// V1 is a server function; V2 separates the server definition and local TUI
/// lifecycle observer. Uninstall removes both managed layouts.
mod opencode {
    use std::path::PathBuf;

    /// The plugin source. Kept dependency-free and untyped so it loads under
    /// OpenCode's bundled runtime as-is; the marker URI doubles as the
    /// installed-ness probe.
    pub(super) const PLUGIN: &str = include_str!("connectors/opencode-v1.js");
    const SERVER: &str = "// uniterm://cli-agent - lifecycle belongs to the local TUI.\nexport default { id: \"uniterm.notify\", setup() {} }\n";
    const TUI: &str = include_str!("connectors/opencode-tui.js");

    pub(super) fn plugin_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| Some(super::home()?.join(".config")))?;
        Some(base.join("opencode/plugins/uniterm-notify.ts"))
    }

    pub(super) fn status() -> super::ConnectorStatus {
        let Some(path) = plugin_path() else {
            return super::ConnectorStatus::NotInstalled;
        };
        let directory = path.with_extension("");
        let legacy = std::fs::read_to_string(&path).ok();
        let server = std::fs::read_to_string(directory.join("index.ts")).ok();
        let tui = std::fs::read_to_string(directory.join("tui.ts")).ok();
        if legacy.as_deref() == Some(PLUGIN) && server.is_none() && tui.is_none()
            || legacy.is_none() && server.as_deref() == Some(SERVER) && tui.as_deref() == Some(TUI)
        {
            return super::ConnectorStatus::Installed;
        }
        if legacy
            .iter()
            .chain(server.iter())
            .chain(tui.iter())
            .any(|text| text.contains(super::MARKER))
        {
            super::ConnectorStatus::Outdated
        } else {
            super::ConnectorStatus::NotInstalled
        }
    }

    // Probe only during explicit installation, never during status refresh or
    // on the core loop. A missing CLI defaults to the current v2 contract.
    fn legacy_cli() -> std::io::Result<bool> {
        use std::io::Read;
        use std::process::{Command, Stdio};
        let mut child = match Command::new("opencode")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        // An explicit version probe has a bounded deadline and a kernel exit
        // watch, never a recurring timer or process-table scan.
        let exited = crate::process_watch::ProcessWatch::new(child.id() as i32)
            .and_then(|watch| watch.wait_exit(std::time::Duration::from_secs(2)))
            .unwrap_or(false);
        if !exited && !child.try_wait().is_ok_and(|status| status.is_some()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "opencode --version timed out; connector unchanged",
            ));
        }
        if !child.wait()?.success() {
            return Err(std::io::Error::other(
                "opencode --version failed; connector unchanged",
            ));
        }
        let mut version = String::new();
        if let Some(stdout) = child.stdout.take() {
            stdout.take(256).read_to_string(&mut version)?;
        }
        legacy_version(&version)
    }

    fn legacy_version(version: &str) -> std::io::Result<bool> {
        let version = version.trim();
        let version = version.strip_prefix("opencode ").unwrap_or(version);
        match version.trim_start_matches('v').split('.').next() {
            Some("1") => Ok(true),
            Some("2") => Ok(false),
            _ => Err(std::io::Error::other(
                "unrecognized OpenCode version; connector unchanged",
            )),
        }
    }

    #[test]
    fn version_probe_requires_a_known_contract() {
        assert!(legacy_version("1.2.27\n").unwrap());
        assert!(!legacy_version("v2.0.0-beta.3\n").unwrap());
        assert!(!legacy_version("opencode v2.0.26\n").unwrap());
        assert!(legacy_version("error loading configuration").is_err());
        assert!(legacy_version("3.0.0").is_err());
    }

    fn remove_file(path: &std::path::Path) -> std::io::Result<()> {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    pub(super) fn install_version(path: &std::path::Path, legacy: bool) -> std::io::Result<()> {
        let directory = path.with_extension("");
        if legacy {
            super::write_atomic(path, PLUGIN)?;
            remove_file(&directory.join("index.ts"))?;
            remove_file(&directory.join("tui.ts"))?;
            let _ = std::fs::remove_dir(directory);
        } else {
            super::write_atomic(&directory.join("index.ts"), SERVER)?;
            super::write_atomic(&directory.join("tui.ts"), TUI)?;
            remove_file(path)?;
        }
        Ok(())
    }

    pub(super) fn toggle(install: bool) -> Option<std::io::Result<()>> {
        let path = plugin_path()?;
        Some(if install {
            legacy_cli().and_then(|legacy| install_version(&path, legacy))
        } else {
            (|| {
                remove_file(&path)?;
                let directory = path.with_extension("");
                remove_file(&directory.join("index.ts"))?;
                remove_file(&directory.join("tui.ts"))?;
                let _ = std::fs::remove_dir(directory);
                Ok(())
            })()
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
    fn typed_waits_failures_and_child_hooks_preserve_lifecycle() {
        for (provider, payload, event) in [
            (
                "claude",
                json!({"hook_event_name":"PreToolUse", "tool_name":"AskUserQuestion"}),
                "question",
            ),
            (
                "claude",
                json!({"hook_event_name":"PostToolUseFailure", "tool_name":"Bash"}),
                "tool_end",
            ),
            ("claude", json!({"hook_event_name":"StopFailure"}), "error"),
            (
                "claude",
                json!({"hook_event_name":"Elicitation"}),
                "question",
            ),
            (
                "claude",
                json!({"hook_event_name":"ElicitationResult"}),
                "tool_end",
            ),
            (
                "gemini",
                json!({"hook_event_name":"Notification", "notification_type":"ToolPermission"}),
                "permission_request",
            ),
            (
                "cursor",
                json!({"hook_event_name":"stop", "status":"error"}),
                "error",
            ),
            (
                "cursor",
                json!({"hook_event_name":"stop", "status":"aborted"}),
                "idle",
            ),
            (
                "kiro",
                json!({"hook_event_name":"PreToolUse"}),
                "tool_start",
            ),
            (
                "kiro",
                json!({"hook_event_name":"SessionEnd"}),
                "session_end",
            ),
        ] {
            let envelope = hook_envelope(provider, &serde_json::to_vec(&payload).unwrap()).unwrap();
            assert!(
                envelope.contains(&format!("\"event\":\"{event}\"")),
                "{envelope}"
            );
        }
        for event in ["PreToolUse", "PostToolUse", "Stop", "SessionEnd"] {
            assert!(claude_fields(
                json!({"hook_event_name":event,"agent_id":"child","session_id":"parent"})
            )
            .is_none());
        }
        assert_eq!(claude_fields(json!({"hook_event_name":"PermissionRequest", "agent_id":"child", "tool_name":"Bash", "session_id":"parent"})).unwrap()["event"], "permission_request");
        for kind in ["OtherAlert", "", "permission_prompt"] {
            assert!(hook_envelope(
                "gemini",
                &serde_json::to_vec(
                    &json!({"hook_event_name":"Notification", "notification_type":kind})
                )
                .unwrap()
            )
            .is_none());
        }
    }

    #[test]
    fn codex_pre_review_permission_hook_does_not_claim_a_human_wait() {
        let envelope = hook_envelope(
            "codex",
            br#"{"hook_event_name":"PermissionRequest","session_id":"thread"}"#,
        )
        .unwrap();
        let mut terminal = crate::terminal::Terminal::new(80, 24);
        terminal.feed(envelope.as_bytes());
        let events = terminal.take_agent_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, None);
        assert_eq!(events[0].session_id.as_deref(), Some("thread"));
        assert!(!codex::entries()
            .iter()
            .any(|entry| entry.command.contains("permission_request")));
    }

    #[test]
    fn grok_notifications_and_child_teardown_do_not_fabricate_blockers() {
        for (kind, event) in [
            ("permission_prompt", "permission_request"),
            ("idle_prompt", "notification"),
            ("task_complete", "notification"),
            ("unknown", "notification"),
        ] {
            let fields = grok::hook_fields(&serde_json::to_vec(&json!({"hook_event_name":"Notification", "notificationType":kind, "sessionId":"root"})).unwrap()).unwrap();
            assert_eq!(fields["event"], event);
            assert_eq!(fields["session_id"], "root");
        }
        for (hook, event) in [("StopCancelled", "idle"), ("StopFailure", "error")] {
            let fields =
                grok::hook_fields(&serde_json::to_vec(&json!({"hook_event_name":hook})).unwrap())
                    .unwrap();
            assert_eq!(fields["event"], event);
        }
        assert!(grok::hook_fields(
            br#"{"hook_event_name":"SessionEnd","subagentType":"explore","sessionId":"child"}"#
        )
        .is_none());
    }

    #[test]
    fn nested_install_uninstall_round_trip() {
        let path = temp_file("roundtrip", "settings.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(marker_status(&path), ConnectorStatus::NotInstalled);
        nested::install(&path, &claude::entries()).unwrap();
        assert_eq!(marker_status(&path), ConnectorStatus::Installed);
        // Idempotent: a second install adds nothing.
        nested::install(&path, &claude::entries()).unwrap();
        let v = read(&path);
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "duplicate hook group after reinstall");
        // The envelope carries the marker URI + the event name the parser maps.
        let cmd = stop[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains(MARKER) && cmd.contains("agent hook claude"));
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
        nested::install(&path, &claude::entries()).unwrap();
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
            let global = home.join(".kiro/hooks/uniterm-notify.json");
            let hooks = read(&global);
            assert_eq!(hooks["version"], "v1");
            assert!(hooks["hooks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hook| hook["trigger"] == "SessionEnd"));
            assert!(kiro::current_global_hooks());
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

    /// Run environment-sensitive fixtures in a single-test child process.
    /// Other library tests may spawn PTYs while these exercise connectors;
    /// a local mutex cannot isolate process-global HOME from those threads.
    fn temp_env(home: &Path, f: impl FnOnce()) {
        const CHILD: &str = "UNITERM_CONNECTOR_FIXTURE_CHILD";
        let thread = std::thread::current();
        let name = thread.name().expect("named test thread");
        if std::env::var(CHILD).as_deref() == Ok(name) {
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::remove_var("GROK_HOME");
            std::env::set_var("HOME", home);
            f();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD, name)
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("GROK_HOME")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated connector fixture failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn connector_lock_releases_despite_an_inherited_descriptor() {
        let path = temp_file("lock-inheritance", "connectors.lock");
        let owner = connector_lock_at(&path).unwrap();
        // dup and fork share the same open file description and flock.
        let inherited = owner.0.try_clone().unwrap();
        assert_eq!(
            connector_lock_at(&path).err().unwrap().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(owner);
        let next_owner = connector_lock_at(&path).unwrap();
        drop(inherited);
        // Closing the old inherited descriptor cannot release the new owner.
        assert_eq!(
            connector_lock_at(&path).err().unwrap().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(next_owner);
        assert!(connector_lock_at(&path).is_ok());
    }

    #[test]
    fn flat_connector_upgrade_replaces_legacy_commands_without_duplicate_status_hooks() {
        let home = temp_file("flat-upgrade", "x")
            .parent()
            .unwrap()
            .join("home");
        std::fs::create_dir_all(&home).unwrap();
        temp_env(&home, || {
            for (provider, event, path) in [
                ("cursor", "sessionStart", cursor::hooks_path().unwrap()),
                ("kiro", "agentSpawn", kiro::agent_path().unwrap()),
            ] {
                write_json(
                    &path,
                    &json!({"hooks":{event:[
                        {"command": "echo keep-user-hook"},
                        {"command": hook_command(provider, "session_start")}
                    ]}}),
                )
                .unwrap();
                let (result, status) = install(provider);
                result.unwrap();
                assert_eq!(status, ConnectorStatus::Installed);
                let installed = read(&path);
                let entries = installed["hooks"][event].as_array().unwrap();
                assert_eq!(entries.len(), 2, "{provider}");
                assert_eq!(entries[0]["command"], "echo keep-user-hook");
                assert!(entries[1]["command"]
                    .as_str()
                    .unwrap()
                    .contains("agent hook"));
                install(provider).0.unwrap();
                assert_eq!(read(&path), installed);
            }
        });
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
                .contains("agent hook grok"));
            assert!(!entry["command"].as_str().unwrap().contains("printf"));
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
            let plug = home.join(".config/opencode/plugins/uniterm-notify.ts");
            write_atomic(
                &plug,
                "// uniterm://cli-agent\nexport const Old = async () => ({})",
            )
            .unwrap();
            assert_eq!(status("opencode"), ConnectorStatus::Outdated);
            for legacy in [true, false, true, false] {
                opencode::install_version(&plug, legacy).unwrap();
                assert_eq!(status("opencode"), ConnectorStatus::Installed);
                assert_eq!(plug.exists(), legacy);
                assert_eq!(plug.with_extension("").join("tui.ts").exists(), !legacy);
            }
            std::fs::write(
                plug.with_extension("").join("tui.ts"),
                "// uniterm://cli-agent old",
            )
            .unwrap();
            assert_eq!(status("opencode"), ConnectorStatus::Outdated);
            opencode::install_version(&plug, false).unwrap();
            assert_eq!(toggle("opencode"), ConnectorStatus::NotInstalled);
            assert!(!plug.exists());
            assert!(!plug.with_extension("").exists());
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
        assert!(nested::install(&path, &claude::entries()).is_err());
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

    fn claude_fields(json: Value) -> Option<serde_json::Map<String, Value>> {
        claude::hook_fields(json.to_string().as_bytes())
    }

    #[test]
    fn claude_notification_type_alone_decides_permission() {
        let base = |kind: Option<&str>, message: &str| {
            let mut hook = json!({
                "hook_event_name": "Notification",
                "session_id": "s-1",
                "transcript_path": "/tmp/demo/s-1.jsonl",
                "message": message,
            });
            if let Some(kind) = kind {
                hook["notification_type"] = Value::String(kind.into());
            }
            claude_fields(hook).unwrap()
        };
        let permission = base(Some("permission_prompt"), "Claude needs your permission");
        assert_eq!(permission["event"], "permission_request");
        assert_eq!(permission["session_id"], "s-1");
        assert_eq!(permission["transcript_path"], "/tmp/demo/s-1.jsonl");
        assert_eq!(
            base(Some("elicitation_dialog"), "pick")["event"],
            "question"
        );
        // The idle reminder, a push notification, an unknown type, and a
        // missing type are events only - even when the text says "permission".
        for kind in [
            Some("idle_prompt"),
            Some("push_notification"),
            Some("worker_permission_prompt"),
            Some("brand_new_type"),
            None,
        ] {
            let fields = base(kind, "needs your permission to finish");
            assert_eq!(fields["event"], "notification", "{kind:?}");
            assert_eq!(fields["message"], "needs your permission to finish");
        }
        let push = base(Some("push_notification"), "Demo done. Loop stopped.");
        assert_eq!(push["notification_kind"], "push_notification");
    }

    #[test]
    fn claude_permission_request_names_its_tool() {
        let fields = claude_fields(json!({
            "hook_event_name": "PermissionRequest",
            "session_id": "s-1",
            "tool_name": "Bash",
            "tool_input": {"command": "git push\nrm -rf /tmp/demo", "description": "x"},
        }))
        .unwrap();
        assert_eq!(fields["event"], "permission_request");
        assert_eq!(fields["tool"], "Bash");
        assert_eq!(fields["preview"], "git push rm -rf /tmp/demo");
        let big = claude_fields(json!({
            "hook_event_name": "PermissionRequest",
            "tool_name": "Write",
            "tool_input": {"content": "x".repeat(100_000)},
        }))
        .unwrap();
        assert!(big["preview"].as_str().unwrap().chars().count() <= 512);
        // A permission request without a tool is not understood.
        assert!(claude_fields(json!({"hook_event_name": "PermissionRequest"})).is_none());
    }

    #[test]
    fn claude_loop_is_reported_only_from_a_completed_wakeup_call() {
        let post = |event: &str, tool: &str, input: Value| {
            claude_fields(json!({
                "hook_event_name": event,
                "tool_name": tool,
                "tool_input": input,
                "tool_response": {},
            }))
        };
        let stopped = post("PostToolUse", "ScheduleWakeup", json!({"stop": true})).unwrap();
        assert_eq!(stopped["event"], "loop");
        assert_eq!(stopped["loop_state"], "stopped");
        let scheduled = post(
            "PostToolUse",
            "ScheduleWakeup",
            json!({"delaySeconds": 90, "prompt": "/loop"}),
        )
        .unwrap();
        assert_eq!(scheduled["loop_state"], "scheduled");
        assert_eq!(scheduled["delay_seconds"], 90);
        assert!(post(
            "PostToolUseFailure",
            "ScheduleWakeup",
            json!({"stop": true})
        )
        .unwrap()
        .get("loop_state")
        .is_none());
        assert!(post("PreToolUse", "ScheduleWakeup", json!({"stop": true}))
            .unwrap()
            .get("loop_state")
            .is_none());
        assert!(post("PostToolUse", "Bash", json!({"stop": true}))
            .unwrap()
            .get("loop_state")
            .is_none());
        assert!(post("PostToolUse", "ScheduleWakeup", json!({"stop": false})).is_none());
    }

    #[test]
    fn grok_compatibility_hooks_keep_host_identity_and_notification_semantics() {
        for imported in ["claude", "cursor", "grok"] {
            for (kind, expected) in [
                ("permission_prompt", "permission_request"),
                ("idle_prompt", "notification"),
            ] {
                let input = json!({"hookEventName":"notification", "hook_event_name":"Notification", "sessionId":"grok-session", "session_id":"grok-session", "notificationType":kind, "message":"Waiting for your next prompt"});
                let wire = hook_envelope(imported, input.to_string().as_bytes()).unwrap();
                assert!(wire.contains("\"agent\":\"grok\""));
                assert!(wire.contains(&format!("\"event\":\"{expected}\"")));
            }
            let child = json!({"hookEventName":"session_end", "hook_event_name":"SessionEnd", "sessionId":"child", "subagentType":"explore"});
            assert!(hook_envelope(imported, child.to_string().as_bytes()).is_none());
        }
        let claude = json!({"hook_event_name":"SessionStart", "session_id":"claude-session"});
        assert!(hook_envelope("claude", claude.to_string().as_bytes())
            .unwrap()
            .contains("\"agent\":\"claude\""));
    }

    #[test]
    fn lifecycle_helpers_preserve_session_identity_without_private_transcript_content() {
        for (provider, event) in [
            ("claude", "UserPromptSubmit"),
            ("codex", "SessionStart"),
            ("gemini", "BeforeAgent"),
            ("grok", "SessionStart"),
            ("cursor", "beforeSubmitPrompt"),
            ("kiro", "agentSpawn"),
        ] {
            let input = json!({"hook_event_name":event, "session_id":"owned-session", "transcript_path":"/tmp/owned.jsonl", "prompt":"PRIVATE PROMPT"});
            let wire = hook_envelope(provider, input.to_string().as_bytes()).unwrap();
            let mut term = crate::terminal::Terminal::new(80, 24);
            term.feed(wire.as_bytes());
            let events = term.take_agent_events();
            assert_eq!(
                events[0].session_id.as_deref(),
                Some("owned-session"),
                "{provider}"
            );
            assert_eq!(
                events[0].transcript_path.as_deref(),
                Some("/tmp/owned.jsonl"),
                "{provider}"
            );
            assert!(!wire.contains("PRIVATE PROMPT"));
        }
        let input = json!({"hook_event_name":"SessionStart", "session_id":"child", "parent_session_id":"root"});
        let fields = lifecycle_fields(input.to_string().as_bytes(), codex::EVENTS).unwrap();
        assert_eq!(fields["parent_session_id"], "root");
    }

    #[test]
    fn codex_interrupt_reports_idle_with_the_same_native_session() {
        let input = json!({"hook_event_name":"Interrupt", "session_id":"root"});
        let fields = codex::hook_fields(input.to_string().as_bytes()).unwrap();
        assert_eq!(fields["event"], "idle");
        assert_eq!(fields["session_id"], "root");
        assert!(codex::entries()
            .iter()
            .any(|entry| entry.event == "Interrupt"));
    }

    #[test]
    fn malformed_or_foreign_hook_input_is_silent() {
        assert!(hook_envelope("claude", b"").is_none());
        assert!(hook_envelope("claude", b"{not json").is_none());
        assert!(hook_envelope("claude", b"{\"hook_event_name\":\"ForeignEvent\"}").is_none());
        assert!(hook_envelope("codex", b"{\"hook_event_name\":\"Notification\"}").is_none());
    }

    #[test]
    fn envelope_round_trips_through_the_terminal_parser() {
        // Semicolons beyond vte's sixteen OSC parameters, non-ASCII text, an
        // escape sequence, a BEL, and C1 controls all survive or are dropped
        // deliberately, never truncating or terminating the envelope early.
        let message = format!(
            "a;b;c;d;e;f;g;h;i;j;k;l;m;n;o;p;q 日本 🦀 \u{1b}]0;x\u{07} \u{9c}end{}",
            ";".repeat(40)
        );
        let input = json!({
            "hook_event_name": "Notification",
            "notification_type": "push_notification",
            "message": message,
            "session_id": "s;1",
            "transcript_path": "/tmp/demo/dir;x/s.jsonl",
        });
        let wire = hook_envelope("claude", input.to_string().as_bytes()).unwrap();
        assert!(wire.starts_with("\x1b]777;notify;uniterm://cli-agent;{"));
        assert!(wire.ends_with("}\x07"));
        let body = &wire["\x1b]777;notify;uniterm://cli-agent;".len()..wire.len() - 1];
        assert!(body.bytes().all(|b| b.is_ascii_graphic() || b == b' '));
        assert!(!body.contains(';'));
        let mut term = crate::terminal::Terminal::new(80, 24);
        term.feed(wire.as_bytes());
        let events = term.take_agent_events();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.agent.as_deref(), Some("claude"));
        assert_eq!(event.status, None);
        assert_eq!(event.session_id.as_deref(), Some("s;1"));
        assert_eq!(
            event.transcript_path.as_deref(),
            Some("/tmp/demo/dir;x/s.jsonl")
        );
        let expected = uniterm_core::agent_detail::bounded_text(&message, 512);
        assert_eq!(
            event.details,
            vec![uniterm_core::AgentDetail::Notification {
                kind: "push_notification".into(),
                message: expected,
            }]
        );
        // Nothing reached the grid.
        assert_eq!(term.grid().get(0, 0).ch, ' ');
    }

    #[test]
    fn inbox_discovery_uses_only_supported_session_context_hooks() {
        for provider in ["claude", "gemini"] {
            let output = inbox_context(provider, br#"{"hook_event_name":"SessionStart"}"#).unwrap();
            assert_eq!(
                output["hookSpecificOutput"]["hookEventName"],
                "SessionStart"
            );
            assert!(output["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("ut inbox discover"));
            assert!(inbox_context(provider, br#"{"hook_event_name":"PostToolUse"}"#).is_none());
            let entries = connector(provider).unwrap().expected.unwrap()();
            assert!(entries.iter().any(|e| e.event == "SessionStart"
                && e.command.contains("inbox-context")
                && !e.command.contains(" >/dev/null")));
        }
        assert!(inbox_context("custom", br#"{"hook_event_name":"SessionStart"}"#).is_none());
        assert!(inbox_context("claude", b"invalid").is_none());
    }

    #[test]
    fn claude_hooks_split_status_and_helper_entries() {
        let entries = claude::entries();
        let notification: Vec<_> = entries
            .iter()
            .filter(|entry| entry.event == "Notification")
            .collect();
        assert_eq!(notification.len(), 1);
        let command = &notification[0].command;
        assert!(command.contains("agent hook claude") && command.contains(MARKER));
        // No plain fallback can turn a Notification into a permission.
        assert!(!command.contains("permission_request"));
        assert!(!entries
            .iter()
            .any(|entry| entry.command.contains("permission_request")));
        let wakeup = entries
            .iter()
            .find(|entry| entry.matcher.as_deref() == Some("ScheduleWakeup"))
            .unwrap();
        assert_eq!(wakeup.event, "PostToolUse");
        let session = entries
            .iter()
            .find(|entry| entry.event == "SessionStart")
            .unwrap();
        assert!(session.command.contains("agent hook claude"));
        // A helper returning no event must stay silent through the installed
        // shell command too, especially child PreToolUse and Stop hooks.
        assert!(entries
            .iter()
            .all(|entry| !entry.command.contains("printf")));
    }

    #[test]
    fn old_claude_hooks_are_outdated_until_an_explicit_upgrade() {
        let path = temp_file("outdated", "settings.json");
        // The pre-1.2.1 shape: Notification printed a bare permission.
        let old = printf_entries(
            "claude",
            &[
                ("SessionStart", "session_start"),
                ("Notification", "permission_request"),
                ("Stop", "idle"),
            ],
        );
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "model": "opus",
                "hooks": {"Notification": [
                    {"hooks": [{"type": "command", "command": "notify-send hi"}]}
                ]}
            }))
            .unwrap(),
        )
        .unwrap();
        nested::install(&path, &old).unwrap();
        let current = claude::entries();
        assert_eq!(
            path_status(&path, Some(&current)),
            ConnectorStatus::Outdated
        );
        // Reading status never rewrites the file.
        let before = std::fs::read_to_string(&path).unwrap();
        let _ = path_status(&path, Some(&current));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        // The explicit upgrade swaps our marked entries in one write.
        nested::upgrade(&path, &current).unwrap();
        assert_eq!(
            path_status(&path, Some(&current)),
            ConnectorStatus::Installed
        );
        let v = read(&path);
        assert_eq!(v["model"], "opus");
        let groups = v["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(groups[0]["hooks"][0]["command"], "notify-send hi");
        assert!(!v.to_string().contains("permission_request"));
        assert_eq!(nested::marked_entries(&path), entry_set(&current));
        // A file that cannot be parsed is never rewritten by an upgrade.
        let corrupt = temp_file("outdated-corrupt", "settings.json");
        std::fs::write(&corrupt, "{ \"hooks\": {}, }").unwrap();
        assert!(nested::upgrade(&corrupt, &current).is_err());
        assert_eq!(
            std::fs::read_to_string(&corrupt).unwrap(),
            "{ \"hooks\": {}, }"
        );
    }

    #[test]
    fn a_failed_upgrade_write_keeps_the_previous_connector() {
        // A read-only directory makes the atomic temp-file write fail; the
        // old connector and the user's hooks must survive byte for byte.
        use std::os::unix::fs::PermissionsExt as _;
        let path = temp_file("upgrade-readonly", "settings.json");
        let old = printf_entries("claude", &[("Notification", "permission_request")]);
        std::fs::write(
            &path,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"say done"}]}]}}"#,
        )
        .unwrap();
        nested::install(&path, &old).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let dir = path.parent().unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = nested::upgrade(&path, &claude::entries());
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if result.is_ok() {
            // Running as root ignores directory permissions; nothing to prove.
            return;
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(
            path_status(&path, Some(&claude::entries())),
            ConnectorStatus::Outdated
        );
    }
}
