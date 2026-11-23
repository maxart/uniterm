//! One verified executable prepares off the hot path. Only the final transfer
//! stops the incumbent from servicing its already registered kernel sources.

use super::super::{set_interest, Server};
use super::{read_checkpoint_limited, transfer, write_checkpoint, SourceOffer, CHECKPOINT_VERSION};
use mio::{Registry, Token};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Child;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use uniterm_proto::{
    encode_frame, CoreToAgent, HandoffControlState, ServerMessage, UpgradeCandidate,
};

type Launcher = fn(&str, &str, UnixStream) -> io::Result<Option<Child>>;
static LAUNCHER: OnceLock<Launcher> = OnceLock::new();
pub(super) const TIMEOUT: Duration = Duration::from_secs(15);

/// Supply the binary front door's release verifier. Embedders must explicitly
/// opt in before their servers advertise a live upgrade capability.
pub fn register_launcher(launcher: Launcher) {
    let _ = LAUNCHER.set(launcher);
}

pub(crate) fn capability() -> Option<u32> {
    LAUNCHER.get().map(|_| CHECKPOINT_VERSION)
}

fn timeout(channel: &UnixStream) -> io::Result<()> {
    channel.set_read_timeout(Some(TIMEOUT))?;
    channel.set_write_timeout(Some(TIMEOUT))
}

pub(crate) fn stage(workspace: &str, release: &str) -> io::Result<Option<UpgradeCandidate>> {
    let launch = LAUNCHER
        .get()
        .ok_or_else(|| io::Error::other("server has no verified release launcher"))?;
    let (parent, child) = UnixStream::pair()?;
    timeout(&parent)?;
    let Some(child) = launch(workspace, release, child)? else {
        return Ok(None);
    };
    let candidate = UpgradeCandidate {
        child: Some(child),
        channel: parent,
    };
    if read_checkpoint_limited(&candidate.channel, 128)? != release.as_bytes() {
        return Err(io::Error::other(
            "replacement did not identify the verified release",
        ));
    }
    Ok(Some(candidate))
}

/// Bootstrap a verified replacement using only its inherited private socket.
/// A rejected checkpoint never touches the incumbent's live streams.
pub fn run_candidate(
    path: &Path,
    name: &str,
    mut channel: UnixStream,
    release: &str,
) -> io::Result<()> {
    timeout(&channel)?;
    write_checkpoint(&channel, release.as_bytes())?;
    let (mut server, mut poll, requester) = match transfer::receive(path, name, &channel) {
        Ok(prepared) => prepared,
        Err(error) => {
            if channel.write_all(b"E").is_ok() {
                let message = error.to_string();
                let _ = write_checkpoint(&channel, &message.as_bytes()[..message.len().min(4096)]);
            }
            return Err(error);
        }
    };
    // The export deadline can have only milliseconds left. Once ready, the
    // source owns the deadline: rollback shuts down this private channel.
    channel.set_read_timeout(None)?;
    channel.write_all(b"R")?;
    let mut vote = [0];
    channel.read_exact(&mut vote)?;
    if vote != *b"C" {
        return Err(io::Error::other("Workspace handoff was cancelled"));
    }
    // The candidate already holds the durable tail lock. Every normal owner
    // needs the entire range. No new lock acquisition can fail after commit.
    // Source retirement closes the channel after fencing its runtime.
    let mut discard = [0; 1];
    loop {
        match channel.read(&mut discard) {
            Ok(0) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
            Ok(_) => continue,
        }
    }
    drop(channel);
    server.agents.activate_prepared_runtime();
    server.event_writes_enabled = true;
    server.omarchy.watching = None;
    server.configure_theme_watcher();
    server.agents.send(CoreToAgent::TaskTrackingSet {
        enabled: server.config.auto_track_tasks,
    });
    server.sync_file_watches();
    server.sync_git_change_watch();
    for directory in server.files.watched_directories() {
        server.request_file_listing(directory);
    }
    server.sync_artifact_watches();
    let artifacts: Vec<_> = server
        .artifacts
        .artifacts()
        .filter(|a| a.status != uniterm_core::ArtifactStatus::Superseded)
        .map(|a| a.id)
        .collect();
    for artifact in artifacts {
        server.request_artifact_observation(artifact);
    }

    server.agents.send(CoreToAgent::DevServerWatchSet {
        active: server.observatory_tab == crate::chrome::ObservatoryTab::WebServers,
    });
    server.upgrade_reply(poll.registry(), requester, true, None);
    let result = server.run_loop(&mut poll);
    server.drain_clients_before_exit(&mut poll);
    server.retire_all_ptys(&mut poll);
    result
}

