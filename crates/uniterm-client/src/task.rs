//! The New Task input model: a single editable prompt line with inline
//! slash-commands, parsed into a [`TaskSubmit`], plus the autocomplete engine
//! behind it (slash commands, workflow template names, project names). Pure
//! and testable; the client feeds it raw keys and renders it into the AG3
//! overlay.

use crate::overlay::Overlay;
use crate::text_input::{edit_line, line_with_cursor, LineKey};
use uniterm_core::orchestrate::WORKFLOW_TEMPLATES;

/// The slash commands the input understands: name, one-line description, and
/// whether a name argument follows (so accepting appends a space to keep
/// completing).
const SLASH_COMMANDS: &[(&str, &str, bool)] = &[
    ("/relay", "turn-based relay between two agents", false),
    (
        "/workflow",
        "run a workflow template (solo/pair/triad)",
        true,
    ),
    ("/project", "tag the task with a project name", true),
    ("/save", "record a task without launching a pane", false),
];

/// Rows reserved for suggestions in the overlay (fixed so the box never
/// changes size while typing - a shrinking box would leave residue).
const SUGGESTION_ROWS: usize = 5;
/// Interior text width of the overlay (fixed, same reason).
const BOX_WIDTH: usize = 56;

/// One autocomplete candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    /// The buffer content after accepting this candidate.
    pub insert: String,
    /// What the row shows: the completion and a short description.
    pub label: String,
    pub hint: String,
}

/// The editable New Task prompt.
#[derive(Clone, Debug, Default)]
pub struct TaskInput {
    pub buf: String,
    cursor: usize,
    /// Project-name completions (from the server's task history), filled in
    /// asynchronously after the modal opens.
    pub projects: Vec<String>,
    /// Installed provider ids (from the server's PATH probe), same round trip.
    pub agents: Vec<String>,
    /// The selected suggestion row.
    pub sel: usize,
}

/// A fully parsed submission: the global `@provider`, explicit
/// `@role=provider` selections, and the intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub agent: Option<String>,
    pub role_providers: Vec<uniterm_core::orchestrate::RoleProviderSelection>,
    pub submit: TaskSubmit,
}

/// What a submitted New Task line means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskSubmit {
    /// Nothing typed - do not submit.
    Empty,
    /// A plain prompt: spawn a pane and run it.
    Prompt(String),
    /// `/relay <spec>`: launch the turn-based relay (AG5).
    Relay(String),
    /// `/workflow <name> [prompt]`.
    Workflow { name: String, prompt: String },
    /// `/project <name> [prompt]`.
    Project { name: String, prompt: String },
    /// `/save [title]`: record a task without launching a pane (empty title
    /// just snapshots, since persistence is already automatic).
    Save(String),
}

impl TaskInput {
    pub fn new() -> Self {
        TaskInput::default()
    }

    pub fn insert(&mut self, c: char) {
        edit_line(&mut self.buf, &mut self.cursor, LineKey::Char(c));
        self.sel = 0;
    }

    pub fn backspace(&mut self) {
        edit_line(&mut self.buf, &mut self.cursor, LineKey::Backspace);
        self.sel = 0;
    }

    /// Apply a normalized editing key from the shared cross-terminal decoder.
    pub fn edit(&mut self, key: LineKey) -> bool {
        let changed = edit_line(&mut self.buf, &mut self.cursor, key);
        if changed {
            self.sel = 0;
        }
        changed
    }

