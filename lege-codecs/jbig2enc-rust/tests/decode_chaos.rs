//! Structured-chaos robustness test (jbig2dec-gaps-plan.md Gap E, Phase 1).
//!
//! Takes valid encoder streams and mutates them deterministically, byte by
//! byte, asserting the decoder never panics — it must always return a bounded
//! result or a typed error. This is the stable-toolchain phase gate that stands
//! in for `cargo fuzz` (which needs nightly).

use jbig2enc_rust::decode::{DecodeOptions, decode_embedded, decode_file};
use jbig2enc_rust::jbig2halftone::encode_halftone_document_auto;
use jbig2enc_rust::jbig2structs::{GenericRegionParams, Jbig2Config};
use jbig2enc_rust::jbig2sym::BitImage;
use jbig2enc_rust::{
    Jbig2Context, encode_single_image, encode_single_image_lossless,
    encode_single_image_with_config,
};

/// A halftone stream (pattern dictionary + halftone region with MMR gray
/// planes) for the Phase 4 halftone chaos gate.
#[allow(clippy::field_reassign_with_default)]
fn halftone_stream() -> Vec<u8> {
    let (w, h) = (48u32, 36u32);
    let mut img = BitImage::new(w, h).unwrap();
    let mut s: u32 = 0x51F0;
    for y in 0..h {
        for x in 0..w {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            if ((s >> 16) & 0xFF) < x * 255 / (w - 1) {
                img.set(x, y, true);
            }
        }
    }
    let mut cfg = Jbig2Config::default();
    cfg.want_full_headers = true;
    cfg.symbol_mode = false;
    cfg.halftone.grid_size_m = 4;
    cfg.halftone.quant_levels_n = 16;
    cfg.halftone.gray_mmr = true;
    cfg.halftone.dict_mmr = true;
    let region = GenericRegionParams::new(w, h, 300);
    encode_halftone_document_auto(&img, &cfg, &region, 0, Some(1)).unwrap()
}

fn seed_streams() -> Vec<Vec<u8>> {
    let w = 24u32;
    let h = 17u32;
    let mut px = vec![0u8; (w * h) as usize];
    let mut s = 0xABCDu32;
    for p in px.iter_mut() {
        s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        *p = ((s >> 16) & 1) as u8;
    }
    vec![
        encode_single_image(&px, w, h, false).unwrap().page_data,
        encode_single_image(&px, w, h, true).unwrap().page_data,
        encode_single_image_lossless(&px, w, h, false)
            .unwrap()
            .page_data,
    ]
}

/// A small synthetic text-like page encoded in symbol mode (real symbol
/// dictionary + refinement text region) for the symbol-mode chaos gate.
fn symbol_stream() -> Vec<u8> {
    let (sw, sh) = (48u32, 36u32);
    let mut spx = vec![0u8; (sw * sh) as usize];
    let mut yy = 3u32;
    while yy + 12 < sh {
        let mut xx = 3u32;
        while xx + 8 < sw {
            for dy in 0..10u32 {
                for dx in 0..6u32 {
                    if dx == 0 || dx == 5 || dy == 0 || dy == 9 || (dx + dy) % 3 == 0 {
                        spx[((yy + dy) * sw + (xx + dx)) as usize] = 1;
                    }
                }
            }
            xx += 10;
        }
        yy += 14;
    }
    encode_single_image_with_config(
        &spx,
        sw,
        sh,
        Jbig2Context::with_config(Jbig2Config::text(), false),
    )
    .unwrap()
    .page_data
}

