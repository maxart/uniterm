//! Session rename: the status line shows the new name, the socket file moves
//! (new attaches use the new path), and the old path is gone.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::isolate_state;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = common::socket_root().join(format!("uniterm-it-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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
fn rename_session_moves_socket_and_updates_status() {
    isolate_state();
    let dir = temp_dir("rename-session");
    // Short fixed names on purpose: the Projects sidebar truncates the
    // Workspace button to 14 cells at 80 columns, so a generated name would
    // never render in full. `isolate_state` keeps the durable state private.
    let old_name = "oldname";
    let new_name = "newname";
    let old_sock = dir.join(format!("{old_name}.sock"));
    let sock_srv = old_sock.clone();
    let server = thread::spawn(move || {
        let (mut s, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", "sleep 20"], 80, 24).unwrap();
        let _ = s.run(&mut poll);
    });

    wait_for(&old_sock);
    let mut stream = UnixStream::connect(&old_sock).unwrap();
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
    read_until(&mut stream, &mut dec, 3, |s| {
        s.contains(&format!(" {old_name} "))
    });

    // Unsafe names are rejected rather than becoming socket or state paths.
    stream
        .write_all(&encode_frame(&ClientMessage::RenameSession {
            name: " new/na me ".into(),
        }))
        .unwrap();
    thread::sleep(Duration::from_millis(20));
    assert!(old_sock.exists());
    assert!(!dir.join(format!("{new_name}.sock")).exists());

    stream
        .write_all(&encode_frame(&ClientMessage::RenameSession {
            name: new_name.into(),
        }))
        .unwrap();
    let got = read_until(&mut stream, &mut dec, 3, |s| {
        s.contains(&format!(" {new_name} "))
    });
    assert!(
        got.contains(&format!(" {new_name} ")),
        "status line never showed the new session name"
    );
    let new_sock = dir.join(format!("{new_name}.sock"));
    assert!(new_sock.exists(), "renamed socket missing");
    assert!(!old_sock.exists(), "old socket still present");

    // A fresh client can attach via the new path; the old stream still works.
    let mut c2 = UnixStream::connect(&new_sock).expect("attach via new path");
    c2.write_all(&encode_frame(&ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 80,
        rows: 24,
    }))
    .unwrap();
    c2.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut dec2 = FrameDecoder::new();
    let got2 = read_until(&mut c2, &mut dec2, 3, |s| {
        s.contains(&format!(" {new_name} "))
    });
    assert!(
        got2.contains(&format!(" {new_name} ")),
        "new attach missing status line"
    );

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
}
