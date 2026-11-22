use super::{Manifest, Rule, RuleAnchor, RuleRegion};
use uniterm_core::AgentStatus;

/// Claude Code resumes a session by id with `claude --resume <id>`.
pub const RESUME: &[&str] = &["claude", "--resume"];

/// Native root messages expose user text and explicit assistant end-turn evidence.
pub(super) fn task_observation(
    record: &serde_json::Value,
    turn: &mut Option<String>,
) -> Option<uniterm_core::tasks::TaskObservation> {
    use uniterm_core::tasks::TaskObservation;
    if record
        .get("isSidechain")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
        || record.get("isMeta").and_then(serde_json::Value::as_bool) == Some(true)
        || record
            .get("isCompactSummary")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        || record.get("agentId").is_some_and(|v| !v.is_null())
    {
        return None;
    }
    let message = record.get("message")?;
    match record.get("type")?.as_str()? {
        "user" => {
            let text = super::observed_text(message.get("content")?)?;
            let id = super::observed_id(record.get("uuid"));
            let observation = super::observed_prompt(id.clone(), text)?;
            *turn = id;
            Some(observation)
        }
        "assistant"
            if record
                .get("isApiErrorMessage")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
                && message
                    .get("stop_reason")
                    .and_then(serde_json::Value::as_str)
                    == Some("end_turn") =>
        {
            Some(TaskObservation::Finished { id: turn.clone() })
        }
        _ => None,
    }
}

// Priorities: visible blocked prompts beat activity titles (120 to 115);
// activity titles beat grid activity (110 to 100). Activity is anchored to a spinner or a
// line start so typed prompt text cannot impersonate it (75 to 60), and idle
// hints are the weakest positive signal (20 to 10). No rule matching means
// idle: the matcher never treats output volume as evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "claude",
    executables: &["claude"],
    rules: &[
        Rule {
            status: AgentStatus::Working,
            patterns: &[""],
            anchor: RuleAnchor::AnimatedTitle,
            region: RuleRegion::Title,
            priority: 110,
        },
        Rule {
            status: AgentStatus::Permission,
            patterns: &[
                "do you want to proceed?",
                "would you like to proceed?",
                "do you want to make this edit to ",
                "do you want to create ",
                "do you want to overwrite ",
                "do you want to allow claude to fetch this content?",
                "allow this tool",
                "do you want to allow this connection?",
                "waiting for permission",
                "ctrl+e to explain",
                "yes, and don't ask again",
                "esc to cancel · tab to amend",
            ],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 120,
        },
        Rule {
            status: AgentStatus::Question,
            patterns: &["enter to select", "type your answer"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 115,
        },
        Rule {
            status: AgentStatus::Error,
            patterns: &["api error", "authentication failed", "rate limit reached"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 80,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &[""],
            anchor: RuleAnchor::SpinnerActivity,
            region: RuleRegion::Bottom,
            priority: 75,
        },
        Rule {
            status: AgentStatus::Tool,
            patterns: &["running…"],
            anchor: RuleAnchor::LineStart,
            region: RuleRegion::Bottom,
            priority: 70,
        },
        Rule {
            status: AgentStatus::Tool,
            patterns: &["running..."],
            anchor: RuleAnchor::LineStart,
            region: RuleRegion::Bottom,
            priority: 70,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["what can i help you with?"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 20,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["claude code"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 10,
        },
    ],
};

/// Conversation ids and subagent ids occupy different namespaces in Claude.
/// A message's parentUuid links messages, never agents.
pub(super) fn session_identity(
    line: &str,
    path: &std::path::Path,
) -> Option<super::sessions::Identity> {
    #[derive(serde::Deserialize)]
    struct Record {
        #[serde(rename = "type")]
        kind: String,
        #[serde(rename = "sessionId")]
        session: String,
        #[serde(default, rename = "isSidechain")]
        sidechain: bool,
        #[serde(default, rename = "agentId")]
        agent: Option<String>,
    }
    // Typed deserialization skips message bodies, including private prompts.
    let record: Record = serde_json::from_str(line).ok()?;
    if !matches!(record.kind.as_str(), "user" | "assistant" | "system")
        || !session_component(&record.session)
    {
        return None;
    }
    let subagents = path
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "subagents"));
    if subagents.is_none() && !record.sidechain && record.agent.is_none() {
        return (path.file_stem()?.to_str()? == record.session).then_some(
            super::sessions::Identity {
                id: record.session,
                parent: None,
                work_summary: None,
            },
        );
    }
    // Native storage identifies a child even in versions which omit the
    // redundant isSidechain/agentId fields from transcript records.
    let agent = path.file_stem()?.to_str()?.strip_prefix("agent-")?;
    if !session_component(agent) || record.agent.as_deref().is_some_and(|id| id != agent) {
        return None;
    }
    let owner = subagents?.parent()?.file_name()?.to_str()?;
    if owner != record.session {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct Metadata {
        #[serde(default, rename = "parentAgentId")]
        parent: Option<String>,
        #[serde(rename = "toolUseId")]
        tool: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default, rename = "agentType")]
        agent_type: Option<String>,
    }
    // Missing sidecars do not establish the immediate spawning agent. Wait
    // for their create/write event instead of inventing an edge to the root.
    use std::io::Read;
    let metadata = path.with_extension("meta.json");
    if std::fs::symlink_metadata(&metadata)
        .ok()?
        .file_type()
        .is_symlink()
    {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(metadata)
        .ok()?
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 16 * 1024 {
        return None;
    }
    let metadata: Metadata = serde_json::from_slice(&bytes).ok()?;
    if metadata.tool.is_empty() || metadata.tool.len() > 512 {
        return None;
    }
    let id = subagent_identity(owner, agent)?;
    let work_summary = metadata
        .description
        .as_deref()
        .and_then(super::sessions::metadata_label)
        .or_else(|| {
            metadata
                .name
                .as_deref()
                .and_then(super::sessions::metadata_label)
        })
        .or_else(|| {
            metadata
                .agent_type
                .as_deref()
                .and_then(super::sessions::metadata_label)
        });
    let parent = match metadata.parent {
        Some(parent) => subagent_identity(owner, &parent)?,
        None => owner.to_owned(),
    };
    (id != parent).then_some(super::sessions::Identity {
        id,
        parent: Some(parent),
        work_summary,
    })
}

fn session_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn subagent_identity(session: &str, agent: &str) -> Option<String> {
    session_component(agent).then(|| format!("{session}/agent/{agent}"))
}

pub(super) fn session_watch_root(path: &std::path::Path) -> Option<std::path::PathBuf> {
    // One exact project store, including newly created session/subagents dirs.
    // A hook path is authoritative; no cwd-to-project slug guessing is needed.
    Some(path.parent()?.to_owned())
}

pub(super) fn session_event_path(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".meta.json")?;
    stem.starts_with("agent-")
        .then(|| path.with_file_name(format!("{stem}.jsonl")))
}

