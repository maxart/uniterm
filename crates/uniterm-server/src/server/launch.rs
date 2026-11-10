//! Request-scoped filesystem authority for semantic launches. Only the runtime
//! resolves executables; the core revalidates ownership before applying intent.

use super::*;
use uniterm_proto::{ControlCommand, ControlRequest, ProviderResolution};

const MAX_PENDING_LAUNCHES: usize = 32;

/// Filesystem authority retained only by the semantic request that obtained it.
#[derive(Clone)]
pub(super) struct LaunchResolution {
    search_path: Vec<String>,
    entries: Vec<ProviderResolution>,
}

/// Capture request ownership before asynchronous discovery yields the core loop.
pub(super) struct PendingLaunch {
    source: LaunchSource,
    workspace: String,
    search_path: Vec<String>,
    target: Option<LaunchTargetState>,
}

enum LaunchSource {
    Client {
        token: Token,
        message: ClientMessage,
    },
    Control {
        connection: u64,
        request: ControlRequest,
    },
    Today {
        agent: String,
        project: ProjectId,
        generation: u64,
    },
}

#[derive(PartialEq, Eq)]
struct LaunchTargetState {
    project: ProjectId,
    root: String,
    anchor: PaneId,
    invocation: Option<u64>,
}

impl Server {
    /// Resolutions exist only while replaying the exact request that obtained
    /// them, or its already accepted Git child-creation continuation.
    pub(super) fn resolved_agent(&self, requested: Option<&str>) -> Option<(String, String)> {
        let context = self.launch_resolution.as_ref()?;
        if context.search_path != self.agent_search_path {
            return None;
        }
        context
            .entries
            .iter()
            .find(|entry| entry.requested.as_deref() == requested)?
            .resolved
            .clone()
    }

    fn launch_target(
        &self,
        selector: Option<&str>,
        anchor: Option<PaneId>,
    ) -> Option<LaunchTargetState> {
        let project = if let Some(anchor) = anchor {
            let window = self
                .windows
                .iter()
                .find(|w| w.layout.contains_pane(anchor))?;
            self.projects.iter().find(|p| p.id == window.project)?
        } else {
            self.projects.iter().find(|p| {
                selector.map_or(p.id == self.active_project, |s| p.name == s || p.root == s)
            })?
        };
        let anchor = anchor.or_else(|| {
            if project.id == self.active_project {
                self.windows.get(self.active_window).map(|w| w.active)
            } else {
                project.active_pane.or_else(|| {
                    self.windows
                        .iter()
                        .find(|w| w.project == project.id)
                        .map(|w| w.active)
                })
            }
        })?;
        Some(LaunchTargetState {
            project: project.id,
            root: project.root.clone(),
            anchor,
            invocation: self.timeline_invocations.get(&anchor).copied(),
        })
    }

    fn requested_for_fork(&self, parent: uniterm_core::RunId) -> Vec<Option<String>> {
        self.run_graph
            .run(parent)
            .into_iter()
            .flat_map(|run| &run.roles)
            .filter_map(|role| self.run_graph.role(*role))
            .map(|role| Some(role.provider.clone()))
            .collect()
    }

    fn requested_for_launch(
        agent: &Option<String>,
        roles: &[uniterm_core::orchestrate::RoleProviderSelection],
    ) -> Vec<Option<String>> {
        let mut requested = vec![agent.clone()];
        requested.extend(roles.iter().map(|role| Some(role.provider.clone())));
        requested
    }

    /// Suspend launches behind typed filesystem resolution while retaining their reply contract.
    pub(super) fn defer_client_launch(
        &mut self,
        reg: &Registry,
        token: Token,
        message: &ClientMessage,
    ) -> bool {
        if self.launch_resolution.is_some() {
            return false;
        }
        let requested = match message {
            ClientMessage::Suggest => {
                if self.pending_launches.values().any(|pending| matches!(&pending.source, LaunchSource::Client { token: waiting, message: ClientMessage::Suggest } if *waiting == token)) { return true; }
                uniterm_core::agent::PROVIDERS
                    .iter()
                    .map(|p| Some(p.id.into()))
                    .collect()
            }
            ClientMessage::NewTask {
                agent,
                role_providers,
                options,
                ..
            } if !options.save => Self::requested_for_launch(agent, role_providers),
            ClientMessage::AgentLaunch { agent, .. } => vec![Some(agent.clone())],
            ClientMessage::RunFork { fork } => self.requested_for_fork(fork.parent),
            _ => return false,
        };
        self.queue_provider_resolution(
            reg,
            LaunchSource::Client {
                token,
                message: message.clone(),
            },
            requested,
        );
        true
    }

