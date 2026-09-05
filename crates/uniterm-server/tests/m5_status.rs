//! M5 integration: the status line renders the session name.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::isolate_state;

#[test]
fn status_line_shows_session_name() {
    isolate_state();
    let dir = common::socket_root().join(format!("uniterm-m5-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // The socket stem is the session name shown in the status line.
    // A short fixed name on purpose: the Projects sidebar truncates the
    // Workspace button to 14 cells at 80 columns, so a generated name would
    // never render in full. `isolate_state` keeps the durable state private.
    let workspace = "statusdemo";
    let sock = dir.join(format!("{workspace}.sock"));
    let sock_srv = sock.clone();

    let server = thread::spawn(move || {
        let (mut s, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", "sleep 2"], 80, 24).unwrap();
        let _ = s.run(&mut poll);
    });

    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let mut c = UnixStream::connect(&sock).unwrap();
    c.write_all(&encode_frame(&ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 80,
        rows: 24,
    }))
    .unwrap();
    c.flush().unwrap();
    c.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();

    let mut dec = FrameDecoder::new();
    let mut text = String::new();
    let mut buf = [0u8; 16384];
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !text.contains(workspace) {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                dec.push(&buf[..n]);
                while let Ok(Some(message)) = dec.decode::<ServerMessage>() {
                    if let ServerMessage::RenderOps(ops) = message {
                        text.push_str(&String::from_utf8_lossy(&ops));
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }

    let _ = c.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = c.flush();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        text.contains(workspace),
        "expected the session name in the status line, got: {text:?}"
    );
}
