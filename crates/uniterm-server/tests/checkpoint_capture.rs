//! Detached captures must preserve the durable cell schema through mutations.

use uniterm_core::{GridCapture, LayoutNode, PaneId, ProjectId, StoredLine};
use uniterm_proto::checkpoint::{PaneSnap, ProjectSnap, Snapshot, WinSnap};
use uniterm_server::Terminal;

#[test]
fn compact_capture_matches_existing_bytes_after_source_mutation() {
    for width in [1, 8, 80] {
        let mut terminal = Terminal::new(width, 6);
        terminal.feed("first\r\n界 e\u{301} 👩‍💻\r\n".as_bytes());
        terminal.feed(b"\x1b[4:3;58:2::20:30:40mstyled\x1b[0m\r\n");
        terminal.feed(b"\x1b]8;;https://example.test\x1b\\linked \x1b]8;;\x1b\\\r\n");
        terminal.feed(b"tail\r\n\r\n\r\n");
        for limit in [0, 1, 3, 1000] {
            let expected = terminal.grid().export_lines(limit);
            let capture = terminal.grid().capture_lines(limit);
            let bytes = bincode::serialize(&capture).unwrap();
            assert_eq!(
                bytes,
                bincode::serialize(&expected).unwrap(),
                "width {width}, limit {limit}"
            );
            assert_eq!(
                bincode::deserialize::<Vec<StoredLine>>(&bytes).unwrap(),
                expected
            );
        }
        let expected = terminal.grid().export_lines(1000);
        let capture = terminal.grid().capture_lines(1000);
        terminal.resize(17, 4);
        terminal.feed(b"\x1b[?1049h\x1b[2Jreplacement");
        drop(terminal);
        assert_eq!(
            bincode::serialize(&capture).unwrap(),
            bincode::serialize(&expected).unwrap()
        );
        assert_eq!(capture.into_stored_lines(), expected);
    }
}

#[test]
fn compact_checkpoint_preserves_schema_and_all_event_cursors() {
    let mut terminal = Terminal::new(80, 24);
    terminal.feed(b"captured before mutation");
    let mut snapshot: Snapshot<GridCapture> = Snapshot::new_with_sequence(
        0,
        2,
        ProjectId(1),
        2,
        vec![ProjectSnap {
            id: ProjectId(1),
            name: "project".into(),
            root: "/project".into(),
            active_pane: Some(PaneId(1)),
            metadata: vec![],
        }],
        vec![WinSnap {
            project: ProjectId(1),
            layout: LayoutNode::Leaf(PaneId(1)),
            active: PaneId(1),
            zoomed: None,
            name: Some("tab".into()),
            panes: vec![PaneSnap {
                id: PaneId(1),
                cwd: Some("/project".into()),
                content: terminal.grid().capture_lines(1000),
                metadata: vec![("key".into(), "value".into())],
                launch_args: vec!["--login".into()],
                agent_launch: None,
            }],
        }],
        41,
    );
    snapshot.run_graph_sequence = 39;
    snapshot.artifact_sequence = 40;
    let reference = snapshot.clone().map_content(GridCapture::into_stored_lines);
    let bytes = bincode::serialize(&snapshot).unwrap();
    assert_eq!(
        bytes,
        uniterm_server::persist::serialize(&reference).unwrap()
    );
    let restored: Snapshot = bincode::deserialize(&bytes).unwrap();
    assert_eq!(restored.event_sequence, 41);
    assert_eq!(restored.run_graph_sequence, 39);
    assert_eq!(restored.artifact_sequence, 40);
    assert_eq!(restored.windows[0].panes[0].launch_args, ["--login"]);
}
