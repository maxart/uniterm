//! M4 integration: the ListInfo/KillServer session-management protocol that
//! `uniterm ls` and `uniterm kill` use. A non-attached client can query window
//! and pane counts and stop the server.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, Command, FrameDecoder, ServerMessage, SplitAxis};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn read_info(stream: &mut UnixStream, dec: &mut FrameDecoder) -> (u32, u32) {
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        // Drain any already-buffered frame first.
        if let Ok(Some(ServerMessage::Info { windows, panes })) = dec.decode::<ServerMessage>() {
            return (windows, panes);
        }
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => dec.push(&buf[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    panic!("no Info response");
}

#[test]
fn list_info_reflects_splits_and_windows_then_kill() {
    isolate_state();
    let dir = std::env::temp_dir().join(format!("uniterm-m4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join(format!("{}.sock", unique_workspace_name()));
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
    c.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut dec = FrameDecoder::new();

    // Fresh session: one window, one pane.
    c.write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    c.flush().unwrap();
    assert_eq!(read_info(&mut c, &mut dec), (1, 1));

    // After a split: still one window, now two panes.
    c.write_all(&encode_frame(&ClientMessage::Command(Command::Split(
        SplitAxis::LeftRight,
    ))))
    .unwrap();
    c.write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    c.flush().unwrap();
    assert_eq!(read_info(&mut c, &mut dec), (1, 2));

    // After a new window: two windows, three panes.
    c.write_all(&encode_frame(&ClientMessage::Command(Command::NewWindow)))
        .unwrap();
    c.write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    c.flush().unwrap();
    assert_eq!(read_info(&mut c, &mut dec), (2, 3));

    // Kill stops the server; the thread joins.
    c.write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    c.flush().unwrap();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(&dir);
}
