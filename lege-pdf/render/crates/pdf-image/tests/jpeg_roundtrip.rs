#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! JPEG codec round-trips: vstroebel's encoder produces real
//! baseline streams; the decoder must reconstruct them within lossy-codec
//! tolerance. This pins the Huffman tables, the forward/inverse AAN DCT
//! pair, quantization scaling, and the sampling geometry against each
//! other with no external fixtures.

use pdf_image::jpeg::decode_jpeg;
use pdf_image::jpeg::encoder::write_jpeg;
use pdf_image::{DecodeLimits, DecodedFormat};

/// A gradient + block pattern with enough structure to exercise every AC
/// band while staying compressible.
fn test_rgb(w: usize, h: usize) -> Vec<u8> {
    let mut px = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 3;
            px[i] = ((x * 255) / w.max(1)) as u8;
            px[i + 1] = ((y * 255) / h.max(1)) as u8;
            px[i + 2] = if (x / 8 + y / 8) % 2 == 0 { 200 } else { 40 };
        }
    }
    px
}

fn gray_of(rgb: &[u8]) -> Vec<u8> {
    rgb.chunks_exact(3)
        .map(|p| (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) as u8)
        .collect()
}

/// Mean and max absolute channel error between two equal-length buffers.
fn diff_stats(a: &[u8], b: &[u8]) -> (f64, u8) {
    assert_eq!(a.len(), b.len(), "buffer sizes differ");
    let mut sum = 0u64;
    let mut max = 0u8;
    for (&x, &y) in a.iter().zip(b) {
        let d = x.abs_diff(y);
        sum += d as u64;
        max = max.max(d);
    }
    (sum as f64 / a.len() as f64, max)
}

#[test]
fn grayscale_roundtrip() {
    let (w, h) = (64usize, 48usize);
    let gray = gray_of(&test_rgb(w, h));
    let jpeg = write_jpeg(&gray, w as u16, h as u16, false, 90, false).expect("encode");
    let img = decode_jpeg(&jpeg, &DecodeLimits::default()).expect("decode");
    assert_eq!((img.width, img.height), (w as u32, h as u32));
    assert_eq!(img.format, DecodedFormat::Gray8);
    let (mean, max) = diff_stats(&gray, &img.data);
    assert!(mean < 3.0, "mean error {mean}");
    assert!(max < 40, "max error {max}");
}

#[test]
fn rgb_444_roundtrip() {
    let (w, h) = (64usize, 48usize);
    let rgb = test_rgb(w, h);
    let jpeg = write_jpeg(&rgb, w as u16, h as u16, true, 95, false).expect("encode");
    let img = decode_jpeg(&jpeg, &DecodeLimits::default()).expect("decode");
    assert_eq!((img.width, img.height), (w as u32, h as u32));
    assert_eq!(img.format, DecodedFormat::Rgb8);
    let (mean, max) = diff_stats(&rgb, &img.data);
    assert!(mean < 4.0, "mean error {mean}");
    assert!(max < 48, "max error {max}");
}

#[test]
fn rgb_420_downsampled_roundtrip() {
    let (w, h) = (64usize, 48usize);
    let rgb = test_rgb(w, h);
    let jpeg = write_jpeg(&rgb, w as u16, h as u16, true, 90, true).expect("encode");
    let img = decode_jpeg(&jpeg, &DecodeLimits::default()).expect("decode");
    assert_eq!((img.width, img.height), (w as u32, h as u32));
    assert_eq!(img.format, DecodedFormat::Rgb8);
    // Chroma is quarter resolution: luma must stay tight, chroma edges may
    // ring. Compare luma projections.
    let (mean, _) = diff_stats(&gray_of(&rgb), &gray_of(&img.data));
    assert!(mean < 4.0, "mean luma error {mean}");
}

#[test]
fn non_mcu_multiple_dimensions_roundtrip() {
    // 17x9 exercises right/bottom partial blocks in both the encoder's edge
    // replication and the decoder's padded planes.
    let (w, h) = (17usize, 9usize);
    let gray = gray_of(&test_rgb(w, h));
    let jpeg = write_jpeg(&gray, w as u16, h as u16, false, 92, false).expect("encode");
    let img = decode_jpeg(&jpeg, &DecodeLimits::default()).expect("decode");
    assert_eq!((img.width, img.height), (w as u32, h as u32));
    let (mean, _) = diff_stats(&gray, &img.data);
    assert!(mean < 4.0, "mean error {mean}");
}

#[test]
fn flat_color_is_reconstructed_exactly() {
    // A flat mid-gray survives quantization exactly: DC-only blocks pin the
    // IDCT DC path and the level shift.
    let (w, h) = (32usize, 32usize);
    let gray = vec![128u8; w * h];
    let jpeg = write_jpeg(&gray, w as u16, h as u16, false, 75, false).expect("encode");
    let img = decode_jpeg(&jpeg, &DecodeLimits::default()).expect("decode");
    let (mean, max) = diff_stats(&gray, &img.data);
    assert!(max <= 1, "flat gray mean {mean} max {max}");
}