// Present only during an explicit update. The mio loop continues serving
// while the runtime fences persistence, listeners and transcript observers.
pub(crate) struct Preparation {
    transaction: u64,
    workspace: String,
    cursor: u64,
    dispatch: u64,
    deadline: Instant,
    result: Option<Result<Option<HandoffControlState>, String>>,
    candidate: UpgradeCandidate,
}

impl Server {
    pub(in crate::server) fn upgrade_deadline(&self) -> Option<Instant> {
        self.upgrade_candidate.as_ref().map(|p| p.deadline)
    }

    pub(in crate::server) fn handoff_prepared(
        &mut self,
        transaction: u64,
        result: Result<Option<HandoffControlState>, String>,
    ) {
        if let Some(p) = self
            .upgrade_candidate
            .as_mut()
            .filter(|p| p.transaction == transaction)
        {
            p.result = Some(result);
        }
    }

    fn upgrade_reply(
        &mut self,
        reg: &Registry,
        token: Token,
        activated: bool,
        error: Option<String>,
    ) {
        if let Some(client) = self.clients.get_mut(&token) {
            client.queue(&encode_frame(&ServerMessage::ServerUpgradeResult(
                uniterm_proto::ServerActivation {
                    activated,
                    version: uniterm_proto::BUILD_VERSION.into(),
                    error,
                },
            )));
            client.flush_ready();
            let _ = set_interest(reg, client, token);
        }
    }

    pub(in crate::server) fn request_server_upgrade(
        &mut self,
        reg: &Registry,
        token: Token,
        release: String,
    ) {
        if capability().is_none() || self.upgrade_requester.is_some() {
            self.upgrade_reply(
                reg,
                token,
                false,
                Some("live server update unavailable or already in progress".into()),
            );
            return;
        }
        self.upgrade_requester = Some(token);
        self.agents.send(CoreToAgent::PrepareServerUpgrade {
            workspace: self.name.clone(),
            release,
            requester: token.0,
        });
    }

    pub(in crate::server) fn server_upgrade_prepared(
        &mut self,
        reg: &Registry,
        token: Token,
        result: Result<Option<Arc<UpgradeCandidate>>, String>,
    ) {
        if self.upgrade_requester != Some(token) {
            return;
        }
        let candidate = result.and_then(|result| {
            result
                .map(|handle| {
                    Arc::try_unwrap(handle).map_err(|_| "candidate has multiple owners".to_owned())
                })
                .transpose()
        });
        match candidate {
            Ok(Some(candidate)) => {
                let transaction = self.next_token as u64;
                self.next_token += 1;
                let workspace = self.name.clone();
                let cursor = self.log.current_sequence();
                self.agents.send(CoreToAgent::HandoffControlAdmission {
                    through: Some(cursor),
                    workspace: workspace.clone(),
                    transaction,
                    pause: true,
                });
                self.upgrade_candidate = Some(Box::new(Preparation {
                    transaction,
                    workspace,
                    cursor,
                    dispatch: self.agents.dispatch_sequence(),
                    deadline: Instant::now() + TIMEOUT,
                    result: None,
                    candidate,
                }));
            }
            result => {
                self.upgrade_requester = None;
                self.upgrade_reply(reg, token, false, result.err());
            }
        }
    }

    fn offer_upgrade(
        &mut self,
        token: Token,
        candidate: &mut UpgradeCandidate,
        control: HandoffControlState,
    ) -> io::Result<UnixStream> {
        let claim = self
            .workspace_lock
            .as_ref()
            .ok_or_else(|| io::Error::other("Workspace lost its claim"))?;
        let retirement = candidate.channel.try_clone()?;
        let mut offer = SourceOffer::begin(
            claim,
            candidate
                .child
                .as_mut()
                .ok_or_else(|| io::Error::other("missing candidate process"))?,
            &mut candidate.channel,
        )?;
        let deadline = Instant::now() + TIMEOUT;
        transfer::export(self, offer.channel(), control, token, deadline)?;
        offer
            .channel()
            .set_read_timeout(Some(transfer::remaining(deadline)?))?;
        offer.await_ready()?;
        offer
            .channel()
            .set_write_timeout(Some(transfer::remaining(deadline)?))?;
        if self.agents.handoff_replies_pending() {
            return Err(io::Error::other(
                "Workspace has pending runtime replies; retry the update",
            ));
        }
        offer.commit()?;
        Ok(retirement)
    }

