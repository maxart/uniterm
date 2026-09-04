//! SSH transport for the existing Uniterm client protocol.
//!
//! The remote server remains Unix-socket-only. A short-lived command on the
//! remote host copies that socket byte stream over SSH stdio, while a private
//! local proxy socket lets the unchanged thin client keep its mio hot path.

use std::io;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
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

pub(super) fn cmd_remote(args: &[String]) -> i32 {
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

    match run_remote(target, workspace, pane.map(|pane| (pane, role))) {
        Ok(()) => 0,
        Err(error) => {
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
    match bridge_remote_stdio(&socket) {
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

fn run_remote(
    target: &str,
    workspace: Option<&str>,
    pane: Option<(uniterm_core::PaneId, uniterm_proto::PaneAttachRole)>,
) -> io::Result<()> {
    let spec = SshSpec::new(target);
    let mut workspace = workspace.map(str::to_string);
    loop {
        // Establish and validate one long-lived SSH data connection while the
        // terminal is cooked. Selecting another remote Workspace returns here
        // so its bridge can be replaced by name without treating a remote
        // socket path as a client-machine path.
        let bridge = LocalBridge::start(spec.clone(), workspace.as_deref())?;
        let result = if let Some((pane, role)) = pane {
            uniterm_client::pane_attach(bridge.socket(), pane, role).map(|()| None)
        } else {
            uniterm_client::attach_with_options(
                bridge.socket(),
                uniterm_client::AttachOptions {
                    open_workspaces: false,
                    remote: true,
                },
            )
            .map(Some)
        };
        let bridge_result = bridge.finish();
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
            None | Some(uniterm_client::AttachOutcome::Exit) => return Ok(()),
            Some(uniterm_client::AttachOutcome::RemoteWorkspace(next)) => {
                workspace = Some(next);
            }
            Some(uniterm_client::AttachOutcome::ReviveWorkspace(_)) => {
                return Err(io::Error::other(
                    "the remote client returned a local Workspace handoff",
                ));
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
fn bridge_remote_stdio(socket: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    publish_remote_environment(&mut stream, remote_search_path())?;
    bridge_stream(stream, io::stdin(), io::stdout())
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

fn bridge_stream<R, W>(stream: UnixStream, mut input: R, mut output: W) -> io::Result<()>
where
    R: io::Read + Send + 'static,
    W: io::Write,
{
    output.write_all(REMOTE_READY)?;
    output.flush()?;
    let mut socket_to_stdout = stream.try_clone()?;
    let mut stdin_to_socket = stream;
    let _upload = thread::spawn(move || {
        let result = io::copy(&mut input, &mut stdin_to_socket);
        let _ = stdin_to_socket.shutdown(std::net::Shutdown::Write);
        result
    });
    let download = copy_with_flush(&mut socket_to_stdout, &mut output);
    // Do not join the stdin copier here. If the server closes first, stdin is
    // still the live SSH pipe and joining would deadlock the remote helper.
    // Returning ends this dedicated bridge process and closes both directions.
    download.map(|_| ())
}

/// Forward each available server chunk through SSH immediately. `Stdout` is
/// block-buffered under `ssh -T`, and Uniterm's binary frames contain no
/// newline, so flushing only at EOF leaves small frames invisible until a
/// later resize or click happens to fill the buffer.
fn copy_with_flush(reader: &mut impl io::Read, writer: &mut impl io::Write) -> io::Result<u64> {
    let mut copied = 0;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buf)?;
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
    child: Child,
    stdin: ChildStdin,
    stdout: io::BufReader<ChildStdout>,
}

impl PreparedSshBridge {
    /// Start the actual data connection and wait for its protocol handshake.
    /// This runs before the attach client enters raw mode, so authentication,
    /// host verification, missing binaries, and protocol mismatches are all
    /// reported on the ordinary terminal.
    fn start(spec: &SshSpec, workspace: Option<&str>) -> io::Result<Self> {
        let mut child = spec
            .command()
            .arg(remote_command("remote-bridge", workspace))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                io::Error::new(error.kind(), format!("could not start SSH bridge: {error}"))
            })?;
        let Some(stdin) = child.stdin.take() else {
            stop_failed_bridge(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SSH bridge stdin is missing",
            ));
        };
        let Some(stdout) = child.stdout.take() else {
            stop_failed_bridge(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SSH bridge stdout is missing",
            ));
        };
        let mut stdout = io::BufReader::new(stdout);
        let watchdog = HandshakeWatchdog::arm(child.id(), REMOTE_HANDSHAKE_TIMEOUT);
        if let Err(error) = read_remote_ready(&mut stdout) {
            let timed_out = watchdog.disarm();
            stop_failed_bridge(&mut child);
            if timed_out {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "remote login shell did not complete the Uniterm handshake within {} s; check the remote shell's startup files for anything interactive",
                        REMOTE_HANDSHAKE_TIMEOUT.as_secs()
                    ),
                ));
            }
            return Err(error);
        }
        watchdog.disarm();
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }
}

