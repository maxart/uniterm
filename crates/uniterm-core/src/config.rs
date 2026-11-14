//! Appearance and behaviour configuration, parsed from the Ghostty-style
//! `key = value` format (`docs/10`).
//!
//! This module is pure: [`Config::parse`] takes the file text and returns a
//! `Config`; the file I/O lives in the server/client. Hex `#rrggbb` colours
//! are carried as 24-bit [`Color::Rgb`] (the pipeline is truecolor end to
//! end); bare numbers are xterm-256 palette indices.

use crate::{
    delegate::{
        DelegationPolicy, DELEGATION_DEFAULT_ANSWERS_PER_HOUR, DELEGATION_MAX_ANSWERS_PER_HOUR,
    },
    guardrail::{
        GuardLimits, GUARDRAIL_MAX_ACTIVE_RUNS, GUARDRAIL_MAX_ELAPSED_SECONDS,
        GUARDRAIL_MAX_ITERATIONS, GUARDRAIL_MAX_PROJECT_SELECTORS, GUARDRAIL_MAX_ROLE_PANES,
    },
    Color,
};

/// Where the status line sits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatusPosition {
    Top,
    Bottom,
}

/// Alignment inside existing Project Tab and stack Tab label rectangles.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum TabLabelAlignment {
    /// Place the label at the leading edge.
    Left,
    /// Share spare display cells between both edges.
    #[default]
    Center,
    /// Place the label at the trailing edge.
    Right,
}

impl TabLabelAlignment {
    /// Stable configuration choices, in Settings display order.
    pub const ALL: &'static [Self] = &[Self::Left, Self::Center, Self::Right];

    /// Canonical spelling used by configuration and Settings patches.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
        }
    }

    /// Human-readable choices for the Settings Appearance control.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Center => "Center",
            Self::Right => "Right",
        }
    }

    /// Reject unknown choices so invalid settings cannot silently change alignment.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|alignment| value.trim().eq_ignore_ascii_case(alignment.name()))
    }
}

/// Where an agent attention notification is delivered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotificationDelivery {
    Off,
    /// A clickable toast drawn inside Uniterm.
    Uniterm,
    /// The host terminal's OSC notification facility.
    Terminal,
    /// The desktop operating system's notification center.
    System,
}

impl NotificationDelivery {
    pub const ALL: &'static [NotificationDelivery] = &[
        NotificationDelivery::Off,
        NotificationDelivery::Uniterm,
        NotificationDelivery::Terminal,
        NotificationDelivery::System,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            NotificationDelivery::Off => "off",
            NotificationDelivery::Uniterm => "uniterm",
            NotificationDelivery::Terminal => "terminal",
            NotificationDelivery::System => "system",
        }
    }

    pub fn parse(value: &str) -> NotificationDelivery {
        match value.trim().to_ascii_lowercase().as_str() {
            "uniterm" | "inline" => NotificationDelivery::Uniterm,
            "terminal" => NotificationDelivery::Terminal,
            "system" | "os" | "native" => NotificationDelivery::System,
            _ => NotificationDelivery::Off,
        }
    }
}

/// What an agent notification sounds like. The sound is decided by the
/// Workspace's configuration and played by the attached client, so a remote
/// Workspace chimes on the machine the human is sitting at.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSound {
    Off,
    /// The terminal's own bell (BEL); every terminal, every transport, no
    /// dependency, and the user's bell preference decides sound or flash.
    Bell,
    /// A short two-tone chime synthesised by the client, so it sounds the same
    /// on every platform without shipping or locating an audio file.
    Chime,
    /// A user-supplied audio file named by `notification-sound-file`.
    File,
}

impl NotificationSound {
    pub const ALL: &'static [NotificationSound] = &[
        NotificationSound::Off,
        NotificationSound::Bell,
        NotificationSound::Chime,
        NotificationSound::File,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            NotificationSound::Off => "off",
            NotificationSound::Bell => "bell",
            NotificationSound::Chime => "chime",
            NotificationSound::File => "file",
        }
    }

    pub fn parse(value: &str) -> NotificationSound {
        match value.trim().to_ascii_lowercase().as_str() {
            "bell" | "beep" => NotificationSound::Bell,
            "chime" | "tone" => NotificationSound::Chime,
            "file" | "custom" => NotificationSound::File,
            _ => NotificationSound::Off,
        }
    }
}

/// Built-in semantic palettes. Names are stable config values and are shared
/// by the Settings surface, server chrome, and client overlays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemePreset {
    UnitermDark,
    UnitermLight,
    Catppuccin,
    TokyoNight,
    Dracula,
    Nord,
    GruvboxDark,
    GruvboxLight,
    SolarizedDark,
    SolarizedLight,
    Kanagawa,
    RosePine,
    /// A bundled Omarchy palette with a stable, attributed config name.
    Omarchy(OmarchyTheme),
    Custom,
}

#[path = "config/omarchy.rs"]
mod omarchy;

#[path = "config/omarchy_live.rs"]
mod omarchy_live;

/// Bound externally observed palette input before parsing or allocating it.
pub const MAX_OMARCHY_PALETTE_BYTES: usize = 64 * 1024;

/// An index into the compiled palette catalog; only catalog entries can create it.
/// See `themes/omarchy/README.md` for sources and reproducible generation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OmarchyTheme(usize);

impl ThemePreset {
    /// Complete Settings catalog, including the offline Omarchy snapshot.
    pub const ALL: &'static [ThemePreset] = omarchy::ALL;

    pub const fn name(self) -> &'static str {
        match self {
            ThemePreset::UnitermDark => "uniterm-dark",
            ThemePreset::UnitermLight => "uniterm-light",
            ThemePreset::Catppuccin => "catppuccin",
            ThemePreset::TokyoNight => "tokyo-night",
            ThemePreset::Dracula => "dracula",
            ThemePreset::Nord => "nord",
            ThemePreset::GruvboxDark => "gruvbox-dark",
            ThemePreset::GruvboxLight => "gruvbox-light",
            ThemePreset::SolarizedDark => "solarized-dark",
            ThemePreset::SolarizedLight => "solarized-light",
            ThemePreset::Kanagawa => "kanagawa",
            ThemePreset::RosePine => "rose-pine",
            ThemePreset::Omarchy(theme) => omarchy::PALETTES[theme.0].0,
            ThemePreset::Custom => "custom",
        }
    }

    pub fn parse(name: &str) -> ThemePreset {
        match name.trim().to_ascii_lowercase().as_str() {
            "light" | "uniterm-light" => ThemePreset::UnitermLight,
            "catppuccin" | "catppuccin-mocha" => ThemePreset::Catppuccin,
            "tokyo-night" | "tokyonight" => ThemePreset::TokyoNight,
            "dracula" => ThemePreset::Dracula,
            "nord" => ThemePreset::Nord,
            "gruvbox-dark" | "gruvbox" => ThemePreset::GruvboxDark,
            "gruvbox-light" => ThemePreset::GruvboxLight,
            "solarized-dark" | "solarized" => ThemePreset::SolarizedDark,
            "solarized-light" => ThemePreset::SolarizedLight,
            "kanagawa" => ThemePreset::Kanagawa,
            "rose-pine" | "rosepine" => ThemePreset::RosePine,
            "custom" => ThemePreset::Custom,
            name => omarchy::PALETTES
                .binary_search_by_key(&name, |(name, _)| *name)
                .map(|index| ThemePreset::Omarchy(OmarchyTheme(index)))
                .unwrap_or(ThemePreset::UnitermDark),
        }
    }
}

/// Resolved colours for Uniterm's own chrome (status line, dividers). Child
/// programs render their own colours, which we pass through untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Theme {
    pub background: Color,
    pub surface: Color,
    pub foreground: Color,
    pub muted: Color,
    pub accent: Color,
    /// A surface-blended form of `accent` for persistent buttons that should
    /// remain secondary to the active Tab and Project selection.
    pub accent_muted: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub attention: Color,
    pub selection_bg: Color,
    pub border: Color,
    pub border_active: Color,
    pub status_fg: Color,
    pub status_bg: Color,
    pub status_active_fg: Color,
    pub status_active_bg: Color,
    pub divider: Color,
}

/// Keep secondary controls recognizably on-accent without competing with the
/// full-strength active state. Built-in palettes use RGB, while custom indexed
/// colors safely fall back to the supplied accent.
const fn muted_accent(surface: Color, accent: Color) -> Color {
    match (surface, accent) {
        (Color::Rgb(sr, sg, sb), Color::Rgb(ar, ag, ab)) => Color::Rgb(
            ((sr as u16 * 3 + ar as u16 * 2) / 5) as u8,
            ((sg as u16 * 3 + ag as u16 * 2) / 5) as u8,
            ((sb as u16 * 3 + ab as u16 * 2) / 5) as u8,
        ),
        (_, accent) => accent,
    }
}

impl Theme {
    // The eight arguments are the eight required semantic roles; keeping them
    // named at each palette definition is clearer than positional array slots.
    #[allow(clippy::too_many_arguments)]
    const fn semantic(
        background: Color,
        surface: Color,
        foreground: Color,
        muted: Color,
        accent: Color,
        success: Color,
        warning: Color,
        error: Color,
    ) -> Theme {
        let accent_muted = muted_accent(surface, accent);
        Theme {
            background,
            surface,
            foreground,
            muted,
            accent,
            accent_muted,
            success,
            warning,
            error,
            attention: warning,
            selection_bg: accent,
            border: muted,
            border_active: accent,
            status_fg: foreground,
            status_bg: surface,
            status_active_fg: background,
            status_active_bg: accent,
            divider: muted,
        }
    }

