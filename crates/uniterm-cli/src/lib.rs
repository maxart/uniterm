//! The `uniterm` CLI front-end, shared by the `uniterm` and `ut` binaries.
//!
//! Phase 1 subcommands: bare `[name]` (attach-or-create), `serve` (foreground
//! server), `attach`, `new-session`, `ls`, and `kill`. Richer parsing and the
//! full command language land in Phase 3; see `docs/10`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use uniterm_proto::{
    configured_default_workspace, merge_default_workspace, validate_workspace_name,
    MAX_WORKSPACE_NAME_BYTES,
};
use uniterm_server::server::{default_socket_path, socket_dir, WorkspaceLock};

mod desktop_migration;
mod remote;

fn checked_socket_path(name: &str) -> Result<PathBuf, String> {
    validate_workspace_name(name)
        .map_err(|error| format!("invalid Workspace name '{name}': {error}"))?;
    Ok(default_socket_path(name))
}

enum SocketHealth {
    Live {
        windows: u32,
        panes: u32,
    },
    /// The kernel proved that no listener is reachable at this pathname.
    Stale,
    /// Permission, timeout, protocol, and other failures do not prove death.
    Indeterminate(std::io::Error),
}

fn probe_socket(path: &Path) -> SocketHealth {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return SocketHealth::Stale,
        Err(error) => return SocketHealth::Indeterminate(error),
        Ok(metadata) if !metadata.file_type().is_socket() => {
            return SocketHealth::Indeterminate(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "socket path is not a Unix socket",
            ));
        }
        Ok(_) => {}
    }
    classify_socket_query(uniterm_client::query_info(path))
}

fn classify_socket_query(query: std::io::Result<(u32, u32)>) -> SocketHealth {
    match query {
        Ok((windows, panes)) => SocketHealth::Live { windows, panes },
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            SocketHealth::Stale
        }
        Err(error) => SocketHealth::Indeterminate(error),
    }
}

fn indeterminate_socket_error(name: &str, error: &std::io::Error) -> String {
    format!(
        "Workspace '{name}' could not be verified ({error}); refusing to treat its socket as stale"
    )
}

fn lock_stopped_workspace(name: &str, socket: &Path) -> Result<WorkspaceLock, String> {
    WorkspaceLock::acquire(socket).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            format!(
                "Workspace '{name}' is still owned by a running server, even though its socket is unavailable"
            )
        } else {
            format!("could not lock Workspace '{name}' for maintenance: {error}")
        }
    })
}

/// Resolve the Workspace used by CLI commands that omit a name. Invalid
/// hand-edited values are ignored so they can never become unsafe path keys.
fn default_workspace() -> String {
    uniterm_server::server::config_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| configured_default_workspace(&text))
        .unwrap_or_else(|| "default".into())
}

fn save_default_workspace(name: &str) -> Result<(), String> {
    validate_workspace_name(name)
        .map_err(|error| format!("invalid Workspace name '{name}': {error}"))?;
    let path = uniterm_server::server::config_path()
        .ok_or_else(|| "HOME and XDG_CONFIG_HOME are unset".to_string())?;
    let parent = path
        .parent()
        .ok_or_else(|| "the config path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let merged = merge_default_workspace(&existing, name);
    let temporary = path.with_extension(format!("conf.{}.tmp", std::process::id()));
    let write = (|| -> std::io::Result<()> {
        use std::io::Write as _;

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(merged.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write.map_err(|error| error.to_string())
}

fn follow_default_workspace_rename(old: &str, new: &str) {
    if default_workspace() == old {
        if let Err(error) = save_default_workspace(new) {
            eprintln!(
                "uniterm workspace rename: renamed the Workspace, but could not update the default: {error}"
            );
        }
    }
}

/// Parse argv and dispatch; returns the process exit code.
pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rc = match args.first().map(String::as_str) {
        Some("serve") => cmd_serve(&args[1..]),
        Some("attach" | "a") => cmd_attach(&args[1..]),
        Some("remote" | "--remote") => remote::cmd_remote(&args[1..]),
        Some("remote-bridge") => remote::cmd_remote_bridge(&args[1..]),
        Some("remote-check") => remote::cmd_remote_check(&args[1..]),
        Some("workspace" | "ws") => cmd_workspace(&args[1..]),
        Some("project" | "p") => cmd_project(&args[1..]),
        Some("tab") => cmd_tab(&args[1..]),
        Some("agent") => cmd_agent(&args[1..]),
        Some("instruction" | "instructions" | "steer") => cmd_instruction(&args[1..]),
        Some("run" | "runs") => cmd_run(&args[1..]),
        Some("artifact" | "artifacts") => cmd_artifact(&args[1..]),
        Some("waiting" | "wait") => cmd_waiting(&args[1..]),
        Some("pane") => cmd_pane(&args[1..]),
        Some("config") => cmd_config(&args[1..]),
        Some("migrate") => cmd_migrate(&args[1..]),
        Some("new-workspace" | "new-session" | "new") => cmd_new_session(&args[1..]),
        Some("ls" | "list") => cmd_ls(),
        Some("kill") => cmd_kill(&args[1..]),
        Some("relay") => cmd_submit("relay", &args[1..]),
        Some("workflow") => cmd_submit("workflow", &args[1..]),
        Some("--skill") => {
            print!("{}", include_str!("../../../docs/uniterm-skill.md"));
            0
        }
        Some("help") if args.len() > 1 => print_help_search(&args[1..].join(" ")),
        Some("--version" | "-V") => {
            // A release build prints the bare version; any other build
            // carries a -dev+g<commit> suffix (see build.rs).
            println!(
                "uniterm {}{}",
                env!("CARGO_PKG_VERSION"),
                env!("UNITERM_VERSION_SUFFIX")
            );
            0
        }
        Some("--help" | "-h") => {
            print_help();
            0
        }
        // No args: attach to (or create) the configured default Workspace.
        None => cmd_up(&[]),
        // A bare name (not a flag/known command) is shorthand for "attach that
        // session, creating it if needed" - e.g. `ut work`.
        Some(name) if !name.starts_with('-') => cmd_up(&args),
        Some(other) => {
            eprintln!("uniterm: unknown option '{other}'");
            print_help();
            2
        }
    };
    rc
}

fn cmd_config(args: &[String]) -> i32 {
    if args.first().map(String::as_str) != Some("check") || args.len() > 2 {
        eprintln!("uniterm config: usage: ut config check [PATH]");
        return 2;
    }
    let path = args
        .get(1)
        .map(PathBuf::from)
        .or_else(uniterm_server::server::config_path);
    let Some(path) = path else {
        eprintln!("uniterm config check: HOME and XDG_CONFIG_HOME are unset");
        return 1;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "{}: valid (file does not exist; defaults apply)",
                path.display()
            );
            return 0;
        }
        Err(error) => {
            eprintln!("uniterm config check: {}: {error}", path.display());
            return 1;
        }
    };
    let diagnostics = uniterm_core::Config::diagnostics(&text);
    if diagnostics.is_empty() {
        println!("{}: valid", path.display());
        return 0;
    }
    for diagnostic in &diagnostics {
        eprintln!(
            "{}:{}: {}",
            path.display(),
            diagnostic.line,
            diagnostic.message
        );
    }
    eprintln!("uniterm config check: {} error(s)", diagnostics.len());
    1
}

fn shell() -> String {
    shell_or_default(std::env::var("SHELL").ok(), cfg!(target_os = "android"))
}

fn terminal_safe(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() && *character != '\u{7f}')
        .collect()
}

fn shell_or_default(shell: Option<String>, android: bool) -> String {
    shell.unwrap_or_else(|| if android { "sh" } else { "/bin/sh" }.into())
}

/// `uniterm relay submit <token> [--status done|failed] [--verdict approved|fix|replan] [--summary TEXT]`
/// (and the identical `uniterm workflow submit ...`). This is the completion
/// contract's client end (`docs/07`): an agent signals it is done by calling
/// this with the per-activation token embedded in its injected prompt.
///
/// The pure decision engine lives in `uniterm_core::orchestrate`; the parsed
/// submission is delivered to the session named by `$UNITERM_SOCKET` (exported
/// into every pane the server spawns) and routed into the live run there.
fn cmd_submit(kind: &str, args: &[String]) -> i32 {
    if args.first().map(String::as_str) != Some("submit") {
        eprintln!("uniterm {kind}: usage: {kind} submit <token> [--status done|failed] [--verdict approved|fix|replan] [--summary TEXT]");
        return 2;
    }
    let rest = &args[1..];
    let Some(token) = rest.first().and_then(|t| t.parse::<u64>().ok()) else {
        eprintln!("uniterm {kind}: submit needs a numeric <token>");
        return 2;
    };
    let mut status = "done".to_string();
    let mut verdict: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut artifacts = Vec::new();
    let mut i = 1;
    while i < rest.len() {
        match rest[i].as_str() {
            "--status" => {
                i += 1;
                if let Some(v) = rest.get(i) {
                    status = v.clone();
                }
            }
            "--verdict" => {
                i += 1;
                verdict = rest.get(i).cloned();
            }
            "--summary" => {
                i += 1;
                summary = rest.get(i).cloned();
            }
            "--artifact" => {
                i += 1;
                if let Some(path) = rest.get(i) {
                    artifacts.push(parse_artifact_claim(path));
                }
            }
            other => {
                eprintln!("uniterm {kind}: unknown flag '{other}'");
                return 2;
            }
        }
        i += 1;
    }
    if !matches!(status.as_str(), "done" | "needs_input" | "failed") {
        eprintln!("uniterm {kind}: --status must be done|needs_input|failed");
        return 2;
    }
    if let Some(v) = &verdict {
        if !matches!(v.as_str(), "approved" | "fix" | "replan") {
            eprintln!("uniterm {kind}: --verdict must be approved|fix|replan");
            return 2;
        }
        if kind == "relay" {
            eprintln!("uniterm relay: a verdict applies to workflows, not relay turns");
            return 2;
        }
    }
    // Deliver to the session this pane belongs to: the server exports
    // UNITERM_SOCKET into every pane it spawns.
    let Some(sock) = std::env::var_os("UNITERM_SOCKET").map(PathBuf::from) else {
        eprintln!(
            "uniterm {kind}: not inside a uniterm pane (UNITERM_SOCKET unset); nothing delivered"
        );
        return 1;
    };
    if !sock.exists() {
        eprintln!(
            "uniterm {kind}: Workspace socket {} is gone; nothing delivered",
            sock.display()
        );
        return 1;
    }
    let orchestration_kind = if kind == "relay" {
        uniterm_proto::OrchestrationKind::Relay
    } else {
        uniterm_proto::OrchestrationKind::Workflow
    };
    let submission_status = match status.as_str() {
        "done" => uniterm_proto::SubmissionStatus::Done,
        "needs_input" => uniterm_proto::SubmissionStatus::NeedsInput,
        "failed" => uniterm_proto::SubmissionStatus::Failed,
        _ => unreachable!(),
    };
    match uniterm_client::orchestration_submit(
        &sock,
        orchestration_kind,
        token,
        submission_status,
        verdict.clone(),
        summary.clone().unwrap_or_default(),
        artifacts,
    ) {
        Ok(()) => {
            let mut line = format!("uniterm {kind}: submitted (token {token}, status {status}");
            if let Some(v) = verdict {
                line.push_str(&format!(", verdict {v}"));
            }
            if let Some(s) = summary {
                line.push_str(&format!(", summary \"{s}\""));
            }
            line.push(')');
            println!("{line}");
            0
        }
        Err(e) => {
            eprintln!("uniterm {kind}: delivery failed: {e}");
            1
        }
    }
}

fn parse_artifact_claim(value: &str) -> uniterm_proto::ArtifactClaim {
    let typed = value.split_once('=').and_then(|(kind, path)| {
        let kind = match kind {
            "file" => uniterm_proto::ArtifactKind::File,
            "plan" => uniterm_proto::ArtifactKind::Plan,
            "patch" => uniterm_proto::ArtifactKind::Patch,
            "report" => uniterm_proto::ArtifactKind::Report,
            "test-evidence" => uniterm_proto::ArtifactKind::TestEvidence,
            "findings" => uniterm_proto::ArtifactKind::Findings,
            _ => return None,
        };
        Some((kind, path))
    });
    let (kind, path) = typed.unwrap_or((uniterm_proto::ArtifactKind::File, value));
    uniterm_proto::ArtifactClaim {
        kind,
        path: path.to_string(),
    }
}

/// `uniterm serve [name]` - bind the socket for `name` (the configured default
/// when omitted) and
/// run the user's shell in the pane. Foreground until the last pane exits.
fn cmd_serve(args: &[String]) -> i32 {
    let fallback = default_workspace();
    let name = args.first().map(String::as_str).unwrap_or(&fallback);
    let sock = match checked_socket_path(name) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("uniterm serve: {error}");
            return 2;
        }
    };
    println!("uniterm: serving '{name}' at {}", sock.display());
    println!("uniterm: attach from another terminal with:  uniterm attach {name}");
    match uniterm_server::run_server(&sock, &shell(), &[]) {
        Ok(()) => {
            println!("uniterm: Workspace ended; server stopped");
            0
        }
        Err(e) => {
            eprintln!("uniterm serve: {e}");
            1
        }
    }
}

