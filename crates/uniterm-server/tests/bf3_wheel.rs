//! BF3 integration: the mouse wheel scrolls scrollback via copy-mode.
//!
//! Wheel-up over a pane with history must open the copy-mode viewport (the
//! `[COPY` indicator appears in the render ops); wheel-down back at the live
//! bottom must leave copy-mode again.

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
    let dir = common::socket_root().join(format!("uniterm-it-{}-{tag}", std::process::id()));
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

/// Read render ops until `pred` matches (or the deadline passes); returns the
/// accumulated ops text since the call.
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

fn latest_button_position(render: &str) -> Option<(u16, u16)> {
    let label = render.rfind("[v Latest]")?;
    let before = &render[..label];
    let cup_end = before.rfind('H')?;
    let cup_start = before[..cup_end].rfind("\x1b[")? + 2;
    let (row, col) = before[cup_start..cup_end].split_once(';')?;
    Some((col.parse().ok()?, row.parse().ok()?))
}

/// Directly launched full-screen apps keep the pane child's process group.
/// Their first output must not be mistaken for a return to the shell.
fn direct_app_wheel(modes: &str, up: &str, down: &str) {
    isolate_state();
    let sock = temp_sock("wheel-direct-app");
    let sock_srv = sock.clone();
    let script =
        format!("stty -echo -icanon; printf '\\033[?1049h{modes}DIRECT-APP-READY'; exec cat -v");
    let server = thread::spawn(move || {
        let (mut s, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", &script], 80, 24).unwrap();
        s.set_config(Config {
            sidebar: false,
            ..Config::default()
        });
        let _ = s.run(&mut poll);
    });
    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        }))
        .unwrap();
    let mut dec = FrameDecoder::new();
    let initial = read_until(&mut stream, &mut dec, 3, |s| s.contains("DIRECT-APP-READY"));
    for kind in [MouseKind::WheelUp, MouseKind::WheelDown] {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse { x: 12, y: 7, kind }))
            .unwrap();
    }
    let got = read_until(&mut stream, &mut dec, 3, |s| s.contains(down));
    stream
        .write_all(&encode_frame(&ClientMessage::Input(
            b"TYPING-STILL-WORKS".to_vec(),
        )))
        .unwrap();
    let typed = read_until(&mut stream, &mut dec, 3, |s| {
        s.contains("TYPING-STILL-WORKS")
    });
    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();
    assert!(
        initial.contains("DIRECT-APP-READY"),
        "first attach lost the application's alternate screen: {initial:?}"
    );
    assert!(got.contains(up), "wheel-up never reached the app: {got:?}");
    assert!(
        got.contains(down),
        "wheel-down never reached the app: {got:?}"
    );
    assert!(
        !got.contains("[COPY"),
        "wheel opened Uniterm history: {got:?}"
    );
    assert!(
        typed.contains("TYPING-STILL-WORKS"),
        "typing was lost after scrolling: {typed:?}"
    );
}

#[test]
fn wheel_reaches_direct_alt_screen_app_as_arrows() {
    direct_app_wheel("", "^[[A^[[A^[[A", "^[[B^[[B^[[B");
}

#[test]
fn wheel_reaches_direct_alt_screen_app_as_application_arrows() {
    direct_app_wheel("\\033[?1h", "^[OA^[OA^[OA", "^[OB^[OB^[OB");
}

#[test]
fn wheel_reaches_direct_mouse_app_as_pane_relative_reports() {
    direct_app_wheel("\\033[?1000h\\033[?1006h", "^[[<64;12;6M", "^[[<65;12;6M");
}

fn agent_start(agent: &str) -> String {
    format!("\\033]777;notify;uniterm://cli-agent;{{\"agent\":\"{agent}\",\"event\":\"session_start\"}}\\007")
}

#[test]
fn claude_and_codex_alt_screen_wheels_and_typing_reach_the_app() {
    // Replay the provider-neutral terminal contract with each agent bound.
    // Agent identity must not change routing for any supported mouse mode.
    for agent in ["claude", "codex"] {
        direct_app_wheel(&agent_start(agent), "^[[A^[[A^[[A", "^[[B^[[B^[[B");
        for mode in [1000, 1002, 1003] {
            direct_app_wheel(
                &format!("{}\\033[?{mode}h\\033[?1006h", agent_start(agent)),
                "^[[<64;12;6M",
                "^[[<65;12;6M",
            );
        }
    }
}

