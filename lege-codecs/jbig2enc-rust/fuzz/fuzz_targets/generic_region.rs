#![no_main]
//! Fuzz target: decoding an arbitrary byte sequence as a whole document (which
//! exercises the generic-region path) must never panic, hang, or allocate
//! beyond the configured limits — only a bounded image or a typed error.
//!
//! Run (needs nightly + `cargo install cargo-fuzz`):
//!   cargo +nightly fuzz run generic_region
//! Seed corpus: encoder generic/lossless streams (see the round-trip tests).

use libfuzzer_sys::fuzz_target;

use jbig2enc_rust::decode::{DecodeOptions, DecodeLimits, decode_embedded, decode_file};

fuzz_target!(|data: &[u8]| {
    // Tight limits keep a malicious size field from attempting a huge alloc
    // while still covering the encoder's real streams.
    let opts = DecodeOptions::with_limits(DecodeLimits {
        max_width: 1 << 14,
        max_height: 1 << 14,
        max_page_pixels: 64 * 1024 * 1024,
        max_region_pixels: 64 * 1024 * 1024,
        ..DecodeLimits::default()
    });
    let _ = decode_file(data, &opts);
    let _ = decode_embedded(None, data, &opts);
});
