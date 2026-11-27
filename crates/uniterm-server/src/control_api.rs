//! Private NDJSON control transport owned by the agent runtime.

use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_channel::Sender;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::{mpsc, oneshot, watch};
use uniterm_proto::{
    ControlFrame, ControlRequest, ControlResponse, HandoffControlListeners, HandoffListener,
};

const MAX_CONTROL_LINE: usize = uniterm_proto::CONTROL_MAX_FRAME_BYTES as usize;
const MAX_CONTROL_FRAME: usize = uniterm_proto::CONTROL_MAX_FRAME_BYTES as usize;
const MAX_CONNECTION_QUEUE: usize = uniterm_proto::CONTROL_MAX_QUEUED_FRAMES as usize;
const SHUTDOWN_DRAIN_BOUND: std::time::Duration = std::time::Duration::from_secs(1);

pub(crate) enum Inbound {
    Connected {
        generation: u64,
        connection: u64,
        output: mpsc::Sender<Vec<u8>>,
        read_only: bool,
        inbox_only: bool,
        peer_pid: Option<i32>,
    },
    Request {
        generation: u64,
        connection: u64,
        request: Box<ControlRequest>,
    },
    Disconnected {
        generation: u64,
        connection: u64,
    },
}

pub(crate) struct Listener {
    path: PathBuf,
    identity: (u64, u64),
    owns_path: std::cell::Cell<bool>,
    activate: Option<oneshot::Sender<()>>,
    shutdown: Option<oneshot::Sender<()>>,
    freeze: mpsc::Sender<Freeze>,
    thread: Option<std::thread::JoinHandle<()>>,
}

// The sole acceptor acknowledges only between accepts and only when all
// connection tasks have finished. Dropping the lease (including on rollback
// or cancelled preparation) wakes it and resumes the same kernel backlog.
struct Freeze {
    reply: oneshot::Sender<std::io::Result<OwnedFd>>,
    resume: oneshot::Receiver<()>,
}

impl Listener {
    async fn freeze_if_idle(&self) -> std::io::Result<(oneshot::Sender<()>, HandoffListener)> {
        let (lease, resume) = oneshot::channel();
        let (reply, result) = oneshot::channel();
        self.freeze
            .send(Freeze { reply, resume })
            .await
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        let file = std::fs::File::from(result.await.map_err(std::io::Error::other)??);
        let socket_identity = socket_file_identity(&file)?;
        Ok((
            lease,
            HandoffListener {
                file: std::sync::Arc::new(file),
                identity: self.identity,
                socket_identity,
            },
        ))
    }

    pub(crate) fn bind(
        path: PathBuf,
        inbound: Sender<Inbound>,
        generation: u64,
    ) -> std::io::Result<Self> {
        Self::bind_scoped(path, inbound, generation, false)
    }

    pub(crate) fn bind_scoped(
        path: PathBuf,
        inbound: Sender<Inbound>,
        generation: u64,
        read_only: bool,
    ) -> std::io::Result<Self> {
        bind_private(&path)?;
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let metadata = std::fs::symlink_metadata(&path)?;
        let identity = (metadata.dev(), metadata.ino());
        Self::start(
            path, listener, identity, inbound, generation, read_only, false,
        )
    }

    /// Register the original listener without accepting connections or taking
    /// its pathname. Dropping a candidate before commit leaves the source intact.
    pub(crate) fn adopt(
        path: PathBuf,
        descriptor: HandoffListener,
        inbound: Sender<Inbound>,
        generation: u64,
        read_only: bool,
    ) -> std::io::Result<Self> {
        let listener = Self::check_descriptor(&path, &path, &descriptor)?;
        Self::start(
            path,
            listener,
            descriptor.identity,
            inbound,
            generation,
            read_only,
            true,
        )
    }

    pub(crate) fn activate(&mut self) {
        if let Some(permit) = self.activate.take() {
            self.owns_path.set(true);
            let _ = permit.send(());
        }
    }