    /// Keep automation on the same semantic path as interactive launch requests.
    pub(super) fn defer_control_launch(
        &mut self,
        reg: &Registry,
        connection: u64,
        request: &ControlRequest,
    ) -> bool {
        if self.launch_resolution.is_some() {
            return false;
        }
        let requested = match &request.command {
            ControlCommand::TaskStart { launch } if !launch.options.save => {
                Self::requested_for_launch(&launch.agent, &launch.role_providers)
            }
            ControlCommand::OrchestrationStart { launch } => {
                Self::requested_for_launch(&launch.provider, &launch.role_providers)
            }
            ControlCommand::AgentLaunch { agent, .. }
            | ControlCommand::TimelineManager { agent, .. } => vec![Some(agent.clone())],
            ControlCommand::RunFork { fork } => self.requested_for_fork(fork.parent),
            _ => return false,
        };
        self.queue_provider_resolution(
            reg,
            LaunchSource::Control {
                connection,
                request: request.clone(),
            },
            requested,
        );
        true
    }

    /// Resolve a Today manager without blocking input to the existing fleet.
    pub(super) fn request_today_manager(
        &mut self,
        reg: &Registry,
        agent: String,
        project: ProjectId,
    ) {
        let Some(view) = self.today.as_mut() else {
            return;
        };
        view.message = "Checking installed provider...".into();
        let generation = view.generation;
        self.queue_provider_resolution(
            reg,
            LaunchSource::Today {
                agent: agent.clone(),
                project,
                generation,
            },
            vec![Some(agent)],
        );
    }

    fn source_target(&self, source: &LaunchSource) -> Option<LaunchTargetState> {
        match source {
            LaunchSource::Client {
                message: ClientMessage::Suggest,
                ..
            } => None,
            LaunchSource::Client {
                message: ClientMessage::NewTask { project, .. },
                ..
            } => self.launch_target(project.as_deref(), None),
            LaunchSource::Control {
                request:
                    ControlRequest {
                        command: ControlCommand::TaskStart { launch },
                        ..
                    },
                ..
            } => self.launch_target(launch.project.as_deref(), None),
            LaunchSource::Control {
                request:
                    ControlRequest {
                        command: ControlCommand::OrchestrationStart { launch },
                        ..
                    },
                ..
            } => self.launch_target(launch.project.as_deref(), None),
            LaunchSource::Control {
                request:
                    ControlRequest {
                        command: ControlCommand::AgentLaunch { pane, .. },
                        ..
                    },
                ..
            } => self.launch_target(None, *pane),
            LaunchSource::Control {
                request:
                    ControlRequest {
                        command: ControlCommand::TimelineManager { project, .. },
                        ..
                    },
                ..
            }
            | LaunchSource::Today { project, .. } => self
                .projects
                .iter()
                .find(|p| p.id == *project)
                .and_then(|p| self.launch_target(Some(&p.root), None)),
            LaunchSource::Client {
                message: ClientMessage::RunFork { fork },
                ..
            }
            | LaunchSource::Control {
                request:
                    ControlRequest {
                        command: ControlCommand::RunFork { fork },
                        ..
                    },
                ..
            } => self
                .run_graph
                .run(fork.parent)
                .and_then(|run| self.projects.iter().find(|p| p.id == run.project))
                .and_then(|p| self.launch_target(Some(&p.root), None)),
            _ => self.launch_target(None, None),
        }
    }

