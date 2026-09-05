//! Readiness-driven socket and PTY I/O for the core loop.
//!
//! Accepting clients, draining PTY and client reads within their budgets,
//! queueing pane input, and tearing panes and clients down all live here; the
//! loop itself stays in `server.rs`.

use super::*;

/// What one output batch leaves for the runtime's provider rules to read:
/// the live bottom rows and window title, plus who owns the foreground.
struct ScreenEvidence {
    foreground_pid: Option<i32>,
    process_changed: bool,
    tail: String,
    title: String,
    bound_agent: Option<String>,
}

impl Server {
    /// Preserve an edge-triggered readable notification until runtime work
    /// drains. Writable sockets and the renderer continue making progress.
    fn runtime_read_ready(&mut self, token: Token, readable: bool) -> bool {
        if readable && self.agents.backpressured() {
            if !self.pending_reads.contains(&token) {
                self.pending_reads.push(token);
            }
            false
        } else {
            readable
        }
    }
    /// Queue bytes for a pane's PTY, flushing immediately when the fd is
    /// writable. Returns false when the pane's pending-input queue is full:
    /// dropping input silently would corrupt any send-then-wait script, so
    /// the caller must surface the rejection instead of acknowledging it.
    pub(super) fn queue_pane_input(reg: &Registry, pane: &mut Pane, bytes: &[u8]) -> bool {
        let pending = pane.input.len().saturating_sub(pane.input_offset);
        if !pane_input_has_capacity(pending, bytes.len()) {
            return false;
        }
        if pane.input_offset != 0 {
            pane.input.drain(..pane.input_offset);
            pane.input_offset = 0;
        }
        pane.input.extend_from_slice(bytes);
        Self::flush_pane_input(reg, pane);
        true
    }

    pub(super) fn flush_pane_input(reg: &Registry, pane: &mut Pane) {
        while pane.input_offset < pane.input.len() {
            match pane.pty.write_some(&pane.input[pane.input_offset..]) {
                Ok(0) => break,
                Ok(n) => {
                    pane.input_offset += n;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    pane.input.clear();
                    pane.input_offset = 0;
                    break;
                }
            }
        }
        if pane.input_offset == pane.input.len() {
            pane.input.clear();
            pane.input_offset = 0;
        }
        let interest = if pane.input.is_empty() {
            Interest::READABLE
        } else {
            Interest::READABLE | Interest::WRITABLE
        };
        let _ = reg.reregister(&mut SourceFd(&pane.pty.raw_fd()), pane.token, interest);
    }

    /// Accept one bounded batch. Returning true keeps the event loop active
    /// until a possibly edge-triggered listener backlog has been fully drained.
    pub(super) fn on_accept(&mut self, reg: &Registry) -> bool {
        let mut accepted = 0;
        while accepted < MAX_ACCEPTS_PER_EVENT {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    accepted += 1;
                    if self.clients.len() >= MAX_CLIENTS {
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                    let token = Token(self.next_token);
                    self.next_token += 1;
                    if reg
                        .register(&mut stream, token, Interest::READABLE)
                        .is_err()
                    {
                        continue;
                    }
                    self.clients.insert(
                        token,
                        Client {
                            stream,
                            decoder: FrameDecoder::with_max_frame(MAX_CLIENT_FRAME),
                            renderer: Renderer::new(),
                            outbuf: Vec::new(),
                            out_offset: 0,
                            render_end: None,
                            attached: false,
                            direct_only: false,
                            direct: None,
                            overlay: false,
                            cols: 0,
                            rows: 0,
                            dead: false,
                            repaint_pending: false,
                            exit_notified: false,
                            final_frame: None,
                            write_interest: false,
                            pending_wait: None,
                        },
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return false,
                Err(_) => return false,
            }
        }
        true
    }

    /// Re-read every source whose budget ran out last iteration, in arrival
    /// order, before blocking again. A source may re-queue itself, but the
    /// other ready sources and rendering always get a turn in between, and
    /// the loop still blocks with no timeout once every queue is empty.
    pub(super) fn service_pending_reads(&mut self, reg: &Registry) {
        if self.agents.backpressured() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_reads);
        for token in pending {
            if self.pane_tokens.contains_key(&token) {
                self.on_pty(reg, token, true, false);
            } else if self.clients.contains_key(&token) {
                self.on_client(reg, token, true, false);
            }
        }
    }

