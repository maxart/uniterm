//! `ut agent hook AGENT` and `ut agent connector ...`.
//!
//! The hook verb is what an installed connector runs inside a Pane: it reads
//! one provider hook invocation from stdin, asks the provider's connector
//! module to translate it, and writes a single bounded OSC 777 envelope to the
//! Pane's tty. It never talks to the server and never fails loudly: a hook that
//! cannot be understood exits non-zero in silence, so the agent is never
//! slowed or interrupted and no status is ever synthesized
//! (`docs/06-agentic-supervision.md`).
//!
//! The connector verb installs, upgrades, removes, and reports connectors
//! from the same functions the Agents surface uses; every reported state is
//! re-read from disk after the change.

use std::io::{Read as _, Write as _};

use uniterm_proto::ConnectorStatus;

/// Largest hook input read from stdin. Provider payloads can embed whole
/// files (a Write permission carries the content); anything larger is
/// treated as not understood rather than truncated into invalid JSON.
const HOOK_INPUT_LIMIT: u64 = 8 * 1024 * 1024;

pub(crate) fn command(verb: &str, args: &[String]) -> i32 {
    match verb {
        "hook" => hook(args),
        _ => connector(args),
    }
}

fn hook(args: &[String]) -> i32 {
    let [agent] = args else {
        eprintln!("usage: ut agent hook AGENT < hook-input.json");
        return 2;
    };
    // Outside a uniterm Pane there is no tty stream for the envelope to join.
    if std::env::var_os("UNITERM").is_none() {
        return 0;
    }
    let mut input = Vec::new();
    if std::io::stdin()
        .take(HOOK_INPUT_LIMIT + 1)
        .read_to_end(&mut input)
        .is_err()
        || input.len() as u64 > HOOK_INPUT_LIMIT
    {
        return 1;
    }
    let Some(envelope) = uniterm_server::connectors::hook_envelope(agent, &input) else {
        return 1;
    };
    // One write of the whole envelope, straight to the controlling terminal
    // the provider shares with the Pane (stdout belongs to the provider).
    let written = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .and_then(|mut tty| tty.write_all(envelope.as_bytes()));
    i32::from(written.is_err())
}

fn label(status: ConnectorStatus) -> &'static str {
    match status {
        ConnectorStatus::Installed => "installed",
        ConnectorStatus::NotInstalled => "not installed",
        ConnectorStatus::Unsupported => "unsupported",
        ConnectorStatus::Outdated => "outdated",
    }
}

fn connector(args: &[String]) -> i32 {
    const USAGE: &str =
        "usage: ut agent connector status [AGENT] [--json] | install AGENT | remove AGENT";
    match (
        args.first().map(String::as_str),
        args.get(1).map(String::as_str),
    ) {
        (Some("status") | None, agent) => {
            let json = args.iter().any(|arg| arg == "--json");
            let agent = agent.filter(|value| *value != "--json");
            let ids: Vec<&str> = match agent {
                Some(agent) => vec![agent],
                None => uniterm_core::agent::PROVIDERS
                    .iter()
                    .map(|provider| provider.id)
                    .collect(),
            };
            let rows: Vec<(&str, ConnectorStatus)> = ids
                .into_iter()
                .map(|id| (id, uniterm_server::connectors::status(id)))
                .collect();
            if json {
                let rows: Vec<_> = rows
                    .iter()
                    .map(|(id, status)| serde_json::json!({"agent": id, "connector": status}))
                    .collect();
                println!("{}", serde_json::json!({ "connectors": rows }));
            } else {
                for (id, status) in &rows {
                    let hint = if *status == ConnectorStatus::Outdated {
                        format!("  (upgrade: ut agent connector install {id})")
                    } else {
                        String::new()
                    };
                    println!(
                        "{:<10} {}{}",
                        crate::terminal_safe(id),
                        label(*status),
                        hint
                    );
                }
            }
            0
        }
        (Some(verb @ ("install" | "remove")), Some(agent)) if args.len() == 2 => {
            let (result, status) = if verb == "install" {
                uniterm_server::connectors::install(agent)
            } else {
                uniterm_server::connectors::remove(agent)
            };
            let wanted = if verb == "install" {
                ConnectorStatus::Installed
            } else {
                ConnectorStatus::NotInstalled
            };
            if let Err(error) = &result {
                eprintln!(
                    "uniterm agent connector {verb}: {}",
                    crate::terminal_safe(&error.to_string())
                );
            }
            println!(
                "{} connector: {}",
                crate::terminal_safe(agent),
                label(status)
            );
            if status == ConnectorStatus::Installed && verb == "install" {
                println!(
                    "restart running {} sessions to load it",
                    crate::terminal_safe(agent)
                );
            }
            i32::from(result.is_err() || status != wanted)
        }
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}
