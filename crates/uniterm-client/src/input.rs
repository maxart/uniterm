//! Terminal input decoding for the attach client.
//!
//! Keystrokes and mouse reports arrive as one raw byte stream, so this module
//! owns the split: prefix bindings and menu keys are consumed here, and every
//! other byte is forwarded to the server untouched. Keeping the decoders pure
//! (bytes in, actions and frames out) is what makes them unit-testable without
//! a tty.

use uniterm_proto::{encode_frame, ClientMessage, Command, FocusDir, MouseKind, SplitAxis};

use crate::menu::MenuState;
use crate::tty::flush_passthrough;

/// What a processed key batch asks the attach loop to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Action {
    /// Nothing beyond the frames already appended to `out`.
    None,
    /// The user asked to detach (prefix + d).
    Detach,
    /// The user toggled the New Task overlay (prefix + p) - handled client-side.
    ToggleOverlay,
    /// The user toggled the Observatory rail (prefix + o).
    Observatory,
    /// The user asked to open task management (prefix + t).
    Tasks,
    /// The user asked to open the Manage Agents modal (prefix + a).
    Agents,
    /// The user asked to rename the active window (prefix + ,).
    Rename,
    /// The user asked to rename the session (prefix + $).
    RenameSession,
    /// The user opened the first command-menu group (prefix + m).
    Menu,
    /// The user opened the session switcher (prefix + s).
    Sessions,
    /// Open the schema-backed application Settings surface.
    Settings,
    /// Open the Project manager for the current Workspace.
    Projects,
    /// Open the New Project modal.
    NewProject,
    /// Close the active Workspace and its server.
    CloseWorkspace,
    /// Ask for an explicit second action before a destructive close.
    Confirm(Command),
}

#[derive(Default)]
pub(crate) struct PrefixState {
    pub(crate) pending: bool,
    pub(crate) outer_pending: bool,
    pub(crate) nested: bool,
}

/// Prefix binding that opens the New Task overlay (client-side). `N` and not
/// `p`: `p` is PrevWindow (tmux muscle memory), and binding both made
/// PrevWindow unreachable.
pub(crate) const OVERLAY_KEY: u8 = b'N';
/// Prefix binding that opens the Observatory.
pub(crate) const OBSERVATORY_KEY: u8 = b'o';
/// Prefix binding that opens task management.
pub(crate) const TASKS_KEY: u8 = b't';
/// Prefix binding that opens the Manage Agents modal.
pub(crate) const AGENTS_KEY: u8 = b'a';
/// Prefix binding that opens the Rename-tab input (tmux's rename key).
pub(crate) const RENAME_KEY: u8 = b',';
/// Prefix binding that opens the Rename-session input (tmux's `$`).
pub(crate) const RENAME_SESSION_KEY: u8 = b'$';
/// Prefix binding that opens the first command-menu group.
pub(crate) const MENU_KEY: u8 = b'm';
/// Prefix binding that opens the session switcher (tmux's choose-session key).
pub(crate) const SESSIONS_KEY: u8 = b's';
/// Prefix binding that opens Settings.
pub(crate) const SETTINGS_KEY: u8 = b'g';
/// Prefix binding that opens the Project manager.
pub(crate) const PROJECTS_KEY: u8 = b'P';
/// Prefix binding that opens the New Project modal.
pub(crate) const NEW_PROJECT_KEY: u8 = b'A';
/// Prefix binding that toggles the left-hand Workspace sidebar.
pub(crate) const SIDEBAR_KEY: u8 = b'b';
/// Prefix binding that closes the active Workspace.
pub(crate) const CLOSE_WORKSPACE_KEY: u8 = b'Q';

/// The default prefix key (Ctrl-A; overridable via `prefix` in the config).
/// After it:
///   d detach   % split L/R   " split T/B   h/j/k/l focus   z zoom   x kill pane
///   c new window   n/p next/prev window   1-9 select window (0 = 10)   [ copy-mode
///   N New Task   o Observatory   t Tasks   Ctrl-A sends a literal Ctrl-A
///   A New Project   b toggle Sidebar
///   Q close Workspace
/// These are the built-in bindings; fully rebindable keys + the command language
/// are a later phase (`docs/10`).
#[cfg(test)]
pub(crate) const PREFIX: u8 = 0x01;
pub(crate) const DETACH_KEY: u8 = b'd';

