//! Conservative permission-dialog recognition at the provider boundary.
//! Detection manifests may identify status; local overrides never gain input authority.

use std::hash::{Hash, Hasher};

/// Input for a complete, recognized dialog, including any choice navigation.
/// The provider contract identifies the affirmative option, never a blind Enter.
pub(crate) struct Approval {
    /// The provider can prove a later return from this native picker to its composer.
    pub watch_for_dismissal: bool,
    pub fingerprint: u64,
    pub keys: Vec<u8>,
    /// Which automatic policy, if any, may authorize this complete dialog.
    pub kind: ApprovalKind,
    /// Exact, untruncated absolute directory shown by a complete trust dialog.
    pub directory: Option<String>,
    /// Selected-row identity for a navigation step; None submits the choice.
    pub navigation: Option<u64>,
    /// Minimum time this selection must remain visible before automatic input.
    pub settle: std::time::Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApprovalKind {
    /// An ordinary tool or command permission.
    Permission,
    /// The current Uniterm Project's directory trust prompt.
    WorkspaceTrust,
}

pub(super) fn content(line: &str) -> &str {
    line.trim()
        .trim_matches([
            '│', '┃', '╭', '╮', '╰', '╯', '┌', '┐', '└', '┘', '┏', '┓', '┗', '┛', '─', '━',
        ])
        .trim()
}

fn option(line: &str) -> (&str, bool) {
    let line = content(line);
    let selected = line.starts_with(['❯', '›', '●', '→', '>']);
    let line = line
        .trim_start_matches(['❯', '›', '●', '○', '→', '>'])
        .trim();
    let line = line
        .split_once(". ")
        .filter(|(number, _)| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
        .map(|(_, text)| text)
        .unwrap_or(line);
    (line, selected)
}

fn choice_matches(label: &str, choices: &[&str]) -> bool {
    choices.iter().any(|choice| {
        choice
            .strip_suffix('*')
            .map_or(label == *choice, |prefix| label.starts_with(prefix))
    })
}

/// One provider-owned dialog contract, independent of the status dwell rules.
pub(super) struct Dialog {
    pub headings: &'static [&'static str],
    pub allow: &'static [&'static str],
    pub deny: &'static [&'static str],
    /// Visible choices that may be crossed but are never authorized.
    pub alternatives: &'static [&'static str],
    pub kind: ApprovalKind,
}

const WORKSPACE_TRUST: Dialog = Dialog {
    headings: &[
        "trust this folder?",
        "trust this workspace?",
        "do you trust this folder?",
        "do you trust this workspace?",
        "do you trust the files in this folder?",
        "do you trust the contents of this directory?",
        "do you trust the authors of the files in this folder?",
        "accessing workspace:",
        "quick safety check:",
        "project trust",
    ],
    allow: &[
        "trust",
        "trust and continue",
        "trust folder (*",
        "trust this workspace",
        "trust (this session only)",
        "yes, continue",
        "yes, i trust this folder",
        "yes, trust this folder",
        "yes, trust this workspace",
    ],
    deny: &[
        "back to agent command center",
        "quit",
        "no, quit",
        "no, exit",
        "no, continue without these permissions",
        "don't trust",
        "do not trust",
        "do not trust (this session only)",
    ],
    alternatives: &["trust parent folder (*"],
    kind: ApprovalKind::WorkspaceTrust,
};

pub(super) fn recognize_grid(
    provider: &str,
    screen: &str,
    focused_marker_from_bottom: Option<usize>,
) -> Option<Approval> {
    let mut approval = recognize_dialog(provider, screen, focused_marker_from_bottom)?;
    if approval.kind == ApprovalKind::WorkspaceTrust {
        approval.directory = trust_directory(screen);
    }
    Some(approval)
}

// A path must immediately follow its dialog heading. Never infer it from cwd,
// a basename, a parent-folder option, or text printed before the dialog.
fn trust_directory(screen: &str) -> Option<String> {
    let lines: Vec<_> = screen
        .lines()
        .map(content)
        .filter(|line| !line.is_empty())
        .collect();
    let mut directory = None;
    let mut folder_access = false;
    let mut awaiting_path = false;
    for line in lines {
        let lower = line.to_lowercase();
        let heading = lower == "folder access"
            || WORKSPACE_TRUST
                .headings
                .iter()
                .any(|heading| lower.starts_with(heading));
        if heading {
            // Codex and Claude put explanatory subheadings after the path.
            if (folder_access && lower.starts_with("trust this folder?"))
                || (directory.is_some() && lower.starts_with("quick safety check:"))
            {
                folder_access = false;
                continue;
            }
            directory = None;
            folder_access = lower == "folder access";
            awaiting_path = true;
        } else if awaiting_path {
            awaiting_path = false;
            if line.starts_with('/')
                && !line.contains(['…', '\t'])
                && !line.contains("...")
                && !line.split('/').any(|part| part == "." || part == "..")
            {
                directory = Some(line.to_string());
            }
        } else if line.starts_with('/') {
            // Multiple standalone paths cannot identify one trust target.
            directory = None;
        }
    }
    directory
}

