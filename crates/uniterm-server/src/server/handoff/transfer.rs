//! The private local engine protocol is streamed a Pane at a time so large
//! histories do not need a second fleet-sized buffer in the source process.

use super::*;
use crate::server::{
    BoundWorkspace, ClientSession, PaneState, PtyProcess, Server, Terminal, MAX_CLIENTS,
};
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::Path;
use std::time::{Duration, Instant};
use uniterm_proto::{HandoffControlListeners, HandoffControlState, HandoffListener, PaneId};

#[derive(Serialize, Deserialize)]
struct ListenerId {
    path: (u64, u64),
    socket: (u64, u64),
}

impl From<&HandoffListener> for ListenerId {
    fn from(fd: &HandoffListener) -> Self {
        Self {
            path: fd.identity,
            socket: fd.socket_identity,
        }
    }
}

impl ListenerId {
    fn adopt(self, fd: OwnedFd) -> HandoffListener {
        HandoffListener {
            file: std::sync::Arc::new(File::from(fd)),
            identity: self.path,
            socket_identity: self.socket,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Header {
    workspace: String,
    sequence: u64,
    attach: ListenerId,
    // A filesystem rename preserves the kernel socket's original bind address.
    attach_bound_path: std::path::PathBuf,
    control: ListenerId,
    manager: Option<ListenerId>,
    inbox: ListenerId,
    panes: usize,
    clients: usize,
    requester: usize,
}

#[derive(Serialize, Deserialize)]
struct PaneHeader {
    id: PaneId,
    shell: i32,
    foreground: Option<i32>,
    watch: Option<i32>,
    approval: Option<crate::providers::Approval>,
}

const MAX_TRANSFER: usize = 512 * 1024 * 1024;
const MAX_PANES: usize = 4096;

pub(super) fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Workspace handoff exceeded its preparation deadline",
            )
        })
}

// Each syscall uses what is left of one total transfer budget. A peer making
// slow partial progress cannot restart the timeout indefinitely.
struct TimedChannel<'a> {
    channel: &'a UnixStream,
    deadline: Instant,
}

impl Read for TimedChannel<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.channel
            .set_read_timeout(Some(remaining(self.deadline)?))?;
        self.channel.read(bytes)
    }
}

impl Write for TimedChannel<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.channel
            .set_write_timeout(Some(remaining(self.deadline)?))?;
        self.channel.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Frames<'a> {
    wire: TimedChannel<'a>,
    remaining: usize,
}

impl Frames<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| invalid("Workspace exceeds handoff size limit"))?;
        write_checkpoint(&mut self.wire, bytes)
    }
    fn read(&mut self) -> io::Result<Vec<u8>> {
        let bytes = read_checkpoint_limited(&mut self.wire, self.remaining)?;
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| invalid("Workspace exceeds handoff size limit"))?;
        Ok(bytes)
    }
    fn send(&self, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
        self.wire
            .channel
            .set_write_timeout(Some(remaining(self.wire.deadline)?))?;
        send_descriptors(self.wire.channel, fds)
    }
    fn receive<const N: usize>(&self) -> io::Result<[OwnedFd; N]> {
        self.wire
            .channel
            .set_read_timeout(Some(remaining(self.wire.deadline)?))?;
        receive_descriptors(self.wire.channel)
    }
}

fn codec() -> impl Options {
    bincode::DefaultOptions::new().with_limit(MAX_CHECKPOINT as u64)
}