    /// The built-in dark theme (also the default).
    pub const fn dark() -> Theme {
        Theme::semantic(
            Color::Rgb(0x10, 0x13, 0x18),
            Color::Rgb(0x1a, 0x1f, 0x28),
            Color::Rgb(0xe6, 0xe9, 0xef),
            Color::Rgb(0x7f, 0x89, 0x9b),
            Color::Rgb(0x62, 0xa0, 0xea),
            Color::Rgb(0x57, 0xe3, 0x89),
            Color::Rgb(0xf5, 0xc2, 0x49),
            Color::Rgb(0xff, 0x6b, 0x6b),
        )
    }

    /// The built-in light theme.
    pub const fn light() -> Theme {
        Theme::semantic(
            Color::Rgb(0xfa, 0xfb, 0xfc),
            Color::Rgb(0xea, 0xec, 0xf0),
            Color::Rgb(0x24, 0x29, 0x2f),
            Color::Rgb(0x6e, 0x77, 0x81),
            Color::Rgb(0x09, 0x69, 0xda),
            Color::Rgb(0x1a, 0x7f, 0x37),
            Color::Rgb(0x9a, 0x67, 0x00),
            Color::Rgb(0xcf, 0x22, 0x2e),
        )
    }

    /// Resolve a theme by name; unknown names fall back to dark.
    pub fn named(name: &str) -> Theme {
        match ThemePreset::parse(name) {
            ThemePreset::Omarchy(theme) => {
                let [background, surface, foreground, muted, accent, success, warning, error] =
                    omarchy::PALETTES[theme.0]
                        .1
                        .map(|rgb| Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8));
                Theme::semantic(
                    background, surface, foreground, muted, accent, success, warning, error,
                )
            }
            ThemePreset::UnitermLight => Theme::light(),
            ThemePreset::Catppuccin => Theme::semantic(
                Color::Rgb(0x1e, 0x1e, 0x2e),
                Color::Rgb(0x31, 0x32, 0x44),
                Color::Rgb(0xcd, 0xd6, 0xf4),
                Color::Rgb(0x7f, 0x84, 0xa2),
                Color::Rgb(0x89, 0xb4, 0xfa),
                Color::Rgb(0xa6, 0xe3, 0xa1),
                Color::Rgb(0xf9, 0xe2, 0xaf),
                Color::Rgb(0xf3, 0x8b, 0xa8),
            ),
            ThemePreset::TokyoNight => Theme::semantic(
                Color::Rgb(0x1a, 0x1b, 0x26),
                Color::Rgb(0x24, 0x28, 0x3b),
                Color::Rgb(0xc0, 0xca, 0xf5),
                Color::Rgb(0x56, 0x5f, 0x89),
                Color::Rgb(0x7a, 0xa2, 0xf7),
                Color::Rgb(0x9e, 0xce, 0x6a),
                Color::Rgb(0xe0, 0xaf, 0x68),
                Color::Rgb(0xf7, 0x76, 0x8e),
            ),
            ThemePreset::Dracula => Theme::semantic(
                Color::Rgb(0x28, 0x2a, 0x36),
                Color::Rgb(0x44, 0x47, 0x5a),
                Color::Rgb(0xf8, 0xf8, 0xf2),
                Color::Rgb(0x62, 0x72, 0xa4),
                Color::Rgb(0xbd, 0x93, 0xf9),
                Color::Rgb(0x50, 0xfa, 0x7b),
                Color::Rgb(0xf1, 0xfa, 0x8c),
                Color::Rgb(0xff, 0x55, 0x55),
            ),
            ThemePreset::Nord => Theme::semantic(
                Color::Rgb(0x2e, 0x34, 0x40),
                Color::Rgb(0x3b, 0x42, 0x52),
                Color::Rgb(0xec, 0xef, 0xf4),
                Color::Rgb(0x81, 0xa1, 0xc1),
                Color::Rgb(0x88, 0xc0, 0xd0),
                Color::Rgb(0xa3, 0xbe, 0x8c),
                Color::Rgb(0xeb, 0xcb, 0x8b),
                Color::Rgb(0xbf, 0x61, 0x6a),
            ),
            ThemePreset::GruvboxDark => Theme::semantic(
                Color::Rgb(0x28, 0x28, 0x28),
                Color::Rgb(0x3c, 0x38, 0x36),
                Color::Rgb(0xeb, 0xdb, 0xb2),
                Color::Rgb(0x92, 0x83, 0x74),
                Color::Rgb(0x83, 0xa5, 0x98),
                Color::Rgb(0xb8, 0xbb, 0x26),
                Color::Rgb(0xfa, 0xbd, 0x2f),
                Color::Rgb(0xfb, 0x49, 0x34),
            ),
            ThemePreset::GruvboxLight => Theme::semantic(
                Color::Rgb(0xfb, 0xf1, 0xc7),
                Color::Rgb(0xeb, 0xdb, 0xb2),
                Color::Rgb(0x3c, 0x38, 0x36),
                Color::Rgb(0x7c, 0x6f, 0x64),
                Color::Rgb(0x07, 0x66, 0x78),
                Color::Rgb(0x79, 0x74, 0x0e),
                Color::Rgb(0xb5, 0x76, 0x14),
                Color::Rgb(0x9d, 0x00, 0x06),
            ),
            ThemePreset::SolarizedDark => Theme::semantic(
                Color::Rgb(0x00, 0x2b, 0x36),
                Color::Rgb(0x07, 0x36, 0x42),
                Color::Rgb(0x83, 0x94, 0x96),
                Color::Rgb(0x58, 0x6e, 0x75),
                Color::Rgb(0x26, 0x8b, 0xd2),
                Color::Rgb(0x85, 0x99, 0x00),
                Color::Rgb(0xb5, 0x89, 0x00),
                Color::Rgb(0xdc, 0x32, 0x2f),
            ),
            ThemePreset::SolarizedLight => Theme::semantic(
                Color::Rgb(0xfd, 0xf6, 0xe3),
                Color::Rgb(0xee, 0xe8, 0xd5),
                Color::Rgb(0x65, 0x7b, 0x83),
                Color::Rgb(0x93, 0xa1, 0xa1),
                Color::Rgb(0x26, 0x8b, 0xd2),
                Color::Rgb(0x85, 0x99, 0x00),
                Color::Rgb(0xb5, 0x89, 0x00),
                Color::Rgb(0xdc, 0x32, 0x2f),
            ),
            ThemePreset::Kanagawa => Theme::semantic(
                Color::Rgb(0x1f, 0x1f, 0x28),
                Color::Rgb(0x2a, 0x2a, 0x37),
                Color::Rgb(0xd7, 0xd0, 0xb2),
                Color::Rgb(0x72, 0x72, 0x69),
                Color::Rgb(0x7e, 0x9c, 0xd8),
                Color::Rgb(0x98, 0xbb, 0x6c),
                Color::Rgb(0xe6, 0xc3, 0x84),
                Color::Rgb(0xe8, 0x24, 0x24),
            ),
            ThemePreset::RosePine => Theme::semantic(
                Color::Rgb(0x19, 0x17, 0x24),
                Color::Rgb(0x26, 0x22, 0x33),
                Color::Rgb(0xe0, 0xde, 0xe4),
                Color::Rgb(0x6e, 0x6a, 0x86),
                Color::Rgb(0xc4, 0xa7, 0xe7),
                Color::Rgb(0x9c, 0xcf, 0xd8),
                Color::Rgb(0xf6, 0xc1, 0x77),
                Color::Rgb(0xeb, 0x6f, 0x92),
            ),
            ThemePreset::UnitermDark | ThemePreset::Custom => Theme::dark(),
        }
    }
}

/// One file-extension-specific editor command.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct EditorRule {
    /// Lowercase extension without its leading dot.
    pub extension: String,
    /// User-authored command, including any fixed arguments.
    pub command: String,
}

/// One actionable problem found by [`Config::diagnostics`]. Parsing remains
/// forgiving at runtime, while `uniterm config check` gives hand-edited files
/// a strict validation path with stable line numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub line: usize,
    pub message: String,
}

/// One post-prefix key mapped to a stable semantic action name. The core owns
/// the schema, while the attach client translates actions to protocol verbs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyBinding {
    pub key: u8,
    pub action: String,
}

impl EditorRule {
    fn parse(extension: &str, command: &str) -> Result<Self, String> {
        let extension = extension
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase();
        if extension.is_empty()
            || !extension.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
        {
            return Err(format!("invalid file extension '{extension}'"));
        }
        let command = command.trim();
        if command.is_empty() {
            return Err(format!("editor command for .{extension} is empty"));
        }
        Ok(EditorRule {
            extension,
            command: command.to_string(),
        })
    }
}

/// Explicit status-color values survive named and automatically observed themes.
/// Keeping provenance separate avoids treating palette-derived colors as overrides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThemeOverrides {
    /// Explicit background of inactive status controls.
    pub status_bg: Option<Color>,
    /// Explicit foreground of inactive status controls.
    pub status_fg: Option<Color>,
    /// Explicit background of active status controls.
    pub status_active_bg: Option<Color>,
    /// Explicit foreground of active status controls.
    pub status_active_fg: Option<Color>,
}

impl ThemeOverrides {
    fn apply(self, mut theme: Theme) -> Theme {
        if let Some(color) = self.status_bg {
            theme.status_bg = color;
        }
        if let Some(color) = self.status_fg {
            theme.status_fg = color;
        }
        if let Some(color) = self.status_active_bg {
            theme.status_active_bg = color;
        }
        if let Some(color) = self.status_active_fg {
            theme.status_active_fg = color;
        }
        theme
    }

    fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// The parsed configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The prefix key byte (default Ctrl-A = 0x01).
    pub prefix: u8,
    pub status: bool,
    pub status_position: StatusPosition,
    /// Align only the main Project Tab labels and their stack labels.
    pub tab_label_alignment: TabLabelAlignment,
    pub scrollback_limit: usize,
    pub theme: Theme,
    pub theme_preset: ThemePreset,
    /// Manual palette under explicit overrides, retained when the display preset is Custom.
    pub theme_base_preset: ThemePreset,
    /// User-selected status colors, independent of the current effective palette.
    pub theme_overrides: ThemeOverrides,
    /// Follow the host Omarchy palette only when the runtime confirms eligibility.
    pub follow_omarchy_theme: bool,
    /// Persistent Project rail. It automatically collapses below the width
    /// needed to keep terminal content useful.
    /// Opt-in native orchestration graph surface.
    pub run_graph: bool,
    pub sidebar: bool,
    pub sidebar_width: u16,
    /// Persistent right-hand Observatory rail. The legacy `file-sidebar`
    /// config spelling is retained so existing installations migrate cleanly.
    pub file_sidebar: bool,
    pub file_sidebar_width: u16,
    /// Agent attention delivery and optional completion notices.
    pub notifications: NotificationDelivery,
    pub notify_completion: bool,
    /// Sound played by the client for attention and completion notifications.
    pub notification_sound: NotificationSound,
    /// Audio file used when `notification_sound` is `File`.
    pub notification_sound_file: String,
    pub focus_follows_mouse: bool,
    /// Require confirmation before closing a Pane.
    pub confirm_close: bool,
    /// Require confirmation before closing a Tab and all of its Panes.
    pub confirm_tab_close: bool,
    /// Catch-all command used when opening a file from the file manager.
    pub editor: String,
    /// Ordered extension overrides. Parsing canonicalizes and sorts them.
    pub editor_rules: Vec<EditorRule>,
    /// Restore a session's saved layout/content on start if a snapshot exists
    /// (the built-in resurrect). Autosave is always on; this gates restore.
    pub restore: bool,
    /// Refresh installed agent connectors from this build at startup and after updates.
    pub auto_update_connectors: bool,
    /// Install missing connectors for locally available agent CLIs.
    pub auto_install_connectors: bool,
    /// Keep each Workspace's last explicit approval mode across server restarts.
    pub persist_auto_approve: bool,
    /// Accept directory trust only for the owning Project root, separately from tools.
    pub auto_trust_projects: bool,
    /// Event-driven outer terminal title template. Empty disables updates.
    pub window_title: String,
    /// Derive unnamed Tab labels from observed task summaries or terminal context.
    pub auto_tab_names: bool,
    /// Capture accepted direct agent work as durable Tasks.
    pub auto_track_tasks: bool,
    /// Enable the dedicated Workspace fuzzy switcher.
    pub switcher_enabled: bool,
    /// Event-driven text reserved at the right edge of the Tab bar.
    pub status_right: String,
    /// Forward right-clicks to applications that requested mouse reporting.
    pub pane_right_click: bool,
    /// Hold a Pane's screen still from the moment a text selection starts, so
    /// an application that keeps painting (an agent at work) cannot scroll or
    /// repaint the text being selected, and keep left-button drags for that
    /// selection even in applications that asked for the mouse. Off by
    /// default.
    pub freeze_on_select: bool,
    /// Copy a drag selection to the clipboard when the mouse is released. Off,
    /// the selection stays highlighted until a key copies or dismisses it.
    pub copy_on_select: bool,
    /// Bounded native automation policy applied before a run creates Panes.
    pub guardrails: GuardLimits,
    /// Exact Project names or canonical roots allowed to host native runs.
    /// Empty means all Projects owned by this Workspace.
    pub guardrail_allowed_projects: Vec<String>,
    /// Hourly auto-answer budget per armed Pane before delegation disarms
    /// itself and hands every prompt back to the human.
    pub delegation_max_answers_per_hour: u16,
    /// User overrides keyed by the byte that follows the prefix.
    pub bindings: Vec<KeyBinding>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            prefix: 0x01, // Ctrl-A
            status: true,
            status_position: StatusPosition::Top,
            tab_label_alignment: TabLabelAlignment::default(),
            scrollback_limit: 10_000,
            theme: Theme::dark(),
            theme_preset: ThemePreset::UnitermDark,
            theme_base_preset: ThemePreset::UnitermDark,
            theme_overrides: ThemeOverrides::default(),
            follow_omarchy_theme: false,
            run_graph: false,
            sidebar: true,
            sidebar_width: 24,
            file_sidebar: true,
            file_sidebar_width: 36,
            notifications: NotificationDelivery::Uniterm,
            notify_completion: false,
            notification_sound: NotificationSound::Bell,
            notification_sound_file: String::new(),
            focus_follows_mouse: false,
            confirm_close: true,
            confirm_tab_close: true,
            editor: "vi".into(),
            editor_rules: Vec::new(),
            restore: true,
            auto_update_connectors: true,
            auto_install_connectors: true,
            persist_auto_approve: false,
            auto_trust_projects: true,
            window_title: "{hostname}: {workspace}".into(),
            auto_tab_names: false,
            auto_track_tasks: true,
            switcher_enabled: false,
            status_right: "{zoom}".into(),
            pane_right_click: false,
            freeze_on_select: false,
            copy_on_select: true,
            guardrails: GuardLimits::default(),
            guardrail_allowed_projects: Vec::new(),
            delegation_max_answers_per_hour: DELEGATION_DEFAULT_ANSWERS_PER_HOUR,
            bindings: Vec::new(),
        }
    }
}

impl Config {
    /// Apply only explicit user overrides to an externally resolved palette.
    pub fn apply_theme_overrides(&self, theme: Theme) -> Theme {
        self.theme_overrides.apply(theme)
    }

    /// Select a manual base without losing explicit color overrides or its saved identity.
    pub fn select_theme(&mut self, name: &str) {
        self.theme_base_preset = ThemePreset::parse(name);
        self.theme_preset = if self.theme_overrides.is_empty() {
            self.theme_base_preset
        } else {
            ThemePreset::Custom
        };
        self.theme = self.apply_theme_overrides(Theme::named(name));
    }

