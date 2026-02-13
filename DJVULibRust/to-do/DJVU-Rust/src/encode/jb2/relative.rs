//! Implements the relative location predictor for JB2 symbol instances.
//!
//! This predictor is a simplified version of the one in JB2, using a smaller
//! set of contexts to encode the (x, y) position of a symbol relative to the
//! previously encoded symbol.

use crate::encode::zc::ZEncoder;
use crate::encode::jb2::error::Jb2Error;
use crate::encode::jb2::num_coder::{NumCoder, BIG_NEGATIVE, BIG_POSITIVE};
use crate::encode::jb2::symbol_dict::BitImage;
use std::io::Write;

/// Contexts used by the relative location predictor.
#[repr(usize)]
pub enum RelLocCtx {
    SameRow,
}

/// The number of distinct contexts used by the relative location predictor.
pub const NUM_CONTEXTS: u32 = 1;

/// Predicts and encodes the relative location of symbols.
pub struct RelLocPredictor {
    // Last seen coordinates
    last_x: i32,
    last_y: i32,
    // Base index for our contexts in the main arithmetic coder.
    base_context_index: u32,
    // Context indices for different value types
    ctx_dy: usize,
    ctx_dx: usize,
}

impl RelLocPredictor {
    /// Creates a new relative location predictor.
    pub fn new(base_context_index: u32) -> Self {
        Self {
            last_x: 0,
            last_y: 0,
            base_context_index,
            ctx_dy: base_context_index as usize + 10, // Allocate different context offsets
            ctx_dx: base_context_index as usize + 20,
        }
    }

    /// Resets the predictor's state.
    pub fn reset(&mut self) {
        self.last_x = 0;
        self.last_y = 0;
    }

    /// Predicts the location of a symbol based on its context
    pub fn predict(&self, _x: i32, _y: i32, _sym_id: usize, _dictionary: &[BitImage]) -> (i32, i32) {
        // Simple prediction: use the last seen position
        (self.last_x, self.last_y)
    }

    /// Encodes the location (x, y) relative to the previous one.
    pub fn code_coords<W: Write>(
        &mut self,
        ac: &mut ZEncoder<W>,
        nc: &mut NumCoder,
        contexts: &mut [u8], // Add global context array parameter
        x: i32,
        y: i32,
    ) -> Result<(), Jb2Error> {
        let same_row = y == self.last_y;
        let context = self.base_context_index as usize + RelLocCtx::SameRow as usize;
        ac.encode(same_row, &mut (context as u8))?;

        if same_row {
            // Delta X on the same row
            let dx = x - self.last_x;
            nc.encode_integer(ac, contexts, self.ctx_dx, dx, BIG_NEGATIVE, BIG_POSITIVE)?;
        } else {
            // New row: encode delta Y, then absolute X
            let dy = y - self.last_y;
            nc.encode_integer(ac, contexts, self.ctx_dy, dy, BIG_NEGATIVE, BIG_POSITIVE)?;
            // For a new row, X is coded absolutely.
            nc.encode_integer(ac, contexts, self.ctx_dx, x, 0, BIG_POSITIVE)?;
        }

        self.last_x = x;
        self.last_y = y;
        Ok(())
    }
}
