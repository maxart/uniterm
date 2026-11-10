//! Native resume checks run on blocking workers; only validated continuations
//! may mutate a Pane on the core loop.

use super::*;
use uniterm_proto::{ControlResponse, CoreToAgent, TimelineRequester, TimelineResumeData};

/// Captured ownership is revalidated after filesystem work completes.
pub(super) struct PendingNativeResume {
    target: NativeResumeTarget,
    workspace: String,
    search_path: Vec<String>,
}

enum NativeResumeTarget {
    Restore {
        pane: PaneId,
        token: Token,
        project: Option<ProjectId>,
        project_cwd: Option<PathBuf>,
    },
    Timeline {
        requester: TimelineRequester,
        evidence: TimelineResumeData,
    },
}

#[cfg(test)]
#[derive(Clone)]
struct NativeTestGate {
    started: crossbeam_channel::Sender<()>,
    release: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
static NATIVE_TEST_GATES: std::sync::LazyLock<std::sync::Mutex<HashMap<String, NativeTestGate>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Probe the exact recorded executable, retaining all explicit native arguments.
/// Only the runtime calls this function because PATH and identity require I/O.
pub(crate) fn resolve(
    provider: &str,
    mut argv: Vec<String>,
    search_path: &[String],
) -> Result<Vec<String>, String> {
    #[cfg(test)]
    if let Some(gate) = search_path
        .first()
        .and_then(|path| NATIVE_TEST_GATES.lock().unwrap().get(path).cloned())
    {
        let _ = gate.started.send(());
        let _ = gate.release.recv();
    }
    let command = argv.first().ok_or("The saved resume command is empty")?;
    if !crate::providers::resume_argv_allowed(provider, command) {
        return Err("The recorded provider resume command is not trusted".into());
    }
    let executable = crate::workflow::executable_on_search_path(command, search_path)
        .ok_or_else(|| format!("The saved provider executable {command} is unavailable"))?;
    if crate::providers::launch_identity(&executable).as_deref() != Some(provider) {
        return Err("The saved executable now belongs to a different provider".into());
    }
    argv[0] = if Path::new(&executable).is_absolute() {
        executable
    } else {
        std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(executable)
            .to_string_lossy()
            .into_owned()
    };
    Ok(argv)
}

/// What a saved profile restores with: its resume command, or the provider's
/// fresh launch when no session was recorded. The durable profile keeps its
/// empty command so a session observed later still completes a real resume.
pub(super) fn restore_argv(profile: &crate::persist::AgentLaunchSnap) -> Option<Vec<String>> {
    if profile.resume_command.is_empty() {
        crate::providers::fresh_argv(&profile.provider)
    } else {
        Some(profile.resume_command.clone())
    }
}

/// Wrap only a validated command while keeping the original durable profile.
pub(super) fn resolved_args(
    program: &str,
    profile: &crate::persist::AgentLaunchSnap,
    resolved: &[String],
) -> Option<Vec<String>> {
    // The bare profile remains durable. The worker's concrete executable is
    // used for this spawn only, so a later restore resolves its own PATH.
    let profile = &crate::persist::AgentLaunchSnap {
        resume_command: restore_argv(profile)?,
        ..profile.clone()
    };
    native_resume_args(program, profile)?;
    if resolved.len() != profile.resume_command.len()
        || resolved.get(1..) != profile.resume_command.get(1..)
        || !Path::new(resolved.first()?).is_absolute()
    {
        return None;
    }
    let invocation = crate::workflow::shell_join(resolved)?;
    let wrapped = crate::workflow::announce_wrapped(&profile.provider, &invocation);
    Some(vec![
        "-c".into(),
        format!("{wrapped}; exec {}", crate::workflow::shell_quote(program)),
    ])
}

impl Server {
    fn request_native(&mut self, target: NativeResumeTarget, provider: String, argv: Vec<String>) {
        let request = self.next_native_resume;
        self.next_native_resume = self.next_native_resume.saturating_add(1);
        if let NativeResumeTarget::Restore { pane, .. } = &target {
            self.native_restore_requests.insert(*pane, request);
        }
        let wait_for_slot = matches!(&target, NativeResumeTarget::Restore { .. });
        self.native_resumes.insert(
            request,
            PendingNativeResume {
                target,
                workspace: self.name.clone(),
                search_path: self.agent_search_path.clone(),
            },
        );
        self.agents.send(CoreToAgent::NativeResumeResolve {
            request,
            wait_for_slot,
            provider,
            argv,
            search_path: self.agent_search_path.clone(),
        });
    }

