//! Terminal-native About modal.
//!
//! The identity and copy follow Uniterm Desktop's compact About dialog. The
//! animated background adapts once's terminal-native visual vocabulary:
//! perspective-projected stars, braille subcells, and two depth-based
//! brightness tiers. Its frame clock is owned by the attach loop and exists
//! only while this modal is visible.

use std::time::{Duration, Instant};

use crate::overlay::{panel_style_no_reset, shadow_style, ui_theme, Rect};

const WIDTH: u16 = 64;
const HEIGHT: u16 = 17;
const STAR_COUNT: usize = 100;
const STAR_SPEED: f64 = 0.03;
const STAR_NEAR: f64 = 0.1;
const STAR_FAR: f64 = 3.0;
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const LOGO_TOP: usize = 1;
const VERSION_ROW: usize = 7;
const TAGLINE_TOP: usize = 9;
const DOCS_ROW: usize = 12;
const HINT_ROW: usize = 14;
const DOCS_LABEL: &str = "[ Docs ]";
pub(crate) const DOCS_URL: &str = "https://github.com/maxart/uniterm/tree/main/docs";

/// Aardvark, rendered for `UNITERM` by TheDraw through patorjk.com's TAAG.
/// The 58-cell width is the modal's minimum for the full retro wordmark.
const LOGO: &[&str] = &[
    "▐▄▄▌ ▐▄▄▌▐▄▄▄▄▄▄▌ ▐▄▄▌▐▄▄▄▄▄▄▌ ▐▄▄▄▄▄▌▐▄▄▄▄▄▄▌ ▐▄▄▄▄▄▄▄▄▌ ",
    "▐██▌ ▐██▌▐██▌ ▐██▌▐██▌  ▐██▌  ▐██▌    ▐██▌ ▐██▌▐██▌▐█▌▐██▌",
    "▐██▌ ▐██▌▐██▌ ▐██▌▐██▌  ▐██▌  ▐████▌  ▐██████▌ ▐██▌▐█▌▐██▌",
    "▐▀▀▌ ▐▀▀ ▐▀▀▌ ▐▀▀▌▐▀▀▌  ▐▀▀▌  ▐▀▀▌    ▐▀▀▌ ▐▀▀▌▐▀▀▌   ▐▀▀▌",
    " ▐▄▄▄▄▄▌ ▐▄▄▌ ▐▄▄▌▐▄▄▌  ▐▄▄▌   ▐▄▄▄▄▄▌▐▄▄▌ ▐▄▄▌▐▄▄▌   ▐▄▄▌",
];
const LOGO_WIDTH: usize = 58;
const SHIMMER_PERIOD: usize = LOGO_WIDTH + 16;

const LEFT_DOTS: [u32; 4] = [0x01, 0x02, 0x04, 0x40];
const RIGHT_DOTS: [u32; 4] = [0x08, 0x10, 0x20, 0x80];

/// An action requested by the About modal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AboutAction {
    None,
    Close,
    OpenDocs,
}

#[derive(Clone, Copy, Debug)]
struct Star {
    x: f64,
    y: f64,
    z: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Tone {
    #[default]
    Blank,
    StarDim,
    StarBright,
    LogoDeep,
    LogoTeal,
    Logo,
    LogoBright,
    LogoGlint,
    LogoTop,
    LogoTopBright,
    LogoTopGlint,
    LogoBase,
    LogoBaseBright,
    LogoBaseGlint,
    Text,
    Muted,
    Button,
}

#[derive(Clone, Copy, Debug, Default)]
struct Cell {
    ch: char,
    tone: Tone,
}

/// The visible About surface and its animation state.
pub(crate) struct AboutView {
    stars: Vec<Star>,
    rng: u64,
    next_frame: Instant,
    shimmer: usize,
}

impl AboutView {
    /// Create a fresh starfield and arm its first visible-only frame.
    pub(crate) fn new() -> Self {
        let mut view = AboutView {
            stars: Vec::with_capacity(STAR_COUNT),
            rng: 0x7a6f_4b29_d13c_8e57,
            next_frame: Instant::now() + FRAME_INTERVAL,
            shimmer: 0,
        };
        let spread = f64::from(WIDTH.max(HEIGHT));
        for _ in 0..STAR_COUNT {
            let star = view.random_star(spread);
            view.stars.push(star);
        }
        view
    }

