#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Wild-format JPEG fixtures, decoded against libjpeg-derived ground truth.
//!
//! The `.jpg` fixtures were produced by independent encoders (libjpeg's
//! cjpeg, Pillow, ImageMagick — see `fixtures/README.md`); each `.truth.bin`
//! is Pillow/libjpeg's decode of that exact file, and `.src.bin` files are
//! the pre-JPEG source pixels. This pins progressive scans, restart
//! markers, 4:2:2/4:2:0 sampling, and the Adobe CMYK/YCCK conventions
//! against the reference implementation rather than against ourselves.
//!
//! Four-channel note: the decoder emits libjpeg's *raw* DeviceCMYK bytes and
//! leaves Adobe's inverted-sample polarity for the PDF `/Decode` array to undo
//! (matching PDFium's `CPDF_DIB` path). So for a conforming Adobe file the raw
//! output relates to the true source by the `/Decode [1 0 …]` inversion
//! (`255 - sample`); these tests apply that inversion before comparing.

use pdf_image::jpeg::decode_jpeg;
use pdf_image::{DecodeLimits, DecodedFormat};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

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

fn decode(name: &str) -> pdf_image::DecodedImage {
    decode_jpeg(&fixture(name), &DecodeLimits::default()).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// Grayscale comparisons are decoder-vs-decoder (no upsampling involved):
/// they must be essentially bit-identical to libjpeg.
fn assert_close_gray(name: &str) {
    let img = decode(&format!("{name}.jpg"));
    assert_eq!(img.format, DecodedFormat::Gray8, "{name}");
    let truth = fixture(&format!("{name}.truth.bin"));
    let (mean, max) = diff_stats(&img.data, &truth);
    assert!(mean < 0.5 && max <= 2, "{name}: mean {mean} max {max} vs libjpeg");
}

/// Color comparisons allow for the chroma-upsampling policy difference
/// (ours nearest, libjpeg "fancy" bilinear).
fn assert_close_color(name: &str, format: DecodedFormat, mean_bound: f64) {
    let img = decode(&format!("{name}.jpg"));
    assert_eq!(img.format, format, "{name}");
    let truth = fixture(&format!("{name}.truth.bin"));
    let (mean, _max) = diff_stats(&img.data, &truth);
    assert!(mean < mean_bound, "{name}: mean {mean} vs libjpeg (bound {mean_bound})");
}

#[test]
fn progressive_grayscale() {
    assert_close_gray("prog_gray");
}

#[test]
fn progressive_ycbcr_420() {
    assert_close_color("prog_rgb420", DecodedFormat::Rgb8, 8.0);
}

#[test]
fn baseline_ycbcr_422() {
    assert_close_color("base_rgb422", DecodedFormat::Rgb8, 8.0);
}

#[test]
fn restart_markers_baseline() {
    assert_close_gray("restart_gray");
}

#[test]
fn restart_markers_progressive() {
    assert_close_gray("prog_restart_gray");
}

#[test]
fn adobe_cmyk_photoshop_convention() {
    // `cmyk.jpg` (Adobe APP14 transform 0) stores INVERTED CMYK samples — the
    // Photoshop/Acrobat convention; `cmyk.src.bin` is the true CMYK source.
    // Like PDFium's DCT path, the decoder emits libjpeg's RAW (still-inverted)
    // samples and defers un-inversion to the PDF `/Decode [1 0 …]` array, so the
    // raw output equals `255 - src`.
    let img = decode("cmyk.jpg");
    assert_eq!(img.format, DecodedFormat::Cmyk8);
    let src = fixture("cmyk.src.bin");
    let normalized: Vec<u8> = img.data.iter().map(|&v| 255 - v).collect();
    let (mean, _) = diff_stats(&normalized, &src);
    assert!(mean < 6.0, "Adobe CMYK (raw, /Decode-normalized): mean {mean}");
}

#[test]
fn adobe_cmyk_noninverted_writer_passes_through() {
    // `cmyk_noninverted.jpg` carries an Adobe marker (transform 0) but stores
    // RAW (non-inverted) true-CMYK samples. libjpeg — and thus we, like PDFium —
    // pass those through unchanged; whether they are later inverted is the PDF
    // `/Decode` array's decision, not the codec's. So the raw output equals the
    // true source directly.
    let img = decode("cmyk_noninverted.jpg");
    let src = fixture("cmyk.src.bin");
    let (mean, _) = diff_stats(&img.data, &src);
    assert!(mean < 6.0, "non-inverted CMYK passes through as true DeviceCMYK: mean {mean}");
}

#[test]
fn adobe_ycck_transform2() {
    // ImageMagick YCCK (Adobe transform 2) of four pure-ink quadrants. libjpeg's
    // `ycck_cmyk_convert` output is raw (Adobe-inverted) CMYK; the PDF
    // `/Decode [1 0 …]` array recovers the true inks. Assert the raw output,
    // normalized by that inversion, matches the true source.
    let img = decode("ycck.jpg");
    assert_eq!(img.format, DecodedFormat::Cmyk8);
    let src = fixture("ycck.src.bin");
    let normalized: Vec<u8> = img.data.iter().map(|&v| 255 - v).collect();
    let (mean, _) = diff_stats(&normalized, &src);
    assert!(mean < 6.0, "YCCK (raw, /Decode-normalized): mean {mean}");
    // Patch centers, once /Decode-normalized (`255 - raw`), are exact inks.
    let w = img.width as usize;
    let px = |x: usize, y: usize| &img.data[(y * w + x) * 4..(y * w + x) * 4 + 4];
    for (want, x, y) in [
        ([255u8, 0, 0, 0], 16, 12),
        ([0, 255, 0, 0], 48, 12),
        ([0, 0, 255, 0], 16, 36),
        ([0, 0, 0, 255], 48, 36),
    ] {
        let got = px(x, y);
        for c in 0..4 {
            let normalized = 255 - got[c];
            assert!(
                normalized.abs_diff(want[c]) <= 4,
                "patch ({x},{y}): 255-{got:?} vs {want:?}"
            );
        }
    }
}

#[test]
fn nomad_flute_devicecmyk_ycck() {
    // A real DeviceCMYK `/DCTDecode` image (object 7) from *Eighteen Songs of a
    // Nomad Flute* — Adobe APP14 transform 2 (YCCK), the encoding whose
    // double-inversion bug rendered that book's p0 at 99.9% ink vs PDFium's
    // 2.1%. `.truth.bin` is Pillow's (libjpeg's) *un-inverted* true CMYK decode
    // of this exact file (Pillow un-inverts Adobe CMYK; cross-checked against
    // `cmyk.src.bin`). Our decoder emits RAW, Adobe-inverted libjpeg CMYK and
    // leaves the flip to the PDF `/Decode [1 0 …]` array (PDFium's `CPDF_DIB`
    // convention), so `255 - ours` must equal the true CMYK. The pre-fix double
    // inversion made `255 - ours` diverge from the truth by ~250 — this is the
    // regression guard.
    let img = decode("nomad_flute_p0.jpg");
    assert_eq!(img.format, DecodedFormat::Cmyk8);
    assert_eq!((img.width, img.height), (1181, 414));
    let truth = fixture("nomad_flute_p0.truth.bin");
    let normalized: Vec<u8> = img.data.iter().map(|&v| 255 - v).collect();
    let (mean, _max) = diff_stats(&normalized, &truth);
    assert!(mean < 8.0, "Nomad Flute YCCK CMYK (raw, /Decode-normalized) vs libjpeg: mean {mean}");
}
