//! Round-trip smoke tests added to verify the symbol-encoding fixes did not
//! break PDF-fragment encoding. Each test encodes a small known bitmap,
//! hands the bytes to `jbig2dec` for decoding (in `-e` embedded mode, since
//! jbig2enc-rust's fragment output omits the file header by design), and
//! compares against the original pixel data.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use jbig2enc_rust::{
    Jbig2Config, Jbig2Context, encode_single_image, encode_single_image_with_config,
};
use jbig2enc_rust::jbig2enc::Jbig2Encoder;
use ndarray::Array2;

fn jbig2dec_path() -> PathBuf {
    if let Ok(env) = std::env::var("JBIG2DEC") {
        return PathBuf::from(env);
    }
    let local = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("jbig2dec.exe");
    if local.exists() {
        return local;
    }
    PathBuf::from("jbig2dec")
}

/// 16x12 binary pattern with a stripe and a few isolated pixels — exercises
/// non-byte-aligned widths, multiple set bits, and a non-trivial row count.
fn sample_bitmap() -> (u32, u32, Vec<u8>) {
    let w: u32 = 16;
    let h: u32 = 12;
    let mut pixels = vec![0u8; (w * h) as usize];
    for x in 0..w as usize {
        pixels[5 * w as usize + x] = 1; // solid black row at y=5
    }
    for y in 0..h as usize {
        pixels[y * w as usize + 0] = 1;
        pixels[y * w as usize + (w as usize - 1)] = 1;
    }
    pixels[1 * w as usize + 7] = 1;
    pixels[2 * w as usize + 8] = 1;
    pixels[3 * w as usize + 9] = 1;
    (w, h, pixels)
}

