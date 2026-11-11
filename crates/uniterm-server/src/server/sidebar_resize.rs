//! Sidebar dividers use the same captured-pointer interaction as central splits.
//! Width changes share Settings semantics; only the final gesture saves config.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Side {
    Projects,
    Observatory,
}

pub(super) struct Drag {
    owner: Token,
    side: Side,
    changed: bool,
}

impl Server {
    /// Shared geometry invalidation ends whichever attachment owns capture.
    pub(super) fn finish_any_sidebar_drag(&mut self) {
        if let Some(drag) = &self.sidebar_drag {
            self.finish_sidebar_drag(drag.owner);
        }
    }

    /// The existing painted one-cell borders, excluding the main tab bar.
    pub(super) fn sidebar_dividers(&self) -> impl Iterator<Item = (Side, uniterm_core::Divider)> {
        let area = self.sidebar_area();
        let left = self.sidebar_width();
        let right = self.right_panel_layout(0).area;
        [
            chrome::sidebar_divider(Rect::new(0, area.y, left, area.h), false)
                .map(|divider| (Side::Projects, divider)),
            chrome::sidebar_divider(right, true).map(|divider| (Side::Observatory, divider)),
        ]
        .into_iter()
        .flatten()
    }

    fn sidebar_drag_client_valid(&self, owner: Token) -> bool {
        self.context_menu.is_none()
            && self.clients.get(&owner).is_some_and(|client| {
                client.attached && !client.dead && !client.overlay && client.direct.is_none()
            })
    }

    /// A cancelled capture keeps its last visible size and saves it once.
    pub(super) fn finish_sidebar_drag(&mut self, token: Token) {
        if self
            .sidebar_drag
            .as_ref()
            .is_none_or(|drag| drag.owner != token)
        {
            return;
        }
        let drag = self.sidebar_drag.take().unwrap();
        if drag.changed {
            self.agents.send(uniterm_proto::CoreToAgent::ConfigSave {
                client: token.0 as u64,
                text: self.config.to_text(),
            });
        }
    }

    /// One semantic width path for Settings and mouse preview. PTYs relayout
    /// only when the visible geometry changes; unchanged drags produce no work.
    pub(super) fn apply_sidebar_width_settings(
        &mut self,
        reg: &Registry,
        token: Token,
        left: Option<u16>,
        right: Option<u16>,
        save: bool,
    ) -> bool {
        let old = (self.config.sidebar_width, self.config.file_sidebar_width);
        let old_geometry = (self.sidebar_width(), self.observatory_width());
        if let Some(width) = left {
            self.config.sidebar_width = width.clamp(16, 40);
        }
        if let Some(width) = right {
            self.config.file_sidebar_width = width.clamp(22, 52);
        }
        let changed = old != (self.config.sidebar_width, self.config.file_sidebar_width);
        if old_geometry != (self.sidebar_width(), self.observatory_width()) {
            self.relayout();
            self.full_repaint_all(reg);
        }
        if changed {
            let clients: Vec<_> = self
                .clients
                .iter()
                .filter(|(_, client)| client.attached && client.direct.is_none())
                .map(|(token, _)| *token)
                .collect();
            for client in clients {
                self.reply_settings(reg, client, false, None);
            }
        }
        if save {
            self.agents.send(uniterm_proto::CoreToAgent::ConfigSave {
                client: token.0 as u64,
                text: self.config.to_text(),
            });
        }
        changed
    }

    fn resize_sidebar_at(&mut self, reg: &Registry, owner: Token, side: Side, x: u16) -> bool {
        let (width, current) = match side {
            Side::Projects => {
                let reserved = if self.observatory_width() > 0 { 62 } else { 48 };
                let maximum = 40.min(self.cols.saturating_sub(reserved)).max(16);
                (x.saturating_add(1).clamp(16, maximum), self.sidebar_width())
            }
            Side::Observatory => {
                let maximum = 52
                    .min(
                        self.cols
                            .saturating_sub(self.sidebar_width())
                            .saturating_sub(40),
                    )
                    .max(22);
                (
                    self.cols.saturating_sub(x).clamp(22, maximum),
                    self.observatory_width(),
                )
            }
        };
        if width == current {
            return false;
        }
        self.apply_sidebar_width_settings(
            reg,
            owner,
            (side == Side::Projects).then_some(width),
            (side == Side::Observatory).then_some(width),
            false,
        )
    }