    pub(super) fn on_pty(&mut self, reg: &Registry, token: Token, readable: bool, writable: bool) {
        let readable = self.runtime_read_ready(token, readable);
        let Some(&pane_id) = self.pane_tokens.get(&token) else {
            return;
        };
        let mut buf = [0u8; 65536];
        let mut got_output = false;
        let mut cwd_changed = false;
        let mut eof = false;
        let mut read_budget_spent = false;
        let mut agent_changed = false;
        let mut agent_transitions = Vec::new();
        let mut cooperative_ready = false;
        let mut invocation_ended = false;
        let mut invocation_changed = None;
        let mut log_events: Vec<crate::eventlog::LogEvent> = Vec::new();
        let mut clipboard_ops = Vec::new();
        let mut nested_input_changed = false;
        let mut evidence: Option<ScreenEvidence> = None;
        let mut dev_server_evidence: Option<String> = None;
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            if writable {
                Self::flush_pane_input(reg, pane);
            }
            if readable {
                let mut read = 0usize;
                loop {
                    if read >= PTY_IO_BUDGET {
                        read_budget_spent = true;
                        break;
                    }
                    match pane.pty.read(&mut buf) {
                        Ok(0) => {
                            eof = true;
                            break;
                        }
                        Ok(n) => {
                            read += n;
                            pane.term.feed(&buf[..n]);
                            if let Some(copy) = pane.copy.as_mut() {
                                copy.sync_history(pane.term.grid());
                            }
                            got_output = true;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            eof = true;
                            break;
                        }
                    }
                }
            }
            // Answer any terminal queries the child emitted (DA/DSR/colour) by
            // writing the replies back to its PTY - otherwise querying shells
            // block until they time out.
            let responses = pane.term.take_responses();
            if !responses.is_empty() {
                Self::queue_pane_input(reg, pane, &responses);
            }
            for payload in pane.term.take_clipboard_requests() {
                let mut osc = b"\x1b]52;".to_vec();
                osc.extend_from_slice(&payload);
                osc.push(0x07);
                clipboard_ops.push(osc);
            }
            // Marks are recognized and kept out of the grid. The event log's
            // richer workflow events remain the durable source of truth.
            let _ = pane.term.take_prompt_marks();
            // Apply OSC 777 agent events: bind the agent (id + colour) on first
            // sight, then track its reconciled status.
            for ev in pane.term.take_agent_events() {
                cooperative_ready |= ev.cooperative_ready;
                // A terminal status ends the binding: the agent closed and the
                // pane is back at a plain shell, so the fleet entry must leave.
                if ev.status == Some(AgentStatus::Exited) {
                    invocation_ended = true;
                    if let Some(agent) = pane.agent.take() {
                        pane.last_detection = Some(DetectionRecord {
                            agent: agent.id,
                            status: AgentStatus::Exited,
                            authority: uniterm_proto::DetectionAuthority::Osc777,
                            evidence: "cooperative OSC 777 session end".into(),
                            foreground_pid: agent.foreground_pid,
                            provenance: direct_detection_provenance(
                                uniterm_proto::DetectionSource::Cooperative,
                                agent.foreground_pid,
                            ),
                        });
                        log_events
                            .push(crate::eventlog::LogEvent::AgentUnbound { pane: pane_id.0 });
                        agent_changed = true;
                    }
                    if !pane.launch_args.is_empty() {
                        log_events.push(crate::eventlog::LogEvent::PaneLaunchProfileCleared {
                            pane: pane_id.0,
                        });
                        pane.launch_args.clear();
                    }
                    continue;
                }
                let previous = pane
                    .agent
                    .as_ref()
                    .map(|agent| agent.status)
                    .unwrap_or(AgentStatus::Unknown);
                match (ev.agent, pane.agent.as_mut()) {
                    (Some(id), _) => {
                        let color = uniterm_core::agent::agent_color_or_default(&id);
                        let status = ev.status.unwrap_or(AgentStatus::Working);
                        let started_at = pane
                            .agent
                            .as_ref()
                            .filter(|agent| agent.id == id)
                            .map(|agent| agent.started_at)
                            .unwrap_or_else(std::time::Instant::now);
                        log_events.push(crate::eventlog::LogEvent::AgentBound {
                            pane: pane_id.0,
                            agent: id.clone(),
                        });
                        log_events.push(crate::eventlog::LogEvent::AgentStatus {
                            pane: pane_id.0,
                            status,
                        });
                        if ev.session_id.is_some() || !ev.resume_command.is_empty() {
                            log_events.push(crate::eventlog::LogEvent::AgentSessionObserved {
                                pane: pane_id.0,
                                provider: id.clone(),
                                session_id: ev.session_id.clone(),
                                resume_command: ev.resume_command.clone(),
                            });
                        }
                        pane.agent = Some(PaneAgent {
                            id,
                            color,
                            status,
                            authority: uniterm_proto::DetectionAuthority::Osc777,
                            evidence: "cooperative OSC 777 event".into(),
                            provenance: direct_detection_provenance(
                                uniterm_proto::DetectionSource::Cooperative,
                                pane.pty.foreground_process_group(),
                            ),
                            foreground_pid: pane.pty.foreground_process_group(),
                            started_at,
                            session_id: ev.session_id,
                            resume_command: ev.resume_command,
                        });
                        if previous != status {
                            agent_transitions.push((previous, status));
                        }
                        agent_changed = true;
                    }
                    (None, Some(pa)) => {
                        if ev.session_id.is_some() || !ev.resume_command.is_empty() {
                            log_events.push(crate::eventlog::LogEvent::AgentSessionObserved {
                                pane: pane_id.0,
                                provider: pa.id.clone(),
                                session_id: ev.session_id.clone(),
                                resume_command: ev.resume_command.clone(),
                            });
                        }
                        if ev.session_id.is_some() {
                            pa.session_id = ev.session_id;
                        }
                        if !ev.resume_command.is_empty() {
                            pa.resume_command = ev.resume_command;
                        }
                        if let Some(s) = ev.status {
                            let previous = pa.status;
                            pa.status = s;
                            pa.authority = uniterm_proto::DetectionAuthority::Osc777;
                            pa.evidence = "cooperative OSC 777 event".into();
                            pa.provenance = direct_detection_provenance(
                                uniterm_proto::DetectionSource::Cooperative,
                                pa.foreground_pid,
                            );
                            log_events.push(crate::eventlog::LogEvent::AgentStatus {
                                pane: pane_id.0,
                                status: s,
                            });
                            if previous != s {
                                agent_transitions.push((previous, s));
                            }
                        }
                    }
                    (None, None) => {}
                }
            }
            if got_output {
                cwd_changed |= update_working_directory(&mut pane.cwd, pane.term.reported_cwd());
                // Output volume is deliberately not state evidence. Keyboard
                // echo, a repainting footer, and a resize are all output, so
                // treating bytes as "working" marked idle agents busy while
                // the user typed. Output only decides when the screen and
                // title below are worth re-reading; the provider rules decide
                // the state, and no match means idle.
                let foreground = pane.pty.foreground_process_group();
                let process_changed = foreground != pane.foreground_pid;
                if process_changed && pane.pty.child_owns_foreground() && pane.term.is_alt_screen()
                {
                    // The kernel proved that a local foreground application
                    // returned to the pane shell. Recover even if the app was
                    // killed before emitting its alternate-screen teardown.
                    pane.term.recover_shell_screen();
                }
                let tail = pane.term.evidence_text(12);
                let title = pane.term.terminal_title().to_string();
                let hash = evidence_hash(&tail) ^ evidence_hash(&title).rotate_left(1);
                if process_changed || hash != pane.last_evidence_hash {
                    pane.foreground_pid = foreground;
                    if process_changed {
                        invocation_changed = Some(foreground);
                    }
                    pane.last_evidence_hash = hash;
                    evidence = Some(ScreenEvidence {
                        foreground_pid: foreground,
                        process_changed,
                        tail: tail.clone(),
                        title,
                        bound_agent: pane.agent.as_ref().map(|agent| agent.id.clone()),
                    });
                }
                let dev_tail = pane.term.recent_output_text(12);
                let dev_hash = evidence_hash(&dev_tail);
                if dev_hash != pane.last_dev_server_evidence_hash {
                    pane.last_dev_server_evidence_hash = dev_hash;
                    dev_server_evidence = Some(dev_tail);
                }
            }
            nested_input_changed = !pane.term.take_nested_input_changes().is_empty();
        }
        for e in log_events {
            self.append_event(e);
        }
        if cwd_changed {
            // OSC 7 is emitted only when the shell changes directory. Capture
            // that semantic change immediately instead of adding a timer or
            // filesystem polling to the keystroke-to-pixel path.
            self.persist();
        } else if got_output {
            self.mark_snapshot_dirty();
        }
        if invocation_ended {
            self.cancel_stale_instructions(pane_id, None);
        } else if let Some(invocation) = invocation_changed {
            self.cancel_stale_instructions(pane_id, invocation);
        }
        for (previous, status) in agent_transitions {
            self.notify_agent_transition(pane_id, previous, status);
        }
        if cooperative_ready && !invocation_ended {
            self.deliver_cooperative_instruction(reg, pane_id);
        }
        for osc in clipboard_ops {
            self.send_raw_ops(reg, &osc);
        }
        if let Some(evidence) = evidence {
            self.agents.send(uniterm_proto::CoreToAgent::PaneEvidence {
                pane: pane_id,
                foreground_pid: evidence.foreground_pid,
                process_changed: evidence.process_changed,
                tail: evidence.tail,
                title: evidence.title,
                bound_agent: evidence.bound_agent,
            });
        }
        if let Some(tail) = dev_server_evidence {
            self.agents
                .send(uniterm_proto::CoreToAgent::DevServerEvidence {
                    pane: pane_id,
                    tail,
                });
        }
        if read_budget_spent && !eof {
            self.pending_reads.push(token);
        }
        if got_output {
            // OSC 0/2 can change only the outer title and leave the grid clean.
            // The broadcast is deduplicated so unchanged titles cost nothing.
            self.sync_window_titles(reg);
        }
        if got_output || agent_changed {
            self.service_pending_waits(reg);
        }
        if nested_input_changed
            && self
                .windows
                .get(self.active_window)
                .is_some_and(|window| window.active == pane_id)
        {
            self.broadcast_nested_input(reg);
        }
        if eof {
            // A PTY commonly returns its final bytes and HUP in one readiness
            // batch. Paint those bytes before removing the pane, otherwise an
            // application's exit-time erase/restore sequences are lost and its
            // last UI remains stranded on attached clients.
            if got_output {
                self.broadcast_pane_damage(reg, pane_id);
            }
            self.stash_final_frames();
            self.close_pane(reg, pane_id);
        } else {
            // Pane output and an agent change can land in the same read batch
            // (output that also flips the agent's status, or an in-band OSC 777
            // bind). Deliver the pane's damaged cells first, then refresh the
            // fleet chrome in place. The chrome repaint no longer re-renders
            // panes, so the pane damage must be broadcast on its own or the new
            // output would be stranded until the next batch. Layout is
            // unchanged, so the rails and status bar update without a screen
            // clear instead of blanking the whole frame.
            if got_output {
                self.broadcast_pane_damage(reg, pane_id);
            }
            if agent_changed {
                self.repaint_chrome_all(reg);
            }
        }
    }

    pub(super) fn on_client(
        &mut self,
        reg: &Registry,
        token: Token,
        readable: bool,
        writable: bool,
    ) {
        let readable = self.runtime_read_ready(token, readable);
        let mut remove = self.clients.get(&token).is_some_and(|client| client.dead);
        let mut msgs = Vec::new();

        let mut read_budget_spent = false;
        if readable {
            if let Some(client) = self.clients.get_mut(&token) {
                let mut buf = [0u8; 8192];
                let mut read = 0usize;
                loop {
                    if read >= PTY_IO_BUDGET {
                        read_budget_spent = true;
                        break;
                    }
                    match client.stream.read(&mut buf) {
                        Ok(0) => {
                            remove = true;
                            break;
                        }
                        Ok(n) => {
                            read += n;
                            client.decoder.push(&buf[..n]);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            remove = true;
                            break;
                        }
                    }
                }
                loop {
                    if msgs.len() >= MAX_CLIENT_MESSAGES_PER_EVENT {
                        remove = true;
                        break;
                    }
                    match client.decoder.decode::<ClientMessage>() {
                        Ok(Some(m)) => msgs.push(m),
                        Ok(None) => break,
                        Err(_) => {
                            remove = true;
                            break;
                        }
                    }
                }
            }
        }
        if read_budget_spent && !remove {
            self.pending_reads.push(token);
        }

        coalesce_resizes(&mut msgs);
        for m in msgs {
            self.handle_msg(reg, token, m, &mut remove);
        }

        let mut repaint_after_drain = false;
        if writable {
            let Server { clients, .. } = self;
            if let Some(client) = clients.get_mut(&token) {
                client.flush_ready();
                let _ = set_interest(reg, client, token);
                remove |= client.dead;
                if !client.dead && !client.wants_write() {
                    if let Some(frame) = client.final_frame.take() {
                        // Show the closed pane's last output before the
                        // repaint that reflects its absence.
                        client.renderer.invalidate();
                        client.queue_render(&frame);
                        client.flush_ready();
                        let _ = set_interest(reg, client, token);
                    } else if client.repaint_pending {
                        client.repaint_pending = false;
                        repaint_after_drain = true;
                    }
                }
            }
        }

        if remove {
            self.remove_client(reg, token);
        } else if repaint_after_drain {
            self.full_repaint_client(reg, token);
        }
    }

    /// Send pre-encoded escape bytes to every attached client (e.g. an OSC 52
    /// clipboard write), outside the normal pane render path.
    pub(super) fn send_raw_ops(&mut self, reg: &Registry, raw: &[u8]) {
        let frame = encode_frame(&ServerMessage::RenderOps(raw.to_vec()));
        let Server { clients, .. } = self;
        for (tok, c) in clients.iter_mut() {
            if !c.attached {
                continue;
            }
            c.queue(&frame);
            c.flush();
            let _ = set_interest(reg, c, *tok);
        }
    }

    /// Kill every pane's child process (used by `KillServer`).
    /// Keep the current full frame for every client whose queue could not
    /// take the damage just broadcast. Called right before a pane closes: a
    /// collapsed repaint is regenerated later from state, and by then the
    /// pane and its final output are gone, so the client would never see
    /// them. A client that took the damage needs nothing.
    pub(super) fn stash_final_frames(&mut self) {
        let owed = |client: &Client| {
            client.attached && client.direct.is_none() && !client.dead && client.repaint_pending
        };
        if !self.clients.values().any(owed) {
            return;
        }
        let frame = encode_frame(&ServerMessage::RenderOps(self.build_full_frame()));
        for client in self.clients.values_mut().filter(|client| owed(client)) {
            client.final_frame = Some(frame.clone());
        }
    }

    pub(super) fn kill_all_panes(&mut self) {
        let panes: Vec<PaneId> = self.panes.keys().copied().collect();
        self.terminate_panes(&panes);
    }

    pub(super) fn terminate_panes(&mut self, panes: &[PaneId]) {
        for pane in panes {
            if let Some(pane) = self.panes.get_mut(pane) {
                let _ = pane.pty.begin_terminate();
            }
        }
    }

    pub(super) fn remove_client(&mut self, reg: &Registry, token: Token) {
        let was_visible = self.file_manager_visible();
        let had_clients = self
            .clients
            .values()
            .any(|client| client.attached && !client.dead);
        if let Some(mut c) = self.clients.remove(&token) {
            let _ = reg.deregister(&mut c.stream);
        }
        let geometry_changed = self.recompute_client_geometry();
        self.reconcile_file_manager_runtime(was_visible, had_clients);
        if geometry_changed {
            self.relayout();
            self.full_repaint_all(reg);
        }
    }
}

