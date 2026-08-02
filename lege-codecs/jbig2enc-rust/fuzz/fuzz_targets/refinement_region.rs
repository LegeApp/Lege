#![no_main]
//! Fuzz target: parsing and decoding an arbitrary byte sequence as a standalone
//! generic refinement region segment (T.88 §7.4.7) must never panic, hang, or
//! allocate beyond the configured limits — only a bounded bitmap or a typed
//! error. Also drives the low-level refinement decode against a fixed reference.
//!
//! Run (needs nightly + `cargo install cargo-fuzz`):
//!   cargo +nightly fuzz run refinement_region
//! Seed corpus: the refinement region segment payloads from tests/decode_refine.rs
//! and the SBREFINE text-region streams (see tests/decode_roundtrip.rs).

use libfuzzer_sys::fuzz_target;

use jbig2enc_rust::decode::arith::ArithmeticDecoder;
use jbig2enc_rust::decode::refinement::{
    decode_refinement_region, page_reference_window, parse_refinement_region,
    REFINEMENT_CONTEXT_COUNT,
};
use jbig2enc_rust::decode::DecodeLimits;
use jbig2enc_rust::shared::bitmap::MonoBitmap;
use jbig2enc_rust::shared::mq_table::MqContext;

fuzz_target!(|data: &[u8]| {
    let limits = DecodeLimits {
        max_width: 1 << 12,
        max_height: 1 << 12,
        max_page_pixels: 1 << 22,
        ..DecodeLimits::default()
    };

    // 1) Header parse + full refinement-region decode against a page-buffer
    //    window reference (the standalone-segment path from page.rs).
    if let Ok(seg) = parse_refinement_region(data) {
        // A small fixed "page" to window the reference from.
        if let Ok(page) = MonoBitmap::new(64, 48, true, &limits) {
            if let Ok(reference) =
                page_reference_window(&page, seg.x, seg.y, seg.width, seg.height, &limits)
            {
                let mut ctx = vec![MqContext::default(); REFINEMENT_CONTEXT_COUNT];
                let mut dec = ArithmeticDecoder::new(seg.data);
                let _ = decode_refinement_region(
                    &mut dec, &reference, seg.width, seg.height, 0, 0, seg.grat, &mut ctx, &limits,
                );
            }
        }
    }

    // 2) Drive the low-level refinement decode directly on raw bytes against a
    //    fixed reference, so the arithmetic loop is fuzzed even when the header
    //    parse rejects the input.
    if let Ok(reference) = MonoBitmap::new(12, 16, true, &limits) {
        let mut ctx = vec![MqContext::default(); REFINEMENT_CONTEXT_COUNT];
        let mut dec = ArithmeticDecoder::new(data);
        let _ = decode_refinement_region(
            &mut dec,
            &reference,
            12,
            16,
            0,
            0,
            (-1, -1),
            &mut ctx,
            &limits,
        );
    }
});
