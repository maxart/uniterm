//! Opt-in lifecycle and development-server reliability soak.
//!
//! Run eight hours with:
//! `UNITERM_SOAK_SECONDS=28800 cargo test --release -p uniterm-server --test reliability_soak -- --ignored --nocapture`

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn wait_for(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "server socket did not appear");
        thread::sleep(Duration::from_millis(5));
    }
}

fn list_info(socket: &std::path::Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    if stream
        .write_all(&encode_frame(&ClientMessage::ListInfo))
        .is_err()
    {
        return false;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let mut decoder = FrameDecoder::new();
    let mut buffer = [0; 4096];
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => return false,
            Ok(read) => decoder.push(&buffer[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue
            }
            Err(_) => return false,
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if matches!(message, ServerMessage::Info { .. }) {
                return true;
            }
        }
    }
    false
}

fn detected_server_count(stream: &mut UnixStream, decoder: &mut FrameDecoder) -> Option<usize> {
    stream
        .write_all(&encode_frame(&ClientMessage::Observatory))
        .ok()?;
    let mut buffer = [0; 16 * 1024];
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => return None,
            Ok(read) => decoder.push(&buffer[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue
            }
            Err(_) => return None,
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::DevServers { entries } = message {
                return Some(entries.len());
            }
        }
    }
    None
}

#[test]
#[ignore = "explicit long-running reliability workload"]
fn client_churn_keeps_server_detection_live() {
    isolate_state();
    let duration = std::env::var("UNITERM_SOAK_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(60));
    let dir = std::env::temp_dir().join(format!("ut-soak-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{}.sock", unique_workspace_name()));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 100, 30).unwrap();
        server.run(&mut poll).unwrap();
    });
    wait_for(&socket);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let probing = Arc::new(AtomicBool::new(true));
    let probing_thread = probing.clone();
    let probe_server = thread::spawn(move || {
        while probing_thread.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((_stream, _)) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });

    let mut attached = UnixStream::connect(&socket).unwrap();
    attached
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    attached
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 100,
            rows: 30,
        }))
        .unwrap();
    attached
        .write_all(&encode_frame(&ClientMessage::Input(
            format!("printf 'Ready at http://localhost:{port}\\n'\n").into_bytes(),
        )))
        .unwrap();

    let mut decoder = FrameDecoder::new();
    let detection_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if detected_server_count(&mut attached, &mut decoder).unwrap_or(0) > 0 {
            break;
        }
        assert!(
            Instant::now() < detection_deadline,
            "development server was never detected"
        );
    }

    // Queue more connections than one accept batch. The persistent attached
    // client must remain responsive while the server drains the real backlog
    // over multiple event-loop turns.
    let burst: Vec<_> = (0..96)
        .map(|_| UnixStream::connect(&socket).unwrap())
        .collect();
    drop(burst);
    assert!(
        list_info(&socket),
        "server stopped responding after a multi-batch connection burst"
    );

    let started = Instant::now();
    let mut cycles = 0u64;
    while started.elapsed() < duration {
        assert!(
            list_info(&socket),
            "server stopped responding at cycle {cycles}"
        );
        cycles += 1;
    }
    assert!(
        detected_server_count(&mut attached, &mut decoder).unwrap_or(0) > 0,
        "development-server detection disappeared after {cycles} client cycles"
    );
    eprintln!("completed {cycles} client lifecycle cycles in {duration:?}");

    attached
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();
    probing.store(false, Ordering::Relaxed);
    probe_server.join().unwrap();
    let _ = std::fs::remove_dir_all(dir);
}
