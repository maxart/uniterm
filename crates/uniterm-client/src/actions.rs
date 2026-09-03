//! Surface actions shared by the key, mouse, and menu paths.
//!
//! Every modal reports what the user asked for as a small action enum; this
//! module is the single place that turns one of those into state changes plus
//! outbound frames. Clicks, key bindings, and menu entries therefore run the
//! same semantic command path and cannot drift apart.

use uniterm_core::menu::MenuAction;
use uniterm_proto::{encode_frame, ClientMessage, Command, SplitAxis};

use crate::about::{self, AboutAction, AboutView};
use crate::agents::{AgentsAction, AgentsView};
use crate::observatory::ObservatoryAction;
use crate::overlay::Overlay;
use crate::projects::{NewProjectAction, NewProjectView, ProjectAction, ProjectsView};
use crate::settings::{SettingsAction, SettingsView};
use crate::task::{LineInput, TaskInput, TaskSubmit};
use crate::taskview::{TaskAction, TaskView};
use crate::tty::write_stdout;
use crate::View;

/// Apply a task-manager action to the modal state + outbound queue - one
/// definition shared by the key and mouse paths, so they cannot drift.
pub(crate) fn apply_task_action(
    action: TaskAction,
    taskman: &mut Option<TaskView>,
    server_out: &mut Vec<u8>,
    cols: u16,
    rows: u16,
) {
    match action {
        TaskAction::None => {}
        TaskAction::Redraw => {
            if let Some(tv) = taskman {
                write_stdout(&tv.render(cols, rows));
            }
        }
        TaskAction::Close => {
            *taskman = None;
            server_out.extend(encode_frame(&ClientMessage::Refresh));
        }
        TaskAction::SetStatus(id, status) => {
            server_out.extend(encode_frame(&ClientMessage::TaskSetStatus { id, status }));
        }
        TaskAction::Retitle(id, title) => {
            server_out.extend(encode_frame(&ClientMessage::TaskRetitle { id, title }));
        }
        TaskAction::Delete(id) => {
            server_out.extend(encode_frame(&ClientMessage::TaskDelete { id }));
        }
    }
}

/// Apply a Manage Agents action to the modal state + outbound queue - one
/// definition shared by the key and mouse paths, so they cannot drift.
pub(crate) fn apply_agents_action(
    action: AgentsAction,
    agentman: &mut Option<AgentsView>,
    server_out: &mut Vec<u8>,
    cols: u16,
    rows: u16,
) {
    match action {
        AgentsAction::None => {}
        AgentsAction::Redraw => {
            if let Some(av) = agentman {
                write_stdout(&av.render(cols, rows));
            }
        }
        AgentsAction::Close => {
            *agentman = None;
            server_out.extend(encode_frame(&ClientMessage::Refresh));
        }
        AgentsAction::ToggleConnector(agent) => {
            server_out.extend(encode_frame(&ClientMessage::ConnectorToggle { agent }));
        }
        AgentsAction::Launch(agent, target) => {
            server_out.extend(encode_frame(&ClientMessage::AgentLaunch { agent, target }));
            // Close so the user lands on the agent.
            *agentman = None;
            server_out.extend(encode_frame(&ClientMessage::Refresh));
        }
        AgentsAction::StopAll => {
            // The Agents view already ran its two-step confirm.
            server_out.extend(encode_frame(&ClientMessage::AgentsStopAll {
                scope: uniterm_proto::StopScope::Workspace,
                confirmed: true,
            }));
        }
    }
}

pub(crate) fn apply_about_action(
    action: AboutAction,
    about: &mut Option<AboutView>,
    server_out: &mut Vec<u8>,
) {
    match action {
        AboutAction::None => {}
        AboutAction::Close => {
            *about = None;
            server_out.extend(encode_frame(&ClientMessage::Refresh));
        }
        AboutAction::OpenDocs => {
            if open_desktop_url(about::DOCS_URL).is_err() {
                write_stdout(b"\x07");
            }
        }
    }
}

pub(crate) fn apply_settings_action(
    action: SettingsAction,
    settings: &mut Option<SettingsView>,
    server_out: &mut Vec<u8>,
    cols: u16,
    rows: u16,
) -> bool {
    match action {
        SettingsAction::None => {}
        SettingsAction::Redraw => {
            if let Some(view) = settings {
                write_stdout(&view.render(cols, rows));
            }
        }
        SettingsAction::Close => {
            *settings = None;
            server_out.extend(encode_frame(&ClientMessage::Refresh));
        }
        SettingsAction::Apply(patch) => {
            server_out.extend(encode_frame(&ClientMessage::SettingsApply(patch)));
            if let Some(view) = settings {
                write_stdout(&view.render(cols, rows));
            }
        }
        SettingsAction::MigrateDesktop => {
            *settings = None;
            return true;
        }
    }
    false
}