    /// The autocomplete candidates for the current buffer:
    /// - empty buffer, or a partial `/word`: the slash commands;
    /// - a trailing `@partial` word: the installed agents;
    /// - `/workflow <partial-name>`: the bundled template names;
    /// - `/project <partial-name>`: project names from the task history;
    /// - anything else (a plain prompt, or past the name): none.
    pub fn suggestions(&self) -> Vec<Suggestion> {
        let s = self.buf.trim_start();
        // An @provider or @role=provider word being typed (anywhere in the
        // line) completes to the installed providers.
        if let Some(word) = self
            .buf
            .split(' ')
            .next_back()
            .filter(|w| w.starts_with('@'))
        {
            let (prefix, partial) = match word[1..].split_once('=') {
                Some((role, partial)) if !role.is_empty() => (format!("@{role}="), partial),
                _ => ("@".to_string(), &word[1..]),
            };
            let head = &self.buf[..self.buf.len() - word.len()];
            return self
                .agents
                .iter()
                .filter(|a| a.starts_with(partial))
                .map(|a| Suggestion {
                    insert: format!("{head}{prefix}{a} "),
                    label: format!("{prefix}{a}"),
                    hint: format!("run with {}", uniterm_core::agent::agent_name(a)),
                })
                .collect();
        }
        if s.is_empty() || (s.starts_with('/') && !s.contains(' ')) {
            let mut out: Vec<Suggestion> = SLASH_COMMANDS
                .iter()
                .filter(|(cmd, ..)| cmd.starts_with(s))
                .map(|(cmd, desc, takes_name)| Suggestion {
                    insert: if *takes_name {
                        format!("{cmd} ")
                    } else {
                        (*cmd).to_string()
                    },
                    label: (*cmd).to_string(),
                    hint: (*desc).to_string(),
                })
                .collect();
            if s.is_empty() {
                out.push(Suggestion {
                    insert: "@".to_string(),
                    label: "@provider".to_string(),
                    hint: "global provider; roles use @role=provider".to_string(),
                });
            }
            return out;
        }
        if let Some(partial) = name_argument(s, "/workflow") {
            return WORKFLOW_TEMPLATES
                .iter()
                .filter(|t| t.name.starts_with(partial))
                .map(|t| Suggestion {
                    insert: format!("/workflow {} ", t.name),
                    label: t.name.to_string(),
                    hint: t.summary.to_string(),
                })
                .collect();
        }
        if let Some(partial) = name_argument(s, "/project") {
            return self
                .projects
                .iter()
                .filter(|p| p.starts_with(partial))
                .map(|p| Suggestion {
                    insert: format!("/project {p} "),
                    label: p.clone(),
                    hint: "existing project".to_string(),
                })
                .collect();
        }
        Vec::new()
    }

    /// Move the suggestion selection (wrapping).
    pub fn sel_up(&mut self) {
        let n = self.suggestions().len();
        if n > 0 {
            self.sel = (self.sel + n - 1) % n;
        }
    }

    pub fn sel_down(&mut self) {
        let n = self.suggestions().len();
        if n > 0 {
            self.sel = (self.sel + 1) % n;
        }
    }

    /// Accept the selected suggestion (Tab): the buffer becomes its insertion.
    /// Returns whether anything was accepted.
    pub fn accept(&mut self) -> bool {
        let sug = self.suggestions();
        if sug.is_empty() {
            return false;
        }
        let s = &sug[self.sel.min(sug.len() - 1)];
        self.buf = s.insert.clone();
        self.cursor = self.buf.len();
        self.sel = 0;
        true
    }

    /// Parse the current buffer into a submission intent, extracting the first
    /// global `@provider` and every `@role=provider` selection.
    pub fn parse(&self) -> Parsed {
        let mut agent: Option<String> = None;
        let mut role_providers = Vec::new();
        let mut words: Vec<&str> = Vec::new();
        for w in self.buf.split_whitespace() {
            let selection = w
                .strip_prefix('@')
                .and_then(|selection| selection.split_once('='))
                .filter(|(role, provider)| !role.is_empty() && !provider.is_empty());
            if let Some((role, provider)) = selection {
                role_providers.push(uniterm_core::orchestrate::RoleProviderSelection {
                    role: role.to_string(),
                    provider: provider.to_string(),
                });
            } else if agent.is_none() && w.len() > 1 && w.starts_with('@') {
                agent = Some(w[1..].to_string());
            } else {
                words.push(w);
            }
        }
        let line = words.join(" ");
        Parsed {
            agent,
            role_providers,
            submit: parse_line(&line),
        }
    }