    fn queue_provider_resolution(
        &mut self,
        reg: &Registry,
        source: LaunchSource,
        mut requested: Vec<Option<String>>,
    ) {
        if self.pending_launches.len() >= MAX_PENDING_LAUNCHES {
            self.reject_launch(
                reg,
                source,
                "Provider discovery is busy; retry after pending requests finish",
            );
            return;
        }
        requested.sort();
        requested.dedup();
        let target = self.source_target(&source);
        let request = self.next_launch_request;
        self.next_launch_request = self.next_launch_request.saturating_add(1);
        let search_path = self.agent_search_path.clone();
        self.pending_launches.insert(
            request,
            PendingLaunch {
                source,
                target,
                workspace: self.name.clone(),
                search_path: search_path.clone(),
            },
        );
        self.agents
            .send(uniterm_proto::CoreToAgent::ResolveProviders {
                request,
                search_path,
                requested,
            });
    }

    /// Apply filesystem authority only while its original target and requester still own it.
    pub(super) fn finish_provider_resolution(
        &mut self,
        reg: &Registry,
        request: u64,
        entries: Vec<ProviderResolution>,
        error: Option<String>,
    ) {
        let Some(pending) = self.pending_launches.remove(&request) else {
            return;
        };
        match &pending.source {
            LaunchSource::Client { token, .. }
                if self.clients.get(token).is_none_or(|c| c.dead) =>
            {
                return
            }
            LaunchSource::Today { generation, .. }
                if self
                    .today
                    .as_ref()
                    .is_none_or(|view| view.generation != *generation) =>
            {
                return
            }
            _ => {}
        }
        if !self.running {
            self.reject_launch(
                reg,
                pending.source,
                "Workspace stopped before launch completed",
            );
            return;
        }
        let error = error.or_else(|| {
            (pending.workspace != self.name
                || pending.search_path != self.agent_search_path
                || pending.target != self.source_target(&pending.source))
            .then(|| "Launch target or executable search path changed; retry the request".into())
        });
        if let Some(error) = error {
            self.reject_launch(reg, pending.source, &error);
            return;
        }
        let previous = self.launch_resolution.replace(LaunchResolution {
            search_path: pending.search_path,
            entries,
        });
        match pending.source {
            LaunchSource::Client { token, message } => {
                self.handle_msg(reg, token, message, &mut false)
            }
            LaunchSource::Control {
                connection,
                request,
            } => self.on_control_request(reg, connection, request),
            LaunchSource::Today { agent, project, .. } => {
                let launched = self.launch_today_manager(reg, &agent, project, false, true);
                if let Some(view) = self.today.as_mut() {
                    view.message = if launched.is_some() {
                        "Read-only manager started in a background Tab"
                    } else {
                        "Could not start manager: check provider and Project"
                    }
                    .into();
                }
                self.paint_today(reg);
            }
        }
        self.launch_resolution = previous;
    }

    /// A disconnected control requester cannot create processes from a late callback.
    pub(super) fn cancel_control_launches(&mut self, connection: u64) {
        let cancelled: Vec<_> = self
            .pending_launches
            .iter()
            .filter_map(|(id, pending)| match &pending.source {
                LaunchSource::Control {
                    connection: owner,
                    request,
                } if *owner == connection => Some((*id, request.id)),
                _ => None,
            })
            .collect();
        for (pending, id) in cancelled {
            self.pending_launches.remove(&pending);
            // Release the dispatcher's pending-request accounting even though
            // the disconnected output socket will discard this response.
            self.agents
                .send(uniterm_proto::CoreToAgent::ControlResponse {
                    connection,
                    response: uniterm_proto::ControlResponse::error(
                        id,
                        "cancelled",
                        "Control connection closed before launch",
                    ),
                });
        }
    }

