//! S1 integration: click-drag text selection is always on.
//!
//! Dragging across pane text must yank it to the clipboard (an OSC 52 write
//! in the render ops) and return the pane to the live screen.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_core::Config;
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, MouseKind, ServerMessage};
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
fn drag_selects_text_and_yanks_to_clipboard() {
    isolate_state();
    let sock = temp_sock("selection");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut s, mut poll) = Server::bind(
            &sock_srv,
            "/bin/sh",
            &["-c", "printf HELLOWORLD; sleep 5"],
            80,
            24,
        )
        .unwrap();
        s.set_config(Config {
            sidebar: false,
            ..Config::default()
        });
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
    let pre = read_until(&mut stream, &mut dec, 3, |s| s.contains("HELLOWORLD"));
    assert!(pre.contains("HELLOWORLD"), "pane text never arrived");

    // The top bar owns row 1, so press on the 'H' at screen cell (1,2), drag
    // to the second 'O', and release. "HELLO" is yanked as OSC 52.
    for (x, kind) in [
        (1, MouseKind::Click),
        (3, MouseKind::Drag),
        (5, MouseKind::Drag),
        (5, MouseKind::Release),
    ] {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse { x, y: 2, kind }))
            .unwrap();
    }
    // Wait for both the yank AND the post-release repaint (a full frame with
    // no copy-mode indicator).
    let got = read_until(&mut stream, &mut dec, 3, |s| {
        s.contains("]52;c;")
            && s.rsplit("\x1b[r\x1b[2J")
                .next()
                .map(|last| !last.is_empty() && !last.contains("[COPY"))
                .unwrap_or(false)
    });
    assert!(
        got.contains("]52;c;SEVMTE8="),
        "expected OSC 52 yank of HELLO, got: {got:?}"
    );
    let last = got.rsplit("\x1b[r\x1b[2J").next().unwrap_or("");
    assert!(
        !last.contains("[COPY"),
        "pane still frozen in copy-mode after release"
    );

    let _ = server.join();
}
