//! Phase 0 regression gate (llm-docs/SIMD_AND_PARALLELISM_PLAN.md): encode a
//! synthetic image with djvulibrust, decode it with `ddjvu` (djvulibre, the
//! reference C decoder), and check the round trip is sane. This is the
//! standing check every later SIMD/parallel phase must keep passing —
//! not just "our own decoder agrees with itself," but "the real DjVu
//! ecosystem can still read our output."
//!
//! Skips (rather than fails) if `ddjvu` isn't on PATH, since this is meant
//! to run in dev environments with djvulibre installed, not as a hard CI
//! dependency.

use djvu_encoder::doc::page_encoder::{PageComponents, PageEncodeParams};
use djvu_encoder::image::image_formats::{Pixel, Pixmap};
use std::process::Command;

fn ddjvu_available() -> bool {
    Command::new("ddjvu")
        .arg("--help")
        .output()
        .map(|o| o.status.success() || !o.stdout.is_empty() || !o.stderr.is_empty())
        .unwrap_or(false)
}

/// Minimal binary PPM (P6) reader: returns (width, height, rgb_bytes).
fn read_ppm(path: &str) -> (usize, usize, Vec<u8>) {
    let data = std::fs::read(path).expect("read ppm");
    assert_eq!(&data[0..2], b"P6", "not a binary PPM");

    // Walk whitespace/comment-separated header tokens: width, height, maxval.
    let mut idx = 2;
    let mut tokens = Vec::new();
    while tokens.len() < 3 {
        while data[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if data[idx] == b'#' {
            while data[idx] != b'\n' {
                idx += 1;
            }
            continue;
        }
        let start = idx;
        while !data[idx].is_ascii_whitespace() {
            idx += 1;
        }
        tokens.push(std::str::from_utf8(&data[start..idx]).unwrap().to_string());
    }
    idx += 1; // single whitespace byte after maxval, per PPM spec

    let width: usize = tokens[0].parse().unwrap();
    let height: usize = tokens[1].parse().unwrap();
    let pixels = data[idx..idx + width * height * 3].to_vec();
    (width, height, pixels)
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sum_sq: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum();
    let mse = sum_sq / a.len() as f64;
    if mse == 0.0 {
        return f64::INFINITY;
    }
    20.0 * (255.0f64).log10() - 10.0 * mse.log10()
}

#[test]
fn encode_decode_roundtrip_via_ddjvu() {
    if !ddjvu_available() {
        eprintln!("SKIP: ddjvu not found on PATH");
        return;
    }

    let width = 256u32;
    let height = 192u32;
    let background = Pixmap::from_fn(width, height, |x, y| {
        let r = ((x * 255 / width) % 256) as u8;
        let g = ((y * 255 / height) % 256) as u8;
        let b = (((x + y) * 128 / (width + height)) % 256) as u8;
        Pixel::new(r, g, b)
    });
    let source_raw = background.as_raw().to_vec();

    let page = PageComponents::new_with_dimensions(width, height)
        .with_background(background)
        .expect("with_background");
    let params = PageEncodeParams::default();
    let encoded = page
        .encode(&params, 1, 300, 1, Some(2.2))
        .expect("page encode");

    let dir = std::env::temp_dir();
    let djvu_path = dir.join("djvulibrust_roundtrip_test.djvu");
    let ppm_path = dir.join("djvulibrust_roundtrip_test.ppm");
    std::fs::write(&djvu_path, &encoded).expect("write djvu");

    let output = Command::new("ddjvu")
        .args([
            "-format=ppm",
            djvu_path.to_str().unwrap(),
            ppm_path.to_str().unwrap(),
        ])
        .output()
        .expect("run ddjvu");
    assert!(
        output.status.success(),
        "ddjvu failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let (decoded_width, decoded_height, decoded_raw) = read_ppm(ppm_path.to_str().unwrap());
    assert_eq!(decoded_width, width as usize, "decoded width mismatch");
    assert_eq!(decoded_height, height as usize, "decoded height mismatch");

    // djvulibrust's background is bottom-up internally but the public
    // Pixmap and ddjvu's PPM output are both top-down, so no flip needed
    // here. Default encode is lossy (bg_quality=90), so we check PSNR
    // rather than exact equality.
    let quality_db = psnr(&source_raw, &decoded_raw);
    assert!(
        quality_db > 25.0,
        "round-trip PSNR too low: {quality_db:.2} dB (source vs ddjvu-decoded output)"
    );
    eprintln!("round-trip PSNR: {quality_db:.2} dB");
}

#[test]
fn lossless_iw44_is_rejected_before_writing_a_lossy_stream() {
    let page = PageComponents::new_with_dimensions(48, 36)
        .with_background(Pixmap::from_pixel(48, 36, Pixel::white()))
        .expect("with_background");
    let params = PageEncodeParams {
        lossless: true,
        ..PageEncodeParams::default()
    };

    let error = page
        .encode(&params, 1, 300, 1, Some(2.2))
        .expect_err("IW44 cannot promise a bit-exact lossless raster");
    assert!(
        error
            .to_string()
            .contains("Lossless IW44 raster encoding is not supported")
    );
}
