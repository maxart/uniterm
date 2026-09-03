//! M3 integration: a split command produces a second pane with a divider.
//!
//! Uses a plain std `UnixStream` client (no tty). Sends `Attach` then a
//! `Split(LeftRight)` command and checks the resulting frame draws a vertical
//! divider (proving two panes are laid out). Then kills both panes so the
//! server shuts down and the thread joins.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, Command, FrameDecoder, ServerMessage, SplitAxis};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

const VBAR: &[u8] = "\u{2502}".as_bytes(); // the vertical divider glyph

#[test]
fn split_creates_a_divider() {
    isolate_state();
    let dir = std::env::temp_dir().join(format!("uniterm-m3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join(format!("{}.sock", unique_workspace_name()));
    let sock_srv = sock.clone();

    let server = thread::spawn(move || {
        // Interactive shell (no self-exit); the test kills the panes to stop it.
        let (mut s, mut poll) = Server::bind(&sock_srv, "/bin/sh", &[], 80, 24).unwrap();
        let _ = s.run(&mut poll);
    });

    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::Split(
            SplitAxis::LeftRight,
        ))))
        .unwrap();
    stream.flush().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();

    let mut dec = FrameDecoder::new();
    let mut saw_divider = false;
    let mut buf = [0u8; 16384];
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !saw_divider {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                dec.push(&buf[..n]);
                while let Ok(Some(msg)) = dec.decode::<ServerMessage>() {
                    if let ServerMessage::RenderOps(ops) = msg {
                        if ops.windows(VBAR.len()).any(|w| w == VBAR) {
                            saw_divider = true;
                        }
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }

    // Stop the server: kill both panes so its window empties and it exits.
    let _ = stream.write_all(&encode_frame(&ClientMessage::Command(Command::KillPane)));
    let _ = stream.write_all(&encode_frame(&ClientMessage::Command(Command::KillPane)));
    let _ = stream.flush();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        saw_divider,
        "expected a vertical divider in the render ops after a left/right split"
    );
}
