//! S2 integration: the zoom-out overview shows every window as a tile and
//! switches on click; Esc cancels.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_core::Config;
use uniterm_proto::{encode_frame, ClientMessage, Command, FrameDecoder, MouseKind, ServerMessage};
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

fn last_frame(s: &str) -> &str {
    s.rsplit("\x1b[r\x1b[2J").next().unwrap_or("")
}

/// The 1-based columns of every positioned vertical-divider glyph
/// (`ESC[r;cH` immediately followed by `│`) in `s`.
fn divider_cols(s: &str) -> Vec<u16> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(pos) = rest.find('\u{2502}') {
        let head = &rest[..pos];
        if let Some(esc) = head.rfind("\x1b[") {
            let body = &head[esc + 2..];
            // Parse `r;cH` only when that cursor-position escape directly
            // precedes the glyph. A styled status divider ends in `m` and is
            // deliberately ignored here.
            if let Some(inner) = body.strip_suffix('H') {
                if let Some((_, c)) = inner.split_once(';') {
                    if let Ok(col) = c.parse::<u16>() {
                        out.push(col);
                    }
                }
            }
        }
        rest = &rest[pos + '\u{2502}'.len_utf8()..];
    }
    out
}

#[test]
fn overview_lists_tiles_and_click_switches() {
    isolate_state();
    let sock = temp_sock("overview");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        // A solid band of '#' so the sampled miniature still shows '#' runs.
        let (mut s, mut poll) = Server::bind(
            &sock_srv,
            "/bin/sh",
            &["-c", "printf '%.0s#' $(seq 1 200); sleep 15"],
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
    read_until(&mut stream, &mut dec, 3, |s| s.contains("####"));

    // Name window 0, split it (the miniature must show the split), and add a
    // second window, then zoom out.
    stream
        .write_all(&encode_frame(&ClientMessage::RenameWindow {
            name: "alpha".into(),
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::Split(
            uniterm_proto::SplitAxis::LeftRight,
        ))))
        .unwrap();
    read_until(&mut stream, &mut dec, 2, |s| !s.is_empty());
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::NewWindow)))
        .unwrap();
    read_until(&mut stream, &mut dec, 2, |s| s.contains(" 2 "));
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::Overview)))
        .unwrap();
    let got = read_until(&mut stream, &mut dec, 3, |s| {
        s.contains(" 1:alpha ") && s.contains("\u{250C}")
    });
    assert!(
        got.contains(" 1:alpha ") && got.contains(" 2 "),
        "overview tiles missing: {got:?}"
    );
    // The miniature reflects window 0's content (sampled '#' band) AND its
    // split: a mini divider column strictly inside tile 0's interior.
    let last = last_frame(&got);
    assert!(last.contains("#####"), "sampled tile content missing");
    let mini_divider = divider_cols(last)
        .into_iter()
        .any(|c| (3..=38).contains(&c));
    assert!(
        mini_divider,
        "mini split divider missing inside the first tile"
    );

    // Click the left tile (window 0, named alpha): the overview closes and
    // window 0's live content is repainted.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::Click,
        }))
        .unwrap();
    let got = read_until(&mut stream, &mut dec, 3, |s| {
        let last = last_frame(s);
        last.contains("#####") && !last.contains("\u{250C} 1:alpha ")
    });
    let last = last_frame(&got);
    assert!(
        last.contains("#####") && !last.contains("\u{250C} 1:alpha "),
        "click did not switch to the picked window: {last:?}"
    );

    // Reopen and cancel with Esc: back to the same window, no tiles.
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::Overview)))
        .unwrap();
    read_until(&mut stream, &mut dec, 2, |s| {
        last_frame(s).contains("\u{250C} 1:alpha ")
    });
    stream
        .write_all(&encode_frame(&ClientMessage::Input(vec![0x1b])))
        .unwrap();
    let got = read_until(&mut stream, &mut dec, 3, |s| {
        let last = last_frame(s);
        last.contains("#####") && !last.contains("\u{250C}")
    });
    assert!(
        !last_frame(&got).contains("\u{250C}"),
        "Esc did not close the overview"
    );

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
}
