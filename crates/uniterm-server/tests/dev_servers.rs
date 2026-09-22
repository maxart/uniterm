//! Observatory development-server discovery and pane-link handoff over a real
//! client/server socket.

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
    let dir =
        common::socket_root().join(format!("uniterm-dev-servers-{}-{tag}", std::process::id()));
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

fn read_full_frame_until(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
    predicate: impl Fn(&str) -> bool,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut buffer = [0u8; 32_768];
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => decoder.push(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("render read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::RenderOps(ops) = message {
                let frame = String::from_utf8_lossy(&ops).into_owned();
                if frame.contains("\x1b[r\x1b[2J") {
                    seen.push((
                        frame.contains(" AGENTS"),
                        frame.contains(" FILES"),
                        frame.contains(" WEB SERVERS"),
                    ));
                    if predicate(&frame) {
                        return frame;
                    }
                }
            }
        }
    }
    panic!("matching full frame did not arrive; observed tabs: {seen:?}");
}

fn read_servers_until(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
    count: usize,
) -> Vec<uniterm_proto::DevServerEntry> {
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut buffer = [0u8; 32_768];
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => decoder.push(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("server projection read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::DevServers { entries } = message {
                if entries.len() == count {
                    return entries;
                }
            }
        }
    }
    panic!("development-server projection never reached {count} entries");
}

fn announce_server(port: u16) -> Vec<u8> {
    format!("printf 'Ready at http://localhost:{port}\\n'\n").into_bytes()
}

#[test]
fn observatory_lists_announced_server_and_plain_url_click_opens_it() {
    isolate_state();
    let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
        return;
    };
    let port = listener.local_addr().unwrap().port();
    let url = format!("http://localhost:{port}");
    let command = format!("printf 'Server listening on {url}\\n'; sleep 5");
    let sock = temp_sock("open");
    let sock_server = sock.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&sock_server, "/bin/sh", &["-c", &command], 100, 30).unwrap();
        server.set_config(Config {
            status: false,
            sidebar: false,
            ..Config::default()
        });
        let _ = server.run(&mut poll);
    });

    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 100,
            rows: 30,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Observatory))
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 16384];
    let mut listed = None;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && listed.is_none() {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => {
                decoder.push(&buf[..read]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    if let ServerMessage::DevServers { entries } = message {
                        if !entries.is_empty() {
                            listed = entries.into_iter().next();
                        }
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
    let listed = listed.expect("announced server never reached Observatory");
    assert_eq!(listed.label, "server");
    assert_eq!(listed.url, url);
    assert_eq!(listed.port, port);
    assert!(!listed.project_name.is_empty());
    assert!(!listed.project_root.is_empty());

    for kind in [MouseKind::Click, MouseKind::Release] {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse { x: 25, y: 1, kind }))
            .unwrap();
    }
    let mut opened = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && opened.is_none() {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => {
                decoder.push(&buf[..read]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    if let ServerMessage::OpenUrl { url } = message {
                        opened = Some(url);
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
    assert_eq!(opened.as_deref(), Some(url.as_str()));

    let _ = stream.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = server.join();
    drop(listener);
}

#[test]
fn web_server_scope_toggles_between_project_and_workspace() {
    isolate_state();
    let Ok(first_listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
        return;
    };
    let Ok(second_listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
        return;
    };
    let first_port = first_listener.local_addr().unwrap().port();
    let second_port = second_listener.local_addr().unwrap().port();
    let first_url = format!("http://localhost:{first_port}");
    let second_url = format!("http://localhost:{second_port}");
    let first_port_label = format!(":{first_port}");
    let second_port_label = format!(":{second_port}");
    let sock = temp_sock("scope");
    let root = sock.parent().unwrap().to_path_buf();
    let server_sock = sock.clone();
    let first_command = format!("printf 'Ready at {first_url}\\n'; sleep 30");
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_sock, "/bin/sh", &["-c", &first_command], 100, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    wait_for(&sock);

    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 100,
            rows: 30,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Observatory))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    read_servers_until(&mut stream, &mut decoder, 1);

    stream
        .write_all(&encode_frame(&ClientMessage::ProjectCreate {
            name: "Second".into(),
            root: root.to_string_lossy().into_owned(),
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Input(announce_server(
            second_port,
        ))))
        .unwrap();
    read_servers_until(&mut stream, &mut decoder, 2);

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 95,
            y: 1,
            kind: MouseKind::Click,
        }))
        .unwrap();
    let project_frame = read_full_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains("WEB SERVERS")
    });
    assert!(project_frame.contains("WEB SERVERS 1"));
    assert!(project_frame.contains("project"));
    assert!(project_frame.contains(&second_url));
    assert!(
        !project_frame.contains(&first_port_label),
        "Project scope leaked a server from another Project"
    );

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 95,
            y: 3,
            kind: MouseKind::Click,
        }))
        .unwrap();
    let workspace_frame = read_full_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains("WEB SERVERS 2")
            && frame.contains("workspace")
            && frame.contains(&first_port_label)
            && frame.contains(&second_port_label)
    });
    assert!(workspace_frame.contains("Second \u{00B7}"));

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 95,
            y: 3,
            kind: MouseKind::Click,
        }))
        .unwrap();
    let project_frame = read_full_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains("WEB SERVERS 1") && frame.contains("project") && frame.contains(&second_url)
    });
    assert!(!project_frame.contains(&first_port_label));

    let _ = stream.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = server.join();
    drop((first_listener, second_listener));
    let _ = std::fs::remove_dir_all(root);
}
