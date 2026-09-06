//! Phase 0 benchmark harness (llm-docs/SIMD_AND_PARALLELISM_PLAN.md).
//!
//! Reports wall time and summed process CPU time (across all threads, via
//! /proc/self/task/*/schedstat on Linux) for the color-conversion primitive
//! and for a full page encode, so later phases have a baseline to compare
//! against instead of assuming a win.
//!
//! Usage:
//!   cargo run --release --example benchmark
//!   cargo run --release --example benchmark --features simd   # enables wide/avx2
//!   DJVU_PRIMITIVES=wide cargo run --release --example benchmark --features simd
//!
//! Env vars:
//!   DJVU_BENCH_ITERS  — iterations per stage (default 20)
//!   DJVU_BENCH_PAGE_ITERS — iterations for the full page encode
//!                       (default min(DJVU_BENCH_ITERS, 5))
//!   DJVU_BENCH_EXPECT_BYTES — if set, assert the page encode produced
//!                       exactly this many bytes (guards bit-identity while
//!                       optimizing)
//!   DJVU_BENCH_SKIP_MULTIPAGE — set to 1 to skip the multi-page stage
//!   DJVU_BENCH_SKIP_JB2 — set to 1 to skip the bilevel (JB2) stage
//!   DJVU_BENCH_JB2_WIDTH / DJVU_BENCH_JB2_HEIGHT — bilevel page size
//!                       (default 2480x3508, A4 at 300 dpi)
//!   DJVU_BENCH_JB2_EXPECT_BYTES — assert the JB2 page encode size
//!   DJVU_BENCH_IMAGE  — synthetic image content: `gradient` (default, very
//!                       smooth, so most high-frequency buckets are empty) or
//!                       `noise` (dense high-frequency detail, the worst case
//!                       for the coefficient coder), or `white` (the blank BG44
//!                       layer every bitonal page carries)
//!   DJVU_BENCH_WIDTH  — synthetic image width (default 1600)
//!   DJVU_BENCH_HEIGHT — synthetic image height (default 1200)

use djvu_encoder::doc::page_encoder::{PageComponents, PageEncodeParams};
use djvu_encoder::encode::iw44::rgb_to_ycbcr_planes;
use djvu_encoder::encode::iw44::transform::Encode as IwTransform;
use djvu_encoder::encode::jb2::cc_image::{analyze_page, shapes_to_encoder_format};
use djvu_encoder::encode::jb2::encoder::JB2Encoder;
use djvu_encoder::encode::jb2::symbol_dict::BitImage;
use djvu_encoder::image::image_formats::{Pixel, Pixmap};
use djvu_encoder::{DjvuBuilder, Page, PageBuilder};
use std::time::Instant;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Sum of CPU time (nanoseconds) across all threads of this process, via
/// /proc/self/task/*/schedstat field 0 ("time spent running", ns). Falls
/// back to 0 if unavailable (e.g. non-Linux).
fn process_cpu_time_ns() -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path().join("schedstat");
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Some(field0) = contents.split_whitespace().next() {
                total += field0.parse::<u64>().unwrap_or(0);
            }
        }
    }
    total
}

struct Timing {
    wall_ms: f64,
    cpu_ms: f64,
}

fn time_it<F: FnMut()>(iters: usize, mut f: F) -> Timing {
    let cpu_start = process_cpu_time_ns();
    let wall_start = Instant::now();
    for _ in 0..iters {
        f();
    }
    let wall_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
    let cpu_ms = (process_cpu_time_ns().saturating_sub(cpu_start)) as f64 / 1_000_000.0;
    Timing { wall_ms, cpu_ms }
}