#[test]
fn truncated_stream_is_tolerated() {
    let (w, h) = (64usize, 48usize);
    let gray = gray_of(&test_rgb(w, h));
    let jpeg = write_jpeg(&gray, w as u16, h as u16, false, 90, false).expect("encode");
    // Cut midway through the entropy-coded data (after SOS): decoding must
    // not panic and must still produce a full-size (partially blank) image.
    let sos = jpeg
        .windows(2)
        .position(|w| w == [0xFF, 0xDA])
        .expect("has SOS");
    let cut = &jpeg[..sos + (jpeg.len() - sos) / 2];
    let img = decode_jpeg(cut, &DecodeLimits::default()).expect("tolerant decode");
    assert_eq!((img.width, img.height), (w as u32, h as u32));
}

#[test]
fn garbage_and_limits_are_typed_errors() {
    assert!(decode_jpeg(b"not a jpeg at all", &DecodeLimits::default()).is_err());
    // Pixel limit enforced from SOF before any allocation.
    let (w, h) = (64usize, 48usize);
    let gray = vec![100u8; w * h];
    let jpeg = write_jpeg(&gray, w as u16, h as u16, false, 80, false).expect("encode");
    let tiny = DecodeLimits {
        max_pixels: 16,
        ..DecodeLimits::default()
    };
    assert!(decode_jpeg(&jpeg, &tiny).is_err());
}

/// `SOF` height `0xFFFF` means the real height arrives in a later DNL marker.
/// When the stream is truncated before that marker the dictionary's `/Height`
/// is the only source left, and without it the image decodes as 65535 rows —
/// the real content squeezed into the top few percent, the rest flat grey.
/// PDFium repairs this in `PatchUpKnownBadHeaderWithInvalidHeight`; so do we,
/// under the same guards (real SOF, height exactly 0xFFFF, widths agree).
#[test]
fn dnl_height_placeholder_falls_back_to_the_dictionary() {
    use pdf_image::jpeg::decode_jpeg_with_descriptor;
    use pdf_image::{ImageDescriptor, StreamFilter};

    let (w, h) = (64usize, 48usize);
    let gray = gray_of(&test_rgb(w, h));
    let mut jpeg = write_jpeg(&gray, w as u16, h as u16, false, 90, false).expect("encode");
    // Overwrite the SOF0 height with the DNL placeholder.
    let sof = jpeg
        .windows(2)
        .position(|p| p == [0xFF, 0xC0])
        .expect("has SOF0");
    jpeg[sof + 5] = 0xFF;
    jpeg[sof + 6] = 0xFF;

    let descriptor = |width: u32, height: u32| ImageDescriptor {
        width,
        height,
        bits_per_component: 8,
        color_space: None,
        is_mask: false,
        interpolate: false,
        filters: vec![StreamFilter::DctDecode],
        object: None,
    };

    // Without the dictionary the placeholder stands and the image is 65535 rows.
    let bare = decode_jpeg(&jpeg, &DecodeLimits::default()).expect("decode");
    assert_eq!(bare.height, 0xFFFF);

    // With it, the dictionary's height wins and the pixels match the original.
    let repaired = decode_jpeg_with_descriptor(
        &jpeg,
        &DecodeLimits::default(),
        Some(&descriptor(w as u32, h as u32)),
    )
    .expect("decode");
    assert_eq!((repaired.width, repaired.height), (w as u32, h as u32));
    let (mean, max) = diff_stats(&gray, &repaired.data);
    assert!(max <= 24, "repaired mean {mean} max {max}");

    // A dictionary whose width disagrees is not trusted: the guard requires the
    // widths to match, so a genuinely 65535-row image is never rewritten.
    let mismatched = decode_jpeg_with_descriptor(
        &jpeg,
        &DecodeLimits::default(),
        Some(&descriptor(w as u32 + 1, h as u32)),
    )
    .expect("decode");
    assert_eq!(mismatched.height, 0xFFFF);
}

// --- scaled decode -----------------------------------------------------------

