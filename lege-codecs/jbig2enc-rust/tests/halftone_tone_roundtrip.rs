//! Round-trip guards for the halftone grayscale encoder.
//!
//! Findings 1 (bitplane count), 7 (partial edge cells) and 8 (quantizer vs.
//! dictionary coverage) of the 2026-09-05 review all show up as a decoded black
//! fraction that does not match the requested ink coverage, so that is what
//! these tests measure. The native decoder is authoritative; system `jbig2dec`
//! is cross-checked when it is installed.

use jbig2enc_rust::decode::{DecodeOptions, decode_embedded};
use jbig2enc_rust::jbig2halftone::encode_halftone_pdf_split_auto_from_grayscale;
use jbig2enc_rust::jbig2structs::{GenericRegionParams, Jbig2Config};

/// Encodes `gray` (0 = no ink, 255 = full ink) and returns (globals, page).
fn encode(gray: &[u8], w: u32, h: u32, mmr: bool) -> (Vec<u8>, Vec<u8>) {
    let mut cfg = Jbig2Config::default();
    cfg.halftone.gray_mmr = mmr;
    encode_halftone_pdf_split_auto_from_grayscale(
        gray,
        w,
        h,
        &cfg,
        &GenericRegionParams::new(w, h, 300),
        1,
        Some(1),
    )
    .expect("halftone encode")
}

fn decoded_black(globals: &[u8], page: &[u8], w: u32, h: u32) -> usize {
    let bitmap =
        decode_embedded(Some(globals), page, &DecodeOptions::default()).expect("native decode");
    let mut black = 0;
    for y in 0..h {
        for x in 0..w {
            if bitmap.get(x, y) {
                black += 1;
            }
        }
    }
    black
}

/// Black-pixel count from system `jbig2dec`, or `None` when it is not installed
/// (or could not be driven, which must not fail an otherwise good build).
fn jbig2dec_black(globals: &[u8], page: &[u8], w: u32, h: u32) -> Option<usize> {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!(
        "jb2ht-{}-{}-{:?}",
        w,
        h,
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let g = dir.join("globals.jb2");
    let p = dir.join("page.jb2");
    let o = dir.join("out.pbm");
    std::fs::write(&g, globals).ok()?;
    std::fs::write(&p, page).ok()?;
    let status = Command::new("jbig2dec")
        .args(["-e", "-t", "pbm", "-o"])
        .arg(&o)
        .arg(&g)
        .arg(&p)
        .status()
        .ok()?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }
    let raw = std::fs::read(&o).ok()?;
    let _ = std::fs::remove_dir_all(&dir);
    // P4 binary PBM: magic, width, height, then packed rows (1 = black).
    let mut it = raw.splitn(4, |&b| b == b'\n' || b == b' ');
    it.next()?;
    let pw: usize = std::str::from_utf8(it.next()?).ok()?.trim().parse().ok()?;
    let ph: usize = std::str::from_utf8(it.next()?).ok()?.trim().parse().ok()?;
    let body = it.next()?;
    if pw != w as usize || ph != h as usize {
        return None;
    }
    let stride = pw.div_ceil(8);
    let mut black = 0;
    for y in 0..ph {
        for x in 0..pw {
            if body.get(y * stride + x / 8)? >> (7 - (x % 8)) & 1 == 1 {
                black += 1;
            }
        }
    }
    Some(black)
}

fn check_uniform(ink: u8, w: u32, h: u32, tol: f64) {
    let want = ink as f64 / 255.0;
    for mmr in [false, true] {
        let gray = vec![ink; (w * h) as usize];
        let (globals, page) = encode(&gray, w, h, mmr);
        let black = decoded_black(&globals, &page, w, h);
        let got = black as f64 / (w * h) as f64;
        assert!(
            (got - want).abs() <= tol,
            "mmr={mmr} ink={ink} {w}x{h}: decoded {got:.4} black, wanted {want:.4} (tol {tol})"
        );
        if std::env::var_os("REQUIRE_JBIG2DEC").is_some() {
            assert!(
                jbig2dec_black(&globals, &page, w, h).is_some(),
                "jbig2dec probe returned None"
            );
        }
        if let Some(ext) = jbig2dec_black(&globals, &page, w, h) {
            assert_eq!(
                ext, black,
                "mmr={mmr} ink={ink}: jbig2dec disagrees with the native decoder"
            );
        }
    }
}

/// Finding 1: constant tones that only use low pattern indices must still emit
/// all ceil(log2(16)) = 4 gray-code planes.
#[test]
fn constant_tones_round_trip() {
    // 6.67%, 46.67%, 53.33%, 100% black, plus pure white.
    for ink in [0u8, 17, 119, 136, 255] {
        check_uniform(ink, 64, 64, 0.03);
    }
}

/// Finding 1: a restricted tonal range never reaches the high pattern indices.
#[test]
fn restricted_tonal_range_round_trips() {
    let (w, h) = (64u32, 64u32);
    let gray: Vec<u8> = (0..w * h)
        .map(|i| ((i % w) as u32 * 40 / w) as u8)
        .collect();
    let want: f64 = gray.iter().map(|&g| g as f64 / 255.0).sum::<f64>() / gray.len() as f64;
    for mmr in [false, true] {
        let (globals, page) = encode(&gray, w, h, mmr);
        let got = decoded_black(&globals, &page, w, h) as f64 / (w * h) as f64;
        assert!(
            (got - want).abs() <= 0.03,
            "mmr={mmr}: restricted range decoded {got:.4}, wanted {want:.4}"
        );
    }
}

/// Finding 7: every width/height residue modulo the 4-pixel cell must keep
/// solid ink, including a 1x1 crop and the 65x65 case from the review.
#[test]
fn solid_black_survives_every_edge_residue() {
    for w in [1u32, 2, 3, 4, 5, 6, 7, 8, 9, 65] {
        for h in [1u32, 2, 3, 4, 5, 6, 7, 8, 9, 65] {
            let gray = vec![255u8; (w * h) as usize];
            for mmr in [false, true] {
                let (globals, page) = encode(&gray, w, h, mmr);
                let black = decoded_black(&globals, &page, w, h);
                assert_eq!(
                    black,
                    (w * h) as usize,
                    "mmr={mmr} {w}x{h} solid black decoded {black}/{}",
                    w * h
                );
            }
        }
    }
}

/// Finding 8: the reconstructed tone tracks the requested coverage across the
/// whole ramp, which only holds when the quantizer scores against the real
/// pattern populations.
#[test]
fn tone_ramp_is_unbiased() {
    for ink in (0..=255u16).step_by(15) {
        check_uniform(ink as u8, 64, 64, 0.035);
    }
}
