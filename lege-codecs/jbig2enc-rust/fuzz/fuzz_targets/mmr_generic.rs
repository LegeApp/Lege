#![no_main]
//! Fuzz target: decoding an arbitrary byte sequence as MMR (Group 4) data, and
//! as a pattern-dictionary segment payload, must never panic, hang, or allocate
//! beyond the configured limits — only a bounded bitmap or a typed error.
//!
//! Run (needs nightly + `cargo install cargo-fuzz`):
//!   cargo +nightly fuzz run mmr_generic
//! Seed corpus: encoder halftone streams' MMR blocks / pattern-dictionary
//! payloads (see tests/decode_halftone.rs).

use libfuzzer_sys::fuzz_target;

use jbig2enc_rust::decode::mmr::{decode_mmr_bitmap, decode_mmr_plane};
use jbig2enc_rust::decode::pattern_dictionary::decode_pattern_dictionary;
use jbig2enc_rust::decode::DecodeLimits;
use jbig2enc_rust::shared::mq_table::MqContext;

fuzz_target!(|data: &[u8]| {
    let limits = DecodeLimits {
        max_width: 1 << 12,
        max_height: 1 << 12,
        max_region_pixels: 1 << 22,
        max_symbols: 4096,
        ..DecodeLimits::default()
    };
    // Derive plausible dimensions from the first bytes so the MMR width/height
    // vary across inputs without being unbounded.
    let w = 1 + (data.first().copied().unwrap_or(1) as u32) % 256;
    let h = 1 + (data.get(1).copied().unwrap_or(1) as u32) % 256;
    let body = data.get(2..).unwrap_or(&[]);
    let _ = decode_mmr_bitmap(body, w, h, &limits);
    let _ = decode_mmr_plane(body, w, h, &limits);

    // Also drive the pattern-dictionary parser (MMR + arithmetic collective
    // bitmaps) over the raw bytes.
    let mut generic_ctx = vec![MqContext::default(); 1usize << 16];
    let _ = decode_pattern_dictionary(data, &limits, &mut generic_ctx);
});
