//! Uniterm's on-demand parser checkpoint. Normal parser advancement uses
//! upstream's implementation and carries no recording buffer or callbacks.

extern crate alloc;

use self::alloc::vec::Vec;
use crate::{Params, Parser, State, MAX_INTERMEDIATES, MAX_OSC_PARAMS};

/// Parser data needed to consume the next byte on the same terminal.
/// Already-delivered actions belong to the caller's emulator checkpoint.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ParserCheckpoint {
    state: State,
    intermediates: [u8; MAX_INTERMEDIATES],
    intermediate_idx: usize,
    params: Params,
    param: u16,
    osc_raw: Vec<u8>,
    osc_params: [(usize, usize); MAX_OSC_PARAMS],
    osc_num_params: usize,
    ignoring: bool,
    utf8_prefix: Vec<u8>,
}

#[derive(Default)]
struct Scalar(Option<Result<char, ()>>);

impl utf8parse::Receiver for Scalar {
    fn codepoint(&mut self, value: char) {
        self.0 = Some(Ok(value));
    }
    fn invalid_sequence(&mut self) {
        self.0 = Some(Err(()));
    }
}

// A UTF-8 prefix has a unique scalar encoding. Complete a copy to recover
// the original bytes without retaining input during normal parsing. The
// first continuation can have a restricted range (E0/ED/F0/F4); at least
// one of 80 or A0 is legal. All subsequent continuations accept 80.
fn partial_scalar(parser: &utf8parse::Parser) -> Option<Vec<u8>> {
    for first in [0x80, 0xa0] {
        let mut copy = parser.clone();
        let mut scalar = Scalar::default();
        for (i, byte) in [first, 0x80, 0x80].into_iter().enumerate() {
            copy.advance(&mut scalar, byte);
            match scalar.0 {
                Some(Ok(c)) => {
                    let mut bytes = [0; 4];
                    let encoded = c.encode_utf8(&mut bytes).as_bytes();
                    let end = encoded.len().checked_sub(i + 1)?;
                    return Some(encoded[..end].to_vec());
                }
                Some(Err(())) => break,
                None => {}
            }
        }
    }
    None
}

impl<const N: usize> Parser<N> {
    /// Capture at an arbitrary byte boundary, doing work only on request.
    pub fn checkpoint(&self) -> Option<ParserCheckpoint> {
        let utf8_prefix = if matches!(self.state, State::Utf8) {
            partial_scalar(&self.utf8_parser)?
        } else {
            Vec::new()
        };
        Some(ParserCheckpoint {
            state: self.state,
            intermediates: self.intermediates,
            intermediate_idx: self.intermediate_idx,
            params: self.params.clone(),
            param: self.param,
            osc_raw: self.osc_raw.to_vec(),
            osc_params: self.osc_params,
            osc_num_params: self.osc_num_params,
            ignoring: self.ignoring,
            utf8_prefix,
        })
    }

    /// Restore checked lengths and a valid partial scalar without invoking
    /// any previously delivered terminal action.
    pub fn from_checkpoint(cp: ParserCheckpoint) -> Option<Self> {
        if matches!(cp.state, State::Anywhere)
            || cp.intermediate_idx > MAX_INTERMEDIATES
            || !cp.params.valid_checkpoint()
            || cp.osc_num_params > MAX_OSC_PARAMS
            || cp.osc_params[..cp.osc_num_params]
                .iter()
                .any(|&(a, b)| a > b || b > cp.osc_raw.len())
            || cp.utf8_prefix.len() > 3
            || matches!(cp.state, State::Utf8) == cp.utf8_prefix.is_empty()
        {
            return None;
        }
        let mut utf8_parser = utf8parse::Parser::new();
        let mut scalar = Scalar::default();
        for byte in cp.utf8_prefix {
            utf8_parser.advance(&mut scalar, byte);
            if scalar.0.is_some() {
                return None;
            }
        }
        #[cfg(feature = "no_std")]
        let osc_raw = {
            let mut bytes = arrayvec::ArrayVec::new();
            bytes.try_extend_from_slice(&cp.osc_raw).ok()?;
            bytes
        };
        #[cfg(not(feature = "no_std"))]
        let osc_raw = cp.osc_raw;
        Some(Self {
            state: cp.state,
            intermediates: cp.intermediates,
            intermediate_idx: cp.intermediate_idx,
            params: cp.params,
            param: cp.param,
            osc_raw,
            osc_params: cp.osc_params,
            osc_num_params: cp.osc_num_params,
            ignoring: cp.ignoring,
            utf8_parser,
        })
    }
}
