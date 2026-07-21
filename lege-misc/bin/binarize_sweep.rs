//! Comprehensive parameter sweep for Sauvola-Otsu fusion binarization.
//!
//! Usage:
//!   cargo run -p lege --bin binarize_sweep -- <input-image> <out-dir> [target_h]
//!
//! Sweeps k_factor and writes per-mode PNGs so you can see what each arm
//! contributes at each setting.  Also writes a csv summary of per-arm pixel
//! counts for quick diffing.

use anyhow::{Context, Result, anyhow};
use image::{GrayImage, ImageBuffer, Luma, RgbImage};

use lege::color::BinarizationOptions;
use lege::color::binarization::{binarize_image_raw, compute_adaptive_gpu_constants};
use lege::color::color_processing::rgb_to_grayscale_u8;

use lege_gpu::binarization::wgpu::WgpuBinarizer;
use lege_gpu::binarization::{BinarizationMode, BinarizationParams};

/// Save a gray image to out_dir/{name}.png
fn save_gray(dir: &std::path::Path, name: &str, data: &[u8], w: u32, h: u32) -> Result<()> {
    let img: GrayImage = ImageBuffer::<Luma<u8>, _>::from_raw(w, h, data.to_vec())
        .ok_or_else(|| anyhow!("buffer/size mismatch for {name} ({}!={w}x{h})", data.len()))?;
    let path = dir.join(format!("{name}.png"));
    img.save(&path)
        .with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

/// Downscale interleaved RGB to nw×nh with Lanczos3.
fn resize_rgb(rgb: &[u8], w: u32, h: u32, nw: u32, nh: u32) -> Vec<u8> {
    let img = RgbImage::from_raw(w, h, rgb.to_vec()).expect("rgb dims");
    image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3).into_raw()
}

/// Downscale to target_h (preserving aspect) for pipeline operating point.
fn to_operating_point(rgb: &[u8], w: u32, h: u32, target_h: u32) -> (Vec<u8>, u32, u32) {
    if h <= target_h {
        return (rgb.to_vec(), w, h);
    }
    let nh = target_h.max(1);
    let nw = ((w as u64 * nh as u64) / h as u64).max(1) as u32;
    (resize_rgb(rgb, w, h, nw, nh), nw, nh)
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: binarize_sweep <input-image> <out-dir> [target_h]";
    let input = args.next().ok_or_else(|| anyhow!("{usage}"))?;
    let out_dir = std::path::PathBuf::from(args.next().ok_or_else(|| anyhow!("{usage}"))?);
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

    let (rgb, w, h) = to_operating_point(&rgb_raw, w, h, target_h);
    let gray = rgb_to_grayscale_u8(&rgb);
    let (wu, hu) = (w as usize, h as usize);
    let base_constants = compute_adaptive_gpu_constants(&gray, wu, hu);

    println!(
        "input {}x{} (op pt {}x{}), base_constants: sauvola_window={} bg_window={} percentile_c={} otsu_threshold={}",
        rgb_raw.len(),
        h,
        w,
        h,
        base_constants.sauvola_window,
        base_constants.bg_window,
        base_constants.percentile_c,
        base_constants.otsu_threshold
    );

    let mut binarizer = WgpuBinarizer::new()
        .map_err(|e| {
            eprintln!("GPU unavailable ({e}); CPU fallback fused only.");
            e
        })
        .ok();

    let k_factors: &[f32] = &[0.05, 0.10, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40];
    let modes: &[(&str, u32)] = &[("fused", 0), ("sauvola", 1), ("otsu", 2)];

    // CSV summary
    let csv_path = out_dir.join("sweep_summary.csv");
    let mut csv = String::from("k_factor,mode,ink_px,paper_px,total_px\n");

    for &k in k_factors {
        let constants = base_constants;
        // constants.sauvola_window = 31; // optionally fix window for cleaner comparison

        for &(mode_label, debug_mode) in modes {
            let label = format!("k{:03}_{}", (k * 100.0) as u8, mode_label);
            let params = BinarizationParams {
                width: w,
                height: h,
                mode: BinarizationMode::Adaptive,
                invert_output: false,
                k_factor: k,
                fixed_threshold: 200,
                adaptive: constants,
                debug_mode,
            };

            let data = if let Some(ref mut b) = binarizer {
                b.binarize_gray_raw(&gray, &params).unwrap_or_else(|e| {
                    eprintln!("  [{label}] GPU error: {e}; falling back to CPU fused");
                    // CPU fallback only for fused
                    binarize_image_raw(
                        &rgb,
                        wu,
                        hu,
                        &BinarizationOptions {
                            k_factor: k,
                            ..Default::default()
                        },
                    )
                })
            } else {
                // CPU fallback (fused mode only)
                binarize_image_raw(
                    &rgb,
                    wu,
                    hu,
                    &BinarizationOptions {
                        k_factor: k,
                        ..Default::default()
                    },
                )
            };

            save_gray(&out_dir, &label, &data, w, h)?;

            let ink_px = data.iter().filter(|&&p| p == 0).count();
            let paper_px = data.iter().filter(|&&p| p == 255).count();
            csv.push_str(&format!(
                "{k:.2},{mode_label},{ink_px},{paper_px},{}\n",
                wu * hu
            ));
            println!("  {label}: ink={ink_px} paper={paper_px} (of {})", wu * hu);
        }
    }

    // Also sweep window size at default k_factor on fused mode
    println!("\n--- Window sweep at k=0.20 (fused) ---");
    for &win in &[15u32, 31, 51, 81, 101] {
        let mut c = base_constants;
        c.sauvola_window = win;
        let label = format!("win{:03}_fused", win);
        if let Some(ref mut b) = binarizer {
            let params = BinarizationParams {
                width: w,
                height: h,
                mode: BinarizationMode::Adaptive,
                invert_output: false,
                k_factor: 0.20,
                fixed_threshold: 200,
                adaptive: c,
                debug_mode: 0,
            };
            match b.binarize_gray_raw(&gray, &params) {
                Ok(data) => {
                    let ink_px = data.iter().filter(|&&p| p == 0).count();
                    save_gray(&out_dir, &label, &data, w, h)?;
                    println!("  {label}: ink={ink_px}");
                    csv.push_str(&format!(
                        "0.20,fused_win{win},{ink_px},{},{}\n",
                        wu * hu - ink_px,
                        wu * hu
                    ));
                }
                Err(e) => eprintln!("  {label} failed: {e}"),
            }
        }
    }

    std::fs::write(&csv_path, &csv)?;
    println!("\nSummary written to {}", csv_path.display());
    println!("done -> {}", out_dir.display());
    Ok(())
}
