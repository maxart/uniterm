//! P2-1: the server resurrects its window/pane structure from a snapshot.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_core::{LayoutNode, PaneId, ProjectId, SplitDir};
use uniterm_proto::{encode_frame, ClientMessage, FrameDecoder, ServerMessage};
use uniterm_server::persist::{self, PaneSnap, ProjectSnap, Snapshot, WinSnap};

mod common;

#[test]
fn server_restores_structure_from_snapshot() {
    let base = common::socket_root().join(format!("uniterm-p2-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    // Isolate the snapshot state dir for this test process.
    std::env::set_var("XDG_STATE_HOME", &base);

    // Craft a snapshot with one window of three panes (a nested layout).
    // Its legacy-style Panes deliberately have no cwd. Restore must launch
    // every one at the owning Project root on hosts such as macOS where older
    // builds could not read `/proc/<pid>/cwd`.
    let observed = base.join("restored-cwds.txt");
    let record_cwd = vec![
        "-c".into(),
        format!("pwd >> '{}'; sleep 10", observed.display()),
    ];
    let mut layout = LayoutNode::Leaf(PaneId(1));
    layout.split(PaneId(1), SplitDir::Horizontal, PaneId(2));
    layout.split(PaneId(2), SplitDir::Vertical, PaneId(3));
    let snap = Snapshot::new(
        0,
        4,
        ProjectId(1),
        2,
        vec![ProjectSnap {
            id: ProjectId(1),
            name: "Default".into(),
            root: base.to_string_lossy().into_owned(),
            active_pane: Some(PaneId(1)),
            metadata: Vec::new(),
        }],
        vec![WinSnap {
            project: ProjectId(1),
            layout,
            active: PaneId(1),
            zoomed: None,
            name: None,
            panes: vec![
                PaneSnap {
                    id: PaneId(1),
                    cwd: None,
                    content: vec![],
                    metadata: Vec::new(),
                    launch_args: record_cwd.clone(),
                    agent_launch: None,
                },
                PaneSnap {
                    id: PaneId(2),
                    cwd: None,
                    content: vec![],
                    metadata: Vec::new(),
                    launch_args: record_cwd.clone(),
                    agent_launch: None,
                },
                PaneSnap {
                    id: PaneId(3),
                    cwd: Some(
                        base.join("removed-subdirectory")
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    content: vec![],
                    metadata: Vec::new(),
                    launch_args: record_cwd,
                    agent_launch: None,
                },
            ],
        }],
    );
    persist::save("restoresess", &snap).unwrap();

    // Start a server for that session; run_server should load + restore it.
    let sock = base.join("restoresess.sock");
    let sock_srv = sock.clone();
    let server = thread::spawn(move || {
        let _ = uniterm_server::run_server(&sock_srv, "/bin/sh", &[]);
    });
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let cwd_deadline = Instant::now() + Duration::from_secs(3);
    let restored_cwds = loop {
        let lines = std::fs::read_to_string(&observed).unwrap_or_default();
        let lines: Vec<_> = lines.lines().map(str::to_string).collect();
        if lines.len() >= 3 || Instant::now() >= cwd_deadline {
            break lines;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let canonical_root = std::fs::canonicalize(&base)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(restored_cwds.len(), 3);
    assert!(
        restored_cwds.iter().all(|cwd| cwd == &canonical_root),
        "restored Panes did not inherit their Project root: {restored_cwds:?}"
    );

    // Query the restored structure: one window, three panes.
    let mut c = UnixStream::connect(&sock).unwrap();
    c.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    c.write_all(&encode_frame(&ClientMessage::ListInfo))
        .unwrap();
    c.flush().unwrap();
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 4096];
    let mut info = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && info.is_none() {
        if let Ok(n) = c.read(&mut buf) {
            if n == 0 {
                break;
            }
            dec.push(&buf[..n]);
            while let Ok(Some(m)) = dec.decode::<ServerMessage>() {
                if let ServerMessage::Info { windows, panes } = m {
                    info = Some((windows, panes));
                }
            }
        }
    }

    let _ = c.write_all(&encode_frame(&ClientMessage::KillServer));
    let _ = c.flush();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(&base);

    assert_eq!(
        info,
        Some((1, 3)),
        "expected 1 window and 3 panes restored from the snapshot"
    );
}
