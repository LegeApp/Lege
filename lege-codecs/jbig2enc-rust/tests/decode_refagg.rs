//! Phase 5e: SDREFAGG=1 refinement-coded symbol dictionaries (REFAGGNINST=1).
//!
//! A page defines base symbols (Huffman dictionary), then an SDREFAGG
//! dictionary that codes each new symbol as a refinement of a base symbol, then
//! a text region placing the refined symbols. Checked native == expected ==
//! jbig2dec, where the expected page is built from the known refinement targets.

mod common;

use common::writer::{aggregate_page, refagg_page, standalone_file, TestBitmap};
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
fn refagg_refined_symbols() {
    // Base symbols: same height, non-decreasing width (the dict writers' rules).
    let base = vec![glyph(4, 8, 1), glyph(5, 8, 2), glyph(6, 8, 3)];
    // Each new symbol refines base[i] (RDX=RDY=0) with one pixel toggled.
    let refs = vec![0usize, 1, 2];
    let targets = vec![
        toggle(&base[0], 2, 3),
        toggle(&base[1], 3, 5),
        toggle(&base[2], 4, 2),
    ];
    // Place the three refined symbols left to right.
    let placements = [(0usize, 1i32), (1, 7), (2, 14)];
    let (w, h) = (28u32, 12u32);

    let stream = refagg_page(w, h, &base, &refs, &targets, &placements);

    // Expected: each refined target OR-composited at (s, 0).
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

    let file = standalone_file(&stream);
    if let Some(res) = common::oracle::decode_standalone(&file) {
        let pbm = res.unwrap_or_else(|e| panic!("jbig2dec failed: {e}"));
        for y in 0..h {
            for x in 0..w {
                assert_eq!(pbm.get(x, y) != 0, expected.get(x, y), "oracle ({x},{y})");
            }
        }
    }
}

#[test]
fn refagg_aggregate_symbol() {
    // Two base symbols aggregated (REFAGGNINST=2) into one new symbol, then that
    // aggregate placed by a text region.
    let base = vec![glyph(4, 8, 10), glyph(5, 8, 11)];
    // Aggregate: base 0 at S=0 (x 0..3), base 1 at S=4 (x 4..8). SYMWIDTH = 9.
    let instances = [(0usize, 0i32), (1, 4)];
    let sym_width = 9u32;
    let sym_height = 8u32;
    // Place the aggregate symbol at S=1 and S=12.
    let placements = [(0usize, 1i32), (0, 12)];
    let (w, h) = (28u32, 12u32);

    let stream = aggregate_page(w, h, &base, &instances, sym_width, sym_height, &placements);

    // The aggregate symbol = base0 at (0,0) OR base1 at (4,0), 9x8.
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
    // Expected page: the aggregate placed at each S.
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

    let file = standalone_file(&stream);
    if let Some(res) = common::oracle::decode_standalone(&file) {
        let pbm = res.unwrap_or_else(|e| panic!("jbig2dec failed: {e}"));
        for y in 0..h {
            for x in 0..w {
                assert_eq!(pbm.get(x, y) != 0, expected.get(x, y), "oracle ({x},{y})");
            }
        }
    }
}