pub(crate) fn apply_project_action(
    action: ProjectAction,
    projects: &mut Option<ProjectsView>,
    new_project: &mut Option<NewProjectView>,
    rename: &mut Option<(LineInput, RenameTarget)>,
    server_out: &mut Vec<u8>,
    size: (u16, u16),
    remote: bool,
) {
    let (cols, rows) = size;
    match action {
        ProjectAction::None => {}
        ProjectAction::Redraw => {
            if let Some(view) = projects {
                write_stdout(&view.overlay().render(cols, rows));
            }
        }
        ProjectAction::Close => {
            *projects = None;
            server_out.extend(encode_frame(&ClientMessage::Refresh));
        }
        ProjectAction::Switch(project) => {
            server_out.extend(encode_frame(&ClientMessage::ProjectSwitch { project }));
            *projects = None;
            server_out.extend(encode_frame(&ClientMessage::Refresh));
        }
        ProjectAction::Create => {
            *projects = None;
            let view = if remote {
                NewProjectView::for_remote()
            } else {
                NewProjectView::new()
            };
            write_stdout(&view.overlay().render(cols, rows));
            *new_project = Some(view);
        }
        ProjectAction::Rename(project, name) => {
            *projects = None;
            *rename = Some((
                LineInput::with_text("Rename Project", name),
                RenameTarget::Project(project),
            ));
            if let Some((input, _)) = rename {
                write_stdout(&input.overlay().render(cols, rows));
            }
        }
        ProjectAction::Move(project, direction) => {
            server_out.extend(encode_frame(&ClientMessage::ProjectMove {
                project,
                direction,
            }));
            if let Some(view) = projects {
                write_stdout(&view.overlay().render(cols, rows));
            }
        }
        ProjectAction::Remove(project) => {
            // The Projects view already ran its two-step confirm.
            server_out.extend(encode_frame(&ClientMessage::ProjectRemove {
                project,
                confirmed: true,
            }));
        }
    }
}

pub(crate) fn apply_new_project_action(
    action: NewProjectAction,
    new_project: &mut Option<NewProjectView>,
    projects_pending: &mut bool,
    project_create_pending: &mut bool,
    server_out: &mut Vec<u8>,
    cols: u16,
    rows: u16,
) {
    match action {
        NewProjectAction::None => {}
        NewProjectAction::Redraw => {
            if let Some(view) = new_project {
                write_stdout(&view.overlay().render(cols, rows));
            }
        }
        NewProjectAction::Close => {
            *new_project = None;
            *project_create_pending = false;
            server_out.extend(encode_frame(&ClientMessage::Refresh));
        }
        NewProjectAction::Submit { name, root } => {
            server_out.extend(encode_frame(&ClientMessage::ProjectCreate { name, root }));
            // Keep the modal until the host confirms that its PTY opened at
            // the requested path. A remote path must never fail silently.
            *project_create_pending = true;
            *projects_pending = true;
        }
    }
}

pub(crate) fn apply_observatory_action(
    action: ObservatoryAction,
    view: &mut Option<View>,
    server_out: &mut Vec<u8>,
    cols: u16,
    rows: u16,
) {
    match action {
        ObservatoryAction::None => {}
        ObservatoryAction::Redraw => {
            if let Some(View::Observatory(observatory)) = view {
                write_stdout(&observatory.render(cols, rows));
            }
        }
        ObservatoryAction::Close => {
            *view = None;
            server_out.extend(encode_frame(&ClientMessage::Refresh));
        }
        ObservatoryAction::Refresh => {
            server_out.extend(encode_frame(&ClientMessage::Observatory));
        }
        ObservatoryAction::Jump(pane) => {
            server_out.extend(encode_frame(&ClientMessage::AgentFocus { pane }));
            *view = None;
            server_out.extend(encode_frame(&ClientMessage::Refresh));
        }
        ObservatoryAction::Stop(pane) => {
            server_out.extend(encode_frame(&ClientMessage::AgentStop { pane }));
            server_out.extend(encode_frame(&ClientMessage::Observatory));
        }
        ObservatoryAction::Waiting { id, action, text } => {
            server_out.extend(encode_frame(&ClientMessage::WaitingAct {
                id,
                action,
                text,
            }));
        }
        ObservatoryAction::OpenUrl(url) => {
            if open_desktop_url(&url).is_err() {
                write_stdout(b"\x07");
            }
        }
    }
}