pub(super) fn global_files() -> Vec<(String, std::path::PathBuf)> {
    let mut files = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let root = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(home).join(".claude"));
        files.push(("Claude user instructions".into(), root.join("CLAUDE.md")));
        files.push(("Claude user settings".into(), root.join("settings.json")));
    }
    let system = if cfg!(target_os = "macos") {
        "/Library/Application Support/ClaudeCode"
    } else {
        "/etc/claude-code"
    };
    files.push((
        "Claude system instructions".into(),
        std::path::Path::new(system).join("CLAUDE.md"),
    ));
    files
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
    headings: &[
        "do you trust the files in this folder?",
        "accessing workspace:",
        "quick safety check:",
    ],
    allow: &["yes, i trust this folder", "yes, trust this folder"],
    deny: &["no, exit", "no, continue without these permissions"],
    alternatives: &[],
    kind: super::approval::ApprovalKind::WorkspaceTrust,
}];

const AUTO_MODE_HEADING: &str = "teach auto mode about your environment?";
const AUTO_MODE_BODY: &str =
    "auto mode works better when it knows your environment. takes about a minute.";
const AUTO_MODE_FOOTER: &str = "enter to confirm · esc to cancel";
const AUTO_MODE_STATUS: &str = "⏵⏵ auto mode on (shift+tab to cycle)";

/// Keep the newest complete heading as the boundary between this modal and older dialogs.
pub(super) fn auto_mode_screen(screen: &str) -> Option<&str> {
    screen
        .rmatch_indices(AUTO_MODE_HEADING)
        .find_map(|(offset, _)| {
            let start = screen[..offset].rfind('\n').map_or(0, |index| index + 1);
            let line = screen[start..].lines().next()?;
            (super::approval::content(line) == AUTO_MODE_HEADING).then_some(&screen[start..])
        })
}

/// A parked cursor or a padded modal can place the heading above the usual status tail.
pub(super) fn auto_mode_evidence(term: &crate::terminal::Terminal, tail: &str) -> Option<String> {
    if (!tail.contains("Enter to confirm · Esc to cancel")
        && !tail.contains("Space to toggle · Esc to cancel")
        && !tail.contains("Enter to continue · Esc to cancel"))
        || tail.contains("Teach auto mode about your environment?")
    {
        return None;
    }
    let screen = term.evidence_text(48);
    screen
        .contains("Teach auto mode about your environment?")
        .then_some(screen)
}

