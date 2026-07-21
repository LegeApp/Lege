//! Phase 5f: intermediate regions + refinement referring to a region's
//! auxiliary buffer (T.88 §7.4.7.4, §8.2).
//!
//! An intermediate generic region is stored (not drawn); an immediate
//! refinement region referring to it uses its bitmap as GRREFERENCE, refines,
//! and composites onto the page.
//!
//! Verification note: native round-trip only. jbig2dec 0.20 reports
//! "intermediate generic region (NYI)" and aborts — it does not implement
//! intermediate regions. The native check is strong: the refinement reproduces
//! the target only if the intermediate region was stored and found as the
//! refinement reference.

mod common;

use common::writer::{intermediate_refine_page, standalone_file, TestBitmap};
use jbig2enc_rust::decode::{decode_embedded, DecodeOptions};

fn bordered(w: u32, h: u32, seed: u32) -> TestBitmap {
    let mut bm = TestBitmap::new(w, h);
    let mut s = seed.wrapping_add(1);
    for y in 0..h {
        for x in 0..w {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            if x == 0 || y == 0 || x == w - 1 || y == h - 1 || (s >> 17) & 1 == 1 {
                bm.set(x, y, true);
            }
        }
    }
    bm
}

#[test]
fn intermediate_region_then_refinement() {
    let reference = bordered(24, 18, 7);
    let mut target = reference.clone();
    for &(x, y) in &[(5u32, 5u32), (10, 10), (15, 4), (3, 13)] {
        target.set(x, y, !reference.get(x, y));
    }

    let stream = intermediate_refine_page(&reference, &target);
    let opts = DecodeOptions::default();

    let got = decode_embedded(None, &stream, &opts)
        .unwrap_or_else(|e| panic!("native decode failed: {e}"));
    assert_eq!(got.width(), target.width);
    assert_eq!(got.height(), target.height);
    for y in 0..target.height {
        for x in 0..target.width {
            assert_eq!(got.get(x, y), target.get(x, y), "native ({x},{y})");
        }
    }

    // No jbig2dec oracle: intermediate regions are NYI in jbig2dec 0.20.
    let _ = standalone_file(&stream);
}
