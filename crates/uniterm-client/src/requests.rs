//! One-shot control-socket requests.
//!
//! Every helper here opens its own short-lived blocking connection, sends one
//! `ClientMessage`, and waits for the matching reply. They are how the CLI
//! reaches the same command vocabulary the attach client uses, so no command
//! exists for only one of the two front doors (`docs/10`).

use std::path::Path;

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};

/// Query a running server for its window/pane counts (used by `uniterm ls`).
/// Uses a plain blocking connection - no tty, no raw mode.
pub fn query_info(sock_path: &Path) -> std::io::Result<(u32, u32)> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_millis(500)))?;
    stream.write_all(&encode_frame(&ClientMessage::ListInfo))?;
    stream.flush()?;

    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 4096];
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
    while std::time::Instant::now() < deadline {
        let n = match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                break
            }
            Err(e) => return Err(e),
        };
        dec.push(&buf[..n]);
        while let Ok(Some(msg)) = dec.decode::<ServerMessage>() {
            if let ServerMessage::Info { windows, panes } = msg {
                return Ok((windows, panes));
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no info response",
    ))
}

/// Read or mutate the current Workspace hierarchy. Mutations are answered by
/// the server's post-change projection, so CLI output never guesses at state.
pub fn workspace_request(
    sock_path: &Path,
    request: ClientMessage,
) -> std::io::Result<(
    String,
    uniterm_core::ProjectId,
    Vec<uniterm_proto::ProjectInfo>,
)> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_millis(800)))?;
    stream.write_all(&encode_frame(&request))?;
    stream.flush()?;
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.push(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::Workspace {
                name,
                active_project,
                projects,
            } = message
            {
                return Ok((name, active_project, projects));
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no Workspace response",
    ))
}

/// Run one Git worktree lifecycle command and wait for its authoritative
/// runtime result. Git operations may legitimately take longer than ordinary
/// hierarchy reads, so this uses a bounded 30 second response window.
pub fn worktree_request(
    sock_path: &Path,
    operation: uniterm_proto::WorktreeOperation,
) -> std::io::Result<uniterm_proto::WorktreeResult> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    stream.write_all(&encode_frame(&ClientMessage::Worktree { operation }))?;
    stream.flush()?;
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.push(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::Worktrees(result) = message {
                return Ok(result);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no worktree response",
    ))
}

/// Create a Git-isolated child Run and wait for both Git and orchestration
/// authority to report the final combined outcome.
pub fn run_fork(
    sock_path: &Path,
    fork: uniterm_proto::RunForkRequest,
) -> std::io::Result<uniterm_proto::RunForkResult> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    stream.write_all(&encode_frame(&ClientMessage::RunFork { fork }))?;
    stream.flush()?;
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.push(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::RunForked(result) = message {
                return Ok(result);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no child Run response",
    ))
}

/// List every live Pane in one running Workspace without attaching a TTY.
pub fn pane_list(sock_path: &Path) -> std::io::Result<(String, Vec<uniterm_proto::PaneInfo>)> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_millis(800)))?;
    stream.write_all(&encode_frame(&ClientMessage::PaneList))?;
    stream.flush()?;
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.push(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::Panes { workspace, panes } = message {
                return Ok((workspace, panes));
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no Pane list response",
    ))
}

/// Inspect the Workspace-native run graph without attaching a TTY.
pub fn run_list(
    sock_path: &Path,
    project: Option<uniterm_core::ProjectId>,
    active_only: bool,
) -> std::io::Result<(String, Vec<uniterm_proto::RunEntry>)> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_millis(800)))?;
    stream.write_all(&encode_frame(&ClientMessage::RunList {
        project,
        active_only,
    }))?;
    stream.flush()?;
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.push(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::Runs { workspace, runs } = message {
                return Ok((workspace, runs));
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no run graph response",
    ))
}

/// Inspect the Workspace-native typed artifact ledger without attaching a TTY.
pub fn artifact_list(
    sock_path: &Path,
    project: Option<uniterm_core::ProjectId>,
    run: Option<uniterm_core::RunId>,
    include_superseded: bool,
) -> std::io::Result<(String, Vec<uniterm_proto::ArtifactEntry>)> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_millis(800)))?;
    stream.write_all(&encode_frame(&ClientMessage::ArtifactList {
        project,
        run,
        include_superseded,
    }))?;
    stream.flush()?;
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.push(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::Artifacts {
                workspace,
                artifacts,
            } = message
            {
                return Ok((workspace, artifacts));
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no Artifact list response",
    ))
}

/// Focus one stable Pane id and wait for the server to confirm it still exists.
pub fn pane_focus(sock_path: &Path, pane: uniterm_core::PaneId) -> std::io::Result<bool> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_millis(800)))?;
    stream.write_all(&encode_frame(&ClientMessage::PaneFocus { pane }))?;
    stream.flush()?;
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 4096];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.push(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::PaneFocused {
                pane: response,
                found,
            } = message
            {
                if response == pane {
                    return Ok(found);
                }
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no Pane focus response",
    ))
}

