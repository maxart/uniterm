//! A width-aware VT emulator: bytes in, [`Grid`] mutations out.
//!
//! We use `vte` (Alacritty's parser) as the escape-sequence state machine and
//! lay our own cell model + cursor semantics on top, exactly as `docs/04` calls
//! for. Grapheme clusters, wide cells, scroll regions, alternate screens,
//! destructive edits, ANSI and DEC modes, semantic OSC routing, and reflow all
//! share the same damage-tracked grid contract used by the renderer.

use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};
use uniterm_core::{AgentDetail, AgentStatus, Attrs, Cell, Color, Grid, LoopState, UnderlineStyle};
use vte::{Params, Parser, Perform};

/// A parsed OSC 777 agent event: the agent id and any status it implies. The
/// server binds these to the pane (agent colour + reconciled status). See
/// `docs/06-agentic-supervision.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEvent {
    pub agent: Option<String>,
    pub status: Option<AgentStatus>,
    /// True only when the provider explicitly announced that it can accept a
    /// new prompt. Heuristic idle evidence never creates this signal.
    pub cooperative_ready: bool,
    /// A real tool completion warrants one descendant-listener discovery.
    pub tool_completed: bool,
    /// Provider-owned native session identity, if its connector exposes one.
    pub session_id: Option<String>,
    /// Parent identity for a child session; never replaces the Pane owner.
    pub parent_session_id: Option<String>,
    /// Complete provider-owned argv used for native resume. Unknown fields are
    /// ignored, so connectors can adopt this without a protocol flag day.
    pub resume_command: Vec<String>,
    /// Provider-owned transcript location, if its connector exposes one.
    pub transcript_path: Option<String>,
    /// Provider-neutral details carried by this envelope. A detail never
    /// implies a status: `notification` and `loop` envelopes have none.
    pub details: Vec<AgentDetail>,
    /// An explicit submitted prompt or turn boundary, never inferred from idle.
    pub task: Option<uniterm_core::tasks::TaskObservation>,
}

/// The mouse-reporting protocol a pane's app asked for (DEC private modes
/// 9/1000/1002/1003). The server forwards mouse events to the pane only when
/// the app opted in, at the granularity it asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseMode {
    /// The app did not ask for mouse input.
    #[default]
    Off,
    /// X10 compatibility (`?9h`): presses only.
    X10,
    /// Normal tracking (`?1000h`): presses, releases, wheel.
    Normal,
    /// Button-event tracking (`?1002h`): normal + motion while held.
    Button,
    /// Any-event tracking (`?1003h`): all motion.
    Any,
}

/// A terminal screen: a grid plus the cursor and current graphic state, driven
/// by a `vte` parser.
pub struct Terminal {
    grid: Grid,
    parser: Parser,
    st: EmuState,
    scrollback_limit: usize,
}

/// The attach client requests only disambiguation (flag 1), so never promise
/// release events or alternate-key data the host was not asked to produce.
#[derive(Clone, Copy, Default)]
struct KeyboardMode {
    flags: u16,
    stack: [u16; 16],
    depth: usize,
}

impl KeyboardMode {
    fn push(&mut self, flags: u16) {
        if self.depth == self.stack.len() {
            self.stack.copy_within(1.., 0);
            self.depth -= 1;
        }
        self.stack[self.depth] = self.flags;
        self.depth += 1;
        self.flags = flags & 1;
    }

    fn pop(&mut self, count: usize) {
        if count > self.depth {
            self.flags = 0;
            self.depth = 0;
        } else if count != 0 {
            self.depth -= count;
            self.flags = self.stack[self.depth];
        }
    }
}

struct EmuState {
    width: u16,
    height: u16,
    cx: u16,
    cy: u16,
    fg: Color,
    bg: Color,
    attrs: Attrs,
    underline_color: Color,
    underline_color_handle: u8,
    /// Current OSC 8 hyperlink handle applied to subsequently printed cells.
    hyperlink: u32,
    /// Deferred wrap: set after writing the last column so the wrap happens on
    /// the next printable char, not eagerly (the standard xterm behaviour).
    wrap_pending: bool,
    /// DECAWM (`?7`): whether a glyph beyond the right margin wraps.
    autowrap: bool,
    /// DECOM (`?6`): row addressing is relative to the scroll region.
    origin_mode: bool,
    /// IRM (`CSI 4 h/l`): printed glyphs insert rather than overwrite.
    insert_mode: bool,
    cursor_visible: bool,
    bracketed_paste: bool,
    /// Independent bounded Kitty disambiguation stacks for primary/alternate screens.
    keyboard: [KeyboardMode; 2],
    last_char: Option<char>,
    tabs: Vec<bool>,
    dec_line_drawing: bool,
    /// Vertical scroll region [top, bottom] inclusive (DECSTBM). Line feeds at
    /// `bottom` scroll within this region; a region anchored at the screen top
    /// feeds the scrollback ring.
    scroll_top: u16,
    scroll_bottom: u16,
    /// Saved cursor + graphic state for DECSC/DECRC (ESC 7 / ESC 8).
    saved: Option<(u16, u16, Color, Color, Attrs, Color, bool)>,
    /// While in the alternate screen (`?1049h`), the primary grid is stashed
    /// here and restored on exit (`?1049l`); full-screen apps (vim/less/htop)
    /// thus leave the prior screen intact instead of leaving content behind.
    alt_backup: Option<Grid>,
    alt_cursor: Option<(u16, u16, Color, Color, Attrs, Color, bool)>,
    /// DECCKM (`?1h`): arrow keys the server synthesizes (wheel emulation) must
    /// be SS3 (`ESC O A`) instead of CSI while the app is in application mode.
    app_cursor: bool,
    /// The mouse protocol the app asked for, and whether it wants SGR encoding
    /// (`?1006h`). Drives the server's mouse routing (forward vs scrollback).
    mouse_mode: MouseMode,
    mouse_sgr: bool,
    /// Whether a nested Uniterm client asked the outer attach client to give
    /// its prefix priority. Changes are projected over the client protocol.
    nested_input: bool,
    nested_input_changes: Vec<bool>,
    /// OSC 777 agent events parsed out of the stream, drained by the server.
    agent_events: Vec<AgentEvent>,
    cwd: Option<String>,
    prompt_marks: Vec<String>,
    clipboard_requests: Vec<Vec<u8>>,
    /// Last OSC 0/2 title published by the pane application.
    terminal_title: String,
    title_revision: u64,
    default_fg: Color,
    default_bg: Color,
    dark_theme: bool,
    palette_notifications: bool,
    scrollback_limit: usize,
    /// Bytes to write back to the PTY in answer to terminal queries (DA, DSR,
    /// colour). A real terminal answers these; if we don't, shells that query
    /// on startup (bash+starship, zsh) block until they time out - the
    /// "prompt appears only after a long wait / a Ctrl-C" bug. tmux answers the
    /// same set (see its input_reply).
    responses: Vec<u8>,
}

impl EmuState {
    fn blank_cell(&self) -> Cell {
        Cell {
            ch: ' ',
            fg: self.fg,
            bg: self.bg,
            attrs: Attrs::NONE,
            ..Cell::default()
        }
    }
}

fn default_tabs(width: u16) -> Vec<bool> {
    (0..width).map(|x| x != 0 && x % 8 == 0).collect()
}

impl Terminal {
    pub fn new(width: u16, height: u16) -> Self {
        Terminal {
            grid: Grid::new(width.max(1), height.max(1)),
            parser: Parser::new(),
            st: EmuState {
                width: width.max(1),
                height: height.max(1),
                cx: 0,
                cy: 0,
                fg: Color::DEFAULT,
                bg: Color::DEFAULT,
                attrs: Attrs::NONE,
                underline_color: Color::DEFAULT,
                underline_color_handle: 0,
                hyperlink: 0,
                wrap_pending: false,
                autowrap: true,
                origin_mode: false,
                insert_mode: false,
                cursor_visible: true,
                bracketed_paste: false,
                keyboard: [KeyboardMode::default(); 2],
                last_char: None,
                tabs: default_tabs(width.max(1)),
                dec_line_drawing: false,
                scroll_top: 0,
                scroll_bottom: height.max(1) - 1,
                saved: None,
                alt_backup: None,
                alt_cursor: None,
                app_cursor: false,
                mouse_mode: MouseMode::Off,
                mouse_sgr: false,
                nested_input: false,
                nested_input_changes: Vec::new(),
                agent_events: Vec::new(),
                cwd: None,
                prompt_marks: Vec::new(),
                clipboard_requests: Vec::new(),
                terminal_title: String::new(),
                title_revision: 0,
                default_fg: Color::Rgb(0xcc, 0xcc, 0xcc),
                default_bg: Color::Rgb(0x1e, 0x1e, 0x2e),
                dark_theme: true,
                palette_notifications: false,
                scrollback_limit: 10_000,
                responses: Vec::new(),
            },
            scrollback_limit: 10_000,
        }
    }

    /// Whether the focused application opted into disambiguated CSI-u input.
    /// This belongs to each screen, so another Pane cannot change its keys.
    pub fn keyboard_disambiguation(&self) -> bool {
        self.st.keyboard[usize::from(self.is_alt_screen())].flags & 1 != 0
    }