/// `uniterm attach [name]` - attach a client to the named server.
fn cmd_attach(args: &[String]) -> i32 {
    let fallback = default_workspace();
    let name = args.first().map(String::as_str).unwrap_or(&fallback);
    let mut sock = match checked_socket_path(name) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("uniterm attach: {error}");
            return 2;
        }
    };
    if !sock.exists() {
        eprintln!(
            "uniterm attach: no Workspace '{name}' at {} (start one with `uniterm workspace new {name}`)",
            sock.display()
        );
        return 1;
    }
    let mut open_workspaces = false;
    loop {
        let options = uniterm_client::AttachOptions {
            open_workspaces,
            remote: false,
        };
        match uniterm_client::attach_with_options(&sock, options) {
            Ok(uniterm_client::AttachOutcome::Exit) => return 0,
            Ok(uniterm_client::AttachOutcome::ReviveWorkspace(name)) => {
                if let Err(error) = ensure_workspace_running(&name) {
                    eprintln!("uniterm workspace switch: {error}");
                    return 1;
                }
                sock = match checked_socket_path(&name) {
                    Ok(socket) => socket,
                    Err(error) => {
                        eprintln!("uniterm workspace switch: {error}");
                        return 1;
                    }
                };
                open_workspaces = false;
            }
            Ok(uniterm_client::AttachOutcome::RemoteWorkspace(_)) => {
                eprintln!("uniterm attach: remote Workspace handoff on a local socket");
                return 1;
            }
            Ok(uniterm_client::AttachOutcome::MigrateDesktop) => {
                let migration = run_migration_command(&["from-desktop".into()], true);
                let handoff = migration_handoff(&sock, &migration.targets);
                sock = handoff.socket;
                open_workspaces = handoff.open_workspaces;
                if !sock.exists() {
                    return migration.exit_code;
                }
            }
            Err(e) => {
                eprintln!("uniterm attach: {e}");
                return 1;
            }
        }
    }
}

/// `ut [name]` - attach to `name`, creating the session first if it does not
/// exist yet. The zero-friction entry point: `ut work` just works.
fn cmd_up(args: &[String]) -> i32 {
    let fallback = default_workspace();
    let name = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
        .unwrap_or(&fallback);
    if let Err(error) = checked_socket_path(name) {
        eprintln!("uniterm: {error}");
        return 2;
    }
    match ensure_workspace_available(name) {
        Ok(_) => {}
        Err(error) => {
            eprintln!("uniterm: {error}");
            return 1;
        }
    }
    cmd_attach(&[name.to_string()])
}

/// Resolve one Workspace and start its detached server when necessary.
///
/// This is shared by the local zero-friction entry point and the SSH bridge so
/// a remote attach has the same create-or-attach behavior as `ut NAME`.
fn ensure_workspace_available(name: &str) -> Result<PathBuf, String> {
    let sock = checked_socket_path(name)?;
    match probe_socket(&sock) {
        SocketHealth::Live { .. } => return Ok(sock),
        SocketHealth::Stale => {}
        SocketHealth::Indeterminate(error) => {
            return Err(indeterminate_socket_error(name, &error));
        }
    }
    let mut child = spawn_detached_server(name).map_err(|error| error.to_string())?;
    wait_for_server(&mut child, &sock, name).map_err(|error| error.to_string())?;
    Ok(sock)
}

/// `uniterm new-session [-d] [name]` - start a detached server for `name` in the
/// background, then attach (unless `-d`).
fn cmd_new_session(args: &[String]) -> i32 {
    let detached = args.iter().any(|a| a == "-d" || a == "--detach");
    let fallback = default_workspace();
    let name = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
        .unwrap_or(&fallback);
    let sock = match checked_socket_path(name) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("uniterm workspace new: {error}");
            return 2;
        }
    };
    match probe_socket(&sock) {
        SocketHealth::Live { .. } => {
            eprintln!("uniterm workspace new: Workspace '{name}' already exists");
            return if detached {
                1
            } else {
                cmd_attach(&[name.to_string()])
            };
        }
        SocketHealth::Stale => {}
        SocketHealth::Indeterminate(error) => {
            eprintln!(
                "uniterm workspace new: {}",
                indeterminate_socket_error(name, &error)
            );
            return 1;
        }
    }
    let mut child = match spawn_detached_server(name) {
        Ok(child) => child,
        Err(error) => {
            eprintln!("uniterm workspace new: {error}");
            return 1;
        }
    };
    if let Err(error) = wait_for_server(&mut child, &sock, name) {
        eprintln!("uniterm workspace new: {error}");
        return 1;
    }
    if detached {
        println!("uniterm: started detached Workspace '{name}'");
        0
    } else {
        cmd_attach(&[name.to_string()])
    }
}

/// Re-exec `uniterm serve <name>` in its own session (setsid), detached from our
/// controlling terminal and stdio, so it survives this process exiting.
fn spawn_detached_server(name: &str) -> std::io::Result<std::process::Child> {
    spawn_detached_server_at(name, None)
}

fn spawn_detached_server_at(
    name: &str,
    cwd: Option<&std::path::Path>,
) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    // SAFETY: setsid in the forked child before exec is async-signal-safe and
    // only detaches the new process into its own session.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    cmd.spawn()
}

/// Wait until a newly detached Workspace answers its control protocol.
/// A socket file alone is insufficient because it may be stale or the server
/// may have failed during startup after binding it.
fn wait_for_server(
    child: &mut std::process::Child,
    socket: &std::path::Path,
    name: &str,
) -> std::io::Result<()> {
    wait_for_server_until(
        child,
        socket,
        name,
        std::time::Instant::now() + std::time::Duration::from_secs(5),
    )
}

fn wait_for_server_until(
    child: &mut std::process::Child,
    socket: &std::path::Path,
    name: &str,
    deadline: std::time::Instant,
) -> std::io::Result<()> {
    loop {
        if socket.exists() && uniterm_client::query_info(socket).is_ok() {
            // Startup diagnostics are only needed until readiness. Dropping
            // the pipe preserves the prior detached behavior after this point.
            child.stderr.take();
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            let details = child_stderr(child);
            return Err(std::io::Error::other(format!(
                "Workspace '{name}' server exited during startup ({status}){details}"
            )));
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let details = child_stderr(child);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("Workspace '{name}' did not become ready within 5 seconds{details}"),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn child_stderr(child: &mut std::process::Child) -> String {
    use std::io::Read;

    let Some(mut stderr) = child.stderr.take() else {
        return String::new();
    };
    let mut bytes = Vec::new();
    if stderr.read_to_end(&mut bytes).is_err() || bytes.is_empty() {
        return String::new();
    }
    format!(": {}", String::from_utf8_lossy(&bytes).trim())
}

#[derive(Clone, Debug)]
struct ListedWorkspace {
    running: bool,
    projects: usize,
    tabs: usize,
    panes: u32,
}

/// Discover responsive Workspace servers once from the runtime socket
/// directory. Probing is strictly read-only: a timeout or permission failure
/// never proves that a listener is stale.
fn running_workspace_sockets() -> Vec<(String, PathBuf, u32, u32)> {
    let Ok(entries) = std::fs::read_dir(socket_dir()) else {
        return Vec::new();
    };
    let mut workspaces = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("sock") {
            continue;
        }
        if !std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_socket())
        {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|value| value.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if let SocketHealth::Live { windows, panes } = probe_socket(&path) {
            workspaces.push((name, path, windows, panes));
        }
    }
    workspaces.sort_by(|left, right| left.0.cmp(&right.0));
    workspaces
}

/// `uniterm ls` - list responsive and stopped Workspaces without mutating
/// socket state.
fn cmd_ls() -> i32 {
    let mut workspaces: BTreeMap<String, ListedWorkspace> =
        uniterm_server::workspace_catalog::list()
            .into_iter()
            .map(|(name, definition)| {
                (
                    name,
                    ListedWorkspace {
                        running: false,
                        projects: definition.projects.len(),
                        tabs: definition.tab_count(),
                        panes: 0,
                    },
                )
            })
            .collect();
    for (name, path, windows, panes) in running_workspace_sockets() {
        let projects =
            uniterm_client::workspace_request(&path, uniterm_proto::ClientMessage::WorkspaceState)
                .map(|(_, _, items)| items.len())
                .unwrap_or(0);
        workspaces.insert(
            name,
            ListedWorkspace {
                running: true,
                projects,
                tabs: windows as usize,
                panes,
            },
        );
    }
    print_workspaces(&workspaces)
}

fn print_workspaces(workspaces: &BTreeMap<String, ListedWorkspace>) -> i32 {
    if workspaces.is_empty() {
        println!("no Workspaces");
        return 0;
    }
    for (name, workspace) in workspaces {
        let project_label = if workspace.projects == 1 {
            "Project"
        } else {
            "Projects"
        };
        let tab_label = if workspace.tabs == 1 { "Tab" } else { "Tabs" };
        if workspace.running {
            let pane_label = if workspace.panes == 1 {
                "pane"
            } else {
                "panes"
            };
            println!(
                "{name}\trunning, {} {project_label}, {} {tab_label}, {} {pane_label}",
                workspace.projects, workspace.tabs, workspace.panes
            );
        } else {
            println!(
                "{name}\tstopped, {} {project_label}, {} {tab_label}",
                workspace.projects, workspace.tabs
            );
        }
    }
    0
}

/// Manage the top-level Workspace catalog. A Workspace is one durable server
/// and remains compatible with the historical `new-session` command.
fn cmd_workspace(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None | Some("list" | "ls") => cmd_ls(),
        Some("default" | "set-default") => {
            let Some(name) = args.get(1) else {
                println!("{}", default_workspace());
                return 0;
            };
            if args.len() != 2 {
                eprintln!("uniterm workspace default: usage: ut workspace default [name]");
                return 2;
            }
            match save_default_workspace(name) {
                Ok(()) => {
                    println!("uniterm: default Workspace is now '{name}'");
                    0
                }
                Err(error) => {
                    eprintln!("uniterm workspace default: {error}");
                    1
                }
            }
        }
        Some("new") => cmd_new_session(&args[1..]),
        Some("switch" | "attach") => cmd_up(&args[1..]),
        Some("stop" | "remove" | "kill") => cmd_kill(&args[1..]),
        Some("forget") => cmd_forget(&args[1..]),
        Some("rename") => {
            let Some(old) = args.get(1) else {
                eprintln!("uniterm workspace rename: usage: ut workspace rename <old> <new>");
                return 2;
            };
            let Some(new) = args.get(2) else {
                eprintln!("uniterm workspace rename: usage: ut workspace rename <old> <new>");
                return 2;
            };
            let socket = match checked_socket_path(old) {
                Ok(socket) => socket,
                Err(error) => {
                    eprintln!("uniterm workspace rename: {error}");
                    return 2;
                }
            };
            if let Err(error) = validate_workspace_name(new) {
                eprintln!("uniterm workspace rename: invalid Workspace name '{new}': {error}");
                return 2;
            }
            match probe_socket(&socket) {
                SocketHealth::Live { .. } => {
                    match uniterm_client::workspace_request(
                        &socket,
                        uniterm_proto::ClientMessage::RenameSession { name: new.clone() },
                    ) {
                        Ok((name, _, _)) => {
                            follow_default_workspace_rename(old, &name);
                            println!("uniterm: renamed Workspace '{old}' to '{name}'");
                            0
                        }
                        Err(error) => {
                            eprintln!("uniterm workspace rename: {error}");
                            1
                        }
                    }
                }
                SocketHealth::Indeterminate(error) => {
                    eprintln!(
                        "uniterm workspace rename: {}",
                        indeterminate_socket_error(old, &error)
                    );
                    1
                }
                SocketHealth::Stale if !uniterm_server::workspace_catalog::exists(old) => {
                    eprintln!("uniterm workspace rename: no Workspace '{old}'");
                    1
                }
                SocketHealth::Stale if workspace_present(new) => {
                    eprintln!("uniterm workspace rename: Workspace '{new}' already exists");
                    1
                }
                SocketHealth::Stale => {
                    let _old_lock = match lock_stopped_workspace(old, &socket) {
                        Ok(lock) => lock,
                        Err(error) => {
                            eprintln!("uniterm workspace rename: {error}");
                            return 1;
                        }
                    };
                    let new_socket = default_socket_path(new);
                    let _new_lock = match lock_stopped_workspace(new, &new_socket) {
                        Ok(lock) => lock,
                        Err(error) => {
                            eprintln!("uniterm workspace rename: {error}");
                            return 1;
                        }
                    };
                    if uniterm_server::persist::exists(old) {
                        if let Err(error) = uniterm_server::persist::rename(old, new) {
                            eprintln!("uniterm workspace rename: {error}");
                            return 1;
                        }
                    }
                    if uniterm_server::eventlog::exists(old) {
                        if let Err(error) = uniterm_server::eventlog::rename(old, new) {
                            eprintln!("uniterm workspace rename: {error}");
                            return 1;
                        }
                    }
                    match uniterm_server::workspace_catalog::rename(old, new) {
                        Ok(()) => {
                            follow_default_workspace_rename(old, new);
                            println!("uniterm: renamed stopped Workspace '{old}' to '{new}'");
                            0
                        }
                        Err(error) => {
                            eprintln!("uniterm workspace rename: {error}");
                            1
                        }
                    }
                }
            }
        }
        Some(other) => {
            eprintln!("uniterm workspace: unknown command '{other}'");
            2
        }
    }
}

fn project_socket(args: &[String]) -> Result<(PathBuf, Vec<String>), String> {
    let mut workspace = std::env::var_os("UNITERM_SOCKET").map(PathBuf::from);
    let mut rest = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--workspace" || args[index] == "-w" {
            if let Some(name) = args.get(index + 1) {
                workspace = Some(checked_socket_path(name)?);
                index += 2;
                continue;
            }
        }
        rest.push(args[index].clone());
        index += 1;
    }
    Ok((
        workspace.unwrap_or_else(|| default_socket_path(&default_workspace())),
        rest,
    ))
}

fn resolve_project(
    projects: &[uniterm_proto::ProjectInfo],
    value: &str,
) -> Option<uniterm_core::ProjectId> {
    value
        .parse::<u64>()
        .ok()
        .map(uniterm_core::ProjectId)
        .filter(|id| projects.iter().any(|project| project.id == *id))
        .or_else(|| {
            projects
                .iter()
                .find(|project| project.name.eq_ignore_ascii_case(value))
                .map(|project| project.id)
        })
}

