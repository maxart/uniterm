use super::{Manifest, Rule, RuleAnchor, RuleRegion};
use uniterm_core::AgentStatus;

/// Codex resumes a session by id with `codex resume <id>`.
pub const RESUME: &[&str] = &["codex", "resume"];

/// Rollouts distinguish actual user turns from injected user-role context.
pub(super) fn task_observation(
    record: &serde_json::Value,
    turn: &mut Option<String>,
) -> Option<uniterm_core::tasks::TaskObservation> {
    use uniterm_core::tasks::TaskObservation;
    let payload = record.get("payload")?;
    match record.get("type")?.as_str()? {
        "event_msg" => {
            let id = super::observed_id(payload.get("turn_id"))
                .or_else(|| super::observed_id(payload.pointer("/turn_attribution/turn_id")));
            match payload.get("type")?.as_str()? {
                "task_started" | "turn_started" => {
                    *turn = id;
                    None
                }
                "user_message" => {
                    let text = payload.get("message")?.as_str()?.to_owned();
                    super::observed_prompt(id.or_else(|| turn.clone()), text)
                }
                "task_complete" | "turn_complete" => Some(TaskObservation::Finished {
                    id: id.or_else(|| turn.clone()),
                }),
                "turn_aborted" | "task_aborted" => Some(TaskObservation::Interrupted {
                    id: id.or_else(|| turn.clone()),
                }),
                _ => None,
            }
        }
        "turn_context" => {
            *turn = super::observed_id(payload.get("turn_id"));
            None
        }
        "response_item"
            if payload.get("type")?.as_str()? == "message"
                && payload.get("role")?.as_str()? == "user" =>
        {
            let id = super::observed_id(
                payload.pointer("/internal_chat_message_metadata_passthrough/turn_id"),
            )
            .or_else(|| turn.clone());
            super::observed_prompt(id, super::observed_text(payload.get("content")?)?)
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
    id: "codex",
    executables: &["codex"],
    rules: &[
        Rule {
            status: AgentStatus::Permission,
            patterns: &["action required"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Title,
            priority: 110,
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
            patterns: &[
                "would you like to run the following command?",
                "would you like to make the following edits?",
                "would you like to send input to the existing terminal?",
                "would you like to send input to terminal ",
                "do you want to approve network access to ",
                "press enter to confirm",
                "allow codex to",
                "allow command?",
                "trust this folder?",
            ],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 120,
        },
        Rule {
            status: AgentStatus::Question,
            patterns: &[
                "waiting for your response",
                "answer the question",
                "enter to submit answer",
                "enter to submit all",
            ],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 115,
        },
        Rule {
            status: AgentStatus::Error,
            patterns: &["stream disconnected", "usage limit", "failed to"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 80,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &["working", "thinking"],
            anchor: RuleAnchor::SpinnerLine,
            region: RuleRegion::Bottom,
            priority: 75,
        },
        Rule {
            status: AgentStatus::Working,
            patterns: &["running command"],
            anchor: RuleAnchor::LineStart,
            region: RuleRegion::Bottom,
            priority: 60,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["codex>"],
            anchor: RuleAnchor::LineStart,
            region: RuleRegion::Bottom,
            priority: 20,
        },
        Rule {
            status: AgentStatus::Idle,
            patterns: &["ask codex"],
            anchor: RuleAnchor::Anywhere,
            region: RuleRegion::Bottom,
            priority: 10,
        },
    ],
};

/// Codex rollout headers declare identity and explicit spawned-thread ancestry.
pub(super) fn session_identity(line: &str) -> Option<super::sessions::Identity> {
    #[derive(serde::Deserialize)]
    struct Header {
        #[serde(rename = "type")]
        kind: String,
        payload: Metadata,
    }
    #[derive(serde::Deserialize)]
    struct Metadata {
        id: String,
        #[serde(default)]
        parent_thread_id: Option<String>,
        #[serde(default)]
        agent_nickname: Option<String>,
        #[serde(default)]
        agent_role: Option<String>,
        #[serde(default)]
        source: serde_json::Value,
    }
    // Unknown payload fields include long instructions. Deserialize only
    // identity fields, so prompts are neither retained nor projected.
    let record: Header = serde_json::from_str(line).ok()?;
    if record.kind != "session_meta" {
        return None;
    }
    let payload = record.payload;
    let id = payload.id;
    if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return None;
    }
    let parent = payload
        .parent_thread_id
        .as_deref()
        .or_else(|| {
            payload
                .source
                .pointer("/subagent/thread_spawn/parent_thread_id")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            payload
                .source
                .pointer("/subagent/thread_spawn/parent_session_id")
                .and_then(serde_json::Value::as_str)
        });
    if parent.is_some_and(|parent| {
        parent.is_empty()
            || parent.len() > 512
            || parent == id
            || parent.chars().any(char::is_control)
    }) {
        return None;
    }
    let parent = parent.map(str::to_owned);
    // Other subagent sources (review/compact) are not root TUI sessions.
    if payload.source.pointer("/subagent").is_some() && parent.is_none() {
        return None;
    }
    let work_summary = parent.as_ref().and_then(|_| {
        let source = payload.source.pointer("/subagent/thread_spawn");
        let nickname = payload
            .agent_nickname
            .as_deref()
            .or_else(|| source?.get("agent_nickname")?.as_str())
            .and_then(super::sessions::metadata_label);
        let role = payload
            .agent_role
            .as_deref()
            .or_else(|| source?.get("agent_role")?.as_str())
            .and_then(super::sessions::metadata_label);
        match (nickname, role) {
            (Some(name), Some(role)) => {
                super::sessions::metadata_label(&format!("{name} ({role})"))
            }
            (name, role) => name.or(role),
        }
    });
    Some(super::sessions::Identity {
        id,
        parent,
        work_summary,
    })
}

pub(super) fn session_watch_root(path: &std::path::Path) -> Option<std::path::PathBuf> {
    path.ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "sessions"))
        .map(std::path::Path::to_owned)
}

/// Native storage root for an exact process, respecting per-invocation homes.
pub(super) fn session_root(pid: Option<i32>) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(pid) = pid {
        use std::io::Read;
        if let Ok(file) = std::fs::File::open(format!("/proc/{pid}/environ")) {
            let mut bytes = Vec::new();
            if file.take(128 * 1024).read_to_end(&mut bytes).is_ok() {
                if let Some(home) = bytes
                    .split(|byte| *byte == 0)
                    .find_map(|entry| entry.strip_prefix(b"CODEX_HOME="))
                {
                    use std::os::unix::ffi::OsStrExt;
                    return Some(
                        std::path::Path::new(std::ffi::OsStr::from_bytes(home)).join("sessions"),
                    );
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
    std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".codex"))
        })
        .map(|home| home.join("sessions"))
}

