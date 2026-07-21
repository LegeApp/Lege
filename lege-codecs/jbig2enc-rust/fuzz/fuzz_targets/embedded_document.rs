#![no_main]
//! Fuzz target: the full embedded-document pipeline — globals + page stream,
//! both strictness modes (jbig2decplan.md §21.6, Phase 5f). Arbitrary bytes can
//! form any segment type, so this exercises every Phase 5 path (Huffman, striped
//! pages, refinement templates, aggregate/refined dictionaries, retained
//! contexts, intermediate regions, recovery). It must never panic, hang, or
//! allocate beyond the configured limits.
//!
//! Run (needs nightly + `cargo install cargo-fuzz`):
//!   cargo +nightly fuzz run embedded_document
//! Seed corpus: the round-trip and Phase-5 writer streams.

use libfuzzer_sys::fuzz_target;

use jbig2enc_rust::decode::{
    decode_embedded_with_context, decode_globals, DecodeLimits, DecodeOptions, DecodeStrictness,
    DecoderContext,
};

fuzz_target!(|data: &[u8]| {
    let limits = DecodeLimits {
        max_width: 1 << 14,
        max_height: 1 << 14,
        max_page_pixels: 64 * 1024 * 1024,
        max_region_pixels: 64 * 1024 * 1024,
        ..DecodeLimits::default()
    };

    // Split the input into a globals stream and a page stream, so the referred-
    // dictionary / retained-context / Huffman-table cross-segment paths are hit.
    let split = if data.is_empty() { 0 } else { data[0] as usize % (data.len() + 1) };
    let (globals_bytes, page_bytes) = data.split_at(split.min(data.len()));

    for strictness in [DecodeStrictness::Strict, DecodeStrictness::Compatible] {
        let opts = DecodeOptions { limits: limits.clone(), strictness };

        // Decode globals once, then a page against them (mirrors the PDF path).
        let mut ctx = DecoderContext::new();
        if let Ok(globals) = decode_globals(globals_bytes, &opts) {
            let _ = decode_embedded_with_context(
                Some(globals_bytes),
                page_bytes,
                &opts,
                &mut ctx,
            );
            let _ = globals;
        }
        // Also decode the whole input as a bare page stream.
        let _ = decode_embedded_with_context(None, data, &opts, &mut ctx);
    }
});
