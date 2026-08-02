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

use jbig2enc_rust::decode::generic::GenericScratch;
use jbig2enc_rust::decode::iaid::IaidContexts;
use jbig2enc_rust::decode::integer::IntegerContexts;
use jbig2enc_rust::decode::refinement::REFINEMENT_CONTEXT_COUNT;
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
    let mut iaid_ctx = IaidContexts::default();
    let mut generic_ctx = vec![MqContext::default(); 1usize << 16];
    let mut refine_ctx = vec![MqContext::default(); REFINEMENT_CONTEXT_COUNT];
    let mut scratch = GenericScratch::default();
    let _ = decode_symbol_dictionary(
        data,
        &[],
        &[],
        &limits,
        &mut int_ctx,
        &mut iaid_ctx,
        &mut generic_ctx,
        &mut refine_ctx,
        &mut scratch,
        true,
    );
});
