//! The pure decision brains for multi-agent orchestration (workflows + relay).
//! See `docs/07-workflows-and-relay.md`.
//!
//! This is the single most important design principle of the agentic layer: the
//! decision logic is a set of pure functions from (state, event) to a
//! next-action, with NO I/O and NO UI, tested exhaustively here in core, and
//! only then wired to real panes in `uniterm-server`. Every tricky case (a
//! verdict that stalls, an iteration cap, a forged token, an idle race with an
//! explicit submit) is a table-testable transition, not a tangle of callbacks.
//!
//! The completion contract is load-bearing: the engine advances on an explicit
//! `submit` carrying the live per-activation token (a role cannot forge another
//! role's completion), and the idle heuristic is only ever a safety net.

/// Provider-neutral capabilities a role requires from whichever local CLI is
/// selected for it. The core validates requirements and selections, while the
/// server owns installed-provider discovery and executable resolution.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderRequirement {
    /// Stable capability names. An empty list accepts any installed provider.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl ProviderRequirement {
    /// A role with no provider-specific requirement.
    pub fn any() -> Self {
        Self::default()
    }

    /// A role requiring all named provider capabilities.
    pub fn all(capabilities: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            capabilities: capabilities.into_iter().map(Into::into).collect(),
        }
    }
}

/// One explicit user choice assigning a provider id to a named role.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoleProviderSelection {
    pub role: String,
    pub provider: String,
}

/// A malformed provider selection that can be rejected before any Pane or run
/// is created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoleProviderSelectionError {
    EmptyRole,
    EmptyProvider { role: String },
    RoleTooLong,
    ProviderTooLong { role: String },
    UnknownRole { role: String },
    DuplicateRole { role: String },
}

impl std::fmt::Display for RoleProviderSelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyRole => write!(f, "provider selection has an empty role name"),
            Self::EmptyProvider { role } => {
                write!(f, "provider selection for role '{role}' is empty")
            }
            Self::RoleTooLong => write!(f, "provider-selection role exceeds 256 bytes"),
            Self::ProviderTooLong { role } => {
                write!(f, "provider selection for role '{role}' exceeds 256 bytes")
            }
            Self::UnknownRole { role } => write!(f, "unknown orchestration role '{role}'"),
            Self::DuplicateRole { role } => {
                write!(f, "provider selected more than once for role '{role}'")
            }
        }
    }
}

impl std::error::Error for RoleProviderSelectionError {}

/// Validate explicit role selections and align them with `roles`. Missing
/// entries remain `None` so the server can apply the global or first-installed
/// fallback without moving provider discovery into core.
pub fn align_role_provider_selections(
    roles: &[Role],
    selections: &[RoleProviderSelection],
) -> Result<Vec<Option<String>>, RoleProviderSelectionError> {
    let mut aligned = vec![None; roles.len()];
    for selection in selections {
        if selection.role.is_empty() {
            return Err(RoleProviderSelectionError::EmptyRole);
        }
        if selection.role.len() > 256 {
            return Err(RoleProviderSelectionError::RoleTooLong);
        }
        if selection.provider.is_empty() {
            return Err(RoleProviderSelectionError::EmptyProvider {
                role: selection.role.clone(),
            });
        }
        if selection.provider.len() > 256 {
            return Err(RoleProviderSelectionError::ProviderTooLong {
                role: selection.role.clone(),
            });
        }
        let Some(index) = roles.iter().position(|role| role.name == selection.role) else {
            return Err(RoleProviderSelectionError::UnknownRole {
                role: selection.role.clone(),
            });
        };
        if aligned[index].is_some() {
            return Err(RoleProviderSelectionError::DuplicateRole {
                role: selection.role.clone(),
            });
        }
        aligned[index] = Some(selection.provider.clone());
    }
    Ok(aligned)
}

/// A role slot in an orchestration (planner, builder, verifier, ...). Which
/// concrete agent fills it is chosen elsewhere; the engine is agent-agnostic.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Role {
    pub name: String,
    /// Exactly one role is the verifier; only it may produce a verdict.
    pub verifier: bool,
    /// Capabilities required from the selected provider.
    #[serde(default)]
    pub provider_requirement: ProviderRequirement,
}

