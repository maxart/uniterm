//! File-sidebar mouse hit testing must follow optional header rows exactly.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_core::Config;
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, MouseKind, ServerMessage};
use uniterm_server::Server;

mod common;

use common::{isolate_state, unique_workspace_name};

fn wait_for(path: &std::path::Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("server socket never appeared at {}", path.display());
}

fn read_frame_until(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
    predicate: impl Fn(&str) -> bool,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
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
                // Chrome (the Observatory file rows and Git summary) now
                // updates as a damage frame with no screen clear; match any
                // frame the predicate accepts, not only a full-clear frame.
                if predicate(&frame) {
                    return frame;
                }
            }
        }
    }
    panic!("matching frame did not arrive");
}

#[test]
fn git_summary_does_not_shift_file_clicks_to_the_next_row() {
    isolate_state();
    let base =
        common::socket_root().join(format!("uniterm-file-sidebar-mouse-{}", std::process::id()));
    let root = base.join("Project");
    std::fs::create_dir_all(&root).unwrap();
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&root)
        .status()
        .unwrap()
        .success());
    for name in [
        "file0.txt",
        "file1.txt",
        "file2.txt",
        "file3.txt",
        "file4.txt",
        "file5.txt",
        "file6.txt",
        "file7.txt",
    ] {
        std::fs::write(root.join(name), format!("{name}\n")).unwrap();
    }

    let socket = base.join(format!("{}.sock", unique_workspace_name()));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 30"], 120, 12).unwrap();
        server.set_config(Config {
            sidebar: false,
            file_sidebar: true,
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
            cols: 120,
            rows: 12,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Command(
            uniterm_proto::Command::FileSidebarToggle,
        )))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::ProjectCreate {
            name: "MouseProject".into(),
            root: root.to_string_lossy().into_owned(),
        }))
        .unwrap();

    let mut decoder = FrameDecoder::new();
    let theme = uniterm_core::Theme::dark();
    let file_heading = format!(
        "\x1b[3;85H\x1b[0;{};49m\u{2502}\x1b[0;1;{};49m FILES",
        theme.divider.sgr_fg(),
        theme.muted.sgr_fg()
    );
    read_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains(&file_heading)
            && frame.contains(" Git")
            && frame.contains("file0.txt")
            && frame.contains("file2.txt")
    });

    // The matching Observatory margins and Git summary leave three visible
    // file rows. Move the selection beyond that first viewport so its stable
    // origin advances without changing click-to-row geometry.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 100,
            y: 8,
            kind: MouseKind::Click,
        }))
        .unwrap();
    for _ in 0..4 {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse {
                x: 100,
                y: 8,
                kind: MouseKind::WheelDown,
            }))
            .unwrap();
    }
    let selected_file5 = format!(
        "\x1b[0;{};{}m   file5.txt",
        theme.selection_bg.sgr_bg(),
        theme.status_active_fg.sgr_fg()
    );
    read_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains(&selected_file5)
    });

    // file3 is now the first visible row, at 1-based terminal row 7. Clicking
    // it must leave the viewport anchored and keep the highlight on row 7.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 100,
            y: 7,
            kind: MouseKind::Click,
        }))
        .unwrap();
    let border = format!("\x1b[0;{};49m", theme.divider.sgr_fg());
    let selected_file3_at_clicked_row = format!(
        "\x1b[7;85H{border}\u{2502}\x1b[0;{};{}m   file3.txt",
        theme.selection_bg.sgr_bg(),
        theme.status_active_fg.sgr_fg()
    );
    let selected = read_frame_until(&mut stream, &mut decoder, |frame| {
        frame.contains(&selected_file3_at_clicked_row)
    });
    assert!(selected.contains(&selected_file3_at_clicked_row));

    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(base);
}
