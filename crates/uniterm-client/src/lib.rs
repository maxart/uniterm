//! `uniterm-client` - the thin attach client.
//!
//! It connects to the server's Unix socket, puts the real terminal into raw
//! mode, forwards keystrokes as input, and paints the render-op frames the
//! server sends. State lives in the server; this process is disposable, which
//! is the whole point of the client-server split (`docs/03`/`docs/04`).
//!
//! Persistent Pane and Observatory content is painted from the server's
//! damage-tracked render ops, never redrawn client-side. `ratatui` is reserved
//! for low-frequency dialogs and management surfaces in this crate.

use std::io::Read;
use std::os::unix::io::RawFd;
use std::path::Path;

use mio::net::UnixStream;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use uniterm_proto::{
    encode_frame, ClientMessage, Command, FrameDecoder, PaneAttachRole, ServerMessage,
};

mod about;
mod actions;
pub mod agents;
mod chime;
mod input;
pub mod menu;
pub mod observatory;
pub mod overlay;
pub mod projects;
mod requests;
mod resize;
pub mod sessions;
pub mod settings;
pub mod task;
pub mod taskview;
pub mod text_input;
mod tty;
use about::AboutView;
use actions::{
    apply_about_action, apply_agents_action, apply_new_project_action, apply_observatory_action,
    apply_project_action, apply_settings_action, apply_task_action, close_confirmation,
    open_desktop_url, run_menu_action, submit_task, RenameTarget, Surface,
};
use agents::{AgentsAction, AgentsView};
use input::{
    handle_menu_keys, process_input_with_bindings, scan_stdin_chunk, strip_focus_events, Action,
    MenuKeys, PrefixState, DETACH_KEY,
};
use menu::MenuState;
use observatory::{ObservatoryAction, ObservatoryView};
use overlay::Overlay;
use projects::{NewProjectView, ProjectsView};
pub use requests::{
    agent_explain, agent_launch, agent_wait, artifact_list, control, control_request,
    hierarchy_focus, import_workspace, instruction_add, instruction_cancel, instruction_list,
    instruction_replace, instruction_send_now, kill_server, orchestration_submit, pane_focus,
    pane_list, pane_read, pane_send, pane_wait_output, query_info, run_fork, run_list, tab_move,
    waiting_act, waiting_list, workflow_submit, workspace_request, worktree_request,
    AgentWaitResult, PaneWaitResult,
};
use sessions::{SessionAction, SessionsState};
use settings::{SettingsAction, SettingsView};
use std::path::PathBuf;
use task::{LineInput, TaskInput};
use taskview::{TaskAction, TaskView};
use text_input::{decode_key, LineKey};
use tty::{
    drain_server_messages, flush, install_winch, load_client_config, sanitize_terminal_title,
    service_server_output, tty_size, write_stdout, write_sync_frame, TtyGuard,
};
use uniterm_proto::{ChromeMenu, MouseKind};

/// Why [`attach_once`] returned: the client is done, or the user picked
/// another session to attach to (the outer loop reconnects).
enum Outcome {
    Exit,
    Switch(PathBuf),
    ReviveWorkspace(String),
    RemoteWorkspace(String),
    MigrateDesktop,
}

/// Why the attach UI returned to the CLI front door.
pub enum AttachOutcome {
    Exit,
    /// A stopped Workspace was selected. The CLI starts its server after raw
    /// terminal mode is restored, then attaches normally.
    ReviveWorkspace(String),
    /// A remote Workspace was selected. The SSH front door replaces its
    /// bridge and attaches to this host-owned Workspace by name.
    RemoteWorkspace(String),
    /// Settings requested the interactive Uniterm Desktop importer. The CLI
    /// runs it after this function drops raw terminal mode.
    MigrateDesktop,
}

/// Initial surfaces requested by the CLI when entering the attach UI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttachOptions {
    /// Probe live Workspace sockets and open the switcher after attaching.
    pub open_workspaces: bool,
    /// The socket is a local SSH bridge, so sibling local sockets are not
    /// remote Workspaces. Host catalog requests and selected Workspace
    /// handoffs therefore travel through the server protocol by name.
    pub remote: bool,
}

/// A read-only overlay plus the structured rows behind it, so a mouse click can
/// be resolved back to an action (jump to an agent's window, etc.).
enum View {
    Observatory(ObservatoryView),
}

impl View {
    fn render(&self, cols: u16, rows: u16) -> Vec<u8> {
        match self {
            View::Observatory(observatory) => observatory.render(cols, rows),
        }
    }
}

const STDIN: Token = Token(0);
const SERVER: Token = Token(1);
const WINCH: Token = Token(2);

/// Placeholder retained for the Phase 0 smoke test.
pub fn describe() -> &'static str {
    "uniterm-client: attach client + Observatory"
}

/// Attach to the server at `sock_path`. Blocks until the user detaches or the
/// pane's process exits, then restores the terminal. Picking a session in the
/// switcher reconnects to that session's socket without leaving raw mode.
pub fn attach(sock_path: &Path) -> std::io::Result<AttachOutcome> {
    attach_with_options(sock_path, AttachOptions::default())
}

/// Attach with an optional initial Workspace switcher. Migration uses this to
/// make every newly imported Workspace visible without another detach cycle.
pub fn attach_with_options(
    sock_path: &Path,
    options: AttachOptions,
) -> std::io::Result<AttachOutcome> {
    let cfg = load_client_config();
    overlay::set_ui_theme(cfg.theme);
    // Raw mode; restored on drop.
    let _tty = TtyGuard::enable(libc::STDIN_FILENO, cfg.focus_follows_mouse)?;
    // Watch for terminal resizes (SIGWINCH) so the server relayouts and the
    // status line/chrome never end up drawn at a stale size. Installed once;
    // every connection registers the same pipe.
    let winch = install_winch()?;
    let mut current = sock_path.to_path_buf();
    let mut open_workspaces = options.open_workspaces;
    loop {
        match attach_once(&current, &cfg, winch.read, open_workspaces, options.remote)? {
            Outcome::Exit => return Ok(AttachOutcome::Exit),
            Outcome::Switch(next) => {
                current = next;
                open_workspaces = false;
            }
            Outcome::ReviveWorkspace(name) => return Ok(AttachOutcome::ReviveWorkspace(name)),
            Outcome::RemoteWorkspace(name) => return Ok(AttachOutcome::RemoteWorkspace(name)),
            Outcome::MigrateDesktop => return Ok(AttachOutcome::MigrateDesktop),
        }
    }
}

/// Attach the local terminal directly to one server-owned Pane.
///
/// The Pane keeps its existing PTY geometry. Resize signals request an
/// authoritative repaint but never resize the Pane, and observer input is
/// discarded locally as well as rejected by the server.
pub fn pane_attach(
    sock_path: &Path,
    pane: uniterm_core::PaneId,
    role: PaneAttachRole,
) -> std::io::Result<()> {
    let cfg = load_client_config();
    let _tty = TtyGuard::enable(libc::STDIN_FILENO, false)?;
    let winch = install_winch()?;
    pane_attach_once(sock_path, pane, role, cfg.prefix, winch.read)
}

