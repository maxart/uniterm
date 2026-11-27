//! BF4 integration: mouse passthrough to an app that asked for tracking.
//!
//! The pane app enables SGR mouse mode, then `cat -v` echoes whatever reaches
//! its tty as visible text. A forwarded click/release pair must arrive as
//! pane-relative SGR reports; hover must not (the app asked for ?1000, not
//! ?1003).

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
    nested_input: &mut Option<bool>,
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
                    match msg {
                        ServerMessage::RenderOps(ops) => {
                            got.push_str(&String::from_utf8_lossy(&ops));
                        }
                        ServerMessage::NestedInput { enabled } => {
                            *nested_input = Some(enabled);
                        }
                        _ => {}
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
fn click_reaches_a_mouse_mode_app_translated() {
    isolate_state();
    let sock = temp_sock("mouse-passthru");
    let sock_srv = sock.clone();
    // Enable normal tracking + SGR encoding on the pane's tty, then echo
    // everything cat receives as visible characters.
    let script =
        "printf '\\033]777;uniterm-input;1\\007\\033[?1000h\\033[?1006h'; printf READY; cat -v";
    let server = thread::spawn(move || {
        let (mut s, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", script], 80, 24).unwrap();
        s.set_config(Config {
            sidebar: false,
            pane_right_click: true,
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
    let mut nested_input = None;
    let pre = read_until(&mut stream, &mut dec, &mut nested_input, 3, |s| {
        s.contains("READY")
    });
    assert!(pre.contains("READY"), "pane app never started");
    assert_eq!(nested_input, Some(true));

    // Hover first (must NOT be forwarded: mode is ?1000, not ?1003), then a
    // click + release at screen cell (12, 7). The default top bar owns screen
    // row 1, so the Pane receives row 6.
    for kind in [MouseKind::Hover, MouseKind::Click, MouseKind::Release] {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse { x: 12, y: 7, kind }))
            .unwrap();
    }
    let got = read_until(&mut stream, &mut dec, &mut nested_input, 3, |s| {
        s.contains("[<0;12;6m")
    });
    assert!(
        got.contains("[<0;12;6M"),
        "press report never reached the app: {got:?}"
    );
    assert!(
        got.contains("[<0;12;6m"),
        "release report never reached the app: {got:?}"
    );
    assert!(
        !got.contains("[<35;"),
        "hover was forwarded despite ?1000-only tracking: {got:?}"
    );

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 12,
            y: 7,
            kind: MouseKind::RightClick,
        }))
        .unwrap();
    let right = read_until(&mut stream, &mut dec, &mut nested_input, 3, |s| {
        s.contains("[<2;12;6M")
    });
    assert!(
        right.contains("[<2;12;6M"),
        "configured right-click did not reach the opted-in app: {right:?}"
    );

    // `cat` never exits on its own; stop the server so join() returns.
    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
}