/// Resolve only a known identity, never the newest file or shared cwd.
/// Header validation follows in the generic observer before publication.
pub(super) fn session_path(root: &std::path::Path, id: &str) -> Option<std::path::PathBuf> {
    if id.is_empty()
        || id.len() > 512
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return None;
    }
    let suffix = format!("-{id}.jsonl");
    let mut directories = vec![(root.to_owned(), 0)];
    while let Some((directory, depth)) = directories.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        let mut paths = Vec::new();
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().ends_with(&suffix) {
                return Some(entry.path());
            }
            if depth < 3 && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                paths.push((entry.path(), depth + 1));
            }
        }
        paths.sort(); // Visit recent year/month/day directories first.
        directories.extend(paths);
    }
    None
}

pub(super) fn global_files() -> Vec<(String, std::path::PathBuf)> {
    let root = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".codex"))
        });
    root.map(|root| {
        vec![
            ("Codex user instructions".into(), root.join("AGENTS.md")),
            (
                "Codex instruction override".into(),
                root.join("AGENTS.override.md"),
            ),
            ("Codex user settings".into(), root.join("config.toml")),
        ]
    })
    .unwrap_or_default()
}

/// Interactive CLI arguments, kept beside this provider's detection rules.
pub(super) const LAUNCH: super::LaunchSpec = super::LaunchSpec {
    prefix: &[],
    prompt_flag: None,
    model_flag: Some("--model"),
    profile_flag: Some("--profile"),
};

