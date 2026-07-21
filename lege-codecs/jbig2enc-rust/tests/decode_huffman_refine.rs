//! Phase 5e: Huffman text regions with refinement (SBHUFF=1 ∧ SBREFINE=1).
//!
//! The symbol IDs and positions are Huffman-coded while each refinement bitmap
//! is a byte-aligned arithmetic block sized by SBHUFFRSIZE (§6.4.11.5).
//!
//! Verification note: unlike every other Phase 5 feature, this one is checked
//! by native round-trip only, NOT against jbig2dec. jbig2dec 0.20's Huffman
//! text-region path is broken here — it does not consume the SBRAT field
//! (§7.4.3.1.3), which the spec's Figure-35 field order places before
//! SBNUMINSTANCES, so it mis-reads the symbol-ID runcode table and fails. (Its
//! *arithmetic* refine path reads SBRAT correctly — see decode_refine.) This
//! decoder follows the documented field order; the refinement bitmap itself is
//! decoded by the generic refinement decoder that is jbig2dec-verified
//! elsewhere.

mod common;

use common::writer::{huffman_refine_text_page, standalone_file, TestBitmap};
use jbig2enc_rust::decode::{decode_embedded, DecodeOptions};

fn glyph(w: u32, h: u32, seed: u32) -> TestBitmap {
    let mut bm = TestBitmap::new(w, h);
    let mut s = seed.wrapping_add(1);
    for y in 0..h {
        for x in 0..w {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            if (s >> 16) & 1 == 1 || x == 0 || y == 0 {
                bm.set(x, y, true);
            }
        }
    }
    bm
}

fn toggle(base: &TestBitmap, x: u32, y: u32) -> TestBitmap {
    let mut t = base.clone();
    t.set(x, y, !base.get(x, y));
    t
}

#[test]
fn huffman_text_with_refinement() {
    // Base symbols, all same height / non-decreasing width (dict writer rules).
    let base = vec![glyph(5, 8, 1), glyph(6, 8, 2)];
    // Two refined instances placed left to right.
    let instances = vec![
        (0usize, 1i32, toggle(&base[0], 2, 3)),
        (1usize, 9i32, toggle(&base[1], 3, 5)),
    ];
    let (w, h) = (24u32, 12u32);

    let stream = huffman_refine_text_page(w, h, &base, &instances);

    // Expected: each refined target at (s, 0).
    let mut expected = TestBitmap::new(w, h);
    for (_, s, target) in &instances {
        for gy in 0..target.height {
            for gx in 0..target.width {
                if target.get(gx, gy) {
                    let px = s + gx as i32;
                    if px >= 0 && (px as u32) < w {
                        expected.set(px as u32, gy, true);
                    }
                }
            }
        }
    }

    let opts = DecodeOptions::default();
    let got = decode_embedded(None, &stream, &opts)
        .unwrap_or_else(|e| panic!("native decode failed: {e}"));
    for y in 0..h {
        for x in 0..w {
            assert_eq!(got.get(x, y), expected.get(x, y), "native ({x},{y})");
        }
    }
    // No jbig2dec oracle check here — see the module doc comment: jbig2dec 0.20
    // cannot decode Huffman+refinement text regions.
    let _ = standalone_file(&stream);
}
