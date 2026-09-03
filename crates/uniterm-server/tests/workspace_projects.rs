//! The durable product hierarchy is Workspace > Project > Tab > Pane.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use uniterm_proto::{
    encode_frame, ClientMessage, Command, FrameDecoder, ImportedProject, ImportedTab,
    ImportedWorkspace, MouseKind, ProjectMoveDirection, ServerMessage, WorkspaceImportMode,
};
use uniterm_server::Server;

/// A Workspace name that cannot collide with anything a user runs: `ut-`
/// followed by the process id and a nanosecond nonce. A server bound against
/// the real state directory reads, and on clean stop deletes, the snapshot and
/// stream of whatever Workspace shares its name.
fn unique_workspace_name() -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("ut-{}-{nonce}", std::process::id())
}

/// Point durable state at a per-process directory so nothing these tests
/// write or delete can reach a real Workspace.
fn isolate_state() {
    let state = std::env::temp_dir().join(format!(
        "uniterm-workspace-projects-state-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&state).unwrap();
    std::env::set_var("XDG_STATE_HOME", &state);
}

fn receive_workspace(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
) -> (uniterm_core::ProjectId, Vec<uniterm_proto::ProjectInfo>) {
    let (active, projects, _) = receive_workspace_with_render(stream, decoder);
    (active, projects)
}

fn receive_workspace_with_render(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
) -> (
    uniterm_core::ProjectId,
    Vec<uniterm_proto::ProjectInfo>,
    String,
) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buffer = [0u8; 8192];
    let mut rendered = String::new();
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => decoder.push(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("Workspace read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            match message {
                ServerMessage::RenderOps(ops) => {
                    rendered.push_str(&String::from_utf8_lossy(&ops));
                }
                ServerMessage::Workspace {
                    active_project,
                    projects,
                    ..
                } => return (active_project, projects, rendered),
                _ => {}
            }
        }
    }
    panic!("Workspace projection did not arrive");
}

fn wait_for_lines(path: &std::path::Path, count: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let lines: Vec<String> = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        if lines.len() >= count {
            return lines;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("{} never reached {count} lines", path.display());
}

fn receive_import(stream: &mut UnixStream, decoder: &mut FrameDecoder) -> (u32, u32, u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buffer = [0u8; 8192];
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => decoder.push(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("import read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::WorkspaceImported {
                projects_added,
                tabs_added,
                projects_merged,
                error,
            } = message
            {
                assert_eq!(error, None);
                return (projects_added, tabs_added, projects_merged);
            }
        }
    }
    panic!("Workspace import result did not arrive");
}

fn receive_project_created(stream: &mut UnixStream, decoder: &mut FrameDecoder) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buffer = [0u8; 8192];
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => decoder.push(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("Project creation read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::ProjectCreated { error } = message {
                return error;
            }
        }
    }
    panic!("Project creation result did not arrive");
}

fn receive_workspaces(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
) -> (String, Vec<uniterm_proto::WorkspaceInfo>) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buffer = [0u8; 8192];
    while Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => decoder.push(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("Workspace catalog read failed: {error}"),
        }
        while let Ok(Some(message)) = decoder.decode::<ServerMessage>() {
            if let ServerMessage::Workspaces { current, entries } = message {
                return (current, entries);
            }
        }
    }
    panic!("Workspace catalog did not arrive");
}