pub(crate) fn open_desktop_url(url: &str) -> std::io::Result<()> {
    if !is_safe_http_url(url) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "only HTTP(S) links can be opened",
        ));
    }

    #[cfg(target_os = "macos")]
    let child = std::process::Command::new("open").arg(url).spawn()?;

    #[cfg(target_os = "windows")]
    let child = std::process::Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn()?;

    #[cfg(all(unix, not(target_os = "macos")))]
    let child = {
        let mut last = None;
        let mut started = None;
        for (program, prefix) in [("xdg-open", None), ("gio", Some("open")), ("wslview", None)] {
            let mut command = std::process::Command::new(program);
            if let Some(prefix) = prefix {
                command.arg(prefix);
            }
            match command.arg(url).spawn() {
                Ok(child) => {
                    started = Some(child);
                    break;
                }
                Err(error) => last = Some(error),
            }
        }
        started.ok_or_else(|| {
            last.unwrap_or_else(|| std::io::Error::other("no desktop URL opener is available"))
        })?
    };

    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

pub(crate) fn is_safe_http_url(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
        && url
            .split_once("//")
            .is_some_and(|(_, rest)| !rest.is_empty())
}

/// Turn a submitted New Task line into a server message, or `None` if there is
/// nothing to launch (empty, or a client-only `/save`).
pub(crate) fn submit_task(ti: &TaskInput) -> Option<ClientMessage> {
    let parsed = ti.parse();
    let agent = parsed.agent;
    let role_providers = parsed.role_providers;
    let task = |prompt: String, relay: bool, workflow: Option<String>, project: Option<String>| {
        Some(ClientMessage::NewTask {
            prompt,
            relay,
            agent,
            role_providers,
            workflow,
            project,
        })
    };
    match parsed.submit {
        TaskSubmit::Empty => None,
        TaskSubmit::Prompt(p) => task(p, false, None, None),
        TaskSubmit::Relay(spec) => task(spec, true, None, None),
        TaskSubmit::Workflow { name, prompt } => task(prompt, false, Some(name), None),
        TaskSubmit::Project { name, prompt } => task(prompt, false, None, Some(name)),
        // /save <title>: record a task without launching; empty just refreshes
        // (persistence is already automatic).
        TaskSubmit::Save(title) if !title.is_empty() => Some(ClientMessage::SaveTask { title }),
        TaskSubmit::Save(_) => Some(ClientMessage::Refresh),
    }
}

/// A client-side surface a menu action needs to open (beyond frames already
/// appended to the outbound buffer).
pub(crate) enum Surface {
    None,
    Task,
    Rename,
    RenameSession,
    RenameProject(uniterm_core::ProjectId),
    Sessions,
    Settings,
    About,
    Projects,
    NewProject,
    Confirm(ClientMessage),
    Detach,
}

pub(crate) fn close_confirmation(message: &ClientMessage) -> Overlay {
    let (target, consequence) = match message {
        ClientMessage::Command(Command::KillWindow) => (
            "Close this Tab and every Pane in it?",
            "This terminates its running process.",
        ),
        ClientMessage::ProjectRemove { .. } => (
            "Remove this Project from the Workspace?",
            "This closes every Tab and Pane it owns and stops their processes.",
        ),
        _ => (
            "Close the focused Pane?",
            "This terminates its running process.",
        ),
    };
    Overlay::with_footer(
        "Confirm close",
        vec![target.into(), consequence.into()],
        &[("Y / Enter", "close"), ("any other key", "cancel")],
    )
}

/// Which thing an open rename input renames on Enter.
pub(crate) enum RenameTarget {
    Window,
    Session,
    Project(uniterm_core::ProjectId),
}