#[test]
fn wheel_returns_to_shell_history_after_a_stranded_foreground_app() {
    isolate_state();
    let sock = temp_sock("wheel-shell-return");
    let sock_srv = sock.clone();
    // Input gates ensure the server observes the shell and foreground job
    // separately. The job exits without restoring the primary screen.
    let script = "seq 1 60; printf SHELL-READY; read -r go; set -m; \
                  sh -c 'printf \"\\033[?1049hAPP-READY\"; read -r go'; \
                  printf SHELL-RETURNED; read -r go";
    let server = thread::spawn(move || {
        let (mut s, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", script], 80, 24).unwrap();
        s.set_config(Config {
            sidebar: false,
            ..Config::default()
        });
        let _ = s.run(&mut poll);
    });
    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        }))
        .unwrap();
    let mut dec = FrameDecoder::new();
    let initial = read_until(&mut stream, &mut dec, 3, |s| s.contains("SHELL-READY"));
    stream
        .write_all(&encode_frame(&ClientMessage::Input(b"\n".to_vec())))
        .unwrap();
    let app = read_until(&mut stream, &mut dec, 3, |s| s.contains("APP-READY"));
    stream
        .write_all(&encode_frame(&ClientMessage::Input(b"\n".to_vec())))
        .unwrap();
    // Recovery restores the saved primary screen, including SHELL-READY.
    let returned = read_until(&mut stream, &mut dec, 3, |s| s.contains("SHELL-READY"));
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 12,
            y: 7,
            kind: MouseKind::WheelUp,
        }))
        .unwrap();
    let history = read_until(&mut stream, &mut dec, 3, |s| s.contains("[COPY"));
    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();
    assert!(initial.contains("SHELL-READY"), "{initial:?}");
    assert!(app.contains("APP-READY"), "{app:?}");
    assert!(
        returned.contains("SHELL-READY"),
        "shell screen was not recovered: {returned:?}"
    );
    assert!(
        history.contains("[COPY"),
        "shell history was not restored: {history:?}"
    );
}

#[test]
#[ignore = "requires nvim on PATH; run explicitly for the live editor reproduction"]
fn wheel_scrolls_inside_neovim() {
    isolate_state();
    let dir = common::temp_dir("wheel-nvim");
    let file = dir.join("lines.txt");
    std::fs::write(&file, "editor text\n".repeat(200)).unwrap();
    let sock = temp_sock("wheel-nvim");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut s, mut poll) = Server::bind(
            &sock_srv,
            "/usr/bin/env",
            &[
                &format!("NVIM_LOG_FILE={}", dir.join("nvim.log").display()),
                "nvim",
                "--clean",
                "-n",
                "-i",
                "NONE",
                "-c",
                "set mouse= laststatus=2 statusline=EDITOR-LINE-%l",
                "-c",
                "normal! 50G",
                file.to_str().unwrap(),
            ],
            80,
            24,
        )
        .unwrap();
        s.set_config(Config {
            sidebar: false,
            ..Config::default()
        });
        let _ = s.run(&mut poll);
    });
    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        }))
        .unwrap();
    let mut dec = FrameDecoder::new();
    let mut screen = uniterm_server::Terminal::new(80, 24);
    let mut read_screen = |stream: &mut UnixStream, expected: &str| {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 16384];
        while Instant::now() < deadline {
            if let Ok(n) = stream.read(&mut buf) {
                dec.push(&buf[..n]);
                while let Ok(Some(msg)) = dec.decode::<ServerMessage>() {
                    if let ServerMessage::RenderOps(ops) = msg {
                        screen.feed(&ops);
                    }
                }
            }
            let text = screen.evidence_text(24);
            if text.contains(expected) {
                return text;
            }
        }
        screen.evidence_text(24)
    };
    let initial = read_screen(&mut stream, "EDITOR-LINE-50");
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 12,
            y: 7,
            kind: MouseKind::WheelDown,
        }))
        .unwrap();
    let down = read_screen(&mut stream, "EDITOR-LINE-53");
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 12,
            y: 7,
            kind: MouseKind::WheelUp,
        }))
        .unwrap();
    let up = read_screen(&mut stream, "EDITOR-LINE-50");
    stream
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    server.join().unwrap();
    assert!(initial.contains("EDITOR-LINE-50"), "{initial}");
    assert!(
        down.contains("EDITOR-LINE-53"),
        "wheel-down did not move inside Neovim: {down}"
    );
    assert!(
        up.contains("EDITOR-LINE-50"),
        "wheel-up did not move inside Neovim: {up}"
    );
    assert!(!up.contains("[COPY"));
}

#[test]
fn wheel_scrolls_scrollback_and_returns_to_live() {
    scrollback_round_trip("");
}

#[test]
fn claude_and_codex_inline_sessions_keep_scrollback_and_return_to_live() {
    for agent in ["claude", "codex"] {
        scrollback_round_trip(&agent_start(agent));
    }
}