fn pane_attach_once(
    sock_path: &Path,
    pane: uniterm_core::PaneId,
    role: PaneAttachRole,
    prefix: u8,
    winch_fd: RawFd,
) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(sock_path)?;
    let mut poll = Poll::new()?;
    poll.registry()
        .register(&mut stream, SERVER, Interest::READABLE)?;
    let stdin_fd = libc::STDIN_FILENO;
    poll.registry()
        .register(&mut SourceFd(&stdin_fd), STDIN, Interest::READABLE)?;
    poll.registry()
        .register(&mut SourceFd(&winch_fd), WINCH, Interest::READABLE)?;

    let mut server_out = encode_frame(&ClientMessage::PaneAttach { pane, role });
    let mut decoder = FrameDecoder::new();
    let mut events = Events::with_capacity(16);
    let mut server_write_interest = false;
    let mut prefix_pending = false;
    let mut current_role = role;
    let mut accepted = false;
    let mut rejected = None;
    let mut detaching = false;
    service_server_output(
        poll.registry(),
        &mut stream,
        &mut server_out,
        &mut server_write_interest,
    )?;

    let mut resizes = resize::ResizeCoalescer::default();
    'attached: loop {
        let timeout = resizes.timeout(std::time::Instant::now());
        match poll.poll(&mut events, timeout) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        for event in events.iter() {
            match event.token() {
                STDIN if event.is_readable() && accepted && !detaching => {
                    // Keystrokes must not overtake a held resize.
                    if let Some((cols, rows)) = resizes.take_pending() {
                        server_out.extend(encode_frame(&ClientMessage::Resize { cols, rows }));
                    }
                    let mut buf = [0u8; 4096];
                    loop {
                        // SAFETY: stdin is registered and `buf` is live.
                        let read = unsafe {
                            libc::read(stdin_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                        };
                        if read == 0 {
                            server_out.extend(encode_frame(&ClientMessage::Detach));
                            detaching = true;
                            break;
                        }
                        if read < 0 {
                            let error = std::io::Error::last_os_error();
                            if error.kind() == std::io::ErrorKind::Interrupted {
                                continue;
                            }
                            break;
                        }
                        let mut input = Vec::with_capacity(read as usize);
                        for byte in &buf[..read as usize] {
                            if prefix_pending {
                                prefix_pending = false;
                                if *byte == DETACH_KEY {
                                    server_out.extend(encode_frame(&ClientMessage::Detach));
                                    detaching = true;
                                    input.clear();
                                    break;
                                }
                                input.push(prefix);
                                if *byte != prefix {
                                    input.push(*byte);
                                }
                            } else if *byte == prefix {
                                prefix_pending = true;
                            } else {
                                input.push(*byte);
                            }
                        }
                        if !input.is_empty() {
                            if current_role.can_control() {
                                server_out.extend(encode_frame(&ClientMessage::Input(input)));
                            } else {
                                write_stdout(b"\x07");
                            }
                        }
                        if detaching || (read as usize) < buf.len() {
                            break;
                        }
                    }
                    service_server_output(
                        poll.registry(),
                        &mut stream,
                        &mut server_out,
                        &mut server_write_interest,
                    )?;
                }
                WINCH if event.is_readable() => {
                    let mut buf = [0u8; 64];
                    // SAFETY: this is the owned non-blocking signal pipe.
                    while unsafe {
                        libc::read(winch_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                    } > 0
                    {}
                    // Hold the size until the storm settles; the flush below
                    // the event loop sends one Resize for the whole gesture.
                    resizes.note(tty_size(libc::STDOUT_FILENO), std::time::Instant::now());
                }
                SERVER => {
                    if event.is_readable() {
                        let mut buf = [0u8; 16 * 1024];
                        let mut messages = Vec::new();
                        let mut disconnect = None;
                        loop {
                            match stream.read(&mut buf) {
                                Ok(0) => {
                                    disconnect = Some(server_disconnect_error(
                                        "direct Pane connection",
                                        None,
                                    ));
                                    break;
                                }
                                Ok(read) => {
                                    decoder.push(&buf[..read]);
                                    drain_server_messages(&mut decoder, &mut messages).map_err(
                                        |error| {
                                            std::io::Error::new(
                                                std::io::ErrorKind::InvalidData,
                                                format!("invalid direct Pane frame: {error:?}"),
                                            )
                                        },
                                    )?;
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                    break
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                    continue
                                }
                                Err(error) => {
                                    disconnect = Some(server_disconnect_error(
                                        "direct Pane connection",
                                        Some(error),
                                    ));
                                    break;
                                }
                            }
                        }
                        let mut render = Vec::new();
                        for message in messages {
                            match message {
                                ServerMessage::RenderOps(ops) => render.extend_from_slice(&ops),
                                ServerMessage::PaneAttached {
                                    pane: attached,
                                    role,
                                    ..
                                } if attached == pane => {
                                    current_role = role;
                                    accepted = true;
                                }
                                ServerMessage::PaneAttachRejected {
                                    pane: rejected_pane,
                                    reason,
                                } if rejected_pane == pane => {
                                    rejected = Some(reason);
                                    break 'attached;
                                }
                                ServerMessage::PaneAttachRevoked { pane: revoked, .. }
                                    if revoked == pane =>
                                {
                                    current_role = PaneAttachRole::Observer;
                                    write_stdout(b"\x07");
                                }
                                ServerMessage::Bell => write_stdout(b"\x07"),
                                ServerMessage::Chime {
                                    kind,
                                    sound,
                                    file,
                                    pane_active,
                                } => {
                                    // A single-Pane attach cannot see terminal
                                    // focus; treat the terminal as focused.
                                    if chime::should_sound(kind, pane_active, true)
                                        && chime::play(kind, sound, &file) == chime::Playback::Bell
                                    {
                                        write_stdout(b"\x07");
                                    }
                                }
                                ServerMessage::Detached | ServerMessage::Exited => break 'attached,
                                ServerMessage::WindowTitle { title } => {
                                    let title = sanitize_terminal_title(&title);
                                    write_stdout(format!("\x1b]0;{title}\x07").as_bytes());
                                }
                                _ => {}
                            }
                        }
                        if !render.is_empty() {
                            write_sync_frame(&render);
                        }
                        if let Some(error) = disconnect {
                            return Err(error);
                        }
                    }
                    if event.is_writable() {
                        service_server_output(
                            poll.registry(),
                            &mut stream,
                            &mut server_out,
                            &mut server_write_interest,
                        )?;
                    }
                }
                _ => {}
            }
        }
        if let Some((cols, rows)) = resizes.take_due(std::time::Instant::now()) {
            server_out.extend(encode_frame(&ClientMessage::Resize { cols, rows }));
            service_server_output(
                poll.registry(),
                &mut stream,
                &mut server_out,
                &mut server_write_interest,
            )?;
        }
    }
    if let Some(reason) = rejected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            reason,
        ));
    }
    if !accepted {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "server closed before accepting the direct Pane attachment",
        ));
    }
    Ok(())
}

