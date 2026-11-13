//! Coalesce continuing scrolling output, without delaying ordinary edits or
//! arming a clock while there is no visible damage. See docs/04-multiplexer-core.md.

use super::*;
use std::time::{Duration, Instant};

// At most eight milliseconds of scrolling damage accumulates after a paint.
// The first paint is immediate; later chunks cannot postpone the tail forever.
const SCROLL_FRAME_INTERVAL: Duration = Duration::from_millis(8);

/// Keep burst cadence local to each pane so one producer cannot postpone
/// another pane's paint, without creating a recurring idle timer.
#[derive(Default)]
pub(super) struct OutputRenderSchedule {
    last_paint: Option<Instant>,
    deadline: Option<Instant>,
}

impl OutputRenderSchedule {
    fn defer_scroll(&mut self, now: Instant) -> bool {
        if let Some(due) = self.last_paint.map(|last| last + SCROLL_FRAME_INTERVAL) {
            if now < due {
                self.deadline = Some(due);
                return true;
            }
        }
        false
    }

    /// Start the next cadence interval only after a paint consumes pending work.
    pub(super) fn painted(&mut self, now: Instant) {
        self.last_paint = Some(now);
        self.deadline = None;
    }

    /// Let the next input response paint immediately while retaining an older
    /// tail's deadline in case the application never responds.
    pub(super) fn prioritize_input(&mut self) {
        self.last_paint = None;
    }
}

impl Server {
    /// Disarm work already consumed by an incremental or full repaint so a
    /// clean pane cannot keep the event loop awake.
    pub(super) fn cancel_pane_render(&mut self, pane: PaneId) {
        self.pending_pane_renders.retain(|id| *id != pane);
        if let Some(pane) = self.panes.get_mut(&pane) {
            pane.output_render.deadline = None;
        }
    }

    /// Wake only for queued damage; historical paint timestamps never arm poll.
    pub(super) fn pane_render_deadline(&self) -> Option<Instant> {
        self.pending_pane_renders
            .iter()
            .filter_map(|id| self.panes.get(id)?.output_render.deadline)
            .min()
    }

    fn pane_output_observed(&self, pane_id: PaneId) -> bool {
        let workspace_visible = self.visible_pane_rect(pane_id).is_some()
            && self.context_menu.is_none()
            && ((self.today.is_none() && self.overview.is_none())
                || self.overwatch_chat_pane() == Some(pane_id))
            && self
                .panes
                .get(&pane_id)
                .is_some_and(|pane| pane.copy.is_none());
        self.clients.values().any(|client| {
            !client.dead
                && (client
                    .direct
                    .as_ref()
                    .is_some_and(|direct| direct.pane == pane_id)
                    || (workspace_visible
                        && client.attached
                        && client.direct.is_none()
                        && !client.overlay))
        })
    }

