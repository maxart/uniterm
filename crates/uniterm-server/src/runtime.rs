//! The two-runtime skeleton (Decision R1).
//!
//! The hot path is a single-threaded `mio` loop. The agentic half is a `tokio`
//! runtime on its own thread. They communicate *only* over channels, and the
//! agent side wakes the core loop with a [`mio::Waker`] - so the core loop can
//! block indefinitely with no timeout when idle (zero idle wakeups) and still
//! be woken the instant the agent runtime has something to say.
//!
//! This is a Phase 0 skeleton: the agent side here just echoes status updates.
//! In production this thread hosts the supervisor, provider adapters, the
//! workflow/relay engines, git/file watchers, and the control server. The
//! *shape* - crossbeam channels + a Waker, no shared mutable state across the
//! boundary - is the part being proven.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{bounded, select, unbounded, Receiver, Sender};
use mio::{Events, Poll, Token, Waker};
use notify::Watcher as _;
use uniterm_core::AgentStatus;
use uniterm_proto::{
    AgentToCore, ControlCommand, ControlEvent, ControlFrame, ControlResult, ControlStreamError,
    CoreToAgent, FileEntry, FileOperation, PaneId, ProjectId, WorkspaceInfo, FILE_LISTING_LIMIT,
};

/// A terminal tree cannot usefully render an unbounded single directory.
/// Bounding one listing prevents an imported root with millions of immediate
/// children from exhausting the server while leaving the returned prefix
/// browsable.
/// Expanded folders beyond this bound remain browsable and manually
/// refreshable, but do not consume another OS watch registration.
const MAX_PROJECT_WATCHES: usize = 256;
/// Artifact hashing stays bounded even though it runs away from the mio loop.
const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

/// Token identifying the waker in the mio event set. Real PTY/socket fds get
/// their own tokens starting above this.
const WAKER_TOKEN: Token = Token(0);

enum RuntimeInput {
    Core(CoreToAgent),
    Control(crate::control_api::Inbound),
    Subscription(SubscriptionEnd),
    ProviderChanged,
    ProviderLoaded(crate::providers::Catalog),
}

struct ActiveSubscription {
    token: u64,
    live: tokio::sync::mpsc::Sender<crate::eventlog::EventEnvelope>,
}

#[derive(Clone, Copy)]
enum SubscriptionEndAction {
    KeepConnection,
    Disconnect,
}

struct SubscriptionEnd {
    generation: u64,
    connection: u64,
    token: u64,
    action: SubscriptionEndAction,
}

struct SubscriptionStart {
    workspace: String,
    subscription: u64,
    after: u64,
    through: u64,
    output: Sender<Vec<u8>>,
    live: tokio::sync::mpsc::Receiver<crate::eventlog::EventEnvelope>,
    generation: u64,
    token: u64,
    ended: Sender<SubscriptionEnd>,
}

fn control_path_workspace(path: Option<&Path>) -> Option<String> {
    path?
        .file_name()?
        .to_str()?
        .strip_suffix(".control.sock")
        .map(str::to_string)
}

/// Read the durable host catalog and probe sibling listeners only after the
/// Manage Workspaces surface explicitly asks. This runs inside
/// `spawn_blocking`, so filesystem and socket work never enters the mio loop.
fn workspace_catalog_snapshot() -> Vec<WorkspaceInfo> {
    crate::workspace_catalog::list()
        .into_iter()
        .map(|(name, definition)| {
            let running =
                std::os::unix::net::UnixStream::connect(crate::server::default_socket_path(&name))
                    .is_ok();
            WorkspaceInfo {
                name,
                windows: u32::try_from(definition.tab_count()).unwrap_or(u32::MAX),
                panes: 0,
                projects: u32::try_from(definition.projects.len()).unwrap_or(u32::MAX),
                running,
            }
        })
        .collect()
}

fn envelope_frame(
    subscription: u64,
    envelope: crate::eventlog::EventEnvelope,
) -> std::io::Result<ControlFrame> {
    Ok(ControlFrame::Event(ControlEvent {
        version: uniterm_proto::CONTROL_API_VERSION,
        subscription,
        sequence: envelope.sequence,
        timestamp_ms: envelope.timestamp_ms,
        workspace: envelope.workspace,
        event: serde_json::to_value(envelope.event)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?,
    }))
}

fn subscription_error(subscription: u64, error: &std::io::Error) -> ControlFrame {
    ControlFrame::StreamError(ControlStreamError {
        version: uniterm_proto::CONTROL_API_VERSION,
        subscription,
        code: match error.kind() {
            std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof => {
                "invalid_event_stream"
            }
            _ => "event_stream_unavailable",
        }
        .into(),
        message: error.to_string(),
    })
}

async fn stream_subscription(start: SubscriptionStart) {
    let SubscriptionStart {
        workspace,
        subscription,
        after,
        through,
        output,
        mut live,
        generation,
        token,
        ended,
    } = start;
    let report = |action| {
        let _ = ended.try_send(SubscriptionEnd {
            generation,
            connection: subscription,
            token,
            action,
        });
    };
    const HISTORY_QUEUE: usize = 8;
    let (history_tx, mut history_rx) = tokio::sync::mpsc::channel(HISTORY_QUEUE);
    let catch_up = tokio::task::spawn_blocking(move || {
        crate::eventlog::visit_through(&workspace, after, through, |envelope| {
            history_tx.blocking_send(envelope).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "subscription catch-up receiver closed",
                )
            })
        })
    });

    while let Some(envelope) = history_rx.recv().await {
        let Ok(frame) = envelope_frame(subscription, envelope) else {
            report(SubscriptionEndAction::Disconnect);
            return;
        };
        if !crate::control_api::send_bounded(&output, frame) {
            report(SubscriptionEndAction::Disconnect);
            return;
        }
    }
    let catch_up = match catch_up.await {
        Ok(result) => result,
        Err(error) => Err(std::io::Error::other(format!(
            "event catch-up worker failed: {error}"
        ))),
    };
    if let Err(error) = catch_up {
        let sent =
            crate::control_api::send_bounded(&output, subscription_error(subscription, &error));
        report(if sent {
            SubscriptionEndAction::KeepConnection
        } else {
            SubscriptionEndAction::Disconnect
        });
        return;
    }

    let mut cursor = through;
    while let Some(envelope) = live.recv().await {
        if envelope.sequence <= cursor {
            continue;
        }
        if envelope.sequence != cursor.saturating_add(1) {
            let error = std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "live event sequence is not contiguous",
            );
            let sent =
                crate::control_api::send_bounded(&output, subscription_error(subscription, &error));
            report(if sent {
                SubscriptionEndAction::KeepConnection
            } else {
                SubscriptionEndAction::Disconnect
            });
            return;
        }
        cursor = envelope.sequence;
        let Ok(frame) = envelope_frame(subscription, envelope) else {
            report(SubscriptionEndAction::Disconnect);
            return;
        };
        if !crate::control_api::send_bounded(&output, frame) {
            report(SubscriptionEndAction::Disconnect);
            return;
        }
    }
}

/// Handles the core loop keeps to talk to the agent runtime.
pub struct AgentRuntime {
    to_agent: Sender<CoreToAgent>,
    from_agent: Receiver<AgentToCore>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AgentRuntime {
    /// Send a message to the agent runtime (called from the hot path; cheap and
    /// non-blocking).
    pub fn send(&self, msg: CoreToAgent) {
        // A full channel or dead runtime must never stall the render loop, so a
        // failed send is dropped rather than blocked on. The registry/event log
        // remain the durable record; a dropped in-flight hint is recoverable.
        let _ = self.to_agent.send(msg);
    }

    /// Take every queued reply (called by the core loop on a waker event).
    pub(crate) fn drain(&self) -> Vec<AgentToCore> {
        let mut out = Vec::new();
        while let Ok(m) = self.from_agent.try_recv() {
            out.push(m);
        }
        out
    }

    /// Flush queued runtime work and join its thread before server discovery
    /// state is removed. This makes a disappeared socket a reliable signal
    /// that final persistence writes have completed.
    pub(crate) fn shutdown(&mut self) {
        let Some(handle) = self.thread.take() else {
            return;
        };
        let (dead_tx, _dead_rx) = unbounded();
        let old = std::mem::replace(&mut self.to_agent, dead_tx);
        drop(old);
        let _ = handle.join();
    }
}

impl Drop for AgentRuntime {
    fn drop(&mut self) {
        // Dropping the sender closes the channel; the agent thread's recv loop
        // ends and the runtime shuts down. Join so shutdown is orderly.
        // (Replace the sender with a dropped one by taking the thread handle.)
        self.shutdown();
    }
}

/// Spawn the tokio agent runtime on its own thread and return handles plus the
/// waker the core loop must register in its `mio::Poll`.
pub(crate) fn spawn_agent_runtime(waker: Arc<Waker>) -> AgentRuntime {
    spawn_agent_runtime_inner(waker, None, None)
}

pub(crate) fn spawn_agent_runtime_with_control(
    waker: Arc<Waker>,
    control_path: PathBuf,
) -> std::io::Result<AgentRuntime> {
    let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
    let mut runtime = spawn_agent_runtime_inner(waker, Some(control_path), Some(ready_tx));
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(runtime),
        Ok(Err((kind, message))) => {
            runtime.shutdown();
            Err(std::io::Error::new(kind, message))
        }
        Err(_) => {
            runtime.shutdown();
            Err(std::io::Error::other(
                "control API runtime stopped during startup",
            ))
        }
    }
}