// Codex paints its startup picker at the top of the alternate screen while
// leaving the cursor near the bottom. Check the fixed heading before reading
// outside the usual evidence window. Ordinary output takes a single cell check.
pub(super) fn folder_access_surface(term: &crate::terminal::Terminal) -> bool {
    let grid = term.grid();
    const HEADING: &[u8] = b"  Folder access";
    term.is_alt_screen()
        && grid.height() >= 2
        && usize::from(grid.width()) >= HEADING.len()
        && grid.get(2, 1).ch == 'F'
        && HEADING
            .iter()
            .enumerate()
            .all(|(x, byte)| grid.get(x as u16, 1).ch == char::from(*byte))
}

// The saved decision concerns the one displayed absolute path. A wrapped
// continuation between that path and the question invalidates the layout.
pub(super) fn complete_folder_access(screen: &str) -> bool {
    let mut rows = screen.lines().map(str::trim).filter(|row| !row.is_empty());
    if rows.next() != Some("Folder access")
        || !rows.next().is_some_and(|row| row.starts_with('/'))
        || !rows
            .next()
            .is_some_and(|row| row.starts_with("Trust this folder?"))
    {
        return false;
    }
    let footer = rows.next_back();
    let cancel = rows.next_back().map(|row| row.trim_start_matches("› "));
    let accept = rows.next_back().map(|row| row.trim_start_matches("› "));
    accept == Some("1. Trust and continue")
        && matches!(
            (cancel, footer),
            (Some("2. Quit"), Some("enter continue · esc quit"))
                | (
                    Some("2. Back to Agent Command Center"),
                    Some("enter continue · esc back")
                )
        )
}

// A native command sheet occupies the bottom edge of the alternate screen.
// The screen before it can contain a transcript of unrelated earlier choices.
pub(super) const COMMAND_HEADING: &str = "  Would you like to run the following command?\n";
const COMMAND_FOOTER: &str = "  Press enter to confirm or esc to cancel";

pub(super) fn may_show_command(term: &crate::terminal::Terminal) -> bool {
    let grid = term.grid();
    term.is_alt_screen()
        && (term.terminal_title().starts_with("[ ! ] Action Required")
            || (grid.width() > 2 && grid.get(2, grid.height() - 1).ch == 'P'))
}

pub(super) fn command_choice(
    term: &crate::terminal::Terminal,
    dialog: &str,
) -> Option<super::Approval> {
    if term.cursor_visible() {
        return None;
    }
    let (body, _) = dialog.strip_suffix(COMMAND_FOOTER)?.rsplit_once('\n')?;
    let mut lines = body.lines().collect::<Vec<_>>();
    while lines.last() == Some(&"") {
        lines.pop();
    }
    let choices_continue = lines.last() == Some(&"↓");
    if choices_continue {
        lines.pop();
    }
    let first = lines
        .iter()
        .rposition(|line| matches!(*line, "› 1. Yes, proceed (y)" | "  1. Yes, proceed (y)"))?;
    if !lines[..first].iter().any(|line| line.starts_with("  $ ")) {
        return None;
    }
    let mut selected = None;
    let mut persistent = false;
    let mut denial = false;
    let mut selected_row = 0;
    for (offset, line) in lines[first..].iter().enumerate() {
        // Wrapped explanations belong to the preceding menu item. Command
        // contents above this block can freely contain '>' or quoted choices.
        if line.starts_with("     ") && (persistent || denial) {
            continue;
        }
        let label = line
            .strip_prefix("› ")
            .or_else(|| line.strip_prefix("  "))?;
        let index = if offset == 0 {
            1
        } else if !persistent && !denial && label.starts_with("2. Yes, and don't ask again") {
            persistent = true;
            2
        } else if !denial
            && (label == "2. No, and tell Codex what to do differently (esc)" && !persistent
                || label == "3. No, and tell Codex what to do differently (esc)" && persistent)
        {
            denial = true;
            if persistent {
                3
            } else {
                2
            }
        } else {
            return None;
        };
        if line.starts_with('›') {
            if selected.replace(index).is_some() {
                return None;
            }
            let below = dialog.lines().count() - 1 - first - offset;
            selected_row = usize::from(term.grid().height()) - 1 - below;
        }
    }
    // When the command is folded, Codex can clip the denial completely
    // while leaving the one-time choice and a wrapped persistent grant visible.
    let folded = lines[..first].last().is_some_and(|line| {
        line.trim().starts_with("[… ") && line.trim().ends_with(" lines] ^a view all")
    });
    // A short viewport can reserve its last menu row for a scroll arrow.
    // Submit only a visible highlighted one-time choice in that layout.
    let known_choices = if choices_continue {
        persistent && !denial && selected == Some(1)
    } else {
        denial || persistent && folded
    };
    if !known_choices {
        return None;
    }
    let selected = selected?;
    let grid = term.grid();
    let marker = grid.get(0, selected_row as u16);
    let heading_y = usize::from(grid.height()).checked_sub(dialog.lines().count() + 1)?;
    let heading = grid.get(2, heading_y as u16);
    // The native sheet is one continuous fill from the bold heading to the
    // footer. Stale transcript text and other controls cannot supply its body.
    if marker.ch != '›'
        || marker.attrs.0 & uniterm_core::Attrs::BOLD.0 == 0
        || heading.attrs.0 & uniterm_core::Attrs::BOLD.0 == 0
        || marker.bg == heading.bg && marker.attrs.0 & uniterm_core::Attrs::REVERSE.0 == 0
        || (heading_y as u16..grid.height() - 1)
            .any(|y| y != selected_row as u16 && grid.get(0, y).bg != heading.bg)
    {
        return None;
    }
    // Keep all displayed command and reason text verbatim in the identity.
    // Selection highlighting and navigation leave this prefix unchanged.
    let request = lines[..first].join("\n");
    let mut choice = super::approval::fingerprint(&[COMMAND_HEADING, &request], b"\r");
    choice.watch_for_dismissal = true;
    if selected != 1 {
        choice.keys = b"\x1b[A".to_vec();
        choice.navigation = Some(selected);
    }
    choice.settle = std::time::Duration::from_millis(150);
    Some(choice)
}

