//! Effective host palette projection. Filesystem work belongs to the runtime.

use super::*;
use uniterm_core::Theme;

#[derive(Default)]
pub(super) struct State {
    pub(super) available: bool,
    pub(super) generation: u64,
    pub(super) manual_theme: Option<Theme>,
    pub(super) observed_theme: Option<Theme>,
    pub(super) error: Option<String>,
    watching: Option<bool>,
}

impl Server {
    pub(super) fn configure_theme_watcher(&mut self) {
        let enabled = self.config.follow_omarchy_theme;
        if Some(enabled) == self.omarchy.watching {
            return;
        }
        self.omarchy.watching = Some(enabled);
        self.omarchy.generation = self.omarchy.generation.wrapping_add(1);
        self.omarchy.error = None;
        if !enabled {
            // A later enable must establish its own last-valid observation.
            // Config reloads cannot revive a palette from a disabled period.
            self.omarchy.observed_theme = None;
        }
        self.agents
            .send(uniterm_proto::CoreToAgent::OmarchyThemeWatch {
                enabled,
                generation: self.omarchy.generation,
            });
    }

    pub(super) fn set_theme_following(&mut self, reg: &Registry, enabled: bool) {
        if self.config.follow_omarchy_theme == enabled {
            return;
        }
        self.config.follow_omarchy_theme = enabled;
        self.configure_theme_watcher();
        if !enabled {
            let manual = self.omarchy.manual_theme.unwrap_or_else(|| {
                self.config
                    .apply_theme_overrides(Theme::named(self.config.theme_base_preset.name()))
            });
            self.apply_effective_theme(reg, manual);
        }
        self.broadcast_theme_settings(reg);
    }

    pub(super) fn receive_omarchy_theme(
        &mut self,
        reg: &Registry,
        generation: u64,
        theme: Option<Theme>,
        error: Option<String>,
    ) {
        if !self.config.follow_omarchy_theme
            || !self.omarchy.available
            || generation != self.omarchy.generation
        {
            return;
        }
        let error_changed = self.omarchy.error != error;
        self.omarchy.error = error;
        if let Some(theme) = theme {
            self.omarchy.observed_theme = Some(theme);
            let theme = self.config.apply_theme_overrides(theme);
            self.apply_effective_theme(reg, theme);
        }
        if error_changed {
            self.broadcast_theme_settings(reg);
        }
    }

    /// OS updates never relayout, resize a PTY, save config, or redraw unchanged colors.
    pub(super) fn apply_effective_theme(&mut self, reg: &Registry, theme: Theme) {
        if self.config.theme == theme {
            return;
        }
        self.config.theme = theme;
        for pane in self.panes.values_mut() {
            pane.term
                .set_default_colors(theme.foreground, theme.background);
            let response = pane.term.take_responses();
            if !response.is_empty() {
                Self::queue_pane_input(reg, pane, &response);
            }
        }
        let frame = encode_frame(&ServerMessage::ThemeChanged { theme });
        for (token, client) in &mut self.clients {
            if client.attached && client.direct.is_none() {
                client.queue(&frame);
                client.flush();
                let _ = set_interest(reg, client, *token);
            }
        }
        // No client means there is nothing to paint. First attach uses the
        // cached palette and Settings snapshot, without a background frame.
        if !self
            .clients
            .values()
            .any(|client| client.attached && client.direct.is_none())
        {
            return;
        }
        self.paint_today(reg);
        if self.overview.is_some() {
            // The overview contains theme-colored miniature surfaces too.
            self.full_repaint_all(reg);
        } else {
            self.repaint_chrome_all(reg);
        }
    }

    pub(super) fn broadcast_theme_settings(&mut self, reg: &Registry) {
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
}

#[cfg(test)]
mod tests {
    use super::super::{reaper::common, test_transport};
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    fn fixture() -> (Server, Poll) {
        common::isolate_state();
        let name = common::unique_workspace_name();
        let root = common::socket_root().join(&name);
        std::fs::create_dir_all(&root).unwrap();
        Server::bind_internal(
            &root.join(format!("{name}.sock")),
            "/bin/cat",
            &[],
            120,
            30,
            false,
        )
        .unwrap()
    }

