//! Client-owned overlays freeze their covered server frame until close.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn wait_for(path: &std::path::Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("server socket never appeared");
}

fn read_render(stream: &mut UnixStream, decoder: &mut FrameDecoder) -> Option<Vec<u8>> {
    let mut buffer = [0u8; 32_768];
    loop {
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::RenderOps(ops) = message {
                return Some(ops);
            }
        }
        match stream.read(&mut buffer) {
            Ok(0) => return None,
            Ok(size) => decoder.push(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return None;
            }
            Err(error) => panic!("render read failed: {error}"),
        }
    }
}

#[test]
fn pane_activity_is_quiet_under_an_overlay_and_reconciles_once_on_close() {
    isolate_state();
    let dir =
        std::env::temp_dir().join(format!("uniterm-overlay-quiescence-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{}.sock", unique_workspace_name()));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 100, 24).unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&socket);

    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 100,
            rows: 24,
        }))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    assert!(read_render(&mut stream, &mut decoder).is_some());

    stream
        .write_all(&encode_frame(&ClientMessage::OverlayVisible { on: true }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Input(
            b"printf 'HIDDEN-WHILE-COVERED\\n'\n".to_vec(),
        )))
        .unwrap();
    thread::sleep(Duration::from_millis(150));
    assert_eq!(read_render(&mut stream, &mut decoder), None);

    stream
        .write_all(&encode_frame(&ClientMessage::OverlayVisible { on: false }))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut reconciled = None;
    while Instant::now() < deadline {
        if let Some(frame) = read_render(&mut stream, &mut decoder) {
            reconciled = Some(frame);
            break;
        }
    }
    let reconciled = reconciled.expect("close repaint");
    let frame = String::from_utf8_lossy(&reconciled);
    assert!(frame.contains("HIDDEN-WHILE-COVERED"));

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    drop(stream);
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(dir);
}
