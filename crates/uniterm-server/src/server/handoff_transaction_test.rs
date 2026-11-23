//! Two-process qualification harness. Only the committed owner consumes the
//! public listener, writes to the transferred PTY or appends to the serving
//! event stream. Candidate preparation checks the acknowledged durable cursor.

use super::super::handoff::{
    read_checkpoint, receive_descriptors, send_descriptors, write_checkpoint,
};
use super::{live_update_test, Server, WorkspaceLock};
use crate::pty::PtyProcess;
use crate::server::reaper::common;
use bincode::Options as _;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Command, Stdio};
use uniterm_core::{AgentStatus, PaneId};
use uniterm_proto::WaitingAction;

const TEST: &str =
    "server::socket::handoff_transaction_test::pending_approval_survives_two_process_handoff";
const LIMIT: usize = 1024 * 1024;
const SCRIPT: &str = r#"stty raw -echo || exit 1
printf 'Would you like to run the following command?\r\n echo test\r\n› 1. Yes, proceed (y)\r\n  2. No, and tell Codex what to do differently (esc)\r\nPress enter to confirm\r\n'
bytes=$(dd bs=1 count=2 2>/dev/null | od -An -tu1)
printf 'received %s %s\n' "$$" "$bytes" > received
exit 17"#;

// The child keeps the original selection visible until both retried approvals
// have received a reply. Only the provider's repaint may advance the action.
const NAVIGATE: &str = r#"stty raw -echo || exit 1
printf 'Do you want to proceed?\r\n❯ 2. No, continue without running it\r\n1. Yes\r\nEnter to confirm · Esc to cancel'
dd bs=1 count=3 2>/dev/null | od -An -tu1 > navigation
dd bs=1 count=1 2>/dev/null | od -An -tu1 > refresh
printf '\033[2J\033[HDo you want to proceed?\r\n2. No, continue without running it\r\n❯ 1. Yes\r\nEnter to confirm · Esc to cancel'
dd bs=1 count=1 2>/dev/null | od -An -tu1 > approved
printf '\r\nnavigation-confirmed\r\n'
bytes=$(dd bs=1 count=1 2>/dev/null | od -An -tu1)
printf 'received %s %s %s\n' "$$" "$(cat approved)" "$bytes" > received
exit 17"#;

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    inbox: Vec<u8>,
    native_sessions: Vec<u8>,
    control_identities: [((u64, u64), (u64, u64)); 3],
    pane: PaneId,
    pid: i32,
    pane_state: Vec<u8>,
    terminal: Vec<u8>,
    sequence: u64,
    client: Vec<u8>,
    layout: super::super::handoff::WorkspaceLayout,
    fingerprint: u64,
    navigation: Option<u64>,
    keys: Vec<u8>,
    model: super::super::handoff::WorkspaceModel,
}

fn helper(phase: &str, socket: &Path) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([TEST, "--exact", "--nocapture"])
        .env("UT_TEST_TWO_PHASE", phase)
        .env("UT_TEST_TWO_SOCKET", socket);
    cmd
}