fn spawn_agent_runtime_inner(
    waker: Arc<Waker>,
    control_path: Option<PathBuf>,
    control_ready: Option<Sender<Result<(), (std::io::ErrorKind, String)>>>,
) -> AgentRuntime {
    let (to_agent_tx, to_agent_rx) = unbounded::<CoreToAgent>();
    let (from_agent_tx, from_agent_rx) = unbounded::<AgentToCore>();

    let thread = std::thread::Builder::new()
        .name("uniterm-agent-rt".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("build tokio agent runtime");
            rt.block_on(agent_main(
                to_agent_rx,
                from_agent_tx,
                waker,
                control_path,
                control_ready,
            ));
        })
        .expect("spawn agent runtime thread");

    AgentRuntime {
        to_agent: to_agent_tx,
        from_agent: from_agent_rx,
        thread: Some(thread),
    }
}

/// The agent runtime's main loop. Bridges the sync crossbeam receiver into async
/// via `spawn_blocking`, dispatches each message, and wakes the core loop when
/// it has a reply. This stand-in models the real supervisor/workflow dispatch.
async fn agent_main(
    rx: Receiver<CoreToAgent>,
    tx: Sender<AgentToCore>,
    waker: Arc<Waker>,
    control_path: Option<PathBuf>,
    control_ready: Option<Sender<Result<(), (std::io::ErrorKind, String)>>>,
) {
    let mut control_workspace = control_path_workspace(control_path.as_deref());
    let (control_tx, control_rx) =
        bounded(usize::try_from(uniterm_proto::CONTROL_MAX_QUEUED_REQUESTS).unwrap_or(64));
    let (subscription_end_tx, subscription_end_rx) =
        bounded(usize::try_from(uniterm_proto::CONTROL_MAX_CONNECTIONS).unwrap_or(128));
    let mut control_generation = 1u64;
    let mut control = None;
    if let Some(path) = control_path {
        match crate::control_api::Listener::bind(path, control_tx.clone(), control_generation) {
            Ok(listener) => {
                control = Some(listener);
                if let Some(ready) = control_ready {
                    let _ = ready.send(Ok(()));
                }
            }
            Err(error) => {
                if let Some(ready) = control_ready {
                    let _ = ready.send(Err((error.kind(), error.to_string())));
                }
                return;
            }
        }
    }
    let mut control_outputs = HashMap::new();
    let mut subscribe_after = HashMap::new();
    let mut control_pending_requests = 0usize;
    let mut subscriptions: HashMap<u64, ActiveSubscription> = HashMap::new();
    let mut next_subscription_token = 1u64;
    let mut providers = Arc::new(crate::providers::Catalog::load());
    let (provider_changed_tx, provider_changed_rx) = bounded(1);
    let _provider_watcher =
        crate::providers::ManifestWatcher::start(provider_changed_tx.clone()).ok();
    let (provider_loaded_tx, provider_loaded_rx) = bounded(1);
    let mut provider_reload_inflight = false;
    let mut provider_reload_again = false;
    let mut file_watchers: HashMap<ProjectId, ProjectWatcher> = HashMap::new();
    let mut artifact_watchers: HashMap<ProjectId, ArtifactProjectWatcher> = HashMap::new();
    let mut git_watchers = crate::git_status::GitWatchManager::new();
    let mut dev_probes: HashMap<(PaneId, u16), tokio::task::JoinHandle<()>> = HashMap::new();
    let mut dev_probe_ports: HashSet<(PaneId, u16)> = HashSet::new();
    let mut dev_watch_active = false;
    let mut process_cache: HashMap<(PaneId, i32), Option<crate::providers::Match>> = HashMap::new();
    let mut poisoned_event_logs: HashSet<String> = HashSet::new();
    let worktree_serial = Arc::new(tokio::sync::Mutex::new(()));
    loop {
        let rx2 = rx.clone();
        let control_rx2 = control_rx.clone();
        let subscription_end_rx2 = subscription_end_rx.clone();
        let provider_changed_rx2 = provider_changed_rx.clone();
        let provider_loaded_rx2 = provider_loaded_rx.clone();
        let recv = tokio::task::spawn_blocking(move || {
            select! {
                recv(rx2) -> msg => msg.map(RuntimeInput::Core),
                recv(control_rx2) -> msg => msg.map(RuntimeInput::Control),
                recv(subscription_end_rx2) -> msg => msg.map(RuntimeInput::Subscription),
                recv(provider_changed_rx2) -> msg => msg.map(|()| RuntimeInput::ProviderChanged),
                recv(provider_loaded_rx2) -> msg => msg.map(RuntimeInput::ProviderLoaded),
            }
        })
        .await;
        let input = match recv {
            Ok(Ok(input)) => input,
            _ => break,
        };
        let msg = match input {
            RuntimeInput::Core(msg) => msg,
            RuntimeInput::ProviderChanged => {
                if provider_reload_inflight {
                    provider_reload_again = true;
                } else {
                    provider_reload_inflight = true;
                    let loaded = provider_loaded_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = loaded.try_send(crate::providers::Catalog::load());
                    });
                }
                continue;
            }
            RuntimeInput::ProviderLoaded(catalog) => {
                let activate = catalog.activation_valid();
                if activate {
                    providers = Arc::new(catalog);
                    process_cache.clear();
                }
                provider_reload_inflight = false;
                if activate {
                    if tx.send(AgentToCore::ProviderManifestsReloaded).is_err() {
                        break;
                    }
                    let _ = waker.wake();
                }
                if provider_reload_again {
                    provider_reload_again = false;
                    let _ = provider_changed_tx.try_send(());
                }
                continue;
            }
            RuntimeInput::Subscription(ended) => {
                if ended.generation != control_generation
                    || subscriptions
                        .get(&ended.connection)
                        .is_none_or(|active| active.token != ended.token)
                {
                    continue;
                }
                subscriptions.remove(&ended.connection);
                if matches!(ended.action, SubscriptionEndAction::Disconnect) {
                    control_outputs.remove(&ended.connection);
                    subscribe_after.retain(|(tracked, _), _| *tracked != ended.connection);
                }
                continue;
            }
            RuntimeInput::Control(crate::control_api::Inbound::Connected {
                generation,
                connection,
                output,
            }) => {
                if generation != control_generation {
                    continue;
                }
                control_outputs.insert(connection, output);
                continue;
            }
            RuntimeInput::Control(crate::control_api::Inbound::Request {
                generation,
                connection,
                request,
            }) => {
                if generation != control_generation || !control_outputs.contains_key(&connection) {
                    continue;
                }
                let pending_limit =
                    usize::try_from(uniterm_proto::CONTROL_MAX_QUEUED_REQUESTS).unwrap_or(64);
                if control_pending_requests >= pending_limit {
                    control_outputs.remove(&connection);
                    subscriptions.remove(&connection);
                    subscribe_after.retain(|(tracked, _), _| *tracked != connection);
                    continue;
                }
                if let ControlCommand::Subscribe { after_sequence } = &request.command {
                    subscribe_after.insert((connection, request.id), *after_sequence);
                }
                if tx
                    .send(AgentToCore::ControlRequest {
                        connection,
                        request,
                    })
                    .is_err()
                {
                    break;
                }
                control_pending_requests = control_pending_requests.saturating_add(1);
                let _ = waker.wake();
                continue;
            }
            RuntimeInput::Control(crate::control_api::Inbound::Disconnected {
                generation,
                connection,
            }) => {
                if generation != control_generation {
                    continue;
                }
                control_outputs.remove(&connection);
                subscribe_after.retain(|(tracked, _), _| *tracked != connection);
                subscriptions.remove(&connection);
                continue;
            }
        };

        dev_probes.retain(|_, task| !task.is_finished());

        if let CoreToAgent::PtyExited { pane } | CoreToAgent::PaneClosed { pane } = &msg {
            process_cache.retain(|(tracked, _), _| tracked != pane);
            dev_probe_ports.retain(|(tracked, _)| tracked != pane);
            dev_probes.retain(|(tracked, _), task| {
                if tracked == pane {
                    task.abort();
                    false
                } else {
                    true
                }
            });
        }

        let reply = match msg {
            CoreToAgent::ControlRename { workspace, path } => {
                control_outputs.clear();
                subscribe_after.clear();
                subscriptions.clear();
                drop(control.take());
                control_generation = control_generation.saturating_add(1);
                control_workspace = Some(workspace);
                control = crate::control_api::Listener::bind(
                    PathBuf::from(path),
                    control_tx.clone(),
                    control_generation,
                )
                .map_err(|error| eprintln!("uniterm: renamed control API unavailable: {error}"))
                .ok();
                None
            }
            CoreToAgent::ControlResponse {
                connection,
                mut response,
            } => {
                control_pending_requests = control_pending_requests.saturating_sub(1);
                let pending_cursor = subscribe_after.remove(&(connection, response.id));
                let mut subscribed_through = match &response.result {
                    Some(ControlResult::Subscribed {
                        current_sequence, ..
                    }) => Some(*current_sequence),
                    _ => None,
                };
                if subscribed_through.is_some()
                    && subscriptions
                        .get(&connection)
                        .is_some_and(|subscription| !subscription.live.is_closed())
                {
                    response = uniterm_proto::ControlResponse::error(
                        response.id,
                        "already_subscribed",
                        "one control connection may own only one event subscription",
                    );
                    subscribed_through = None;
                }
                if let Some(output) = control_outputs.get(&connection).cloned() {
                    if !crate::control_api::send_bounded(&output, ControlFrame::Response(response))
                    {
                        control_outputs.remove(&connection);
                        subscriptions.remove(&connection);
                    } else if let Some(through) = subscribed_through {
                        let after = pending_cursor.unwrap_or(0);
                        if let Some(workspace) = control_workspace.clone() {
                            let capacity =
                                usize::try_from(uniterm_proto::CONTROL_MAX_QUEUED_FRAMES)
                                    .unwrap_or(64);
                            let (live, live_rx) = tokio::sync::mpsc::channel(capacity);
                            let subscription_token = next_subscription_token;
                            next_subscription_token = next_subscription_token.saturating_add(1);
                            subscriptions.insert(
                                connection,
                                ActiveSubscription {
                                    token: subscription_token,
                                    live,
                                },
                            );
                            tokio::spawn(stream_subscription(SubscriptionStart {
                                workspace,
                                subscription: connection,
                                after,
                                through,
                                output,
                                live: live_rx,
                                generation: control_generation,
                                token: subscription_token,
                                ended: subscription_end_tx.clone(),
                            }));
                        }
                    }
                }
                None
            }
            CoreToAgent::WorktreeRun {
                request,
                workspace,
                operation,
            } => {
                if poisoned_event_logs.contains(&workspace)
                    && !matches!(
                        &operation,
                        uniterm_proto::WorktreeRuntimeOperation::Inspect {
                            action: uniterm_proto::WorktreeAction::List,
                            ..
                        } | uniterm_proto::WorktreeRuntimeOperation::RollbackAdd { .. }
                    )
                {
                    Some(AgentToCore::WorktreeFinished {
                    request,
                    result: crate::worktree::reject(
                        operation,
                        "worktree operation refused because Workspace durability is unavailable",
                    ),
                    })
                } else {
                    let tx = tx.clone();
                    let waker = waker.clone();
                    let serial = worktree_serial.clone();
                    tokio::spawn(async move {
                        let _guard = serial.lock().await;
                        let result =
                            tokio::task::spawn_blocking(move || crate::worktree::run(operation))
                                .await
                                .expect("worktree operation");
                        if tx
                            .send(AgentToCore::WorktreeFinished { request, result })
                            .is_ok()
                        {
                            let _ = waker.wake();
                        }
                    });
                    None
                }
            }
            CoreToAgent::OscAgentEvent { pane, .. } => {
                // Real code parses the OSC 777 payload and reconciles status.
                Some(AgentToCore::SetAgentStatus {
                    pane,
                    status: AgentStatus::Working,
                })
            }
            CoreToAgent::PtyExited { pane } | CoreToAgent::PaneClosed { pane } => {
                Some(AgentToCore::SetAgentStatus {
                    pane,
                    status: AgentStatus::Exited,
                })
            }
            // Manage Agents disk work: PATH probes, connector settings reads,
            // and the toggle's settings-file edit all block on the filesystem,
            // so they run here (spawn_blocking), never on the core loop.
            CoreToAgent::AgentsDiskQuery {
                client,
                search_path,
            } => Some(
                tokio::task::spawn_blocking(move || AgentToCore::AgentsDiskState {
                    client,
                    providers: providers_disk_state(&search_path),
                })
                .await
                .expect("agents disk probe"),
            ),
            CoreToAgent::ConnectorToggle {
                agent,
                client,
                search_path,
            } => Some(
                tokio::task::spawn_blocking(move || {
                    let _ = crate::connectors::toggle(&agent);
                    AgentToCore::AgentsDiskState {
                        client,
                        providers: providers_disk_state(&search_path),
                    }
                })
                .await
                .expect("connector toggle"),
            ),
            CoreToAgent::ConfigSave { client, text } => Some(
                tokio::task::spawn_blocking(move || AgentToCore::ConfigSaved {
                    client,
                    error: save_config_atomic(&text)
                        .err()
                        .map(|error| error.to_string()),
                })
                .await
                .expect("config save"),
            ),
            CoreToAgent::EditorSettingsValidate {
                client,
                editor,
                editor_rules,
            } => Some(
                tokio::task::spawn_blocking(move || {
                    let error = validate_editor_commands(&editor, &editor_rules).err();
                    AgentToCore::EditorSettingsValidated {
                        client,
                        editor,
                        editor_rules,
                        error,
                    }
                })
                .await
                .expect("editor settings validation"),
            ),
            CoreToAgent::EditorOpen {
                project,
                path,
                command,
            } => Some(
                tokio::task::spawn_blocking(move || AgentToCore::EditorResolved {
                    project,
                    path,
                    error: validate_editor_command(&command).err(),
                    command,
                })
                .await
                .expect("file editor validation"),
            ),
            CoreToAgent::SnapshotSave { name, bytes } => {
                if poisoned_event_logs.contains(&name) {
                    Some(AgentToCore::DurabilityError {
                        workspace: name,
                        operation: "snapshot skipped after event-log failure".into(),
                        error: "the event stream has an unrecorded sequence".into(),
                    })
                } else {
                    let workspace = name.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        // The structural event is authoritative and was queued
                        // before this checkpoint. Flush it first so a hard
                        // power loss cannot leave a snapshot claiming a
                        // sequence that never reached stable storage.
                        crate::eventlog::sync(&name)?;
                        crate::persist::save_bytes(&name, &bytes)
                    })
                    .await;
                    match result {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(AgentToCore::DurabilityError {
                            workspace,
                            operation: "snapshot save".into(),
                            error: error.to_string(),
                        }),
                        Err(error) => Some(AgentToCore::DurabilityError {
                            workspace,
                            operation: "snapshot worker".into(),
                            error: error.to_string(),
                        }),
                    }
                }
            }
            CoreToAgent::SnapshotDelete { name } => {
                let _ = tokio::task::spawn_blocking(move || crate::persist::delete(&name)).await;
                None
            }
            CoreToAgent::EventAppend { name, line } => {
                let workspace = name.clone();
                if poisoned_event_logs.contains(&workspace) {
                    Some(AgentToCore::DurabilityError {
                        workspace,
                        operation: "event append skipped after prior failure".into(),
                        error: "the durable stream is frozen at its last consistent prefix".into(),
                    })
                } else {
                    let stream_line = line.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        crate::eventlog::append_line(&name, &line)
                    })
                    .await;
                    match result {
                        Ok(Ok(())) => {
                            if let Ok(envelope) =
                                serde_json::from_str::<crate::eventlog::EventEnvelope>(&stream_line)
                            {
                                let ids: Vec<u64> = subscriptions.keys().copied().collect();
                                for connection in ids {
                                    if envelope.workspace != workspace {
                                        continue;
                                    }
                                    let sent = subscriptions.get(&connection).is_some_and(
                                        |subscription| {
                                            subscription.live.try_send(envelope.clone()).is_ok()
                                        },
                                    );
                                    if !sent {
                                        subscriptions.remove(&connection);
                                        control_outputs.remove(&connection);
                                    }
                                }
                            }
                            None
                        }
                        Ok(Err(error)) => {
                            poisoned_event_logs.insert(workspace.clone());
                            Some(AgentToCore::DurabilityError {
                                workspace,
                                operation: "event append".into(),
                                error: error.to_string(),
                            })
                        }
                        Err(error) => {
                            poisoned_event_logs.insert(workspace.clone());
                            Some(AgentToCore::DurabilityError {
                                workspace,
                                operation: "event writer".into(),
                                error: error.to_string(),
                            })
                        }
                    }
                }
            }
            CoreToAgent::EventRename { old, new } => {
                let poisoned = poisoned_event_logs.remove(&old);
                let old_name = old.clone();
                let new_name = new.clone();
                let result =
                    tokio::task::spawn_blocking(move || crate::eventlog::rename(&old, &new)).await;
                match result {
                    Ok(Ok(())) => {
                        if poisoned {
                            poisoned_event_logs.insert(new_name);
                        }
                        None
                    }
                    Ok(Err(error)) => {
                        poisoned_event_logs.insert(new_name);
                        if poisoned {
                            poisoned_event_logs.insert(old_name.clone());
                        }
                        Some(AgentToCore::DurabilityError {
                            workspace: old_name,
                            operation: "event-log rename".into(),
                            error: error.to_string(),
                        })
                    }
                    Err(error) => {
                        poisoned_event_logs.insert(new_name);
                        if poisoned {
                            poisoned_event_logs.insert(old_name.clone());
                        }
                        Some(AgentToCore::DurabilityError {
                            workspace: old_name,
                            operation: "event-log rename worker".into(),
                            error: error.to_string(),
                        })
                    }
                }
            }
            CoreToAgent::EventDelete { name } => {
                poisoned_event_logs.remove(&name);
                let _ = tokio::task::spawn_blocking(move || crate::eventlog::delete(&name)).await;
                None
            }
            CoreToAgent::WorkspaceCatalogAppend { name, line } => {
                let _ = tokio::task::spawn_blocking(move || {
                    crate::workspace_catalog::append_line(&name, &line)
                })
                .await;
                None
            }
            CoreToAgent::WorkspaceCatalogRename { old, new } => {
                let _ = tokio::task::spawn_blocking(move || {
                    crate::workspace_catalog::rename(&old, &new)
                })
                .await;
                None
            }
            CoreToAgent::WorkspaceCatalogQuery { client } => {
                let entries = tokio::task::spawn_blocking(workspace_catalog_snapshot)
                    .await
                    .unwrap_or_default();
                Some(AgentToCore::WorkspaceCatalogState { client, entries })
            }
            CoreToAgent::PaneEvidence {
                pane,
                foreground_pid,
                process_changed,
                tail,
                title,
                bound_agent,
            } => {
                let providers = providers.clone();
                let process = if bound_agent.is_some() {
                    None
                } else if let Some(pid) = foreground_pid {
                    let key = (pane, pid);
                    if process_changed || !process_cache.contains_key(&key) {
                        let providers = providers.clone();
                        let found = tokio::task::spawn_blocking(move || {
                            process_command(pid)
                                .and_then(|command| providers.process(&command))
                                .map(|found| found.with_invocation(Some(pid)))
                        })
                        .await
                        .expect("provider process evidence");
                        process_cache.retain(|(tracked, _), _| *tracked != pane);
                        process_cache.insert(key, found.clone());
                        found
                    } else {
                        process_cache.get(&key).cloned().flatten()
                    }
                } else {
                    None
                };
                tokio::task::spawn_blocking(move || {
                    let agent = process
                        .as_ref()
                        .and_then(|found| found.agent.clone())
                        .or(bound_agent);
                    let screen = agent
                        .as_deref()
                        .and_then(|agent| providers.screen(agent, &tail, &title))
                        .map(|found| found.with_invocation(foreground_pid));
                    let log = agent
                        .as_deref()
                        .and_then(|agent| providers.log(agent))
                        .map(|found| found.with_invocation(foreground_pid));
                    // A visible permission/question is safety-critical and may
                    // supersede a stale log. Otherwise native structured logs
                    // outrank grid heuristics, which outrank process identity.
                    let screen_needs_human = screen
                        .as_ref()
                        .and_then(|found| found.status)
                        .is_some_and(AgentStatus::needs_human);
                    match (process, log, screen, screen_needs_human) {
                        (_, _, Some(screen), true) => Some(AgentToCore::AgentDetected {
                            pane,
                            foreground_pid,
                            agent: screen.agent,
                            status: screen.status,
                            authority: uniterm_proto::DetectionAuthority::Grid,
                            evidence: screen.evidence,
                            provenance: screen.provenance,
                        }),
                        (_, Some(log), _, false) => Some(AgentToCore::AgentDetected {
                            pane,
                            foreground_pid,
                            agent: log.agent,
                            status: log.status,
                            authority: uniterm_proto::DetectionAuthority::Log,
                            evidence: log.evidence,
                            provenance: log.provenance,
                        }),
                        (_, None, Some(screen), false) => Some(AgentToCore::AgentDetected {
                            pane,
                            foreground_pid,
                            agent: screen.agent,
                            status: screen.status,
                            authority: uniterm_proto::DetectionAuthority::Grid,
                            evidence: screen.evidence,
                            provenance: screen.provenance,
                        }),
                        (Some(process), None, None, false) => Some(AgentToCore::AgentDetected {
                            pane,
                            foreground_pid,
                            agent: process.agent,
                            status: process.status,
                            authority: uniterm_proto::DetectionAuthority::Process,
                            evidence: process.evidence,
                            provenance: process.provenance,
                        }),
                        (None, None, None, false) => None,
                        (_, _, _, true) => unreachable!("human evidence includes a screen match"),
                    }
                })
                .await
                .expect("provider evidence")
            }
            CoreToAgent::DevServerEvidence { pane, tail } => {
                let matches =
                    tokio::task::spawn_blocking(move || crate::dev_server::detect_servers(&tail))
                        .await
                        .expect("development server detection");
                let mut servers = Vec::with_capacity(matches.len());
                for server in matches {
                    let key = (pane, server.port);
                    if dev_probe_ports.contains(&key) {
                        continue;
                    }
                    if crate::dev_server::probe_port(server.port).await
                        == crate::dev_server::PortProbe::Listening
                    {
                        servers.push(server);
                    }
                }
                for server in &servers {
                    let key = (pane, server.port);
                    dev_probe_ports.insert(key);
                    let replace = dev_probes.get(&key).is_none_or(|task| task.is_finished());
                    if dev_watch_active && replace {
                        if let Some(old) = dev_probes.remove(&key) {
                            old.abort();
                        }
                        dev_probes.insert(
                            key,
                            spawn_dev_server_probe(pane, server.port, tx.clone(), waker.clone()),
                        );
                    }
                }
                (!servers.is_empty()).then_some(AgentToCore::DevServersDetected { pane, servers })
            }
            CoreToAgent::DevServerWatchSet { active } => {
                dev_watch_active = active;
                if active {
                    for key in dev_probe_ports.iter().copied() {
                        let replace = dev_probes
                            .get(&key)
                            .is_none_or(tokio::task::JoinHandle::is_finished);
                        if replace {
                            if let Some(old) = dev_probes.remove(&key) {
                                old.abort();
                            }
                            dev_probes.insert(
                                key,
                                spawn_dev_server_probe(key.0, key.1, tx.clone(), waker.clone()),
                            );
                        }
                    }
                } else {
                    for (_, task) in dev_probes.drain() {
                        task.abort();
                    }
                }
                None
            }
            CoreToAgent::DevServerForget { pane, port } => {
                let key = (pane, port);
                dev_probe_ports.remove(&key);
                if let Some(task) = dev_probes.remove(&key) {
                    task.abort();
                }
                None
            }
            CoreToAgent::SystemNotification { title, body } => {
                let _ =
                    tokio::task::spawn_blocking(move || system_notification(&title, &body)).await;
                None
            }
            CoreToAgent::FileList {
                project,
                root,
                directory,
            } => Some(
                tokio::task::spawn_blocking(move || {
                    let result = list_project_directory(&root, &directory);
                    AgentToCore::FileListing {
                        project,
                        directory,
                        entries: result
                            .as_ref()
                            .map(|(entries, _)| entries.clone())
                            .unwrap_or_default(),
                        truncated: result.as_ref().is_ok_and(|(_, truncated)| *truncated),
                        error: result.err().map(|error| error.to_string()),
                    }
                })
                .await
                .expect("file directory list"),
            ),
            CoreToAgent::FileMutate {
                project,
                root,
                operation,
            } => Some(
                tokio::task::spawn_blocking(move || {
                    let directory = operation_parent(&operation);
                    AgentToCore::FileMutationDone {
                        project,
                        directory,
                        error: mutate_project_file(&root, operation)
                            .err()
                            .map(|error| error.to_string()),
                    }
                })
                .await
                .expect("file mutation"),
            ),
            CoreToAgent::ArtifactValidate {
                kind,
                task_id,
                token,
                project_root,
                expected,
                reported,
            } => {
                let worker = tokio::task::spawn_blocking(move || {
                    let result = validate_artifacts(&project_root, &expected, &reported);
                    (
                        result.as_ref().cloned().unwrap_or_default(),
                        result.err().map(|error| error.to_string()),
                    )
                });
                let (artifacts, error) =
                    match tokio::time::timeout(Duration::from_secs(25), worker).await {
                        Ok(Ok(result)) => result,
                        Ok(Err(error)) => (
                            Vec::new(),
                            Some(format!("artifact validator failed: {error}")),
                        ),
                        Err(_) => (Vec::new(), Some("artifact validation timed out".into())),
                    };
                Some(AgentToCore::ArtifactValidated {
                    kind,
                    task_id,
                    token,
                    artifacts,
                    error,
                })
            }
            CoreToAgent::ArtifactWatchSet { projects } => {
                set_artifact_watches(&mut artifact_watchers, projects, tx.clone(), waker.clone());
                None
            }
            CoreToAgent::ArtifactObserve {
                artifact,
                project_root,
                claim,
            } => Some(
                tokio::task::spawn_blocking(move || {
                    match observe_artifact(&project_root, &claim) {
                        Ok(Some(observation)) => AgentToCore::ArtifactObserved {
                            artifact,
                            observation: Some(observation),
                            missing: false,
                            error: None,
                        },
                        Ok(None) => AgentToCore::ArtifactObserved {
                            artifact,
                            observation: None,
                            missing: true,
                            error: None,
                        },
                        Err(error) => AgentToCore::ArtifactObserved {
                            artifact,
                            observation: None,
                            missing: false,
                            error: Some(error.to_string()),
                        },
                    }
                })
                .await
                .expect("artifact observation"),
            ),
            CoreToAgent::RelayCheckpointCreate {
                task_id,
                token,
                project_root,
            } => Some(
                tokio::task::spawn_blocking(move || {
                    let result = create_git_checkpoint(&project_root);
                    AgentToCore::RelayCheckpointCreated {
                        task_id,
                        token,
                        checkpoint: result.as_ref().ok().cloned(),
                        error: result.err().map(|error| error.to_string()),
                    }
                })
                .await
                .expect("relay checkpoint creation"),
            ),
            CoreToAgent::RelayCheckpointRollback {
                waiting_id,
                task_id,
                project_root,
                checkpoint,
            } => Some(
                tokio::task::spawn_blocking(move || {
                    let error = rollback_git_checkpoint(&project_root, &checkpoint)
                        .err()
                        .map(|error| error.to_string());
                    AgentToCore::RelayCheckpointRolledBack {
                        waiting_id,
                        task_id,
                        checkpoint,
                        error,
                    }
                })
                .await
                .expect("relay checkpoint rollback"),
            ),
            CoreToAgent::FileWatchSet {
                project,
                root,
                directories,
            } => {
                set_project_watches(
                    &mut file_watchers,
                    project,
                    &root,
                    &directories,
                    tx.clone(),
                    waker.clone(),
                );
                None
            }
            CoreToAgent::GitChangeWatchSet { project, root } => Some(
                git_watchers
                    .set(project, root, tx.clone(), waker.clone())
                    .await,
            ),
        };

        if let Some(reply) = reply {
            if tx.send(reply).is_ok() {
                // Wake the core loop so it drains us on its next tick.
                let _ = waker.wake();
            }
        }
    }
}