    pub(crate) fn check_descriptor(
        path: &Path,
        bound_path: &Path,
        descriptor: &HandoffListener,
    ) -> std::io::Result<UnixListener> {
        let listener = UnixListener::from(descriptor.file.as_fd().try_clone_to_owned()?);
        let metadata = std::fs::symlink_metadata(path)?;
        // SAFETY: geteuid only reads the current effective user id.
        let uid = unsafe { libc::geteuid() };
        if !metadata.file_type().is_socket()
            || metadata.mode() & 0o777 != 0o600
            || metadata.uid() != uid
            || (metadata.dev(), metadata.ino()) != descriptor.identity
            || listener.local_addr()?.as_pathname() != Some(bound_path)
            || socket_file_identity(&descriptor.file)? != descriptor.socket_identity
            || nix::sys::socket::getsockopt(&listener, nix::sys::socket::sockopt::SockType)?
                != nix::sys::socket::SockType::Stream
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected Workspace control listener",
            ));
        }
        Ok(listener)
    }

    fn start(
        path: PathBuf,
        listener: UnixListener,
        identity: (u64, u64),
        inbound: Sender<Inbound>,
        generation: u64,
        read_only: bool,
        prepared: bool,
    ) -> std::io::Result<Self> {
        let inbox_only = path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == "inbox");
        listener.set_nonblocking(true)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        // Register with the kernel before acknowledging candidate readiness.
        let listener = {
            let _enter = runtime.enter();
            tokio::net::UnixListener::from_std(listener)?
        };
        let (permit, activation) = if prepared {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let (shutdown, shutdown_rx) = oneshot::channel();
        let (freeze, freeze_rx) = mpsc::channel(1);
        let accept_path = path.clone();
        let thread = match std::thread::Builder::new()
            .name("uniterm-control".into())
            .spawn(move || {
                if activation.is_some_and(|gate| gate.blocking_recv().is_err()) {
                    return;
                }
                runtime.block_on(accept_loop(
                    listener,
                    shutdown_rx,
                    freeze_rx,
                    inbound,
                    generation,
                    read_only,
                    inbox_only,
                ));
                // A congested dispatcher may still own a lifecycle send.
                // Listener teardown must not wait for that dispatcher, which
                // can itself be the caller dropping this Listener.
                runtime.shutdown_background();
            }) {
            Ok(thread) => thread,
            Err(error) => {
                if !prepared {
                    remove_if_unchanged(&path, identity);
                }
                return Err(error);
            }
        };
        Ok(Self {
            path: accept_path,
            identity,
            owns_path: std::cell::Cell::new(!prepared),
            activate: permit,
            shutdown: Some(shutdown),
            freeze,
            thread: Some(thread),
        })
    }
}

// The private peer receives a duplicate of this exact listening socket.
// Darwin exposes the socket inode through fstat but does not implement
// getsockopt(SO_ACCEPTCONN). XNU lazily assigns a Unix PCB inode, including
// zero on the first assignment after boot; sample again in that case.
pub(crate) fn socket_file_identity(file: &std::fs::File) -> std::io::Result<(u64, u64)> {
    let mut metadata = file.metadata()?;
    if metadata.ino() == 0 {
        metadata = file.metadata()?;
    }
    if !metadata.file_type().is_socket() || metadata.ino() == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unidentified control socket",
        ));
    }
    Ok((metadata.dev(), metadata.ino()))
}

/// Validate every scope before voting to commit. The kernel name and the
/// original filesystem identity must both still identify that endpoint.
pub(crate) fn check_handoff_listeners(
    control_path: &Path,
    descriptors: &HandoffControlListeners,
) -> std::io::Result<()> {
    let parent = control_path.parent().unwrap_or(Path::new("."));
    let name = control_path
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    Listener::check_descriptor(control_path, control_path, &descriptors.control)?;
    if let Some(manager) = &descriptors.manager {
        let path = parent.join("manager").join(name);
        Listener::check_descriptor(&path, &path, manager)?;
    }
    let path = parent.join("inbox").join(name);
    Listener::check_descriptor(&path, &path, &descriptors.inbox)?;
    Ok(())
}

