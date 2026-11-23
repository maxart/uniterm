//! Workspace claims follow authoritative rename publication, even if shutdown
//! times out while the ordered durability worker is still running.

use super::reaper::common;
use super::*;
use std::time::{Duration, Instant};

fn fixture() -> (Server, Poll, PathBuf) {
    common::isolate_state();
    let name = common::unique_workspace_name();
    let dir = common::socket_root().join(&name);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join(format!("{name}.sock"));
    let (server, poll) = Server::bind(&socket, "/bin/sh", &[], 80, 24).unwrap();
    (server, poll, socket)
}

fn pump_until(server: &mut Server, poll: &Poll, mut done: impl FnMut(&Server) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut observed = false;
        for reply in server.agents.drain() {
            server.on_agent_reply(poll.registry(), reply);
            observed |= done(server);
        }
        if observed || done(server) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "rename worker did not reach its expected boundary"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn wait_for_released_claim(socket: &Path) {
    // Concurrent PTY tests can fork while the guard is held. Their children
    // briefly retain the flock until exec closes the inherited descriptor.
    // Keep the immediate rejection checks before acknowledgment; after it,
    // allow only this bounded kernel handoff, never an unrelated I/O error.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match WorkspaceLock::acquire(socket) {
            Ok(_) => return,
            Err(error)
                if error.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("completed rename retained its Workspace claim: {error}"),
        }
    }
}

#[test]
fn delayed_rename_holds_old_name_until_exact_success_acknowledgment() {
    let (mut server, mut poll, old_socket) = fixture();
    let old = server.name.clone();
    let new = common::unique_workspace_name();
    let gate = crate::runtime::hold_rename(&old);
    server.rename_session(poll.registry(), &new);
    pump_until(&mut server, &poll, |_| gate.entered.try_recv().is_ok());
    let request = *server.pending_workspace_renames.keys().next().unwrap();
    assert!(WorkspaceLock::acquire(&old_socket).is_err());
    assert!(crate::eventlog::exists(&old));
    assert!(!crate::eventlog::exists(&new));
    for (request, old, new) in [
        (request + 1, old.clone(), new.clone()),
        (request, old.clone(), "another-name".into()),
        (request, "another-name".into(), new.clone()),
    ] {
        server.on_agent_reply(
            poll.registry(),
            uniterm_proto::AgentToCore::EventRenamed {
                request,
                old,
                new,
                error: None,
            },
        );
        assert!(WorkspaceLock::acquire(&old_socket).is_err());
    }
    gate.release();
    pump_until(&mut server, &poll, |server| {
        server.pending_workspace_renames.is_empty()
    });
    assert!(!crate::eventlog::exists(&old));
    assert!(crate::eventlog::exists(&new));
    wait_for_released_claim(&old_socket);
    assert!(WorkspaceLock::acquire(&server.sock_path).is_err());
    server.retire_all_ptys(&mut poll);
}

#[test]
fn failed_rename_keeps_both_names_owned_and_surfaces_durability_error() {
    let (mut server, mut poll, old_socket) = fixture();
    let old = server.name.clone();
    let new = common::unique_workspace_name();
    let gate = crate::runtime::hold_rename(&old);
    server.rename_session(poll.registry(), &new);
    pump_until(&mut server, &poll, |_| gate.entered.try_recv().is_ok());
    let new_log = crate::eventlog::EventLog::path(&new);
    // Install the failure only after preflight, while the actual worker waits.
    std::fs::create_dir_all(&new_log).unwrap();
    gate.release();
    pump_until(&mut server, &poll, |server| {
        server
            .durability_error
            .as_deref()
            .is_some_and(|error| error.contains("rename"))
    });
    assert_eq!(server.pending_workspace_renames.len(), 1);
    assert!(WorkspaceLock::acquire(&old_socket).is_err());
    assert!(WorkspaceLock::acquire(&server.sock_path).is_err());
    assert!(crate::eventlog::exists(&old));
    // Later skipped writes may update the chrome diagnostic; pump_until
    // already observed the specific rename failure at its acknowledgment.
    assert!(server.durability_error.is_some());
    server.retire_all_ptys(&mut poll);
}

#[test]
fn repeated_renames_release_only_their_own_completed_claims() {
    let (mut server, mut poll, original_socket) = fixture();
    let original = server.name.clone();
    let middle = common::unique_workspace_name();
    let final_name = common::unique_workspace_name();
    let first_gate = crate::runtime::hold_rename(&original);
    let second_gate = crate::runtime::hold_rename(&middle);
    server.rename_session(poll.registry(), &middle);
    pump_until(&mut server, &poll, |_| {
        first_gate.entered.try_recv().is_ok()
    });
    let middle_socket = server.sock_path.clone();
    server.rename_session(poll.registry(), &final_name);
    assert_eq!(server.pending_workspace_renames.len(), 2);
    assert!(WorkspaceLock::acquire(&original_socket).is_err());
    assert!(WorkspaceLock::acquire(&middle_socket).is_err());
    first_gate.release();
    let mut second_entered = false;
    pump_until(&mut server, &poll, |server| {
        second_entered |= second_gate.entered.try_recv().is_ok();
        second_entered && server.pending_workspace_renames.len() == 1
    });
    assert_eq!(server.pending_workspace_renames.len(), 1);
    wait_for_released_claim(&original_socket);
    assert!(WorkspaceLock::acquire(&middle_socket).is_err());
    second_gate.release();
    pump_until(&mut server, &poll, |server| {
        server.pending_workspace_renames.is_empty()
    });
    wait_for_released_claim(&middle_socket);
    assert!(crate::eventlog::exists(&final_name));
    server.retire_all_ptys(&mut poll);
}

#[test]
fn shutdown_timeout_retains_old_and_current_claims_until_worker_exit() {
    let (mut server, mut poll, old_socket) = fixture();
    let old = server.name.clone();
    let new = common::unique_workspace_name();
    let gate = crate::runtime::hold_rename(&old);
    server.rename_session(poll.registry(), &new);
    pump_until(&mut server, &poll, |_| gate.entered.try_recv().is_ok());
    let new_socket = server.sock_path.clone();
    server.retire_all_ptys(&mut poll);
    server.agents.shutdown_with_bound(Duration::from_millis(20));
    drop(server);
    assert!(WorkspaceLock::acquire(&old_socket).is_err());
    assert!(WorkspaceLock::acquire(&new_socket).is_err());
    gate.release();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let (Ok(_old), Ok(_new)) = (
            WorkspaceLock::acquire(&old_socket),
            WorkspaceLock::acquire(&new_socket),
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "completed worker retained Workspace claims"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!crate::eventlog::exists(&old));
    assert!(crate::eventlog::exists(&new));
}
