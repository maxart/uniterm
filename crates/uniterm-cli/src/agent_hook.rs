//! `ut agent hook AGENT` and `ut agent connector ...`.
//!
//! The hook verb is what an installed connector runs inside a Pane: it reads
//! one provider hook invocation from stdin, asks the provider's connector
//! module to translate it, and writes a single bounded OSC 777 envelope to the
//! Pane's tty, falling back to its exact inherited control socket when the
//! harness detaches hooks from that tty. It never fails loudly: a hook that
//! cannot be understood exits non-zero in silence, so the agent is never
//! slowed or interrupted and no status is ever synthesized
//! (`docs/06-agentic-supervision.md`).
//!
//! The connector verb installs, upgrades, removes, and reports connectors
//! from the same functions the Agents surface uses; every reported state is
//! re-read from disk after the change.

use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use uniterm_proto::{
    ConnectorStatus, ControlCommand, ControlFrame, ControlRequest, ControlResult, PaneId,
    CONTROL_API_VERSION, CONTROL_MAX_FRAME_BYTES,
};

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
    // Outside a Uniterm Pane there is no authorized destination for a hook.
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
    i32::from(written.is_err() && socket_report(&envelope).is_err())
}

/// Detached hooks retain Pane environment even when `/dev/tty` is unavailable.
/// Query the invocation immediately before reporting so a delayed response
/// cannot be applied to a replacement process in the same Pane.
fn socket_report(envelope: &str) -> io::Result<()> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "invalid hook destination");
    let json = envelope
        .strip_prefix("\x1b]777;notify;uniterm://cli-agent;")
        .and_then(|value| value.strip_suffix('\x07'))
        .ok_or_else(invalid)?;
    let socket = PathBuf::from(std::env::var_os("UNITERM_SOCKET").ok_or_else(invalid)?);
    let workspace = socket
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(invalid)?;
    let pane = PaneId(
        std::env::var("UNITERM_PANE_ID")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .ok_or_else(invalid)?,
    );
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut reader = BufReader::new(connect_hook(
        &socket.with_extension("control.sock"),
        deadline,
    )?);
    let context = hook_request(
        &mut reader,
        workspace,
        1,
        ControlCommand::AgentHookContext { pane },
        deadline,
    )?;
    let ControlResult::AgentHookContext {
        pane: observed,
        foreground_pid: Some(foreground_pid),
        invocation,
    } = context
    else {
        return Err(invalid());
    };
    if observed != pane {
        return Err(invalid());
    }
    match hook_request(
        &mut reader,
        workspace,
        2,
        ControlCommand::AgentHookReport {
            pane,
            source_pid: std::process::id() as i32,
            foreground_pid,
            invocation,
            json: json.to_owned(),
        },
        deadline,
    )? {
        ControlResult::Mutation {
            resource,
            found: true,
            accepted: true,
            ..
        } if resource == "agent_hook" => Ok(()),
        _ => Err(io::Error::other("hook report rejected")),
    }
}

/// A full local listen backlog must not block the harness inside `connect`.
/// Nonblocking Unix sockets fail immediately on that condition on Linux;
/// platforms returning EINPROGRESS share the report's remaining deadline.
fn connect_hook(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    // SAFETY: sockaddr_un is an integer/byte structure; zero initializes its
    // padding and terminates the pathname copied below.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let path = path.as_os_str().as_bytes();
    if path.is_empty() || path.contains(&0) || path.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid hook socket",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path.iter_mut().zip(path) {
        *target = *byte as libc::c_char;
    }
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + path.len() + 1;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as u8;
    }
    // SAFETY: socket has no pointer arguments. A successful descriptor is
    // immediately transferred to UnixStream, which closes it on every return.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh, uniquely owned Unix stream socket.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: fd remains valid and F_SETFD takes an integer flag argument.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    stream.set_nonblocking(true)?;
    // SAFETY: address and its checked length describe an initialized Unix
    // socket address and remain alive throughout this synchronous call.
    if unsafe {
        libc::connect(
            fd,
            (&address as *const libc::sockaddr_un).cast(),
            length as libc::socklen_t,
        )
    } < 0
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|duration| !duration.is_zero())
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "hook connect timed out"))?;
            let mut descriptor = libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: poll receives one initialized descriptor owned above.
            let ready =
                unsafe { libc::poll(&mut descriptor, 1, remaining.as_millis().max(1) as i32) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                continue;
            }
            if let Some(error) = stream.take_error()? {
                return Err(error);
            }
            if descriptor.revents & libc::POLLOUT == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "hook socket disconnected",
                ));
            }
            break;
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn hook_request(
    reader: &mut BufReader<UnixStream>,
    workspace: &str,
    id: u64,
    command: ControlCommand,
    deadline: Instant,
) -> io::Result<ControlResult> {
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "hook report timed out"))
    };
    reader.get_ref().set_write_timeout(Some(remaining()?))?;
    writeln!(
        reader.get_mut(),
        "{}",
        serde_json::to_string(&ControlRequest {
            version: CONTROL_API_VERSION,
            id,
            workspace: workspace.to_owned(),
            command,
        })?
    )?;
    let mut line = Vec::new();
    loop {
        // Recompute the deadline per read: a partial response must not renew
        // the hook's budget and keep the harness waiting indefinitely.
        reader.get_ref().set_read_timeout(Some(remaining()?))?;
        let available = reader.fill_buf()?;
        let length = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if length == 0 || line.len() + length > CONTROL_MAX_FRAME_BYTES as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid hook response",
            ));
        }
        line.extend_from_slice(&available[..length]);
        reader.consume(length);
        if line.ends_with(b"\n") {
            break;
        }
    }
    match serde_json::from_slice::<ControlFrame>(&line)? {
        ControlFrame::Response(response)
            if response.version == CONTROL_API_VERSION
                && response.id == id
                && response.error.is_none() =>
        {
            response
                .result
                .ok_or_else(|| io::Error::other("missing hook result"))
        }
        _ => Err(io::Error::other("hook request rejected")),
    }
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
