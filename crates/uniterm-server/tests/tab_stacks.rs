//! Stack ancestry, managed layouts, first attach, and crash recovery.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use uniterm_proto::{
    encode_frame, ClientMessage, Command, ControlCommand, ControlFrame, ControlRequest,
    ControlResult, FrameDecoder, MouseKind, PaneInfo, ServerMessage, CONTROL_API_VERSION,
};
use uniterm_server::{Server, Terminal};

mod common;

// These scenarios change process-wide runtime paths and fork PTYs. Keep their
// Workspace lifetimes separate, including the two stop/restore transitions.
static WORKSPACE_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_restored_server(socket: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match uniterm_server::run_server(socket, "/bin/sh", &[]) {
            Ok(()) => return,
            Err(error)
                if error.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < deadline =>
            {
                // A fork in flight can briefly retain the old flock until
                // exec/exit. The preceding server thread has already joined;
                // keep the ordinary exclusive claim and bound this restart.
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("could not restore test Workspace: {error}"),
        }
    }
}

struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
    screen: Terminal,
    actions: Vec<uniterm_proto::ChromeAction>,
    focus_settings: Vec<bool>,
    frames: usize,
}

impl Client {
    fn send(&mut self, msg: ClientMessage) {
        self.stream.write_all(&encode_frame(&msg)).unwrap();
    }

    fn barrier(&mut self) -> Vec<PaneInfo> {
        self.send(ClientMessage::PaneList);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut bytes = [0; 32768];
        while Instant::now() < deadline {
            while let Some(msg) = self.decoder.decode::<ServerMessage>().unwrap() {
                match msg {
                    ServerMessage::RenderOps(ops) => {
                        self.frames += 1;
                        self.screen.feed(&ops);
                    }
                    ServerMessage::Panes { panes, .. } => return panes,
                    ServerMessage::OpenChromeAction { action } => self.actions.push(action),
                    ServerMessage::Settings { settings, .. } => {
                        self.focus_settings.push(settings.focus_follows_mouse)
                    }
                    _ => {}
                }
            }
            match self.stream.read(&mut bytes) {
                Ok(0) => panic!("server disconnected"),
                Ok(n) => self.decoder.push(&bytes[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("{e}"),
            }
        }
        panic!("server did not answer");
    }

    // PaneList is a model barrier, not a rendering fence. Under socket
    // backpressure its reply can precede the coalesced authoritative repaint.
    // Consume the real stream without requesting a Refresh that could mask a
    // missing repaint, and keep every visual assertion bounded.
    fn wait_screen(&mut self, ready: impl Fn(&Terminal) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut bytes = [0; 32768];
        loop {
            if ready(&self.screen) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "expected rendered state: {}",
                self.screen.dump_text()
            );
            while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                match message {
                    ServerMessage::RenderOps(ops) => {
                        self.frames += 1;
                        self.screen.feed(&ops);
                    }
                    ServerMessage::OpenChromeAction { action } => self.actions.push(action),
                    ServerMessage::Settings { settings, .. } => {
                        self.focus_settings.push(settings.focus_follows_mouse)
                    }
                    _ => {}
                }
            }
            if ready(&self.screen) {
                return;
            }
            match self.stream.read(&mut bytes) {
                Ok(0) => panic!("server disconnected"),
                Ok(n) => self.decoder.push(&bytes[..n]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("{error}"),
            }
        }
    }

    #[allow(dead_code)]
    fn tab_x(&self, label: &str) -> u16 {
        let row: String = (0..160).map(|x| self.screen.grid().get(x, 23).ch).collect();
        (row.find(label)
            .unwrap_or_else(|| panic!("{label} absent: {row}"))
            + 1) as u16
    }

    #[allow(dead_code)]
    fn mouse(&mut self, x: u16, y: u16, kind: MouseKind) {
        self.send(ClientMessage::Mouse { x, y, kind });
    }
}

fn control(socket: &std::path::Path, workspace: &str, command: ControlCommand) -> bool {
    use std::io::BufRead;
    let mut stream = UnixStream::connect(socket.with_extension("control.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    serde_json::to_writer(
        &mut stream,
        &ControlRequest {
            version: CONTROL_API_VERSION,
            id: 1,
            workspace: workspace.into(),
            command,
        },
    )
    .unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    std::io::BufReader::new(stream)
        .read_line(&mut line)
        .unwrap();
    matches!(serde_json::from_str::<ControlFrame>(&line).unwrap(),
        ControlFrame::Response(response) if matches!(response.result,
            Some(ControlResult::Mutation { accepted: true, .. })))
}

fn start(socket: &std::path::Path) -> std::thread::JoinHandle<()> {
    let socket = socket.to_owned();
    std::thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&socket, "/bin/sh", &[], 120, 24).unwrap();
        server.set_config(uniterm_core::Config {
            sidebar: false,
            file_sidebar: false,
            confirm_close: false,
            status_position: uniterm_core::StatusPosition::Bottom,
            ..Default::default()
        });
        server.run(&mut poll).unwrap();
    })
}
fn attach(socket: &std::path::Path) -> Client {
    common::wait_for_socket(socket);
    let stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut client = Client {
        stream,
        decoder: FrameDecoder::new(),
        screen: Terminal::new(120, 24),
        actions: Vec::new(),
        focus_settings: Vec::new(),
        frames: 0,
    };
    client.send(ClientMessage::Attach {
        term: "xterm-256color".into(),
        cols: 120,
        rows: 24,
    });
    client
}