    pub(in crate::server) fn finish_server_upgrade(&mut self, reg: &Registry) {
        let Some(p) = self.upgrade_candidate.as_ref() else {
            return;
        };
        let token = self
            .upgrade_requester
            .expect("prepared upgrade has a requester");
        let changed = self.log.current_sequence() != p.cursor
            || self.name != p.workspace
            || self.agents.dispatch_sequence() != p.dispatch
            || !self.running
            || !self.clients.contains_key(&token);
        let expired = Instant::now() >= p.deadline;
        if p.result.is_none() && !changed && !expired {
            return;
        }
        let Preparation {
            workspace,
            transaction,
            result,
            mut candidate,
            ..
        } = *self.upgrade_candidate.take().unwrap();
        self.upgrade_requester = None;
        let offer = if changed {
            Err(io::Error::other(
                "Workspace has pending work; retry the update",
            ))
        } else if expired {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Workspace preparation timed out",
            ))
        } else {
            result
                .unwrap()
                .map_err(io::Error::other)
                .and_then(|state| {
                    state.ok_or_else(|| io::Error::other("Workspace listeners did not prepare"))
                })
                .and_then(|control| self.offer_upgrade(token, &mut candidate, control))
        };
        let retirement = match offer {
            Ok(channel) => channel,
            Err(error) => {
                self.agents.send(CoreToAgent::HandoffControlAdmission {
                    through: None,
                    workspace,
                    transaction,
                    pause: false,
                });
                self.upgrade_reply(reg, token, false, Some(error.to_string()));
                return;
            }
        };
        candidate.child.take();
        self.running = false;
        self.event_writes_enabled = false;
        self.clients.clear();
        self.panes.clear();
        self.agents.send(CoreToAgent::RelinquishHandoffControl {
            retired: Some(Arc::new(retirement)),
            workspace,
            transaction,
        });
        self.agents
            .shutdown_retaining((self.workspace_lock.take(), candidate));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{reaper::common, ClientSession};

    #[test]
    fn waiting_for_runtime_preparation_services_clients_and_cancels_on_new_work() {
        common::isolate_state();
        let name = common::unique_workspace_name();
        let directory = common::temp_dir("prepare");
        let path = directory.join(format!("{name}.sock"));
        let (mut server, poll) = Server::bind(&path, "/bin/sh", &[], 80, 24).unwrap();
        let requests = server.agents.capture_requests();
        let (client, mut reader) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let token = server
            .register_client(
                poll.registry(),
                mio::net::UnixStream::from_std(client),
                ClientSession::default(),
            )
            .unwrap();
        let (channel, _peer) = UnixStream::pair().unwrap();
        server.upgrade_requester = Some(token);
        server.server_upgrade_prepared(
            poll.registry(),
            token,
            Ok(Some(Arc::new(UpgradeCandidate {
                child: None,
                channel,
            }))),
        );
        let CoreToAgent::HandoffControlAdmission {
            transaction,
            pause: true,
            through: Some(_),
            ..
        } = requests.recv().unwrap()
        else {
            panic!("expected preparation");
        };
        server.finish_server_upgrade(poll.registry());
        server.handle_msg(
            poll.registry(),
            token,
            uniterm_proto::ClientMessage::ListInfo,
            &mut false,
        );
        let mut header = [0; 4];
        reader.read_exact(&mut header).unwrap();
        let mut body = vec![0; u32::from_be_bytes(header) as usize];
        reader.read_exact(&mut body).unwrap();
        assert!(matches!(
            bincode::deserialize::<ServerMessage>(&body).unwrap(),
            ServerMessage::Info { .. }
        ));
        server
            .agents
            .send(CoreToAgent::TaskTrackingSet { enabled: true });
        server.finish_server_upgrade(poll.registry());
        assert!(server.upgrade_deadline().is_none());
        server.handoff_prepared(transaction, Ok(None));
        assert!(server.upgrade_candidate.is_none());
        assert!(requests.try_iter().any(|r| matches!(r, CoreToAgent::HandoffControlAdmission {transaction:t, pause:false,..} if t == transaction)));
        drop(server);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
