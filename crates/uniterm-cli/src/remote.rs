//! SSH transport for the existing Uniterm client protocol.
//!
//! The remote server remains Unix-socket-only. A short-lived command on the
//! remote host copies that socket byte stream over SSH stdio, while a private
//! local proxy socket lets the unchanged thin client keep its mio hot path.

mod forward;
mod handshake;

use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use uniterm_proto::WIRE_PROTOCOL_VERSION;

const REMOTE_READY: &[u8] = b"UNITERM-REMOTE/1\n";
const MAX_REMOTE_PREAMBLE: usize = 64 * 1024;
/// Longest the remote login shell may take to reach the Uniterm handshake.
/// Generous enough for password and second-factor prompts, which SSH reads
/// from the controlling terminal, but bounded: an rc file that blocks on
/// stdin, opens a pager, or auto-attaches another multiplexer must produce a
/// diagnostic instead of a silent hang with the terminal still in cooked mode.
const REMOTE_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// How long one open OpenSSH prompt (host key, password, second factor) may
/// wait for the person at the keyboard. Reading a fingerprint or fetching a
/// code is not handshake time, so the bound above is suspended meanwhile.
/// Only a local keystroke renews this one, never output from the remote host.
const REMOTE_PROMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

pub(super) fn cmd_remote(args: &[String]) -> i32 {
    remote_with_origin(args, None, None)
}

// A new client reaches this entry after a menu update inside a saved SSH
// profile. Keep the local return destination across a change of process image.
pub(super) fn resume_client(args: &[String]) -> i32 {
    let Some((origin, remote)) = args.split_first() else {
        println!("uniterm remote resume 1");
        return 0;
    };
    match super::checked_socket_path(origin) {
        Ok(socket) => remote_with_origin(remote, Some(&socket), None),
        Err(_) => 2,
    }
}

pub(super) fn connect_profile(profile: &uniterm_core::machine::MachineProfile) -> i32 {
    remote_with_origin(
        &[profile.target.clone(), profile.workspace.clone()],
        None,
        Some(profile.id),
    )
}

fn remote_with_origin(args: &[String], origin: Option<&Path>, profile_id: Option<u64>) -> i32 {
    let Some(target) = args.first() else {
        eprintln!("uniterm remote: usage: ut remote <ssh-target> [Workspace] [--pane ID] [--observe|--takeover]");
        return 2;
    };
    if let Err(error) = validate_target(target) {
        eprintln!("uniterm remote: {error}");
        return 2;
    }
    let mut workspace = None;
    let mut pane = None;
    let mut role = uniterm_proto::PaneAttachRole::Controller;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--pane" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("uniterm remote: --pane requires a stable Pane id");
                    return 2;
                };
                pane = match value.parse::<u64>() {
                    Ok(value) if value != 0 => Some(uniterm_core::PaneId(value)),
                    _ => {
                        eprintln!("uniterm remote: invalid Pane id '{value}'");
                        return 2;
                    }
                };
                index += 2;
            }
            "--observe" if role == uniterm_proto::PaneAttachRole::Controller => {
                role = uniterm_proto::PaneAttachRole::Observer;
                index += 1;
            }
            "--takeover" if role == uniterm_proto::PaneAttachRole::Controller => {
                role = uniterm_proto::PaneAttachRole::Takeover;
                index += 1;
            }
            value if !value.starts_with('-') && workspace.is_none() => {
                workspace = Some(value);
                index += 1;
            }
            value => {
                eprintln!("uniterm remote: invalid or conflicting option '{value}'");
                return 2;
            }
        }
    }
    if pane.is_none() && role != uniterm_proto::PaneAttachRole::Controller {
        eprintln!("uniterm remote: --observe and --takeover require --pane");
        return 2;
    }
    if let Some(workspace) = workspace {
        if let Err(error) = uniterm_proto::validate_workspace_name(workspace) {
            eprintln!("uniterm remote: invalid Workspace name '{workspace}': {error}");
            return 2;
        }
    }

    let mut terminal = uniterm_client::AttachTerminal::default();
    match run_remote(
        target,
        workspace,
        pane.map(|pane| (pane, role)),
        origin,
        profile_id,
        &mut terminal,
        None,
    ) {
        Ok(true) => {
            let socket = match origin.map(Path::to_owned) {
                Some(socket) => socket,
                None => match super::ensure_workspace_available(&super::default_workspace()) {
                    Ok(socket) => socket,
                    Err(error) => {
                        terminal.release();
                        eprintln!("uniterm: {error}");
                        return 1;
                    }
                },
            };
            super::attach_loop(socket, None, terminal)
        }
        Ok(false) => 0,
        Err(error) => {
            if let Some(socket) = origin {
                return super::attach_loop(socket.to_owned(), Some(error.to_string()), terminal);
            }
            terminal.release();
            eprintln!("uniterm remote: {error}");
            1
        }
    }
}

pub(super) fn cmd_remote_check(args: &[String]) -> i32 {
    match parse_protocol(args, false) {
        Ok(()) => {
            println!("uniterm remote protocol {WIRE_PROTOCOL_VERSION}");
            0
        }
        Err(error) => {
            eprintln!("uniterm remote-check: {error}");
            1
        }
    }
}

