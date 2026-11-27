//! Add and Edit share a compact profile form and one field-to-row mapping.

use crate::overlay::{
    clip_text, wrapped_footer_rows, wrapped_modal_height, Overlay, OverlayRowStyle,
};
use crate::text_input::{decode_key, edit_line, line_with_cursor, LineKey};
use std::borrow::Cow;
use unicode_width::UnicodeWidthStr;
use uniterm_core::machine::{validate_label, validate_target, MachineProfile};

const LABELS: [&str; 3] = ["Server label", "SSH target", "Workspace"];
const EXAMPLES: [&str; 3] = ["Build server", "dev@build", "default"];
const HINTS: [&str; 3] = [
    "Choose a name for this server in your list.",
    "Enter a hostname, user@host, or an SSH config alias.",
    "Leave blank to use the Workspace named 'default'.",
];
const WIDTH: u16 = 56;
const FOOTER: &[(&str, &str)] = &[("Enter", "Save"), ("Esc", "Cancel"), ("Tab", "Next field")];

pub(super) struct Editor {
    pub(super) id: Option<u64>,
    values: [String; 3],
    field: usize,
    cursor: usize,
    error: Option<String>,
    partial_utf8: Vec<u8>,
}

pub(super) struct Form {
    pub(super) overlay: Overlay,
    fields: Vec<Option<usize>>,
}

impl Form {
    pub(super) fn field_at(&self, cols: u16, rows: u16, x: u16, y: u16) -> Option<usize> {
        let row = self.overlay.content_row_at(cols, rows, x, y)?;
        self.fields.get(row).copied().flatten()
    }
}

impl Editor {
    pub(super) fn new(profile: Option<&MachineProfile>) -> Self {
        let values = profile
            .map(|p| [p.label.clone(), p.target.clone(), p.workspace.clone()])
            .unwrap_or_default();
        Self {
            id: profile.map(|p| p.id),
            cursor: values[0].len(),
            values,
            field: 0,
            error: None,
            partial_utf8: Vec::new(),
        }
    }