/// A submitted sheet returns to a visible composer and prints a one-time approval receipt.
pub(super) fn command_dismissed(term: &crate::terminal::Terminal) -> bool {
    if !term.is_alt_screen() || !term.cursor_visible() {
        return false;
    }
    let (_, y) = term.cursor();
    let grid = term.grid();
    if y >= grid.height() || grid.get(0, y).ch != '›' {
        return false;
    }
    let screen = term.bottom_text(48);
    let mut rows = screen.trim_end().lines();
    rows.next_back().is_some_and(|line| {
        line.trim()
            .strip_prefix("← for agents")
            .is_some_and(|rest| {
                let rest = rest.trim_start();
                rest.is_empty()
                    || rest.starts_with("· ? for shortcuts")
                    || rest.starts_with("⚠ ") && rest.ends_with(" · f2 to view")
            })
    }) && !screen.contains(COMMAND_HEADING)
        && one_time_receipt(rows)
}

// Codex wraps its shortened command snippet with a two-cell hanging indent.
// The observed 56-column receipt occupies three rows and the 80-column one uses two.
fn one_time_receipt<'a>(rows: impl Iterator<Item = &'a str>) -> bool {
    let mut continuations = 0;
    for line in rows {
        if line.starts_with("✔ You approved codex to run ") {
            continuations = 2;
        } else if continuations > 0
            && line
                .strip_prefix("  ")
                .is_some_and(|rest| !rest.is_empty() && !rest.starts_with(char::is_whitespace))
        {
            continuations -= 1;
        } else {
            continuations = 0;
            continue;
        }
        if line.ends_with(" this time") {
            return true;
        }
    }
    false
}

// Approval contracts stay beside the provider UI vocabulary.
pub(super) const APPROVAL_DIALOGS: &[super::approval::Dialog] = &[super::approval::Dialog {
    headings: &[
        "trust this folder?",
        "do you trust the contents of this directory?",
    ],
    allow: &[
        "trust and continue",
        "yes, continue",
        "yes, i trust this folder",
    ],
    deny: &["back to agent command center", "quit", "no, quit"],
    alternatives: &[],
    kind: super::approval::ApprovalKind::WorkspaceTrust,
}];

/// Codex carries the current task before an optional directory suffix.
pub(super) fn task_title(title: &str) -> &str {
    title.strip_prefix("Codex: ").unwrap_or(title)
}

#[cfg(test)]
mod approval_tests {
    use crate::providers::permission_approval;
    use crate::terminal::Terminal;

    const FOLDED: &[u8] = include_bytes!("../../tests/fixtures/approval/codex-command-folded.ansi");