pub(super) fn cmd_remote_bridge(args: &[String]) -> i32 {
    if let Err(error) = parse_protocol(args, true) {
        eprintln!("uniterm remote-bridge: {error}");
        return 1;
    }
    let workspace = args
        .get(2)
        .cloned()
        .unwrap_or_else(super::default_workspace);
    let socket = match super::ensure_workspace_available(&workspace) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("uniterm remote-bridge: {error}");
            return 1;
        }
    };
    match bridge_remote_stdio(&socket, &workspace) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("uniterm remote-bridge: {error}");
            1
        }
    }
}

fn parse_protocol(args: &[String], optional_workspace: bool) -> Result<(), String> {
    let valid_len = args.len() == 2 || (optional_workspace && args.len() == 3);
    if !valid_len || args.first().map(String::as_str) != Some("--protocol") {
        return Err("invalid internal SSH bridge invocation".into());
    }
    let offered = args[1]
        .parse::<u32>()
        .map_err(|_| "invalid wire protocol version".to_string())?;
    if offered != WIRE_PROTOCOL_VERSION {
        return Err(format!(
            "wire protocol mismatch (local {offered}, remote {WIRE_PROTOCOL_VERSION}); install the same Uniterm build on both hosts"
        ));
    }
    if let Some(workspace) = args.get(2) {
        uniterm_proto::validate_workspace_name(workspace)
            .map_err(|error| format!("invalid Workspace name '{workspace}': {error}"))?;
    }
    Ok(())
}

fn validate_target(target: &str) -> Result<(), &'static str> {
    if target.is_empty() {
        return Err("SSH target must not be empty");
    }
    if target.starts_with('-') {
        return Err("SSH target must not start with '-'");
    }
    if target
        .bytes()
        .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err("SSH target contains a control character");
    }
    Ok(())
}

/// Keep the source attachment until the candidate SSH peer has confirmed its
/// protocol and Workspace socket. The selected outcome consumes this bridge.
pub(super) fn attach(
    terminal: &mut uniterm_client::AttachTerminal,
    socket: &Path,
    options: uniterm_client::AttachOptions,
    candidate: &mut Option<LocalBridge>,
) -> io::Result<uniterm_client::AttachOutcome> {
    terminal.attach_preparing(socket, options, &mut |target, workspace, winch| {
        *candidate = Some(LocalBridge::start(
            SshSpec::new(target),
            Some(workspace),
            Some(winch),
        )?);
        Ok(())
    })
}

pub(super) fn run_remote(
    target: &str,
    workspace: Option<&str>,
    pane: Option<(uniterm_core::PaneId, uniterm_proto::PaneAttachRole)>,
    origin: Option<&Path>,
    mut profile_id: Option<u64>,
    terminal: &mut uniterm_client::AttachTerminal,
    mut candidate: Option<LocalBridge>,
) -> io::Result<bool> {
    let mut target = target.to_string();
    let mut workspace = workspace.map(str::to_string);
    loop {
        let spec = SshSpec::new(&target);
        let bridge = match candidate.take() {
            Some(bridge) => bridge,
            None => LocalBridge::start(spec, workspace.as_deref(), None)?,
        };
        let result = if let Some((pane, role)) = pane {
            uniterm_client::pane_attach(bridge.socket(), pane, role).map(|()| None)
        } else {
            attach(
                terminal,
                bridge.socket(),
                uniterm_client::AttachOptions {
                    open_workspaces: false,
                    machine_error: None,
                    remote: true,
                    remote_target: Some(target.clone()),
                    remote_profile: profile_id,
                },
                &mut candidate,
            )
            .map(Some)
        };
        // A deliberate handoff already has a ready destination. Reap the
        // old transport locally even if its remote peer ignores pipe EOF.
        let switched = matches!(
            &result,
            Ok(Some(
                uniterm_client::AttachOutcome::Machine(..)
                    | uniterm_client::AttachOutcome::LocalMachine
                    | uniterm_client::AttachOutcome::RemoteWorkspace(..)
                    | uniterm_client::AttachOutcome::RemoteUpdate(..)
            ))
        );
        let bridge_result = bridge.finish(switched);
        let outcome = match (result, bridge_result) {
            (Ok(outcome), Ok(())) => outcome,
            (Err(client), Ok(())) => return Err(client),
            (Ok(_), Err(bridge)) => return Err(bridge),
            (Err(client), Err(bridge)) => {
                return Err(io::Error::new(
                    client.kind(),
                    format!("{client}; SSH bridge also failed: {bridge}"),
                ));
            }
        };
        match outcome {
            None | Some(uniterm_client::AttachOutcome::Exit) => return Ok(false),
            Some(uniterm_client::AttachOutcome::LocalMachine) => return Ok(true),
            Some(uniterm_client::AttachOutcome::Machine(profile, _)) => {
                profile_id = Some(profile.id);
                target = profile.target;
                workspace = Some(profile.workspace);
            }
            Some(uniterm_client::AttachOutcome::RemoteWorkspace(next)) => {
                workspace = Some(next);
            }
            Some(
                uniterm_client::AttachOutcome::ReviveWorkspace(_)
                | uniterm_client::AttachOutcome::Update(_),
            ) => {
                return Err(io::Error::other(
                    "the remote client returned a local Workspace handoff",
                ));
            }
            Some(uniterm_client::AttachOutcome::RemoteUpdate(active)) => {
                if active.is_some() {
                    workspace = active;
                }
                terminal.release();
                super::update::interactive(super::update::AttachTarget::Remote {
                    target: &target,
                    workspace: workspace.as_deref(),
                    origin,
                });
            }
            Some(uniterm_client::AttachOutcome::MigrateDesktop) => {
                return Err(io::Error::other(
                    "Desktop migration is unavailable through a remote attach",
                ));
            }
        }
    }
}