fn spawn_dev_server_probe(
    pane: PaneId,
    port: u16,
    tx: Sender<AgentToCore>,
    waker: Arc<Waker>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut liveness = crate::dev_server::LivenessState::new();
        loop {
            tokio::time::sleep(crate::dev_server::PROBE_INTERVAL).await;
            let probe = crate::dev_server::probe_port(port).await;
            if !liveness.observe(probe) {
                continue;
            }
            if tx.send(AgentToCore::DevServerDown { pane, port }).is_ok() {
                let _ = waker.wake();
            }
            break;
        }
    })
}

fn validate_artifacts(
    project_root: &str,
    expected: &[uniterm_proto::ArtifactClaim],
    reported: &[uniterm_proto::ArtifactClaim],
) -> std::io::Result<Vec<uniterm_proto::ArtifactObservation>> {
    let mut artifacts = Vec::new();
    for claim in expected.iter().chain(reported) {
        let Some(observation) = observe_artifact(project_root, claim)? else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("artifact must be a non-empty file: {}", claim.path),
            ));
        };
        if artifacts
            .iter()
            .any(|artifact: &uniterm_proto::ArtifactObservation| artifact.path == observation.path)
        {
            continue;
        }
        artifacts.push(observation);
    }
    Ok(artifacts)
}

