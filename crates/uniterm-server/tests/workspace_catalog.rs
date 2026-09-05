//! A clean Workspace stop remembers only Projects and Tabs, then reconstructs
//! fresh shells through the production server entry point.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{
    encode_frame, ClientMessage, Command, FocusDir, FrameDecoder, MouseKind, ServerMessage,
    SplitAxis, WorkspaceLayoutDefinition,
};

mod common;

use common::{isolate_state, unique_workspace_name};

fn wait_for_socket(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("Workspace socket never appeared at {}", path.display());
}

fn workspace_state(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
) -> (uniterm_core::ProjectId, Vec<uniterm_proto::ProjectInfo>) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buffer = [0u8; 8192];
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => decoder.push(&buffer[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("Workspace read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::Workspace {
                active_project,
                projects,
                ..
            } = message
            {
                return (active_project, projects);
            }
        }
    }
    panic!("Workspace state did not arrive");
}

fn tasks(stream: &mut UnixStream, decoder: &mut FrameDecoder) -> Vec<uniterm_proto::TaskEntry> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buffer = [0u8; 8192];
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => decoder.push(&buffer[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("task read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::Tasks { items } = message {
                return items;
            }
        }
    }
    panic!("task list did not arrive");
}

fn start(socket: std::path::PathBuf) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        uniterm_server::server::run_server(&socket, "/bin/sh", &[]).unwrap();
    })
}

#[test]
fn clean_stop_rebuilds_projects_tabs_and_split_geometry_without_runtime_state() {
    isolate_state();
    let base =
        common::socket_root().join(format!("uniterm-workspace-catalog-{}", std::process::id()));
    let config = base.join("config");
    let project_root = base.join("Site");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &config);
    let workspace = unique_workspace_name();
    let socket = base.join(format!("{workspace}.sock"));

    let first_server = start(socket.clone());
    wait_for_socket(&socket);
    let mut client = UnixStream::connect(&socket).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    client
        .write_all(&encode_frame(&ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 100,
            rows: 30,
        }))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::ProjectCreate {
            name: "Site".into(),
            root: project_root.to_string_lossy().into_owned(),
        }))
        .unwrap();
    let (site, projects) = workspace_state(&mut client, &mut decoder);
    assert_eq!(projects.len(), 2);
    client
        .write_all(&encode_frame(&ClientMessage::Command(Command::NewWindow)))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::RenameWindow {
            name: "Web".into(),
        }))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Command(Command::Split(
            SplitAxis::LeftRight,
        ))))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Command(Command::ResizePane(
            FocusDir::Right,
        ))))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Command(Command::Split(
            SplitAxis::TopBottom,
        ))))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Command(Command::ResizePane(
            FocusDir::Down,
        ))))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::WorkspaceState))
        .unwrap();
    let (_, projects) = workspace_state(&mut client, &mut decoder);
    assert_eq!(
        projects
            .iter()
            .find(|project| project.id == site)
            .unwrap()
            .tabs,
        2
    );
    client
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 95,
            y: 3,
            kind: MouseKind::Click,
        }))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 95,
            y: 1,
            kind: MouseKind::Click,
        }))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 95,
            y: 3,
            kind: MouseKind::Click,
        }))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::WorkspaceState))
        .unwrap();
    let _ = workspace_state(&mut client, &mut decoder);
    // Durable Tasks are a projection of the event stream and must outlive an
    // intentional stop, unlike the terminal snapshot.
    client
        .write_all(&encode_frame(&ClientMessage::SaveTask {
            title: "Ship the catalog".into(),
        }))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Tasks))
        .unwrap();
    assert_eq!(tasks(&mut client, &mut decoder).len(), 1);
    client
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    first_server.join().unwrap();

    let definition = uniterm_server::workspace_catalog::load(&workspace).unwrap();
    assert!(definition.agent_scope_workspace);
    assert!(definition.server_scope_workspace);
    assert_eq!(definition.projects.len(), 2);
    let site_definition = definition
        .projects
        .iter()
        .find(|project| project.id == site)
        .unwrap();
    assert_eq!(site_definition.root, project_root.to_string_lossy());
    assert_eq!(site_definition.tabs.len(), 2);
    assert_eq!(site_definition.tabs[1].name.as_deref(), Some("Web"));
    let WorkspaceLayoutDefinition::Split {
        dir,
        first_ratio,
        first,
        second,
    } = &site_definition.tabs[1].layout
    else {
        panic!("remembered Web Tab lost its left-right split");
    };
    assert_eq!(*dir, uniterm_core::SplitDir::Horizontal);
    assert_eq!(*first_ratio, 5_500);
    assert_eq!(first.as_ref(), &WorkspaceLayoutDefinition::Pane);
    let WorkspaceLayoutDefinition::Split {
        dir,
        first_ratio,
        first,
        second: nested_second,
    } = second.as_ref()
    else {
        panic!("remembered Web Tab lost its nested top-bottom split");
    };
    assert_eq!(*dir, uniterm_core::SplitDir::Vertical);
    assert_eq!(*first_ratio, 5_500);
    assert_eq!(first.as_ref(), &WorkspaceLayoutDefinition::Pane);
    assert_eq!(nested_second.as_ref(), &WorkspaceLayoutDefinition::Pane);
    assert_eq!(site_definition.tabs[1].layout.pane_count(), Some(3));
    // The snapshot is the crash marker and goes; the stream stays.
    assert!(!uniterm_server::persist::exists(&workspace));
    assert!(uniterm_server::eventlog::exists(&workspace));
    // Every checkpoint records the definition, but identical consecutive
    // definitions are suppressed, so the catalog holds only real changes.
    let catalog = std::path::PathBuf::from(std::env::var_os("XDG_STATE_HOME").unwrap())
        .join("uniterm")
        .join("workspaces")
        .join(format!(
            "{}.jsonl",
            uniterm_proto::workspace_catalog_key(&workspace)
        ));
    let lines: Vec<String> = std::fs::read_to_string(&catalog)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert!(
        lines.windows(2).all(|pair| pair[0] != pair[1]),
        "catalog holds consecutive identical definitions ({} lines)",
        lines.len()
    );
    assert!(lines.len() <= 12, "catalog grew to {} lines", lines.len());

    let second_server = start(socket.clone());
    wait_for_socket(&socket);
    let mut restored = UnixStream::connect(&socket).unwrap();
    restored
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut restored_decoder = FrameDecoder::new();
    restored
        .write_all(&encode_frame(&ClientMessage::WorkspaceState))
        .unwrap();
    let (active, projects) = workspace_state(&mut restored, &mut restored_decoder);
    assert_eq!(active, site);
    assert_eq!(projects.len(), 2);
    assert_eq!(
        projects
            .iter()
            .find(|project| project.id == site)
            .unwrap()
            .tabs,
        2
    );
    let restored_site = projects.iter().find(|project| project.id == site).unwrap();
    assert_eq!(restored_site.tabs, 2);
    assert_eq!(restored_site.panes, 4);
    restored
        .write_all(&encode_frame(&ClientMessage::Tasks))
        .unwrap();
    let restored_tasks = tasks(&mut restored, &mut restored_decoder);
    assert_eq!(restored_tasks.len(), 1);
    assert_eq!(restored_tasks[0].title, "Ship the catalog");
    restored
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    second_server.join().unwrap();
    let restored_definition = uniterm_server::workspace_catalog::load(&workspace).unwrap();
    assert!(restored_definition.agent_scope_workspace);
    assert!(restored_definition.server_scope_workspace);

    uniterm_server::workspace_catalog::delete(&workspace).unwrap();
    let _ = std::fs::remove_dir_all(base);
}
