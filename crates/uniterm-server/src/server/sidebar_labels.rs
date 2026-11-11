//! A finite label reveal, armed by pointer attention and never by idle time.
//! Each attachment owns its animation so peers keep their own static chrome.

use super::*;
use std::time::Instant;

pub(super) struct Reveal {
    tab: ObservatoryTab,
    rect: Rect,
    label: &'static str,
    animation: chrome::TabLabelReveal,
}

impl Reveal {
    fn new(tab: ObservatoryTab, rect: Rect, label: &'static str, now: Instant) -> Option<Self> {
        Some(Self {
            tab,
            rect,
            label,
            animation: chrome::TabLabelReveal::new(label, usize::from(rect.w), now)?,
        })
    }

    fn ops(&self, selected: ObservatoryTab, theme: &uniterm_core::Theme) -> Vec<u8> {
        // DEC save/restore also protects the rendition and any hidden cursor.
        format!(
            "\x1b7\x1b[{};{}H{}{}\x1b8",
            self.rect.y + 1,
            self.rect.x + 1,
            chrome::tab_strip_style(theme, self.tab == selected),
            self.animation.text()
        )
        .into_bytes()
    }
}

impl Server {
    fn sidebar_reveal_valid(&self, token: Token, reveal: &Reveal) -> bool {
        self.context_menu.is_none()
            && self.overview.is_none()
            && self.clients.get(&token).is_some_and(|client| {
                client.attached && !client.dead && !client.overlay && client.direct.is_none()
            })
            && self
                .observatory_tab_slots()
                .into_iter()
                .any(|(tab, rect, label)| {
                    tab == reveal.tab && rect == reveal.rect && label == reveal.label
                })
    }

    pub(super) fn sidebar_label_deadline(&self) -> Option<Instant> {
        self.sidebar_label_reveals
            .values()
            .filter_map(|reveal| reveal.animation.deadline())
            .min()
    }

    /// A repaint includes the current label window in the same supersedable frame.
    pub(super) fn sidebar_label_frames(&self, base: &[u8]) -> HashMap<Token, Vec<u8>> {
        self.sidebar_label_reveals
            .iter()
            .filter_map(|(&token, reveal)| {
                if reveal.animation.offset().is_none() || !self.sidebar_reveal_valid(token, reveal)
                {
                    return None;
                }
                let mut frame = base.to_vec();
                frame.extend(reveal.ops(self.observatory_tab, &self.config.theme));
                Some((token, encode_frame(&ServerMessage::RenderOps(frame))))
            })
            .collect()
    }

    pub(super) fn cancel_sidebar_label(&mut self, reg: &Registry, token: Token, restore: bool) {
        if let Some(mut reveal) = self.sidebar_label_reveals.remove(&token) {
            if restore
                && reveal.animation.offset().is_some()
                && self.sidebar_reveal_valid(token, &reveal)
            {
                reveal.animation.finish();
                self.send_sidebar_label(reg, token, &reveal);
            }
        }
    }

    fn send_sidebar_label(&mut self, reg: &Registry, token: Token, reveal: &Reveal) {
        let encoded = encode_frame(&ServerMessage::RenderOps(
            reveal.ops(self.observatory_tab, &self.config.theme),
        ));
        if let Some(client) = self.clients.get_mut(&token) {
            client.queue_render(&encoded);
            client.flush();
            let _ = set_interest(reg, client, token);
        }
    }

    /// Pointer movement within a slot does not restart a completed reveal.
    pub(super) fn hover_sidebar_label(&mut self, reg: &Registry, token: Token, x: u16, y: u16) {
        let target = self
            .observatory_tab_slots()
            .into_iter()
            .find(|(_, rect, _)| rect.contains(x, y));
        if self
            .sidebar_label_reveals
            .get(&token)
            .is_some_and(|reveal| {
                target.is_some_and(|(tab, rect, _)| tab == reveal.tab && rect == reveal.rect)
                    && self.sidebar_reveal_valid(token, reveal)
            })
        {
            return;
        }
        self.cancel_sidebar_label(reg, token, true);
        if let Some((tab, rect, label)) = target {
            if let Some(reveal) = Reveal::new(tab, rect, label, Instant::now()) {
                if self.sidebar_reveal_valid(token, &reveal) {
                    self.sidebar_label_reveals.insert(token, reveal);
                }
            }
        }
    }

    pub(super) fn flush_sidebar_labels(&mut self, reg: &Registry) {
        self.flush_sidebar_labels_at(reg, Instant::now());
    }

