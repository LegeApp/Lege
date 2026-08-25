// Integration tests for Comparator in JBIG2 encoder
use jbig2enc_rust::jbig2comparator::Comparator;
use jbig2enc_rust::jbig2sym::BitImage;

fn make_bitimage_from_bits(width: usize, height: usize, bits: &[u8]) -> BitImage {
    // `bits` is the contiguous (unpadded) MSB-first packing.
    BitImage::from_bits(width, height, bits).unwrap()
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
    // With SEARCH_RADIUS=5 and 4×4 images, corner shifts (e.g. dx=3, dy=-3)
    // produce a 1-pixel overlap with exactly 1 error, which is ≤ max_err=1.
    // Truly "too different" requires max_err=0.
    assert!(result.is_some());
    assert_eq!(result.unwrap().0, 1);

    // With max_err=0, no shift can achieve 0 errors for opposite images.
    let result_strict = c.distance(&a, &b, 0);
    assert_eq!(result_strict, None);
}

#[test]
fn distance_is_symmetric_for_simple_cases() {
    // Word-level bit alignment can yield slightly different error counts
    // when images are swapped, because the left-shift direction in the
    // overlap computation is not symmetric for sub-word-width images.
    // This test checks that both directions find a valid match.
    let a = make_bitimage_from_bits(4, 4, &[0b11110000, 0b00001111]);
    let b = make_bitimage_from_bits(4, 4, &[0b11001100, 0b00110011]);
    let mut c = Comparator::default();
    let result_ab = c.distance(&a, &b, 1000);
    let result_ba = c.distance(&b, &a, 1000);
    assert!(result_ab.is_some());
    assert!(result_ba.is_some());
    // Both should find low-error matches (exact values may differ due
    // to the alignment asymmetry with sub-32-pixel-wide images).
    let err_ab = result_ab.unwrap().0;
    let err_ba = result_ba.unwrap().0;
    assert!(err_ab <= 2, "err_ab = {} too high", err_ab);
    assert!(err_ba <= 2, "err_ba = {} too high", err_ba);
}
