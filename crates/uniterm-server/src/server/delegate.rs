//! Per-invocation permission opt-in and budgets for the shared approval path.
//!
//! Recognition, kernel ownership, queue priority and duplicate suppression live
//! in `waiting::approve_permission`. Delegation never writes terminal input.
//! Arms and explicit opt-outs are live-only and are never restored from history.

use super::*;

#[derive(Clone)]
pub(super) struct DelegationState {
    agent: String,
    pid: i32,
    /// Durable invocation sequence prevents PID reuse from inheriting consent.
    invocation: u64,
    armed_at_ms: u64,
    budget: uniterm_core::delegate::DelegationBudget,
}

fn withhold(reason: impl Into<String>) -> uniterm_core::DelegateVerdict {
    uniterm_core::DelegateVerdict::Withhold {
        reason: reason.into(),
    }
}

impl Server {
    /// Arm only the current kernel-owned invocation; never infer approval
    /// authority from screen status or an unknown process identity.
    pub(super) fn arm_delegation(
        &mut self,
        reg: &Registry,
        pane: PaneId,
    ) -> Result<(), (String, String)> {
        if self.durability_error.is_some() || !self.event_writes_enabled {
            return Err((
                "durability_unavailable".into(),
                "Approval requires healthy event history".into(),
            ));
        }
        let target = self.panes.get(&pane).ok_or_else(|| {
            (
                "not_found".into(),
                "Pane not found in this Workspace".into(),
            )
        })?;
        let agent = target
            .agent
            .as_ref()
            .ok_or_else(|| ("no_agent".into(), "No agent is bound to the Pane".into()))?;
        if crate::providers::launch_manifest(&agent.id).is_none() {
            return Err((
                "unsupported_provider".into(),
                "No trusted permission recognition contract for this provider".into(),
            ));
        }
        let pid = agent
            .foreground_pid
            .filter(|pid| *pid > 0)
            .filter(|pid| target.pty.foreground_process_group() == Some(*pid))
            .ok_or_else(|| {
                (
                    "stale_invocation".into(),
                    "The agent no longer owns the Pane foreground".into(),
                )
            })?;
        let invocation = self
            .timeline_invocations
            .get(&pane)
            .copied()
            .ok_or_else(|| {
                (
                    "stale_invocation".into(),
                    "No current invocation is recorded".into(),
                )
            })?;
        let now_ms = super::guardrail::unix_time_ms();
        let policy = self.config.delegation_policy();
        let state = DelegationState {
            agent: agent.id.clone(),
            pid,
            invocation,
            armed_at_ms: now_ms,
            budget: uniterm_core::delegate::DelegationBudget::new(&policy, now_ms),
        };
        use sha2::Digest as _;
        let policy_digest = format!(
            "{:x}",
            sha2::Sha256::digest(policy.max_answers_per_hour.to_string().as_bytes())
        );
        self.append_event(crate::eventlog::LogEvent::DelegationArmed {
            pane,
            invocation: Some(pid),
            policy_digest,
        });
        self.delegation_opt_out.remove(&pane);
        self.delegated.insert(pane, state);
        self.maybe_auto_approve(reg, pane);
        self.repaint_chrome_all(reg);
        Ok(())
    }