/// One connection to one server: the whole attach loop. Returns how it ended
/// (exit, or a switch to another session's socket).
fn attach_once(
    sock_path: &Path,
    cfg: &uniterm_core::Config,
    winch_fd: RawFd,
    open_workspaces: bool,
    remote: bool,
) -> std::io::Result<Outcome> {
    let prefix = cfg.prefix;
    // Where the status line sits, mirroring the server's chrome area. A
    // keyboard-opened command menu uses this to choose its vertical side.
    let mut status_top = cfg.status_position == uniterm_core::StatusPosition::Top;
    let mut confirm_close = cfg.confirm_close;
    let mut confirm_tab_close = cfg.confirm_tab_close;
    // Shadowed mutable: a session rename moves the socket, and the server
    // notifies us so switcher/current detection and the rename prefill follow.
    let mut sock_path = sock_path.to_path_buf();
    let mut stream = UnixStream::connect(&sock_path)?;
    let (mut cols, mut rows) = tty_size(libc::STDOUT_FILENO);
    let mut outcome = Outcome::Exit;

    let mut poll = Poll::new()?;
    poll.registry()
        .register(&mut stream, SERVER, Interest::READABLE)?;
    let stdin_fd = libc::STDIN_FILENO;
    poll.registry()
        .register(&mut SourceFd(&stdin_fd), STDIN, Interest::READABLE)?;
    poll.registry()
        .register(&mut SourceFd(&winch_fd), WINCH, Interest::READABLE)?;

    // Outbound bytes to the server, starting with the Attach handshake.
    let mut server_out = encode_frame(&ClientMessage::Attach {
        term: std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
        cols,
        rows,
    });
    let mut decoder = FrameDecoder::new();
    let mut prefix_state = PrefixState::default();
    // The New Task overlay input, drawn client-side on top of the pane frame.
    // `None` when closed.
    let mut task: Option<TaskInput> = None;
    // A read-only overlay (Observatory/Tasks) + its rows. Any key closes it; a
    // click on a row can act (jump to a window).
    let mut view: Option<View> = None;
    // The open menu-bar dropdown (MB3) and the Rename-tab input (MB5).
    let mut menu: Option<MenuState> = None;
    let mut rename: Option<(LineInput, RenameTarget)> = None;
    // Prefill for the session-rename input (edit, don't retype).
    let mut session_name: String = sock_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    // The session-switcher modal (S3).
    let mut sessions = open_workspaces.then(|| SessionsState::open(&sock_path));
    // The task-manager modal (AG7 v2).
    let mut taskman: Option<TaskView> = None;
    // The Manage Agents modal (AG8).
    let mut agentman: Option<AgentsView> = None;
    let mut settings: Option<SettingsView> = None;
    let mut about: Option<AboutView> = None;
    let mut projectman: Option<ProjectsView> = None;
    let mut new_project: Option<NewProjectView> = None;
    let mut close_confirm: Option<(Overlay, ClientMessage)> = None;
    // Whether the user asked for the task-manager / Manage Agents modal and
    // the server's snapshot has not arrived yet. Replies also follow every
    // mutation (the modal re-projects server truth), so without this flag a
    // reply landing after the user closed the modal would reopen it.
    let mut tasks_pending = false;
    let mut agents_pending = false;
    let mut settings_pending = false;
    let mut projects_pending = false;
    let mut sessions_pending = false;
    let mut project_create_pending = false;
    // Buffered partial mouse sequence across reads, a partial keyboard escape
    // sequence across reads (an arrow split over two reads must not be
    // misread as a lone Esc), and the last hover cell (to dedupe motion
    // events so we don't flood the server).
    let mut mouse_leftover: Vec<u8> = Vec::new();
    let mut key_pending: Vec<u8> = Vec::new();
    // Terminal focus as reported by the host (CSI I / CSI O). Assumed focused
    // until told otherwise; it only decides whether a completion chime for
    // the Pane already on screen is redundant.
    let mut terminal_focused = true;
    let mut last_hover: Option<(u16, u16)> = None;
    let mut last_drag: Option<(u16, u16)> = None;
    let mut events = Events::with_capacity(64);
    let mut server_write_interest = false;

    if sessions.is_some() {
        server_out.extend(encode_frame(&ClientMessage::OverlayVisible { on: true }));
    }
    service_server_output(
        poll.registry(),
        &mut stream,
        &mut server_out,
        &mut server_write_interest,
    )?;

    let mut overlay_visible = sessions.is_some();
    let mut resizes = resize::ResizeCoalescer::default();
    'outer: loop {
        // epoll_wait is never auto-restarted (SA_RESTART does not apply), so
        // the SIGWINCH that *feeds* the resize pipe also interrupts the poll -
        // treat EINTR as a wakeup, not an error, or every resize detaches.
        let now = std::time::Instant::now();
        let timeout = resize::min_timeout(
            about.as_ref().map(|view| view.poll_timeout(now)),
            resizes.timeout(now),
        );
        if let Err(e) = poll.poll(&mut events, timeout) {
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        for ev in events.iter() {
            match ev.token() {
                STDIN if ev.is_readable() => {
                    // Keystrokes must not overtake a held resize: a program
                    // that asks for its window size on the next key has to
                    // see the geometry the user already dragged to.
                    if let Some((cols, rows)) = resizes.take_pending() {
                        server_out.extend(encode_frame(&ClientMessage::Resize { cols, rows }));
                    }
                    let mut buf = [0u8; 4096];
                    loop {
                        // SAFETY: valid fd, live buffer.
                        let n = unsafe {
                            libc::read(stdin_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                        };
                        // When the fd runs dry while a partial keyboard escape
                        // is held back, flush it as-is on this same wakeup: a
                        // genuinely lone Esc still closes its modal promptly.
                        let flush_pending = n < 0
                            && !key_pending.is_empty()
                            && std::io::Error::last_os_error().kind()
                                == std::io::ErrorKind::WouldBlock;
                        if n > 0 || flush_pending {
                            let (mouse_events, mut passthrough) = if flush_pending {
                                (Vec::new(), std::mem::take(&mut key_pending))
                            } else {
                                // Pull mouse reports out of the stream first so
                                // they never reach the pane; feed the rest as
                                // keyboard, reassembling an escape sequence
                                // split across reads (a trailing partial is
                                // held for the next read of this drain).
                                scan_stdin_chunk(
                                    &buf[..n as usize],
                                    &mut mouse_leftover,
                                    &mut key_pending,
                                )
                            };
                            for (mx, my, kind) in mouse_events {
                                match kind {
                                    MouseKind::Hover => {
                                        // While a menu is open, hovering its
                                        // items moves the selection; otherwise
                                        // hover only moves pane focus, and only
                                        // when no overlay is capturing the mouse.
                                        if !cfg.focus_follows_mouse {
                                            continue;
                                        }
                                        if let Some(ms) = &mut menu {
                                            if let Some(i) = menu::item_at(
                                                ms, cols, rows, status_top, prefix, mx, my,
                                            ) {
                                                if ms.sel != i {
                                                    ms.sel = i;
                                                    write_stdout(&menu::render_menu(
                                                        ms, cols, rows, status_top, prefix,
                                                    ));
                                                }
                                            }
                                            continue;
                                        }
                                        if view.is_some()
                                            || task.is_some()
                                            || close_confirm.is_some()
                                            || rename.is_some()
                                            || sessions.is_some()
                                            || taskman.is_some()
                                            || agentman.is_some()
                                            || settings.is_some()
                                            || about.is_some()
                                            || new_project.is_some()
                                            || projectman.is_some()
                                            || last_hover == Some((mx, my))
                                        {
                                            continue;
                                        }
                                        last_hover = Some((mx, my));
                                        server_out.extend(encode_frame(&ClientMessage::Mouse {
                                            x: mx,
                                            y: my,
                                            kind,
                                        }));
                                    }
                                    MouseKind::Click if close_confirm.is_some() => {
                                        close_confirm = None;
                                        server_out.extend(encode_frame(&ClientMessage::Refresh));
                                    }
                                    MouseKind::Click if menu.is_some() => {
                                        let ms = *menu.as_ref().unwrap();
                                        if let Some(i) = menu::item_at(
                                            &ms, cols, rows, status_top, prefix, mx, my,
                                        ) {
                                            let action = ms.menu().items[i].action;
                                            let project = ms.project();
                                            menu = None;
                                            server_out
                                                .extend(encode_frame(&ClientMessage::Refresh));
                                            match run_menu_action(
                                                action,
                                                project,
                                                confirm_close,
                                                confirm_tab_close,
                                                &mut server_out,
                                                &mut tasks_pending,
                                                &mut agents_pending,
                                            ) {
                                                Surface::None => {}
                                                Surface::Task => {
                                                    task = Some(TaskInput::new());
                                                    server_out.extend(encode_frame(
                                                        &ClientMessage::Suggest,
                                                    ));
                                                }
                                                Surface::Rename => {
                                                    rename = Some((
                                                        LineInput::new("Rename tab"),
                                                        RenameTarget::Window,
                                                    ))
                                                }
                                                Surface::RenameSession => {
                                                    rename = Some((
                                                        LineInput::with_text(
                                                            "Rename Workspace",
                                                            session_name.clone(),
                                                        ),
                                                        RenameTarget::Session,
                                                    ))
                                                }
                                                Surface::RenameProject(project) => {
                                                    rename = Some((
                                                        LineInput::new("Rename Project"),
                                                        RenameTarget::Project(project),
                                                    ))
                                                }
                                                Surface::Sessions => {
                                                    if remote {
                                                        sessions_pending = true;
                                                        server_out.extend(encode_frame(
                                                            &ClientMessage::WorkspaceList,
                                                        ));
                                                    } else {
                                                        sessions =
                                                            Some(SessionsState::open(&sock_path));
                                                    }
                                                }
                                                Surface::Settings => {
                                                    settings_pending = true;
                                                }
                                                Surface::About => {
                                                    tasks_pending = false;
                                                    agents_pending = false;
                                                    settings_pending = false;
                                                    projects_pending = false;
                                                    about = Some(AboutView::new());
                                                }
                                                Surface::Projects => {
                                                    projects_pending = true;
                                                }
                                                Surface::NewProject => {
                                                    new_project = Some(if remote {
                                                        NewProjectView::for_remote()
                                                    } else {
                                                        NewProjectView::new()
                                                    });
                                                }
                                                Surface::Confirm(message) => {
                                                    let overlay = close_confirmation(&message);
                                                    write_stdout(&overlay.render(cols, rows));
                                                    close_confirm = Some((overlay, *message));
                                                }
                                                Surface::Detach => {
                                                    server_out.extend(encode_frame(
                                                        &ClientMessage::Detach,
                                                    ));
                                                    let _ = flush(&mut stream, &mut server_out);
                                                    break 'outer;
                                                }
                                            }
                                        } else {
                                            menu = None;
                                            server_out
                                                .extend(encode_frame(&ClientMessage::Refresh));
                                        }
                                    }
                                    MouseKind::Click if taskman.is_some() => {
                                        let action =
                                            taskman.as_mut().unwrap().click(cols, rows, mx, my);
                                        apply_task_action(
                                            action,
                                            &mut taskman,
                                            &mut server_out,
                                            cols,
                                            rows,
                                        );
                                    }
                                    MouseKind::Click if agentman.is_some() => {
                                        let action =
                                            agentman.as_mut().unwrap().click(cols, rows, mx, my);
                                        apply_agents_action(
                                            action,
                                            &mut agentman,
                                            &mut server_out,
                                            cols,
                                            rows,
                                        );
                                    }
                                    MouseKind::Click if settings.is_some() => {
                                        let action =
                                            settings.as_mut().unwrap().click(cols, rows, mx, my);
                                        let migrate = apply_settings_action(
                                            action,
                                            &mut settings,
                                            &mut server_out,
                                            cols,
                                            rows,
                                        );
                                        if migrate {
                                            server_out.extend(encode_frame(&ClientMessage::Detach));
                                            let _ = flush(&mut stream, &mut server_out);
                                            outcome = Outcome::MigrateDesktop;
                                            break 'outer;
                                        }
                                    }
                                    MouseKind::Click if about.is_some() => {
                                        let action =
                                            about.as_ref().unwrap().click(cols, rows, mx, my);
                                        apply_about_action(action, &mut about, &mut server_out);
                                    }
                                    MouseKind::Click if new_project.is_some() => {
                                        let outside = new_project
                                            .as_ref()
                                            .map(|view| {
                                                !view.overlay().contains(cols, rows, mx, my)
                                            })
                                            .unwrap_or(false);
                                        if outside {
                                            new_project = None;
                                            server_out
                                                .extend(encode_frame(&ClientMessage::Refresh));
                                        }
                                    }
                                    MouseKind::Click if projectman.is_some() => {
                                        let action =
                                            projectman.as_mut().unwrap().click(cols, rows, mx, my);
                                        apply_project_action(
                                            action,
                                            &mut projectman,
                                            &mut new_project,
                                            &mut rename,
                                            &mut server_out,
                                            (cols, rows),
                                            remote,
                                        );
                                    }
                                    MouseKind::Click if sessions.is_some() => {
                                        let action =
                                            sessions.as_mut().unwrap().click(cols, rows, mx, my);
                                        match action {
                                            SessionAction::None => {}
                                            SessionAction::Redraw => {
                                                let st = sessions.as_ref().unwrap();
                                                write_stdout(&st.overlay().render(cols, rows));
                                            }
                                            SessionAction::Close => {
                                                sessions = None;
                                                server_out
                                                    .extend(encode_frame(&ClientMessage::Refresh));
                                            }
                                            SessionAction::Switch { name, path } => {
                                                server_out
                                                    .extend(encode_frame(&ClientMessage::Detach));
                                                let _ = flush(&mut stream, &mut server_out);
                                                outcome = if remote {
                                                    Outcome::RemoteWorkspace(name)
                                                } else {
                                                    Outcome::Switch(path)
                                                };
                                                break 'outer;
                                            }
                                            SessionAction::Revive(name) => {
                                                server_out
                                                    .extend(encode_frame(&ClientMessage::Detach));
                                                let _ = flush(&mut stream, &mut server_out);
                                                outcome = if remote {
                                                    Outcome::RemoteWorkspace(name)
                                                } else {
                                                    Outcome::ReviveWorkspace(name)
                                                };
                                                break 'outer;
                                            }
                                            SessionAction::SetDefault(name) => {
                                                let st = sessions.as_mut().unwrap();
                                                st.set_default(&name);
                                                write_stdout(&st.overlay().render(cols, rows));
                                            }
                                            SessionAction::KillCurrent
                                            | SessionAction::Kill { .. } => {}
                                        }
                                    }
                                    MouseKind::Click if rename.is_some() => {
                                        // Like New Task: a click outside cancels.
                                        let outside = rename
                                            .as_ref()
                                            .map(|(r, _)| !r.overlay().contains(cols, rows, mx, my))
                                            .unwrap_or(false);
                                        if outside {
                                            rename = None;
                                            server_out
                                                .extend(encode_frame(&ClientMessage::Refresh));
                                        }
                                    }
                                    MouseKind::Click if view.is_some() => {
                                        let action = match view.as_mut() {
                                            Some(View::Observatory(observatory)) => {
                                                observatory.click(cols, rows, mx, my)
                                            }
                                            None => ObservatoryAction::None,
                                        };
                                        apply_observatory_action(
                                            action,
                                            &mut view,
                                            &mut server_out,
                                            cols,
                                            rows,
                                        );
                                    }
                                    MouseKind::Click if task.is_some() => {
                                        // New Task: a click outside its box cancels;
                                        // clicks inside are ignored (typing works).
                                        let outside = task
                                            .as_ref()
                                            .map(|t| !t.overlay().contains(cols, rows, mx, my))
                                            .unwrap_or(false);
                                        if outside {
                                            task = None;
                                            server_out
                                                .extend(encode_frame(&ClientMessage::Refresh));
                                        }
                                    }
                                    MouseKind::Click => {
                                        server_out.extend(encode_frame(&ClientMessage::Mouse {
                                            x: mx,
                                            y: my,
                                            kind,
                                        }));
                                    }
                                    MouseKind::RightClick => {
                                        if view.is_none()
                                            && task.is_none()
                                            && close_confirm.is_none()
                                            && menu.is_none()
                                            && rename.is_none()
                                            && sessions.is_none()
                                            && taskman.is_none()
                                            && agentman.is_none()
                                            && settings.is_none()
                                            && about.is_none()
                                            && new_project.is_none()
                                            && projectman.is_none()
                                        {
                                            server_out.extend(encode_frame(
                                                &ClientMessage::Mouse { x: mx, y: my, kind },
                                            ));
                                        }
                                    }
                                    // Drags are deduped by cell like hover so
                                    // they don't flood the socket.
                                    MouseKind::Drag => {
                                        if view.is_some()
                                            || task.is_some()
                                            || close_confirm.is_some()
                                            || menu.is_some()
                                            || rename.is_some()
                                            || sessions.is_some()
                                            || taskman.is_some()
                                            || agentman.is_some()
                                            || settings.is_some()
                                            || about.is_some()
                                            || new_project.is_some()
                                            || projectman.is_some()
                                            || last_drag == Some((mx, my))
                                        {
                                            continue;
                                        }
                                        last_drag = Some((mx, my));
                                        server_out.extend(encode_frame(&ClientMessage::Mouse {
                                            x: mx,
                                            y: my,
                                            kind,
                                        }));
                                    }
                                    // Wheel + release go to the server (scroll
                                    // or app passthrough) unless an overlay is
                                    // capturing the mouse.
                                    MouseKind::WheelUp | MouseKind::WheelDown
                                        if taskman.is_some() =>
                                    {
                                        let tv = taskman.as_mut().unwrap();
                                        let key: &[u8] = if kind == MouseKind::WheelUp {
                                            b"k"
                                        } else {
                                            b"j"
                                        };
                                        if tv.handle(key, cols, rows) == TaskAction::Redraw {
                                            write_stdout(&tv.render(cols, rows));
                                        }
                                    }
                                    MouseKind::WheelUp | MouseKind::WheelDown
                                        if agentman.is_some() =>
                                    {
                                        let av = agentman.as_mut().unwrap();
                                        let key: &[u8] = if kind == MouseKind::WheelUp {
                                            b"k"
                                        } else {
                                            b"j"
                                        };
                                        if av.handle(key, cols, rows) == AgentsAction::Redraw {
                                            write_stdout(&av.render(cols, rows));
                                        }
                                    }
                                    MouseKind::WheelUp | MouseKind::WheelDown
                                        if settings.is_some() =>
                                    {
                                        let view = settings.as_mut().unwrap();
                                        let key: &[u8] = if kind == MouseKind::WheelUp {
                                            b"k"
                                        } else {
                                            b"j"
                                        };
                                        if view.handle(key, cols, rows) == SettingsAction::Redraw {
                                            write_stdout(&view.render(cols, rows));
                                        }
                                    }
                                    MouseKind::WheelUp | MouseKind::WheelDown
                                        if about.is_some() => {}
                                    MouseKind::WheelUp | MouseKind::WheelDown if view.is_some() => {
                                        let key: &[u8] = if kind == MouseKind::WheelUp {
                                            b"k"
                                        } else {
                                            b"j"
                                        };
                                        let action = match view.as_mut() {
                                            Some(View::Observatory(observatory)) => {
                                                observatory.handle(key, cols, rows)
                                            }
                                            None => ObservatoryAction::None,
                                        };
                                        apply_observatory_action(
                                            action,
                                            &mut view,
                                            &mut server_out,
                                            cols,
                                            rows,
                                        );
                                    }
                                    MouseKind::WheelUp
                                    | MouseKind::WheelDown
                                    | MouseKind::Release => {
                                        last_drag = None;
                                        if view.is_none()
                                            && task.is_none()
                                            && close_confirm.is_none()
                                            && menu.is_none()
                                            && rename.is_none()
                                            && sessions.is_none()
                                            && taskman.is_none()
                                            && agentman.is_none()
                                            && settings.is_none()
                                            && about.is_none()
                                            && new_project.is_none()
                                            && projectman.is_none()
                                        {
                                            server_out.extend(encode_frame(
                                                &ClientMessage::Mouse { x: mx, y: my, kind },
                                            ));
                                        }
                                    }
                                }
                            }
                            let (focus, cleaned) = strip_focus_events(&passthrough);
                            passthrough = cleaned;
                            if let Some(focused) = focus {
                                terminal_focused = focused;
                            }
                            if focus == Some(true) {
                                server_out.extend(encode_frame(&ClientMessage::FocusGained));
                            }
                            if passthrough.is_empty() {
                                continue; // pure mouse read; nothing to key-process
                            }
                            let chunk = &passthrough[..];
                            if let Some((_, message)) = close_confirm.take() {
                                if chunk
                                    .iter()
                                    .any(|key| matches!(key, b'y' | b'Y' | b'\r' | b'\n'))
                                {
                                    server_out.extend(encode_frame(&message));
                                }
                                server_out.extend(encode_frame(&ClientMessage::Refresh));
                                continue;
                            }
                            if let Some(ms) = &mut menu {
                                // The menu captures keys: arrows/hjkl navigate,
                                // Enter runs, Esc/q closes.
                                match handle_menu_keys(chunk, ms) {
                                    MenuKeys::Redraw => {
                                        write_stdout(&menu::render_menu(
                                            ms, cols, rows, status_top, prefix,
                                        ));
                                    }
                                    MenuKeys::Switched => {
                                        // A different menu (different box size):
                                        // erase via repaint, composite the new
                                        // box when the frame arrives.
                                        server_out.extend(encode_frame(&ClientMessage::Refresh));
                                    }
                                    MenuKeys::Close => {
                                        menu = None;
                                        server_out.extend(encode_frame(&ClientMessage::Refresh));
                                    }
                                    MenuKeys::Run => {
                                        let action = ms.action();
                                        let project = ms.project();
                                        menu = None;
                                        server_out.extend(encode_frame(&ClientMessage::Refresh));
                                        match run_menu_action(
                                            action,
                                            project,
                                            confirm_close,
                                            confirm_tab_close,
                                            &mut server_out,
                                            &mut tasks_pending,
                                            &mut agents_pending,
                                        ) {
                                            Surface::None => {}
                                            Surface::Task => {
                                                task = Some(TaskInput::new());
                                                server_out
                                                    .extend(encode_frame(&ClientMessage::Suggest));
                                            }
                                            Surface::Rename => {
                                                rename = Some((
                                                    LineInput::new("Rename tab"),
                                                    RenameTarget::Window,
                                                ))
                                            }
                                            Surface::RenameSession => {
                                                rename = Some((
                                                    LineInput::with_text(
                                                        "Rename Workspace",
                                                        session_name.clone(),
                                                    ),
                                                    RenameTarget::Session,
                                                ))
                                            }
                                            Surface::RenameProject(project) => {
                                                rename = Some((
                                                    LineInput::new("Rename Project"),
                                                    RenameTarget::Project(project),
                                                ))
                                            }
                                            Surface::Sessions => {
                                                if remote {
                                                    sessions_pending = true;
                                                    server_out.extend(encode_frame(
                                                        &ClientMessage::WorkspaceList,
                                                    ));
                                                } else {
                                                    sessions =
                                                        Some(SessionsState::open(&sock_path));
                                                }
                                            }
                                            Surface::Settings => {
                                                settings_pending = true;
                                            }
                                            Surface::About => {
                                                tasks_pending = false;
                                                agents_pending = false;
                                                settings_pending = false;
                                                projects_pending = false;
                                                about = Some(AboutView::new());
                                            }
                                            Surface::Projects => {
                                                projects_pending = true;
                                            }
                                            Surface::NewProject => {
                                                new_project = Some(if remote {
                                                    NewProjectView::for_remote()
                                                } else {
                                                    NewProjectView::new()
                                                });
                                            }
                                            Surface::Confirm(message) => {
                                                let overlay = close_confirmation(&message);
                                                write_stdout(&overlay.render(cols, rows));
                                                close_confirm = Some((overlay, *message));
                                            }
                                            Surface::Detach => {
                                                server_out
                                                    .extend(encode_frame(&ClientMessage::Detach));
                                                let _ = flush(&mut stream, &mut server_out);
                                                break 'outer;
                                            }
                                        }
                                    }
                                    MenuKeys::None => {}
                                }
                                continue;
                            }
                            if let Some(st) = &mut sessions {
                                // Typing always filters by Workspace name.
                                // Arrows move through matches, Enter switches,
                                // Ctrl-G x kills, and Escape closes.
                                match st.handle(chunk) {
                                    SessionAction::None => {}
                                    SessionAction::Redraw => {
                                        write_stdout(&st.overlay().render(cols, rows));
                                    }
                                    SessionAction::Close => {
                                        sessions = None;
                                        server_out.extend(encode_frame(&ClientMessage::Refresh));
                                    }
                                    SessionAction::Switch { name, path } => {
                                        server_out.extend(encode_frame(&ClientMessage::Detach));
                                        let _ = flush(&mut stream, &mut server_out);
                                        outcome = if remote {
                                            Outcome::RemoteWorkspace(name)
                                        } else {
                                            Outcome::Switch(path)
                                        };
                                        break 'outer;
                                    }
                                    SessionAction::Revive(name) => {
                                        server_out.extend(encode_frame(&ClientMessage::Detach));
                                        let _ = flush(&mut stream, &mut server_out);
                                        outcome = if remote {
                                            Outcome::RemoteWorkspace(name)
                                        } else {
                                            Outcome::ReviveWorkspace(name)
                                        };
                                        break 'outer;
                                    }
                                    SessionAction::SetDefault(name) => {
                                        st.set_default(&name);
                                        write_stdout(&st.overlay().render(cols, rows));
                                    }
                                    SessionAction::KillCurrent => {
                                        // Killing the Workspace we are on: the
                                        // server EOF then ends this client.
                                        server_out.extend(encode_frame(&ClientMessage::KillServer));
                                        sessions = None;
                                    }
                                    SessionAction::Kill { index, path } => {
                                        let _ = kill_server(&path);
                                        st.mark_stopped(index);
                                        write_stdout(&st.overlay().render(cols, rows));
                                    }
                                }
                                continue;
                            }
                            // Modal key routing checks taskman before agentman,
                            // the same order every render path uses - the keys
                            // must always drive the modal that is on top.
                            if taskman.is_some() {
                                let action = taskman.as_mut().unwrap().handle(chunk, cols, rows);
                                apply_task_action(
                                    action,
                                    &mut taskman,
                                    &mut server_out,
                                    cols,
                                    rows,
                                );
                                continue;
                            }
                            if agentman.is_some() {
                                let action = agentman.as_mut().unwrap().handle(chunk, cols, rows);
                                apply_agents_action(
                                    action,
                                    &mut agentman,
                                    &mut server_out,
                                    cols,
                                    rows,
                                );
                                continue;
                            }
                            if settings.is_some() {
                                let action = settings.as_mut().unwrap().handle(chunk, cols, rows);
                                let migrate = apply_settings_action(
                                    action,
                                    &mut settings,
                                    &mut server_out,
                                    cols,
                                    rows,
                                );
                                if migrate {
                                    server_out.extend(encode_frame(&ClientMessage::Detach));
                                    let _ = flush(&mut stream, &mut server_out);
                                    outcome = Outcome::MigrateDesktop;
                                    break 'outer;
                                }
                                continue;
                            }
                            if about.is_some() {
                                let action = about.as_ref().unwrap().handle(chunk);
                                apply_about_action(action, &mut about, &mut server_out);
                                continue;
                            }
                            if new_project.is_some() {
                                let action = new_project.as_mut().unwrap().handle(chunk);
                                apply_new_project_action(
                                    action,
                                    &mut new_project,
                                    &mut project_create_pending,
                                    &mut server_out,
                                    cols,
                                    rows,
                                );
                                continue;
                            }
                            if projectman.is_some() {
                                let action = projectman.as_mut().unwrap().handle(chunk);
                                apply_project_action(
                                    action,
                                    &mut projectman,
                                    &mut new_project,
                                    &mut rename,
                                    &mut server_out,
                                    (cols, rows),
                                    remote,
                                );
                                continue;
                            }
                            if view.is_some() {
                                let action = match view.as_mut() {
                                    Some(View::Observatory(observatory)) => {
                                        observatory.handle(chunk, cols, rows)
                                    }
                                    None => ObservatoryAction::None,
                                };
                                apply_observatory_action(
                                    action,
                                    &mut view,
                                    &mut server_out,
                                    cols,
                                    rows,
                                );
                                continue;
                            }
                            if let Some((ri, target)) = &mut rename {
                                // The rename input captures keys: edit, Enter
                                // applies, Esc/Ctrl-C cancels.
                                let mut close = false;
                                let mut index = 0;
                                while index < chunk.len() {
                                    let (key, used) = decode_key(chunk, index);
                                    index += used.max(1);
                                    match key {
                                        LineKey::Escape | LineKey::Cancel => {
                                            close = true;
                                            break;
                                        }
                                        LineKey::Enter => {
                                            let msg = match target {
                                                RenameTarget::Window => {
                                                    ClientMessage::RenameWindow {
                                                        name: ri.buf.clone(),
                                                    }
                                                }
                                                RenameTarget::Session => {
                                                    ClientMessage::RenameSession {
                                                        name: ri.buf.clone(),
                                                    }
                                                }
                                                RenameTarget::Project(project) => {
                                                    ClientMessage::ProjectRename {
                                                        project: *project,
                                                        name: ri.buf.clone(),
                                                    }
                                                }
                                            };
                                            server_out.extend(encode_frame(&msg));
                                            close = true;
                                            break;
                                        }
                                        key => {
                                            ri.edit(key);
                                        }
                                    }
                                }
                                if close {
                                    rename = None;
                                    server_out.extend(encode_frame(&ClientMessage::Refresh));
                                } else {
                                    write_stdout(&ri.overlay().render(cols, rows));
                                }
                                continue;
                            }
                            if let Some(ti) = &mut task {
                                // The New Task overlay captures input: edit
                                // the line, arrows move the suggestion
                                // selection, Tab completes, Enter submits,
                                // Esc/Ctrl-C cancels.
                                let mut close = false;
                                let mut i = 0;
                                while i < chunk.len() {
                                    let (key, used) = decode_key(chunk, i);
                                    i += used.max(1);
                                    match key {
                                        LineKey::Escape | LineKey::Cancel => {
                                            close = true; // Ctrl-C: cancel
                                            break;
                                        }
                                        LineKey::Enter => {
                                            if let Some(msg) = submit_task(ti) {
                                                server_out.extend(encode_frame(&msg));
                                            }
                                            close = true;
                                            break;
                                        }
                                        LineKey::Tab => {
                                            ti.accept(); // Tab: take the suggestion
                                        }
                                        LineKey::Up => ti.sel_up(),
                                        LineKey::Down => ti.sel_down(),
                                        key => {
                                            ti.edit(key);
                                        }
                                    }
                                }
                                if close {
                                    task = None;
                                    server_out.extend(encode_frame(&ClientMessage::Refresh));
                                } else {
                                    write_stdout(&ti.overlay().render(cols, rows));
                                }
                                continue;
                            }
                            match process_input_with_bindings(
                                chunk,
                                prefix,
                                confirm_close,
                                confirm_tab_close,
                                &cfg.bindings,
                                &mut prefix_state,
                                &mut server_out,
                            ) {
                                Action::Detach => {
                                    // Detach requested: send it, best-effort flush, exit.
                                    server_out.extend(encode_frame(&ClientMessage::Detach));
                                    let _ = flush(&mut stream, &mut server_out);
                                    break 'outer;
                                }
                                Action::ToggleOverlay => {
                                    let ti = TaskInput::new();
                                    write_stdout(&ti.overlay().render(cols, rows));
                                    task = Some(ti);
                                    // Fetch /project completions in the background.
                                    server_out.extend(encode_frame(&ClientMessage::Suggest));
                                }
                                Action::Observatory => {
                                    server_out.extend(encode_frame(&ClientMessage::Command(
                                        Command::Observatory,
                                    )));
                                }
                                Action::Tasks => {
                                    tasks_pending = true;
                                    server_out.extend(encode_frame(&ClientMessage::Tasks));
                                }
                                Action::Agents => {
                                    // Ask the server for the agents snapshot;
                                    // the modal opens when the reply arrives.
                                    agents_pending = true;
                                    server_out.extend(encode_frame(&ClientMessage::Agents));
                                }
                                Action::Rename => {
                                    let ri = LineInput::new("Rename tab");
                                    write_stdout(&ri.overlay().render(cols, rows));
                                    rename = Some((ri, RenameTarget::Window));
                                }
                                Action::RenameSession => {
                                    let ri = LineInput::with_text(
                                        "Rename Workspace",
                                        session_name.clone(),
                                    );
                                    write_stdout(&ri.overlay().render(cols, rows));
                                    rename = Some((ri, RenameTarget::Session));
                                }
                                Action::Menu => {
                                    let ms = MenuState::open(0);
                                    write_stdout(&menu::render_menu(
                                        &ms, cols, rows, status_top, prefix,
                                    ));
                                    menu = Some(ms);
                                }
                                Action::Sessions => {
                                    if remote {
                                        sessions_pending = true;
                                        server_out
                                            .extend(encode_frame(&ClientMessage::WorkspaceList));
                                    } else {
                                        let st = SessionsState::open(&sock_path);
                                        write_stdout(&st.overlay().render(cols, rows));
                                        sessions = Some(st);
                                    }
                                }
                                Action::Settings => {
                                    settings_pending = true;
                                    server_out.extend(encode_frame(&ClientMessage::Settings));
                                }
                                Action::Projects => {
                                    projects_pending = true;
                                    server_out.extend(encode_frame(&ClientMessage::WorkspaceState));
                                }
                                Action::NewProject => {
                                    let view = if remote {
                                        NewProjectView::for_remote()
                                    } else {
                                        NewProjectView::new()
                                    };
                                    write_stdout(&view.overlay().render(cols, rows));
                                    new_project = Some(view);
                                }
                                Action::CloseWorkspace => {
                                    server_out.extend(encode_frame(&ClientMessage::KillServer));
                                }
                                Action::Confirm(command) => {
                                    let message = ClientMessage::Command(command);
                                    let overlay = close_confirmation(&message);
                                    write_stdout(&overlay.render(cols, rows));
                                    close_confirm = Some((overlay, message));
                                }
                                Action::None => {}
                            }
                        } else if n == 0 {
                            break 'outer;
                        } else {
                            let e = std::io::Error::last_os_error();
                            match e.kind() {
                                std::io::ErrorKind::WouldBlock => break,
                                std::io::ErrorKind::Interrupted => continue,
                                _ => break 'outer,
                            }
                        }
                    }
                    service_server_output(
                        poll.registry(),
                        &mut stream,
                        &mut server_out,
                        &mut server_write_interest,
                    )?;
                }
                WINCH if ev.is_readable() => {
                    // Drain the self-pipe, then report the new size to the server.
                    let mut b = [0u8; 64];
                    loop {
                        let n = unsafe {
                            libc::read(winch_fd, b.as_mut_ptr() as *mut libc::c_void, b.len())
                        };
                        if n <= 0 {
                            break;
                        }
                    }
                    let (nc, nr) = tty_size(libc::STDOUT_FILENO);
                    cols = nc;
                    rows = nr;
                    // Local overlays follow the terminal immediately; the
                    // server hears one settled size per gesture (flushed
                    // below the event loop) instead of every intermediate one.
                    resizes.note((cols, rows), std::time::Instant::now());
                    if let Some(v) = &view {
                        write_stdout(&v.render(cols, rows));
                    } else if let Some(ti) = &task {
                        write_stdout(&ti.overlay().render(cols, rows));
                    } else if let Some((overlay, _)) = &close_confirm {
                        write_stdout(&overlay.render(cols, rows));
                    } else if let Some((ri, _)) = &rename {
                        write_stdout(&ri.overlay().render(cols, rows));
                    } else if let Some(st) = &sessions {
                        write_stdout(&st.overlay().render(cols, rows));
                    } else if let Some(tv) = &taskman {
                        write_stdout(&tv.render(cols, rows));
                    } else if let Some(av) = &agentman {
                        write_stdout(&av.render(cols, rows));
                    } else if let Some(settings) = &settings {
                        write_stdout(&settings.render(cols, rows));
                    } else if let Some(about) = &about {
                        write_stdout(&about.render(cols, rows));
                    } else if let Some(view) = &new_project {
                        write_stdout(&view.overlay().render(cols, rows));
                    } else if let Some(projects) = &projectman {
                        write_stdout(&projects.overlay().render(cols, rows));
                    } else if let Some(ms) = &menu {
                        write_stdout(&menu::render_menu(ms, cols, rows, status_top, prefix));
                    }
                    service_server_output(
                        poll.registry(),
                        &mut stream,
                        &mut server_out,
                        &mut server_write_interest,
                    )?;
                }
                SERVER => {
                    if ev.is_readable() {
                        let mut buf = [0u8; 16384];
                        let mut messages = Vec::new();
                        let mut disconnect = None;
                        loop {
                            match stream.read(&mut buf) {
                                Ok(0) => {
                                    disconnect =
                                        Some(server_disconnect_error("server connection", None));
                                    break;
                                }
                                Ok(n) => {
                                    decoder.push(&buf[..n]);
                                    drain_server_messages(&mut decoder, &mut messages).map_err(
                                        |error| {
                                            std::io::Error::new(
                                                std::io::ErrorKind::InvalidData,
                                                format!("invalid server frame: {error:?}"),
                                            )
                                        },
                                    )?;
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                                Err(error) => {
                                    disconnect = Some(server_disconnect_error(
                                        "server connection",
                                        Some(error),
                                    ));
                                    break;
                                }
                            }
                        }
                        // All render ops of this batch plus the overlay
                        // repaint go out as ONE synchronized write: separate
                        // writes let the terminal render pane bytes over the
                        // overlay for a visible flicker frame.
                        let mut frame: Vec<u8> = Vec::new();
                        let mut menu_composited = false;
                        for message in messages {
                            match message {
                                ServerMessage::RenderOps(ops) => {
                                    frame.extend_from_slice(&ops);
                                }
                                ServerMessage::Bell => write_stdout(b"\x07"),
                                ServerMessage::Chime {
                                    kind,
                                    sound,
                                    file,
                                    pane_active,
                                } => {
                                    if chime::should_sound(kind, pane_active, terminal_focused)
                                        && chime::play(kind, sound, &file) == chime::Playback::Bell
                                    {
                                        write_stdout(b"\x07");
                                    }
                                }
                                ServerMessage::WindowTitle { title } => {
                                    let title = sanitize_terminal_title(&title);
                                    write_stdout(format!("\x1b]0;{title}\x07").as_bytes());
                                }
                                ServerMessage::Suggestions { projects, agents } => {
                                    if let Some(ti) = &mut task {
                                        ti.projects = projects;
                                        ti.agents = agents;
                                        write_stdout(&ti.overlay().render(cols, rows));
                                    }
                                }
                                ServerMessage::SessionRenamed { name } => {
                                    sock_path = sock_path.with_file_name(format!("{name}.sock"));
                                    session_name = name;
                                }
                                ServerMessage::NestedInput { enabled } => {
                                    if prefix_state.nested != enabled {
                                        prefix_state = PrefixState {
                                            nested: enabled,
                                            ..PrefixState::default()
                                        };
                                    }
                                }
                                ServerMessage::Detached | ServerMessage::Exited => break 'outer,
                                // Info is only sent in response to ListInfo (the
                                // ls/query path), never during an attach.
                                ServerMessage::Info { .. } => {}
                                ServerMessage::Fleet { entries } => {
                                    if let Some(View::Observatory(observatory)) = &mut view {
                                        observatory.refresh(entries);
                                    } else {
                                        view =
                                            Some(View::Observatory(ObservatoryView::new(entries)));
                                    }
                                    if let Some(View::Observatory(observatory)) = &view {
                                        write_stdout(&observatory.render(cols, rows));
                                    }
                                }
                                ServerMessage::DevServers { entries } => {
                                    if let Some(View::Observatory(observatory)) = &mut view {
                                        observatory.refresh_servers(entries);
                                        write_stdout(&observatory.render(cols, rows));
                                    }
                                }
                                ServerMessage::Waiting { items }
                                | ServerMessage::WaitingActed { items, .. } => {
                                    if let Some(View::Observatory(observatory)) = &mut view {
                                        observatory.refresh_waiting(items);
                                        write_stdout(&observatory.render(cols, rows));
                                    }
                                }
                                ServerMessage::OpenUrl { url } => {
                                    if open_desktop_url(&url).is_err() {
                                        write_stdout(b"\x07");
                                    }
                                }
                                ServerMessage::OpenMenu {
                                    menu: requested,
                                    x,
                                    y,
                                    width,
                                    open_up,
                                } => {
                                    let title = match requested {
                                        ChromeMenu::Tabs => "Tabs",
                                        ChromeMenu::Agents => "Agents",
                                        ChromeMenu::Workspace => "Workspace",
                                        ChromeMenu::Projects => "Projects",
                                        ChromeMenu::Project(_) => "Project",
                                    };
                                    if let Some(index) = uniterm_core::menu::MENUS
                                        .iter()
                                        .position(|menu| menu.title == title)
                                    {
                                        let state = match requested {
                                            ChromeMenu::Project(project) => {
                                                MenuState::anchored_project(
                                                    index, project, x, y, width, open_up,
                                                )
                                            }
                                            _ => MenuState::anchored(index, x, y, width, open_up),
                                        };
                                        frame.extend_from_slice(&menu::render_menu(
                                            &state, cols, rows, status_top, prefix,
                                        ));
                                        menu_composited = true;
                                        menu = Some(state);
                                    }
                                }
                                ServerMessage::OpenChromeAction { action } => match action {
                                    uniterm_proto::ChromeAction::NewTask => {
                                        let input = TaskInput::new();
                                        frame
                                            .extend_from_slice(&input.overlay().render(cols, rows));
                                        task = Some(input);
                                        server_out.extend(encode_frame(&ClientMessage::Suggest));
                                    }
                                    uniterm_proto::ChromeAction::Tasks => {
                                        tasks_pending = true;
                                        server_out.extend(encode_frame(&ClientMessage::Tasks));
                                    }
                                    uniterm_proto::ChromeAction::Config => {
                                        agents_pending = true;
                                        server_out.extend(encode_frame(&ClientMessage::Agents));
                                    }
                                },
                                // A snapshot refreshes the open modal, or opens
                                // it if the user asked and is still waiting.
                                // Anything else is a late reply to a mutation on
                                // a modal already closed - dropped, so it cannot
                                // reopen what the user dismissed. Opening one
                                // modal closes the other: with a single modal at
                                // a time, keys and paint can never disagree
                                // about which one is on top.
                                ServerMessage::Tasks { items } => {
                                    if let Some(tv) = &mut taskman {
                                        tv.refresh(items);
                                    } else if tasks_pending {
                                        agentman = None;
                                        taskman = Some(TaskView::new(items));
                                    }
                                    tasks_pending = false;
                                    if let Some(tv) = &taskman {
                                        write_stdout(&tv.render(cols, rows));
                                    }
                                }
                                ServerMessage::Agents { items } => {
                                    if let Some(av) = &mut agentman {
                                        av.refresh(items);
                                    } else if agents_pending {
                                        taskman = None;
                                        agentman = Some(AgentsView::new(items));
                                    }
                                    agents_pending = false;
                                    if let Some(av) = &agentman {
                                        write_stdout(&av.render(cols, rows));
                                    }
                                }
                                ServerMessage::Settings {
                                    settings: snapshot,
                                    saved,
                                    error,
                                } => {
                                    confirm_close = snapshot.confirm_close;
                                    confirm_tab_close = snapshot.confirm_tab_close;
                                    overlay::set_ui_theme(uniterm_core::Theme::named(
                                        &snapshot.theme,
                                    ));
                                    status_top = snapshot.status_top;
                                    if let Some(view) = &mut settings {
                                        view.refresh(*snapshot, saved, error);
                                    } else if settings_pending {
                                        taskman = None;
                                        agentman = None;
                                        settings = Some(SettingsView::new(*snapshot, saved, error));
                                    }
                                    settings_pending = false;
                                    if let Some(view) = &settings {
                                        write_stdout(&view.render(cols, rows));
                                    }
                                }
                                ServerMessage::Workspaces { current, entries } => {
                                    if sessions_pending {
                                        let state = SessionsState::from_remote(entries, &current);
                                        write_stdout(&state.overlay().render(cols, rows));
                                        sessions = Some(state);
                                    }
                                    sessions_pending = false;
                                }
                                ServerMessage::ProjectCreated { error } => {
                                    if project_create_pending {
                                        project_create_pending = false;
                                        if let Some(error) = error {
                                            if let Some(view) = &mut new_project {
                                                view.reject(error);
                                                write_stdout(&view.overlay().render(cols, rows));
                                            }
                                        } else {
                                            // The server has switched to the new
                                            // Project's fresh Tab. Drop every
                                            // overlay, including a Manage
                                            // Projects modal this was opened
                                            // from, so the user lands in it.
                                            new_project = None;
                                            projectman = None;
                                            projects_pending = false;
                                            server_out
                                                .extend(encode_frame(&ClientMessage::Refresh));
                                        }
                                    }
                                }
                                // Workspace projections are consumed by the
                                // control/query path. An attach loop only
                                // receives one after an explicit request.
                                ServerMessage::Workspace {
                                    name,
                                    active_project: _,
                                    projects,
                                } => {
                                    if let Some(view) = &mut projectman {
                                        view.refresh(name, projects);
                                    } else if projects_pending {
                                        projectman = Some(ProjectsView::new(name, projects));
                                    }
                                    projects_pending = false;
                                    if let Some(view) = &projectman {
                                        write_stdout(&view.overlay().render(cols, rows));
                                    }
                                }
                                ServerMessage::AgentExplanation { .. } => {}
                                ServerMessage::Panes { .. }
                                | ServerMessage::PaneFocused { .. }
                                | ServerMessage::HierarchyFocused { .. }
                                | ServerMessage::TabMoved { .. }
                                | ServerMessage::PaneOutput { .. }
                                | ServerMessage::PaneSent { .. }
                                | ServerMessage::PaneOutputWaited { .. }
                                | ServerMessage::AgentWaited { .. }
                                | ServerMessage::PaneAttached { .. }
                                | ServerMessage::PaneAttachRejected { .. }
                                | ServerMessage::PaneAttachRevoked { .. }
                                | ServerMessage::Instructions { .. }
                                | ServerMessage::InstructionChanged { .. } => {}
                                ServerMessage::AgentLaunchResult { .. } => {}
                                ServerMessage::WorkspaceImported { .. }
                                | ServerMessage::Worktrees(_)
                                | ServerMessage::Runs { .. }
                                | ServerMessage::Artifacts { .. }
                                | ServerMessage::RunForked(_) => {}
                            }
                        }
                        // Keep the overlay on top of any pane repaint
                        // underneath - composited into the same atomic frame.
                        if !frame.is_empty() {
                            if let Some(v) = &view {
                                frame.extend_from_slice(&v.render(cols, rows));
                            } else if let Some(ti) = &task {
                                frame.extend_from_slice(&ti.overlay().render(cols, rows));
                            } else if let Some((overlay, _)) = &close_confirm {
                                frame.extend_from_slice(&overlay.render(cols, rows));
                            } else if let Some((ri, _)) = &rename {
                                frame.extend_from_slice(&ri.overlay().render(cols, rows));
                            } else if let Some(st) = &sessions {
                                frame.extend_from_slice(&st.overlay().render(cols, rows));
                            } else if let Some(tv) = &taskman {
                                frame.extend_from_slice(&tv.render(cols, rows));
                            } else if let Some(av) = &agentman {
                                frame.extend_from_slice(&av.render(cols, rows));
                            } else if let Some(settings) = &settings {
                                frame.extend_from_slice(&settings.render(cols, rows));
                            } else if let Some(about) = &about {
                                frame.extend_from_slice(&about.render(cols, rows));
                            } else if let Some(view) = &new_project {
                                frame.extend_from_slice(&view.overlay().render(cols, rows));
                            } else if let Some(projects) = &projectman {
                                frame.extend_from_slice(&projects.overlay().render(cols, rows));
                            } else if let Some(ms) = menu.as_ref().filter(|_| !menu_composited) {
                                frame.extend_from_slice(&menu::render_menu(
                                    ms, cols, rows, status_top, prefix,
                                ));
                            }
                            write_sync_frame(&frame);
                        }
                        if let Some(error) = disconnect {
                            return Err(error);
                        }
                    }
                    if ev.is_writable() || !server_out.is_empty() {
                        service_server_output(
                            poll.registry(),
                            &mut stream,
                            &mut server_out,
                            &mut server_write_interest,
                        )?;
                    }
                }
                _ => {}
            }
        }
        if let Some((cols, rows)) = resizes.take_due(std::time::Instant::now()) {
            server_out.extend(encode_frame(&ClientMessage::Resize { cols, rows }));
            service_server_output(
                poll.registry(),
                &mut stream,
                &mut server_out,
                &mut server_write_interest,
            )?;
        }
        if let Some(view) = &mut about {
            let now = std::time::Instant::now();
            if view.tick(now, cols, rows) {
                write_sync_frame(&view.render(cols, rows));
            }
        }
        // Tell the server when an overlay starts/stops covering the frame:
        // our overlay writes desync its per-client render caches, so damage
        // batches must re-emit absolute positions/styles while one is up.
        let now_open = view.is_some()
            || task.is_some()
            || close_confirm.is_some()
            || rename.is_some()
            || sessions.is_some()
            || taskman.is_some()
            || agentman.is_some()
            || settings.is_some()
            || about.is_some()
            || new_project.is_some()
            || projectman.is_some()
            || menu.is_some();
        if now_open != overlay_visible {
            overlay_visible = now_open;
            server_out.extend(encode_frame(&ClientMessage::OverlayVisible {
                on: now_open,
            }));
            service_server_output(
                poll.registry(),
                &mut stream,
                &mut server_out,
                &mut server_write_interest,
            )?;
        }
    }

    // Terminal teardown (leave alt screen, restore termios) happens in the
    // caller's TtyGuard Drop; a Switch outcome keeps the terminal as-is and
    // reconnects.
    Ok(outcome)
}

fn server_disconnect_error(context: &str, source: Option<std::io::Error>) -> std::io::Error {
    match source {
        Some(error) => std::io::Error::new(
            error.kind(),
            format!("{context} failed before a detach or exit response: {error}"),
        ),
        None => std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            format!("{context} closed before a detach or exit response"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unexpected_server_eof_is_not_a_clean_attach_exit() {
        let error = server_disconnect_error("server connection", None);
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionAborted);
        assert!(error
            .to_string()
            .contains("before a detach or exit response"));
    }

    #[test]
    fn server_read_failure_keeps_its_error_kind_and_context() {
        let error = server_disconnect_error(
            "server connection",
            Some(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "peer vanished",
            )),
        );
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        assert!(error.to_string().contains("peer vanished"));
    }
}
