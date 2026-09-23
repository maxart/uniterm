//! S1 integration: click-drag text selection is always on.
//!
//! Dragging across pane text must yank it to the clipboard (an OSC 52 write
//! in the render ops) and return the pane to the live screen. With the
//! `freeze-on-select` setting the pane's screen must hold still from the
//! first drag until release while the application keeps printing.

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

/// The printable text of rendered ops with every CSI sequence removed, so
/// copy-mode's per-cell styling does not split words.
fn visible_text(ops: &str) -> String {
    let mut out = String::new();
    let mut chars = ops.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// Run the freeze scenario: press and drag over "HELLOWORLD", let the pane's
/// program print forty lines (proved consumed by the OSC 52 it emits after
/// them), drag again, and return the visible text of the copy-mode frame
/// painted for that second drag. Releasing then yanks and resumes live output.
fn select_while_output_scrolls(tag: &str, freeze_on_select: bool) -> String {
    isolate_state();
    let sock = temp_sock(tag);
    let sock_srv = sock.clone();
    let go = common::temp_dir(tag).join("go");
    let script = format!(
        "printf 'HELLOWORLD\\n'; while [ ! -e {} ]; do sleep 0.05; done; \
         i=1; while [ $i -le 40 ]; do echo line$i; i=$((i+1)); done; \
         printf '\\033]52;c;RE9ORQ==\\007'; sleep 5",
        go.display()
    );
    let server = thread::spawn(move || {
        let (mut s, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", &script], 80, 24).unwrap();
        s.set_config(Config {
            sidebar: false,
            freeze_on_select,
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

    for (x, kind) in [(1, MouseKind::Click), (3, MouseKind::Drag)] {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse { x, y: 2, kind }))
            .unwrap();
    }
    let frozen = read_until(&mut stream, &mut dec, 3, |s| s.contains("[COPY"));
    assert!(frozen.contains("[COPY"), "the drag never entered copy-mode");

    // Release the program: forty lines, then a clipboard write that proves
    // the server parsed everything before it.
    std::fs::write(&go, b"").unwrap();
    let consumed = read_until(&mut stream, &mut dec, 5, |s| s.contains("]52;c;RE9ORQ=="));
    assert!(
        consumed.contains("]52;c;RE9ORQ=="),
        "program output never arrived"
    );

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 5,
            y: 2,
            kind: MouseKind::Drag,
        }))
        .unwrap();
    let dragged = read_until(&mut stream, &mut dec, 3, |s| {
        s.rsplit("\x1b[r\x1b[2J")
            .next()
            .is_some_and(|last| last.contains("[COPY"))
    });
    let frame = visible_text(dragged.rsplit("\x1b[r\x1b[2J").next().unwrap_or(""));
    assert!(
        frame.contains("[COPY"),
        "no copy-mode frame after the drag: {frame:?}"
    );

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 5,
            y: 2,
            kind: MouseKind::Release,
        }))
        .unwrap();
    let released = read_until(&mut stream, &mut dec, 3, |s| {
        s.contains("]52;c;SEVMTE8=")
            && s.rsplit("\x1b[r\x1b[2J")
                .next()
                .is_some_and(|last| !last.contains("[COPY") && last.contains("line40"))
    });
    assert!(
        released.contains("]52;c;SEVMTE8="),
        "expected OSC 52 yank of HELLO, got: {released:?}"
    );
    let live = released.rsplit("\x1b[r\x1b[2J").next().unwrap_or("");
    assert!(
        !live.contains("[COPY") && live.contains("line40"),
        "release did not resume the live screen: {live:?}"
    );

    let _ = server.join();
    frame
}

#[test]
fn freeze_on_select_holds_the_pane_while_output_scrolls() {
    let frame = select_while_output_scrolls("freeze", true);
    assert!(frame.contains("HELLOWORLD"), "{frame:?}");
    assert!(
        !frame.contains("line"),
        "output reached the frozen selection: {frame:?}"
    );
}

#[test]
fn selection_without_freeze_on_select_shows_output_written_under_it() {
    let frame = select_while_output_scrolls("nofreeze", false);
    assert!(frame.contains("HELLOWORLD"), "{frame:?}");
    assert!(
        frame.contains("line1"),
        "the default logical viewport hid an in-place write: {frame:?}"
    );
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
