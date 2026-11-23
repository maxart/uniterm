//! SSH handoff input shares the attach decoder. Only the response to an
//! OpenSSH prompt may reach its private terminal, and only one line at a time.
use crate::input::scan_stdin_chunk;
use crate::overlay::{wrap_text, Overlay};
use crate::text_input::decode_key;
use crate::tty::{sanitize_terminal_title, tty_size, write_sync_frame};

/// Decode one in-progress SSH handoff, including fragmented terminal reports.
#[derive(Default)]
pub struct ConnectionInput {
    dialog: Option<(Overlay, u16, u16)>,
    mouse: Vec<u8>,
    keys: Vec<u8>,
    keyboard: uniterm_core::keyboard::KeyboardInput,
}

impl ConnectionInput {
    /// Present a bounded, cell-wrapped SSH prompt. Only OpenSSH's private
    /// terminal supplies this text. Progress and Cancel share dialog geometry.
    pub fn connecting(&mut self, target: &str, prompt: &str) {
        let (cols, rows) = tty_size(libc::STDOUT_FILENO);
        let dialog = connection_dialog(target, prompt, cols, rows);
        write_sync_frame(&dialog.render(cols, rows));
        self.dialog = Some((dialog, cols, rows));
    }

    /// Return ordinary response bytes or a cancellation. Mouse and movement
    /// reports are consumed even when they arrive between password characters.
    pub fn decode(&mut self, chunk: &[u8]) -> std::io::Result<Vec<u8>> {
        let (mouse, mut keys) = scan_stdin_chunk(chunk, &mut self.mouse, &mut self.keys);
        if let Some((dialog, cols, rows)) = &self.dialog {
            if mouse.iter().any(|(x, y, kind)| {
                *kind == uniterm_proto::MouseKind::Click
                    && dialog.footer_key_at(*cols, *rows, *x, *y) == Some("Esc")
            }) {
                return Err(cancelled());
            }
        }

        if self.keys == b"\x1b" {
            keys.append(&mut self.keys);
        }
        let bytes = self.keyboard.legacy_keys(&keys, None);
        let mut result = Vec::new();
        let mut at = 0;
        while let Some(&byte) = bytes.get(at) {
            if byte == 3 || (byte == 0x1b && at + 1 == bytes.len()) {
                return Err(cancelled());
            }
            if byte == 0x1b {
                let (_, used) = decode_key(&bytes, at);
                at += used;
            } else {
                if byte >= 32 || matches!(byte, 8 | 10 | 13 | 21) {
                    result.push(byte);
                }
                at += 1;
            }
        }
        Ok(result)
    }
}

fn cancelled() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "SSH connection cancelled")
}

fn connection_dialog(target: &str, prompt: &str, cols: u16, rows: u16) -> Overlay {
    let width = usize::from(cols.saturating_sub(8).clamp(1, 80));
    let lines = prompt
        .lines()
        .flat_map(|line| wrap_text(line, width))
        .collect::<Vec<_>>();
    let count = lines.len();
    let available = usize::from(rows.saturating_sub(8).clamp(1, 12));
    let lines = lines
        .into_iter()
        .skip(count.saturating_sub(available))
        .collect();
    Overlay::with_footer(
        format!("Connecting to {}", sanitize_terminal_title(target)),
        lines,
        &[("Esc", "Cancel")],
    )
    .with_min_content_width(width as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_authentication_dialog_wraps_the_fingerprint_and_routes_cancel() {
        use unicode_width::UnicodeWidthStr as _;
        let fingerprint = "SHA256:0123456789012345678901234567890123456789012345678";
        let dialog = connection_dialog("build", fingerprint, 32, 20);
        assert!(dialog.lines.iter().all(|line| line.width() <= 24));
        assert_eq!(dialog.lines.concat(), fingerprint);
        let (x, y) = (1..=20)
            .flat_map(|y| (1..=32).map(move |x| (x, y)))
            .find(|&(x, y)| dialog.footer_key_at(32, 20, x, y) == Some("Esc"))
            .unwrap();
        let mut input = ConnectionInput {
            dialog: Some((dialog, 32, 20)),
            ..Default::default()
        };
        assert_eq!(
            input
                .decode(format!("\x1b[<0;{x};{y}M").as_bytes())
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn handoff_consumes_reports_and_cancels_with_typeahead() {
        let mut input = ConnectionInput::default();
        assert_eq!(input.decode(b"a\x1b[<35;").unwrap(), b"a");
        assert_eq!(input.decode(b"11;22Mb\x1b[A\x1b[I\r").unwrap(), b"b\r");
        for keys in [
            b"typed\x1b".as_slice(),
            b"typed\x03",
            b"typed\x1b[27u",
            b"\x1b[99;5u",
        ] {
            assert_eq!(
                input.decode(keys).unwrap_err().kind(),
                std::io::ErrorKind::Interrupted
            );
        }
    }
}