    /// Explicit disarm and budget exhaustion also exclude this invocation
    /// from Workspace automation. Lifecycle cleanup retires that exclusion.
    pub(super) fn disarm_delegation(&mut self, pane: PaneId, reason: &str) -> bool {
        let opt_out = matches!(reason, "disarmed" | "budget_exhausted");
        let state = self.delegated.get(&pane);
        let current = self.timeline_invocations.get(&pane).copied();
        let changed_opt_out =
            opt_out && current.is_some() && self.delegation_opt_out.get(&pane).copied() != current;
        if state.is_none() && !changed_opt_out {
            if !opt_out && reason != "foreground_changed" {
                self.delegation_opt_out.remove(&pane);
            }
            return false;
        }
        let pid = state.map(|state| state.pid).or_else(|| {
            self.panes
                .get(&pane)
                .and_then(|p| p.agent.as_ref())
                .and_then(|a| a.foreground_pid)
        });
        self.append_event(crate::eventlog::LogEvent::DelegationDisarmed {
            pane,
            invocation: pid,
            reason: reason.into(),
        });
        self.delegated.remove(&pane);
        if opt_out {
            if let Some(invocation) = current {
                self.delegation_opt_out.insert(pane, invocation);
            }
        } else if reason != "foreground_changed" {
            self.delegation_opt_out.remove(&pane);
        }
        true
    }

    /// Revoke all pane arms even when the Workspace switch was already off.
    pub(super) fn disarm_all_delegations(&mut self) {
        let panes: Vec<_> = self.delegated.keys().copied().collect();
        for pane in panes {
            self.disarm_delegation(pane, "workspace_off");
        }
    }

    /// An explicit exclusion belongs to one recorded invocation only.
    pub(super) fn delegation_suppressed(&self, pane: PaneId) -> bool {
        self.timeline_invocations
            .get(&pane)
            .is_some_and(|invocation| self.delegation_opt_out.get(&pane) == Some(invocation))
    }

    /// Reject cached bindings after a process-group or invocation replacement.
    fn delegation_live(&self, pane: PaneId, state: &DelegationState) -> bool {
        self.timeline_invocations.get(&pane) == Some(&state.invocation)
            && self.panes.get(&pane).is_some_and(|target| {
                target.pty.foreground_process_group() == Some(state.pid)
                    && target.agent.as_ref().is_some_and(|agent| {
                        agent.id == state.agent && agent.foreground_pid == Some(state.pid)
                    })
            })
    }

    /// Called only after the shared recognizer identifies a new live dialog.
    /// Exhaustion suppresses Workspace fallback until explicit re-arm.
    pub(super) fn delegation_allows(&mut self, pane: PaneId, fingerprint: u64) -> bool {
        let Some(state) = self.delegated.get(&pane) else {
            return true;
        };
        if !self.delegation_live(pane, state) {
            self.disarm_delegation(pane, "invocation_changed");
            return false;
        }
        let now_ms = super::guardrail::unix_time_ms();
        let exhausted = state.budget.remaining(now_ms) == 0;
        let verdict = if exhausted {
            withhold("Hourly approval budget exhausted; re-arm to resume")
        } else {
            uniterm_core::evaluate_delegation(uniterm_core::WaitingKind::Permission)
        };
        self.append_event(crate::eventlog::LogEvent::DelegationDecision {
            record: uniterm_core::DelegationRecord {
                pane,
                waiting_id: self.waiting_id_for(pane),
                invocation: Some(state.pid),
                agent: state.agent.clone(),
                tool: "permission".into(),
                // Keep audit identity without copying command text into history.
                preview: format!("dialog:{fingerprint:016x}"),
                verdict,
            },
        });
        if exhausted {
            self.disarm_delegation(pane, "budget_exhausted");
        }
        !exhausted
    }

    /// Navigation never consumes budget; a failed queue write never consumes it.
    pub(super) fn delegation_delivered(&mut self, pane: PaneId) {
        let now_ms = super::guardrail::unix_time_ms();
        let exhausted = self.delegated.get_mut(&pane).is_some_and(|state| {
            state.budget.delivered(now_ms);
            state.budget.remaining(now_ms) == 0
        });
        if exhausted {
            self.disarm_delegation(pane, "budget_exhausted");
        }
    }

    /// Resolve a single Pane directly, or list arms in stable Pane order.
    pub(super) fn delegation_entries(
        &self,
        pane: Option<PaneId>,
    ) -> Vec<uniterm_proto::DelegationEntry> {
        if let Some(pane) = pane {
            return self.delegation_entry(pane).into_iter().collect();
        }
        let mut entries: Vec<_> = self
            .delegated
            .keys()
            .filter_map(|pane| self.delegation_entry(*pane))
            .collect();
        entries.sort_by_key(|entry| entry.pane.0);
        entries
    }

