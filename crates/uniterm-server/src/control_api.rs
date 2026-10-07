//! Private NDJSON control transport owned by the agent runtime.

use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_channel::Sender;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::{mpsc, oneshot};
use uniterm_proto::{ControlFrame, ControlRequest, ControlResponse};

const MAX_CONTROL_LINE: usize = uniterm_proto::CONTROL_MAX_FRAME_BYTES as usize;
const MAX_CONTROL_FRAME: usize = uniterm_proto::CONTROL_MAX_FRAME_BYTES as usize;
const MAX_CONNECTION_QUEUE: usize = uniterm_proto::CONTROL_MAX_QUEUED_FRAMES as usize;

pub(crate) enum Inbound {
    Connected {
        generation: u64,
        connection: u64,
        output: mpsc::Sender<Vec<u8>>,
        read_only: bool,
    },
    Request {
        generation: u64,
        connection: u64,
        request: ControlRequest,
    },
    Disconnected {
        generation: u64,
        connection: u64,
    },
}

pub(crate) struct Listener {
    path: PathBuf,
    identity: (u64, u64),
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Listener {
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
        listener.set_nonblocking(true)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (shutdown, shutdown_rx) = oneshot::channel();
        let accept_path = path.clone();
        let thread = match std::thread::Builder::new()
            .name("uniterm-control".into())
            .spawn(move || {
                runtime.block_on(accept_loop(
                    listener,
                    shutdown_rx,
                    inbound,
                    generation,
                    read_only,
                ));
                // A congested dispatcher may still own a lifecycle send.
                // Listener teardown must not wait for that dispatcher, which
                // can itself be the caller dropping this Listener.
                runtime.shutdown_background();
            }) {
            Ok(thread) => thread,
            Err(error) => {
                remove_if_unchanged(&path, identity);
                return Err(error);
            }
        };
        Ok(Self {
            path: accept_path,
            identity,
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        remove_if_unchanged(&self.path, self.identity);
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
    listener: UnixListener,
    mut shutdown: oneshot::Receiver<()>,
    inbound: Sender<Inbound>,
    generation: u64,
    read_only: bool,
) {
    static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);
    let Ok(listener) = tokio::net::UnixListener::from_std(listener) else {
        return;
    };
    let mut connections = tokio::task::JoinSet::new();
    let mut accept_failed = false;
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        accept_failed = false;
                        let connection = NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed).max(1);
                        connections.spawn(connection_loop(stream, generation, connection, inbound.clone(), read_only));
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
) {
    let (output, mut responses) = mpsc::channel(MAX_CONNECTION_QUEUE);
    if !lifecycle(
        &inbound,
        Inbound::Connected {
            generation,
            connection,
            output,
            read_only,
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
                        inbound.try_send(Inbound::Request { generation, connection, request }).err().map(|_| {
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
    use uniterm_proto::{ControlCommand, ControlResponse, ControlResult, CONTROL_API_VERSION};

    #[tokio::test]
    async fn full_intake_and_invalid_requests_return_errors_without_dispatch() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (inbound, received) = crossbeam_channel::bounded(1);
        let worker = tokio::spawn(connection_loop(server, 1, 7, inbound, false));
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
                request: ControlRequest { id: 1, .. },
                ..
            }
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
        let worker = tokio::spawn(connection_loop(server, 1, 9, inbound, false));
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
                request: ControlRequest { id: 23, .. },
                ..
            }
        ));
        assert!(received.is_empty());
        drop(output);
        worker.await.unwrap();
    }
}