    fn attach(server: &mut Server, poll: &Poll) -> (Token, UnixStream, Vec<ServerMessage>) {
        let mut peer = UnixStream::connect(&server.sock_path).unwrap();
        peer.set_nonblocking(true).unwrap();
        server.on_accept(poll.registry());
        let token = *server.clients.keys().max_by_key(|token| token.0).unwrap();
        server.handle_msg(
            poll.registry(),
            token,
            ClientMessage::Attach {
                term: "xterm-256color".into(),
                cols: 120,
                rows: 30,
            },
            &mut false,
        );
        let messages = decode(&test_transport::drain(
            server,
            poll.registry(),
            token,
            &mut peer,
        ));
        (token, peer, messages)
    }

    fn decode(bytes: &[u8]) -> Vec<ServerMessage> {
        let mut decoder = FrameDecoder::new();
        decoder.push(bytes);
        let mut messages = Vec::new();
        while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            messages.push(message);
        }
        messages
    }

    #[test]
    fn omarchy_updates_are_generation_scoped_idle_safe_and_preserve_manual_config() {
        let (mut server, mut poll) = fixture();
        let requests = server.agents.capture_requests();
        let manual =
            Config::parse("theme = nord\nstatus-bg = #010203\nfollow-omarchy-theme = true\n");
        let manual_text = manual.to_text();
        server.set_config(manual.clone());
        server.omarchy.available = true;
        let generation = server.omarchy.generation;
        requests.try_iter().for_each(drop);
        server.receive_omarchy_theme(poll.registry(), generation, Some(Theme::light()), None);
        let effective = manual.apply_theme_overrides(Theme::light());
        assert_eq!(server.config.theme, effective);
        assert_eq!(server.config.to_text(), manual_text);
        assert!(
            requests.is_empty(),
            "an OS update must not save config or request file listings"
        );
        let (token, mut peer, messages) = attach(&mut server, &poll);
        assert!(messages.iter().any(|message| matches!(message, ServerMessage::Settings { settings, .. } if settings.effective_theme == effective && settings.follow_omarchy_theme)));
        requests.try_iter().for_each(drop);
        let dimensions: Vec<_> = server
            .panes
            .values()
            .map(|pane| (pane.term.grid().width(), pane.term.grid().height()))
            .collect();
        server.receive_omarchy_theme(poll.registry(), generation, Some(Theme::light()), None);
        assert!(test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());
        assert!(requests.is_empty());
        server.receive_omarchy_theme(
            poll.registry(),
            generation.wrapping_sub(1),
            Some(Theme::dark()),
            Some("old".into()),
        );
        assert!(test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());
        assert_eq!(server.config.theme, effective);
        server.set_config(manual.clone());
        assert_eq!(
            server.config.theme, effective,
            "config reload must retain the observed palette"
        );
        requests.try_iter().for_each(drop);
        let invalid = uniterm_proto::SettingsPatch {
            follow_omarchy_theme: Some(false),
            tab_label_alignment: Some("invalid".into()),
            ..Default::default()
        };
        server.apply_settings(poll.registry(), token, invalid);
        assert!(server.config.follow_omarchy_theme);
        assert_eq!(server.omarchy.generation, generation);
        assert_eq!(server.config.theme, effective);
        assert!(requests.is_empty());
        test_transport::drain(&mut server, poll.registry(), token, &mut peer);
        server.receive_omarchy_theme(
            poll.registry(),
            generation,
            None,
            Some("bad palette".into()),
        );
        assert_eq!(server.config.theme, effective);
        test_transport::drain(&mut server, poll.registry(), token, &mut peer);
        server.set_theme_following(poll.registry(), false);
        assert_eq!(server.config.theme, manual.theme);
        server.set_theme_following(poll.registry(), true);
        test_transport::drain(&mut server, poll.registry(), token, &mut peer);
        server.receive_omarchy_theme(poll.registry(), generation, Some(Theme::dark()), None);
        assert_eq!(server.config.theme, manual.theme);
        assert!(test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());
        assert!(server.omarchy.observed_theme.is_none());
        let generation = server.omarchy.generation;
        server.receive_omarchy_theme(
            poll.registry(),
            generation,
            None,
            Some("new source is invalid".into()),
        );
        server.set_config(manual.clone());
        assert_eq!(
            server.config.theme, manual.theme,
            "reload must not revive an observation from before following was disabled"
        );
        server.receive_omarchy_theme(poll.registry(), generation, Some(Theme::dark()), None);
        assert_eq!(
            server.config.theme,
            manual.apply_theme_overrides(Theme::dark())
        );
        assert_eq!(
            dimensions,
            server
                .panes
                .values()
                .map(|pane| (pane.term.grid().width(), pane.term.grid().height()))
                .collect::<Vec<_>>()
        );
        server.omarchy.available = false;
        let before = server.config.to_text();
        server.apply_settings(
            poll.registry(),
            token,
            uniterm_proto::SettingsPatch {
                follow_omarchy_theme: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(
            server.config.to_text(),
            before,
            "ineligible host refuses the setting"
        );
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    #[ignore = "explicit release-mode 24-Pane input-to-render performance qualification"]
    fn omarchy_24_pane_input_and_theme_switch_latency() {
        let (mut server, mut poll) = fixture();
        let requests = server.agents.capture_requests();
        server.config.auto_tab_names = false;
        for _ in 1..24 {
            let pane = server.spawn_pane(poll.registry(), &[]).unwrap();
            server.push_window(pane);
        }
        server.relayout();
        let (token, mut peer, _) = attach(&mut server, &poll);
        let mut screen = crate::Terminal::new(120, 30);
        let mut timings = [Vec::new(), Vec::new()];
        let mut events = Events::with_capacity(128);
        for (mode, samples) in timings.iter_mut().enumerate() {
            for sample in 0..210 {
                let marker = format!("latency-{mode}-{sample:04}");
                let start = Instant::now();
                if mode == 1 {
                    server.apply_effective_theme(
                        poll.registry(),
                        if sample % 2 == 0 {
                            Theme::light()
                        } else {
                            Theme::dark()
                        },
                    );
                }
                server.handle_msg(
                    poll.registry(),
                    token,
                    ClientMessage::Input(format!("{marker}\n").into_bytes()),
                    &mut false,
                );
                loop {
                    match poll.poll(&mut events, Some(Duration::from_millis(20))) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => panic!("{error}"),
                    }
                    for event in events.iter() {
                        if server.pane_tokens.contains_key(&event.token()) {
                            server.on_pty(
                                poll.registry(),
                                event.token(),
                                event.is_readable(),
                                event.is_writable(),
                            );
                        }
                    }
                    server.flush_detection_due(poll.registry());
                    server.flush_pane_renders_due(poll.registry(), Instant::now(), false);
                    let bytes =
                        test_transport::drain(&mut server, poll.registry(), token, &mut peer);
                    for message in decode(&bytes) {
                        if let ServerMessage::RenderOps(ops) = message {
                            screen.feed(&ops);
                        }
                    }
                    if screen.dump_text().contains(&marker) {
                        break;
                    }
                    assert!(
                        start.elapsed() < Duration::from_secs(2),
                        "input marker missing"
                    );
                }
                if sample >= 10 {
                    samples.push(start.elapsed());
                }
                requests.try_iter().for_each(drop);
            }
            samples.sort();
        }
        let baseline = timings[0][190];
        let switching = timings[1][190];
        eprintln!(
            "24-Pane input-to-render p95: manual={baseline:?}, changing palette={switching:?}"
        );
        assert!(switching < Duration::from_millis(50));
        assert!(
            switching <= baseline + Duration::from_millis(5),
            "theme application added material input latency"
        );
        server.retire_all_ptys(&mut poll);
    }
}