/// Read a bounded recent-output projection without scraping another process or
/// changing Pane focus. `None` means the stable Pane id is stale.
pub fn pane_read(
    sock_path: &Path,
    pane: uniterm_core::PaneId,
    lines: u32,
) -> std::io::Result<Option<(String, bool)>> {
    match control_reply(
        sock_path,
        ClientMessage::PaneRead { pane, lines },
        std::time::Duration::from_millis(800),
    )? {
        ServerMessage::PaneOutput {
            pane: response,
            found,
            text,
            truncated,
        } if response == pane => Ok(found.then_some((text, truncated))),
        _ => Err(unexpected_control_reply("Pane output")),
    }
}

/// Send exact bytes to a stable Pane and wait for authoritative acceptance.
///
/// The `Ok(Some(accepted))` form reports an existing Pane; `accepted` is false
/// when the server's pending-input queue was full and the bytes were dropped.
pub fn pane_send(
    sock_path: &Path,
    pane: uniterm_core::PaneId,
    bytes: Vec<u8>,
) -> std::io::Result<Option<bool>> {
    match control_reply(
        sock_path,
        ClientMessage::PaneSend { pane, bytes },
        std::time::Duration::from_millis(800),
    )? {
        ServerMessage::PaneSent {
            pane: response,
            found,
            accepted,
        } if response == pane => Ok(found.then_some(accepted)),
        _ => Err(unexpected_control_reply("Pane input acknowledgement")),
    }
}

/// Move the active Tab semantically and wait for the server's no-op/moved
/// acknowledgement.
pub fn tab_move(
    sock_path: &Path,
    direction: uniterm_proto::TabMoveDirection,
) -> std::io::Result<bool> {
    match control_reply(
        sock_path,
        ClientMessage::TabMove { direction },
        std::time::Duration::from_millis(800),
    )? {
        ServerMessage::TabMoved { moved } => Ok(moved),
        _ => Err(unexpected_control_reply("Tab move acknowledgement")),
    }
}

/// Outcome of an output wait armed inside the server event loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneWaitResult {
    pub found: bool,
    pub matched: bool,
    pub timed_out: bool,
    pub text: String,
    pub truncated: bool,
}

pub fn pane_wait_output(
    sock_path: &Path,
    pane: uniterm_core::PaneId,
    needle: String,
    timeout: std::time::Duration,
) -> std::io::Result<PaneWaitResult> {
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    match control_reply(
        sock_path,
        ClientMessage::PaneWaitOutput {
            pane,
            needle,
            timeout_ms,
        },
        timeout + std::time::Duration::from_millis(800),
    )? {
        ServerMessage::PaneOutputWaited {
            pane: response,
            found,
            matched,
            timed_out,
            text,
            truncated,
        } if response == pane => Ok(PaneWaitResult {
            found,
            matched,
            timed_out,
            text,
            truncated,
        }),
        _ => Err(unexpected_control_reply("Pane output wait")),
    }
}

/// Outcome of an agent-status wait armed inside the server event loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentWaitResult {
    pub found: bool,
    pub matched: bool,
    pub timed_out: bool,
    pub status: Option<uniterm_core::AgentStatus>,
}

pub fn agent_wait(
    sock_path: &Path,
    pane: uniterm_core::PaneId,
    status: uniterm_core::AgentStatus,
    timeout: std::time::Duration,
) -> std::io::Result<AgentWaitResult> {
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    match control_reply(
        sock_path,
        ClientMessage::AgentWait {
            pane,
            status,
            timeout_ms,
        },
        timeout + std::time::Duration::from_millis(800),
    )? {
        ServerMessage::AgentWaited {
            pane: response,
            found,
            matched,
            timed_out,
            status,
        } if response == pane => Ok(AgentWaitResult {
            found,
            matched,
            timed_out,
            status,
        }),
        _ => Err(unexpected_control_reply("agent wait")),
    }
}

/// Launch an agent and wait until the server confirms the target Pane was
/// accepted or spawned. `None` is an authoritative launch failure.
pub fn agent_launch(
    sock_path: &Path,
    agent: String,
    target: uniterm_proto::LaunchTarget,
) -> std::io::Result<Option<uniterm_core::PaneId>> {
    match control_reply(
        sock_path,
        ClientMessage::AgentLaunch {
            agent: agent.clone(),
            target,
        },
        std::time::Duration::from_secs(2),
    )? {
        ServerMessage::AgentLaunchResult {
            agent: response,
            pane,
        } if response == agent => Ok(pane),
        _ => Err(unexpected_control_reply("agent launch")),
    }
}