    /// Take any pending answers to terminal queries; the server writes these
    /// back to the pane's PTY so querying shells do not block.
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.st.responses)
    }

    /// Take any OSC 777 agent events parsed since the last call.
    pub fn take_agent_events(&mut self) -> Vec<AgentEvent> {
        std::mem::take(&mut self.st.agent_events)
    }

    /// Detached hooks join the same lifecycle queue without feeding bytes
    /// through the VT parser or changing any terminal cells.
    pub(crate) fn queue_agent_event(&mut self, event: AgentEvent) {
        self.st.agent_events.push(event);
    }

    /// Whether the foreground terminal application is another Uniterm client.
    pub fn nested_input(&self) -> bool {
        self.st.nested_input
    }

    /// Drain nested-input transitions parsed since the previous PTY batch.
    pub fn take_nested_input_changes(&mut self) -> Vec<bool> {
        std::mem::take(&mut self.st.nested_input_changes)
    }

    /// Current working directory reported cooperatively through OSC 7.
    pub fn reported_cwd(&self) -> Option<&str> {
        self.st.cwd.as_deref()
    }

    /// Monotonic observation revision prevents inherited shell titles naming a new agent.
    pub(crate) fn title_revision(&self) -> u64 {
        self.st.title_revision
    }

    /// Sanitized title last reported through OSC 0 or OSC 2.
    pub fn terminal_title(&self) -> &str {
        &self.st.terminal_title
    }

    /// Set the semantic defaults used to answer color and theme queries.
    /// The server calls this from its configured theme rather than reporting
    /// hard-coded dark colors to every child.
    pub fn set_default_colors(&mut self, foreground: Color, background: Color) {
        let changed = self.st.default_fg != foreground || self.st.default_bg != background;
        self.st.default_fg = foreground;
        self.st.default_bg = background;
        self.st.dark_theme = color_luminance(background) < color_luminance(foreground);
        if changed && self.st.palette_notifications {
            push_theme_report(&mut self.st.responses, self.st.dark_theme);
        }
    }

    /// Drain shell-integration prompt marks received through OSC 133.
    pub fn take_prompt_marks(&mut self) -> Vec<String> {
        std::mem::take(&mut self.st.prompt_marks)
    }

    /// Drain clipboard payloads received through OSC 52 for policy-aware routing.
    pub fn take_clipboard_requests(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.st.clipboard_requests)
    }

    /// Set the scrollback limit (from config).
    pub fn set_scrollback_limit(&mut self, limit: usize) {
        self.scrollback_limit = limit;
        self.st.scrollback_limit = limit;
        self.grid.set_scrollback_limit(limit);
    }

    #[inline]
    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    #[inline]
    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grid
    }

    /// Plain-text bottom of the live screen for agent fallback detection.
    /// This is independent of client copy-mode and scrollback position: the
    /// server owns the grid and snapshots only the current bottom rows.
    pub fn bottom_text(&self, max_lines: usize) -> String {
        let height = self.grid.height() as usize;
        let start = height.saturating_sub(max_lines);
        self.screen_text(start, height)
    }

    /// The region provider rules read: `max_lines` rows ending at the bottom
    /// of the screen, widened upward to include the live cursor row and the
    /// rows above it when the cursor sits higher. A full-screen agent keeps
    /// its footer hints and spinner in the bottom rows below its input box,
    /// while a shell or a freshly launched agent still works near the top;
    /// both must be visible to detection without reading scrollback.
    pub fn evidence_text(&self, max_lines: usize) -> String {
        let height = self.grid.height() as usize;
        let cursor = usize::from(self.st.cy).min(height.saturating_sub(1));
        let start = height
            .saturating_sub(max_lines)
            .min(cursor.saturating_sub(max_lines.saturating_sub(1)));
        self.screen_text(start, height)
    }

    /// Plain text from the rows immediately preceding the live cursor.
    ///
    /// Development servers often announce themselves at the top of a fresh
    /// pane, so sampling only the bottom screen rows can miss the line. This
    /// bounded view follows actual output without reading scrollback or adding
    /// work while the pane is idle.
    pub fn recent_output_text(&self, max_lines: usize) -> String {
        let height = self.grid.height() as usize;
        let end = usize::from(self.st.cy).min(height.saturating_sub(1)) + 1;
        let start = end.saturating_sub(max_lines);
        self.screen_text(start, end)
    }

    /// Bounded recent text across scrollback and the live cursor row for the
    /// control protocol. This never spawns a scraper or changes copy-mode.
    /// The line buffer is reused across rows so an armed output wait does not
    /// allocate per cell on the core loop.
    pub fn automation_output_text(&self, max_lines: usize) -> String {
        let history = self
            .grid
            .total_lines()
            .saturating_sub(self.grid.height() as usize);
        let end = history
            .saturating_add(usize::from(self.st.cy))
            .saturating_add(1)
            .min(self.grid.total_lines());
        let start = end.saturating_sub(max_lines);
        let mut output = String::new();
        let mut line = Vec::new();
        for line_index in start..end {
            line.clear();
            for column in 0..self.grid.width() {
                let cell = self.grid.line_cell(line_index, column);
                if !cell.is_continuation() {
                    self.grid.write_cell_text(cell, &mut line);
                }
            }
            while matches!(line.last(), Some(b' ' | b'\t')) {
                line.pop();
            }
            output.push_str(std::str::from_utf8(&line).unwrap_or_default());
            if line_index + 1 < end {
                output.push('\n');
            }
        }
        output
    }

    fn screen_text(&self, start: usize, end: usize) -> String {
        let mut output = String::with_capacity(usize::from(self.grid.width()) * (end - start));
        let mut line = Vec::with_capacity(usize::from(self.grid.width()));
        for row in start..end {
            line.clear();
            for col in 0..self.grid.width() {
                // Evidence is sampled on output for fast approval. Reuse one
                // row buffer instead of allocating an owned String per cell,
                // including for multi-codepoint graphemes.
                self.grid
                    .write_cell_text(self.grid.get(col, row as u16), &mut line);
            }
            output.push_str(String::from_utf8_lossy(&line).trim_end());
            if row + 1 < end {
                output.push('\n');
            }
        }
        output
    }

    /// Extract an HTTP(S) URL under one grid cell from either its OSC 8 target
    /// or a visibly printed URL token. Uniterm owns mouse reporting during an
    /// attach, so it resolves the link before handing it to the local client.
    pub fn url_at(&self, x: u16, y: u16) -> Option<String> {
        if x >= self.grid.width() || y >= self.grid.height() {
            return None;
        }
        let clicked = self.grid.get(x, y);
        if let Some(uri) = self.grid.hyperlink(clicked.hyperlink) {
            if uri.starts_with("http://") || uri.starts_with("https://") {
                return Some(uri.into());
            }
        }
        let part = |col: u16| {
            let cell = self.grid.get(col, y);
            if cell.is_continuation() {
                return false;
            }
            let text = self.grid.cell_text_owned(cell);
            !text.is_empty()
                && text
                    .chars()
                    .all(|c| c.is_ascii_graphic() && !matches!(c, '"' | '`' | '<' | '>'))
        };
        if !part(x) {
            return None;
        }
        let mut start = x;
        while start > 0 && part(start - 1) {
            start -= 1;
        }
        let mut end = x + 1;
        while end < self.grid.width() && part(end) {
            end += 1;
        }
        let mut token = String::new();
        for col in start..end {
            let cell = self.grid.get(col, y);
            if !cell.is_continuation() {
                token.push_str(&self.grid.cell_text_owned(cell));
            }
        }
        let at = token.find("https://").or_else(|| token.find("http://"))?;
        let url = token[at..].trim_end_matches(['.', ',', ';', ':', ')', ']', '}']);
        let authority = url
            .split_once("//")?
            .1
            .split('/')
            .next()
            .unwrap_or_default();
        (!authority.is_empty() && !authority.chars().any(char::is_whitespace)).then(|| url.into())
    }

    #[inline]
    pub fn cursor(&self) -> (u16, u16) {
        (self.st.cx, self.st.cy)
    }

    /// Whether the app currently holds the alternate screen (htop, vim, ...).
    #[inline]
    pub fn is_alt_screen(&self) -> bool {
        self.st.alt_backup.is_some()
    }

    /// Restore the primary buffer and shell-facing input modes after the
    /// kernel proves that a foreground application returned without cleaning
    /// up its terminal state.
    pub fn recover_shell_screen(&mut self) {
        let Terminal { grid, st, .. } = self;
        Performer { grid, st }.recover_shell_screen();
    }

    /// DECCKM state: application cursor keys (`ESC O A`) vs normal (`ESC [ A`).
    #[inline]
    pub fn app_cursor(&self) -> bool {
        self.st.app_cursor
    }

    /// The mouse protocol the app asked for ([`MouseMode::Off`] if none).
    #[inline]
    pub fn mouse_mode(&self) -> MouseMode {
        self.st.mouse_mode
    }

    /// Whether the app asked for SGR mouse encoding (`?1006h`).
    #[inline]
    pub fn mouse_sgr(&self) -> bool {
        self.st.mouse_sgr
    }

    /// Whether the child requested a visible cursor through DECTCEM.
    #[inline]
    pub fn cursor_visible(&self) -> bool {
        self.st.cursor_visible
    }

    /// Whether pasted text must be bracketed for the active child application.
    #[inline]
    pub fn bracketed_paste(&self) -> bool {
        self.st.bracketed_paste
    }

    /// Resize both terminal buffers by reflowing retained logical lines and
    /// remapping active and saved cursor state into the new viewport.
    pub fn resize(&mut self, width: u16, height: u16) {
        let (w, h) = (width.max(1), height.max(1));
        if w == self.st.width && h == self.st.height {
            return;
        }
        // Reflow both buffers. The visible alternate buffer keeps history
        // disabled, while the stashed primary carries normal scrollback and its
        // independently saved cursor through the same width transformation.
        let blank = self.st.blank_cell();
        let cursor_col = self.st.cx + u16::from(self.st.wrap_pending);
        let cursor =
            self.grid
                .resize_reflow(w, h, blank, (cursor_col, self.st.cy), self.st.wrap_pending);
        self.st.cx = cursor.0;
        self.st.cy = cursor.1;
        self.st.wrap_pending = cursor.2;
        self.st.underline_color_handle = self.grid.intern_underline_color(self.st.underline_color);
        if let Some(primary) = self.st.alt_backup.as_mut() {
            primary.set_scrollback_limit(self.scrollback_limit);
            let saved = self
                .st
                .alt_cursor
                .map(|(cx, cy, .., pending)| (cx + u16::from(pending), cy))
                .unwrap_or((0, 0));
            let saved_pending = self.st.alt_cursor.is_some_and(|(.., pending)| pending);
            let mapped = primary.resize_reflow(w, h, blank, saved, saved_pending);
            if let Some((cx, cy, .., pending)) = self.st.alt_cursor.as_mut() {
                *cx = mapped.0;
                *cy = mapped.1;
                *pending = mapped.2;
            }
            self.grid.set_scrollback_limit(0);
        }
        self.st.width = w;
        self.st.height = h;
        self.st.scroll_top = 0;
        self.st.scroll_bottom = h - 1;
        let mut tabs = default_tabs(w);
        for (x, stop) in self.st.tabs.iter().copied().enumerate().take(w as usize) {
            tabs[x] = stop;
        }
        self.st.tabs = tabs;
    }

    /// Feed raw PTY bytes into the emulator, mutating the grid.
    pub fn feed(&mut self, bytes: &[u8]) {
        // Split the borrows: `parser` drives, `perf` holds grid + state.
        let Terminal {
            grid, parser, st, ..
        } = self;
        let mut scalar = Performer { grid, st };
        // Nothing is buffered across feeds. Keep tiny reads on the original
        // sink, including continuation bytes for vte's pending UTF-8/escapes.
        if bytes.len() <= 1 {
            feed_bytes(parser, &mut scalar, bytes);
            return;
        }
        let mut perf = BufferedPerformer::new(scalar);
        // Probe only a bounded prefix through callbacks, never by interpreting
        // raw bytes. Unproductive feeds use the original sink for the remainder.
        let (probe, rest) = bytes.split_at(bytes.len().min(perf.bytes.len()));
        feed_bytes(parser, &mut perf, probe);
        // The same parser may retain an unfinished escape or UTF-8 sequence.
        // Commit pending cells before either sink can receive its next callback.
        perf.flush();
        if rest.is_empty() {
            return;
        }
        if perf.used_bulk {
            feed_bytes(parser, &mut perf, rest);
            perf.flush();
        } else {
            feed_bytes(parser, &mut perf.inner, rest);
        }
    }

    /// The visible grid as plain text, trailing spaces trimmed. For demos and
    /// tests - the real render path is the damage-tracked [`crate::Renderer`].
    pub fn dump_text(&self) -> String {
        let mut out = String::new();
        for y in 0..self.grid.height() {
            let mut line = String::new();
            for x in 0..self.grid.width() {
                let cell = self.grid.get(x, y);
                if !cell.is_continuation() {
                    line.push_str(&self.grid.cell_text_owned(cell));
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

// Keep one small byte loop per sink type, separate from buffer setup and sink
// selection. This lets vte inline its advance into the loop without duplicating
// it at each feed call site or paying a parser call for every input byte.
#[inline(never)]
fn feed_bytes<P: Perform>(parser: &mut vte::Parser, performer: &mut P, bytes: &[u8]) {
    for &byte in bytes {
        parser.advance(performer, byte);
    }
}

/// The `vte::Perform` sink: applies parsed actions to the grid + cursor.
struct Performer<'a> {
    grid: &'a mut Grid,
    st: &'a mut EmuState,
}

/// Amortize ordinary row writes without interpreting bytes outside vte.
/// All semantic callbacks flush first; the scalar sink remains the authority
/// for graphemes, wide repairs, modes, REP and deferred wrapping.
struct BufferedPerformer<'a> {
    inner: Performer<'a>,
    bytes: [u8; 128],
    len: usize,
    limit: usize,
    first_print: bool,
    // Sticky only within this feed, including bulk writes flushed by callbacks.
    used_bulk: bool,
}

impl<'a> BufferedPerformer<'a> {
    fn new(inner: Performer<'a>) -> Self {
        Self {
            inner,
            bytes: [0; 128],
            len: 0,
            limit: 0,
            first_print: true,
            used_bulk: false,
        }
    }

    // Inspect the actual preceding glyph: an ASCII character following Prepend
    // may have extended a cluster instead of establishing an ordinary boundary.
    fn can_batch(&self) -> bool {
        let st = &self.inner.st;
        if st.insert_mode
            || st.dec_line_drawing
            || st.wrap_pending
            || st.cx == 0
            || st.cx >= st.width
            || st.cy >= st.height
        {
            return false;
        }
        let previous = self.inner.grid.get(st.cx - 1, st.cy);
        previous.width == 1 && previous.cluster == 0 && (' '..='~').contains(&previous.ch)
    }

    fn flush(&mut self) {
        let len = std::mem::take(&mut self.len);
        if len == 1 {
            // A lone tail cannot amortize the bulk writer's setup. Scalar
            // printing also owns any wide repair at this last destination.
            self.inner.print(char::from(self.bytes[0]));
            return;
        }
        let mut offset = 0;
        while offset < len {
            if self.can_batch() {
                let st = &mut self.inner.st;
                let cell = Cell {
                    fg: st.fg,
                    bg: st.bg,
                    attrs: st.attrs,
                    underline_color: st.underline_color_handle,
                    hyperlink: st.hyperlink,
                    ..Cell::default()
                };
                let count =
                    self.inner
                        .grid
                        .set_ascii_prefix(st.cx, st.cy, &self.bytes[offset..len], cell);
                if count != 0 {
                    self.used_bulk |= count >= 2;
                    let end = st.cx + count as u16;
                    if end >= st.width {
                        st.cx = st.width - 1;
                        st.wrap_pending = st.autowrap;
                    } else {
                        st.cx = end;
                    }
                    st.last_char = Some(char::from(self.bytes[offset + count - 1]));
                    offset += count;
                    continue;
                }
            }
            // A wide overlap or complex predecessor needs the complete scalar
            // operation, which also guarantees progress when no prefix fits.
            self.inner.print(char::from(self.bytes[offset]));
            offset += 1;
        }
    }

    #[inline]
    fn finish_run(&mut self) {
        // Control-heavy output often has no pending cells. Do not enter the
        // bulk flush routine merely to discover that its loop is empty.
        if self.len != 0 {
            self.flush();
        }
        self.first_print = true;
    }

    // Keep buffer bookkeeping out of the first-print dispatch so isolated
    // prints do not inherit the continuation path's stack frame.
    #[inline(never)]
    fn print_continuation(&mut self, c: char, ascii: bool) {
        if ascii && (self.len != 0 || self.can_batch()) {
            if self.len == 0 {
                self.limit =
                    usize::from(self.inner.st.width - self.inner.st.cx).min(self.bytes.len());
            }
            self.bytes[self.len] = c as u8;
            self.len += 1;
            if self.len == self.limit {
                self.flush();
            }
        } else {
            self.flush();
            self.first_print = !ascii;
            self.inner.print(c);
        }
    }
}

impl Perform for BufferedPerformer<'_> {
    #[inline(always)]
    fn print(&mut self, c: char) {
        let ascii = (' '..='~').contains(&c);
        if self.first_print {
            // SGR-per-cell and other isolated prints avoid enqueue/eligibility
            // work. Subsequent ASCII still proves its actual glyph boundary.
            self.first_print = !ascii;
            self.inner.print(c);
        } else {
            self.print_continuation(c, ascii);
        }
    }

    // Keep every vte 0.13 Perform callback here, including ignored DCS callbacks:
    // future scalar handlers must never observe a partially committed row.
    fn execute(&mut self, byte: u8) {
        self.finish_run();
        self.inner.execute(byte);
    }

    fn csi_dispatch(&mut self, params: &Params, inter: &[u8], ignore: bool, action: char) {
        self.finish_run();
        self.inner.csi_dispatch(params, inter, ignore, action);
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        self.finish_run();
        self.inner.osc_dispatch(params, bell_terminated);
    }

    fn esc_dispatch(&mut self, inter: &[u8], ignore: bool, byte: u8) {
        self.finish_run();
        self.inner.esc_dispatch(inter, ignore, byte);
    }

    fn hook(&mut self, params: &Params, inter: &[u8], ignore: bool, action: char) {
        self.finish_run();
        self.inner.hook(params, inter, ignore, action);
    }

    fn put(&mut self, byte: u8) {
        self.finish_run();
        self.inner.put(byte);
    }

    fn unhook(&mut self) {
        self.finish_run();
        self.inner.unhook();
    }
}

impl Performer<'_> {
    /// Line feed (index): move down one row, scrolling the region at the bottom.
    fn linefeed(&mut self) {
        if self.st.cy == self.st.scroll_bottom {
            self.scroll_region_up();
        } else if self.st.cy + 1 < self.st.height {
            self.st.cy += 1;
        }
    }

    /// Reverse index: move up one row, scrolling the region down at the top.
    fn reverse_index(&mut self) {
        if self.st.cy == self.st.scroll_top {
            self.scroll_region_down();
        } else if self.st.cy > 0 {
            self.st.cy -= 1;
        }
    }

    /// Scroll the active region up by one line. When the region begins at the
    /// screen top, the departing line goes into the scrollback ring. Inline
    /// applications use this DECSTBM pattern to retain conversation history.
    fn scroll_region_up(&mut self) {
        let (top, bot) = (self.st.scroll_top, self.st.scroll_bottom);
        let blank = self.st.blank_cell();
        self.grid.scroll_region_up(top, bot, blank);
    }

    /// Scroll the active region down by one line (top line blanked).
    fn scroll_region_down(&mut self) {
        let (top, bot) = (self.st.scroll_top, self.st.scroll_bottom);
        let blank = self.st.blank_cell();
        self.grid.scroll_region_down(top, bot, blank);
    }

    /// DCH: delete `n` chars at the cursor, shifting the rest of the line left.
    fn delete_chars(&mut self, n: u16) {
        let blank = self.st.blank_cell();
        self.grid.delete_cells(self.st.cy, self.st.cx, n, blank);
        // Editing a row breaks its soft-wrap relationship and cancels the
        // deferred wrap at the right margin. Leaving either state armed makes
        // the next shell redraw land on the following row, so deleted input
        // remains visible even though readline has removed it.
        self.grid.set_row_wrapped(self.st.cy, false);
        self.st.wrap_pending = false;
    }

    /// ICH: insert `n` blanks at the cursor, shifting the rest of the line right.
    fn insert_chars(&mut self, n: u16) {
        self.st.wrap_pending = false;
        let blank = self.st.blank_cell();
        self.grid.insert_cells(self.st.cy, self.st.cx, n, blank);
    }

    /// ECH: erase `n` chars from the cursor (no shift).
    fn erase_chars(&mut self, n: u16) {
        let blank = self.st.blank_cell();
        self.grid
            .erase_cells(self.st.cy, self.st.cx, self.st.cx.saturating_add(n), blank);
        self.grid.set_row_wrapped(self.st.cy, false);
        self.st.wrap_pending = false;
    }

    /// IL: insert `n` blank lines at the cursor row, within the scroll region.
    fn insert_lines(&mut self, n: u16) {
        self.st.wrap_pending = false;
        if self.st.cy < self.st.scroll_top || self.st.cy > self.st.scroll_bottom {
            return;
        }
        for _ in 0..n {
            let blank = self.st.blank_cell();
            self.grid
                .scroll_region_down(self.st.cy, self.st.scroll_bottom, blank);
        }
    }

    /// DL: delete `n` lines at the cursor row, within the scroll region.
    fn delete_lines(&mut self, n: u16) {
        self.st.wrap_pending = false;
        if self.st.cy < self.st.scroll_top || self.st.cy > self.st.scroll_bottom {
            return;
        }
        for _ in 0..n {
            let blank = self.st.blank_cell();
            self.grid
                .scroll_region_up_discarding(self.st.cy, self.st.scroll_bottom, blank);
        }
    }

    fn erase_in_line(&mut self, mode: u16) {
        let (y, w) = (self.st.cy, self.st.width);
        let blank = self.st.blank_cell();
        let (from, to) = match mode {
            0 => (self.st.cx, w),     // cursor to end
            1 => (0, self.st.cx + 1), // start to cursor
            2 => (0, w),              // whole line
            _ => return,
        };
        self.grid.erase_cells(y, from, to.min(w), blank);
        // EL always cancels a pending wrap. EL 0 also severs the current
        // row's continuation, matching xterm and Ghostty. EL 1 and EL 2
        // intentionally preserve the row flag.
        if mode == 0 {
            self.grid.set_row_wrapped(y, false);
        }
        self.st.wrap_pending = false;
    }

    fn erase_in_display(&mut self, mode: u16) {
        let (w, h) = (self.st.width, self.st.height);
        let blank = self.st.blank_cell();
        match mode {
            0 => {
                // cursor to end of screen
                self.erase_in_line(0);
                for y in (self.st.cy + 1)..h {
                    self.grid.erase_cells(y, 0, w, blank);
                }
            }
            1 => {
                for y in 0..self.st.cy {
                    self.grid.erase_cells(y, 0, w, blank);
                }
                self.erase_in_line(1);
            }
            2 => {
                for y in 0..h {
                    self.grid.erase_cells(y, 0, w, blank);
                }
                self.st.wrap_pending = false;
            }
            3 => self.grid.clear_scrollback(),
            _ => {}
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        // Borrow complete parameter groups: color-heavy TUIs must not copy
        // parameters per SGR, and both extended-colour forms must retain their grouping:
        // semicolon (`38;5;n`, `38;2;r;g;b` - separate single-value groups,
        // consumed by look-ahead) and colon subparams (`38:5:n`, `38:2::r:g:b`
        // - one multi-value group). btop and Claude Code are truecolor-first;
        // dropping 38;2 rendered them monochrome.
        let mut groups = params.iter().peekable();
        if groups.peek().is_none() {
            self.reset_sgr();
            return;
        }
        while let Some(group) = groups.next() {
            let code = group.first().copied().unwrap_or(0);
            match code {
                0 => self.reset_sgr(),
                1 => self.st.attrs.0 |= Attrs::BOLD.0,
                2 => self.st.attrs.0 |= Attrs::DIM.0,
                3 => self.st.attrs.0 |= Attrs::ITALIC.0,
                4 => {
                    let style = group
                        .get(1)
                        .copied()
                        .map_or(Some(UnderlineStyle::Single), underline_style);
                    if let Some(style) = style {
                        self.st.attrs.set_underline_style(style);
                    }
                }
                5 => self.st.attrs.0 |= Attrs::BLINK.0,
                7 => self.st.attrs.0 |= Attrs::REVERSE.0,
                8 => self.st.attrs.0 |= Attrs::INVISIBLE.0,
                9 => self.st.attrs.0 |= Attrs::STRIKE.0,
                22 => self.st.attrs.0 &= !(Attrs::BOLD.0 | Attrs::DIM.0),
                23 => self.st.attrs.0 &= !Attrs::ITALIC.0,
                24 => self.st.attrs.set_underline_style(UnderlineStyle::None),
                25 => self.st.attrs.0 &= !Attrs::BLINK.0,
                27 => self.st.attrs.0 &= !Attrs::REVERSE.0,
                28 => self.st.attrs.0 &= !Attrs::INVISIBLE.0,
                29 => self.st.attrs.0 &= !Attrs::STRIKE.0,
                30..=37 => self.st.fg = Color::Idx((code - 30) as u8),
                39 => self.st.fg = Color::DEFAULT,
                40..=47 => self.st.bg = Color::Idx((code - 40) as u8),
                49 => self.st.bg = Color::DEFAULT,
                90..=97 => self.st.fg = Color::Idx((code - 90 + 8) as u8),
                100..=107 => self.st.bg = Color::Idx((code - 100 + 8) as u8),
                38 | 48 | 58 => {
                    let color = parse_extended_color(group, &mut groups);
                    if let Some(c) = color {
                        if code == 38 {
                            self.st.fg = c;
                        } else if code == 48 {
                            self.st.bg = c;
                        } else {
                            self.st.underline_color = c;
                            self.st.underline_color_handle = self.grid.intern_underline_color(c);
                        }
                    }
                }
                59 => {
                    self.st.underline_color = Color::DEFAULT;
                    self.st.underline_color_handle = 0;
                }
                _ => {}
            }
        }
    }

    // Pre-optimization semantics retained only as a differential test oracle.
    // Add future SGR semantics to both implementations; keep their traversal independent.
    #[cfg(test)]
    fn apply_sgr_reference(&mut self, params: &Params) {
        // Keep every param group whole so both extended-colour forms parse:
        // semicolon (`38;5;n`, `38;2;r;g;b` - separate single-value groups,
        // consumed by look-ahead) and colon subparams (`38:5:n`, `38:2::r:g:b`
        // - one multi-value group). btop and Claude Code are truecolor-first;
        // dropping 38;2 rendered them monochrome.
        let groups: Vec<Vec<u16>> = params.iter().map(|p| p.to_vec()).collect();
        if groups.is_empty() {
            self.reset_sgr();
            return;
        }
        let mut i = 0;
        while i < groups.len() {
            let code = groups[i].first().copied().unwrap_or(0);
            match code {
                0 => self.reset_sgr(),
                1 => self.st.attrs.0 |= Attrs::BOLD.0,
                2 => self.st.attrs.0 |= Attrs::DIM.0,
                3 => self.st.attrs.0 |= Attrs::ITALIC.0,
                4 => {
                    let style = groups[i]
                        .get(1)
                        .copied()
                        .map_or(Some(UnderlineStyle::Single), underline_style);
                    if let Some(style) = style {
                        self.st.attrs.set_underline_style(style);
                    }
                }
                5 => self.st.attrs.0 |= Attrs::BLINK.0,
                7 => self.st.attrs.0 |= Attrs::REVERSE.0,
                8 => self.st.attrs.0 |= Attrs::INVISIBLE.0,
                9 => self.st.attrs.0 |= Attrs::STRIKE.0,
                22 => self.st.attrs.0 &= !(Attrs::BOLD.0 | Attrs::DIM.0),
                23 => self.st.attrs.0 &= !Attrs::ITALIC.0,
                24 => self.st.attrs.set_underline_style(UnderlineStyle::None),
                25 => self.st.attrs.0 &= !Attrs::BLINK.0,
                27 => self.st.attrs.0 &= !Attrs::REVERSE.0,
                28 => self.st.attrs.0 &= !Attrs::INVISIBLE.0,
                29 => self.st.attrs.0 &= !Attrs::STRIKE.0,
                30..=37 => self.st.fg = Color::Idx((code - 30) as u8),
                39 => self.st.fg = Color::DEFAULT,
                40..=47 => self.st.bg = Color::Idx((code - 40) as u8),
                49 => self.st.bg = Color::DEFAULT,
                90..=97 => self.st.fg = Color::Idx((code - 90 + 8) as u8),
                100..=107 => self.st.bg = Color::Idx((code - 100 + 8) as u8),
                38 | 48 | 58 => {
                    let (color, consumed) = parse_extended_color_reference(&groups[i..]);
                    if let Some(c) = color {
                        if code == 38 {
                            self.st.fg = c;
                        } else if code == 48 {
                            self.st.bg = c;
                        } else {
                            self.st.underline_color = c;
                            self.st.underline_color_handle = self.grid.intern_underline_color(c);
                        }
                    }
                    i += consumed;
                    continue;
                }
                59 => {
                    self.st.underline_color = Color::DEFAULT;
                    self.st.underline_color_handle = 0;
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn reset_sgr(&mut self) {
        self.st.fg = Color::DEFAULT;
        self.st.bg = Color::DEFAULT;
        self.st.attrs = Attrs::NONE;
        self.st.underline_color = Color::DEFAULT;
        self.st.underline_color_handle = 0;
    }

    fn save_cursor(&mut self) {
        self.st.saved = Some((
            self.st.cx,
            self.st.cy,
            self.st.fg,
            self.st.bg,
            self.st.attrs,
            self.st.underline_color,
            self.st.wrap_pending,
        ));
    }

    fn restore_cursor(&mut self) {
        if let Some((cx, cy, fg, bg, attrs, underline_color, pending)) = self.st.saved {
            self.st.cx = cx.min(self.st.width - 1);
            self.st.cy = cy.min(self.st.height - 1);
            self.st.fg = fg;
            self.st.bg = bg;
            self.st.attrs = attrs;
            self.st.underline_color = underline_color;
            self.st.underline_color_handle = self.grid.intern_underline_color(underline_color);
            self.st.wrap_pending = pending;
        }
    }

    /// Enter the alternate screen: stash the primary grid + cursor and switch to
    /// a fresh, cleared alt grid (no scrollback). Idempotent.
    fn enter_alt(&mut self) {
        if self.st.alt_backup.is_some() {
            return;
        }
        let (w, h) = (self.st.width, self.st.height);
        let mut alt = Grid::new(w, h);
        alt.set_scrollback_limit(0);
        alt.mark_all_damaged(); // clear the visible screen on enter
        let primary = std::mem::replace(self.grid, alt);
        self.st.alt_backup = Some(primary);
        self.st.underline_color_handle = self.grid.intern_underline_color(self.st.underline_color);
        self.st.alt_cursor = Some((
            self.st.cx,
            self.st.cy,
            self.st.fg,
            self.st.bg,
            self.st.attrs,
            self.st.underline_color,
            self.st.wrap_pending,
        ));
        self.st.cx = 0;
        self.st.cy = 0;
        self.st.hyperlink = 0;
        self.st.wrap_pending = false;
        self.st.scroll_top = 0;
        self.st.scroll_bottom = h - 1;
        self.st.origin_mode = false;
    }

    /// Exit the alternate screen: restore the stashed primary grid + cursor and
    /// force a full repaint of it.
    fn exit_alt(&mut self) {
        if let Some(primary) = self.st.alt_backup.take() {
            *self.grid = primary;
            self.grid.mark_all_damaged();
            if let Some((cx, cy, fg, bg, attrs, underline_color, pending)) =
                self.st.alt_cursor.take()
            {
                self.st.cx = cx.min(self.st.width - 1);
                self.st.cy = cy.min(self.st.height - 1);
                self.st.fg = fg;
                self.st.bg = bg;
                self.st.attrs = attrs;
                self.st.underline_color = underline_color;
                self.st.underline_color_handle = self.grid.intern_underline_color(underline_color);
                self.st.wrap_pending = pending;
            }
            self.st.scroll_top = 0;
            self.st.scroll_bottom = self.st.height - 1;
            self.st.origin_mode = false;
            self.st.hyperlink = 0;
        }
    }

    /// Restore shell-facing terminal state after an application returned
    /// without undoing all of its private modes.
    ///
    /// This is intentionally provider-neutral. It is used by both cooperative
    /// agent lifecycle events and OSC 133 shell prompt boundaries, including
    /// prompt marks forwarded through an SSH process where local foreground
    /// process tracking cannot see the remote child.
    fn recover_shell_screen(&mut self) {
        self.exit_alt();
        self.st.cursor_visible = true;
        self.st.bracketed_paste = false;
        self.st.keyboard = [KeyboardMode::default(); 2];
        self.st.app_cursor = false;
        self.st.mouse_mode = MouseMode::Off;
        self.st.mouse_sgr = false;
        self.set_nested_input(false);
        self.st.origin_mode = false;
        self.st.hyperlink = 0;
        self.st.scroll_top = 0;
        self.st.scroll_bottom = self.st.height - 1;
        self.st.wrap_pending = false;
    }

    fn set_nested_input(&mut self, enabled: bool) {
        if self.st.nested_input != enabled {
            self.st.nested_input = enabled;
            self.st.nested_input_changes.push(enabled);
        }
    }

    fn home(&mut self) {
        self.st.cx = 0;
        self.st.cy = if self.st.origin_mode {
            self.st.scroll_top
        } else {
            0
        };
        self.st.wrap_pending = false;
    }

    fn set_private_mode(&mut self, mode: u16, enabled: bool) {
        match mode {
            1 => self.st.app_cursor = enabled,
            6 => {
                self.st.origin_mode = enabled;
                self.home();
            }
            7 => {
                self.st.autowrap = enabled;
                self.st.wrap_pending = false;
            }
            9 => {
                self.st.mouse_mode = if enabled {
                    MouseMode::X10
                } else {
                    MouseMode::Off
                }
            }
            25 => self.st.cursor_visible = enabled,
            1000 => {
                self.st.mouse_mode = if enabled {
                    MouseMode::Normal
                } else {
                    MouseMode::Off
                }
            }
            1002 => {
                self.st.mouse_mode = if enabled {
                    MouseMode::Button
                } else {
                    MouseMode::Off
                }
            }
            1003 => {
                self.st.mouse_mode = if enabled {
                    MouseMode::Any
                } else {
                    MouseMode::Off
                }
            }
            1006 => self.st.mouse_sgr = enabled,
            1048 => {
                if enabled {
                    self.save_cursor();
                } else {
                    self.restore_cursor();
                }
            }
            47 | 1047 | 1049 => {
                if enabled {
                    self.enter_alt();
                } else {
                    self.exit_alt();
                }
            }
            2004 => self.st.bracketed_paste = enabled,
            // DECSM 2026 synchronizes the child application's output. Parsing
            // remains continuous and the server already coalesces real damage,
            // so no separate timer or frame state is required here.
            2026 => {}
            _ => {}
        }
    }

    fn reset_terminal(&mut self) {
        let (width, height) = (self.st.width, self.st.height);
        let mut grid = Grid::new(width, height);
        grid.set_scrollback_limit(self.st.scrollback_limit);
        grid.mark_all_damaged();
        *self.grid = grid;
        self.st.cx = 0;
        self.st.cy = 0;
        self.st.fg = Color::DEFAULT;
        self.st.bg = Color::DEFAULT;
        self.st.attrs = Attrs::NONE;
        self.st.underline_color = Color::DEFAULT;
        self.st.underline_color_handle = 0;
        self.st.hyperlink = 0;
        self.st.wrap_pending = false;
        self.st.autowrap = true;
        self.st.origin_mode = false;
        self.st.insert_mode = false;
        self.st.cursor_visible = true;
        self.st.bracketed_paste = false;
        self.st.keyboard = [KeyboardMode::default(); 2];
        self.st.last_char = None;
        self.st.tabs = default_tabs(width);
        self.st.dec_line_drawing = false;
        self.st.scroll_top = 0;
        self.st.scroll_bottom = height - 1;
        self.st.saved = None;
        self.st.alt_backup = None;
        self.st.alt_cursor = None;
        self.st.app_cursor = false;
        self.st.mouse_mode = MouseMode::Off;
        self.st.mouse_sgr = false;
        self.set_nested_input(false);
    }
}

/// Convert an extended underline subparameter to the modeled shape.
fn underline_style(value: u16) -> Option<UnderlineStyle> {
    match value {
        0 => Some(UnderlineStyle::None),
        1 => Some(UnderlineStyle::Single),
        2 => Some(UnderlineStyle::Double),
        3 => Some(UnderlineStyle::Curly),
        4 => Some(UnderlineStyle::Dotted),
        5 => Some(UnderlineStyle::Dashed),
        _ => None,
    }
}

/// Consume only the borrowed groups belonging to an extended color.
/// Unknown modes leave the following group for ordinary SGR processing;
/// truncated recognized modes consume the same available fields as before.
fn parse_extended_color<'a>(
    head: &[u16],
    groups: &mut std::iter::Peekable<impl Iterator<Item = &'a [u16]>>,
) -> Option<Color> {
    if head.len() > 1 {
        return match head[1] {
            5 if head.len() >= 3 => Some(Color::Idx(head[2] as u8)),
            2 if head.len() >= 6 => Some(Color::Rgb(head[3] as u8, head[4] as u8, head[5] as u8)),
            2 if head.len() >= 5 => Some(Color::Rgb(head[2] as u8, head[3] as u8, head[4] as u8)),
            _ => None,
        };
    }
    match groups.peek().and_then(|group| group.first()).copied() {
        Some(5) => {
            groups.next();
            groups
                .next()
                .and_then(|group| group.first())
                .copied()
                .map(|n| Color::Idx(n as u8))
        }
        Some(2) => {
            groups.next();
            let mut next = || groups.next().and_then(|group| group.first()).copied();
            match (next(), next(), next()) {
                (Some(r), Some(g), Some(b)) => Some(Color::Rgb(r as u8, g as u8, b as u8)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Parse an extended-colour SGR at `groups[0]` (38, 48, or 58).
/// Returns the colour (if well-formed) and how many groups were consumed.
///
/// Forms (ITU-T T.416 / xterm):
/// - semicolon: `38;5;n` and `38;2;r;g;b` - each value its own group
/// - colon:     `38:5:n`, `38:2:r:g:b`, and `38:2::r:g:b` (with a colourspace
///   id) - one group carrying subparams
#[cfg(test)]
fn parse_extended_color_reference(groups: &[Vec<u16>]) -> (Option<Color>, usize) {
    let head = &groups[0];
    if head.len() > 1 {
        // Colon form: everything is in this one group.
        let color = match head[1] {
            5 if head.len() >= 3 => Some(Color::Idx(head[2] as u8)),
            2 if head.len() >= 6 => {
                // 38:2:<colourspace>:r:g:b
                Some(Color::Rgb(head[3] as u8, head[4] as u8, head[5] as u8))
            }
            2 if head.len() >= 5 => Some(Color::Rgb(head[2] as u8, head[3] as u8, head[4] as u8)),
            _ => None,
        };
        return (color, 1);
    }
    // Semicolon form: look ahead over single-value groups.
    let val = |i: usize| groups.get(i).and_then(|g| g.first()).copied();
    match val(1) {
        Some(5) => match val(2) {
            Some(n) => (Some(Color::Idx(n as u8)), 3),
            None => (None, 2),
        },
        Some(2) => match (val(2), val(3), val(4)) {
            (Some(r), Some(g), Some(b)) => (Some(Color::Rgb(r as u8, g as u8, b as u8)), 5),
            _ => (None, groups.len().min(5)),
        },
        _ => (None, 1),
    }
}

/// First subparam of the `n`th CSI param, or `default` if absent/zero-empty.
fn param(params: &Params, n: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(n)
        .and_then(|p| p.first().copied())
        .filter(|v| *v != 0)
        .unwrap_or(default)
}

/// Whether a CSI carried one of the xterm private-marker bytes (`<`, `=`,
/// `>`, `?`) that change the meaning of the final byte. vte delivers them in
/// the intermediates slice ahead of any true intermediate.
fn has_private_marker(inter: &[u8]) -> bool {
    inter.iter().any(|byte| matches!(byte, b'<'..=b'?'))
}

fn params_flat(params: &Params) -> impl Iterator<Item = u16> + '_ {
    params
        .iter()
        .map(|values| values.first().copied().unwrap_or(0))
}

fn dec_line_drawing(c: char) -> char {
    match c {
        '`' => '◆',
        'a' => '▒',
        'b' => '␉',
        'c' => '␌',
        'd' => '␍',
        'e' => '␊',
        'f' => '°',
        'g' => '±',
        'h' => '␤',
        'i' => '␋',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        _ => c,
    }
}

fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    const ANSI: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xcd, 0x00, 0x00),
        (0x00, 0xcd, 0x00),
        (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee),
        (0xcd, 0x00, 0xcd),
        (0x00, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    match index {
        0..=15 => ANSI[index as usize],
        16..=231 => {
            let value = index - 16;
            let component = |part: u8| if part == 0 { 0 } else { 55 + part * 40 };
            (
                component(value / 36),
                component((value / 6) % 6),
                component(value % 6),
            )
        }
        232..=255 => {
            let grey = 8 + (index - 232) * 10;
            (grey, grey, grey)
        }
    }
}

fn color_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Default => (0, 0, 0),
        Color::Idx(index) => indexed_rgb(index),
        Color::Rgb(red, green, blue) => (red, green, blue),
    }
}

fn color_luminance(color: Color) -> u32 {
    let (red, green, blue) = color_rgb(color);
    u32::from(red) * 299 + u32::from(green) * 587 + u32::from(blue) * 114
}

fn osc_rgb(color: Color) -> String {
    let (red, green, blue) = color_rgb(color);
    format!("rgb:{red:02x}{red:02x}/{green:02x}{green:02x}/{blue:02x}{blue:02x}")
}

fn push_theme_report(output: &mut Vec<u8>, dark: bool) {
    output.extend_from_slice(if dark {
        b"\x1b[?997;1n"
    } else {
        b"\x1b[?997;2n"
    });
}

/// Parse an OSC 777 JSON payload into an [`AgentEvent`]. Unknown/extra fields are
/// ignored; the status is inferred from the `event` name (or a `status` field).
pub(crate) fn parse_osc777(json: &[u8]) -> Option<AgentEvent> {
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        event: Option<String>,
        #[serde(default)]
        status: Option<String>,
        #[serde(default, alias = "sessionId")]
        session_id: Option<String>,
        #[serde(default)]
        parent_session_id: Option<String>,
        #[serde(default, alias = "resumeCommand")]
        resume_command: Vec<String>,
        #[serde(default, alias = "transcriptPath")]
        transcript_path: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default, alias = "notificationKind")]
        notification_kind: Option<String>,
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        tool: Option<String>,
        #[serde(default)]
        preview: Option<String>,
        #[serde(default, alias = "loopState")]
        loop_state: Option<String>,
        #[serde(default, alias = "delaySeconds")]
        delay_seconds: Option<u32>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        turn_id: Option<String>,
        #[serde(default)]
        turn_complete: bool,
        #[serde(default)]
        turn_interrupted: bool,
    }
    let raw: Raw = serde_json::from_slice(json).ok()?;
    let task_id = raw
        .turn_id
        .filter(|id| !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control));
    let task = if raw.event.as_deref() == Some("prompt_submit") {
        raw.prompt.and_then(|text| {
            // Reject oversized payloads rather than silently presenting a
            // truncated assignment as the user's complete request.
            (!text.trim().is_empty() && text.len() <= 16_384 && !text.contains('\0'))
                .then_some(uniterm_core::tasks::TaskObservation::Prompt { id: task_id, text })
        })
    } else if raw.turn_interrupted || raw.event.as_deref() == Some("turn_interrupted") {
        Some(uniterm_core::tasks::TaskObservation::Interrupted { id: task_id })
    } else if raw.turn_complete || raw.event.as_deref() == Some("turn_complete") {
        Some(uniterm_core::tasks::TaskObservation::Finished { id: task_id })
    } else {
        None
    };
    let mut details = Vec::new();
    if let Some(summary) = raw.summary {
        details.push(AgentDetail::Summary {
            text: uniterm_core::agent_detail::bounded_text(
                &summary,
                uniterm_core::agent_detail::DETAIL_TEXT_LIMIT,
            ),
        });
    }
    // A notification is recorded whenever the envelope carries one, whatever
    // status (if any) the connector derived from the provider's own type.
    if raw.event.as_deref() == Some("notification")
        || raw.message.is_some()
        || raw.notification_kind.is_some()
    {
        details.push(AgentDetail::Notification {
            kind: uniterm_core::agent_detail::bounded_text(
                raw.notification_kind.as_deref().unwrap_or("notification"),
                uniterm_core::agent_detail::DETAIL_TEXT_LIMIT,
            ),
            message: uniterm_core::agent_detail::bounded_text(
                raw.message.as_deref().unwrap_or_default(),
                uniterm_core::agent_detail::DETAIL_TEXT_LIMIT,
            ),
        });
    }
    // Only a permission envelope that names its tool carries a payload; a
    // bare permission signal must not erase a richer one already shown.
    if raw.event.as_deref() == Some("permission_request") {
        if let Some(tool) = raw.tool.as_deref().filter(|tool| !tool.trim().is_empty()) {
            details.push(AgentDetail::PermissionAsked {
                tool: uniterm_core::agent_detail::bounded_text(
                    tool,
                    uniterm_core::agent_detail::DETAIL_TEXT_LIMIT,
                ),
                preview: uniterm_core::agent_detail::bounded_text(
                    raw.preview.as_deref().unwrap_or_default(),
                    uniterm_core::agent_detail::DETAIL_TEXT_LIMIT,
                ),
            });
        }
    }
    if raw.event.as_deref() == Some("loop") {
        let state = match raw.loop_state.as_deref() {
            Some("scheduled") => Some(LoopState::Scheduled),
            Some("stopped") => Some(LoopState::Stopped),
            _ => None,
        };
        if let Some(state) = state {
            details.push(AgentDetail::Loop {
                state,
                delay_seconds: raw.delay_seconds.filter(|_| state == LoopState::Scheduled),
            });
        }
    }
    let cooperative_ready =
        raw.event.as_deref() == Some("idle") || raw.status.as_deref() == Some("idle");
    let status = raw
        .event
        .as_deref()
        .and_then(AgentStatus::from_event)
        .or_else(|| raw.status.as_deref().and_then(AgentStatus::from_event));
    Some(AgentEvent {
        agent: raw.agent,
        status,
        cooperative_ready,
        tool_completed: raw.event.as_deref() == Some("tool_end"),
        session_id: raw
            .session_id
            .filter(|value| !value.is_empty() && value.len() <= 512),
        parent_session_id: raw
            .parent_session_id
            .filter(|value| !value.is_empty() && value.len() <= 512),
        resume_command: raw
            .resume_command
            .into_iter()
            .filter(|value| value.len() <= 4_096 && !value.contains('\0'))
            .take(64)
            .collect(),
        transcript_path: raw
            .transcript_path
            .filter(|value| value.len() <= 4_096 && !value.chars().any(char::is_control)),
        details,
        task,
    })
}

impl Perform for Performer<'_> {
    fn print(&mut self, c: char) {
        let input = c;
        let c = if self.st.dec_line_drawing {
            dec_line_drawing(c)
        } else {
            c
        };

        let previous = if self.st.wrap_pending {
            Some((self.st.cx, self.st.cy))
        } else if self.st.cx > 0 {
            Some((self.st.cx - 1, self.st.cy))
        } else {
            None
        };
        if let Some((mut x, y)) = previous {
            if self.grid.get(x, y).is_continuation() && x > 0 {
                x -= 1;
            }
            let old = self.grid.get(x, y);
            if self.grid.grapheme_extends(old, c) {
                let mut text = self.grid.cell_text_owned(old);
                text.push(c);
                let width = text.as_str().width().clamp(1, 2) as u8;
                if width == 2 && self.st.width == 1 {
                    // A widening cluster cannot move to a two-column row here.
                    // Keep its text in one modeled cell, like narrow reflow;
                    // display extraction supplies the replacement glyph until
                    // the pane grows. Never advance the cursor beyond the row.
                    self.grid.set_grapheme(x, y, &text, old, 1);
                    self.st.cx = 0;
                    self.st.wrap_pending = self.st.autowrap;
                } else if width == 2 && x + 1 >= self.st.width {
                    let blank = self.st.blank_cell();
                    self.grid.clear_glyph_at(x, y, blank);
                    if self.st.autowrap {
                        self.grid.set_row_wrapped(y, true);
                        self.st.cx = 0;
                        self.linefeed();
                        self.st.wrap_pending = false;
                        self.grid.set_grapheme(0, self.st.cy, &text, old, 2);
                        if self.st.width == 2 {
                            self.st.cx = 1;
                            self.st.wrap_pending = true;
                        } else {
                            self.st.cx = 2;
                        }
                    } else {
                        self.grid.set_grapheme(x, y, "�", old, 1);
                    }
                } else if self.grid.append_to_grapheme(x, y, c) {
                    let end = x.saturating_add(width as u16);
                    if end >= self.st.width {
                        self.st.cx = self.st.width - 1;
                        self.st.wrap_pending = self.st.autowrap;
                    } else if !self.st.wrap_pending {
                        self.st.cx = end;
                    }
                }
                return;
            }
        }

        // Keep ordinary scalars as scalars through the grid write. Only the
        // dotted-circle fallback needs a multi-scalar UTF-8 representation.
        let mut scalar = c;
        let mut dotted = [0; 7];
        let mut width = c.width().unwrap_or(0).clamp(0, 2) as u8;
        let dotted_text = if width == 0 {
            let prefix = '◌'.encode_utf8(&mut dotted).len();
            let len = prefix + c.encode_utf8(&mut dotted[prefix..]).len();
            width = 1;
            Some(std::str::from_utf8(&dotted[..len]).expect("encoded dotted scalar"))
        } else {
            None
        };
        if width == 2 && self.st.width == 1 {
            scalar = '�';
            width = 1;
        }
        if self.st.wrap_pending {
            if self.st.autowrap {
                self.grid.set_row_wrapped(self.st.cy, true);
                self.st.cx = 0;
                self.linefeed();
            }
            self.st.wrap_pending = false;
        }
        if width == 2 && self.st.cx + 1 >= self.st.width {
            if self.st.autowrap {
                self.grid.set_row_wrapped(self.st.cy, true);
                self.st.cx = 0;
                self.linefeed();
            } else {
                scalar = '�';
                width = 1;
            }
        }
        if self.st.insert_mode {
            let blank = self.st.blank_cell();
            self.grid
                .insert_cells(self.st.cy, self.st.cx, width as u16, blank);
        }
        let cell = Cell {
            ch: c,
            fg: self.st.fg,
            bg: self.st.bg,
            attrs: self.st.attrs,
            underline_color: self.st.underline_color_handle,
            hyperlink: self.st.hyperlink,
            ..Cell::default()
        };
        if let Some(text) = dotted_text {
            self.grid
                .set_grapheme(self.st.cx, self.st.cy, text, cell, width);
        } else {
            self.grid
                .set_scalar(self.st.cx, self.st.cy, scalar, cell, width);
        }
        let end = self.st.cx + width as u16;
        if end >= self.st.width {
            self.st.cx = self.st.width - 1;
            self.st.wrap_pending = self.st.autowrap;
        } else {
            self.st.cx = end;
        }
        self.st.last_char = Some(input);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x0A..=0x0C => {
                // LF / VT / FF all line-feed here.
                self.grid.set_row_wrapped(self.st.cy, false);
                self.linefeed();
                self.st.wrap_pending = false;
            }
            0x0D => {
                self.st.cx = 0;
                self.st.wrap_pending = false;
            }
            0x08 => {
                self.st.cx = self.st.cx.saturating_sub(1);
                self.st.wrap_pending = false;
            }
            0x09 => {
                self.st.cx = ((self.st.cx + 1)..self.st.width)
                    .find(|x| self.st.tabs[*x as usize])
                    .unwrap_or(self.st.width - 1);
                self.st.wrap_pending = false;
            }
            _ => {} // BEL and others: ignored in M1
        }
    }

    fn csi_dispatch(&mut self, params: &Params, inter: &[u8], _ignore: bool, action: char) {
        match action {
            'H' | 'f' => {
                let row = param(params, 0, 1);
                let col = param(params, 1, 1);
                self.st.cy = if self.st.origin_mode {
                    (self.st.scroll_top + row - 1).min(self.st.scroll_bottom)
                } else {
                    (row - 1).min(self.st.height - 1)
                };
                self.st.cx = (col - 1).min(self.st.width - 1);
                self.st.wrap_pending = false;
            }
            'A' => {
                let top = if self.st.origin_mode {
                    self.st.scroll_top
                } else {
                    0
                };
                self.st.cy = self.st.cy.saturating_sub(param(params, 0, 1)).max(top);
                self.st.wrap_pending = false;
            }
            'B' | 'e' => {
                let bottom = if self.st.origin_mode {
                    self.st.scroll_bottom
                } else {
                    self.st.height - 1
                };
                self.st.cy = self.st.cy.saturating_add(param(params, 0, 1)).min(bottom);
                self.st.wrap_pending = false;
            }
            'C' | 'a' => {
                self.st.cx = self
                    .st
                    .cx
                    .saturating_add(param(params, 0, 1))
                    .min(self.st.width - 1);
                self.st.wrap_pending = false;
            }
            'D' => {
                self.st.cx = self.st.cx.saturating_sub(param(params, 0, 1));
                self.st.wrap_pending = false;
            }
            'E' => {
                let bottom = if self.st.origin_mode {
                    self.st.scroll_bottom
                } else {
                    self.st.height - 1
                };
                self.st.cy = self.st.cy.saturating_add(param(params, 0, 1)).min(bottom);
                self.st.cx = 0;
                self.st.wrap_pending = false;
            }
            'F' => {
                let top = if self.st.origin_mode {
                    self.st.scroll_top
                } else {
                    0
                };
                self.st.cy = self.st.cy.saturating_sub(param(params, 0, 1)).max(top);
                self.st.cx = 0;
                self.st.wrap_pending = false;
            }
            'G' | '`' => {
                self.st.cx = (param(params, 0, 1) - 1).min(self.st.width - 1);
                self.st.wrap_pending = false;
            }
            'd' => {
                let row = param(params, 0, 1) - 1;
                self.st.cy = if self.st.origin_mode {
                    (self.st.scroll_top + row).min(self.st.scroll_bottom)
                } else {
                    row.min(self.st.height - 1)
                };
                self.st.wrap_pending = false;
            }
            'J' => self.erase_in_display(param(params, 0, 0)),
            'K' => self.erase_in_line(param(params, 0, 0)),
            // SGR only when no private marker precedes the parameters. vte
            // collects `<`, `=`, `>`, and `?` into `inter`, so `ESC[>4m`
            // (XTMODKEYS reset, emitted by Bubble Tea, crossterm, and
            // ncurses on every exit or alt-screen switch) must never reach
            // `apply_sgr` as a bare `4` and underline the rest of the Pane.
            'm' if inter.is_empty() => self.apply_sgr(params),
            // XTMODKEYS (`>...m`), the key-modifier query (`?...m`), and
            // XTMODKEYS disable (`>...n`): Uniterm does not implement
            // modifyOtherKeys, so they are consumed without touching any
            // attribute or emitting a reply that could desynchronise the app.
            'm' if has_private_marker(inter) => {}
            'n' if inter.contains(&b'>') => {}
            // Line-editing ops shells use for readline: without these, deleted
            // characters linger and inserts overwrite.
            'P' => self.delete_chars(param(params, 0, 1)), // DCH
            '@' => self.insert_chars(param(params, 0, 1)), // ICH
            'X' => self.erase_chars(param(params, 0, 1)),  // ECH
            'L' => self.insert_lines(param(params, 0, 1)), // IL
            'M' => self.delete_lines(param(params, 0, 1)), // DL
            'b' => {
                if let Some(c) = self.st.last_char {
                    for _ in 0..param(params, 0, 1) {
                        self.print(c);
                    }
                }
            }
            'Z' => {
                for _ in 0..param(params, 0, 1) {
                    self.st.cx = (0..self.st.cx)
                        .rev()
                        .find(|x| self.st.tabs[*x as usize])
                        .unwrap_or(0);
                }
                self.st.wrap_pending = false;
            }
            'g' => match param(params, 0, 0) {
                0 => self.st.tabs[self.st.cx as usize] = false,
                3 => self.st.tabs.fill(false),
                _ => {}
            },
            'S' => {
                for _ in 0..param(params, 0, 1) {
                    self.scroll_region_up();
                }
            }
            'T' => {
                for _ in 0..param(params, 0, 1) {
                    self.scroll_region_down();
                }
            }
            // DECSTBM: set the vertical scroll region.
            'r' if inter.is_empty() => {
                let top = param(params, 0, 1) - 1;
                let bot = param(params, 1, self.st.height) - 1;
                if top < bot && bot < self.st.height {
                    self.st.scroll_top = top;
                    self.st.scroll_bottom = bot;
                    self.home();
                }
            }
            // ANSI.SYS save/restore cursor. `ESC[?...s` / `ESC[?...r` save and
            // restore DEC private modes in xterm and are ignored here, and
            // `ESC[?u` / `ESC[>u` / `ESC[=u` belong to the kitty keyboard
            // protocol, so all of these require an unmarked sequence.
            's' if inter.is_empty() => self.save_cursor(),
            'u' if inter.is_empty() => self.restore_cursor(),
            'u' if inter == b"?" => {
                let keyboard = &self.st.keyboard[usize::from(self.st.alt_backup.is_some())];
                self.st.responses.extend_from_slice(if keyboard.flags == 0 {
                    b"\x1b[?0u"
                } else {
                    b"\x1b[?1u"
                });
            }
            'u' if inter == b">" || inter == b"<" || inter == b"=" => {
                let keyboard = &mut self.st.keyboard[usize::from(self.st.alt_backup.is_some())];
                match inter[0] {
                    b'>' => keyboard.push(param(params, 0, 0)),
                    b'<' => keyboard.pop(param(params, 0, 1) as usize),
                    b'=' => {
                        let flags = param(params, 0, 0) & 1;
                        match param(params, 1, 1) {
                            1 => keyboard.flags = flags,
                            2 => keyboard.flags |= flags,
                            3 => keyboard.flags &= !flags,
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            // Device Attributes: shells/apps query this on startup and block for
            // the answer. Primary DA (ESC[c), Secondary DA (ESC[>c). Values match
            // tmux/xterm so callers recognize us as a capable terminal.
            'c' => {
                if inter.contains(&b'>') {
                    self.st.responses.extend_from_slice(b"\x1b[>84;0;0c");
                } else {
                    self.st.responses.extend_from_slice(b"\x1b[?1;2c");
                }
            }
            // Device Status Report: 5 = "are you ok" -> ESC[0n; 6 = cursor
            // position -> ESC[row;colR. (Private DSR "?n" is ignored.)
            'n' if inter.is_empty() => match param(params, 0, 0) {
                5 => self.st.responses.extend_from_slice(b"\x1b[0n"),
                6 => {
                    let row = if self.st.origin_mode {
                        self.st.cy - self.st.scroll_top + 1
                    } else {
                        self.st.cy + 1
                    };
                    let s = format!("\x1b[{row};{}R", self.st.cx + 1);
                    self.st.responses.extend_from_slice(s.as_bytes());
                }
                _ => {}
            },
            // Explicit dark/light preference query (DSR 996).
            'n' if inter.contains(&b'?') && param(params, 0, 0) == 996 => {
                push_theme_report(&mut self.st.responses, self.st.dark_theme);
            }
            // XTWINOPS cell and pixel-size queries. Uniterm owns cell geometry
            // but not the outer terminal's pixel metrics, so the pixel query
            // (14) answers 0;0 exactly like tmux: applications must already
            // tolerate unknown pixels, and staying silent would stall any app
            // that reads the reply with a blocking timeout.
            't' if inter.is_empty() => match param(params, 0, 0) {
                14 => self.st.responses.extend_from_slice(b"\x1b[4;0;0t"),
                18 | 19 => {
                    let response = format!("\x1b[8;{};{}t", self.st.height, self.st.width);
                    self.st.responses.extend_from_slice(response.as_bytes());
                }
                21 => {
                    let title = self.st.terminal_title.replace(['\x1b', '\x07'], " ");
                    self.st
                        .responses
                        .extend_from_slice(format!("\x1b]l{title}\x1b\\").as_bytes());
                }
                _ => {}
            },
            // DEC private mode set/reset (ESC[?...h / ESC[?...l). We act on the
            // alternate-screen modes and track cursor-key/mouse modes (so the
            // server can route wheel/mouse input correctly); the rest (autowrap,
            // cursor visibility, bracketed paste) are handled client-side or
            // are harmless.
            'h' if inter.contains(&b'?') => {
                for mode in params_flat(params) {
                    if mode == 2031 {
                        // Dark/light change notifications: report the current
                        // state on enable, as the proposal expects, then on
                        // every theme change (set_default_colors).
                        self.st.palette_notifications = true;
                        push_theme_report(&mut self.st.responses, self.st.dark_theme);
                    } else {
                        self.set_private_mode(mode, true);
                    }
                }
            }
            'l' if inter.contains(&b'?') => {
                for mode in params_flat(params) {
                    if mode == 2031 {
                        self.st.palette_notifications = false;
                    } else {
                        self.set_private_mode(mode, false);
                    }
                }
            }
            'h' if inter.is_empty() => {
                for mode in params_flat(params) {
                    if mode == 4 {
                        self.st.insert_mode = true;
                    }
                }
            }
            'l' if inter.is_empty() => {
                for mode in params_flat(params) {
                    if mode == 4 {
                        self.st.insert_mode = false;
                    }
                }
            }
            _ => {} // other CSI: IL/DL handled above; remainder ignored
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if matches!(params.first(), Some(value) if *value == b"0" || *value == b"2") {
            let mut title = Vec::new();
            for (index, value) in params[1..].iter().enumerate() {
                if index != 0 {
                    title.push(b';');
                }
                title.extend_from_slice(value);
            }
            self.st.title_revision = self.st.title_revision.saturating_add(1);
            self.st.terminal_title = String::from_utf8_lossy(&title)
                .chars()
                .filter(|character| !character.is_control() && *character != '\u{7f}')
                .take(512)
                .collect();
            return;
        }
        // OSC 8 hyperlinks: ESC ] 8 ; params ; URI ST. Store one grid-local
        // handle on printed cells so reordering and damage rendering retain the
        // semantic target instead of exposing only the visible label.
        if params.first() == Some(&b"8".as_slice()) && params.len() >= 3 {
            let mut uri = Vec::new();
            for (index, value) in params[2..].iter().enumerate() {
                if index != 0 {
                    uri.push(b';');
                }
                uri.extend_from_slice(value);
            }
            self.st.hyperlink = std::str::from_utf8(&uri)
                .ok()
                .map(|uri| self.grid.intern_hyperlink(uri))
                .unwrap_or(0);
            return;
        }
        // A Uniterm attach nested in this Pane reverses prefix priority while
        // it owns the terminal. This private marker is deliberately simple so
        // it survives an ordinary SSH byte stream. Unknown terminals ignore it.
        if params.first() == Some(&b"777".as_slice())
            && params.get(1) == Some(&b"uniterm-input".as_slice())
        {
            match params.get(2).copied() {
                Some(b"1") => self.set_nested_input(true),
                Some(b"0") => self.set_nested_input(false),
                _ => {}
            }
            return;
        }
        // OSC 777 agent metadata: ESC ] 777 ; notify ; <uri> ; <json> ST/BEL.
        // The JSON payload may itself contain ';', so rejoin params[3..].
        if params.first() == Some(&b"777".as_slice()) && params.len() >= 4 && params[1] == b"notify"
        {
            let mut json = Vec::new();
            for (i, p) in params[3..].iter().enumerate() {
                if i > 0 {
                    json.push(b';');
                }
                json.extend_from_slice(p);
            }
            if let Some(ev) = parse_osc777(&json) {
                if ev.status == Some(AgentStatus::Exited) {
                    // Agent wrappers can still report a clean lifecycle end
                    // after the child was killed before restoring its terminal
                    // modes. Recover the primary buffer and input modes here so
                    // the following shell never inherits a stranded agent UI.
                    self.recover_shell_screen();
                }
                self.st.agent_events.push(ev);
            }
            return;
        }
        if params.first() == Some(&b"7".as_slice()) {
            if let Some(value) = params
                .get(1)
                .and_then(|value| std::str::from_utf8(value).ok())
            {
                if value.len() <= 4096 {
                    self.st.cwd = Some(value.to_string());
                }
            }
            return;
        }
        if params.first() == Some(&b"133".as_slice()) {
            let mark = params[1..]
                .iter()
                .filter_map(|value| std::str::from_utf8(value).ok())
                .collect::<Vec<_>>()
                .join(";");
            if self.st.alt_backup.is_some() && matches!(mark.split(';').next(), Some("A" | "D")) {
                // A new prompt, or the command-finished boundary immediately
                // before it, proves the foreground application returned. This
                // also works through SSH, where the local PTY only sees the
                // long-lived ssh process and cannot observe the remote process
                // group transition.
                self.recover_shell_screen();
            }
            if mark.len() <= 4096 && self.st.prompt_marks.len() < 1024 {
                self.st.prompt_marks.push(mark);
            }
            return;
        }
        if params.first() == Some(&b"52".as_slice()) {
            let mut payload = Vec::new();
            for (idx, value) in params[1..].iter().enumerate() {
                if idx != 0 {
                    payload.push(b';');
                }
                payload.extend_from_slice(value);
            }
            if payload.len() <= 1024 * 1024 && self.st.clipboard_requests.len() < 16 {
                self.st.clipboard_requests.push(payload);
            }
            return;
        }
        // Answer indexed/default colour queries from the configured semantic
        // palette. The server may update these when its theme changes.
        if params.first() == Some(&b"4".as_slice()) && params.len() >= 3 {
            if let (Ok(index), true) = (
                std::str::from_utf8(params[1]).unwrap_or("").parse::<u8>(),
                params[2] == b"?",
            ) {
                let response = format!("\x1b]4;{index};{}\x07", osc_rgb(Color::Idx(index)));
                self.st.responses.extend_from_slice(response.as_bytes());
                return;
            }
        }
        if params.len() >= 2 && params[1] == b"?" {
            match params[0] {
                b"10" => self.st.responses.extend_from_slice(
                    format!("\x1b]10;{}\x07", osc_rgb(self.st.default_fg)).as_bytes(),
                ),
                b"11" => self.st.responses.extend_from_slice(
                    format!("\x1b]11;{}\x07", osc_rgb(self.st.default_bg)).as_bytes(),
                ),
                _ => {}
            }
        }
    }

    fn esc_dispatch(&mut self, inter: &[u8], _ignore: bool, byte: u8) {
        if inter == [b'('] {
            match byte {
                b'0' => self.st.dec_line_drawing = true,
                b'B' => self.st.dec_line_drawing = false,
                _ => {}
            }
            return;
        }
        match byte {
            b'M' => self.reverse_index(), // RI
            b'D' => {
                self.grid.set_row_wrapped(self.st.cy, false);
                self.linefeed();
                self.st.wrap_pending = false;
            }
            b'E' => {
                // NEL: CR + line feed.
                self.grid.set_row_wrapped(self.st.cy, false);
                self.st.cx = 0;
                self.linefeed();
                self.st.wrap_pending = false;
            }
            b'H' => self.st.tabs[self.st.cx as usize] = true,
            b'7' => self.save_cursor(),    // DECSC
            b'8' => self.restore_cursor(), // DECRC
            b'c' => self.reset_terminal(), // RIS
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_text_preserves_graphemes_without_owned_cell_strings() {
        let mut terminal = Terminal::new(40, 4);
        terminal.feed("e\u{301} 界 👩\u{200d}💻\r\nnext".as_bytes());
        assert_eq!(
            terminal.evidence_text(4),
            "e\u{301} 界 👩\u{200d}💻\nnext\n\n"
        );
    }

    #[test]
    fn visible_http_links_are_resolved_by_grid_cell() {
        let mut terminal = Terminal::new(80, 4);
        terminal.feed(b"visit https://localhost:5173/path?q=1 now");
        assert_eq!(
            terminal.url_at(12, 0).as_deref(),
            Some("https://localhost:5173/path?q=1")
        );
        assert_eq!(terminal.url_at(2, 0), None);
    }

    #[test]
    fn osc8_labels_retain_their_click_target() {
        let mut terminal = Terminal::new(40, 4);
        terminal.feed(b"\x1b]8;;https://example.com/docs\x1b\\open docs\x1b]8;;\x1b\\");
        assert_eq!(terminal.dump_text().lines().next(), Some("open docs"));
        assert_eq!(
            terminal.url_at(3, 0).as_deref(),
            Some("https://example.com/docs")
        );
        assert_eq!(terminal.grid().get(9, 0).hyperlink, 0);
    }

    fn row_text(t: &Terminal, y: u16) -> String {
        let mut text = String::new();
        for x in 0..t.grid().width() {
            let cell = t.grid().get(x, y);
            if !cell.is_continuation() {
                text.push_str(&t.grid().cell_text_owned(cell));
            }
        }
        text.trim_end().to_string()
    }

    #[test]
    fn prints_and_advances_cursor() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"hi");
        assert_eq!(t.grid().get(0, 0).ch, 'h');
        assert_eq!(t.grid().get(1, 0).ch, 'i');
        assert_eq!(t.cursor(), (2, 0));
    }

    #[test]
    fn carriage_return_and_linefeed() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"ab\r\ncd");
        assert_eq!(t.grid().get(0, 0).ch, 'a');
        assert_eq!(t.grid().get(0, 1).ch, 'c');
        assert_eq!(t.cursor(), (2, 1));
    }

    #[test]
    fn deferred_wrap_at_edge() {
        let mut t = Terminal::new(3, 3);
        t.feed(b"abcd"); // abc fills row 0, d wraps to row 1
        assert_eq!(t.grid().get(0, 0).ch, 'a');
        assert_eq!(t.grid().get(2, 0).ch, 'c');
        assert_eq!(t.grid().get(0, 1).ch, 'd');
    }

    #[test]
    fn scroll_on_overflow() {
        let mut t = Terminal::new(4, 2);
        t.feed(b"a\r\nb\r\nc"); // three lines into a 2-row screen -> 'a' scrolls off
        assert_eq!(t.grid().get(0, 0).ch, 'b');
        assert_eq!(t.grid().get(0, 1).ch, 'c');
    }

    #[test]
    fn cup_positions_cursor() {
        let mut t = Terminal::new(20, 10);
        t.feed(b"\x1b[3;5HX"); // row 3, col 5 (1-based)
        assert_eq!(t.grid().get(4, 2).ch, 'X');
    }

    #[test]
    fn sgr_sets_colour() {
        let mut t = Terminal::new(20, 3);
        t.feed(b"\x1b[31mR\x1b[0mN");
        assert_eq!(t.grid().get(0, 0).fg, Color::Idx(1)); // red
        assert_eq!(t.grid().get(1, 0).fg, Color::DEFAULT); // reset
    }

    #[test]
    fn sgr_256_colour() {
        let mut t = Terminal::new(20, 3);
        t.feed(b"\x1b[38;5;200mZ");
        assert_eq!(t.grid().get(0, 0).fg, Color::Idx(200));
    }

    #[test]
    fn sgr_truecolor_semicolon_and_colon_forms() {
        let mut t = Terminal::new(20, 3);
        t.feed(b"\x1b[38;2;10;20;30mA"); // semicolon truecolor fg
        assert_eq!(t.grid().get(0, 0).fg, Color::Rgb(10, 20, 30));
        t.feed(b"\x1b[48;2;40;50;60mB"); // semicolon truecolor bg
        assert_eq!(t.grid().get(1, 0).bg, Color::Rgb(40, 50, 60));
        t.feed(b"\x1b[38:2:70:80:90mC"); // colon form
        assert_eq!(t.grid().get(2, 0).fg, Color::Rgb(70, 80, 90));
        t.feed(b"\x1b[38:2::1:2:3mD"); // colon form with a colourspace id
        assert_eq!(t.grid().get(3, 0).fg, Color::Rgb(1, 2, 3));
        t.feed(b"\x1b[38:5:123mE"); // colon-indexed
        assert_eq!(t.grid().get(4, 0).fg, Color::Idx(123));
        // Params after the colour still apply (bold follows the rgb).
        t.feed(b"\x1b[0m\x1b[38;2;1;1;1;1mF");
        let f = t.grid().get(5, 0);
        assert_eq!(f.fg, Color::Rgb(1, 1, 1));
        assert_ne!(f.attrs.0 & uniterm_core::Attrs::BOLD.0, 0);
    }

    #[test]
    fn evidence_region_reaches_the_cursor_row_and_the_bottom() {
        let mut t = Terminal::new(20, 30);
        t.feed(b"$ top prompt");
        // The cursor is on row 0: the region starts there and still runs to
        // the bottom of the screen.
        let text = t.evidence_text(12);
        assert!(text.starts_with("$ top prompt"));
        assert!(text.lines().count() > 12);
        // With the cursor near the bottom the region is the bottom rows only.
        t.feed(b"\x1b[28;1Hfooter hint");
        let text = t.evidence_text(12);
        assert!(!text.contains("top prompt"));
        assert!(text.contains("footer hint"));
        assert!(text.lines().count() >= 12);
    }

    #[test]
    fn keyboard_negotiation_is_bounded_and_screen_local() {
        let mut t = Terminal::new(40, 3);
        t.grid_mut().clear_damage();
        t.feed(b"\x1b[?u");
        assert_eq!(t.take_responses(), b"\x1b[?0u");
        assert!(!t.keyboard_disambiguation());
        t.feed(b"\x1b[>31u\x1b[?u");
        assert_eq!(t.take_responses(), b"\x1b[?1u");
        assert!(t.keyboard_disambiguation());
        t.feed(b"\x1b[>0u\x1b[<u");
        assert!(t.keyboard_disambiguation());
        t.feed(b"\x1b[=1;3u");
        assert!(!t.keyboard_disambiguation());
        t.feed(b"\x1b[=1;2u");
        assert!(t.keyboard_disambiguation());
        t.feed(b"\x1b[?1049h");
        assert!(!t.keyboard_disambiguation());
        t.feed(b"\x1b[>1u\x1b[<u");
        assert!(!t.keyboard_disambiguation());
        t.feed(b"\x1b[?1049l");
        assert!(t.keyboard_disambiguation());
        for _ in 0..100 {
            t.feed(b"\x1b[>1u");
        }
        assert_eq!(t.st.keyboard[0].depth, 16);
        t.feed(b"\x1b[<100u");
        assert!(!t.keyboard_disambiguation());
        t.feed(b"\x1b[>1u\x1bc");
        assert!(!t.keyboard_disambiguation());
    }

    #[test]
    fn keyboard_queries_leave_cursor_attributes_and_grid_untouched() {
        let mut t = Terminal::new(40, 3);
        t.feed(b"\x1b[1m\x1b[2;5H\x1b[s");
        let cursor = t.cursor();
        let before: Vec<_> = (0..3)
            .flat_map(|y| (0..40).map(move |x| (x, y)))
            .map(|(x, y)| (x, y, t.grid().get(x, y)))
            .collect();
        t.grid_mut().clear_damage();
        t.feed(b"\x1b[?u\x1b[>1u\x1b[=0u\x1b[<u");
        assert!(!t.grid().is_dirty());
        assert_eq!(t.cursor(), cursor);
        assert_eq!(t.st.attrs, Attrs::BOLD);
        for (x, y, cell) in before {
            assert_eq!(t.grid().get(x, y), cell);
        }
        t.feed(b"\x1b[3;8H\x1b[u");
        assert_eq!(t.cursor(), cursor);
    }

    #[test]
    fn xtmodkeys_reset_does_not_underline() {
        // Bubble Tea v2 emits `ESC[>4m` on every stop. Plain SGR 4 would
        // underline everything the next program prints.
        let mut t = Terminal::new(40, 3);
        t.feed(b"\x1b[>4mthis should NOT be underlined\x1b[0m");
        let cell = t.grid().get(0, 0);
        assert_eq!(cell.ch, 't');
        assert_eq!(cell.attrs.underline_style(), UnderlineStyle::None);
        assert_eq!(cell.attrs, Attrs::default());
    }

    #[test]
    fn xtmodkeys_enable_does_not_underline_or_dim() {
        let mut t = Terminal::new(40, 3);
        t.feed(b"\x1b[>4;2mplain text\x1b[0m");
        let cell = t.grid().get(0, 0);
        assert_eq!(cell.ch, 'p');
        assert_eq!(cell.attrs, Attrs::default());
        assert_eq!(cell.attrs.0 & Attrs::DIM.0, 0);
    }

    #[test]
    fn private_query_marker_m_does_not_reach_sgr() {
        let mut t = Terminal::new(40, 3);
        t.feed(b"\x1b[?4mQ");
        let cell = t.grid().get(0, 0);
        assert_eq!(cell.ch, 'Q');
        assert_eq!(cell.attrs, Attrs::default());
        // A query must not produce a reply either; the parser has no
        // modifyOtherKeys state to report.
        assert!(t.take_responses().is_empty());
    }

    #[test]
    fn private_marker_sequences_leave_sgr_state_untouched() {
        // Regression: attributes set before, and left alone by, every
        // private-marker form survive, and a real SGR afterwards still works.
        let mut t = Terminal::new(40, 3);
        t.feed(b"\x1b[1mB");
        t.feed(b"\x1b[>4m\x1b[>4;2m\x1b[?4m\x1b[>4n\x1b[>0;1m");
        t.feed(b"S");
        let s = t.grid().get(1, 0);
        assert_eq!(s.ch, 'S');
        assert_ne!(s.attrs.0 & Attrs::BOLD.0, 0);
        assert_eq!(s.attrs.underline_style(), UnderlineStyle::None);
        assert_eq!(s.attrs.0 & Attrs::DIM.0, 0);
        t.feed(b"\x1b[0m\x1b[4mU\x1b[0m plain");
        assert_eq!(
            t.grid().get(2, 0).attrs.underline_style(),
            UnderlineStyle::Single
        );
        assert_eq!(
            t.grid().get(4, 0).attrs.underline_style(),
            UnderlineStyle::None
        );
        assert_eq!(t.grid().get(4, 0).attrs, Attrs::default());
    }

    #[test]
    fn sgr_preserves_extended_underline_styles_and_colours() {
        let mut t = Terminal::new(20, 3);
        t.feed(b"\x1b[4:3;58:2::10:20:30mA");
        let a = t.grid().get(0, 0);
        assert_eq!(a.attrs.underline_style(), UnderlineStyle::Curly);
        assert_eq!(
            t.grid().underline_color(a.underline_color),
            Color::Rgb(10, 20, 30)
        );

        t.feed(b"\x1b[4:0mB\x1b[4:2;58;5;123mC\x1b[59mD\x1b[24mE");
        assert_eq!(
            t.grid().get(1, 0).attrs.underline_style(),
            UnderlineStyle::None
        );
        let c = t.grid().get(2, 0);
        assert_eq!(c.attrs.underline_style(), UnderlineStyle::Double);
        assert_eq!(t.grid().underline_color(c.underline_color), Color::Idx(123));
        let d = t.grid().get(3, 0);
        assert_eq!(d.attrs.underline_style(), UnderlineStyle::Double);
        assert_eq!(t.grid().underline_color(d.underline_color), Color::Default);
        assert_eq!(
            t.grid().get(4, 0).attrs.underline_style(),
            UnderlineStyle::None
        );
    }

    #[test]
    fn resize_rebinds_the_current_underline_colour_handle() {
        let mut t = Terminal::new(4, 2);
        t.feed(b"\x1b[4:3;58:2::10:20:30mA");
        t.resize(8, 2);
        t.feed(b"B");
        for x in 0..=1 {
            let cell = t.grid().get(x, 0);
            assert_eq!(cell.attrs.underline_style(), UnderlineStyle::Curly);
            assert_eq!(
                t.grid().underline_color(cell.underline_color),
                Color::Rgb(10, 20, 30)
            );
        }
    }

    #[test]
    fn erase_line_clears_to_end() {
        let mut t = Terminal::new(10, 3);
        t.feed(b"hello\r\x1b[K"); // write, CR to col 0, clear to EOL
        assert_eq!(t.grid().get(0, 0).ch, ' ');
        assert_eq!(t.grid().get(4, 0).ch, ' ');
    }

    #[test]
    fn answers_terminal_queries() {
        let mut t = Terminal::new(80, 24);
        // Move cursor so the DSR reply is non-trivial, then query.
        t.feed(b"\x1b[5;10H"); // row 5, col 10
        t.feed(b"\x1b[c"); // primary DA
        t.feed(b"\x1b[6n"); // cursor position report
        t.feed(b"\x1b[>c"); // secondary DA
        let r = String::from_utf8(t.take_responses()).unwrap();
        assert!(r.contains("\x1b[?1;2c"), "primary DA reply missing: {r:?}");
        assert!(r.contains("\x1b[5;10R"), "cursor report missing: {r:?}");
        assert!(
            r.contains("\x1b[>84;0;0c"),
            "secondary DA reply missing: {r:?}"
        );
        // Draining twice yields nothing the second time.
        assert!(t.take_responses().is_empty());
    }

    #[test]
    fn delete_char_shifts_line_left() {
        // The "deleted characters linger" bug: DCH must remove and shift.
        let mut t = Terminal::new(20, 3);
        t.feed(b"abcdef");
        t.feed(b"\x1b[1;3H"); // cursor to col 3 (the 'c')
        t.feed(b"\x1b[P"); // delete one char
        assert_eq!(t.grid().get(0, 0).ch, 'a');
        assert_eq!(t.grid().get(1, 0).ch, 'b');
        assert_eq!(t.grid().get(2, 0).ch, 'd'); // 'c' gone, rest shifted left
        assert_eq!(t.grid().get(3, 0).ch, 'e');
    }

    #[test]
    fn insert_and_erase_chars() {
        let mut t = Terminal::new(20, 3);
        t.feed(b"abc");
        t.feed(b"\x1b[1;1H\x1b[2@"); // insert 2 blanks at start
        assert_eq!(t.grid().get(0, 0).ch, ' ');
        assert_eq!(t.grid().get(2, 0).ch, 'a');
        t.feed(b"\x1b[1;3H\x1b[X"); // erase 1 char at col 3
        assert_eq!(t.grid().get(2, 0).ch, ' ');
    }

    #[test]
    fn shell_edit_operations_cancel_pending_wrap() {
        for operation in [b"\x1b[P".as_slice(), b"\x1b[X", b"\x1b[K", b"\x1b[@"] {
            let mut t = Terminal::new(5, 2);
            t.feed(b"ABCDE");
            assert!(t.st.wrap_pending, "fixture must end at a pending wrap");

            t.feed(operation);
            assert!(
                !t.st.wrap_pending,
                "operation {operation:?} left a pending wrap armed"
            );
            t.feed(b"X");

            assert_eq!(t.grid().get(4, 0).ch, 'X');
            assert_eq!(t.grid().get(0, 1).ch, ' ');
        }
    }

    #[test]
    fn character_edits_break_a_soft_wrapped_row() {
        for operation in [b"\x1b[P".as_slice(), b"\x1b[X", b"\x1b[K"] {
            let mut t = Terminal::new(5, 2);
            t.feed(b"ABCDEF");
            assert!(t.grid().line_wrapped(0));

            t.feed(b"\x1b[1;2H");
            t.feed(operation);

            assert!(
                !t.grid().line_wrapped(0),
                "operation {operation:?} preserved a stale soft wrap"
            );
        }
    }

    #[test]
    fn scroll_region_confines_scrolling() {
        let mut t = Terminal::new(6, 5);
        // Region rows 2..4 (1-based DECSTBM 2;4). Fill rows, force a scroll in
        // the region, and confirm row 0 (outside the region) is untouched.
        t.feed(b"\x1b[2;4r"); // set region rows 2-4, cursor homes
        t.feed(b"TOP\r"); // row 0 (home is 0,0 after DECSTBM)
        t.feed(b"\x1b[2;1HL2\r\nL3\r\nL4\r\nL5"); // fill region and overflow
        assert_eq!(t.grid().get(0, 0).ch, 'T'); // outside region: unchanged
    }

    #[test]
    fn top_anchored_scroll_region_retains_inline_app_history() {
        let mut t = Terminal::new(6, 4);
        t.feed(b"\x1b[1;1HA\x1b[2;1HB\x1b[3;1HC\x1b[4;1HD");

        // Inline TUIs such as Codex reserve a top-anchored region, print its
        // final row, and line-feed so the completed row enters host history.
        t.feed(b"\x1b[1;3r\x1b[3;1H\n");

        assert_eq!(t.grid().scrollback_len(), 1);
        assert_eq!(t.grid().line_cell(0, 0).ch, 'A');
        assert_eq!(t.grid().get(0, 0).ch, 'B');
        assert_eq!(t.grid().get(0, 1).ch, 'C');
        assert_eq!(t.grid().get(0, 2).ch, ' ');
        assert_eq!(t.grid().get(0, 3).ch, 'D');
    }

    #[test]
    fn delete_line_at_screen_top_does_not_create_history() {
        let mut t = Terminal::new(6, 3);
        t.feed(b"\x1b[1;1HA\x1b[2;1HB\x1b[3;1HC");
        t.feed(b"\x1b[1;1H\x1b[M");

        assert_eq!(t.grid().scrollback_len(), 0);
        assert_eq!(t.grid().get(0, 0).ch, 'B');
        assert_eq!(t.grid().get(0, 1).ch, 'C');
        assert_eq!(t.grid().get(0, 2).ch, ' ');
    }

    #[test]
    fn reverse_index_scrolls_region_down_at_top() {
        let mut t = Terminal::new(6, 4);
        t.feed(b"\x1b[1;1HX"); // 'X' at top-left
        t.feed(b"\x1b[1;1H\x1bM"); // cursor home, reverse index -> scroll down
                                   // 'X' moved down a row; top row is now blank.
        assert_eq!(t.grid().get(0, 0).ch, ' ');
        assert_eq!(t.grid().get(0, 1).ch, 'X');
    }

    #[test]
    fn alt_screen_preserves_primary() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"PRIMARY");
        assert_eq!(t.grid().get(0, 0).ch, 'P');
        // Enter alt screen, write different content.
        t.feed(b"\x1b[?1049h");
        assert_eq!(t.grid().get(0, 0).ch, ' '); // alt starts blank
        t.feed(b"ALTSCREEN");
        assert_eq!(t.grid().get(0, 0).ch, 'A');
        // Exit: the primary content must be back, alt content gone.
        t.feed(b"\x1b[?1049l");
        assert_eq!(t.grid().get(0, 0).ch, 'P');
        assert_eq!(t.grid().get(1, 0).ch, 'R');
        assert!(t.grid().is_dirty()); // exit forces a full repaint
    }

    #[test]
    fn resize_in_alt_screen_resizes_stashed_primary() {
        // The "stale content after exiting htop" bug: a resize while the app
        // holds the alt screen must resize the stashed primary too, or exit
        // restores an old-geometry grid that repaints only part of the pane.
        let mut t = Terminal::new(10, 4);
        t.feed(b"PRIMARY");
        t.feed(b"\x1b[?1049h");
        t.feed(b"ALT");
        t.resize(16, 6); // grow while in the alt screen
        assert_eq!(t.grid().width(), 16);
        assert_eq!(t.grid().height(), 6);
        t.feed(b"\x1b[?1049l");
        // The restored primary matches the new geometry and kept its content.
        assert_eq!(t.grid().width(), 16);
        assert_eq!(t.grid().height(), 6);
        assert_eq!(t.grid().get(0, 0).ch, 'P');
        assert!(t.grid().is_dirty());
        // Every visible row is damaged so the whole pane rect repaints.
        for y in 0..6 {
            assert_eq!(t.grid().row_damage(y), Some((0, 15)), "row {y}");
        }
        // The emulator geometry agrees with the grid: the far column is usable.
        t.feed(b"\x1b[6;16HX");
        assert_eq!(t.grid().get(15, 5).ch, 'X');
    }

    #[test]
    fn shrink_in_alt_screen_clamps_restored_cursor() {
        let mut t = Terminal::new(20, 10);
        t.feed(b"\x1b[10;20H"); // park the primary cursor at the far corner
        t.feed(b"\x1b[?1049h");
        t.resize(8, 4); // shrink while in the alt screen
        t.feed(b"\x1b[?1049l");
        assert_eq!(t.grid().width(), 8);
        assert_eq!(t.grid().height(), 4);
        let (cx, cy) = t.cursor();
        assert!(cx < 8 && cy < 4, "cursor ({cx},{cy}) out of bounds");
    }

    #[test]
    fn resize_in_alt_screen_keeps_alt_scrollback_disabled() {
        let mut t = Terminal::new(10, 3);
        t.feed(b"one\r\ntwo");
        let primary_lines = t.grid().scrollback_len();
        t.feed(b"\x1b[?1049h");
        t.resize(12, 4);
        // Overflow the alt screen; none of it may land in scrollback.
        t.feed(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");
        assert_eq!(t.grid().scrollback_len(), 0);
        t.feed(b"\x1b[?1049l");
        assert_eq!(t.grid().scrollback_len(), primary_lines);
    }

    #[test]
    fn tracks_cursor_key_and_mouse_modes() {
        let mut t = Terminal::new(20, 5);
        assert!(!t.app_cursor());
        assert_eq!(t.mouse_mode(), MouseMode::Off);
        t.feed(b"\x1b[?1h\x1b[?1002h\x1b[?1006h");
        assert!(t.app_cursor());
        assert_eq!(t.mouse_mode(), MouseMode::Button);
        assert!(t.mouse_sgr());
        t.feed(b"\x1b[?1002l\x1b[?1l\x1b[?1006l");
        assert!(!t.app_cursor());
        assert_eq!(t.mouse_mode(), MouseMode::Off);
        assert!(!t.mouse_sgr());
        // The alt-screen accessor reflects 1049 pairs.
        assert!(!t.is_alt_screen());
        t.feed(b"\x1b[?1049h");
        assert!(t.is_alt_screen());
        t.feed(b"\x1b[?1049l");
        assert!(!t.is_alt_screen());
    }

    #[test]
    fn save_and_restore_cursor() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"\x1b[3;7H\x1b7"); // move + DECSC
        t.feed(b"\x1b[1;1H"); // move away
        t.feed(b"\x1b8"); // DECRC -> back to (7,3)
        assert_eq!(t.cursor(), (6, 2));
    }

    #[test]
    fn osc777_tasks_require_explicit_submission_or_turn_boundary() {
        use uniterm_core::tasks::TaskObservation;
        for provider in ["codex", "claude", "grok", "opencode", "pi", "custom"] {
            let event = parse_osc777(serde_json::json!({
                "agent": provider, "event":"prompt_submit", "prompt":"Fix auth\nKeep tests", "turn_id":"one"
            }).to_string().as_bytes()).unwrap();
            assert_eq!(
                event.task,
                Some(TaskObservation::Prompt {
                    id: Some("one".into()),
                    text: "Fix auth\nKeep tests".into()
                })
            );
            for kind in [
                "idle",
                "tool_end",
                "session_start",
                "permission_request",
                "notification",
            ] {
                let event = parse_osc777(
                    serde_json::json!({"agent":provider,"event":kind,"prompt":"not a submission"})
                        .to_string()
                        .as_bytes(),
                )
                .unwrap();
                assert!(event.task.is_none());
            }
            let done =
                parse_osc777(br#"{"event":"idle","turn_complete":true,"turn_id":"one"}"#).unwrap();
            assert_eq!(
                done.task,
                Some(TaskObservation::Finished {
                    id: Some("one".into())
                })
            );
            let interrupted =
                parse_osc777(br#"{"event":"idle","turn_interrupted":true,"turn_complete":true}"#)
                    .unwrap();
            assert_eq!(
                interrupted.task,
                Some(TaskObservation::Interrupted { id: None })
            );
        }
        for text in [String::new(), "x".repeat(16_385), "a\0b".into()] {
            assert!(parse_osc777(
                serde_json::json!({"event":"prompt_submit","prompt":text})
                    .to_string()
                    .as_bytes()
            )
            .unwrap()
            .task
            .is_none());
        }
    }

    #[test]
    fn osc777_details_never_imply_a_status() {
        let mut t = Terminal::new(80, 24);
        t.feed(b"\x1b]777;notify;uniterm://cli-agent;{\"agent\":\"claude\",\"event\":\"notification\",\"notification_kind\":\"idle_prompt\",\"message\":\"waiting\"}\x07");
        t.feed(b"\x1b]777;notify;uniterm://cli-agent;{\"agent\":\"claude\",\"event\":\"loop\",\"loop_state\":\"stopped\",\"delay_seconds\":60}\x07");
        t.feed(b"\x1b]777;notify;uniterm://cli-agent;{\"agent\":\"claude\",\"event\":\"loop\",\"loop_state\":\"later\"}\x07");
        let evs = t.take_agent_events();
        assert_eq!(evs.len(), 3);
        assert!(evs
            .iter()
            .all(|ev| ev.status.is_none() && !ev.cooperative_ready));
        assert_eq!(
            evs[0].details,
            vec![AgentDetail::Notification {
                kind: "idle_prompt".into(),
                message: "waiting".into()
            }]
        );
        // A stopped loop carries no delay; an unknown loop state is nothing.
        assert_eq!(
            evs[1].details,
            vec![AgentDetail::Loop {
                state: LoopState::Stopped,
                delay_seconds: None
            }]
        );
        assert!(evs[2].details.is_empty());
    }

    #[test]
    fn osc777_permission_payload_needs_a_tool() {
        let mut t = Terminal::new(80, 24);
        t.feed(b"\x1b]777;notify;uniterm://cli-agent;{\"agent\":\"claude\",\"event\":\"permission_request\",\"tool\":\"Bash\",\"preview\":\"ls\",\"transcript_path\":\"/tmp/demo.jsonl\"}\x07");
        t.feed(b"\x1b]777;notify;uniterm://cli-agent;{\"agent\":\"claude\",\"event\":\"permission_request\",\"tool\":\" \"}\x07");
        let evs = t.take_agent_events();
        assert_eq!(evs[0].status, Some(AgentStatus::Permission));
        assert_eq!(evs[0].transcript_path.as_deref(), Some("/tmp/demo.jsonl"));
        assert_eq!(
            evs[0].details,
            vec![AgentDetail::PermissionAsked {
                tool: "Bash".into(),
                preview: "ls".into()
            }]
        );
        assert_eq!(evs[1].status, Some(AgentStatus::Permission));
        assert!(evs[1].details.is_empty());
    }

    #[test]
    fn parses_osc777_agent_event() {
        let mut t = Terminal::new(80, 24);
        // ESC ] 777 ; notify ; uniterm://cli-agent ; {json} BEL
        t.feed(b"\x1b]777;notify;uniterm://cli-agent;{\"agent\":\"claude\",\"event\":\"permission_request\"}\x07");
        let evs = t.take_agent_events();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].agent.as_deref(), Some("claude"));
        assert_eq!(evs[0].status, Some(uniterm_core::AgentStatus::Permission));
        assert!(!evs[0].cooperative_ready);
        t.feed(
            b"\x1b]777;notify;uniterm://cli-agent;{\"agent\":\"claude\",\"event\":\"idle\"}\x07",
        );
        let ready = t.take_agent_events();
        assert_eq!(ready[0].status, Some(uniterm_core::AgentStatus::Idle));
        assert!(ready[0].cooperative_ready);
        // Draining again is empty; the OSC text was not printed to the grid.
        assert!(t.take_agent_events().is_empty());
        assert_eq!(t.grid().get(0, 0).ch, ' ');
    }

    #[test]
    fn socket_agent_events_leave_terminal_parser_and_grid_untouched() {
        let mut terminal = Terminal::new(80, 24);
        terminal.feed(b"existing\x1b[");
        terminal.grid.clear_damage();
        let event =
            parse_osc777(br#"{"agent":"codex","event":"session_start","session_id":"native-id"}"#)
                .unwrap();
        terminal.queue_agent_event(event.clone());
        assert_eq!(terminal.take_agent_events(), [event]);
        assert!(terminal.take_agent_events().is_empty());
        assert!(!terminal.grid.is_dirty());
        // The socket report cannot finish or corrupt an in-flight VT escape.
        terminal.feed(b"31mX");
        assert_eq!(terminal.grid.get(8, 0).ch, 'X');
        assert_eq!(terminal.grid.get(8, 0).fg, Color::Idx(1));
    }

    #[test]
    fn nested_uniterm_input_markers_are_stateful_and_recoverable() {
        let mut terminal = Terminal::new(80, 24);
        terminal.feed(b"\x1b]777;uniterm-input;1\x07");
        assert!(terminal.nested_input());
        assert_eq!(terminal.take_nested_input_changes(), vec![true]);

        terminal.feed(b"\x1b]777;uniterm-input;1\x07");
        assert!(terminal.take_nested_input_changes().is_empty());

        terminal.recover_shell_screen();
        assert!(!terminal.nested_input());
        assert_eq!(terminal.take_nested_input_changes(), vec![false]);
    }

    #[test]
    fn answers_color_query() {
        let mut t = Terminal::new(80, 24);
        t.feed(b"\x1b]11;?\x07"); // background colour query
        let r = String::from_utf8(t.take_responses()).unwrap();
        assert!(
            r.starts_with("\x1b]11;rgb:"),
            "bg colour reply missing: {r:?}"
        );
    }

    #[test]
    fn answers_configured_palette_theme_and_window_queries() {
        let mut terminal = Terminal::new(90, 30);
        terminal.set_default_colors(Color::Rgb(0x11, 0x22, 0x33), Color::Rgb(0xee, 0xdd, 0xcc));
        terminal.feed(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]4;196;?\x07");
        terminal.feed(b"\x1b[?996n\x1b[18t");
        let response = String::from_utf8(terminal.take_responses()).unwrap();
        assert!(response.contains("\x1b]10;rgb:1111/2222/3333\x07"));
        assert!(response.contains("\x1b]11;rgb:eeee/dddd/cccc\x07"));
        assert!(response.contains("\x1b]4;196;rgb:ffff/0000/0000\x07"));
        assert!(response.contains("\x1b[?997;2n"));
        assert!(response.contains("\x1b[8;30;90t"));
    }

    #[test]
    fn terminal_titles_are_bounded_and_cannot_nest_controls() {
        let mut terminal = Terminal::new(80, 24);
        terminal.feed(b"\x1b]0;safe\x1b]9;forged\x07");
        assert!(!terminal.terminal_title().contains('\x1b'));
        assert!(terminal.terminal_title().len() <= 512);
    }

    #[test]
    fn osc777_retains_provider_owned_native_resume_identity() {
        let mut terminal = Terminal::new(80, 24);
        terminal.feed(b"\x1b]777;notify;uniterm://cli-agent;{\"agent\":\"codex\",\"event\":\"session_start\",\"session_id\":\"abc\",\"resume_command\":[\"codex\",\"resume\",\"abc\"]}\x07");
        let event = terminal.take_agent_events().pop().unwrap();
        assert_eq!(event.session_id.as_deref(), Some("abc"));
        assert_eq!(event.resume_command, ["codex", "resume", "abc"]);
    }

    #[test]
    fn erase_display_clears_all() {
        let mut t = Terminal::new(6, 3);
        t.feed(b"x\r\ny\x1b[2J");
        for y in 0..3 {
            for x in 0..6 {
                assert_eq!(t.grid().get(x, y).ch, ' ');
            }
        }
    }

    #[test]
    fn unicode_graphemes_and_wide_cells_keep_exact_columns() {
        let mut t = Terminal::new(12, 3);
        t.feed("e\u{301}界👩\u{200d}💻x".as_bytes());
        assert_eq!(t.grid().cell_text_owned(t.grid().get(0, 0)), "e\u{301}");
        assert_eq!(t.grid().get(0, 0).width, 1);
        assert_eq!(t.grid().cell_text_owned(t.grid().get(1, 0)), "界");
        assert!(t.grid().get(2, 0).is_continuation());
        assert_eq!(t.grid().cell_text_owned(t.grid().get(3, 0)), "👩\u{200d}💻");
        assert!(t.grid().get(4, 0).is_continuation());
        assert_eq!(t.grid().get(5, 0).ch, 'x');
        assert_eq!(t.cursor(), (6, 0));
    }

    #[test]
    fn one_column_keycap_keeps_cursor_and_retains_grapheme() {
        // Exercise the immediate scalar sink independently of ASCII buffering.
        let feed = |terminal: &mut Terminal, bytes: &[u8]| {
            let Terminal {
                grid, parser, st, ..
            } = terminal;
            let mut performer = Performer { grid, st };
            for &byte in bytes {
                parser.advance(&mut performer, byte);
            }
        };
        let keycap = "1\u{fe0f}\u{20e3}";
        for split in 0..=keycap.len() {
            let mut terminal = Terminal::new(1, 2);
            feed(&mut terminal, &keycap.as_bytes()[..split]);
            assert!(terminal.st.cx < terminal.st.width);
            feed(&mut terminal, &keycap.as_bytes()[split..]);
            assert_eq!(terminal.cursor(), (0, 0));
            assert!(terminal.st.wrap_pending);
            let cell = terminal.grid.get(0, 0);
            assert_eq!(cell.width, 1);
            assert_eq!(terminal.grid.cell_text_owned(cell), keycap);
            let mut display = Vec::new();
            terminal.grid.write_cell_display_text(cell, &mut display);
            assert_eq!(display, "�".as_bytes());
            feed(&mut terminal, b"X");
            assert_eq!(terminal.cursor(), (0, 1));
            assert_eq!(terminal.grid.get(0, 1).ch, 'X');
            assert_eq!(
                terminal.grid.cell_text_owned(terminal.grid.get(0, 0)),
                keycap
            );
            terminal.resize(4, 2);
            assert!(terminal.grid.export_lines(8).iter().any(|line| {
                line.cells
                    .iter()
                    .any(|cell| cell.text == keycap && cell.width == 2)
            }));
        }
    }

    #[test]
    fn scalar_printing_preserves_fallbacks_wrap_and_repeat() {
        let mut t = Terminal::new(4, 3);
        t.feed("\u{301}".as_bytes());
        assert_eq!(t.grid().cell_text_owned(t.grid().get(0, 0)), "◌\u{301}");
        t.feed(b"abc");
        assert_eq!(t.cursor(), (3, 0));
        t.feed("\u{301}Z\x1b[2b".as_bytes());
        assert_eq!(t.grid().cell_text_owned(t.grid().get(3, 0)), "c\u{301}");
        assert!(t.grid().line_wrapped(0));
        assert_eq!(row_text(&t, 1), "ZZZ");

        // Unicode Prepend can join the following ASCII scalar; it must reach
        // segmentation even when the new scalar would otherwise be cheap.
        let mut prepend = Terminal::new(8, 1);
        prepend.feed("\u{600}A".as_bytes());
        assert!(prepend
            .grid()
            .cell_text_owned(prepend.grid().get(0, 0))
            .ends_with("\u{600}A"));

        let mut narrow = Terminal::new(1, 2);
        narrow.feed("界".as_bytes());
        assert_eq!(row_text(&narrow, 0), "�");
        assert_eq!(narrow.grid().get(0, 0).width, 1);

        let mut no_wrap = Terminal::new(3, 1);
        no_wrap.feed("\x1b[?7lab界".as_bytes());
        assert_eq!(row_text(&no_wrap, 0), "ab�");
        assert_eq!(no_wrap.cursor(), (2, 0));
    }

    #[test]
    fn unchanged_narrow_text_does_not_damage_the_grid() {
        let mut t = Terminal::new(20, 2);
        let text = "\x1b[1;31mabc é\x1b[0m";
        t.feed(text.as_bytes());
        assert_eq!(t.grid().row_damage(0), Some((0, 4)));
        t.grid_mut().clear_damage();
        t.feed(b"\r");
        t.feed(text.as_bytes());
        assert!(!t.grid().is_dirty());
        t.feed(b"");
        assert!(!t.grid().is_dirty());
    }

    #[test]
    fn erasing_either_half_removes_a_wide_glyph() {
        let mut t = Terminal::new(8, 2);
        t.feed("A界B".as_bytes());
        t.feed(b"\x1b[1;3H\x1b[X");
        assert_eq!(t.grid().get(1, 0).ch, ' ');
        assert_eq!(t.grid().get(2, 0).ch, ' ');
        assert_eq!(t.grid().get(3, 0).ch, 'B');
    }

    #[test]
    fn soft_wrapped_lines_reflow_and_cursor_follows_text() {
        let mut t = Terminal::new(5, 3);
        t.feed(b"abcdefghij");
        assert!(t.grid().line_wrapped(0));
        t.resize(8, 3);
        assert_eq!(row_text(&t, 0), "abcdefgh");
        assert_eq!(row_text(&t, 1), "ij");
        assert_eq!(t.cursor(), (2, 1));
        t.feed(b"K");
        assert_eq!(row_text(&t, 1), "ijK");
        t.resize(4, 4);
        assert_eq!(row_text(&t, 0), "abcd");
        assert_eq!(row_text(&t, 1), "efgh");
        assert_eq!(row_text(&t, 2), "ijK");
    }

    #[test]
    fn private_mode_batches_origin_insert_and_cursor_visibility_work() {
        let mut t = Terminal::new(8, 5);
        t.feed(b"\x1b[2;4r\x1b[?6;25;2004h\x1b[1;1HX");
        assert_eq!(t.grid().get(0, 1).ch, 'X');
        assert!(t.cursor_visible());
        assert!(t.bracketed_paste());
        t.feed(b"\x1b[?25;2004l");
        assert!(!t.cursor_visible());
        assert!(!t.bracketed_paste());
        t.feed(b"\x1b[?6l\x1b[1;1HAB\x1b[1;1H\x1b[4hZ");
        assert_eq!(row_text(&t, 0), "ZAB");
    }

    #[test]
    fn dec_special_graphics_and_tab_controls_are_applied() {
        let mut t = Terminal::new(16, 2);
        t.feed(b"\x1b(0lqk\x1b(B");
        assert_eq!(row_text(&t, 0), "┌─┐");
        t.feed(b"\r\x1b[3gA\tB");
        assert_eq!(t.grid().get(15, 0).ch, 'B');
    }

    #[test]
    fn osc_metadata_is_routed_and_never_drawn() {
        let mut t = Terminal::new(20, 2);
        t.feed(b"\x1b]7;file://host/tmp/project\x07");
        t.feed(b"\x1b]133;A\x07");
        t.feed(b"\x1b]52;c;SGVsbG8=\x07");
        assert_eq!(t.reported_cwd(), Some("file://host/tmp/project"));
        assert_eq!(t.take_prompt_marks(), vec!["A"]);
        assert_eq!(t.take_clipboard_requests(), vec![b"c;SGVsbG8=".to_vec()]);
        assert_eq!(row_text(&t, 0), "");
    }

    #[test]
    fn agent_exit_recovers_a_stranded_alternate_screen() {
        let mut t = Terminal::new(20, 3);
        t.feed(b"shell");
        t.feed(b"\x1b[?1049hagent ui");
        t.feed(b"\x1b]777;notify;uniterm://cli-agent;{\"agent\":\"claude\",\"event\":\"session_end\"}\x07");
        assert!(!t.is_alt_screen());
        assert_eq!(row_text(&t, 0), "shell");
        assert!(t.cursor_visible());
        assert!(!t.bracketed_paste());
    }

    #[test]
    fn shell_prompt_recovers_a_stranded_remote_alternate_screen() {
        let mut t = Terminal::new(20, 3);
        t.feed(b"remote shell");
        t.feed(b"\x1b[?1049h\x1b[?25l\x1b[?2004hagent ui");

        // OSC 133 survives an SSH byte stream even though the local PTY cannot
        // observe the remote agent's foreground process exiting.
        t.feed(b"\x1b]133;D;0\x07\x1b]133;A\x07");

        assert!(!t.is_alt_screen());
        assert_eq!(row_text(&t, 0), "remote shell");
        assert!(t.cursor_visible());
        assert!(!t.bracketed_paste());
        assert_eq!(t.take_prompt_marks(), vec!["D;0", "A"]);
    }

    // Independent immediate sink, using the same pinned vte parser. This keeps
    // the full scalar semantics as an oracle without a production feature flag.
    fn feed_scalar(terminal: &mut Terminal, bytes: &[u8]) {
        let Terminal {
            grid, parser, st, ..
        } = terminal;
        let mut performer = Performer { grid, st };
        for &byte in bytes {
            parser.advance(&mut performer, byte);
        }
    }

    fn assert_same_grid(actual: &Grid, expected: &Grid) {
        assert_eq!(actual.width(), expected.width());
        assert_eq!(actual.height(), expected.height());
        assert_eq!(
            actual.export_lines(usize::MAX),
            expected.export_lines(usize::MAX)
        );
        assert_eq!(actual.retained_cell_count(), expected.retained_cell_count());
        assert_eq!(actual.is_dirty(), expected.is_dirty());
        assert_eq!(actual.pending_scroll_up(), expected.pending_scroll_up());
        assert_eq!(actual.total_lines(), expected.total_lines());
        assert_eq!(actual.first_line_id(), expected.first_line_id());
        for line in 0..actual.total_lines() {
            assert_eq!(actual.line_id(line), expected.line_id(line));
            assert_eq!(actual.line_wrapped(line), expected.line_wrapped(line));
            for x in 0..actual.width() {
                let a = actual.line_cell(line, x);
                let b = expected.line_cell(line, x);
                assert_eq!(a, b);
                assert_eq!(
                    actual.hyperlink(a.hyperlink),
                    expected.hyperlink(b.hyperlink)
                );
                assert_eq!(
                    actual.underline_color(a.underline_color),
                    expected.underline_color(b.underline_color)
                );
            }
        }
        for y in 0..actual.height() {
            assert_eq!(actual.row_damage(y), expected.row_damage(y));
        }
    }

    fn assert_same_terminal(actual: &Terminal, expected: &Terminal) {
        assert_same_grid(&actual.grid, &expected.grid);
        assert_eq!(actual.scrollback_limit, expected.scrollback_limit);
        macro_rules! same_state {
            ($($field:ident),+ $(,)?) => { $(
                assert_eq!(actual.st.$field, expected.st.$field, stringify!($field));
            )+ };
        }
        same_state!(
            width,
            height,
            cx,
            cy,
            fg,
            bg,
            attrs,
            underline_color,
            underline_color_handle,
            hyperlink,
            wrap_pending,
            autowrap,
            origin_mode,
            insert_mode,
            cursor_visible,
            bracketed_paste,
            last_char,
            tabs,
            dec_line_drawing,
            scroll_top,
            scroll_bottom,
            saved,
            alt_cursor,
            app_cursor,
            mouse_mode,
            mouse_sgr,
            nested_input,
            nested_input_changes,
            agent_events,
            cwd,
            prompt_marks,
            clipboard_requests,
            terminal_title,
            title_revision,
            default_fg,
            default_bg,
            dark_theme,
            palette_notifications,
            scrollback_limit,
            responses,
        );
        for (a, b) in actual.st.keyboard.iter().zip(&expected.st.keyboard) {
            assert_eq!(a.flags, b.flags);
            assert_eq!(a.stack, b.stack);
            assert_eq!(a.depth, b.depth);
        }
        match (&actual.st.alt_backup, &expected.st.alt_backup) {
            (Some(a), Some(b)) => assert_same_grid(a, b),
            (None, None) => {}
            _ => panic!("alternate-screen backup differs"),
        }
    }

    fn compare_ascii_feeds(width: u16, chunks: &[&[u8]]) {
        let mut actual = Terminal::new(width, 4);
        let mut expected = Terminal::new(width, 4);
        actual.set_scrollback_limit(5);
        expected.set_scrollback_limit(5);
        // Seed wide and clustered targets so replacement can hit either half.
        let seed = "A界e\u{301}Z\r".as_bytes();
        actual.feed(seed);
        feed_scalar(&mut expected, seed);
        for chunk in chunks {
            actual.grid.clear_damage();
            expected.grid.clear_damage();
            feed_scalar(&mut expected, chunk);
            actual.feed(chunk);
            assert_same_terminal(&actual, &expected);
        }
        // Expose hidden last-char, cursor, pending-wrap and saved-mode errors.
        let followup = b"\x1b[3b\x1b[6n\x1b[?1049l\x1b8X\r\nlast";
        actual.feed(followup);
        feed_scalar(&mut expected, followup);
        assert_same_terminal(&actual, &expected);
        actual.resize(width.saturating_add(3), 5);
        expected.resize(width.saturating_add(3), 5);
        assert_same_terminal(&actual, &expected);
        for limit in [2, 0, 5] {
            actual.set_scrollback_limit(limit);
            expected.set_scrollback_limit(limit);
            assert_same_terminal(&actual, &expected);
        }
    }

    #[test]
    fn borrowed_sgr_groups_match_owned_color_and_attribute_semantics() {
        struct Sink<'a> {
            terminal: &'a mut Terminal,
            reference: bool,
        }
        impl Perform for Sink<'_> {
            fn csi_dispatch(&mut self, params: &Params, inter: &[u8], _ignore: bool, action: char) {
                // Match production: apply the truncated parameters on parser overflow.
                if !inter.is_empty() || action != 'm' {
                    return;
                }
                let mut sink = Performer {
                    grid: &mut self.terminal.grid,
                    st: &mut self.terminal.st,
                };
                if self.reference {
                    sink.apply_sgr_reference(params);
                } else {
                    sink.apply_sgr(params);
                }
            }
            fn print(&mut self, c: char) {
                Performer {
                    grid: &mut self.terminal.grid,
                    st: &mut self.terminal.st,
                }
                .print(c);
            }
        }
        fn run(bytes: &[u8], reference: bool) -> Terminal {
            let mut terminal = Terminal::new(80, 4);
            let mut parser = Parser::new();
            let mut sink = Sink {
                terminal: &mut terminal,
                reference,
            };
            for &byte in bytes {
                parser.advance(&mut sink, byte);
            }
            terminal
        }
        let mut parameters: Vec<String> = [
            "",
            "0",
            ";",
            "1;2;3;4;5;7;8;9",
            "22;23;24;25;27;28;29",
            "30;39;40;49;90;107",
            "4:0;4:1;4:2;4:3;4:4;4:5;4:99",
            "38",
            "38;5",
            "38;5;255;1",
            "38;5;65535;22",
            "38;31;1",
            "38;2",
            "38;2;255",
            "38;2;255;1",
            "38;2;255;1;2;31",
            "38:5:240;48:5:1;58:5:7",
            "38:2:1:2:3;48:2::4:5:6;58:2:0:7:8:9",
            "38:2;48:2::;58:2:1:2",
            "38;2:9;1:2;3:4;5:6;7",
            "38:5;240",
            "38:2;1;2;3",
            "38;5:240;1",
            "38;2;1;2:7;3",
            "48;5",
            "58;2;1;2",
            "58;5;4;59",
            "?1;38;5;4",
            ">1;4",
            "1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        // Cut a truecolor sequence at, and beyond, vte's 32-value bound.
        for prefix in [29, 30, 31] {
            parameters.push(format!("{}38;2;9;8;7", "1;".repeat(prefix)));
        }
        let tokens = [
            "",
            "0",
            "1",
            "22",
            "38",
            "48",
            "58",
            "2",
            "5",
            "255",
            "256",
            "65535",
            "4:0",
            "4:3",
            "38:5:240",
            "38:2::1:2:3",
            "38:2:7:8:9",
            "38:99",
            "999",
            "58:2:1:2",
        ];
        let mut state = 0x8d26_53a1_u32;
        for count in 0..512 {
            let mut parts = Vec::new();
            for _ in 0..(count % 12 + 1) {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                parts.push(tokens[state as usize % tokens.len()]);
            }
            parameters.push(parts.join(";"));
        }
        for parameter in parameters {
            let bytes =
                format!("\x1b[1;3;38;5;2;48;5;4;4:3;58:5:7mA\x1b[{parameter}mB\x1b[38;5;9;4:5mC");
            let actual = run(bytes.as_bytes(), false);
            let expected = run(bytes.as_bytes(), true);
            assert_eq!(
                (
                    actual.st.fg,
                    actual.st.bg,
                    actual.st.attrs,
                    actual.st.underline_color
                ),
                (
                    expected.st.fg,
                    expected.st.bg,
                    expected.st.attrs,
                    expected.st.underline_color
                ),
                "SGR {parameter:?}"
            );
            assert_same_terminal(&actual, &expected);
        }
        let mut actual = run(b"\x1b[1;31;4:3;58;5;7mX", false);
        let mut expected = run(b"\x1b[1;31;4:3;58;5;7mX", true);
        Performer {
            grid: &mut actual.grid,
            st: &mut actual.st,
        }
        .apply_sgr(&Params::default());
        Performer {
            grid: &mut expected.grid,
            st: &mut expected.st,
        }
        .apply_sgr_reference(&Params::default());
        assert_same_terminal(&actual, &expected);
    }

    #[test]
    fn feed_probe_records_only_successful_multicell_writes() {
        for (stream, wide, insert, width, productive) in [
            (&b"AB"[..], false, false, 160, false),
            (&b"ABC"[..], false, false, 160, true),
            (&b"ABC\r\x1b[6n"[..], false, false, 160, true),
            (&b"ABC"[..], true, false, 160, false),
            (&b"ABCDE"[..], true, false, 160, true),
            (&b"ABCDE"[..], false, true, 160, false),
            (&b"ABCDE"[..], false, false, 1, false),
            ("界e\u{301}".as_bytes(), false, false, 160, false),
        ] {
            let mut actual = Terminal::new(width, 3);
            let mut expected = Terminal::new(width, 3);
            for terminal in [&mut actual, &mut expected] {
                if wide {
                    terminal.grid.set_grapheme(2, 0, "界", Cell::default(), 2);
                }
                terminal.st.insert_mode = insert;
            }
            {
                let mut perf = BufferedPerformer::new(Performer {
                    grid: &mut actual.grid,
                    st: &mut actual.st,
                });
                for &byte in stream {
                    actual.parser.advance(&mut perf, byte);
                }
                perf.flush();
                assert_eq!(perf.used_bulk, productive, "{stream:?}");
                assert_eq!(perf.len, 0);
            }
            feed_scalar(&mut expected, stream);
            assert_same_terminal(&actual, &expected);
        }
        // Already identical cells still amortize comparison; real damage is not
        // required to make a bulk operation useful. A fresh sink forgets it.
        let mut terminal = Terminal::new(160, 3);
        terminal.feed(b"ABC\r");
        terminal.grid.clear_damage();
        let mut perf = BufferedPerformer::new(Performer {
            grid: &mut terminal.grid,
            st: &mut terminal.st,
        });
        assert!(!perf.used_bulk);
        for c in "ABC".chars() {
            perf.print(c);
        }
        perf.flush();
        assert!(perf.used_bulk);
        assert!(!perf.inner.grid.is_dirty());
    }

    #[test]
    fn feed_probe_transitions_match_scalar_across_external_chunks() {
        let controls = b"\x1b[2;3HX\rY".repeat(48);
        let text = b"ordinary printable run through wrap and history\r\n".repeat(12);
        let modes: &[&[u8]] = &[b"", b"\x1b[4h", b"\x1b(0", b"\x1b[?7l", b"\x1b[?1049h"];
        for &mode in modes {
            for productive_first in [false, true] {
                let mut stream = mode.to_vec();
                for part in if productive_first {
                    [&text, &controls, &text]
                } else {
                    [&controls, &text, &controls]
                } {
                    stream.extend_from_slice(part);
                }
                stream.extend_from_slice(b"\x1b[4l\x1b(B\x1b[?7h\x1b[?1049l\x1b[3b\x1b[6n");
                for width in [1, 2, 7, 160] {
                    compare_ascii_feeds(width, &[&stream, b""]);
                    for size in [1, 2, 8, 16, 32, 64, 127, 128, 129, 256, 4096] {
                        let mut chunks = Vec::new();
                        for chunk in stream.chunks(size) {
                            chunks.push(chunk);
                            chunks.push(&b""[..]);
                        }
                        compare_ascii_feeds(width, &chunks);
                    }
                }
            }
        }
    }

    #[test]
    fn feed_probe_internal_boundary_preserves_pending_parser_state() {
        let sequences: &[&[u8]] = &[
            "界e\u{301}1\u{fe0f}\u{20e3}".as_bytes(),
            b"\x1b[38;2;4;5;6mZ\x1b[6n",
            b"\x1b]2;boundary title\x1b\\",
            b"\x1b]8;;https://example.test\x07Z\x1b]8;;\x07",
            b"\x1bP1;2qignored\x1b\\",
            b"\xe2\x82\x1b[2J",
        ];
        for &sequence in sequences {
            for split in 1..sequence.len() {
                for productive in [false, true] {
                    // Put the internal 128-byte cut at every position within
                    // each sequence, after either useful writes or controls.
                    let mut stream = vec![if productive { b'A' } else { b'\r' }; 128 - split];
                    stream.extend_from_slice(sequence);
                    stream.extend_from_slice(b"ASCII after boundary\r\n");
                    stream.extend_from_slice("界e\u{301}\x1b[2b\x1b[6n".as_bytes());
                    for width in [2, 7, 160] {
                        compare_ascii_feeds(width, &[&stream]);
                        for external in [1, 127, 128, 129] {
                            compare_ascii_feeds(
                                width,
                                &[&stream[..external], b"", &stream[external..]],
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn buffered_ascii_flushes_before_every_semantic_callback() {
        fn callback(performer: &mut impl Perform, case: u8) {
            match case {
                0 => performer.execute(b'\r'),
                1 => performer.csi_dispatch(&Params::default(), b"", false, 'c'),
                2 => performer.osc_dispatch(&[b"2", b"title"], true),
                3 => performer.esc_dispatch(b"", false, b'7'),
                4 => performer.hook(&Params::default(), b"", false, 'q'),
                5 => performer.put(b'x'),
                6 => performer.unhook(),
                _ => performer.print('\u{301}'),
            }
        }
        for case in 0..8 {
            let mut actual = Terminal::new(12, 2);
            let mut expected = Terminal::new(12, 2);
            {
                let mut buffered = BufferedPerformer::new(Performer {
                    grid: &mut actual.grid,
                    st: &mut actual.st,
                });
                buffered.print('A');
                buffered.print('B');
                assert_eq!(buffered.len, 1);
                callback(&mut buffered, case);
                assert_eq!(buffered.len, 0);
                buffered.print('D');
                let mut scalar = Performer {
                    grid: &mut expected.grid,
                    st: &mut expected.st,
                };
                scalar.print('A');
                scalar.print('B');
                callback(&mut scalar, case);
                scalar.print('D');
            }
            assert_same_terminal(&actual, &expected);
        }
    }

    #[test]
    fn buffered_ascii_matches_scalar_across_callbacks_and_chunks() {
        let streams: &[&[u8]] = &[
            b"ABC\r\nDEF\tGH\x08I\x00\x07\x18\x1aJK\x7f\xff",
            b"AB\x1b7CD\x1b[6n\x1b[2b\x1b8EF\x1b[2K\x1b[2;3HGH\x1b[1P\x1b[2@IJ",
            b"AB\x1b[1;3;4;38;2;1;2;3;48;5;9;58;5;4mCDE\x1b[0mFG",
            b"AB\x1b[4hCDE\x1b[4lFG\x1b[?7lHIJKLMNOP\x1b[?7hQR",
            b"AB\x1b(0lqk\x1b(BEF\x1b[2;3r\x1b[?6h\x1b[1;1HX\r\nYZ\r\nlast\x1b[?6l",
            b"AB\x1b[?1049hCD\x1b[?25;2004;1003;1006;1hEF\x1b[?1049lGH\x1bcIJ",
            b"AB\x1b[>1uCD\x1b[<uEF\x1b[?2031hGH\x1b[?2031l",
            b"AB\x1bP1;2qignored\x1b\\CD\x1bPqstop\x18EF\x1b_ignored\x1b\\GH",
            b"AB\x1b]8;;https://example.test\x1b\\CDE\x1b]8;;\x07FG",
            b"AB\x1b]7;file://host/tmp\x07CD\x1b]133;A\x07EF\x1b]52;c;SGk=\x1b\\GH",
            b"AB\x1b]2;title\x07CD\x1b[?1049hEF\x1b]777;notify;uniterm://cli-agent;{\"agent\":\"codex\",\"event\":\"session_end\"}\x07GH",
            "\u{600}ABe\u{301}CD1\u{fe0f}\u{20e3}EF👩\u{200d}💻GH界IJ".as_bytes(),
            b"AB\xc2\xa2C\xe2\x82\xacDE\xe4\xbf\x99\xb5F\xc2\x85G\xe2\x1b[2JHI",
        ];
        for &stream in streams {
            for width in [1, 2, 7, 160] {
                compare_ascii_feeds(width, &[stream]);
                for split in 0..=stream.len() {
                    compare_ascii_feeds(width, &[&stream[..split], b"", &stream[split..]]);
                }
                let bytewise: Vec<&[u8]> = stream.chunks(1).collect();
                compare_ascii_feeds(width, &bytewise);
                let small_chunks: Vec<&[u8]> = stream.chunks(3).collect();
                compare_ascii_feeds(width, &small_chunks);
            }
        }
    }

    #[test]
    fn buffered_ascii_mixed_tiny_feeds_preserve_parser_state() {
        let streams: &[&[u8]] = &[
            b"AB\x1b[31mC\x1b[32mD\x1b[0mEF\x1b[6n\x1b[2b",
            "AB¢C€DE1\u{fe0f}\u{20e3}FG\u{600}HI".as_bytes(),
            b"AB\x1b]8;;https://example.test\x1b\\CD\x1b]8;;\x07EF\x1bPqignored\x1b\\GH",
            b"AB\xe4\xbf\x99\xb5C\xe2\x1b[2JDE\xc2\x85F\xffGH",
        ];
        let sizes = [1, 0, 2, 1, 3, 0];
        for &stream in streams {
            for rotation in 0..sizes.len() {
                let mut chunks = Vec::new();
                let mut at = 0;
                let mut step = rotation;
                while at < stream.len() {
                    let end = (at + sizes[step % sizes.len()]).min(stream.len());
                    chunks.push(&stream[at..end]);
                    at = end;
                    step += 1;
                }
                chunks.push(b"");
                for width in [1, 2, 7, 160] {
                    compare_ascii_feeds(width, &chunks);
                }
            }
        }
    }

    #[test]
    fn buffered_ascii_prefix_then_wide_repair_matches_scalar() {
        for column in [3, 6] {
            for autowrap in [false, true] {
                for insert in [false, true] {
                    let mut actual = Terminal::new(8, 3);
                    let mut expected = Terminal::new(8, 3);
                    for terminal in [&mut actual, &mut expected] {
                        let styled = Cell {
                            bg: Color::Idx(4),
                            hyperlink: terminal.grid.intern_hyperlink("https://example.test/wide"),
                            ..Cell::default()
                        };
                        terminal.grid.set_grapheme(column, 0, "界", styled, 2);
                        terminal.grid.set_grapheme(
                            if column == 3 { 5 } else { 3 },
                            0,
                            "e\u{301}",
                            styled,
                            1,
                        );
                        terminal.st.autowrap = autowrap;
                        terminal.st.insert_mode = insert;
                    }
                    for chunk in [
                        &b"abcdefghij"[..],
                        &b"\x1b[4hKL\x1b[4lMN"[..],
                        &b"\rABC\x1b[6n\x1b[2b"[..],
                    ] {
                        actual.grid.clear_damage();
                        expected.grid.clear_damage();
                        feed_scalar(&mut expected, chunk);
                        actual.feed(chunk);
                        assert_same_terminal(&actual, &expected);
                    }
                }
            }
        }
    }

    #[test]
    fn buffered_ascii_guard_rejects_out_of_bounds_cursor() {
        for (cx, cy) in [(8, 0), (u16::MAX, 0), (1, 3), (1, u16::MAX)] {
            let mut terminal = Terminal::new(8, 3);
            terminal.st.cx = cx;
            terminal.st.cy = cy;
            let buffered = BufferedPerformer::new(Performer {
                grid: &mut terminal.grid,
                st: &mut terminal.st,
            });
            assert!(!buffered.can_batch());
        }
    }

    #[test]
    fn buffered_ascii_matches_scalar_at_capacity_wrap_and_history_edges() {
        for length in [0, 1, 127, 128, 129, 159, 160, 161, 513] {
            let bytes: Vec<u8> = (0..length).map(|index| b' ' + (index % 95) as u8).collect();
            for width in [1, 2, 7, 128, 160] {
                compare_ascii_feeds(width, &[&bytes, b"\x1b[6n"]);
                let mut lines = Vec::new();
                for _ in 0..12 {
                    lines.extend_from_slice(&bytes);
                    lines.extend_from_slice(b"\r\n");
                }
                compare_ascii_feeds(width, &[&lines]);
            }
        }
        for &bytes in &[&b"ABCDEF"[..], "\u{600}AB".as_bytes(), b"AB\xcc\x81C"] {
            for mask in 0..(1usize << bytes.len().saturating_sub(1)) {
                let mut chunks = Vec::new();
                let mut start = 0;
                for end in 1..=bytes.len() {
                    if end == bytes.len() || mask & (1 << (end - 1)) != 0 {
                        chunks.push(&bytes[start..end]);
                        start = end;
                    }
                }
                compare_ascii_feeds(7, &chunks);
            }
        }
    }

    #[test]
    fn buffered_ascii_noop_and_changed_endpoints_keep_precise_damage() {
        let mut terminal = Terminal::new(12, 2);
        terminal.feed(b"abcdefghij");
        terminal.grid.clear_damage();
        terminal.feed(b"\rabcdefghij");
        assert!(!terminal.grid.is_dirty());
        terminal.feed(b"\rabXdefgYij");
        assert_eq!(terminal.grid.row_damage(0), Some((2, 7)));
        assert_eq!(terminal.grid.row_damage(1), None);
        terminal.grid.clear_damage();
        terminal.feed(b"");
        assert!(!terminal.grid.is_dirty());
    }

    #[test]
    fn parser_results_do_not_depend_on_pty_chunk_boundaries() {
        let stream = b"start\r\n\x1b[38;2;1;2;3mwide: \xe7\x95\x8c e\xcc\x81\x1b[0m\r\n\x1b[2Kdone\x1b]133;D;0\x07";
        let mut whole = Terminal::new(24, 5);
        whole.feed(stream);
        let mut chunked = Terminal::new(24, 5);
        let mut at = 0;
        let mut seed = 0x1234_5678u32;
        while at < stream.len() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let take = ((seed >> 29) as usize + 1).min(stream.len() - at);
            chunked.feed(&stream[at..at + take]);
            at += take;
        }
        assert_eq!(
            whole.grid().export_lines(20),
            chunked.grid().export_lines(20)
        );
        assert_eq!(whole.cursor(), chunked.cursor());
        assert_eq!(whole.take_prompt_marks(), chunked.take_prompt_marks());
    }

    #[test]
    fn detection_tail_is_always_the_live_bottom() {
        let mut terminal = Terminal::new(20, 3);
        terminal.feed(b"old\r\none\r\ntwo\r\npermission required");
        let tail = terminal.bottom_text(2);
        assert!(!tail.contains("old"));
        assert!(tail.contains("two"));
        assert!(tail.contains("permission required"));
    }

    #[test]
    fn recent_output_text_includes_announcements_at_the_top_of_a_fresh_pane() {
        let mut terminal = Terminal::new(80, 30);
        terminal.feed(b"Server listening on http://localhost:4321\r\n");
        assert!(terminal
            .recent_output_text(12)
            .contains("Server listening on http://localhost:4321"));
        assert!(!terminal.bottom_text(12).contains("Server listening"));
    }

    #[test]
    fn mode_2031_reports_the_current_theme_on_enable_and_on_change() {
        let mut terminal = Terminal::new(80, 24);
        terminal.feed(b"\x1b[?2031h");
        assert_eq!(terminal.take_responses(), b"\x1b[?997;1n".to_vec());
        terminal.set_default_colors(Color::Rgb(0x11, 0x11, 0x11), Color::Rgb(0xee, 0xee, 0xee));
        assert_eq!(terminal.take_responses(), b"\x1b[?997;2n".to_vec());
        terminal.feed(b"\x1b[?2031l");
        terminal.set_default_colors(Color::Rgb(0xee, 0xee, 0xee), Color::Rgb(0x11, 0x11, 0x11));
        assert!(terminal.take_responses().is_empty());
    }

    #[test]
    fn automation_output_includes_bounded_scrollback() {
        let mut terminal = Terminal::new(12, 3);
        terminal.feed(b"one\r\ntwo\r\nthree\r\nfour");
        let output = terminal.automation_output_text(3);
        assert!(!output.contains("one"));
        assert!(output.contains("two"));
        assert!(output.contains("three"));
        assert!(output.contains("four"));
    }
}