    /// Center the compact dialog while preserving room for its shadow.
    pub(crate) fn rect(cols: u16, rows: u16) -> Rect {
        let w = WIDTH.min(cols.saturating_sub(4)).max(6);
        let h = HEIGHT.min(rows.saturating_sub(4)).max(6);
        let x = (cols.saturating_sub(w) / 2).max(1) + 1;
        let y = (rows.saturating_sub(h) / 2).max(1) + 1;
        Rect { x, y, w, h }
    }

    /// Return the damage-gated timeout for the attach loop.
    pub(crate) fn poll_timeout(&self, now: Instant) -> Duration {
        self.next_frame.saturating_duration_since(now)
    }

    /// Advance one perspective frame when due.
    pub(crate) fn tick(&mut self, now: Instant, cols: u16, rows: u16) -> bool {
        if now < self.next_frame {
            return false;
        }
        let r = Self::rect(cols, rows);
        self.advance(
            usize::from(r.w.saturating_sub(2)),
            usize::from(r.h.saturating_sub(2)),
        );
        self.shimmer = (self.shimmer + 1) % SHIMMER_PERIOD;
        // Schedule from now instead of replaying missed frames after a stalled
        // terminal. Animation should never create a catch-up burst.
        self.next_frame = now + FRAME_INTERVAL;
        true
    }

    /// Escape closes the modal; Enter or `d` opens the documentation.
    pub(crate) fn handle(&self, input: &[u8]) -> AboutAction {
        if input
            .iter()
            .any(|key| matches!(key, 0x1b | 0x03 | b'q' | b'Q'))
        {
            AboutAction::Close
        } else if input
            .iter()
            .any(|key| matches!(key, b'\r' | b'\n' | b'd' | b'D'))
        {
            AboutAction::OpenDocs
        } else {
            AboutAction::None
        }
    }

    /// Resolve outside dismissal and the centered Docs button.
    pub(crate) fn click(&self, cols: u16, rows: u16, cx: u16, cy: u16) -> AboutAction {
        let r = Self::rect(cols, rows);
        if cx < r.x || cx >= r.x + r.w || cy < r.y || cy >= r.y + r.h {
            return AboutAction::Close;
        }
        let inner = usize::from(r.w.saturating_sub(2));
        let docs_x = r.x + 1 + ((inner.saturating_sub(DOCS_LABEL.len())) / 2) as u16;
        let docs_y = r.y + 1 + DOCS_ROW as u16;
        if cy == docs_y && cx >= docs_x && cx < docs_x + DOCS_LABEL.len() as u16 {
            AboutAction::OpenDocs
        } else {
            AboutAction::None
        }
    }

    /// Draw the current modal frame at absolute terminal coordinates.
    pub(crate) fn render(&self, cols: u16, rows: u16) -> Vec<u8> {
        let r = Self::rect(cols, rows);
        let width = usize::from(r.w.saturating_sub(2));
        let height = usize::from(r.h.saturating_sub(2));
        let mut cells = self.star_cells(width, height);

        if width >= LOGO_WIDTH {
            put_logo(&mut cells, width, height, LOGO_TOP, LOGO, self.shimmer);
        } else if let Some(compact) = uniterm_core::agent::wordmark("Uniterm") {
            let compact: Vec<&str> = compact.iter().map(String::as_str).collect();
            if compact.iter().all(|line| line.chars().count() <= width) {
                put_logo(&mut cells, width, height, LOGO_TOP, &compact, self.shimmer);
            } else {
                put_center(
                    &mut cells,
                    width,
                    height,
                    LOGO_TOP + 1,
                    "UNITERM",
                    Tone::Logo,
                );
            }
        }
        put_center(
            &mut cells,
            width,
            height,
            VERSION_ROW,
            &format!(
                "v{}{}",
                env!("CARGO_PKG_VERSION"),
                env!("UNITERM_VERSION_SUFFIX")
            ),
            Tone::Text,
        );
        put_center(
            &mut cells,
            width,
            height,
            TAGLINE_TOP,
            "A cross-platform terminal multiplexer",
            Tone::Muted,
        );
        put_center(
            &mut cells,
            width,
            height,
            TAGLINE_TOP + 1,
            "for agentic engineering",
            Tone::Muted,
        );
        put_center(
            &mut cells,
            width,
            height,
            DOCS_ROW,
            DOCS_LABEL,
            Tone::Button,
        );
        put_center(
            &mut cells,
            width,
            height,
            HINT_ROW,
            "Esc close  |  Enter open docs",
            Tone::Muted,
        );

        let mut out = Vec::new();
        let shadow = shadow_style();
        for row in 0..r.h {
            let (sy, sx) = (r.y + 1 + row, r.x + 1);
            if sy > rows {
                break;
            }
            out.extend_from_slice(format!("\x1b[{sy};{sx}H{shadow}").as_bytes());
            out.extend(std::iter::repeat_n(
                b' ',
                r.w.min(cols.saturating_sub(sx - 1)) as usize,
            ));
        }

        let panel = panel_style_no_reset();
        out.extend_from_slice(panel.as_bytes());
        out.extend_from_slice(
            format!(
                "\x1b[{};{}H{}",
                r.y,
                r.x,
                titled_border('\u{250C}', '\u{2510}', width)
            )
            .as_bytes(),
        );
        for row in 0..height {
            let y = r.y + 1 + row as u16;
            out.extend_from_slice(format!("\x1b[{y};{}H{panel}\u{2502}", r.x).as_bytes());
            let mut last_tone = None;
            for cell in &cells[row * width..(row + 1) * width] {
                if last_tone != Some(cell.tone) {
                    out.extend_from_slice(tone_style(cell.tone).as_bytes());
                    last_tone = Some(cell.tone);
                }
                out.extend_from_slice(cell.ch.encode_utf8(&mut [0; 4]).as_bytes());
            }
            out.extend_from_slice(format!("{panel}\u{2502}").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "\x1b[{};{}H{}\x1b[0m",
                r.y + r.h - 1,
                r.x,
                bottom_border(width)
            )
            .as_bytes(),
        );
        out
    }