/// Manage Projects within a Workspace. Names or stable numeric ids are
/// accepted for switch, rename, move, and remove.
fn cmd_project(args: &[String]) -> i32 {
    let (sock, args) = match project_socket(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("uniterm project: {error}");
            return 2;
        }
    };
    if args.first().map(String::as_str) == Some("worktree") {
        return cmd_project_worktree(&sock, &args[1..]);
    }
    let request = match args.first().map(String::as_str) {
        None | Some("list" | "ls") => uniterm_proto::ClientMessage::WorkspaceState,
        Some("add" | "new") => {
            let (Some(name), Some(root)) = (args.get(1), args.get(2)) else {
                eprintln!(
                    "uniterm project add: usage: ut project add <name> <root> [-w Workspace]"
                );
                return 2;
            };
            let Ok(root) = std::fs::canonicalize(root) else {
                eprintln!("uniterm project add: root does not exist: {root}");
                return 1;
            };
            if !root.is_dir() {
                eprintln!(
                    "uniterm project add: root is not a directory: {}",
                    root.display()
                );
                return 1;
            }
            uniterm_proto::ClientMessage::ProjectCreate {
                name: name.clone(),
                root: root.to_string_lossy().into_owned(),
            }
        }
        Some(action @ ("switch" | "rename" | "move" | "remove" | "metadata")) => {
            let Some(value) = args.get(1) else {
                eprintln!("uniterm project {action}: a Project name or id is required");
                return 2;
            };
            let Ok((_, _, projects)) = uniterm_client::workspace_request(
                &sock,
                uniterm_proto::ClientMessage::WorkspaceState,
            ) else {
                eprintln!(
                    "uniterm project: Workspace is not running at {}",
                    sock.display()
                );
                return 1;
            };
            let Some(project) = resolve_project(&projects, value) else {
                eprintln!("uniterm project {action}: unknown Project '{value}'");
                return 1;
            };
            match action {
                "switch" => uniterm_proto::ClientMessage::ProjectSwitch { project },
                "remove" => {
                    if projects
                        .iter()
                        .find(|item| item.id == project)
                        .is_some_and(|item| item.worktree.is_some())
                    {
                        eprintln!(
                            "uniterm project remove: use 'ut project worktree remove {value}' so Git can verify the worktree"
                        );
                        return 2;
                    }
                    // An explicit `ut project remove` is the operator's
                    // confirmation; the server still records the decision.
                    uniterm_proto::ClientMessage::ProjectRemove {
                        project,
                        confirmed: true,
                    }
                }
                "rename" => {
                    let Some(name) = args.get(2) else {
                        eprintln!("uniterm project rename: a new name is required");
                        return 2;
                    };
                    uniterm_proto::ClientMessage::ProjectRename {
                        project,
                        name: name.clone(),
                    }
                }
                "move" => {
                    let direction = match args.get(2).map(String::as_str) {
                        Some("up") => uniterm_proto::ProjectMoveDirection::Up,
                        Some("down") => uniterm_proto::ProjectMoveDirection::Down,
                        _ => {
                            eprintln!(
                                "uniterm project move: usage: ut project move <project> <up|down>"
                            );
                            return 2;
                        }
                    };
                    uniterm_proto::ClientMessage::ProjectMove { project, direction }
                }
                "metadata" => {
                    let (Some(key), Some(value)) = (args.get(2), args.get(3)) else {
                        eprintln!("uniterm project metadata: usage: ut project metadata <project> <key> <value>");
                        return 2;
                    };
                    uniterm_proto::ClientMessage::ProjectMetadata {
                        project,
                        key: key.clone(),
                        value: value.clone(),
                    }
                }
                _ => unreachable!(),
            }
        }
        Some(other) => {
            eprintln!("uniterm project: unknown command '{other}'");
            return 2;
        }
    };
    match uniterm_client::workspace_request(&sock, request) {
        Ok((workspace, _, projects)) => {
            println!("Workspace {workspace}");
            for project in projects {
                let mark = if project.active { '*' } else { ' ' };
                println!(
                    "{mark} {}\t{}\t{} Tabs, {} panes{}",
                    project.id.0,
                    project.name,
                    project.tabs,
                    project.panes,
                    if project.attention > 0 {
                        format!(", {} need attention", project.attention)
                    } else {
                        String::new()
                    }
                );
                println!("    {}", project.root);
            }
            0
        }
        Err(error) => {
            eprintln!("uniterm project: {error}");
            1
        }
    }
}

fn cmd_project_worktree(sock: &Path, args: &[String]) -> i32 {
    use uniterm_proto::WorktreeOperation;

    let operation = match args.first().map(String::as_str) {
        None | Some("list" | "ls") if args.len() <= 1 => WorktreeOperation::List,
        Some("add") if (4..=5).contains(&args.len()) => {
            let mut path = PathBuf::from(&args[3]);
            if path.is_relative() {
                let Ok(current) = std::env::current_dir() else {
                    eprintln!("uniterm project worktree: could not resolve the current directory");
                    return 1;
                };
                path = current.join(path);
            }
            WorktreeOperation::Add {
                name: args[1].clone(),
                repository: args[2].clone(),
                path: path.to_string_lossy().into_owned(),
                base: args.get(4).cloned(),
            }
        }
        Some(action @ ("open" | "remove" | "cleanup")) => {
            let Some(value) = args.get(1) else {
                eprintln!("uniterm project worktree {action}: a Project name or id is required");
                return 2;
            };
            let Ok((_, _, projects)) = uniterm_client::workspace_request(
                sock,
                uniterm_proto::ClientMessage::WorkspaceState,
            ) else {
                eprintln!(
                    "uniterm project worktree: Workspace is not running at {}",
                    sock.display()
                );
                return 1;
            };
            let Some(project) = resolve_project(&projects, value) else {
                eprintln!("uniterm project worktree {action}: unknown Project '{value}'");
                return 1;
            };
            match action {
                "open" if args.len() == 2 => WorktreeOperation::Open { project },
                "cleanup" if args.len() == 2 => WorktreeOperation::Cleanup { project },
                "remove" => {
                    let force = args[2..].iter().any(|arg| arg == "--force");
                    let yes = args[2..].iter().any(|arg| arg == "--yes");
                    let valid = args[2..]
                        .iter()
                        .all(|arg| matches!(arg.as_str(), "--force" | "--yes"));
                    if !valid || force != yes {
                        eprintln!(
                            "uniterm project worktree remove: destructive removal requires both --force and --yes"
                        );
                        return 2;
                    }
                    WorktreeOperation::Remove { project, force }
                }
                _ => {
                    eprintln!("uniterm project worktree {action}: unexpected arguments");
                    return 2;
                }
            }
        }
        _ => {
            eprintln!(
                "uniterm project worktree: usage:\n  ut project worktree list\n  ut project worktree add NAME REPO PATH [BASE]\n  ut project worktree open PROJECT\n  ut project worktree remove PROJECT [--force --yes]\n  ut project worktree cleanup PROJECT"
            );
            return 2;
        }
    };

    match uniterm_client::worktree_request(sock, operation) {
        Ok(result) => {
            for item in &result.items {
                let state = match item.state {
                    uniterm_proto::WorktreeState::Active => "active",
                    uniterm_proto::WorktreeState::Missing => "missing",
                    uniterm_proto::WorktreeState::Prunable => "prunable",
                };
                let dirty = if item.dirty { ", dirty" } else { "" };
                println!(
                    "{}\t{}\t{}{}\n    {}\n    repository: {}",
                    item.registration.project.0,
                    item.registration.project_name,
                    state,
                    dirty,
                    item.registration.path,
                    item.registration.repository
                );
                println!(
                    "    branch: {}\thead: {}",
                    item.current_branch
                        .as_deref()
                        .unwrap_or(&item.registration.branch),
                    item.head
                        .as_deref()
                        .unwrap_or(&item.registration.created_head)
                );
            }
            if let Some(error) = result.error {
                eprintln!("uniterm project worktree: {error}");
            }
            if result.accepted {
                0
            } else {
                1
            }
        }
        Err(error) => {
            eprintln!("uniterm project worktree: {error}");
            1
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MigrationConflictPolicy {
    Prompt,
    Rename,
    Skip,
    Merge,
    Archive,
}

struct MigrationOptions {
    data_dir: Option<PathBuf>,
    workspace: Option<String>,
    dry_run: bool,
    yes: bool,
    conflict: MigrationConflictPolicy,
}

struct PreparedWorkspace {
    source_name: String,
    key: String,
    workspace: uniterm_proto::ImportedWorkspace,
}

#[derive(Default)]
struct MigrationReport {
    targets: Vec<PathBuf>,
}

struct MigrationCommandResult {
    exit_code: i32,
    targets: Vec<PathBuf>,
}

struct MigrationHandoff {
    socket: PathBuf,
    open_workspaces: bool,
}

fn cmd_migrate(args: &[String]) -> i32 {
    run_migration_command(args, false).exit_code
}

fn run_migration_command(args: &[String], from_settings: bool) -> MigrationCommandResult {
    let Some(command) = args.first().map(String::as_str) else {
        eprintln!("uniterm migrate: usage: ut migrate from-desktop [options]");
        return MigrationCommandResult {
            exit_code: 2,
            targets: Vec::new(),
        };
    };
    if command != "from-desktop" {
        eprintln!("uniterm migrate: unknown source '{command}'");
        return MigrationCommandResult {
            exit_code: 2,
            targets: Vec::new(),
        };
    }
    if args[1..]
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!(
            "ut migrate from-desktop [--dry-run] [--workspace NAME] [--data-dir PATH]\n\
             \x20                         [--on-conflict prompt|rename|skip|merge|archive] [-y]"
        );
        return MigrationCommandResult {
            exit_code: 0,
            targets: Vec::new(),
        };
    }
    let options = match parse_migration_options(&args[1..]) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("uniterm migrate: {error}");
            return MigrationCommandResult {
                exit_code: 2,
                targets: Vec::new(),
            };
        }
    };
    let result = run_desktop_migration(options);
    if from_settings {
        println!();
        println!("Press Enter to return to Uniterm.");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
    match result {
        Ok(report) => MigrationCommandResult {
            exit_code: 0,
            targets: report.targets,
        },
        Err(error) => {
            eprintln!("uniterm migrate: {error}");
            MigrationCommandResult {
                exit_code: 1,
                targets: Vec::new(),
            }
        }
    }
}

fn migration_handoff(current: &Path, targets: &[PathBuf]) -> MigrationHandoff {
    let socket = targets
        .iter()
        .find(|target| target.as_path() == current)
        .or_else(|| targets.first())
        .cloned()
        .unwrap_or_else(|| current.to_path_buf());
    MigrationHandoff {
        socket,
        open_workspaces: targets.len() > 1,
    }
}

fn parse_migration_options(args: &[String]) -> Result<MigrationOptions, String> {
    let mut options = MigrationOptions {
        data_dir: None,
        workspace: None,
        dry_run: false,
        yes: false,
        conflict: MigrationConflictPolicy::Prompt,
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--dry-run" => options.dry_run = true,
            "--yes" | "-y" => options.yes = true,
            "--all" => {}
            "--data-dir" => {
                index += 1;
                options.data_dir = Some(PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| "--data-dir requires a path".to_string())?,
                ));
            }
            "--workspace" => {
                index += 1;
                options.workspace = Some(
                    args.get(index)
                        .ok_or_else(|| "--workspace requires a name or id".to_string())?
                        .clone(),
                );
            }
            "--on-conflict" => {
                index += 1;
                options.conflict = match args.get(index).map(String::as_str) {
                    Some("prompt") => MigrationConflictPolicy::Prompt,
                    Some("rename") => MigrationConflictPolicy::Rename,
                    Some("skip") => MigrationConflictPolicy::Skip,
                    Some("merge") => MigrationConflictPolicy::Merge,
                    Some("archive") => MigrationConflictPolicy::Archive,
                    Some(value) => {
                        return Err(format!(
                            "unknown conflict policy '{value}' (use prompt|rename|skip|merge|archive)"
                        ));
                    }
                    None => return Err("--on-conflict requires a policy".into()),
                };
            }
            other => return Err(format!("unknown option '{other}'")),
        }
        index += 1;
    }
    Ok(options)
}

