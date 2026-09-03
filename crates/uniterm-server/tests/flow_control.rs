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
    let dir = std::env::temp_dir().join(format!("uniterm-flow-{}", std::process::id()));
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