pub(super) fn export(
    server: &Server,
    ch: &UnixStream,
    control: HandoffControlState,
    requester: mio::Token,
    deadline: Instant,
) -> io::Result<()> {
    let mut frames = Frames {
        wire: TimedChannel {
            channel: ch,
            deadline,
        },
        remaining: MAX_TRANSFER,
    };
    let model = WorkspaceModel::capture(server)?;
    let layout = WorkspaceLayout::capture(server);
    layout.validate(server.panes.keys().copied())?;
    let listeners = control.listeners;
    let mut clients: Vec<_> = server.clients.iter().collect();
    clients.sort_unstable_by_key(|(token, _)| token.0);
    let requester = clients
        .iter()
        .position(|(token, _)| **token == requester)
        .ok_or_else(|| invalid("upgrade requester disconnected"))?;
    if server.panes.len() > MAX_PANES {
        return Err(invalid("too many Panes for live transfer"));
    }
    let file = File::from(server.listener.as_fd().try_clone_to_owned()?);

    let header = Header {
        workspace: server.name.clone(),
        sequence: server.log.current_sequence(),
        attach_bound_path: server
            .listener
            .local_addr()?
            .as_pathname()
            .ok_or_else(|| invalid("unnamed attach listener"))?
            .to_owned(),
        attach: ListenerId {
            path: server.socket_identity,
            socket: crate::control_api::socket_file_identity(&file)?,
        },
        control: (&listeners.control).into(),
        inbox: (&listeners.inbox).into(),
        manager: listeners.manager.as_ref().map(Into::into),
        panes: server.panes.len(),
        clients: clients.len(),
        requester,
    };
    frames.write(&serde_json::to_vec(&header)?)?;
    let [socket_claim, state_claim] = server
        .workspace_lock
        .as_ref()
        .ok_or_else(|| invalid("source has no Workspace claim"))?
        .handoff_fds();
    frames.send(&[
        file.as_fd(),
        socket_claim.as_fd(),
        state_claim.as_fd(),
        listeners.control.file.as_fd(),
        listeners.inbox.file.as_fd(),
    ])?;
    if let Some(manager) = &listeners.manager {
        frames.send(&[manager.file.as_fd()])?;
    }
    frames.write(&control.inbox)?;
    frames.write(&control.native_sessions)?;
    frames.write(&serde_json::to_vec(&layout)?)?;
    frames.write(&codec().serialize(&model).map_err(io::Error::other)?)?;
    for (&id, pane) in &server.panes {
        let pid = pane
            .pty
            .child_pid()
            .ok_or_else(|| invalid("childless recovery Pane"))? as i32;
        let watch = server
            .pane_watches
            .get(&id)
            .and_then(|t| server.process_watches.get(t))
            .map(|(_, w)| w.pid);
        let approval = pane
            .agent
            .as_ref()
            .and_then(|a| crate::providers::permission_approval(&a.id, &pane.term));
        let info = PaneHeader {
            id,
            shell: pid,
            foreground: pane.pty.foreground_process_group(),
            watch,
            approval,
        };
        frames.write(&serde_json::to_vec(&info)?)?;
        frames.write(&serde_json::to_vec(&pane.state)?)?;
        let terminal = pane.term.live_checkpoint().map_err(|e| {
            io::Error::new(e.kind(), format!("Pane {} terminal checkpoint: {e}", id.0))
        })?;
        frames.write(&terminal)?;
        // SAFETY: pane.pty owns the master throughout this send.
        frames.send(&[unsafe { std::os::fd::BorrowedFd::borrow_raw(pane.pty.raw_fd()) }])?;
    }
    for (_, client) in clients {
        client.session.validate()?;
        frames.write(
            &codec()
                .serialize(&client.session)
                .map_err(io::Error::other)?,
        )?;
        frames.send(&[client.stream.as_fd()])?;
    }
    Ok(())
}

