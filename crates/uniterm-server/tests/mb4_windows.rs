//! MB4 integration: window (tab) rename + kill over a real socket.
//!
//! Renaming the active window must show its centered `1:name` label in the status line; killing
//! a window must close all its panes and remove it, and killing the last
//! window must stop the server.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, Command, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_sock(tag: &str) -> std::path::PathBuf {
    let dir = common::socket_root().join(format!("uniterm-it-{}-{tag}", std::process::id()));
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

fn read_until(
    stream: &mut UnixStream,
    dec: &mut FrameDecoder,
    secs: u64,
    pred: impl Fn(&str) -> bool,
) -> String {
    let mut got = String::new();
    let mut buf = [0u8; 16384];
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline && !pred(&got) {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                dec.push(&buf[..n]);
                while let Ok(Some(msg)) = dec.decode::<ServerMessage>() {
                    if let ServerMessage::RenderOps(ops) = msg {
                        got.push_str(&String::from_utf8_lossy(&ops));
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    got
}

#[test]
fn rename_shows_in_status_and_kill_window_closes_it() {
    isolate_state();
    let sock = temp_sock("mb4");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut s, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", "sleep 30"], 80, 24).unwrap();
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
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut dec = FrameDecoder::new();

    // Rename window 0 and check the status line segment.
    stream
        .write_all(&encode_frame(&ClientMessage::RenameWindow {
            name: "build".into(),
        }))
        .unwrap();
    let got = read_until(&mut stream, &mut dec, 3, |s| s.contains(" 1:build "));
    assert!(
        got.contains(" 1:build "),
        "renamed window missing from status line"
    );

    // Later windows appear in order. Closing the third activates the Tab that
    // was immediately to its left, matching browser-style Tab behavior.
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::NewWindow)))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::RenameWindow {
            name: "tests".into(),
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::NewWindow)))
        .unwrap();
    let got = read_until(&mut stream, &mut dec, 3, |s| s.contains(" 3 "));
    assert!(got.contains(" 3 "), "third window missing");
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::KillWindow)))
        .unwrap();
    let theme = uniterm_core::Theme::dark();
    let active_second = format!(
        "\x1b[1;{};{}m  2:tests  ",
        theme.status_active_bg.sgr_bg(),
        theme.status_active_fg.sgr_fg()
    );
    let got = read_until(&mut stream, &mut dec, 3, |s| {
        s.rsplit("\x1b[r\x1b[2J")
            .next()
            .map(|last| last.contains(&active_second) && !last.contains(" 3 "))
            .unwrap_or(false)
    });
    let last = got.rsplit("\x1b[r\x1b[2J").next().unwrap_or("");
    assert!(
        last.contains(&active_second) && !last.contains(" 3 "),
        "closing the third Tab did not activate its left neighbor: {last:?}"
    );

    // Closing the second falls back to the first, then closing the final window
    // ends the session.
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::KillWindow)))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::KillWindow)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !server.is_finished() {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        server.is_finished(),
        "server kept running after the last window was killed"
    );
    let _ = server.join();
}