fn synthetic_rgb(width: u32, height: u32) -> Pixmap {
    match std::env::var("DJVU_BENCH_IMAGE").as_deref() {
        Ok("white") => Pixmap::from_fn(width, height, |_, _| Pixel::new(255, 255, 255)),
        Ok("noise") => Pixmap::from_fn(width, height, |x, y| {
            // Deterministic hash-based noise: dense detail in every band, the
            // opposite extreme from the smooth gradient.
            let h =
                (x.wrapping_mul(374761393) ^ y.wrapping_mul(668265263)).wrapping_mul(1274126177);
            let h = h ^ (h >> 15);
            Pixel::new((h >> 3) as u8, (h >> 11) as u8, (h >> 19) as u8)
        }),
        _ => Pixmap::from_fn(width, height, |x, y| {
            let r = ((x * 255 / width.max(1)) % 256) as u8;
            let g = ((y * 255 / height.max(1)) % 256) as u8;
            let b = (((x + y) * 128 / (width + height).max(1)) % 256) as u8;
            Pixel::new(r, g, b)
        }),
    }
}

/// A synthetic scanned text page: a grid of glyph-like blobs drawn from a small
/// alphabet, which is what a real document page looks like to the JB2 encoder
/// (thousands of small connected components, most of them repeats).
fn synthetic_bilevel(width: u32, height: u32) -> BitImage {
    let mut bm = BitImage::new(width, height).expect("BitImage::new");
    // 15x21 glyph cells on a 25x50 grid with a blank line every 25 rows: about
    // 5,000 glyphs on an A4 page at 300 dpi, the density of a page of 12 pt
    // body text.
    let (mx, my) = (width / 12, height / 16);
    let (cw, ch) = (25u32, 50u32);
    let mut gi = 0u32;
    let mut y = my;
    let mut row = 0u32;
    while y + ch < height - my {
        if row % 26 >= 25 {
            y += ch;
            row += 1;
            continue;
        }
        let mut x = mx;
        while x + cw < width - mx {
            // 24 distinct 5x7 glyph patterns, chosen deterministically.
            let g = (gi.wrapping_mul(2654435761) >> 13) % 24;
            gi += 1;
            if g == 23 {
                // a space
                x += cw;
                continue;
            }
            for dy in 0..21u32 {
                let bits = ((g + 1).wrapping_mul(dy / 3 + 3).wrapping_mul(2654435761) >> 11) & 0x1f;
                for dx in 0..15u32 {
                    if (bits >> (dx / 3)) & 1 != 0 {
                        bm.set_usize((x + dx) as usize, (y + dy) as usize, true);
                    }
                }
            }
            x += cw;
        }
        y += ch;
        row += 1;
    }
    bm
}