fn exclude_contender(socket: &Path) {
    let output = live_update_test::helper("claim", socket).output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

fn expect_byte(channel: &mut UnixStream, expected: u8) {
    let mut tag = [0u8];
    channel.read_exact(&mut tag).unwrap();
    assert_eq!(tag[0], expected);
}

fn duplicate(fd: RawFd) -> OwnedFd {
    // SAFETY: callers retain the original PTY or listener throughout this call.
    unsafe { BorrowedFd::borrow_raw(fd) }
        .try_clone_to_owned()
        .unwrap()
}

// Bounded blocking observation by the fixture only, while its engine is paused.
fn read_until(fd: RawFd, suffix: &[u8]) -> Vec<u8> {
    let mut file = File::from(duplicate(fd));
    let mut bytes = Vec::new();
    while !bytes.ends_with(suffix) {
        let mut readiness = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: readiness describes exactly one live descriptor.
        assert_eq!(unsafe { libc::poll(&mut readiness, 1, 5000) }, 1);
        let mut buf = [0; 256];
        let n = file.read(&mut buf).unwrap();
        assert!(n > 0 && bytes.len() + n <= LIMIT);
        bytes.extend_from_slice(&buf[..n]);
    }
    bytes
}

fn serve_to_exit(server: &mut Server, poll: &mut mio::Poll) {
    server.run_loop(poll).unwrap();
    server.drain_clients_before_exit(poll);
    server.retire_all_ptys(poll);
    server.agents.shutdown();
    crate::eventlog::sync(server.name()).unwrap();
    crate::eventlog::EventLog::resume_at(server.name(), server.log.current_sequence()).unwrap();
}

// Stop dispatching new work in this isolated fixture while the real ordered
// persistence worker proves its cursor. Apply every earlier reply normally.
fn wait_for_reply(
    engine: &mut Server,
    poll: &mut mio::Poll,
    mut completed: impl FnMut(&uniterm_proto::AgentToCore) -> bool,
) {
    let mut events = mio::Events::with_capacity(16);
    loop {
        let mut done = false;
        for reply in engine.agents.drain() {
            if completed(&reply) {
                assert!(!done, "duplicate runtime acknowledgement");
                done = true;
            } else {
                engine.on_agent_reply(poll.registry(), reply);
            }
        }
        if done {
            return;
        }
        poll.poll(&mut events, Some(std::time::Duration::from_secs(5)))
            .unwrap();
        assert!(
            !events.is_empty(),
            "runtime did not acknowledge handoff request"
        );
    }
}

fn pause_control_admission(
    engine: &mut Server,
    poll: &mut mio::Poll,
) -> uniterm_proto::HandoffControlState {
    let mut listeners = None;
    engine
        .agents
        .send(uniterm_proto::CoreToAgent::HandoffControlAdmission {
            through: None,
            workspace: engine.name().to_owned(),
            transaction: 1,
            pause: true,
        });
    wait_for_reply(engine, poll, |reply| {
        if let uniterm_proto::AgentToCore::HandoffControlAdmissionChanged {
            transaction,
            result,
        } = reply
        {
            assert_eq!(*transaction, 1);
            listeners = result.as_ref().unwrap().clone();
            true
        } else {
            false
        }
    });
    listeners.unwrap()
}

fn flush_log(engine: &mut Server, poll: &mut mio::Poll) {
    let expected = engine.log.current_sequence();
    engine
        .agents
        .send(uniterm_proto::CoreToAgent::VerifyEventCursor {
            workspace: engine.name().to_owned(),
            through: expected,
        });
    let expected_workspace = engine.name().to_string();
    wait_for_reply(engine, poll, |reply| {
        if let uniterm_proto::AgentToCore::EventCursorVerified {
            workspace,
            through,
            result,
        } = reply
        {
            assert_eq!(workspace, &expected_workspace);
            assert_eq!(*through, expected);
            result.as_ref().unwrap();
            true
        } else {
            false
        }
    });
    assert_eq!(engine.log.current_sequence(), expected);
    assert!(engine.durability_error.is_none());
}

fn candidate(socket: &Path) {
    let mut channel = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    let [master, listener, socket_claim, state_claim, peer] =
        receive_descriptors(&channel).unwrap();
    let claim = WorkspaceLock {
        _socket_claim: socket_claim.into(),
        _state_claim: state_claim.into(),
    };
    let listener = UnixListener::from(listener);
    let payload = read_checkpoint(&channel).unwrap();
    let cp: Checkpoint = serde_json::from_slice(&payload).unwrap();
    let [public, manager, inbox] = receive_descriptors(&channel).unwrap();
    if cp.version != 1 {
        channel.write_all(b"N").unwrap();
        return;
    }
    let [public_id, manager_id, inbox_id] = cp.control_identities;
    let inherited = |fd: OwnedFd, (identity, socket_identity)| uniterm_proto::HandoffListener {
        file: std::sync::Arc::new(File::from(fd)),
        identity,
        socket_identity,
    };
    let control_listeners = uniterm_proto::HandoffControlListeners {
        control: inherited(public, public_id),
        manager: Some(inherited(manager, manager_id)),
        inbox: inherited(inbox, inbox_id),
    };
    let control_handoff = uniterm_proto::HandoffControlState {
        listeners: control_listeners,
        inbox: cp.inbox.into(),
        native_sessions: cp.native_sessions.into(),
    };
    if let Err(error) = crate::runtime::prepare_handoff_control(
        &socket.with_extension("control.sock"),
        &control_handoff,
    ) {
        eprintln!("candidate declined control handoff: {error}");
        channel.write_all(b"N").unwrap();
        return;
    }
    let pty = PtyProcess::from_handoff(master, cp.pid).unwrap();
    assert_eq!(pty.foreground_process_group(), Some(cp.pid));
    let terminal = crate::terminal::Terminal::from_live_checkpoint(&cp.terminal).unwrap();
    // A new release may have changed either the recognizer or its hashing.
    // Decline before commit if the same screen no longer means the same action.
    let Ok(state) = serde_json::from_slice::<super::super::PaneState>(&cp.pane_state) else {
        channel.write_all(b"N").unwrap();
        return;
    };
    let agent = state.agent.as_ref().unwrap();
    assert_eq!(agent.foreground_pid, Some(cp.pid));
    let recognized = crate::providers::permission_approval(&agent.id, &terminal);
    if !recognized.is_some_and(|choice| {
        choice.kind == crate::providers::ApprovalKind::Permission
            && choice.navigation == cp.navigation
            && choice.fingerprint == cp.fingerprint
            && choice.keys == cp.keys
    }) {
        channel.write_all(b"N").unwrap();
        return;
    }
    // Library-test helpers have private state directories. Link just the
    // source's unique log so both workers exercise the same durable inode.
    let workspace = socket.file_stem().unwrap().to_str().unwrap();
    let path = crate::eventlog::EventLog::path(workspace);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::hard_link(std::env::var_os("UT_TEST_HANDOFF_LOG").unwrap(), &path).unwrap();
    let Ok(writer) = crate::eventlog::EventLog::resume_at(workspace, cp.sequence) else {
        channel.write_all(b"N").unwrap();
        return;
    };
    let session: super::super::ClientSession = bincode::DefaultOptions::new()
        .with_limit(LIMIT as u64)
        .deserialize(&cp.client)
        .unwrap();
    if session.validate().is_err() {
        channel.write_all(b"N").unwrap();
        return;
    }
    claim.handoff_range(libc::F_WRLCK as _, 1, false).unwrap();
    // The candidate serves the original Workspace without running the
    // cold-start shell, crash recovery, or a second launch event.
    let bound = super::super::BoundWorkspace {
        control_handoff: Some((control_handoff, writer)),
        listener: mio::net::UnixListener::from_std(listener.try_clone().unwrap()),
        identity: super::socket_identity(socket).unwrap(),
        claim,
    };
    let (mut engine, mut poll) =
        Server::from_bound_workspace(socket, workspace, "/bin/sh", (80, 24), bound, false).unwrap();
    engine
        .register_pane(poll.registry(), cp.pane, pty, terminal, state)
        .unwrap();
    cp.layout.install(&mut engine).unwrap();
    cp.model.install(&mut engine).unwrap();
    assert_eq!(engine.waiting.items()[0].invocation, Some(cp.pid));
    engine
        .register_client(
            poll.registry(),
            mio::net::UnixStream::from_std(UnixStream::from(peer)),
            session,
        )
        .unwrap();
    // The candidate now owns a working poller and registered sockets, but its
    // acceptors and PTY readers are dormant. A clean abort must preserve all
    // original socket names and kernel backlogs, just like a candidate crash.
    if let Ok(mode) = std::env::var("UT_TEST_DECLINE_READY") {
        drop(engine);
        match mode.as_str() {
            "prepared-abort" => channel.write_all(b"N").unwrap(),
            "invalid-vote" => channel.write_all(b"?").unwrap(),
            "commit-write-error" => {
                channel.shutdown(std::net::Shutdown::Read).unwrap();
                channel.write_all(b"R").unwrap();
            }
            "lost-before-vote" => {}
            _ => unreachable!(),
        }
        return;
    }
    channel.write_all(b"R").unwrap();
    expect_byte(&mut channel, b'C');
    engine
        .workspace_lock
        .as_ref()
        .unwrap()
        .handoff_range(libc::F_WRLCK as _, 0, true)
        .unwrap();
    exclude_contender(socket);
    engine.event_writes_enabled = true;
    serve_to_exit(&mut engine, &mut poll);
}

fn source(socket: &Path, scenario: &str) {
    let (script, provider, suffix): (_, _, &[u8]) = if scenario == "navigation" {
        (
            NAVIGATE,
            "claude",
            "Enter to confirm · Esc to cancel".as_bytes(),
        )
    } else {
        (SCRIPT, "codex", b"Press enter to confirm\r\n")
    };
    let partial = script.replace(
        "bytes=$(dd",
        "dd bs=1 count=2 2>/dev/null > partial-input\nprintf x > input-drained\nbytes=$(dd",
    );
    let script = if scenario == "partial-pty-input" {
        &partial
    } else {
        script
    };
    let (mut engine, mut poll) = Server::bind(socket, "/bin/sh", &["-c", script], 80, 24).unwrap();
    let pane = *engine.panes.keys().next().unwrap();
    let master = engine.panes[&pane].pty.raw_fd();
    let pid = engine.panes[&pane].pty.child_pid().unwrap() as i32;
    let screen = read_until(master, suffix);
    engine.panes.get_mut(&pane).unwrap().term.feed(&screen);
    if scenario == "partial-pty-input" {
        let target = engine.panes.get_mut(&pane).unwrap();
        assert_eq!(target.pty.write_some(b"A").unwrap(), 1);
        target.input = b"AB".to_vec();
        target.input_offset = 1;
    }
    engine.bind_agent(pane, provider);
    engine
        .panes
        .get_mut(&pane)
        .unwrap()
        .agent
        .as_mut()
        .unwrap()
        .status = AgentStatus::Permission;
    engine.sync_waiting_agent(pane, AgentStatus::Permission);
    let id = engine.waiting.items()[0].id;
    let delivered = matches!(scenario, "retry" | "fingerprint-mismatch" | "navigation");
    if delivered {
        assert_eq!(
            engine.waiting_action(poll.registry(), id, WaitingAction::Approve, ""),
            (true, true)
        );
    }
    if scenario == "busy-editor" {
        // Hold the real external-editor worker on a FIFO until the old
        // engine has acknowledged our upgrade attempt. No timing races.
        use std::os::unix::ffi::OsStrExt;
        let fifo = socket.with_extension("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: path is a valid C string in the test's private directory.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let mut release = File::options().read(true).write(true).open(&fifo).unwrap();
        engine
            .agents
            .send(uniterm_proto::CoreToAgent::EditorLaunchExternal {
                project: engine.active_project,
                path: fifo.to_str().unwrap().into(),
                command: r#"/bin/sh -c 'read line < "$0"'"#.into(),
                cwd: socket.parent().unwrap().to_str().unwrap().into(),
            });
        let assert_busy = |engine: &mut Server, poll: &mut mio::Poll| {
            engine
                .agents
                .send(uniterm_proto::CoreToAgent::HandoffControlAdmission {
                    through: None,
                    workspace: engine.name().into(),
                    transaction: 1,
                    pause: true,
                });
            wait_for_reply(engine, poll, |reply| match reply {
                uniterm_proto::AgentToCore::HandoffControlAdmissionChanged { result, .. } => {
                    assert!(
                        matches!(result, Err(message) if message.contains("background operation"))
                    );
                    true
                }
                _ => false,
            });
        };
        assert_busy(&mut engine, &mut poll);
        release.write_all(b"done\n").unwrap();
        let mut held = None;
        wait_for_reply(&mut engine, &mut poll, |reply| {
            if let uniterm_proto::AgentToCore::EditorExternalFinished { error, .. } = reply {
                assert!(error.is_none());
                held = Some(reply.clone());
                return true;
            }
            false
        });
        // An outstanding editor result still fences handoff. Once the last
        // copy is consumed, admission must succeed independently of scheduling.
        assert_busy(&mut engine, &mut poll);
        drop(held.take().unwrap());
    }
    let control_state = pause_control_admission(&mut engine, &mut poll);
    let control_listeners = control_state.listeners;
    flush_log(&mut engine, &mut poll);
    let sequence = engine.log.current_sequence();
    let mut observer = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    observer.write_all(&id.to_be_bytes()).unwrap();
    observer.write_all(&pane.0.to_be_bytes()).unwrap();
    observer.write_all(&pid.to_be_bytes()).unwrap();
    expect_byte(&mut observer, b'Q');
    engine.on_accept(poll.registry());
    let token = *engine.clients.keys().next().unwrap();
    engine.on_client(poll.registry(), token, true, false);
    let client = engine.clients.get_mut(&token).unwrap();
    assert_eq!(client.decoder.pending_bytes().unwrap().len(), 3);
    // Interrupt an outgoing frame at a known byte offset too. The receiver
    // must send only the remaining suffix, through the usual writable path.
    let bell = uniterm_proto::encode_frame(&uniterm_proto::ServerMessage::Bell);
    client.queue(&bell);
    client.stream.write_all(&bell[..2]).unwrap();
    client.out_offset = 2;
    super::super::io::set_interest(poll.registry(), client, token).unwrap();
    let mut transferred_session = bincode::DefaultOptions::new()
        .serialize(&client.session)
        .unwrap();
    if scenario == "invalid-client-offset" {
        let mut invalid: super::super::ClientSession = bincode::DefaultOptions::new()
            .deserialize(&transferred_session)
            .unwrap();
        invalid.out_offset = invalid.outbuf.len() + 1;
        transferred_session = bincode::DefaultOptions::new().serialize(&invalid).unwrap();
    }
    let peer_fd = client.stream.as_raw_fd();
    observer.write_all(b"P").unwrap();
    let lock_path = crate::persist::lock_path(socket.file_stem().unwrap().to_str().unwrap());
    std::env::set_var("UT_TEST_HANDOFF_LOCK", lock_path);
    let (mut channel, child_channel) = UnixStream::pair().unwrap();
    let mut candidate = helper("candidate", socket);
    if matches!(
        scenario,
        "prepared-abort" | "invalid-vote" | "lost-before-vote" | "commit-write-error"
    ) {
        candidate.env("UT_TEST_DECLINE_READY", scenario);
    }
    let mut child = candidate
        .env(
            "UT_TEST_HANDOFF_LOG",
            crate::eventlog::EventLog::path(engine.name()),
        )
        .stdin(Stdio::from(OwnedFd::from(child_channel)))
        .spawn()
        .unwrap();
    drop(candidate); // Command retains its configured bootstrap stdin for reuse.
    let claim = engine.workspace_lock.as_ref().unwrap();
    // A competing full-file lock conflicts with byte zero even before the new
    // process owns the tail. SCM_RIGHTS alone cannot transfer a POSIX lock.
    let mut offer =
        super::super::handoff::SourceOffer::begin(claim, &mut child, &mut channel).unwrap();
    let channel = offer.channel();
    exclude_contender(socket);
    // SAFETY: the Pane and accepted client own these descriptors throughout sendmsg.
    let master_fd = unsafe { BorrowedFd::borrow_raw(master) };
    let peer_fd = unsafe { BorrowedFd::borrow_raw(peer_fd) };
    send_descriptors(
        channel,
        &[
            master_fd,
            engine.listener.as_fd(),
            claim._socket_claim.as_fd(),
            claim._state_claim.as_fd(),
            peer_fd,
        ],
    )
    .unwrap();
    let target = &engine.panes[&pane];
    let provider = target.agent.as_ref().unwrap().id.clone();
    let choice = crate::providers::permission_approval(&provider, &target.term).unwrap();
    let manager = control_listeners.manager.as_ref().unwrap();
    let controls = [
        &control_listeners.control,
        manager,
        &control_listeners.inbox,
    ];
    let cp = Checkpoint {
        inbox: control_state.inbox.to_vec(),
        native_sessions: control_state.native_sessions.to_vec(),
        control_identities: controls.map(|l| (l.identity, l.socket_identity)),
        version: if scenario == "incompatible" { 2 } else { 1 },
        pane,
        pid,
        pane_state: if scenario == "invalid-pane-offset" {
            let mut invalid = serde_json::to_value(&target.state).unwrap();
            invalid["input_offset"] = serde_json::json!(target.state.input.len() + 1);
            serde_json::to_vec(&invalid).unwrap()
        } else {
            serde_json::to_vec(&target.state).unwrap()
        },
        fingerprint: if scenario == "fingerprint-mismatch" {
            !choice.fingerprint
        } else {
            choice.fingerprint
        },
        navigation: choice.navigation,
        keys: choice.keys,
        terminal: target.term.live_checkpoint().unwrap(),
        layout: super::super::handoff::WorkspaceLayout::capture(&engine),
        sequence: if scenario == "stale-cursor" {
            sequence - 1
        } else {
            sequence
        },
        client: transferred_session,
        model: super::super::handoff::WorkspaceModel::capture(&engine).unwrap(),
    };
    let payload = serde_json::to_vec(&cp).unwrap();
    assert!(payload.len() <= LIMIT);
    write_checkpoint(channel, &payload).unwrap();
    let mut fds = controls.map(|l| l.file.as_fd());
    if scenario == "swapped-control-scopes" {
        fds.swap(0, 1);
    }
    send_descriptors(channel, &fds).unwrap();
    let vote = offer.await_ready();
    exclude_contender(socket);
    let committed = if scenario == "rollback" {
        vote.unwrap();
        offer.rollback().unwrap();
        false
    } else if vote.is_err() {
        offer.rollback().unwrap();
        false
    } else {
        offer.commit().is_ok()
    };
    assert_eq!(
        committed,
        !matches!(
            scenario,
            "incompatible"
                | "rollback"
                | "prepared-abort"
                | "invalid-vote"
                | "commit-write-error"
                | "lost-before-vote"
                | "fingerprint-mismatch"
                | "stale-cursor"
                | "swapped-control-scopes"
                | "invalid-client-offset"
                | "invalid-pane-offset"
        )
    );
    if !committed {
        // Cancellation reaps the abandoned candidate and restores the source range.
        assert!(child.try_wait().unwrap().is_some());
        exclude_contender(socket);
        // Resume the existing core poll immediately. Its edge-triggered
        // readiness includes requests that arrived while the source paused.
        // The runtime will reopen each control listener on its own reactor.
        engine
            .agents
            .send(uniterm_proto::CoreToAgent::HandoffControlAdmission {
                through: None,
                workspace: engine.name().to_owned(),
                transaction: 1,
                pause: false,
            });
        serve_to_exit(&mut engine, &mut poll);
        return;
    } else {
        // From this point the old server can reap its child, but must never
        // accept requests or replay input, even if the candidate loses its reply.
        let (retire, mut activated) = UnixStream::pair().unwrap();
        let keep_open = retire.try_clone().unwrap();
        engine
            .agents
            .send(uniterm_proto::CoreToAgent::RelinquishHandoffControl {
                retired: Some(std::sync::Arc::new(retire)),
                workspace: engine.name().to_owned(),
                transaction: 1,
            });
        wait_for_reply(&mut engine, &mut poll, |reply| {
            if let uniterm_proto::AgentToCore::HandoffControlAdmissionChanged {
                transaction: 1,
                result,
            } = reply
            {
                assert!(result.as_ref().unwrap().is_none());
                true
            } else {
                false
            }
        });
        activated
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        assert_eq!(
            activated.read(&mut [0]).unwrap(),
            0,
            "activation waited for runtime shutdown"
        );
        drop(keep_open);
        engine.agents.shutdown();
        drop(engine.workspace_lock.take());
        if scenario == "source-exit" {
            // Simulate losing the source process immediately after commit.
            // The observer waits for our exit before supplying the approval.
            std::process::exit(0);
        }
        assert!(child.wait().unwrap().success());
    }
    // SAFETY: pid is the source's own unreaped shell and status is writable.
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 17);
}

#[test]
fn pending_approval_survives_two_process_handoff() {
    if let Some(phase) = std::env::var_os("UT_TEST_TWO_PHASE") {
        common::isolate_state();
        let socket = std::env::var_os("UT_TEST_TWO_SOCKET").unwrap();
        let socket = Path::new(&socket);
        // SAFETY: each helper runs only this test. Bound all blocking IPC.
        unsafe { libc::alarm(30) };
        if phase == "candidate" {
            candidate(socket);
        } else {
            source(socket, phase.to_str().unwrap());
        }
        return;
    }
    for scenario in [
        "commit",
        "rollback",
        "prepared-abort",
        "invalid-vote",
        "lost-before-vote",
        "commit-write-error",
        "partial-pty-input",
        "retry",
        "incompatible",
        "fingerprint-mismatch",
        "stale-cursor",
        "swapped-control-scopes",
        "invalid-client-offset",
        "invalid-pane-offset",
        "source-exit",
        "busy-editor",
        "navigation",
    ] {
        let root = common::temp_dir("handoff");
        let socket = root.join(format!("{}.sock", common::unique_workspace_name()));
        let (mut observer, sender) = UnixStream::pair().unwrap();
        let timeout = Some(std::time::Duration::from_secs(40));
        observer.set_read_timeout(timeout).unwrap();
        let ack = (scenario == "partial-pty-input").then(|| {
            use std::os::unix::ffi::OsStrExt;
            let fifo = root.join("input-drained");
            let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
            // SAFETY: this is a new FIFO inside the fixture's private working directory.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            File::options().read(true).write(true).open(fifo).unwrap()
        });
        let mut source = helper(scenario, &socket)
            .env("TMPDIR", &root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_RUNTIME_DIR", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .current_dir(&root)
            .stdin(Stdio::from(OwnedFd::from(sender)))
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut id = [0; 8];
        observer.read_exact(&mut id).unwrap();
        let mut pane = [0; 8];
        let mut pid = [0; 4];
        observer.read_exact(&mut pane).unwrap();
        observer.read_exact(&mut pid).unwrap();
        let pane = PaneId(u64::from_be_bytes(pane));
        let pid = i32::from_be_bytes(pid);
        // Connect while the original acceptors are frozen. Queue the same
        // harmless query at each scope before allowing preparation to finish.
        let public = socket.with_extension("control.sock");
        let file_name = public.file_name().unwrap();
        let control_peers = [
            public.clone(),
            root.join("manager").join(file_name),
            root.join("inbox").join(file_name),
        ]
        .map(|path| {
            let mut stream = UnixStream::connect(path).unwrap();
            stream.set_read_timeout(timeout).unwrap();
            let request = uniterm_proto::ControlRequest {
                version: uniterm_proto::CONTROL_API_VERSION,
                id: 73,
                workspace: socket.file_stem().unwrap().to_str().unwrap().to_owned(),
                command: uniterm_proto::ControlCommand::PaneList,
            };
            serde_json::to_writer(&stream, &request).unwrap();
            stream.write_all(b"\n").unwrap();
            stream
        });
        let mut peer = UnixStream::connect(&socket).unwrap();
        peer.set_read_timeout(timeout).unwrap();
        let approve = uniterm_proto::encode_frame(&uniterm_proto::ClientMessage::WaitingAct {
            id: u64::from_be_bytes(id),
            action: WaitingAction::Approve,
            text: String::new(),
        });
        if scenario == "navigation" {
            peer.write_all(&uniterm_proto::encode_frame(
                &uniterm_proto::ClientMessage::PaneWaitOutput {
                    pane,
                    needle: "navigation-confirmed".into(),
                    timeout_ms: 10_000,
                },
            ))
            .unwrap();
        }
        peer.write_all(&approve[..3]).unwrap();
        observer.write_all(b"Q").unwrap();
        expect_byte(&mut observer, b'P');
        if scenario == "source-exit" {
            assert!(source.wait().unwrap().success());
        }
        for (stream, expected_error) in
            control_peers
                .into_iter()
                .zip([None, Some("read_only"), Some("inbox_only")])
        {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::BufReader::new(stream), &mut line)
                .unwrap_or_else(|error| panic!("{scenario}: {error}"));
            let uniterm_proto::ControlFrame::Response(reply) = serde_json::from_str(&line).unwrap()
            else {
                panic!("expected scoped reply")
            };
            assert_eq!(reply.id, 73);
            assert_eq!(
                reply.error.as_ref().map(|error| error.code.as_str()),
                expected_error
            );
            assert_eq!(reply.result.is_some(), expected_error.is_none());
        }
        if let Some(ack) = ack {
            let mut ready = libc::pollfd {
                fd: ack.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one initialized pollfd refers to the test FIFO.
            assert_eq!(
                unsafe { libc::poll(&mut ready, 1, 5000) },
                1,
                "pending PTY bytes needed another keystroke"
            );
            assert_eq!(std::fs::read(root.join("partial-input")).unwrap(), b"AB");
        }
        peer.write_all(&approve[3..]).unwrap();
        peer.write_all(&approve).unwrap();
        let sentinel = uniterm_proto::encode_frame(&uniterm_proto::ClientMessage::PaneSend {
            pane,
            bytes: vec![b'Z'],
        });
        if scenario != "navigation" {
            peer.write_all(&sentinel).unwrap();
        }
        let mut bell_seen = false;
        let mut results = Vec::new();
        let mut decoder = uniterm_proto::FrameDecoder::new();
        let mut buf = [0; 4096];
        while results.len() < 2 {
            let n = peer
                .read(&mut buf)
                .unwrap_or_else(|error| panic!("{scenario}: {error}"));
            assert!(n > 0, "closed before approval reply: {scenario}");
            decoder.push(&buf[..n]);
            while let Some(msg) = decoder.decode::<uniterm_proto::ServerMessage>().unwrap() {
                if matches!(msg, uniterm_proto::ServerMessage::Bell) {
                    assert!(!bell_seen && results.is_empty());
                    bell_seen = true;
                }
                if let uniterm_proto::ServerMessage::WaitingActed {
                    found, accepted, ..
                } = msg
                {
                    assert!(found && bell_seen);
                    results.push(u8::from(accepted));
                }
            }
        }
        assert_eq!(
            results,
            [
                u8::from(!matches!(
                    scenario,
                    "retry" | "fingerprint-mismatch" | "navigation"
                )),
                0
            ],
            "{scenario}"
        );
        if scenario == "navigation" {
            peer.write_all(&uniterm_proto::encode_frame(
                &uniterm_proto::ClientMessage::PaneSend {
                    pane,
                    bytes: vec![b'R'],
                },
            ))
            .unwrap();
            'wait: loop {
                let n = peer
                    .read(&mut buf)
                    .unwrap_or_else(|error| panic!("{scenario}: {error}"));
                assert!(
                    n > 0,
                    "connection closed before the original output wait completed"
                );
                decoder.push(&buf[..n]);
                while let Some(message) = decoder.decode::<uniterm_proto::ServerMessage>().unwrap()
                {
                    if let uniterm_proto::ServerMessage::PaneOutputWaited {
                        found,
                        matched,
                        timed_out,
                        ..
                    } = message
                    {
                        assert!(found && matched && !timed_out);
                        break 'wait;
                    }
                }
            }
            assert_eq!(
                std::fs::read_to_string(root.join("refresh"))
                    .unwrap()
                    .trim(),
                "82"
            );
            peer.write_all(&sentinel).unwrap();
            assert_eq!(
                std::fs::read_to_string(root.join("navigation"))
                    .unwrap()
                    .split_whitespace()
                    .collect::<Vec<_>>(),
                ["27", "91", "66"]
            );
        }
        // The candidate inherits stdout. EOF also joins its teardown when
        // the source was killed and can no longer reap that process.
        let output = source.wait_with_output().unwrap();
        assert!(output.status.success(), "{scenario}: {output:?}");
        let proof = std::fs::read_to_string(root.join("received")).unwrap();
        assert_eq!(
            proof.split_whitespace().collect::<Vec<_>>(),
            ["received", &pid.to_string(), "13", "90"]
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn control_handoff_preserves_kernel_backlogs_and_reopens_every_scope_on_failure() {
    use crate::control_api::{HandoffAdmission, Inbound, Listener};

    common::isolate_state();
    let root = common::temp_dir("ctl");
    let name = common::unique_workspace_name();
    let public = root.join(&name);
    let manager = root.join("manager").join(&name);
    let inbox = root.join("inbox").join(&name);
    let (tx, rx) = crossbeam_channel::bounded(32);
    let public_listener = Listener::bind(public.clone(), tx.clone(), 1).unwrap();
    let manager_listener = Listener::bind_scoped(manager.clone(), tx.clone(), 1, true).unwrap();
    let inbox_listener = Listener::bind(inbox.clone(), tx, 1).unwrap();
    let listeners = [
        Some(&public_listener),
        Some(&manager_listener),
        Some(&inbox_listener),
    ];
    let mut admission = HandoffAdmission::default();
    // No dispatcher runs in this test. Keep each accepted connection's
    // output sender alive just as the real dispatcher would.
    let receive = || rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
    let scope = |message: Inbound, manager, inbox| {
        let Inbound::Connected {
            read_only,
            inbox_only,
            output,
            ..
        } = message
        else {
            panic!("expected connection")
        };
        assert_eq!((read_only, inbox_only), (manager, inbox));
        output
    };
    let input = UnixStream::connect(&inbox).unwrap();
    let inbox_output = scope(receive(), false, true);
    assert!(admission
        .change(1, true, listeners, || false)
        .await
        .is_err());
    // The inbox vetoed after the public and manager listeners had prepared.
    let administrative = UnixStream::connect(&manager).unwrap();
    let manager_output = scope(receive(), true, false);
    drop((input, administrative, inbox_output, manager_output));
    for _ in 0..2 {
        assert!(matches!(receive(), Inbound::Disconnected { .. }));
    }
    assert!(admission.change(2, true, listeners, || true).await.is_err());
    admission
        .change(3, true, listeners, || false)
        .await
        .unwrap();
    admission
        .change(3, true, listeners, || false)
        .await
        .unwrap();
    let queued = [
        UnixStream::connect(&public).unwrap(),
        UnixStream::connect(&manager).unwrap(),
        UnixStream::connect(&inbox).unwrap(),
    ];
    assert!(admission
        .change(2, false, listeners, || false)
        .await
        .is_err());
    assert!(admission
        .change(4, true, listeners, || false)
        .await
        .is_err());
    assert!(rx
        .recv_timeout(std::time::Duration::from_millis(50))
        .is_err());
    admission
        .change(3, false, listeners, || false)
        .await
        .unwrap();
    // Each peer keeps its original kernel connection and capability scope.
    let mut scopes = Vec::new();
    let mut outputs = Vec::new();
    for _ in &queued {
        let Inbound::Connected {
            read_only,
            inbox_only,
            output,
            ..
        } = receive()
        else {
            panic!("expected queued connection")
        };
        scopes.push((read_only, inbox_only));
        outputs.push(output);
    }
    scopes.sort();
    assert_eq!(scopes, [(false, false), (false, true), (true, false)]);
    assert!(admission
        .change(3, true, listeners, || false)
        .await
        .is_err());
    drop((queued, outputs));
    for _ in 0..3 {
        assert!(matches!(receive(), Inbound::Disconnected { .. }));
    }
    admission
        .change(4, true, listeners, || false)
        .await
        .unwrap();
    admission.renamed(listeners);
    let mut public_listener = Some(public_listener);
    let mut manager_listener = Some(manager_listener);
    let mut inbox_listener = Some(inbox_listener);
    assert!(admission
        .relinquish(
            4,
            [
                &mut public_listener,
                &mut manager_listener,
                &mut inbox_listener
            ]
        )
        .is_err());
    let listeners = [
        public_listener.as_ref(),
        manager_listener.as_ref(),
        inbox_listener.as_ref(),
    ];
    assert!(admission
        .change(4, true, listeners, || false)
        .await
        .is_err());
    let inherited = admission
        .change(5, true, listeners, || false)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..2 {
        admission
            .relinquish(
                5,
                [
                    &mut public_listener,
                    &mut manager_listener,
                    &mut inbox_listener,
                ],
            )
            .unwrap();
    }
    assert!(admission
        .change(5, false, [None, None, None], || false)
        .await
        .is_err());
    assert!(admission
        .change(6, true, [None, None, None], || false)
        .await
        .is_err());
    crate::control_api::check_handoff_listeners(&public, &inherited).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