    fn star_cells(&self, width: usize, height: usize) -> Vec<Cell> {
        let mut cells = vec![
            Cell {
                ch: ' ',
                tone: Tone::Blank,
            };
            width.saturating_mul(height)
        ];
        if width == 0 || height == 0 {
            return cells;
        }
        let sub_width = width * 2;
        let sub_height = height * 4;
        let center_x = sub_width as f64 / 2.0;
        let center_y = sub_height as f64 / 2.0;
        for star in &self.stars {
            if star.z <= 0.0 {
                continue;
            }
            let sx = center_x + star.x / star.z;
            let sy = center_y + star.y / star.z;
            if sx < 0.0 || sx >= sub_width as f64 || sy < 0.0 || sy >= sub_height as f64 {
                continue;
            }
            let (sx, sy) = (sx as usize, sy as usize);
            let (col, row) = (sx / 2, sy / 4);
            let bit = if sx % 2 == 0 {
                LEFT_DOTS[sy % 4]
            } else {
                RIGHT_DOTS[sy % 4]
            };
            let cell = &mut cells[row * width + col];
            let current = if cell.ch == ' ' {
                0
            } else {
                cell.ch as u32 - 0x2800
            };
            cell.ch = char::from_u32(0x2800 + (current | bit)).unwrap_or('\u{2800}');
            if star.z < STAR_FAR / 2.0 || cell.tone == Tone::StarBright {
                cell.tone = Tone::StarBright;
            } else {
                cell.tone = Tone::StarDim;
            }
        }
        cells
    }

    fn advance(&mut self, width: usize, height: usize) {
        let sub_width = width * 2;
        let sub_height = height * 4;
        let center_x = sub_width as f64 / 2.0;
        let center_y = sub_height as f64 / 2.0;
        let spread = width.max(height) as f64;
        for index in 0..self.stars.len() {
            let mut star = self.stars[index];
            star.z -= STAR_SPEED;
            let outside = if star.z <= STAR_NEAR {
                true
            } else {
                let sx = center_x + star.x / star.z;
                let sy = center_y + star.y / star.z;
                sx < 0.0 || sx >= sub_width as f64 || sy < 0.0 || sy >= sub_height as f64
            };
            if outside {
                star = self.random_star(spread);
            }
            self.stars[index] = star;
        }
    }

    fn random_star(&mut self, spread: f64) -> Star {
        Star {
            x: (self.random_unit() - 0.5) * spread,
            y: (self.random_unit() - 0.5) * spread,
            z: STAR_NEAR + self.random_unit() * (STAR_FAR - STAR_NEAR),
        }
    }