fn run_desktop_migration(options: MigrationOptions) -> Result<MigrationReport, String> {
    use std::io::IsTerminal as _;

    let data_dir = desktop_migration::detect_data_dir(options.data_dir.as_deref())?;
    let source = desktop_migration::load(data_dir)?;
    println!("Uniterm Desktop data: {}", source.data_dir.display());
    for warning in &source.warnings {
        eprintln!("warning: {warning}");
    }

    let mut prepared = Vec::new();
    let mut unavailable = Vec::new();
    for workspace in source.workspaces.iter().filter(|workspace| {
        options.workspace.as_ref().is_none_or(|wanted| {
            workspace.name.eq_ignore_ascii_case(wanted) || workspace.source_id == *wanted
        })
    }) {
        let mut projects = Vec::new();
        let mut roots = std::collections::HashSet::new();
        for project in &workspace.projects {
            match std::fs::canonicalize(&project.path) {
                Ok(root) if root.is_dir() => {
                    if !roots.insert(root.clone()) {
                        eprintln!(
                            "skip duplicate Project path: {} / {} ({})",
                            workspace.name,
                            project.name,
                            root.display()
                        );
                        continue;
                    }
                    projects.push(uniterm_proto::ImportedProject {
                        source_id: project.source_id.clone(),
                        name: if project.name.is_empty() {
                            root.file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("Project")
                                .to_string()
                        } else {
                            project.name.clone()
                        },
                        root: root.to_string_lossy().into_owned(),
                        tabs: project.tabs.clone(),
                    });
                }
                _ => unavailable.push(format!(
                    "{} / {} ({})",
                    workspace.name, project.name, project.path
                )),
            }
        }
        if !projects.is_empty() {
            let key = if workspace.source_id == "default" {
                "default".into()
            } else {
                migration_workspace_key(&workspace.name)
            };
            prepared.push(PreparedWorkspace {
                source_name: workspace.name.clone(),
                key,
                workspace: workspace.imported(projects),
            });
        }
    }
    if options.workspace.is_some() && prepared.is_empty() && unavailable.is_empty() {
        return Err("the requested Desktop Workspace was not found".into());
    }
    for project in &unavailable {
        eprintln!("skip missing Project path: {project}");
    }
    if prepared.is_empty() {
        return Err("no Desktop Workspaces have usable Project paths".into());
    }

    let project_count: usize = prepared
        .iter()
        .map(|workspace| workspace.workspace.projects.len())
        .sum();
    let tab_count: usize = prepared
        .iter()
        .flat_map(|workspace| &workspace.workspace.projects)
        .map(|project| project.tabs.len())
        .sum();
    println!(
        "Found {} Workspaces, {project_count} Projects, and {tab_count} Tabs.",
        prepared.len()
    );
    for workspace in &prepared {
        let conflict = workspace_present(&workspace.key);
        println!(
            "  {} -> {}  ({} Projects, {} Tabs{})",
            workspace.source_name,
            workspace.key,
            workspace.workspace.projects.len(),
            workspace
                .workspace
                .projects
                .iter()
                .map(|project| project.tabs.len())
                .sum::<usize>(),
            if conflict { ", conflict" } else { "" }
        );
    }
    if options.dry_run {
        println!("Dry run only. No CLI Workspace was changed.");
        return Ok(MigrationReport::default());
    }
    if options.conflict == MigrationConflictPolicy::Prompt
        && prepared
            .iter()
            .any(|workspace| workspace_present(&workspace.key))
        && !std::io::stdin().is_terminal()
    {
        return Err("a Workspace conflict requires --on-conflict rename|skip|merge|archive".into());
    }
    if !options.yes {
        if !std::io::stdin().is_terminal() {
            return Err("confirmation requires a terminal; pass --yes for automation".into());
        }
        if !prompt_yes_no("Import this hierarchy? [y/N] ")? {
            println!("Migration cancelled.");
            return Ok(MigrationReport::default());
        }
    }

    let mut report = MigrationReport::default();
    for workspace in prepared {
        let mut target = workspace.key.clone();
        let mut mode = uniterm_proto::WorkspaceImportMode::Fresh;
        if workspace_present(&target) {
            match resolve_conflict(&workspace, options.conflict)? {
                ConflictResolution::Skip => {
                    println!("Skipped Workspace '{}'.", workspace.source_name);
                    continue;
                }
                ConflictResolution::Cancel => {
                    println!("Migration cancelled; completed imports were left intact.");
                    return Ok(report);
                }
                ConflictResolution::Rename => {
                    target = unique_workspace_key(&format!("{}-desktop", target));
                }
                ConflictResolution::Merge => {
                    mode = uniterm_proto::WorkspaceImportMode::Merge;
                    ensure_workspace_running(&target)?;
                }
                ConflictResolution::Archive => {
                    let archive = archive_workspace(&target)?;
                    println!("Archived existing Workspace '{target}' as '{archive}'.");
                }
            }
        }

        let socket = checked_socket_path(&target)?;
        let fresh = mode == uniterm_proto::WorkspaceImportMode::Fresh;
        if fresh {
            let first_root = PathBuf::from(&workspace.workspace.projects[0].root);
            start_detached_workspace(&target, Some(&first_root))?;
        }
        match uniterm_client::import_workspace(&socket, workspace.workspace, mode) {
            Ok((projects, tabs, merged)) => {
                println!(
                    "Imported '{}' as '{target}': {projects} Projects added, {tabs} Tabs added, {merged} Projects merged.",
                    workspace.source_name
                );
                report.targets.push(socket);
            }
            Err(error) => {
                if fresh {
                    let _ = uniterm_client::kill_server(&socket);
                }
                return Err(format!("Workspace '{}': {error}", workspace.source_name));
            }
        }
    }
    println!(
        "Migration complete: {} Workspaces imported.",
        report.targets.len()
    );
    Ok(report)
}

enum ConflictResolution {
    Rename,
    Merge,
    Archive,
    Skip,
    Cancel,
}

fn resolve_conflict(
    workspace: &PreparedWorkspace,
    policy: MigrationConflictPolicy,
) -> Result<ConflictResolution, String> {
    use std::io::IsTerminal as _;
    match policy {
        MigrationConflictPolicy::Rename => Ok(ConflictResolution::Rename),
        MigrationConflictPolicy::Skip => Ok(ConflictResolution::Skip),
        MigrationConflictPolicy::Merge => Ok(ConflictResolution::Merge),
        MigrationConflictPolicy::Archive => Ok(ConflictResolution::Archive),
        MigrationConflictPolicy::Prompt => {
            if !std::io::stdin().is_terminal() {
                return Err(format!(
                    "Workspace '{}' already exists; pass --on-conflict rename|skip|merge|archive",
                    workspace.key
                ));
            }
            println!();
            println!("CLI Workspace '{}' already exists.", workspace.key);
            println!("  [r] Import as a new suffixed Workspace (recommended)");
            println!("  [m] Merge only missing Projects and Tabs");
            println!("  [a] Archive the existing Workspace, then import");
            println!("  [s] Skip this Workspace");
            println!("  [c] Cancel the remaining migration");
            loop {
                let answer = prompt_line("Choose r/m/a/s/c: ")?;
                match answer.trim().to_ascii_lowercase().as_str() {
                    "r" | "rename" => return Ok(ConflictResolution::Rename),
                    "m" | "merge" => return Ok(ConflictResolution::Merge),
                    "a" | "archive" => return Ok(ConflictResolution::Archive),
                    "s" | "skip" => return Ok(ConflictResolution::Skip),
                    "c" | "cancel" | "q" => return Ok(ConflictResolution::Cancel),
                    _ => eprintln!("Please choose r, m, a, s, or c."),
                }
            }
        }
    }
}

fn prompt_yes_no(prompt: &str) -> Result<bool, String> {
    Ok(matches!(
        prompt_line(prompt)?.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn prompt_line(prompt: &str) -> Result<String, String> {
    use std::io::Write as _;
    print!("{prompt}");
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|error| error.to_string())?;
    Ok(line)
}

fn migration_workspace_key(name: &str) -> String {
    let mut key = String::new();
    let mut separator = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            key.push(ch);
            separator = false;
        } else if !separator && !key.is_empty() {
            key.push('-');
            separator = true;
        }
        if key.len() >= 64 {
            break;
        }
    }
    while key.ends_with(['-', '.']) {
        key.pop();
    }
    key = key.trim_start_matches(['-', '.', '_']).to_string();
    if key.is_empty() || matches!(key.as_str(), "." | "..") {
        "desktop".into()
    } else {
        key
    }
}

fn workspace_present(name: &str) -> bool {
    let Ok(socket) = checked_socket_path(name) else {
        return false;
    };
    std::fs::symlink_metadata(&socket).is_ok()
        || uniterm_server::persist::exists(name)
        || uniterm_server::eventlog::exists(name)
        || uniterm_server::workspace_catalog::exists(name)
}

fn unique_workspace_key(base: &str) -> String {
    let base = bounded_workspace_key(base, None);
    if !workspace_present(&base) {
        return base;
    }
    for suffix in 2..10_000 {
        let candidate = bounded_workspace_key(&base, Some(&suffix.to_string()));
        if !workspace_present(&candidate) {
            return candidate;
        }
    }
    bounded_workspace_key(&base, Some(&std::process::id().to_string()))
}

fn bounded_workspace_key(base: &str, suffix: Option<&str>) -> String {
    let suffix_len = suffix.map_or(0, |value| value.len() + 1);
    let keep = MAX_WORKSPACE_NAME_BYTES.saturating_sub(suffix_len);
    let mut prefix = base.chars().take(keep).collect::<String>();
    while prefix.ends_with(['.', '-']) {
        prefix.pop();
    }
    if prefix.is_empty() {
        prefix.push_str("workspace");
    }
    match suffix {
        Some(suffix) => format!("{prefix}-{suffix}"),
        None => prefix,
    }
}

fn start_detached_workspace(name: &str, cwd: Option<&std::path::Path>) -> Result<(), String> {
    let sock = checked_socket_path(name)?;
    let mut child = spawn_detached_server_at(name, cwd).map_err(|error| error.to_string())?;
    wait_for_server(&mut child, &sock, name).map_err(|error| error.to_string())
}

fn ensure_workspace_running(name: &str) -> Result<(), String> {
    let socket = checked_socket_path(name)?;
    match probe_socket(&socket) {
        SocketHealth::Live { .. } => return Ok(()),
        SocketHealth::Stale => {}
        SocketHealth::Indeterminate(error) => {
            return Err(indeterminate_socket_error(name, &error));
        }
    }
    if !uniterm_server::persist::exists(name) && !uniterm_server::workspace_catalog::exists(name) {
        return Err(format!("Workspace '{name}' is no longer available"));
    }
    start_detached_workspace(name, None)
}

fn archive_workspace(name: &str) -> Result<String, String> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let archive = unique_workspace_key(&bounded_workspace_key(
        name,
        Some(&format!("before-desktop-{timestamp}")),
    ));
    let socket = checked_socket_path(name)?;
    let live = match probe_socket(&socket) {
        SocketHealth::Live { .. } => true,
        SocketHealth::Stale => false,
        SocketHealth::Indeterminate(error) => {
            return Err(indeterminate_socket_error(name, &error));
        }
    };
    let _offline_locks = if live {
        None
    } else {
        let archive_socket = checked_socket_path(&archive)?;
        Some((
            lock_stopped_workspace(name, &socket)?,
            lock_stopped_workspace(&archive, &archive_socket)?,
        ))
    };
    if live {
        let (actual, _, _) = uniterm_client::workspace_request(
            &socket,
            uniterm_proto::ClientMessage::RenameSession {
                name: archive.clone(),
            },
        )
        .map_err(|error| format!("could not archive live Workspace: {error}"))?;
        if actual != archive {
            return Err("the existing Workspace could not be archived".into());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if uniterm_server::persist::exists(&archive) && !uniterm_server::persist::exists(name) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !uniterm_server::persist::exists(&archive) || uniterm_server::persist::exists(name) {
            return Err("timed out while archiving the existing Workspace".into());
        }
    } else if uniterm_server::persist::exists(name)
        || uniterm_server::workspace_catalog::exists(name)
    {
        let snapshot_moved = uniterm_server::persist::exists(name);
        if snapshot_moved {
            uniterm_server::persist::rename(name, &archive)
                .map_err(|error| format!("could not archive Workspace snapshot: {error}"))?;
        }
        if let Err(error) = uniterm_server::eventlog::rename(name, &archive) {
            if error.kind() != std::io::ErrorKind::NotFound {
                if snapshot_moved {
                    return match uniterm_server::persist::rename(&archive, name) {
                        Ok(()) => Err(format!("could not archive Workspace event log: {error}")),
                        Err(rollback) => Err(format!(
                            "could not archive Workspace event log: {error}; snapshot rollback also failed: {rollback}"
                        )),
                    };
                }
                return Err(format!("could not archive Workspace event log: {error}"));
            }
        }
        uniterm_server::workspace_catalog::rename(name, &archive)
            .map_err(|error| format!("could not archive Workspace definition: {error}"))?;
    } else {
        return Err(format!("Workspace '{name}' is no longer available"));
    }
    Ok(archive)
}

fn resolve_pane_target(value: Option<&str>) -> Result<uniterm_core::PaneId, String> {
    let value = match value {
        Some("." | "current") | None => std::env::var("UNITERM_PANE_ID")
            .map_err(|_| "pane id is required outside a Uniterm Pane".to_string())?,
        Some(value) => value.to_string(),
    };
    value
        .parse::<u64>()
        .map(uniterm_core::PaneId)
        .map_err(|_| format!("invalid Pane id '{value}'"))
}

fn cmd_instruction(args: &[String]) -> i32 {
    let (socket, args) = match project_socket(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("uniterm instruction: {error}");
            return 2;
        }
    };
    let json = args.iter().any(|arg| arg == "--json");
    let args: Vec<_> = args.into_iter().filter(|arg| arg != "--json").collect();
    match args.first().map(String::as_str) {
        None | Some("list" | "ls") => {
            if args.len() > 1 {
                eprintln!(
                    "uniterm instruction list: usage: ut instruction list [-w Workspace] [--json]"
                );
                return 2;
            }
            match uniterm_client::instruction_list(&socket) {
                Ok(items) if json => match serde_json::to_string_pretty(&items) {
                    Ok(output) => {
                        println!("{output}");
                        0
                    }
                    Err(error) => {
                        eprintln!("uniterm instruction list: could not encode JSON: {error}");
                        1
                    }
                },
                Ok(items) if items.is_empty() => {
                    println!("no queued instructions");
                    0
                }
                Ok(items) => {
                    for item in items {
                        let agent = item.agent.as_deref().unwrap_or("agent");
                        println!(
                            "{}\t{} / Tab {} / Pane {}\t{}\t{}\t{}",
                            item.id,
                            item.project_name,
                            item.tab,
                            item.pane.0,
                            agent,
                            item.author.label(),
                            item.text
                        );
                    }
                    0
                }
                Err(error) => {
                    eprintln!("uniterm instruction list: {error}");
                    1
                }
            }
        }
        Some("add" | "queue") => {
            let pane = match resolve_pane_target(args.get(1).map(String::as_str)) {
                Ok(pane) => pane,
                Err(error) => {
                    eprintln!("uniterm instruction add: {error}");
                    return 2;
                }
            };
            let text = args.get(2..).unwrap_or_default().join(" ");
            if text.trim().is_empty() {
                eprintln!("uniterm instruction add: instruction text is required");
                return 2;
            }
            match uniterm_client::instruction_add(
                &socket,
                pane,
                uniterm_core::InstructionAuthor::Cli,
                text,
            ) {
                Ok((id, true, true, items)) => {
                    print_instruction_change(json, id, true, true, &items);
                    0
                }
                Ok((_, false, _, _)) => {
                    eprintln!("uniterm instruction add: no Pane {}", pane.0);
                    1
                }
                Ok(_) => {
                    eprintln!(
                        "uniterm instruction add: Pane {} has no active agent invocation",
                        pane.0
                    );
                    1
                }
                Err(error) => {
                    eprintln!("uniterm instruction add: {error}");
                    1
                }
            }
        }
        Some("replace") => {
            let Some(id) = args.get(1).and_then(|value| value.parse::<u64>().ok()) else {
                eprintln!("uniterm instruction replace: a numeric instruction id is required");
                return 2;
            };
            let text = args.get(2..).unwrap_or_default().join(" ");
            if text.trim().is_empty() {
                eprintln!("uniterm instruction replace: instruction text is required");
                return 2;
            }
            match uniterm_client::instruction_replace(
                &socket,
                id,
                uniterm_core::InstructionAuthor::Cli,
                text,
            ) {
                Ok((new_id, found, accepted, items)) => {
                    print_instruction_change(json, new_id, found, accepted, &items);
                    if found && accepted {
                        0
                    } else {
                        1
                    }
                }
                Err(error) => {
                    eprintln!("uniterm instruction replace: {error}");
                    1
                }
            }
        }
        Some(command @ ("cancel" | "send-now" | "now")) => {
            let Some(id) = args.get(1).and_then(|value| value.parse::<u64>().ok()) else {
                eprintln!("uniterm instruction {command}: a numeric instruction id is required");
                return 2;
            };
            if args.len() != 2 {
                eprintln!("uniterm instruction {command}: unexpected arguments");
                return 2;
            }
            let result = if command == "cancel" {
                uniterm_client::instruction_cancel(&socket, id)
            } else {
                uniterm_client::instruction_send_now(&socket, id)
            };
            match result {
                Ok((result_id, found, accepted, items)) => {
                    print_instruction_change(json, result_id, found, accepted, &items);
                    if found && accepted {
                        0
                    } else {
                        1
                    }
                }
                Err(error) => {
                    eprintln!("uniterm instruction {command}: {error}");
                    1
                }
            }
        }
        Some(other) => {
            eprintln!("uniterm instruction: unknown command '{other}'");
            eprintln!(
                "usage: ut instruction <list|add|replace|cancel|send-now> ... [-w Workspace] [--json]"
            );
            2
        }
    }
}

