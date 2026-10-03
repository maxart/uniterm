//! Search and preview a large theme catalog without applying while browsing.
use std::cell::Cell;

use crate::overlay::{
    footer_spans, footer_text, modal_hit, modal_rect, modal_visible_rows, panel_style,
    render_list_modal, ui_theme, ModalHit,
};
use crate::text_input::{decode_key, edit_line, LineKey};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

const HEADER_ROWS: usize = 2;
const FOOTER: &[(&str, &str)] = &[("Enter", "Apply"), ("Esc", "Back")];

pub(crate) enum PickerAction {
    None,
    Redraw,
    Back,
    Apply(String),
}

pub(crate) struct ThemePicker {
    names: Vec<String>,
    current: String,
    query: String,
    query_cursor: usize,
    matches: Vec<usize>,
    selected: usize,
    scroll: Cell<usize>,
}

impl ThemePicker {
    pub(crate) fn new(names: Vec<String>, current: String) -> Self {
        let selected = names.iter().position(|name| *name == current).unwrap_or(0);
        Self {
            matches: (0..names.len()).collect(),
            names,
            current,
            query: String::new(),
            query_cursor: 0,
            selected,
            scroll: Cell::new(0),
        }
    }

    pub(crate) fn refresh(&mut self, names: Vec<String>, current: String) {
        self.current = current;
        if self.names != names {
            let selected = self.selected_name().map(str::to_owned);
            self.names = names;
            self.filter();
            if let Some(name) = selected {
                self.selected = self
                    .matches
                    .iter()
                    .position(|index| self.names[*index] == name)
                    .unwrap_or(0);
            }
        }
    }

    fn query_line(&self, width: usize) -> String {
        let mut used = 1;
        let mut before = Vec::new();
        for part in self.query[..self.query_cursor].graphemes(true).rev() {
            if used + part.width() > width {
                break;
            }
            before.push(part);
            used += part.width();
        }
        let mut shown = before.into_iter().rev().collect::<String>();
        shown.push('█');
        shown.push_str(&fit(
            &self.query[self.query_cursor..],
            width.saturating_sub(used),
        ));
        shown
    }

    fn selected_name(&self) -> Option<&str> {
        self.matches
            .get(self.selected)
            .map(|index| self.names[*index].as_str())
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase().replace(['-', '_'], " ");
        self.matches = self
            .names
            .iter()
            .enumerate()
            .filter_map(|(index, name)| {
                let name = format!("{} {name}", display_name(name).0)
                    .to_lowercase()
                    .replace(['-', '_'], " ");
                query
                    .split_whitespace()
                    .all(|word| name.contains(word))
                    .then_some(index)
            })
            .collect();
        self.selected = 0;
        self.scroll.set(0);
    }

    fn visible(rows: u16, cols: u16) -> usize {
        modal_visible_rows(modal_rect(cols, rows).h)
            .saturating_sub(HEADER_ROWS)
            .max(1)
    }

    fn list_width(cols: u16, rows: u16) -> usize {
        usize::from(modal_rect(cols, rows).w.saturating_sub(3)) / 2
    }

    fn start(&self, cols: u16, rows: u16) -> usize {
        let visible = Self::visible(rows, cols);
        let mut start = self
            .scroll
            .get()
            .min(self.matches.len().saturating_sub(visible));
        if self.selected < start {
            start = self.selected;
        }
        if self.selected >= start + visible {
            start = self.selected + 1 - visible;
        }
        self.scroll.set(start);
        start
    }

    fn move_by(&mut self, delta: isize) -> PickerAction {
        let next = self
            .selected
            .saturating_add_signed(delta)
            .min(self.matches.len().saturating_sub(1));
        if next == self.selected {
            return PickerAction::None;
        }
        self.selected = next;
        PickerAction::Redraw
    }

    fn apply(&self) -> PickerAction {
        self.selected_name()
            .map_or(PickerAction::None, |name| PickerAction::Apply(name.into()))
    }

