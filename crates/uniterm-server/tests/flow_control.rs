//! Large client input is delivered through writable readiness without loss.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

#[test]
fn large_input_crosses_nonblocking_pty_without_truncation() {
    isolate_state();
    let dir = common::socket_root().join(format!("uniterm-flow-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{}.sock", unique_workspace_name()));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/usr/bin/wc", &["-c"], 80, 24).unwrap();
        server.run(&mut poll).unwrap();
    });
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        }))
        .unwrap();
    let mut payload = Vec::with_capacity(1024 * 1024 + 1);
    for _ in 0..(512 * 1024) {
        payload.extend_from_slice(b"x\n");
    }
    payload.push(0x04);
    stream
        .write_all(&encode_frame(&ClientMessage::Input(payload)))
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    let mut decoder = FrameDecoder::new();
    let mut buf = [0; 64 * 1024];
    let mut rendered = String::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && !rendered.contains("1048576") {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                decoder.push(&buf[..n]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    if let ServerMessage::RenderOps(ops) = message {
                        rendered.push_str(&String::from_utf8_lossy(&ops));
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("client read failed: {error}"),
        }
    }
    assert!(
        rendered.contains("1048576"),
        "large PTY input was truncated: {:?}",
        rendered.chars().rev().take(300).collect::<String>()
    );
    server.join().unwrap();
}

/// A client that falls behind while a pane floods output and then exits is
/// still handed that pane's final frame once it catches up, and the Exited
/// notice when the Workspace stops.
#[test]
fn a_backpressured_client_still_receives_a_closed_panes_final_frame() {
    isolate_state();
    let dir = common::socket_root().join(format!("uniterm-flow-late-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{}.sock", unique_workspace_name()));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/usr/bin/wc", &["-c"], 200, 60).unwrap();
        server.run(&mut poll).unwrap();
    });
    common::wait_for_socket(&socket);

    // A tall, wide client whose every visible row changes on each PTY read:
    // that makes each frame large enough for a few dozen of them to exceed
    // even a Linux socket buffer, so this client is backpressured when the
    // pane exits (macOS buffers a few KiB and gets there with far less). A
    // second pane keeps the Workspace alive, so delivery happens on the
    // ordinary writable path once the client reads again.
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 200,
            rows: 60,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Command(
            uniterm_proto::Command::Split(uniterm_proto::SplitAxis::LeftRight),
        )))
        .unwrap();
    let lines_per_chunk = 5 * 1024;
    let chunks = 4;
    let mut total = 0usize;
    for chunk_index in 0..chunks {
        let mut chunk = Vec::with_capacity(lines_per_chunk * 100 + 1);
        for line in 0..lines_per_chunk {
            let glyph = b'a' + ((chunk_index * lines_per_chunk + line) % 26) as u8;
            chunk.extend(std::iter::repeat_n(glyph, 99));
            chunk.push(b'\n');
        }
        total += chunk.len();
        if chunk_index + 1 == chunks {
            chunk.push(0x04);
        }
        stream
            .write_all(&encode_frame(&ClientMessage::Input(chunk)))
            .unwrap();
    }
    // Read nothing while the echo floods the socket: the server collapses
    // what it cannot write into one deferred repaint, and the pane exits
    // before this client has caught up.
    thread::sleep(Duration::from_secs(6));
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();

    let mut decoder = FrameDecoder::new();
    let mut buf = [0; 64 * 1024];
    let mut rendered = String::new();
    let mut exited = false;
    let mut killed = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !exited {
        if !killed && rendered.contains(&total.to_string()) {
            stream
                .write_all(&encode_frame(&ClientMessage::KillServer))
                .unwrap();
            killed = true;
        }
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                decoder.push(&buf[..n]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    match message {
                        ServerMessage::RenderOps(ops) => {
                            rendered.push_str(&String::from_utf8_lossy(&ops));
                        }
                        ServerMessage::Exited => exited = true,
                        _ => {}
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("client read failed: {error}"),
        }
    }
    assert!(
        rendered.contains(&total.to_string()),
        "the closed pane's final frame was lost (expected {total}): {:?}",
        rendered.chars().rev().take(300).collect::<String>()
    );
    assert!(exited, "the Exited notice was lost");
    server.join().unwrap();
}