/// A rollback releases all three socket scopes, including any acceptors
/// already paused when another scope reported a live connection.
#[derive(Default)]
pub(crate) struct HandoffAdmission {
    transaction: u64,
    state: AdmissionState,
}

#[derive(Default)]
enum AdmissionState {
    #[default]
    Serving,
    Prepared {
        _leases: Vec<oneshot::Sender<()>>,
        listeners: HandoffControlListeners,
    },
    Relinquished,
}

impl HandoffAdmission {
    /// Interrupt idle Inbox watches only for a fresh preparation that can own the acceptors.
    pub(crate) fn can_prepare(&self, transaction: u64) -> bool {
        transaction > self.transaction && matches!(self.state, AdmissionState::Serving)
    }

    pub(crate) fn cancel_watch_drain(&mut self, transaction: u64) {
        if self.can_prepare(transaction) {
            self.transaction = transaction;
        }
    }

    pub(crate) async fn change(
        &mut self,
        transaction: u64,
        pause: bool,
        listeners: [Option<&Listener>; 3],
        dispatched_work_pending: impl FnOnce() -> bool,
    ) -> Result<Option<HandoffControlListeners>, String> {
        if transaction == 0 || transaction < self.transaction {
            return Err("stale Workspace handoff transaction".into());
        }
        if matches!(self.state, AdmissionState::Relinquished) {
            return Err("Workspace control endpoints have already transferred".into());
        }
        if transaction == self.transaction {
            if !pause {
                for listener in listeners.into_iter().flatten() {
                    listener.owns_path.set(true);
                }
                self.state = AdmissionState::Serving;
                return Ok(None);
            }
            return match &self.state {
                AdmissionState::Prepared { listeners, .. } => Ok(Some(listeners.clone())),
                _ => Err("Workspace handoff transaction already ended".into()),
            };
        }
        if !pause || !matches!(self.state, AdmissionState::Serving) {
            return Err("Workspace handoff transaction does not own the acceptors".into());
        }
        self.transaction = transaction;
        let endpoints = listeners;
        let [Some(control), manager, Some(inbox)] = endpoints else {
            return Err("Workspace control endpoints are unavailable".into());
        };
        let mut leases = Vec::new();
        let mut freeze = async |listener: &Listener| -> Result<HandoffListener, String> {
            let (lease, descriptor) = listener.freeze_if_idle().await.map_err(|e| e.to_string())?;
            leases.push(lease);
            Ok(descriptor)
        };
        let listeners = HandoffControlListeners {
            control: freeze(control).await?,
            manager: match manager {
                Some(listener) => Some(freeze(listener).await?),
                None => None,
            },
            inbox: freeze(inbox).await?,
        };
        // All connection tasks have stopped admitting work. A request could
        // have reached the dispatcher just before its client disconnected.
        if dispatched_work_pending() {
            return Err("Workspace has pending control requests".into());
        }
        // A dying or late-retiring source must not unlink transferred sockets.
        // Explicit rollback gives cleanup ownership back before releasing leases.
        for listener in endpoints.into_iter().flatten() {
            listener.owns_path.set(false);
        }
        self.state = AdmissionState::Prepared {
            _leases: leases,
            listeners: listeners.clone(),
        };
        Ok(Some(listeners))
    }

    pub(crate) fn relinquish(
        &mut self,
        transaction: u64,
        listeners: [&mut Option<Listener>; 3],
    ) -> Result<(), String> {
        if transaction == self.transaction && matches!(self.state, AdmissionState::Relinquished) {
            return Ok(());
        }
        if transaction != self.transaction || !matches!(self.state, AdmissionState::Prepared { .. })
        {
            return Err("Workspace handoff has no prepared control endpoints".into());
        }
        // Shutdown each acceptor while its pause lease is still held. No
        // source task can race the replacement for a queued connection.
        for listener in listeners {
            drop(listener.take());
        }
        self.state = AdmissionState::Relinquished;
        Ok(())
    }

    /// A rename drops the preceding generation's acceptors. A delayed
    /// handoff message cannot consume the new generation's sockets.
    pub(crate) fn renamed(&mut self, listeners: [Option<&Listener>; 3]) {
        for listener in listeners.into_iter().flatten() {
            listener.owns_path.set(true);
        }
        self.state = AdmissionState::Serving;
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.activate.take();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if self.owns_path.get() {
            remove_if_unchanged(&self.path, self.identity);
        }
    }
}

