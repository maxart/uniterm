//! Cross-terminal single-line editing primitives.
//!
//! Terminals encode the same editing gesture in several ways. In particular,
//! Option-Backspace on macOS commonly arrives as `Esc Backspace`, while other
//! terminals send Ctrl-W or a modified CSI sequence. This module normalizes
//! those spellings before a modal decides what Enter, Tab, or Escape means.

/// A normalized key used by single-line modal inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKey {
    Char(char),
    Left,
    Right,
    WordLeft,
    WordRight,
    Home,
    End,
    Backspace,
    Delete,
    WordBackspace,
    WordDelete,
    DeleteBefore,
    DeleteAfter,
    Up,
    Down,
    Enter,
    Tab,
    Escape,
    Cancel,
    Unknown,
}

/// Decode one terminal key and return the key plus the number of bytes used.
/// Both legacy xterm sequences and CSI-u keyboard protocol spellings are
/// accepted so inputs behave consistently across macOS, Linux, and BSD hosts.
pub fn decode_key(input: &[u8], at: usize) -> (LineKey, usize) {
    let Some(&byte) = input.get(at) else {
        return (LineKey::Unknown, 0);
    };
    if byte == 0x1b {
        let rest = &input[at + 1..];
        if rest.is_empty() {
            return (LineKey::Escape, 1);
        }
        return match rest[0] {
            0x7f | 0x08 => (LineKey::WordBackspace, 2),
            b'b' | b'B' => (LineKey::WordLeft, 2),
            b'f' | b'F' => (LineKey::WordRight, 2),
            b'd' | b'D' => (LineKey::WordDelete, 2),
            b'[' => decode_csi(rest),
            _ => (LineKey::Unknown, 2),
        };
    }
    let key = match byte {
        0x01 => LineKey::Home,
        0x02 => LineKey::Left,
        0x03 => LineKey::Cancel,
        0x04 => LineKey::Delete,
        0x05 => LineKey::End,
        0x06 => LineKey::Right,
        0x08 | 0x7f => LineKey::Backspace,
        b'\t' => LineKey::Tab,
        b'\n' | b'\r' => LineKey::Enter,
        0x0b => LineKey::DeleteAfter,
        0x15 => LineKey::DeleteBefore,
        0x17 => LineKey::WordBackspace,
        c if (0x20..0x7f).contains(&c) => LineKey::Char(c as char),
        c if c >= 0x80 => {
            let width = match c {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => 1,
            };
            match input
                .get(at..at.saturating_add(width))
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .and_then(|text| text.chars().next())
            {
                Some(ch) => return (LineKey::Char(ch), ch.len_utf8()),
                None => LineKey::Unknown,
            }
        }
        _ => LineKey::Unknown,
    };
    (key, 1)
}

fn decode_csi(rest: &[u8]) -> (LineKey, usize) {
    let Some(final_at) = rest
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte) && *byte != b'[')
    else {
        return (LineKey::Unknown, rest.len() + 1);
    };
    let final_byte = rest[final_at];
    let params = std::str::from_utf8(&rest[1..final_at]).unwrap_or_default();
    let modifier = params
        .split(';')
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(1);
    let word = matches!(modifier, 3..=8);
    let key = match final_byte {
        b'A' => LineKey::Up,
        b'B' => LineKey::Down,
        b'C' if word => LineKey::WordRight,
        b'D' if word => LineKey::WordLeft,
        b'C' => LineKey::Right,
        b'D' => LineKey::Left,
        b'H' => LineKey::Home,
        b'F' => LineKey::End,
        b'~' => match params.split(';').next().unwrap_or_default() {
            "1" | "7" => LineKey::Home,
            "4" | "8" => LineKey::End,
            "3" if word => LineKey::WordDelete,
            "3" => LineKey::Delete,
            _ => LineKey::Unknown,
        },
        // Kitty/CSI-u: codepoint;modifier u. 8 and 127 are the common
        // Backspace spellings, and 46 is Delete.
        b'u' => match params.split(';').next().unwrap_or_default() {
            "8" | "127" if word => LineKey::WordBackspace,
            "8" | "127" => LineKey::Backspace,
            "46" if word => LineKey::WordDelete,
            "46" => LineKey::Delete,
            _ => LineKey::Unknown,
        },
        _ => LineKey::Unknown,
    };
    (key, final_at + 2)
}