/// Split off a trailing partial keyboard escape sequence - a lone final
/// `ESC`, or an unfinished CSI (`ESC [` with no final byte yet) - so an arrow
/// key whose bytes straddle two reads is not misread as a lone Esc (which
/// closes modals). The tail is prepended to the next read of the same drain;
/// the caller flushes it unchanged when the fd runs dry, so a genuinely lone
/// Esc is never delayed past its own wakeup. A long tail (> 16 bytes) is
/// treated as complete junk rather than held forever.
pub(crate) fn split_partial_escape(chunk: &mut Vec<u8>) -> Vec<u8> {
    let Some(esc) = chunk.iter().rposition(|&b| b == 0x1b) else {
        return Vec::new();
    };
    let rest = &chunk[esc + 1..];
    let partial = match rest {
        [] => true,
        [b'[', params @ ..] if params.len() <= 16 => {
            !params.iter().any(|&b| (0x40..=0x7e).contains(&b))
        }
        _ => false,
    };
    if partial {
        chunk.split_off(esc)
    } else {
        Vec::new()
    }
}

/// Remove terminal focus reports from keyboard input and report the focus
/// state they leave behind: `Some(true)` after a focus-in (which requests an
/// authoritative repaint), `Some(false)` after a focus-out, `None` when the
/// chunk carried neither. Both reports are consumed so neither can leak into
/// a child application as typed escape bytes.
pub(crate) fn strip_focus_events(input: &[u8]) -> (Option<bool>, Vec<u8>) {
    let mut focused = None;
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input.get(index..index + 3) == Some(b"\x1b[I") {
            focused = Some(true);
            index += 3;
        } else if input.get(index..index + 3) == Some(b"\x1b[O") {
            focused = Some(false);
            index += 3;
        } else {
            output.push(input[index]);
            index += 1;
        }
    }
    (focused, output)
}

/// The result of [`scan_mouse`]: parsed mouse events, the non-mouse passthrough
/// bytes, and any trailing partial mouse sequence to prepend to the next read.
pub(crate) type MouseScan = (Vec<(u16, u16, MouseKind)>, Vec<u8>, Vec<u8>);

/// Scan one stdin read while preserving escape sequences split at read edges.
pub(crate) fn scan_stdin_chunk(
    chunk: &[u8],
    mouse_leftover: &mut Vec<u8>,
    key_pending: &mut Vec<u8>,
) -> (Vec<(u16, u16, MouseKind)>, Vec<u8>) {
    let mut raw = std::mem::take(mouse_leftover);
    raw.extend_from_slice(&std::mem::take(key_pending));
    raw.extend_from_slice(chunk);
    let (events, mut passthrough, leftover) = scan_mouse(&raw);
    *mouse_leftover = leftover;
    *key_pending = split_partial_escape(&mut passthrough);
    (events, passthrough)
}

/// Split a stdin chunk into SGR mouse events and the remaining (non-mouse)
/// bytes, returning any trailing partial mouse sequence to prepend next read.
/// Mouse bytes are pulled out so they never reach the pane as garbage; only
/// confirmed `ESC [ < ...` sequences (and a bare `ESC [` tail) are buffered, so
/// a lone Escape key is never delayed.
pub(crate) fn scan_mouse(buf: &[u8]) -> MouseScan {
    let mut events = Vec::new();
    let mut pass = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == 0x1b && buf.get(i + 1) == Some(&b'[') {
            match buf.get(i + 2) {
                Some(&b'<') => {
                    if let Some(off) = buf[i + 3..].iter().position(|&b| b == b'M' || b == b'm') {
                        if let Some(ev) =
                            parse_sgr_mouse(&buf[i + 3..i + 3 + off], buf[i + 3 + off])
                        {
                            events.push(ev);
                        }
                        i = i + 3 + off + 1;
                        continue;
                    }
                    return (events, pass, buf[i..].to_vec()); // incomplete mouse seq
                }
                None => return (events, pass, buf[i..].to_vec()), // bare "ESC[" tail
                _ => {}                                           // ESC[<other> (e.g. arrow key)
            }
        }
        pass.push(buf[i]);
        i += 1;
    }
    (events, pass, Vec::new())
}