fn remove_if_unchanged(path: &Path, identity: (u64, u64)) {
    let unchanged = std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_socket() && (metadata.dev(), metadata.ino()) == identity
    });
    if unchanged {
        let _ = std::fs::remove_file(path);
    }
}

fn bind_private(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    if path.exists() {
        // A connect to a regular file has different errno values on macOS
        // and Linux. Classify the path before probing socket liveness.
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "control path is not a socket",
            ));
        }
        match UnixStream::connect(path) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "control socket is live",
                ))
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) => {}
            // Exhaustion and access failures are not evidence of a stale
            // listener; never unlink another live socket on that basis.
            Err(error) => return Err(error),
        }
        std::fs::remove_file(path)?;
    }
    Ok(())
}

async fn accept_loop(
    listener: tokio::net::UnixListener,
    mut shutdown: oneshot::Receiver<()>,
    mut freeze: mpsc::Receiver<Freeze>,
    inbound: Sender<Inbound>,
    generation: u64,
    read_only: bool,
    inbox_only: bool,
) {
    static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);
    let mut connections = tokio::task::JoinSet::new();
    let (stopping, _) = watch::channel(());
    let mut accept_failed = false;
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            Some(request) = freeze.recv() => {
                while connections.try_join_next().is_some() {}
                if !connections.is_empty() {
                    let _ = request.reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "Workspace has an active control connection",
                    )));
                } else {
                    let fd = listener.as_fd().try_clone_to_owned();
                    let acquired = fd.is_ok();
                    if request.reply.send(fd).is_err() || !acquired { continue; }
                    tokio::select! {
                        _ = &mut shutdown => break,
                        _ = request.resume => {}
                    }
                }
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        accept_failed = false;
                        let connection = NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed).max(1);
                        connections.spawn(connection_loop(stream, generation, connection, inbound.clone(), read_only, inbox_only, stopping.subscribe()));
                    }
                    Err(error) => {
                        if !accept_failed {
                            eprintln!("uniterm: control accept failed (will retry): {error}");
                            accept_failed = true;
                        }
                        // Only an actual accept failure arms this backoff. Idle
                        // listeners have no timer, and exhaustion never kills one.
                        tokio::select! {
                            _ = &mut shutdown => break,
                            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
                        }
                    }
                }
            }
        }
    }
    drop(listener);
    drain_connections(&mut connections, &stopping).await;
}

