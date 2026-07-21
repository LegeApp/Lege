#![no_main]
//! Fuzz target: decoding an arbitrary byte sequence as a text-region segment
//! payload (against a small fixed symbol set) must never panic, hang, or
//! allocate beyond the configured limits — only a bounded region or a typed
//! error.
//!
//! Run (needs nightly + `cargo install cargo-fuzz`):
//!   cargo +nightly fuzz run text_region
//! Seed corpus: the text-region segment payloads emitted by the encoder's
//! `symbol` / `sym_unify` streams (see tests/decode_roundtrip.rs).

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;

use jbig2enc_rust::decode::iaid::IaidContexts;
use jbig2enc_rust::decode::integer::IntegerContexts;
use jbig2enc_rust::decode::text_region::decode_text_region;
use jbig2enc_rust::decode::DecodeLimits;
use jbig2enc_rust::shared::bitmap::MonoBitmap;
use jbig2enc_rust::shared::mq_table::MqContext;

fuzz_target!(|data: &[u8]| {
    let limits = DecodeLimits {
        max_width: 1 << 12,
        max_height: 1 << 12,
        max_symbols: 4096,
        ..DecodeLimits::default()
    };

    // A small fixed symbol set so symbol-id decoding has something to resolve.
    let mut symbols: Vec<Arc<MonoBitmap>> = Vec::new();
    for w in [3u32, 5, 8, 4] {
        if let Ok(bm) = MonoBitmap::new(w, 6, true, &limits) {
            symbols.push(Arc::new(bm));
        }
    }

    let mut int_ctx = IntegerContexts::default();
    let mut iaid_ctx = IaidContexts::default();
    let mut refine_ctx = vec![MqContext::default(); 1usize << 13];
    let _ = decode_text_region(data, &symbols, &limits, &mut int_ctx, &mut iaid_ctx, &mut refine_ctx);
});