    /// Render the New Task overlay: the input line, a fixed-height suggestion
    /// list (selection marked), and the key hints. Every line is padded to a
    /// fixed width so the box geometry never changes while typing.
    pub fn overlay(&self) -> Overlay {
        let pad = |s: String| format!("{s:<BOX_WIDTH$}");
        let shown = line_with_cursor(&self.buf, self.cursor, BOX_WIDTH.saturating_sub(4));
        let mut lines = vec![String::new(), pad(format!("> {shown}"))];
        lines.push(String::new());
        let sug = self.suggestions();
        for i in 0..SUGGESTION_ROWS {
            let line = match sug.get(i) {
                Some(s) => {
                    let mark = if i == self.sel.min(sug.len().saturating_sub(1)) {
                        '\u{25B8}'
                    } else {
                        ' '
                    };
                    format!(" {mark} {:<12} {}", s.label, s.hint)
                }
                None => String::new(),
            };
            let mut clipped: String = line.chars().take(BOX_WIDTH).collect();
            clipped = pad(clipped);
            lines.push(clipped);
        }
        Overlay::with_footer(
            "New Task",
            lines,
            &[
                ("enter", "launch"),
                ("tab", "complete"),
                ("\u{2191}\u{2193}", "select"),
                ("esc", "cancel"),
            ],
        )
    }
}

/// Parse a (whitespace-normalized, `@agent`-stripped) line into an intent.
fn parse_line(s: &str) -> TaskSubmit {
    let s = s.trim();
    if s.is_empty() {
        return TaskSubmit::Empty;
    }
    if let Some(rest) = s.strip_prefix("/relay") {
        return TaskSubmit::Relay(rest.trim().to_string());
    }
    if let Some(rest) = s.strip_prefix("/workflow") {
        let (name, prompt) = split_first_word(rest.trim());
        return TaskSubmit::Workflow { name, prompt };
    }
    if let Some(rest) = s.strip_prefix("/project") {
        let (name, prompt) = split_first_word(rest.trim());
        return TaskSubmit::Project { name, prompt };
    }
    if let Some(rest) = s.strip_prefix("/save") {
        return TaskSubmit::Save(rest.trim().to_string());
    }
    TaskSubmit::Prompt(s.to_string())
}

/// The partial name argument of `cmd` in `s`, when the cursor is still inside
/// that argument (`"/workflow tri"` -> `Some("tri")`; a second word means the
/// name is done -> `None`).
fn name_argument<'a>(s: &'a str, cmd: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(cmd)?;
    let rest = rest.strip_prefix(' ')?;
    let partial = rest.trim_start();
    (!partial.contains(' ')).then_some(partial)
}

/// A generic single-line input overlay (Rename tab, and future prompts): a
/// titled box with one editable line, no slash-command parsing.
#[derive(Clone, Debug)]
pub struct LineInput {
    pub buf: String,
    cursor: usize,
    title: String,
}

impl LineInput {
    pub fn new(title: impl Into<String>) -> Self {
        LineInput {
            buf: String::new(),
            cursor: 0,
            title: title.into(),
        }
    }

    /// An input prefilled with the current value, so a rename edits rather
    /// than retypes.
    pub fn with_text(title: impl Into<String>, text: impl Into<String>) -> Self {
        let buf = text.into();
        let cursor = buf.len();
        LineInput {
            buf,
            cursor,
            title: title.into(),
        }
    }

    pub fn insert(&mut self, c: char) {
        edit_line(&mut self.buf, &mut self.cursor, LineKey::Char(c));
    }

    pub fn backspace(&mut self) {
        edit_line(&mut self.buf, &mut self.cursor, LineKey::Backspace);
    }

