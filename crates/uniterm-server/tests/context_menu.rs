//! Right-click context menus cross the client protocol and drive server-owned
//! Pane actions without forwarding the click to the child PTY.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_core::Config;
use uniterm_proto::{
    encode_frame, ClientMessage, Command, FrameDecoder, MouseKind, ServerMessage, SplitAxis,
};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_sock() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("uniterm-context-{}", std::process::id()));
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

fn read_until_render(stream: &mut UnixStream, decoder: &mut FrameDecoder, needle: &str) -> String {
    let mut rendered = String::new();
    let mut buffer = [0u8; 16384];
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !rendered.contains(needle) {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                decoder.push(&buffer[..read]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    if let ServerMessage::RenderOps(ops) = message {
                        rendered.push_str(&String::from_utf8_lossy(&ops));
                    }
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => panic!("could not read server frame: {error}"),
        }
    }
    rendered
}

fn read_pane_count(stream: &mut UnixStream, decoder: &mut FrameDecoder) -> Option<u32> {
    let mut buffer = [0u8; 16384];
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => return None,
            Ok(read) => {
                decoder.push(&buffer[..read]);
                while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
                    if let ServerMessage::Info { panes, .. } = message {
                        return Some(panes);
                    }
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => panic!("could not read server info: {error}"),
        }
    }
    None
}

#[test]
fn pane_menu_split_targets_the_pane_and_close_is_immediate() {
    isolate_state();
    let socket = temp_sock();
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 30"], 80, 24).unwrap();
        server.set_config(Config {
            sidebar: false,
            confirm_close: true,
            ..Config::default()
        });
        let _ = server.run(&mut poll);
    });

    wait_for(&socket);
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        }))
        .unwrap();

    // Right-click at client cell (10, 5). The server stores zero-based (9, 4),
    // so the first item is inside the box at client cell (11, 6).
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::RightClick,
        }))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let menu = read_until_render(&mut stream, &mut decoder, "Close pane");
    assert!(
        menu.contains("Split right"),
        "Pane menu did not render: {menu:?}"
    );

    // Some macOS terminals deliver the actionable primary click on button
    // release. A release-only report must still activate the selected row.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 11,
            y: 6,
            kind: MouseKind::Release,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    assert_eq!(read_pane_count(&mut stream, &mut decoder), Some(2));

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::RightClick,
        }))
        .unwrap();
    let menu = read_until_render(&mut stream, &mut decoder, "Close pane");
    assert!(
        !menu.contains("Confirm close"),
        "Pane menu unexpectedly requested confirmation: {menu:?}"
    );

    // With a second Pane in the Tab the menu also offers "Move to new tab",
    // so Close pane is the eighth row.
    assert!(menu.contains("Move to new tab"));
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 11,
            y: 13,
            kind: MouseKind::Click,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    assert_eq!(read_pane_count(&mut stream, &mut decoder), Some(1));

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
}

fn read_info(stream: &mut UnixStream, decoder: &mut FrameDecoder) -> Option<(u32, u32)> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buffer = [0u8; 16_384];
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => decoder.push(&buffer[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("info read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::Info { windows, panes } = message {
                return Some((windows, panes));
            }
        }
    }
    None
}

