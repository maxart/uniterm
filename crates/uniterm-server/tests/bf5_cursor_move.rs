//! BF5: a cursor-only change must still move the visible cursor.
//!
//! Typing a space over an already-blank cell (or a bare `\r`) changes no cell,
//! so the grid stays clean and the damage renderer rightly emits nothing - but
//! the cursor did move, and the client must be told or the visible cursor
//! sticks in place. The server compares the cursor position against what was
//! last broadcast and sends a cursor-only frame when only it changed.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_sock(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("uniterm-it-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{}.sock", unique_workspace_name()))
}

fn wait_for(path: &std::path::Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("server socket never appeared at {}", path.display());
}

/// True if `bytes` contains a CSI cursor-position sequence (`ESC [ r ; c H`).
fn contains_cup(bytes: &[u8]) -> bool {
    let mut i = 0;
    while let Some(off) = bytes[i..].windows(2).position(|w| w == b"\x1b[") {
        let mut j = i + off + 2;
        let mut saw_digit = false;
        while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
            saw_digit |= bytes[j].is_ascii_digit();
            j += 1;
        }
        if saw_digit && j < bytes.len() && bytes[j] == b'H' {
            return true;
        }
        i += off + 2;
    }
    false
}

#[test]
fn cursor_only_change_is_broadcast() {
    isolate_state();
    let sock = temp_sock("cursor-only");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        // "AB" damages two cells; after a pause, a bare `\r` moves the cursor
        // without touching any cell.
        let (mut s, mut poll) = Server::bind(
            &sock_srv,
            "/bin/sh",
            &["-c", "printf AB; sleep 0.6; printf '\\r'; sleep 2"],
            80,
            24,
        )
        .unwrap();
        let _ = s.run(&mut poll);
    });

    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        }))
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();

    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    let mut seen_ab = false;
    let mut moved_after_ab = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !moved_after_ab {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                dec.push(&buf[..n]);
                while let Ok(Some(msg)) = dec.decode::<ServerMessage>() {
                    let ServerMessage::RenderOps(ops) = msg else {
                        continue;
                    };
                    if !seen_ab {
                        // The frame that paints "AB" (attach baseline or the
                        // incremental diff); everything after it can only be
                        // the cursor-only move.
                        seen_ab = String::from_utf8_lossy(&ops).contains("AB");
                    } else if contains_cup(&ops) {
                        moved_after_ab = true;
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }

    let _ = server.join();
    assert!(seen_ab, "never saw the pane's 'AB' output");
    assert!(
        moved_after_ab,
        "the bare \\r produced no cursor-move frame: a cursor-only change was swallowed"
    );
}
