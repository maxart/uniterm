//! On-demand live emulator checkpoint. The VTE parser retains partial sequences
//! in its own state. Normal PTY reads need no handoff-specific work.

use super::{EmuState, Grid, Parser, Terminal};
use bincode::Options;
use std::io::{self, Write};

const MAGIC: &[u8] = b"UTTERM\x02";
const LIMIT: usize = 64 * 1024 * 1024;

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

// Bincode pre-scans the entire value when its serializer has a size limit.
// Bound the output instead, so a live grid and its history are visited once.
// Buffer small writes so the limit check runs per chunk rather than per cell.
struct LimitedBytes<const MAX: usize>(Vec<u8>);

impl<const MAX: usize> Write for LimitedBytes<MAX> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX - self.0.len() {
            return Err(invalid(
                "terminal checkpoint exceeds the 64 MiB live update limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Terminal {
    /// Capture the live emulator, including undelivered responses and damage,
    /// for a compatible engine on a private handoff channel.
    pub fn live_checkpoint(&self) -> io::Result<Vec<u8>> {
        let parser = self
            .parser
            .checkpoint()
            .ok_or_else(|| invalid("invalid partial UTF-8 state"))?;
        let payload = (&self.grid, &self.st, self.scrollback_limit, parser);
        let mut bytes = io::BufWriter::new(LimitedBytes::<LIMIT>(MAGIC.to_vec()));
        bincode::DefaultOptions::new()
            .serialize_into(&mut bytes, &payload)
            .map_err(invalid)?;
        Ok(bytes.into_inner().map_err(|error| error.into_error())?.0)
    }

    /// Resume at the exact next PTY byte without delivering past callbacks.
    /// Only accept state from the current Workspace's private handoff peer.
    pub fn from_live_checkpoint(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > LIMIT {
            return Err(invalid(
                "terminal checkpoint exceeds the 64 MiB live update limit",
            ));
        }
        let payload = bytes
            .strip_prefix(MAGIC)
            .ok_or_else(|| invalid("unsupported terminal checkpoint"))?;
        let (grid, st, scrollback_limit, parser): (Grid, EmuState, usize, vte::ParserCheckpoint) =
            bincode::DefaultOptions::new()
                .with_limit(payload.len() as u64)
                .deserialize(payload)
                .map_err(invalid)?;
        let valid_grid = |grid: &Grid| {
            grid.width() == st.width && grid.height() == st.height && grid.valid_live_geometry()
        };
        if !valid_grid(&grid)
            || !st.alt_backup.as_ref().is_none_or(valid_grid)
            || st.alt_cursor.is_some() != st.alt_backup.is_some()
            || st
                .alt_cursor
                .is_some_and(|(x, y, ..)| x >= st.width || y >= st.height)
            || st.cx >= st.width
            || st.cy >= st.height
            || st.scroll_top > st.scroll_bottom
            || st.scroll_bottom >= st.height
            || st.tabs.len() != usize::from(st.width)
            || st.keyboard.iter().any(|mode| mode.depth > mode.stack.len())
        {
            return Err(invalid("invalid terminal checkpoint state"));
        }
        let parser =
            Parser::from_checkpoint(parser).ok_or_else(|| invalid("invalid parser checkpoint"))?;
        Ok(Self {
            grid,
            st,
            scrollback_limit,
            parser,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_limit_rejects_the_first_oversized_chunk_without_appending_it() {
        let mut bytes = LimitedBytes::<4>(vec![1]);
        bytes.write_all(&[2, 3]).unwrap();
        assert!(bytes.write_all(&[4, 5]).is_err());
        bytes.write_all(&[4]).unwrap();
        assert_eq!(bytes.0, [1, 2, 3, 4]);
    }

    #[test]
    fn checkpoints_reject_unknown_truncated_trailing_and_invalid_state() {
        let mut term = Terminal::new(5, 3);
        let bytes = term.live_checkpoint().unwrap();
        assert!(Terminal::from_live_checkpoint(b"UTTERM\x01").is_err());
        for end in 0..bytes.len() {
            assert!(Terminal::from_live_checkpoint(&bytes[..end]).is_err());
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert!(Terminal::from_live_checkpoint(&trailing).is_err());
        term.st.cy = 3;
        assert!(Terminal::from_live_checkpoint(&term.live_checkpoint().unwrap()).is_err());
    }

    #[test]
    fn live_checkpoint_validates_parser_ranges_and_restricted_utf8_prefixes() {
        for scalar in [
            '\u{80}',
            '\u{7ff}',
            '\u{800}',
            '\u{d7ff}',
            '\u{e000}',
            '\u{10000}',
            '\u{10ffff}',
        ] {
            let mut utf8 = [0u8; 4];
            let bytes = scalar.encode_utf8(&mut utf8).as_bytes();
            for split in 1..bytes.len() {
                let mut original = Terminal::new(8, 2);
                original.feed(&bytes[..split]);
                let mut restored =
                    Terminal::from_live_checkpoint(&original.live_checkpoint().unwrap()).unwrap();
                restored.feed(&bytes[split..]);
                original.feed(&bytes[split..]);
                assert_eq!(restored.dump_text(), original.dump_text());
            }
        }
        let mut term = Terminal::new(8, 2);
        term.feed(b"\x1b[7:3;");
        let valid = serde_json::to_value(term.parser.checkpoint().unwrap()).unwrap();
        for (field, value) in [
            ("intermediate_idx", serde_json::json!(8)),
            ("osc_num_params", serde_json::json!(99)),
            ("utf8_prefix", serde_json::json!([128])),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(
                Parser::<1024>::from_checkpoint(serde_json::from_value(invalid).unwrap()).is_none()
            );
        }
        let mut invalid = valid;
        invalid["params"]["subparams"][0] = serde_json::json!(0);
        assert!(
            Parser::<1024>::from_checkpoint(serde_json::from_value(invalid).unwrap()).is_none()
        );
    }

    #[test]
    fn a_long_incomplete_sequence_resumes_without_recording_its_payload() {
        let mut term = Terminal::new(5, 3);
        term.feed(b"\x1bPq");
        term.feed(&vec![b'a'; 1024 * 1024]);
        let mut restored =
            Terminal::from_live_checkpoint(&term.live_checkpoint().unwrap()).unwrap();
        term.feed(b"\x1b\\done");
        restored.feed(b"\x1b\\done");
        assert_eq!(restored.dump_text(), term.dump_text());
    }
}
