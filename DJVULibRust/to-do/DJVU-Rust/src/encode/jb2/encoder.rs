//! The main JB2 encoder facade.
//!
//! This module brings all the individual components (symbol dictionary builder,
//! dictionary encoder, record stream encoder) together to provide a simple
//! public API for encoding a full JB2 page.

use crate::encode::zc::ZEncoder;
use crate::encode::jb2::error::Jb2Error;
use crate::encode::jb2::record::RecordStreamEncoder;
use crate::encode::jb2::symbol_dict::{
    BitImage, ConnectedComponent, SymDictBuilder, SymDictEncoder,
};
use std::io::{Cursor, Write};

// Context partitioning for the JB2 encoder.
// We need separate context pools for different parts of the encoding process.
// These are ALLOCATED context counts, not maximum context values.

// 1. Contexts for direct bitmap coding (allocate a reasonable subset).
const DIRECT_BITMAP_CONTEXTS: u32 = 64; // Much smaller allocation
// 2. Contexts for refinement bitmap coding (allocate a reasonable subset).
const REFINEMENT_BITMAP_CONTEXTS: u32 = 96; // Much smaller allocation
// 3. Contexts for the symbol dictionary's number coder.
const SYM_DICT_NC_CONTEXTS: u32 = 64;
// 4. Contexts for the record stream's number coder.
const RECORD_STREAM_NC_CONTEXTS: u32 = 32;

// Base indices for each context pool.
const DIRECT_BITMAP_BASE: u32 = 0;
const REFINEMENT_BITMAP_BASE: u32 = DIRECT_BITMAP_BASE + DIRECT_BITMAP_CONTEXTS;
const SYM_DICT_NC_BASE: u32 = REFINEMENT_BITMAP_BASE + REFINEMENT_BITMAP_CONTEXTS;
const RECORD_STREAM_NC_BASE: u32 = SYM_DICT_NC_BASE + SYM_DICT_NC_CONTEXTS;

// Total number of contexts required by the arithmetic coder.
const TOTAL_CONTEXTS: u32 = RECORD_STREAM_NC_BASE + RECORD_STREAM_NC_CONTEXTS;

/// The main JB2 encoder.
pub struct JB2Encoder<W: Write> {
    _writer: W,
    sym_dict_encoder: SymDictEncoder,
    dictionary: Vec<BitImage>,
}

impl<W: Write> JB2Encoder<W> {
    /// Creates a new JB2 encoder that writes to the given writer.
    pub fn new(writer: W) -> Self {
        let sym_dict_encoder =
            SymDictEncoder::new(SYM_DICT_NC_BASE, SYM_DICT_NC_CONTEXTS, DIRECT_BITMAP_BASE);
        Self {
            _writer: writer,
            sym_dict_encoder,
            dictionary: Vec::new(),
        }
    }

    /// Encodes a single page from a bitmap image.
    ///
    /// `max_error` controls the aggressiveness of the symbol matching. A higher
    /// value allows more different-looking symbols to be clustered together,
    /// which can improve compression at the cost of quality.
    pub fn encode_page(&mut self, image: &BitImage, max_error: u32) -> Result<Vec<u8>, Jb2Error> {
        // Build dictionary and find connected components
        let mut builder = SymDictBuilder::new(max_error);
        let (dictionary, components) = builder.build(image);

        // Encode the dictionary chunk
        let dict_chunk = self.encode_dictionary_chunk(&dictionary)?;

        // Encode page chunk
        let page_chunk = self.encode_page_chunk(&components)?;

        // Combine chunks
        let mut result = Vec::new();
        result.extend_from_slice(&dict_chunk);
        result.extend_from_slice(&page_chunk);

        Ok(result)
    }

    /// Encodes and writes the JB2DS (dictionary) chunk.
    pub fn encode_dictionary_chunk(&mut self, dictionary: &[BitImage]) -> Result<Vec<u8>, Jb2Error> {
        // Store the dictionary for later use in page encoding.
        self.dictionary = dictionary.to_vec();

        let chunk_data = {
            let mut buffer = Cursor::new(Vec::new());
            {
                let mut ac = ZEncoder::new(&mut buffer, true)?;
                // Create context array for the arithmetic coder
                let mut contexts = vec![0u8; TOTAL_CONTEXTS as usize];
                self.sym_dict_encoder.encode(&mut ac, dictionary, &mut contexts)?;

            }
            buffer.into_inner()
        };

        // Don't add JB2D header - that's only for standalone .jb2 files
        // For DjVu pages, the raw JB2 data goes directly into the stream
        Ok(chunk_data)
    }

    /// Encodes and writes the Sjbz (page data) chunk.
    pub fn encode_page_chunk(
        &mut self,
        components: &[ConnectedComponent],
    ) -> Result<Vec<u8>, Jb2Error> {
        let chunk_data = {
            let mut buffer = Cursor::new(Vec::new());
            {
                let mut ac = ZEncoder::new(&mut buffer, true)?;
                let mut record_encoder = RecordStreamEncoder::new(
                    RECORD_STREAM_NC_BASE,
                    RECORD_STREAM_NC_CONTEXTS,
                    REFINEMENT_BITMAP_BASE,
                );

                // Create context array for the arithmetic coder
                let mut contexts = vec![0u8; TOTAL_CONTEXTS as usize];

                for component in components {
                    let sym_id = component.dict_symbol_index.unwrap_or(0);
                    let reference_symbol = &self.dictionary[sym_id];

                    // Decide whether to use refinement. If the component's bitmap is not an
                    // exact match for the dictionary symbol, we must use refinement.
                    let is_refinement = component.bitmap != *reference_symbol;

                    record_encoder.code_record(
                        &mut ac,
                        component,
                        &self.dictionary,
                        is_refinement,
                        &mut contexts,
                    )?;
                }


            }
            buffer.into_inner()
        };

        // Don't add Sjbz header - that's handled by the page encoder
        // Return raw JB2 data that will be BZZ-compressed
        Ok(chunk_data)
    }
}
