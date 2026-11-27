//! Freeze the stable Workspace model independently from descriptors and grids.
//! Unsupported in-flight operations stay with the serving engine.

use super::super::*;
use serde::{Deserialize, Serialize};
use std::io;

fn busy(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, reason)
}

// Borrow during capture, own after decode. Maintaining one field list keeps
// both directions of the private schema in step without copying a task history
// merely to feed the encoder.
macro_rules! workspace_fields {
    ($($(#[$attr:meta])* $name:ident : $ty:ty),+ $(,)?) => {
        #[derive(Deserialize)]
        struct Fields { $($(#[$attr])* $name: $ty),+ }
        #[derive(Serialize)]
        struct Refs<'a> { $($(#[$attr])* $name: &'a $ty),+ }
        impl Fields {
            fn capture(server: &Server) -> io::Result<Vec<u8>> {
                serde_json::to_vec(&Refs { $($name: &server.$name),+ }).map_err(io::Error::other)
            }
            fn install(self, server: &mut Server) { $(server.$name = self.$name;)+ }
        }
    };
}

workspace_fields! {
    program: String,
    agent_search_path: Vec<String>,
    hostname: String,
    sidebar_agent_scope: SidebarScope,
    sidebar_server_scope: SidebarScope,
    observatory_tab: ObservatoryTab,
    observatory_scroll: [usize; 4],
    last_broadcast_title: Option<String>,
    last_cursor: Option<(u16, u16)>,
    last_cursor_visible: Option<bool>,
    last_catalog_line: Option<String>,
    workspace_catalog_enabled: bool,
    next_native_resume: u64,
    next_worktree_request: u64,
    next_launch_request: u64,
    orchestration_token_state: u64,
    next_dev_server_sequence: u64,
    process_names: HashMap<PaneId, (i32, String)>,
    tasks: uniterm_core::TaskList,
    task_owners: HashMap<PaneId, u64>,
    task_evidence: HashMap<PaneId, observed_tasks::Evidence>,
    tasks_dirty: bool,
    waiting: uniterm_core::WaitingQueue,
    auto_approval: uniterm_core::approval::AutoApproval,
    pending_approval: HashMap<PaneId, waiting::PendingApproval>,
    delegated: HashMap<PaneId, delegate::DelegationState>,
    delegation_opt_out: HashMap<PaneId, u64>,
    instructions: uniterm_core::InstructionQueue,
    run_graph: uniterm_core::RunGraph,
    run_graph_sequence: u64,
    artifact_sequence: u64,
    files: FileManager,
    omarchy: theme_follow::State,
    overwatch_focused: bool,
    overwatch_ui_dirty: bool,
    overwatch_visibility_dirty: bool,
    overwatch: uniterm_core::overwatch::State,
    overwatch_generation: u64,
    overwatch_model_catalog: Option<uniterm_core::agent_models::ModelCatalog>,
    overwatch_model_context: Option<(String, Vec<String>)>,
    overwatch_restore_failed: bool,
    overview: Option<usize>,
    today_generation: u64,
    timeline_invocations: HashMap<PaneId, u64>,
    restored_invocations: HashMap<PaneId, u64>,
    pending_pane_renders: Vec<PaneId>,
    #[serde(with = "monotonic::option")]
    snapshot_deadline: Option<std::time::Instant>,
    notification: Option<AgentToast>,
    pending_notifications: HashMap<PaneId, PendingAgentNotification>,
}

/// State shared by all Panes and clients, including live-only consent. This
/// checkpoint travels only on the verified replacement's private channel.
#[derive(Serialize, Deserialize)]
pub struct WorkspaceModel {
    fields: Vec<u8>,
    artifacts: Vec<u8>,
    config: String,
    effective_theme: uniterm_core::Theme,
}

impl WorkspaceModel {
    /// Capture a quiescent model. A rejected operation remains entirely under
    /// the incumbent engine's ownership and can finish normally.
    pub fn capture(server: &Server) -> io::Result<Self> {
        use bincode::Options;
        if !server.running || server.durability_error.is_some() || !server.event_writes_enabled {
            return Err(busy("Workspace has no healthy live event writer"));
        }
        if server.preparing_workspace_rename.is_some()
            || !server.pending_workspace_renames.is_empty()
            || server.pending_catalog_line.is_some()
        {
            return Err(busy(
                "Workspace identity or catalog update is still pending",
            ));
        }
        if !server.retiring_ptys.is_empty()
            || !server.recovery_panes.is_empty()
            || !server.native_resumes.is_empty()
            || !server.native_restore_requests.is_empty()
            || !server.native_history_controls.is_empty()
            || server.panes.values().any(|p| p.recovery.is_some())
        {
            return Err(busy("a Pane is closing or recovering"));
        }
        if !server.pending_launches.is_empty()
            || server.launch_resolution.is_some()
            || !server.pending_worktrees.is_empty()
            || !server.pending_project_directories.is_empty()
        {
            return Err(busy(
                "a launch, Project directory change or worktree request is pending",
            ));
        }
        if !server.workflows.is_empty()
            || !server.relays.is_empty()
            || !server.pending_orchestration_submissions.is_empty()
            || !server.pending_prompt_deliveries.is_empty()
            || !server.pending_relay_activations.is_empty()
        {
            return Err(busy("a workflow or relay still owns a live operation"));
        }
        if !server.dev_servers.is_empty()
            || !server.pending_artifact_observations.is_empty()
            || !server.dirty_artifact_observations.is_empty()
        {
            return Err(busy(
                "a development server or artifact observation is active",
            ));
        }
        if server.overwatch.enabled
            || server.overwatch.pane.is_some()
            || server.overwatch_due.is_some()
            || server.overwatch_launch_pending
            || !server.overwatch_model_requests.is_empty()
        {
            return Err(busy("Overwatch currently owns a live operation"));
        }
        if server.today.is_some()
            || server.context_menu.is_some()
            || server.mouse_sel.is_some()
            || server.divider_drag.is_some()
            || server.sidebar_drag.is_some()
            || server.scrollbar_drag.is_some()
            || server.tab_drag.is_some()
            || !server.sidebar_label_reveals.is_empty()
        {
            return Err(busy(
                "close the active Workspace view or finish the pointer gesture",
            ));
        }
        let config = server.config.to_text();
        let mut check = Config::parse(&config);
        check.theme = server.config.theme;
        if check != server.config {
            return Err(busy("active Workspace configuration cannot round-trip"));
        }
        Ok(Self {
            fields: Fields::capture(server)?,
            artifacts: bincode::DefaultOptions::new()
                .with_limit(super::MAX_CHECKPOINT as u64)
                .serialize(&server.artifacts)
                .map_err(io::Error::other)?,
            config,
            effective_theme: server.config.theme,
        })
    }

    /// Install the private model before the candidate admits any traffic.
    /// Read-only caches are built again on demand.
    pub fn install(self, server: &mut Server) -> io::Result<()> {
        use bincode::Options;
        if !server.agents.awaiting_handoff() {
            return Err(busy("Workspace model import needs a prepared engine"));
        }
        let fields: Fields = serde_json::from_slice(&self.fields)?;
        let artifacts = bincode::DefaultOptions::new()
            .with_limit(self.artifacts.len() as u64)
            .deserialize(&self.artifacts)
            .map_err(io::Error::other)?;
        let mut config = Config::parse(&self.config);
        config.theme = self.effective_theme;
        fields.install(server);
        server.artifacts = artifacts;
        server.config = config;
        Ok(())
    }
}