/// Connect SSH stdio to the remote server socket.
///
/// The ready line is outside the binary protocol. It lets the local proxy
/// reject login-shell noise and setup failures before forwarding framed bytes.
fn bridge_remote_stdio(socket: &Path, workspace: &str) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    publish_remote_environment(&mut stream, remote_search_path())?;
    bridge_stream(stream, io::stdin(), io::stdout(), workspace)
}

fn remote_search_path() -> Vec<String> {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|entry| entry.is_absolute())
                .map(|entry| entry.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn publish_remote_environment(stream: &mut UnixStream, search_path: Vec<String>) -> io::Result<()> {
    io::Write::write_all(
        stream,
        &uniterm_proto::encode_frame(&uniterm_proto::ClientMessage::RemoteEnvironment {
            search_path,
        }),
    )
}

fn bridge_stream<R, W>(
    stream: UnixStream,
    mut input: R,
    mut output: W,
    workspace: &str,
) -> io::Result<()>
where
    R: io::Read + Send + 'static,
    W: io::Write,
{
    output.write_all(REMOTE_READY)?;
    // The client sees a local proxy socket. Identify the Workspace selected
    // by this host, including when SSH was invoked without a Workspace name.
    // Subsequent live renames arrive in the same frame from the server.
    output.write_all(&uniterm_proto::encode_frame(
        &uniterm_proto::ServerMessage::SessionRenamed {
            name: workspace.to_owned(),
        },
    ))?;
    output.flush()?;
    let mut socket_to_stdout = stream.try_clone()?;
    let mut stdin_to_socket = stream;
    let _upload = thread::spawn(move || {
        let result = copy_with_flush(&mut input, &mut stdin_to_socket);
        let _ = stdin_to_socket.shutdown(std::net::Shutdown::Write);
        result
    });
    let download = copy_with_flush(&mut socket_to_stdout, &mut output);
    // Do not join the stdin copier here. If the server closes first, stdin is
    // still the live SSH pipe and joining would deadlock the remote helper.
    // Returning ends this dedicated bridge process and closes both directions.
    download.map(|_| ())
}

/// Forward each interactive chunk as soon as it arrives. Both peers can
/// wait for a reply to a short frame while keeping their write halves open.
/// A buffered stream copy that waits for EOF or more input can deadlock the
/// attach handshake. SSH stdout also needs an explicit flush for binary data.
fn copy_with_flush(reader: &mut impl io::Read, writer: &mut impl io::Write) -> io::Result<u64> {
    let mut copied = 0;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let read = match reader.read(&mut buf) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            return Ok(copied);
        }
        writer.write_all(&buf[..read])?;
        writer.flush()?;
        copied += read as u64;
    }
}

#[derive(Clone)]
struct SshSpec {
    target: String,
}

impl SshSpec {
    fn new(target: &str) -> Self {
        Self {
            target: target.to_string(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new("ssh");
        command
            .arg("-T")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ServerAliveCountMax=4")
            .arg(&self.target);
        command
    }
}

struct PreparedSshBridge {
    identity: Option<String>,
    /// Attach-stream bytes that arrived behind the probe reply.
    pending: Vec<u8>,
    process: SshProcess,
    stdin: UnixStream,
    stdout: io::BufReader<UnixStream>,
}

struct SshProcess {
    child: Option<Child>,
    prompt: Option<handshake::PromptTty>,
    diagnostics: Option<handshake::Diagnostics>,
}

impl SshProcess {
    fn finish(&mut self, cancel: bool) -> io::Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        // Allow ordinary remote exits to report their status. A peer which
        // ignores EOF cannot keep the physical terminal waiting indefinitely.
        let exited = child.try_wait()?.is_some()
            || (!cancel
                && uniterm_server::process_watch::ProcessWatch::new(child.id() as i32)
                    .and_then(|watch| watch.wait_exit(std::time::Duration::from_millis(250)))
                    .unwrap_or(false));
        if !exited {
            if self.prompt.is_some() {
                // SAFETY: the private session leader is owned and unreaped.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
            }
            let _ = child.kill();
        }
        let status = child.wait()?;
        let detail = self
            .diagnostics
            .take()
            .map(|reader| reader.finish())
            .unwrap_or_default();
        if !cancel && exited && !status.success() {
            let detail = if detail.is_empty() {
                detail
            } else {
                format!(": {detail}")
            };
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("SSH bridge exited with {status}{detail}"),
            ));
        }
        Ok(())
    }
}

impl Drop for SshProcess {
    fn drop(&mut self) {
        let _ = self.finish(true);
    }
}

