//! S3 integration: selection in an app that owns the mouse, and keeping a
//! selection on screen.
//!
//! An agent UI on the alternate screen asks for full mouse tracking, as vim
//! with `mouse=a` does. Without `freeze-on-select` every press, drag, and
//! release is the app's. With it, a left-button drag is uniterm's frozen
//! selection and the app never sees it, while a plain click still reaches
//! the app whole. With `copy-on-select` off, a released selection stays
//! highlighted until `y` copies it or a plain click dismisses it.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_core::Config;
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, MouseKind, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

/// The full-repaint prelude every `full_repaint_all` frame starts with.
const FRAME: &str = "\x1b[r\x1b[2J";

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

fn last_frame(ops: &str) -> &str {
    ops.rsplit(FRAME).next().unwrap_or("")
}

/// Spawn a server whose first pane runs `script`, attach an 80x24 client,
/// then let the script past its `{go}` gate and wait for `ready` to be
/// painted. The gate keeps the app's output after the attach, so no resize
/// lands between what it paints and the mouse events that follow.
fn attach(
    tag: &str,
    script: &str,
    config: Config,
    ready: &str,
) -> (thread::JoinHandle<()>, UnixStream, FrameDecoder) {
    isolate_state();
    let sock = temp_sock(tag);
    let sock_srv = sock.clone();
    let go = common::temp_dir(tag).join("go");
    let script = script.replace("{go}", &go.display().to_string());
    let server = thread::spawn(move || {
        let (mut s, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", &script], 80, 24).unwrap();
        s.set_config(config);
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
    std::fs::write(&go, b"").unwrap();
    let pre = read_until(&mut stream, &mut dec, 5, |s| s.contains(ready));
    assert!(pre.contains(ready), "pane app never started: {pre:?}");
    (server, stream, dec)
}

/// A shell gate that waits for the `{go}` file `attach` creates.
const GATE: &str = "while [ ! -e {go} ]; do sleep 0.05; done; ";

fn mouse(stream: &mut UnixStream, events: &[(u16, MouseKind)]) {
    for &(x, kind) in events {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse { x, y: 2, kind }))
            .unwrap();
    }
}

/// An agent-like app: alternate screen, all-motion SGR mouse tracking, some
/// text to select, then `cat -v` echoing every report it receives. It runs as
/// a foreground job in its own process group (`set -m`), as a shell launches
/// a real app; a shell that itself owns the foreground on the alternate
/// screen is what the server treats as a stranded screen and recovers.
const MOUSE_APP: &str = "set -m; sh -c \"printf '\\033[?1049h\\033[?1003h\\033[?1006h'; \
                         printf HELLOWORLD; exec cat -v\"";

fn gated(script: &str) -> String {
    format!("{GATE}{script}")
}

#[test]
fn freeze_on_select_takes_the_drag_from_a_mouse_mode_app_and_defers_clicks() {
    let (server, mut stream, mut dec) = attach(
        "app-takeover",
        &gated(MOUSE_APP),
        Config {
            sidebar: false,
            freeze_on_select: true,
            ..Config::default()
        },
        "HELLOWORLD",
    );

    // Press on 'H', drag to the second 'O', release: uniterm's selection.
    mouse(
        &mut stream,
        &[
            (1, MouseKind::Click),
            (3, MouseKind::Drag),
            (5, MouseKind::Drag),
            (5, MouseKind::Release),
        ],
    );
    let got = read_until(&mut stream, &mut dec, 3, |s| {
        s.contains("]52;c;") && !last_frame(s).contains("[COPY")
    });
    assert!(
        got.contains("]52;c;SEVMTE8="),
        "expected OSC 52 yank of HELLO, got: {got:?}"
    );
    assert!(
        !got.contains("[<0;") && !got.contains("[<32;"),
        "the app received mouse reports for uniterm's selection: {got:?}"
    );

    // A plain click is the app's: the withheld press arrives with the release.
    mouse(
        &mut stream,
        &[(8, MouseKind::Click), (8, MouseKind::Release)],
    );
    let click = read_until(&mut stream, &mut dec, 3, |s| s.contains("[<0;8;1m"));
    assert!(
        click.contains("[<0;8;1M") && click.contains("[<0;8;1m"),
        "deferred press and release did not reach the app: {click:?}"
    );
    assert!(
        click.find("[<0;8;1M") < click.find("[<0;8;1m"),
        "press must be delivered before release: {click:?}"
    );

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
}

#[test]
fn a_mouse_mode_app_keeps_every_drag_without_freeze_on_select() {
    let (server, mut stream, mut dec) = attach(
        "app-keeps-drag",
        &gated(MOUSE_APP),
        Config {
            sidebar: false,
            ..Config::default()
        },
        "HELLOWORLD",
    );
    mouse(
        &mut stream,
        &[
            (1, MouseKind::Click),
            (3, MouseKind::Drag),
            (5, MouseKind::Release),
        ],
    );
    let got = read_until(&mut stream, &mut dec, 3, |s| s.contains("[<0;5;1m"));
    assert!(
        got.contains("[<0;1;1M") && got.contains("[<32;3;1M") && got.contains("[<0;5;1m"),
        "press, drag, and release must all reach the app: {got:?}"
    );
    assert!(
        !got.contains("]52;c;") && !got.contains("[COPY"),
        "uniterm selected in an app that owns the mouse: {got:?}"
    );

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
}

#[test]
fn copy_on_select_off_keeps_the_selection_until_a_key_copies_or_a_click_dismisses_it() {
    let (server, mut stream, mut dec) = attach(
        "keep-selection",
        &gated("printf HELLOWORLD; sleep 5"),
        Config {
            sidebar: false,
            freeze_on_select: true,
            copy_on_select: false,
            ..Config::default()
        },
        "HELLOWORLD",
    );

    // Two drags and the release each repaint; the release must leave the
    // selection on screen and nothing on the clipboard.
    mouse(
        &mut stream,
        &[
            (1, MouseKind::Click),
            (3, MouseKind::Drag),
            (5, MouseKind::Drag),
            (5, MouseKind::Release),
        ],
    );
    // With copy-on-select off the release keeps the selection and writes
    // nothing to the clipboard, so there is no unique end marker to wait for.
    // Read a settle window and assert the resulting state rather than counting
    // full-frame repaints, which coalesce differently across platforms.
    let kept = read_until(&mut stream, &mut dec, 2, |_| false);
    assert!(
        kept.contains("[COPY"),
        "the drag never entered copy-mode: {kept:?}"
    );
    assert!(
        last_frame(&kept).contains("[COPY"),
        "selection was dropped on release: {kept:?}"
    );
    assert!(
        !kept.contains("]52;c;"),
        "clipboard written despite copy-on-select off"
    );

    // `y` copies the kept selection and resumes the live screen.
    stream
        .write_all(&encode_frame(&ClientMessage::Input(b"y".to_vec())))
        .unwrap();
    let copied = read_until(&mut stream, &mut dec, 3, |s| {
        s.contains("]52;c;") && !last_frame(s).contains("[COPY")
    });
    assert!(
        copied.contains("]52;c;SEVMTE8="),
        "expected OSC 52 yank of HELLO on y, got: {copied:?}"
    );
    assert!(!last_frame(&copied).contains("[COPY"));

    // Select again, then a plain click dismisses it without copying.
    mouse(
        &mut stream,
        &[
            (1, MouseKind::Click),
            (3, MouseKind::Drag),
            (3, MouseKind::Release),
        ],
    );
    let again = read_until(&mut stream, &mut dec, 2, |_| false);
    assert!(
        last_frame(&again).contains("[COPY"),
        "second selection not kept: {again:?}"
    );
    mouse(
        &mut stream,
        &[(7, MouseKind::Click), (7, MouseKind::Release)],
    );
    let dismissed = read_until(&mut stream, &mut dec, 3, |s| {
        !last_frame(s).contains("[COPY")
    });
    assert!(
        !last_frame(&dismissed).contains("[COPY"),
        "plain click did not dismiss the kept selection: {dismissed:?}"
    );
    assert!(!dismissed.contains("]52;c;"), "dismissal must not copy");

    let _ = server.join();
}