/// Parse the body of an SGR mouse report (`b;x;y` between `<` and `M`/`m`).
/// Returns hover (any motion), left press/release, right press, or wheel.
pub(crate) fn parse_sgr_mouse(body: &[u8], term: u8) -> Option<(u16, u16, MouseKind)> {
    let s = std::str::from_utf8(body).ok()?;
    let mut it = s.split(';');
    let b: u32 = it.next()?.parse().ok()?;
    let x: u16 = it.next()?.parse().ok()?;
    let y: u16 = it.next()?.parse().ok()?;
    if b & 64 != 0 {
        // Scroll wheel: 64 = up, 65 = down (66/67 are horizontal; ignored).
        return match b & 3 {
            0 => Some((x, y, MouseKind::WheelUp)),
            1 => Some((x, y, MouseKind::WheelDown)),
            _ => None,
        };
    }
    let kind = if b & 32 != 0 {
        match b & 3 {
            3 => MouseKind::Hover, // motion, no button held
            0 => MouseKind::Drag,  // motion with the left button held
            _ => return None,      // other-button drags
        }
    } else {
        match (b & 3, term) {
            (0, b'M') => MouseKind::Click,
            (0, b'm') => MouseKind::Release,
            (2, b'M') => MouseKind::RightClick,
            _ => return None,
        }
    };
    Some((x, y, kind))
}

/// Feed raw stdin bytes through the prefix state machine, appending encoded
/// `Input`/`Command` frames to `out`. Returns the [`Action`] the loop must take
/// (detach, toggle the overlay, or nothing). `prefix` is the configured prefix
/// byte (default Ctrl-A).
pub(crate) fn process_input_with_bindings(
    bytes: &[u8],
    prefix: u8,
    confirm_close: bool,
    confirm_tab_close: bool,
    bindings: &[uniterm_core::KeyBinding],
    state: &mut PrefixState,
    out: &mut Vec<u8>,
) -> Action {
    let mut passthrough: Vec<u8> = Vec::new();
    for &b in bytes {
        if state.nested && !state.outer_pending {
            if state.pending {
                state.pending = false;
                if b == prefix {
                    state.outer_pending = true;
                } else {
                    passthrough.push(prefix);
                    passthrough.push(b);
                }
            } else if b == prefix {
                state.pending = true;
            } else {
                passthrough.push(b);
            }
            continue;
        }
        if state.nested {
            state.outer_pending = false;
            state.pending = true;
        }
        if state.pending {
            state.pending = false;
            // Preserve ordering: flush typed bytes before acting on the binding.
            flush_passthrough(&mut passthrough, out);
            if let Some(binding) = configured_binding(b, bindings) {
                match binding {
                    ConfiguredBinding::Disabled => continue,
                    ConfiguredBinding::Action(action) => return action,
                    ConfiguredBinding::Command(command) => {
                        if requires_close_confirmation(command, confirm_close, confirm_tab_close) {
                            return Action::Confirm(command);
                        }
                        out.extend(encode_frame(&ClientMessage::Command(command)));
                        continue;
                    }
                }
            } else if b == OVERLAY_KEY {
                return Action::ToggleOverlay;
            } else if b == OBSERVATORY_KEY {
                return Action::Observatory;
            } else if b == TASKS_KEY {
                return Action::Tasks;
            } else if b == AGENTS_KEY {
                return Action::Agents;
            } else if b == RENAME_KEY {
                return Action::Rename;
            } else if b == RENAME_SESSION_KEY {
                return Action::RenameSession;
            } else if b == MENU_KEY {
                return Action::Menu;
            } else if b == SESSIONS_KEY {
                return Action::Sessions;
            } else if b == SETTINGS_KEY {
                return Action::Settings;
            } else if b == PROJECTS_KEY {
                return Action::Projects;
            } else if b == NEW_PROJECT_KEY {
                return Action::NewProject;
            } else if b == CLOSE_WORKSPACE_KEY {
                return Action::CloseWorkspace;
            } else if let Some(cmd) = command_for(b) {
                if requires_close_confirmation(cmd, confirm_close, confirm_tab_close) {
                    return Action::Confirm(cmd);
                }
                out.extend(encode_frame(&ClientMessage::Command(cmd)));
            } else {
                match b {
                    DETACH_KEY => return Action::Detach,
                    x if x == prefix => passthrough.push(prefix), // prefix prefix => literal
                    other => {
                        // Unbound: send prefix + key through (tmux behaviour).
                        passthrough.push(prefix);
                        passthrough.push(other);
                    }
                }
            }
        } else if b == prefix {
            state.pending = true;
        } else {
            passthrough.push(b);
        }
    }
    flush_passthrough(&mut passthrough, out);
    Action::None
}

