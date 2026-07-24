//! Phase 5d: striped pages (unknown height) and unknown-length generic regions.
//!
//! Neither form is emitted by this crate's encoder, so both are built with the
//! test-only writer and checked against the source bitmap and jbig2dec.

mod common;

use common::writer::{
    TestBitmap, standalone_file, striped_unknown_height_page, unknown_length_generic_page,
};
use jbig2enc_rust::decode::{DecodeOptions, decode_embedded};

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

fn opts() -> DecodeOptions {
    DecodeOptions::default()
}

fn assert_native_pixels(stream: &[u8], expect: &TestBitmap, label: &str) {
    let got = decode_embedded(None, stream, &opts())
        .unwrap_or_else(|e| panic!("{label}: native decode failed: {e}"));
    assert_eq!(got.width(), expect.width, "{label}: width");
    assert_eq!(got.height(), expect.height, "{label}: height");
    for y in 0..expect.height {
        for x in 0..expect.width {
            assert_eq!(got.get(x, y), expect.get(x, y), "{label}: pixel ({x},{y})");
        }
    }
}

fn assert_oracle_pixels(stream: &[u8], expect: &TestBitmap, label: &str) {
    let file = standalone_file(stream);
    let Some(res) = common::oracle::decode_standalone(&file) else {
        return;
    };
    let pbm = res.unwrap_or_else(|e| panic!("{label}: jbig2dec failed: {e}"));
    assert_eq!(pbm.width, expect.width, "{label}: oracle width");
    assert_eq!(pbm.height, expect.height, "{label}: oracle height");
    for y in 0..expect.height {
        for x in 0..expect.width {
            assert_eq!(
                pbm.get(x, y) != 0,
                expect.get(x, y),
                "{label}: oracle pixel ({x},{y})"
            );
        }
    }
}

#[test]
fn unknown_length_generic() {
    for &(w, h) in &[(37u32, 20u32), (64, 8), (17, 33)] {
        let bm = random_bitmap(w, h, w * 7 + h);
        let stream = unknown_length_generic_page(&bm);
        assert_native_pixels(&stream, &bm, "unknown-length");
        assert_oracle_pixels(&stream, &bm, "unknown-length");
    }
}

#[test]
fn striped_unknown_height() {
    // Three vertically stacked bands form a page of originally-unknown height.
    let bands = vec![
        random_bitmap(40, 10, 1),
        random_bitmap(40, 12, 2),
        random_bitmap(40, 8, 3),
    ];
    let stream = striped_unknown_height_page(40, &bands);

    // Expected page: bands stacked vertically, total height 30.
    let total_h: u32 = bands.iter().map(|b| b.height).sum();
    let mut expect = TestBitmap::new(40, total_h);
    let mut y0 = 0;
    for b in &bands {
        for y in 0..b.height {
            for x in 0..b.width {
                if b.get(x, y) {
                    expect.set(x, y0 + y, true);
                }
            }
        }
        y0 += b.height;
    }

    assert_native_pixels(&stream, &expect, "striped");
    assert_oracle_pixels(&stream, &expect, "striped");
}