    fn random_unit(&mut self) -> f64 {
        // xorshift64* keeps the animation dependency-free and deterministic
        // enough for exact render tests.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let value = self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (value >> 11) as f64 / ((1u64 << 53) as f64)
    }
}

fn put_center(cells: &mut [Cell], width: usize, height: usize, row: usize, text: &str, tone: Tone) {
    if row >= height || width == 0 {
        return;
    }
    let chars: Vec<char> = text.chars().take(width).collect();
    let start = width.saturating_sub(chars.len()) / 2;
    for (offset, ch) in chars.into_iter().enumerate() {
        cells[row * width + start + offset] = Cell { ch, tone };
    }
}

/// Paint a centered logo while a diagonal highlight travels left to right.
/// Spaces remain transparent so the modal's starfield can show through.
fn put_logo(
    cells: &mut [Cell],
    width: usize,
    height: usize,
    top: usize,
    lines: &[&str],
    shimmer: usize,
) {
    for (row_offset, line) in lines.iter().enumerate() {
        let row = top + row_offset;
        if row >= height {
            break;
        }
        let line_width = line.chars().count().min(width);
        let start = width.saturating_sub(line_width) / 2;
        for (column, ch) in line.chars().take(width).enumerate() {
            if ch == ' ' {
                continue;
            }
            let phase = (column + row_offset * 2 + SHIMMER_PERIOD - shimmer % SHIMMER_PERIOD)
                % SHIMMER_PERIOD;
            let base = match (row_offset, ch) {
                (0, '▄') => Tone::LogoTop,
                (0..=2, '▐') => Tone::LogoBright,
                (0..=2, '▌') => Tone::LogoTeal,
                (0..=2, _) => Tone::Logo,
                (3, '▀') => Tone::LogoBright,
                (3, _) => Tone::LogoDeep,
                (4, '▄') => Tone::LogoBase,
                (4, '▐') => Tone::Logo,
                (4, '▌') => Tone::LogoTeal,
                _ => Tone::LogoDeep,
            };
            let tone = match phase {
                0 => match base {
                    Tone::LogoTop => Tone::LogoTopGlint,
                    Tone::LogoBase => Tone::LogoBaseGlint,
                    _ => Tone::LogoGlint,
                },
                1 | 73 => match base {
                    Tone::LogoTop => Tone::LogoTopBright,
                    Tone::LogoBase => Tone::LogoBaseBright,
                    _ => Tone::LogoBright,
                },
                _ => base,
            };
            cells[row * width + start + column] = Cell { ch, tone };
        }
    }
}

fn tone_style(tone: Tone) -> String {
    let theme = ui_theme();
    let background = theme.background.sgr_bg();
    match tone {
        Tone::Blank => format!("\x1b[0;{};{}m", theme.muted.sgr_fg(), background),
        Tone::StarDim => format!("\x1b[0;2;{};{}m", theme.muted.sgr_fg(), background),
        Tone::StarBright => {
            format!("\x1b[0;1;{};{}m", theme.foreground.sgr_fg(), background)
        }
        Tone::LogoDeep => format!("\x1b[0;1;38;2;85;85;255;{background}m"),
        Tone::LogoTeal => format!("\x1b[0;1;38;2;0;170;170;{background}m"),
        Tone::Logo => format!("\x1b[0;1;38;2;85;255;255;{background}m"),
        Tone::LogoBright => format!("\x1b[0;1;38;2;205;255;255;{background}m"),
        Tone::LogoGlint => format!("\x1b[0;1;38;2;255;255;255;{background}m"),
        Tone::LogoTop => "\x1b[0;1;38;2;85;255;255;48;2;0;170;170m".into(),
        Tone::LogoTopBright => "\x1b[0;1;38;2;205;255;255;48;2;0;170;170m".into(),
        Tone::LogoTopGlint => "\x1b[0;1;38;2;255;255;255;48;2;0;170;170m".into(),
        Tone::LogoBase => "\x1b[0;1;38;2;85;85;255;48;2;0;0;170m".into(),
        Tone::LogoBaseBright => "\x1b[0;1;38;2;85;255;255;48;2;0;0;170m".into(),
        Tone::LogoBaseGlint => "\x1b[0;1;38;2;255;255;255;48;2;0;0;170m".into(),
        Tone::Text => format!("\x1b[0;1;{};{}m", theme.foreground.sgr_fg(), background),
        Tone::Muted => format!("\x1b[0;{};{}m", theme.muted.sgr_fg(), background),
        Tone::Button => format!(
            "\x1b[0;1;{};{}m",
            theme.status_active_fg.sgr_fg(),
            theme.accent_muted.sgr_bg()
        ),
    }
}

fn titled_border(left: char, right: char, width: usize) -> String {
    let title = " About Uniterm ";
    let mut middle = if width > title.chars().count() {
        format!("\u{2500}{title}")
    } else {
        String::new()
    };
    middle.extend(std::iter::repeat_n(
        '\u{2500}',
        width.saturating_sub(middle.chars().count()),
    ));
    format!("{left}{middle}{right}")
}

fn bottom_border(width: usize) -> String {
    format!(
        "\u{2514}{}\u{2518}",
        std::iter::repeat_n('\u{2500}', width).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_ports_identity_copy_and_terminal_starfield() {
        let view = AboutView::new();
        let rendered = String::from_utf8(view.render(120, 40)).unwrap();
        let rect = AboutView::rect(120, 40);
        assert_eq!(
            rect,
            Rect {
                x: 29,
                y: 12,
                w: 64,
                h: 17
            }
        );
        assert!(rendered.contains("About Uniterm"));
        assert!(rendered.contains(concat!("v", env!("CARGO_PKG_VERSION"))));
        assert!(rendered.contains("A cross-platform terminal multiplexer"));
        assert!(rendered.contains("for agentic engineering"));
        assert!(rendered.contains(DOCS_LABEL));
        assert!(rendered
            .chars()
            .any(|ch| ('\u{2801}'..='\u{28ff}').contains(&ch)));
        assert!(rendered.contains(&format!(
            "\x1b[{};{}H",
            rect.y + 1 + VERSION_ROW as u16,
            rect.x
        )));
        assert!(rendered.contains(&tone_style(Tone::Button)));
        assert!(rendered.contains(&tone_style(Tone::LogoDeep)));
        assert!(rendered.contains(&tone_style(Tone::LogoGlint)));
        assert!(rendered.contains(&tone_style(Tone::LogoTop)));
        assert!(rendered.contains(&tone_style(Tone::LogoBase)));
    }

    #[test]
    fn aardvark_logo_is_exact_and_shimmer_changes_only_its_tones() {
        assert!(LOGO.iter().all(|line| line.chars().count() == LOGO_WIDTH));
        let (width, height) = (usize::from(WIDTH - 2), usize::from(HEIGHT - 2));
        let mut first = vec![
            Cell {
                ch: ' ',
                tone: Tone::Blank,
            };
            width * height
        ];
        let mut second = first.clone();
        put_logo(&mut first, width, height, LOGO_TOP, LOGO, 0);
        put_logo(&mut second, width, height, LOGO_TOP, LOGO, 1);

        for (offset, expected) in LOGO.iter().enumerate() {
            let row = &first[(LOGO_TOP + offset) * width..(LOGO_TOP + offset + 1) * width];
            let text: String = row.iter().map(|cell| cell.ch).collect();
            assert!(text.contains(expected.trim_end()), "{expected:?}");
        }
        assert!(first
            .iter()
            .zip(&second)
            .all(|(left, right)| left.ch == right.ch));
        assert!(first
            .iter()
            .zip(&second)
            .any(|(left, right)| left.tone != right.tone));
    }

    #[test]
    fn modal_rows_fill_the_exact_box_width() {
        let view = AboutView::new();
        let rect = AboutView::rect(100, 30);
        let rendered = view.render(100, 30);
        let segments = crate::overlay::render_segments(&rendered);
        for row in rect.y..rect.y + rect.h {
            assert!(
                segments
                    .iter()
                    .any(|(y, x, width)| *y == row && *x == rect.x && *width == rect.w as usize),
                "missing exact-width modal row {row}"
            );
        }
    }

    #[test]
    fn keyboard_and_clicks_share_docs_and_close_actions() {
        let view = AboutView::new();
        assert_eq!(view.handle(b"\x1b"), AboutAction::Close);
        assert_eq!(view.handle(b"\r"), AboutAction::OpenDocs);
        assert_eq!(view.handle(b"x"), AboutAction::None);

        let rect = AboutView::rect(100, 30);
        let inner = usize::from(rect.w - 2);
        let docs_x = rect.x + 1 + ((inner - DOCS_LABEL.len()) / 2) as u16;
        let docs_y = rect.y + 1 + DOCS_ROW as u16;
        assert_eq!(view.click(100, 30, docs_x, docs_y), AboutAction::OpenDocs);
        assert_eq!(view.click(100, 30, 1, 1), AboutAction::Close);
        assert_eq!(
            view.click(100, 30, rect.x + 1, rect.y + 1),
            AboutAction::None
        );
    }

    #[test]
    fn animation_moves_only_when_its_deadline_is_due() {
        let mut view = AboutView::new();
        let first_z = view.stars[0].z;
        let first_shimmer = view.shimmer;
        assert!(!view.tick(view.next_frame - Duration::from_millis(1), 100, 30));
        assert_eq!(view.stars[0].z, first_z);
        assert_eq!(view.shimmer, first_shimmer);
        assert!(view.tick(view.next_frame, 100, 30));
        assert_ne!(view.stars[0].z, first_z);
        assert_eq!(view.shimmer, first_shimmer + 1);
    }
}