impl PreparedSshBridge {
    /// Keep protocol bytes, remote diagnostics and OpenSSH prompts on distinct
    /// descriptors. Only the private controlling tty is a source of prompts.
    fn start(spec: &SshSpec, workspace: Option<&str>, winch: Option<RawFd>) -> io::Result<Self> {
        let mut command = spec.command();
        let mut prompt = if winch.is_some() {
            Some(handshake::PromptTty::new()?)
        } else {
            None
        };
        if let Some(tty) = &prompt {
            tty.configure(&mut command);
        }
        let (diagnostics, stderr) = handshake::Diagnostics::new(winch.is_none())?;
        let (mut stdin, from_client) = UnixStream::pair()?;
        let (stdout, to_client) = UnixStream::pair()?;
        let mut child = command
            .arg(uniterm_client::machines::bridge_command(workspace)?)
            .stdin(OwnedFd::from(from_client))
            .stdout(OwnedFd::from(to_client))
            .stderr(stderr)
            .spawn()
            .map_err(|error| {
                io::Error::new(error.kind(), format!("could not start SSH bridge: {error}"))
            })?;
        drop(command);
        let mut stdout = io::BufReader::new(stdout);
        let mut paint = |ui: &mut uniterm_client::ConnectionInput, text: &str| {
            ui.connecting(&spec.target, text)
        };
        let mut probe = handshake::WorkspaceProbe::new(&mut stdin, workspace);
        let result = handshake::ready(
            &mut stdout,
            prompt.as_mut().map(|tty| handshake::Handoff {
                tty: &mut tty.master,
                input: libc::STDIN_FILENO,
                winch: winch.unwrap_or(-1),
                prompt_timeout: REMOTE_PROMPT_TIMEOUT,
                paint: &mut paint,
            }),
            REMOTE_HANDSHAKE_TIMEOUT,
            Some(&mut probe),
        );
        diagnostics.end_native_prompts();
        if winch.is_some() {
            // Handshake input belongs exclusively to this transition.
            // SAFETY: fd 0 is the terminal owned by AttachTerminal.
            unsafe {
                libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
            }
        }
        if let Err(error) = result {
            if winch.is_some() {
                // SAFETY: the private SSH session leader remains unreaped.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
            }
            stop_failed_bridge(&mut child);
            let diagnostic = diagnostics.finish();
            if diagnostic.is_empty()
                || matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::TimedOut
                )
            {
                return Err(error);
            }
            return Err(io::Error::new(
                error.kind(),
                format!("{diagnostic} ({error})"),
            ));
        }
        let pending = std::mem::take(&mut probe.pending);
        Ok(Self {
            identity: probe.identity.or_else(|| workspace.map(str::to_owned)),
            pending,
            process: SshProcess {
                child: Some(child),
                prompt,
                diagnostics: Some(diagnostics),
            },
            stdin,
            stdout,
        })
    }
}

// One name per bridge for the life of this process. A remote-to-remote
// switch holds two bridges at once, and a finished bridge's name must never
// come back while anything could still refer to it.
static TRANSPORT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn private_transport_dir() -> io::Result<PathBuf> {
    let base = Path::new("/tmp");
    // An earlier process with this pid can have left names behind. Skip them
    // without reusing or removing anything this process did not create.
    for _ in 0..100 {
        let sequence = TRANSPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("uniterm-ssh-{}-{sequence}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a private SSH transport directory",
    ))
}

/// The pathname of one single-use listener. Exactly one owner removes it,
/// exactly once: the bridge worker, as soon as the name has served its purpose.
struct ProxySocket(PathBuf);

impl Drop for ProxySocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        if let Some(directory) = self.0.parent() {
            let _ = std::fs::remove_dir(directory);
        }
    }
}

pub(super) struct LocalBridge {
    /// Where the one client connects. The worker owns and removes the name;
    /// this copy is never used to unlink or to wake the worker.
    socket: PathBuf,
    /// One explicit byte wakes either the listener or the active forwarding loop.
    wake: UnixStream,
    // Keep SSH stdout open while reaping it. An ordinary detach may still
    // have a final server frame in flight when the local client disconnects.
    _output_until_exit: UnixStream,
    process: SshProcess,
    thread: Option<JoinHandle<io::Result<()>>>,
}