impl Role {
    pub fn new(name: &str, verifier: bool) -> Self {
        Role {
            name: name.to_string(),
            verifier,
            provider_requirement: ProviderRequirement::any(),
        }
    }

    /// Add provider capabilities to a role without naming a concrete agent.
    pub fn requiring(
        name: &str,
        verifier: bool,
        capabilities: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.to_string(),
            verifier,
            provider_requirement: ProviderRequirement::all(capabilities),
        }
    }
}

/// The verifier's structured verdict. Findings feed stall detection.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Verdict {
    Approved,
    Fix(String),
    Replan(String),
}

/// The status a role/turn reports on submit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SubmitStatus {
    Done,
    NeedsInput,
    Failed,
}

/// An explicit completion submission (over the control protocol).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submit {
    /// The activation token embedded in the injected prompt; must match the
    /// live token or the submit is ignored as a forgery.
    pub token: u64,
    /// The role index claiming to submit.
    pub role: usize,
    pub status: SubmitStatus,
    /// Present only for the verifier; ignored (rejected) from other roles.
    pub verdict: Option<Verdict>,
}

/// What drives the engine forward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Begin: open the first role's turn.
    Start,
    /// An explicit completion signal (the primary trigger).
    Submit(Submit),
    /// The idle-hold safety net fired for a role (fallback only, never for the
    /// verifier - a verdict cannot be guessed from idleness).
    Idle { role: usize },
    /// A turn/role exceeded its stall window.
    Stall,
    /// A Workspace guard reached a cooperative boundary and requires human
    /// direction before the current role can continue.
    Guardrail { reason: String },
    /// A human resolved an escalation and asked the current role to retry.
    Resume,
    /// A human asked to stop.
    Stop,
}

/// The next action the server should interpret. `Inject` triggers a real
/// bracketed-paste prompt delivery; `Abort` creates a waiting-queue escalation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Deliver `role`'s prompt with `token` embedded (workflow "Inject" / relay
    /// "OpenTurn").
    Inject { role: usize, token: u64 },
    /// Nothing to do until an explicit submit arrives.
    AwaitSubmit,
    /// The orchestration finished successfully.
    Complete,
    /// Pause into the waiting queue while retaining restartable run state.
    Escalate { reason: String },
    /// Pause into the waiting queue with a reason (workflow "Escalate").
    Abort { reason: String },
    /// The event did not apply in the current phase; state is unchanged.
    Hold,
}

/// Lifecycle phase of the orchestration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Phase {
    Idle,
    Awaiting,
    Paused,
    Done,
    Aborted,
}

/// The full orchestration state (shared by workflow and relay - their pure
/// decision logic coincides; the I/O differences, artifact gates and git
/// checkpoints, live server-side per `docs/07`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct State {
    pub roles: Vec<Role>,
    /// Current role/turn index.
    pub cur: usize,
    pub iteration: u32,
    pub max_iterations: u32,
    /// The live activation token (0 = none open). Minted on each turn open.
    pub token: u64,
    token_seq: u64,
    /// Findings of the last `fix` verdict, for verdict-stall detection.
    last_fix: Option<String>,
    /// Where a `fix` / `replan` verdict routes back to (role indices).
    pub fix_target: usize,
    pub replan_target: usize,
    pub phase: Phase,
}

impl State {
    /// A new orchestration over `roles` with an iteration cap. `fix_target`
    /// defaults to the role before the verifier (the builder), `replan_target`
    /// to role 0 (the planner).
    pub fn new(roles: Vec<Role>, max_iterations: u32) -> Self {
        let verifier_idx = roles.iter().position(|r| r.verifier).unwrap_or(0);
        let fix_target = verifier_idx.saturating_sub(1);
        State {
            roles,
            cur: 0,
            iteration: 0,
            max_iterations,
            token: 0,
            token_seq: 0,
            last_fix: None,
            fix_target,
            replan_target: 0,
            phase: Phase::Idle,
        }
    }

    fn open_turn(&mut self) -> Action {
        self.token_seq += 1;
        self.token = self.token_seq;
        self.phase = Phase::Awaiting;
        Action::Inject {
            role: self.cur,
            token: self.token,
        }
    }

