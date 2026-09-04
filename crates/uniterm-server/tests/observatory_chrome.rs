//! Persistent chrome integration: Workspace/Tab/Observatory controls are
//! server-rendered, share their hit geometry, and remain usable when lists
//! overflow their viewport.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{
    encode_frame, ChromeAction, ChromeMenu, ClientMessage, Command, FrameDecoder, MouseKind,
    ServerMessage,
};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn temp_sock(tag: &str) -> std::path::PathBuf {
    let dir = common::socket_root().join(format!(
        "uniterm-observatory-chrome-{}-{tag}",
        std::process::id()
    ));
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
                if frame.contains("\x1b[r\x1b[2J") && predicate(&frame) {
                    return frame;
                }
            }
        }
    }
    panic!("matching full frame did not arrive");
}

fn read_menu(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
    expected: ChromeMenu,
) -> (u16, u16, u16, bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
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
            Err(error) => panic!("menu read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::OpenMenu {
                menu,
                x,
                y,
                width,
                open_up,
            } = message
            {
                assert_eq!(menu, expected);
                return (x, y, width, open_up);
            }
        }
    }
    panic!("{expected:?} menu request did not arrive");
}

fn read_window_count(stream: &mut UnixStream, decoder: &mut FrameDecoder) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buffer = [0u8; 16_384];
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => decoder.push(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("info read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::Info { windows, .. } = message {
                return windows;
            }
        }
    }
    panic!("window count did not arrive");
}

fn read_chrome_action(stream: &mut UnixStream, decoder: &mut FrameDecoder, expected: ChromeAction) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buffer = [0u8; 16_384];
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => decoder.push(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("action read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::OpenChromeAction { action } = message {
                assert_eq!(action, expected);
                return;
            }
        }
    }
    panic!("{expected:?} action did not arrive");
}