/// Consent and the subsequent scope picker share a heading but ask for different actions.
pub(super) fn auto_mode_status(screen: &str) -> Option<(AgentStatus, &'static str)> {
    let modal = auto_mode_screen(screen)?;
    let mut rows = modal
        .lines()
        .map(super::approval::content)
        .filter(|row| !row.is_empty());
    let footer = rows.next_back()?;
    // A partial consent may still need human attention. It can report status
    // with its own body and footer before every option is eligible for input.
    if auto_mode_selection(modal).is_some()
        || (footer == AUTO_MODE_FOOTER && rows.clone().any(|row| row == AUTO_MODE_BODY))
    {
        return Some((
            AgentStatus::Permission,
            "Teach auto mode about your environment?",
        ));
    }
    (matches!(
        footer,
        "←/→ to change · enter to continue · esc to cancel"
            | "↑/↓ to navigate · space to toggle · esc to cancel"
    ) && rows.any(|row| row.contains("also scan shell history")))
    .then_some((
        AgentStatus::Question,
        "Choose which sources auto mode may scan",
    ))
}

/// Only the three numbered choices in the consent modal can authorize input.
/// Its background composer may contain arbitrary draft text, including a choice label.
fn auto_mode_selection(screen: &str) -> Option<(usize, usize)> {
    use super::approval::content;

    let mut rows = screen
        .split('\n')
        .enumerate()
        .map(|(index, line)| (index, content(line)))
        .filter(|(_, line)| !line.is_empty());
    let (_, heading) = rows.next()?;
    if heading != AUTO_MODE_HEADING {
        return None;
    }
    // A narrow pane can wrap the explanatory paragraph across several rows.
    let mut remaining = AUTO_MODE_BODY;
    while !remaining.is_empty() {
        let (_, row) = rows.next()?;
        remaining = remaining.strip_prefix(row)?.trim_start();
    }
    let mut selected = None;
    for (index, expected) in ["1. yes", "2. not now", "3. don't show again"]
        .iter()
        .enumerate()
    {
        let (line_index, row) = rows.next()?;
        let label = if let Some(label) = row.strip_prefix('❯') {
            if selected.replace((index, line_index)).is_some() {
                return None;
            }
            label.trim_start()
        } else {
            row
        };
        if label != *expected {
            return None;
        }
    }
    let selected = selected?;
    let (_, footer) = rows.next()?;
    if footer != AUTO_MODE_FOOTER {
        return None;
    }
    if let Some((_, draft)) = rows.next() {
        let draft = draft.strip_prefix('❯')?;
        if !draft.is_empty() && !draft.starts_with(char::is_whitespace) {
            return None;
        }
        let (_, status) = rows.next()?;
        let suffix = status.strip_prefix(AUTO_MODE_STATUS)?;
        if !matches!(suffix, "" | " · ← for agents") || rows.next().is_some() {
            return None;
        }
    }

    let (index, row) = selected;
    Some((index, screen.split('\n').count().saturating_sub(row + 1)))
}

/// A manual approval on the last choice requires two repaints before confirmation.
pub(super) fn auto_mode_consent(
    screen: &str,
    focused_marker_from_bottom: Option<usize>,
) -> Option<super::Approval> {
    let (selected, row_from_bottom) = auto_mode_selection(screen)?;
    // Claude parks even its hidden hardware cursor on the active option's arrow.
    // Text left in the transcript above a live composer has no such focus.
    if focused_marker_from_bottom != Some(row_from_bottom) {
        return None;
    }
    // Every selection belongs to the same modal. Keep draft edits, frame padding
    // and arrow movement out of the fingerprint used for manual continuation.
    let keys = if selected == 0 {
        b"\r".as_slice()
    } else {
        b"\x1b[A"
    };
    let mut choice = super::approval::fingerprint(&[AUTO_MODE_HEADING], keys);
    choice.navigation = (selected > 0).then_some(selected as u64);
    // Claude 2.1.329 ignores consent callbacks during the first 500 ms after mount.
    choice.settle = std::time::Duration::from_millis(500);
    Some(choice)
}

/// Preserve the provider's generated task text, excluding its application prefix.
pub(super) fn task_title(title: &str) -> &str {
    title.strip_prefix("Claude Code: ").unwrap_or(title)
}
