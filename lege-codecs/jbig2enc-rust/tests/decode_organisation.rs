//! Phase 5f: random-access file organisation (T.88 §D.2).
//!
//! The encoder emits only the sequential organisation, so a random-access file
//! (all headers first, then data) is built with the test-only writer and
//! checked native == source == jbig2dec.

mod common;

use common::writer::{TestBitmap, nominal_at, random_access_generic_file};
use jbig2enc_rust::decode::{DecodeOptions, decode_file};

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
fn random_access_generic_page() {
    let opts = DecodeOptions::default();
    for &(w, h) in &[(40u32, 24u32), (17, 33)] {
        let bm = random_bitmap(w, h, w * 3 + h);
        let file = random_access_generic_file(&bm, 0, &nominal_at(0), false);

        // Native decode of the standalone (random-access) file.
        let doc = decode_file(&file, &opts).expect("native decode");
        let got = doc.first_page().expect("a page");
        assert_eq!(got.width(), w);
        assert_eq!(got.height(), h);
        for y in 0..h {
            for x in 0..w {
                assert_eq!(got.get(x, y), bm.get(x, y), "native ({x},{y})");
            }
        }

        // jbig2dec on the same standalone file.
        if let Some(res) = common::oracle::decode_standalone(&file) {
            let pbm = res.expect("jbig2dec decode");
            assert_eq!(pbm.width, w);
            assert_eq!(pbm.height, h);
            for y in 0..h {
                for x in 0..w {
                    assert_eq!(pbm.get(x, y) != 0, bm.get(x, y), "oracle ({x},{y})");
                }
            }
        }
    }
}
