//! The agent-state model. See `docs/06-agentic-supervision.md`.
//!
//! This enum is the single, agent-agnostic status every supervision surface is
//! written against. It lives in core because it is pure data; the *detection*
//! that produces it (OSC 777, log-tail, grid heuristics, exit notification)
//! lives in `uniterm-server`.

/// The reconciled status of an agent running in a pane.
///
/// `Permission` and `Question` are the two states that mean "a human is the
/// bottleneck" and are what the waiting queue is built from. The distinction
/// between `Idle` (done, healthy, waiting for you) and `Permission`/`Question`
/// (blocked, needs you now) is the most important signal the product surfaces.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum AgentStatus {
    /// Not yet observed.
    #[default]
    Unknown,
    /// Session opening.
    Starting,
    /// Actively producing output or thinking.
    Working,
    /// Running a tool call.
    Tool,
    /// Blocked, waiting for a human to approve or deny an action.
    Permission,
    /// Blocked, waiting for a human to answer.
    Question,
    /// Turn finished, waiting for the next prompt.
    Idle,
    /// Failed.
    Error,
    /// The process ended.
    Exited,
}

impl AgentStatus {
    /// Whether a human is currently the bottleneck for this agent.
    /// These are exactly the statuses that populate the waiting queue.
    pub fn needs_human(self) -> bool {
        matches!(self, AgentStatus::Permission | AgentStatus::Question)
    }

    /// Sort priority for the fleet view: lower sorts first. The things that need
    /// a human or are stuck sort to the top. See `docs/08-observatory.md`.
    pub fn fleet_priority(self) -> u8 {
        match self {
            AgentStatus::Permission => 0,
            AgentStatus::Question => 1,
            AgentStatus::Error => 2,
            AgentStatus::Tool => 3,
            AgentStatus::Working => 4,
            AgentStatus::Starting => 5,
            AgentStatus::Idle => 6,
            AgentStatus::Exited => 7,
            AgentStatus::Unknown => 8,
        }
    }

    /// A short human label for the fleet view.
    pub fn label(self) -> &'static str {
        match self {
            AgentStatus::Unknown => "unknown",
            AgentStatus::Starting => "starting",
            AgentStatus::Working => "working",
            AgentStatus::Tool => "tool",
            AgentStatus::Permission => "permission",
            AgentStatus::Question => "question",
            AgentStatus::Idle => "idle",
            AgentStatus::Error => "error",
            AgentStatus::Exited => "exited",
        }
    }

    /// Map an OSC 777 lifecycle event name to a status, if it implies one.
    pub fn from_event(event: &str) -> Option<AgentStatus> {
        Some(match event {
            "session_start" => AgentStatus::Starting,
            "prompt_submit" | "tool_end" => AgentStatus::Working,
            "tool_start" => AgentStatus::Tool,
            "permission_request" => AgentStatus::Permission,
            "question" => AgentStatus::Question,
            "idle" => AgentStatus::Idle,
            "error" => AgentStatus::Error,
            "session_end" | "exiting" => AgentStatus::Exited,
            _ => return None,
        })
    }
}

/// A supported AI agent: a stable id, a display name, and a signature colour
/// used by fleet surfaces to preserve the provider's visual identity.
pub struct Provider {
    pub id: &'static str,
    pub name: &'static str,
    pub color: crate::Color,
    /// The CLI command that launches this agent interactively (it must accept
    /// an initial prompt as its argument). Discovery = this on $PATH.
    pub command: &'static str,
}