/// Decode a standalone JBIG2 file (with file header) via jbig2dec.
fn decode_standalone(file_bytes: &[u8], w: u32, h: u32) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("tempdir");
    let in_path = dir.path().join("file.jb2");
    let out_path = dir.path().join("out.pbm");

    std::fs::File::create(&in_path)
        .and_then(|mut f| f.write_all(file_bytes))
        .expect("write input");

    let dec = jbig2dec_path();
    let output = Command::new(&dec)
        .arg("-o")
        .arg(&out_path)
        .arg(&in_path)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn jbig2dec at {:?}: {}", dec, e));
    if !output.status.success() {
        panic!(
            "jbig2dec (standalone) exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let pbm = std::fs::read(&out_path).expect("read pbm output");
    parse_p4_pbm(&pbm, w, h)
}

/// Decode a JBIG2 PDF fragment (embedded, no file header) via jbig2dec.
/// If `globals` is `Some`, jbig2dec is invoked in two-file mode (globals, page).
/// Returns 1 byte per pixel (1 = black).
fn decode_embedded(globals: Option<&[u8]>, page: &[u8], w: u32, h: u32) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("tempdir");
    let page_path = dir.path().join("page.jb2");
    let out_path = dir.path().join("out.pbm");

    {
        let mut f = std::fs::File::create(&page_path).expect("create page");
        f.write_all(page).expect("write page");
    }

    let dec = jbig2dec_path();
    let mut cmd = Command::new(&dec);
    cmd.arg("-e").arg("-o").arg(&out_path);
    let globals_path: PathBuf;
    if let Some(g) = globals {
        globals_path = dir.path().join("globals.jb2");
        std::fs::File::create(&globals_path).unwrap().write_all(g).unwrap();
        cmd.arg(&globals_path);
    }
    cmd.arg(&page_path);

    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn jbig2dec at {:?}: {}", dec, e));
    if !output.status.success() {
        panic!(
            "jbig2dec exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let pbm = std::fs::read(&out_path).expect("read pbm output");
    parse_p4_pbm(&pbm, w, h)
}

/// Minimal P4 (binary) PBM parser. Returns 1 byte per pixel (1 = black).
fn parse_p4_pbm(bytes: &[u8], expected_w: u32, expected_h: u32) -> Vec<u8> {
    let mut i = 0usize;
    assert_eq!(&bytes[i..i + 2], b"P4", "expected P4 magic");
    i += 2;

    fn skip_ws_and_comments(bytes: &[u8], i: &mut usize) {
        loop {
            while *i < bytes.len()
                && (bytes[*i] == b' ' || bytes[*i] == b'\t' || bytes[*i] == b'\r' || bytes[*i] == b'\n')
            {
                *i += 1;
            }
            if *i < bytes.len() && bytes[*i] == b'#' {
                while *i < bytes.len() && bytes[*i] != b'\n' {
                    *i += 1;
                }
            } else {
                break;
            }
        }
    }

    fn read_uint(bytes: &[u8], i: &mut usize) -> u32 {
        let start = *i;
        while *i < bytes.len() && bytes[*i].is_ascii_digit() {
            *i += 1;
        }
        std::str::from_utf8(&bytes[start..*i]).unwrap().parse().unwrap()
    }

    skip_ws_and_comments(bytes, &mut i);
    let w = read_uint(bytes, &mut i);
    skip_ws_and_comments(bytes, &mut i);
    let h = read_uint(bytes, &mut i);
    i += 1; // one whitespace byte before the raster

    assert_eq!(w, expected_w, "PBM width mismatch");
    assert_eq!(h, expected_h, "PBM height mismatch");

    let row_bytes = (w as usize + 7) / 8;
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h as usize {
        let row = &bytes[i + y * row_bytes..i + (y + 1) * row_bytes];
        for x in 0..w as usize {
            let byte = row[x / 8];
            let bit = (byte >> (7 - (x % 8))) & 1;
            out.push(bit);
        }
    }
    out
}

#[test]
fn standalone_roundtrip_default_config() {
    // Covers the magic-byte fix (T.88 §D.4.1, 8-byte ID string) and the
    // sequential-organisation file-header flag fix. Both are required for
    // jbig2dec to accept the file at all.
    let (w, h, original) = sample_bitmap();
    let result = encode_single_image(&original, w, h, /* pdf_mode = */ false)
        .expect("encode standalone (default cfg)");
    assert!(
        result.global_data.is_none(),
        "standalone mode should not produce a separate global stream"
    );
    let decoded = decode_standalone(&result.page_data, w, h);
    assert_eq!(
        decoded, original,
        "standalone JBIG2 round-trip differs from input"
    );
}

#[test]
fn pdf_fragment_roundtrip_default_config() {
    let (w, h, original) = sample_bitmap();

    let result = encode_single_image(&original, w, h, /* pdf_mode = */ true)
        .expect("encode PDF fragment (default cfg)");

    let decoded = decode_embedded(
        result.global_data.as_deref(),
        &result.page_data,
        w,
        h,
    );
    assert_eq!(
        decoded, original,
        "PDF fragment round-trip differs from input (default config)"
    );
}

#[test]
fn pdf_fragment_roundtrip_lossless_config() {
    let (w, h, original) = sample_bitmap();

    let ctx = Jbig2Context::with_lossless_config(/* pdf_mode = */ true);
    let result = encode_single_image_with_config(&original, w, h, ctx)
        .expect("encode PDF fragment (lossless cfg)");

    let decoded = decode_embedded(
        result.global_data.as_deref(),
        &result.page_data,
        w,
        h,
    );
    assert_eq!(
        decoded, original,
        "PDF fragment round-trip differs from input (lossless config)"
    );
}

/// Round-trip with a larger bitmap so the generic-region encoder has to cross
/// more than one packed word per row and the IADS / per-symbol code paths see
/// non-trivial work even when symbol mode is off.
#[test]
fn pdf_fragment_roundtrip_wide_bitmap() {
    let w: u32 = 64;
    let h: u32 = 24;
    let mut pixels = vec![0u8; (w * h) as usize];
    // Diagonal line + dotted border
    for d in 0..(h as usize).min(w as usize) {
        pixels[d * w as usize + d] = 1;
        pixels[d * w as usize + (w as usize - 1 - d)] = 1;
    }
    for x in (0..w as usize).step_by(4) {
        pixels[0 * w as usize + x] = 1;
        pixels[(h as usize - 1) * w as usize + x] = 1;
    }

    let result = encode_single_image(&pixels, w, h, /* pdf_mode = */ true)
        .expect("encode wide bitmap");
    let decoded = decode_embedded(
        result.global_data.as_deref(),
        &result.page_data,
        w,
        h,
    );
    assert_eq!(decoded, pixels, "wide-bitmap PDF fragment round-trip differs");
}

/// Build an Array2<u8> from a packed binary list (one byte per pixel, 0/1).
fn array2_from_pixels(w: u32, h: u32, pixels: &[u8]) -> Array2<u8> {
    Array2::from_shape_vec((h as usize, w as usize), pixels.to_vec()).expect("shape")
}

/// A page containing a repeated "letter-like" glyph so the symbol-mode planner
/// sees multiple identical instances and exercises the symbol-dict /
/// text-region encoding paths affected by fixes #2 and #4–#5.
fn page_with_repeated_glyph(w: u32, h: u32, glyph: &[(u32, u32)], reps_x: u32, reps_y: u32) -> Vec<u8> {
    let mut pixels = vec![0u8; (w * h) as usize];
    let glyph_w = 8u32;
    let glyph_h = 10u32;
    for ry in 0..reps_y {
        for rx in 0..reps_x {
            let ox = 2 + rx * (glyph_w + 2);
            let oy = 2 + ry * (glyph_h + 2);
            if ox + glyph_w > w || oy + glyph_h > h {
                continue;
            }
            for &(gx, gy) in glyph {
                let x = ox + gx;
                let y = oy + gy;
                if x < w && y < h {
                    pixels[(y * w + x) as usize] = 1;
                }
            }
        }
    }
    pixels
}

#[test]
fn pdf_fragment_roundtrip_symbol_mode_repeated_glyph() {
    let w: u32 = 96;
    let h: u32 = 48;
    // 8x10 "H"-shaped glyph
    let glyph: Vec<(u32, u32)> = (0..10)
        .flat_map(|y| [(0u32, y), (6u32, y)].into_iter())
        .chain((1..6).map(|x| (x, 5)))
        .collect();
    let pixels = page_with_repeated_glyph(w, h, &glyph, 9, 3);

    let mut cfg = Jbig2Config::text();
    // Force PDF fragment output (no file header), exercise base symbol encoding
    // (not sym-unify, per the review scope).
    cfg.want_full_headers = false;
    cfg.refine = false;
    cfg.text_refine = false;

    let mut encoder = Jbig2Encoder::new(&cfg);
    encoder
        .add_page(&array2_from_pixels(w, h, &pixels))
        .expect("add_page");
    let split = encoder.flush_pdf_split().expect("flush_pdf_split");

    let page_stream = split
        .page_streams
        .into_iter()
        .next()
        .expect("at least one page stream");

    let decoded = decode_embedded(split.global_segments.as_deref(), &page_stream, w, h);
    assert_eq!(
        decoded, pixels,
        "symbol-mode PDF fragment round-trip differs from input"
    );
}