fn observe_artifact(
    project_root: &str,
    claim: &uniterm_proto::ArtifactClaim,
) -> std::io::Result<Option<uniterm_proto::ArtifactObservation>> {
    use sha2::Digest as _;
    use std::io::Read as _;

    let root = std::fs::canonicalize(project_root)?;
    if !root.is_dir() || claim.path.is_empty() || claim.path.as_bytes().contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Project root and artifact path must be valid",
        ));
    }
    let path = Path::new(&claim.path);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let canonical = match std::fs::canonicalize(&candidate) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !canonical.starts_with(&root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("artifact escapes the Project root: {}", canonical.display()),
        ));
    }
    let mut file = std::fs::File::open(&canonical)?;
    let metadata = file.metadata()?;
    let current = std::fs::canonicalize(&canonical)?;
    let current_metadata = current.metadata()?;
    if !current.starts_with(&root)
        || metadata.dev() != current_metadata.dev()
        || metadata.ino() != current_metadata.ino()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "artifact changed identity while Project ownership was validated",
        ));
    }
    if !metadata.is_file() || metadata.len() == 0 {
        return Ok(None);
    }
    if metadata.len() > MAX_ARTIFACT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("artifact exceeds {MAX_ARTIFACT_BYTES} bytes"),
        ));
    }
    let relative = current.strip_prefix(&root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "artifact lost Project-relative ownership",
        )
    })?;
    let normalized = relative
        .to_str()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "artifact path is not valid UTF-8",
            )
        })?
        .replace(std::path::MAIN_SEPARATOR, "/");
    if normalized.is_empty()
        || normalized.len() > uniterm_core::ARTIFACT_PATH_MAX_BYTES
        || normalized.chars().any(char::is_control)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact path is not bounded safe UTF-8 display data",
        ));
    }
    let mut digest = sha2::Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        if size > MAX_ARTIFACT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("artifact exceeds {MAX_ARTIFACT_BYTES} bytes while reading"),
            ));
        }
        digest.update(&buffer[..read]);
    }
    if size == 0 {
        return Ok(None);
    }
    Ok(Some(uniterm_proto::ArtifactObservation {
        kind: claim.kind,
        path: normalized,
        digest: format!("{:x}", digest.finalize()),
        size,
    }))
}

