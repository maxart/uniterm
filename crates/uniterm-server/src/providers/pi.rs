use super::{Manifest, Rule, RuleAnchor, RuleRegion};
use uniterm_core::AgentStatus;

/// Pi opens a session by id with `pi --session <id>`.
pub const RESUME: &[&str] = &["pi", "--session"];

/// Pi persists accepted user messages separately from extension-owned context.
pub(super) fn task_observation(
    record: &serde_json::Value,
    turn: &mut Option<String>,
) -> Option<uniterm_core::tasks::TaskObservation> {
    if record.get("type")?.as_str()? != "message" {
        return None;
    }
    let message = record.get("message")?;
    if message.get("role")?.as_str()? != "user" {
        return None;
    }
    let id = super::observed_id(record.get("id"));
    let observed =
        super::observed_prompt(id.clone(), super::observed_text(message.get("content")?)?)?;
    *turn = id;
    // Assistant end messages can be followed by automatic retries. Only the
    // extension's settled boundary may finish a Task for this provider.
    Some(observed)
}

// Priorities: visible blocked prompts beat activity titles (120 to 115);
// activity titles beat grid activity (110 to 100). Activity is anchored to a spinner or a
// line start so typed prompt text cannot impersonate it (75 to 60), and idle
// hints are the weakest positive signal (20 to 10). No rule matching means
// idle: the matcher never treats output volume as evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "pi",
    executables: &["pi", "pi-coding-agent"],
    rules: &[
        Rule {
            status: AgentStatus::Working,
            patterns: &["working"],
            anchor: RuleAnchor::SpinnerLine,
            region: RuleRegion::Bottom,
            priority: 75,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &[""],
            anchor: RuleAnchor::SpinnerLine,
            region: RuleRegion::Title,
            priority: 105,
        },
        Rule {
            status: AgentStatus::Permission,
            patterns: &["project trust", "saved decision:", "current session:"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 120,
        },
        Rule {
            status: AgentStatus::Error,
            patterns: &[
                "api error",
                "authentication failed",
                "rate limit",
                "request failed",
                "no api key",
            ],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 80,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &[
                "working...",
                "retrying (",
                "compacting context...",
                "auto-compacting...",
                "summarizing branch...",
            ],
            anchor: RuleAnchor::LineStart,
            region: RuleRegion::Bottom,
            priority: 60,
        },
    ],
};

/// Pi and OMP persist an explicit session header as their first JSONL record.
pub(super) fn session_identity(line: &str) -> Option<super::sessions::Identity> {
    let record: serde_json::Value = serde_json::from_str(line).ok()?;
    if record.get("type")?.as_str()? != "session" {
        return None;
    }
    let id = record.get("id")?.as_str()?;
    if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return None;
    }
    Some(super::sessions::Identity {
        id: id.into(),
        parent: None,
        work_summary: None,
    })
}

/// Interactive CLI arguments, kept beside this provider's detection rules.
pub(super) const LAUNCH: super::LaunchSpec = super::LaunchSpec {
    prefix: &[],
    prompt_flag: None,
    model_flag: Some("--model"),
    profile_flag: None,
};

// Approval contracts stay beside the provider UI vocabulary.
pub(super) const APPROVAL_DIALOGS: &[super::approval::Dialog] = &[super::approval::Dialog {
    headings: &["project trust"],
    allow: &["trust", "trust (this session only)"],
    deny: &["do not trust", "do not trust (this session only)"],
    alternatives: &[],
    kind: super::approval::ApprovalKind::WorkspaceTrust,
}];

/// Pi puts its task summary after a stable application marker.
pub(super) fn task_title(title: &str) -> &str {
    title.strip_prefix("π - ").unwrap_or(title)
}