    fn advance_after_done(&mut self) -> Action {
        if self.cur + 1 >= self.roles.len() {
            self.phase = Phase::Done;
            return Action::Complete;
        }
        self.cur += 1;
        self.open_turn()
    }

    fn pause(&mut self, reason: &str) -> Action {
        self.phase = Phase::Paused;
        Action::Escalate {
            reason: reason.to_string(),
        }
    }

    fn abort(&mut self, reason: &str) -> Action {
        self.phase = Phase::Aborted;
        Action::Abort {
            reason: reason.to_string(),
        }
    }

    fn route_fix(&mut self, findings: String) -> Action {
        // Verdict-stall: two consecutive identical fix findings -> not
        // converging, escalate instead of looping forever.
        if self.last_fix.as_deref() == Some(findings.as_str()) {
            return self.pause("verdict stalled (two identical fix verdicts)");
        }
        self.last_fix = Some(findings);
        self.iteration += 1;
        if self.iteration > self.max_iterations {
            return self.pause("max iterations exceeded");
        }
        self.cur = self.fix_target;
        self.open_turn()
    }

    fn route_replan(&mut self) -> Action {
        self.iteration += 1;
        if self.iteration > self.max_iterations {
            return self.pause("max iterations exceeded");
        }
        // A replan is a fresh loop; forget the prior fix findings.
        self.last_fix = None;
        self.cur = self.replan_target;
        self.open_turn()
    }
}

/// Advance the relay/workflow state by one event, returning the next action.
/// This is the pure brain - no I/O. The workflow and relay entry points below
/// both delegate here.
pub fn step(state: &mut State, event: Event) -> Action {
    if matches!(state.phase, Phase::Done | Phase::Aborted) {
        return Action::Hold; // terminal: ignore further events
    }
    if state.phase == Phase::Paused {
        return match event {
            Event::Resume => state.open_turn(),
            Event::Stop => state.abort("stopped by human"),
            _ => Action::Hold,
        };
    }
    match event {
        Event::Start => {
            if state.phase != Phase::Idle {
                return Action::Hold;
            }
            state.cur = 0;
            state.open_turn()
        }
        Event::Stop => state.abort("stopped by human"),
        Event::Stall => state.pause("turn stalled"),
        Event::Guardrail { reason } => state.pause(&reason),
        Event::Resume => Action::Hold,
        Event::Idle { role } => {
            // Safety net only: applies to the current, non-verifier role.
            if state.phase != Phase::Awaiting || role != state.cur {
                return Action::Hold;
            }
            if state.roles[state.cur].verifier {
                return Action::Hold; // never guess a verdict from idleness
            }
            state.advance_after_done()
        }
        Event::Submit(sub) => {
            if state.phase != Phase::Awaiting {
                return Action::Hold;
            }
            // Forged or mismatched token/role: ignore (a role cannot complete
            // for another, nor submit against a stale turn).
            if sub.token != state.token || sub.role != state.cur {
                return Action::Hold;
            }
            let is_verifier = state.roles[state.cur].verifier;
            // Only the verifier may produce a verdict.
            if sub.verdict.is_some() && !is_verifier {
                return Action::Hold;
            }
            match sub.status {
                SubmitStatus::Failed => state.pause("role reported failure"),
                SubmitStatus::NeedsInput => Action::AwaitSubmit,
                SubmitStatus::Done => {
                    if is_verifier {
                        match sub.verdict {
                            Some(Verdict::Approved) => {
                                state.phase = Phase::Done;
                                Action::Complete
                            }
                            Some(Verdict::Fix(f)) => state.route_fix(f),
                            Some(Verdict::Replan(_)) => state.route_replan(),
                            // Verifier done without a verdict: wait for it.
                            None => Action::AwaitSubmit,
                        }
                    } else {
                        state.advance_after_done()
                    }
                }
            }
        }
    }
}

/// The relay decision brain (turn-based async handoff). See `docs/07`.
pub fn decide_relay_next(state: &mut State, event: Event) -> Action {
    step(state, event)
}