/// A minified draw decodes at a reduced DCT size instead of full resolution.
/// The reduced raster must be a faithful area-average of the full one, not a
/// point-sampled (aliased) one, so comparing it against a box-filtered full
/// decode is the right check.
#[test]
fn scaled_decode_matches_a_box_filtered_full_decode() {
    let (w, h) = (128usize, 96usize);
    let jpeg = write_jpeg(&test_rgb(w, h), w as u16, h as u16, true, 90, false).unwrap();
    let limits = DecodeLimits::default();

    let full = pdf_image::jpeg::decode_jpeg(&jpeg, &limits).unwrap();
    assert_eq!((full.width, full.height), (w as u32, h as u32));

    for denom in [2usize, 4, 8] {
        let (tw, th) = ((w / denom) as u32, (h / denom) as u32);
        let small =
            pdf_image::jpeg::decode_jpeg_scaled(&jpeg, &limits, None, Some((tw, th))).unwrap();
        assert_eq!(
            (small.width, small.height),
            (tw, th),
            "1/{denom} decode dimensions"
        );

        // Box-filter the full decode down and compare.
        let mut expect = vec![0u8; (tw * th) as usize * 3];
        for y in 0..th as usize {
            for x in 0..tw as usize {
                for c in 0..3 {
                    let mut acc = 0u32;
                    for dy in 0..denom {
                        for dx in 0..denom {
                            let sy = y * denom + dy;
                            let sx = x * denom + dx;
                            acc += full.data[(sy * full.stride) + sx * 3 + c] as u32;
                        }
                    }
                    expect[(y * tw as usize + x) * 3 + c] = (acc / (denom * denom) as u32) as u8;
                }
            }
        }
        let got: Vec<u8> = (0..th as usize)
            .flat_map(|y| small.data[y * small.stride..y * small.stride + tw as usize * 3].to_vec())
            .collect();
        let (mean, max) = diff_stats(&got, &expect);
        assert!(
            mean < 1.5 && max <= 12,
            "1/{denom} decode differs from a box-filtered full decode: mean {mean}, max {max}"
        );
    }
}

/// An image too large to hold at full resolution is reduced until it fits
/// rather than rejected, so the page renders instead of going blank.
#[test]
fn an_oversized_image_is_reduced_rather_than_rejected() {
    let (w, h) = (256usize, 256usize);
    let jpeg = write_jpeg(&test_rgb(w, h), w as u16, h as u16, true, 80, false).unwrap();
    // Budget for a 1/4-scale raster only (64x64 = 4096 px).
    let tight = DecodeLimits {
        max_pixels: 4096,
        ..DecodeLimits::default()
    };

    let out = pdf_image::jpeg::decode_jpeg(&jpeg, &tight).expect("reduced, not rejected");
    assert!(
        (out.width as u64) * (out.height as u64) <= 4096,
        "reduced to {}x{}, over the budget",
        out.width,
        out.height
    );
    assert_eq!((out.width, out.height), (64, 64));

    // Still rejected when even a 1/8 decode cannot fit.
    let hopeless = DecodeLimits {
        max_pixels: 4,
        ..DecodeLimits::default()
    };
    assert!(pdf_image::jpeg::decode_jpeg(&jpeg, &hopeless).is_err());
}

/// A `SOF` that over-declares its height is the same defect as the 0xFFFF
/// placeholder without the sentinel: pdfjs issue10989 codes 10308x6304 but
/// writes height 60000, so every row past the real data decodes to flat
/// mid-grey and swamps the page. Clamp to the dictionary, which is what the
/// placement rectangle is derived from.
#[test]
fn a_sof_height_over_the_dictionary_is_clamped() {
    use pdf_image::jpeg::decode_jpeg_with_descriptor;
    use pdf_image::{ImageDescriptor, StreamFilter};

    let (w, h) = (64usize, 48usize);
    let gray = gray_of(&test_rgb(w, h));
    let mut jpeg = write_jpeg(&gray, w as u16, h as u16, false, 90, false).expect("encode");
    let sof = jpeg
        .windows(2)
        .position(|p| p == [0xFF, 0xC0])
        .expect("has SOF0");
    // Claim 4x the real height (not the 0xFFFF sentinel).
    jpeg[sof + 5] = 0;
    jpeg[sof + 6] = 192;

    let descriptor = |width: u32, height: u32| ImageDescriptor {
        width,
        height,
        bits_per_component: 8,
        color_space: None,
        is_mask: false,
        interpolate: false,
        filters: vec![StreamFilter::DctDecode],
        object: None,
    };

    // Without the dictionary the stream's claim stands.
    let bare = decode_jpeg(&jpeg, &DecodeLimits::default()).expect("decode");
    assert_eq!(bare.height, 192);

    // With it, the height is clamped and the real rows survive intact.
    let clamped = decode_jpeg_with_descriptor(
        &jpeg,
        &DecodeLimits::default(),
        Some(&descriptor(w as u32, h as u32)),
    )
    .expect("decode");
    assert_eq!((clamped.width, clamped.height), (w as u32, h as u32));
    let got: Vec<u8> = (0..h)
        .flat_map(|y| clamped.data[y * clamped.stride..y * clamped.stride + w].to_vec())
        .collect();
    let (mean, _) = diff_stats(&got, &gray);
    assert!(
        mean < 3.0,
        "clamped rows should still be the real image: mean {mean}"
    );

    // One-directional: a SOF *smaller* than the dictionary still wins, since
    // that is the ordinary mismatch and the stream is the authority on it.
    let smaller = decode_jpeg_with_descriptor(
        &jpeg,
        &DecodeLimits::default(),
        Some(&descriptor(w as u32, 400)),
    )
    .expect("decode");
    assert_eq!(
        smaller.height, 192,
        "a larger dictionary height must not win"
    );
}