pub(crate) fn requires_close_confirmation(
    command: Command,
    confirm_close: bool,
    confirm_tab_close: bool,
) -> bool {
    match command {
        Command::KillPane => confirm_close,
        Command::KillWindow => confirm_tab_close,
        _ => false,
    }
}

#[cfg(test)]
pub(crate) fn process_input(
    bytes: &[u8],
    prefix: u8,
    confirm_close: bool,
    prefix_pending: &mut bool,
    out: &mut Vec<u8>,
) -> Action {
    let mut state = PrefixState {
        pending: *prefix_pending,
        ..PrefixState::default()
    };
    let action = process_input_with_bindings(
        bytes,
        prefix,
        confirm_close,
        confirm_close,
        &[],
        &mut state,
        out,
    );
    *prefix_pending = state.pending;
    action
}

#[derive(Clone, Copy)]
pub(crate) enum ConfiguredBinding {
    Disabled,
    Action(Action),
    Command(Command),
}

pub(crate) fn configured_binding(
    key: u8,
    bindings: &[uniterm_core::KeyBinding],
) -> Option<ConfiguredBinding> {
    let action = bindings
        .iter()
        .find(|binding| binding.key == key)?
        .action
        .as_str();
    Some(match action {
        "none" => ConfiguredBinding::Disabled,
        "detach" => ConfiguredBinding::Action(Action::Detach),
        "new-task" => ConfiguredBinding::Action(Action::ToggleOverlay),
        "observatory" => ConfiguredBinding::Action(Action::Observatory),
        "tasks" => ConfiguredBinding::Action(Action::Tasks),
        "agents" => ConfiguredBinding::Action(Action::Agents),
        "rename-tab" => ConfiguredBinding::Action(Action::Rename),
        "rename-workspace" => ConfiguredBinding::Action(Action::RenameSession),
        "menu" => ConfiguredBinding::Action(Action::Menu),
        "workspaces" => ConfiguredBinding::Action(Action::Sessions),
        "settings" => ConfiguredBinding::Action(Action::Settings),
        "projects" => ConfiguredBinding::Action(Action::Projects),
        "new-project" => ConfiguredBinding::Action(Action::NewProject),
        "close-workspace" => ConfiguredBinding::Action(Action::CloseWorkspace),
        "split-right" => ConfiguredBinding::Command(Command::Split(SplitAxis::LeftRight)),
        "split-down" => ConfiguredBinding::Command(Command::Split(SplitAxis::TopBottom)),
        "focus-left" => ConfiguredBinding::Command(Command::Focus(FocusDir::Left)),
        "focus-right" => ConfiguredBinding::Command(Command::Focus(FocusDir::Right)),
        "focus-up" => ConfiguredBinding::Command(Command::Focus(FocusDir::Up)),
        "focus-down" => ConfiguredBinding::Command(Command::Focus(FocusDir::Down)),
        "resize-left" => ConfiguredBinding::Command(Command::ResizePane(FocusDir::Left)),
        "resize-right" => ConfiguredBinding::Command(Command::ResizePane(FocusDir::Right)),
        "resize-up" => ConfiguredBinding::Command(Command::ResizePane(FocusDir::Up)),
        "resize-down" => ConfiguredBinding::Command(Command::ResizePane(FocusDir::Down)),
        "zoom" => ConfiguredBinding::Command(Command::ZoomToggle),
        "kill-pane" => ConfiguredBinding::Command(Command::KillPane),
        "new-tab" => ConfiguredBinding::Command(Command::NewWindow),
        "next-tab" => ConfiguredBinding::Command(Command::NextWindow),
        "previous-tab" => ConfiguredBinding::Command(Command::PrevWindow),
        "move-tab-left" => {
            ConfiguredBinding::Command(Command::MoveTab(uniterm_proto::TabMoveDirection::Previous))
        }
        "move-tab-right" => {
            ConfiguredBinding::Command(Command::MoveTab(uniterm_proto::TabMoveDirection::Next))
        }
        "kill-tab" => ConfiguredBinding::Command(Command::KillWindow),
        "overview" => ConfiguredBinding::Command(Command::Overview),
        "copy-mode" => ConfiguredBinding::Command(Command::CopyMode),
        "files" => ConfiguredBinding::Command(Command::FileSidebarToggle),
        "sidebar" => ConfiguredBinding::Command(Command::SidebarToggle),
        // Invalid action names never enter Config::bindings.
        _ => return None,
    })
}