const COMMAND_HEADINGS: &[&str] = &[
    "allow this command?",
    "would you like to run the following command?",
];

// This vocabulary is shared by bundled providers, including those that only
// advertise process identity. Only a complete trailing command picker qualifies.
pub(super) fn shared_command(screen: &str) -> Option<Approval> {
    if !COMMAND_HEADINGS
        .iter()
        .any(|heading| screen.contains(heading))
    {
        return None;
    }
    let mut rows = screen
        .lines()
        .rev()
        .filter(|line| !content(line).is_empty());
    let footer = content(rows.next()?);
    if !matches!(
        footer,
        "enter to confirm · esc to cancel"
            | "press enter to confirm"
            | "press enter to confirm or esc to cancel"
            | "esc to cancel"
    ) {
        return None;
    }
    let deny = option(rows.next()?).0;
    if !matches!(
        deny,
        "deny" | "no" | "no (esc)" | "no, and tell codex what to do differently (esc)"
    ) {
        return None;
    }
    let mut allow = option(rows.next()?).0;
    if matches!(allow, "allow always" | "yes, and don't ask again") {
        allow = option(rows.next()?).0;
    }
    if !matches!(
        allow,
        "yes" | "allow once" | "yes, proceed (y)" | "yes, just this once"
    ) {
        return None;
    }
    for row in rows {
        let text = content(row);
        if COMMAND_HEADINGS.contains(&text) {
            return selected_choices(screen, &[text], &[allow], &[deny], &["allow always"]);
        }
        // A newer prompt, unrecognized option or second dialog ends the
        // preceding heading's authority. Command previews are indented or
        // prefixed with the shell prompt.
        if (!row.starts_with(char::is_whitespace) && !text.starts_with("$ "))
            || text.ends_with('?')
            || option(row).1
            || option(row).0 != text
        {
            return None;
        }
    }
    None
}

// Policy failures describe unavailable actions rather than active choices.
pub(super) fn policy_blocked(lower: &str) -> bool {
    [
        "blocked by policy",
        "denied by policy",
        "disabled by your",
        "managed by your",
        "organization policy",
        "company policy",
    ]
    .iter()
    .any(|text| lower.contains(text))
}

fn recognize_dialog(
    provider: &str,
    screen: &str,
    focused_marker_from_bottom: Option<usize>,
) -> Option<Approval> {
    let manifest = super::launch_manifest(provider)?;
    let lower = screen.to_lowercase();
    if policy_blocked(&lower) {
        return None;
    }
    // A Claude consent or scope picker supersedes headings still visible above it.
    // Keep later tool dialogs eligible for the ordinary provider contracts.
    let lower = if manifest.id == super::claude::MANIFEST.id {
        if let Some(modal) = super::claude::auto_mode_screen(&lower) {
            if let Some(choice) =
                super::claude::auto_mode_consent(modal, focused_marker_from_bottom)
            {
                return Some(choice);
            }
            modal
        } else {
            &lower
        }
    } else {
        &lower
    };
    if super::grok::MANIFEST.executables.contains(&provider) {
        if let Some(choice) = super::grok::approval(screen) {
            return Some(choice);
        }
    }
    // A Uniterm Project is already an explicit directory trust decision. Use
    // one strict contract for every bundled provider and executable alias.
    if let Some(choice) = selected_dialog(lower, &WORKSPACE_TRUST) {
        return Some(choice);
    }
    let dialogs = super::approval_dialogs(manifest.id);
    for dialog in dialogs {
        if let Some(choice) = selected_dialog(lower, dialog) {
            return Some(choice);
        }
    }
    if let Some(choice) = shared_command(lower) {
        return Some(choice);
    }
    // The common contract covers bundled providers without custom layouts.
    // Local detection overrides never gain authority to type into a terminal.
    let headings: Vec<_> = manifest
        .rules
        .iter()
        .filter(|rule| {
            rule.status == uniterm_core::AgentStatus::Permission
                && matches!(rule.region, super::RuleRegion::Bottom)
        })
        .flat_map(|rule| rule.patterns.iter().copied())
        .filter(|pattern| {
            // Status clues about workspace trust or the current selection
            // cannot authorize an ordinary tool permission.
            !WORKSPACE_TRUST.headings.contains(pattern)
                && ![
                    "yes",
                    "allow once",
                    "allow always",
                    "deny",
                    "press enter",
                    "saved decision:",
                    "current session:",
                    "esc to",
                    "ctrl+",
                    "←/→",
                ]
                .iter()
                .any(|prefix| pattern.starts_with(prefix))
        })
        .collect();
    selected_choices(
        lower,
        &headings,
        &[
            "yes",
            "yes, proceed (y)",
            "yes, proceed",
            "yes, allow once",
            "yes, just this once",
            "allow once",
        ],
        &["no", "no (esc)", "deny", "reject", "reject once"],
        &[],
    )
}

