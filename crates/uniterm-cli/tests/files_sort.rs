//! Files order is one semantic command with a durable Workspace preference.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};
use uniterm_proto::{
    encode_frame, ClientMessage, FileSort, FrameDecoder, MouseKind, ServerMessage,
};
use uniterm_server::terminal::Terminal;
mod common;

fn screen_until(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
    screen: &mut Terminal,
    predicate: impl Fn(&Terminal) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bytes = [0; 65536];
    while Instant::now() < deadline {
        match stream.read(&mut bytes) {
            Ok(0) => break,
            Ok(n) => decoder.push(&bytes[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => panic!("{e}"),
        }
        while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            if let ServerMessage::RenderOps(ops) = message {
                screen.feed(&ops);
            }
        }
        if predicate(screen) {
            return;
        }
    }
    panic!("screen did not settle: {}", screen.evidence_text(30));
}

fn row(screen: &Terminal, y: u16) -> String {
    (85..120)
        .map(|x| screen.grid().cell_text_owned(screen.grid().get(x, y)))
        .collect()
}

#[test]
fn files_sort_click_cli_restore_and_sidebar_title_styles_agree() {
    let state = common::isolate_state();
    let base = common::temp_dir("files-sort");
    let runtime = common::socket_root().join(format!("ut-fsort-{}", std::process::id()));
    let config = base.join("config");
    let project = base.join("project");
    for path in [&runtime, &config, &project] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::create_dir(project.join("z-folder")).unwrap();
    for (name, seconds) in [("a-old", 100), ("b-new", 200)] {
        let file = std::fs::File::create(project.join(name)).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
        )
        .unwrap();
    }
    let name = common::unique_workspace_name();
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_ut"))
            .args(args)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", &config)
            .env("SHELL", "/bin/sh")
            .env_remove("UNITERM")
            .env_remove("UNITERM_SOCKET")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&["workspace", "new", "-d", &name]);
    run(&[
        "project",
        "add",
        "Sort",
        project.to_str().unwrap(),
        "--workspace",
        &name,
    ]);
    let socket = runtime.join("uniterm").join(format!("{name}.sock"));
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 120,
            rows: 20,
        }))
        .unwrap();
    stream
        .write_all(&encode_frame(&ClientMessage::Command(
            uniterm_proto::Command::FileSidebarToggle,
        )))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let mut screen = Terminal::new(120, 20);
    screen_until(&mut stream, &mut decoder, &mut screen, |s| {
        row(s, 2).contains("By name") && row(s, 5).contains("a-old")
    });
    assert!(row(&screen, 4).contains("z-folder"));
    let left = screen.grid().get(1, 2);
    let right = screen.grid().get(86, 2);
    assert_eq!(left.fg, right.fg);
    assert_eq!(left.attrs, right.attrs);
    assert_eq!(left.attrs, uniterm_core::Attrs::BOLD);
    // Coordinates on the wire are 1-based, matching terminal reports.
    stream
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 115,
            y: 3,
            kind: MouseKind::Click,
        }))
        .unwrap();
    screen_until(&mut stream, &mut decoder, &mut screen, |s| {
        row(s, 2).contains("By recency") && row(s, 5).contains("b-new")
    });
    assert!(row(&screen, 4).contains("z-folder"));
    run(&["files", "sort", "name", "--workspace", &name]);
    screen_until(&mut stream, &mut decoder, &mut screen, |s| {
        row(s, 2).contains("By name") && row(s, 5).contains("a-old")
    });
    // The shell's first prompt and asynchronous sidebar updates can follow the
    // requested sort frame. Establish a quiet boundary before testing the no-op.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bytes = [0; 65536];
    loop {
        assert!(Instant::now() < deadline, "startup output never settled");
        match stream.read(&mut bytes) {
            Ok(0) => panic!("server disconnected before the no-op check"),
            Ok(n) => decoder.push(&bytes[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(error) => panic!("{error}"),
        }
        while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            if let ServerMessage::RenderOps(ops) = message {
                screen.feed(&ops);
            }
        }
    }
    // An identical command must render nothing after startup has settled.
    run(&["files", "sort", "name", "--workspace", &name]);
    let mut bytes = [0; 65536];
    if let Ok(n) = stream.read(&mut bytes) {
        decoder.push(&bytes[..n]);
    }
    while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
        if let ServerMessage::RenderOps(ops) = message {
            assert!(ops.is_empty(), "unchanged order repainted");
        }
    }
    run(&["files", "sort", "recency", "--workspace", &name]);
    run(&["workspace", "stop", &name]);
    assert_eq!(
        uniterm_server::workspace_catalog::load(&name)
            .unwrap()
            .file_sort,
        FileSort::Recency
    );
    run(&["workspace", "new", "-d", &name]);
    run(&["workspace", "stop", &name]);
    assert_eq!(
        uniterm_server::workspace_catalog::load(&name)
            .unwrap()
            .file_sort,
        FileSort::Recency
    );
}
