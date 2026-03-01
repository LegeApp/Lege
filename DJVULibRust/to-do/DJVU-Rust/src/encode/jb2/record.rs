//! Handles the JB2 record stream state machine.
//!
//! This module is responsible for encoding the sequence of symbol instances
//! that make up the content of a page.

use crate::encode::zc::ZEncoder;
use crate::encode::jb2::context;
use crate::encode::jb2::error::Jb2Error;
use crate::encode::jb2::num_coder::NumCoder;
use crate::encode::jb2::relative::{self, RelLocPredictor};
use crate::encode::jb2::symbol_dict::{BitImage, ConnectedComponent};
use std::io::Write;

/// JB2 record types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordType {
    /// A symbol that is an instance of a dictionary symbol.
    SymbolInstance = 1,
    /// A symbol encoded as a refinement of a dictionary symbol.
    SymbolRefinement = 2,
}

/// Encodes the stream of symbol instance records for a page.
pub struct RecordStreamEncoder {
    nc: NumCoder,
    rlp: RelLocPredictor,
    refinement_base_context: u32,
    // Context handles for specific operations
    ctx_sym_id: usize,
    ctx_rel_loc: usize,
    ctx_rec_type: usize,
}

impl RecordStreamEncoder {
    /// Creates a new record stream encoder.
    /// It requires a base context index to ensure its contexts don't overlap
    /// with other components.
    pub fn new(base_context_index: u32, max_contexts: u32, refinement_base_context: u32) -> Self {
        // Partition the available contexts between the relative location predictor
        // and the general-purpose number coder.
        let rlp_contexts = relative::NUM_CONTEXTS;
        let nc_contexts = max_contexts - rlp_contexts;
        let nc_base_index = base_context_index + rlp_contexts;

        let nc = NumCoder::new(nc_base_index.try_into().unwrap(), nc_contexts.try_into().unwrap());

        // Allocate context indices (not handles)
        let ctx_rec_type = nc_base_index as usize;
        let ctx_sym_id = nc_base_index as usize + 1;
        let ctx_rel_loc = nc_base_index as usize + 2;

        Self {
            nc,
            rlp: RelLocPredictor::new(base_context_index),
            refinement_base_context,
            ctx_rec_type,
            ctx_sym_id,
            ctx_rel_loc,
        }
    }

    /// Encodes a single connected component as a record, potentially as a refinement.
    pub fn code_record<W: Write>(
        &mut self,
        ac: &mut ZEncoder<W>,
        component: &ConnectedComponent,
        dictionary: &[BitImage],
        is_refinement: bool,
        contexts: &mut [u8], // Add global context array parameter
    ) -> Result<(), Jb2Error> {
        let rec_type = if is_refinement {
            RecordType::SymbolRefinement
        } else {
            RecordType::SymbolInstance
        };

        // 1. Encode the record type.
        self.code_rec_type(ac, rec_type, contexts)?;

        // 2. Encode the symbol ID.
        let sym_id = component.dict_symbol_index.unwrap_or(0);
        self.nc.encode_integer(
            ac,
            contexts,
            self.ctx_sym_id,
            sym_id as i32,
            0,
            dictionary.len() as i32 - 1,
        )?;

        // 3. Encode the location (and get the relative offset for refinement).
        // Get the predicted location
        let (pred_dx, pred_dy) = self.rlp.predict(
            component.bounds.x as i32,
            component.bounds.y as i32,
            sym_id,
            dictionary,
        );

        // Encode the difference between actual and predicted location
        let dx = component.bounds.x as i32 - pred_dx;
        let dy = component.bounds.y as i32 - pred_dy;

        // Encode the relative location using reasonable bounds
        self.nc.encode_integer(ac, contexts, self.ctx_rel_loc, dx, -32768, 32767)?;
        self.nc.encode_integer(ac, contexts, self.ctx_rel_loc + 1, dy, -32768, 32767)?;

        // 4. If it's a refinement, encode the actual bitmap differences.
        if is_refinement {
            let reference_symbol = &dictionary[sym_id];
            context::encode_bitmap_refine(
                ac,
                &component.bitmap,
                reference_symbol,
                dx,
                dy,
                self.refinement_base_context as usize,
            )?;
        }

        Ok(())
    }

    /// Encodes the record type using the number coder.
    fn code_rec_type<W: Write>(
        &mut self,
        ac: &mut ZEncoder<W>,
        rec_type: RecordType,
        contexts: &mut [u8],
    ) -> Result<(), Jb2Error> {
        // Encode record type as integer (1 for SymbolInstance, 2 for SymbolRefinement)
        self.nc.encode_integer(
            ac,
            contexts,
            self.ctx_rec_type,
            rec_type as i32,
            1,
            2,
        )
    }
}