    /// Parse the Ghostty-style `key = value` config text. Unknown keys and
    /// malformed values are ignored (forgiving, like Ghostty).
    pub fn parse(text: &str) -> Config {
        let mut c = Config::default();
        let mut confirm_tab_close = None;
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let key = k.trim();
            let val = config_value(v);
            let val = val.as_str();
            match key {
                "prefix" => {
                    if let Some(b) = parse_key(val) {
                        c.prefix = b;
                    }
                }
                "status" => {
                    if let Some(value) = parse_bool_checked(val) {
                        c.status = value;
                    }
                }
                "auto-update-connectors" => {
                    if let Some(value) = parse_bool_checked(val) {
                        c.auto_update_connectors = value;
                    }
                }
                "auto-install-connectors" => {
                    if let Some(value) = parse_bool_checked(val) {
                        c.auto_install_connectors = value;
                    }
                }
                "restore" | "autosave" => {
                    if let Some(value) = parse_bool_checked(val) {
                        c.restore = value;
                    }
                }
                "tab-label-alignment" => {
                    if let Some(alignment) = TabLabelAlignment::parse(val) {
                        c.tab_label_alignment = alignment;
                    }
                }
                "status-position" => {
                    if val.eq_ignore_ascii_case("top") {
                        c.status_position = StatusPosition::Top;
                    } else if val.eq_ignore_ascii_case("bottom") {
                        c.status_position = StatusPosition::Bottom;
                    }
                }
                "scrollback-limit" => {
                    if let Ok(n) = val.parse() {
                        c.scrollback_limit = n;
                    }
                }
                "theme" => {
                    // Preserve existing line-order semantics: a later palette
                    // selection replaces earlier status overrides.
                    c.theme_overrides = ThemeOverrides::default();
                    c.select_theme(val);
                }
                "follow-omarchy-theme" => set_bool(&mut c.follow_omarchy_theme, val),
                "run-graph" => set_bool(&mut c.run_graph, val),
                "sidebar" => set_bool(&mut c.sidebar, val),
                "sidebar-width" => {
                    if let Ok(width) = val.parse::<u16>() {
                        c.sidebar_width = width.clamp(16, 40);
                    }
                }
                "file-sidebar" => set_bool(&mut c.file_sidebar, val),
                "file-sidebar-width" => {
                    if let Ok(width) = val.parse::<u16>() {
                        c.file_sidebar_width = width.clamp(22, 52);
                    }
                }
                "notifications" | "notification-delivery" => {
                    c.notifications = NotificationDelivery::parse(val)
                }
                "notify-completion" => set_bool(&mut c.notify_completion, val),
                "notification-sound" => c.notification_sound = NotificationSound::parse(val),
                "notification-sound-file" => c.notification_sound_file = val.trim().to_string(),
                "focus-follows-mouse" => set_bool(&mut c.focus_follows_mouse, val),
                "confirm-close" => set_bool(&mut c.confirm_close, val),
                "confirm-tab-close" => {
                    if let Some(value) = parse_bool_checked(val) {
                        confirm_tab_close = Some(value);
                    }
                }
                "window-title" => c.window_title = val.to_string(),
                "auto-trust-projects" => set_bool(&mut c.auto_trust_projects, val),
                "persist-auto-approve" => set_bool(&mut c.persist_auto_approve, val),
                "auto-track-tasks" => set_bool(&mut c.auto_track_tasks, val),
                "auto-tab-names" => set_bool(&mut c.auto_tab_names, val),
                "switcher-enabled" => set_bool(&mut c.switcher_enabled, val),
                "status-right" => c.status_right = val.to_string(),
                "pane-right-click" => set_bool(&mut c.pane_right_click, val),
                "freeze-on-select" => set_bool(&mut c.freeze_on_select, val),
                "copy-on-select" => set_bool(&mut c.copy_on_select, val),
                "guardrail-max-active-runs" => {
                    if let Ok(value) = val.parse::<u16>() {
                        if (1..=GUARDRAIL_MAX_ACTIVE_RUNS).contains(&value) {
                            c.guardrails.max_active_runs = value;
                        }
                    }
                }
                "guardrail-max-role-panes" => {
                    if let Ok(value) = val.parse::<u16>() {
                        if (1..=GUARDRAIL_MAX_ROLE_PANES).contains(&value) {
                            c.guardrails.max_role_panes = value;
                        }
                    }
                }
                "guardrail-max-iterations" => {
                    if let Ok(value) = val.parse::<u32>() {
                        if (1..=GUARDRAIL_MAX_ITERATIONS).contains(&value) {
                            c.guardrails.max_iterations = value;
                        }
                    }
                }
                "guardrail-max-elapsed-minutes" => {
                    if let Ok(value) = val.parse::<u64>() {
                        let max_minutes = GUARDRAIL_MAX_ELAPSED_SECONDS / 60;
                        if (1..=max_minutes).contains(&value) {
                            c.guardrails.max_elapsed_seconds = value * 60;
                        }
                    }
                }
                "guardrail-allowed-project" => {
                    if !val.is_empty()
                        && val.len() <= 4096
                        && !val.chars().any(char::is_control)
                        && c.guardrail_allowed_projects.len() < GUARDRAIL_MAX_PROJECT_SELECTORS
                        && !c.guardrail_allowed_projects.iter().any(|item| item == val)
                    {
                        c.guardrail_allowed_projects.push(val.to_string());
                    }
                }
                "delegation-max-answers-per-hour" => {
                    if let Ok(value) = val.parse::<u16>() {
                        if (1..=DELEGATION_MAX_ANSWERS_PER_HOUR).contains(&value) {
                            c.delegation_max_answers_per_hour = value;
                        }
                    }
                }
                "editor" if !val.is_empty() => c.editor = val.to_string(),
                key if key.starts_with("editor.") => {
                    if let Ok(rule) = EditorRule::parse(&key["editor.".len()..], val) {
                        c.editor_rules
                            .retain(|existing| existing.extension != rule.extension);
                        c.editor_rules.push(rule);
                        c.editor_rules
                            .sort_by(|left, right| left.extension.cmp(&right.extension));
                    }
                }
                key if key.starts_with("bind.") => {
                    if let Some(key) = parse_binding_key(&key["bind.".len()..]) {
                        if is_binding_action(val) {
                            c.bindings.retain(|binding| binding.key != key);
                            c.bindings.push(KeyBinding {
                                key,
                                action: val.to_ascii_lowercase(),
                            });
                            c.bindings.sort_by_key(|binding| binding.key);
                        }
                    }
                }
                "status-bg" => {
                    if let Some(col) = parse_color(val) {
                        c.theme_preset = ThemePreset::Custom;
                        c.theme.status_bg = col;
                        c.theme_overrides.status_bg = Some(col);
                    }
                }
                "status-fg" => {
                    if let Some(col) = parse_color(val) {
                        c.theme_preset = ThemePreset::Custom;
                        c.theme.status_fg = col;
                        c.theme_overrides.status_fg = Some(col);
                    }
                }
                "status-active-bg" => {
                    if let Some(col) = parse_color(val) {
                        c.theme_preset = ThemePreset::Custom;
                        c.theme.status_active_bg = col;
                        c.theme_overrides.status_active_bg = Some(col);
                    }
                }
                "status-active-fg" => {
                    if let Some(col) = parse_color(val) {
                        c.theme_preset = ThemePreset::Custom;
                        c.theme.status_active_fg = col;
                        c.theme_overrides.status_active_fg = Some(col);
                    }
                }
                _ => {} // background/foreground/font-* etc.: parsed-tolerant, unused in M5
            }
        }
        // `confirm-close` historically covered both Panes and Tabs. Preserve
        // that behavior for existing config files until the dedicated Tab
        // preference is written by Settings.
        c.confirm_tab_close = confirm_tab_close.unwrap_or(c.confirm_close);
        c
    }

    /// Validate all Uniterm-owned keys without changing the forgiving runtime
    /// parser. This intentionally diagnoses misspellings instead of silently
    /// turning them into defaults.
    pub fn diagnostics(text: &str) -> Vec<ConfigDiagnostic> {
        let mut diagnostics = Vec::new();
        let mut allowed_project_count = 0usize;
        for (index, raw) in text.lines().enumerate() {
            let line_number = index + 1;
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, raw_value)) = line.split_once('=') else {
                diagnostics.push(ConfigDiagnostic {
                    line: line_number,
                    message: "expected `key = value`".into(),
                });
                continue;
            };
            let key = key.trim();
            let value = config_value(raw_value);
            let value = value.as_str();
            if key == "guardrail-allowed-project" {
                allowed_project_count += 1;
            }
            let problem = match key {
                "prefix" if parse_key(value).is_none() => Some("expected a key like C-a".into()),
                "status"
                | "restore"
                | "auto-update-connectors"
                | "auto-install-connectors"
                | "auto-trust-projects"
                | "follow-omarchy-theme"
                | "persist-auto-approve"
                | "auto-track-tasks"
                | "auto-tab-names"
                | "switcher-enabled"
                | "autosave"
                | "run-graph"
                | "sidebar"
                | "file-sidebar"
                | "notify-completion"
                | "focus-follows-mouse"
                | "confirm-close"
                | "confirm-tab-close"
                | "pane-right-click"
                | "freeze-on-select"
                | "copy-on-select"
                    if parse_bool_checked(value).is_none() =>
                {
                    Some("expected true/false, on/off, yes/no, or 1/0".into())
                }
                "delegation-max-answers-per-hour"
                    if !matches!(
                        value.parse::<u16>(),
                        Ok(value) if (1..=DELEGATION_MAX_ANSWERS_PER_HOUR).contains(&value)
                    ) =>
                {
                    Some(format!(
                        "expected an integer from 1 to {DELEGATION_MAX_ANSWERS_PER_HOUR}"
                    ))
                }
                "tab-label-alignment" if TabLabelAlignment::parse(value).is_none() => {
                    Some("expected left, center, or right".into())
                }
                "status-position"
                    if !value.eq_ignore_ascii_case("top")
                        && !value.eq_ignore_ascii_case("bottom") =>
                {
                    Some("expected top or bottom".into())
                }
                "scrollback-limit" if value.parse::<usize>().is_err() => {
                    Some("expected a non-negative integer".into())
                }
                "sidebar-width" | "file-sidebar-width" if value.parse::<u16>().is_err() => {
                    Some("expected an integer width".into())
                }
                "guardrail-max-active-runs"
                    if !matches!(
                        value.parse::<u16>(),
                        Ok(value) if (1..=GUARDRAIL_MAX_ACTIVE_RUNS).contains(&value)
                    ) =>
                {
                    Some(format!(
                        "expected an integer from 1 to {GUARDRAIL_MAX_ACTIVE_RUNS}"
                    ))
                }
                "guardrail-max-role-panes"
                    if !matches!(
                        value.parse::<u16>(),
                        Ok(value) if (1..=GUARDRAIL_MAX_ROLE_PANES).contains(&value)
                    ) =>
                {
                    Some(format!(
                        "expected an integer from 1 to {GUARDRAIL_MAX_ROLE_PANES}"
                    ))
                }
                "guardrail-max-iterations"
                    if !matches!(
                        value.parse::<u32>(),
                        Ok(value) if (1..=GUARDRAIL_MAX_ITERATIONS).contains(&value)
                    ) =>
                {
                    Some(format!(
                        "expected an integer from 1 to {GUARDRAIL_MAX_ITERATIONS}"
                    ))
                }
                "guardrail-max-elapsed-minutes"
                    if !matches!(
                        value.parse::<u64>(),
                        Ok(value)
                            if (1..=GUARDRAIL_MAX_ELAPSED_SECONDS / 60).contains(&value)
                    ) =>
                {
                    Some(format!(
                        "expected an integer from 1 to {}",
                        GUARDRAIL_MAX_ELAPSED_SECONDS / 60
                    ))
                }
                "guardrail-allowed-project"
                    if value.is_empty()
                        || value.len() > 4096
                        || value.chars().any(char::is_control) =>
                {
                    Some("expected a non-empty Project name or canonical root".into())
                }
                "guardrail-allowed-project"
                    if allowed_project_count > GUARDRAIL_MAX_PROJECT_SELECTORS =>
                {
                    Some(format!(
                        "at most {GUARDRAIL_MAX_PROJECT_SELECTORS} allowed Projects may be configured"
                    ))
                }
                "theme" if !is_theme_name(value) => Some(format!("unknown theme '{value}'")),
                "notifications" | "notification-delivery"
                    if !matches!(
                        value.to_ascii_lowercase().as_str(),
                        "off" | "uniterm" | "inline" | "terminal" | "system" | "os" | "native"
                    ) =>
                {
                    Some("expected off, uniterm, terminal, or system".into())
                }
                "notification-sound"
                    if !matches!(
                        value.to_ascii_lowercase().as_str(),
                        "off" | "bell" | "beep" | "chime" | "tone" | "file" | "custom"
                    ) =>
                {
                    Some("expected off, bell, chime, or file".into())
                }
                "status-bg" | "status-fg" | "status-active-bg" | "status-active-fg"
                    if parse_color(value).is_none() =>
                {
                    Some("expected #rrggbb or a palette index from 0 to 255".into())
                }
                "editor" if value.is_empty() => Some("editor command cannot be empty".into()),
                key if key.starts_with("editor.") => {
                    EditorRule::parse(&key["editor.".len()..], value).err()
                }
                key if key.starts_with("bind.") => {
                    let key = &key["bind.".len()..];
                    if parse_binding_key(key).is_none() {
                        Some(format!("invalid binding key '{key}'"))
                    } else if !is_binding_action(value) {
                        Some(format!("unknown binding action '{value}'"))
                    } else {
                        None
                    }
                }
                "prefix"
                | "status"
                | "restore"
                | "auto-update-connectors"
                | "auto-install-connectors"
                | "autosave"
                | "status-position"
                | "tab-label-alignment"
                | "scrollback-limit"
                | "theme"
                | "run-graph"
                | "sidebar"
                | "sidebar-width"
                | "file-sidebar"
                | "file-sidebar-width"
                | "notifications"
                | "notification-delivery"
                | "notify-completion"
                | "notification-sound"
                | "notification-sound-file"
                | "focus-follows-mouse"
                | "confirm-close"
                | "confirm-tab-close"
                | "auto-trust-projects"
                | "follow-omarchy-theme"
                | "persist-auto-approve"
                | "auto-track-tasks"
                | "auto-tab-names"
                | "switcher-enabled"
                | "window-title"
                | "status-right"
                | "pane-right-click"
                | "freeze-on-select"
                | "copy-on-select"
                | "guardrail-max-active-runs"
                | "guardrail-max-role-panes"
                | "guardrail-max-iterations"
                | "guardrail-max-elapsed-minutes"
                | "guardrail-allowed-project"
                | "delegation-max-answers-per-hour"
                | "editor"
                | "status-bg"
                | "status-fg"
                | "status-active-bg"
                | "status-active-fg"
                | "default-workspace" => None,
                _ => Some(format!("unknown key '{key}'")),
            };
            if let Some(message) = problem {
                diagnostics.push(ConfigDiagnostic {
                    line: line_number,
                    message,
                });
            }
        }
        diagnostics
    }

    /// Canonical config text used by the runtime's atomic settings writer.
    pub fn to_text(&self) -> String {
        let mut text = format!(
            "# Uniterm configuration\n\
theme = {}\n\
follow-omarchy-theme = {}\n\
prefix = C-{}\n\
status = {}\n\
status-position = {}\n\
tab-label-alignment = {}\n\
run-graph = {}\n\
sidebar = {}\n\
sidebar-width = {}\n\
file-sidebar = {}\n\
file-sidebar-width = {}\n\
notification-delivery = {}\n\
notify-completion = {}\n\
notification-sound = {}\n\
notification-sound-file = {}\n\
focus-follows-mouse = {}\n\
confirm-close = {}\n\
confirm-tab-close = {}\n\
auto-trust-projects = {}\n\
persist-auto-approve = {}\n\
auto-track-tasks = {}\n\
auto-tab-names = {}\n\
switcher-enabled = {}\n\
window-title = {}\n\
status-right = {}\n\
pane-right-click = {}\n\
freeze-on-select = {}\n\
copy-on-select = {}\n\
guardrail-max-active-runs = {}\n\
guardrail-max-role-panes = {}\n\
guardrail-max-iterations = {}\n\
guardrail-max-elapsed-minutes = {}\n\
delegation-max-answers-per-hour = {}\n\
scrollback-limit = {}\n\
restore = {}\n\
auto-update-connectors = {}\n\
auto-install-connectors = {}\n\
editor = {}\n",
            if self.theme_preset == ThemePreset::Custom {
                self.theme_base_preset.name()
            } else {
                self.theme_preset.name()
            },
            bool_name(self.follow_omarchy_theme),
            ((self.prefix | 0x60) as char),
            bool_name(self.status),
            if self.status_position == StatusPosition::Top {
                "top"
            } else {
                "bottom"
            },
            self.tab_label_alignment.name(),
            bool_name(self.run_graph),
            bool_name(self.sidebar),
            self.sidebar_width,
            bool_name(self.file_sidebar),
            self.file_sidebar_width,
            self.notifications.name(),
            bool_name(self.notify_completion),
            self.notification_sound.name(),
            self.notification_sound_file,
            bool_name(self.focus_follows_mouse),
            bool_name(self.confirm_close),
            bool_name(self.confirm_tab_close),
            bool_name(self.auto_trust_projects),
            bool_name(self.persist_auto_approve),
            bool_name(self.auto_track_tasks),
            bool_name(self.auto_tab_names),
            bool_name(self.switcher_enabled),
            config_value_text(&self.window_title),
            config_value_text(&self.status_right),
            bool_name(self.pane_right_click),
            bool_name(self.freeze_on_select),
            bool_name(self.copy_on_select),
            self.guardrails.max_active_runs,
            self.guardrails.max_role_panes,
            self.guardrails.max_iterations,
            self.guardrails.max_elapsed_seconds / 60,
            self.delegation_max_answers_per_hour,
            self.scrollback_limit,
            bool_name(self.restore),
            bool_name(self.auto_update_connectors),
            bool_name(self.auto_install_connectors),
            config_value_text(&self.editor),
        );
        for (key, color) in [
            ("status-bg", self.theme_overrides.status_bg),
            ("status-fg", self.theme_overrides.status_fg),
            ("status-active-bg", self.theme_overrides.status_active_bg),
            ("status-active-fg", self.theme_overrides.status_active_fg),
        ] {
            match color {
                Some(Color::Rgb(r, g, b)) => {
                    text.push_str(&format!("{key} = #{r:02x}{g:02x}{b:02x}\n"))
                }
                Some(Color::Idx(index)) => text.push_str(&format!("{key} = {index}\n")),
                Some(Color::Default) | None => {}
            }
        }
        for selector in &self.guardrail_allowed_projects {
            text.push_str(&format!(
                "guardrail-allowed-project = {}\n",
                config_value_text(selector)
            ));
        }
        for rule in &self.editor_rules {
            text.push_str(&format!(
                "editor.{} = {}\n",
                rule.extension,
                config_value_text(&rule.command)
            ));
        }
        for binding in &self.bindings {
            text.push_str(&format!(
                "bind.{} = {}\n",
                binding_key_name(binding.key),
                binding.action
            ));
        }
        text
    }

    /// The delegation policy this config permits: the hourly auto-answer
    /// budget, always bounded.
    pub fn delegation_policy(&self) -> DelegationPolicy {
        DelegationPolicy {
            max_answers_per_hour: self.delegation_max_answers_per_hour,
        }
    }

    /// Parse the Settings surface's semicolon-separated exact Project
    /// selectors. An empty value deliberately restores the allow-all default.
    pub fn parse_guardrail_allowed_projects(value: &str) -> Result<Vec<String>, String> {
        let mut selectors = Vec::new();
        for selector in value
            .split(';')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            if selector.len() > 4096 || selector.chars().any(char::is_control) {
                return Err("Allowed Projects must be names or roots of at most 4096 bytes".into());
            }
            if !selectors.iter().any(|existing| existing == selector) {
                if selectors.len() == GUARDRAIL_MAX_PROJECT_SELECTORS {
                    return Err(format!(
                        "At most {GUARDRAIL_MAX_PROJECT_SELECTORS} allowed Projects may be configured"
                    ));
                }
                selectors.push(selector.to_string());
            }
        }
        Ok(selectors)
    }

    /// Canonical one-line value presented by the Settings text editor.
    pub fn guardrail_allowed_projects_text(&self) -> String {
        self.guardrail_allowed_projects.join("; ")
    }

    /// Parse the Settings surface's `ext=command; ...` shorthand.
    pub fn parse_editor_rules(value: &str) -> Result<Vec<EditorRule>, String> {
        let mut rules = Vec::new();
        for assignment in value
            .split(';')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            let Some((extension, command)) = assignment.split_once('=') else {
                return Err(format!("expected extension=command, got '{assignment}'"));
            };
            let rule = EditorRule::parse(extension, command)?;
            if rules
                .iter()
                .any(|existing: &EditorRule| existing.extension == rule.extension)
            {
                return Err(format!("duplicate editor for .{}", rule.extension));
            }
            rules.push(rule);
        }
        if rules.len() > 64 {
            return Err("at most 64 file editor overrides are allowed".into());
        }
        rules.sort_by(|left, right| left.extension.cmp(&right.extension));
        Ok(rules)
    }

    /// Render extension overrides for the single-line Settings editor.
    pub fn editor_rules_text(&self) -> String {
        self.editor_rules
            .iter()
            .map(|rule| format!("{}={}", rule.extension, rule.command))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Select an extension override or fall back to the catch-all editor.
    pub fn editor_for_path(&self, path: &str) -> &str {
        let extension = std::path::Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        extension
            .as_deref()
            .and_then(|extension| {
                self.editor_rules
                    .iter()
                    .find(|rule| rule.extension == extension)
            })
            .map_or(self.editor.as_str(), |rule| rule.command.as_str())
    }
}