// A final Pane close can queue its acknowledgement immediately before the
// dispatcher drops this listener. Let writers finish those queued frames,
// with one shutdown-only bound for all clients, including stalled readers.
async fn drain_connections(
    connections: &mut tokio::task::JoinSet<()>,
    stopping: &watch::Sender<()>,
) {
    let _ = stopping.send(());
    let _ = tokio::time::timeout(SHUTDOWN_DRAIN_BOUND, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    connections.shutdown().await;
}

// Lifecycle messages must reach the dispatcher even when request intake is
// full. Never block the socket reactor on that bounded crossbeam seam.
async fn lifecycle(inbound: &Sender<Inbound>, message: Inbound) -> bool {
    match inbound.try_send(message) {
        Ok(()) => true,
        Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
        Err(crossbeam_channel::TrySendError::Full(message)) => {
            let inbound = inbound.clone();
            tokio::task::spawn_blocking(move || inbound.send(message).is_ok())
                .await
                .unwrap_or(false)
        }
    }
}

async fn connection_loop(
    stream: tokio::net::UnixStream,
    generation: u64,
    connection: u64,
    inbound: Sender<Inbound>,
    read_only: bool,
    inbox_only: bool,
    mut stopping: watch::Receiver<()>,
) {
    let (output, mut responses) = mpsc::channel(MAX_CONNECTION_QUEUE);
    if !lifecycle(
        &inbound,
        Inbound::Connected {
            generation,
            connection,
            output,
            read_only,
            inbox_only,
            peer_pid: stream.peer_cred().ok().and_then(|c| c.pid()),
        },
    )
    .await
    {
        return;
    }
    // Splitting borrows one socket; no dup(2), descriptor per writer, or
    // operating-system thread per connection, including idle subscribers.
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = Vec::new();
    loop {
        tokio::select! {
            _ = stopping.changed() => {
                // Sender clones held by subscriptions must not prolong a
                // listener shutdown or admit new output after the final drain.
                responses.close();
                while let Some(response) = responses.recv().await {
                    if !write_line(&mut write, &response).await { break; }
                }
                // Teardown owns all connection removal. The dispatcher may
                // itself be dropping this listener with a full intake queue.
                return;
            }
            response = responses.recv() => {
                let Some(response) = response else { break; };
                if !write_line(&mut write, &response).await { break; }
            }
            read = read_bounded_line(&mut reader, &mut line) => {
                let read = match read {
                    Ok(read) => read,
                    Err(error) => {
                        let frame = ControlFrame::Response(ControlResponse::error(0, "invalid_request", error.to_string()));
                        if let Some(bytes) = encode(frame) { let _ = write_line(&mut write, &bytes).await; }
                        break;
                    }
                };
                if read == 0 { break; }
                let request = serde_json::from_slice::<ControlRequest>(&line);
                let response = match request {
                    Ok(request) => {
                        let id = request.id;
                        inbound.try_send(Inbound::Request { generation, connection, request: Box::new(request) }).err().map(|_| {
                            ControlResponse::error(id, "server_busy", "Request was not accepted; retry after pending requests complete")
                        })
                    }
                    Err(error) => {
                        let id = serde_json::from_slice::<serde_json::Value>(&line).ok()
                            .and_then(|value| value.get("id").and_then(serde_json::Value::as_u64)).unwrap_or(0);
                        Some(ControlResponse::error(id, "invalid_request", error.to_string()))
                    }
                };
                line.clear();
                if let Some(response) = response {
                    if let Some(bytes) = encode(ControlFrame::Response(response)) {
                        if !write_line(&mut write, &bytes).await { break; }
                    }
                }
            }
        }
    }
    lifecycle(
        &inbound,
        Inbound::Disconnected {
            generation,
            connection,
        },
    )
    .await;
}

async fn read_bounded_line<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    line: &mut Vec<u8>,
) -> std::io::Result<usize> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(line.len());
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > MAX_CONTROL_LINE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "control request exceeds the line limit",
            ));
        }
        line.extend_from_slice(&available[..take]);
        let complete = available[take - 1] == b'\n';
        reader.consume(take);
        if complete {
            return Ok(line.len());
        }
    }
}

async fn write_line<W: tokio::io::AsyncWrite + Unpin>(stream: &mut W, line: &[u8]) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(5), stream.write_all(line))
        .await
        .is_ok_and(|result| result.is_ok())
}

fn encode(frame: ControlFrame) -> Option<Vec<u8>> {
    let mut line = serde_json::to_vec(&frame).ok()?;
    if line.len() >= MAX_CONTROL_FRAME {
        let ControlFrame::Response(response) = frame else {
            return None;
        };
        line = serde_json::to_vec(&ControlFrame::Response(ControlResponse::error(
            response.id, "response_too_large", "Response exceeds the control frame budget; the request may already have completed. Inspect the target before retrying a mutation."
        ))).ok()?;
    }
    line.push(b'\n');
    Some(line)
}

