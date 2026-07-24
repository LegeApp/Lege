//! Decoder benchmarks (jbig2dec-gaps-plan.md Gap D).
//!
//! `decode_generic_a4_300dpi` synthesises a text-like A4-at-300dpi bitmap
//! (2480×3508), encodes it once as a generic region in setup, and benchmarks
//! decoding it. This is the dominant real-world case for the PDF renderer.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use jbig2enc_rust::decode::{DecodeOptions, DecoderContext, decode_file_with_context};
use jbig2enc_rust::jbig2halftone::encode_halftone_document_auto;
use jbig2enc_rust::jbig2structs::{GenericRegionParams, Jbig2Config};
use jbig2enc_rust::jbig2sym::BitImage;
use jbig2enc_rust::{Jbig2Context, encode_single_image_with_config};

/// Deterministically synthesise a "text-like" bitmap: rows of short black runs
/// (glyph strokes) with generous white gaps, at the given size.
fn synth_textlike(w: u32, h: u32) -> Vec<u8> {
    let mut px = vec![0u8; (w as usize) * (h as usize)];
    let mut s: u32 = 0x9E37_79B9;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    };
    // Lay down ~40 text lines; within each line, scatter short ink runs.
    let line_h = 60u32;
    for line in 0..(h / line_h) {
        let base_y = line * line_h + 10;
        let mut x = 8u32;
        while x < w - 20 {
            let run = 2 + (next() % 6);
            let gap = 2 + (next() % 10);
            let glyph_h = 8 + (next() % 24);
            if next() % 3 != 0 {
                for yy in base_y..(base_y + glyph_h).min(h) {
                    for xx in x..(x + run).min(w) {
                        px[(yy * w + xx) as usize] = 1;
                    }
                }
            }
            x += run + gap;
        }
    }
    px
}

fn bench_decode_generic_a4(c: &mut Criterion) {
    let (w, h) = (2480u32, 3508u32);
    let px = synth_textlike(w, h);

    // Encode once in a generic (lossless, symbol-off) configuration.
    let ctx = Jbig2Context::with_lossless_config(false);
    let encoded = encode_single_image_with_config(&px, w, h, ctx)
        .expect("encode A4 generic")
        .page_data;
    eprintln!("encoded A4 generic stream = {} bytes", encoded.len());

    let opts = DecodeOptions::default();
    let mut dctx = DecoderContext::new();

    let mut group = c.benchmark_group("decode_generic_a4_300dpi");
    group.sample_size(20);
    group.bench_function("decode", |b| {
        b.iter(|| {
            let doc =
                decode_file_with_context(black_box(&encoded), &opts, &mut dctx).expect("decode");
            black_box(doc.pages.len());
        });
    });
    group.finish();
}

/// Synthesise a text-heavy page (many repeated glyphs) so the encoder builds a
/// symbol dictionary + text region, then benchmark decoding it (Gap D).
fn synth_glyph_page(w: u32, h: u32) -> Vec<u8> {
    let mut px = vec![0u8; (w as usize) * (h as usize)];
    let mut s: u32 = 0x1234_5678;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    };
    let (gw, gh) = (10u32, 14u32);
    let mut y = 4;
    while y + gh + 4 < h {
        let mut x = 4;
        while x + gw + 4 < w {
            let kind = next() % 6;
            for dy in 0..gh {
                for dx in 0..gw {
                    let on = match kind {
                        0 => dx == 0 || dx == gw - 1 || dy == 0 || dy == gh - 1,
                        1 => dx == gw / 2 || dy == gh / 2,
                        2 => (dx + dy) % 3 == 0,
                        3 => dx == dy || dx + dy == gw - 1,
                        4 => dy < 3 || dy >= gh - 3,
                        _ => dx < 2 || dx >= gw - 2,
                    };
                    if on {
                        px[((y + dy) * w + (x + dx)) as usize] = 1;
                    }
                }
            }
            x += gw + 4;
        }
        y += gh + 4;
    }
    px
}

fn bench_decode_symbol_page(c: &mut Criterion) {
    // ~US-Letter-ish text page at moderate size to keep encode setup quick.
    let (w, h) = (1240u32, 1754u32);
    let px = synth_glyph_page(w, h);

    // Encode once in symbol mode (arithmetic dictionary + text region).
    let ctx = Jbig2Context::with_config(Jbig2Config::text(), false);
    let encoded = encode_single_image_with_config(&px, w, h, ctx)
        .expect("encode symbol page")
        .page_data;
    eprintln!("encoded symbol page stream = {} bytes", encoded.len());

    let opts = DecodeOptions::default();
    let mut dctx = DecoderContext::new();

    let mut group = c.benchmark_group("decode_symbol_page");
    group.sample_size(20);
    group.bench_function("decode", |b| {
        b.iter(|| {
            let doc =
                decode_file_with_context(black_box(&encoded), &opts, &mut dctx).expect("decode");
            black_box(doc.pages.len());
        });
    });
    group.finish();
}

/// Same glyph page, but encoded with SBREFINE=1 per-instance refinement
/// (`text_refine`), so decoding drives the generic-refinement path once per
/// refined instance. Cheap add-on to guard the refinement decode hot loop.
fn bench_decode_refined_symbol_page(c: &mut Criterion) {
    let (w, h) = (1240u32, 1754u32);
    let px = synth_glyph_page(w, h);

    let mut cfg = Jbig2Config::text();
    cfg.text_refine = true;
    let ctx = Jbig2Context::with_config(cfg, false);
    let encoded = encode_single_image_with_config(&px, w, h, ctx)
        .expect("encode refined symbol page")
        .page_data;
    eprintln!(
        "encoded refined symbol page stream = {} bytes",
        encoded.len()
    );

    let opts = DecodeOptions::default();
    let mut dctx = DecoderContext::new();

    let mut group = c.benchmark_group("decode_refined_symbol_page");
    group.sample_size(20);
    group.bench_function("decode", |b| {
        b.iter(|| {
            let doc =
                decode_file_with_context(black_box(&encoded), &opts, &mut dctx).expect("decode");
            black_box(doc.pages.len());
        });
    });
    group.finish();
}

/// Synthesise a grayscale-ramp bilevel page and encode it as a halftone
/// document (pattern dictionary + halftone region with arithmetic gray planes),
/// then benchmark decoding it (Gap D, Phase 4).
#[allow(clippy::field_reassign_with_default)]
fn bench_decode_halftone_page(c: &mut Criterion) {
    let (w, h) = (1240u32, 1754u32);
    let mut img = BitImage::new(w, h).expect("halftone bitmap");
    let mut s: u32 = 0x2468_ACE0;
    for y in 0..h {
        for x in 0..w {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
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
    let region = GenericRegionParams::new(w, h, 300);
    let encoded = encode_halftone_document_auto(&img, &cfg, &region, 0, Some(1))
        .expect("encode halftone page");
    eprintln!("encoded halftone page stream = {} bytes", encoded.len());

    let opts = DecodeOptions::default();
    let mut dctx = DecoderContext::new();

    let mut group = c.benchmark_group("decode_halftone_page");
    group.sample_size(20);
    group.bench_function("decode", |b| {
        b.iter(|| {
            let doc =
                decode_file_with_context(black_box(&encoded), &opts, &mut dctx).expect("decode");
            black_box(doc.pages.len());
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_decode_generic_a4,
    bench_decode_symbol_page,
    bench_decode_refined_symbol_page,
    bench_decode_halftone_page
);
criterion_main!(benches);