    pub(crate) fn handle(&mut self, input: &[u8], cols: u16, rows: u16) -> PickerAction {
        let mut at = 0;
        let mut changed = false;
        while at < input.len() {
            let tail = &input[at..];
            if tail[0] == 0x15 {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.query_cursor = 0;
                    self.filter();
                    changed = true;
                }
                at += 1;
                continue;
            }
            let navigation = [
                (b"\x1b[5~".as_slice(), -(Self::visible(rows, cols) as isize)),
                (b"\x1b[6~".as_slice(), Self::visible(rows, cols) as isize),
                (b"\x1b[H".as_slice(), isize::MIN),
                (b"\x1b[F".as_slice(), isize::MAX),
                (b"\x1b[1~".as_slice(), isize::MIN),
                (b"\x1b[4~".as_slice(), isize::MAX),
                (b"\x1b[Z".as_slice(), -1),
                (b"\x10".as_slice(), -1),
                (b"\x0e".as_slice(), 1),
            ];
            if let Some((sequence, delta)) = navigation
                .iter()
                .find(|(sequence, _)| tail.starts_with(sequence))
            {
                changed |= matches!(self.move_by(*delta), PickerAction::Redraw);
                at += sequence.len();
                continue;
            }
            let (key, used) = decode_key(input, at);
            at += used.max(1);
            match key {
                LineKey::Escape | LineKey::Cancel => return PickerAction::Back,
                LineKey::Enter => {
                    return match self.apply() {
                        PickerAction::None if changed => PickerAction::Redraw,
                        action => action,
                    }
                }
                LineKey::Down | LineKey::Tab => {
                    changed |= matches!(self.move_by(1), PickerAction::Redraw)
                }
                LineKey::Up => changed |= matches!(self.move_by(-1), PickerAction::Redraw),
                key => {
                    let before = self.query.clone();
                    if edit_line(&mut self.query, &mut self.query_cursor, key) {
                        if self.query != before {
                            self.filter();
                        }
                        changed = true;
                    }
                }
            }
        }
        if changed {
            PickerAction::Redraw
        } else {
            PickerAction::None
        }
    }

    pub(crate) fn wheel(&mut self, down: bool) -> PickerAction {
        self.move_by(if down { 3 } else { -3 })
    }

    pub(crate) fn click(&mut self, cols: u16, rows: u16, x: u16, y: u16) -> PickerAction {
        let rect = modal_rect(cols, rows);
        let width = Self::list_width(cols, rows);
        match modal_hit(rect, width as u16 + 1, x, y) {
            ModalHit::Outside => PickerAction::Back,
            ModalHit::Bar(column) => {
                match footer_spans(FOOTER)
                    .iter()
                    .find(|(span, _)| span.contains(&column))
                    .map(|(_, index)| *index)
                {
                    Some(0) => self.apply(),
                    Some(1) => PickerAction::Back,
                    _ => PickerAction::None,
                }
            }
            ModalHit::ListRow(slot) if slot >= HEADER_ROWS => {
                let visible = Self::visible(rows, cols);
                let selected = if x == rect.x + width as u16 && self.matches.len() > visible {
                    (slot - HEADER_ROWS) * (self.matches.len() - 1)
                        / visible.saturating_sub(1).max(1)
                } else {
                    self.start(cols, rows) + slot - HEADER_ROWS
                };
                if selected < self.matches.len() {
                    self.selected = selected;
                    PickerAction::Redraw
                } else {
                    PickerAction::None
                }
            }
            _ => PickerAction::None,
        }
    }

    pub(crate) fn render(&self, cols: u16, rows: u16) -> Vec<u8> {
        let rect = modal_rect(cols, rows);
        let inner = usize::from(rect.w - 2);
        let list_w = Self::list_width(cols, rows);
        let detail_w = inner - list_w - 1;
        let height = modal_visible_rows(rect.h);
        let visible = Self::visible(rows, cols);
        let start = self.start(cols, rows);
        let end = (start + visible).min(self.matches.len());
        let panel = panel_style();
        let theme = ui_theme();
        let selected_style = format!(
            "\x1b[0;{};{}m",
            theme.status_active_fg.sgr_fg(),
            theme.status_active_bg.sgr_bg()
        );
        let muted = format!(
            "\x1b[0;{};{}m",
            theme.muted.sgr_fg(),
            theme.surface.sgr_bg()
        );
        let mut detail = Vec::new();
        let mut push =
            |text: &str, style: &str| detail.push(format!("{style}{}{panel}", fit(text, detail_w)));
        if let Some(name) = self.selected_name() {
            let (collection, label) = display_name(name);
            push(&format!(" Preview: {label}"), &panel);
            push(&format!(" {collection}"), &muted);
            push(
                if name == self.current {
                    " Current theme"
                } else {
                    " Preview only - Enter to apply"
                },
                &muted,
            );
            push("", &panel);
            let preview = uniterm_core::Theme::named(name);
            let surface = format!(
                "\x1b[0;{};{}m",
                preview.foreground.sgr_fg(),
                preview.background.sgr_bg()
            );
            let active = format!(
                "\x1b[0;{};{}m",
                preview.status_active_fg.sgr_fg(),
                preview.status_active_bg.sgr_bg()
            );
            push(" Workspace   Tab 1", &active);
            push(" $ uniterm", &surface);
            for (text, color) in [
                (" Ready", preview.success),
                (" Attention", preview.warning),
                (" Error", preview.error),
            ] {
                push(
                    text,
                    &format!("\x1b[0;{};{}m", color.sgr_fg(), preview.background.sgr_bg()),
                );
            }
            push("", &panel);
            push(" Wheel / arrows / Tab: browse", &muted);
            push(" PgUp/PgDn, Home/End: navigate", &muted);
            push(" Type to search; Ctrl-U clears", &muted);
        } else {
            push(" No matching themes", &panel);
            push(" Try fewer words or Ctrl-U", &muted);
            push(" to clear the search.", &muted);
        }
        detail.resize(height, format!("{panel}{}", " ".repeat(detail_w)));
        detail.truncate(height);
        render_list_modal(
            cols,
            rows,
            " Choose theme ",
            list_w,
            |slot| {
                if slot == 0 {
                    let shown = if self.query.is_empty() {
                        "Search themes... █".into()
                    } else {
                        self.query_line(list_w.saturating_sub(4))
                    };
                    return Some(format!("{panel}{}", fit(&format!(" / {shown}"), list_w)));
                }
                if slot == 1 {
                    let first = if self.matches.is_empty() {
                        0
                    } else {
                        start + 1
                    };
                    return Some(format!(
                        "{muted}{}{panel}",
                        fit(
                            &format!(" {first}-{end} / {} matches", self.matches.len()),
                            list_w
                        )
                    ));
                }
                let index = start + slot - HEADER_ROWS;
                let name = self
                    .matches
                    .get(index)
                    .map(|index| self.names[*index].as_str());
                let text = name.map_or(String::new(), |name| {
                    let marker = if name == self.current { '*' } else { ' ' };
                    format!(
                        "{marker} {} ({})",
                        display_name(name).1,
                        display_name(name).0
                    )
                });
                let style = if name.is_some() && index == self.selected {
                    &selected_style
                } else {
                    &panel
                };
                let track = if self.matches.len() > visible {
                    let thumb =
                        start * (visible - 1) / self.matches.len().saturating_sub(visible).max(1);
                    if slot - HEADER_ROWS == thumb {
                        '█'
                    } else {
                        '│'
                    }
                } else {
                    ' '
                };
                Some(format!(
                    "{style}{}{muted}{track}{panel}",
                    fit(&text, list_w.saturating_sub(1))
                ))
            },
            &detail,
            &footer_text(FOOTER, inner),
        )
    }
}

