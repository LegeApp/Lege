//! Phase 5f: DecodeStrictness::Compatible recovery (jbig2decplan.md §20).
//!
//! A well-formed page stream followed by trailing garbage errors in Strict mode
//! but decodes in Compatible mode, recording a RecoveryEvent::TrailingGarbage.

mod common;

use common::writer::{TestBitmap, nominal_at, single_generic_page};
use jbig2enc_rust::decode::{
    DecodeOptions, DecodeStrictness, DecoderContext, RecoveryEvent, decode_embedded_with_context,
};

fn random_bitmap(w: u32, h: u32, seed: u32) -> TestBitmap {
    let mut bm = TestBitmap::new(w, h);
    let mut s = seed.wrapping_add(1);
    for y in 0..h {
        for x in 0..w {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            if (s >> 16) & 1 == 1 {
                bm.set(x, y, true);
            }
        }
    }
    bm
}

#[test]
fn trailing_garbage_strict_vs_compatible() {
    let bm = random_bitmap(40, 24, 5);
    let mut stream = single_generic_page(&bm, 0, &nominal_at(0), false);
    // Append a few stray bytes — too short to parse as a segment header.
    stream.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x01]);

    // Strict: the trailing bytes are a parse error.
    let strict = DecodeOptions::default();
    let mut ctx = DecoderContext::new();
    assert!(
        decode_embedded_with_context(None, &stream, &strict, &mut ctx).is_err(),
        "strict mode must reject trailing garbage"
    );

    // Compatible: the page decodes and a recovery event is recorded.
    let compatible = DecodeOptions {
        strictness: DecodeStrictness::Compatible,
        ..DecodeOptions::default()
    };
    let mut ctx = DecoderContext::new();
    let page = decode_embedded_with_context(None, &stream, &compatible, &mut ctx)
        .expect("compatible mode must recover");
    assert_eq!(page.width(), bm.width);
    for y in 0..bm.height {
        for x in 0..bm.width {
            assert_eq!(page.get(x, y), bm.get(x, y), "pixel ({x},{y})");
        }
    }
    assert_eq!(ctx.recovery_events.len(), 1, "one recovery event expected");
    assert!(
        matches!(
            ctx.recovery_events[0],
            RecoveryEvent::TrailingGarbage { bytes: 5, .. }
        ),
        "unexpected recovery event: {:?}",
        ctx.recovery_events
    );
}

#[test]
fn clean_stream_records_no_recovery() {
    let bm = random_bitmap(24, 16, 9);
    let stream = single_generic_page(&bm, 0, &nominal_at(0), false);
    let compatible = DecodeOptions {
        strictness: DecodeStrictness::Compatible,
        ..DecodeOptions::default()
    };
    let mut ctx = DecoderContext::new();
    let _ = decode_embedded_with_context(None, &stream, &compatible, &mut ctx).unwrap();
    assert!(ctx.recovery_events.is_empty());
}
