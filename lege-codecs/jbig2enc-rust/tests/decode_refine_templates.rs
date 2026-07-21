//! Phase 5e: GRTEMPLATE-1 refinement and TPGRON typical prediction.
//!
//! The encoder emits only GRTEMPLATE-0 without TPGRON, so these are built with
//! the test-only refinement writer: a page painted with a generic region
//! (the reference) then refined into a target. Checked native == target and,
//! at region offset (0,0) where jbig2dec agrees, native == jbig2dec.

mod common;

use common::writer::{refinement_page, standalone_file, TestBitmap};
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

fn toggled(base: &TestBitmap, changes: &[(u32, u32)]) -> TestBitmap {
    let mut t = base.clone();
    for &(x, y) in changes {
        t.set(x, y, !base.get(x, y));
    }
    t
}

fn opts() -> DecodeOptions {
    DecodeOptions::default()
}

fn check(reference: &TestBitmap, target: &TestBitmap, grtemplate: u8, tpgron: bool, label: &str) {
    let stream = refinement_page(reference, target, grtemplate, tpgron);
    let got = decode_embedded(None, &stream, &opts())
        .unwrap_or_else(|e| panic!("{label}: native decode failed: {e}"));
    for y in 0..target.height {
        for x in 0..target.width {
            assert_eq!(got.get(x, y), target.get(x, y), "{label}: native ({x},{y})");
        }
    }
    let file = standalone_file(&stream);
    if let Some(res) = common::oracle::decode_standalone(&file) {
        let pbm = res.unwrap_or_else(|e| panic!("{label}: jbig2dec failed: {e}"));
        for y in 0..target.height {
            for x in 0..target.width {
                assert_eq!(
                    pbm.get(x, y) != 0,
                    target.get(x, y),
                    "{label}: oracle ({x},{y})"
                );
            }
        }
    }
}

#[test]
fn grtemplate1_no_tpgron() {
    let reference = bordered(20, 16, 1);
    let target = toggled(&reference, &[(5, 5), (8, 9), (12, 3), (2, 11)]);
    check(&reference, &target, 1, false, "gr1");
}

#[test]
fn grtemplate0_tpgron() {
    let reference = bordered(24, 18, 2);
    let target = toggled(&reference, &[(6, 6), (10, 10), (15, 4)]);
    check(&reference, &target, 0, true, "gr0-tpgron");
}

#[test]
fn grtemplate1_tpgron() {
    let reference = bordered(22, 20, 3);
    let target = toggled(&reference, &[(7, 7), (11, 12), (3, 15)]);
    check(&reference, &target, 1, true, "gr1-tpgron");
}

#[test]
fn identity_refinements() {
    // Target == reference exercises the pure typical-prediction path under TPGRON.
    let reference = bordered(18, 14, 4);
    check(&reference, &reference, 0, true, "identity-gr0-tpgron");
    check(&reference, &reference, 1, true, "identity-gr1-tpgron");
}

#[test]
fn grtemplate0_no_tpgron_still_works() {
    // The Phase 3 baseline path must be unaffected by the refactor.
    let reference = bordered(16, 12, 5);
    let target = toggled(&reference, &[(4, 4), (9, 7)]);
    check(&reference, &target, 0, false, "gr0");
}
