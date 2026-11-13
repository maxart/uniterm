//! Delegation extends the existing approval pipeline; these real PTY tests
//! assert input, progress, scope and revocation rather than canned replies.

use std::path::PathBuf;
use uniterm_core::PaneId;
use uniterm_proto::{ControlCommand, WaitingAction};

mod common;
#[path = "common/control.rs"]
mod control;
use control::*;

struct Harness {
    socket: PathBuf,
    control: Control,
    pane: u64,
    dir: PathBuf,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn new(provider: &str, budget: u16) -> Self {
        let dir = common::temp_dir("delegation");
        let (socket, control, pane, thread) = start_server(uniterm_core::Config {
            delegation_max_answers_per_hour: budget,
            ..Default::default()
        });
        let mut h = Self {
            socket,
            control,
            pane,
            dir,
            thread: Some(thread),
        };
        // Native envelopes avoid inheriting the test runner's Codex ancestry.
        let start = h.dir.join("start");
        std::fs::write(&start, format!("\x1b]777;notify;uniterm://cli-agent;{{\"agent\":\"{provider}\",\"event\":\"session_start\",\"session_id\":\"owned\"}}\x07")).unwrap();
        h.run(&format!("stty -echo; {}; while IFS= read -r file; do if [ -n \"$file\" ]; then cat \"$file\"; else echo approved >> '{}'; fi; done", cat(&[&start]), h.dir.join("answers").display()));
        h.control.until_bound("owned");
        h
    }
    fn run(&mut self, script: &str) {
        let file = self.dir.join("agent.sh");
        std::fs::write(&file, script).unwrap();
        self.control
            .type_line(self.pane, &format!("sh '{}'", file.display()));
    }
    fn arm(&mut self) {
        self.control.arm(self.pane);
    }
    fn mode(&mut self, enabled: bool) {
        self.control.request(ControlCommand::AutoApprove {
            enabled: Some(enabled),
            confirmed: false,
        });
    }
    fn disarm(&mut self) {
        self.control.request(ControlCommand::DelegationDisarm {
            pane: PaneId(self.pane),
        });
    }
    fn screen(&mut self, name: &str, text: &str) {
        let file = self.dir.join(name);
        let marker = format!("fixture:{name}");
        std::fs::write(&file, format!("\x1b[2J\x1b[H{text}\r\n{marker}")).unwrap();
        self.control.type_line(self.pane, file.to_str().unwrap());
        self.control.until_output(self.pane, &marker);
    }
}
impl Harness {
    fn stop(&mut self) {
        if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&self.socket) {
            use std::io::Write as _;
            let _ = stream.write_all(&uniterm_proto::encode_frame(
                &uniterm_proto::ClientMessage::KillServer,
            ));
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.stop();
    }
}

const DIALOG: &str = "Would you like to run the following command?\r\n$ git status\r\n› 1. Yes, proceed (y)\r\n  2. No, and tell Codex what to do differently (esc)\r\nPress enter to confirm";

fn delivered(h: &Harness) -> usize {
    count(&events(&h.socket), "PermissionApprovalDelivery")
}
fn wait_delivered(h: &Harness, expected: usize) {
    wait_for("approval delivery", TIMEOUT, || delivered(h) == expected);
}