/// The built-in agent registry. Ids are the normalized agent identifiers that
/// appear in OSC 777 payloads.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "claude",
        command: "claude",
        name: "Claude Code",
        color: crate::Color::Rgb(0xd9, 0x77, 0x57),
    },
    Provider {
        id: "codex",
        command: "codex",
        name: "Codex",
        color: crate::Color::Rgb(0x7a, 0x9d, 0xff),
    },
    Provider {
        id: "opencode",
        command: "opencode",
        name: "OpenCode",
        color: crate::Color::Rgb(0xb7, 0xb1, 0xb1),
    },
    Provider {
        id: "gemini",
        command: "gemini",
        name: "Gemini",
        color: crate::Color::Rgb(0x7a, 0x93, 0xff),
    },
    Provider {
        id: "grok",
        command: "grok",
        name: "Grok",
        color: crate::Color::Rgb(0xd4, 0xd4, 0xd4),
    },
    Provider {
        id: "kiro",
        command: "kiro",
        name: "Kiro",
        color: crate::Color::Rgb(0x6b, 0x46, 0xc1),
    },
    Provider {
        id: "cursor",
        command: "agent",
        name: "Cursor Agent",
        // Cursor's brand mark is monochrome. Keep the terminal identity
        // neutral and bright enough to remain distinct from muted UI text.
        color: crate::Color::Rgb(0xe0, 0xe0, 0xe0),
    },
    Provider {
        id: "pi",
        command: "pi",
        name: "Pi",
        // Pi's built-in dark theme uses this teal for its primary accent.
        color: crate::Color::Rgb(0x8a, 0xbe, 0xb7),
    },
];

/// The banner art for an agent: its display name set in ANSI Compact.
/// `None` when the name has no block rendering (odd characters), so the caller
/// can fall back to plain text.
pub fn agent_logo(id: &str) -> Option<Vec<String>> {
    let p = provider(id);
    wordmark(p.map(|p| p.name).unwrap_or(id))
}