fn scrollback_round_trip(prefix: &str) {
    isolate_state();
    let sock = temp_sock("wheel");
    let sock_srv = sock.clone();
    // Emit 60 numbered lines into a 24-row pane so real history exists.
    let script = format!("printf '{prefix}'; seq 1 60; echo HISTORY-READY; sleep 30");
    let server = thread::spawn(move || {
        let (mut s, mut poll) =
            Server::bind(&sock_srv, "/bin/sh", &["-c", &script], 80, 24).unwrap();
        s.set_config(Config {
            sidebar: false,
            ..Config::default()
        });
        let _ = s.run(&mut poll);
    });

    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
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

    // Wait for the pane output to arrive (the tail of seq).
    // Wait for the marker line, not for "60": a cursor-position sequence can
    // contain those digits long before the sixtieth line exists, and copy-mode
    // needs the whole history in place before the wheel starts scrolling.
    let pre = read_until(&mut stream, &mut dec, 10, |s| s.contains("HISTORY-READY"));
    assert!(pre.contains("HISTORY-READY"), "pane output never arrived");

    // Wheel-down at the live bottom is a no-op, including in bound agents.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::WheelDown,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Refresh))
        .unwrap();
    let unchanged = read_until(&mut stream, &mut dec, 3, |s| s.contains("HISTORY-READY"));
    assert!(
        unchanged.contains("HISTORY-READY"),
        "live output changed: {unchanged:?}"
    );
    assert!(
        !unchanged.contains("[COPY"),
        "wheel-down opened history: {unchanged:?}"
    );

    // Wheel-up over the pane: copy-mode opens, scrolled into history.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::WheelUp,
        }))
        .unwrap();
    let scrolled = read_until(&mut stream, &mut dec, 10, |s| s.contains("[COPY"));
    assert!(
        scrolled.contains("[COPY"),
        "wheel-up did not open the copy-mode viewport"
    );

    // Once the viewport is over one page behind, the Latest button appears
    // beside (without replacing) the current-line/total-lines indicator.
    for _ in 0..9 {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse {
                x: 10,
                y: 5,
                kind: MouseKind::WheelUp,
            }))
            .unwrap();
    }
    let far_back = read_until(&mut stream, &mut dec, 10, |s| s.contains("[v Latest]"));
    assert!(
        far_back.contains("[COPY"),
        "line-count indicator disappeared"
    );
    let (button_x, button_y) =
        latest_button_position(&far_back).expect("Latest button position in render ops");
    for kind in [MouseKind::Click, MouseKind::Release] {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse {
                x: button_x,
                y: button_y,
                kind,
            }))
            .unwrap();
    }
    stream
        .write_all(&encode_frame(&ClientMessage::Refresh))
        .unwrap();
    let latest = read_until(&mut stream, &mut dec, 3, |s| {
        s.rsplit("\x1b[r\x1b[2J")
            .next()
            .is_some_and(|last| !last.is_empty() && !last.contains("[COPY"))
    });
    let last_frame = latest.rsplit("\x1b[r\x1b[2J").next().unwrap_or("");
    assert!(
        !last_frame.contains("[COPY"),
        "Latest button did not resume live output"
    );

    // Re-enter copy-mode so the wheel-down path remains covered too.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::WheelUp,
        }))
        .unwrap();
    let scrolled = read_until(&mut stream, &mut dec, 3, |s| s.contains("[COPY"));
    assert!(scrolled.contains("[COPY"));

    // Wheel-down enough to hit the bottom: copy-mode exits (no more [COPY in
    // the final full frame).
    for _ in 0..20 {
        stream
            .write_all(&encode_frame(&ClientMessage::Mouse {
                x: 10,
                y: 5,
                kind: MouseKind::WheelDown,
            }))
            .unwrap();
    }
    // Queue a full-frame barrier after the wheel burst so the assertion does
    // not depend on how the socket batches the copy-mode and live repaints.
    stream
        .write_all(&encode_frame(&ClientMessage::Refresh))
        .unwrap();
    let back = read_until(&mut stream, &mut dec, 3, |s| {
        // The last full frame after leaving copy-mode has no indicator.
        s.rsplit("\x1b[r\x1b[2J")
            .next()
            .map(|last| !last.is_empty() && !last.contains("[COPY"))
            .unwrap_or(false)
    });
    let last_frame = back.rsplit("\x1b[r\x1b[2J").next().unwrap_or("");
    assert!(
        !last_frame.contains("[COPY"),
        "wheel-down at the bottom did not leave copy-mode"
    );

    let _ = stream.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = server.join();
}

#[test]
fn wheel_scrolls_history_emitted_by_an_inline_tui_region() {
    isolate_state();
    let sock = temp_sock("wheel-inline-region");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let (mut s, mut poll) = Server::bind(
            &sock_srv,
            "/bin/sh",
            &[
                "-c",
                "printf '\\033[1;1HA\\033[2;1HB\\033[3;1HC\\033[4;1HD\\033[1;3r\\033[3;1H\\n'; sleep 5",
            ],
            80,
            24,
        )
        .unwrap();
        s.set_config(Config {
            sidebar: false,
            ..Config::default()
        });
        let _ = s.run(&mut poll);
    });

    wait_for(&sock);
    let mut stream = UnixStream::connect(&sock).unwrap();
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

    let pre = read_until(&mut stream, &mut dec, 3, |s| s.contains('D'));
    assert!(pre.contains('D'), "inline TUI output never arrived");

    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 10,
            y: 5,
            kind: MouseKind::WheelUp,
        }))
        .unwrap();
    let scrolled = read_until(&mut stream, &mut dec, 3, |s| s.contains("[COPY"));
    assert!(
        scrolled.contains("[COPY"),
        "wheel-up could not reach inline TUI history"
    );

    let _ = stream.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = server.join();
}