/// Kills the SSH bridge if the handshake has not arrived by the deadline. The
/// thread blocks on a channel rather than polling, and exits as soon as the
/// handshake completes or fails on its own.
struct HandshakeWatchdog {
    cancel: Option<std::sync::mpsc::Sender<()>>,
    timed_out: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HandshakeWatchdog {
    fn arm(child_pid: u32, timeout: std::time::Duration) -> Self {
        let (cancel, cancelled) = std::sync::mpsc::channel::<()>();
        let timed_out = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&timed_out);
        let thread = thread::spawn(move || {
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = cancelled.recv_timeout(timeout)
            {
                flag.store(true, Ordering::SeqCst);
                // The child is unreaped until `stop_failed_bridge`, so its
                // pid cannot have been recycled.
                // SAFETY: kill takes a pid and a signal number and touches no
                // memory owned by this process.
                unsafe {
                    libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
                }
            }
        });
        Self {
            cancel: Some(cancel),
            timed_out,
            thread: Some(thread),
        }
    }

    /// Stop the watchdog and report whether it already fired.
    fn disarm(mut self) -> bool {
        drop(self.cancel.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.timed_out.load(Ordering::SeqCst)
    }
}

fn private_transport_dir() -> io::Result<PathBuf> {
    let base = Path::new("/tmp");
    for attempt in 0..100 {
        let path = base.join(format!("uniterm-ssh-{}-{attempt}", std::process::id()));
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

fn remote_command(subcommand: &str, workspace: Option<&str>) -> String {
    let workspace = workspace.map(|name| format!(" {name}")).unwrap_or_default();
    let bridge = format!(
        "if command -v uniterm >/dev/null 2>&1; then exec uniterm {subcommand} --protocol {WIRE_PROTOCOL_VERSION}{workspace}; \
         elif command -v ut >/dev/null 2>&1; then exec ut {subcommand} --protocol {WIRE_PROTOCOL_VERSION}{workspace}; \
         else printf '%s\\n' 'Uniterm is not installed on the remote host (expected uniterm or ut on PATH)' >&2; exit 127; fi"
    );
    // OpenSSH commands run through a non-interactive shell, which commonly
    // omits nvm, Volta, Cargo, Homebrew, and other user CLI paths. Resolve the
    // bridge from one interactive login shell so a newly detached Workspace
    // inherits the same PATH as the user's terminal. Startup noise is already
    // bounded and discarded by `read_remote_ready` before raw mode begins.
    format!(
        "TERM=${{TERM:-xterm-256color}}; export TERM; exec \"${{SHELL:-/bin/sh}}\" -lic {}",
        shell_single_quote(&bridge)
    )
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct LocalBridge {
    directory: PathBuf,
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl LocalBridge {
    fn start(spec: SshSpec, workspace: Option<&str>) -> io::Result<Self> {
        let directory = private_transport_dir()?;
        let socket = directory.join("bridge.sock");
        let listener = match UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = std::fs::remove_dir(&directory);
                return Err(error);
            }
        };
        if let Err(error) =
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        {
            drop(listener);
            let _ = std::fs::remove_file(&socket);
            let _ = std::fs::remove_dir(&directory);
            return Err(error);
        }
        let prepared = match PreparedSshBridge::start(&spec, workspace) {
            Ok(prepared) => prepared,
            Err(error) => {
                drop(listener);
                let _ = std::fs::remove_file(&socket);
                let _ = std::fs::remove_dir(&directory);
                return Err(error);
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let (stream, _) = listener.accept()?;
            if thread_stop.load(Ordering::Acquire) {
                let mut child = prepared.child;
                stop_failed_bridge(&mut child);
                return Ok(());
            }
            bridge_connection(stream, prepared)
        });
        Ok(Self {
            directory,
            socket,
            stop,
            thread: Some(thread),
        })
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    fn finish(mut self) -> io::Result<()> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        // Wake a bridge which was prepared successfully but whose local client
        // failed before connecting to the private socket.
        let _ = UnixStream::connect(&self.socket);
        let result = match self.thread.take() {
            Some(thread) => thread
                .join()
                .map_err(|_| io::Error::other("SSH bridge worker panicked"))?,
            None => Ok(()),
        };
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir(&self.directory);
        result
    }
}

impl Drop for LocalBridge {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn bridge_connection(stream: UnixStream, prepared: PreparedSshBridge) -> io::Result<()> {
    let PreparedSshBridge {
        mut child,
        mut stdin,
        mut stdout,
    } = prepared;
    let mut client_to_child = stream.try_clone()?;
    let mut child_to_client = stream;
    let upload = thread::spawn(move || {
        let result = io::copy(&mut client_to_child, &mut stdin);
        drop(stdin);
        result
    });
    let download = io::copy(&mut stdout, &mut child_to_client);
    let _ = child_to_client.shutdown(std::net::Shutdown::Both);
    let upload = upload
        .join()
        .map_err(|_| io::Error::other("SSH bridge upload worker panicked"))?;
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!("SSH bridge exited with {status}"),
        ));
    }
    ignore_local_disconnect(upload)?;
    ignore_local_disconnect(download)?;
    Ok(())
}

fn ignore_local_disconnect(result: io::Result<u64>) -> io::Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::ConnectionReset
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn stop_failed_bridge(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn read_remote_ready(reader: &mut impl io::BufRead) -> io::Result<()> {
    let mut consumed = 0;
    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "SSH bridge closed before its protocol handshake",
            ));
        }
        consumed += read;
        if consumed > MAX_REMOTE_PREAMBLE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SSH login emitted too much text before the Uniterm handshake",
            ));
        }
        if line.ends_with(REMOTE_READY) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::time::Duration;

    #[test]
    fn remote_targets_cannot_be_parsed_as_ssh_options() {
        assert!(validate_target("host").is_ok());
        assert!(validate_target("user@example.com").is_ok());
        assert!(validate_target("-oProxyCommand=bad").is_err());
        assert!(validate_target("host\ncommand").is_err());
    }

    #[test]
    fn remote_command_checks_both_installed_binary_names() {
        let command = remote_command("remote-bridge", Some("work"));
        assert!(command.contains("${SHELL:-/bin/sh}"));
        assert!(command.contains("-lic"));
        assert!(command.contains("command -v uniterm"));
        assert!(command.contains("command -v ut"));
        assert!(command.contains(&format!(
            "remote-bridge --protocol {WIRE_PROTOCOL_VERSION} work"
        )));
    }

    #[test]
    fn remote_command_quotes_the_login_shell_payload_as_one_argument() {
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
        let command = remote_command("remote-bridge", Some("work"));
        assert!(command.ends_with("exit 127; fi'"));
        assert!(std::process::Command::new("sh")
            .args(["-n", "-c", &command])
            .status()
            .unwrap()
            .success());
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
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let prepared = PreparedSshBridge {
            stdin: child.stdin.take().unwrap(),
            stdout: io::BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        let (mut client, proxy) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || bridge_connection(proxy, prepared));

        client.write_all(b"attach frame").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut echoed = Vec::new();
        client.read_to_end(&mut echoed).unwrap();

        assert_eq!(echoed, b"attach frame");
        worker.join().unwrap().unwrap();
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
    fn failed_bridge_process_is_returned_to_the_caller() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 23")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let prepared = PreparedSshBridge {
            stdin: child.stdin.take().unwrap(),
            stdout: io::BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        let (client, proxy) = UnixStream::pair().unwrap();
        drop(client);

        let error = bridge_connection(proxy, prepared).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert!(error.to_string().contains("exit status: 23"));
    }

    #[test]
    fn handshake_discards_bounded_login_noise() {
        let input = b"banner\nnotice\nUNITERM-REMOTE/1\nframed bytes";
        let mut reader = io::BufReader::new(&input[..]);
        read_remote_ready(&mut reader).unwrap();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"framed bytes");
    }

    #[test]
    fn handshake_accepts_a_banner_without_a_final_newline() {
        let input = b"bannerUNITERM-REMOTE/1\nframed bytes";
        let mut reader = io::BufReader::new(&input[..]);
        read_remote_ready(&mut reader).unwrap();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"framed bytes");
    }

    #[test]
    fn handshake_watchdog_kills_a_silent_bridge_and_spares_a_prompt_one() {
        let mut silent = std::process::Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let watchdog = HandshakeWatchdog::arm(silent.id(), Duration::from_millis(150));
        let status = silent.wait().unwrap();
        assert!(!status.success(), "silent bridge was not killed");
        assert!(watchdog.disarm());

        let mut prompt = std::process::Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let watchdog = HandshakeWatchdog::arm(prompt.id(), Duration::from_secs(30));
        assert!(!watchdog.disarm());
        assert!(
            prompt.try_wait().unwrap().is_none(),
            "prompt bridge was killed"
        );
        let _ = prompt.kill();
        let _ = prompt.wait();
    }

    #[test]
    fn bridge_rejects_protocol_mismatch() {
        let args = vec!["--protocol".to_string(), "999".to_string()];
        let error = parse_protocol(&args, false).unwrap_err();
        assert!(error.contains("wire protocol mismatch"));
    }
}
