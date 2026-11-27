//! Shared-canvas geometry stays valid across attaches, resizes, and detaches.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn attach(path: &std::path::Path, cols: u16, rows: u16) -> UnixStream {
    let mut stream = UnixStream::connect(path).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols,
            rows,
        }))
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream
}

fn expect_height(stream: &mut UnixStream, decoder: &mut FrameDecoder, rows: u16) {
    let mut buf = [0; 32 * 1024];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            if let ServerMessage::RenderOps(ops) = message {
                let frame = String::from_utf8_lossy(&ops);
                let last_row = frame
                    .split("\x1b[")
                    .filter_map(|part| {
                        let (row, rest) = part.split_once(';')?;
                        rest.starts_with("1H").then(|| row.parse::<u16>().ok())?
                    })
                    .max();
                if frame.contains("\x1b[r\x1b[2J") && last_row == Some(rows) {
                    return;
                }
            }
        }
        assert!(Instant::now() < deadline, "no full frame of height {rows}");
        match stream.read(&mut buf) {
            Ok(0) => panic!("connection closed before a full frame of height {rows}"),
            Ok(n) => decoder.push(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("read frame: {error}"),
        }
    }
}

fn wait_for(path: &std::path::Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("server socket did not appear");
}

#[test]
fn smallest_attached_client_defines_shared_canvas() {
    isolate_state();
    let dir = common::socket_root().join(format!("uniterm-multi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{}.sock", unique_workspace_name()));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 3"], 80, 24).unwrap();
        server.run(&mut poll).unwrap();
    });
    wait_for(&socket);

    let mut large = attach(&socket, 80, 24);
    let mut decoder = FrameDecoder::new();
    expect_height(&mut large, &mut decoder, 24);

    let mut small = attach(&socket, 40, 12);
    expect_height(&mut large, &mut decoder, 12);
    small
        .write_all(&encode_frame(&ClientMessage::Resize { cols: 60, rows: 20 }))
        .unwrap();
    expect_height(&mut large, &mut decoder, 20);
    small
        .write_all(&encode_frame(&ClientMessage::Detach))
        .unwrap();
    expect_height(&mut large, &mut decoder, 24);

    drop(large);
    server.join().unwrap();
}

/// Observe the child PTY as well as the outer paint. A full desktop frame
/// around a phone-sized shell would still leave the user's program stranded.
fn sample_pty(client: &mut UnixStream, dir: &std::path::Path, label: &str) -> (u16, u16) {
    client
        .write_all(&encode_frame(&ClientMessage::Input(
            format!("{label}\n").into_bytes(),
        )))
        .unwrap();
    let path = dir.join(label);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some((rows, cols)) = text.trim().split_once(' ') {
                return (rows.parse().unwrap(), cols.parse().unwrap());
            }
        }
        assert!(Instant::now() < deadline, "child did not report {label}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn broken_phone_output_restores_desktop_while_its_input_socket_stays_open() {
    isolate_state();
    let dir = common::temp_dir("phone-size");
    let socket = dir.join(format!("{}.sock", unique_workspace_name()));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let script = r#"stty -echo; while IFS= read -r label; do stty size > "$1/$label"; echo "$label"; done"#;
        let args = [
            "-c",
            script,
            "phone-size",
            server_socket.parent().unwrap().to_str().unwrap(),
        ];
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &args, 164, 30).unwrap();
        server.run(&mut poll).unwrap();
    });
    wait_for(&socket);
    let mut desktop = attach(&socket, 164, 30);
    let mut decoder = FrameDecoder::new();
    expect_height(&mut desktop, &mut decoder, 30);
    let full_size = sample_pty(&mut desktop, &dir, "original");
    let mut phone = attach(&socket, 40, 12);
    expect_height(&mut desktop, &mut decoder, 12);
    let small_size = sample_pty(&mut desktop, &dir, "small");
    assert!(small_size.0 < full_size.0 && small_size.1 < full_size.1);

    // An unrelated control connection closing cannot evict a quiet phone.
    drop(UnixStream::connect(&socket).unwrap());
    assert_eq!(sample_pty(&mut desktop, &dir, "quiet"), small_size);
    // Drain the attach reply so no pending writable event can mask a failed
    // broadcast. The bridge's upload descriptor survives the broken download.
    expect_height(&mut phone, &mut FrameDecoder::new(), 12);
    // Linux reports EPIPE for this half-close while the upload is still open.
    // BSD Unix sockets can continue accepting writes after the peer's SHUT_RD.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    phone.shutdown(std::net::Shutdown::Read).unwrap();
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    drop(phone);
    desktop
        .write_all(&encode_frame(&ClientMessage::Input(b"dirty\n".to_vec())))
        .unwrap();
    expect_height(&mut desktop, &mut decoder, 30);
    assert_eq!(sample_pty(&mut desktop, &dir, "restored"), full_size);

    // A new small attachment participates normally and can disappear without
    // Detach or a final valid frame. No desktop resize or keyboard input follows.
    let mut reconnected = attach(&socket, 40, 12);
    expect_height(&mut desktop, &mut decoder, 12);
    reconnected
        .write_all(&encode_frame(&ClientMessage::Detach)[..2])
        .unwrap();
    drop(reconnected);
    expect_height(&mut desktop, &mut decoder, 30);
    assert_eq!(sample_pty(&mut desktop, &dir, "again"), full_size);
    desktop
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();
}