pub(super) fn receive(
    socket: &Path,
    workspace: &str,
    ch: &UnixStream,
) -> io::Result<(Server, mio::Poll, mio::Token)> {
    use std::os::unix::net::UnixStream as StdStream;
    let mut frames = Frames {
        wire: TimedChannel {
            channel: ch,
            deadline: Instant::now() + super::activation::TIMEOUT,
        },
        remaining: MAX_TRANSFER,
    };
    let h: Header = serde_json::from_slice(&frames.read()?)?;
    if h.workspace != workspace
        || h.panes == 0
        || h.panes > MAX_PANES
        || h.clients > MAX_CLIENTS
        || h.requester >= h.clients
    {
        return Err(invalid("invalid transfer scope or resource count"));
    }
    let [listen, socket_claim, state_claim, public, inbox] = frames.receive()?;
    let claim = crate::server::WorkspaceLock::prepare_inherited(
        socket,
        workspace,
        socket_claim.into(),
        state_claim.into(),
    )?;
    let attach = h.attach.adopt(listen);
    if h.attach_bound_path.parent() != socket.parent() {
        return Err(invalid(
            "attach listener belongs to a different runtime directory",
        ));
    }
    let public_listener =
        crate::control_api::Listener::check_descriptor(socket, &h.attach_bound_path, &attach)?;
    let listener = mio::net::UnixListener::from_std(public_listener);
    let manager = h
        .manager
        .map(|id| -> io::Result<_> {
            let [fd] = frames.receive()?;
            Ok(id.adopt(fd))
        })
        .transpose()?;
    let controls = HandoffControlState {
        listeners: HandoffControlListeners {
            control: h.control.adopt(public),
            manager,
            inbox: h.inbox.adopt(inbox),
        },
        inbox: frames.read()?.into(),
        native_sessions: frames.read()?.into(),
    };
    let layout: WorkspaceLayout = serde_json::from_slice(&frames.read()?)?;
    let model: WorkspaceModel = codec()
        .deserialize(&frames.read()?)
        .map_err(io::Error::other)?;
    let writer = crate::eventlog::EventLog::resume_at(workspace, h.sequence)?;
    let bound = BoundWorkspace {
        control_handoff: Some((controls, writer)),
        listener,
        identity: attach.identity,
        claim,
    };
    let (mut server, poll) =
        Server::from_bound_workspace(socket, workspace, "", (1, 1), bound, false)?;
    model.install(&mut server)?;
    let mut shells = std::collections::HashSet::new();
    for _ in 0..h.panes {
        let info: PaneHeader = serde_json::from_slice(&frames.read()?)?;
        let state: PaneState = serde_json::from_slice(&frames.read()?)?;
        let terminal = Terminal::from_live_checkpoint(&frames.read()?)?;
        let [master] = frames.receive()?;
        let pty = PtyProcess::from_handoff(master, info.shell)?;
        if server.panes.contains_key(&info.id)
            || !shells.insert(info.shell)
            || pty.foreground_process_group() != info.foreground
            || info.watch.is_some_and(|pid| Some(pid) != info.foreground)
            || info.approval
                != state
                    .agent
                    .as_ref()
                    .and_then(|a| crate::providers::permission_approval(&a.id, &terminal))
        {
            return Err(invalid("Pane identity or recognized prompt changed"));
        }
        if let Some(file) = state.editor_file.clone() {
            server.editor_panes.insert(file, info.id);
        }
        server.register_pane(poll.registry(), info.id, pty, terminal, state)?;
        if let Some(pid) = info.watch {
            server.register_process_watch(poll.registry(), info.id, pid);
            if !server.pane_watches.contains_key(&info.id) {
                return Err(invalid("could not restore foreground exit watch"));
            }
        }
    }
    layout.install(&mut server)?;
    let mut requester = None;
    for i in 0..h.clients {
        let session: ClientSession = codec()
            .deserialize(&frames.read()?)
            .map_err(io::Error::other)?;
        let [fd] = frames.receive()?;
        let peer = StdStream::from(fd);
        peer.peer_addr()?;
        if peer.local_addr()?.as_pathname() != Some(&h.attach_bound_path) {
            return Err(invalid("client stream belongs to a different Workspace"));
        }
        let token = server.register_client(
            poll.registry(),
            mio::net::UnixStream::from_std(peer),
            session,
        )?;
        if i == h.requester {
            requester = Some(token);
        }
    }
    Ok((
        server,
        poll,
        requester.ok_or_else(|| invalid("missing upgrade requester"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exhausted_transfer_budget_rejects_the_next_frame_and_descriptor() {
        let (local, _peer) = UnixStream::pair().unwrap();
        let mut frames = Frames {
            wire: TimedChannel {
                channel: &local,
                deadline: Instant::now(),
            },
            remaining: MAX_TRANSFER,
        };
        assert_eq!(
            frames.write(b"state").unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            frames.receive::<1>().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
}