fn selected_dialog(screen: &str, dialog: &Dialog) -> Option<Approval> {
    let mut choice = selected_choices(
        screen,
        dialog.headings,
        dialog.allow,
        dialog.deny,
        dialog.alternatives,
    )?;
    choice.kind = dialog.kind;
    Some(choice)
}

fn selected_choices(
    screen: &str,
    headings: &[&str],
    allow: &[&str],
    deny: &[&str],
    alternatives: &[&str],
) -> Option<Approval> {
    let lines: Vec<_> = screen.lines().collect();
    // Use the newest heading; an earlier completed dialog is not authority.
    let header = lines.iter().rposition(|line| {
        headings
            .iter()
            .any(|heading| content(line).starts_with(heading))
    })?;
    let choice = lines
        .iter()
        .enumerate()
        .skip(header + 1)
        .find_map(|(index, line)| choice_matches(option(line).0, allow).then_some(index))?;
    let denial = lines
        .iter()
        .enumerate()
        .skip(header + 1)
        .find_map(|(index, line)| {
            let (label, _) = option(line);
            (choice_matches(label, deny) || label.starts_with("no, ")).then_some(index)
        })?;
    let selected: Vec<_> = lines
        .iter()
        .enumerate()
        .skip(header + 1)
        .filter(|(_, line)| option(line).1)
        .map(|(index, _)| index)
        .collect();
    let [selected] = selected.as_slice() else {
        return None;
    };
    let label = option(lines[*selected]).0;
    if !choice_matches(label, allow)
        && !choice_matches(label, deny)
        && !choice_matches(label, alternatives)
        && !label.starts_with("no, ")
        && !label.starts_with("yes, ")
        && label != "allow always"
    {
        return None;
    }
    let end = choice.max(denial).max(*selected);
    // A complete footer is also the barrier against partial PTY writes.
    if !lines[end..].iter().any(|line| line.contains("esc")) {
        return None;
    }
    // Do not act on a previous dialog left above a new input prompt.
    if lines[end + 1..].iter().any(|line| {
        let line = content(line);
        line.starts_with(['>', '❯', '›', '→']) || line.starts_with("choose ")
    }) {
        return None;
    }
    let mut result = fingerprint(&lines[..=end], b"\r");
    if *selected != choice {
        let between = &lines[choice.min(*selected)..=choice.max(*selected)];
        let count = between
            .iter()
            .filter(|line| {
                let (label, _) = option(line);
                choice_matches(label, allow)
                    || choice_matches(label, deny)
                    || choice_matches(label, alternatives)
                    || label.starts_with("no, ")
                    || label.starts_with("yes, ")
                    || label == "allow always"
            })
            .count()
            .checked_sub(1)?;
        if count == 0 || count > 8 {
            return None;
        }
        let arrow = if *selected < choice {
            b"\x1b[B"
        } else {
            b"\x1b[A"
        };
        // React/Ink and other TUIs can batch input events before updating
        // selection state. Send only one arrow and wait for the changed grid.
        result.keys = arrow.to_vec();
        result.navigation = Some(*selected as u64);
    }
    Some(result)
}