fn create_git_checkpoint(project_root: &str) -> std::io::Result<String> {
    let root = std::fs::canonicalize(project_root)?;
    let top = git_output(&root, &["rev-parse", "--show-toplevel"])?;
    let repository = std::fs::canonicalize(top.trim())?;
    if !root.starts_with(&repository) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Project root is outside Git's authoritative repository root",
        ));
    }
    let created = git_output(&root, &["stash", "create", "uniterm relay checkpoint"])?;
    let checkpoint = if created.trim().is_empty() {
        git_output(&root, &["rev-parse", "HEAD"])?
    } else {
        created
    };
    let checkpoint = checkpoint.trim().to_string();
    if checkpoint.len() < 40 || !checkpoint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Git returned an invalid checkpoint object id",
        ));
    }
    git_status(
        &root,
        &["cat-file", "-e", &format!("{checkpoint}^{{commit}}")],
    )?;
    Ok(checkpoint)
}

fn rollback_git_checkpoint(project_root: &str, checkpoint: &str) -> std::io::Result<()> {
    let root = std::fs::canonicalize(project_root)?;
    if checkpoint.len() < 40 || !checkpoint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid checkpoint object id",
        ));
    }
    git_status(
        &root,
        &["cat-file", "-e", &format!("{checkpoint}^{{commit}}")],
    )?;
    // This explicit user action restores tracked files and the index for the
    // Project path without moving the current branch. Untracked files are
    // intentionally retained rather than deleted implicitly.
    git_status(&root, &["checkout", checkpoint, "--", "."])?;
    git_status(&root, &["diff", "--quiet", checkpoint, "--", "."])
}

fn git_output(root: &Path, arguments: &[&str]) -> std::io::Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_status(root: &Path, arguments: &[&str]) -> std::io::Result<()> {
    git_output(root, arguments).map(|_| ())
}

struct ProjectWatcher {
    watcher: notify::RecommendedWatcher,
    watched: HashSet<PathBuf>,
}

struct ArtifactProjectWatcher {
    _watcher: notify::RecommendedWatcher,
    root: PathBuf,
    artifacts: HashSet<uniterm_core::ArtifactId>,
}

fn set_artifact_watches(
    watchers: &mut HashMap<ProjectId, ArtifactProjectWatcher>,
    projects: Vec<uniterm_proto::ArtifactWatchProject>,
    tx: Sender<AgentToCore>,
    waker: Arc<Waker>,
) {
    let mut previous_watchers = std::mem::take(watchers);
    let mut next_watchers = HashMap::new();
    let mut reobserve = HashSet::new();
    for project in projects {
        let Ok(root) = std::fs::canonicalize(&project.root) else {
            continue;
        };
        let previous = previous_watchers.remove(&project.project);
        let mut exact: HashMap<PathBuf, uniterm_core::ArtifactId> = HashMap::new();
        let mut parents: HashMap<PathBuf, Vec<uniterm_core::ArtifactId>> = HashMap::new();
        for artifact in project
            .artifacts
            .into_iter()
            .take(uniterm_core::ARTIFACT_LEDGER_CAP)
        {
            let path = root.join(&artifact.path);
            if !path.starts_with(&root) {
                continue;
            }
            let Some(parent) = path.parent() else {
                continue;
            };
            let parent = parent.to_path_buf();
            exact.insert(path, artifact.artifact);
            parents.entry(parent).or_default().push(artifact.artifact);
        }
        if exact.is_empty() {
            continue;
        }
        let artifact_ids: HashSet<_> = exact.values().copied().collect();
        let event_exact = exact;
        let event_parents = parents.clone();
        let event_tx = tx.clone();
        let event_waker = waker.clone();
        let Ok(mut watcher) =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else {
                    return;
                };
                let mut changed = HashSet::new();
                for path in event.paths {
                    if let Some(artifact) = event_exact.get(&path) {
                        changed.insert(*artifact);
                    }
                    if let Some(artifacts) = event_parents.get(&path) {
                        changed.extend(artifacts.iter().copied());
                    }
                }
                if changed.is_empty() {
                    return;
                }
                let mut artifacts: Vec<_> = changed.into_iter().collect();
                artifacts.sort();
                if event_tx
                    .send(AgentToCore::ArtifactFilesChanged { artifacts })
                    .is_ok()
                {
                    let _ = event_waker.wake();
                }
            })
        else {
            if let Some(previous) = previous.filter(|previous| previous.root == root) {
                next_watchers.insert(project.project, previous);
            }
            continue;
        };
        let mut watched = 0usize;
        for parent in parents.keys() {
            if watcher
                .watch(parent, notify::RecursiveMode::NonRecursive)
                .is_ok()
            {
                watched += 1;
            }
        }
        if watched > 0 {
            match previous.as_ref() {
                Some(previous) if previous.root == root => {
                    reobserve.extend(artifact_ids.difference(&previous.artifacts).copied());
                }
                _ => reobserve.extend(artifact_ids.iter().copied()),
            }
            next_watchers.insert(
                project.project,
                ArtifactProjectWatcher {
                    _watcher: watcher,
                    root,
                    artifacts: artifact_ids,
                },
            );
        } else if let Some(previous) = previous.filter(|previous| previous.root == root) {
            next_watchers.insert(project.project, previous);
        }
    }
    *watchers = next_watchers;
    if !reobserve.is_empty() {
        let mut artifacts: Vec<_> = reobserve.into_iter().collect();
        artifacts.sort();
        if tx
            .send(AgentToCore::ArtifactFilesChanged { artifacts })
            .is_ok()
        {
            let _ = waker.wake();
        }
    }
}

fn set_project_watches(
    watchers: &mut HashMap<ProjectId, ProjectWatcher>,
    project: ProjectId,
    root: &str,
    directories: &[String],
    tx: Sender<AgentToCore>,
    waker: Arc<Waker>,
) {
    if directories.is_empty() {
        watchers.remove(&project);
        return;
    }
    let Ok(root) = std::fs::canonicalize(root) else {
        watchers.remove(&project);
        return;
    };
    let wanted: HashSet<PathBuf> = directories
        .iter()
        .take(MAX_PROJECT_WATCHES)
        .filter_map(|directory| safe_existing_directory(&root, directory).ok())
        .collect();
    if wanted.is_empty() {
        watchers.remove(&project);
        return;
    }
    if let std::collections::hash_map::Entry::Vacant(entry) = watchers.entry(project) {
        let event_root = root.clone();
        let Ok(watcher) =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else {
                    return;
                };
                let mut directories = HashSet::new();
                for path in event.paths {
                    let directory = if path.is_dir() {
                        path
                    } else {
                        path.parent().unwrap_or(&event_root).to_path_buf()
                    };
                    if directory.starts_with(&event_root) {
                        directories.insert(directory.to_string_lossy().into_owned());
                    }
                }
                for directory in directories {
                    if tx
                        .send(AgentToCore::FileChanged { project, directory })
                        .is_ok()
                    {
                        let _ = waker.wake();
                    }
                }
            })
        else {
            return;
        };
        entry.insert(ProjectWatcher {
            watcher,
            watched: HashSet::new(),
        });
    }
    let Some(state) = watchers.get_mut(&project) else {
        return;
    };
    let removed: Vec<PathBuf> = state.watched.difference(&wanted).cloned().collect();
    for directory in &removed {
        let _ = state.watcher.unwatch(directory);
    }
    let mut watched: HashSet<PathBuf> = state.watched.intersection(&wanted).cloned().collect();
    let added: Vec<PathBuf> = wanted.difference(&watched).cloned().collect();
    for directory in &added {
        if state
            .watcher
            .watch(directory, notify::RecursiveMode::NonRecursive)
            .is_ok()
        {
            watched.insert(directory.clone());
        }
    }
    state.watched = watched;
}