/// A refine-mode page (SBREFINE=1 text region whose instances carry non-zero
/// RDW per-instance refinements): variable-width "H" glyphs, some one pixel
/// wider, so the clusterer refines width-8 instances onto a width-7 prototype.
/// This exercises the inline generic-refinement decode path under mutation.
fn refine_stream() -> Vec<u8> {
    let (cols, rows) = (6u32, 4u32);
    let (pitch_x, pitch_y) = (12u32, 13u32);
    let w = 4 + cols * pitch_x;
    let h = 4 + rows * pitch_y;
    let mut px = vec![0u8; (w * h) as usize];
    for ry in 0..rows {
        for rx in 0..cols {
            let ox = 2 + rx * pitch_x;
            let oy = 2 + ry * pitch_y;
            let wide = (rx + ry) % 2 == 1;
            let mut set = |gx: u32, gy: u32| {
                if ox + gx < w {
                    px[((oy + gy) * w + (ox + gx)) as usize] = 1;
                }
            };
            for gy in 0..10 {
                set(0, gy);
                set(6, gy);
            }
            for gx in 1..6 {
                set(gx, 5);
            }
            if wide {
                set(7, 5);
            }
        }
    }
    let mut cfg = Jbig2Config::text();
    cfg.text_refine = true;
    encode_single_image_with_config(&px, w, h, Jbig2Context::with_config(cfg, false))
        .unwrap()
        .page_data
}

/// Tight limits so a mutated size field cannot make the symbol chaos gate slow
/// by attempting a large (still under the generous default) decode.
fn tight_opts() -> DecodeOptions {
    use jbig2enc_rust::decode::DecodeLimits;
    DecodeOptions::with_limits(DecodeLimits {
        max_width: 1 << 10,
        max_height: 1 << 10,
        max_page_pixels: 1 << 20,
        max_region_pixels: 1 << 20,
        max_symbols: 4096,
        max_symbol_pixels: 1 << 16,
        max_total_dictionary_pixels: 1 << 22,
        ..DecodeLimits::default()
    })
}

/// Every single-byte mutation of a symbol-mode stream must return a bounded
/// result or a typed error — never a panic (Gap C symbol-mode chaos gate).
#[test]
fn symbol_stream_single_byte_mutations_never_panic() {
    let opts = tight_opts();
    for stream in [symbol_stream(), refine_stream(), halftone_stream()] {
        for pos in 0..stream.len() {
            for &val in &[0x00u8, 0x01, 0x80, 0xFF, 0xAA] {
                let mut m = stream.clone();
                m[pos] = val;
                let _ = decode_file(&m, &opts);
                let _ = decode_embedded(None, &m, &opts);
            }
        }
        for cut in 0..=stream.len() {
            let _ = decode_file(&stream[..cut], &opts);
        }
    }
}

#[test]
fn single_byte_mutations_never_panic() {
    let opts = DecodeOptions::default();
    for stream in seed_streams() {
        // Every byte position × a spread of replacement values.
        for pos in 0..stream.len() {
            for &val in &[0x00u8, 0x01, 0x7F, 0x80, 0xFF, 0x55, 0xAA] {
                let mut m = stream.clone();
                m[pos] = val;
                // Both entry points; ignore Ok/Err, only care about no panic.
                let _ = decode_file(&m, &opts);
                let _ = decode_embedded(None, &m, &opts);
            }
        }
    }
}

#[test]
fn truncations_never_panic() {
    let opts = DecodeOptions::default();
    for stream in seed_streams() {
        for cut in 0..=stream.len() {
            let _ = decode_file(&stream[..cut], &opts);
            let _ = decode_embedded(None, &stream[..cut], &opts);
        }
    }
}

#[test]
fn xorshift_multibyte_mutations_never_panic() {
    let opts = DecodeOptions::default();
    let mut s: u32 = 0x1234_5678;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    };
    for stream in seed_streams() {
        if stream.is_empty() {
            continue;
        }
        for _ in 0..2000 {
            let mut m = stream.clone();
            let flips = 1 + (next() % 5) as usize;
            for _ in 0..flips {
                let pos = (next() as usize) % m.len();
                m[pos] ^= (next() & 0xFF) as u8;
            }
            let _ = decode_file(&m, &opts);
            let _ = decode_embedded(None, &m, &opts);
        }
    }
}
