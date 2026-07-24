//! Full spec: Huffman refinement/aggregate symbol dictionary
//! (SDHUFF=1 ∧ SDREFAGG=1, T.88 §6.5.8.2, Figure 25).
//!
//! New symbols are refinement-coded (REFAGGNINST=1) with Huffman-coded deltas
//! and byte-aligned arithmetic refinement blocks. Verification is native
//! round-trip only: jbig2dec 0.20's Huffman refine path does not consume the
//! SBRAT/SDRAT fields and cannot decode this. The check is content-dependent
//! (the refined targets differ from the base symbols), so it exercises the
//! full path end to end against a spec-compliant writer.

mod common;

use common::writer::{TestBitmap, huffman_aggregate_page, huffman_refagg_page};
use jbig2enc_rust::decode::{DecodeOptions, decode_embedded};

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
fn huffman_refinement_dictionary() {
    let base = vec![glyph(5, 8, 1), glyph(6, 8, 2), glyph(7, 8, 3)];
    let refs = vec![0usize, 1, 2];
    let targets = vec![
        toggle(&base[0], 2, 3),
        toggle(&base[1], 3, 5),
        toggle(&base[2], 4, 2),
    ];
    let placements = [(0usize, 1i32), (1, 8), (2, 16)];
    let (w, h) = (32u32, 12u32);

    let stream = huffman_refagg_page(w, h, &base, &refs, &targets, &placements);

    let mut expected = TestBitmap::new(w, h);
    for &(sym, s) in &placements {
        let g = &targets[sym];
        for gy in 0..g.height {
            for gx in 0..g.width {
                if g.get(gx, gy) {
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
}

#[test]
fn huffman_aggregate_dictionary() {
    // REFAGGNINST>1: one new symbol aggregating two base symbols (an internal
    // Huffman text region), then placed by an outer text region.
    let base = vec![glyph(4, 8, 10), glyph(5, 8, 11)];
    let instances = [(0usize, 0i32), (1, 4)]; // base0 at x0..3, base1 at x4..8
    let sym_width = 9u32;
    let sym_height = 8u32;
    let placements = [(0usize, 1i32), (0, 12)];
    let (w, h) = (28u32, 12u32);

    let stream =
        huffman_aggregate_page(w, h, &base, &instances, sym_width, sym_height, &placements);

    // Aggregate symbol = base0 at (0,0) OR base1 at (4,0), 9x8.
    let mut agg = TestBitmap::new(sym_width, sym_height);
    for gy in 0..8 {
        for gx in 0..4 {
            if base[0].get(gx, gy) {
                agg.set(gx, gy, true);
            }
        }
        for gx in 0..5 {
            if base[1].get(gx, gy) {
                agg.set(4 + gx, gy, true);
            }
        }
    }
    let mut expected = TestBitmap::new(w, h);
    for &(_, s) in &placements {
        for gy in 0..agg.height {
            for gx in 0..agg.width {
                if agg.get(gx, gy) {
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
}