fn set_bool(target: &mut bool, value: &str) {
    if let Some(value) = parse_bool_checked(value) {
        *target = value;
    }
}

fn parse_bool_checked(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn is_theme_name(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    ThemePreset::ALL.iter().any(|theme| theme.name() == value)
        || matches!(
            value.as_str(),
            "light"
                | "catppuccin-mocha"
                | "tokyonight"
                | "gruvbox"
                | "solarized"
                | "rosepine"
                | "custom"
        )
}

fn strip_inline_comment(value: &str) -> &str {
    let bytes = value.as_bytes();
    let mut in_quotes = false;
    let mut index = 0;
    while index + 1 < bytes.len() {
        let byte = bytes[index];
        // Inside quotes, `\"` is an escaped quote, not a toggle.
        if in_quotes && byte == b'\\' {
            index += 2;
            continue;
        }
        if byte == b'"' {
            in_quotes = !in_quotes;
        }
        if !in_quotes && byte.is_ascii_whitespace() && bytes[index + 1] == b'#' {
            return &value[..index];
        }
        index += 1;
    }
    value
}

/// Extract the effective value from the right-hand side of a `key = value`
/// line. Two forms exist: the serializer's canonical quoted form (one
/// double-quoted string, recognizing the `\\` and `\"` escapes
/// `config_value_text` writes) and a verbatim form for hand-written values
/// like `sh -c "echo #"` whose quotes are shell syntax the editor command
/// runner must see. The verbatim form only strips the inline comment, and
/// the comment scan respects quoted regions so a `#` inside quotes is kept.
/// Parser and validator share this so `ut config check` judges exactly what
/// the runtime sees.
fn config_value(raw: &str) -> String {
    let raw = raw.trim();
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        // The final quote closes the value only when it is not itself
        // escaped, i.e. preceded by an even-length run of backslashes.
        let bytes = raw.as_bytes();
        let mut backslashes = 0usize;
        let mut look = bytes.len() - 1;
        while look > 0 && bytes[look - 1] == b'\\' {
            backslashes += 1;
            look -= 1;
        }
        if backslashes.is_multiple_of(2) {
            return unescape_quoted(&raw[1..raw.len() - 1]);
        }
    }
    strip_inline_comment(raw).trim().to_string()
}