fn control_reply(
    sock_path: &Path,
    request: ClientMessage,
    timeout: std::time::Duration,
) -> std::io::Result<ServerMessage> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.write_all(&encode_frame(&request))?;
    stream.flush()?;
    let mut decoder = FrameDecoder::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = match std::io::Read::read(&mut stream, &mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        };
        decoder.push(&buffer[..read]);
        if let Some(message) = decoder.decode::<ServerMessage>().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid server frame: {error:?}"),
            )
        })? {
            return Ok(message);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no control response",
    ))
}

fn unexpected_control_reply(expected: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("unexpected response while waiting for {expected}"),
    )
}

/// Focus one Project/Tab location and wait for the authoritative stable Pane.
///
/// `tab` and `pane` are 1-based ordinals from [`pane_list`]. When `pane` is
/// omitted, the target Tab keeps its remembered active Pane.
pub fn hierarchy_focus(
    sock_path: &Path,
    project: uniterm_core::ProjectId,
    tab: u32,
    pane: Option<u32>,
) -> std::io::Result<Option<uniterm_core::PaneId>> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_millis(800)))?;
    stream.write_all(&encode_frame(&ClientMessage::HierarchyFocus {
        project,
        tab,
        pane,
    }))?;
    stream.flush()?;
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 4096];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.push(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::HierarchyFocused {
                project: response_project,
                tab: response_tab,
                pane: response_pane,
                focused,
            } = message
            {
                if response_project == project && response_tab == tab && response_pane == pane {
                    return Ok(focused);
                }
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no hierarchy focus response",
    ))
}

/// Apply one validated hierarchy import and wait for the server's committed
/// counts. Imports may spawn many fresh shell PTYs, so this control path uses a
/// longer timeout than ordinary hierarchy queries.
pub fn import_workspace(
    sock_path: &Path,
    workspace: uniterm_proto::ImportedWorkspace,
    mode: uniterm_proto::WorkspaceImportMode,
) -> std::io::Result<(u32, u32, u32)> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    stream.write_all(&encode_frame(&ClientMessage::WorkspaceImport {
        workspace,
        mode,
    }))?;
    stream.flush()?;
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.push(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::WorkspaceImported {
                projects_added,
                tabs_added,
                projects_merged,
                error,
            } = message
            {
                if let Some(error) = error {
                    return Err(std::io::Error::other(error));
                }
                return Ok((projects_added, tabs_added, projects_merged));
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no Workspace import response",
    ))
}

/// Ask a Workspace to explain agent detection for one Pane or every Pane.
pub fn agent_explain(
    sock_path: &Path,
    pane: Option<uniterm_core::PaneId>,
) -> std::io::Result<Vec<uniterm_proto::AgentDetectionInfo>> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_millis(800)))?;
    stream.write_all(&encode_frame(&ClientMessage::AgentExplain { pane }))?;
    stream.flush()?;
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.push(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::AgentExplanation { entries } = message {
                return Ok(entries);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no agent explanation response",
    ))
}

/// Ask a running server to stop (used by `uniterm kill`).
pub fn kill_server(sock_path: &Path) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.write_all(&encode_frame(&ClientMessage::KillServer))?;
    stream.flush()
}

/// Deliver one fire-and-forget control message without attaching a TTY.
pub fn control(sock_path: &Path, message: ClientMessage) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.write_all(&encode_frame(&message))?;
    stream.flush()
}

/// Return the Workspace-scoped human-attention queue without attaching a TTY.
pub fn waiting_list(sock_path: &Path) -> std::io::Result<Vec<uniterm_proto::WaitingEntry>> {
    waiting_request(sock_path, ClientMessage::WaitingList).map(|(_, _, items)| items)
}

/// Apply one semantic waiting action and return its authoritative result.
pub fn waiting_act(
    sock_path: &Path,
    id: u64,
    action: uniterm_proto::WaitingAction,
    text: String,
) -> std::io::Result<(bool, bool, Vec<uniterm_proto::WaitingEntry>)> {
    waiting_request(sock_path, ClientMessage::WaitingAct { id, action, text })
}

fn waiting_request(
    sock_path: &Path,
    request: ClientMessage,
) -> std::io::Result<(bool, bool, Vec<uniterm_proto::WaitingEntry>)> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_millis(800)))?;
    stream.write_all(&encode_frame(&request))?;
    stream.flush()?;
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.push(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            match message {
                ServerMessage::Waiting { items } => return Ok((true, true, items)),
                ServerMessage::WaitingActed {
                    found,
                    accepted,
                    items,
                    ..
                } => return Ok((found, accepted, items)),
                _ => {}
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no waiting queue response",
    ))
}

/// Return queued human direction for this Workspace.
pub fn instruction_list(sock_path: &Path) -> std::io::Result<Vec<uniterm_proto::InstructionEntry>> {
    instruction_request(sock_path, ClientMessage::InstructionList).map(|(_, _, _, items)| items)
}

