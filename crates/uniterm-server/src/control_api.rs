//! Private NDJSON control transport owned by the agent runtime.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use uniterm_proto::{ControlFrame, ControlRequest};

const MAX_CONTROL_LINE: usize = uniterm_proto::CONTROL_MAX_FRAME_BYTES as usize;
const MAX_CONTROL_FRAME: usize = uniterm_proto::CONTROL_MAX_FRAME_BYTES as usize;
const MAX_CONNECTION_QUEUE: usize = uniterm_proto::CONTROL_MAX_QUEUED_FRAMES as usize;
const MAX_CONTROL_CONNECTIONS: usize = uniterm_proto::CONTROL_MAX_CONNECTIONS as usize;

pub(crate) enum Inbound {
    Connected {
        generation: u64,
        connection: u64,
        output: Sender<Vec<u8>>,
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
    shutdown: Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Listener {
    pub(crate) fn bind(
        path: PathBuf,
        inbound: Sender<Inbound>,
        generation: u64,
    ) -> std::io::Result<Self> {
        bind_private(&path)?;
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let metadata = std::fs::symlink_metadata(&path)?;
        let identity = (metadata.dev(), metadata.ino());
        let (shutdown, shutdown_rx) = bounded(1);
        let accept_path = path.clone();
        let thread = match std::thread::Builder::new()
            .name("uniterm-control".into())
            .spawn(move || accept_loop(listener, shutdown_rx, inbound, generation))
        {
            Ok(thread) => thread,
            Err(error) => {
                remove_if_unchanged(&path, identity);
                return Err(error);
            }
        };
        Ok(Self {
            path: accept_path,
            identity,
            shutdown,
            thread: Some(thread),
        })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.shutdown.try_send(());
        let _ = UnixStream::connect(&self.path);
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
        if UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "control socket is live",
            ));
        }
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "control path is not a socket",
            ));
        }
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn accept_loop(
    listener: UnixListener,
    shutdown: Receiver<()>,
    inbound: Sender<Inbound>,
    generation: u64,
) {
    static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);
    let active = Arc::new(AtomicUsize::new(0));
    while let Ok((stream, _)) = listener.accept() {
        if shutdown.try_recv().is_ok() {
            break;
        }
        if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONTROL_CONNECTIONS {
            active.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let connection = NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed).max(1);
        let writer = match stream.try_clone() {
            Ok(writer) => writer,
            Err(_) => {
                active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
        };
        let (output, output_rx) = bounded(MAX_CONNECTION_QUEUE);
        if inbound
            .send(Inbound::Connected {
                generation,
                connection,
                output,
            })
            .is_err()
        {
            active.fetch_sub(1, Ordering::AcqRel);
            break;
        }
        std::thread::spawn(move || write_loop(writer, output_rx));
        let inbound = inbound.clone();
        let active = active.clone();
        std::thread::spawn(move || read_loop(stream, generation, connection, inbound, active));
    }
}

fn read_loop(
    stream: UnixStream,
    generation: u64,
    connection: u64,
    inbound: Sender<Inbound>,
    active: Arc<AtomicUsize>,
) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        let Ok(read) = read_bounded_line(&mut reader, &mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        let Ok(request) = serde_json::from_slice::<ControlRequest>(&line) else {
            break;
        };
        if inbound
            .try_send(Inbound::Request {
                generation,
                connection,
                request,
            })
            .is_err()
        {
            let _ = reader.get_ref().shutdown(Shutdown::Both);
            break;
        }
    }
    let _ = inbound.send(Inbound::Disconnected {
        generation,
        connection,
    });
    active.fetch_sub(1, Ordering::AcqRel);
}

fn read_bounded_line(
    reader: &mut BufReader<UnixStream>,
    line: &mut Vec<u8>,
) -> std::io::Result<usize> {
    loop {
        let available = reader.fill_buf()?;
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

fn write_loop(mut stream: UnixStream, output: Receiver<Vec<u8>>) {
    for line in output {
        if stream.write_all(&line).is_err() {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

pub(crate) fn send_bounded(output: &Sender<Vec<u8>>, frame: ControlFrame) -> bool {
    let Ok(mut line) = serde_json::to_vec(&frame) else {
        return false;
    };
    if line.len() >= MAX_CONTROL_FRAME {
        return false;
    }
    line.push(b'\n');
    !matches!(
        output.try_send(line),
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniterm_proto::{ControlCommand, ControlResponse, ControlResult, CONTROL_API_VERSION};

    #[test]
    fn full_request_intake_disconnects_the_flooding_reader() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let (inbound, received) = bounded(1);
        inbound
            .send(Inbound::Disconnected {
                generation: 0,
                connection: 0,
            })
            .unwrap();
        let active = Arc::new(AtomicUsize::new(1));
        let worker_active = Arc::clone(&active);
        let worker = std::thread::spawn(move || {
            read_loop(server, 1, 7, inbound, worker_active);
        });
        let request = ControlRequest {
            version: CONTROL_API_VERSION,
            id: 1,
            workspace: "bounded".into(),
            command: ControlCommand::Capabilities,
        };
        serde_json::to_writer(&mut client, &request).unwrap();
        client.write_all(b"\n").unwrap();
        let mut byte = [0u8; 1];
        assert!(matches!(
            std::io::Read::read(&mut client, &mut byte),
            Ok(0) | Err(_)
        ));

        let _prefill = received.recv().unwrap();
        let disconnected = received.recv().unwrap();
        assert!(matches!(
            disconnected,
            Inbound::Disconnected {
                generation: 1,
                connection: 7
            }
        ));
        worker.join().unwrap();
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn lagging_connection_never_grows_past_its_bounded_queue() {
        let (output, _reader) = bounded(1);
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
        assert_eq!(output.len(), 1);
    }
}