/// The workflow decision brain (role-based sequence). Shares the pure logic with
/// relay; the differences (artifact gates, git checkpoints) are server-side I/O.
pub fn decide_workflow_next(state: &mut State, event: Event) -> Action {
    step(state, event)
}

/// One role slot in a bundled template: its name, whether it is the verifier,
/// and its prompt template. Prompts interpolate `{goal}`, `{role}`, `{token}`,
/// and `{submit}` (the completion-contract command) via [`render_role_prompt`].
#[derive(Clone, Copy, Debug)]
pub struct RoleSpec {
    pub name: &'static str,
    pub verifier: bool,
    /// Provider-neutral capabilities required by this role. Bundled roles
    /// currently accept every registered interactive CLI.
    pub provider_capabilities: &'static [&'static str],
    /// Typed Project-relative files that must validate before this role's
    /// `done` submission may advance.
    pub expected_artifacts: &'static [ArtifactRequirement],
    pub prompt: &'static str,
}

/// One typed artifact gate declared by a bundled workflow role.
#[derive(Clone, Copy, Debug)]
pub struct ArtifactRequirement {
    /// Semantic class retained by the ledger after validation.
    pub kind: crate::ArtifactKind,
    /// Project-relative path checked outside the mio loop.
    pub path: &'static str,
}

/// A bundled workflow template: what `/workflow <name>` accepts. User-defined
/// TOML templates (docs/07) will extend this list; the bundled trio ships so
/// the name has meaning (and can be suggested) from day one.
#[derive(Clone, Copy, Debug)]
pub struct WorkflowTemplate {
    pub name: &'static str,
    /// One-line summary shown in suggestion lists.
    pub summary: &'static str,
    /// The ordered role slots the template spawns (verifier last).
    pub roles: &'static [RoleSpec],
}

impl WorkflowTemplate {
    /// The engine roles for this template.
    pub fn engine_roles(&self) -> Vec<Role> {
        self.roles
            .iter()
            .map(|r| Role::requiring(r.name, r.verifier, r.provider_capabilities.iter().copied()))
            .collect()
    }
}

/// Look up a bundled template by name.
pub fn workflow_template(name: &str) -> Option<&'static WorkflowTemplate> {
    WORKFLOW_TEMPLATES.iter().find(|t| t.name == name)
}

/// Render a role's prompt: interpolate the goal and the completion contract.
/// `{submit}` becomes the exact command the agent must run when finished, with
/// the live per-turn token embedded - the token is what makes the contract
/// unforgeable across roles/turns (docs/07).
pub fn render_role_prompt(spec: &RoleSpec, goal: &str, token: u64) -> String {
    let submit = if spec.verifier {
        format!(
            "uniterm workflow submit {token} --verdict approved|fix --summary \"<one-line findings>\" \
             (approved = ship it; fix = send it back with your findings)"
        )
    } else {
        format!("uniterm workflow submit {token}")
    };
    spec.prompt
        .replace("{goal}", goal)
        .replace("{role}", spec.name)
        .replace("{token}", &token.to_string())
        .replace("{submit}", &submit)
}

const PLANNER_PROMPT: &str = "You are the PLANNER in a multi-agent workflow. Goal: {goal}. \
Produce a concrete implementation plan for the goal: the files to touch, the steps in order, \
the risks, and how to verify the result. Write the plan to WORKFLOW_PLAN.md in the working \
directory so the builder (running in the next pane) can follow it. Do NOT implement anything. \
When the plan is written, run exactly this command to hand off: {submit}";

const BUILDER_PROMPT: &str = "You are the BUILDER in a multi-agent workflow. Goal: {goal}. \
If WORKFLOW_PLAN.md exists in the working directory, follow it; otherwise plan briefly and \
proceed. Implement the goal completely: write the code, make it build, and run the tests you \
can. A verifier (running in the next pane) will review your work afterwards - if it sends \
findings back, you will receive them here; address them and hand off again. When your \
implementation is ready for review, run exactly this command: {submit}";

const SOLO_PROMPT: &str = "You are working solo in a workflow pane. Goal: {goal}. \
Implement the goal completely: plan briefly, write the code, make it build, run the tests \
you can, and verify your own result. When everything is done, run exactly this command: \
{submit}";