/// Keep only the last `Resize` of one client's decoded batch.
///
/// Every distinct size relayouts the window, reflows each pane's scrollback,
/// and repaints every client, so intermediate sizes a client managed to send
/// before the server read the socket are pure waste: only the newest one
/// describes the terminal. Other messages keep their order.
pub(super) fn coalesce_resizes(msgs: &mut Vec<ClientMessage>) {
    let is_resize = |m: &ClientMessage| matches!(m, ClientMessage::Resize { .. });
    if msgs.iter().filter(|m| is_resize(m)).count() < 2 {
        return;
    }
    let last = msgs.iter().rposition(is_resize).unwrap_or(0);
    let mut index = 0;
    msgs.retain(|m| {
        let keep = index == last || !is_resize(m);
        index += 1;
        keep
    });
}

pub(super) fn set_interest(
    reg: &Registry,
    client: &mut Client,
    token: Token,
) -> std::io::Result<()> {
    let want_write = client.wants_write();
    if want_write == client.write_interest {
        return Ok(());
    }
    let interest = if want_write {
        Interest::READABLE | Interest::WRITABLE
    } else {
        Interest::READABLE
    };
    reg.reregister(&mut client.stream, token, interest)?;
    client.write_interest = want_write;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_keeps_only_its_last_resize_and_preserves_other_order() {
        let mut msgs = vec![
            ClientMessage::Resize { cols: 10, rows: 5 },
            ClientMessage::Input(b"a".to_vec()),
            ClientMessage::Resize { cols: 20, rows: 6 },
            ClientMessage::Input(b"b".to_vec()),
            ClientMessage::Resize { cols: 30, rows: 7 },
        ];
        coalesce_resizes(&mut msgs);
        assert!(matches!(msgs[0], ClientMessage::Input(ref b) if b == b"a"));
        assert!(matches!(msgs[1], ClientMessage::Input(ref b) if b == b"b"));
        assert!(matches!(
            msgs[2],
            ClientMessage::Resize { cols: 30, rows: 7 }
        ));
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn a_lone_resize_and_a_resize_free_batch_are_untouched() {
        let mut lone = vec![
            ClientMessage::Input(b"a".to_vec()),
            ClientMessage::Resize { cols: 10, rows: 5 },
        ];
        coalesce_resizes(&mut lone);
        assert_eq!(lone.len(), 2);
        assert!(matches!(
            lone[1],
            ClientMessage::Resize { cols: 10, rows: 5 }
        ));
        let mut none = vec![ClientMessage::Input(b"a".to_vec())];
        coalesce_resizes(&mut none);
        assert_eq!(none.len(), 1);
        let mut empty: Vec<ClientMessage> = Vec::new();
        coalesce_resizes(&mut empty);
        assert!(empty.is_empty());
    }

    #[test]
    fn unchanged_client_interest_does_not_reregister_the_stream() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut client = Client {
            stream: UnixStream::from_std(stream),
            decoder: FrameDecoder::new(),
            renderer: Renderer::new(),
            outbuf: Vec::new(),
            out_offset: 0,
            render_end: None,
            attached: true,
            direct_only: false,
            direct: None,
            overlay: false,
            cols: 80,
            rows: 24,
            dead: false,
            repaint_pending: false,
            exit_notified: false,
            final_frame: None,
            write_interest: false,
            pending_wait: None,
        };
        let poll = Poll::new().unwrap();

        set_interest(poll.registry(), &mut client, Token(99)).unwrap();
        client.queue(b"pending");
        assert!(set_interest(poll.registry(), &mut client, Token(99)).is_err());
        assert!(!client.write_interest);
    }
}