    #[test]
    fn compact_native_receipt_returns_to_the_composer_with_shortened_footer() {
        let mut term = Terminal::new(56, 23);
        term.feed(include_bytes!(
            "../../tests/fixtures/approval/codex-command-dismissed-compact.ansi"
        ));
        assert!(super::command_dismissed(&term));
    }

    #[test]
    fn native_receipt_wraps_with_hanging_indent_at_56_and_80_columns() {
        for (width, height, capture) in [
            (
                56,
                23,
                include_bytes!("../../tests/fixtures/approval/codex-receipt-56.ansi").as_slice(),
            ),
            (
                80,
                24,
                include_bytes!("../../tests/fixtures/approval/codex-receipt-80.ansi").as_slice(),
            ),
        ] {
            let mut term = Terminal::new(width, height);
            term.feed(capture);
            assert!(super::command_dismissed(&term));
        }
        for receipt in [
            "✔ You approved codex to run env\nnext line this time",
            "✔ You approved codex to run env\n\n  other text this time",
            "✔ You approved codex to run env\n  more words\nother text this time",
        ] {
            assert!(!super::one_time_receipt(receipt.lines()));
        }
    }

    #[test]
    fn native_command_approval_uses_the_live_selection_for_folded_and_tall_commands() {
        for (width, height, capture) in [
            (80, 24, FOLDED),
            (
                120,
                90,
                include_bytes!("../../tests/fixtures/approval/codex-command-tall.ansi").as_slice(),
            ),
            (
                80,
                24,
                include_bytes!("../../tests/fixtures/approval/codex-command-quoted.ansi")
                    .as_slice(),
            ),
        ] {
            let mut term = Terminal::new(width, height);
            term.feed(capture);
            let once = permission_approval("codex", &term).unwrap();
            assert_eq!(once.keys, b"\r");
            assert_eq!(once.settle, std::time::Duration::from_millis(150));
            assert!(once.navigation.is_none());
            assert_eq!(
                permission_approval("codex", &term).unwrap().fingerprint,
                once.fingerprint
            );
            let row = if height == 90 || capture == FOLDED {
                8
            } else {
                14
            };
            term.feed(format!("\x1b[{row};15Hchanged request").as_bytes());
            assert_ne!(
                permission_approval("codex", &term).unwrap().fingerprint,
                once.fingerprint
            );
        }
    }

    #[test]
    fn native_command_approval_accepts_wrapped_redirection_with_complete_footer() {
        let mut term = Terminal::new(130, 32);
        term.feed(include_bytes!(
            "../../tests/fixtures/approval/codex-command-wrapped-redirection.ansi"
        ));
        let once = permission_approval("codex", &term).unwrap();
        assert_eq!(once.keys, b"\r");
        assert!(once.navigation.is_none());
        assert!(term
            .dump_text()
            .contains("\n  > /tmp/pr33-candidate-release.log 2>&1"));
        // A screenshot can crop the footer while Uniterm still owns it. A
        // genuinely absent footer must leave the same selected choice alone.
        term.feed(b"\x1b[32;1H\x1b[2K");
        assert!(permission_approval("codex", &term).is_none());
    }

    #[test]
    fn native_command_approval_requires_visible_command_and_choice_above_scroll_hint() {
        let capture = include_bytes!("../../tests/fixtures/approval/codex-command-scroll.ansi");
        for erase in [
            b"\x1b[17;1H\x1b[2K".as_slice(),
            b"\x1b[9;1H\x1b[2K",
            b"\x1b[14;1H\x1b[2K",
        ] {
            let mut term = Terminal::new(130, 18);
            term.feed(capture);
            assert_eq!(permission_approval("codex", &term).unwrap().keys, b"\r");
            term.feed(erase);
            assert!(permission_approval("codex", &term).is_none());
        }
    }

    #[test]
    fn native_command_approval_rejects_stale_and_ambiguous_screens() {
        for change in [
            b"\x1b[?25h".as_slice(),
            b"\x1b[24;1H\x1b[2K",
            b"\x1b[24;1H\x1b[2K> A newer prompt",
            b"\x1b[21;1H\x1b[0m",
            b"\x1b[20;1H\x1b[2K",
            b"\x1b[8;1H\x1b[0m ",
            b"\x1b[8;3HBlocked by policy",
            b"\x1b[23;1H> Another selection",
        ] {
            let mut term = Terminal::new(80, 24);
            term.feed(FOLDED);
            term.feed(change);
            if change.ends_with(b"[0m") {
                term.feed("›".as_bytes());
            }
            assert!(permission_approval("codex", &term).is_none(), "{change:?}");
        }
    }