    // Terminal reads can split a scalar anywhere, including the first key
    // following Add. Retain at most the three incomplete UTF-8 bytes.
    pub(super) fn join_input<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        if self.partial_utf8.is_empty() {
            return Cow::Borrowed(bytes);
        }
        let mut joined = std::mem::take(&mut self.partial_utf8);
        joined.extend_from_slice(bytes);
        Cow::Owned(joined)
    }

    pub(super) fn decode(&mut self, bytes: &[u8]) -> (LineKey, usize) {
        if bytes.len() < 4
            && bytes[0] >= 0xc2
            && std::str::from_utf8(bytes)
                .is_err_and(|e| e.valid_up_to() == 0 && e.error_len().is_none())
        {
            self.partial_utf8.extend_from_slice(bytes);
            (LineKey::Unknown, bytes.len())
        } else {
            decode_key(bytes, 0)
        }
    }

    pub(super) fn focus(&mut self, field: usize) -> bool {
        if self.field == field {
            return false;
        }
        self.field = field;
        self.cursor = self.values[field].len();
        self.error = None;
        true
    }

    pub(super) fn input(&mut self, key: LineKey) -> bool {
        match key {
            LineKey::Tab | LineKey::Down => self.focus((self.field + 1) % self.values.len()),
            LineKey::BackTab | LineKey::Up => {
                self.focus((self.field + self.values.len() - 1) % self.values.len())
            }
            _ => {
                let changed = edit_line(&mut self.values[self.field], &mut self.cursor, key);
                if changed {
                    self.error = None;
                }
                changed
            }
        }
    }

    pub(super) fn reject(&mut self, message: String) {
        self.error = Some(message);
    }

    pub(super) fn profile(&mut self) -> Option<MachineProfile> {
        let [label, target, workspace] = &self.values;
        let problem = if validate_label(label).is_err() {
            Some((
                0,
                if label.trim().is_empty() {
                    "Enter a server label."
                } else {
                    "Use up to 128 bytes of printable text."
                }
                .to_owned(),
            ))
        } else if validate_target(target).is_err() {
            let why = if target.is_empty() {
                "Enter an SSH target."
            } else if target.len() > 1024 {
                "Use an SSH target of at most 1024 bytes."
            } else {
                "Remove whitespace, control characters or a leading '-'."
            };
            Some((1, why.to_owned()))
        } else {
            uniterm_proto::validate_workspace_name(workspace)
                .err()
                .filter(|_| !workspace.is_empty())
                .map(|why| (2, format!("Workspace {why}.")))
        };
        if let Some((field, message)) = problem {
            self.focus(field);
            self.reject(message);
            return None;
        }
        Some(MachineProfile {
            id: self.id.unwrap_or(0), // New identities are allocated under the catalog lock.
            label: label.clone(),
            target: target.clone(),
            workspace: if workspace.is_empty() {
                "default".into()
            } else {
                workspace.clone()
            },
            enabled: true, // An existing profile retains its setting under the same lock.
        })
    }

    pub(super) fn form(&self, cols: u16, rows: u16) -> Form {
        // Fit the fields above the actual footer, including its wrapped rows.
        let width = usize::from(cols.saturating_sub(8).min(WIDTH));
        let footer_rows = wrapped_footer_rows(FOOTER, width + 2).len();
        let capacity =
            usize::from(wrapped_modal_height(rows).saturating_sub(3)).saturating_sub(footer_rows);
        let title = if self.id.is_some() {
            "Edit Server"
        } else {
            "Add Server"
        };
        let mut lines = Vec::new();
        let mut styles = Vec::new();
        let mut fields = Vec::new();
        let mut push = |line: String, style: OverlayRowStyle, field: Option<usize>| {
            lines.push(line);
            styles.push(style);
            fields.push(field);
        };
        for (field, label) in LABELS.iter().enumerate() {
            let active = self.field == field;
            let single_field = capacity < 4 || (capacity < 7 && width < 24);
            if single_field && !active {
                continue;
            }
            let stacked = capacity >= 7 || (single_field && capacity >= 3);
            if stacked {
                if field > 0 && capacity >= 9 {
                    push(String::new(), OverlayRowStyle::Plain, None);
                }
                push(format!(" {label}"), OverlayRowStyle::Section, Some(field));
            }
            let mark = if active { "›" } else { " " };
            let prefix = if stacked {
                format!(" {mark} ")
            } else if capacity < 3 {
                format!("{mark} ")
            } else {
                format!("{mark} {label}  ")
            };
            let input_width = width.saturating_sub(prefix.width());
            let value = &self.values[field];
            let shown = if value.is_empty() {
                let placeholder = if field == 2 {
                    "default".to_owned()
                } else {
                    format!("e.g. {}", EXAMPLES[field])
                };
                format!("{}{}", if active { "█ " } else { "" }, placeholder)
            } else if active {
                line_with_cursor(value, self.cursor, input_width)
            } else {
                clip_text(value, input_width)
            };
            let style = if active {
                OverlayRowStyle::CardSelected
            } else if value.is_empty() {
                OverlayRowStyle::CardDetail
            } else {
                OverlayRowStyle::ComposerInput
            };
            push(
                clip_text(&format!("{prefix}{shown}"), width),
                style,
                Some(field),
            );
        }
        if capacity >= 2 {
            let (message, style) = self
                .error
                .as_ref()
                .map_or((HINTS[self.field], OverlayRowStyle::CardDetail), |error| {
                    (error.as_str(), OverlayRowStyle::Error)
                });
            push(clip_text(&format!(" {message}"), width), style, None);
        }
        let title = if capacity < 3 {
            format!("{title} / {}", LABELS[self.field])
        } else {
            title.to_owned()
        };
        Form {
            overlay: Overlay::with_footer(title, lines, FOOTER)
                .with_wrapped_footer()
                .with_min_content_width(WIDTH)
                .with_row_styles(styles),
            fields,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_input::decode_key;

    #[test]
    fn profile_form_keeps_unicode_cursor_and_clicks_inside_visible_fields() {
        let mut editor = Editor::new(None);
        for key in "界e\u{301}".repeat(18).chars() {
            editor.input(LineKey::Char(key));
        }
        for (cols, rows) in [(100, 40), (36, 13), (25, 11)] {
            let form = editor.form(cols, rows);
            let rect = form.overlay.geometry(cols, rows);
            let mut saw_input = false;
            for (row, field) in form.fields.iter().enumerate() {
                let y = rect.y + 1 + row as u16;
                for x in [rect.x + 1, rect.x + rect.w - 2] {
                    assert_eq!(form.field_at(cols, rows, x, y), *field);
                }
                assert_eq!(form.field_at(cols, rows, rect.x, y), None);
                if form.overlay.row_style_at(row) == OverlayRowStyle::CardSelected {
                    assert!(form.overlay.lines[row].contains("e\u{301}█"));
                    saw_input = true;
                }
            }
            assert!(saw_input);
            let footer = rect.y + rect.h
                - 1
                - wrapped_footer_rows(FOOTER, usize::from(rect.w - 2)).len() as u16;
            assert_eq!(form.field_at(cols, rows, rect.x + 3, footer), None);
            assert_eq!(
                form.overlay.footer_key_at(cols, rows, rect.x + 3, footer),
                Some("Enter")
            );
            assert_eq!(form.overlay.footer_key_at(cols, rows, rect.x, footer), None);
        }
        let key = decode_key(b"\x1b[Z", 0).0;
        assert!(editor.input(key));
        assert_eq!(editor.field, 2);
        assert!(editor.input(LineKey::Tab));
        assert_eq!(editor.field, 0);
        assert!(!editor.focus(0));
        assert!(!editor.input(LineKey::End));
    }

    #[test]
    fn form_validation_focuses_the_invalid_field_and_retains_entered_values() {
        let mut editor = Editor::new(None);
        editor.values = ["Build".into(), "-bad".into(), "bad/name".into()];
        assert!(editor.profile().is_none());
        assert_eq!(editor.field, 1);
        assert!(editor.error.is_some());
        editor.values[1] = "dev@build".into();
        assert!(editor.profile().is_none());
        assert_eq!(editor.field, 2);
        editor.values[2].clear();
        let profile = editor.profile().unwrap();
        assert_eq!(profile.workspace, "default");
        assert_eq!(profile.target, "dev@build");
    }
}
