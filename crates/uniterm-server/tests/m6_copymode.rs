//! M6 integration: entering copy-mode renders the copy-mode indicator, and a
//! yank produces an OSC 52 clipboard write.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, Command, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn drain_render(c: &mut UnixStream, dec: &mut FrameDecoder, needle: &str, secs: u64) -> String {
    let mut text = String::new();
    let mut buf = [0u8; 16384];
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline && !text.contains(needle) {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                dec.push(&buf[..n]);
                while let Ok(Some(message)) = dec.decode::<ServerMessage>() {
                    if let ServerMessage::RenderOps(ops) = message {
                        text.push_str(&String::from_utf8_lossy(&ops));
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    text
}

#[test]
fn copy_mode_indicator_and_osc52_yank() {
    isolate_state();
    let dir = std::env::temp_dir().join(format!("uniterm-m6-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join(format!("{}.sock", unique_workspace_name()));
    let sock_srv = sock.clone();

    let server = thread::spawn(move || {
        // Emit a couple of lines, then keep the shell alive.
        let (mut s, mut poll) = Server::bind(
            &sock_srv,
            "/bin/sh",
            &["-c", "printf 'AA\\nBB\\n'; sleep 3"],
            80,
            24,
        )
        .unwrap();
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

    c.write_all(&encode_frame(&ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 80,
        rows: 24,
    }))
    .unwrap();
    // Enter copy-mode.
    c.write_all(&encode_frame(&ClientMessage::Command(Command::CopyMode)))
        .unwrap();
    c.flush().unwrap();
    let indicator = drain_render(&mut c, &mut dec, "[COPY", 2);
    assert!(
        indicator.contains("[COPY"),
        "expected the copy-mode indicator, got: {indicator:?}"
    );

    // Select from the top and yank: expect an OSC 52 clipboard write.
    for key in [b"g".as_slice(), b"v", b"l", b"l", b"y"] {
        c.write_all(&encode_frame(&ClientMessage::Input(key.to_vec())))
            .unwrap();
    }
    c.flush().unwrap();
    let after = drain_render(&mut c, &mut dec, "\x1b]52;c;", 2);

    let _ = c.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = c.flush();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        after.contains("\x1b]52;c;"),
        "expected an OSC 52 clipboard write on yank"
    );
}