fn print_instruction_change(
    json: bool,
    id: u64,
    found: bool,
    accepted: bool,
    items: &[uniterm_proto::InstructionEntry],
) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": id,
                "found": found,
                "accepted": accepted,
                "items": items,
            })
        );
    } else if found && accepted {
        println!("instruction {id} accepted");
    } else if !found {
        eprintln!("no queued instruction {id}");
    } else {
        eprintln!("instruction {id} could not be accepted");
    }
}

fn cmd_waiting(args: &[String]) -> i32 {
    let (socket, args) = match project_socket(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("uniterm waiting: {error}");
            return 2;
        }
    };
    match args.first().map(String::as_str) {
        None | Some("list" | "ls") => {
            let options: &[String] = if args.is_empty() { &args } else { &args[1..] };
            if options.iter().any(|arg| arg != "--json") {
                eprintln!("uniterm waiting list: usage: ut waiting list [-w Workspace] [--json]");
                return 2;
            }
            match uniterm_client::waiting_list(&socket) {
                Ok(items) if args.iter().any(|arg| arg == "--json") => {
                    match serde_json::to_string_pretty(&items) {
                        Ok(output) => {
                            println!("{output}");
                            0
                        }
                        Err(error) => {
                            eprintln!("uniterm waiting list: could not encode JSON: {error}");
                            1
                        }
                    }
                }
                Ok(items) if items.is_empty() => {
                    println!("no waiting items");
                    0
                }
                Ok(items) => {
                    for item in items {
                        let agent = item.agent.as_deref().unwrap_or("agent");
                        println!(
                            "{}\t{}\t{} / Tab {} / Pane {}\t{}\t{}",
                            item.id,
                            item.kind.label(),
                            item.project_name,
                            item.tab,
                            item.pane.0,
                            agent,
                            item.summary
                        );
                    }
                    0
                }
                Err(error) => {
                    eprintln!("uniterm waiting list: {error}");
                    1
                }
            }
        }
        Some(command @ ("focus" | "answer" | "dismiss" | "stop" | "resume" | "rollback")) => {
            let Some(id) = args.get(1).and_then(|value| value.parse::<u64>().ok()) else {
                eprintln!("uniterm waiting {command}: a numeric waiting id is required");
                return 2;
            };
            let (action, text) = match command {
                "focus" => (uniterm_proto::WaitingAction::Focus, String::new()),
                "dismiss" => (uniterm_proto::WaitingAction::Dismiss, String::new()),
                "stop" => (uniterm_proto::WaitingAction::Stop, String::new()),
                "resume" => (uniterm_proto::WaitingAction::Resume, String::new()),
                "rollback" => (uniterm_proto::WaitingAction::Rollback, String::new()),
                "answer" => {
                    let text = args[2..].join(" ");
                    if text.trim().is_empty() {
                        eprintln!("uniterm waiting answer: answer text is required");
                        return 2;
                    }
                    (uniterm_proto::WaitingAction::Answer, text)
                }
                _ => unreachable!(),
            };
            match uniterm_client::waiting_act(&socket, id, action, text) {
                Ok((false, _, _)) => {
                    eprintln!("uniterm waiting {command}: no active waiting item {id}");
                    1
                }
                Ok((true, false, _)) => {
                    eprintln!("uniterm waiting {command}: item {id} could not accept the action");
                    1
                }
                Ok((true, true, _)) => 0,
                Err(error) => {
                    eprintln!("uniterm waiting {command}: {error}");
                    1
                }
            }
        }
        Some(other) => {
            eprintln!("uniterm waiting: unknown command '{other}'");
            eprintln!("usage: ut waiting <list|focus|answer|dismiss|stop|resume|rollback> ... [-w Workspace]");
            2
        }
    }
}

fn cmd_run(args: &[String]) -> i32 {
    let (socket, args) = match project_socket(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("uniterm run: {error}");
            return 2;
        }
    };
    if args.first().is_some_and(|value| value == "fork") {
        if !(4..=5).contains(&args.len()) {
            eprintln!("usage: ut run fork PARENT NAME PATH [BASE] [-w Workspace]");
            return 2;
        }
        let Some(parent) = args[1]
            .parse::<u64>()
            .ok()
            .filter(|parent| *parent > 0)
            .map(uniterm_core::RunId)
        else {
            eprintln!("uniterm run fork: PARENT needs a positive Run id");
            return 2;
        };
        let mut path = PathBuf::from(&args[3]);
        if path.is_relative() {
            let Ok(current) = std::env::current_dir() else {
                eprintln!("uniterm run fork: could not resolve the current directory");
                return 1;
            };
            path = current.join(path);
        }
        let request = uniterm_proto::RunForkRequest {
            parent,
            name: args[2].clone(),
            path: path.to_string_lossy().into_owned(),
            base: args.get(4).cloned(),
        };
        return match uniterm_client::run_fork(&socket, request) {
            Ok(result) if result.worktree.accepted => {
                let child = result
                    .child
                    .map_or_else(|| "?".into(), |run| run.0.to_string());
                let item = result.worktree.items.first();
                println!(
                    "Run {} forked as child Run {} in {}",
                    result.parent.0,
                    child,
                    item.map_or("the new worktree", |item| item.registration.path.as_str())
                );
                0
            }
            Ok(result) => {
                eprintln!(
                    "uniterm run fork: {}",
                    result
                        .worktree
                        .error
                        .as_deref()
                        .unwrap_or("child Run was not created")
                );
                1
            }
            Err(error) => {
                eprintln!("uniterm run fork: {error}");
                1
            }
        };
    }
    let mut options: &[String] = &args;
    if options
        .first()
        .is_some_and(|value| matches!(value.as_str(), "list" | "ls"))
    {
        options = &options[1..];
    }
    let mut json = false;
    let mut active_only = false;
    let mut project = None;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--json" => json = true,
            "--active" => active_only = true,
            "--project" => {
                index += 1;
                let Some(value) = options
                    .get(index)
                    .and_then(|value| value.parse::<u64>().ok())
                else {
                    eprintln!("uniterm run list: --project needs a numeric Project id");
                    return 2;
                };
                project = Some(uniterm_core::ProjectId(value));
            }
            other => {
                eprintln!("uniterm run list: unknown option '{other}'");
                eprintln!("usage: ut run list [-w Workspace] [--project ID] [--active] [--json]");
                return 2;
            }
        }
        index += 1;
    }
    match uniterm_client::run_list(&socket, project, active_only) {
        Ok((workspace, runs)) if json => {
            match serde_json::to_string_pretty(&serde_json::json!({
                "workspace": workspace,
                "runs": runs,
            })) {
                Ok(output) => {
                    println!("{output}");
                    0
                }
                Err(error) => {
                    eprintln!("uniterm run list: could not encode JSON: {error}");
                    1
                }
            }
        }
        Ok((workspace, runs)) if runs.is_empty() => {
            println!("Workspace {workspace}: no matching runs");
            0
        }
        Ok((workspace, runs)) => {
            println!("Workspace {workspace}");
            for run in runs {
                let parent = run
                    .parent
                    .map(|parent| parent.0.to_string())
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{}\t{}\t{}\tProject {}\tTask {}\tparent {}\t{}",
                    run.id.0,
                    run.kind.label(),
                    run.status.label(),
                    run.project.0,
                    run.task_id,
                    parent,
                    run.title
                );
                for role in run.roles {
                    let activation = role.activation.as_ref().map_or_else(
                        || "never".to_string(),
                        |activation| {
                            format!(
                                "activation {} {}",
                                activation.id,
                                if activation.active {
                                    "active"
                                } else {
                                    "closed"
                                }
                            )
                        },
                    );
                    println!(
                        "  role {}\t{}\tPane {}\t{}\t{}",
                        role.id.0, role.name, role.pane.0, role.provider, activation
                    );
                }
                if !run.children.is_empty() {
                    let children = run
                        .children
                        .iter()
                        .map(|child| child.0.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    println!("  children {children}");
                }
            }
            0
        }
        Err(error) => {
            eprintln!("uniterm run list: {error}");
            1
        }
    }
}

fn cmd_artifact(args: &[String]) -> i32 {
    let (socket, args) = match project_socket(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("uniterm artifact: {error}");
            return 2;
        }
    };
    let mut options: &[String] = &args;
    if options
        .first()
        .is_some_and(|value| matches!(value.as_str(), "list" | "ls"))
    {
        options = &options[1..];
    }
    let mut json = false;
    let mut include_superseded = false;
    let mut project = None;
    let mut run = None;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--json" => json = true,
            "--all" => include_superseded = true,
            "--project" => {
                index += 1;
                let Some(value) = options
                    .get(index)
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|value| *value != 0)
                else {
                    eprintln!(
                        "uniterm artifact list: --project needs a nonzero numeric Project id"
                    );
                    return 2;
                };
                project = Some(uniterm_core::ProjectId(value));
            }
            "--run" => {
                index += 1;
                let Some(value) = options
                    .get(index)
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|value| *value != 0)
                else {
                    eprintln!("uniterm artifact list: --run needs a nonzero numeric Run id");
                    return 2;
                };
                run = Some(uniterm_core::RunId(value));
            }
            other => {
                eprintln!("uniterm artifact list: unknown option '{other}'");
                eprintln!(
                    "usage: ut artifact list [-w Workspace] [--project ID] [--run ID] [--all] [--json]"
                );
                return 2;
            }
        }
        index += 1;
    }
    match uniterm_client::artifact_list(&socket, project, run, include_superseded) {
        Ok((workspace, artifacts)) if json => {
            match serde_json::to_string_pretty(&serde_json::json!({
                "workspace": workspace,
                "artifacts": artifacts,
            })) {
                Ok(output) => {
                    println!("{output}");
                    0
                }
                Err(error) => {
                    eprintln!("uniterm artifact list: could not encode JSON: {error}");
                    1
                }
            }
        }
        Ok((workspace, artifacts)) if artifacts.is_empty() => {
            println!("Workspace {workspace}: no matching artifacts");
            0
        }
        Ok((workspace, artifacts)) => {
            println!("Workspace {workspace}");
            for artifact in artifacts {
                println!(
                    "{}\t{}\t{}\tProject {}\tRun {}\tRole {}\t{} bytes\t{}\t{}",
                    artifact.id.0,
                    artifact.kind.label(),
                    artifact.status.label(),
                    artifact.project.0,
                    artifact.producer_run.0,
                    artifact.producer_role.0,
                    artifact.size,
                    artifact.digest,
                    artifact.path
                );
            }
            0
        }
        Err(error) => {
            eprintln!("uniterm artifact list: {error}");
            1
        }
    }
}