/// What keyboard input did to an open menu.
pub(crate) enum MenuKeys {
    /// Nothing relevant.
    None,
    /// Selection moved within the same box; redraw it locally.
    Redraw,
    /// A different menu opened (different box size); needs a repaint under it.
    Switched,
    /// Close without running anything.
    Close,
    /// Run the selected item.
    Run,
}

/// Drive an open menu from raw key bytes: arrows/`jk` move the selection,
/// arrows/`hl` switch menus, Enter runs, Esc/`q`/Ctrl-C closes. The whole
/// chunk is processed (several keys can arrive in one read - a menu switch
/// must not drop the tail); only Run/Close short-circuit.
pub(crate) fn handle_menu_keys(chunk: &[u8], ms: &mut MenuState) -> MenuKeys {
    let mut switched = false;
    let mut changed = false;
    let mut i = 0;
    while i < chunk.len() {
        let b = chunk[i];
        if b == 0x1b {
            if chunk.get(i + 1) == Some(&b'[') {
                match chunk.get(i + 2) {
                    Some(b'A') => ms.prev_item(),
                    Some(b'B') => ms.next_item(),
                    Some(b'C') => {
                        ms.next_menu();
                        switched = true;
                    }
                    Some(b'D') => {
                        ms.prev_menu();
                        switched = true;
                    }
                    _ => return MenuKeys::Close,
                }
                changed = true;
                i += 3;
                continue;
            }
            return MenuKeys::Close; // lone Esc
        }
        match b {
            0x0d | 0x0a => return MenuKeys::Run,
            b'q' | 0x03 => return MenuKeys::Close,
            b'k' => {
                ms.prev_item();
                changed = true;
            }
            b'j' => {
                ms.next_item();
                changed = true;
            }
            b'h' => {
                ms.prev_menu();
                switched = true;
                changed = true;
            }
            b'l' => {
                ms.next_menu();
                switched = true;
                changed = true;
            }
            _ => {}
        }
        i += 1;
    }
    if switched {
        MenuKeys::Switched
    } else if changed {
        MenuKeys::Redraw
    } else {
        MenuKeys::None
    }
}

