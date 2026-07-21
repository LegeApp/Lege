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
//!   DJVU_BENCH_WIDTH  — synthetic image width (default 1600)
//!   DJVU_BENCH_HEIGHT — synthetic image height (default 1200)

use djvu_encoder::doc::page_encoder::{PageComponents, PageEncodeParams};
use djvu_encoder::encode::iw44::rgb_to_ycbcr_planes;
use djvu_encoder::encode::iw44::transform::Encode as IwTransform;
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
    Pixmap::from_fn(width, height, |x, y| {
        let r = ((x * 255 / width.max(1)) % 256) as u8;
        let g = ((y * 255 / height.max(1)) % 256) as u8;
        let b = (((x + y) * 128 / (width + height).max(1)) % 256) as u8;
        Pixel::new(r, g, b)
    })
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
    let page_iters = iters.min(5).max(1);
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

    // --- Stage 3: multi-page document assembly, sequential vs page-level
    // parallel (via the thread-safe DjvuDocument API — see
    // tests/page_parallel_test.rs and llm-docs/SIMD_AND_PARALLELISM_PLAN.md
    // Phase 4). Uses smaller per-page images by default so this stays fast;
    // override with DJVU_BENCH_PAGE_{WIDTH,HEIGHT} for a more realistic size.
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
