//! Binarization comparison / A-B harness.
//!
//! Usage:
//!   cargo run -p Legencode --bin binarize_compare -- <input-image> <out-dir> [mode] [target_h]
//!
//! `mode` (default `all`):
//!   arms     Per-arm CPU vs GPU dump on the same gray, wired to the GPU debug
//!            modes in adaptive_final.wgsl:
//!              0 fused  1 Sauvola  2 Otsu  3 bg  4 normalized  5 mean  6 stddev
//!            Files: cpu_fused.png, gpu_{fused,sauvola,otsu,bg,normalized,mean,stddev}.png
//!
//!   filters  fable-review #6 (pre-binarization downscale filter). Downscales the
//!            input to `target_h` with each resampling filter, then binarizes.
//!            Lanczos3's negative lobes ring at stroke edges; CatmullRom (a cubic in
//!            the Mitchell family), Triangle (~area/bilinear) and Gaussian are the
//!            candidates. Files: filter_<name>.png  (compare edge speckle/ringing).
//!
//!   res      fable-review #6 (binarize-then-reduce vs reduce-then-binarize). Both
//!            end at the same `target_h`:
//!              res_reduce_then_binarize.png  — current pipeline order (downscale, then binarize)
//!              res_binarize_then_reduce.png  — binarize at full res, then reduce the
//!                                              *bilevel* by >=50% ink coverage per block
//!            The second should preserve faint thin strokes the first averages away.
//!
//!   window   fable-review #6 (sauvola window clamps). GPU fused output at several
//!            explicit window sizes, to see how much the window actually changes the
//!            result at this resolution. Files: window_<N>.png
//!
//!   all      Everything above.
//!
//! `target_h` defaults to h/2 (so "full res" is 2x the target — the review's setup).

use anyhow::{Context, Result, anyhow};
use image::imageops::FilterType;
use image::{GrayImage, ImageBuffer, Luma, RgbImage};

use Legencode::color::binarization::{binarize_image_raw, compute_adaptive_gpu_constants};
use Legencode::color::color_processing::rgb_to_grayscale_u8;
use Legencode::types::BinarizationOptions;

use lege_gpu::binarization::wgpu::WgpuBinarizer;
use lege_gpu::binarization::{BinarizationMode, BinarizationParams};

