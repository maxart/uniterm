//! Saved SSH Server commands. Client-local profiles never retarget a command
//! issued in a Pane; remote control always names its own SSH target explicitly.
use uniterm_client::machines::{catalog_path, discover, list_at, select, update_at};
use uniterm_core::machine::{MachineEvent, MachineProfile};

pub(super) fn cmd(args: &[String]) -> i32 {
    match run(args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("uniterm machine: {error}");
            1
        }
    }
}
fn run(args: &[String]) -> std::io::Result<i32> {
    let path = catalog_path()?;
    let mut args = args.to_vec();
    let json = args.iter().any(|arg| arg == "--json");
    args.retain(|arg| arg != "--json");
    let verb = args.first().map(String::as_str).unwrap_or("list");
    match verb {
        "list" if args.len() <= 1 => {
            let profiles = list_at(&path)?;
            if json {
                println!("{}", serde_json::to_string(&profiles)?);
            } else if profiles.is_empty() {
                println!("No saved Servers. Use ut machine add LABEL SSH_TARGET [WORKSPACE]");
            } else {
                for p in profiles {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        p.id,
                        p.label,
                        p.target,
                        p.workspace,
                        if p.enabled { "enabled" } else { "disabled" }
                    );
                }
            }
        }
        "add" if (3..=4).contains(&args.len()) => {
            let profiles = update_at(&path, |_, next| {
                Ok(MachineEvent::Saved(MachineProfile {
                    id: next,
                    label: args[1].clone(),
                    target: args[2].clone(),
                    workspace: args.get(3).cloned().unwrap_or_else(|| "default".into()),
                    enabled: true,
                }))
            })?;
            let profile = profiles.last().unwrap();
            if json {
                println!("{}", serde_json::to_string(profile)?);
            } else {
                println!("Saved Server {} ({})", profile.id, profile.label);
            }
        }
        "rename" if args.len() == 3 => {
            update_at(&path, |profiles, _| {
                let mut p = select(profiles, &args[1])?.clone();
                p.label = args[2].clone();
                Ok(MachineEvent::Saved(p))
            })?;
        }
        "enable" | "disable" | "remove" if args.len() == 2 => {
            update_at(&path, |profiles, _| {
                let mut p = select(profiles, &args[1])?.clone();
                if verb == "remove" {
                    Ok(MachineEvent::Removed(p.id))
                } else {
                    p.enabled = verb == "enable";
                    Ok(MachineEvent::Saved(p))
                }
            })?;
        }
        "status" | "sessions" if args.len() == 2 => {
            let profiles = list_at(&path)?;
            let p = select(&profiles, &args[1])?;
            if !p.enabled {
                return Err(std::io::Error::other("Server is disabled"));
            }
            let catalog = discover(&p.target)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"server":p,"state":"reachable_snapshot","workspaces":catalog.workspaces})
                );
            } else {
                println!("{}: reachable snapshot", p.label);
                for w in catalog.workspaces {
                    println!(
                        "{}\t{}\t{} Projects\t{} Tabs\t{} Panes",
                        w.name,
                        if w.running { "running" } else { "stopped" },
                        w.projects,
                        w.windows,
                        w.panes
                    );
                }
            }
        }
        "connect" | "reconnect" if args.len() == 2 => {
            let profiles = list_at(&path)?;
            let p = select(&profiles, &args[1])?;
            if !p.enabled {
                return Err(std::io::Error::other("Server is disabled"));
            }
            return Ok(super::remote::cmd_remote(&[
                p.target.clone(),
                p.workspace.clone(),
            ]));
        }
        _ => {
            eprintln!("usage: ut machine list [--json] | add LABEL SSH_TARGET [WORKSPACE] | rename ID LABEL | enable|disable|remove ID | status|sessions ID [--json] | connect|reconnect ID");
            return Ok(2);
        }
    }
    Ok(0)
}

pub(super) fn catalog() -> i32 {
    let mut workspaces: std::collections::BTreeMap<String, uniterm_proto::WorkspaceInfo> =
        uniterm_server::workspace_catalog::list()
            .into_iter()
            .map(|(name, definition)| {
                (
                    name.clone(),
                    uniterm_proto::WorkspaceInfo {
                        name,
                        projects: definition.projects.len() as u32,
                        windows: definition.tab_count() as u32,
                        panes: 0,
                        running: false,
                    },
                )
            })
            .collect();
    for (name, path, windows, panes) in super::running_workspace_sockets() {
        let projects =
            uniterm_client::workspace_request(&path, uniterm_proto::ClientMessage::WorkspaceState)
                .map(|(_, _, projects)| projects.len() as u32)
                .unwrap_or(0);
        workspaces.insert(
            name.clone(),
            uniterm_proto::WorkspaceInfo {
                name,
                projects,
                windows,
                panes,
                running: true,
            },
        );
    }
    println!(
        "{}",
        serde_json::json!({"protocol":uniterm_proto::WIRE_PROTOCOL_VERSION,"workspaces":workspaces.into_values().collect::<Vec<_>>() })
    );
    0
}