pub(crate) fn send_bounded(output: &mpsc::Sender<Vec<u8>>, frame: ControlFrame) -> bool {
    encode(frame).is_some_and(|line| output.try_send(line).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt as _;
    use uniterm_proto::{ControlCommand, ControlResponse, ControlResult, CONTROL_API_VERSION};

    #[tokio::test]
    async fn shutdown_drains_queued_acknowledgement_with_live_sender_and_full_intake() {
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let (inbound, received) = crossbeam_channel::bounded(1);
        let (stopping, stop) = watch::channel(());
        let mut connections = tokio::task::JoinSet::new();
        connections.spawn(connection_loop(
            server,
            1,
            7,
            inbound.clone(),
            false,
            false,
            stop,
        ));
        let (connected, received) = tokio::task::spawn_blocking(move || {
            (
                received
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap(),
                received,
            )
        })
        .await
        .unwrap();
        let Inbound::Connected { output, .. } = connected else {
            panic!()
        };
        // Listener::drop may run on a dispatcher that cannot consume another
        // lifecycle event until teardown returns.
        assert!(inbound
            .try_send(Inbound::Disconnected {
                generation: 1,
                connection: 99
            })
            .is_ok());
        let bytes = encode(ControlFrame::Response(ControlResponse::ok(
            47,
            ControlResult::Mutation {
                resource: "pane".into(),
                id: Some(1),
                found: true,
                accepted: true,
            },
        )))
        .unwrap();
        output.try_send(bytes.clone()).unwrap();
        // Do not yield between queueing the response and shutdown. Keep the
        // producer alive, as a subscription worker can retain a sender clone.
        drain_connections(&mut connections, &stopping).await;
        assert!(output.is_closed());
        assert!(connections.is_empty());
        assert_eq!(received.len(), 1);
        let mut actual = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.read_to_end(&mut actual),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            actual, bytes,
            "the complete acknowledgement must precede EOF"
        );
    }

    #[tokio::test]
    async fn shutdown_drain_bounds_idle_and_nonreading_clients() {
        for nonreading in [false, true] {
            let (stopping, _) = watch::channel(());
            let mut connections = tokio::task::JoinSet::new();
            let mut clients = Vec::new();
            let expected: Vec<u8> = if nonreading {
                (b'a'..=b'd')
                    .flat_map(|byte| vec![byte; MAX_CONTROL_FRAME - 1])
                    .collect()
            } else {
                Vec::new()
            };
            // Four stalled writers must share one deadline, not consume one
            // second each. Retained senders exercise idle connections too.
            for connection in 0..4 {
                let (client, server) = tokio::net::UnixStream::pair().unwrap();
                let (inbound, received) = crossbeam_channel::bounded(1);
                connections.spawn(connection_loop(
                    server,
                    1,
                    connection,
                    inbound,
                    false,
                    false,
                    stopping.subscribe(),
                ));
                let connected = tokio::task::spawn_blocking(move || {
                    received
                        .recv_timeout(std::time::Duration::from_secs(2))
                        .unwrap()
                })
                .await
                .unwrap();
                let Inbound::Connected { output, .. } = connected else {
                    panic!()
                };
                for frame in expected.chunks(MAX_CONTROL_FRAME - 1) {
                    output.try_send(frame.to_vec()).unwrap();
                }
                clients.push((client, output));
            }
            tokio::time::timeout(
                SHUTDOWN_DRAIN_BOUND + std::time::Duration::from_secs(2),
                drain_connections(&mut connections, &stopping),
            )
            .await
            .expect("retained senders or stalled writers held listener shutdown");
            assert!(connections.is_empty());
            for (mut client, output) in clients {
                assert!(output.is_closed());
                let mut actual = Vec::new();
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    client.read_to_end(&mut actual),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(
                    expected.starts_with(&actual),
                    "shutdown replayed a partially written frame"
                );
            }
        }
    }

    #[tokio::test]
    async fn full_intake_and_invalid_requests_return_errors_without_dispatch() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (inbound, received) = crossbeam_channel::bounded(1);
        let (_stop, stopping) = watch::channel(());
        let worker = tokio::spawn(connection_loop(
            server, 1, 7, inbound, false, false, stopping,
        ));
        let mut client = BufReader::new(client);
        let request = ControlRequest {
            version: CONTROL_API_VERSION,
            id: 1,
            workspace: "bounded".into(),
            command: ControlCommand::Capabilities,
        };
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        client.get_mut().write_all(&bytes).await.unwrap();
        let mut line = String::new();
        client.read_line(&mut line).await.unwrap();
        let ControlFrame::Response(response) = serde_json::from_str(&line).unwrap() else {
            panic!()
        };
        assert_eq!(response.id, 1);
        assert_eq!(response.error.unwrap().code, "server_busy");
        let Inbound::Connected { output, .. } = received.try_recv().unwrap() else {
            panic!()
        };
        client
            .get_mut()
            .write_all(b"{\"id\":42,\"method\":\"unknown\"}\n")
            .await
            .unwrap();
        line.clear();
        client.read_line(&mut line).await.unwrap();
        let ControlFrame::Response(response) = serde_json::from_str(&line).unwrap() else {
            panic!()
        };
        assert_eq!(response.id, 42);
        assert_eq!(response.error.unwrap().code, "invalid_request");
        assert!(received.is_empty());
        // The same connection accepts the next valid request exactly once.
        client.get_mut().write_all(&bytes).await.unwrap();
        let received_request =
            tokio::task::spawn_blocking(move || (received.recv().unwrap(), received))
                .await
                .unwrap();
        assert!(matches!(
            received_request.0,
            Inbound::Request {
                request, ..
            } if request.id == 1
        ));
        drop(output);
        worker.await.unwrap();
        assert!(matches!(
            received_request.1.try_recv().unwrap(),
            Inbound::Disconnected { connection: 7, .. }
        ));
    }

    #[test]
    fn lagging_connection_never_grows_past_its_bounded_queue() {
        let (output, _reader) = mpsc::channel(1);
        let frame = ControlFrame::Response(ControlResponse::ok(
            1,
            ControlResult::Capabilities {
                server_build: None,
                protocol_version: 1,
                capabilities: Vec::new(),
                max_frame_bytes: uniterm_proto::CONTROL_MAX_FRAME_BYTES,
                max_connections: uniterm_proto::CONTROL_MAX_CONNECTIONS,
                max_queued_frames: uniterm_proto::CONTROL_MAX_QUEUED_FRAMES,
                max_queued_requests: uniterm_proto::CONTROL_MAX_QUEUED_REQUESTS,
            },
        ));
        assert!(send_bounded(&output, frame.clone()));
        assert!(!send_bounded(&output, frame));
        assert_eq!(output.capacity(), 0);
    }

    #[test]
    fn oversized_response_returns_a_correlated_error_instead_of_closing() {
        let frame = ControlFrame::Response(ControlResponse::ok(
            17,
            ControlResult::PaneOutput {
                pane: uniterm_proto::PaneId(1),
                found: true,
                text: "x".repeat(MAX_CONTROL_FRAME),
                truncated: false,
            },
        ));
        let bytes = encode(frame).unwrap();
        assert!(bytes.len() < MAX_CONTROL_FRAME);
        let ControlFrame::Response(response) = serde_json::from_slice(&bytes).unwrap() else {
            panic!()
        };
        assert_eq!(response.id, 17);
        assert_eq!(response.error.unwrap().code, "response_too_large");
    }

    #[tokio::test]
    async fn partial_request_survives_an_interleaved_response_without_replay() {
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let (inbound, received) = crossbeam_channel::bounded(4);
        let (_stop, stopping) = watch::channel(());
        let worker = tokio::spawn(connection_loop(
            server, 1, 9, inbound, false, false, stopping,
        ));
        let (connected, received) =
            tokio::task::spawn_blocking(move || (received.recv().unwrap(), received))
                .await
                .unwrap();
        let Inbound::Connected { output, .. } = connected else {
            panic!()
        };
        client
            .write_all(b"{\"version\":1,\"id\":23,")
            .await
            .unwrap();
        output.send(b"interleaved\n".to_vec()).await.unwrap();
        let mut client = BufReader::new(client);
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(line, "interleaved\n");
        client
            .get_mut()
            .write_all(b"\"workspace\":\"test\",\"method\":\"capabilities\"}\n")
            .await
            .unwrap();
        let (request, received) = tokio::task::spawn_blocking(move || {
            (
                received
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap(),
                received,
            )
        })
        .await
        .unwrap();
        assert!(matches!(
            request,
            Inbound::Request {
                request, ..
            } if request.id == 23
        ));
        assert!(received.is_empty());
        drop(output);
        worker.await.unwrap();
    }
}
