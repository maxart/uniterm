//! Conservative permission-dialog recognition at the provider boundary.
//! Detection manifests may identify status; local overrides never gain input authority.

use std::hash::{Hash, Hasher};

/// Input for a complete, recognized dialog, including any choice navigation.
/// The provider contract identifies the affirmative option, never a blind Enter.
pub(crate) struct Approval {
    pub fingerprint: u64,
    pub keys: Vec<u8>,
    /// Selected-row identity for a navigation step; None submits the choice.
    pub navigation: Option<u64>,
}

fn content(line: &str) -> &str {
    line.trim().trim_matches(['│', '┃']).trim()
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

/// One provider-owned dialog contract, independent of the status dwell rules.
pub(super) struct Dialog {
    pub headings: &'static [&'static str],
    pub allow: &'static [&'static str],
    pub deny: &'static [&'static str],
}

pub(super) fn recognize(provider: &str, screen: &str) -> Option<Approval> {
    let manifest = super::launch_manifest(provider)?;
    let lower = screen.to_lowercase();
    // Policy failures are not interactive requests for approval.
    if [
        "blocked by policy",
        "denied by policy",
        "disabled by your",
        "managed by your",
        "organization policy",
        "company policy",
    ]
    .iter()
    .any(|text| lower.contains(text))
    {
        return None;
    }
    if super::grok::MANIFEST.executables.contains(&provider) {
        if let Some(choice) = super::grok::approval(screen) {
            return Some(choice);
        }
    }
    let dialogs = super::approval_dialogs(manifest.id);
    for dialog in dialogs {
        if let Some(choice) = selected_dialog(&lower, dialog) {
            return Some(choice);
        }
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
            ![
                "yes",
                "allow once",
                "allow always",
                "deny",
                "press enter",
                "esc to",
                "ctrl+",
                "←/→",
            ]
            .iter()
            .any(|prefix| pattern.starts_with(prefix))
        })
        .collect();
    selected_choices(
        &lower,
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
    )
}

fn selected_dialog(screen: &str, dialog: &Dialog) -> Option<Approval> {
    selected_choices(screen, dialog.headings, dialog.allow, dialog.deny)
}

fn selected_choices(
    screen: &str,
    headings: &[&str],
    allow: &[&str],
    deny: &[&str],
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
        .find_map(|(index, line)| allow.contains(&option(line).0).then_some(index))?;
    let denial = lines
        .iter()
        .enumerate()
        .skip(header + 1)
        .find_map(|(index, line)| {
            let (label, _) = option(line);
            (deny.contains(&label) || label.starts_with("no, ")).then_some(index)
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
    if !allow.contains(&label)
        && !deny.contains(&label)
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
                allow.contains(&label)
                    || deny.contains(&label)
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
        keys: keys.to_vec(),
        navigation: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROMPT: &str = "Would you like to run the following command?\n  echo hello\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again\n  3. No, and tell Codex what to do differently (esc)\nPress enter to confirm";

    #[test]
    fn recognizes_only_selected_one_time_permission_choices() {
        assert_eq!(recognize("codex", PROMPT).unwrap().keys, b"\r");
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
            assert_eq!(recognize(provider, screen).unwrap().keys, b"y");
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