    /// Apply a normalized editing key from the shared cross-terminal decoder.
    pub fn edit(&mut self, key: LineKey) -> bool {
        edit_line(&mut self.buf, &mut self.cursor, key)
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    /// Render the input overlay showing the current line + a block cursor.
    pub fn overlay(&self) -> Overlay {
        let shown = line_with_cursor(&self.buf, self.cursor, BOX_WIDTH.saturating_sub(2));
        Overlay::with_footer(
            self.title.clone(),
            vec![String::new(), format!("> {shown}"), String::new()],
            &[("enter", "apply"), ("esc", "cancel")],
        )
    }
}

/// Split "word rest of the line" into (word, rest).
fn split_first_word(s: &str) -> (String, String) {
    match s.split_once(char::is_whitespace) {
        Some((w, rest)) => (w.to_string(), rest.trim().to_string()),
        None => (s.to_string(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_the_buffer() {
        let mut t = TaskInput::new();
        for c in "fix bug".chars() {
            t.insert(c);
        }
        t.backspace();
        assert_eq!(t.buf, "fix bu");
        assert_eq!(t.parse().submit, TaskSubmit::Prompt("fix bu".into()));
    }

    #[test]
    fn empty_does_not_submit() {
        assert_eq!(TaskInput::new().parse().submit, TaskSubmit::Empty);
        let mut t = TaskInput::new();
        t.insert(' ');
        assert_eq!(t.parse().submit, TaskSubmit::Empty);
    }

    fn with_buf(s: &str) -> TaskInput {
        TaskInput {
            buf: s.into(),
            ..Default::default()
        }
    }

    #[test]
    fn suggests_slash_commands_when_empty_or_partial() {
        let all: Vec<String> = TaskInput::new()
            .suggestions()
            .iter()
            .map(|s| s.label.clone())
            .collect();
        assert_eq!(
            all,
            ["/relay", "/workflow", "/project", "/save", "@provider"]
        );
        // A partial filters; accepting a name-taking command appends a space.
        let mut t = with_buf("/wo");
        let sug = t.suggestions();
        assert_eq!(sug.len(), 1);
        assert_eq!(sug[0].label, "/workflow");
        assert!(t.accept());
        assert_eq!(t.buf, "/workflow ");
        // A plain prompt gets no suggestions.
        assert!(with_buf("fix the tests").suggestions().is_empty());
    }

    #[test]
    fn suggests_workflow_templates_and_project_names() {
        let mut t = with_buf("/workflow ");
        let names: Vec<String> = t.suggestions().iter().map(|s| s.label.clone()).collect();
        assert_eq!(names, ["solo", "pair", "triad"]);
        t.buf = "/workflow tri".into();
        assert!(t.accept());
        assert_eq!(t.buf, "/workflow triad ");
        // Past the name (a second word), suggestions stop.
        assert!(with_buf("/workflow triad build it")
            .suggestions()
            .is_empty());

        let mut p = with_buf("/project a");
        p.projects = vec!["acme".into(), "beta".into()];
        let names: Vec<String> = p.suggestions().iter().map(|s| s.label.clone()).collect();
        assert_eq!(names, ["acme"]);
        assert!(p.accept());
        assert_eq!(p.buf, "/project acme ");
    }

    #[test]
    fn selection_wraps_and_tab_uses_it() {
        let mut t = TaskInput::new(); // empty -> 4 commands + the @provider row
        t.sel_down();
        assert_eq!(t.sel, 1);
        t.sel_up();
        t.sel_up();
        assert_eq!(t.sel, 4); // wrapped to the last row (@provider)
        assert!(t.accept());
        assert_eq!(t.buf, "@");
    }

    #[test]
    fn suggests_installed_agents_after_at() {
        let mut t = with_buf("@c");
        t.agents = vec!["claude".into(), "codex".into(), "gemini".into()];
        let names: Vec<String> = t.suggestions().iter().map(|s| s.label.clone()).collect();
        assert_eq!(names, ["@claude", "@codex"]);
        assert!(t.accept());
        assert_eq!(t.buf, "@claude ");
        // Mid-line: "fix it @ge" completes in place, keeping the prompt.
        let mut m = with_buf("fix it @ge");
        m.agents = vec!["claude".into(), "gemini".into()];
        assert!(m.accept());
        assert_eq!(m.buf, "fix it @gemini ");

        let mut role = with_buf("/workflow pair @verifier=c");
        role.agents = vec!["claude".into(), "codex".into(), "gemini".into()];
        let names: Vec<String> = role
            .suggestions()
            .iter()
            .map(|suggestion| suggestion.label.clone())
            .collect();
        assert_eq!(names, ["@verifier=claude", "@verifier=codex"]);
        assert!(role.accept());
        assert_eq!(role.buf, "/workflow pair @verifier=claude ");
    }

    #[test]
    fn parse_extracts_the_agent_word() {
        let p = with_buf("@claude fix the tests").parse();
        assert_eq!(p.agent.as_deref(), Some("claude"));
        assert_eq!(p.submit, TaskSubmit::Prompt("fix the tests".into()));
        let w = with_buf("/workflow pair @codex ship the feature").parse();
        assert_eq!(w.agent.as_deref(), Some("codex"));
        assert_eq!(
            w.submit,
            TaskSubmit::Workflow {
                name: "pair".into(),
                prompt: "ship the feature".into()
            }
        );
        let mixed =
            with_buf("/workflow triad @claude @planner=gemini @builder=codex ship the feature")
                .parse();
        assert_eq!(mixed.agent.as_deref(), Some("claude"));
        assert_eq!(
            mixed.role_providers,
            [
                uniterm_core::orchestrate::RoleProviderSelection {
                    role: "planner".into(),
                    provider: "gemini".into(),
                },
                uniterm_core::orchestrate::RoleProviderSelection {
                    role: "builder".into(),
                    provider: "codex".into(),
                },
            ]
        );
        assert_eq!(
            mixed.submit,
            TaskSubmit::Workflow {
                name: "triad".into(),
                prompt: "ship the feature".into(),
            }
        );
        // A lone '@' is not an agent selection.
        let n = with_buf("email me @ noon").parse();
        assert_eq!(n.agent, None);
    }

    #[test]
    fn overlay_geometry_is_stable_while_typing() {
        // The box must not change size as suggestions appear/disappear, or the
        // client would leave residue behind a shrinking overlay.
        let empty = TaskInput::new().overlay().geometry(100, 30);
        let mid = with_buf("/workflow ").overlay().geometry(100, 30);
        let plain = with_buf("a plain prompt with no completions at all")
            .overlay()
            .geometry(100, 30);
        assert_eq!((empty.w, empty.h), (mid.w, mid.h));
        assert_eq!((empty.w, empty.h), (plain.w, plain.h));
    }

    #[test]
    fn recognizes_slash_commands() {
        let mk = |s: &str| with_buf(s).parse().submit;
        assert_eq!(
            mk("/relay build then verify"),
            TaskSubmit::Relay("build then verify".into())
        );
        assert_eq!(
            mk("/workflow ship implement the thing"),
            TaskSubmit::Workflow {
                name: "ship".into(),
                prompt: "implement the thing".into()
            }
        );
        assert_eq!(
            mk("/project acme"),
            TaskSubmit::Project {
                name: "acme".into(),
                prompt: String::new()
            }
        );
        assert_eq!(mk("/save"), TaskSubmit::Save(String::new()));
        assert_eq!(
            mk("/save review the PR"),
            TaskSubmit::Save("review the PR".into())
        );
        // A leading slash that isn't a known command is just a prompt.
        assert_eq!(
            mk("/usr/bin/env"),
            TaskSubmit::Prompt("/usr/bin/env".into())
        );
    }
}