    fn reject_launch(&mut self, reg: &Registry, source: LaunchSource, error: &str) {
        match source {
            LaunchSource::Control {
                connection,
                request,
            } => self
                .agents
                .send(uniterm_proto::CoreToAgent::ControlResponse {
                    connection,
                    response: uniterm_proto::ControlResponse::error(
                        request.id,
                        "launch_refused",
                        error,
                    ),
                }),
            LaunchSource::Today { .. } => {
                if let Some(view) = self.today.as_mut() {
                    view.message = error.into();
                }
                self.paint_today(reg);
            }
            LaunchSource::Client { token, message } => {
                let reply = match message {
                    ClientMessage::NewTask { .. } => ServerMessage::TaskStarted {
                        error: Some(error.into()),
                    },
                    ClientMessage::AgentLaunch { agent, .. } => {
                        ServerMessage::AgentLaunchResult { agent, pane: None }
                    }
                    ClientMessage::RunFork { fork } => {
                        ServerMessage::RunForked(uniterm_proto::RunForkResult {
                            parent: fork.parent,
                            child: None,
                            worktree: super::worktree::rejected(
                                uniterm_proto::WorktreeAction::Add,
                                error,
                            ),
                        })
                    }
                    _ => ServerMessage::Suggestions {
                        projects: self.project_names(),
                        agents: Vec::new(),
                        launch: Vec::new(),
                        active_project: String::new(),
                        project_roots: Vec::new(),
                    },
                };
                if let Some(client) = self.clients.get_mut(&token) {
                    client.queue(&encode_frame(&reply));
                    client.flush();
                    let _ = set_interest(reg, client, token);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::reaper::common;
    use super::*;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    struct Peer {
        socket: std::os::unix::net::UnixStream,
        decoder: uniterm_proto::FrameDecoder,
    }
    impl Peer {
        fn connect(path: &Path) -> Self {
            let socket = std::os::unix::net::UnixStream::connect(path).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            Self {
                socket,
                decoder: uniterm_proto::FrameDecoder::new(),
            }
        }
        fn send(&mut self, message: ClientMessage) {
            self.socket.write_all(&encode_frame(&message)).unwrap();
        }
        fn until(&mut self, accept: impl Fn(&ServerMessage) -> bool) -> ServerMessage {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
                    if accept(&message) {
                        return message;
                    }
                }
                assert!(
                    Instant::now() < deadline,
                    "core stopped responding while provider resolution was suspended"
                );
                let mut bytes = [0; 32768];
                match self.socket.read(&mut bytes) {
                    Ok(0) => panic!("client disconnected"),
                    Ok(n) => self.decoder.push(&bytes[..n]),
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(e) => panic!("{e}"),
                }
            }
        }
    }

    fn task(save: bool) -> ClientMessage {
        ClientMessage::NewTask {
            prompt: "test provider authority".into(),
            relay: false,
            agent: Some("/bin/sh".into()),
            role_providers: Vec::new(),
            workflow: None,
            project: None,
            options: uniterm_proto::TaskLaunchOptions {
                save,
                ..Default::default()
            },
        }
    }

    #[test]
    fn cancelled_control_changed_path_and_stopped_callbacks_cannot_launch() {
        common::isolate_state();
        let root = common::socket_root().join(format!("lc-{}", common::unique_nonce()));
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join(format!("{}.sock", common::unique_workspace_name()));
        let (mut server, poll) = Server::bind(&socket, "/bin/sh", &[], 80, 24).unwrap();
        let request = ControlRequest {
            version: uniterm_proto::CONTROL_API_VERSION,
            id: 1,
            workspace: server.name.clone(),
            command: ControlCommand::AgentLaunch {
                agent: "/bin/sh".into(),
                target: uniterm_proto::LaunchTarget::NewPane,
                pane: None,
                background: true,
            },
        };
        let entries = || {
            vec![ProviderResolution {
                requested: Some("/bin/sh".into()),
                resolved: Some(("/bin/sh".into(), "/bin/sh".into())),
            }]
        };
        let before = server.panes.len();
        assert!(server.defer_control_launch(poll.registry(), 7, &request));
        let cancelled = server.next_launch_request - 1;
        server.cancel_control_launches(7);
        server.finish_provider_resolution(poll.registry(), cancelled, entries(), None);
        assert_eq!(server.panes.len(), before);
        assert!(server.pending_launches.is_empty());

        assert!(server.defer_control_launch(poll.registry(), 8, &request));
        let stale = server.next_launch_request - 1;
        server.agent_search_path.push("/changed-path".into());
        server.finish_provider_resolution(poll.registry(), stale, entries(), None);
        assert_eq!(server.panes.len(), before);
        assert!(server.launch_resolution.is_none());

        // The callback is at-most-once, including a duplicate arriving after
        // rejection. A fresh request still succeeds with current authority.
        server.finish_provider_resolution(poll.registry(), stale, entries(), None);
        assert_eq!(server.panes.len(), before);
        assert!(server.defer_control_launch(poll.registry(), 8, &request));
        let current = server.next_launch_request - 1;
        server.finish_provider_resolution(poll.registry(), current, entries(), None);
        assert_eq!(server.panes.len(), before + 1);
        assert!(server.pending_launches.is_empty());
        assert!(server.launch_resolution.is_none());

        assert!(server.defer_control_launch(poll.registry(), 8, &request));
        let stopped = server.next_launch_request - 1;
        server.running = false;
        server.finish_provider_resolution(poll.registry(), stopped, entries(), None);
        assert_eq!(server.panes.len(), before + 1);
        assert!(server.pending_launches.is_empty());
        assert!(server.launch_resolution.is_none());
    }

    #[test]
    fn delayed_provider_resolution_keeps_pty_and_persistence_responsive_and_rejects_stale_targets()
    {
        common::isolate_state();
        let name = common::unique_workspace_name();
        let root = common::socket_root().join(format!("pl-{}", common::unique_nonce()));
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join(format!("{name}.sock"));
        let path = root.to_string_lossy().into_owned();
        let (started, release) = crate::runtime::provider_resolution_gate(path.clone());
        let child_path = path.clone();
        let child_socket = socket.clone();
        let thread = std::thread::spawn(move || {
            let (mut server, mut poll) =
                Server::bind(&child_socket, "/bin/sh", &[], 100, 30).unwrap();
            server.agent_search_path = vec![child_path];
            server.run(&mut poll).unwrap();
        });
        common::wait_for_socket(&socket);
        let mut peer = Peer::connect(&socket);
        peer.send(ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 100,
            rows: 30,
        });
        peer.send(ClientMessage::Suggest);
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        peer.send(ClientMessage::Input(
            b"printf 'CORE_REMAINS_RESPONSIVE\\n'\r".to_vec(),
        ));
        peer.until(|m| matches!(m, ServerMessage::RenderOps(bytes) if String::from_utf8_lossy(bytes).contains("CORE_REMAINS_RESPONSIVE")));
        peer.send(task(true));
        assert!(matches!(
            peer.until(|m| matches!(m, ServerMessage::TaskStarted { .. })),
            ServerMessage::TaskStarted { error: None }
        ));
        let deadline = Instant::now() + Duration::from_secs(3);
        while !crate::eventlog::EventLog::load(&name)
            .iter()
            .any(|event| matches!(event, crate::eventlog::LogEvent::TaskCreated { .. }))
        {
            assert!(
                Instant::now() < deadline,
                "provider discovery blocked ordered persistence"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        release.send(()).unwrap();
        peer.until(|m| matches!(m, ServerMessage::Suggestions { .. }));
        let (started, release) = crate::runtime::provider_resolution_gate(path.clone());
        peer.send(task(false));
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        peer.send(ClientMessage::Command(uniterm_proto::Command::NewWindow));
        peer.send(ClientMessage::ListInfo);
        peer.until(|m| matches!(m, ServerMessage::Info { .. }));
        release.send(()).unwrap();
        assert!(
            matches!(peer.until(|m| matches!(m, ServerMessage::TaskStarted { .. })), ServerMessage::TaskStarted { error: Some(error) } if error.contains("changed"))
        );
        peer.send(ClientMessage::Tasks);
        assert!(matches!(
            peer.until(|m| matches!(m, ServerMessage::Tasks { .. })),
            ServerMessage::Tasks { total: 1, .. }
        ));
        let (started, release) = crate::runtime::provider_resolution_gate(path);
        let mut disconnected = Peer::connect(&socket);
        disconnected.send(task(false));
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        drop(disconnected);
        // Allow the kernel's readable EOF to be consumed before releasing the
        // independent blocking worker. This is a test wait, never a core timer.
        std::thread::sleep(Duration::from_millis(50));
        release.send(()).unwrap();
        peer.send(ClientMessage::Tasks);
        assert!(matches!(
            peer.until(|m| matches!(m, ServerMessage::Tasks { .. })),
            ServerMessage::Tasks { total: 1, .. }
        ));
        peer.send(ClientMessage::KillServer);
        thread.join().unwrap();
    }
}
