//! Keep lock lifetime assertions in a process that never spawns PTY children.
//! A concurrent fork can inherit an open `flock` until exec, delaying release
//! after the owning guard drops. Library tests also share environment variables,
//! so changing their state directory can race another test's Workspace binding.

mod common;

use uniterm_server::{persist, server::WorkspaceLock};

#[test]
fn workspace_lock_outlives_the_socket_path() {
    let state = common::isolate_state();
    let dir = common::temp_dir("lifetime-lock");
    let name = common::unique_workspace_name();
    let path = dir.join(format!("{name}.sock"));
    let first = WorkspaceLock::acquire(&path).unwrap();
    // The durable claim lives with the durable files, not the socket.
    assert!(persist::lock_path(&name).starts_with(&state));
    assert!(persist::lock_path(&name).is_file());
    std::fs::write(&path, b"pathname placeholder").unwrap();
    std::fs::remove_file(&path).unwrap();

    let error = WorkspaceLock::acquire(&path).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);

    drop(first);
    let replacement = WorkspaceLock::acquire(&path).unwrap();
    drop(replacement);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&state).unwrap();
}