/// Undo the two escapes the serializer's quoted form may contain. Any other
/// backslash is literal, so a quoted hand-written path like
/// `"C:\tools\e.exe"` survives.
fn unescape_quoted(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Serialize a free-text config value, quoting (and escaping) when it
/// contains a `#` that would otherwise be truncated as an inline comment on
/// the next load.
fn config_value_text(value: &str) -> String {
    if !value.contains('#') {
        return value.to_string();
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

const fn bool_name(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

/// Parse a key spec like `C-a`, `C-b`, or a single char, to its control byte.
fn parse_key(s: &str) -> Option<u8> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("C-").or_else(|| s.strip_prefix("c-")) {
        let ch = rest.chars().next()?;
        if ch.is_ascii_alphabetic() {
            // Ctrl-<letter> = letter & 0x1f
            return Some((ch.to_ascii_lowercase() as u8) & 0x1f);
        }
    }
    None
}

fn parse_binding_key(value: &str) -> Option<u8> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("space") {
        return Some(b' ');
    }
    if value.len() == 1 && value.as_bytes()[0].is_ascii() {
        return Some(value.as_bytes()[0]);
    }
    parse_key(value)
}

fn binding_key_name(key: u8) -> String {
    if key == b' ' {
        "space".into()
    } else if key < 0x20 {
        format!("C-{}", (key | 0x60) as char)
    } else {
        char::from(key).to_string()
    }
}

fn is_binding_action(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "none"
            | "detach"
            | "split-right"
            | "split-down"
            | "focus-left"
            | "focus-right"
            | "focus-up"
            | "focus-down"
            | "resize-left"
            | "resize-right"
            | "resize-up"
            | "resize-down"
            | "zoom"
            | "kill-pane"
            | "new-tab"
            | "next-tab"
            | "previous-tab"
            | "move-tab-left"
            | "servers"
            | "local"
            | "today"
            | "run-graph"
            | "move-tab-right"
            | "kill-tab"
            | "overview"
            | "copy-mode"
            | "files"
            | "sidebar"
            | "observatory"
            | "new-task"
            | "tasks"
            | "agents"
            | "switcher"
            | "rename-tab"
            | "rename-workspace"
            | "menu"
            | "workspaces"
            | "settings"
            | "projects"
            | "new-project"
            | "close-workspace"
    )
}

/// Parse a colour: `#rrggbb` (true colour) or a bare 0-255 palette index.
fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }
    s.parse::<u8>().ok().map(Color::Idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omarchy_following_defaults_off_and_roundtrips_without_changing_manual_theme() {
        assert!(!Config::default().follow_omarchy_theme);
        for enabled in [false, true] {
            let text = format!("theme = nord\nfollow-omarchy-theme = {enabled}\n");
            let config = Config::parse(&text);
            assert_eq!(config.follow_omarchy_theme, enabled);
            assert_eq!(config.theme, Theme::named("nord"));
            assert!(Config::diagnostics(&text).is_empty());
            assert_eq!(Config::parse(&config.to_text()), config);
        }
        assert_eq!(
            Config::diagnostics("follow-omarchy-theme = maybe\n").len(),
            1
        );
        assert!(!Config::parse("follow-omarchy-theme = maybe\n").follow_omarchy_theme);
    }

    #[test]
    fn explicit_theme_overrides_survive_external_palettes_and_unrelated_settings_saves() {
        let manual = Config::parse(
            "theme = dracula\nfollow-omarchy-theme = true\nstatus-bg = #010203\nstatus-fg = 250\nstatus-active-bg = 12\nstatus-active-fg = #aabbcc\n",
        );
        assert_eq!(manual.theme_preset, ThemePreset::Custom);
        assert_eq!(manual.theme_base_preset, ThemePreset::Dracula);
        let applied = manual.apply_theme_overrides(Theme::light());
        assert_eq!(applied.background, Theme::light().background);
        assert_eq!(applied.status_bg, Color::Rgb(1, 2, 3));
        assert_eq!(applied.status_fg, Color::Idx(250));
        assert_eq!(applied.status_active_bg, Color::Idx(12));
        assert_eq!(applied.status_active_fg, Color::Rgb(170, 187, 204));
        let mut live = manual.clone();
        live.theme = applied;
        let restored = Config::parse(&live.to_text());
        assert_eq!(
            restored, manual,
            "an observed palette must never replace the saved manual base"
        );
        assert!(Config::diagnostics(&live.to_text()).is_empty());
        live.select_theme("nord");
        assert_eq!(live.theme_preset, ThemePreset::Custom);
        assert_eq!(live.theme_base_preset, ThemePreset::Nord);
        assert_eq!(live.theme.background, Theme::named("nord").background);
        assert_eq!(live.theme.status_bg, manual.theme.status_bg);
        assert_eq!(Config::parse(&live.to_text()), live);
    }

    #[test]
    fn later_manual_theme_lines_keep_existing_override_reset_semantics() {
        let config = Config::parse("theme = nord\nstatus-bg = #123456\ntheme = dracula\n");
        assert_eq!(config.theme_preset, ThemePreset::Dracula);
        assert_eq!(config.theme_base_preset, ThemePreset::Dracula);
        assert_eq!(config.theme_overrides, ThemeOverrides::default());
        assert_eq!(config.theme, Theme::named("dracula"));
        assert_eq!(Config::parse(&config.to_text()), config);
        assert!(Config::diagnostics("status-bg = #aébcd\n").len() == 1);
        assert_eq!(Config::parse("status-bg = #aébcd\n"), Config::default());
    }

    #[test]
    fn automatic_task_tracking_defaults_on_and_roundtrips() {
        assert!(Config::default().auto_track_tasks);
        for enabled in [false, true] {
            let config = Config::parse(&format!("auto-track-tasks = {enabled}\n"));
            assert_eq!(config.auto_track_tasks, enabled);
            assert_eq!(Config::parse(&config.to_text()).auto_track_tasks, enabled);
        }
        assert_eq!(Config::diagnostics("auto-track-tasks = maybe\n").len(), 1);
    }

    #[test]
    fn run_graph_is_opt_in_and_round_trips() {
        assert!(!Config::default().run_graph);
        let config = Config::parse("run-graph = true\n");
        assert!(config.run_graph);
        assert!(Config::parse(&config.to_text()).run_graph);
        assert!(Config::diagnostics("run-graph = true\n").is_empty());
        assert_eq!(Config::diagnostics("run-graph = invalid\n").len(), 1);
    }

    #[test]
    fn connector_policies_default_on_and_round_trip_independently() {
        assert!(Config::default().auto_update_connectors);
        assert!(Config::default().auto_install_connectors);
        for update in [false, true] {
            for install in [false, true] {
                let text = format!(
                    "auto-update-connectors = {update}\nauto-install-connectors = {install}\n"
                );
                let config = Config::parse(&text);
                assert_eq!(config.auto_update_connectors, update);
                assert_eq!(config.auto_install_connectors, install);
                assert_eq!(Config::parse(&config.to_text()), config);
                assert!(Config::diagnostics(&text).is_empty());
            }
        }
        for key in ["auto-update-connectors", "auto-install-connectors"] {
            let invalid = format!("{key} = maybe\n");
            assert_eq!(Config::diagnostics(&invalid).len(), 1);
            assert!(Config::parse(&invalid).auto_update_connectors);
            assert!(Config::parse(&invalid).auto_install_connectors);
        }
    }

    #[test]
    fn navigation_options_are_independent_and_default_off() {
        let defaults = Config::default();
        assert!(!defaults.auto_tab_names && !defaults.switcher_enabled);
        for names in [false, true] {
            for switcher in [false, true] {
                let text = format!("auto-tab-names = {names}\nswitcher-enabled = {switcher}\n");
                let config = Config::parse(&text);
                assert_eq!(config.auto_tab_names, names);
                assert_eq!(config.switcher_enabled, switcher);
                assert_eq!(Config::parse(&config.to_text()), config);
                assert!(Config::diagnostics(&text).is_empty());
            }
        }
        assert_eq!(
            Config::diagnostics("auto-tab-names = maybe\nswitcher-enabled = maybe\n").len(),
            2
        );
    }

    #[test]
    fn tab_label_alignment_defaults_roundtrips_and_validates() {
        assert_eq!(
            Config::default().tab_label_alignment,
            TabLabelAlignment::Center
        );
        assert_eq!(
            Config::parse("").tab_label_alignment,
            TabLabelAlignment::Center
        );
        for alignment in TabLabelAlignment::ALL {
            for value in [
                alignment.name().to_string(),
                alignment.name().to_uppercase(),
            ] {
                let text = format!("tab-label-alignment = {value}\n");
                let config = Config::parse(&text);
                assert_eq!(config.tab_label_alignment, *alignment);
                assert_eq!(Config::parse(&config.to_text()), config);
                assert!(config
                    .to_text()
                    .contains(&format!("tab-label-alignment = {}\n", alignment.name())));
                assert!(Config::diagnostics(&text).is_empty());
            }
        }
        for value in ["", "middle", "left right", "0"] {
            let text = format!("tab-label-alignment = {value}\n");
            assert_eq!(
                Config::parse(&text).tab_label_alignment,
                TabLabelAlignment::Center
            );
            let errors = Config::diagnostics(&text);
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].message, "expected left, center, or right");
        }
        assert_eq!(
            Config::parse("tab-label-alignment = right\ntab-label-alignment = invalid\n")
                .tab_label_alignment,
            TabLabelAlignment::Right
        );
    }

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.prefix, 0x01);
        assert!(c.status);
        assert_eq!(c.status_position, StatusPosition::Top);
        assert!(c.sidebar);
        assert!(c.file_sidebar);
        assert_eq!(c.file_sidebar_width, 36);
        assert!(c.confirm_close);
        assert!(c.confirm_tab_close);
    }

    #[test]
    fn parses_ghostty_style() {
        let text = "\
# a comment
theme = light
prefix = C-b
status = off
status-position = top
scrollback-limit = 5000
";
        let c = Config::parse(text);
        assert_eq!(c.prefix, 0x02); // Ctrl-B
        assert!(!c.status);
        assert_eq!(c.status_position, StatusPosition::Top);
        assert_eq!(c.scrollback_limit, 5000);
        assert_eq!(c.theme, Theme::light());
    }

    #[test]
    fn diagnostics_report_every_bad_line_without_changing_runtime_tolerance() {
        let text = "statuz = on\nstatus = perhaps\ntheme = midnight\nstatus-bg = #abc\nbroken\n";
        let diagnostics = Config::diagnostics(text);
        assert_eq!(
            diagnostics.iter().map(|item| item.line).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert!(Config::parse(text).status);
    }

    #[test]
    fn diagnostics_accept_canonical_output() {
        assert!(Config::diagnostics(&Config::default().to_text()).is_empty());
    }

    #[test]
    fn guardrail_limits_and_exact_project_selectors_round_trip() {
        let text = "guardrail-max-active-runs = 4\n\
guardrail-max-role-panes = 12\n\
guardrail-max-iterations = 7\n\
guardrail-max-elapsed-minutes = 45\n\
guardrail-allowed-project = api\n\
guardrail-allowed-project = /work/api\n\
guardrail-allowed-project = api\n";
        let config = Config::parse(text);
        assert_eq!(config.guardrails.max_active_runs, 4);
        assert_eq!(config.guardrails.max_role_panes, 12);
        assert_eq!(config.guardrails.max_iterations, 7);
        assert_eq!(config.guardrails.max_elapsed_seconds, 45 * 60);
        assert_eq!(
            config.guardrail_allowed_projects,
            vec!["api".to_string(), "/work/api".to_string()]
        );
        assert!(Config::diagnostics(&config.to_text()).is_empty());
        assert_eq!(Config::parse(&config.to_text()), config);
    }

    #[test]
    fn delegation_budget_defaults_parses_and_round_trips() {
        let config = Config::default();
        assert_eq!(
            config.delegation_max_answers_per_hour,
            DELEGATION_DEFAULT_ANSWERS_PER_HOUR
        );
        assert!(config.delegation_policy().validate().is_ok());
        let config = Config::parse("delegation-max-answers-per-hour = 12\n");
        assert_eq!(config.delegation_max_answers_per_hour, 12);
        assert_eq!(config.delegation_policy().max_answers_per_hour, 12);
        assert!(Config::diagnostics(&config.to_text()).is_empty());
        assert_eq!(Config::parse(&config.to_text()), config);
    }

    #[test]
    fn invalid_delegation_budget_fails_closed_and_is_diagnosed() {
        let text = "delegation-max-answers-per-hour = 0\ndelegation-max-answers-per-hour = 241\n";
        let config = Config::parse(text);
        assert_eq!(
            config.delegation_max_answers_per_hour,
            DELEGATION_DEFAULT_ANSWERS_PER_HOUR
        );
        let diagnostics = Config::diagnostics(text);
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.message.contains("expected an integer from 1 to")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn removed_delegation_keys_are_diagnosed_as_unknown() {
        // The retired allowlist key is diagnosed, not silently ignored: a
        // config that once gated approvals must visibly say it no longer
        // does.
        let key = "delegation-allow = cargo test\n";
        let diagnostics = Config::diagnostics(key);
        assert_eq!(diagnostics.len(), 1, "{key}");
        assert!(
            diagnostics[0].message.contains("unknown key"),
            "{diagnostics:?}"
        );
        // The forgiving runtime parser ignores it, leaving defaults.
        assert_eq!(Config::parse(key), Config::default());
        let retired = "delegation-auto-arm = on\n";
        assert_eq!(Config::parse(retired), Config::default());
        assert!(Config::diagnostics(retired)[0]
            .message
            .contains("unknown key"));
    }

    #[test]
    fn invalid_guardrail_values_fail_closed_to_defaults_and_are_diagnosed() {
        let text = "guardrail-max-active-runs = 0\n\
guardrail-max-role-panes = 999\n\
guardrail-max-iterations = nope\n\
guardrail-max-elapsed-minutes = 10081\n\
guardrail-allowed-project = \n";
        let config = Config::parse(text);
        assert_eq!(config.guardrails, GuardLimits::default());
        assert!(config.guardrail_allowed_projects.is_empty());
        assert_eq!(Config::diagnostics(text).len(), 5);
    }

    #[test]
    fn settings_project_selector_text_is_bounded_deduplicated_and_clearable() {
        assert_eq!(
            Config::parse_guardrail_allowed_projects(" api; /work/web ; api ").unwrap(),
            vec!["api".to_string(), "/work/web".to_string()]
        );
        assert!(Config::parse_guardrail_allowed_projects("")
            .unwrap()
            .is_empty());
        assert!(Config::parse_guardrail_allowed_projects("api; bad\nroot").is_err());

        let config = Config {
            guardrail_allowed_projects: vec!["api".into(), "/work/web".into()],
            ..Config::default()
        };
        assert_eq!(config.guardrail_allowed_projects_text(), "api; /work/web");
    }

    #[test]
    fn quoted_hash_values_survive_parse_and_settings_rewrite() {
        let config =
            Config::parse("window-title = \"{workspace} # prod\"\nstatus-right = {zoom}\n");
        assert_eq!(config.window_title, "{workspace} # prod");
        assert!(Config::diagnostics(&config.to_text()).is_empty());
        let reparsed = Config::parse(&config.to_text());
        assert_eq!(reparsed.window_title, "{workspace} # prod");
        assert_eq!(reparsed.status_right, "{zoom}");
    }

    #[test]
    fn quoted_hash_values_with_inner_quotes_survive_a_rewrite() {
        // The shape the review called out: a shell one-liner carrying both a
        // double quote and a `#`. It must round-trip through to_text and the
        // parser without being truncated at the inner ` #`.
        let config = Config::parse("editor = sh -c \"echo #\"\n");
        assert_eq!(config.editor, "sh -c \"echo #\"");
        let text = config.to_text();
        assert!(
            text.contains("editor = \"sh -c \\\"echo #\\\"\""),
            "editor line should be quoted and escaped, got: {text}"
        );
        let reparsed = Config::parse(&text);
        assert_eq!(reparsed.editor, "sh -c \"echo #\"");
        assert!(Config::diagnostics(&text).is_empty());
        // Literal backslashes in hand-written quoted values are not eaten.
        let config = Config::parse("editor = \"C:\\tools\\e.exe --flag # keep\"\n");
        assert_eq!(config.editor, "C:\\tools\\e.exe --flag # keep");
        let reparsed = Config::parse(&config.to_text());
        assert_eq!(reparsed.editor, "C:\\tools\\e.exe --flag # keep");
    }

    #[test]
    fn status_position_is_case_insensitive_in_both_parser_and_diagnostics() {
        let text = "status-position = TOP\n";
        assert_eq!(Config::parse(text).status_position, StatusPosition::Top);
        assert!(Config::diagnostics(text).is_empty());
    }

    #[test]
    fn semantic_bindings_override_by_key_and_round_trip() {
        let config =
            Config::parse("bind.r = move-tab-right\nbind.r = rename-tab\nbind.space = none\n");
        assert_eq!(
            config.bindings,
            vec![
                KeyBinding {
                    key: b' ',
                    action: "none".into()
                },
                KeyBinding {
                    key: b'r',
                    action: "rename-tab".into()
                }
            ]
        );
        assert_eq!(Config::parse(&config.to_text()).bindings, config.bindings);
    }

    #[test]
    fn status_defaults_top_and_allows_an_explicit_bottom() {
        assert_eq!(Config::default().status_position, StatusPosition::Top);
        assert_eq!(
            Config::parse("status-position = bottom\n").status_position,
            StatusPosition::Bottom
        );
    }

    #[test]
    fn project_trust_defaults_on_roundtrips_and_validates_independently() {
        assert!(Config::default().auto_trust_projects);
        for enabled in [true, false] {
            let text = format!("auto-trust-projects = {enabled}\n");
            assert!(Config::diagnostics(&text).is_empty());
            let config = Config::parse(&text);
            assert_eq!(config.auto_trust_projects, enabled);
            assert!(!config.persist_auto_approve);
            assert_eq!(
                Config::parse(&config.to_text()).auto_trust_projects,
                enabled
            );
        }
        assert_eq!(
            Config::diagnostics("auto-trust-projects = maybe\n").len(),
            1
        );
    }

    #[test]
    fn approval_persistence_is_explicit_and_round_trips() {
        assert!(!Config::default().persist_auto_approve);
        for enabled in [false, true] {
            let text = format!("persist-auto-approve = {enabled}\n");
            assert!(Config::diagnostics(&text).is_empty());
            let config = Config::parse(&text);
            assert_eq!(config.persist_auto_approve, enabled);
            assert_eq!(
                Config::parse(&config.to_text()).persist_auto_approve,
                enabled
            );
        }
        assert_eq!(
            Config::diagnostics("persist-auto-approve = maybe\n").len(),
            1
        );
    }

    #[test]
    fn restore_defaults_on_and_is_configurable() {
        assert!(Config::default().restore); // resurrect on by default
        assert!(!Config::parse("restore = false").restore);
        assert!(!Config::parse("autosave = off").restore); // alias
        assert!(Config::parse("restore = true").restore);
    }

    #[test]
    fn freeze_on_select_defaults_off_and_round_trips() {
        assert!(!Config::default().freeze_on_select);
        assert!(Config::parse("freeze-on-select = true\n").freeze_on_select);
        assert!(!Config::parse("freeze-on-select = off\n").freeze_on_select);
        let reparsed = Config::parse(&Config::parse("freeze-on-select = yes\n").to_text());
        assert!(reparsed.freeze_on_select);
        assert_eq!(
            Config::diagnostics("freeze-on-select = sometimes\n")
                .iter()
                .map(|item| item.line)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn copy_on_select_defaults_on_and_round_trips() {
        assert!(Config::default().copy_on_select);
        assert!(!Config::parse("copy-on-select = false\n").copy_on_select);
        let reparsed = Config::parse(&Config::parse("copy-on-select = no\n").to_text());
        assert!(!reparsed.copy_on_select);
        assert_eq!(Config::diagnostics("copy-on-select = maybe\n").len(), 1);
    }

    #[test]
    fn tab_close_confirmation_is_independent_and_legacy_compatible() {
        let legacy = Config::parse("confirm-close = false\n");
        assert!(!legacy.confirm_close);
        assert!(!legacy.confirm_tab_close);

        let config = Config::parse("confirm-close = false\nconfirm-tab-close = true\n");
        assert!(!config.confirm_close);
        assert!(config.confirm_tab_close);

        let reparsed = Config::parse(&config.to_text());
        assert_eq!(reparsed.confirm_close, config.confirm_close);
        assert_eq!(reparsed.confirm_tab_close, config.confirm_tab_close);
    }

    #[test]
    fn parses_hex_and_index_colors() {
        let c = Config::parse("status-bg = #1e1e2e\nstatus-fg = 250\n");
        // A hex colour is carried as true colour; a bare index is taken as-is.
        assert_eq!(c.theme.status_bg, Color::Rgb(0x1e, 0x1e, 0x2e));
        assert_eq!(c.theme.status_fg, Color::Idx(250));
    }

    #[test]
    fn inline_comments_do_not_conflict_with_hex_colours() {
        let config = Config::parse("theme = dracula # personal choice\nstatus-bg = #1e1e2e\n");
        assert_eq!(config.theme_preset, ThemePreset::Custom);
        assert_eq!(config.theme.status_bg, Color::Rgb(0x1e, 0x1e, 0x2e));
        assert_eq!(config.theme.foreground, Theme::named("dracula").foreground);
    }

    #[test]
    fn unknown_keys_ignored() {
        // background/font-* are parsed-tolerantly and don't change M5 defaults.
        let c = Config::parse("font-family = \"Fira\"\nbackground = #000000\nbogus\n");
        assert_eq!(c, Config::default());
    }

    #[test]
    fn omarchy_catalog_names_validate_and_round_trip_without_overriding_defaults() {
        assert_eq!(Config::default().theme_preset, ThemePreset::UnitermDark);
        assert_eq!(ThemePreset::ALL.len(), 190);
        let mut previous = "";
        for &(name, _) in omarchy::PALETTES {
            assert!(name > previous, "catalog must be sorted and unique: {name}");
            previous = name;
            assert!(name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
            let text = format!("theme = {name}\n");
            assert!(Config::diagnostics(&text).is_empty(), "{name}");
            let config = Config::parse(&text);
            assert!(matches!(config.theme_preset, ThemePreset::Omarchy(_)));
            assert_eq!(config.theme_preset.name(), name);
            assert_eq!(Config::parse(&config.to_text()).theme, config.theme);
            assert_eq!(
                Config::parse(&config.to_text()).theme_preset,
                config.theme_preset
            );
            assert_eq!(
                Theme::named(&format!(" {} ", name.to_uppercase())),
                config.theme
            );
        }
        assert_eq!(Theme::named("omarchy-missing"), Theme::dark());
        assert_eq!(ThemePreset::parse("nord"), ThemePreset::Nord);
        assert!(!Config::diagnostics("theme = omarchy-missing\n").is_empty());
    }

    #[test]
    fn omarchy_palettes_preserve_upstream_roles_and_local_minimal() {
        let nord = Theme::named("omarchy-nord");
        assert_eq!(nord.background, Color::Rgb(0x2e, 0x34, 0x40));
        assert_eq!(nord.surface, Color::Rgb(0x3b, 0x42, 0x52));
        assert_eq!(nord.accent, Color::Rgb(0x81, 0xa1, 0xc1));
        assert_eq!(nord.error, Color::Rgb(0xbf, 0x61, 0x6a));
        let minimal = Theme::named("omarchy-local-minimal");
        assert_eq!(minimal.background, Color::Rgb(0, 0, 0));
        assert_eq!(minimal.foreground, Color::Rgb(0xe7, 0xe9, 0xea));
        assert_eq!(minimal.accent, Color::Rgb(0xd3, 0xd3, 0xd3));
        assert_eq!(minimal.success, minimal.foreground);
        assert_eq!(minimal.error, Color::Rgb(0x8a, 0x8a, 0x8a));
        let custom = Config::parse("theme = omarchy-local-minimal\nstatus-bg = #123456\n");
        assert_eq!(custom.theme_preset, ThemePreset::Custom);
        assert_eq!(custom.theme.status_bg, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(custom.theme.accent, minimal.accent);
    }

    #[test]
    fn semantic_presets_and_settings_round_trip() {
        for preset in ThemePreset::ALL {
            assert_eq!(ThemePreset::parse(preset.name()), *preset);
            let theme = Theme::named(preset.name());
            assert_ne!(theme.foreground, theme.surface);
            assert_ne!(theme.accent_muted, theme.accent);
            assert_ne!(theme.accent_muted, theme.surface);
            assert_eq!(theme.status_active_bg, theme.accent);
            assert_eq!(theme.selection_bg, theme.accent);
        }
        let config = Config::parse(
            "theme = nord\nsidebar = false\nsidebar-width = 31\nfocus-follows-mouse = true\n",
        );
        let reparsed = Config::parse(&config.to_text());
        assert_eq!(reparsed.theme_preset, ThemePreset::Nord);
        assert!(!reparsed.sidebar);
        assert_eq!(reparsed.sidebar_width, 31);
        assert!(reparsed.focus_follows_mouse);
    }

    #[test]
    fn notification_and_file_sidebar_settings_round_trip() {
        let config = Config::parse(
            "file-sidebar = true\nfile-sidebar-width = 38\nnotification-delivery = system\nnotify-completion = true\nnotification-sound = chime\nnotification-sound-file = /tmp/ding.wav\n",
        );
        let reparsed = Config::parse(&config.to_text());
        assert!(reparsed.file_sidebar);
        assert_eq!(reparsed.file_sidebar_width, 38);
        assert_eq!(reparsed.notifications, NotificationDelivery::System);
        assert!(reparsed.notify_completion);
        assert_eq!(reparsed.notification_sound, NotificationSound::Chime);
        assert_eq!(reparsed.notification_sound_file, "/tmp/ding.wav");
    }

    #[test]
    fn notification_sound_defaults_to_the_bell_and_rejects_unknown_values() {
        let config = Config::parse("");
        assert_eq!(config.notification_sound, NotificationSound::Bell);
        assert!(config.notification_sound_file.is_empty());
        assert_eq!(
            Config::parse("notification-sound = file\n").notification_sound,
            NotificationSound::File
        );
        assert_eq!(
            Config::parse("notification-sound = off\n").notification_sound,
            NotificationSound::Off
        );
        assert_eq!(
            NotificationSound::parse(" Chime "),
            NotificationSound::Chime
        );
        for sound in NotificationSound::ALL {
            assert_eq!(NotificationSound::parse(sound.name()), *sound);
        }
        let problems = Config::diagnostics("notification-sound = loud\n");
        assert!(
            problems.iter().any(|problem| problem
                .message
                .contains("expected off, bell, chime, or file")),
            "{problems:?}"
        );
    }

    #[test]
    fn editor_defaults_overrides_and_settings_shorthand_round_trip() {
        let config =
            Config::parse("editor = nvim --clean\neditor.md = glow\neditor.RS = nvim --clean\n");
        assert_eq!(config.editor_for_path("README.md"), "glow");
        assert_eq!(config.editor_for_path("src/main.RS"), "nvim --clean");
        assert_eq!(config.editor_for_path("LICENSE"), "nvim --clean");
        assert_eq!(config.editor_rules_text(), "md=glow; rs=nvim --clean");

        let rules = Config::parse_editor_rules(".md=glow; rs=nvim --clean").unwrap();
        assert_eq!(rules, config.editor_rules);
        assert!(Config::parse_editor_rules("md=glow; md=vim").is_err());
        assert!(Config::parse_editor_rules("md").is_err());

        let reparsed = Config::parse(&config.to_text());
        assert_eq!(reparsed.editor, config.editor);
        assert_eq!(reparsed.editor_rules, config.editor_rules);
    }
}
