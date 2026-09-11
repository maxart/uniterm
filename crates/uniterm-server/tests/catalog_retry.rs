//! Catalog failures must remain retryable and must not erase crash recovery.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};

mod common;
use common::{isolate_state, unique_workspace_name};

fn runtime_barrier(client: &mut UnixStream) {
    client
        .write_all(&encode_frame(&ClientMessage::WorkspaceList))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let mut buffer = [0; 8192];
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match client.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => decoder.push(&buffer[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(error) => panic!("read: {error}"),
        }
        while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            if matches!(message, ServerMessage::Workspaces { .. }) {
                return;
            }
        }
    }
    panic!("runtime did not finish queued persistence work");
}

fn connect(socket: &Path) -> UnixStream {
    common::wait_for_socket(socket);
    let client = UnixStream::connect(socket).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    client
}

fn start(socket: PathBuf) -> thread::JoinHandle<()> {
    thread::spawn(move || uniterm_server::run_server(&socket, "/bin/sh", &[]).unwrap())
}

fn socket_path(name: &str) -> PathBuf {
    let dir = common::socket_root().join(format!("ut-retry-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{name}.sock"))
}

#[test]
fn failed_catalog_save_retries_the_same_definition_at_clean_stop() {
    let state = isolate_state().join("uniterm");
    let name = unique_workspace_name();
    let socket = socket_path(&name);
    let path = state
        .join(uniterm_proto::WORKSPACE_CATALOG_DIR)
        .join(format!(
            "{}.jsonl",
            uniterm_proto::workspace_catalog_key(&name)
        ));
    // A directory at the exact file path deterministically rejects writes,
    // including when the test process has elevated filesystem permissions.
    std::fs::create_dir_all(&path).unwrap();
    let server = start(socket.clone());
    let mut client = connect(&socket);
    runtime_barrier(&mut client);
    assert!(uniterm_server::workspace_catalog::load(&name).is_none());
    assert!(uniterm_server::persist::exists(&name));
    std::fs::remove_dir(&path).unwrap();
    // A failed append can also leave an unterminated record. The retried
    // definition must remain readable after that partial prefix.
    std::fs::write(&path, b"{\"partial\":").unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();

    let definition = uniterm_server::workspace_catalog::load(&name).expect("retried definition");
    assert_eq!(definition.tab_count(), 1);
    assert!(!uniterm_server::persist::exists(&name));

    let server = start(socket.clone());
    let mut client = connect(&socket);
    runtime_barrier(&mut client);
    client
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();
    assert_eq!(
        uniterm_server::workspace_catalog::load(&name),
        Some(definition)
    );
}

#[test]
fn persistent_catalog_failure_retains_the_crash_checkpoint() {
    let state = isolate_state().join("uniterm");
    let name = unique_workspace_name();
    let socket = socket_path(&name);
    let path = state
        .join(uniterm_proto::WORKSPACE_CATALOG_DIR)
        .join(format!(
            "{}.jsonl",
            uniterm_proto::workspace_catalog_key(&name)
        ));
    std::fs::create_dir_all(&path).unwrap();
    let server = start(socket.clone());
    let mut client = connect(&socket);
    runtime_barrier(&mut client);
    client
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();
    assert!(uniterm_server::persist::exists(&name));
    assert!(uniterm_server::workspace_catalog::load(&name).is_none());
}