fn cmd_pane(args: &[String]) -> i32 {
    let (socket, args) = match project_socket(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("uniterm pane: {error}");
            return 2;
        }
    };
    match args.first().map(String::as_str) {
        Some("list" | "ls") => {
            if args[1..].iter().any(|arg| arg != "--json") {
                eprintln!("uniterm pane list: usage: ut pane list [-w Workspace] [--json]");
                return 2;
            }
            let json = args[1..].iter().any(|arg| arg == "--json");
            match uniterm_client::pane_list(&socket) {
                Ok((workspace, panes)) if json => {
                    let output = serde_json::json!({
                        "workspace": workspace,
                        "panes": panes,
                    });
                    match serde_json::to_string_pretty(&output) {
                        Ok(output) => {
                            println!("{output}");
                            0
                        }
                        Err(error) => {
                            eprintln!("uniterm pane list: could not encode JSON: {error}");
                            1
                        }
                    }
                }
                Ok((workspace, panes)) => {
                    println!("Workspace {workspace}");
                    for pane in panes {
                        let mark = if pane.active { '*' } else { ' ' };
                        println!(
                            "{mark} {}\tProject {} ({})\t{} ({})\tPane {}",
                            pane.id.0,
                            pane.project_name,
                            pane.project.0,
                            pane.tab_name,
                            pane.tab,
                            pane.pane
                        );
                    }
                    0
                }
                Err(error) => {
                    eprintln!("uniterm pane list: {error}");
                    1
                }
            }
        }
        Some("focus") => match args.len() {
            2 => {
                let Ok(pane) = args[1].parse::<u64>() else {
                    eprintln!("uniterm pane focus: pane id must be numeric");
                    return 2;
                };
                match uniterm_client::pane_focus(&socket, uniterm_core::PaneId(pane)) {
                    Ok(true) => 0,
                    Ok(false) => {
                        eprintln!("uniterm pane focus: no Pane {pane} in this Workspace");
                        1
                    }
                    Err(error) => {
                        eprintln!("uniterm pane focus: {error}");
                        1
                    }
                }
            }
            4 => {
                let Ok(pane) = args[3].parse::<u32>() else {
                    eprintln!("uniterm pane focus: Pane ordinal must be numeric");
                    return 2;
                };
                if pane == 0 {
                    eprintln!("uniterm pane focus: Pane ordinal is 1-based");
                    return 2;
                }
                let (project, project_name, tab) =
                    match resolve_hierarchy_location(&socket, &args[1], &args[2]) {
                        Ok(location) => location,
                        Err(error) => {
                            eprintln!("uniterm pane focus: {error}");
                            return 1;
                        }
                    };
                match uniterm_client::hierarchy_focus(&socket, project, tab, Some(pane)) {
                    Ok(Some(_)) => 0,
                    Ok(None) => {
                        eprintln!(
                                "uniterm pane focus: no Pane {pane} in Project '{project_name}' Tab {tab}"
                            );
                        1
                    }
                    Err(error) => {
                        eprintln!("uniterm pane focus: {error}");
                        1
                    }
                }
            }
            _ => {
                eprintln!(
                        "uniterm pane focus: usage: ut pane focus <pane-id> | <project> <tab> <pane> [-w Workspace]"
                    );
                2
            }
        },
        Some("read") => {
            let pane_value = args.get(1).filter(|value| !value.starts_with('-'));
            let pane = match resolve_pane_target(pane_value.map(String::as_str)) {
                Ok(pane) => pane,
                Err(error) => {
                    eprintln!("uniterm pane read: {error}");
                    return 2;
                }
            };
            let lines = args
                .iter()
                .position(|value| value == "--lines")
                .and_then(|index| args.get(index + 1))
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(200);
            let json = args.iter().any(|value| value == "--json");
            match uniterm_client::pane_read(&socket, pane, lines) {
                Ok(Some((text, truncated))) if json => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "pane": pane.0,
                            "text": text,
                            "truncated": truncated,
                        })
                    );
                    0
                }
                Ok(Some((text, truncated))) => {
                    print!("{text}");
                    if !text.ends_with('\n') {
                        println!();
                    }
                    if truncated {
                        eprintln!(
                            "uniterm pane read: output exceeds 256 KiB; oldest bytes were dropped"
                        );
                    }
                    0
                }
                Ok(None) => {
                    eprintln!("uniterm pane read: no Pane {}", pane.0);
                    1
                }
                Err(error) => {
                    eprintln!("uniterm pane read: {error}");
                    1
                }
            }
        }
        Some("send-keys" | "send") => {
            let Some(value) = args.get(1) else {
                eprintln!("uniterm pane send-keys: usage: ut pane send-keys PANE TEXT [--enter]");
                return 2;
            };
            let pane = match resolve_pane_target(Some(value)) {
                Ok(pane) => pane,
                Err(error) => {
                    eprintln!("uniterm pane send-keys: {error}");
                    return 2;
                }
            };
            let Some(text) = args.get(2) else {
                eprintln!("uniterm pane send-keys: TEXT is required");
                return 2;
            };
            let mut bytes = text.as_bytes().to_vec();
            if args.iter().any(|value| value == "--enter") {
                bytes.push(b'\n');
            }
            match uniterm_client::pane_send(&socket, pane, bytes) {
                Ok(Some(true)) => 0,
                Ok(Some(false)) => {
                    eprintln!(
                        "uniterm pane send-keys: Pane {} input queue is full; bytes dropped",
                        pane.0
                    );
                    1
                }
                Ok(None) => {
                    eprintln!("uniterm pane send-keys: no Pane {}", pane.0);
                    1
                }
                Err(error) => {
                    eprintln!("uniterm pane send-keys: {error}");
                    1
                }
            }
        }
        Some("wait-output" | "wait") => {
            let (Some(pane_value), Some(needle)) = (args.get(1), args.get(2)) else {
                eprintln!("uniterm pane wait-output: usage: ut pane wait-output PANE TEXT [--timeout SECONDS] [--json]");
                return 2;
            };
            let pane = match resolve_pane_target(Some(pane_value)) {
                Ok(pane) => pane,
                Err(error) => {
                    eprintln!("uniterm pane wait-output: {error}");
                    return 2;
                }
            };
            let seconds = args
                .iter()
                .position(|value| value == "--timeout")
                .and_then(|index| args.get(index + 1))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(30);
            match uniterm_client::pane_wait_output(
                &socket,
                pane,
                needle.clone(),
                std::time::Duration::from_secs(seconds),
            ) {
                Ok(result) if args.iter().any(|value| value == "--json") => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "pane": pane.0,
                            "found": result.found,
                            "matched": result.matched,
                            "timed_out": result.timed_out,
                            "text": result.text,
                            "truncated": result.truncated,
                        })
                    );
                    i32::from(!result.matched)
                }
                Ok(result) if result.matched => 0,
                Ok(result) if !result.found => {
                    eprintln!("uniterm pane wait-output: no Pane {}", pane.0);
                    1
                }
                Ok(_) => {
                    eprintln!("uniterm pane wait-output: timed out");
                    1
                }
                Err(error) => {
                    eprintln!("uniterm pane wait-output: {error}");
                    1
                }
            }
        }
        Some("metadata") => {
            let Some(pane) = args.get(1).and_then(|value| value.parse::<u64>().ok()) else {
                eprintln!("uniterm pane metadata: pane id must be numeric");
                return 2;
            };
            let (Some(key), Some(value)) = (args.get(2), args.get(3)) else {
                eprintln!(
                    "uniterm pane metadata: key and value are required (empty value removes)"
                );
                return 2;
            };
            let ttl_seconds = args
                .iter()
                .position(|value| value == "--ttl")
                .and_then(|index| args.get(index + 1))
                .and_then(|value| value.parse().ok());
            match uniterm_client::control(
                &socket,
                uniterm_proto::ClientMessage::PaneMetadata {
                    pane: uniterm_core::PaneId(pane),
                    key: key.clone(),
                    value: value.clone(),
                    ttl_seconds,
                },
            ) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("uniterm pane metadata: {error}");
                    1
                }
            }
        }
        _ => {
            eprintln!("uniterm pane: usage: ut pane <list|focus|read|send-keys|wait-output|metadata> ... [-w Workspace]");
            2
        }
    }
}

fn resolve_hierarchy_location(
    socket: &Path,
    project_value: &str,
    tab_value: &str,
) -> Result<(uniterm_core::ProjectId, String, u32), String> {
    let (_, panes) = uniterm_client::pane_list(socket)
        .map_err(|error| format!("could not list Workspace Panes: {error}"))?;
    let project = project_value
        .parse::<u64>()
        .ok()
        .map(uniterm_core::ProjectId)
        .filter(|id| panes.iter().any(|pane| pane.project == *id))
        .or_else(|| {
            panes
                .iter()
                .find(|pane| pane.project_name.eq_ignore_ascii_case(project_value))
                .map(|pane| pane.project)
        })
        .ok_or_else(|| format!("unknown Project '{project_value}'"))?;
    let project_name = panes
        .iter()
        .find(|pane| pane.project == project)
        .map(|pane| pane.project_name.clone())
        .ok_or_else(|| format!("Project {} has no live Panes", project.0))?;

    if let Ok(tab) = tab_value.parse::<u32>() {
        if tab != 0
            && panes
                .iter()
                .any(|pane| pane.project == project && pane.tab == tab)
        {
            return Ok((project, project_name, tab));
        }
    }
    let mut matching_tabs: Vec<u32> = panes
        .iter()
        .filter(|pane| pane.project == project && pane.tab_name.eq_ignore_ascii_case(tab_value))
        .map(|pane| pane.tab)
        .collect();
    matching_tabs.sort_unstable();
    matching_tabs.dedup();
    match matching_tabs.as_slice() {
        [tab] => Ok((project, project_name, *tab)),
        [] => Err(format!(
            "no Tab '{tab_value}' in Project '{project_name}'"
        )),
        _ => Err(format!(
            "Tab name '{tab_value}' is ambiguous in Project '{project_name}'; use its 1-based ordinal"
        )),
    }
}

fn cmd_tab(args: &[String]) -> i32 {
    let (socket, args) = match project_socket(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("uniterm tab: {error}");
            return 2;
        }
    };
    if args.first().map(String::as_str) == Some("move") {
        let direction = match args.get(1).map(String::as_str) {
            Some("left" | "previous" | "prev") if args.len() == 2 => {
                uniterm_proto::TabMoveDirection::Previous
            }
            Some("right" | "next") if args.len() == 2 => uniterm_proto::TabMoveDirection::Next,
            _ => {
                eprintln!("uniterm tab move: usage: ut tab move left|right [-w Workspace]");
                return 2;
            }
        };
        return match uniterm_client::tab_move(&socket, direction) {
            Ok(true) => 0,
            Ok(false) => {
                eprintln!("uniterm tab move: the active Project has only one Tab");
                1
            }
            Err(error) => {
                eprintln!("uniterm tab move: {error}");
                1
            }
        };
    }
    if args.first().map(String::as_str) == Some("new") {
        if args.len() > 2 {
            eprintln!("uniterm tab new: usage: ut tab new [project] [-w Workspace]");
            return 2;
        }
        let (active, projects) = match uniterm_client::workspace_request(
            &socket,
            uniterm_proto::ClientMessage::WorkspaceState,
        ) {
            Ok((_, active, projects)) => (active, projects),
            Err(error) => {
                eprintln!("uniterm tab new: {error}");
                return 1;
            }
        };
        let project = match args.get(1) {
            Some(value) => match resolve_project(&projects, value) {
                Some(project) => project,
                None => {
                    eprintln!("uniterm tab new: unknown Project '{value}'");
                    return 1;
                }
            },
            None => active,
        };
        return match uniterm_client::control_request(
            &socket,
            uniterm_proto::ControlCommand::TabCreate { project },
        ) {
            Ok(response) => match response.result {
                Some(uniterm_proto::ControlResult::Mutation {
                    accepted: true,
                    id: Some(ordinal),
                    ..
                }) => {
                    println!("{ordinal}");
                    0
                }
                Some(_) => {
                    eprintln!("uniterm tab new: the Tab was not created");
                    1
                }
                None => {
                    eprintln!(
                        "uniterm tab new: {}",
                        response
                            .error
                            .map_or("no result".to_string(), |error| error.message)
                    );
                    1
                }
            },
            Err(error) => {
                eprintln!("uniterm tab new: {error}");
                1
            }
        };
    }
    if args.first().map(String::as_str) == Some("rename") {
        if args.len() != 4 {
            eprintln!(
                "uniterm tab rename: usage: ut tab rename <project> <tab> <name> [-w Workspace]"
            );
            return 2;
        }
        let (project, project_name, tab) =
            match resolve_hierarchy_location(&socket, &args[1], &args[2]) {
                Ok(location) => location,
                Err(error) => {
                    eprintln!("uniterm tab rename: {error}");
                    return 1;
                }
            };
        let name = args[3].trim().to_string();
        if name.is_empty() {
            eprintln!("uniterm tab rename: a non-empty name is required");
            return 2;
        }
        return match uniterm_client::control_request(
            &socket,
            uniterm_proto::ControlCommand::TabRename { project, tab, name },
        ) {
            Ok(response) => match response.result {
                Some(uniterm_proto::ControlResult::Mutation { accepted: true, .. }) => 0,
                Some(_) => {
                    eprintln!("uniterm tab rename: no Tab {tab} in Project '{project_name}'");
                    1
                }
                None => {
                    eprintln!(
                        "uniterm tab rename: {}",
                        response
                            .error
                            .map_or("no result".to_string(), |error| error.message)
                    );
                    1
                }
            },
            Err(error) => {
                eprintln!("uniterm tab rename: {error}");
                1
            }
        };
    }
    if !matches!(args.first().map(String::as_str), Some("focus" | "select")) || args.len() != 3 {
        eprintln!(
            "uniterm tab: usage: ut tab focus <project> <tab> | tab new [project] | tab rename <project> <tab> <name> | tab move left|right [-w Workspace]"
        );
        return 2;
    }
    let (project, project_name, tab) = match resolve_hierarchy_location(&socket, &args[1], &args[2])
    {
        Ok(location) => location,
        Err(error) => {
            eprintln!("uniterm tab focus: {error}");
            return 1;
        }
    };
    match uniterm_client::hierarchy_focus(&socket, project, tab, None) {
        Ok(Some(_)) => 0,
        Ok(None) => {
            eprintln!("uniterm tab focus: no Tab {tab} in Project '{project_name}'");
            1
        }
        Err(error) => {
            eprintln!("uniterm tab focus: {error}");
            1
        }
    }
}

