//! M2 integration: a real client-server round trip over a Unix socket.
//!
//! These use a plain std `UnixStream` as the client (no tty needed) so they run
//! in CI. They prove the two properties M2 exists for: a client receives the
//! pane's rendered output, and the server survives a client detach.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
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

fn attach_frame() -> Vec<u8> {
    encode_frame(&ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 80,
        rows: 24,
    })
}

fn control_reply(
    path: &std::path::Path,
    request: ClientMessage,
    timeout: Duration,
) -> ServerMessage {
    let mut stream = UnixStream::connect(path).unwrap();
    stream.set_read_timeout(Some(timeout)).unwrap();
    stream.write_all(&encode_frame(&request)).unwrap();
    stream.flush().unwrap();
    let mut decoder = FrameDecoder::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "server closed before its control reply");
        decoder.push(&buffer[..read]);
        if let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            return message;
        }
    }
}

#[test]
fn client_receives_pane_output() {
    isolate_state();
    let sock = temp_sock("output");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut s, mut poll) = Server::bind(
            &sock_srv,
            "/bin/sh",
            &["-c", "printf HELLO; sleep 1"],
            80,
            24,
        )
        .unwrap();
        let _ = s.run(&mut poll);
    });

    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream.write_all(&attach_frame()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();

    let mut dec = FrameDecoder::new();
    let mut got = String::new();
    let mut buf = [0u8; 8192];
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !got.contains("HELLO") {
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

    let _ = server.join();
    assert!(
        got.contains("HELLO"),
        "expected the pane's 'HELLO' output in the render ops, got: {got:?}"
    );
}

#[test]
fn server_survives_client_detach() {
    isolate_state();
    let sock = temp_sock("detach");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut s, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", "sleep 2"], 80, 24).unwrap();
        let _ = s.run(&mut poll);
    });

    wait_for(&sock);

    // First client attaches then detaches.
    {
        let mut c1 = UnixStream::connect(&sock).unwrap();
        c1.write_all(&attach_frame()).unwrap();
        c1.write_all(&encode_frame(&ClientMessage::Detach)).unwrap();
        c1.flush().unwrap();
    } // c1 dropped

    thread::sleep(Duration::from_millis(100));

    // The server must still be reachable: a second client can attach.
    let reconnect = UnixStream::connect(&sock);
    assert!(
        reconnect.is_ok(),
        "server should survive a client detach and still accept connections"
    );

    let _ = server.join();
}

#[test]
fn first_attach_receives_title_and_frame_without_a_second_message() {
    isolate_state();
    let sock = temp_sock("first-title");
    let workspace = sock.file_stem().unwrap().to_str().unwrap().to_string();
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(
            &sock_srv,
            "/bin/sh",
            &["-c", "printf '\x1b]0;editor-title\x07'; sleep 1"],
            80,
            24,
        )
        .unwrap();
        let config = uniterm_core::Config {
            window_title: "{workspace}: {terminal_title}".into(),
            ..uniterm_core::Config::default()
        };
        server.set_config(config);
        let _ = server.run(&mut poll);
    });

    wait_for(&sock);
    thread::sleep(Duration::from_millis(50));
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream.write_all(&attach_frame()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let mut buffer = [0u8; 8192];
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut title: Option<String> = None;
    let mut rendered = false;
    // The shell's own title write races the attach; keep the latest title
    // until it carries the application's text.
    while Instant::now() < deadline
        && (!title.as_deref().is_some_and(|t| t.contains("editor-title")) || !rendered)
    {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                decoder.push(&buffer[..read]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    match message {
                        ServerMessage::WindowTitle { title: current } => title = Some(current),
                        ServerMessage::RenderOps(ops) if !ops.is_empty() => rendered = true,
                        _ => {}
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("title read failed: {error}"),
        }
    }
    let _ = server.join();
    assert_eq!(title, Some(format!("{workspace}: editor-title")));
    assert!(
        rendered,
        "first Attach must deliver its authoritative frame"
    );
}

#[test]
fn pane_control_waits_on_output_events_and_reads_the_server_grid() {
    isolate_state();
    let sock = temp_sock("pane-wait");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(
            &sock_srv,
            "/bin/sh",
            &["-c", "sleep 0.1; printf READY; sleep 1"],
            80,
            24,
        )
        .unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&sock);
    let wait = control_reply(
        &sock,
        ClientMessage::PaneWaitOutput {
            pane: uniterm_core::PaneId(1),
            needle: "READY".into(),
            timeout_ms: 2_000,
        },
        Duration::from_secs(3),
    );
    assert!(matches!(
        wait,
        ServerMessage::PaneOutputWaited {
            found: true,
            matched: true,
            timed_out: false,
            ref text,
            ..
        } if text.contains("READY")
    ));
    let read = control_reply(
        &sock,
        ClientMessage::PaneRead {
            pane: uniterm_core::PaneId(1),
            lines: 20,
        },
        Duration::from_secs(1),
    );
    assert!(matches!(
        read,
        ServerMessage::PaneOutput {
            found: true,
            ref text,
            ..
        } if text.contains("READY")
    ));
    let _ = server.join();
}

#[test]
fn unchanged_title_is_sent_once_while_output_keeps_flowing() {
    isolate_state();
    let sock = temp_sock("title-once");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(
            &sock_srv,
            "/bin/sh",
            &[
                "-c",
                "printf '\x1b]0;editor-title\x07'; i=0; while [ $i -lt 30 ]; do echo tick $i; i=$((i+1)); sleep 0.02; done; sleep 2",
            ],
            80,
            24,
        )
        .unwrap();
        server.set_config(uniterm_core::Config {
            window_title: "{workspace}: {terminal_title}".into(),
            ..uniterm_core::Config::default()
        });
        let _ = server.run(&mut poll);
    });
    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream.write_all(&attach_frame()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let mut buffer = [0u8; 16_384];
    let deadline = Instant::now() + Duration::from_millis(1500);
    // The shell's title write races the attach, so the title may change
    // once early on. After it has settled, streaming output must not
    // resend it.
    let mut settled = false;
    let mut resent = 0usize;
    let mut renders = 0usize;
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                decoder.push(&buffer[..read]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    match message {
                        ServerMessage::WindowTitle { title } => {
                            if settled {
                                resent += 1;
                            } else if title.contains("editor-title") {
                                settled = true;
                                renders = 0;
                            }
                        }
                        ServerMessage::RenderOps(ops) if !ops.is_empty() => renders += 1,
                        _ => {}
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("read failed: {error}"),
        }
    }
    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
    // Output kept streaming after the title settled, and the unchanged
    // title never crossed the socket again.
    assert!(settled, "the application title never arrived");
    assert!(
        renders > 5,
        "expected streaming output, saw {renders} frames"
    );
    assert_eq!(resent, 0, "unchanged title was resent {resent} times");
}