    #[test]
    fn native_command_approval_navigates_off_a_persistent_grant_before_confirming() {
        let mut term = Terminal::new(80, 24);
        term.feed(FOLDED);
        let once = permission_approval("codex", &term).unwrap();
        term.feed(
            "\x1b[21;1H\x1b[22m\x1b[39;48;5;234m  \x1b[22;1H\x1b[1m\x1b[38;5;232;48;5;75m› "
                .as_bytes(),
        );
        let persistent = permission_approval("codex", &term).unwrap();
        assert_eq!(persistent.keys, b"\x1b[A");
        assert_eq!(persistent.fingerprint, once.fingerprint);
        assert_eq!(persistent.navigation, Some(2));
    }
}

#[cfg(test)]
mod session_tests {
    #[test]
    fn native_child_labels_use_only_explicit_nickname_and_role() {
        let child = super::session_identity(r#"{"type":"session_meta","payload":{"id":"child","parent_thread_id":"root","agent_nickname":"Atlas","agent_role":"reviewer","base_instructions":{"text":"PRIVATE PROMPT"},"first_user_message":"PRIVATE REQUEST"}}"#).unwrap();
        assert_eq!(child.work_summary.as_deref(), Some("Atlas (reviewer)"));
        let child = super::session_identity(r#"{"type":"session_meta","payload":{"id":"child","source":{"subagent":{"thread_spawn":{"parent_thread_id":"root","agent_nickname":"Finch","agent_role":"explorer"}}}}}"#).unwrap();
        assert_eq!(child.work_summary.as_deref(), Some("Finch (explorer)"));
        let child = super::session_identity(r#"{"type":"session_meta","payload":{"id":"child","parent_thread_id":"root","prompt":"PRIVATE PROMPT","description":"not a declared rollout field"}}"#).unwrap();
        assert_eq!(child.work_summary, None);
        let root = super::session_identity(r#"{"type":"session_meta","payload":{"id":"root","agent_nickname":"not child work","source":"cli"}}"#).unwrap();
        assert_eq!(root.work_summary, None);
        let line = serde_json::json!({"type":"session_meta","payload":{"id":"child","parent_thread_id":"root","agent_role":"\u{001b}\u{0007}研究".repeat(300)}}).to_string();
        let child = super::session_identity(&line).unwrap();
        let label = child.work_summary.unwrap();
        assert_eq!(label.chars().count(), 200);
        assert!(!label.chars().any(char::is_control));
    }

    #[test]
    fn known_session_lookup_searches_past_large_history() {
        let root =
            std::env::temp_dir().join(format!("uniterm-session-lookup-{}", std::process::id()));
        let recent = root.join("2026/10/06");
        let older = root.join("2025/01/01");
        std::fs::create_dir_all(&recent).unwrap();
        std::fs::create_dir_all(&older).unwrap();
        for id in 0..4100 {
            std::fs::write(recent.join(format!("rollout-{id}.jsonl")), "").unwrap();
        }
        let expected = older.join("rollout-target.jsonl");
        std::fs::write(&expected, "").unwrap();
        assert_eq!(super::session_path(&root, "target"), Some(expected));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rollout_identity_preserves_explicit_parent_and_ignores_ephemeral_sessions() {
        let root = super::session_identity(
            r#"{"type":"session_meta","payload":{"id":"root","source":"cli"}}"#,
        )
        .unwrap();
        assert_eq!(root.id, "root");
        assert_eq!(root.parent, None);
        let child = super::session_identity(r#"{"type":"session_meta","payload":{"id":"child","source":{"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}}}"#).unwrap();
        assert_eq!(child.parent.as_deref(), Some("root"));
        assert!(super::session_identity(
            r#"{"type":"session_meta","payload":{"id":"review","source":{"subagent":"review"}}}"#
        )
        .is_none());
        assert!(
            super::session_identity(r#"{"type":"response_item","payload":{"id":"guess"}}"#)
                .is_none()
        );
    }
}