fn cmd_agent(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("manifests") {
        if args.get(1).map(String::as_str) != Some("validate") || args.len() != 3 {
            eprintln!("uniterm agent manifests: usage: ut agent manifests validate PATH");
            return 2;
        }
        let path = PathBuf::from(&args[2]);
        return match uniterm_server::providers::validate_file(&path) {
            Ok(summary) => {
                println!(
                    "{}: valid manifest {} ({} providers, {} rules)",
                    terminal_safe(&path.display().to_string()),
                    summary.manifest_version,
                    summary.providers,
                    summary.rules,
                );
                0
            }
            Err(error) => {
                eprintln!(
                    "uniterm agent manifests validate: {}",
                    terminal_safe(&error)
                );
                1
            }
        };
    }
    let (socket, args) = match project_socket(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("uniterm agent: {error}");
            return 2;
        }
    };
    match args.first().map(String::as_str) {
        Some("list") => {
            let json = args.iter().any(|value| value == "--json");
            let entries = match uniterm_client::control_request(
                &socket,
                uniterm_proto::ControlCommand::AgentList,
            ) {
                Ok(response) => match response.result {
                    Some(uniterm_proto::ControlResult::Fleet { entries }) => entries,
                    _ => {
                        eprintln!(
                            "uniterm agent list: {}",
                            response
                                .error
                                .map_or("unexpected control result".to_string(), |error| {
                                    error.message
                                })
                        );
                        return 1;
                    }
                },
                Err(error) => {
                    eprintln!("uniterm agent list: {error}");
                    return 1;
                }
            };
            if json {
                let workspace = socket
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default();
                match serde_json::to_string(&serde_json::json!({
                    "workspace": workspace,
                    "agents": entries,
                })) {
                    Ok(text) => println!("{text}"),
                    Err(error) => {
                        eprintln!("uniterm agent list: could not encode JSON: {error}");
                        return 1;
                    }
                }
                return 0;
            }
            if entries.is_empty() {
                println!("no agents are running in this Workspace");
                return 0;
            }
            println!(
                "{:<6} {:<10} {:<11} {:<18} {:<14} evidence",
                "pane", "agent", "status", "project", "tab"
            );
            for entry in entries {
                let tab = if entry.tab_name.is_empty() {
                    entry.tab.to_string()
                } else {
                    format!("{}:{}", entry.tab, entry.tab_name)
                };
                println!(
                    "{:<6} {:<10} {:<11} {:<18} {:<14} {}",
                    entry.pane_id.0,
                    terminal_safe(&entry.agent),
                    entry.status.label(),
                    terminal_safe(&entry.project_name),
                    terminal_safe(&tab),
                    terminal_safe(&entry.evidence)
                );
            }
            0
        }
        Some("explain") => {
            let pane = match args.get(1) {
                Some(value) => match resolve_pane_target(Some(value)) {
                    Ok(id) => Some(id),
                    Err(error) => {
                        eprintln!("uniterm agent explain: {error}");
                        return 2;
                    }
                },
                None => None,
            };
            match uniterm_client::agent_explain(&socket, pane) {
                Ok(entries) if entries.is_empty() => {
                    println!("no matching Panes");
                    0
                }
                Ok(entries) => {
                    for entry in entries {
                        println!(
                            "pane {}  Project {} / Tab {}  {}  {}  {:?}",
                            entry.pane.0,
                            entry.project.0,
                            entry.tab,
                            terminal_safe(entry.agent.as_deref().unwrap_or("no agent")),
                            entry.status.label(),
                            entry.authority,
                        );
                        println!("  evidence: {}", terminal_safe(&entry.evidence));
                        if let Some(pid) = entry.foreground_pid {
                            println!("  process group: {pid}");
                        }
                        println!(
                            "  source: {:?}  precedence: {}",
                            entry.provenance.source, entry.provenance.precedence
                        );
                        if let Some(version) = &entry.provenance.manifest_version {
                            println!("  manifest version: {}", terminal_safe(version));
                        }
                        if let Some(rule) = &entry.provenance.matched_rule {
                            println!("  matched rule: {}", terminal_safe(rule));
                        }
                        if let Some(confidence) = entry.provenance.confidence {
                            println!("  confidence: {confidence}");
                        }
                        if let Some(dwell_ms) = entry.provenance.dwell_ms {
                            println!("  dwell hint: {dwell_ms} ms");
                        }
                        if !entry.provenance.capabilities.is_empty() {
                            let capabilities = entry
                                .provenance
                                .capabilities
                                .iter()
                                .map(|capability| format!("{capability:?}").to_ascii_lowercase())
                                .collect::<Vec<_>>()
                                .join(", ");
                            println!("  capabilities: {capabilities}");
                        }
                        println!(
                            "  evidence timestamp: {} ms since epoch",
                            entry.provenance.evidence_timestamp_ms
                        );
                        if let Some(pid) = entry.provenance.invocation_pid {
                            println!("  invocation pid: {pid}");
                        }
                    }
                    0
                }
                Err(error) => {
                    eprintln!("uniterm agent explain: {error}");
                    1
                }
            }
        }
        Some("start") => {
            let Some(agent) = args.get(1) else {
                eprintln!("uniterm agent start: usage: ut agent start NAME [--current|--tab]");
                return 2;
            };
            let target = if args.iter().any(|value| value == "--current") {
                uniterm_proto::LaunchTarget::CurrentPane
            } else if args.iter().any(|value| value == "--tab") {
                uniterm_proto::LaunchTarget::NewWindow
            } else {
                uniterm_proto::LaunchTarget::NewPane
            };
            match uniterm_client::agent_launch(&socket, agent.clone(), target) {
                Ok(Some(pane)) => {
                    println!("{}", pane.0);
                    0
                }
                Ok(None) => {
                    eprintln!("uniterm agent start: '{agent}' is unavailable or failed to start");
                    1
                }
                Err(error) => {
                    eprintln!("uniterm agent start: {error}");
                    1
                }
            }
        }
        Some("prompt" | "send-keys") => {
            let (Some(pane_value), Some(text)) = (args.get(1), args.get(2)) else {
                eprintln!("uniterm agent prompt: usage: ut agent prompt PANE TEXT");
                return 2;
            };
            let pane = match resolve_pane_target(Some(pane_value)) {
                Ok(pane) => pane,
                Err(error) => {
                    eprintln!("uniterm agent prompt: {error}");
                    return 2;
                }
            };
            let mut bytes = text.as_bytes().to_vec();
            if args[0] == "prompt" || args.iter().any(|value| value == "--enter") {
                bytes.push(b'\n');
            }
            match uniterm_client::pane_send(&socket, pane, bytes) {
                Ok(Some(true)) => 0,
                Ok(Some(false)) => {
                    eprintln!(
                        "uniterm agent prompt: Pane {} input queue is full; bytes dropped",
                        pane.0
                    );
                    1
                }
                Ok(None) => {
                    eprintln!("uniterm agent prompt: no Pane {}", pane.0);
                    1
                }
                Err(error) => {
                    eprintln!("uniterm agent prompt: {error}");
                    1
                }
            }
        }
        Some("read") => {
            let pane = match resolve_pane_target(args.get(1).map(String::as_str)) {
                Ok(pane) => pane,
                Err(error) => {
                    eprintln!("uniterm agent read: {error}");
                    return 2;
                }
            };
            match uniterm_client::pane_read(&socket, pane, 200) {
                Ok(Some((text, _))) => {
                    println!("{text}");
                    0
                }
                Ok(None) => {
                    eprintln!("uniterm agent read: no Pane {}", pane.0);
                    1
                }
                Err(error) => {
                    eprintln!("uniterm agent read: {error}");
                    1
                }
            }
        }
        Some("wait") => {
            let (Some(pane_value), Some(status_value)) = (args.get(1), args.get(2)) else {
                eprintln!(
                    "uniterm agent wait: usage: ut agent wait PANE STATUS [--timeout SECONDS]"
                );
                return 2;
            };
            let pane = match resolve_pane_target(Some(pane_value)) {
                Ok(pane) => pane,
                Err(error) => {
                    eprintln!("uniterm agent wait: {error}");
                    return 2;
                }
            };
            let Some(status) = parse_agent_status(status_value) else {
                eprintln!("uniterm agent wait: unknown status '{status_value}'");
                return 2;
            };
            let seconds = args
                .iter()
                .position(|value| value == "--timeout")
                .and_then(|index| args.get(index + 1))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(30);
            match uniterm_client::agent_wait(
                &socket,
                pane,
                status,
                std::time::Duration::from_secs(seconds),
            ) {
                Ok(result) if result.matched => 0,
                Ok(result) if !result.found => {
                    eprintln!("uniterm agent wait: no Pane {}", pane.0);
                    1
                }
                Ok(result) => {
                    eprintln!(
                        "uniterm agent wait: timed out (current: {})",
                        result
                            .status
                            .map_or("none", uniterm_core::AgentStatus::label)
                    );
                    1
                }
                Err(error) => {
                    eprintln!("uniterm agent wait: {error}");
                    1
                }
            }
        }
        Some("attach") => {
            let role = if args.iter().skip(2).any(|value| value == "--observe") {
                uniterm_proto::PaneAttachRole::Observer
            } else if args.iter().skip(2).any(|value| value == "--takeover") {
                uniterm_proto::PaneAttachRole::Takeover
            } else {
                uniterm_proto::PaneAttachRole::Controller
            };
            if let Some(flag) = args
                .iter()
                .skip(2)
                .find(|value| !matches!(value.as_str(), "--observe" | "--takeover"))
            {
                eprintln!("uniterm agent attach: unknown option '{flag}'");
                return 2;
            }
            if args.iter().any(|value| value == "--observe")
                && args.iter().any(|value| value == "--takeover")
            {
                eprintln!("uniterm agent attach: choose either --observe or --takeover");
                return 2;
            }
            let pane = match resolve_pane_target(args.get(1).map(String::as_str)) {
                Ok(pane) => pane,
                Err(error) => {
                    eprintln!("uniterm agent attach: {error}");
                    return 2;
                }
            };
            if std::env::var_os("UNITERM_SOCKET").is_some() {
                return match uniterm_client::pane_focus(&socket, pane) {
                    Ok(true) => 0,
                    Ok(false) => {
                        eprintln!("uniterm agent attach: no Pane {}", pane.0);
                        1
                    }
                    Err(error) => {
                        eprintln!("uniterm agent attach: {error}");
                        1
                    }
                };
            }
            match uniterm_client::pane_attach(&socket, pane, role) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("uniterm agent attach: {error}");
                    1
                }
            }
        }
        _ => {
            eprintln!("uniterm agent: usage: ut agent <list|start|prompt|send-keys|read|wait|attach|explain> ...");
            2
        }
    }
}

fn parse_agent_status(value: &str) -> Option<uniterm_core::AgentStatus> {
    use uniterm_core::AgentStatus;
    Some(match value.to_ascii_lowercase().as_str() {
        "unknown" => AgentStatus::Unknown,
        "starting" => AgentStatus::Starting,
        "working" => AgentStatus::Working,
        "tool" => AgentStatus::Tool,
        "permission" => AgentStatus::Permission,
        "question" => AgentStatus::Question,
        "idle" => AgentStatus::Idle,
        "error" => AgentStatus::Error,
        "exited" => AgentStatus::Exited,
        _ => return None,
    })
}

/// Wait until server teardown and its final persistence flush have completed.
fn wait_for_workspace_stop(socket: &Path) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while socket.exists() {
        if std::time::Instant::now() >= deadline {
            return Err("did not stop within 5 seconds".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    Ok(())
}

fn stop_workspace(socket: &Path) -> Result<(), String> {
    uniterm_client::kill_server(socket).map_err(|error| error.to_string())?;
    wait_for_workspace_stop(socket)
}

/// Stop every running server while retaining lightweight definitions.
fn cmd_stop_all() -> i32 {
    let mut workspaces = running_workspace_sockets();
    if workspaces.is_empty() {
        println!("uniterm: no running Workspaces to stop");
        return 0;
    }

    // Signal every server first so their shutdown work can proceed together.
    // If this command runs inside Uniterm, stop its owning server last so that
    // server cannot kill the command before every peer has been signalled.
    if let Some(current) = std::env::var_os("UNITERM_SOCKET").map(PathBuf::from) {
        workspaces.sort_by_key(|(_, socket, _, _)| socket == &current);
    }
    let mut stopping = Vec::new();
    let mut failures = Vec::new();
    for (name, socket, _, _) in workspaces {
        match uniterm_client::kill_server(&socket) {
            Ok(()) => stopping.push((name, socket)),
            Err(error) => failures.push((name, error.to_string())),
        }
    }
    let mut stopped = 0;
    for (name, socket) in stopping {
        match wait_for_workspace_stop(&socket) {
            Ok(()) => stopped += 1,
            Err(error) => failures.push((name, error)),
        }
    }

    if stopped > 0 {
        let label = if stopped == 1 {
            "Workspace"
        } else {
            "Workspaces"
        };
        println!("uniterm: stopped {stopped} {label}");
    }
    for (name, error) in &failures {
        eprintln!("uniterm workspace stop: Workspace '{name}': {error}");
    }
    if failures.is_empty() {
        0
    } else {
        1
    }
}

/// `uniterm workspace stop [name|--all]` - stop one or every server while
/// retaining lightweight Workspace definitions.
fn cmd_kill(args: &[String]) -> i32 {
    if args == ["--all"] {
        return cmd_stop_all();
    }
    let fallback = default_workspace();
    let name = args.first().map(String::as_str).unwrap_or(&fallback);
    let sock = match checked_socket_path(name) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("uniterm workspace stop: {error}");
            return 2;
        }
    };
    if !sock.exists() {
        if uniterm_server::workspace_catalog::exists(name) {
            println!("uniterm: Workspace '{name}' is already stopped");
            return 0;
        }
        eprintln!("uniterm workspace stop: no Workspace '{name}'");
        return 1;
    }
    match stop_workspace(&sock) {
        Ok(()) => {
            println!("uniterm: stopped Workspace '{name}'");
            0
        }
        Err(e) => {
            eprintln!("uniterm workspace stop: {e}");
            1
        }
    }
}

fn remembered_workspace_names() -> BTreeSet<String> {
    uniterm_server::workspace_catalog::list_names()
        .into_iter()
        .chain(uniterm_server::persist::list_names())
        .chain(uniterm_server::eventlog::list_names())
        .collect()
}

