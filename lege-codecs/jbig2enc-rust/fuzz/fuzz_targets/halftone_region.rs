#![no_main]
//! Fuzz target: decoding an arbitrary byte sequence as a whole document (which
//! exercises the pattern-dictionary + halftone-region path) must never panic,
//! hang, or allocate beyond the configured limits — only a bounded image or a
//! typed error.
//!
//! Run (needs nightly + `cargo install cargo-fuzz`):
//!   cargo +nightly fuzz run halftone_region
//! Seed corpus: encoder halftone streams (see tests/decode_halftone.rs).

use libfuzzer_sys::fuzz_target;

use jbig2enc_rust::decode::{decode_embedded, decode_file, DecodeLimits, DecodeOptions};

fuzz_target!(|data: &[u8]| {
    let opts = DecodeOptions::with_limits(DecodeLimits {
        max_width: 1 << 13,
        max_height: 1 << 13,
        max_page_pixels: 32 * 1024 * 1024,
        max_region_pixels: 32 * 1024 * 1024,
        max_symbols: 4096,
        ..DecodeLimits::default()
    });
    let _ = decode_file(data, &opts);
    let _ = decode_embedded(None, data, &opts);
});