    /// Capture before routing into either rail or the center so movement and
    /// release can cross every content boundary without becoming app input.
    pub(super) fn handle_sidebar_resize(
        &mut self,
        reg: &Registry,
        token: Token,
        x: u16,
        y: u16,
        kind: MouseKind,
    ) -> bool {
        if let Some(drag) = &self.sidebar_drag {
            let owner = drag.owner;
            let side = drag.side;
            if !self.sidebar_drag_client_valid(owner)
                || !self
                    .sidebar_dividers()
                    .any(|(candidate, _)| candidate == side)
            {
                self.finish_sidebar_drag(owner);
            } else if owner == token {
                match kind {
                    MouseKind::Drag | MouseKind::Release => {
                        let changed = self.resize_sidebar_at(reg, owner, side, x);
                        if let Some(drag) = &mut self.sidebar_drag {
                            drag.changed |= changed;
                        }
                        if kind == MouseKind::Release {
                            self.finish_sidebar_drag(owner);
                        }
                        return true;
                    }
                    MouseKind::Hover | MouseKind::Click | MouseKind::RightClick => {
                        self.finish_sidebar_drag(owner)
                    }
                    _ => return true,
                }
            } else if matches!(
                kind,
                MouseKind::Drag | MouseKind::Release | MouseKind::Click
            ) {
                return true;
            }
        }
        if kind != MouseKind::Click || !self.sidebar_drag_client_valid(token) {
            return false;
        }
        let target = self
            .sidebar_dividers()
            .find(|(_, divider)| divider.rect.contains(x, y));
        let Some((side, _)) = target else {
            return false;
        };
        self.cancel_sidebar_label(reg, token, true);
        self.tab_drag = None;
        self.divider_drag = None;
        self.scrollbar_drag = None;
        self.mouse_sel = None;
        self.sidebar_drag = Some(Drag {
            owner: token,
            side,
            changed: false,
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::reaper::common;
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::time::Instant;
    use uniterm_proto::CoreToAgent;

    fn fixture() -> (Server, Poll) {
        common::isolate_state();
        let name = common::unique_workspace_name();
        let root = common::socket_root().join(&name);
        std::fs::create_dir_all(&root).unwrap();
        let (mut server, poll) = Server::bind_internal(
            &root.join(format!("{name}.sock")),
            "/bin/sh",
            &[],
            140,
            30,
            false,
        )
        .unwrap();
        server.config.sidebar = true;
        server.config.sidebar_width = 24;
        server.config.file_sidebar = true;
        server.config.file_sidebar_width = 36;
        server.relayout();
        (server, poll)
    }

    fn attach(server: &mut Server, poll: &Poll) -> (Token, UnixStream) {
        let mut peer = UnixStream::connect(&server.sock_path).unwrap();
        peer.set_nonblocking(true).unwrap();
        server.on_accept(poll.registry());
        let token = *server.clients.keys().max_by_key(|token| token.0).unwrap();
        server.handle_msg(
            poll.registry(),
            token,
            ClientMessage::Attach {
                term: "xterm".into(),
                cols: server.cols,
                rows: server.rows,
            },
            &mut false,
        );
        drain(&mut peer);
        (token, peer)
    }

    fn drain(peer: &mut UnixStream) -> Vec<ServerMessage> {
        let mut decoder = FrameDecoder::new();
        let mut bytes = [0; 65536];
        loop {
            match peer.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => decoder.push(&bytes[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("client read: {error}"),
            }
        }
        let mut messages = Vec::new();
        while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            messages.push(message);
        }
        messages
    }

    fn border(server: &Server, side: Side) -> Rect {
        server
            .sidebar_dividers()
            .find(|(candidate, _)| *candidate == side)
            .unwrap()
            .1
            .rect
    }

    fn mouse(server: &mut Server, poll: &Poll, token: Token, x: u16, y: u16, kind: MouseKind) {
        server.on_mouse(poll.registry(), token, x + 1, y + 1, kind);
    }

    fn saves(requests: &crossbeam_channel::Receiver<CoreToAgent>) -> Vec<(u64, String)> {
        requests
            .try_iter()
            .filter_map(|request| match request {
                CoreToAgent::ConfigSave { client, text } => Some((client, text)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sidebar_resize_captures_owner_crosses_content_saves_once_and_is_idle_when_unchanged() {
        let (mut server, mut poll) = fixture();
        let requests = server.agents.capture_requests();
        let (owner, mut peer) = attach(&mut server, &poll);
        let (other, mut other_peer) = attach(&mut server, &poll);
        drain(&mut peer);
        requests.try_iter().for_each(drop);
        let active = server.input_pane();
        let rect = border(&server, Side::Projects);
        mouse(
            &mut server,
            &poll,
            owner,
            rect.x,
            rect.y + 4,
            MouseKind::Click,
        );
        let started = Instant::now();
        for _ in 0..10_000 {
            mouse(&mut server, &poll, owner, rect.x, 0, MouseKind::Drag);
        }
        eprintln!(
            "10,000 unchanged sidebar drags: {:?}, zero output and requests",
            started.elapsed()
        );
        assert!(drain(&mut peer).is_empty());
        assert!(drain(&mut other_peer).is_empty());
        assert!(requests.try_recv().is_err());
        assert!(server.sidebar_label_deadline().is_none());
        assert_eq!(server.input_pane(), active);
        mouse(&mut server, &poll, other, 70, 2, MouseKind::Drag);
        mouse(&mut server, &poll, other, 70, 2, MouseKind::Release);
        assert_eq!(server.sidebar_width(), 24);
        assert_eq!(server.sidebar_drag.as_ref().unwrap().owner, owner);
        mouse(&mut server, &poll, owner, 125, 0, MouseKind::Drag);
        assert_eq!(server.sidebar_width(), 40);
        assert!(
            saves(&requests).is_empty(),
            "preview cannot save on every move"
        );
        mouse(&mut server, &poll, owner, 31, 29, MouseKind::Release);
        assert_eq!(server.sidebar_width(), 32);
        assert!(server.sidebar_drag.is_none());
        let saved = saves(&requests);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].0, owner.0 as u64);
        assert_eq!(uniterm_core::Config::parse(&saved[0].1).sidebar_width, 32);
        assert_eq!(server.input_pane(), active);
        assert!(server.panes[&active].copy.is_none());
        drain(&mut peer);
        drain(&mut other_peer);

        let rect = border(&server, Side::Observatory);
        mouse(
            &mut server,
            &poll,
            owner,
            rect.x,
            rect.y + 4,
            MouseKind::Click,
        );
        mouse(&mut server, &poll, owner, 0, 0, MouseKind::Drag);
        assert_eq!(server.observatory_width(), 52);
        mouse(&mut server, &poll, owner, 139, 29, MouseKind::Release);
        assert_eq!(server.observatory_width(), 22);
        assert_eq!(saves(&requests).len(), 1);
        assert_eq!(server.input_pane(), active);
        let actual = drain(&mut peer);
        assert!(actual
            .iter()
            .any(|message| matches!(message, ServerMessage::RenderOps(_))));
        assert!(drain(&mut other_peer)
            .iter()
            .any(|message| matches!(message, ServerMessage::RenderOps(_))));
        server.handle_msg(
            poll.registry(),
            owner,
            ClientMessage::SettingsApply(uniterm_proto::SettingsPatch {
                sidebar_width: Some(100),
                file_sidebar_width: Some(1),
                ..Default::default()
            }),
            &mut false,
        );
        assert_eq!(
            (server.sidebar_width(), server.observatory_width()),
            (40, 22)
        );
        assert_eq!(saves(&requests).len(), 1);
        drain(&mut peer);
        server.handle_msg(
            poll.registry(),
            owner,
            ClientMessage::SettingsApply(uniterm_proto::SettingsPatch {
                sidebar_width: Some(40),
                file_sidebar_width: Some(22),
                ..Default::default()
            }),
            &mut false,
        );
        assert!(
            drain(&mut peer).is_empty(),
            "unchanged Settings geometry also avoids repaint"
        );
        assert_eq!(
            saves(&requests).len(),
            1,
            "explicit Settings still persists its request"
        );
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn sidebar_resize_cancellation_and_capped_noop_preserve_preferences() {
        let (mut server, mut poll) = fixture();
        let requests = server.agents.capture_requests();
        let (owner, mut peer) = attach(&mut server, &poll);
        for message in [
            ClientMessage::FocusLost,
            ClientMessage::OverlayVisible { on: true },
            ClientMessage::Resize {
                cols: 140,
                rows: 30,
            },
            ClientMessage::Input(vec![]),
        ] {
            let rect = border(&server, Side::Projects);
            mouse(
                &mut server,
                &poll,
                owner,
                rect.x,
                rect.y + 4,
                MouseKind::Click,
            );
            let x = if rect.x == 27 { 23 } else { 27 };
            mouse(&mut server, &poll, owner, x, rect.y + 4, MouseKind::Drag);
            server.handle_msg(poll.registry(), owner, message, &mut false);
            assert!(server.sidebar_drag.is_none());
            assert_eq!(saves(&requests).len(), 1);
            if server.clients[&owner].overlay {
                assert!(!server.handle_sidebar_resize(
                    poll.registry(),
                    owner,
                    rect.x,
                    rect.y + 4,
                    MouseKind::Click
                ));
            }
            server.handle_msg(
                poll.registry(),
                owner,
                ClientMessage::OverlayVisible { on: false },
                &mut false,
            );
            drain(&mut peer);
        }
        let (other, mut other_peer) = attach(&mut server, &poll);
        let rect = border(&server, Side::Projects);
        mouse(
            &mut server,
            &poll,
            owner,
            rect.x,
            rect.y + 4,
            MouseKind::Click,
        );
        mouse(&mut server, &poll, owner, 30, rect.y + 4, MouseKind::Drag);
        server.handle_msg(
            poll.registry(),
            other,
            ClientMessage::Resize {
                cols: 100,
                rows: 24,
            },
            &mut false,
        );
        assert!(
            server.sidebar_drag.is_none(),
            "a peer resize invalidates captured coordinates"
        );
        assert_eq!(saves(&requests).len(), 1);
        server.handle_msg(poll.registry(), other, ClientMessage::Detach, &mut false);
        drain(&mut other_peer);
        let active = server.input_pane();
        server.clients.get_mut(&owner).unwrap().direct = Some(DirectAttachment {
            pane: active,
            role: PaneAttachRole::Observer,
            last_cursor_visible: None,
        });
        let rect = border(&server, Side::Projects);
        assert!(!server.handle_sidebar_resize(
            poll.registry(),
            owner,
            rect.x,
            rect.y + 4,
            MouseKind::Click
        ));
        server.clients.get_mut(&owner).unwrap().direct = None;
        server.cols = 88;
        server.config.sidebar_width = 40;
        server.config.file_sidebar_width = 52;
        server.relayout();
        server.full_repaint_all(poll.registry());
        drain(&mut peer);
        for side in [Side::Projects, Side::Observatory] {
            let rect = border(&server, side);
            mouse(
                &mut server,
                &poll,
                owner,
                rect.x,
                rect.y + 4,
                MouseKind::Click,
            );
            mouse(
                &mut server,
                &poll,
                owner,
                rect.x,
                rect.y + 4,
                MouseKind::Release,
            );
            assert!(server.sidebar_drag.is_none());
            assert!(saves(&requests).is_empty());
            assert!(drain(&mut peer).is_empty());
            assert_eq!(
                (
                    server.config.sidebar_width,
                    server.config.file_sidebar_width
                ),
                (40, 52)
            );
        }
        let rect = border(&server, Side::Projects);
        mouse(
            &mut server,
            &poll,
            owner,
            rect.x,
            rect.y + 4,
            MouseKind::Click,
        );
        mouse(&mut server, &poll, owner, 16, rect.y + 4, MouseKind::Drag);
        server.handle_msg(poll.registry(), owner, ClientMessage::Detach, &mut false);
        assert!(server.sidebar_drag.is_none());
        assert_eq!(saves(&requests).len(), 1);
        assert!(!server.handle_sidebar_resize(
            poll.registry(),
            owner,
            rect.x,
            rect.y,
            MouseKind::Click
        ));
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn sidebar_resize_shared_borders_keep_narrow_center_today_and_overwatch_usable() {
        let (mut server, mut poll) = fixture();
        let requests = server.agents.capture_requests();
        for cols in [60, 72, 88, 96, 140] {
            for status in [StatusPosition::Top, StatusPosition::Bottom] {
                server.cols = cols;
                server.config.status_position = status;
                server.config.sidebar_width = 40;
                server.config.file_sidebar_width = 52;
                server.relayout();
                let mut screen = Terminal::new(cols, server.rows);
                screen.feed(&server.build_full_frame());
                for (_, divider) in server.sidebar_dividers() {
                    for y in divider.rect.y..divider.rect.bottom() {
                        assert_eq!(
                            screen.grid().get(divider.rect.x, y).ch,
                            '│',
                            "{cols} columns, y={y}"
                        );
                    }
                    assert!(!divider.rect.contains(
                        divider.rect.x,
                        if status == StatusPosition::Top { 0 } else { 29 }
                    ));
                }
                assert!(server.content_area().0.w >= if cols >= 88 { 40 } else { 48 });
                assert_eq!(
                    server.sidebar_dividers().count(),
                    if cols < 72 {
                        0
                    } else if cols < 88 {
                        1
                    } else {
                        2
                    }
                );
                if cols == 88 || cols == 140 {
                    let root = Path::new("/tmp/uniterm-resize-fixtures");
                    std::fs::create_dir_all(root).unwrap();
                    std::fs::write(
                        root.join(format!("resized-{cols}-{status:?}.txt")),
                        screen.dump_text(),
                    )
                    .unwrap();
                    std::fs::write(
                        root.join(format!("resized-{cols}-{status:?}.ansi")),
                        server.build_full_frame(),
                    )
                    .unwrap();
                }
            }
        }
        server.config.status_position = StatusPosition::Top;
        server.rows = 7;
        assert_eq!(
            server.sidebar_dividers().count(),
            0,
            "short terminals hide both borders"
        );
        server.rows = 30;
        server.config.sidebar_width = 24;
        server.config.file_sidebar_width = 36;
        server.relayout();
        let (owner, mut peer) = attach(&mut server, &poll);
        let active = server.input_pane();
        let project = server.active_project;
        let pane = server
            .spawn_pane_at(
                poll.registry(),
                &["-c", "exec cat"],
                Some(Path::new("/tmp")),
            )
            .unwrap();
        server.panes.get_mut(&pane).unwrap().metadata.insert(
            "uniterm.overwatch".into(),
            MetadataValue {
                value: "owned".into(),
                expires: None,
            },
        );
        server.overwatch = uniterm_core::overwatch::State {
            enabled: true,
            pane: Some(pane),
            project: Some(project),
            ..Default::default()
        };
        server.observatory_tab = ObservatoryTab::Overwatch;
        server.open_today(poll.registry());
        drain(&mut peer);
        requests.try_iter().for_each(drop);
        let started = Instant::now();
        for (side, x) in [(Side::Projects, 31), (Side::Observatory, 88)] {
            let rect = border(&server, side);
            mouse(
                &mut server,
                &poll,
                owner,
                rect.x,
                rect.y + 4,
                MouseKind::Click,
            );
            mouse(&mut server, &poll, owner, x, rect.y + 4, MouseKind::Release);
            drain(&mut peer);
            let center = server.chrome_area().0;
            let today = server.today.as_ref().unwrap();
            assert_eq!(
                (today.grid.width(), today.grid.height()),
                (center.w, center.h)
            );
            let chat = server.overwatch_rect().unwrap();
            assert_eq!(
                (
                    server.panes[&pane].term.grid().width(),
                    server.panes[&pane].term.grid().height()
                ),
                (chat.w, chat.h)
            );
            assert_eq!(server.input_pane(), active);
            assert_eq!(server.active_project, project);
            assert_eq!(saves(&requests).len(), 1);
        }
        eprintln!(
            "Two changed sidebar drags with Today and Overwatch render: {:?}",
            started.elapsed()
        );
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .term
            .feed(b"RESIZED_CHAT");
        let mut screen = Terminal::new(server.cols, server.rows);
        let frame = server.build_full_frame();
        screen.feed(&frame);
        assert!(screen.dump_text().contains("RESIZED_CHAT"));
        std::fs::write("/tmp/uniterm-resize-fixtures/today-overwatch.ansi", frame).unwrap();
        std::fs::write(
            "/tmp/uniterm-resize-fixtures/today-overwatch.txt",
            screen.dump_text(),
        )
        .unwrap();
        server.retire_all_ptys(&mut poll);
    }
}