fn list_project_directory(root: &str, directory: &str) -> std::io::Result<(Vec<FileEntry>, bool)> {
    let root = std::fs::canonicalize(root)?;
    let directory = safe_existing_directory(&root, directory)?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in std::fs::read_dir(directory)? {
        if entries.len() == FILE_LISTING_LIMIT {
            truncated = true;
            break;
        }
        let item = item?;
        let path = item.path();
        let file_type = item.file_type()?;
        let metadata = item.metadata().ok();
        entries.push(FileEntry {
            name: item.file_name().to_string_lossy().into_owned(),
            path: path.to_string_lossy().into_owned(),
            is_dir: file_type.is_dir(),
            is_symlink: file_type.is_symlink(),
            size: metadata.map_or(0, |metadata| metadata.len()),
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok((entries, truncated))
}

fn safe_existing_directory(root: &Path, directory: &str) -> std::io::Result<PathBuf> {
    let requested = Path::new(directory);
    let requested = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let canonical = std::fs::canonicalize(requested)?;
    if canonical.starts_with(root) && canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "path is outside the Project root",
        ))
    }
}

fn safe_entry_path(root: &Path, value: &str) -> std::io::Result<PathBuf> {
    let requested = Path::new(value);
    let requested = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let parent = requested.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let parent = std::fs::canonicalize(parent)?;
    if parent.starts_with(root) {
        Ok(parent.join(requested.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
        })?))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "path is outside the Project root",
        ))
    }
}

fn validate_file_name(name: &str) -> std::io::Result<&str> {
    let name = name.trim();
    if name.is_empty()
        || matches!(name, "." | "..")
        || Path::new(name).components().count() != 1
        || name.contains('/')
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "enter one file or folder name",
        ));
    }
    Ok(name)
}

fn operation_parent(operation: &FileOperation) -> String {
    match operation {
        FileOperation::CreateFile { parent, .. }
        | FileOperation::CreateDirectory { parent, .. } => parent.clone(),
        FileOperation::Rename { path, .. } | FileOperation::Delete { path } => Path::new(path)
            .parent()
            .unwrap_or_else(|| Path::new(path))
            .to_string_lossy()
            .into_owned(),
    }
}

fn mutate_project_file(root: &str, operation: FileOperation) -> std::io::Result<()> {
    let root = std::fs::canonicalize(root)?;
    match operation {
        FileOperation::CreateFile { parent, name } => {
            let parent = safe_existing_directory(&root, &parent)?;
            let name = validate_file_name(&name)?;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(parent.join(name))?;
        }
        FileOperation::CreateDirectory { parent, name } => {
            let parent = safe_existing_directory(&root, &parent)?;
            std::fs::create_dir(parent.join(validate_file_name(&name)?))?;
        }
        FileOperation::Rename { path, name } => {
            let source = safe_entry_path(&root, &path)?;
            if source == root {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the Project root cannot be renamed",
                ));
            }
            let target = source
                .parent()
                .unwrap_or(&root)
                .join(validate_file_name(&name)?);
            if target.try_exists()? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "a file or folder with that name already exists",
                ));
            }
            std::fs::rename(source, target)?;
        }
        FileOperation::Delete { path } => {
            let target = safe_entry_path(&root, &path)?;
            if target == root {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the Project root cannot be deleted",
                ));
            }
            let metadata = std::fs::symlink_metadata(&target)?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                std::fs::remove_dir_all(target)?;
            } else {
                std::fs::remove_file(target)?;
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn system_notification(title: &str, body: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let status = Command::new("terminal-notifier")
        .args(["-title", title, "-message", body])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if status.is_ok_and(|status| status.success()) {
        return Ok(());
    }
    Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg("on run argv")
        .arg("-e")
        .arg("display notification (item 2 of argv) with title (item 1 of argv)")
        .arg("-e")
        .arg("end run")
        .arg(title)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
}

#[cfg(target_os = "linux")]
fn system_notification(title: &str, body: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Ok(());
    }
    Command::new("notify-send")
        .arg("--")
        .arg(title)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn system_notification(_title: &str, _body: &str) -> std::io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn process_command(pid: i32) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let text = String::from_utf8_lossy(&bytes).replace('\0', " ");
    (!text.trim().is_empty()).then(|| text.trim().to_string())
}

#[cfg(target_os = "macos")]
fn process_command(pid: i32) -> Option<String> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!command.is_empty()).then_some(command)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_command(_pid: i32) -> Option<String> {
    None
}

fn save_config_atomic(text: &str) -> std::io::Result<()> {
    let path = crate::server::config_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "HOME and XDG_CONFIG_HOME are unset",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let merged = merge_config_text(&existing, text);
    let temporary = path.with_extension("conf.tmp");
    std::fs::write(&temporary, merged)?;
    std::fs::rename(temporary, path)
}

fn validate_editor_command(command: &str) -> Result<(), String> {
    if command.len() > 512 {
        return Err("editor commands are limited to 512 bytes".into());
    }
    let words = shell_words::split(command).map_err(|error| format!("invalid command: {error}"))?;
    let program = words
        .first()
        .filter(|program| !program.is_empty())
        .ok_or_else(|| "editor command is empty".to_string())?;
    if crate::workflow::on_path(program) {
        Ok(())
    } else {
        Err(format!(
            "editor executable '{program}' was not found on PATH"
        ))
    }
}

fn validate_editor_commands(
    editor: &str,
    rules: &[uniterm_core::EditorRule],
) -> Result<(), String> {
    validate_editor_command(editor).map_err(|error| format!("Default editor: {error}"))?;
    for rule in rules {
        validate_editor_command(&rule.command)
            .map_err(|error| format!("Editor for .{}: {error}", rule.extension))?;
    }
    Ok(())
}

/// Replace Settings-owned keys while retaining comments and advanced keys a
/// user maintains by hand. This keeps the graphical surface and the config
/// file as two views of one schema without treating the modal as the owner of
/// the entire file.
fn merge_config_text(existing: &str, canonical: &str) -> String {
    let settings: Vec<(&str, &str)> = canonical.lines().filter_map(config_assignment).collect();
    let mut seen = std::collections::HashSet::new();
    let mut output = String::new();

    for line in existing.lines() {
        let Some((key, _)) = config_assignment(line) else {
            output.push_str(line);
            output.push('\n');
            continue;
        };
        if repeated_settings_key(key) {
            continue;
        }
        let Some((_, value)) = settings.iter().find(|(setting, _)| *setting == key) else {
            if key.starts_with("editor.") {
                continue;
            }
            output.push_str(line);
            output.push('\n');
            continue;
        };
        let comment = line
            .split_once('=')
            .and_then(|(_, rhs)| inline_comment(rhs))
            .unwrap_or("");
        output.push_str(key);
        output.push_str(" = ");
        output.push_str(value);
        output.push_str(comment);
        output.push('\n');
        seen.insert(key);
    }

    if !output.is_empty() && !output.ends_with("\n\n") {
        output.push('\n');
    }
    for (key, value) in settings {
        if repeated_settings_key(key) || seen.insert(key) {
            output.push_str(key);
            output.push_str(" = ");
            output.push_str(value);
            output.push('\n');
        }
    }
    output
}

fn repeated_settings_key(key: &str) -> bool {
    key == "guardrail-allowed-project"
}

fn config_assignment(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once('=')?;
    Some((key.trim(), value.trim()))
}

fn inline_comment(value: &str) -> Option<&str> {
    let bytes = value.as_bytes();
    let index = bytes
        .windows(2)
        .position(|pair| pair[0].is_ascii_whitespace() && pair[1] == b'#')?;
    Some(&value[index..])
}