    /// Report the shared live verdict without typing or mutating budget state.
    pub(super) fn explain_delegation(&self, pane: PaneId) -> (bool, uniterm_core::DelegateVerdict) {
        let state = self.delegated.get(&pane);
        let found = state.is_some();
        let blocked = |reason| (found, withhold(reason));
        if self.durability_error.is_some() || !self.event_writes_enabled {
            return blocked("Event history is unavailable");
        }
        if self.delegation_suppressed(pane) {
            return blocked(
                "This invocation is excluded from automatic approval; re-arm to resume",
            );
        }
        if let Some(state) = state {
            if !self.delegation_live(pane, state) {
                return blocked("The armed invocation no longer owns the foreground");
            }
            if state.budget.remaining(super::guardrail::unix_time_ms()) == 0 {
                return blocked("Hourly approval budget exhausted; re-arm to resume");
            }
        } else if !self.auto_approval.enabled() {
            return blocked("Neither per-pane nor Workspace approval is enabled");
        }
        let Some(target) = self.panes.get(&pane) else {
            return blocked("Pane not found");
        };
        let Some(agent) = target.agent.as_ref() else {
            return blocked("No agent is bound to the Pane");
        };
        if target.input.len() != target.input_offset {
            return blocked("Human input is pending");
        }
        let Some(pid) = agent.foreground_pid.filter(|pid| *pid > 0) else {
            return blocked("No verified foreground owner");
        };
        if target.pty.foreground_process_group() != Some(pid) {
            return blocked("The agent no longer owns the foreground");
        }
        let Some(invocation) = self.timeline_invocations.get(&pane) else {
            return blocked("No recorded invocation");
        };
        let Some(choice) = crate::providers::permission_approval(&agent.id, &target.term) else {
            return blocked("No complete recognized permission dialog is visible");
        };
        if !self
            .auto_approval
            .is_new(pane, *invocation, choice.fingerprint, choice.navigation)
        {
            return blocked("This dialog was already answered; waiting for provider progress");
        }
        (found, uniterm_core::DelegateVerdict::Allow)
    }