    /// Keep saved output childless until its own executable has been checked.
    pub(super) fn request_native_restore(
        &mut self,
        pane: PaneId,
        project: Option<ProjectId>,
        project_cwd: Option<PathBuf>,
        profile: crate::persist::AgentLaunchSnap,
    ) {
        let token = self.panes[&pane].token;
        let argv = restore_argv(&profile).unwrap_or_default();
        self.request_native(
            NativeResumeTarget::Restore {
                pane,
                token,
                project,
                project_cwd,
            },
            profile.provider,
            argv,
        );
    }

    /// Continue explicit historical resume through the same executable check.
    pub(super) fn request_native_timeline(
        &mut self,
        requester: TimelineRequester,
        evidence: TimelineResumeData,
    ) {
        let provider = evidence.provider.clone();
        let argv = evidence.argv.clone();
        self.request_native(
            NativeResumeTarget::Timeline {
                requester,
                evidence,
            },
            provider,
            argv,
        );
    }

    /// Avoid duplicate filesystem work when Enter repeats during recovery.
    pub(super) fn native_restore_pending(&self, pane: PaneId) -> bool {
        self.native_restore_requests.get(&pane).and_then(|request| self.native_resumes.get(request))
            .is_some_and(|pending| matches!(&pending.target,
                NativeResumeTarget::Restore { token, .. } if self.panes.get(&pane).is_some_and(|pane| pane.token == *token)))
    }

    /// A disconnected caller cannot leave a delayed mutation behind.
    pub(super) fn cancel_native_resumes(&mut self, _reg: &Registry, connection: u64) {
        let cancelled: Vec<_> = self
            .native_history_controls
            .iter()
            .filter(|(target, _)| *target == connection)
            .copied()
            .collect();
        for key @ (_, id) in cancelled {
            self.native_history_controls.remove(&key);
            self.agents.send(CoreToAgent::ControlResponse {
                connection,
                response: ControlResponse::error(
                    id,
                    "cancelled",
                    "The control caller disconnected",
                ),
            });
        }
        self.native_resumes.retain(|_, pending| {
            if let NativeResumeTarget::Timeline {
                requester:
                    TimelineRequester::Control {
                        connection: target,
                        id,
                    },
                ..
            } = &pending.target
            {
                if *target == connection {
                    // Release runtime request accounting even after its socket closed.
                    self.agents.send(CoreToAgent::ControlResponse {
                        connection,
                        response: ControlResponse::error(
                            *id,
                            "cancelled",
                            "The control caller disconnected",
                        ),
                    });
                    return false;
                }
            }
            true
        });
    }