/// Append the frames a menu action implies; returns which client-side surface
/// (if any) the caller must open.
pub(crate) fn run_menu_action(
    a: MenuAction,
    project: Option<uniterm_core::ProjectId>,
    confirm_close: bool,
    confirm_tab_close: bool,
    out: &mut Vec<u8>,
    tasks_pending: &mut bool,
    agents_pending: &mut bool,
) -> Surface {
    let mut cmd = |c: Command| out.extend(encode_frame(&ClientMessage::Command(c)));
    match a {
        MenuAction::SplitRight => cmd(Command::Split(SplitAxis::LeftRight)),
        MenuAction::SplitDown => cmd(Command::Split(SplitAxis::TopBottom)),
        MenuAction::Zoom => cmd(Command::ZoomToggle),
        MenuAction::ZoomOut => cmd(Command::Overview),
        MenuAction::CopyMode => cmd(Command::CopyMode),
        MenuAction::ClosePane if confirm_close => {
            return Surface::Confirm(ClientMessage::Command(Command::KillPane))
        }
        MenuAction::ClosePane => cmd(Command::KillPane),
        MenuAction::NewTab => cmd(Command::NewWindow),
        MenuAction::SwitchProject => {
            if let Some(project) = project {
                out.extend(encode_frame(&ClientMessage::ProjectSwitch { project }));
            }
        }
        MenuAction::NewProjectTab => {
            if let Some(project) = project {
                out.extend(encode_frame(&ClientMessage::ProjectSwitch { project }));
                out.extend(encode_frame(&ClientMessage::Command(Command::NewWindow)));
            }
        }
        MenuAction::RenameProject => {
            if let Some(project) = project {
                return Surface::RenameProject(project);
            }
        }
        MenuAction::MoveProjectUp | MenuAction::MoveProjectDown => {
            if let Some(project) = project {
                let direction = if a == MenuAction::MoveProjectUp {
                    uniterm_proto::ProjectMoveDirection::Up
                } else {
                    uniterm_proto::ProjectMoveDirection::Down
                };
                out.extend(encode_frame(&ClientMessage::ProjectMove {
                    project,
                    direction,
                }));
            }
        }
        MenuAction::CloseProject => {
            if let Some(project) = project {
                return Surface::Confirm(ClientMessage::ProjectRemove {
                    project,
                    confirmed: true,
                });
            }
        }
        MenuAction::CloseTab if confirm_tab_close => {
            return Surface::Confirm(ClientMessage::Command(Command::KillWindow))
        }
        MenuAction::CloseTab => cmd(Command::KillWindow),
        MenuAction::NextTab => cmd(Command::NextWindow),
        MenuAction::PrevTab => cmd(Command::PrevWindow),
        MenuAction::Observatory => {
            out.extend(encode_frame(&ClientMessage::Command(Command::Observatory)))
        }
        MenuAction::Tasks => {
            *tasks_pending = true;
            out.extend(encode_frame(&ClientMessage::Tasks));
        }
        MenuAction::ManageAgents => {
            *agents_pending = true;
            out.extend(encode_frame(&ClientMessage::Agents));
        }
        // The menu item is the consent: terminating the session is what it
        // says on the label.
        MenuAction::KillSession => out.extend(encode_frame(&ClientMessage::KillServer)),
        MenuAction::RenameTab => return Surface::Rename,
        MenuAction::RenameSession => return Surface::RenameSession,
        MenuAction::NewTask => return Surface::Task,
        MenuAction::Sessions => return Surface::Sessions,
        MenuAction::Settings => {
            out.extend(encode_frame(&ClientMessage::Settings));
            return Surface::Settings;
        }
        MenuAction::About => return Surface::About,
        MenuAction::Projects => {
            out.extend(encode_frame(&ClientMessage::WorkspaceState));
            return Surface::Projects;
        }
        MenuAction::NewProject => return Surface::NewProject,
        MenuAction::Sidebar => cmd(Command::SidebarToggle),
        MenuAction::Detach => return Surface::Detach,
    }
    Surface::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniterm_proto::FrameDecoder;

    fn decode_inputs(bytes: &[u8]) -> Vec<ClientMessage> {
        let mut d = FrameDecoder::new();
        d.push(bytes);
        let mut out = Vec::new();
        while let Ok(Some(m)) = d.decode::<ClientMessage>() {
            out.push(m);
        }
        out
    }

    #[test]
    fn new_task_preserves_explicit_role_provider_selections() {
        let mut input = TaskInput::new();
        for character in "/workflow pair @claude @verifier=codex ship it".chars() {
            input.insert(character);
        }
        assert!(matches!(
            submit_task(&input),
            Some(ClientMessage::NewTask {
                prompt,
                agent: Some(provider),
                role_providers,
                workflow: Some(template),
                ..
            }) if prompt == "ship it"
                && provider == "claude"
                && template == "pair"
                && role_providers == [uniterm_proto::RoleProviderSelection {
                    role: "verifier".into(),
                    provider: "codex".into(),
                }]
        ));
    }

    #[test]
    fn tab_menu_close_uses_the_tab_confirmation_setting() {
        let mut out = Vec::new();
        let mut tasks_pending = false;
        let mut agents_pending = false;
        assert!(matches!(
            run_menu_action(
                MenuAction::CloseTab,
                None,
                true,
                true,
                &mut out,
                &mut tasks_pending,
                &mut agents_pending,
            ),
            Surface::Confirm(ClientMessage::Command(Command::KillWindow))
        ));
        assert!(out.is_empty());

        assert!(matches!(
            run_menu_action(
                MenuAction::CloseTab,
                None,
                true,
                false,
                &mut out,
                &mut tasks_pending,
                &mut agents_pending,
            ),
            Surface::None
        ));
        assert!(matches!(
            decode_inputs(&out).as_slice(),
            [ClientMessage::Command(Command::KillWindow)]
        ));
    }

    #[test]
    fn workspace_sidebar_menu_action_sends_toggle_command() {
        let mut out = Vec::new();
        let mut tasks_pending = false;
        let mut agents_pending = false;
        assert!(matches!(
            run_menu_action(
                MenuAction::Sidebar,
                None,
                false,
                false,
                &mut out,
                &mut tasks_pending,
                &mut agents_pending,
            ),
            Surface::None
        ));
        assert!(matches!(
            decode_inputs(&out).as_slice(),
            [ClientMessage::Command(Command::SidebarToggle)]
        ));
    }

    #[test]
    fn workspace_observatory_menu_action_sends_toggle_command() {
        let mut out = Vec::new();
        let mut tasks_pending = false;
        let mut agents_pending = false;
        assert!(matches!(
            run_menu_action(
                MenuAction::Observatory,
                None,
                false,
                false,
                &mut out,
                &mut tasks_pending,
                &mut agents_pending,
            ),
            Surface::None
        ));
        assert!(matches!(
            decode_inputs(&out).as_slice(),
            [ClientMessage::Command(Command::Observatory)]
        ));
    }

    #[test]
    fn workspace_settings_menu_action_requests_settings() {
        let mut out = Vec::new();
        let mut tasks_pending = false;
        let mut agents_pending = false;
        assert!(matches!(
            run_menu_action(
                MenuAction::Settings,
                None,
                false,
                false,
                &mut out,
                &mut tasks_pending,
                &mut agents_pending,
            ),
            Surface::Settings
        ));
        assert!(matches!(
            decode_inputs(&out).as_slice(),
            [ClientMessage::Settings]
        ));
    }

    #[test]
    fn workspace_about_menu_action_opens_client_surface_without_server_work() {
        let mut out = Vec::new();
        let mut tasks_pending = false;
        let mut agents_pending = false;
        assert!(matches!(
            run_menu_action(
                MenuAction::About,
                None,
                false,
                false,
                &mut out,
                &mut tasks_pending,
                &mut agents_pending,
            ),
            Surface::About
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn project_context_actions_keep_the_resolved_project_target() {
        let mut out = Vec::new();
        let mut tasks_pending = false;
        let mut agents_pending = false;
        assert!(matches!(
            run_menu_action(
                MenuAction::MoveProjectDown,
                Some(uniterm_core::ProjectId(7)),
                false,
                false,
                &mut out,
                &mut tasks_pending,
                &mut agents_pending,
            ),
            Surface::None
        ));
        assert!(matches!(
            decode_inputs(&out).as_slice(),
            [ClientMessage::ProjectMove {
                project: uniterm_core::ProjectId(7),
                direction: uniterm_proto::ProjectMoveDirection::Down,
            }]
        ));

        assert!(matches!(
            run_menu_action(
                MenuAction::RenameProject,
                Some(uniterm_core::ProjectId(7)),
                false,
                false,
                &mut Vec::new(),
                &mut tasks_pending,
                &mut agents_pending,
            ),
            Surface::RenameProject(uniterm_core::ProjectId(7))
        ));
    }

    #[test]
    fn new_project_submit_waits_for_the_authoritative_workspace_reply() {
        let mut view = Some(NewProjectView::new());
        let mut projects_pending = false;
        let mut create_pending = false;
        let mut out = Vec::new();
        apply_new_project_action(
            NewProjectAction::Submit {
                name: "Added".into(),
                root: "/tmp".into(),
            },
            &mut view,
            &mut projects_pending,
            &mut create_pending,
            &mut out,
            80,
            24,
        );
        assert!(view.is_some());
        assert!(projects_pending);
        assert!(create_pending);
        assert!(matches!(
            decode_inputs(&out).as_slice(),
            [ClientMessage::ProjectCreate { name, root }]
                if name == "Added" && root == "/tmp"
        ));
    }

    #[test]
    fn desktop_url_handoff_accepts_only_single_http_urls() {
        assert!(is_safe_http_url("http://localhost:5173/path"));
        assert!(is_safe_http_url("https://example.com"));
        assert!(!is_safe_http_url("file:///tmp/private"));
        assert!(!is_safe_http_url("https://example.com\n--flag"));
        assert!(!is_safe_http_url("javascript:alert(1)"));
    }
}