/// One glyph from the three-row ANSI Compact FIGlet font.
struct Glyph {
    ch: char,
    rows: [&'static str; 3],
}

const fn g(ch: char, rows: [&'static str; 3]) -> Glyph {
    Glyph { ch, rows }
}

/// ANSI Compact's visible A-Z alphabet, plus the separators supported by
/// custom provider ids. The source FIGfont is MIT licensed by Loic Cressot.
#[rustfmt::skip]
const WORDMARK_FONT: &[Glyph] = &[
    g('A', ["▄████▄", "██▄▄██", "██  ██"]),
    g('B', ["█████▄", "██▄▄██", "██▄▄█▀"]),
    g('C', ["▄█████", "██",     "▀█████"]),
    g('D', ["████▄",  "██  ██", "████▀"]),
    g('E', ["██████", "██▄▄",   "██▄▄▄▄"]),
    g('F', ["██████", "██▄▄",   "██"]),
    g('G', [" ▄████", "██  ▄▄▄", " ▀███▀"]),
    g('H', ["██  ██", "██████", "██  ██"]),
    g('I', ["██",     "██",     "██"]),
    g('J', ["   ██",  "   ██",  "████▀"]),
    g('K', ["██ ▄█▀", "████",   "██ ▀█▄"]),
    g('L', ["██",     "██",     "██████"]),
    g('M', ["██▄  ▄██", "██ ▀▀ ██", "██    ██"]),
    g('N', ["███  ██", "██ ▀▄██", "██   ██"]),
    g('O', ["▄████▄", "██  ██", "▀████▀"]),
    g('P', ["█████▄", "██▄▄█▀", "██"]),
    g('Q', ["▄█████▄", "██ ▄ ██", "▀█████▀"]),
    g('R', ["█████▄", "██▄▄██▄", "██   ██"]),
    g('S', ["▄█████", "▀▀▀▄▄▄", "█████▀"]),
    g('T', ["██████", "  ██",   "  ██"]),
    g('U', ["██  ██", "██  ██", "▀████▀"]),
    g('V', ["██  ██", "██▄▄██", " ▀██▀"]),
    g('W', ["██     ██", "██ ▄█▄ ██", " ▀██▀██▀"]),
    g('X', ["██  ██", " ████",  "██  ██"]),
    g('Y', ["██  ██", " ▀██▀",  "  ██"]),
    g('Z', ["██████", " ▄▄▀▀",  "██████"]),
    g(' ', [" ", " ", " "]),
    g('-', ["", "▄▄▄", ""]),
];

/// Set `text` in ANSI Compact. Pure layout - the caller owns colour and
/// placement. `None` when any character has no glyph, so callers fall back to
/// plain text rather than render holes.
pub fn wordmark(text: &str) -> Option<Vec<String>> {
    let glyphs: Vec<&Glyph> = text
        .chars()
        .map(|c| {
            let up = c.to_ascii_uppercase();
            WORDMARK_FONT.iter().find(|g| g.ch == up)
        })
        .collect::<Option<_>>()?;
    let mut lines = vec![String::new(); 3];
    for (i, glyph) in glyphs.iter().enumerate() {
        let width = glyph
            .rows
            .iter()
            .map(|row| row.chars().count())
            .max()
            .unwrap_or(0);
        for (line, row) in lines.iter_mut().zip(glyph.rows) {
            line.push_str(row);
            line.extend(std::iter::repeat_n(' ', width - row.chars().count()));
            if i + 1 < glyphs.len() {
                line.push(' ');
            }
        }
    }
    Some(lines)
}

/// Look up a provider by (normalized) id.
pub fn provider(id: &str) -> Option<&'static Provider> {
    let id = id.to_ascii_lowercase();
    PROVIDERS.iter().find(|p| {
        p.id == id
            || id
                .strip_prefix(p.id)
                .is_some_and(|suffix| suffix.starts_with('-'))
    })
}

/// The signature colour for an agent id, if known.
pub fn agent_color(id: &str) -> Option<crate::Color> {
    provider(id).map(|p| p.color)
}

/// The signature colour every runtime fleet surface uses, including the
/// neutral fallback for an agent absent from the built-in registry.
pub fn agent_color_or_default(id: &str) -> crate::Color {
    agent_color(id).unwrap_or(crate::Color::Idx(244))
}

/// The display name for an agent id, or the id itself if unknown.
pub fn agent_name(id: &str) -> &str {
    provider(id).map(|p| p.name).unwrap_or(id)
}

/// Sort a slice of items by their agent status priority (the things that need a
/// human or are stuck sort first) - the Observatory fleet order (`docs/08`).
pub fn fleet_sort<T>(items: &mut [T], status: impl Fn(&T) -> AgentStatus) {
    items.sort_by_key(|it| status(it).fleet_priority());
}

/// The subset of items whose agent needs a human now (the waiting queue).
pub fn waiting_queue<T>(items: &[T], status: impl Fn(&T) -> AgentStatus) -> Vec<&T> {
    items.iter().filter(|it| status(it).needs_human()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_human_only_for_blocked_states() {
        assert!(AgentStatus::Permission.needs_human());
        assert!(AgentStatus::Question.needs_human());
        assert!(!AgentStatus::Working.needs_human());
        assert!(!AgentStatus::Idle.needs_human());
    }

    #[test]
    fn blocked_states_sort_before_healthy_ones() {
        assert!(AgentStatus::Permission.fleet_priority() < AgentStatus::Working.fleet_priority());
        assert!(AgentStatus::Error.fleet_priority() < AgentStatus::Idle.fleet_priority());
    }

    #[test]
    fn default_is_unknown() {
        assert_eq!(AgentStatus::default(), AgentStatus::Unknown);
    }

    #[test]
    fn event_maps_to_status() {
        assert_eq!(AgentStatus::from_event("idle"), Some(AgentStatus::Idle));
        assert_eq!(
            AgentStatus::from_event("permission_request"),
            Some(AgentStatus::Permission)
        );
        assert_eq!(AgentStatus::from_event("nonsense"), None);
    }

    #[test]
    fn fleet_sorts_blocked_first_and_filters_waiting() {
        let mut fleet = vec![
            ("a", AgentStatus::Working),
            ("b", AgentStatus::Permission),
            ("c", AgentStatus::Idle),
            ("d", AgentStatus::Question),
        ];
        fleet_sort(&mut fleet, |(_, s)| *s);
        // Permission then Question sort ahead of Working/Idle.
        assert_eq!(fleet[0].0, "b");
        assert_eq!(fleet[1].0, "d");
        let waiting = waiting_queue(&fleet, |(_, s)| *s);
        assert_eq!(waiting.len(), 2); // permission + question
        assert!(waiting.iter().all(|(_, s)| s.needs_human()));
    }

    #[test]
    fn registry_colors_and_names() {
        assert_eq!(
            agent_color("claude"),
            Some(crate::Color::Rgb(0xd9, 0x77, 0x57))
        );
        assert_eq!(
            agent_color("codex"),
            Some(crate::Color::Rgb(0x7a, 0x9d, 0xff))
        );
        assert_eq!(
            agent_color("opencode"),
            Some(crate::Color::Rgb(0xb7, 0xb1, 0xb1))
        );
        assert_eq!(
            agent_color("gemini"),
            Some(crate::Color::Rgb(0x7a, 0x93, 0xff))
        );
        assert_eq!(
            agent_color("grok"),
            Some(crate::Color::Rgb(0xd4, 0xd4, 0xd4))
        );
        assert_eq!(
            agent_color("kiro"),
            Some(crate::Color::Rgb(0x6b, 0x46, 0xc1))
        );
        assert_eq!(
            agent_color("cursor"),
            Some(crate::Color::Rgb(0xe0, 0xe0, 0xe0))
        );
        assert_eq!(agent_color("pi"), Some(crate::Color::Rgb(0x8a, 0xbe, 0xb7)));
        // Prefix match: "claude-code" resolves to the claude provider.
        assert_eq!(agent_name("claude-code"), "Claude Code");
        assert_eq!(agent_name("cursor-agent"), "Cursor Agent");
        assert_eq!(agent_name("pico"), "pico");
        assert!(agent_color("unknown-agent").is_none());
        assert_eq!(
            agent_color_or_default("unknown-agent"),
            crate::Color::Idx(244)
        );
        assert_eq!(agent_name("unknown-agent"), "unknown-agent");
    }

    #[test]
    fn wordmark_sets_names_in_equal_width_rows() {
        let art = wordmark("Codex").expect("registry names must render");
        assert_eq!(art.len(), 3);
        let w = art[0].chars().count();
        assert!(art.iter().all(|l| l.chars().count() == w), "ragged rows");
        // ANSI Compact uses only block cells and gaps.
        assert!(art
            .iter()
            .flat_map(|l| l.chars())
            .all(|c| matches!(c, '\u{2588}' | '\u{2580}' | '\u{2584}' | ' ')));
        // A char outside the font falls back rather than rendering holes.
        assert_eq!(wordmark("naïve"), None);
    }

    #[test]
    fn ansi_compact_matches_the_reference_claude_code_wordmark() {
        assert_eq!(
            wordmark("Claude Code").unwrap(),
            vec![
                "▄█████ ██     ▄████▄ ██  ██ ████▄  ██████   ▄█████ ▄████▄ ████▄  ██████",
                "██     ██     ██▄▄██ ██  ██ ██  ██ ██▄▄     ██     ██  ██ ██  ██ ██▄▄  ",
                "▀█████ ██████ ██  ██ ▀████▀ ████▀  ██▄▄▄▄   ▀█████ ▀████▀ ████▀  ██▄▄▄▄",
            ]
        );
    }

    #[test]
    fn every_provider_has_a_banner() {
        for p in PROVIDERS {
            let art = agent_logo(p.id).unwrap_or_else(|| panic!("{} has no banner", p.id));
            assert!(!art.is_empty());
            assert_eq!(art.len(), 3, "{}", p.id);
            let w = art[0].chars().count();
            assert!(art.iter().all(|l| l.chars().count() == w), "{}", p.id);
            assert!(art
                .iter()
                .flat_map(|line| line.chars())
                .all(|c| matches!(c, '\u{2588}' | '\u{2580}' | '\u{2584}' | ' ')));
        }
        assert_eq!(agent_logo("gemini"), wordmark("Gemini"));
        // Custom agents render through the font; odd ids fall back to None.
        assert!(agent_logo("sh").is_some());
        assert!(agent_logo("weird🤖id").is_none());
    }
}