    fn delegation_entry(&self, pane: PaneId) -> Option<uniterm_proto::DelegationEntry> {
        let state = self.delegated.get(&pane)?;
        let now_ms = super::guardrail::unix_time_ms();
        Some(uniterm_proto::DelegationEntry {
            pane,
            agent: state.agent.clone(),
            invocation: Some(state.pid),
            armed_at_ms: state.armed_at_ms,
            answers_used: state.budget.used(now_ms),
            answers_remaining: state.budget.remaining(now_ms),
            unlimited: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::reaper::common;
    use super::*;

    const DIALOG: &str = "\x1b[2J\x1b[HWould you like to run the following command?\r\n$ git status\r\n› 1. Yes, proceed (y)\r\n  2. No, and tell Codex what to do differently (esc)\r\nPress enter to confirm";

    fn fixture() -> (Server, Poll, PaneId) {
        common::isolate_state();
        let name = common::unique_workspace_name();
        let dir = common::socket_root().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut server, poll) = Server::bind_internal(
            &dir.join(format!("{name}.sock")),
            "/bin/sh",
            &[],
            120,
            30,
            false,
        )
        .unwrap();
        server.event_writes_enabled = true;
        let pane = server.windows[0].active;
        server.bind_agent(pane, "codex");
        (server, poll, pane)
    }

    #[test]
    fn delegation_refuses_known_history_failure_and_unrecorded_authority() {
        let (mut server, mut poll, pane) = fixture();
        server.arm_delegation(poll.registry(), pane).unwrap();
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .term
            .feed(DIALOG.as_bytes());
        server.durability_error = Some("simulated log failure".into());
        let before = server.log.current_sequence();
        server.maybe_auto_approve(poll.registry(), pane);
        assert_eq!(server.log.current_sequence(), before);
        assert!(!server.auto_approval.has_delivery(pane));
        assert_eq!(
            server.arm_delegation(poll.registry(), pane).unwrap_err().0,
            "durability_unavailable"
        );
        server.durability_error = None;
        server.event_writes_enabled = false;
        server.maybe_auto_approve(poll.registry(), pane);
        assert!(!server.auto_approval.has_delivery(pane));
        assert_eq!(
            server.arm_delegation(poll.registry(), pane).unwrap_err().0,
            "durability_unavailable"
        );
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn cached_identity_cannot_approve_for_another_kernel_owner_or_invocation() {
        let (mut server, mut poll, pane) = fixture();
        server.arm_delegation(poll.registry(), pane).unwrap();
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .term
            .feed(DIALOG.as_bytes());
        // Both cached facts agree with each other, but not the kernel owner.
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .agent
            .as_mut()
            .unwrap()
            .foreground_pid = Some(i32::MAX);
        server.delegated.get_mut(&pane).unwrap().pid = i32::MAX;
        server.maybe_auto_approve(poll.registry(), pane);
        assert!(!server.auto_approval.has_delivery(pane));
        assert_eq!(
            server.arm_delegation(poll.registry(), pane).unwrap_err().0,
            "stale_invocation"
        );
        let pid = server.panes[&pane].pty.foreground_process_group().unwrap();
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .agent
            .as_mut()
            .unwrap()
            .foreground_pid = Some(pid);
        server.delegated.get_mut(&pane).unwrap().pid = pid;
        *server.timeline_invocations.get_mut(&pane).unwrap() += 1;
        server.maybe_auto_approve(poll.registry(), pane);
        assert!(!server.auto_approval.has_delivery(pane));
        assert!(!server.delegated.contains_key(&pane));
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn temporary_foreground_loss_preserves_explicit_invocation_opt_out() {
        let (mut server, mut poll, pane) = fixture();
        server.set_auto_approve(poll.registry(), Some(true));
        server.disarm_delegation(pane, "disarmed");
        assert!(server.delegation_suppressed(pane));
        server.disarm_delegation(pane, "foreground_changed");
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .term
            .feed(DIALOG.as_bytes());
        server.maybe_auto_approve(poll.registry(), pane);
        assert!(server.delegation_suppressed(pane));
        assert!(!server.auto_approval.has_delivery(pane));
        // A genuinely new invocation has no inherited exclusion.
        *server.timeline_invocations.get_mut(&pane).unwrap() += 1;
        assert!(!server.delegation_suppressed(pane));
        server.retire_all_ptys(&mut poll);
    }

    #[test]
    fn human_input_takes_priority_and_budget_is_captured_at_arm_time() {
        let (mut server, mut poll, pane) = fixture();
        server.config.delegation_max_answers_per_hour = 1;
        server.arm_delegation(poll.registry(), pane).unwrap();
        server.config.delegation_max_answers_per_hour = 240;
        server
            .panes
            .get_mut(&pane)
            .unwrap()
            .term
            .feed(DIALOG.as_bytes());
        server.panes.get_mut(&pane).unwrap().input.push(b'x');
        server.maybe_auto_approve(poll.registry(), pane);
        assert!(!server.auto_approval.has_delivery(pane));
        assert_eq!(server.delegation_entry(pane).unwrap().answers_remaining, 1);
        server.panes.get_mut(&pane).unwrap().input.clear();
        server.maybe_auto_approve(poll.registry(), pane);
        assert!(server.auto_approval.has_delivery(pane));
        assert!(server.delegated.is_empty());
        assert!(server.delegation_suppressed(pane));
        server.retire_all_ptys(&mut poll);
    }
}