/// Every registry provider's on-disk facts, in registry order. Runs inside
/// `spawn_blocking` - both probes hit the filesystem.
fn providers_disk_state(search_path: &[String]) -> Vec<uniterm_proto::ProviderDiskState> {
    uniterm_core::agent::PROVIDERS
        .iter()
        .map(|p| uniterm_proto::ProviderDiskState {
            id: p.id.to_string(),
            installed: crate::workflow::executable_on_search_path(p.command, search_path).is_some(),
            connector: crate::connectors::status(p.id),
        })
        .collect()
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn settings_merge_preserves_comments_and_unknown_keys() {
        let existing =
            "# personal notes\ntheme = nord # readable\nfont-family = Iosevka\nsidebar = false\neditor.md = glow\neditor.rs = vim\n";
        let canonical = "# generated\ntheme = dracula\nsidebar = true\nconfirm-close = false\neditor = nvim\neditor.md = glow --style dark\n";
        let merged = merge_config_text(existing, canonical);
        assert!(merged.contains("# personal notes"));
        assert!(merged.contains("theme = dracula # readable"));
        assert!(merged.contains("font-family = Iosevka"));
        assert!(merged.contains("sidebar = true"));
        assert!(merged.contains("confirm-close = false"));
        assert!(merged.contains("editor = nvim"));
        assert!(merged.contains("editor.md = glow --style dark"));
        assert!(!merged.contains("editor.rs"));
        assert!(!merged.contains("# generated"));
    }

    #[test]
    fn settings_merge_preserves_or_clears_every_allowed_project_selector() {
        let existing = "guardrail-allowed-project = old-a\n\
guardrail-allowed-project = old-b\n\
theme = nord\n";
        let canonical = "theme = dracula\n\
guardrail-allowed-project = api\n\
guardrail-allowed-project = /work/web\n";
        let merged = merge_config_text(existing, canonical);
        assert!(!merged.contains("old-a"));
        assert!(!merged.contains("old-b"));
        assert_eq!(merged.matches("guardrail-allowed-project =").count(), 2);
        assert!(merged.contains("guardrail-allowed-project = api"));
        assert!(merged.contains("guardrail-allowed-project = /work/web"));

        let cleared = merge_config_text(&merged, "theme = dracula\n");
        assert!(!cleared.contains("guardrail-allowed-project"));
    }

    #[test]
    fn editor_validation_parses_arguments_and_rejects_missing_programs() {
        assert!(validate_editor_command("sh -c 'printf ok'").is_ok());
        assert!(validate_editor_command("definitely-not-a-uniterm-editor").is_err());
        assert!(validate_editor_command("'unterminated").is_err());
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;

    #[test]
    fn file_operations_stay_inside_the_project_root() {
        let root =
            std::env::temp_dir().join(format!("uniterm-file-manager-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root_text = root.to_string_lossy().into_owned();

        mutate_project_file(
            &root_text,
            FileOperation::CreateDirectory {
                parent: root_text.clone(),
                name: "src".into(),
            },
        )
        .unwrap();
        mutate_project_file(
            &root_text,
            FileOperation::CreateFile {
                parent: root.join("src").to_string_lossy().into_owned(),
                name: "main.rs".into(),
            },
        )
        .unwrap();
        let (entries, truncated) = list_project_directory(&root_text, &root_text).unwrap();
        assert!(!truncated);
        assert_eq!(entries[0].name, "src");
        assert!(entries[0].is_dir);

        let outside = root.with_extension("outside");
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        let error = mutate_project_file(
            &root_text,
            FileOperation::CreateFile {
                parent: outside.to_string_lossy().into_owned(),
                name: "escape".into(),
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }
}

/// The core loop's event surface. In Phase 0 it owns just the waker; Phase 1
/// registers PTY masters and the client socket alongside it.
pub struct CoreLoop {
    poll: Poll,
    events: Events,
    agent: AgentRuntime,
}

impl CoreLoop {
    pub fn new() -> std::io::Result<Self> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKER_TOKEN)?);
        let agent = spawn_agent_runtime(waker);
        Ok(CoreLoop {
            poll,
            events: Events::with_capacity(128),
            agent,
        })
    }

    /// Hand a message to the agent runtime.
    pub fn send_to_agent(&self, msg: CoreToAgent) {
        self.agent.send(msg);
    }

    /// Block until an event arrives (or `timeout` elapses), then return any
    /// messages the agent runtime produced.
    ///
    /// Passing `timeout == None` means "sleep until woken" - when there is no
    /// armed render tick and no pending work, the core loop consumes no CPU at
    /// all. That is the zero-idle-wakeup property in `docs/03-system-architecture.md`.
    pub fn tick(&mut self, timeout: Option<Duration>) -> std::io::Result<Vec<AgentToCore>> {
        self.poll.poll(&mut self.events, timeout)?;
        let mut out = Vec::new();
        for ev in self.events.iter() {
            if ev.token() == WAKER_TOKEN {
                out.extend(self.agent.drain());
            }
            // Phase 1: other tokens => PTY reads and client socket I/O.
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use uniterm_proto::PaneId;

    fn wait_for_control_request(
        poll: &mut Poll,
        events: &mut Events,
        runtime: &AgentRuntime,
        id: u64,
        timeout: Duration,
    ) -> Option<u64> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() || poll.poll(events, Some(remaining)).is_err() {
                return None;
            }
            if let Some(connection) =
                runtime
                    .drain()
                    .into_iter()
                    .find_map(|message| match message {
                        AgentToCore::ControlRequest {
                            connection,
                            request,
                        } if request.id == id => Some(connection),
                        _ => None,
                    })
            {
                return Some(connection);
            }
        }
    }

    #[test]
    fn large_subscription_catch_up_does_not_stall_control_dispatch() {
        const EVENTS: u64 = 100_000;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = format!("subscription-responsive-{}-{nonce}", std::process::id());
        let mut bytes = Vec::with_capacity(EVENTS as usize * 150);
        for sequence in 1..=EVENTS {
            let envelope = crate::eventlog::EventEnvelope {
                version: crate::eventlog::EVENT_VERSION,
                sequence,
                timestamp_ms: sequence,
                workspace: workspace.clone(),
                event: crate::eventlog::LogEvent::TaskCreated {
                    id: sequence,
                    title: "responsive catch-up".into(),
                    status: uniterm_core::TaskStatus::Todo,
                },
            };
            serde_json::to_writer(&mut bytes, &envelope).unwrap();
            bytes.push(b'\n');
        }
        let log_path = crate::persist::state_dir().join(format!("{workspace}.log"));
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        crate::persist::open_private_append(&log_path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();

        let root = std::env::temp_dir().join(format!("uniterm-catch-up-{nonce}"));
        std::fs::create_dir_all(&root).unwrap();
        let control_path = root.join(format!("{workspace}.control.sock"));
        let mut poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), WAKER_TOKEN).unwrap());
        let runtime = spawn_agent_runtime_with_control(waker, control_path.clone()).unwrap();
        let mut events = Events::with_capacity(8);

        let mut subscriber = std::os::unix::net::UnixStream::connect(&control_path).unwrap();
        serde_json::to_writer(
            &mut subscriber,
            &uniterm_proto::ControlRequest {
                version: uniterm_proto::CONTROL_API_VERSION,
                id: 1,
                workspace: workspace.clone(),
                command: ControlCommand::Subscribe {
                    after_sequence: EVENTS - 10,
                },
            },
        )
        .unwrap();
        subscriber.write_all(b"\n").unwrap();
        let subscriber_connection =
            wait_for_control_request(&mut poll, &mut events, &runtime, 1, Duration::from_secs(2))
                .expect("subscription request");
        runtime.send(CoreToAgent::ControlResponse {
            connection: subscriber_connection,
            response: uniterm_proto::ControlResponse::ok(
                1,
                ControlResult::Subscribed {
                    subscription: subscriber_connection,
                    current_sequence: EVENTS,
                },
            ),
        });

        let mut second = std::os::unix::net::UnixStream::connect(&control_path).unwrap();
        serde_json::to_writer(
            &mut second,
            &uniterm_proto::ControlRequest {
                version: uniterm_proto::CONTROL_API_VERSION,
                id: 2,
                workspace: workspace.clone(),
                command: ControlCommand::Capabilities,
            },
        )
        .unwrap();
        second.write_all(b"\n").unwrap();
        assert!(
            wait_for_control_request(
                &mut poll,
                &mut events,
                &runtime,
                2,
                Duration::from_millis(300),
            )
            .is_some(),
            "large catch-up blocked the control dispatcher"
        );

        drop(runtime);
        crate::eventlog::delete(&workspace).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn subscription_catch_up_precedes_queued_live_events_exactly_once() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = format!("subscription-order-{}-{nonce}", std::process::id());
        let mut log = crate::eventlog::EventLog::open(&workspace);
        let mut live = None;
        for sequence in 1..=4 {
            let (name, line) = log
                .record(crate::eventlog::LogEvent::TaskCreated {
                    id: sequence,
                    title: format!("task {sequence}"),
                    status: uniterm_core::TaskStatus::Todo,
                })
                .unwrap();
            crate::eventlog::append_line(&name, &line).unwrap();
            if sequence == 4 {
                live = Some(serde_json::from_str::<crate::eventlog::EventEnvelope>(&line).unwrap());
            }
        }

        let (output, received) = bounded(16);
        let (live_tx, live_rx) = tokio::sync::mpsc::channel(4);
        let (ended, _end_rx) = bounded(1);
        live_tx.try_send(live.unwrap()).unwrap();
        drop(live_tx);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(stream_subscription(SubscriptionStart {
            workspace: workspace.clone(),
            subscription: 7,
            after: 1,
            through: 3,
            output,
            live: live_rx,
            generation: 1,
            token: 1,
            ended,
        }));

        let sequences: Vec<u64> = received
            .try_iter()
            .map(|line| serde_json::from_slice::<ControlFrame>(&line).unwrap())
            .filter_map(|frame| match frame {
                ControlFrame::Event(event) => Some(event.sequence),
                _ => None,
            })
            .collect();
        assert_eq!(sequences, [2, 3, 4]);
        crate::eventlog::delete(&workspace).unwrap();
    }

    #[test]
    fn failed_subscription_catch_up_returns_a_structured_stream_error() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = format!("subscription-error-{}-{nonce}", std::process::id());
        let (output, received) = bounded(4);
        let (live_tx, live_rx) = tokio::sync::mpsc::channel(1);
        let (ended, end_rx) = bounded(1);
        drop(live_tx);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(stream_subscription(SubscriptionStart {
            workspace,
            subscription: 9,
            after: 0,
            through: 1,
            output,
            live: live_rx,
            generation: 1,
            token: 2,
            ended,
        }));
        let frame = received
            .try_recv()
            .map(|line| serde_json::from_slice::<ControlFrame>(&line).unwrap())
            .unwrap();
        assert!(matches!(
            frame,
            ControlFrame::StreamError(ControlStreamError {
                subscription: 9,
                ref code,
                ..
            }) if code == "event_stream_unavailable"
        ));
        assert!(matches!(
            end_rx.try_recv().unwrap(),
            SubscriptionEnd {
                token: 2,
                action: SubscriptionEndAction::KeepConnection,
                ..
            }
        ));
    }

    #[test]
    fn idle_control_listener_does_not_wake_the_core() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "uniterm-control-idle-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("control directory");
        let path = root.join("idle.control.sock");

        let mut poll = Poll::new().expect("poll");
        let waker = Arc::new(Waker::new(poll.registry(), WAKER_TOKEN).expect("waker"));
        let runtime =
            spawn_agent_runtime_with_control(waker, path.clone()).expect("control runtime");
        assert!(path.exists(), "control socket was not bound");

        let mut events = Events::with_capacity(4);
        poll.poll(&mut events, Some(Duration::from_millis(100)))
            .expect("idle poll");
        assert!(events.is_empty(), "idle control API woke the core loop");

        drop(runtime);
        assert!(!path.exists(), "control socket survived runtime shutdown");
        std::fs::remove_dir_all(root).expect("remove control directory");
    }

    #[test]
    fn boundary_round_trips_through_both_runtimes() {
        // Send an event from the (sync) core side; the (tokio) agent side
        // handles it and wakes us; we drain the reply. This exercises the whole
        // mio<->tokio seam.
        let mut core = CoreLoop::new().expect("core loop");
        core.send_to_agent(CoreToAgent::OscAgentEvent {
            pane: PaneId(7),
            payload: r#"{"event":"prompt_submit"}"#.to_string(),
        });

        // Wait (with a generous timeout so the test can't hang) for the waker.
        let replies = core.tick(Some(Duration::from_secs(5))).expect("tick");

        assert_eq!(replies.len(), 1);
        match &replies[0] {
            AgentToCore::SetAgentStatus { pane, status } => {
                assert_eq!(*pane, PaneId(7));
                assert_eq!(*status, AgentStatus::Working);
            }
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    #[test]
    fn exit_event_maps_to_exited_status() {
        let mut core = CoreLoop::new().expect("core loop");
        core.send_to_agent(CoreToAgent::PtyExited { pane: PaneId(1) });
        let replies = core.tick(Some(Duration::from_secs(5))).expect("tick");
        assert_eq!(replies.len(), 1);
        assert!(matches!(
            replies[0],
            AgentToCore::SetAgentStatus {
                status: AgentStatus::Exited,
                ..
            }
        ));
    }

    #[test]
    fn editor_requests_validate_on_the_runtime_side() {
        let mut core = CoreLoop::new().expect("core loop");
        core.send_to_agent(CoreToAgent::EditorSettingsValidate {
            client: 42,
            editor: "sh".into(),
            editor_rules: vec![uniterm_core::EditorRule {
                extension: "md".into(),
                command: "definitely-not-a-uniterm-editor".into(),
            }],
        });
        let replies = core.tick(Some(Duration::from_secs(5))).expect("tick");
        assert!(matches!(
            replies.as_slice(),
            [AgentToCore::EditorSettingsValidated {
                client: 42,
                error: Some(error),
                ..
            }] if error.contains("Editor for .md")
        ));

        core.send_to_agent(CoreToAgent::EditorOpen {
            project: uniterm_core::ProjectId(3),
            path: "/tmp/readme.md".into(),
            command: "sh -c 'printf ok'".into(),
        });
        let replies = core.tick(Some(Duration::from_secs(5))).expect("tick");
        assert!(matches!(
            replies.as_slice(),
            [AgentToCore::EditorResolved {
                project: uniterm_core::ProjectId(3),
                path,
                error: None,
                ..
            }] if path == "/tmp/readme.md"
        ));
    }

    #[test]
    fn artifact_gate_requires_nonempty_files_inside_the_project() {
        let root = std::env::temp_dir().join(format!(
            "uniterm-artifact-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("plan.md"), b"ready").unwrap();
        std::fs::write(root.join("empty.md"), b"").unwrap();

        let claim = |path: &str| uniterm_proto::ArtifactClaim {
            kind: uniterm_proto::ArtifactKind::Plan,
            path: path.into(),
        };
        let valid = validate_artifacts(&root.to_string_lossy(), &[claim("plan.md")], &[]).unwrap();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].path, "plan.md");
        assert_eq!(valid[0].kind, uniterm_proto::ArtifactKind::Plan);
        assert_eq!(valid[0].size, 5);
        assert_eq!(valid[0].digest.len(), 64);
        assert!(validate_artifacts(&root.to_string_lossy(), &[claim("empty.md")], &[],).is_err());
        assert!(validate_artifacts(&root.to_string_lossy(), &[], &[claim("../outside")],).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn artifact_watch_reobserves_once_then_stays_idle_without_a_timer() {
        let root = std::env::temp_dir().join(format!(
            "uniterm-artifact-watch-idle-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("report.md"), b"ready\n").unwrap();

        let mut core = CoreLoop::new().expect("core loop");
        core.send_to_agent(CoreToAgent::ArtifactWatchSet {
            projects: vec![uniterm_proto::ArtifactWatchProject {
                project: ProjectId(7),
                root: root.to_string_lossy().into_owned(),
                artifacts: vec![uniterm_proto::ArtifactWatchEntry {
                    artifact: uniterm_core::ArtifactId(9),
                    path: "report.md".into(),
                }],
            }],
        });
        let initial = core
            .tick(Some(Duration::from_secs(5)))
            .expect("initial observation");
        assert!(matches!(
            initial.as_slice(),
            [AgentToCore::ArtifactFilesChanged { artifacts }]
                if artifacts == &[uniterm_core::ArtifactId(9)]
        ));

        let idle = core
            .tick(Some(Duration::from_millis(150)))
            .expect("idle artifact watch");
        assert!(idle.is_empty(), "idle artifact watch woke the core");

        drop(core);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relay_checkpoint_is_cached_only_after_git_can_restore_it() {
        let root = std::env::temp_dir().join(format!(
            "uniterm-checkpoint-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        if git_status(&root, &["init"]).is_err() {
            let _ = std::fs::remove_dir_all(root);
            return;
        }
        git_status(&root, &["config", "user.email", "uniterm@example.invalid"]).unwrap();
        git_status(&root, &["config", "user.name", "Uniterm Test"]).unwrap();
        std::fs::write(root.join("tracked.txt"), b"base\n").unwrap();
        git_status(&root, &["add", "tracked.txt"]).unwrap();
        git_status(&root, &["commit", "-m", "base"]).unwrap();
        std::fs::write(root.join("tracked.txt"), b"checkpoint\n").unwrap();

        let checkpoint = create_git_checkpoint(&root.to_string_lossy()).unwrap();
        std::fs::write(root.join("tracked.txt"), b"bad turn\n").unwrap();
        rollback_git_checkpoint(&root.to_string_lossy(), &checkpoint).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "checkpoint\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relay_checkpoint_failure_returns_no_false_reference() {
        let root = std::env::temp_dir().join(format!(
            "uniterm-checkpoint-failure-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        assert!(create_git_checkpoint(&root.to_string_lossy()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_event_append_freezes_followups_and_snapshots() {
        let mut core = CoreLoop::new().expect("core loop");
        let name = format!("{}/workspace", "x".repeat(300));
        core.send_to_agent(CoreToAgent::EventAppend {
            name: name.clone(),
            line: "first\n".into(),
        });
        let failed = core
            .tick(Some(Duration::from_secs(5)))
            .expect("failed append");
        assert!(matches!(
            failed.as_slice(),
            [AgentToCore::DurabilityError { operation, .. }] if operation == "event append"
        ));

        core.send_to_agent(CoreToAgent::EventAppend {
            name: name.clone(),
            line: "later\n".into(),
        });
        let skipped = core
            .tick(Some(Duration::from_secs(5)))
            .expect("skipped append");
        assert!(matches!(
            skipped.as_slice(),
            [AgentToCore::DurabilityError { operation, .. }]
                if operation == "event append skipped after prior failure"
        ));

        core.send_to_agent(CoreToAgent::SnapshotSave {
            name,
            bytes: vec![1, 2, 3],
        });
        let skipped = core
            .tick(Some(Duration::from_secs(5)))
            .expect("skipped snapshot");
        assert!(matches!(
            skipped.as_slice(),
            [AgentToCore::DurabilityError { operation, .. }]
                if operation == "snapshot skipped after event-log failure"
        ));
    }

    #[test]
    fn web_server_probe_is_armed_by_evidence_and_disarms_when_down() {
        let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
            return;
        };
        let port = listener.local_addr().expect("listener address").port();
        let mut core = CoreLoop::new().expect("core loop");
        core.send_to_agent(CoreToAgent::DevServerWatchSet { active: true });
        core.send_to_agent(CoreToAgent::DevServerEvidence {
            pane: PaneId(9),
            tail: format!("Server listening on http://localhost:{port}"),
        });
        let detected = core.tick(Some(Duration::from_secs(2))).expect("detection");
        assert!(matches!(
            detected.as_slice(),
            [AgentToCore::DevServersDetected { pane: PaneId(9), servers }]
                if servers.len() == 1 && servers[0].port == port
        ));
        drop(listener);
        let down = core.tick(Some(Duration::from_secs(2))).expect("down probe");
        assert!(matches!(
            down.as_slice(),
            [AgentToCore::DevServerDown { pane: PaneId(9), port: down_port }]
                if *down_port == port
        ));
    }

    #[test]
    fn hidden_web_server_surface_does_not_run_liveness_ticks() {
        let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
            return;
        };
        let port = listener.local_addr().expect("listener address").port();
        let mut core = CoreLoop::new().expect("core loop");
        core.send_to_agent(CoreToAgent::DevServerEvidence {
            pane: PaneId(10),
            tail: format!("Server listening on http://localhost:{port}"),
        });
        let detected = core.tick(Some(Duration::from_secs(2))).expect("detection");
        assert!(matches!(
            detected.as_slice(),
            [AgentToCore::DevServersDetected { .. }]
        ));
        drop(listener);

        let hidden = core
            .tick(Some(Duration::from_millis(200)))
            .expect("hidden wait");
        assert!(hidden.is_empty(), "hidden surface performed liveness work");

        core.send_to_agent(CoreToAgent::DevServerWatchSet { active: true });
        let down = core
            .tick(Some(Duration::from_secs(2)))
            .expect("visible probe");
        assert!(matches!(
            down.as_slice(),
            [AgentToCore::DevServerDown { pane: PaneId(10), port: down_port }]
                if *down_port == port
        ));
    }
}