    /// Apply a worker result only to the captured Workspace and target lifetime.
    pub(super) fn finish_native_resume(
        &mut self,
        reg: &Registry,
        request: u64,
        result: Result<Vec<String>, String>,
    ) {
        let Some(pending) = self.native_resumes.remove(&request) else {
            return;
        };
        let result =
            if pending.workspace != self.name || pending.search_path != self.agent_search_path {
                Err(
                "The Workspace or executable search path changed while checking the saved command"
                    .into(),
            )
            } else {
                result
            };
        match pending.target {
            NativeResumeTarget::Timeline {
                requester,
                evidence,
            } => {
                let result = if self.running {
                    result.map(|argv| (evidence, argv))
                } else {
                    Err("The Workspace is stopping".into())
                };
                self.finish_timeline_resume_resolved(reg, requester, result);
            }
            NativeResumeTarget::Restore {
                pane,
                token,
                project,
                project_cwd,
            } => {
                if self.native_restore_requests.get(&pane) == Some(&request) {
                    self.native_restore_requests.remove(&pane);
                }
                if !self.running
                    || self
                        .panes
                        .get(&pane)
                        .is_none_or(|p| p.token != token || p.recovery.is_none())
                {
                    return;
                }
                let current_project = self
                    .windows
                    .iter()
                    .find(|window| window.layout.contains_pane(pane))
                    .map(|window| window.project);
                let current_root = self
                    .windows
                    .iter()
                    .find(|window| window.layout.contains_pane(pane))
                    .and_then(|window| {
                        self.projects
                            .iter()
                            .find(|project| project.id == window.project)
                    })
                    .map(|project| PathBuf::from(&project.root));
                let result = if !self.event_writes_enabled || self.durability_error.is_some() {
                    Err("History is unavailable; native resume was not started".into())
                } else if current_project != project || current_root != project_cwd {
                    Err("The owning Project changed while checking the saved executable".into())
                } else {
                    result
                };
                let old = self
                    .panes
                    .remove(&pane)
                    .expect("validated unavailable Pane");
                let saved = old
                    .recovery
                    .as_ref()
                    .expect("validated recovery")
                    .saved
                    .clone();
                let saved = crate::persist::PaneSnap {
                    id: saved.id,
                    cwd: saved.cwd,
                    content: saved.content.into_stored_lines(),
                    metadata: old
                        .metadata
                        .iter()
                        .filter(|(_, value)| value.expires.is_none())
                        .map(|(key, value)| (key.clone(), value.value.clone()))
                        .collect(),
                    launch_args: saved.launch_args,
                    agent_launch: saved.agent_launch,
                };
                if let Err(error) = self.restore_pane_resolved(
                    reg,
                    saved,
                    current_project,
                    current_root.as_deref(),
                    Some(result),
                ) {
                    eprintln!("uniterm: Pane {} resume failed: {error}", pane.0);
                    self.panes.insert(pane, old);
                    return;
                }
                let _ = reg.deregister(&mut SourceFd(&old.pty.raw_fd()));
                self.pane_tokens.remove(&old.token);
                if self.panes[&pane].agent.is_some() {
                    self.recovery_panes.retain(|id| *id != pane);
                }
                self.relayout();
                self.persist();
                self.full_repaint_all(reg);
            }
        }
    }