/// Apply an editing or movement key. Returns whether the line or cursor
/// changed. Non-editing keys are left to the modal.
pub fn edit_line(buf: &mut String, cursor: &mut usize, key: LineKey) -> bool {
    *cursor = floor_boundary(buf, (*cursor).min(buf.len()));
    let old_cursor = *cursor;
    let old_len = buf.len();
    match key {
        LineKey::Char(ch) => {
            buf.insert(*cursor, ch);
            *cursor += ch.len_utf8();
        }
        LineKey::Left => *cursor = previous_boundary(buf, *cursor),
        LineKey::Right => *cursor = next_boundary(buf, *cursor),
        LineKey::WordLeft => *cursor = previous_word(buf, *cursor),
        LineKey::WordRight => *cursor = next_word(buf, *cursor),
        LineKey::Home => *cursor = 0,
        LineKey::End => *cursor = buf.len(),
        LineKey::Backspace => {
            let start = previous_boundary(buf, *cursor);
            buf.drain(start..*cursor);
            *cursor = start;
        }
        LineKey::Delete => {
            let end = next_boundary(buf, *cursor);
            buf.drain(*cursor..end);
        }
        LineKey::WordBackspace => {
            let start = previous_word(buf, *cursor);
            buf.drain(start..*cursor);
            *cursor = start;
        }
        LineKey::WordDelete => {
            let end = next_word(buf, *cursor);
            buf.drain(*cursor..end);
        }
        LineKey::DeleteBefore => {
            buf.drain(..*cursor);
            *cursor = 0;
        }
        LineKey::DeleteAfter => {
            buf.truncate(*cursor);
        }
        _ => return false,
    }
    old_cursor != *cursor || old_len != buf.len()
}

fn floor_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index = index.saturating_sub(1);
    }
    index
}

fn previous_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_boundary(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .char_indices()
        .nth(1)
        .map_or(value.len(), |(index, _)| cursor + index)
}

fn is_word_separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '/' | '\\' | '-' | '_' | '.' | ':' | '=')
}

fn previous_word(value: &str, cursor: usize) -> usize {
    let mut chars: Vec<(usize, char)> = value[..cursor].char_indices().collect();
    while chars.last().is_some_and(|(_, ch)| is_word_separator(*ch)) {
        chars.pop();
    }
    while chars.last().is_some_and(|(_, ch)| !is_word_separator(*ch)) {
        chars.pop();
    }
    chars.last().map_or(0, |(index, ch)| index + ch.len_utf8())
}

fn next_word(value: &str, cursor: usize) -> usize {
    let mut iter = value[cursor..].char_indices().peekable();
    while iter.peek().is_some_and(|(_, ch)| is_word_separator(*ch)) {
        iter.next();
    }
    while iter.peek().is_some_and(|(_, ch)| !is_word_separator(*ch)) {
        iter.next();
    }
    iter.peek().map_or(value.len(), |(index, _)| cursor + index)
}

/// Render the part of a line around its cursor with a block cursor inserted.
pub fn line_with_cursor(value: &str, cursor: usize, width: usize) -> String {
    let cursor = floor_boundary(value, cursor.min(value.len()));
    let before: Vec<char> = value[..cursor].chars().collect();
    let after: Vec<char> = value[cursor..].chars().collect();
    let before_keep = width
        .saturating_sub(1)
        .min(before.len())
        .min(width.saturating_sub(after.len().min(width / 3) + 1));
    let start = before.len().saturating_sub(before_keep);
    let mut shown: String = before[start..].iter().collect();
    shown.push('\u{2588}');
    shown.extend(
        after
            .into_iter()
            .take(width.saturating_sub(shown.chars().count())),
    );
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_backspace_and_ctrl_w_delete_words_without_becoming_escape() {
        for bytes in [b"\x1b\x7f".as_slice(), b"\x17".as_slice()] {
            let mut value = "~/Work/Uniterm Desktop".to_string();
            let mut cursor = value.len();
            let (key, used) = decode_key(bytes, 0);
            assert_eq!(used, bytes.len());
            assert_eq!(key, LineKey::WordBackspace);
            assert!(edit_line(&mut value, &mut cursor, key));
            assert_eq!(value, "~/Work/Uniterm ");
        }
    }

    #[test]
    fn modified_arrows_and_delete_follow_words() {
        let mut value = "one two three".to_string();
        let mut cursor = value.len();
        let (left, _) = decode_key(b"\x1b[1;3D", 0);
        edit_line(&mut value, &mut cursor, left);
        assert_eq!(&value[cursor..], "three");
        let (delete, _) = decode_key(b"\x1b[3;5~", 0);
        edit_line(&mut value, &mut cursor, delete);
        assert_eq!(value, "one two ");
    }

    #[test]
    fn decodes_one_complete_utf8_scalar() {
        let bytes = "é".as_bytes();
        assert_eq!(decode_key(bytes, 0), (LineKey::Char('é'), 2));
    }

    #[test]
    fn edits_utf8_only_at_character_boundaries() {
        let mut value = "a馈b".to_string();
        let mut cursor = value.len();
        edit_line(&mut value, &mut cursor, LineKey::Left);
        edit_line(&mut value, &mut cursor, LineKey::Backspace);
        assert_eq!(value, "ab");
    }
}
