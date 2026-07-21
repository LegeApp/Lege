//! Phase 5a: generic-region templates 1–3 and TPGDON typical prediction.
//!
//! This crate's encoder only emits template-0, TPGDON-off generic regions, so
//! these forms are exercised with the test-only stream writer
//! (`tests/common/writer.rs`). Every stream is checked two ways:
//!
//! 1. **native round-trip** — `decode_embedded` reproduces the source bitmap
//!    exactly (internal consistency of writer + decoder), and
//! 2. **jbig2dec oracle** — the very same bytes decoded by system jbig2dec
//!    match the source (spec conformance; skipped when jbig2dec is absent).

mod common;

use common::writer::{nominal_at, single_generic_page, standalone_file, TestBitmap};
use jbig2enc_rust::decode::{decode_embedded, DecodeOptions};

/// A pseudo-random but deterministic bitmap.
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

/// A banded bitmap with many identical adjacent rows — exercises the TPGDON
/// duplicate-row path (LTP = 1).
fn banded_bitmap(w: u32, h: u32, seed: u32) -> TestBitmap {
    let mut bm = TestBitmap::new(w, h);
    let mut s = seed.wrapping_add(1);
    let mut y = 0;
    while y < h {
        // Generate one row, then repeat it for a random band height.
        s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let band = 1 + ((s >> 8) % 4);
        let mut row = vec![false; w as usize];
        for cell in row.iter_mut() {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            *cell = (s >> 16) & 1 == 1;
        }
        for _ in 0..band {
            if y >= h {
                break;
            }
            for (x, &v) in row.iter().enumerate() {
                bm.set(x as u32, y, v);
            }
            y += 1;
        }
    }
    bm
}

fn opts() -> DecodeOptions {
    DecodeOptions::default()
}

/// Decode `stream` natively and assert it equals `bm`.
fn assert_native_matches(stream: &[u8], bm: &TestBitmap, label: &str) {
    let got = decode_embedded(None, stream, &opts())
        .unwrap_or_else(|e| panic!("{label}: native decode failed: {e}"));
    assert_eq!(got.width(), bm.width, "{label}: width");
    assert_eq!(got.height(), bm.height, "{label}: height");
    for y in 0..bm.height {
        for x in 0..bm.width {
            assert_eq!(
                got.get(x, y),
                bm.get(x, y),
                "{label}: native pixel ({x},{y})"
            );
        }
    }
}

/// Decode the standalone file wrapper with jbig2dec and assert it equals `bm`.
fn assert_oracle_matches(stream: &[u8], bm: &TestBitmap, label: &str) {
    let file = standalone_file(stream);
    let Some(res) = common::oracle::decode_standalone(&file) else {
        return; // jbig2dec not installed; skip.
    };
    let pbm = res.unwrap_or_else(|e| panic!("{label}: jbig2dec failed: {e}"));
    assert_eq!(pbm.width as u32, bm.width, "{label}: oracle width");
    assert_eq!(pbm.height as u32, bm.height, "{label}: oracle height");
    for y in 0..bm.height {
        for x in 0..bm.width {
            assert_eq!(
                pbm.get(x, y) != 0,
                bm.get(x, y),
                "{label}: oracle pixel ({x},{y})"
            );
        }
    }
}

fn check(bm: &TestBitmap, template: u8, tpgdon: bool, label: &str) {
    let at = nominal_at(template);
    let stream = single_generic_page(bm, template, &at, tpgdon);
    assert_native_matches(&stream, bm, label);
    assert_oracle_matches(&stream, bm, label);
}

#[test]
fn all_templates_random() {
    for template in 0u8..=3 {
        let bm = random_bitmap(37, 24, 100 + template as u32);
        check(&bm, template, false, &format!("random t{template}"));
    }
}

#[test]
fn all_templates_tpgdon() {
    for template in 0u8..=3 {
        let bm = banded_bitmap(40, 30, 200 + template as u32);
        check(&bm, template, true, &format!("tpgdon t{template}"));
    }
}

#[test]
fn odd_widths_all_templates() {
    // The §21.2 odd-width property matrix, rerun per template.
    for template in 0u8..=3 {
        for &w in &[1u32, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65] {
            for &h in &[1u32, 3, 17] {
                let bm = random_bitmap(w, h, w * 31 + h + template as u32 * 7);
                check(&bm, template, false, &format!("odd t{template} {w}x{h}"));
            }
        }
    }
}

#[test]
fn tpgdon_all_duplicate_rows() {
    // A bitmap whose rows are all identical: after the first row every row is a
    // TPGDON duplicate (LTP stays 1).
    for template in 0u8..=3 {
        let mut bm = TestBitmap::new(24, 16);
        for x in (0..24).step_by(3) {
            for y in 0..16 {
                bm.set(x, y, true);
            }
        }
        check(&bm, template, true, &format!("all-dup t{template}"));
    }
}