#[test]
fn chrome_buttons_open_anchored_menus_and_plus_survives_tab_overflow() {
    isolate_state();
    let socket = temp_sock("menus");
    // The Workspace button truncates its label to the Projects rail width, so
    // assert on the leading cells of the generated name rather than all of it.
    let workspace = socket.file_stem().unwrap().to_str().unwrap().to_string();
    let button_label = &workspace[..workspace.len().min(10)];
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 30"], 120, 20).unwrap();
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
            cols: 120,
            rows: 20,
        }))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let frame = read_full_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains("Files") && frame.contains("Servers")
    });
    assert!(!frame.contains("File manager"));
    assert!(!frame.contains("Web servers"));
    assert!(frame.contains(" PROJECTS"));
    assert!(frame.contains(" AGENTS"));
    assert!(!frame.contains(" Manage..."));
    assert!(frame.contains("New Task"));
    assert!(frame.contains("Tasks..."));
    assert!(frame.contains("Config"));
    assert!(frame.contains(" + "));
    assert!(
        frame.contains(button_label) && frame.contains("\u{25BE} "),
        "Workspace button must keep its label left and filled dropdown marker right"
    );
    assert!(
        !frame.contains(" Pane "),
        "obsolete Pane bar button remains"
    );
    let theme = uniterm_core::Theme::dark();
    let button_background = theme.accent_muted.sgr_bg();
    assert!(
        frame.matches(&button_background).count() >= 3,
        "secondary controls must use the muted accent"
    );
    assert!(
        frame.contains(&format!("\x1b[0;{};49mproject", theme.muted.sgr_fg())),
        "the Agents scope toggle must render as muted text"
    );
    assert!(
        frame.contains(&theme.status_active_bg.sgr_bg()),
        "the active center Tab must retain the full accent"
    );
    let footer_button = format!(
        "\x1b[0;1;{};{}m",
        theme.foreground.sgr_fg(),
        theme.status_bg.sgr_bg()
    );
    for (column, label) in [
        (86, "  New Task "),
        (98, "  Tasks... "),
        (110, "   Config  "),
    ] {
        assert!(
            frame.contains(&format!("\x1b[20;{column}H{footer_button}{label}")),
            "footer controls must use centered labels, the inactive Observatory tab background, and one-cell gaps"
        );
    }
    let rail_separator = format!(
        "\x1b[0;{};{}m\u{2502}",
        theme.status_bg.sgr_bg(),
        theme.divider.sgr_fg()
    );
    assert_eq!(
        frame.matches(&rail_separator).count(),
        2,
        "both sidebar dividers must continue through the status row"
    );
    let right_heading = format!(
        "\x1b[3;85H\x1b[0;{};49m\u{2502}\x1b[0;1;{};49m",
        theme.divider.sgr_fg(),
        theme.muted.sgr_fg()
    );
    assert!(frame.contains(&format!("{right_heading} AGENTS")));

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 110,
            y: 1,
            kind: MouseKind::Click,
        }))
        .unwrap();
    read_full_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains(&format!("{right_heading} WEB SERVERS"))
    });
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 87,
            y: 1,
            kind: MouseKind::Click,
        }))
        .unwrap();
    read_full_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains(&format!("{right_heading} AGENTS"))
    });

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 2,
            y: 1,
            kind: MouseKind::Click,
        }))
        .unwrap();
    assert_eq!(
        read_menu(&mut stream, &mut decoder, ChromeMenu::Workspace),
        (1, 1, 23, false)
    );

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 2,
            y: 5,
            kind: MouseKind::RightClick,
        }))
        .unwrap();
    assert_eq!(
        read_menu(
            &mut stream,
            &mut decoder,
            ChromeMenu::Project(uniterm_core::ProjectId(1)),
        ),
        (1, 5, 23, false)
    );

    // The blank separator after the pair is sidebar background, so it opens
    // the empty-rail menu rather than accidentally targeting either Project.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 2,
            y: 9,
            kind: MouseKind::RightClick,
        }))
        .unwrap();
    assert_eq!(
        read_menu(&mut stream, &mut decoder, ChromeMenu::Projects),
        (1, 9, 23, false)
    );

    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::Observatory)))
        .unwrap();
    read_full_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains(" + ") && !frame.contains("Files")
    });
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::Observatory)))
        .unwrap();
    read_full_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains("Files") && frame.contains(" AGENTS")
    });

    // These were button cells before the gaps were introduced. They must not
    // emit an action, so the following valid click remains the next message.
    for x in [97, 109] {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse {
                x,
                y: 20,
                kind: MouseKind::Click,
            }))
            .unwrap();
    }

    for (x, action) in [
        (90, ChromeAction::NewTask),
        (102, ChromeAction::Tasks),
        (115, ChromeAction::Config),
    ] {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse {
                x,
                y: 20,
                kind: MouseKind::Click,
            }))
            .unwrap();
        read_chrome_action(&mut stream, &mut decoder, action);
    }

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 26,
            y: 1,
            kind: MouseKind::RightClick,
        }))
        .unwrap();
    let tab_menu = read_menu(&mut stream, &mut decoder, ChromeMenu::Tabs);
    assert_eq!(tab_menu.1, 1);
    assert!(!tab_menu.3);

    // The first `+` trails the one fitting Tab. Later, overflow fixes it at
    // the right edge of the center bar, immediately after the right control.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 34,
            y: 1,
            kind: MouseKind::Click,
        }))
        .unwrap();
    for _ in 0..8 {
        stream
            .write_all(&encode_frame(&ClientMessage::Command(Command::NewWindow)))
            .unwrap();
    }
    let overflow = read_full_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains(" < ") && frame.contains(" > ") && frame.contains(" + ")
    });
    assert!(overflow.contains(" + "));
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 83,
            y: 1,
            kind: MouseKind::Click,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    assert_eq!(read_window_count(&mut stream, &mut decoder), 11);

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(socket.parent().unwrap());
}

#[test]
fn projects_sidebar_wheel_reaches_items_beyond_the_viewport() {
    isolate_state();
    let socket = temp_sock("projects");
    let root = socket.parent().unwrap().to_path_buf();
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 30"], 120, 12).unwrap();
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
            cols: 120,
            rows: 12,
        }))
        .unwrap();
    for index in 2..=6 {
        stream
            .write_all(&encode_frame(&ClientMessage::ProjectCreate {
                name: format!("Project{index}"),
                root: root.to_string_lossy().into_owned(),
            }))
            .unwrap();
    }
    let mut decoder = FrameDecoder::new();
    read_full_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains("PROJECTS 5-6/6") && frame.contains("Project6")
    });

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 2,
            y: 6,
            kind: MouseKind::WheelUp,
        }))
        .unwrap();
    let scrolled = read_full_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains("PROJECTS 4-5/6") && frame.contains("Project4")
    });
    assert!(scrolled.contains("Project4"));
    assert!(!scrolled.contains("\u{2584}"));

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(root);
}