fn forget_stopped_workspace(name: &str) -> Result<bool, String> {
    let socket = checked_socket_path(name)?;
    match probe_socket(&socket) {
        SocketHealth::Live { .. } => {
            return Err(format!(
                "Workspace '{name}' is running; stop it first with `ut workspace stop {name}`"
            ));
        }
        SocketHealth::Stale => {}
        SocketHealth::Indeterminate(error) => {
            return Err(indeterminate_socket_error(name, &error));
        }
    }
    let _workspace_lock = lock_stopped_workspace(name, &socket)?;
    let existed = uniterm_server::workspace_catalog::exists(name)
        || uniterm_server::persist::exists(name)
        || uniterm_server::eventlog::exists(name);
    if !existed {
        return Ok(false);
    }
    if std::fs::symlink_metadata(&socket).is_ok_and(|metadata| metadata.file_type().is_socket()) {
        let _ = std::fs::remove_file(socket);
    }
    uniterm_server::workspace_catalog::delete(name).map_err(|error| error.to_string())?;
    uniterm_server::persist::delete(name);
    uniterm_server::eventlog::delete(name).map_err(|error| error.to_string())?;
    Ok(true)
}

/// Permanently remove every stopped Workspace and recovery artifact. Refuse
/// the entire operation if any server is still running.
fn cmd_forget_all() -> i32 {
    let running = running_workspace_sockets();
    if !running.is_empty() {
        let names = running
            .iter()
            .map(|(name, _, _, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "uniterm workspace forget: running Workspaces found ({names}); stop them first with `ut workspace stop --all`"
        );
        return 1;
    }

    let names = remembered_workspace_names();
    if names.is_empty() {
        println!("uniterm: no stopped Workspaces to forget");
        return 0;
    }
    let mut forgotten = 0;
    let mut failures = Vec::new();
    for name in names {
        match forget_stopped_workspace(&name) {
            Ok(true) => forgotten += 1,
            Ok(false) => {}
            Err(error) => failures.push(error),
        }
    }
    if forgotten > 0 {
        let label = if forgotten == 1 {
            "Workspace"
        } else {
            "Workspaces"
        };
        println!("uniterm: forgot {forgotten} {label}");
    }
    for error in &failures {
        eprintln!("uniterm workspace forget: {error}");
    }
    if failures.is_empty() {
        0
    } else {
        1
    }
}

/// Permanently remove a stopped Workspace definition and any stale crash
/// recovery artifacts. Running Workspaces must be stopped explicitly first.
fn cmd_forget(args: &[String]) -> i32 {
    if args == ["--all"] {
        return cmd_forget_all();
    }
    let Some(name) = args.first().map(String::as_str) else {
        eprintln!("uniterm workspace forget: usage: ut workspace forget <name|--all>");
        return 2;
    };
    match forget_stopped_workspace(name) {
        Ok(true) => {
            println!("uniterm: forgot Workspace '{name}'");
            0
        }
        Ok(false) => {
            eprintln!("uniterm workspace forget: no Workspace '{name}'");
            1
        }
        Err(error) => {
            eprintln!("uniterm workspace forget: {error}");
            1
        }
    }
}

fn print_help() {
    println!(
        "uniterm {} - a terminal multiplexer built for agentic engineering\n\
         \n\
         USAGE:\n\
         \x20 uniterm [name]                    attach to a Workspace, creating it if needed\n\
         \x20 uniterm workspace list            list running and stopped Workspaces\n\
         \x20 uniterm workspace default [NAME]  show or set the default Workspace\n\
         \x20 uniterm workspace new [-d] NAME   create a Workspace\n\
         \x20 uniterm workspace switch NAME     attach to a Workspace\n\
         \x20 uniterm workspace rename OLD NEW  rename a Workspace\n\
         \x20 uniterm workspace stop NAME|--all stop one or all running Workspaces\n\
         \x20 uniterm workspace forget NAME|--all permanently forget stopped Workspaces\n\
         \x20 uniterm project list [-w NAME]    list Projects\n\
         \x20 uniterm project add NAME PATH     add a Project and first Tab\n\
         \x20 uniterm project switch NAME       switch Project\n\
         \x20 uniterm project move NAME up|down reorder a Project\n\
         \x20 uniterm project worktree ...     create, list, open, remove, or clean worktrees\n\
         \x20 uniterm migrate from-desktop     import Desktop Workspaces, Projects, and Tabs\n\
         \x20 uniterm agent list [--json]       every running agent with status and Pane\n\
         \x20 uniterm agent start|prompt|wait ... control agents by stable Pane id\n\
         \x20 uniterm agent attach PANE [--observe|--takeover] direct Pane stream\n\
         \x20 uniterm agent explain [PANE]      explain agent detection\n\
         \x20 uniterm agent manifests validate PATH validate provider data offline\n\
         \x20 uniterm instruction add PANE TEXT queue direction for an agent\n\
         \x20 uniterm instruction list|replace|cancel|send-now ... manage direction\n\
         \x20 uniterm run list [--active]      inspect native agent run relationships\n\
         \x20 uniterm run fork RUN NAME PATH   launch an isolated child Run\n\
         \x20 uniterm artifact list [--run ID] inspect durable produced artifacts\n\
         \x20 uniterm waiting list|answer ...   handle the human-attention queue\n\
         \x20 uniterm pane list [--json]        list live Panes and stable ids\n\
         \x20 uniterm pane focus PANE           focus a Pane by stable id\n\
         \x20 uniterm pane read|send-keys|wait-output ... automate a Pane\n\
         \x20 uniterm tab focus PROJECT TAB     focus a Tab by name or ordinal\n\
         \x20 uniterm tab new [PROJECT]         create a Tab and print its ordinal\n\
         \x20 uniterm tab rename PROJECT TAB NAME name a Tab\n\
         \x20 uniterm tab move left|right       reorder the active Tab\n\
         \x20 uniterm pane focus PROJECT TAB PANE focus by hierarchy ordinals\n\
         \x20 uniterm pane metadata ...         publish sidebar metadata\n\
         \x20 uniterm config check [PATH]       validate every config line\n\
         \x20 uniterm remote HOST [NAME] [--pane ID] attach over SSH\n\
         \x20 uniterm --version\n\
         \x20 uniterm --skill                   print agent-driving instructions\n\
         \x20 uniterm help QUERY                search command help\n\
         \n\
         While attached, the prefix is Ctrl-A:\n\
         \x20 d detach   %% split L/R   \" split T/B   h/j/k/l focus   z zoom\n\
         \x20 x kill pane   c new Tab   n/p next/prev Tab   </> move Tab   ; last Pane\n\
         \x20 1-9 select Tab (0 = 10)   [ copy-mode with w/b/e big-word motion\n\
         \x20 P Projects   g Settings   s Workspaces\n\
         \n\
         See docs/ for the design and current build status.",
        env!("CARGO_PKG_VERSION")
    );
}

fn print_help_search(query: &str) -> i32 {
    let query = query.to_ascii_lowercase();
    let topics = [
        (
            "pane automation read output send keys wait",
            "ut pane list|read|send-keys|wait-output ...",
        ),
        (
            "agent automation start prompt read wait attach",
            "ut agent list|start|prompt|send-keys|read|wait|attach|explain ...",
        ),
        (
            "waiting queue human attention answer dismiss stop resume",
            "ut waiting list|focus|answer|dismiss|stop|resume|rollback ...",
        ),
        (
            "instruction queue steer direction follow-up replace cancel send now",
            "ut instruction list|add|replace|cancel|send-now ...",
        ),
        (
            "run graph roles parent child lifecycle automation",
            "ut run list [-w Workspace] [--project ID] [--active] [--json]; ut run fork PARENT NAME PATH [BASE]",
        ),
        (
            "artifact ledger evidence plan patch report digest produced files",
            "ut artifact list [-w Workspace] [--project ID] [--run ID] [--all] [--json]",
        ),
        (
            "tab focus reorder move",
            "ut tab focus PROJECT TAB | ut tab new [PROJECT] | ut tab rename PROJECT TAB NAME | ut tab move left|right",
        ),
        (
            "config validate bindings",
            "ut config check [PATH]; bind.KEY = semantic-action",
        ),
        (
            "worktree isolation git project",
            "ut project worktree list|add|open|remove|cleanup ...",
        ),
        ("skill agents integration", "ut --skill"),
        (
            "workspace session",
            "ut workspace list|new|switch|rename|stop|forget ...",
        ),
    ];
    let mut found = false;
    for (keywords, usage) in topics {
        if keywords.contains(&query) || query.split_whitespace().all(|word| keywords.contains(word))
        {
            println!("{usage}");
            found = true;
        }
    }
    if !found {
        eprintln!("uniterm help: no topic matching '{query}'");
    }
    i32::from(!found)
}

#[cfg(test)]
mod tests {
    use super::{
        bounded_workspace_key, checked_socket_path, classify_socket_query,
        configured_default_workspace, merge_default_workspace, migration_handoff,
        migration_workspace_key, parse_artifact_claim, parse_migration_options, shell_or_default,
        wait_for_server_until, SocketHealth,
    };
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn shell_fallback_is_termux_compatible_without_changing_other_platforms() {
        assert_eq!(
            shell_or_default(Some("/custom/shell".into()), true),
            "/custom/shell"
        );
        assert_eq!(shell_or_default(None, true), "sh");
        assert_eq!(shell_or_default(None, false), "/bin/sh");
    }

    #[test]
    fn artifact_claims_accept_typed_prefixes_without_stealing_equals_from_paths() {
        let plan = parse_artifact_claim("plan=WORKFLOW_PLAN.md");
        assert_eq!(plan.kind, uniterm_core::ArtifactKind::Plan);
        assert_eq!(plan.path, "WORKFLOW_PLAN.md");

        let path = parse_artifact_claim("reports/result=green.txt");
        assert_eq!(path.kind, uniterm_core::ArtifactKind::File);
        assert_eq!(path.path, "reports/result=green.txt");
    }

    #[test]
    fn detached_start_reports_child_exit() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "echo startup-boom >&2; exit 7"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let socket = std::env::temp_dir().join(format!(
            "uniterm-cli-no-socket-{}-exit.sock",
            std::process::id()
        ));
        let error = wait_for_server_until(
            &mut child,
            &socket,
            "broken",
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exited during startup"));
        assert!(error.to_string().contains("7"));
        assert!(error.to_string().contains("startup-boom"));
    }

    #[test]
    fn detached_start_timeout_reaps_child() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 5"])
            .spawn()
            .unwrap();
        let socket = std::env::temp_dir().join(format!(
            "uniterm-cli-no-socket-{}-timeout.sock",
            std::process::id()
        ));
        let error = wait_for_server_until(
            &mut child,
            &socket,
            "slow",
            Instant::now() + Duration::from_millis(20),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn ambiguous_query_failures_are_not_classified_as_stale() {
        assert!(matches!(
            classify_socket_query(Err(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied
            ))),
            SocketHealth::Indeterminate(_)
        ));
        assert!(matches!(
            classify_socket_query(Err(std::io::Error::from(std::io::ErrorKind::TimedOut))),
            SocketHealth::Indeterminate(_)
        ));
        assert!(matches!(
            classify_socket_query(Err(std::io::Error::from(
                std::io::ErrorKind::ConnectionRefused
            ))),
            SocketHealth::Stale
        ));
    }

    #[test]
    fn migration_workspace_keys_cannot_escape_storage_directories() {
        assert_eq!(migration_workspace_key("My Work"), "My-Work");
        assert_eq!(migration_workspace_key("../../outside"), "outside");
        assert_eq!(migration_workspace_key("///"), "desktop");
    }

    #[test]
    fn every_cli_socket_path_uses_the_canonical_workspace_validator() {
        assert!(checked_socket_path("work.api").is_ok());
        assert!(checked_socket_path("../outside").is_err());
        assert!(checked_socket_path("two words").is_err());

        let archived = bounded_workspace_key(
            &"x".repeat(uniterm_proto::MAX_WORKSPACE_NAME_BYTES),
            Some("before-desktop-1234567890"),
        );
        assert!(archived.len() <= uniterm_proto::MAX_WORKSPACE_NAME_BYTES);
        assert!(uniterm_proto::validate_workspace_name(&archived).is_ok());
    }

    #[test]
    fn default_workspace_config_uses_the_last_valid_assignment() {
        let text = "default-workspace = Personal\ndefault-workspace = Work # current\n";
        assert_eq!(configured_default_workspace(text).as_deref(), Some("Work"));
        assert_eq!(
            configured_default_workspace("default-workspace = ../bad"),
            None
        );
    }

    #[test]
    fn setting_default_workspace_preserves_other_config_and_deduplicates() {
        let existing = "# personal\ntheme = nord\ndefault-workspace = Old\nsidebar = true\ndefault-workspace = Older\n";
        let merged = merge_default_workspace(existing, "Work");
        assert!(merged.contains("# personal\n"));
        assert!(merged.contains("theme = nord\n"));
        assert!(merged.contains("sidebar = true\n"));
        assert_eq!(merged.matches("default-workspace =").count(), 1);
        assert_eq!(
            configured_default_workspace(&merged).as_deref(),
            Some("Work")
        );
    }

    #[test]
    fn migration_conflict_policy_is_explicitly_parsed() {
        let options =
            parse_migration_options(&["--dry-run".into(), "--on-conflict".into(), "merge".into()])
                .unwrap();
        assert!(options.dry_run);
        assert!(matches!(
            options.conflict,
            super::MigrationConflictPolicy::Merge
        ));
    }

    #[test]
    fn settings_migration_handoff_uses_imported_targets_and_refreshes_many() {
        let current = PathBuf::from("/tmp/original.sock");
        let imported = PathBuf::from("/tmp/imported.sock");
        let one = migration_handoff(&current, std::slice::from_ref(&imported));
        assert_eq!(one.socket, imported);
        assert!(!one.open_workspaces);

        let multiple = migration_handoff(
            &current,
            &[PathBuf::from("/tmp/other.sock"), current.clone()],
        );
        assert_eq!(multiple.socket, current);
        assert!(multiple.open_workspaces);

        let cancelled = migration_handoff(&current, &[]);
        assert_eq!(cancelled.socket, current);
        assert!(!cancelled.open_workspaces);
    }
}
