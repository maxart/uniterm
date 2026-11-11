//! The New Task input model: an editable prompt with launch options and inline
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
    ("/project", "launch inside a Workspace project", true),
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
    /// Project-name completions (from the Workspace catalog), filled in
    /// asynchronously after the modal opens.
    pub projects: Vec<String>,
    /// Installed provider ids (from the server's PATH probe), same round trip.
    pub agents: Vec<String>,
    /// The selected suggestion row.
    pub sel: usize,
    pub project: Option<String>,
    pub active_project: String,
    pub project_roots: Vec<(String, String)>,
    pub launch: Vec<uniterm_proto::TaskProviderOptions>,
    pub agent: Option<String>,
    pub mode: usize,
    pub target: usize,
    pub roles: Vec<uniterm_core::orchestrate::RoleProviderSelection>,
    pub model: String,
    pub profile: String,
    pub source_task: Option<u64>,
    pub options_open: bool,
    option: usize,
    pub pending: bool,
    pub error: Option<String>,
    input_bytes: Vec<u8>,
    pasting: bool,
    literal_prompt: bool,
}

/// Composer intent emitted only by an explicit key outside bracketed paste.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputAction {
    Redraw,
    Submit,
    Cancel,
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

    /// Preserve bracketed paste across reads; pasted newlines never launch a task.
    pub fn handle_input(&mut self, bytes: &[u8]) -> InputAction {
        if self.pending {
            return InputAction::Redraw;
        }
        self.input_bytes.extend_from_slice(bytes);
        loop {
            if self.pasting {
                let Some(end) = self.input_bytes.windows(6).position(|w| w == b"\x1b[201~") else {
                    if self.input_bytes.len() > 262_144 {
                        self.error = Some("Paste exceeds 256 KiB".into());
                        self.input_bytes.drain(..self.input_bytes.len() - 5);
                    }
                    return InputAction::Redraw;
                };
                let text = String::from_utf8_lossy(&self.input_bytes[..end])
                    .replace("\r\n", "\n")
                    .replace('\r', "\n");
                self.input_bytes.drain(..end + 6);
                if self.error.as_deref() != Some("Paste exceeds 256 KiB") {
                    for c in text
                        .chars()
                        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
                    {
                        if self.options_open {
                            self.option_key(LineKey::Char(c));
                        } else {
                            self.insert(c);
                        }
                    }
                }
                self.pasting = false;
                continue;
            }
            if self.input_bytes.is_empty() {
                return InputAction::Redraw;
            }
            if self.input_bytes.starts_with(b"\x1b[200~") {
                self.input_bytes.drain(..6);
                self.pasting = true;
                continue;
            }
            if self.input_bytes.len() > 1 && b"\x1b[200~".starts_with(&self.input_bytes) {
                return InputAction::Redraw;
            }
            if self.input_bytes[0] == 0x0f {
                self.input_bytes.remove(0);
                if self.options_open {
                    self.options_open = false;
                } else {
                    self.open_options();
                }
                continue;
            }
            if self.input_bytes[0] == 0x0a && !self.options_open {
                self.input_bytes.remove(0);
                self.insert('\n');
                continue;
            }
            let (key, used) = crate::text_input::decode_key(&self.input_bytes, 0);
            self.input_bytes
                .drain(..used.max(1).min(self.input_bytes.len()));
            if self.options_open {
                self.option_key(key);
                continue;
            }
            match key {
                LineKey::Escape | LineKey::Cancel => {
                    self.input_bytes.clear();
                    return InputAction::Cancel;
                }
                LineKey::Enter => {
                    self.input_bytes.clear();
                    return InputAction::Submit;
                }
                LineKey::Tab => {
                    self.accept();
                }
                LineKey::Up => self.sel_up(),
                LineKey::Down => self.sel_down(),
                key => {
                    self.edit(key);
                }
            }
        }
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
    /// - `/project <partial-name>`: actual Workspace Project names;
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
                    insert: format!(
                        "/project {} ",
                        if p.contains(char::is_whitespace) {
                            format!("\"{p}\"")
                        } else {
                            p.clone()
                        }
                    ),
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
        let mut agent = self.agent.clone();
        let mut role_providers = self.roles.clone();
        let literal = self.literal_prompt || self.buf.trim_start().starts_with("-- ");
        let mut line = String::new();
        let mut remaining = self.buf.as_str();
        if self.literal_prompt {
            line.push_str(remaining);
            remaining = "";
        }
        while !remaining.is_empty() {
            let word_end = remaining
                .find(char::is_whitespace)
                .unwrap_or(remaining.len());
            let word = &remaining[..word_end];
            let rest = &remaining[word_end..];
            let space_end = rest
                .find(|c: char| !c.is_whitespace())
                .unwrap_or(rest.len());
            let space = &rest[..space_end];
            remaining = &rest[space_end..];
            if word == "--" {
                line.push_str(remaining);
                break;
            }
            let mut option = false;
            if let Some(value) = word.strip_prefix('@') {
                if let Some((role, provider)) = value.split_once('=') {
                    if !role.is_empty() && !provider.is_empty() {
                        role_providers.push(uniterm_core::orchestrate::RoleProviderSelection {
                            role: role.into(),
                            provider: provider.into(),
                        });
                        option = true;
                    }
                } else if uniterm_core::agent::provider(value).is_some()
                    || self.agents.iter().any(|a| a == value)
                {
                    agent = Some(value.into());
                    option = true;
                }
            }
            if !option {
                line.push_str(word);
                line.push_str(space);
            }
        }
        let mut submit = if literal {
            TaskSubmit::Prompt(line)
        } else {
            parse_line(&line)
        };
        if let TaskSubmit::Prompt(prompt) = submit {
            submit = match self.mode {
                1 => TaskSubmit::Workflow {
                    name: "solo".into(),
                    prompt,
                },
                2 => TaskSubmit::Workflow {
                    name: "pair".into(),
                    prompt,
                },
                3 => TaskSubmit::Workflow {
                    name: "triad".into(),
                    prompt,
                },
                4 => TaskSubmit::Relay(prompt),
                5 => TaskSubmit::Save(prompt),
                _ => TaskSubmit::Prompt(prompt),
            };
        }
        Parsed {
            agent,
            role_providers,
            submit,
        }
    }

    /// Reuse a task as an explicitly linked new attempt without losing its history.
    pub fn from_task(task: uniterm_proto::TaskEntry) -> Self {
        let mode = match task.context.workflow.as_deref() {
            Some("solo") => 1,
            Some("pair") => 2,
            Some("triad") => 3,
            _ if task.context.relay => 4,
            _ => 0,
        };
        let title = task.context.prompt.unwrap_or(task.title);
        let buf = if task.notes.is_empty() {
            title
        } else {
            format!("{}\n\n{}", title, task.notes)
        };
        Self {
            cursor: buf.len(),
            buf,
            project: task.project_name,
            agent: task.context.provider,
            model: task.context.model.unwrap_or_default(),
            profile: task.context.profile.unwrap_or_default(),
            source_task: Some(task.id),
            roles: task.context.role_providers,
            mode,
            options_open: true,
            literal_prompt: true,
            ..Default::default()
        }
    }

    fn open_options(&mut self) {
        let parsed = self.parse();
        let prompt = match parsed.submit {
            TaskSubmit::Empty => String::new(),
            TaskSubmit::Prompt(prompt) => prompt,
            TaskSubmit::Project { name, prompt } => {
                self.project = Some(name);
                prompt
            }
            TaskSubmit::Workflow { name, prompt } => {
                self.mode = match name.as_str() {
                    "solo" => 1,
                    "pair" => 2,
                    "triad" => 3,
                    _ => {
                        self.error = Some("Choose a supported workflow: solo, pair, triad".into());
                        return;
                    }
                };
                prompt
            }
            TaskSubmit::Relay(prompt) => {
                self.mode = 4;
                prompt
            }
            TaskSubmit::Save(prompt) => {
                self.mode = 5;
                prompt
            }
        };
        self.agent = parsed.agent;
        self.roles = parsed.role_providers;
        self.buf = prompt;
        self.cursor = self.buf.len();
        self.literal_prompt = true;
        self.options_open = true;
    }

    fn role_names(&self) -> Vec<&str> {
        let parsed = self.parse();
        match parsed.submit {
            TaskSubmit::Workflow { name, .. } => WORKFLOW_TEMPLATES
                .iter()
                .find(|t| t.name == name)
                .map(|t| t.roles.iter().map(|r| r.name).collect())
                .unwrap_or_default(),
            TaskSubmit::Relay(_) => vec!["builder", "reviewer"],
            _ => Vec::new(),
        }
    }

    fn option_rows(&self) -> Vec<(String, String)> {
        let modes = [
            "Single agent",
            "Workflow: solo",
            "Workflow: pair",
            "Workflow: triad",
            "Relay",
            "Save for later",
        ];
        let mut rows = vec![
            (
                "Project".into(),
                self.project
                    .as_ref()
                    .unwrap_or(&self.active_project)
                    .clone(),
            ),
            (
                "Agent".into(),
                self.agent.clone().unwrap_or_else(|| {
                    self.agents
                        .first()
                        .map(|a| format!("{a} (default)"))
                        .unwrap_or_else(|| "No installed agent".into())
                }),
            ),
            ("Mode".into(), modes[self.mode].into()),
            (
                "Placement".into(),
                if self.role_names().is_empty() {
                    ["New pane", "New tab", "Child tab"][self.target].into()
                } else {
                    "New tab with role panes".into()
                },
            ),
        ];
        for role in self.role_names() {
            let value = self
                .roles
                .iter()
                .find(|r| r.role == role)
                .map(|r| r.provider.clone())
                .unwrap_or_else(|| "inherit agent".into());
            rows.push((format!("Role: {role}"), value));
        }
        let agent = self.parse().agent.or_else(|| self.agents.first().cloned());
        if self.role_names().is_empty() {
            if let Some(cap) = self
                .launch
                .iter()
                .find(|cap| Some(&cap.id) == agent.as_ref())
            {
                if cap.model {
                    rows.push(("Model".into(), self.model.clone()));
                }
                if cap.profile {
                    rows.push(("Profile / agent".into(), self.profile.clone()));
                }
            }
        }
        rows
    }

    /// Options use the same parsed launch request as quick entry.
    pub fn option_key(&mut self, key: LineKey) {
        let rows = self.option_rows();
        self.option = self.option.min(rows.len().saturating_sub(1));
        match key {
            LineKey::Up => self.option = (self.option + rows.len() - 1) % rows.len(),
            LineKey::Down | LineKey::Tab => self.option = (self.option + 1) % rows.len(),
            LineKey::Escape | LineKey::Enter => self.options_open = false,
            LineKey::Left | LineKey::Right => {
                let forward = key == LineKey::Right;
                let cycle = |index: usize, len: usize| {
                    if forward {
                        (index + 1) % len
                    } else {
                        (index + len - 1) % len
                    }
                };
                match self.option {
                    0 => {
                        let mut choices = vec![None];
                        choices.extend(self.projects.iter().cloned().map(Some));
                        let i = choices.iter().position(|p| p == &self.project).unwrap_or(0);
                        self.project = choices[cycle(i, choices.len())].clone();
                    }
                    1 => {
                        let mut choices = vec![None];
                        choices.extend(self.agents.iter().cloned().map(Some));
                        let i = choices.iter().position(|p| p == &self.agent).unwrap_or(0);
                        self.agent = choices[cycle(i, choices.len())].clone();
                        self.model.clear();
                        self.profile.clear();
                    }
                    2 => {
                        self.mode = cycle(self.mode, 6);
                        self.roles.clear();
                        self.model.clear();
                        self.profile.clear();
                    }
                    3 if self.role_names().is_empty() => self.target = cycle(self.target, 3),
                    3 => {}
                    index => {
                        if let Some(role) = self.role_names().get(index - 4).map(|s| s.to_string())
                        {
                            let old = self
                                .roles
                                .iter()
                                .find(|r| r.role == role)
                                .map(|r| r.provider.clone());
                            let mut choices = vec![None];
                            choices.extend(self.agents.iter().cloned().map(Some));
                            let i = choices.iter().position(|p| p == &old).unwrap_or(0);
                            self.roles.retain(|r| r.role != role);
                            if let Some(provider) = choices[cycle(i, choices.len())].clone() {
                                self.roles
                                    .push(uniterm_core::orchestrate::RoleProviderSelection {
                                        role,
                                        provider,
                                    });
                            }
                        }
                    }
                }
            }
            LineKey::Char(c) if !c.is_control() => match rows[self.option].0.as_str() {
                "Model" => self.model.push(c),
                "Profile / agent" => self.profile.push(c),
                _ => {}
            },
            LineKey::Backspace => match rows[self.option].0.as_str() {
                "Model" => {
                    self.model.pop();
                }
                "Profile / agent" => {
                    self.profile.pop();
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Render the New Task overlay: the input line, a fixed-height suggestion
    /// list (selection marked), and the key hints. Every line is padded to a
    /// fixed width so the box geometry never changes while typing.
    pub fn overlay(&self) -> Overlay {
        let pad = |s: String| {
            use unicode_width::UnicodeWidthStr as _;
            let text = crate::overlay::clip_text(&s, BOX_WIDTH);
            format!(
                "{text}{}",
                " ".repeat(BOX_WIDTH.saturating_sub(text.width()))
            )
        };
        let shown = line_with_cursor(&self.buf, self.cursor, BOX_WIDTH.saturating_sub(4));
        let mut lines = vec![String::new(), pad(format!("> {shown}"))];
        lines.push(String::new());
        let sug = self.suggestions();
        let start = self.sel.saturating_sub(SUGGESTION_ROWS - 1);
        for slot in 0..SUGGESTION_ROWS {
            let i = start + slot;
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
        if self.options_open {
            lines.truncate(3);
            let options = self.option_rows();
            let start = self.option.saturating_sub(4);
            for i in start..start + 5 {
                lines.push(pad(options
                    .get(i)
                    .map(|(name, value)| {
                        format!(
                            "{} {name}: {}",
                            if i == self.option { ">" } else { " " },
                            if value.is_empty() {
                                "native default"
                            } else {
                                value
                            }
                        )
                    })
                    .unwrap_or_default()));
            }
        }
        let parsed_project = match self.parse().submit {
            TaskSubmit::Project { name, .. } => Some(name),
            _ => None,
        };
        let selected = parsed_project
            .as_ref()
            .or(self.project.as_ref())
            .unwrap_or(&self.active_project);
        let root = self
            .project_roots
            .iter()
            .find(|(name, _)| name == selected)
            .map(|(_, root)| root.as_str())
            .unwrap_or("active project");
        lines.push(pad(format!("Directory: {root}")));
        lines.push(pad(self.error.clone().unwrap_or_else(|| {
            if self.pending {
                "Launching...".into()
            } else if self.options_open {
                "Up/down: field; left/right: choose; type model/profile".into()
            } else {
                "Ctrl-O options | Ctrl-J newline | -- literal prompt".into()
            }
        })));
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
    if s.trim().is_empty() {
        return TaskSubmit::Empty;
    }
    if let Some(rest) = command_rest(s, "/relay") {
        return TaskSubmit::Relay(rest.trim().to_string());
    }
    if let Some(rest) = command_rest(s, "/workflow") {
        let (name, prompt) = split_first_word(rest.trim());
        return TaskSubmit::Workflow { name, prompt };
    }
    if let Some(rest) = command_rest(s, "/project") {
        let (name, prompt) = split_first_word(rest.trim());
        return TaskSubmit::Project { name, prompt };
    }
    if let Some(rest) = command_rest(s, "/save") {
        return TaskSubmit::Save(rest.trim().to_string());
    }
    TaskSubmit::Prompt(s.to_string())
}

fn command_rest<'a>(text: &'a str, command: &str) -> Option<&'a str> {
    let rest = text.trim_start().strip_prefix(command)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some(rest)
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
    if let Some(rest) = s.strip_prefix('"') {
        if let Some((name, prompt)) = rest.split_once('"') {
            return (name.to_string(), prompt.trim_start().to_string());
        }
    }
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
    #[test]
    fn literal_prompts_and_command_boundaries_survive() {
        for prompt in [
            "Review @README.md and @scope/package",
            "first  line\n  indented\ttext",
            "/workflow-not-a-command",
            "/save/path",
        ] {
            assert_eq!(
                with_buf(prompt).parse().submit,
                TaskSubmit::Prompt(prompt.into())
            );
        }
        assert_eq!(
            with_buf("-- /workflow literal @claude").parse().submit,
            TaskSubmit::Prompt("/workflow literal @claude".into())
        );
        assert_eq!(
            with_buf("/project \"Project B\" preserve  spaces")
                .parse()
                .submit,
            TaskSubmit::Project {
                name: "Project B".into(),
                prompt: "preserve  spaces".into()
            }
        );
    }

    #[test]
    fn suggestions_scroll_to_every_provider_and_wrap() {
        let mut input = with_buf("@");
        input.agents = (0..8).map(|i| format!("provider{i}")).collect();
        for i in 0..8 {
            assert!(input
                .overlay()
                .lines
                .iter()
                .any(|line| line.contains(&format!("▸ @provider{i}"))));
            input.sel_down();
        }
        assert_eq!(input.sel, 0);
        input.sel_up();
        assert!(input.accept());
        assert_eq!(input.buf, "@provider7 ");
    }

    #[test]
    fn multiline_paste_is_literal_and_cannot_submit() {
        let mut input = TaskInput::new();
        assert_eq!(input.handle_input(b"\x1b[200~first\n"), InputAction::Redraw);
        assert_eq!(
            input.handle_input("界\n  next\tline\x1b[20".as_bytes()),
            InputAction::Redraw
        );
        assert_eq!(input.handle_input(b"1~"), InputAction::Redraw);
        assert_eq!(input.buf, "first\n界\n  next\tline");
        assert_eq!(input.handle_input(b"\r"), InputAction::Submit);
        input.pending = true;
        assert_eq!(input.handle_input(b"\r"), InputAction::Redraw);
    }

    #[test]
    fn fragmented_paste_targets_only_editable_options() {
        let mut input = with_buf("original prompt");
        input.agents = vec!["test".into()];
        input.launch = vec![uniterm_proto::TaskProviderOptions {
            id: "test".into(),
            model: true,
            profile: true,
        }];
        input.options_open = true;
        input.option = 4;
        input.handle_input(b"\x1b[20");
        input.handle_input(b"0~model\r\n");
        input.handle_input(b"value\x1b[201~");
        assert_eq!(input.model, "modelvalue");
        input.option = 5;
        input.handle_input(b"\x1b[200~profile\tvalue\x1b[201~");
        assert_eq!(input.profile, "profilevalue");
        input.option = 0;
        input.handle_input(b"\x1b[200~ignored\x1b[201~");
        assert_eq!(input.project, None);
        assert_eq!(input.buf, "original prompt");
    }

    #[test]
    fn structured_options_preserve_prompt_and_native_defaults() {
        let mut input = with_buf("build  the feature");
        input.projects = vec!["Project B".into()];
        input.agents = vec!["codex".into()];
        input.option_key(LineKey::Right);
        assert_eq!(input.project.as_deref(), Some("Project B"));
        input.option_key(LineKey::Down);
        input.option_key(LineKey::Right);
        assert_eq!(input.agent.as_deref(), Some("codex"));
        input.option_key(LineKey::Down);
        input.option_key(LineKey::Right);
        assert_eq!(
            input.parse().submit,
            TaskSubmit::Workflow {
                name: "solo".into(),
                prompt: "build  the feature".into()
            }
        );
        assert_eq!(input.model, "");
        assert_eq!(input.profile, "");
    }
}
