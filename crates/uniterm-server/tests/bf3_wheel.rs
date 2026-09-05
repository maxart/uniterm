//! BF3 integration: the mouse wheel scrolls scrollback via copy-mode.
//!
//! Wheel-up over a pane with history must open the copy-mode viewport (the
//! `[COPY` indicator appears in the render ops); wheel-down back at the live
//! bottom must leave copy-mode again.

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

/// Read render ops until `pred` matches (or the deadline passes); returns the
/// accumulated ops text since the call.
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

fn latest_button_position(render: &str) -> Option<(u16, u16)> {
    let label = render.rfind("[v Latest]")?;
    let before = &render[..label];
    let cup_end = before.rfind('H')?;
    let cup_start = before[..cup_end].rfind("\x1b[")? + 2;
    let (row, col) = before[cup_start..cup_end].split_once(';')?;
    Some((col.parse().ok()?, row.parse().ok()?))
}

#[test]
fn wheel_scrolls_scrollback_and_returns_to_live() {
    isolate_state();
    let sock = temp_sock("wheel");
    let sock_srv = sock.clone();
    // Emit 60 numbered lines into a 24-row pane so real history exists.
    let server = thread::spawn(move || {
        let (mut s, mut poll) = Server::bind(
            &sock_srv,
            "/bin/sh",
            &["-c", "seq 1 60; echo HISTORY-READY; sleep 30"],
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

    // Wait for the pane output to arrive (the tail of seq).
    // Wait for the marker line, not for "60": a cursor-position sequence can
    // contain those digits long before the sixtieth line exists, and copy-mode
    // needs the whole history in place before the wheel starts scrolling.
    let pre = read_until(&mut stream, &mut dec, 10, |s| s.contains("HISTORY-READY"));
    assert!(pre.contains("HISTORY-READY"), "pane output never arrived");

    // Wheel-up over the pane: copy-mode opens, scrolled into history.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::WheelUp,
        }))
        .unwrap();
    let scrolled = read_until(&mut stream, &mut dec, 10, |s| s.contains("[COPY"));
    assert!(
        scrolled.contains("[COPY"),
        "wheel-up did not open the copy-mode viewport"
    );

    // Once the viewport is over one page behind, the Latest button appears
    // beside (without replacing) the current-line/total-lines indicator.
    for _ in 0..9 {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse {
                x: 10,
                y: 5,
                kind: MouseKind::WheelUp,
            }))
            .unwrap();
    }
    let far_back = read_until(&mut stream, &mut dec, 10, |s| s.contains("[v Latest]"));
    assert!(
        far_back.contains("[COPY"),
        "line-count indicator disappeared"
    );
    let (button_x, button_y) =
        latest_button_position(&far_back).expect("Latest button position in render ops");
    for kind in [MouseKind::Click, MouseKind::Release] {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse {
                x: button_x,
                y: button_y,
                kind,
            }))
            .unwrap();
    }
    stream
        .write_all(&encode_frame(&ClientMessage::Refresh))
        .unwrap();
    let latest = read_until(&mut stream, &mut dec, 3, |s| {
        s.rsplit("\x1b[r\x1b[2J")
            .next()
            .is_some_and(|last| !last.is_empty() && !last.contains("[COPY"))
    });
    let last_frame = latest.rsplit("\x1b[r\x1b[2J").next().unwrap_or("");
    assert!(
        !last_frame.contains("[COPY"),
        "Latest button did not resume live output"
    );

    // Re-enter copy-mode so the wheel-down path remains covered too.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::WheelUp,
        }))
        .unwrap();
    let scrolled = read_until(&mut stream, &mut dec, 3, |s| s.contains("[COPY"));
    assert!(scrolled.contains("[COPY"));

    // Wheel-down enough to hit the bottom: copy-mode exits (no more [COPY in
    // the final full frame).
    for _ in 0..20 {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse {
                x: 10,
                y: 5,
                kind: MouseKind::WheelDown,
            }))
            .unwrap();
    }
    // Queue a full-frame barrier after the wheel burst so the assertion does
    // not depend on how the socket batches the copy-mode and live repaints.
    stream
        .write_all(&encode_frame(&ClientMessage::Refresh))
        .unwrap();
    let back = read_until(&mut stream, &mut dec, 3, |s| {
        // The last full frame after leaving copy-mode has no indicator.
        s.rsplit("\x1b[r\x1b[2J")
            .next()
            .map(|last| !last.is_empty() && !last.contains("[COPY"))
            .unwrap_or(false)
    });
    let last_frame = back.rsplit("\x1b[r\x1b[2J").next().unwrap_or("");
    assert!(
        !last_frame.contains("[COPY"),
        "wheel-down at the bottom did not leave copy-mode"
    );

    let _ = stream.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = server.join();
}

#[test]
fn wheel_scrolls_history_emitted_by_an_inline_tui_region() {
    isolate_state();
    let sock = temp_sock("wheel-inline-region");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut s, mut poll) = Server::bind(
            &sock_srv,
            "/bin/sh",
            &[
                "-c",
                "printf '\\033[1;1HA\\033[2;1HB\\033[3;1HC\\033[4;1HD\\033[1;3r\\033[3;1H\\n'; sleep 5",
            ],
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

    let pre = read_until(&mut stream, &mut dec, 3, |s| s.contains('D'));
    assert!(pre.contains('D'), "inline TUI output never arrived");

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::WheelUp,
        }))
        .unwrap();
    let scrolled = read_until(&mut stream, &mut dec, 3, |s| s.contains("[COPY"));
    assert!(
        scrolled.contains("[COPY"),
        "wheel-up could not reach inline TUI history"
    );

    let _ = stream.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = server.join();
}