fn save_gray(dir: &std::path::Path, name: &str, data: &[u8], w: u32, h: u32) -> Result<()> {
    let img: GrayImage = ImageBuffer::<Luma<u8>, _>::from_raw(w, h, data.to_vec())
        .ok_or_else(|| anyhow!("buffer/size mismatch for {name} ({}!={w}x{h})", data.len()))?;
    let path = dir.join(format!("{name}.png"));
    img.save(&path)
        .with_context(|| format!("saving {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

/// Downscale interleaved RGB to `nw x nh` with the given resampling filter.
fn resize_rgb(rgb: &[u8], w: u32, h: u32, nw: u32, nh: u32, filter: FilterType) -> Vec<u8> {
    let img = RgbImage::from_raw(w, h, rgb.to_vec()).expect("rgb dims");
    image::imageops::resize(&img, nw, nh, filter).into_raw()
}

/// Downscale to `target_h` (preserving aspect) if the input is taller — matching the
/// pipeline's operating point. Returns the possibly-resized RGB and its dimensions.
/// GPU-using modes need this: a full 300-dpi scan overflows the GPU integral cap and
/// isn't the resolution the pipeline actually binarizes at.
fn to_operating_point(rgb: &[u8], w: u32, h: u32, target_h: u32) -> (Vec<u8>, u32, u32) {
    if h <= target_h {
        return (rgb.to_vec(), w, h);
    }
    let nh = target_h.max(1);
    let nw = ((w as u64 * nh as u64) / h as u64).max(1) as u32;
    (
        resize_rgb(rgb, w, h, nw, nh, FilterType::Lanczos3),
        nw,
        nh,
    )
}

/// Reduce a bilevel image (0=ink, 255=paper) by integer `factor` using ink coverage:
/// an output block is ink iff >=50% of its covered source pixels are ink.
fn coverage_reduce(bin: &[u8], w: usize, h: usize, factor: usize) -> (Vec<u8>, u32, u32) {
    let nw = w.div_ceil(factor);
    let nh = h.div_ceil(factor);
    let mut out = vec![255u8; nw * nh];
    for oy in 0..nh {
        for ox in 0..nw {
            let (mut ink, mut total) = (0usize, 0usize);
            for dy in 0..factor {
                let sy = oy * factor + dy;
                if sy >= h {
                    break;
                }
                for dx in 0..factor {
                    let sx = ox * factor + dx;
                    if sx >= w {
                        break;
                    }
                    total += 1;
                    if bin[sy * w + sx] < 128 {
                        ink += 1;
                    }
                }
            }
            if total > 0 && ink * 2 >= total {
                out[oy * nw + ox] = 0;
            }
        }
    }
    (out, nw as u32, nh as u32)
}

fn dump_arms(
    dir: &std::path::Path,
    rgb_full: &[u8],
    w_full: u32,
    h_full: u32,
    target_h: u32,
    binarizer: Option<&mut WgpuBinarizer>,
) -> Result<()> {
    // Binarize at the pipeline's operating point (target_h), not the raw scan res.
    let (rgb, w, h) = to_operating_point(rgb_full, w_full, h_full, target_h);
    let rgb = rgb.as_slice();
    let (wu, hu) = (w as usize, h as usize);
    let opt = BinarizationOptions::default();

    let cpu_fused = binarize_image_raw(rgb, wu, hu, &opt);
    save_gray(dir, "cpu_fused", &cpu_fused, w, h)?;

    let Some(binarizer) = binarizer else {
        eprintln!("[arms] GPU unavailable; CPU dump only.");
        return Ok(());
    };
    let gray = rgb_to_grayscale_u8(rgb);
    let constants = compute_adaptive_gpu_constants(&gray, wu, hu);
    for (mode, name) in [
        (0u32, "gpu_fused"),
        (1, "gpu_sauvola"),
        (2, "gpu_otsu"),
        (3, "gpu_bg"),
        (4, "gpu_normalized"),
        (5, "gpu_mean"),
        (6, "gpu_stddev"),
    ] {
        let params = BinarizationParams {
            width: w,
            height: h,
            mode: BinarizationMode::Adaptive,
            invert_output: opt.invert,
            k_factor: opt.k_factor,
            fixed_threshold: opt.fixed_threshold,
            adaptive: constants,
            debug_mode: mode,
        };
        match binarizer.binarize_gray_raw(&gray, &params) {
            Ok(data) => save_gray(dir, name, &data, w, h)?,
            Err(e) => eprintln!("[arms] debug_mode {mode} ({name}) failed: {e}"),
        }
    }
    Ok(())
}

fn dump_filters(dir: &std::path::Path, rgb: &[u8], w: u32, h: u32, target_h: u32) -> Result<()> {
    let nh = target_h.max(1);
    let nw = ((w as u64 * nh as u64) / h as u64).max(1) as u32;
    let opt = BinarizationOptions::default();
    for (filter, name) in [
        (FilterType::Lanczos3, "lanczos3"),
        (FilterType::CatmullRom, "catmullrom_mitchell"),
        (FilterType::Triangle, "triangle_area"),
        (FilterType::Gaussian, "gaussian"),
    ] {
        let small = resize_rgb(rgb, w, h, nw, nh, filter);
        let bin = binarize_image_raw(&small, nw as usize, nh as usize, &opt);
        save_gray(dir, &format!("filter_{name}"), &bin, nw, nh)?;
    }
    println!("[filters] downscaled {w}x{h} -> {nw}x{nh}");
    Ok(())
}

fn dump_res(dir: &std::path::Path, rgb: &[u8], w: u32, h: u32, target_h: u32) -> Result<()> {
    let factor = (h as f32 / target_h.max(1) as f32).round().max(1.0) as usize;
    let opt = BinarizationOptions::default();

    // Reduce-then-binarize (current pipeline order).
    let nh = h / factor as u32;
    let nw = w / factor as u32;
    let (nw, nh) = (nw.max(1), nh.max(1));
    let small = resize_rgb(rgb, w, h, nw, nh, FilterType::Lanczos3);
    let bin_a = binarize_image_raw(&small, nw as usize, nh as usize, &opt);
    save_gray(dir, "res_reduce_then_binarize", &bin_a, nw, nh)?;

    // Binarize-then-reduce (the experiment): binarize full res, coverage-reduce bilevel.
    let bin_full = binarize_image_raw(rgb, w as usize, h as usize, &opt);
    let (bin_b, bw, bh) = coverage_reduce(&bin_full, w as usize, h as usize, factor);
    save_gray(dir, "res_binarize_then_reduce", &bin_b, bw, bh)?;

    println!("[res] factor={factor}: reduce->bin {nw}x{nh}, bin->reduce {bw}x{bh}");
    Ok(())
}

fn dump_window(
    dir: &std::path::Path,
    rgb_full: &[u8],
    w_full: u32,
    h_full: u32,
    target_h: u32,
    binarizer: Option<&mut WgpuBinarizer>,
) -> Result<()> {
    let Some(binarizer) = binarizer else {
        eprintln!("[window] GPU unavailable; skipping window sweep.");
        return Ok(());
    };
    let (rgb, w, h) = to_operating_point(rgb_full, w_full, h_full, target_h);
    let rgb = rgb.as_slice();
    let (wu, hu) = (w as usize, h as usize);
    let gray = rgb_to_grayscale_u8(rgb);
    let base = compute_adaptive_gpu_constants(&gray, wu, hu);
    let opt = BinarizationOptions::default();
    for win in [15u32, 31, 51, 81, 101] {
        let mut c = base;
        c.sauvola_window = win; // override the adaptive window
        let params = BinarizationParams {
            width: w,
            height: h,
            mode: BinarizationMode::Adaptive,
            invert_output: opt.invert,
            k_factor: opt.k_factor,
            fixed_threshold: opt.fixed_threshold,
            adaptive: c,
            debug_mode: 0,
        };
        match binarizer.binarize_gray_raw(&gray, &params) {
            Ok(data) => save_gray(dir, &format!("window_{win:03}"), &data, w, h)?,
            Err(e) => eprintln!("[window] win={win} failed: {e}"),
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: binarize_compare <input-image> <out-dir> [arms|filters|res|window|all] [target_h]";
    let input = args.next().ok_or_else(|| anyhow!("{usage}"))?;
    let out_dir = std::path::PathBuf::from(args.next().ok_or_else(|| anyhow!("{usage}"))?);
    let mode = args.next().unwrap_or_else(|| "all".to_string());
    std::fs::create_dir_all(&out_dir).with_context(|| format!("mkdir {}", out_dir.display()))?;

    let rgb = image::open(&input)
        .with_context(|| format!("opening {input}"))?
        .to_rgb8();
    let (w, h) = rgb.dimensions();
    let rgb_raw = rgb.into_raw();
    let target_h: u32 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or((h / 2).max(1));
    println!("input {w}x{h}, mode={mode}, target_h={target_h}");

    // One binarizer shared across GPU-using modes; None if GPU is unavailable.
    let mut binarizer = WgpuBinarizer::new()
        .map_err(|e| eprintln!("GPU unavailable ({e}); GPU modes skipped."))
        .ok();

    let run = |m: &str, b: &mut Option<WgpuBinarizer>| -> Result<()> {
        match m {
            "arms" => dump_arms(&out_dir, &rgb_raw, w, h, target_h, b.as_mut()),
            "filters" => dump_filters(&out_dir, &rgb_raw, w, h, target_h),
            "res" => dump_res(&out_dir, &rgb_raw, w, h, target_h),
            "window" => dump_window(&out_dir, &rgb_raw, w, h, target_h, b.as_mut()),
            other => Err(anyhow!("unknown mode '{other}'")),
        }
    };

    if mode == "all" {
        for m in ["arms", "filters", "res", "window"] {
            run(m, &mut binarizer)?;
        }
    } else {
        run(&mode, &mut binarizer)?;
    }

    println!("done -> {}", out_dir.display());
    Ok(())
}