/// Map a post-prefix key byte to a built-in command, if bound.
pub(crate) fn command_for(key: u8) -> Option<Command> {
    Some(match key {
        b'%' => Command::Split(SplitAxis::LeftRight),
        b'"' => Command::Split(SplitAxis::TopBottom),
        b'h' => Command::Focus(FocusDir::Left),
        b'j' => Command::Focus(FocusDir::Down),
        b'k' => Command::Focus(FocusDir::Up),
        b'l' => Command::Focus(FocusDir::Right),
        b'z' => Command::ZoomToggle,
        b'x' => Command::KillPane,
        b'c' => Command::NewWindow,
        b'n' => Command::NextWindow,
        b'p' => Command::PrevWindow,
        // Windows are 1-indexed for humans: `1` is the first window and `0`
        // selects window 10. The wire index stays 0-based.
        b'1'..=b'9' => Command::SelectWindow(key - b'1'),
        b'0' => Command::SelectWindow(9),
        b'&' => Command::KillWindow,
        b'w' => Command::Overview,
        b'[' => Command::CopyMode,
        b'f' => Command::FileSidebarToggle,
        SIDEBAR_KEY => Command::SidebarToggle,
        // Shift-H/J/K/L: resize the active pane.
        b'H' => Command::ResizePane(FocusDir::Left),
        b'J' => Command::ResizePane(FocusDir::Down),
        b'K' => Command::ResizePane(FocusDir::Up),
        b'L' => Command::ResizePane(FocusDir::Right),
        b'<' => Command::MoveTab(uniterm_proto::TabMoveDirection::Previous),
        b'>' => Command::MoveTab(uniterm_proto::TabMoveDirection::Next),
        b';' => Command::LastPane,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::close_confirmation;
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
    fn plain_input_passes_through() {
        let mut pending = false;
        let mut out = Vec::new();
        assert_eq!(
            process_input(b"ls\n", PREFIX, false, &mut pending, &mut out),
            Action::None
        );
        let msgs = decode_inputs(&out);
        assert!(matches!(&msgs[0], ClientMessage::Input(v) if v == b"ls\n"));
    }

    #[test]
    fn prefix_then_d_detaches() {
        let mut pending = false;
        let mut out = Vec::new();
        assert_eq!(
            process_input(&[PREFIX, DETACH_KEY], PREFIX, false, &mut pending, &mut out),
            Action::Detach
        );
    }

    #[test]
    fn nested_input_gives_one_prefix_to_inner_and_two_to_outer() {
        let mut state = PrefixState {
            nested: true,
            ..PrefixState::default()
        };
        let mut out = Vec::new();
        let action = process_input_with_bindings(
            &[PREFIX, b'o'],
            PREFIX,
            false,
            false,
            &[],
            &mut state,
            &mut out,
        );
        assert_eq!(action, Action::None);
        assert!(matches!(
            decode_inputs(&out).as_slice(),
            [ClientMessage::Input(bytes)] if bytes == &[PREFIX, b'o']
        ));

        out.clear();
        let action = process_input_with_bindings(
            &[PREFIX, PREFIX, b'o'],
            PREFIX,
            false,
            false,
            &[],
            &mut state,
            &mut out,
        );
        assert_eq!(action, Action::Observatory);
        assert!(out.is_empty());
    }

    #[test]
    fn destructive_bindings_respect_confirm_close() {
        let mut pending = false;
        let mut out = Vec::new();
        assert_eq!(
            process_input(&[PREFIX, b'x'], PREFIX, true, &mut pending, &mut out),
            Action::Confirm(Command::KillPane)
        );
        assert!(out.is_empty());
        assert!(
            close_confirmation(&ClientMessage::Command(Command::KillPane))
                .render(80, 24)
                .windows("Confirm close".len())
                .any(|bytes| bytes == b"Confirm close")
        );
    }

    #[test]
    fn tab_close_confirmation_is_independently_configurable() {
        let mut state = PrefixState::default();
        let mut out = Vec::new();
        assert_eq!(
            process_input_with_bindings(
                &[PREFIX, b'&'],
                PREFIX,
                true,
                true,
                &[],
                &mut state,
                &mut out,
            ),
            Action::Confirm(Command::KillWindow)
        );
        assert!(out.is_empty());

        assert_eq!(
            process_input_with_bindings(
                &[PREFIX, b'&'],
                PREFIX,
                true,
                false,
                &[],
                &mut state,
                &mut out,
            ),
            Action::None
        );
        assert!(matches!(
            decode_inputs(&out).as_slice(),
            [ClientMessage::Command(Command::KillWindow)]
        ));
    }

    #[test]
    fn semantic_config_bindings_override_builtin_keys() {
        let config = uniterm_core::Config::parse("bind.x = move-tab-right\nbind.d = none\n");
        let mut state = PrefixState::default();
        let mut output = Vec::new();
        assert_eq!(
            process_input_with_bindings(
                &[PREFIX, b'x'],
                PREFIX,
                true,
                true,
                &config.bindings,
                &mut state,
                &mut output,
            ),
            Action::None
        );
        assert!(matches!(
            decode_inputs(&output).as_slice(),
            [ClientMessage::Command(Command::MoveTab(
                uniterm_proto::TabMoveDirection::Next
            ))]
        ));
        output.clear();
        assert_eq!(
            process_input_with_bindings(
                &[PREFIX, b'd'],
                PREFIX,
                false,
                false,
                &config.bindings,
                &mut state,
                &mut output,
            ),
            Action::None
        );
        assert!(output.is_empty());
    }

    #[test]
    fn new_project_shortcut_opens_modal_and_sidebar_shortcut_toggles() {
        let mut pending = false;
        let mut out = Vec::new();
        assert_eq!(
            process_input(
                &[PREFIX, NEW_PROJECT_KEY],
                PREFIX,
                false,
                &mut pending,
                &mut out,
            ),
            Action::NewProject
        );
        assert!(out.is_empty());

        assert_eq!(
            process_input(
                &[PREFIX, SIDEBAR_KEY],
                PREFIX,
                false,
                &mut pending,
                &mut out,
            ),
            Action::None
        );
        assert!(matches!(
            decode_inputs(&out).as_slice(),
            [ClientMessage::Command(Command::SidebarToggle)]
        ));
    }

    #[test]
    fn close_workspace_shortcut_is_client_side_and_explicit() {
        let mut pending = false;
        let mut out = Vec::new();
        assert_eq!(
            process_input(
                &[PREFIX, CLOSE_WORKSPACE_KEY],
                PREFIX,
                false,
                &mut pending,
                &mut out,
            ),
            Action::CloseWorkspace
        );
        assert!(out.is_empty());
    }

    #[test]
    fn prefix_bindings_do_not_collide() {
        let bindings = [
            (OVERLAY_KEY, "New Task"),
            (OBSERVATORY_KEY, "Observatory"),
            (TASKS_KEY, "Tasks"),
            (AGENTS_KEY, "Manage Agents"),
            (RENAME_KEY, "Rename tab"),
            (RENAME_SESSION_KEY, "Rename Workspace"),
            (MENU_KEY, "Menu"),
            (SESSIONS_KEY, "Manage Workspaces"),
            (SETTINGS_KEY, "Settings"),
            (PROJECTS_KEY, "Manage Projects"),
            (NEW_PROJECT_KEY, "New Project"),
            (CLOSE_WORKSPACE_KEY, "Close Workspace"),
            (DETACH_KEY, "Detach"),
            (b'%', "Split right"),
            (b'\"', "Split down"),
            (b'h', "Focus left"),
            (b'j', "Focus down"),
            (b'k', "Focus up"),
            (b'l', "Focus right"),
            (b'z', "Zoom"),
            (b'x', "Close pane"),
            (b'c', "New tab"),
            (b'n', "Next tab"),
            (b'p', "Previous tab"),
            (b'1', "Select tab 1"),
            (b'2', "Select tab 2"),
            (b'3', "Select tab 3"),
            (b'4', "Select tab 4"),
            (b'5', "Select tab 5"),
            (b'6', "Select tab 6"),
            (b'7', "Select tab 7"),
            (b'8', "Select tab 8"),
            (b'9', "Select tab 9"),
            (b'0', "Select tab 10"),
            (b'&', "Close tab"),
            (b'w', "Overview"),
            (b'[', "Copy mode"),
            (b'f', "File manager"),
            (SIDEBAR_KEY, "Sidebar"),
            (b'H', "Resize left"),
            (b'J', "Resize down"),
            (b'K', "Resize up"),
            (b'L', "Resize right"),
            (b'<', "Move tab previous"),
            (b'>', "Move tab next"),
        ];
        let mut seen = [None; 256];
        for (key, label) in bindings {
            assert_eq!(
                seen[key as usize].replace(label),
                None,
                "prefix key {key:?} is shared by {label}"
            );
        }
    }

    #[test]
    fn mouse_scanner_extracts_events_and_keeps_keys() {
        // Hover (motion, b=35=32+3), then the letter 'x', then a left click.
        let (evs, pass, left) = scan_mouse(b"\x1b[<35;10;5Mx\x1b[<0;7;3M");
        assert_eq!(pass, b"x");
        assert!(left.is_empty());
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0], (10, 5, MouseKind::Hover));
        assert_eq!(evs[1], (7, 3, MouseKind::Click));
    }

    #[test]
    fn mouse_scanner_parses_wheel_and_release() {
        let (evs, pass, left) = scan_mouse(b"\x1b[<64;4;7M\x1b[<65;4;8M\x1b[<0;1;1m");
        assert!(pass.is_empty());
        assert!(left.is_empty());
        assert_eq!(
            evs,
            vec![
                (4, 7, MouseKind::WheelUp),
                (4, 8, MouseKind::WheelDown),
                (1, 1, MouseKind::Release),
            ]
        );
        // A right press opens Uniterm's context menu; its release is consumed.
        let (evs, _, _) = scan_mouse(b"\x1b[<2;1;1M\x1b[<2;1;1m");
        assert_eq!(evs, vec![(1, 1, MouseKind::RightClick)]);
        // Motion with the left button held (b=32) is a drag; motion with a
        // non-left button held (b=34) is dropped.
        let (evs, _, _) = scan_mouse(b"\x1b[<32;9;9M\x1b[<34;9;9M");
        assert_eq!(evs, vec![(9, 9, MouseKind::Drag)]);
    }

    #[test]
    fn mouse_scanner_buffers_partial_sequence() {
        // A sequence split mid-way is returned as leftover to prepend next read.
        let (evs, pass, left) = scan_mouse(b"ab\x1b[<0;7");
        assert_eq!(pass, b"ab");
        assert!(evs.is_empty());
        assert_eq!(left, b"\x1b[<0;7");
        // Prepending the rest completes it.
        let mut whole = left;
        whole.extend_from_slice(b";3M");
        let (evs, _, _) = scan_mouse(&whole);
        assert_eq!(evs, vec![(7, 3, MouseKind::Click)]);
    }

    #[test]
    fn stdin_scanner_reassembles_mouse_reports_split_at_every_byte() {
        let report = b"\x1b[<35;120;40M";
        for split in 1..report.len() {
            let mut mouse_leftover = Vec::new();
            let mut key_pending = Vec::new();
            let (first_events, first_pass) =
                scan_stdin_chunk(&report[..split], &mut mouse_leftover, &mut key_pending);
            assert!(first_events.is_empty(), "split {split}");
            assert!(first_pass.is_empty(), "split {split}");

            let (events, pass) =
                scan_stdin_chunk(&report[split..], &mut mouse_leftover, &mut key_pending);
            assert_eq!(events, vec![(120, 40, MouseKind::Hover)], "split {split}");
            assert!(pass.is_empty(), "split {split}");
            assert!(mouse_leftover.is_empty(), "split {split}");
            assert!(key_pending.is_empty(), "split {split}");
        }
    }

    #[test]
    fn focus_reports_are_consumed_and_focus_in_requests_repaint() {
        let (focused, bytes) = strip_focus_events(b"a\x1b[Ib\x1b[Oc");
        // The last report wins: this chunk ends unfocused.
        assert_eq!(focused, Some(false));
        assert_eq!(bytes, b"abc");
        let (focused, bytes) = strip_focus_events(b"\x1b[Ox\x1b[I");
        assert_eq!(focused, Some(true));
        assert_eq!(bytes, b"x");
        let (focused, bytes) = strip_focus_events(b"plain");
        assert_eq!(focused, None);
        assert_eq!(bytes, b"plain");
    }

    #[test]
    fn mouse_scanner_passes_arrow_keys_through() {
        // ESC[A (up arrow) is not a mouse report; it must reach the pane.
        let (evs, pass, left) = scan_mouse(b"\x1b[A");
        assert!(evs.is_empty());
        assert_eq!(pass, b"\x1b[A");
        assert!(left.is_empty());
    }

    #[test]
    fn prefix_then_p_toggles_overlay() {
        let mut pending = false;
        let mut out = Vec::new();
        assert_eq!(
            process_input(
                &[PREFIX, OVERLAY_KEY],
                PREFIX,
                false,
                &mut pending,
                &mut out,
            ),
            Action::ToggleOverlay
        );
        // prefix+p is PrevWindow again (it used to be shadowed by New Task).
        let mut out2 = Vec::new();
        assert_eq!(
            process_input(&[PREFIX, b'p'], PREFIX, false, &mut pending, &mut out2),
            Action::None
        );
        let msgs = decode_inputs(&out2);
        assert!(matches!(
            &msgs[0],
            ClientMessage::Command(Command::PrevWindow)
        ));
        // Nothing was sent to the server; the overlay is purely client-side.
        assert!(out.is_empty());
    }

    #[test]
    fn double_prefix_sends_literal() {
        let mut pending = false;
        let mut out = Vec::new();
        assert_eq!(
            process_input(&[PREFIX, PREFIX], PREFIX, false, &mut pending, &mut out),
            Action::None
        );
        let msgs = decode_inputs(&out);
        assert!(matches!(&msgs[0], ClientMessage::Input(v) if v == &[PREFIX]));
    }

    #[test]
    fn prefix_spans_reads() {
        // Prefix at the end of one read, 'd' at the start of the next.
        let mut pending = false;
        let mut out = Vec::new();
        assert_eq!(
            process_input(&[PREFIX], PREFIX, false, &mut pending, &mut out),
            Action::None
        );
        assert!(pending);
        assert_eq!(
            process_input(&[DETACH_KEY], PREFIX, false, &mut pending, &mut out),
            Action::Detach
        );
    }
}