fn main() {
    let iters = env_usize("DJVU_BENCH_ITERS", 20);
    let width = env_usize("DJVU_BENCH_WIDTH", 1600) as u32;
    let height = env_usize("DJVU_BENCH_HEIGHT", 1200) as u32;

    println!("backend={}", djvu_encoder::active_primitives_backend());
    println!("image={width}x{height} iters={iters}");
    println!();

    // --- Stage 1: color conversion only ---
    let img = synthetic_rgb(width, height);
    let npix = (width * height) as usize;
    let mut out_y = vec![0i8; npix];
    let mut out_cb = vec![0i8; npix];
    let mut out_cr = vec![0i8; npix];
    let raw = img.as_raw().to_vec();

    let t = time_it(iters, || {
        rgb_to_ycbcr_planes(&raw, &mut out_y, &mut out_cb, &mut out_cr);
    });
    println!(
        "color_convert_ms  wall={:.3} cpu={:.3} (per-iter wall={:.4})",
        t.wall_ms,
        t.cpu_ms,
        t.wall_ms / iters as f64
    );

    // --- Stage 1.5: IW44 wavelet transform only (filter_fh + filter_fv,
    // 5 levels, matching Encode::forward's default) ---
    let tw = width as usize;
    let th = height as usize;
    let rowsize = (tw + 31) & !31;
    let padded_h = (th + 31) & !31;
    let pristine: Vec<i16> = (0..rowsize * padded_h)
        .map(|i| (((i as u32).wrapping_mul(2654435761) >> 16) as i16).wrapping_sub(16384))
        .collect();
    let mut working = pristine.clone();

    let t = time_it(iters, || {
        working.copy_from_slice(&pristine);
        IwTransform::forward(&mut working, tw, th, rowsize, 5);
    });
    println!(
        "iw44_transform_ms wall={:.3} cpu={:.3} (per-iter wall={:.4})",
        t.wall_ms,
        t.cpu_ms,
        t.wall_ms / iters as f64
    );

    // --- Stage 2: full page encode (IW44 background only) ---
    let page_iters = env_usize("DJVU_BENCH_PAGE_ITERS", iters.min(5).max(1)).max(1);
    let mut output_bytes = 0usize;
    let t = time_it(page_iters, || {
        let page = PageComponents::new_with_dimensions(width, height)
            .with_background(img.clone())
            .expect("with_background");
        let params = PageEncodeParams::default();
        let encoded = page
            .encode(&params, 1, 300, 1, Some(2.2))
            .expect("page encode");
        output_bytes = encoded.len();
    });
    println!(
        "page_encode_ms    wall={:.3} cpu={:.3} (per-iter wall={:.4}) iters={} output_bytes={}",
        t.wall_ms,
        t.cpu_ms,
        t.wall_ms / page_iters as f64,
        page_iters,
        output_bytes
    );
    if let Ok(expected) = std::env::var("DJVU_BENCH_EXPECT_BYTES") {
        let expected: usize = expected.parse().expect("DJVU_BENCH_EXPECT_BYTES");
        assert_eq!(
            output_bytes, expected,
            "page encode output changed: expected {expected} bytes, got {output_bytes}"
        );
    }

    // --- Stage 3: multi-page document assembly, sequential vs page-level
    // parallel (via the thread-safe DjvuDocument API — see
    // tests/page_parallel_test.rs and llm-docs/SIMD_AND_PARALLELISM_PLAN.md
    // Phase 4). Uses smaller per-page images by default so this stays fast;
    // override with DJVU_BENCH_PAGE_{WIDTH,HEIGHT} for a more realistic size.
    // --- Stage 2b: bilevel (JB2) page, the path Lege's document pages take ---
    if std::env::var("DJVU_BENCH_SKIP_JB2").is_err() {
        let jw = env_usize("DJVU_BENCH_JB2_WIDTH", 2480) as u32;
        let jh = env_usize("DJVU_BENCH_JB2_HEIGHT", 3508) as u32;
        let jb2_iters = env_usize("DJVU_BENCH_JB2_ITERS", 3).max(1);
        let bilevel = synthetic_bilevel(jw, jh);

        let mut ccs = 0usize;
        let t = time_it(jb2_iters, || {
            let cc = analyze_page(&bilevel, 300, 1);
            ccs = cc.extract_shapes().len();
        });
        println!(
            "jb2_cc_analysis_ms wall={:.3} cpu={:.3} (per-iter wall={:.4}) size={jw}x{jh} shapes={ccs}",
            t.wall_ms,
            t.cpu_ms,
            t.wall_ms / jb2_iters as f64
        );

        // The JB2 symbol coder on its own (no page container, no BG44 layer).
        let cc = analyze_page(&bilevel, 300, 1);
        let (dictionary, parents, blits) = shapes_to_encoder_format(cc.extract_shapes(), jh as i32);
        let mut sjbz_bytes = 0usize;
        let t = time_it(jb2_iters, || {
            let mut enc = JB2Encoder::new(Vec::new());
            let sjbz = enc
                .encode_page_with_shapes(jw, jh, &dictionary, &parents, &blits, 0, None)
                .expect("encode_page_with_shapes");
            sjbz_bytes = sjbz.len();
        });
        println!(
            "jb2_sjbz_only_ms   wall={:.3} cpu={:.3} (per-iter wall={:.4}) blits={} output_bytes={sjbz_bytes}",
            t.wall_ms,
            t.cpu_ms,
            t.wall_ms / jb2_iters as f64,
            blits.len()
        );
        if let Ok(expected) = std::env::var("DJVU_BENCH_SJBZ_EXPECT_BYTES") {
            let expected: usize = expected.parse().expect("DJVU_BENCH_SJBZ_EXPECT_BYTES");
            assert_eq!(sjbz_bytes, expected, "Sjbz output changed");
        }

        let mut jb2_bytes = 0usize;
        let t = time_it(jb2_iters, || {
            let page = PageComponents::new_with_dimensions(jw, jh)
                .with_jb2_auto_extract(bilevel.clone())
                .expect("with_jb2_auto_extract");
            let mut params = PageEncodeParams::default();
            params.use_iw44 = false;
            let encoded = page
                .encode(&params, 1, 300, 1, Some(2.2))
                .expect("jb2 page encode");
            jb2_bytes = encoded.len();
        });
        println!(
            "jb2_page_encode_ms wall={:.3} cpu={:.3} (per-iter wall={:.4}) iters={jb2_iters} output_bytes={jb2_bytes}",
            t.wall_ms,
            t.cpu_ms,
            t.wall_ms / jb2_iters as f64
        );
        if let Ok(expected) = std::env::var("DJVU_BENCH_JB2_EXPECT_BYTES") {
            let expected: usize = expected.parse().expect("DJVU_BENCH_JB2_EXPECT_BYTES");
            assert_eq!(
                jb2_bytes, expected,
                "JB2 page encode output changed: expected {expected} bytes, got {jb2_bytes}"
            );
        }
    }

    if std::env::var("DJVU_BENCH_SKIP_MULTIPAGE").is_ok() {
        return;
    }
    let page_count = env_usize("DJVU_BENCH_PAGES", 8);
    let page_width = env_usize("DJVU_BENCH_PAGE_WIDTH", 800) as u32;
    let page_height = env_usize("DJVU_BENCH_PAGE_HEIGHT", 600) as u32;
    let doc_iters = 3usize;

    let make_pages = || -> Vec<Page> {
        (0..page_count)
            .map(|i| {
                let bg = synthetic_rgb(page_width, page_height);
                PageBuilder::new(i, page_width, page_height)
                    .with_background(bg)
                    .expect("with_background")
                    .build()
                    .expect("page build")
            })
            .collect()
    };

    let t_seq = time_it(doc_iters, || {
        let doc = DjvuBuilder::new(page_count).with_dpi(300).build();
        for page in make_pages() {
            doc.add_page(page).expect("add_page");
        }
        let _ = doc.finalize().expect("finalize");
    });
    println!(
        "multipage_seq_ms  wall={:.3} cpu={:.3} (per-iter wall={:.4}) pages={page_count} page_size={page_width}x{page_height}",
        t_seq.wall_ms,
        t_seq.cpu_ms,
        t_seq.wall_ms / doc_iters as f64,
    );

    #[cfg(feature = "rayon")]
    {
        use rayon::prelude::*;
        let t_par = time_it(doc_iters, || {
            let doc = DjvuBuilder::new(page_count).with_dpi(300).build();
            make_pages().into_par_iter().for_each(|page| {
                let encoded = doc.encode_page(page).expect("encode_page");
                doc.add_encoded_page(encoded).expect("add_encoded_page");
            });
            let _ = doc.finalize().expect("finalize");
        });
        println!(
            "multipage_par_ms  wall={:.3} cpu={:.3} (per-iter wall={:.4}) pages={page_count} speedup={:.2}x",
            t_par.wall_ms,
            t_par.cpu_ms,
            t_par.wall_ms / doc_iters as f64,
            t_seq.wall_ms / t_par.wall_ms,
        );
    }
    #[cfg(not(feature = "rayon"))]
    {
        println!("multipage_par_ms  skipped (rebuild with --features rayon)");
    }
}
