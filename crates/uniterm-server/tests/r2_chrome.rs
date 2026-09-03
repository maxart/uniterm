//! R2 regression: the status line must survive pane operations. After a stress
//! sequence of split/kill/zoom/new-window, a full frame must still contain the
//! session name (the status line).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, Command, FrameDecoder, ServerMessage, SplitAxis};
use uniterm_server::Server;

mod common;

use common::isolate_state;

#[test]
fn status_line_survives_pane_ops() {
    isolate_state();
    let dir = std::env::temp_dir().join(format!("uniterm-r2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // A short fixed name on purpose: the Projects sidebar truncates the
    // Workspace button to 14 cells at 80 columns, so a generated name would
    // never render in full. `isolate_state` keeps the durable state private.
    let workspace = "chromedemo";
    let sock = dir.join(format!("{workspace}.sock"));
    let sock_srv = sock.clone();

    let server = thread::spawn(move || {
        let (mut s, mut poll) = Server::bind(&sock_srv, "/bin/sh", &[], 80, 24).unwrap();
        let _ = s.run(&mut poll);
    });
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let mut c = UnixStream::connect(&sock).unwrap();
    c.set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    c.write_all(&encode_frame(&ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 80,
        rows: 24,
    }))
    .unwrap();

    // A stress sequence of structural operations.
    let ops = [
        Command::Split(SplitAxis::LeftRight),
        Command::Split(SplitAxis::TopBottom),
        Command::KillPane,
        Command::ZoomToggle,
        Command::ZoomToggle,
        Command::NewWindow,
        Command::SelectWindow(0),
        Command::Focus(uniterm_proto::FocusDir::Left),
    ];
    for op in ops {
        c.write_all(&encode_frame(&ClientMessage::Command(op)))
            .unwrap();
    }
    c.flush().unwrap();

    // Drain frames for a moment; assert the session name (status line) is present
    // in the render stream after the operations.
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
        "status line (session name) missing after pane ops"
    );
}