    fn flush_sidebar_labels_at(&mut self, reg: &Registry, now: Instant) {
        if self.sidebar_label_reveals.is_empty() {
            return;
        }
        let tokens: Vec<_> = self.sidebar_label_reveals.keys().copied().collect();
        for token in tokens {
            let Some(mut reveal) = self.sidebar_label_reveals.remove(&token) else {
                continue;
            };
            if !self.sidebar_reveal_valid(token, &reveal) {
                continue;
            }
            if reveal.animation.advance(now) {
                self.send_sidebar_label(reg, token, &reveal);
            }
            self.sidebar_label_reveals.insert(token, reveal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::reaper::common;
    use super::*;
    use std::io::Read;
    use std::time::Duration;

    fn drain(peer: &mut std::os::unix::net::UnixStream, screen: &mut Terminal) -> usize {
        let mut decoder = FrameDecoder::new();
        let mut count = 0;
        let mut bytes = [0; 65536];
        loop {
            match peer.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => decoder.push(&bytes[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("client read: {error}"),
            }
        }
        while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            if let ServerMessage::RenderOps(ops) = message {
                count += ops.len();
                screen.feed(&ops);
            }
        }
        count
    }

    fn attach(
        server: &mut Server,
        poll: &Poll,
    ) -> (Token, std::os::unix::net::UnixStream, Terminal) {
        let mut peer = std::os::unix::net::UnixStream::connect(&server.sock_path).unwrap();
        peer.set_nonblocking(true).unwrap();
        server.on_accept(poll.registry());
        let token = *server.clients.keys().max_by_key(|token| token.0).unwrap();
        server.handle_msg(
            poll.registry(),
            token,
            ClientMessage::Attach {
                term: "xterm-256color".into(),
                cols: 140,
                rows: 30,
            },
            &mut false,
        );
        let mut screen = Terminal::new(140, 30);
        drain(&mut peer, &mut screen);
        (token, peer, screen)
    }

    #[test]
    fn label_tick_preserves_renderer_cache_and_next_pty_damage_is_one_byte() {
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
        server.config.file_sidebar = true;
        server.config.file_sidebar_width = 36;
        server.overwatch.enabled = true;
        server.relayout();
        let (token, mut peer, mut screen) = attach(&mut server, &poll);
        let pane = server.input_pane();
        let pane_rect = server.visible_pane_rect(pane).unwrap();
        let terminal = &mut server.panes.get_mut(&pane).unwrap().term;
        terminal.grid_mut().clear_damage();
        terminal.feed(b"\x1b[31mA");
        server.broadcast_pane_damage(poll.registry(), pane);
        assert!(drain(&mut peer, &mut screen) < 80);

        let (_, rect, _) = server.observatory_tab_slots()[3];
        server.hover_sidebar_label(poll.registry(), token, rect.x, rect.y);
        let due = server.sidebar_label_deadline().unwrap();
        server.flush_sidebar_labels_at(poll.registry(), due);
        assert!(drain(&mut peer, &mut screen) < 150);
        server.panes.get_mut(&pane).unwrap().term.feed(b"B");
        server.broadcast_pane_damage(poll.registry(), pane);
        assert_eq!(
            drain(&mut peer, &mut screen),
            1,
            "the label preserved the exact PTY cursor and SGR caches"
        );
        assert_eq!(screen.grid().get(pane_rect.x, pane_rect.y).ch, 'A');
        assert_eq!(screen.grid().get(pane_rect.x + 1, pane_rect.y).ch, 'B');
        assert_eq!(
            screen.grid().get(pane_rect.x + 1, pane_rect.y).fg,
            Color::Idx(1)
        );
        server.broadcast_pane_damage(poll.registry(), pane);
        assert_eq!(drain(&mut peer, &mut screen), 0);
    }

    #[test]
    fn sidebar_label_reveal_is_attachment_local_padded_finite_and_cancellable() {
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
        server.config.file_sidebar = true;
        server.config.file_sidebar_width = 36;
        server.config.focus_follows_mouse = false;
        server.overwatch.enabled = true;
        server.relayout();
        let ordinary = server.input_pane();
        let (token, mut peer, mut screen) = attach(&mut server, &poll);
        let (_, mut other_peer, mut other_screen) = attach(&mut server, &poll);
        drain(&mut peer, &mut screen);
        let (_, rect, _) = server.observatory_tab_slots()[3];
        let label = |screen: &Terminal| {
            (rect.x..rect.right())
                .map(|x| screen.grid().get(x, rect.y).ch)
                .collect::<String>()
        };
        assert_eq!(label(&screen), " Overw… ");
        assert!(server.sidebar_label_deadline().is_none());
        let initial: Vec<_> = (0..30)
            .flat_map(|y| (0..140).map(move |x| (x, y)))
            .map(|(x, y)| screen.grid().get(x, y))
            .collect();
        let cursor = screen.cursor();
        server.on_mouse(
            poll.registry(),
            token,
            rect.x + 1,
            rect.y + 1,
            MouseKind::Hover,
        );
        assert_eq!(server.input_pane(), ordinary);
        assert_eq!(server.observatory_tab, ObservatoryTab::Agents);
        assert_eq!(drain(&mut peer, &mut screen), 0);
        let mut steps = 0;
        while let Some(due) = server.sidebar_label_deadline() {
            server.flush_sidebar_labels_at(poll.registry(), due);
            assert!(drain(&mut peer, &mut screen) < 150, "one label only");
            assert_eq!(drain(&mut other_peer, &mut other_screen), 0);
            assert_eq!(screen.cursor(), cursor);
            assert!(label(&screen).starts_with(' ') && label(&screen).ends_with(' '));
            for y in 0..30 {
                for x in 0..140 {
                    if !rect.contains(x, y) {
                        assert_eq!(
                            screen.grid().get(x, y),
                            initial[usize::from(y) * 140 + usize::from(x)]
                        );
                    }
                }
            }
            if steps == 0 {
                assert_eq!(label(&screen), " Overwa ");
                // A normal chrome update must preserve this attachment's window.
                server.repaint_chrome_all(poll.registry());
                drain(&mut peer, &mut screen);
                drain(&mut other_peer, &mut other_screen);
                assert_eq!(label(&screen), " Overwa ");
                assert_eq!(label(&other_screen), " Overw… ");
            }
            steps += 1;
            assert!(steps < 10);
        }
        assert_eq!(label(&screen), " Overw… ");
        server.on_mouse(
            poll.registry(),
            token,
            rect.x + 2,
            rect.y + 1,
            MouseKind::Hover,
        );
        assert!(
            server.sidebar_label_deadline().is_none(),
            "stationary attention cannot loop"
        );
        server.flush_sidebar_labels_at(poll.registry(), Instant::now() + Duration::from_secs(3600));
        assert_eq!(drain(&mut peer, &mut screen), 0);

        for cancel in [
            ClientMessage::FocusLost,
            ClientMessage::OverlayVisible { on: true },
            ClientMessage::Resize {
                cols: 140,
                rows: 30,
            },
        ] {
            server.cancel_sidebar_label(poll.registry(), token, true);
            server.on_mouse(
                poll.registry(),
                token,
                rect.x + 1,
                rect.y + 1,
                MouseKind::Hover,
            );
            let due = server.sidebar_label_deadline().unwrap();
            server.flush_sidebar_labels_at(poll.registry(), due);
            drain(&mut peer, &mut screen);
            server.handle_msg(poll.registry(), token, cancel, &mut false);
            assert!(server.sidebar_label_deadline().is_none());
            server.handle_msg(
                poll.registry(),
                token,
                ClientMessage::OverlayVisible { on: false },
                &mut false,
            );
            drain(&mut peer, &mut screen);
            assert_eq!(label(&screen), " Overw… ");
        }
        server.on_mouse(
            poll.registry(),
            token,
            rect.x + 1,
            rect.y + 1,
            MouseKind::Hover,
        );
        server.on_mouse(poll.registry(), token, 1, 5, MouseKind::Hover);
        assert!(server.sidebar_label_deadline().is_none());
        server.on_mouse(
            poll.registry(),
            token,
            rect.x + 1,
            rect.y + 1,
            MouseKind::Hover,
        );
        // A slow transport receives one superseding repaint, not a frame queue
        // that grows once per step. Its eventual full frame includes the window.
        server.clients.get_mut(&token).unwrap().render_end = Some(1);
        let due = server.sidebar_label_deadline().unwrap();
        server.flush_sidebar_labels_at(poll.registry(), due);
        assert!(server.clients[&token].repaint_pending);
        assert!(server.clients[&token].outbuf.is_empty());
        {
            let client = server.clients.get_mut(&token).unwrap();
            client.render_end = None;
            client.repaint_pending = false;
        }
        server.full_repaint_client(poll.registry(), token);
        drain(&mut peer, &mut screen);
        assert_eq!(label(&screen), " Overwa ");
        server.handle_msg(poll.registry(), token, ClientMessage::Detach, &mut false);
        assert!(server.sidebar_label_deadline().is_none());
        server.on_mouse(
            poll.registry(),
            token,
            rect.x + 1,
            rect.y + 1,
            MouseKind::Hover,
        );
        server.config.file_sidebar = false;
        server.relayout();
        server.flush_sidebar_labels(poll.registry());
        assert!(server.sidebar_label_deadline().is_none());

        if let Some(root) = std::env::var_os("UNITERM_SELECTOR_FIXTURES") {
            let root = PathBuf::from(root);
            std::fs::create_dir_all(&root).unwrap();
            server.config.file_sidebar = true;
            for width in [22, 36, 52] {
                for enabled in [false, true] {
                    server.overwatch.enabled = enabled;
                    server.config.file_sidebar_width = width;
                    server.relayout();
                    let frame = server.build_full_frame();
                    let mut actual = Terminal::new(140, 30);
                    actual.feed(&frame);
                    let name =
                        format!("sidebar-{width}-{}", if enabled { "four" } else { "three" });
                    std::fs::write(root.join(format!("{name}.ansi")), frame).unwrap();
                    std::fs::write(root.join(format!("{name}.txt")), actual.dump_text()).unwrap();
                }
            }
        }
    }
}
