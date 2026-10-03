//! Pure keyboard protocol adaptation shared by the attach client and pane input.
//!
//! The host reports disambiguated keys; only applications that opt in receive
//! those encodings. Legacy shells and Uniterm's own controls keep their usual
//! bytes. Ordinary text is borrowed without allocating.

use std::borrow::Cow;

/// Decode CSI-u keys for a legacy consumer, or decode only a configured
/// prefix byte while preserving every other modified key for the child TUI.
pub fn legacy_keys(input: &[u8], prefix_only: Option<u8>) -> Cow<'_, [u8]> {
    KeyboardInput::default().legacy_keys(input, prefix_only)
}

/// Preserve literal paste data across input reads without buffering its body.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyboardInput {
    pasting: bool,
    marker_position: usize,
}

impl KeyboardInput {
    /// Observe a raw byte and return whether it belongs to a paste or its
    /// delimiters, so prefix shortcuts cannot execute pasted control bytes.
    pub fn observe_byte(&mut self, byte: u8) -> bool {
        let was_pasting = self.pasting;
        let marker = if self.pasting {
            b"\x1b[201~"
        } else {
            b"\x1b[200~"
        };
        if byte == marker[self.marker_position] {
            self.marker_position += 1;
            if self.marker_position == marker.len() {
                self.pasting = !self.pasting;
                self.marker_position = 0;
            }
        } else {
            self.marker_position = usize::from(byte == 0x1b);
        }
        was_pasting || self.pasting || self.marker_position != 0
    }

    /// Decode keys while retaining bracketed paste boundaries between calls.
    pub fn legacy_keys<'a>(&mut self, input: &'a [u8], prefix_only: Option<u8>) -> Cow<'a, [u8]> {
        let mut output: Option<Vec<u8>> = None;
        let mut index = 0;
        while index < input.len() {
            if input[index] != 0x1b || self.pasting {
                self.observe_byte(input[index]);
                if let Some(output) = &mut output {
                    output.push(input[index]);
                }
                index += 1;
                continue;
            }
            if let Some((length, key, modifiers)) = csi_key(&input[index..]) {
                let mut storage = [0; 8];
                if let Some(bytes) = legacy_key(key, modifiers, &mut storage) {
                    if prefix_only.is_none_or(|prefix| bytes == [prefix]) {
                        let output = output.get_or_insert_with(|| {
                            let mut output = Vec::with_capacity(input.len());
                            output.extend_from_slice(&input[..index]);
                            output
                        });
                        output.extend_from_slice(bytes);
                        self.marker_position = 0;
                        index += length;
                        continue;
                    }
                }
            }
            self.observe_byte(input[index]);
            if let Some(output) = &mut output {
                output.push(input[index]);
            }
            index += 1;
        }
        output.map_or(Cow::Borrowed(input), Cow::Owned)
    }
}

fn csi_key(input: &[u8]) -> Option<(usize, u32, u32)> {
    let tail = input.strip_prefix(b"\x1b[")?;
    let end = tail
        .iter()
        .take(24)
        .position(|b| (0x40..=0x7e).contains(b))?;
    if tail[end] != b'u' {
        return None;
    }
    let mut params = std::str::from_utf8(&tail[..end]).ok()?.split(';');
    let key = params.next()?.parse().ok()?;
    let modifiers = params.next().map_or(Some(1), |value| value.parse().ok())?;
    if params.next().is_some() || modifiers == 0 {
        return None;
    }
    Some((end + 3, key, modifiers - 1))
}