fn display_name(name: &str) -> (&'static str, String) {
    let (source, name) = if let Some(name) = name.strip_prefix("omarchy-community-") {
        ("Omarchy community", name)
    } else if let Some(name) = name.strip_prefix("omarchy-local-") {
        ("Local", name)
    } else if let Some(name) = name.strip_prefix("omarchy-") {
        ("Omarchy", name)
    } else {
        ("Uniterm", name)
    };
    (
        source,
        name.split('-')
            .map(|word| {
                let mut chars = word.chars();
                chars.next().map_or(String::new(), |first| {
                    first.to_uppercase().collect::<String>() + chars.as_str()
                })
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Clip at display-cell boundaries and remove terminal controls at output.
fn fit(text: &str, width: usize) -> String {
    let safe: String = text.chars().filter(|c| !c.is_control()).collect();
    let mut out = String::new();
    let mut used = 0;
    for part in safe.graphemes(true) {
        let cells = part.width();
        if used + cells > width {
            break;
        }
        out.push_str(part);
        used += cells;
    }
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker() -> ThemePicker {
        ThemePicker::new(
            uniterm_core::config::ThemePreset::ALL
                .iter()
                .map(|p| p.name().into())
                .collect(),
            "uniterm-dark".into(),
        )
    }

    #[test]
    fn scrollbar_reaches_the_end_and_batched_empty_search_repaints() {
        let mut picker = picker();
        let rect = modal_rect(80, 24);
        let track_x = rect.x + ThemePicker::list_width(80, 24) as u16;
        let track_y = rect.y + HEADER_ROWS as u16 + ThemePicker::visible(24, 80) as u16;
        assert!(matches!(
            picker.click(80, 24, track_x, track_y),
            PickerAction::Redraw
        ));
        assert_eq!(picker.selected, picker.names.len() - 1);
        assert_eq!(picker.current, "uniterm-dark");
        assert!(matches!(
            picker.handle(b"zzzzzzz\r", 80, 24),
            PickerAction::Redraw
        ));
        assert!(picker.matches.is_empty());
    }

    #[test]
    fn catalog_pages_to_last_theme_and_click_uses_scrolled_rows() {
        let mut picker = picker();
        assert!(ThemePicker::visible(24, 80) >= 12);
        picker.handle(b"\x1b[F", 80, 24);
        assert_eq!(picker.selected, picker.names.len() - 1);
        let start = picker.start(80, 24);
        assert!(start > 0);
        let rect = modal_rect(80, 24);
        let expected = picker.names[start].clone();
        assert!(matches!(
            picker.click(80, 24, rect.x + 2, rect.y + 3),
            PickerAction::Redraw
        ));
        assert_eq!(picker.selected_name(), Some(expected.as_str()));
        assert_eq!(
            picker.start(80, 24),
            start,
            "clicking must not shift the viewport"
        );
        assert_eq!(picker.current, "uniterm-dark", "preview does not apply");
        assert!(
            matches!(picker.handle(b"\r", 80, 24), PickerAction::Apply(name) if name == expected)
        );
    }

    #[test]
    fn search_matches_words_and_wheel_does_not_edit_query_or_apply() {
        let mut picker = picker();
        picker.handle(b"rose pine", 80, 24);
        assert!(picker.matches.len() > 1);
        assert!(picker
            .matches
            .iter()
            .all(|index| picker.names[*index].contains("rose-pine")));
        picker.wheel(true);
        assert_eq!(picker.query, "rose pine");
        assert_eq!(picker.current, "uniterm-dark");
        picker.handle(b"\x15local minimal", 80, 24);
        assert_eq!(picker.selected_name(), Some("omarchy-local-minimal"));
        picker.handle(b"\x1b[D\x15", 80, 24);
        assert!(
            picker.query.is_empty(),
            "Ctrl-U clears even inside the query"
        );
        assert_eq!(picker.matches.len(), picker.names.len());
        picker.handle(b"zzzzzz", 80, 24);
        assert!(matches!(picker.handle(b"\r", 80, 24), PickerAction::None));
        assert!(matches!(picker.wheel(true), PickerAction::None));
        assert!(matches!(picker.handle(b"\x1b", 80, 24), PickerAction::Back));
    }

    #[test]
    fn page_navigation_resize_and_footer_share_bounded_geometry() {
        let mut picker = picker();
        picker.handle(b"\x1b[6~", 80, 24);
        assert_eq!(picker.selected, ThemePicker::visible(24, 80));
        picker.handle(b"\x1b[5~", 80, 24);
        assert_eq!(picker.selected, 0);
        picker.handle(b"\x1b[F", 80, 24);
        for (cols, rows) in [(60, 18), (80, 24), (120, 40)] {
            let rect = modal_rect(cols, rows);
            let start = picker.start(cols, rows);
            assert!(
                picker.selected >= start
                    && picker.selected < start + ThemePicker::visible(rows, cols)
            );
            for (row, col, count) in crate::overlay::render_segments(&picker.render(cols, rows)) {
                let shadow = col == rect.x + 1 && count == usize::from(rect.w);
                if row >= rect.y && row < rect.y + rect.h && !shadow {
                    assert!(
                        col + count as u16 <= rect.x + rect.w,
                        "row {row} overflows at {cols}x{rows}"
                    );
                }
            }
            assert!(matches!(
                picker.click(cols, rows, rect.x + 2, rect.y + 1),
                PickerAction::None
            ));
            let apply = footer_spans(FOOTER)[0].0.start;
            assert!(matches!(
                picker.click(cols, rows, rect.x + 1 + apply as u16, rect.y + rect.h - 2),
                PickerAction::Apply(_)
            ));
        }
    }

    #[test]
    fn applied_theme_starts_visible_and_unicode_query_keeps_a_cursor() {
        let mut picker = picker();
        let last = picker.names.last().unwrap().clone();
        picker = ThemePicker::new(picker.names, last.clone());
        assert_eq!(picker.selected_name(), Some(last.as_str()));
        assert!(picker.start(80, 24) > 0);
        picker.handle("界👩‍💻e\u{301}".repeat(20).as_bytes(), 80, 24);
        let line = picker.query_line(20);
        assert_eq!(line.width(), 20);
        assert!(line.contains('█'));
        assert_eq!(fit("\x1bhello\n", 5), "hello");
    }
}