const VERIFIER_PROMPT: &str = "You are the VERIFIER in a multi-agent workflow. Goal: {goal}. \
The builder (in the previous pane) claims the goal is implemented. Review the working tree \
critically: does it build, do the tests pass, does it actually satisfy the goal, are there \
bugs or shortcuts? Only you may pass verdict. When your review is complete, run exactly this \
command with your verdict: {submit}";

/// The bundled templates (docs/07 "Templates": a solo agent, a pair, and a
/// planner-builder-verifier triad).
pub const WORKFLOW_TEMPLATES: &[WorkflowTemplate] = &[
    WorkflowTemplate {
        name: "solo",
        summary: "one agent does the whole task",
        roles: &[RoleSpec {
            name: "builder",
            verifier: false,
            provider_capabilities: &["interactive_cli"],
            expected_artifacts: &[],
            prompt: SOLO_PROMPT,
        }],
    },
    WorkflowTemplate {
        name: "pair",
        summary: "builder implements, verifier reviews",
        roles: &[
            RoleSpec {
                name: "builder",
                verifier: false,
                provider_capabilities: &["interactive_cli"],
                expected_artifacts: &[],
                prompt: BUILDER_PROMPT,
            },
            RoleSpec {
                name: "verifier",
                verifier: true,
                provider_capabilities: &["interactive_cli"],
                expected_artifacts: &[],
                prompt: VERIFIER_PROMPT,
            },
        ],
    },
    WorkflowTemplate {
        name: "triad",
        summary: "planner designs, builder implements, verifier reviews",
        roles: &[
            RoleSpec {
                name: "planner",
                verifier: false,
                provider_capabilities: &["interactive_cli"],
                expected_artifacts: &[ArtifactRequirement {
                    kind: crate::ArtifactKind::Plan,
                    path: "WORKFLOW_PLAN.md",
                }],
                prompt: PLANNER_PROMPT,
            },
            RoleSpec {
                name: "builder",
                verifier: false,
                provider_capabilities: &["interactive_cli"],
                expected_artifacts: &[],
                prompt: BUILDER_PROMPT,
            },
            RoleSpec {
                name: "verifier",
                verifier: true,
                provider_capabilities: &["interactive_cli"],
                expected_artifacts: &[],
                prompt: VERIFIER_PROMPT,
            },
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn triad() -> State {
        State::new(
            vec![
                Role::new("planner", false),
                Role::new("builder", false),
                Role::new("verifier", true),
            ],
            3,
        )
    }

    fn submit(token: u64, role: usize, status: SubmitStatus, verdict: Option<Verdict>) -> Event {
        Event::Submit(Submit {
            token,
            role,
            status,
            verdict,
        })
    }

    #[test]
    fn happy_path_plan_build_verify_approve() {
        let mut s = triad();
        assert_eq!(
            step(&mut s, Event::Start),
            Action::Inject { role: 0, token: 1 }
        );
        // planner done -> builder
        assert_eq!(
            step(&mut s, submit(1, 0, SubmitStatus::Done, None)),
            Action::Inject { role: 1, token: 2 }
        );
        // builder done -> verifier
        assert_eq!(
            step(&mut s, submit(2, 1, SubmitStatus::Done, None)),
            Action::Inject { role: 2, token: 3 }
        );
        // verifier approves -> complete
        assert_eq!(
            step(
                &mut s,
                submit(3, 2, SubmitStatus::Done, Some(Verdict::Approved))
            ),
            Action::Complete
        );
        assert_eq!(s.phase, Phase::Done);
    }

    #[test]
    fn forged_token_is_ignored() {
        let mut s = triad();
        step(&mut s, Event::Start); // token 1, role 0 awaiting
                                    // Wrong token: ignored, still awaiting the real submit.
        assert_eq!(
            step(&mut s, submit(999, 0, SubmitStatus::Done, None)),
            Action::Hold
        );
        assert_eq!(s.phase, Phase::Awaiting);
        assert_eq!(s.cur, 0);
        // Wrong role (claims to be role 2) also ignored.
        assert_eq!(
            step(&mut s, submit(1, 2, SubmitStatus::Done, None)),
            Action::Hold
        );
    }

    #[test]
    fn needs_input_keeps_the_same_activation_open() {
        let mut state = triad();
        step(&mut state, Event::Start);
        assert_eq!(
            step(&mut state, submit(1, 0, SubmitStatus::NeedsInput, None)),
            Action::AwaitSubmit
        );
        assert_eq!(state.phase, Phase::Awaiting);
        assert_eq!(state.cur, 0);
        assert_eq!(state.token, 1);
    }

    #[test]
    fn non_verifier_verdict_is_rejected() {
        let mut s = triad();
        step(&mut s, Event::Start);
        // planner (role 0, not verifier) tries to produce a verdict: rejected.
        assert_eq!(
            step(
                &mut s,
                submit(1, 0, SubmitStatus::Done, Some(Verdict::Approved))
            ),
            Action::Hold
        );
        assert_eq!(s.phase, Phase::Awaiting);
    }

    #[test]
    fn fix_routes_back_to_builder_and_increments_iteration() {
        let mut s = triad();
        step(&mut s, Event::Start);
        step(&mut s, submit(1, 0, SubmitStatus::Done, None)); // -> builder t2
        step(&mut s, submit(2, 1, SubmitStatus::Done, None)); // -> verifier t3
        let a = step(
            &mut s,
            submit(3, 2, SubmitStatus::Done, Some(Verdict::Fix("A".into()))),
        );
        // fix_target is the builder (index 1); fresh token minted.
        assert_eq!(a, Action::Inject { role: 1, token: 4 });
        assert_eq!(s.iteration, 1);
    }

    #[test]
    fn two_identical_fix_verdicts_stall_and_abort() {
        let mut s = triad();
        step(&mut s, Event::Start);
        step(&mut s, submit(1, 0, SubmitStatus::Done, None)); // builder t2
        step(&mut s, submit(2, 1, SubmitStatus::Done, None)); // verifier t3
        step(
            &mut s,
            submit(3, 2, SubmitStatus::Done, Some(Verdict::Fix("same".into()))),
        ); // -> builder t4
        step(&mut s, submit(4, 1, SubmitStatus::Done, None)); // builder done -> verifier t5
        let a = step(
            &mut s,
            submit(5, 2, SubmitStatus::Done, Some(Verdict::Fix("same".into()))),
        );
        assert!(matches!(a, Action::Escalate { .. }));
        assert_eq!(s.phase, Phase::Paused);
    }

    #[test]
    fn iteration_cap_aborts() {
        // Cap of 1: a second distinct fix pushes iteration past the cap.
        let mut s = State::new(
            vec![Role::new("builder", false), Role::new("verifier", true)],
            1,
        );
        step(&mut s, Event::Start); // builder t1
        step(&mut s, submit(1, 0, SubmitStatus::Done, None)); // verifier t2
                                                              // first fix: iteration 1 (== cap, ok) -> builder t3
        assert!(matches!(
            step(
                &mut s,
                submit(2, 1, SubmitStatus::Done, Some(Verdict::Fix("a".into())))
            ),
            Action::Inject { .. }
        ));
        step(&mut s, submit(3, 0, SubmitStatus::Done, None)); // builder -> verifier t4
                                                              // second, different fix: iteration 2 > cap -> abort
        let a = step(
            &mut s,
            submit(4, 1, SubmitStatus::Done, Some(Verdict::Fix("b".into()))),
        );
        assert!(matches!(a, Action::Escalate { .. }));
    }

    #[test]
    fn idle_safety_net_advances_nonverifier_but_not_verifier() {
        let mut s = triad();
        step(&mut s, Event::Start); // planner awaiting
                                    // Idle for the current planner: safety-net advance to builder.
        assert_eq!(
            step(&mut s, Event::Idle { role: 0 }),
            Action::Inject { role: 1, token: 2 }
        );
        step(&mut s, submit(2, 1, SubmitStatus::Done, None)); // -> verifier t3
                                                              // Idle for the verifier must NOT advance (can't guess a verdict).
        assert_eq!(step(&mut s, Event::Idle { role: 2 }), Action::Hold);
        assert_eq!(s.phase, Phase::Awaiting);
    }

    #[test]
    fn failure_and_stall_and_stop_escalate() {
        let mut s = triad();
        step(&mut s, Event::Start);
        assert!(matches!(
            step(&mut s, submit(1, 0, SubmitStatus::Failed, None)),
            Action::Escalate { .. }
        ));

        let mut s2 = triad();
        step(&mut s2, Event::Start);
        assert!(matches!(
            step(&mut s2, Event::Stall),
            Action::Escalate { .. }
        ));

        let mut s3 = triad();
        step(&mut s3, Event::Start);
        assert!(matches!(step(&mut s3, Event::Stop), Action::Abort { .. }));

        let mut guarded = triad();
        step(&mut guarded, Event::Start);
        assert_eq!(
            step(
                &mut guarded,
                Event::Guardrail {
                    reason: "elapsed boundary".into(),
                }
            ),
            Action::Escalate {
                reason: "elapsed boundary".into()
            }
        );
        assert_eq!(guarded.phase, Phase::Paused);
    }

    #[test]
    fn a_human_can_resume_a_paused_role_with_a_fresh_token() {
        let mut state = triad();
        step(&mut state, Event::Start);
        assert!(matches!(
            step(&mut state, submit(1, 0, SubmitStatus::Failed, None)),
            Action::Escalate { .. }
        ));
        assert_eq!(state.phase, Phase::Paused);
        assert_eq!(
            step(&mut state, Event::Resume),
            Action::Inject { role: 0, token: 2 }
        );
        assert_eq!(state.phase, Phase::Awaiting);
    }

    #[test]
    fn events_after_terminal_are_held() {
        let mut s = triad();
        step(&mut s, Event::Start);
        step(&mut s, Event::Stop); // aborted
        assert_eq!(
            step(&mut s, submit(1, 0, SubmitStatus::Done, None)),
            Action::Hold
        );
    }

    #[test]
    fn replan_routes_to_planner() {
        let mut s = triad();
        step(&mut s, Event::Start);
        step(&mut s, submit(1, 0, SubmitStatus::Done, None)); // builder t2
        step(&mut s, submit(2, 1, SubmitStatus::Done, None)); // verifier t3
        let a = step(
            &mut s,
            submit(3, 2, SubmitStatus::Done, Some(Verdict::Replan("x".into()))),
        );
        assert_eq!(a, Action::Inject { role: 0, token: 4 }); // back to planner
        assert_eq!(s.iteration, 1);
    }

    #[test]
    fn workflow_entry_point_matches_relay() {
        let mut a = triad();
        let mut b = triad();
        assert_eq!(
            decide_relay_next(&mut a, Event::Start),
            decide_workflow_next(&mut b, Event::Start)
        );
    }

    #[test]
    fn role_provider_selections_align_without_naming_providers_in_core() {
        let roles = triad().roles;
        let aligned = align_role_provider_selections(
            &roles,
            &[
                RoleProviderSelection {
                    role: "verifier".into(),
                    provider: "provider-b".into(),
                },
                RoleProviderSelection {
                    role: "planner".into(),
                    provider: "provider-a".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            aligned,
            [Some("provider-a".into()), None, Some("provider-b".into())]
        );
    }

    #[test]
    fn role_provider_selections_reject_unknown_and_duplicate_roles() {
        let roles = triad().roles;
        let error = align_role_provider_selections(
            &roles,
            &[RoleProviderSelection {
                role: "reviewer".into(),
                provider: "provider-a".into(),
            }],
        )
        .unwrap_err();
        assert_eq!(
            error,
            RoleProviderSelectionError::UnknownRole {
                role: "reviewer".into()
            }
        );

        let duplicate = RoleProviderSelection {
            role: "builder".into(),
            provider: "provider-a".into(),
        };
        let error =
            align_role_provider_selections(&roles, &[duplicate.clone(), duplicate]).unwrap_err();
        assert_eq!(
            error,
            RoleProviderSelectionError::DuplicateRole {
                role: "builder".into()
            }
        );
    }
}