fn legacy_key(key: u32, modifiers: u32, storage: &mut [u8; 8]) -> Option<&[u8]> {
    // Ignore lock bits, but preserve keys with Super/Hyper/Meta, which have
    // no equivalent in the legacy protocol.
    if modifiers & !(7 | 64 | 128) != 0 {
        return None;
    }
    if (57417..=57426).contains(&key) {
        let (parameter, final_byte) = match key {
            57417 => (b'1', b'D'),
            57418 => (b'1', b'C'),
            57419 => (b'1', b'A'),
            57420 => (b'1', b'B'),
            57421 => (b'5', b'~'),
            57422 => (b'6', b'~'),
            57423 => (b'1', b'H'),
            57424 => (b'1', b'F'),
            57425 => (b'2', b'~'),
            _ => (b'3', b'~'),
        };
        storage[..2].copy_from_slice(b"\x1b[");
        let mut length = 2;
        if modifiers & 7 != 0 || final_byte == b'~' {
            storage[length] = parameter;
            length += 1;
        }
        if modifiers & 7 != 0 {
            storage[length] = b';';
            storage[length + 1] = b'1' + (modifiers & 7) as u8;
            length += 2;
        }
        storage[length] = final_byte;
        return Some(&storage[..length + 1]);
    }
    let mut key = match key {
        57399..=57408 => b'0' as u32 + key - 57399,
        57409 => b'.' as u32,
        57410 => b'/' as u32,
        57411 => b'*' as u32,
        57412 => b'-' as u32,
        57413 => b'+' as u32,
        57414 => 13,
        57415 => b'=' as u32,
        57416 => b',' as u32,
        57344..=63743 => return None,
        _ => key,
    };
    if key == 9 && modifiers & 7 == 1 {
        storage[..3].copy_from_slice(b"\x1b[Z");
        return Some(&storage[..3]);
    }
    if modifiers & 4 != 0 {
        key = match key {
            32 | 50 | 64 => 0,
            97..=122 => key - 96,
            65..=90 => key - 64,
            51 | 91 => 27,
            52 | 92 => 28,
            53 | 93 => 29,
            54 | 94 | 126 => 30,
            47 | 55 | 95 => 31,
            56 | 63 => 127,
            127 => 8,
            _ => key,
        };
    } else if modifiers & 1 != 0 {
        if (97..=122).contains(&key) {
            key -= 32;
        } else if let Some(index) = b"`1234567890-=[]\\;',./"
            .iter()
            .position(|&byte| u32::from(byte) == key)
        {
            key = u32::from(b"~!@#$%^&*()_+{}|:\"<>?"[index]);
        }
    }
    let character = char::from_u32(key)?;
    let offset = usize::from(modifiers & 2 != 0);
    if offset != 0 {
        storage[0] = 0x1b;
    }
    let length = character.encode_utf8(&mut storage[offset..]).len();
    Some(&storage[..offset + length])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_input_and_modified_child_keys_are_borrowed() {
        for input in [b"text\r\x03".as_slice(), b"\x1b[13;2u", b"\x1b[99;5u"] {
            assert!(matches!(legacy_keys(input, Some(1)), Cow::Borrowed(_)));
        }
        assert!(matches!(
            legacy_keys(b"ordinary text", None),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn legacy_controls_and_prefix_preserve_key_order() {
        let input = b"hello\x1b[97;5u\x1b[13;2u\x1b[99;5u\x1b[27u";
        assert_eq!(
            legacy_keys(input, Some(1)),
            b"hello\x01\x1b[13;2u\x1b[99;5u\x1b[27u".as_slice()
        );
        assert_eq!(legacy_keys(input, None), b"hello\x01\r\x03\x1b".as_slice());
        assert_eq!(
            legacy_keys(b"\x1b[105;5u\x1b[120;3u\x1b[9;2u\x1b[57414u", None),
            b"\t\x1bx\x1b[Z\r".as_slice()
        );
    }

    #[test]
    fn legacy_keypad_and_shifted_alt_keys_retain_their_meaning() {
        assert_eq!(
            legacy_keys(b"\x1b[57419u\x1b[57421;5u\x1b[57425u", None),
            b"\x1b[A\x1b[5;5~\x1b[2~".as_slice()
        );
        assert_eq!(
            legacy_keys(b"\x1b[49;4u\x1b[97;4u", None),
            b"\x1b!\x1bA".as_slice()
        );
    }

    #[test]
    fn pasted_escape_codes_remain_literal_across_every_read_boundary() {
        let paste = b"\x1b[200~literal \x1b[97;5ud\x01d\x1b[13;2u\x1b[201~";
        for split in 0..=paste.len() {
            let mut keyboard = KeyboardInput::default();
            let mut output = keyboard.legacy_keys(&paste[..split], None).into_owned();
            output.extend_from_slice(&keyboard.legacy_keys(&paste[split..], None));
            assert_eq!(output, paste, "split {split}");
            assert_eq!(
                keyboard.legacy_keys(b"\x1b[99;5u", None),
                b"\x03".as_slice()
            );
        }
        let mut keyboard = KeyboardInput::default();
        for byte in paste {
            assert_eq!(keyboard.legacy_keys(&[*byte], Some(1)), [*byte].as_slice());
        }
        assert_eq!(
            keyboard.legacy_keys(b"\x1b[97;5u", Some(1)),
            b"\x01".as_slice()
        );
    }

    #[test]
    fn malformed_or_unimplemented_keys_are_preserved() {
        for input in [
            b"\x1b[999999999999999999u".as_slice(),
            b"\x1b[13;0u",
            b"\x1b[13;2",
            b"\x1b[?1u",
            b"\x1b[97;9u",
        ] {
            assert!(matches!(legacy_keys(input, None), Cow::Borrowed(_)));
        }
    }
}