/// Queue direction for the Pane's current agent invocation.
pub fn instruction_add(
    sock_path: &Path,
    pane: uniterm_core::PaneId,
    author: uniterm_core::InstructionAuthor,
    text: String,
) -> std::io::Result<(u64, bool, bool, Vec<uniterm_proto::InstructionEntry>)> {
    instruction_request(
        sock_path,
        ClientMessage::InstructionAdd { pane, author, text },
    )
}

/// Replace queued direction with a fresh durable item.
pub fn instruction_replace(
    sock_path: &Path,
    id: u64,
    author: uniterm_core::InstructionAuthor,
    text: String,
) -> std::io::Result<(u64, bool, bool, Vec<uniterm_proto::InstructionEntry>)> {
    instruction_request(
        sock_path,
        ClientMessage::InstructionReplace { id, author, text },
    )
}

/// Cancel queued direction without injecting it.
pub fn instruction_cancel(
    sock_path: &Path,
    id: u64,
) -> std::io::Result<(u64, bool, bool, Vec<uniterm_proto::InstructionEntry>)> {
    instruction_request(sock_path, ClientMessage::InstructionCancel { id })
}

/// Explicitly inject queued direction without waiting for cooperative ready.
pub fn instruction_send_now(
    sock_path: &Path,
    id: u64,
) -> std::io::Result<(u64, bool, bool, Vec<uniterm_proto::InstructionEntry>)> {
    instruction_request(sock_path, ClientMessage::InstructionSendNow { id })
}

fn instruction_request(
    sock_path: &Path,
    request: ClientMessage,
) -> std::io::Result<(u64, bool, bool, Vec<uniterm_proto::InstructionEntry>)> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_millis(800)))?;
    stream.write_all(&encode_frame(&request))?;
    stream.flush()?;
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.push(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            match message {
                ServerMessage::Instructions { items } => return Ok((0, true, true, items)),
                ServerMessage::InstructionChanged {
                    id,
                    found,
                    accepted,
                    items,
                } => return Ok((id, found, accepted, items)),
                _ => {}
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no instruction queue response",
    ))
}

/// Deliver a workflow completion-contract submission (`uniterm workflow
/// submit <token> ...`) to the session at `sock_path`. Fire-and-forget: the
/// server drops forged/stale tokens silently.
pub fn workflow_submit(
    sock_path: &Path,
    token: u64,
    failed: bool,
    verdict: Option<String>,
    summary: String,
) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.write_all(&encode_frame(&ClientMessage::WorkflowSubmit {
        token,
        failed,
        verdict,
        summary,
    }))?;
    stream.flush()
}

/// Deliver the versioned workflow or relay completion contract.
pub fn orchestration_submit(
    sock_path: &Path,
    kind: uniterm_proto::OrchestrationKind,
    token: u64,
    status: uniterm_proto::SubmissionStatus,
    verdict: Option<String>,
    summary: String,
    artifacts: Vec<uniterm_proto::ArtifactClaim>,
) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let mut stream = StdUnixStream::connect(sock_path)?;
    stream.write_all(&encode_frame(&ClientMessage::OrchestrationSubmit {
        kind,
        token,
        status,
        verdict,
        summary,
        artifacts,
    }))?;
    stream.flush()
}

/// Send one control API request to a Workspace's control socket and return
/// its response. The control socket sits beside the Workspace socket as
/// `<name>.control.sock`; the request is one NDJSON line and so is the reply.
/// Used by CLI verbs whose operation exists only in the control vocabulary,
/// such as renaming a Tab by hierarchy position without attaching.
pub fn control_request(
    sock_path: &Path,
    command: uniterm_proto::ControlCommand,
) -> std::io::Result<uniterm_proto::ControlResponse> {
    use std::io::{BufRead as _, Write as _};
    use std::os::unix::net::UnixStream as StdUnixStream;

    let workspace = sock_path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Workspace socket needs a UTF-8 file stem",
            )
        })?
        .to_string();
    let control = sock_path.with_extension("control.sock");
    let mut stream = StdUnixStream::connect(&control)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let request = uniterm_proto::ControlRequest {
        version: uniterm_proto::CONTROL_API_VERSION,
        id: 1,
        workspace,
        command,
    };
    let mut line = serde_json::to_string(&request)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let mut reader = std::io::BufReader::new(stream);
    let mut reply = String::new();
    loop {
        reply.clear();
        if reader.read_line(&mut reply)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the control socket closed before answering",
            ));
        }
        match serde_json::from_str::<uniterm_proto::ControlFrame>(reply.trim_end()) {
            Ok(uniterm_proto::ControlFrame::Response(response)) => return Ok(response),
            Ok(_) => continue,
            Err(error) => {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error));
            }
        }
    }
}
