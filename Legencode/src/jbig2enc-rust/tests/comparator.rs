// Integration tests for Comparator in JBIG2 encoder
use jbig2enc_rust as jbig2;
use jbig2enc_rust::jbig2comparator::Comparator;
use jbig2enc_rust::jbig2sym::BitImage;

use bitvec::prelude::*;
fn make_bitimage_from_bits(width: usize, height: usize, bits: &[u8]) -> BitImage {
    let bv = BitVec::<u8, Msb0>::from_slice(bits);
    BitImage::from_bits(width, height, bv.as_bitslice())
}
/// Brute-force, byte-per-pixel reference implementation that
/// mimics the algorithm used by `Comparator::distance`.
fn slow_distance(a: &BitImage, b: &BitImage, search_radius: i32, max_err: u32) -> u32 {
    assert_eq!(a.width, b.width);
    assert_eq!(a.height, b.height);

    let w = a.width as i32;
    let h = a.height as i32;
    let mut best_err = max_err;

    for dy in -search_radius..=search_radius {
        for dx in -search_radius..=search_radius {
            let mut err = 0_u32;

            // overlap rectangle after shifting (dx,dy)
            let x0 = 0.max(-dx);
            let y0 = 0.max(-dy);
            let x1 = w.min(w - dx);
            let y1 = h.min(h - dy);

            for y in y0..y1 {
                for x in x0..x1 {
                    if a.get(x as u32, y as u32) != b.get((x + dx) as u32, (y + dy) as u32) {
                        err += 1;
                        if err >= best_err {
                            break;
                        }
                    }
                }
                if err >= best_err {
                    break;
                }
            }
            best_err = best_err.min(err);
        }
    }
    best_err
}

/// Helper that converts a compact bitmap written as rows of `0`/`1` chars.
fn img_from_strings(rows: &[&str]) -> BitImage {
    let h = rows.len();
    let w = rows[0].len();
    let mut img = BitImage::new(w as u32, h as u32).unwrap();
    for (y, row) in rows.iter().enumerate() {
        for (x, ch) in row.chars().enumerate() {
            if ch == '1' {
                img.set(x as u32, y as u32, true);
            }
        }
    }
    img
}

#[test]
fn fast_and_slow_distance_match() {
    const R: i32 = 2; // SEARCH_RADIUS in production code
    const MAX_ERR: u32 = 10_000;

    // A few representative glyph pairs: identical, shifted, mixed.
    let fixtures = vec![
        (
            img_from_strings(&["11110000", "11110000", "00001111", "00001111"]),
            img_from_strings(&["11110000", "11110000", "00001111", "00001111"]),
        ),
        (
            img_from_strings(&["11110000", "11110000", "00001111", "00001111"]),
            img_from_strings(&["00111100", "00111100", "11000011", "11000011"]),
        ),
        (
            img_from_strings(&["11001100", "11001100", "00110011", "00110011"]),
            img_from_strings(&["00110011", "00110011", "11001100", "11001100"]),
        ),
    ];

    for (a, b) in fixtures {
        let mut comparator = Comparator::default();
        let fast = comparator.distance(&a, &b, MAX_ERR);
        let slow = slow_distance(&a, &b, R, MAX_ERR);
        let fast_err = fast.map(|(err, _, _)| err);
        assert_eq!(
            fast_err,
            Some(slow),
            "Mismatch for fixture\nfast = {:?}, slow = {:?}",
            fast_err,
            slow
        );
    }
}

#[test]
fn identical_images_have_zero_distance() {
    let a = make_bitimage_from_bits(4, 4, &[0b10101010, 0b01010101]);
    let mut c = Comparator::default();
    let result = c.distance(&a, &a, 0);
    assert_eq!(result, Some((0, 0, 0)));
}

#[test]
fn distance_none_for_too_different() {
    let a = make_bitimage_from_bits(4, 4, &[0b11111111, 0b11111111]);
    let b = make_bitimage_from_bits(4, 4, &[0b00000000, 0b00000000]);
    let mut c = Comparator::default();
    let result = c.distance(&a, &b, 1);
    assert_eq!(result, None);
}

#[test]
fn distance_is_symmetric_for_simple_cases() {
    let a = make_bitimage_from_bits(4, 4, &[0b11110000, 0b00001111]);
    let b = make_bitimage_from_bits(4, 4, &[0b11001100, 0b00110011]);
    let mut c = Comparator::default();
    let err_ab = c.distance(&a, &b, 1000).unwrap().0;
    let err_ba = c.distance(&b, &a, 1000).unwrap().0;
    assert_eq!(err_ab, err_ba);
}
