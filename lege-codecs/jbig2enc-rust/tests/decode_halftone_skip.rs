//! Phase 5e: halftone HENABLESKIP (T.88 §6.6.5.1).
//!
//! A halftone region with an axis-aligned grid whose rightmost column of cells
//! falls outside the region, coded with HENABLESKIP=1 (the outside cells are
//! not decoded). Built with the test-only writer and checked native ==
//! expected == jbig2dec.

mod common;

use common::writer::{halftone_skip_page, standalone_file, TestBitmap};
use jbig2enc_rust::decode::{decode_embedded, DecodeOptions};

fn solid(w: u32, h: u32) -> TestBitmap {
    let mut bm = TestBitmap::new(w, h);
    for y in 0..h {
        for x in 0..w {
            bm.set(x, y, true);
        }
    }
    bm
}

#[test]
fn halftone_enable_skip() {
    let hdpw = 4u32;
    // Pattern 0 = blank, pattern 1 = solid 4x4.
    let patterns = vec![TestBitmap::new(hdpw, hdpw), solid(hdpw, hdpw)];
    // Region 16x16; grid 5x4. With HRX = HDPW<<8, x = ng*4, so ng=4 -> x=16 is
    // outside (skipped). y = mg*4, all inside.
    let (w, h) = (16u32, 16u32);
    let (hgw, hgh) = (5u32, 4u32);
    // Pattern index per [mg][ng]; the ng=4 column is skipped and ignored.
    let cell_values: Vec<Vec<bool>> = vec![
        vec![true, false, true, false, true],
        vec![false, true, false, true, false],
        vec![true, true, false, false, true],
        vec![false, false, true, true, false],
    ];

    let stream = halftone_skip_page(w, h, &patterns, hdpw, hgw, hgh, &cell_values);

    // Expected: for each non-skipped cell (ng in 0..4), place its pattern at
    // (ng*4, mg*4). ng=4 is outside and contributes nothing.
    let mut expected = TestBitmap::new(w, h);
    for mg in 0..hgh {
        for ng in 0..4u32 {
            if cell_values[mg as usize][ng as usize] {
                // pattern 1 = solid
                for dy in 0..hdpw {
                    for dx in 0..hdpw {
                        let px = ng * hdpw + dx;
                        let py = mg * hdpw + dy;
                        if px < w && py < h {
                            expected.set(px, py, true);
                        }
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
