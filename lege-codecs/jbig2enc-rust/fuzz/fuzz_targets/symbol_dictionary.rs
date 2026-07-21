#![no_main]
//! Fuzz target: decoding an arbitrary byte sequence as a symbol-dictionary
//! segment payload must never panic, hang, or allocate beyond the configured
//! limits — only a bounded dictionary or a typed error.
//!
//! Run (needs nightly + `cargo install cargo-fuzz`):
//!   cargo +nightly fuzz run symbol_dictionary
//! Seed corpus: the symbol-dictionary segment payloads emitted by the encoder's
//! `symbol` / `sym_unify` streams (see tests/decode_roundtrip.rs).

use libfuzzer_sys::fuzz_target;

use jbig2enc_rust::decode::integer::IntegerContexts;
use jbig2enc_rust::decode::symbol_dictionary::decode_symbol_dictionary;
use jbig2enc_rust::decode::DecodeLimits;
use jbig2enc_rust::shared::mq_table::MqContext;

fuzz_target!(|data: &[u8]| {
    let limits = DecodeLimits {
        max_width: 1 << 12,
        max_height: 1 << 12,
        max_symbols: 4096,
        max_symbol_pixels: 1 << 20,
        max_total_dictionary_pixels: 1 << 24,
        ..DecodeLimits::default()
    };
    let mut int_ctx = IntegerContexts::default();
    let mut generic_ctx = vec![MqContext::default(); 1usize << 16];
    let _ = decode_symbol_dictionary(data, &[], &limits, &mut int_ctx, &mut generic_ctx);
});
