//! Server-side workflow support: agent discovery (which providers are
//! installed) and shell-safe launch lines. The decision engine itself is pure
//! and lives in `uniterm_core::orchestrate`; the server only owns the I/O
//! (panes, prompt injection, the submit socket path). See `docs/07`.

use uniterm_core::agent::{provider, PROVIDERS};

/// One provider choice after registry and PATH resolution. The command is
/// retained with the durable orchestration so recovery never silently changes
/// which CLI owns an existing role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRoleProvider {
    pub id: String,
    pub command: String,
}

/// Snapshot `$PATH` once for server-owned executable resolution. Remote
/// bridges may replace this snapshot explicitly when SSH supplied a narrower
/// process environment than the user's interactive login shell.
pub fn search_path_from_env() -> Vec<String> {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|entry| entry.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve one executable against an explicit search path. Returning the
/// concrete path makes launches independent of a stale shell environment in
/// an already-running remote Workspace.
pub fn executable_on_search_path(cmd: &str, search_path: &[String]) -> Option<String> {
    if cmd.contains('/') {
        let path = std::path::Path::new(cmd);
        return (path.is_file() && is_executable(path)).then(|| cmd.to_string());
    }
    search_path.iter().find_map(|directory| {
        let path = std::path::Path::new(directory).join(cmd);
        (path.is_file() && is_executable(&path)).then(|| path.to_string_lossy().into_owned())
    })
}

/// Whether `cmd` resolves to an executable: a path-bearing command is checked
/// directly (custom agents by absolute path), a bare name is probed on
/// `$PATH`. A one-shot probe at use time - never a background poll (invariant:
/// no idle work).
pub fn on_path(cmd: &str) -> bool {
    executable_on_search_path(cmd, &search_path_from_env()).is_some()
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The provider ids whose CLI is installed on this machine, registry order.
pub fn installed_agents() -> Vec<String> {
    installed_agents_on_search_path(&search_path_from_env())
}

/// The installed provider ids resolved against one server-owned path snapshot.
pub fn installed_agents_on_search_path(search_path: &[String]) -> Vec<String> {
    PROVIDERS
        .iter()
        .filter(|provider| executable_on_search_path(provider.command, search_path).is_some())
        .map(|p| p.id.to_string())
        .collect()
}

/// Resolve the agent to launch as `(id, command)`: an explicit id maps
/// through the registry (and must be installed); an unknown id is taken as a
/// literal command if it is on `$PATH` (custom agents, tests); `None` picks
/// the first installed provider. The id is what launch-time binding and the
/// lifecycle envelopes carry, so the fleet colour and entry match.
pub fn resolve_agent(requested: Option<&str>) -> Option<(String, String)> {
    resolve_agent_on_search_path(requested, &search_path_from_env())
}

/// Resolve an agent against a server-owned search path and retain the
/// executable's concrete path so launch does not depend on a pane shell's
/// inherited environment.
pub fn resolve_agent_on_search_path(
    requested: Option<&str>,
    search_path: &[String],
) -> Option<(String, String)> {
    match requested {
        Some(id) => {
            if let Some(p) = provider(id) {
                executable_on_search_path(p.command, search_path)
                    .map(|command| (p.id.to_string(), command))
            } else {
                executable_on_search_path(id, search_path).map(|command| (id.to_string(), command))
            }
        }
        None => PROVIDERS.iter().find_map(|provider| {
            executable_on_search_path(provider.command, search_path)
                .map(|command| (provider.id.to_string(), command))
        }),
    }
}

/// Resolve one provider per ordered role. Explicit role choices override the
/// global provider, which overrides the first installed registry provider.
/// Pure role-name validation happens in core; installed CLI and capability
/// checks remain here on the I/O-owning server side.
pub fn resolve_role_providers(
    roles: &[uniterm_core::orchestrate::Role],
    global: Option<&str>,
    selections: &[uniterm_core::orchestrate::RoleProviderSelection],
) -> Result<Vec<ResolvedRoleProvider>, String> {
    resolve_role_providers_on_search_path(roles, global, selections, &search_path_from_env())
}

/// Resolve orchestration roles against one explicit executable search path.
pub fn resolve_role_providers_on_search_path(
    roles: &[uniterm_core::orchestrate::Role],
    global: Option<&str>,
    selections: &[uniterm_core::orchestrate::RoleProviderSelection],
    search_path: &[String],
) -> Result<Vec<ResolvedRoleProvider>, String> {
    if global.is_some_and(|provider| provider.is_empty() || provider.len() > 256) {
        return Err("global provider must contain between 1 and 256 bytes".into());
    }
    let aligned = uniterm_core::orchestrate::align_role_provider_selections(roles, selections)
        .map_err(|error| error.to_string())?;
    roles
        .iter()
        .zip(aligned)
        .map(|(role, selected)| {
            let requested = selected.as_deref().or(global);
            let Some((id, command)) = resolve_agent_on_search_path(requested, search_path) else {
                return Err(match requested {
                    Some(provider) => format!(
                        "provider '{provider}' for role '{}' is not installed",
                        role.name
                    ),
                    None => format!("no installed provider can fill role '{}'", role.name),
                });
            };
            for capability in &role.provider_requirement.capabilities {
                if capability != "interactive_cli" {
                    return Err(format!(
                        "provider '{id}' does not satisfy capability '{capability}' required by role '{}'",
                        role.name
                    ));
                }
            }
            Ok(ResolvedRoleProvider { id, command })
        })
        .collect()
}

/// A shell command printing the OSC 777 lifecycle envelope to stdout - which
/// at launch time IS the pane's PTY, so no `/dev/tty` indirection is needed.
/// The envelope itself is defined once, in `connectors` (the hooks emit the
/// same bytes), so the parser sees one shape from either source.
pub fn osc777_announce(agent: &str, event: &str) -> String {
    crate::connectors::envelope_printf(agent, event)
}

/// Wrap an agent invocation so the pane's own stream announces its lifecycle
/// even when no connector is installed: `session_start` before it boots and
/// `session_end` when it exits (the trailing printf runs whether the agent
/// exited cleanly or died on a signal), so the binding never goes stale.
pub fn announce_wrapped(agent_id: &str, invocation: &str) -> String {
    format!(
        "{}; {invocation}; {}",
        osc777_announce(agent_id, "session_start"),
        osc777_announce(agent_id, "session_end"),
    )
}

/// Single-quote `s` for a POSIX shell (the standard `'...'` with embedded
/// quotes as `'\''`), so a prompt can be passed as one argv safely.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Turn provider-owned argv into a shell-safe invocation without interpreting
/// or manufacturing any provider-specific flag.
pub fn shell_join(argv: &[String]) -> Option<String> {
    (!argv.is_empty()).then(|| {
        argv.iter()
            .map(|argument| shell_quote(argument))
            .collect::<Vec<_>>()
            .join(" ")
    })
}

/// Build a shell-safe first invocation from a resolved executable and one
/// initial prompt argument. Custom executable paths are untrusted input and
/// must be quoted just like the prompt.
pub fn launch_invocation(command: &str, prompt: &str) -> String {
    format!("{} {}", shell_quote(command), shell_quote(prompt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_handles_quotes_and_spaces() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_join_preserves_provider_owned_argv() {
        assert_eq!(
            shell_join(&["agent".into(), "resume id's".into()]).as_deref(),
            Some("'agent' 'resume id'\\''s'")
        );
        assert_eq!(shell_join(&[]), None);
    }

    #[test]
    fn launch_invocation_quotes_custom_executable_paths() {
        assert_eq!(
            launch_invocation("/tmp/agent with spaces", "fix it's tests"),
            "'/tmp/agent with spaces' 'fix it'\\''s tests'"
        );
    }

    #[test]
    fn path_probe_finds_sh_and_rejects_nonsense() {
        assert!(on_path("sh"));
        assert!(!on_path("definitely-not-a-real-binary-xyz"));
        // Unknown ids resolve as literal commands only when installed; an
        // unknown-but-installed command binds under its own name.
        let (id, command) = resolve_agent(Some("sh")).unwrap();
        assert_eq!(id, "sh");
        assert!(std::path::Path::new(&command).is_absolute());
        assert!(command.ends_with("/sh"));
        assert_eq!(resolve_agent(Some("definitely-not-real-xyz")), None);
    }

    #[test]
    fn role_provider_resolution_applies_explicit_choices_over_global_fallback() {
        let roles = vec![
            uniterm_core::orchestrate::Role::new("builder", false),
            uniterm_core::orchestrate::Role::new("verifier", true),
        ];
        let providers = resolve_role_providers(
            &roles,
            Some("sh"),
            &[uniterm_core::orchestrate::RoleProviderSelection {
                role: "verifier".into(),
                provider: "/bin/sh".into(),
            }],
        )
        .unwrap();
        assert_eq!(providers[0].id, "sh");
        assert!(std::path::Path::new(&providers[0].command).is_absolute());
        assert!(providers[0].command.ends_with("/sh"));
        assert_eq!(providers[1].id, "/bin/sh");
        assert_eq!(providers[1].command, "/bin/sh");
    }

    #[test]
    fn role_provider_resolution_reports_the_role_with_a_missing_cli() {
        let roles = vec![uniterm_core::orchestrate::Role::new("builder", false)];
        let error = resolve_role_providers(
            &roles,
            None,
            &[uniterm_core::orchestrate::RoleProviderSelection {
                role: "builder".into(),
                provider: "definitely-not-real-xyz".into(),
            }],
        )
        .unwrap_err();
        assert!(error.contains("role 'builder'"));
        assert!(error.contains("not installed"));
    }

    #[test]
    fn announce_wrapper_emits_a_parseable_envelope() {
        // End to end: run the announce printf through a real shell and feed
        // its bytes to the emulator - the exact path a launched pane takes.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(osc777_announce("codex", "session_start"))
            .output()
            .expect("sh runs");
        let mut term = crate::terminal::Terminal::new(80, 24);
        term.feed(&out.stdout);
        let evs = term.take_agent_events();
        assert_eq!(evs.len(), 1, "envelope did not parse: {:?}", out.stdout);
        assert_eq!(evs[0].agent.as_deref(), Some("codex"));
        assert_eq!(evs[0].status, Some(uniterm_core::AgentStatus::Starting));
    }

    #[test]
    fn wrapped_invocation_keeps_start_agent_end_order() {
        let line = announce_wrapped("grok", "grok 'do the thing'");
        let start = line.find("session_start").unwrap();
        let agent = line.find("grok 'do the thing'").unwrap();
        let end = line.find("session_end").unwrap();
        assert!(start < agent && agent < end, "bad order: {line}");
    }
}