#[test]
fn delegation_and_workspace_mode_share_one_delivery_and_repaint_dedup() {
    for workspace_on in [false, true] {
        let mut h = Harness::new("codex", 30);
        h.arm();
        h.mode(workspace_on);
        // Explicit off revokes arms, including while already off.
        if !workspace_on {
            h.arm();
        }
        h.screen("dialog", DIALOG);
        wait_delivered(&h, 1);
        wait_until_file_contains(&h.dir.join("answers"), "approved");
        h.screen("repaint", DIALOG);
        holds_for("duplicate approval", SETTLE, || delivered(&h) == 1);
        assert_eq!(
            std::fs::read_to_string(h.dir.join("answers"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        let entries = h.control.delegation_status();
        assert_eq!(entries[0]["answers_used"], 1);
        // Rearming and toggling cannot forget an already delivered prompt.
        h.arm();
        h.mode(false);
        h.mode(true);
        holds_for("toggle repeated approval", SETTLE, || delivered(&h) == 1);
        let log = events(&h.socket);
        let decision = position(&log, 0, |e| e.get("DelegationDecision").is_some());
        let delivery = position(&log, 0, |e| e.get("PermissionApprovalDelivery").is_some());
        assert!(decision < delivery);
    }
}

#[test]
fn delegated_recognition_is_shared_across_provider_identities() {
    for provider in ["claude", "grok", "pi", "gemini", "opencode"] {
        let mut h = Harness::new(provider, 30);
        h.arm();
        let heading = match provider {
            "claude" => "Do you want to proceed?",
            "grok" => "Approval required",
            "pi" => "Project trust",
            "gemini" => "Allow execution?",
            _ => "Permission required",
        };
        h.screen(
            "dialog",
            &format!("{heading}\r\n$ git status\r\n› 1. Yes\r\n  2. No\r\nEsc to cancel"),
        );
        wait_delivered(&h, 1);
        assert_eq!(
            h.control.delegation_status()[0]["answers_used"],
            1,
            "{provider}"
        );
    }
}

#[test]
fn unknown_partial_and_question_screens_never_gain_input_authority() {
    let mut h = Harness::new("codex", 30);
    h.arm();
    for (name, text) in [
        (
            "question",
            "Choose a database\r\n› 1. Postgres\r\n  2. SQLite\r\nEnter to submit answer",
        ),
        (
            "partial",
            "Would you like to run the following command?\r\n$ git status",
        ),
        (
            "quoted",
            "> would you like to run the following command? press enter to confirm",
        ),
    ] {
        h.screen(name, text);
        holds_for("unrecognized prompt received input", SETTLE, || {
            delivered(&h) == 0
        });
    }
    // The normal five-second status dwell cannot bypass recognition either.
    h.control
        .until("permission dwell", |a| a["status"] == "Permission");
    assert_eq!(delivered(&h), 0);
    let explanation = h.control.request(ControlCommand::DelegationExplain {
        pane: PaneId(h.pane),
    });
    assert_eq!(explanation["verdict"]["outcome"], "withhold");
}

#[test]
fn negative_selection_navigates_before_confirming_and_charges_only_submit() {
    let mut h = Harness::new("codex", 2);
    h.arm();
    let no = DIALOG.replace("› 1.", "  1.").replace("  2.", "› 2.");
    h.screen("negative", &no);
    wait_delivered(&h, 1);
    assert_eq!(h.control.delegation_status()[0]["answers_used"], 0);
    // The arrow is in the fake agent's canonical input buffer; clear it
    // before asking the fake agent to paint the acknowledged selection.
    h.control.request(ControlCommand::PaneSend {
        pane: PaneId(h.pane),
        text: "\u{15}".into(),
    });
    h.screen("affirmative", DIALOG);
    wait_delivered(&h, 2);
    assert_eq!(h.control.delegation_status()[0]["answers_used"], 1);
    holds_for("repeated submit", SETTLE, || delivered(&h) == 2);
}

#[test]
fn budget_exhaustion_and_explicit_disarm_override_workspace_mode() {
    let mut h = Harness::new("codex", 1);
    h.mode(true);
    h.arm();
    h.screen("one", DIALOG);
    wait_delivered(&h, 1);
    assert!(
        h.control.delegation_status().is_empty(),
        "spent arm must revoke immediately"
    );
    h.screen("two", &DIALOG.replace("git status", "git diff"));
    holds_for("workspace mode bypassed exhausted budget", SETTLE, || {
        delivered(&h) == 1
    });
    h.arm();
    wait_delivered(&h, 2);
    h.disarm();
    h.screen("three", &DIALOG.replace("git status", "git log"));
    holds_for("workspace mode bypassed disarm", SETTLE, || {
        delivered(&h) == 2
    });
}

#[test]
fn global_off_revokes_existing_arms_even_when_already_off() {
    for enabled in [false, true] {
        let mut h = Harness::new("codex", 30);
        h.mode(enabled);
        h.arm();
        assert_eq!(h.control.delegation_status().len(), 1);
        h.mode(false);
        assert!(h.control.delegation_status().is_empty());
        h.screen("off", DIALOG);
        holds_for("off left automation active", SETTLE, || delivered(&h) == 0);
        let before = count(&events(&h.socket), "DelegationDisarmed");
        h.mode(false);
        assert_eq!(count(&events(&h.socket), "DelegationDisarmed"), before);
    }
}

#[test]
fn manual_approval_is_available_after_explicit_disarm() {
    let mut h = Harness::new("codex", 30);
    h.mode(true);
    h.disarm();
    h.screen("dialog", DIALOG);
    let agent = h
        .control
        .until("permission", |a| a["status"] == "Permission");
    assert_eq!(delivered(&h), 0);
    h.control.request(ControlCommand::WaitingAct {
        id: agent["waiting"].as_u64().unwrap(),
        action: WaitingAction::Approve,
        text: String::new(),
    });
    wait_delivered(&h, 1);
}

#[test]
fn dismissal_does_not_recreate_a_prompt_without_provider_progress() {
    let mut h = Harness::new("codex", 30);
    h.screen("dialog", DIALOG);
    let agent = h
        .control
        .until("permission", |a| a["status"] == "Permission");
    h.control.request(ControlCommand::WaitingAct {
        id: agent["waiting"].as_u64().unwrap(),
        action: WaitingAction::Dismiss,
        text: String::new(),
    });
    holds_for("dismiss recreated waiting item", SETTLE, || {
        h.control.waiting().is_empty()
    });
}

#[test]
fn confirmation_and_workspace_scope_are_enforced() {
    let mut h = Harness::new("codex", 30);
    let error = h.control.request_raw(ControlCommand::DelegationArm {
        pane: PaneId(h.pane),
        confirmed: false,
    });
    assert_eq!(error["error"]["code"], "confirmation_required");
    assert!(h.control.delegation_status().is_empty());
    let original = h.control.workspace.clone();
    h.control.workspace = "another-workspace".into();
    let error = h.control.request_raw(ControlCommand::DelegationArm {
        pane: PaneId(h.pane),
        confirmed: true,
    });
    assert_eq!(error["error"]["code"], "workspace_mismatch");
    h.control.workspace = original;
    assert!(h.control.delegation_status().is_empty());
}

#[test]
fn same_process_session_replacement_revokes_the_old_arm() {
    let mut h = Harness::new("codex", 30);
    h.arm();
    h.screen("replacement", "\x1b]777;notify;uniterm://cli-agent;{\"agent\":\"codex\",\"event\":\"session_start\",\"session_id\":\"replacement\"}\x07New session");
    h.control.until_bound("replacement");
    assert!(h.control.delegation_status().is_empty());
    h.screen("dialog", DIALOG);
    holds_for("old arm survived session replacement", SETTLE, || {
        delivered(&h) == 0
    });
}

#[test]
fn restart_never_restores_workspace_mode_or_pane_consent() {
    let mut h = Harness::new("codex", 30);
    h.mode(true);
    h.arm();
    h.stop();
    let socket = h.socket.clone();
    h.thread = Some(std::thread::spawn(move || {
        let (mut server, mut poll) =
            uniterm_server::Server::bind(&socket, "/bin/sh", &[], 120, 30).unwrap();
        server.run(&mut poll).unwrap();
    }));
    h.control = Control::connect(&h.socket);
    h.pane = h.control.request(ControlCommand::PaneList)["panes"][0]["id"]
        .as_u64()
        .unwrap();
    assert!(h.control.delegation_status().is_empty());
    assert_eq!(
        h.control.request(ControlCommand::AutoApprove {
            enabled: None,
            confirmed: false
        })["enabled"],
        false
    );
    let start = h.dir.join("start");
    h.run(&format!(
        "stty -echo; {}; while IFS= read -r file; do [ -z \"$file\" ] || cat \"$file\"; done",
        cat(&[&start])
    ));
    h.control.until_bound("owned");
    let before = delivered(&h);
    h.screen("after-restart", DIALOG);
    holds_for("restart retained approval authority", SETTLE, || {
        delivered(&h) == before
    });
}