/// Wait for the single client or for the owner to give up, whichever comes
/// first. Blocks in the kernel with no timeout.
fn accept_or_stop(listener: &UnixListener, wake: &UnixStream) -> io::Result<Option<UnixStream>> {
    use std::os::fd::AsRawFd as _;
    listener.set_nonblocking(true)?;
    loop {
        let mut fds = [listener.as_raw_fd(), wake.as_raw_fd()].map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        // SAFETY: both descriptors are borrowed for the duration of the call.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        // Stopping wins over a client that connected at the same moment.
        if fds[1].revents != 0 {
            return Ok(None);
        }
        match listener.accept() {
            Ok((stream, _)) => {
                return Ok(Some(stream));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
}

impl LocalBridge {
    fn start(spec: SshSpec, workspace: Option<&str>, winch: Option<RawFd>) -> io::Result<Self> {
        validate_target(&spec.target).map_err(io::Error::other)?;
        if let Some(name) = workspace {
            uniterm_proto::validate_workspace_name(name).map_err(io::Error::other)?;
        }
        // Authentication and the Workspace probe can be cancelled, can fail
        // and can take minutes at a prompt. Nothing exists in /tmp until
        // they have succeeded.
        Self::from_prepared(PreparedSshBridge::start(&spec, workspace, winch)?)
    }

    fn from_prepared(prepared: PreparedSshBridge) -> io::Result<Self> {
        let PreparedSshBridge {
            identity,
            pending,
            process,
            stdin,
            stdout,
        } = prepared;
        let output_until_exit = stdout.get_ref().try_clone()?;
        let (wake, woken) = UnixStream::pair()?;
        // Until the worker takes it, an early return removes the name here.
        let name = ProxySocket(private_transport_dir()?.join("bridge.sock"));
        let listener = UnixListener::bind(&name.0)?;
        std::fs::set_permissions(&name.0, std::fs::Permissions::from_mode(0o600))?;
        let socket = name.0.clone();
        let thread = thread::spawn(move || {
            let accepted = accept_or_stop(&listener, &woken);
            // The listener takes one connection in its life. Close it and
            // remove its name now, in every outcome, so the directory does
            // not outlive the handshake however this process later ends.
            drop(listener);
            drop(name);
            let Some(stream) = accepted? else {
                return Ok(());
            };
            forward_prepared(stream, identity, &pending, stdin, stdout, &woken)
        });
        Ok(Self {
            socket,
            process,
            wake,
            _output_until_exit: output_until_exit,
            thread: Some(thread),
        })
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    fn finish(mut self, switched: bool) -> io::Result<()> {
        self.stop_and_join(switched)
    }

    fn stop_and_join(&mut self, cancel: bool) -> io::Result<()> {
        let Some(worker) = self.thread.take() else {
            return Ok(());
        };
        // Signal readiness even when a fork temporarily holds a duplicate of
        // the write end. This socket stays registered after accept, including
        // while either destination is applying backpressure.
        let _ = io::Write::write_all(&mut self.wake, &[1]);
        let result = worker
            .join()
            .map_err(|_| io::Error::other("SSH bridge worker panicked"))?;
        let process = self.process.finish(cancel);
        process.and(result)
    }
}

impl Drop for LocalBridge {
    fn drop(&mut self) {
        let _ = self.stop_and_join(true);
    }
}

/// Start the client's stream exactly where the probe left the Workspace's:
/// the identity the probe consumed, then anything that followed its reply,
/// then live bridge output.
fn forward_prepared(
    stream: UnixStream,
    identity: Option<String>,
    pending: &[u8],
    stdin: UnixStream,
    stdout: io::BufReader<UnixStream>,
    stop: &UnixStream,
) -> io::Result<()> {
    let mut prefix = identity.map_or_else(Vec::new, |name| {
        uniterm_proto::encode_frame(&uniterm_proto::ServerMessage::SessionRenamed { name })
    });
    prefix.extend_from_slice(pending);
    prefix.extend_from_slice(stdout.buffer());
    ignore_local_disconnect(forward::run(
        stream,
        stdin,
        stdout.into_inner(),
        &prefix,
        stop,
    ))
}

fn ignore_local_disconnect(result: io::Result<()>) -> io::Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(error) if disconnected(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn disconnected(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

fn stop_failed_bridge(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::process::Stdio;
    use std::time::Duration;

    #[test]
    fn remote_targets_cannot_be_parsed_as_ssh_options() {
        assert!(validate_target("host").is_ok());
        assert!(validate_target("user@example.com").is_ok());
        assert!(validate_target("-oProxyCommand=bad").is_err());
        assert!(validate_target("host\ncommand").is_err());
    }

    #[test]
    fn data_connection_does_not_depend_on_ssh_multiplexing() {
        let command = SshSpec::new("workbox").command();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(args.first().map(String::as_str), Some("-T"));
        assert!(args.iter().all(|arg| !arg.starts_with("ControlMaster=")));
        assert!(args.iter().all(|arg| !arg.starts_with("ControlPath=")));
        assert!(args.iter().all(|arg| !arg.starts_with("ControlPersist=")));
    }

    #[test]
    fn remote_bridge_identifies_the_resolved_workspace_before_forwarding_frames() {
        use std::io::Write;
        let (bridge, mut server) = UnixStream::pair().unwrap();
        server.write_all(b"server frame").unwrap();
        server.shutdown(std::net::Shutdown::Write).unwrap();
        let mut output = Vec::new();
        bridge_stream(bridge, io::empty(), &mut output, "selected").unwrap();
        let identity = uniterm_proto::encode_frame(&uniterm_proto::ServerMessage::SessionRenamed {
            name: "selected".into(),
        });
        assert_eq!(output, [REMOTE_READY, &identity, b"server frame"].concat());
    }

    #[test]
    fn remote_bridge_publishes_its_search_path_before_attach_bytes() {
        let (mut server, mut bridge) = UnixStream::pair().unwrap();
        publish_remote_environment(
            &mut bridge,
            vec!["/home/test/.local/bin".into(), "/usr/bin".into()],
        )
        .unwrap();

        let mut bytes = [0; 4096];
        let read = server.read(&mut bytes).unwrap();
        let mut decoder =
            uniterm_proto::FrameDecoder::with_max_frame(uniterm_proto::MAX_CLIENT_FRAME);
        decoder.push(&bytes[..read]);
        assert!(matches!(
            decoder
                .decode::<uniterm_proto::ClientMessage>()
                .unwrap()
                .unwrap(),
            uniterm_proto::ClientMessage::RemoteEnvironment { search_path }
                if search_path == ["/home/test/.local/bin", "/usr/bin"]
        ));
    }

    #[test]
    fn expected_local_disconnects_do_not_mask_a_successful_ssh_exit() {
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
        ] {
            ignore_local_disconnect(Err(io::Error::new(kind, "closed"))).unwrap();
        }
        assert!(ignore_local_disconnect(Err(io::Error::other("disk failure"))).is_err());
    }

    #[test]
    fn prepared_bridge_keeps_stdio_open_for_the_full_client_session() {
        let (stdin, child_input) = UnixStream::pair().unwrap();
        let (stdout, child_output) = UnixStream::pair().unwrap();
        let mut child = Command::new("cat")
            .stdin(OwnedFd::from(child_input))
            .stdout(OwnedFd::from(child_output))
            .spawn()
            .unwrap();
        let (mut client, proxy) = UnixStream::pair().unwrap();
        let (_wake, stop) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || forward::run(proxy, stdin, stdout, &[], &stop));

        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        client.write_all(b"attach frame").unwrap();
        let mut echoed = [0; 12];
        client.read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"attach frame");
        client.shutdown(std::net::Shutdown::Write).unwrap();
        worker.join().unwrap().unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn server_chunks_are_flushed_before_the_bridge_reaches_eof() {
        struct OneChunkThenWait {
            chunk: Option<Vec<u8>>,
            release: std::sync::mpsc::Receiver<()>,
        }

        impl io::Read for OneChunkThenWait {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if let Some(chunk) = self.chunk.take() {
                    buf[..chunk.len()].copy_from_slice(&chunk);
                    return Ok(chunk.len());
                }
                self.release.recv().unwrap();
                Ok(0)
            }
        }

        struct FlushProbe {
            pending: Vec<u8>,
            flushed: std::sync::mpsc::Sender<Vec<u8>>,
        }

        impl io::Write for FlushProbe {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.pending.extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                self.flushed.send(self.pending.clone()).map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "flush probe disconnected")
                })
            }
        }

        let frame = b"small binary frame without a newline".to_vec();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (flush_tx, flush_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn({
            let frame = frame.clone();
            move || {
                let mut reader = OneChunkThenWait {
                    chunk: Some(frame),
                    release: release_rx,
                };
                let mut writer = FlushProbe {
                    pending: Vec::new(),
                    flushed: flush_tx,
                };
                copy_with_flush(&mut reader, &mut writer)
            }
        });

        assert_eq!(
            flush_rx
                .recv_timeout(std::time::Duration::from_millis(200))
                .unwrap(),
            frame
        );
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap(), frame.len() as u64);
    }

    #[test]
    fn detach_reaps_an_unresponsive_bridge_despite_inherited_streams() {
        let child = Command::new("sleep").arg("5").spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        let (stdin, _inherited_stdin) = UnixStream::pair().unwrap();
        let (stdout, _inherited_stdout) = UnixStream::pair().unwrap();
        let (client, _connected) = UnixStream::pair().unwrap();
        let (wake, stop) = UnixStream::pair().unwrap();
        let output_until_exit = stdout.try_clone().unwrap();
        let worker = thread::spawn(move || forward::run(client, stdin, stdout, &[], &stop));
        let bridge = LocalBridge {
            socket: PathBuf::from("/nonexistent/bridge.sock"),
            wake,
            _output_until_exit: output_until_exit,
            process: SshProcess {
                child: Some(child),
                prompt: None,
                diagnostics: None,
            },
            thread: Some(worker),
        };
        let start = std::time::Instant::now();
        bridge.finish(false).unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        // SAFETY: inspect only this test's child; status is unused.
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    // A prepared bridge without SSH: the test plays the remote Workspace on
    // the returned streams.
    fn prepared_fixture(identity: &str) -> (LocalBridge, UnixStream, UnixStream) {
        let (stdin, workspace_input) = UnixStream::pair().unwrap();
        let (workspace_output, stdout) = UnixStream::pair().unwrap();
        let bridge = LocalBridge::from_prepared(PreparedSshBridge {
            identity: Some(identity.into()),
            pending: Vec::new(),
            process: SshProcess {
                child: None,
                prompt: None,
                diagnostics: None,
            },
            stdin,
            stdout: io::BufReader::new(stdout),
        })
        .unwrap();
        (bridge, workspace_output, workspace_input)
    }

    fn wait_removed(path: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while path.exists() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!path.exists(), "{} was not removed", path.display());
    }

    fn connect_and_identify(bridge: &LocalBridge) -> (UnixStream, String) {
        use uniterm_proto::ServerMessage;
        let mut client = UnixStream::connect(bridge.socket()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut header = [0; 4];
        client.read_exact(&mut header).unwrap();
        let mut body = vec![0; u32::from_be_bytes(header) as usize];
        client.read_exact(&mut body).unwrap();
        let mut decoder = uniterm_proto::FrameDecoder::new();
        decoder.push(&header);
        decoder.push(&body);
        match decoder.decode::<ServerMessage>().unwrap().unwrap() {
            ServerMessage::SessionRenamed { name } => (client, name),
            other => panic!("unexpected first frame {other:?}"),
        }
    }

    #[test]
    fn concurrent_bridges_keep_distinct_single_use_names_through_accept_and_stop() {
        use std::os::unix::fs::MetadataExt as _;
        // A remote-to-remote switch: the serving bridge and its candidate.
        let (first, mut first_output, _first_input) = prepared_fixture("one");
        let (second, mut second_output, _second_input) = prepared_fixture("two");
        let (first_path, second_path) = (first.socket().to_owned(), second.socket().to_owned());
        let directory = |path: &Path| path.parent().unwrap().to_owned();
        assert_ne!(first_path, second_path);
        for path in [&first_path, &second_path] {
            assert_eq!(
                std::fs::metadata(directory(path)).unwrap().mode() & 0o777,
                0o700
            );
            assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o600);
        }

        // The accepted bridge gives up its name at once. Its neighbour's
        // name and listener are untouched.
        let (mut client, name) = connect_and_identify(&first);
        assert_eq!(name, "one");
        wait_removed(&directory(&first_path));
        assert!(second_path.exists());
        first_output.write_all(b"from one").unwrap();
        let mut bytes = [0; 8];
        client.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"from one");
        // A second client cannot reach an accepted bridge by its old name.
        assert!(UnixStream::connect(&first_path).is_err());

        // Stopping the old generation wakes nothing but itself: the
        // candidate is still waiting for, and then serves, its own client.
        let start = std::time::Instant::now();
        first.finish(true).unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(second_path.exists());
        let (mut client, name) = connect_and_identify(&second);
        assert_eq!(name, "two");
        wait_removed(&directory(&second_path));
        second_output.write_all(b"from two").unwrap();
        client.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"from two");

        // A bridge whose client never arrives is woken without a connection
        // and removes its own name exactly once.
        let (abandoned, _output, _input) = prepared_fixture("three");
        let abandoned_path = abandoned.socket().to_owned();
        assert!(abandoned_path.exists());
        let start = std::time::Instant::now();
        abandoned.finish(true).unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(!directory(&abandoned_path).exists());

        // Released names are never handed out again in this process, and a
        // name left by an earlier process with this pid is skipped, not taken.
        let stale = PathBuf::from(format!(
            "/tmp/uniterm-ssh-{}-{}",
            std::process::id(),
            TRANSPORT_SEQUENCE.load(Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&stale)
            .unwrap();
        let (later, _output, _input) = prepared_fixture("four");
        let later_path = later.socket().to_owned();
        for earlier in [&first_path, &second_path, &abandoned_path] {
            assert_ne!(&later_path, earlier);
        }
        assert_ne!(directory(&later_path), stale);
        drop(later);
        assert!(!directory(&later_path).exists());
        assert!(stale.is_dir());
        std::fs::remove_dir(&stale).unwrap();
        second.finish(true).unwrap();
    }

    #[test]
    fn ordinary_detach_allows_ssh_to_flush_its_final_output_and_exit_cleanly() {
        let (stdin, child_input) = UnixStream::pair().unwrap();
        let (stdout, child_output) = UnixStream::pair().unwrap();
        let child = Command::new("sh")
            .args(["-c", "printf ready; cat >/dev/null; printf last-frame"])
            .stdin(OwnedFd::from(child_input))
            .stdout(OwnedFd::from(child_output))
            .spawn()
            .unwrap();
        let bridge = LocalBridge::from_prepared(PreparedSshBridge {
            identity: None,
            pending: Vec::new(),
            process: SshProcess {
                child: Some(child),
                prompt: None,
                diagnostics: None,
            },
            stdin,
            stdout: io::BufReader::new(stdout),
        })
        .unwrap();
        let mut client = UnixStream::connect(bridge.socket()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut ready = [0; 5];
        client.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");
        drop(client);
        bridge.finish(false).unwrap();
    }

    #[test]
    fn final_remote_output_survives_a_closed_ssh_input() {
        let (mut client, proxy) = UnixStream::pair().unwrap();
        let (stdin, remote_input) = UnixStream::pair().unwrap();
        let (stdout, mut output) = UnixStream::pair().unwrap();
        let (_wake, stop) = UnixStream::pair().unwrap();
        drop(remote_input);
        client.write_all(b"pending input").unwrap();
        output.write_all(b"final output").unwrap();
        drop(output);
        let worker = thread::spawn(move || forward::run(proxy, stdin, stdout, &[], &stop));
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut received = Vec::new();
        client.read_to_end(&mut received).unwrap();
        assert_eq!(received, b"final output");
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn stopping_a_bridge_wakes_the_listener_despite_an_inherited_wake_socket() {
        let (bridge, _output, _input) = prepared_fixture("waiting");
        let _inherited = bridge.wake.try_clone().unwrap();
        let (done, closed) = std::sync::mpsc::channel();
        thread::spawn(move || done.send(bridge.finish(true)).unwrap());
        closed
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
    }

    #[test]
    fn stopping_a_bridge_with_a_full_client_socket_keeps_detach_bounded() {
        let (mut client, mut proxy) = UnixStream::pair().unwrap();
        proxy.set_nonblocking(true).unwrap();
        let fill = [0; 16384];
        loop {
            match proxy.write(&fill) {
                Ok(n) => assert!(n > 0),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("could not fill client socket: {e}"),
            }
        }
        let (stdin, mut remote_input) = UnixStream::pair().unwrap();
        let (stdout, _remote_output) = UnixStream::pair().unwrap();
        let (mut wake, stop) = UnixStream::pair().unwrap();
        let (done, closed) = std::sync::mpsc::channel();
        thread::spawn(move || {
            done.send(forward::run(proxy, stdin, stdout, b"pending", &stop))
                .unwrap();
        });
        // The upload succeeds while the handshake prefix cannot be written.
        client.write_all(b"x").unwrap();
        remote_input
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut uploaded = [0];
        remote_input.read_exact(&mut uploaded).unwrap();
        assert_eq!(&uploaded, b"x");
        wake.write_all(&[1]).unwrap();
        closed
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
    }

    #[test]
    fn failed_bridge_process_is_returned_to_the_caller() {
        let child = Command::new("sh")
            .arg("-c")
            .arg("exit 23")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let error = SshProcess {
            child: Some(child),
            prompt: None,
            diagnostics: None,
        }
        .finish(false)
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert!(error.to_string().contains("exit status: 23"));
    }

    #[test]
    fn bytes_behind_the_probe_reply_reach_the_client_before_later_bridge_output() {
        use uniterm_proto::{encode_frame, ClientMessage, ServerMessage};
        let (mut workspace, ssh_stdout) = UnixStream::pair().unwrap();
        let (ssh_stdin, mut workspace_input) = UnixStream::pair().unwrap();
        // One write carries the marker, both probe replies and the start of the
        // attach stream: more than a probe frame, ending inside a frame.
        let render = encode_frame(&ServerMessage::SessionRenamed {
            name: "n".repeat(3000),
        });
        let (early, late) = render.split_at(render.len() - 7);
        let mut burst = REMOTE_READY.to_vec();
        burst.extend(encode_frame(&ServerMessage::SessionRenamed {
            name: "build".into(),
        }));
        burst.extend(encode_frame(&ServerMessage::Info {
            windows: 1,
            panes: 1,
        }));
        burst.extend(early);
        workspace.write_all(&burst).unwrap();

        let mut stdin = ssh_stdin.try_clone().unwrap();
        let mut stdout = io::BufReader::new(ssh_stdout);
        let mut probe = handshake::WorkspaceProbe::new(&mut stdin, Some("build"));
        handshake::ready(&mut stdout, None, Duration::from_secs(3), Some(&mut probe)).unwrap();
        let (identity, pending) = (probe.identity.take(), std::mem::take(&mut probe.pending));
        assert_eq!(identity.as_deref(), Some("build"));
        assert!(!pending.is_empty() && early.ends_with(&pending));

        let (bridge_end, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let (_wake, stop) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            forward_prepared(bridge_end, identity, &pending, ssh_stdin, stdout, &stop)
        });
        // The rest of the interrupted frame arrives only now, then more output.
        workspace.write_all(late).unwrap();
        workspace.write_all(b"live output").unwrap();
        let mut expected = encode_frame(&ServerMessage::SessionRenamed {
            name: "build".into(),
        });
        expected.extend(&render);
        expected.extend(b"live output");
        let mut received = vec![0; expected.len()];
        client.read_exact(&mut received).unwrap();
        assert_eq!(received, expected);
        // The same connection still carries client input to the Workspace,
        // after the probe request the handshake sent on its behalf.
        client.write_all(b"typed").unwrap();
        let probe_request = encode_frame(&ClientMessage::ListInfo);
        let mut upstream = vec![0; probe_request.len() + 5];
        workspace_input
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        workspace_input.read_exact(&mut upstream).unwrap();
        assert_eq!(upstream, [probe_request.as_slice(), b"typed"].concat());
        drop(workspace);
        drop(client);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn handshake_discards_bounded_login_noise() {
        for banner in ["banner\nnotice\n", "no newline"] {
            let (mut peer, input) = UnixStream::pair().unwrap();
            peer.write_all(banner.as_bytes()).unwrap();
            peer.write_all(REMOTE_READY).unwrap();
            peer.write_all(b"frame").unwrap();
            let mut reader = io::BufReader::with_capacity(3, input);
            handshake::ready(&mut reader, None, Duration::from_secs(1), None).unwrap();
            let mut frame = [0; 5];
            reader.read_exact(&mut frame).unwrap();
            assert_eq!(&frame, b"frame");
        }
    }

    #[test]
    fn handshake_bounds_unterminated_preamble_and_silent_peer() {
        let (mut peer, input) = UnixStream::pair().unwrap();
        let mut reader = io::BufReader::new(input);
        assert_eq!(
            handshake::ready(&mut reader, None, Duration::from_millis(10), None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        let _writer = thread::spawn(move || peer.write_all(&vec![b'x'; MAX_REMOTE_PREAMBLE + 1]));
        assert_eq!(
            handshake::ready(&mut reader, None, Duration::from_secs(2), None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn bridge_rejects_protocol_mismatch() {
        let args = vec!["--protocol".to_string(), "999".to_string()];
        let error = parse_protocol(&args, false).unwrap_err();
        assert!(error.contains("wire protocol mismatch"));
    }
}
