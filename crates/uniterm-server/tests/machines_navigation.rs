//! Machine navigation is client-owned and shares footer/control semantics.
mod common;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use uniterm_proto::{encode_frame, ClientMessage, Command, FrameDecoder, MouseKind, ServerMessage};
use uniterm_server::{Server, Terminal};

fn until(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
    screen: &mut Terminal,
    mut done: impl FnMut(&ServerMessage, &Terminal) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bytes = [0; 32768];
    while Instant::now() < deadline {
        while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            assert!(
                !matches!(&message, ServerMessage::Overwatch { open: true, .. }),
                "removed footer opened Overwatch"
            );
            if let ServerMessage::RenderOps(ops) = &message {
                screen.feed(ops);
            }
            if done(&message, screen) {
                return;
            }
        }
        match stream.read(&mut bytes) {
            Ok(0) => panic!("unexpected close"),
            Ok(n) => decoder.push(&bytes[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("{error}"),
        }
    }
    panic!("missing navigation response: {}", screen.dump_text());
}
#[test]
fn shared_navigation_tabs_match_observatory_styles_and_route_without_network() {
    common::isolate_state();
    let root = common::socket_root().join(format!("machine-nav-{}", common::unique_nonce()));
    std::fs::create_dir_all(&root).unwrap();
    let socket = root.join(format!("{}.sock", common::unique_workspace_name()));
    let server_socket = socket.clone();
    let worker = std::thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(
            &server_socket,
            "/bin/sh",
            &["-c", "printf MACHINE_READY; exec /bin/sh"],
            100,
            30,
        )
        .unwrap();
        server.set_config(uniterm_core::Config {
            sidebar: true,
            sidebar_width: 16,
            file_sidebar: true,
            file_sidebar_width: 30,
            status_position: uniterm_core::StatusPosition::Top,
            ..uniterm_core::Config::default()
        });
        server.run(&mut poll).unwrap();
    });
    common::wait_for_socket(&socket);
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let mut screen = Terminal::new(100, 30);
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm".into(),
            cols: 100,
            rows: 30,
        }))
        .unwrap();
    until(&mut stream, &mut decoder, &mut screen, |_, screen| {
        screen.dump_text().contains("MACHINE_READY")
            && screen.dump_text().contains(" Local   Serv… ")
    });
    assert!(!screen.dump_text().contains("Overwatch"));
    let theme = uniterm_core::Config::default().theme;
    for (left_x, right_x, selected) in [(0, 71, true), (8, 81, false)] {
        let left = screen.grid().get(left_x, 29);
        let right = screen.grid().get(right_x, 0);
        assert_eq!(
            (left.fg, left.bg, left.attrs),
            (right.fg, right.bg, right.attrs)
        );
        assert_eq!(
            left.bg,
            if selected {
                theme.accent_muted
            } else {
                theme.status_bg
            }
        );
        assert_eq!(
            left.attrs,
            if selected {
                uniterm_core::Attrs::BOLD
            } else {
                uniterm_core::Attrs::NONE
            }
        );
    }
    assert_eq!(screen.grid().get(15, 29).ch, '│');
    // The former Overwatch row is now Local/Servers, including padded cells.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 12,
            y: 30,
            kind: MouseKind::Click,
        }))
        .unwrap();
    until(&mut stream, &mut decoder, &mut screen, |message, _| {
        matches!(message, ServerMessage::MachineNavigation { local: false })
    });
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 3,
            y: 30,
            kind: MouseKind::Click,
        }))
        .unwrap();
    until(&mut stream, &mut decoder, &mut screen, |message, _| {
        matches!(message, ServerMessage::MachineNavigation { local: true })
    });
    stream
        .write_all(&encode_frame(&ClientMessage::Resize { cols: 72, rows: 30 }))
        .unwrap();
    screen = Terminal::new(72, 30);
    until(&mut stream, &mut decoder, &mut screen, |_, screen| {
        screen.dump_text().contains(" Local   Serv… ")
    });
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 12,
            y: 30,
            kind: MouseKind::Click,
        }))
        .unwrap();
    until(&mut stream, &mut decoder, &mut screen, |message, _| {
        matches!(message, ServerMessage::MachineNavigation { local: false })
    });
    assert_eq!(screen.grid().get(15, 29).ch, '│');
    assert!(!screen.dump_text().contains("Overwatch"));
    // Both segment boundaries are clickable at the narrow sidebar width.
    for (x, expected_local) in [(1, true), (8, true), (9, false), (15, false)] {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse {
                x,
                y: 30,
                kind: MouseKind::Click,
            }))
            .unwrap();
        until(
            &mut stream,
            &mut decoder,
            &mut screen,
            |message, _| matches!(message, ServerMessage::MachineNavigation { local } if *local == expected_local),
        );
    }
    // Blank former footer space and the divider must not open controls.
    for (x, y) in [(3, 29), (16, 30)] {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse {
                x,
                y,
                kind: MouseKind::Click,
            }))
            .unwrap();
    }
    stream
        .write_all(&encode_frame(&ClientMessage::WorkspaceState))
        .unwrap();
    until(&mut stream, &mut decoder, &mut screen, |message, _| {
        assert!(
            !matches!(message, ServerMessage::MachineNavigation { .. }),
            "non-tab cell navigated"
        );
        matches!(message, ServerMessage::Workspace { .. })
    });
    stream
        .write_all(&encode_frame(&ClientMessage::Command(Command::Servers)))
        .unwrap();
    until(&mut stream, &mut decoder, &mut screen, |message, _| {
        matches!(message, ServerMessage::MachineNavigation { local: false })
    });
    // At this height the reclaimed row allows a second complete Project card.
    stream
        .write_all(&encode_frame(&ClientMessage::ProjectCreate {
            name: "Reclaimed".into(),
            root: root.to_string_lossy().into_owned(),
        }))
        .unwrap();
    until(&mut stream, &mut decoder, &mut screen, |message, _| {
        if let ServerMessage::ProjectCreated { error } = message {
            assert!(error.is_none(), "{error:?}");
            true
        } else {
            false
        }
    });
    stream
        .write_all(&encode_frame(&ClientMessage::Resize { cols: 72, rows: 11 }))
        .unwrap();
    screen = Terminal::new(72, 11);
    until(&mut stream, &mut decoder, &mut screen, |_, screen| {
        screen.grid().get(3, 7).ch == 'R' && screen.dump_text().contains(" Local   Serv… ")
    });
    assert_eq!(screen.grid().get(0, 9).ch, '▄');
    assert_eq!(screen.grid().get(0, 9).fg, theme.selection_bg);
    assert_eq!(screen.grid().get(0, 9).bg, uniterm_core::Color::Default);
    assert_eq!(screen.grid().get(0, 9).attrs, uniterm_core::Attrs::REVERSE);
    assert_eq!(screen.grid().get(0, 10).bg, theme.accent_muted);
    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    worker.join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn idle_today_full_repaint_updates_graph_tab_and_hit_geometry_on_settings_toggle() {
    common::isolate_state();
    let root = common::socket_root().join(format!("today-tabs-{}", common::unique_nonce()));
    std::fs::create_dir_all(&root).unwrap();
    let socket = root.join(format!("{}.sock", common::unique_workspace_name()));
    let server_socket = socket.clone();
    let worker = std::thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(
            &server_socket,
            "/bin/sh",
            &["-c", "printf TODAY_READY; exec /bin/sh"],
            100,
            30,
        )
        .unwrap();
        server.set_config(uniterm_core::Config {
            sidebar: false,
            file_sidebar: false,
            run_graph: false,
            status_position: uniterm_core::StatusPosition::Top,
            ..uniterm_core::Config::default()
        });
        server.run(&mut poll).unwrap();
    });
    common::wait_for_socket(&socket);
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let mut screen = Terminal::new(100, 30);
    let send =
        |stream: &mut UnixStream, message| stream.write_all(&encode_frame(&message)).unwrap();
    send(
        &mut stream,
        ClientMessage::Attach {
            term: "xterm".into(),
            cols: 100,
            rows: 30,
        },
    );
    until(&mut stream, &mut decoder, &mut screen, |_, screen| {
        screen.dump_text().contains("TODAY_READY")
    });
    send(&mut stream, ClientMessage::Command(Command::Today));
    until(&mut stream, &mut decoder, &mut screen, |_, screen| {
        screen.dump_text().contains("TODAY  /  Workspace activity")
            && screen.dump_text().contains("Enter inspect")
    });
    let theme = uniterm_core::Config::default().theme;
    for mode in [b'1', b'2'] {
        send(&mut stream, ClientMessage::Input(vec![mode]));
        send(&mut stream, ClientMessage::PaneList);
        until(&mut stream, &mut decoder, &mut screen, |message, _| {
            matches!(message, ServerMessage::Panes { .. })
        });
        for enabled in [true, false] {
            send(
                &mut stream,
                ClientMessage::SettingsApply(uniterm_proto::SettingsPatch {
                    run_graph: Some(enabled),
                    ..uniterm_proto::SettingsPatch::default()
                }),
            );
            // Check the very first repaint from Settings, before any navigation
            // or timeline event can repair stale cached presentation state.
            until(&mut stream, &mut decoder, &mut screen, |message, screen| {
                let ServerMessage::RenderOps(ops) = message else {
                    return false;
                };
                assert!(ops.starts_with(b"\x1b[r\x1b[2J"));
                assert!(screen.dump_text().contains("TODAY  /  Workspace activity"));
                assert_eq!(
                    screen.dump_text().contains("3 Graph"),
                    enabled,
                    "first full repaint must use the current config"
                );
                true
            });
            // The same right-hand cells select Graph when enabled, Flow when
            // disabled. This also catches stale invisible Graph hit rectangles.
            send(
                &mut stream,
                ClientMessage::Mouse {
                    x: 80,
                    y: 5,
                    kind: MouseKind::Click,
                },
            );
            send(&mut stream, ClientMessage::PaneList);
            until(&mut stream, &mut decoder, &mut screen, |message, _| {
                matches!(message, ServerMessage::Panes { .. })
            });
            assert_eq!(screen.dump_text().contains("AGENT GRAPH"), enabled);
            assert_eq!(screen.grid().get(79, 4).bg, theme.accent_muted);
            assert_eq!(screen.grid().get(79, 4).attrs, uniterm_core::Attrs::BOLD);
            send(&mut stream, ClientMessage::Input(vec![mode]));
            send(&mut stream, ClientMessage::PaneList);
            until(&mut stream, &mut decoder, &mut screen, |message, _| {
                matches!(message, ServerMessage::Panes { .. })
            });
            assert!(!screen.dump_text().contains("AGENT GRAPH"));
        }
    }
    send(&mut stream, ClientMessage::KillServer);
    worker.join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