#[test]
fn desktop_hierarchy_import_creates_fresh_projects_and_tabs() {
    isolate_state();
    let base = std::env::temp_dir().join(format!(
        "uniterm-desktop-hierarchy-import-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base).unwrap();
    let new_root = base.join("NewProject");
    std::fs::create_dir_all(&new_root).unwrap();
    let socket = base.join("Imported.sock");
    let server_socket = socket.clone();
    let root = std::env::current_dir().unwrap();
    let root_text = root.to_string_lossy().into_owned();
    let temp_root = std::env::temp_dir().to_string_lossy().into_owned();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 20"], 100, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let mut client = UnixStream::connect(&socket).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    client
        .write_all(&encode_frame(&ClientMessage::WorkspaceImport {
            workspace: ImportedWorkspace {
                source_id: "desktop-work".into(),
                projects: vec![
                    ImportedProject {
                        source_id: "desktop-p1".into(),
                        name: "Uniterm".into(),
                        root: root_text.clone(),
                        tabs: vec![
                            ImportedTab {
                                name: Some("Build".into()),
                            },
                            ImportedTab { name: None },
                        ],
                    },
                    ImportedProject {
                        source_id: "desktop-p2".into(),
                        name: "Scratch".into(),
                        root: temp_root.clone(),
                        tabs: vec![ImportedTab {
                            name: Some("Notes".into()),
                        }],
                    },
                ],
            },
            mode: WorkspaceImportMode::Fresh,
        }))
        .unwrap();
    assert_eq!(receive_import(&mut client, &mut decoder), (2, 3, 0));

    client
        .write_all(&encode_frame(&ClientMessage::WorkspaceState))
        .unwrap();
    let (_, projects) = receive_workspace(&mut client, &mut decoder);
    assert_eq!(projects.len(), 2);
    assert_eq!(projects[0].name, "Uniterm");
    assert_eq!(projects[0].tabs, 2);
    assert_eq!(projects[1].name, "Scratch");
    assert_eq!(projects[1].tabs, 1);

    // A migrated Workspace remains a normal mutable Workspace. In particular,
    // importing its hierarchy must not prevent subsequent Project creation.
    client
        .write_all(&encode_frame(&ClientMessage::ProjectCreate {
            name: "Added later".into(),
            root: new_root.to_string_lossy().into_owned(),
        }))
        .unwrap();
    let (active, projects) = receive_workspace(&mut client, &mut decoder);
    assert_eq!(projects.len(), 3);
    assert_eq!(
        projects
            .iter()
            .find(|project| project.id == active)
            .unwrap()
            .name,
        "Added later"
    );

    client
        .write_all(&encode_frame(&ClientMessage::WorkspaceImport {
            workspace: ImportedWorkspace {
                source_id: "desktop-work".into(),
                projects: vec![
                    ImportedProject {
                        source_id: "desktop-p1".into(),
                        name: "Must not replace Uniterm".into(),
                        root: root_text,
                        tabs: vec![
                            ImportedTab { name: None },
                            ImportedTab { name: None },
                            ImportedTab {
                                name: Some("Third".into()),
                            },
                        ],
                    },
                    ImportedProject {
                        source_id: "desktop-p2".into(),
                        name: "Must not replace Scratch".into(),
                        root: temp_root,
                        tabs: vec![ImportedTab { name: None }],
                    },
                ],
            },
            mode: WorkspaceImportMode::Merge,
        }))
        .unwrap();
    assert_eq!(receive_import(&mut client, &mut decoder), (0, 1, 2));
    client
        .write_all(&encode_frame(&ClientMessage::WorkspaceState))
        .unwrap();
    let (_, projects) = receive_workspace(&mut client, &mut decoder);
    assert_eq!(projects[0].name, "Uniterm");
    assert_eq!(projects[0].tabs, 3);
    assert_eq!(projects[1].name, "Scratch");
    assert_eq!(projects[1].tabs, 1);

    client
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn projects_own_tabs_and_switch_as_one_scope() {
    isolate_state();
    let base =
        std::env::temp_dir().join(format!("uniterm-workspace-projects-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let workspace = unique_workspace_name();
    let socket = base.join(format!("{workspace}.sock"));
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) =
            Server::bind(&server_socket, "/bin/sh", &["-c", "sleep 20"], 100, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
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
        .write_all(&encode_frame(&ClientMessage::WorkspaceList))
        .unwrap();
    let (current, entries) = receive_workspaces(&mut client, &mut decoder);
    assert_eq!(current, workspace);
    let current = entries
        .iter()
        .find(|entry| entry.name == workspace)
        .expect("current Workspace missing from host catalog");
    assert!(current.running);
    assert_eq!(current.projects, 1);

    client
        .write_all(&encode_frame(&ClientMessage::ProjectCreate {
            name: "Missing".into(),
            root: base.join("missing").to_string_lossy().into_owned(),
        }))
        .unwrap();
    let error = receive_project_created(&mut client, &mut decoder)
        .expect("a missing host folder was accepted");
    assert!(error.contains("Could not open"), "{error}");
    let (_, projects) = receive_workspace(&mut client, &mut decoder);
    assert_eq!(projects.len(), 1);

    client
        .write_all(&encode_frame(&ClientMessage::ProjectCreate {
            name: "Second".into(),
            root: "/tmp".into(),
        }))
        .unwrap();
    let (second, projects) = receive_workspace(&mut client, &mut decoder);
    assert_eq!(projects.len(), 2);
    assert_eq!(projects.iter().find(|p| p.id == second).unwrap().tabs, 1);
    let first = projects
        .iter()
        .find(|project| project.id != second)
        .unwrap()
        .clone();

    client
        .write_all(&encode_frame(&ClientMessage::ProjectMove {
            project: second,
            direction: ProjectMoveDirection::Up,
        }))
        .unwrap();
    let (_, projects, rendered) = receive_workspace_with_render(&mut client, &mut decoder);
    assert_eq!(
        projects
            .iter()
            .map(|project| project.id)
            .collect::<Vec<_>>(),
        [second, first.id]
    );
    let latest = rendered.rsplit("\x1b[r\x1b[2J").next().unwrap_or(&rendered);
    let second_row = latest.find("Second").expect("repaint omitted Second");
    let first_row = latest
        .find(&first.name)
        .expect("repaint omitted the first Project");
    assert!(
        second_row < first_row,
        "sidebar repaint did not reflect the reordered projection"
    );

    client
        .write_all(&encode_frame(&ClientMessage::ProjectMove {
            project: second,
            direction: ProjectMoveDirection::Down,
        }))
        .unwrap();
    let (_, projects) = receive_workspace(&mut client, &mut decoder);
    assert_eq!(
        projects
            .iter()
            .map(|project| project.id)
            .collect::<Vec<_>>(),
        [first.id, second]
    );

    client
        .write_all(&encode_frame(&ClientMessage::Command(Command::NewWindow)))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::WorkspaceState))
        .unwrap();
    let (_, projects) = receive_workspace(&mut client, &mut decoder);
    assert_eq!(projects.iter().find(|p| p.id == second).unwrap().tabs, 2);
    assert_eq!(projects.iter().find(|p| p.id == second).unwrap().panes, 2);

    client
        .write_all(&encode_frame(&ClientMessage::Command(Command::KillWindow)))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::WorkspaceState))
        .unwrap();
    let (active, projects) = receive_workspace(&mut client, &mut decoder);
    assert_eq!(active, second, "closing a Tab must stay in its Project");
    assert_eq!(projects.iter().find(|p| p.id == second).unwrap().tabs, 1);

    let first = first.id;
    // The first Project occupies both 1-based rows y=5 and y=6 after the
    // sidebar's top padding, heading, and title-to-list gap. Clicking its
    // detail row is still a first-class switch.
    client
        .write_all(&encode_frame(&ClientMessage::Mouse {
            x: 2,
            y: 6,
            kind: MouseKind::Click,
        }))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::WorkspaceState))
        .unwrap();
    let (active, _) = receive_workspace(&mut client, &mut decoder);
    assert_eq!(active, first);

    client
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn every_new_tab_and_pane_starts_at_the_project_root() {
    isolate_state();
    let base = std::env::temp_dir().join(format!("uniterm-project-cwd-{}", std::process::id()));
    let root = base.join("SelectedProject");
    std::fs::create_dir_all(&root).unwrap();
    let observed = base.join("cwd.txt");
    let record_cwd = format!("pwd >> '{}'\n", observed.display());
    let socket = base.join("cwd.sock");
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&server_socket, "/bin/sh", &[], 100, 30).unwrap();
        let _ = server.run(&mut poll);
    });
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let mut client = UnixStream::connect(&socket).unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::ProjectCreate {
            name: "SelectedProject".into(),
            root: root.to_string_lossy().into_owned(),
        }))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Input(
            record_cwd.as_bytes().to_vec(),
        )))
        .unwrap();
    wait_for_lines(&observed, 1);

    client
        .write_all(&encode_frame(&ClientMessage::Command(Command::NewWindow)))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Input(
            record_cwd.as_bytes().to_vec(),
        )))
        .unwrap();
    wait_for_lines(&observed, 2);
    client
        .write_all(&encode_frame(&ClientMessage::Command(Command::Split(
            uniterm_proto::SplitAxis::LeftRight,
        ))))
        .unwrap();
    client
        .write_all(&encode_frame(&ClientMessage::Input(
            record_cwd.as_bytes().to_vec(),
        )))
        .unwrap();
    let lines = wait_for_lines(&observed, 3);
    let canonical_root = std::fs::canonicalize(&root)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        lines.iter().all(|line| line == &canonical_root),
        "Project panes started in unexpected directories: {lines:?}"
    );

    client
        .write_all(&encode_frame(&ClientMessage::KillServer))
        .unwrap();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(base);
}