    /// Coalesce continuing visible scrolls while keeping ordinary edits and
    /// input responses immediate; hidden output needs no rendering deadline.
    pub(super) fn schedule_pane_damage(
        &mut self,
        reg: &Registry,
        pane_id: PaneId,
        scrolled: bool,
        now: Instant,
    ) {
        if scrolled && self.pane_output_observed(pane_id) {
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                let pending = pane.output_render.deadline.is_some();
                if pane.term.grid().is_dirty() && pane.output_render.defer_scroll(now) {
                    if !pending {
                        self.pending_pane_renders.push(pane_id);
                    }
                    return;
                }
            }
        }
        // Also cancels an older deferred scroll when a non-scrolling update
        // arrives, so cursor-addressed progress, input echo and mode changes
        // are delivered with all preceding cell damage in order.
        self.broadcast_pane_damage(reg, pane_id);
    }

    /// Service every due pane fairly, drop unobserved work, and allow shutdown
    /// to deliver all remaining tails before sending the final exit notice.
    pub(super) fn flush_pane_renders_due(&mut self, reg: &Registry, now: Instant, force: bool) {
        let mut index = 0;
        while index < self.pending_pane_renders.len() {
            let pane = self.pending_pane_renders[index];
            let deadline = self
                .panes
                .get(&pane)
                .and_then(|pane| pane.output_render.deadline);
            if deadline.is_none() {
                self.pending_pane_renders.swap_remove(index);
            } else if force
                || !self.pane_output_observed(pane)
                || deadline.is_some_and(|deadline| now >= deadline)
            {
                // broadcast removes this entry. Revisit the same index so all
                // due panes progress, without allocating a list every frame.
                self.broadcast_pane_damage(reg, pane);
            } else {
                index += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{reaper::common, test_transport};
    use super::*;
    use std::os::unix::net::UnixStream as Peer;

    fn fixture() -> (Server, Poll, Token, Peer) {
        fixture_with_args(&[])
    }

    fn fixture_with_args(args: &[&str]) -> (Server, Poll, Token, Peer) {
        common::isolate_state();
        let socket = common::temp_dir("scroll-cadence")
            .join(format!("{}.sock", common::unique_workspace_name()));
        let (mut server, poll) =
            Server::bind_internal(&socket, "/bin/sh", args, 160, 50, false).unwrap();
        let mut peer = Peer::connect(socket).unwrap();
        peer.set_nonblocking(true).unwrap();
        server.on_accept(poll.registry());
        let token = *server.clients.keys().next().unwrap();
        test_transport::small_send_buffer(&server, token);
        server.handle_msg(
            poll.registry(),
            token,
            ClientMessage::Attach {
                term: "xterm-256color".into(),
                cols: 160,
                rows: 50,
            },
            &mut false,
        );
        test_transport::drain(&mut server, poll.registry(), token, &mut peer);
        (server, poll, token, peer)
    }

    fn scroll(server: &mut Server, pane: PaneId) {
        let term = &mut server.panes.get_mut(&pane).unwrap().term;
        let height = term.grid().height();
        term.feed(format!("\x1b[{height};1Hnext\r\n").as_bytes());
    }

    fn defer(server: &mut Server, reg: &Registry, pane: PaneId) -> Instant {
        let now = Instant::now();
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .output_render
            .painted(now);
        scroll(server, pane);
        server.schedule_pane_damage(reg, pane, true, now + Duration::from_millis(1));
        server.panes[&pane].output_render.deadline.unwrap()
    }

    #[test]
    fn first_scroll_is_immediate_and_non_scroll_or_keyboard_response_flushes_the_tail() {
        let (mut server, poll, token, mut peer) = fixture();
        let pane = server.input_pane();
        scroll(&mut server, pane);
        server.schedule_pane_damage(poll.registry(), pane, true, Instant::now());
        assert!(server.pane_render_deadline().is_none());
        assert!(!test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());

        defer(&mut server, poll.registry(), pane);
        assert!(test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .term
            .feed(b"\x1b[1;1Hedit");
        server.schedule_pane_damage(poll.registry(), pane, false, Instant::now());
        assert!(server.pane_render_deadline().is_none());
        assert!(!test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());

        defer(&mut server, poll.registry(), pane);
        assert!(Server::queue_pane_input(
            poll.registry(),
            server.panes.get_mut(&pane).unwrap(),
            b"x"
        ));
        scroll(&mut server, pane);
        server.schedule_pane_damage(poll.registry(), pane, true, Instant::now());
        assert!(server.pane_render_deadline().is_none());
        assert!(!test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());
        assert!(!server.panes[&pane].term.grid().is_dirty());
    }

    #[test]
    fn due_panes_are_all_serviced_and_a_full_repaint_cancels_covered_work() {
        let (mut server, poll, token, mut peer) = fixture();
        let first = server.input_pane();
        let second = server
            .split_pane(poll.registry(), first, SplitAxis::TopBottom, true)
            .unwrap();
        test_transport::drain(&mut server, poll.registry(), token, &mut peer);
        let first_due = defer(&mut server, poll.registry(), first);
        let second_due = defer(&mut server, poll.registry(), second);
        assert_eq!(server.pending_pane_renders.len(), 2);
        // More output cannot slide either pane's deadline or duplicate its entry.
        scroll(&mut server, first);
        server.schedule_pane_damage(
            poll.registry(),
            first,
            true,
            first_due - Duration::from_millis(1),
        );
        assert_eq!(server.pending_pane_renders.len(), 2);
        assert_eq!(server.panes[&first].output_render.deadline, Some(first_due));
        server.flush_pane_renders_due(poll.registry(), first_due.max(second_due), false);
        assert!(server.pending_pane_renders.is_empty());
        for pane in [first, second] {
            assert!(!server.panes[&pane].term.grid().is_dirty());
        }
        assert!(!test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());
        server.flush_pane_renders_due(poll.registry(), first_due + Duration::from_secs(1), false);
        assert!(test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());
        defer(&mut server, poll.registry(), first);
        server.full_repaint_all(poll.registry());
        assert!(server.pane_render_deadline().is_none());
    }

    #[test]
    fn hidden_and_detached_damage_has_no_timer_and_forced_flush_preserves_the_tail() {
        let (mut server, poll, token, mut peer) = fixture();
        let pane = server.input_pane();
        let due = defer(&mut server, poll.registry(), pane);
        // The shutdown path must flush before the cadence deadline.
        server.flush_pane_renders_due(poll.registry(), due - Duration::from_millis(1), true);
        assert!(server.pane_render_deadline().is_none());
        assert!(!test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());

        defer(&mut server, poll.registry(), pane);
        server.current_layout.panes.clear();
        server.flush_pane_renders_due(poll.registry(), Instant::now(), false);
        assert!(server.pane_render_deadline().is_none());
        assert!(!server.panes[&pane].term.grid().is_dirty());
        scroll(&mut server, pane);
        server.schedule_pane_damage(poll.registry(), pane, true, Instant::now());
        assert!(server.pane_render_deadline().is_none());
        assert!(test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());

        server.relayout();
        defer(&mut server, poll.registry(), pane);
        server.clients.get_mut(&token).unwrap().attached = false;
        server.flush_pane_renders_due(poll.registry(), Instant::now(), false);
        assert!(server.pane_render_deadline().is_none());
        scroll(&mut server, pane);
        server.schedule_pane_damage(poll.registry(), pane, true, Instant::now());
        assert!(server.pane_render_deadline().is_none());
        assert!(!server.panes[&pane].term.grid().is_dirty());
    }

    #[test]
    fn scrolling_deadline_is_fixed_and_only_armed_by_pending_work() {
        let now = Instant::now();
        let mut schedule = OutputRenderSchedule::default();
        assert!(!schedule.defer_scroll(now));
        assert!(schedule.deadline.is_none());
        schedule.painted(now);
        assert!(schedule.deadline.is_none());
        assert!(schedule.defer_scroll(now + Duration::from_millis(1)));
        assert!(schedule.defer_scroll(now + Duration::from_millis(7)));
        assert_eq!(schedule.deadline, Some(now + SCROLL_FRAME_INTERVAL));
        assert!(!schedule.defer_scroll(now + SCROLL_FRAME_INTERVAL));
        schedule.painted(now + SCROLL_FRAME_INTERVAL);
        assert!(schedule.deadline.is_none());
    }

    #[test]
    fn eof_without_new_bytes_delivers_the_previously_deferred_tail() {
        // This child emits nothing: on_pty must flush the preceding batch's
        // damage even when the EOF readiness itself contains no new output.
        let (mut server, mut poll, token, mut peer) = fixture_with_args(&["-c", "exit 0"]);
        let pane = server.input_pane();
        // Reap only this fixture's child. Registering a fresh exit watch after
        // it has already exited is racy on kqueue; this bounded test wait also
        // works when the child finishes before the attachment is established.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !server.panes.get_mut(&pane).unwrap().pty.try_wait_exited() {
            assert!(Instant::now() < deadline, "fixture child failed to exit");
            std::thread::sleep(Duration::from_millis(1));
        }
        defer(&mut server, poll.registry(), pane);
        assert!(test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());
        let pty_token = server.panes[&pane].token;
        // A concurrent test spawn can briefly inherit a slave descriptor before
        // exec closes it. Reaping our child alone therefore does not establish
        // EOF. Follow actual kernel readiness, without flushing the render timer.
        let mut events = Events::with_capacity(16);
        let eof_deadline = Instant::now() + Duration::from_secs(5);
        while server.panes.contains_key(&pane) {
            let remaining = eof_deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "fixture PTY never reached EOF");
            match poll.poll(&mut events, Some(remaining)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("fixture poll: {error}"),
            }
            for event in &events {
                if event.token() == pty_token
                    && (event.is_readable() || event.is_read_closed() || event.is_error())
                {
                    assert!(!server.agents.backpressured());
                    server.on_pty(poll.registry(), pty_token, true, event.is_writable());
                }
            }
        }
        assert!(server.pane_render_deadline().is_none());
        assert!(server.pending_pane_renders.is_empty());
        let bytes = test_transport::drain(&mut server, poll.registry(), token, &mut peer);
        let mut decoder = FrameDecoder::new();
        decoder.push(&bytes);
        let mut tail_delivered = false;
        while let Some(message) = decoder.decode::<ServerMessage>().unwrap() {
            if let ServerMessage::RenderOps(ops) = message {
                tail_delivered |= ops.windows(4).any(|run| run == b"next");
            }
        }
        assert!(tail_delivered, "EOF discarded pending scrolling damage");
    }

    #[test]
    fn direct_observer_receives_hidden_pane_damage_and_then_stays_quiet() {
        let (mut server, poll, token, mut peer) = fixture();
        let pane = server.input_pane();
        server.handle_msg(
            poll.registry(),
            token,
            ClientMessage::PaneAttach {
                pane,
                role: uniterm_proto::PaneAttachRole::Observer,
            },
            &mut false,
        );
        test_transport::drain(&mut server, poll.registry(), token, &mut peer);
        server.current_layout.panes.clear();
        let due = defer(&mut server, poll.registry(), pane);
        server.flush_pane_renders_due(poll.registry(), due, false);
        assert!(server.pane_render_deadline().is_none());
        assert!(!test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());
        server.flush_pane_renders_due(poll.registry(), due + Duration::from_secs(1), false);
        assert!(test_transport::drain(&mut server, poll.registry(), token, &mut peer).is_empty());
    }

    #[test]
    fn input_prioritizes_the_next_response_without_losing_an_existing_tail() {
        let now = Instant::now();
        let mut schedule = OutputRenderSchedule::default();
        schedule.painted(now);
        assert!(schedule.defer_scroll(now + Duration::from_millis(1)));
        schedule.prioritize_input();
        assert_eq!(schedule.deadline, Some(now + SCROLL_FRAME_INTERVAL));
        assert!(!schedule.defer_scroll(now + Duration::from_millis(2)));
        schedule.painted(now + Duration::from_millis(2));
        assert!(schedule.deadline.is_none());
    }
}
