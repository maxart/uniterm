//! Shared-canvas geometry stays valid across attaches, resizes, and detaches.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn attach(path: &std::path::Path, cols: u16, rows: u16) -> UnixStream {
    let mut stream = UnixStream::connect(path).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols,
            rows,
        }))
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream
}

fn frames(stream: &mut UnixStream, for_time: Duration) -> Vec<String> {
    let mut decoder = FrameDecoder::new();
    let mut output = Vec::new();
    let mut buf = [0; 32 * 1024];
    let deadline = Instant::now() + for_time;
    while Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                decoder.push(&buf[..n]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    if let ServerMessage::RenderOps(ops) = message {
                        output.push(String::from_utf8_lossy(&ops).into_owned());
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
    output
}

fn wait_for(path: &std::path::Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("server socket did not appear");
}

#[test]
fn smallest_attached_client_defines_shared_canvas() {
    isolate_state();
    let dir = std::env::temp_dir().join(format!("uniterm-multi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{}.sock", unique_workspace_name()));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 3"], 80, 24).unwrap();
        server.run(&mut poll).unwrap();
    });
    wait_for(&socket);

    let mut large = attach(&socket, 80, 24);
    let initial = frames(&mut large, Duration::from_millis(250));
    assert!(initial.iter().any(|frame| frame.contains("\x1b[24;1H")));

    let mut small = attach(&socket, 40, 12);
    let shrunk = frames(&mut large, Duration::from_millis(300));
    assert!(shrunk
        .iter()
        .any(|frame| { frame.contains("\x1b[r\x1b[2J") && frame.contains("\x1b[12;1H") }));

    small
        .write_all(&encode_frame(&ClientMessage::Resize { cols: 60, rows: 20 }))
        .unwrap();
    let grown = frames(&mut large, Duration::from_millis(300));
    assert!(grown
        .iter()
        .any(|frame| { frame.contains("\x1b[r\x1b[2J") && frame.contains("\x1b[20;1H") }));

    small
        .write_all(&encode_frame(&ClientMessage::Detach))
        .unwrap();
    let restored = frames(&mut large, Duration::from_millis(300));
    assert!(restored
        .iter()
        .any(|frame| { frame.contains("\x1b[r\x1b[2J") && frame.contains("\x1b[24;1H") }));

    drop(large);
    server.join().unwrap();
}