pub(super) fn fingerprint(lines: &[&str], keys: &'static [u8]) -> Approval {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    // Ignore selection styling and footer animation. Keep the request body
    // and command above the header (some providers put the question last).
    for line in lines {
        let (label, _) = option(line);
        if !label.is_empty() {
            label.hash(&mut hash);
        }
    }
    Approval {
        fingerprint: hash.finish(),
        watch_for_dismissal: false,
        keys: keys.to_vec(),
        kind: ApprovalKind::Permission,
        directory: None,
        navigation: None,
        settle: std::time::Duration::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recognize(provider: &str, screen: &str) -> Option<Approval> {
        recognize_grid(provider, screen, None)
    }

    const PROMPT: &str = "Would you like to run the following command?\n  echo hello\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again\n  3. No, and tell Codex what to do differently (esc)\nPress enter to confirm";

    #[test]
    fn recognizes_only_selected_one_time_permission_choices() {
        let permission = recognize("codex", PROMPT).unwrap();
        assert_eq!(permission.keys, b"\r");
        assert_eq!(permission.kind, ApprovalKind::Permission);
        assert!(recognize("custom", PROMPT).is_none());
        for replacement in [
            "1. Yes, proceed (y)",
            "› 1. No",
            "› 1. Yes, and don't ask again",
        ] {
            assert!(recognize(
                "codex",
                &PROMPT.replace("› 1. Yes, proceed (y)", replacement)
            )
            .is_none());
        }
        assert!(recognize(
            "codex",
            &PROMPT.replace(
                "Would you like to run the following command?",
                "Choose a database"
            )
        )
        .is_none());
        assert!(recognize("codex", &format!("Blocked by policy\n{PROMPT}")).is_none());
        assert!(recognize("codex", "Press enter to confirm\n› 1. Yes\n2. No (esc)").is_none());
        assert!(recognize(
            "codex",
            &PROMPT.replace("3. No, and tell Codex what to do differently (esc)", "")
        )
        .is_none());
    }

    #[test]
    fn complete_workspace_trust_is_shared_by_every_bundled_agent_and_alias() {
        let screens = [
            "Trust this workspace?\n/work\n❯ Yes, trust this workspace\nNo, exit\nEnter to confirm · Esc to cancel",
            "╭─ Do you trust the files in this folder? ─╮\n│ ● Trust folder (work) │\n│ ○ Trust parent folder (home) │\n│ ○ Don't trust │\n│ (Use Enter to select, Esc to close) │\n╰────────────────────────╯",
        ];
        for manifest in super::super::BUILT_INS {
            for provider in std::iter::once(manifest.id).chain(manifest.executables.iter().copied())
            {
                for screen in screens {
                    let approval = recognize(provider, screen).unwrap_or_else(|| {
                        panic!("workspace trust was not recognized for {provider}: {screen}")
                    });
                    assert_eq!(approval.kind, ApprovalKind::WorkspaceTrust, "{provider}");
                    assert_eq!(approval.keys, b"\r", "{provider}");
                }
            }
        }
        assert!(recognize("custom-agent", screens[0]).is_none());
    }

    const SHARED_COMMAND: &str = "Allow this command?\n$ git status\n❯ 1. Allow once\n2. Allow always\n3. Deny\nEnter to confirm · Esc to cancel";

    #[test]
    fn one_time_command_permissions_reach_every_bundled_agent_and_alias() {
        // These exercise the shared contracts. Native layouts have their own fixtures.
        for screen in [
            SHARED_COMMAND,
            include_str!("../../tests/fixtures/approval/codex-command.txt"),
        ] {
            let mut term = crate::terminal::Terminal::new(100, 28);
            term.feed(screen.replace('\n', "\r\n").as_bytes());
            for manifest in super::super::BUILT_INS {
                for provider in
                    std::iter::once(manifest.id).chain(manifest.executables.iter().copied())
                {
                    let approval = super::super::permission_approval(provider, &term)
                        .unwrap_or_else(|| {
                            panic!("unrecognized command permission for {provider}: {screen}")
                        });
                    assert_eq!(approval.kind, ApprovalKind::Permission, "{provider}");
                    assert_eq!(approval.keys, b"\r", "{provider}");
                    assert_eq!(approval.navigation, None, "{provider}");
                    assert!(approval.directory.is_none(), "{provider}");
                }
            }
            assert!(super::super::permission_approval("custom-agent", &term).is_none());
        }
    }

    #[test]
    fn shared_command_permissions_keep_questions_grants_and_stale_screens_manual() {
        for screen in [
            SHARED_COMMAND.replace("Allow this command?", "Choose a response"),
            SHARED_COMMAND.replace("Allow this command?", "❯ Explain Allow this command?"),
            SHARED_COMMAND.replace("Allow this command?", "Trust this workspace?"),
            SHARED_COMMAND.replace("❯ 1. Allow once\n2. Allow always", "❯ 1. Allow always"),
            SHARED_COMMAND.replace("❯ 1. Allow once", "1. Allow once"),
            SHARED_COMMAND.replace("2. Allow always", "❯ 2. Allow always"),
            SHARED_COMMAND.replace("3. Deny\n", ""),
            SHARED_COMMAND.replace("Enter to confirm · Esc to cancel", ""),
            format!("{SHARED_COMMAND}\n❯ a newer draft"),
            format!("{SHARED_COMMAND}\nChoose a location\n❯ Local"),
            format!("Blocked by policy\n{SHARED_COMMAND}"),
        ] {
            for manifest in super::super::BUILT_INS {
                for provider in
                    std::iter::once(manifest.id).chain(manifest.executables.iter().copied())
                {
                    assert!(
                        recognize(provider, &screen).is_none(),
                        "{provider}: {screen}"
                    );
                }
            }
        }
    }

    #[test]
    fn trust_directory_requires_complete_absolute_case_preserved_scope() {
        for provider in super::super::BUILT_INS {
            for path in ["/Work/Project", "/unrelated", "/", "/Work/Project/subdir"] {
                let screen = format!("Trust this workspace?\n{path}\n❯ Yes, trust this workspace\nNo, exit\nEnter to confirm · Esc to cancel");
                assert_eq!(
                    recognize(provider.id, &screen)
                        .unwrap()
                        .directory
                        .as_deref(),
                    Some(path)
                );
            }
        }
        for path in [
            "",
            "project",
            "~/project",
            "/work/../other",
            "/work/./project",
            "/work/…",
            "/work/...",
        ] {
            let screen = format!("Trust this workspace?\n{path}\n❯ Yes, trust this workspace\nNo, exit\nEnter to confirm · Esc to cancel");
            assert!(
                recognize("codex", &screen).unwrap().directory.is_none(),
                "{path}"
            );
        }
        assert_eq!(
            trust_directory("Folder access\n/Work/Project\nTrust this folder? Details"),
            Some("/Work/Project".into())
        );
        assert_eq!(
            trust_directory("Accessing workspace:\n/Work/Project\nQuick safety check: Details"),
            Some("/Work/Project".into())
        );
        assert_eq!(
            trust_directory("Trust this workspace?\n/old\nTrust this workspace?\nUnknown"),
            None
        );
    }

    #[test]
    fn fingerprints_ignore_footer_but_distinguish_stacked_requests() {
        let first = recognize("codex", PROMPT).unwrap().fingerprint;
        assert_eq!(
            first,
            recognize("codex", &format!("{PROMPT}\nframe 2"))
                .unwrap()
                .fingerprint
        );
        assert_ne!(
            first,
            recognize("codex", &PROMPT.replace("echo hello", "echo world"))
                .unwrap()
                .fingerprint
        );
        assert!(recognize(
            "claude",
            "Bash: ls\nDo you want to proceed?\n❯ 1. Yes\n2. No\nEsc to cancel"
        )
        .is_some());
        assert!(recognize(
            "gemini",
            "│ Allow execution? │\n│ ● 1. Yes, allow once │\n│ 2. No (esc) │"
        )
        .is_some());
    }

    const CASES: &[(&str, &str)] = &[
        (
            "codex",
            include_str!("../../tests/fixtures/approval/codex-trust.txt"),
        ),
        (
            "codex",
            include_str!("../../tests/fixtures/approval/codex-command.txt"),
        ),
        (
            "claude",
            include_str!("../../tests/fixtures/approval/claude-command.txt"),
        ),
        (
            "grok",
            include_str!("../../tests/fixtures/approval/grok-command.txt"),
        ),
        (
            "agent",
            include_str!("../../tests/fixtures/approval/grok-command.txt"),
        ),
        (
            "pi",
            include_str!("../../tests/fixtures/approval/pi-trust.txt"),
        ),
        (
            "gemini",
            include_str!("../../tests/fixtures/approval/gemini-command.txt"),
        ),
    ];

    #[test]
    fn provider_dialogs_replay_through_vt_and_reject_incomplete_or_question_screens() {
        for &(provider, screen) in CASES {
            let mut term = crate::terminal::Terminal::new(120, 40);
            let bytes = screen.replace('\n', "\r\n");
            term.feed(bytes.as_bytes());
            assert_eq!(
                super::super::permission_approval(provider, &term)
                    .unwrap_or_else(|| panic!("{provider}: {screen}"))
                    .keys,
                b"\r"
            );
            // Every prefix before the last option must remain inert.
            let end = bytes.find("esc").or_else(|| bytes.find("Esc")).unwrap();
            assert!(recognize(provider, &bytes[..end]).is_none(), "{provider}");
            assert!(
                recognize(provider, &format!("{screen}\n› Choose a database")).is_none(),
                "{provider}"
            );
            assert!(
                recognize(provider, &format!("Blocked by policy\n{screen}")).is_none(),
                "{provider}"
            );
        }
    }

    #[test]
    fn tool_permissions_cover_edits_network_and_existing_terminal_input() {
        for (provider, heading) in [
            ("codex", "Would you like to make the following edits?"),
            (
                "codex",
                "Would you like to send input to the existing terminal?",
            ),
            ("codex", "Would you like to send input to terminal 3?"),
            (
                "codex",
                "Do you want to approve network access to \"example.test\"?",
            ),
            ("claude", "Do you want to make this edit to sample.rs?"),
            ("claude", "Do you want to create sample.rs?"),
            ("claude", "Do you want to overwrite sample.rs?"),
            (
                "claude",
                "Do you want to allow Claude to fetch this content?",
            ),
            ("claude", "Would you like to proceed?"),
        ] {
            let screen = format!("{heading}\n/work/sample.rs\n❯ 1. Yes, just this once\n2. No, continue without running it\nEnter to confirm · Esc to cancel");
            assert_eq!(
                recognize(provider, &screen)
                    .unwrap_or_else(|| panic!("{provider}: {heading}"))
                    .keys,
                b"\r"
            );
            assert!(
                recognize(provider, &screen.replace(heading, &format!("> {heading}"))).is_none()
            );
        }
    }

    const CLAUDE_CONSENT: &str = "Teach auto mode about your environment?\n\
Auto mode works better when it knows your environment. Takes about a minute.\n\
❯ 1. Yes\n\
2. Not now\n\
3. Don't show again\n\
Enter to confirm · Esc to cancel";
    const CLAUDE_MODE: &str = "⏵⏵ auto mode on (shift+tab to cycle)";
    const CLAUDE_SCOPE: &str = "Teach auto mode about your environment?\n\
Claude Code reads this project, your recent Claude sessions, and optionally your shell history.\n\
How you use Claude here Mixed\n\
❯ Also scan shell history on\n\
Also scan your other repos off\n\
Continue\n\
←/→ to change · Enter to continue · Esc to cancel";

    fn focused_claude_consent(screen: &str) -> Option<Approval> {
        let marker = screen.split('\n').rev().position(|line| {
            ["❯ 1.", "❯ 2.", "❯ 3."]
                .iter()
                .any(|prefix| line.trim_start().starts_with(prefix))
        });
        recognize_grid("claude", screen, marker)
    }

    #[test]
    fn claude_auto_mode_consent_can_cover_a_background_composer() {
        let consent = focused_claude_consent(CLAUDE_CONSENT).unwrap();
        assert_eq!(consent.keys, b"\r");
        for draft in ["git status", "yes", "not now", "yes, please", ""] {
            let screen = format!("{CLAUDE_CONSENT}\n❯ {draft}\n{CLAUDE_MODE}");
            let choice = focused_claude_consent(&screen).unwrap();
            assert_eq!(choice.keys, b"\r");
            assert_eq!(choice.fingerprint, consent.fingerprint);
        }
        let wrapped = CLAUDE_CONSENT.replace("environment. Takes", "environment.\nTakes");
        assert_eq!(focused_claude_consent(&wrapped).unwrap().keys, b"\r");
    }

    #[test]
    fn claude_consent_requires_every_row_and_rejects_newer_controls() {
        for line in CLAUDE_CONSENT.lines() {
            assert!(
                focused_claude_consent(&CLAUDE_CONSENT.replace(line, "")).is_none(),
                "{line}"
            );
        }
        for (original, replacement) in [
            ("2. Not now", "2. No, continue"),
            ("2. Not now", "❯ 2. Not now"),
            (
                "2. Not now\n3. Don't show again",
                "3. Don't show again\n2. Not now",
            ),
            ("3. Don't show again", "3. Don't show again\n4. Continue"),
            (
                "Enter to confirm · Esc to cancel",
                "❯ describe Esc handling",
            ),
            (
                "❯ 1. Yes\n2. Not now\n3. Don't show again",
                "❯ 1. Yes\n2. No",
            ),
        ] {
            assert!(
                focused_claude_consent(&CLAUDE_CONSENT.replace(original, replacement)).is_none()
            );
        }
        for tail in [
            "❯ git status",
            "Choose a database\n❯ First",
            CLAUDE_SCOPE,
            &format!("❯ git status\n{CLAUDE_MODE}\nChoose an option"),
        ] {
            assert!(
                focused_claude_consent(&format!("{CLAUDE_CONSENT}\n{tail}")).is_none(),
                "{tail}"
            );
        }
        assert!(focused_claude_consent(CLAUDE_SCOPE).is_none());
        let stale_tool =
            format!("Do you want to proceed?\n{CLAUDE_SCOPE}\n❯ Yes\nNo\nEsc to cancel");
        assert!(focused_claude_consent(&stale_tool).is_none());
        let new_tool = format!("{CLAUDE_SCOPE}\nDo you want to proceed?\n❯ Yes\nNo\nEsc to cancel");
        assert_eq!(focused_claude_consent(&new_tool).unwrap().keys, b"\r");

        let catalog = super::super::Catalog::load_from_paths(None, None, None);
        let status = |screen: &str| catalog.screen("claude", screen, "").unwrap().status;
        assert_eq!(
            status(CLAUDE_CONSENT),
            Some(uniterm_core::AgentStatus::Permission)
        );
        assert_eq!(
            status(CLAUDE_SCOPE),
            Some(uniterm_core::AgentStatus::Question)
        );
        assert_eq!(
            status(&format!("{CLAUDE_CONSENT}\n❯ editing draft")),
            Some(uniterm_core::AgentStatus::Idle)
        );
        assert_eq!(
            status(&CLAUDE_CONSENT.replace("2. Not now", "")),
            Some(uniterm_core::AgentStatus::Permission)
        );
        assert_eq!(
            status("❯ Explain Teach auto mode about your environment?"),
            Some(uniterm_core::AgentStatus::Idle)
        );
    }

    #[test]
    fn claude_consent_rejects_a_focused_composer_in_the_live_grid() {
        let mut term = crate::terminal::Terminal::new(120, 40);
        term.feed(include_bytes!(
            "../../tests/fixtures/approval/claude-consent-40.ansi"
        ));
        assert!(!term.cursor_visible());
        assert!(super::super::permission_approval("claude", &term).is_some());
        // Move the hidden hardware cursor to the active draft without changing
        // any text or styles. Both the composer marker and its text must be inert.
        for bytes in [b"\x1b[38;1H".as_slice(), b"\x1b[38;7H", b"\x1b[?25h"] {
            term.feed(bytes);
            assert!(super::super::permission_approval("claude", &term).is_none());
        }
    }

    #[test]
    fn captured_claude_consent_and_scope_report_status_from_the_screen() {
        use uniterm_core::AgentStatus;
        let catalog = super::super::Catalog::load_from_paths(None, None, None);
        for (rows, bytes) in [
            (
                40,
                include_bytes!("../../tests/fixtures/approval/claude-consent-40.ansi").as_slice(),
            ),
            (
                80,
                include_bytes!("../../tests/fixtures/approval/claude-consent-80.ansi").as_slice(),
            ),
        ] {
            let mut term = crate::terminal::Terminal::new(120, rows);
            term.feed(bytes);
            let status = |term: &crate::terminal::Terminal| {
                let tail = super::super::status_evidence(term, Some("claude"));
                catalog.screen("claude", &tail, "").unwrap().status
            };
            assert_eq!(status(&term), Some(AgentStatus::Permission));
            assert!(!term.cursor_visible());
            let choice = super::super::permission_approval("claude", &term).unwrap();
            assert_eq!(choice.keys, b"\r");
            assert_eq!(choice.settle, std::time::Duration::from_millis(500));
            // A parked cursor below the modal leaves the heading outside the
            // default tail. This may still be a permission, but cannot receive input.
            term.feed(format!("\x1b[{rows};1H").as_bytes());
            assert!(!term.evidence_text(12).contains("Teach auto mode"));
            assert_eq!(status(&term), Some(AgentStatus::Permission));
            assert!(super::super::permission_approval("claude", &term).is_none());
            if rows == 40 {
                term.feed(b"\x1b[32;3H");
                term.feed(include_bytes!(
                    "../../tests/fixtures/approval/claude-consent-to-scope.ansi"
                ));
                assert_eq!(status(&term), Some(AgentStatus::Question));
                assert!(super::super::permission_approval("claude", &term).is_none());
            }
        }
    }

    #[test]
    fn claude_consent_navigation_keeps_one_fingerprint_across_all_three_choices() {
        let affirmative = focused_claude_consent(CLAUDE_CONSENT).unwrap();
        let unselected = CLAUDE_CONSENT.replace("❯ 1. Yes", "1. Yes");
        for (index, label) in [(2, "3. Don't show again"), (1, "2. Not now")] {
            let screen = unselected.replace(label, &format!("❯ {label}"));
            let choice = focused_claude_consent(&screen).unwrap();
            assert_eq!(choice.fingerprint, affirmative.fingerprint);
            assert_eq!(choice.navigation, Some(index));
            assert_eq!(choice.keys, b"\x1b[A");
        }
        assert_eq!(affirmative.navigation, None);
        assert_eq!(affirmative.keys, b"\r");
    }

    #[test]
    fn captured_codex_folder_access_matches_screenshot_contract() {
        let mut term = crate::terminal::Terminal::new(120, 40);
        term.feed(include_bytes!(
            "../../tests/fixtures/approval/codex-live.ansi"
        ));
        assert_eq!(
            super::super::permission_approval("codex", &term)
                .unwrap_or_else(|| panic!("{}", term.evidence_text(48)))
                .keys,
            b"\r"
        );
    }

    #[test]
    fn captured_claude_trust_navigates_from_default_exit() {
        let mut term = crate::terminal::Terminal::new(120, 40);
        term.feed(include_bytes!(
            "../../tests/fixtures/approval/claude-trust.ansi"
        ));
        let screen = term.evidence_text(48);
        assert_eq!(
            super::super::permission_approval("claude", &term)
                .unwrap_or_else(|| panic!("{screen}"))
                .keys,
            b"\x1b[B"
        );
    }

    #[test]
    fn navigation_and_submit_share_the_prompt_but_wait_for_selected_allow() {
        let screen = "Accessing workspace:\n/work\n❯ No, exit\nYes, I trust this folder\nEnter to confirm · Esc to cancel";
        let first = recognize("claude", screen).unwrap();
        assert_eq!(first.keys, b"\x1b[B");
        assert!(first.navigation.is_some());
        let second = recognize(
            "claude",
            &screen.replace("❯ No", "No").replace("Yes, I", "❯ Yes, I"),
        )
        .unwrap();
        assert_eq!(second.keys, b"\r");
        assert_eq!(second.navigation, None);
        assert_eq!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn grok_startup_trust_requires_complete_shortcuts_and_no_newer_prompt() {
        let screen = include_str!("../../tests/fixtures/approval/grok-trust.txt");
        let mut terminal = crate::terminal::Terminal::new(120, 40);
        terminal.feed(include_bytes!(
            "../../tests/fixtures/approval/grok-trust-live.ansi"
        ));
        for provider in ["grok", "agent"] {
            let approval = recognize(provider, screen).unwrap();
            assert_eq!(approval.keys, b"y");
            assert_eq!(approval.kind, ApprovalKind::WorkspaceTrust);
            assert_eq!(
                super::super::permission_approval(provider, &terminal)
                    .unwrap_or_else(|| panic!("{}", terminal.evidence_text(48)))
                    .keys,
                b"y"
            );
            for incomplete in [
                screen.replace("No, quit", ""),
                screen.replace("posing security risks.", ""),
                screen.replace("Yes, proceed                 y", "Yes, proceed"),
                format!("{screen}\n❯ Tell me what to do"),
                format!("{screen}\nBlocked by policy"),
            ] {
                assert!(recognize(provider, &incomplete).is_none(), "{incomplete}");
            }
        }
        assert!(recognize("codex", screen).is_none());
    }

    #[test]
    fn captured_grok_and_agent_choose_one_time_not_persistent_grant() {
        let mut term = crate::terminal::Terminal::new(120, 40);
        term.feed(include_bytes!(
            "../../tests/fixtures/approval/grok-live.ansi"
        ));
        for provider in ["grok", "agent"] {
            assert_eq!(
                super::super::permission_approval(provider, &term)
                    .unwrap_or_else(|| panic!("{}", term.evidence_text(48)))
                    .keys,
                b"\r"
            );
            let screen = term
                .evidence_text(48)
                .replace("3 (●)", "3 (○)")
                .replace("1 (○)", "1 (●)");
            assert!(recognize(provider, &screen).is_none());
        }
    }

    #[test]
    fn captured_opencode_permission_without_persistent_option() {
        let mut term = crate::terminal::Terminal::new(120, 40);
        term.feed(include_bytes!(
            "../../tests/fixtures/approval/opencode-live.ansi"
        ));
        assert_eq!(
            super::super::permission_approval("opencode", &term)
                .unwrap_or_else(|| panic!("{}", term.evidence_text(48)))
                .keys,
            b"\r"
        );
    }

    #[test]
    fn opencode_requires_the_allow_once_button_style() {
        let input = include_str!("../../tests/fixtures/approval/opencode-command.ansi");
        let mut term = crate::terminal::Terminal::new(120, 40);
        term.feed(input.as_bytes());
        assert_eq!(
            super::super::permission_approval("opencode", &term)
                .unwrap()
                .keys,
            b"\r"
        );
        for bytes in [
            input.replace("30;43", "90;40"),
            input.replace("Allow once", "Ask a question"),
            input.replace("enter confirm", ""),
        ] {
            let mut term = crate::terminal::Terminal::new(120, 40);
            term.feed(bytes.as_bytes());
            assert!(super::super::permission_approval("opencode", &term).is_none());
        }
    }

    #[test]
    fn live_grid_approval_budget_includes_negative_screens() {
        for (provider, bytes, expected) in [
            (
                "codex",
                include_bytes!("../../tests/fixtures/approval/codex-live.ansi").as_slice(),
                true,
            ),
            (
                "claude",
                include_bytes!("../../tests/fixtures/approval/claude-trust.ansi").as_slice(),
                true,
            ),
            (
                "claude",
                include_bytes!("../../tests/fixtures/approval/claude-consent-40.ansi").as_slice(),
                true,
            ),
            (
                "opencode",
                include_bytes!("../../tests/fixtures/approval/opencode-live.ansi").as_slice(),
                true,
            ),
            (
                "grok",
                include_bytes!("../../tests/fixtures/approval/grok-live.ansi").as_slice(),
                true,
            ),
            (
                "grok",
                include_bytes!("../../tests/fixtures/approval/grok-trust-live.ansi").as_slice(),
                true,
            ),
            (
                "codex",
                b"> Choose a database\r\n1. SQLite\r\n2. Postgres".as_slice(),
                false,
            ),
        ] {
            let mut term = crate::terminal::Terminal::new(120, 40);
            term.feed(bytes);
            let mut samples = Vec::with_capacity(200);
            for _ in 0..200 {
                let start = std::time::Instant::now();
                assert_eq!(
                    std::hint::black_box(super::super::permission_approval(
                        provider,
                        std::hint::black_box(&term)
                    ))
                    .is_some(),
                    expected
                );
                samples.push(start.elapsed());
            }
            samples.sort_unstable();
            let p95 = samples[samples.len() * 95 / 100];
            eprintln!("{provider} live grid approval ({expected}) p95: {p95:?}");
            // Debug instrumentation and parallel test load are not the shipping
            // budget. Optimized builds enforce a five-millisecond bound.
            let budget = if cfg!(debug_assertions) { 100 } else { 5 };
            assert!(p95 < std::time::Duration::from_millis(budget));
        }
    }

    #[test]
    fn permission_recognition_budget() {
        let mut samples = Vec::with_capacity(2_000);
        for index in 0..2_000 {
            let (provider, screen) = CASES[index % CASES.len()];
            let start = std::time::Instant::now();
            assert!(
                std::hint::black_box(recognize(provider, std::hint::black_box(screen))).is_some()
            );
            samples.push(start.elapsed());
        }
        samples.sort_unstable();
        let p95 = samples[samples.len() * 95 / 100];
        eprintln!("permission recognition p95: {p95:?}");
        assert!(p95 < std::time::Duration::from_millis(5));
    }
}