    /// A single event owns the binding, saved profile, Task transfer and optional
    /// new Today layout, so every committed prefix remains recoverable.
    pub(super) fn bind_native_agent(
        &mut self,
        pane: PaneId,
        profile: crate::persist::AgentLaunchSnap,
        previous_invocation: Option<u64>,
        predecessor: Option<u64>,
    ) {
        let invocation = self
            .panes
            .get(&pane)
            .and_then(|pane| pane.pty.foreground_process_group());
        let structure = predecessor.map(|_| {
            let mut state = crate::eventlog::StructuralProjection::from_snapshot(&self.snapshot());
            if let Some(target) = state
                .windows
                .iter_mut()
                .flat_map(|window| &mut window.panes)
                .find(|target| target.id == pane)
            {
                target.agent_launch = Some(crate::eventlog::StructuralAgentLaunch {
                    provider: profile.provider.clone(),
                    session_id: profile.session_id.clone(),
                    resume_command: profile.resume_command.clone(),
                });
            }
            Box::new(state)
        });
        self.append_event(crate::eventlog::LogEvent::AgentResumed {
            pane: pane.0,
            provider: profile.provider.clone(),
            session_id: profile.session_id.clone(),
            resume_command: profile.resume_command.clone(),
            previous_invocation,
            invocation,
            predecessor,
            structure,
        });
        self.install_agent_binding(pane, &profile.provider);
        if let Some(agent) = self
            .panes
            .get_mut(&pane)
            .and_then(|pane| pane.agent.as_mut())
        {
            agent.session_id = profile.session_id;
            agent.resume_command = profile.resume_command;
            agent.evidence = "restored through provider-owned native resume".into();
        }
        self.restored_invocations.remove(&pane);
        self.append_event(crate::eventlog::LogEvent::AgentStatus {
            pane: pane.0,
            status: AgentStatus::Starting,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::reaper::common;
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    fn fixture() -> (Server, Poll, PathBuf) {
        common::isolate_state();
        let name = common::unique_workspace_name();
        let dir = common::socket_root().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join(format!("{name}.sock"));
        let (mut server, poll) =
            Server::bind_internal(&socket, "/bin/sh", &[], 80, 24, false).unwrap();
        server.agent_search_path = vec![dir.to_string_lossy().into_owned()];
        server.projects[0].root = dir.to_string_lossy().into_owned();
        server.event_writes_enabled = true;
        (server, poll, dir)
    }

    fn profile() -> crate::persist::AgentLaunchSnap {
        crate::persist::AgentLaunchSnap {
            provider: "codex".into(),
            session_id: Some("original-session".into()),
            resume_command: vec![
                "codex".into(),
                "resume".into(),
                "original-session".into(),
                "--profile".into(),
                "explicit-profile".into(),
            ],
        }
    }

    fn install(dir: &Path) {
        let executable = dir.join("codex");
        std::fs::write(&executable, "#!/bin/sh\nexec /bin/cat\n").unwrap();
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn drain_native(server: &mut Server, poll: &mut Poll) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut events = mio::Events::with_capacity(32);
        while !server.native_resumes.is_empty() {
            for reply in server.agents.drain() {
                server.on_agent_reply(poll.registry(), reply);
            }
            if server.native_resumes.is_empty() {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "native worker did not finish");
            poll.poll(&mut events, Some(remaining)).unwrap();
        }
    }

    #[test]
    fn native_restore_missing_provider_retries_and_reassociates_only_exact_task() {
        let (mut server, mut poll, dir) = fixture();
        let pane = server.windows[0].active;
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .term
            .feed(b"original output worth keeping");
        let mut snapshot = server
            .snapshot()
            .map_content(uniterm_core::GridCapture::into_stored_lines);
        let saved = &mut snapshot.windows[0].panes[0];
        saved.cwd = Some(dir.to_string_lossy().into_owned());
        saved.agent_launch = Some(profile());
        let original = saved.clone();
        server.restore(poll.registry(), snapshot).unwrap();
        server.restored_invocations.insert(pane, 42);
        for (id, target, invocation) in [(1, pane, 42), (2, pane, 41), (3, PaneId(u64::MAX), 42)] {
            server
                .tasks
                .insert(id, "saved task", uniterm_core::TaskStatus::Doing);
            server.tasks.set_details(
                id,
                "saved notes",
                uniterm_core::tasks::TaskContext {
                    pane: Some(target),
                    invocation: Some(invocation),
                    ..Default::default()
                },
            );
        }
        assert!(server.panes[&pane].agent.is_none());
        assert_eq!(server.panes[&pane].pty.child_pid(), None);
        drain_native(&mut server, &mut poll);
        assert_eq!(server.panes[&pane].pty.child_pid(), None);
        assert!(server.panes[&pane].term.dump_text().contains("unavailable"));
        let current = server
            .snapshot()
            .map_content(uniterm_core::GridCapture::into_stored_lines);
        assert_eq!(current.windows[0].panes[0].content, original.content);
        assert_eq!(
            current.windows[0].panes[0].agent_launch,
            original.agent_launch
        );
        assert_eq!(server.tasks.get(1).unwrap().context.invocation, Some(42));
        assert_eq!(server.restored_invocations[&pane], 42);
        install(&dir);
        assert!(Server::queue_pane_input(
            poll.registry(),
            server.panes.get_mut(&pane).unwrap(),
            b"\r"
        ));
        server.retry_restored_panes(poll.registry());
        // Extra Enter input during a pending check cannot enqueue another probe.
        let request = server.native_restore_requests[&pane];
        Server::queue_pane_input(poll.registry(), server.panes.get_mut(&pane).unwrap(), b"\r");
        server.retry_restored_panes(poll.registry());
        assert_eq!(server.native_restore_requests[&pane], request);
        drain_native(&mut server, &mut poll);
        assert!(server.panes[&pane].pty.child_pid().is_some());
        assert!(server.panes[&pane].recovery.is_none());
        assert_eq!(
            server.panes[&pane].agent.as_ref().unwrap().resume_command,
            profile().resume_command
        );
        let current = server.timeline_invocations[&pane];
        assert_eq!(
            server.tasks.get(1).unwrap().context.invocation,
            Some(current)
        );
        assert_eq!(server.tasks.get(2).unwrap().context.invocation, Some(41));
        assert_eq!(server.tasks.get(3).unwrap().context.invocation, Some(42));
        assert!(!server.restored_invocations.contains_key(&pane));
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn native_restore_without_a_session_starts_the_provider_fresh() {
        let (mut server, mut poll, dir) = fixture();
        install(&dir);
        let pane = server.windows[0].active;
        let mut snapshot = server
            .snapshot()
            .map_content(uniterm_core::GridCapture::into_stored_lines);
        let empty = crate::persist::AgentLaunchSnap {
            provider: "codex".into(),
            session_id: None,
            resume_command: Vec::new(),
        };
        snapshot.windows[0].panes[0].cwd = Some(dir.to_string_lossy().into_owned());
        snapshot.windows[0].panes[0].agent_launch = Some(empty.clone());
        server.restore(poll.registry(), snapshot).unwrap();
        drain_native(&mut server, &mut poll);
        let restored = &server.panes[&pane];
        assert!(restored.pty.child_pid().is_some());
        assert!(restored.recovery.is_none());
        assert!(!server.recovery_panes.contains(&pane));
        assert!(restored.agent.as_ref().unwrap().resume_command.is_empty());
        let text = restored.term.dump_text();
        assert!(text.contains("Started a new codex session"), "{text:?}");
        assert!(!text.contains("press Enter to retry"));
        assert_eq!(
            server.snapshot().windows[0].panes[0].agent_launch,
            Some(empty)
        );
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn native_restore_rejects_changed_path_and_closed_or_replaced_panes() {
        let (mut server, mut poll, dir) = fixture();
        install(&dir);
        let pane = server.windows[0].active;
        let mut snapshot = server
            .snapshot()
            .map_content(uniterm_core::GridCapture::into_stored_lines);
        snapshot.windows[0].panes[0].agent_launch = Some(profile());
        snapshot.windows[0].panes[0].cwd = Some(dir.to_string_lossy().into_owned());
        server.restore(poll.registry(), snapshot).unwrap();
        let request = server.native_restore_requests[&pane];
        let argv = resolve("codex", profile().resume_command, &server.agent_search_path).unwrap();
        server.agent_search_path.clear();
        server.finish_native_resume(poll.registry(), request, Ok(argv.clone()));
        assert_eq!(server.panes[&pane].pty.child_pid(), None);
        assert_eq!(
            server.snapshot().windows[0].panes[0].agent_launch,
            Some(profile())
        );
        server.request_native_restore(
            pane,
            Some(server.projects[0].id),
            Some(dir.clone()),
            profile(),
        );
        let request = server.native_restore_requests[&pane];
        let original_token = server.panes[&pane].token;
        server.panes.get_mut(&pane).unwrap().token = Token(usize::MAX);
        server.finish_native_resume(poll.registry(), request, Ok(argv.clone()));
        assert_eq!(server.panes[&pane].pty.child_pid(), None);
        server.panes.get_mut(&pane).unwrap().token = original_token;
        server.request_native_restore(pane, Some(server.projects[0].id), Some(dir), profile());
        let request = server.native_restore_requests[&pane];
        let old = server.panes.remove(&pane).unwrap();
        server.finish_native_resume(poll.registry(), request, Ok(argv));
        assert!(!server.panes.contains_key(&pane));
        server.panes.insert(pane, old);
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn native_restore_waits_for_capacity_without_failing_a_large_fleet() {
        const CHILD: &str = "UNITERM_NATIVE_FLEET_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let state = common::isolate_state();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "server::native_resume::tests::native_restore_waits_for_capacity_without_failing_a_large_fleet", "--nocapture"])
                .env(CHILD, "1").env("XDG_STATE_HOME", state)
                .env("XDG_RUNTIME_DIR", common::socket_root()).status().unwrap();
            assert!(status.success());
            return;
        }
        let (mut server, mut poll, dir) = fixture();
        install(&dir);
        let key = dir.to_string_lossy().into_owned();
        let (started_tx, started_rx) = crossbeam_channel::unbounded();
        let (release_tx, release_rx) = crossbeam_channel::unbounded();
        struct ReleaseGate(String, crossbeam_channel::Sender<()>);
        impl Drop for ReleaseGate {
            fn drop(&mut self) {
                NATIVE_TEST_GATES.lock().unwrap().remove(&self.0);
                for _ in 0..40 {
                    let _ = self.1.send(());
                }
            }
        }
        let guard = ReleaseGate(key.clone(), release_tx);
        NATIVE_TEST_GATES.lock().unwrap().insert(
            key,
            NativeTestGate {
                started: started_tx,
                release: release_rx,
            },
        );
        let mut snapshot = server
            .snapshot()
            .map_content(uniterm_core::GridCapture::into_stored_lines);
        let template = snapshot.windows[0].clone();
        snapshot.windows = (1..=40)
            .map(|id| {
                let mut window = template.clone();
                let pane = PaneId(id);
                window.active = pane;
                window.layout = LayoutNode::Leaf(pane);
                window.panes[0].id = pane;
                window.panes[0].cwd = Some(dir.to_string_lossy().into_owned());
                window.panes[0].agent_launch = Some(profile());
                window
            })
            .collect();
        snapshot.next_pane_id = 41;
        server.restore(poll.registry(), snapshot).unwrap();
        // The runtime transport has one slot. Drive the core-owned outbox
        // like the event loop does, including requests waiting for a permit.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut started = 0;
        while started < 32 || server.agents.backpressured() {
            for reply in server.agents.drain() {
                assert!(!matches!(
                    reply,
                    uniterm_proto::AgentToCore::NativeResumeResolved { .. }
                ));
                server.on_agent_reply(poll.registry(), reply);
            }
            started += started_rx.try_iter().count();
            assert!(started <= 32, "more than 32 filesystem workers ran at once");
            assert!(
                Instant::now() < deadline,
                "native restore queue did not drain"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            started_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "more than 32 filesystem workers ran at once"
        );
        // Capacity wait produces no failure reply or retry requirement.
        for reply in server.agents.drain() {
            assert!(!matches!(
                reply,
                uniterm_proto::AgentToCore::NativeResumeResolved { .. }
            ));
            server.on_agent_reply(poll.registry(), reply);
        }
        assert_eq!(server.native_resumes.len(), 40);
        assert!(server
            .panes
            .values()
            .all(|pane| pane.pty.child_pid().is_none()));
        drop(guard);
        drain_native(&mut server, &mut poll);
        assert_eq!(server.panes.len(), 40);
        assert!(server.panes.values().all(|pane| pane.agent.is_some()
            && pane.recovery.is_none()
            && pane.pty.child_pid().is_some()));
        server.retire_all_ptys(&mut poll);
    }

    fn evidence(server: &Server) -> TimelineResumeData {
        TimelineResumeData {
            manager_read_only: false,
            context: uniterm_core::timeline::TimelineContext {
                project: Some(server.projects[0].id),
                root: server.projects[0].root.clone(),
                ..Default::default()
            },
            predecessor: 1,
            provider: profile().provider,
            session_id: profile().session_id,
            argv: profile().resume_command,
        }
    }

    #[test]
    fn native_timeline_cancellation_and_stale_targets_never_spawn() {
        let (mut server, mut poll, dir) = fixture();
        install(&dir);
        let argv = resolve("codex", profile().resume_command, &server.agent_search_path).unwrap();
        let count = server.panes.len();
        let requester = TimelineRequester::Control {
            connection: 17,
            id: 23,
        };
        server.native_history_controls.insert((17, 23));
        server.cancel_native_resumes(poll.registry(), 17);
        server.finish_timeline_resume(poll.registry(), requester.clone(), Ok(evidence(&server)));
        assert!(server.native_resumes.is_empty());
        server.request_native_timeline(requester.clone(), evidence(&server));
        let request = server.next_native_resume - 1;
        server.cancel_native_resumes(poll.registry(), 17);
        server.finish_native_resume(poll.registry(), request, Ok(argv.clone()));
        assert_eq!(server.panes.len(), count);
        server.request_native_timeline(requester.clone(), evidence(&server));
        let request = server.next_native_resume - 1;
        server.projects[0].root.push_str("/changed");
        server.finish_native_resume(poll.registry(), request, Ok(argv.clone()));
        assert_eq!(server.panes.len(), count);
        server.request_native_timeline(
            TimelineRequester::View {
                generation: u64::MAX,
            },
            evidence(&server),
        );
        let request = server.next_native_resume - 1;
        server.finish_native_resume(poll.registry(), request, Ok(argv.clone()));
        assert_eq!(server.panes.len(), count);
        server.request_native_timeline(requester, evidence(&server));
        let request = server.next_native_resume - 1;
        server.agent_search_path.clear();
        server.finish_native_resume(poll.registry(), request, Ok(argv));
        assert_eq!(server.panes.len(), count);
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn native_timeline_success_retains_original_resume_profile() {
        let (mut server, mut poll, dir) = fixture();
        install(&dir);
        let before = server.panes.len();
        server.request_native_timeline(
            TimelineRequester::Control {
                connection: 17,
                id: 23,
            },
            evidence(&server),
        );
        drain_native(&mut server, &mut poll);
        assert_eq!(server.panes.len(), before + 1);
        let agent = server
            .panes
            .values()
            .find_map(|pane| pane.agent.as_ref())
            .unwrap();
        assert_eq!(agent.resume_command, profile().resume_command);
        assert_eq!(agent.session_id, profile().session_id);
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn native_resolver_rejects_forged_and_foreign_aliases() {
        let (mut server, mut poll, dir) = fixture();
        install(&dir);
        let paths = vec![dir.to_string_lossy().into_owned()];
        assert!(resolve("codex", vec!["sh".into()], &paths).is_err());
        assert!(resolve(
            "codex",
            vec![dir.join("codex").to_string_lossy().into_owned()],
            &paths
        )
        .is_err());
        let foreign = dir.join("claude");
        std::fs::write(&foreign, "#!/bin/sh\nexit\n").unwrap();
        std::fs::set_permissions(&foreign, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_file(dir.join("codex")).unwrap();
        std::os::unix::fs::symlink(foreign, dir.join("codex")).unwrap();
        assert!(resolve("codex", profile().resume_command, &paths).is_err());
        server.retire_all_ptys(&mut poll);
    }
}