#[test]
fn child_tab_strip_keeps_sidebar_cells_and_click_targets_in_place() {
    let _serial = WORKSPACE_TEST.lock().unwrap();
    use uniterm_core::{Config, StatusPosition};
    common::isolate_state();
    for position in [StatusPosition::Top, StatusPosition::Bottom] {
        for (left, right) in [(true, true), (true, false), (false, true), (false, false)] {
            let directory = common::temp_dir("stack-rails");
            let socket = directory.join(format!("{}.sock", common::unique_workspace_name()));
            let server_socket = socket.clone();
            let server = std::thread::spawn(move || {
                let (mut server, mut poll) =
                    Server::bind(&server_socket, "/bin/sh", &[], 120, 24).unwrap();
                server.set_config(Config {
                    sidebar: left,
                    sidebar_width: 24,
                    file_sidebar: right,
                    file_sidebar_width: 36,
                    status_position: position,
                    ..Config::default()
                });
                server.run(&mut poll).unwrap();
            });
            let mut client = attach(&socket);
            client.barrier();
            let top = position == StatusPosition::Top;
            let first = u16::from(top);
            let last = if top { 24 } else { 23 };
            let rails = |screen: &Terminal| {
                (first..last)
                    .flat_map(|y| {
                        (0..120).filter_map(move |x| {
                            ((left && x < 24) || (right && x >= 84))
                                .then_some(screen.grid().get(x, y))
                        })
                    })
                    .collect::<Vec<_>>()
            };
            let before = rails(&client.screen);
            client.send(ClientMessage::Command(Command::NewStackTab));
            client.barrier();
            let x = if left { 24 } else { 0 };
            let end = if right { 84 } else { 120 };
            let strip_y = if top { 1 } else { 22 };
            client.wait_screen(|screen| {
                (x..end)
                    .map(|x| screen.grid().get(x, strip_y).ch)
                    .collect::<String>()
                    .contains("2:Tab")
            });
            assert_eq!(
                rails(&client.screen),
                before,
                "sidebar changed for {position:?}, left={left}, right={right}"
            );
            let strip: String = (x..end)
                .map(|x| client.screen.grid().get(x, strip_y).ch)
                .collect();
            assert!(strip.contains("2:Tab"), "missing child tab: {strip}");
            assert!(
                !strip.contains(['↑', '↳']),
                "arrow prefixes remain: {strip}"
            );
            let child_x = x + strip.find("2:Tab").unwrap() as u16;
            let child_style = client.screen.grid().get(child_x, strip_y);
            if right {
                let sidebar_style = client.screen.grid().get(90, if top { 0 } else { 23 });
                assert_eq!(child_style.bg, sidebar_style.bg);
                assert_eq!(child_style.fg, sidebar_style.fg);
            }
            let mut second = attach(&socket);
            second.barrier();
            assert_eq!(
                rails(&second.screen),
                before,
                "first attach moved the sidebars"
            );
            drop(second);
            if right {
                client.mouse(116, last, MouseKind::Click);
                client.barrier();
                assert!(matches!(
                    client.actions.pop(),
                    Some(uniterm_proto::ChromeAction::Config)
                ));
                // The scope button remains one row below the rail's top edge.
                client.mouse(117, first + 2, MouseKind::Click);
                client.barrier();
                client.wait_screen(|screen| {
                    (85..120)
                        .map(|x| screen.grid().get(x, first + 1).ch)
                        .collect::<String>()
                        .contains("project")
                });
                let header: String = (85..120)
                    .map(|x| client.screen.grid().get(x, first + 1).ch)
                    .collect();
                assert!(header.contains("project"), "scope click missed: {header}");
                for (tab_x, label) in [(102, "FILES"), (115, "WEB SERVERS")] {
                    client.mouse(tab_x, if top { 1 } else { 24 }, MouseKind::Click);
                    client.barrier();
                    client.wait_screen(|screen| {
                        (85..120)
                            .map(|x| screen.grid().get(x, first + 1).ch)
                            .collect::<String>()
                            .contains(label)
                    });
                    let header: String = (85..120)
                        .map(|x| client.screen.grid().get(x, first + 1).ch)
                        .collect();
                    assert!(header.contains(label), "sidebar heading moved: {header}");
                    for y in first..last {
                        assert_eq!(client.screen.grid().get(84, y).ch, '│');
                    }
                }
                client.mouse(117, first + 2, MouseKind::Click);
                client.barrier();
                client.wait_screen(|screen| {
                    (85..120)
                        .map(|x| screen.grid().get(x, first + 1).ch)
                        .collect::<String>()
                        .contains("project")
                });
                let header: String = (85..120)
                    .map(|x| client.screen.grid().get(x, first + 1).ch)
                    .collect();
                assert!(
                    header.contains("project"),
                    "server scope click missed: {header}"
                );
            }
            client.send(ClientMessage::Command(Command::UnstackTab));
            client.barrier();
            if left {
                for y in first..last {
                    assert_eq!(client.screen.grid().get(23, y).ch, '│');
                }
            }
            client.send(ClientMessage::KillServer);
            server.join().unwrap();
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}

#[test]
fn closing_stack_members_keeps_the_row_until_the_final_member_closes() {
    let _serial = WORKSPACE_TEST.lock().unwrap();
    common::isolate_state();
    let directory = common::temp_dir("stack-close");
    std::env::set_var("XDG_CONFIG_HOME", directory.join("config"));
    let workspace = common::unique_workspace_name();
    let socket = directory.join(format!("{workspace}.sock"));
    let server = start(&socket);
    let mut client = attach(&socket);
    let root = client.barrier()[0].id;
    client.send(ClientMessage::RenameWindow {
        name: "Parent".into(),
    });
    client.send(ClientMessage::Command(Command::NewWindow));
    let unrelated = client
        .barrier()
        .into_iter()
        .find(|pane| pane.active)
        .unwrap()
        .id;
    client.send(ClientMessage::RenameWindow {
        name: "Unrelated".into(),
    });
    client.send(ClientMessage::PaneFocus { pane: root });
    client.send(ClientMessage::Command(Command::NewStackTab));
    let child = client
        .barrier()
        .into_iter()
        .find(|pane| pane.active)
        .unwrap()
        .id;
    client.send(ClientMessage::Command(Command::NewStackTab));
    let nested = client
        .barrier()
        .into_iter()
        .find(|pane| pane.active)
        .unwrap()
        .id;
    client.send(ClientMessage::PaneFocus { pane: root });
    client.send(ClientMessage::Command(Command::NewStackTab));
    let sibling = client
        .barrier()
        .into_iter()
        .find(|pane| pane.active)
        .unwrap()
        .id;
    let strip = |screen: &Terminal| {
        (0..120)
            .map(|x| screen.grid().get(x, 22).ch)
            .collect::<String>()
    };
    client.wait_screen(|screen| strip(screen).contains(":Tab"));

    // The previous global left-neighbor selection picked Unrelated here.
    // Closing an intermediate child also promotes its own surviving child.
    client.send(ClientMessage::PaneFocus { pane: child });
    client.send(ClientMessage::Command(Command::KillWindow));
    let panes = client.barrier();
    assert_eq!(panes.iter().find(|pane| pane.active).unwrap().id, nested);
    assert_eq!(
        panes
            .iter()
            .find(|pane| pane.id == nested)
            .unwrap()
            .stack_parent,
        Some(root)
    );
    client.wait_screen(|screen| strip(screen).matches(":Tab").count() == 2);

    // Agent-targeted closure follows the same selection policy.
    assert!(control(
        &socket,
        &workspace,
        ControlCommand::PaneClose { pane: nested }
    ));
    let panes = client.barrier();
    assert_eq!(panes.iter().find(|pane| pane.active).unwrap().id, sibling);
    client.wait_screen(|screen| strip(screen).matches(":Tab").count() == 1);
    let mut second = attach(&socket);
    second.barrier();
    second.wait_screen(|screen| strip(screen).matches(":Tab").count() == 1);
    drop(second);

    client.send(ClientMessage::Command(Command::KillPane));
    let panes = client.barrier();
    assert_eq!(panes.len(), 2);
    assert_eq!(panes.iter().find(|pane| pane.active).unwrap().id, root);
    assert!(panes.iter().any(|pane| pane.id == unrelated));
    assert!(panes.iter().all(|pane| pane.stack_parent.is_none()));
    client
        .wait_screen(|screen| !strip(screen).contains(":Tab") && strip(screen).contains("Parent"));

    // A hidden child closing must keep the selected root and its other child.
    client.send(ClientMessage::Command(Command::NewStackTab));
    let hidden = client
        .barrier()
        .into_iter()
        .find(|pane| pane.active)
        .unwrap()
        .id;
    client.send(ClientMessage::Command(Command::NewStackTab));
    let survivor = client
        .barrier()
        .into_iter()
        .find(|pane| pane.active)
        .unwrap()
        .id;
    client.send(ClientMessage::PaneFocus { pane: root });
    client.barrier();
    assert!(control(
        &socket,
        &workspace,
        ControlCommand::PaneClose { pane: hidden }
    ));
    let panes = client.barrier();
    assert_eq!(panes.iter().find(|pane| pane.active).unwrap().id, root);
    assert_eq!(
        panes
            .iter()
            .find(|pane| pane.id == survivor)
            .unwrap()
            .stack_parent,
        Some(root)
    );
    client.wait_screen(|screen| strip(screen).matches(":Tab").count() == 1);
    assert!(control(
        &socket,
        &workspace,
        ControlCommand::PaneClose { pane: survivor }
    ));
    assert_eq!(
        client.barrier().iter().find(|pane| pane.active).unwrap().id,
        root
    );
    client
        .wait_screen(|screen| !strip(screen).contains(":Tab") && strip(screen).contains("Parent"));
    // Explicitly moving the sole member out collapses its row, and repeating
    // that action is a no-op. Creating another child opens the row again.
    client.send(ClientMessage::Command(Command::UnstackTab));
    client.barrier();
    client.wait_screen(|screen| !strip(screen).contains("Parent"));
    client.send(ClientMessage::Command(Command::UnstackTab));
    client.barrier();
    client.send(ClientMessage::Command(Command::NewStackTab));
    let temporary = client
        .barrier()
        .into_iter()
        .find(|pane| pane.active)
        .unwrap()
        .id;
    assert!(control(
        &socket,
        &workspace,
        ControlCommand::PaneClose { pane: temporary }
    ));
    client.barrier();
    client.wait_screen(|screen| strip(screen).contains("Parent"));
    let mut second = attach(&socket);
    second.barrier();
    second.wait_screen(|screen| strip(screen).contains("Parent"));
    drop(second);
    let deadline = Instant::now() + Duration::from_secs(8);
    let snapshot = loop {
        if let Some(snapshot) = uniterm_server::persist::load(&workspace) {
            if snapshot.windows.len() == 2 && snapshot.windows[0].stack_expanded {
                break snapshot;
            }
        }
        assert!(
            Instant::now() < deadline,
            "singleton stack checkpoint missing"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    uniterm_server::persist::save(&workspace, &snapshot).unwrap();
    let restore_socket = socket.clone();
    let server = std::thread::spawn(move || run_restored_server(&restore_socket));
    let mut client = attach(&socket);
    let panes = client.barrier();
    let root = panes
        .iter()
        .find(|pane| pane.tab_name == "Parent")
        .unwrap()
        .id;
    client.send(ClientMessage::PaneFocus { pane: root });
    client.barrier();
    // Restore uses the default top status position.
    client.wait_screen(|screen| {
        (0..120)
            .map(|x| screen.grid().get(x, 1).ch)
            .collect::<String>()
            .contains("Parent")
    });
    // The real runtime now records the catalog, so a clean restart must
    // retain the singleton just as the crash checkpoint did.
    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    let restore_socket = socket.clone();
    let server = std::thread::spawn(move || run_restored_server(&restore_socket));
    let mut client = attach(&socket);
    let panes = client.barrier();
    let root = panes
        .iter()
        .find(|pane| pane.tab_name == "Parent")
        .unwrap()
        .id;
    client.wait_screen(|screen| {
        (0..120)
            .map(|x| screen.grid().get(x, 1).ch)
            .collect::<String>()
            .contains("Parent")
    });
    client.send(ClientMessage::Command(Command::NewStackTab));
    let first = client
        .barrier()
        .into_iter()
        .find(|pane| pane.active)
        .unwrap()
        .id;
    client.send(ClientMessage::RenameWindow {
        name: "First".into(),
    });
    client.send(ClientMessage::PaneFocus { pane: root });
    client.send(ClientMessage::Command(Command::NewStackTab));
    let last = client
        .barrier()
        .into_iter()
        .find(|pane| pane.active)
        .unwrap()
        .id;
    client.send(ClientMessage::RenameWindow {
        name: "Last".into(),
    });
    client.barrier();
    // Closing the selected original member transfers focus and its group.
    client.send(ClientMessage::PaneFocus { pane: root });
    client.barrier();
    assert!(control(
        &socket,
        &workspace,
        ControlCommand::PaneClose { pane: root }
    ));
    let panes = client.barrier();
    assert_eq!(
        panes
            .iter()
            .find(|pane| pane.id == last)
            .unwrap()
            .stack_parent,
        Some(first)
    );
    assert_eq!(panes.iter().find(|pane| pane.active).unwrap().id, first);
    assert!(control(
        &socket,
        &workspace,
        ControlCommand::PaneClose { pane: first }
    ));
    client.barrier();
    client.wait_screen(|screen| {
        (0..120)
            .map(|x| screen.grid().get(x, 1).ch)
            .collect::<String>()
            .contains("Last")
    });
    assert!(control(
        &socket,
        &workspace,
        ControlCommand::PaneClose { pane: last }
    ));
    assert_eq!(client.barrier().len(), 1);
    client.wait_screen(|screen| {
        !(0..120)
            .map(|x| screen.grid().get(x, 1).ch)
            .collect::<String>()
            .contains("Last")
    });
    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn hover_focus_tracks_live_settings_in_every_layout_and_scrolled_viewport() {
    let _serial = WORKSPACE_TEST.lock().unwrap();
    use uniterm_core::layout::PaneLayout;
    common::isolate_state();
    let directory = common::temp_dir("layout-hover");
    std::env::set_var("XDG_CONFIG_HOME", directory.join("config"));
    let socket = directory.join(format!("{}.sock", common::unique_workspace_name()));
    let server_socket = socket.clone();
    let server = std::thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/cat", &[], 120, 24).unwrap();
        server.set_config(uniterm_core::Config {
            sidebar: false,
            file_sidebar: false,
            status_position: uniterm_core::StatusPosition::Bottom,
            ..Default::default()
        });
        server.run(&mut poll).unwrap();
    });
    let mut client = attach(&socket);
    let first = client.barrier()[0].id;
    assert_eq!(client.focus_settings, [false]);
    let mut observer = attach(&socket);
    observer.barrier();
    assert_eq!(observer.focus_settings, [false]);
    client.send(ClientMessage::Command(Command::Split(
        uniterm_proto::SplitAxis::LeftRight,
    )));
    let second = client
        .barrier()
        .into_iter()
        .find(|pane| pane.active)
        .unwrap()
        .id;
    for mode in [
        PaneLayout::Split,
        PaneLayout::Dwindle,
        PaneLayout::Scrolling,
    ] {
        client.send(ClientMessage::Command(Command::PaneLayout(mode)));
        client.send(ClientMessage::PaneFocus { pane: second });
        client.mouse(2, 3, MouseKind::Hover);
        assert_eq!(
            client.barrier().iter().find(|pane| pane.active).unwrap().id,
            second
        );
        client.send(ClientMessage::SettingsApply(uniterm_proto::SettingsPatch {
            focus_follows_mouse: Some(true),
            ..Default::default()
        }));
        client.barrier();
        observer.barrier();
        assert_eq!(client.focus_settings.last(), Some(&true));
        assert_eq!(observer.focus_settings.last(), Some(&true));
        client.mouse(2, 3, MouseKind::Hover);
        assert_eq!(
            client.barrier().iter().find(|pane| pane.active).unwrap().id,
            first,
            "{mode:?}"
        );
        client.mouse(118, 3, MouseKind::Hover);
        assert_eq!(
            client.barrier().iter().find(|pane| pane.active).unwrap().id,
            second,
            "{mode:?}"
        );
        let frames = client.frames;
        client.mouse(118, 3, MouseKind::Hover);
        client.barrier();
        assert_eq!(client.frames, frames, "repeated hover repainted {mode:?}");
        client.send(ClientMessage::SettingsApply(uniterm_proto::SettingsPatch {
            focus_follows_mouse: Some(false),
            ..Default::default()
        }));
        client.barrier();
        observer.barrier();
        assert_eq!(observer.focus_settings.last(), Some(&false));
    }
    client.send(ClientMessage::SettingsApply(uniterm_proto::SettingsPatch {
        focus_follows_mouse: Some(true),
        ..Default::default()
    }));
    client.send(ClientMessage::Command(Command::Split(
        uniterm_proto::SplitAxis::LeftRight,
    )));
    let third = client
        .barrier()
        .into_iter()
        .find(|pane| pane.active)
        .unwrap()
        .id;
    client.mouse(2, 3, MouseKind::Hover);
    assert_eq!(
        client.barrier().iter().find(|pane| pane.active).unwrap().id,
        second
    );
    client.mouse(118, 3, MouseKind::Hover);
    assert_eq!(
        client.barrier().iter().find(|pane| pane.active).unwrap().id,
        third
    );
    // Hovering the scrollbar itself must not select or scroll a Pane.
    client.mouse(2, 23, MouseKind::Hover);
    assert_eq!(
        client.barrier().iter().find(|pane| pane.active).unwrap().id,
        third
    );
    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn scrolling_bar_reserves_content_cells_and_drives_the_visible_columns() {
    let _serial = WORKSPACE_TEST.lock().unwrap();
    use uniterm_core::layout::PaneLayout;
    use uniterm_core::{Config, StatusPosition};
    common::isolate_state();
    for position in [StatusPosition::Top, StatusPosition::Bottom] {
        let directory = common::temp_dir("scrolling-bar");
        let socket = directory.join(format!("{}.sock", common::unique_workspace_name()));
        let server_socket = socket.clone();
        let server = std::thread::spawn(move || {
            let (mut server, mut poll) =
                Server::bind(&server_socket, "/bin/sh", &[], 120, 24).unwrap();
            server.set_config(Config {
                sidebar: true,
                sidebar_width: 24,
                file_sidebar: true,
                file_sidebar_width: 36,
                status_position: position,
                ..Config::default()
            });
            server.run(&mut poll).unwrap();
        });
        let mut client = attach(&socket);
        client.barrier();
        client.send(ClientMessage::Command(Command::NewStackTab));
        let first = client
            .barrier()
            .into_iter()
            .find(|pane| pane.active)
            .unwrap()
            .id;
        for _ in 0..3 {
            client.send(ClientMessage::Command(Command::Split(
                uniterm_proto::SplitAxis::LeftRight,
            )));
        }
        let panes = client.barrier();
        let tab = panes.iter().find(|pane| pane.id == first).unwrap().tab;
        let mut columns: Vec<_> = panes.iter().filter(|pane| pane.tab == tab).collect();
        columns.sort_by_key(|pane| pane.pane);
        client.send(ClientMessage::Command(Command::PaneLayout(
            PaneLayout::Scrolling,
        )));
        client.send(ClientMessage::PaneFocus { pane: first });
        client.barrier();
        let y = if position == StatusPosition::Top {
            23
        } else {
            21
        };
        let row = |screen: &Terminal| {
            (24..84)
                .map(|x| screen.grid().get(x, y).ch)
                .collect::<String>()
        };
        let left = format!("{}{}", "━".repeat(30), " ".repeat(30));
        client.wait_screen(|screen| row(screen) == left);
        assert_eq!(row(&client.screen), left);
        for x in 24..84 {
            let cell = client.screen.grid().get(x, y);
            assert_eq!(
                cell.bg,
                uniterm_core::Color::Default,
                "scrollbar background at {x}"
            );
            assert_eq!(
                cell.fg,
                if x < 54 {
                    Config::default().theme.muted
                } else {
                    uniterm_core::Color::Default
                }
            );
            assert_eq!(cell.attrs, uniterm_core::Attrs::default());
        }
        assert_eq!(client.screen.grid().get(23, y).ch, '│');
        assert_eq!(client.screen.grid().get(84, y).ch, '│');
        client.mouse(60, y + 1, MouseKind::Click);
        assert_eq!(
            client
                .barrier()
                .into_iter()
                .find(|pane| pane.active)
                .unwrap()
                .id,
            columns[1].id
        );
        let middle = format!("{}{}{}", " ".repeat(15), "━".repeat(30), " ".repeat(15));
        client.wait_screen(|screen| row(screen) == middle);
        assert_eq!(row(&client.screen), middle);
        client.mouse(45, y + 1, MouseKind::Click);
        client.mouse(120, y + 1, MouseKind::Drag);
        client.mouse(120, y + 1, MouseKind::Release);
        assert_eq!(
            client
                .barrier()
                .into_iter()
                .find(|pane| pane.active)
                .unwrap()
                .id,
            columns[2].id
        );
        let right = format!("{}{}", " ".repeat(30), "━".repeat(30));
        client.wait_screen(|screen| row(screen) == right);
        assert_eq!(row(&client.screen), right);
        client.mouse(30, y + 1, MouseKind::WheelLeft);
        client.mouse(30, y + 1, MouseKind::WheelLeft);
        client.barrier();
        client.wait_screen(|screen| row(screen) == left);
        assert_eq!(row(&client.screen), left);
        let mut second = attach(&socket);
        second.barrier();
        assert_eq!(row(&second.screen), row(&client.screen));
        drop(second);
        // Layout radio state is read from the targeted Tab when the menu opens.
        client.mouse(30, 8, MouseKind::RightClick);
        client.barrier();
        client.wait_screen(|screen| screen.dump_text().contains("● Scrolling layout"));
        assert!(client.screen.dump_text().contains("● Scrolling layout"));
        client.send(ClientMessage::Input(b"\x1b".to_vec()));
        client.send(ClientMessage::Command(Command::ZoomToggle));
        client.barrier();
        client.wait_screen(|screen| !row(screen).contains('━'));
        assert!(!row(&client.screen).contains('━'));
        client.send(ClientMessage::Command(Command::ZoomToggle));
        client.barrier();
        client.wait_screen(|screen| row(screen).contains('━'));
        client.send(ClientMessage::Command(Command::PaneLayout(
            PaneLayout::Split,
        )));
        client.barrier();
        client.wait_screen(|screen| !row(screen).contains('━'));
        assert!(!row(&client.screen).contains('━'));
        client.send(ClientMessage::KillServer);
        server.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn stacks_layouts_cycles_and_restore_share_durable_state() {
    let _serial = WORKSPACE_TEST.lock().unwrap();
    use uniterm_core::layout::PaneLayout;
    common::isolate_state();
    let workspace = common::unique_workspace_name();
    let dir = common::socket_root().join(format!("ut-stacks-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &dir);
    let socket = dir.join(format!("{workspace}.sock"));
    let server = start(&socket);
    let mut client = attach(&socket);
    let root = client.barrier()[0].id;
    client.send(ClientMessage::RenameWindow {
        name: "Parent".into(),
    });
    client.send(ClientMessage::Command(Command::NewStackTab));
    let child = client.barrier().into_iter().find(|p| p.active).unwrap();
    assert_eq!(child.stack_parent, Some(root));
    client.send(ClientMessage::RenameWindow {
        name: "Child".into(),
    });
    client.send(ClientMessage::Command(Command::NewStackTab));
    let nested = client.barrier().into_iter().find(|p| p.active).unwrap();
    assert_eq!(nested.stack_parent, Some(child.id));
    assert!(!control(
        &socket,
        &workspace,
        ControlCommand::TabStack {
            pane: root,
            parent: Some(nested.id)
        }
    ));
    assert!(control(
        &socket,
        &workspace,
        ControlCommand::TabStack {
            pane: nested.id,
            parent: Some(child.id)
        }
    ));
    for _ in 0..3 {
        client.send(ClientMessage::Command(Command::Split(
            uniterm_proto::SplitAxis::LeftRight,
        )));
    }
    client.send(ClientMessage::Command(Command::PaneLayout(
        PaneLayout::Dwindle,
    )));
    assert!(client
        .barrier()
        .iter()
        .filter(|p| p.tab == nested.tab)
        .all(|p| p.layout_mode == PaneLayout::Dwindle));
    assert!(control(
        &socket,
        &workspace,
        ControlCommand::PaneLayout {
            pane: nested.id,
            mode: PaneLayout::Scrolling
        }
    ));
    let panes = client.barrier();
    assert!(panes
        .iter()
        .filter(|p| p.tab == nested.tab)
        .all(|p| p.layout_mode == PaneLayout::Scrolling));
    client.send(ClientMessage::PaneFocus { pane: nested.id });
    client.barrier();
    client.wait_screen(|screen| {
        let row: String = (0..120).map(|x| screen.grid().get(x, 22).ch).collect();
        row.contains("Parent") && row.contains("Child")
    });
    let row: String = (0..120)
        .map(|x| client.screen.grid().get(x, 22).ch)
        .collect();
    assert!(
        row.contains("Parent") && row.contains("Child"),
        "stack strip: {row}"
    );
    // Closing a hidden intermediate parent promotes its children and updates
    // chrome without selecting a different Pane.
    assert!(control(
        &socket,
        &workspace,
        ControlCommand::PaneClose { pane: child.id }
    ));
    let promoted = client.barrier();
    assert_eq!(
        promoted
            .iter()
            .find(|p| p.id == nested.id)
            .unwrap()
            .stack_parent,
        Some(root)
    );
    assert_eq!(promoted.iter().find(|p| p.active).unwrap().id, nested.id);
    // Agents create children through the same scoped control path, preserving
    // the selected Pane and its existing zoom even for a hidden parent.
    client.send(ClientMessage::Command(Command::ZoomToggle));
    client.barrier();
    assert!(control(
        &socket,
        &workspace,
        ControlCommand::AgentLaunch {
            agent: "sh".into(),
            target: uniterm_proto::LaunchTarget::NewStack,
            pane: Some(root),
            background: true,
        }
    ));
    let launched = client.barrier();
    assert_eq!(launched.iter().find(|p| p.active).unwrap().id, nested.id);
    let worker = launched
        .iter()
        .find(|p| !promoted.iter().any(|old| old.id == p.id))
        .unwrap();
    assert_eq!(worker.stack_parent, Some(root));
    assert!(control(
        &socket,
        &workspace,
        ControlCommand::PaneClose { pane: worker.id }
    ));
    client.send(ClientMessage::Command(Command::ZoomToggle));
    client.barrier();
    let deadline = Instant::now() + Duration::from_secs(8);
    let snapshot = loop {
        if let Some(snapshot) = uniterm_server::persist::load(&workspace) {
            if snapshot.windows.len() == 2
                && snapshot.windows[1].stack_parent == Some(root)
                && snapshot.windows[1].layout_mode == PaneLayout::Scrolling
            {
                break snapshot;
            }
        }
        assert!(Instant::now() < deadline, "checkpoint not persisted");
        std::thread::sleep(Duration::from_millis(20));
    };
    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    // Recreate a crash marker from the exact checkpoint, not a live Workspace.
    uniterm_server::persist::save(&workspace, &snapshot).unwrap();
    let restore_socket = socket.clone();
    let server = std::thread::spawn(move || {
        run_restored_server(&restore_socket);
    });
    let mut client = attach(&socket);
    let restored = client.barrier();
    assert_eq!(
        restored
            .iter()
            .find(|p| p.id == nested.id)
            .unwrap()
            .stack_parent,
        Some(root)
    );
    assert_eq!(
        restored
            .iter()
            .find(|p| p.id == nested.id)
            .unwrap()
            .layout_mode,
        PaneLayout::Scrolling
    );
    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    assert!(uniterm_server::persist::load(&workspace).is_none());
    let restore_socket = socket.clone();
    let server = std::thread::spawn(move || {
        run_restored_server(&restore_socket);
    });
    let mut client = attach(&socket);
    let clean = client.barrier();
    let root = clean.iter().find(|p| p.tab_name == "Parent").unwrap().id;
    let children: Vec<_> = clean
        .iter()
        .filter(|p| p.layout_mode == PaneLayout::Scrolling)
        .collect();
    assert_eq!(children.len(), 4);
    assert!(children.iter().all(|p| p.stack_parent == Some(root)));
    assert!(control(
        &socket,
        &workspace,
        ControlCommand::PaneClose { pane: root }
    ));
    assert!(client.barrier().iter().all(|p| p.stack_parent.is_none()));
    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