#[test]
fn pane_menu_moves_a_pane_between_tabs_and_closes_an_emptied_tab() {
    isolate_state();
    let socket = temp_sock();
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 30"], 80, 24).unwrap();
        server.set_config(Config {
            sidebar: false,
            ..Config::default()
        });
        let _ = server.run(&mut poll);
    });
    wait_for(&socket);
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        }))
        .unwrap();
    // Tab 1 gets two Panes, Tab 2 one Pane, then return to Tab 1.
    for command in [
        Command::Split(SplitAxis::LeftRight),
        Command::NewWindow,
        Command::PrevWindow,
    ] {
        stream
            .write_all(&encode_frame(&ClientMessage::Command(command)))
            .unwrap();
    }
    stream
        .write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    assert_eq!(read_info(&mut stream, &mut decoder), Some((2, 3)));

    // A Pane in Tab 1 can go to Tab 2 or to a fresh Tab; the menu numbers the
    // destination as the Tab bar does.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::RightClick,
        }))
        .unwrap();
    let menu = read_until_render(&mut stream, &mut decoder, "Close pane");
    assert!(menu.contains("Move to tab 2"), "{menu:?}");
    assert!(menu.contains("Move to new tab"), "{menu:?}");
    assert!(!menu.contains("Move to tab 1"), "{menu:?}");
    // Rows: six fixed actions, then Move to tab 2 (client row 12).
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 11,
            y: 12,
            kind: MouseKind::Click,
        }))
        .unwrap();
    // The move itself repaints every client: the menu is gone and the
    // destination Tab is now the active one in the Tab bar.
    let moved = read_until_render(&mut stream, &mut decoder, "\x1b[r\x1b[2J");
    assert!(!moved.contains("Move to"), "{moved:?}");
    stream
        .write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    // Same Tabs, same Panes, nothing killed.
    assert_eq!(read_info(&mut stream, &mut decoder), Some((2, 3)));

    // Tab 2 is now active with two Panes: it offers Tab 1 and a new Tab.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::RightClick,
        }))
        .unwrap();
    let menu = read_until_render(&mut stream, &mut decoder, "Close pane");
    assert!(menu.contains("Move to tab 1"), "{menu:?}");
    assert!(menu.contains("Move to new tab"), "{menu:?}");
    // "Move to new tab" is the row after "Move to tab 1": a third Tab appears
    // in the bar without any further input.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 11,
            y: 13,
            kind: MouseKind::Click,
        }))
        .unwrap();
    let moved = read_until_render(&mut stream, &mut decoder, "\x1b[r\x1b[2J");
    assert!(!moved.contains("Move to"), "{moved:?}");
    stream
        .write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    assert_eq!(read_info(&mut stream, &mut decoder), Some((3, 3)));
    // Return to Tab 1 (the new Tab sits right after its source, Tab 2).
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::PrevWindow)))
        .unwrap();

    // Back in Tab 1, its last Pane offers Tabs 2 and 3 but no new Tab; moving
    // it closes the emptied Tab while every Pane survives.
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::PrevWindow)))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::RightClick,
        }))
        .unwrap();
    let menu = read_until_render(&mut stream, &mut decoder, "Close pane");
    assert!(menu.contains("Move to tab 2"), "{menu:?}");
    assert!(menu.contains("Move to tab 3"), "{menu:?}");
    assert!(!menu.contains("Move to new tab"), "{menu:?}");
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 11,
            y: 12,
            kind: MouseKind::Click,
        }))
        .unwrap();
    let moved = read_until_render(&mut stream, &mut decoder, "\x1b[r\x1b[2J");
    assert!(!moved.contains("Move to"), "{moved:?}");
    stream
        .write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    assert_eq!(read_info(&mut stream, &mut decoder), Some((2, 3)));

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
}

#[test]
fn dragging_a_divider_resizes_the_split() {
    isolate_state();
    let socket = temp_sock();
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 30"], 80, 24).unwrap();
        server.set_config(Config {
            sidebar: false,
            ..Config::default()
        });
        let _ = server.run(&mut poll);
    });
    wait_for(&socket);
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::Split(
            SplitAxis::LeftRight,
        ))))
        .unwrap();
    // 80 columns split evenly: the divider is drawn at client column 41.
    let frame = read_until_render(&mut stream, &mut decoder, ";41H\u{2502}");
    assert!(frame.contains(";41H\u{2502}"));

    // Press on the divider, drag it to column 21, release.
    for (x, kind) in [
        (41, MouseKind::Click),
        (30, MouseKind::Drag),
        (21, MouseKind::Drag),
        (21, MouseKind::Release),
    ] {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse { x, y: 10, kind }))
            .unwrap();
    }
    let frame = read_until_render(&mut stream, &mut decoder, ";21H\u{2502}");
    assert!(frame.contains(";21H\u{2502}"));
    assert!(!frame.contains(";41H\u{2502}"), "{frame:?}");
    stream
        .write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    assert_eq!(read_info(&mut stream, &mut decoder), Some((1, 2)));

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
}

#[test]
fn a_shell_exiting_in_a_moved_pane_closes_only_its_new_tab() {
    isolate_state();
    let socket = temp_sock();
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 80, 24).unwrap();
        server.set_config(Config {
            sidebar: false,
            ..Config::default()
        });
        let _ = server.run(&mut poll);
    });
    wait_for(&socket);
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::Split(
            SplitAxis::LeftRight,
        ))))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    assert_eq!(read_info(&mut stream, &mut decoder), Some((1, 2)));

    // Send the right-hand Pane to its own Tab: "Move to new tab" is the
    // seventh row when there is no other Tab to move to.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::RightClick,
        }))
        .unwrap();
    let menu = read_until_render(&mut stream, &mut decoder, "Close pane");
    assert!(menu.contains("Move to new tab"), "{menu:?}");
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 11,
            y: 12,
            kind: MouseKind::Click,
        }))
        .unwrap();
    let _ = read_until_render(&mut stream, &mut decoder, "\x1b[r\x1b[2J");
    stream
        .write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    assert_eq!(read_info(&mut stream, &mut decoder), Some((2, 2)));

    // The moved shell is the active Pane of the new Tab. Its exit must close
    // that Tab and nothing else: the Pane belongs to exactly one Tab.
    stream
        .write_all(&encode_frame(&ClientMessage::Input(b"exit\n".to_vec())))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut info = None;
    while Instant::now() < deadline {
        stream
            .write_all(&encode_frame(&ClientMessage::ListInfo))
            .unwrap();
        info = read_info(&mut stream, &mut decoder);
        if info == Some((1, 1)) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(info, Some((1, 1)));
    // The surviving Tab is healthy: its menu opens on a live Pane.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::RightClick,
        }))
        .unwrap();
    let menu = read_until_render(&mut stream, &mut decoder, "Close pane");
    assert!(!menu.contains("Move to"), "{menu:?}");

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
}
