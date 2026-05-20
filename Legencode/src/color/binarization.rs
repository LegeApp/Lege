//! Pure-CPU document binarization module in Rust
//! Replicates OpenCV Sauvola-based precision binarization and PBM encoding

use crate::types::BinarizationOptions;
use anyhow::{Result, anyhow};
use image::{GrayImage, Luma};
use ndarray::Array4;
// GPU execution providers intentionally excluded for HeavySauvola – CPU only.
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Value;
use rayon::prelude::*;
use rayon::slice::ParallelSliceMut;
use std::cmp::{max, min};
use std::sync::Mutex;

/// Format bytes into human-readable memory sizes
fn format_memory_size(bytes: usize) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    if unit_index == 0 {
        format!("{} {}", bytes, UNITS[unit_index])
    } else {
        format!("{:.1} {}", size, UNITS[unit_index])
    }
}

/// Heavy-duty ONNX Sauvola processor for degraded documents
pub struct HeavySauvolaProcessor {
    session: Session,
}

impl HeavySauvolaProcessor {
    pub fn new() -> Result<Self> {
        let model_path = crate::runtime_asset_path_if_exists("sauvola.onnx").ok_or_else(|| {
            anyhow!("Heavy-duty Sauvola model not found (expected sauvola.onnx near the executable or under share/lege/models; set LEGE_ASSET_DIR to override).")
        })?;

        // Check model file size
        if let Ok(metadata) = std::fs::metadata(&model_path) {
            #[cfg(feature = "debug-logging")]
            {
                crate::streamline::log_debug_message(&format!(
                    "Loading DIBCO Sauvola model: {} ({:.1} KB)",
                    model_path.display(),
                    metadata.len() as f64 / 1024.0
                ));
            }
        }

        let mut builder = Session::builder()
            .map_err(|e| anyhow!("failed to create ORT session builder: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow!("failed to set ORT optimization level: {e}"))?;

        // Always use CPU; GPU execution providers are intentionally disabled for
        // HeavySauvola to ensure deterministic, stable binarization output.
        #[cfg(feature = "debug-logging")]
        crate::streamline::log_debug_message(
            "Heavy-duty Sauvola: Using CPU execution provider (forced)",
        );
        let session = builder.commit_from_file(&model_path)?;

        Ok(Self { session })
    }

    /// Process an RGB image region directly with zero-copy preprocessing.
    /// This performs grayscale conversion and normalization on the fly.
    pub fn process_rgb_region(
        &mut self,
        rgb_data: &[u8],
        width: u32,
        _height: u32,
        region_x: u32,
        region_y: u32,
        region_w: u32,
        region_h: u32,
        opt: &BinarizationOptions, // New: Pass options for invert_input
    ) -> Result<GrayImage> {
        // Directly construct the NHWC tensor from the source RGB slice
        let img_array = Array4::from_shape_fn(
            (1, region_h as usize, region_w as usize, 1),
            |(_, y, x, _)| {
                let src_x = region_x as usize + x;
                let src_y = region_y as usize + y;
                let idx = (src_y * width as usize + src_x) * 3;

                // Standard sRGB -> Grayscale conversion
                let r = rgb_data[idx] as u32;
                let g = rgb_data[idx + 1] as u32;
                let b = rgb_data[idx + 2] as u32;
                let mut gray = (r * 77 + g * 150 + b * 29) >> 8; // ~= 0.299R + 0.587G + 0.114B

                // Handle input inversion for inverted documents (black background, white text)
                if opt.invert_input {
                    gray = 255 - gray;
                }

                // Normalize to [0.0, 1.0] for the model
                (gray as f32) / 255.0
            },
        );

        let inputs = ort::inputs!["img01_inp" => Value::from_array(img_array)
            .map_err(|e| anyhow!("failed to create ORT input tensor: {e}"))?];
        let outputs = self.session.run(inputs)?;

        let output = outputs
            .values()
            .next()
            .ok_or_else(|| anyhow!("Model produced no output"))?;
        let (shape, data) = output.try_extract_tensor::<f32>()?;
        let output_array = Array4::from_shape_vec(
            (
                shape[0] as usize,
                shape[1] as usize,
                shape[2] as usize,
                shape[3] as usize,
            ),
            data.to_vec(),
        )?;

        Self::postprocess_output_static(output_array)
    }

    /// Process RGB image data directly (no preprocessing to grayscale)
    pub fn process_rgb_patch(
        &mut self,
        rgb_data: &[u8],
        width: u32,
        height: u32,
        opt: &BinarizationOptions,
    ) -> Result<GrayImage> {
        // Convert RGB to grayscale first since the ONNX model expects grayscale input
        // Using OpenCV's standard RGB to grayscale conversion: 0.299*R + 0.587*G + 0.114*B
        let mut gray_data = Vec::with_capacity((width * height) as usize);
        for chunk in rgb_data.chunks(3) {
            let r = chunk[0] as f32;
            let g = chunk[1] as f32;
            let b = chunk[2] as f32;
            let mut gray = 0.299 * r + 0.587 * g + 0.114 * b;

            // Handle input inversion for inverted documents (black background, white text)
            if opt.invert_input {
                gray = 255.0 - gray;
            }

            gray_data.push(gray);
        }

        // Apply the same normalization as the original model: simple /255.0
        // Convert grayscale to NHWC format (batch, height, width, channels=1) for the model
        let img_array =
            Array4::from_shape_fn((1, height as usize, width as usize, 1), |(_, h, w, _)| {
                let pixel_idx = h * width as usize + w;
                gray_data[pixel_idx] / 255.0
            });

        // Run inference
        let img_value = Value::from_array(img_array)
            .map_err(|e| anyhow!("failed to create ORT input tensor: {e}"))?;
        let inputs = ort::inputs!["img01_inp" => img_value];
        let outputs = self.session.run(inputs)?;

        // Extract output
        let output = outputs
            .values()
            .next()
            .ok_or_else(|| anyhow::anyhow!("ORT session returned no output"))?;
        let (shape, data) = output.try_extract_tensor::<f32>()?;
        let output_array = Array4::from_shape_vec(
            (
                shape[0] as usize,
                shape[1] as usize,
                shape[2] as usize,
                shape[3] as usize,
            ),
            data.to_vec(),
        )?;

        // Convert output to grayscale image
        Self::postprocess_output_static(output_array)
    }

    fn postprocess_output_static(output: Array4<f32>) -> Result<GrayImage> {
        let (_, out_h, out_w, _) = output.dim();
        let mut mask = GrayImage::new(out_w as u32, out_h as u32);

        // Simple thresholding of model output
        // This loop is small (per patch) so parallelization is overkill
        for y in 0..out_h {
            for x in 0..out_w {
                // Model outputs probability/confidence values - threshold at 0.5
                let pixel_value = if output[[0, y, x, 0]] > 0.5 {
                    255u8
                } else {
                    0u8
                };
                mask.put_pixel(x as u32, y as u32, Luma([pixel_value]));
            }
        }

        Ok(mask)
    }
}

/// Cached ORT session for `HeavySauvolaProcessor`. Building the session is expensive
/// (~hundreds of ms of ORT graph optimization), so we create it once and reuse.
static HEAVY_PROCESSOR_CACHE: std::sync::OnceLock<Option<Mutex<HeavySauvolaProcessor>>> =
    std::sync::OnceLock::new();

fn get_or_init_heavy_processor() -> Option<std::sync::MutexGuard<'static, HeavySauvolaProcessor>> {
    HEAVY_PROCESSOR_CACHE
        .get_or_init(|| HeavySauvolaProcessor::new().ok().map(Mutex::new))
        .as_ref()
        .and_then(|m| m.lock().ok())
}

/// Binarizes an RGB image based on the provided options.
///
/// # Arguments
/// - `image`: The input RGB image data as a byte slice (interleaved R,G,B).
/// - `width`: The width of the image in pixels.
/// - `height`: The height of the image in pixels.
/// - `options`: The binarization options (e.g., threshold, window size, etc.).
///
/// # Returns
/// A `Vec<u8>` containing the PBM P4 formatted binary image for downstream use.
///
/// # Panics
/// Panics if the input image size does not match `width * height * 3`.
pub fn binarize_image(
    image: &[u8],
    width: usize,
    height: usize,
    options: &BinarizationOptions,
) -> Vec<u8> {
    assert_eq!(
        image.len(),
        width * height * 3,
        "Input image size does not match width*height*3"
    );

    #[cfg(feature = "debug-logging")]
    crate::streamline::log_debug_message(&format!(
        "[Binarization] Input: {}x{} RGB ({} bytes), heavy_duty={}, fixed_threshold={}, invert_input={}, invert_output={}",
        width,
        height,
        image.len(),
        options.use_heavy_duty,
        options.use_fixed_threshold,
        options.invert_input,
        options.invert
    ));

    let bin = binarize_image_raw(image, width, height, options);
    let pbm_result = pbm::make_pbm_p4(&bin, width, height);

    #[cfg(feature = "debug-logging")]
    crate::streamline::log_debug_message(&format!(
        "[Binarization] Output: {}x{} binary ({} raw bytes -> {} PBM bytes)",
        width,
        height,
        bin.len(),
        pbm_result.len()
    ));

    pbm_result
}

/// Binarizes an RGB image and returns raw binary data (1 byte per pixel).
///
/// # Arguments
/// - `image`: The input RGB image data as a byte slice (interleaved R,G,B).
/// - `width`: The width of the image in pixels.
/// - `height`: The height of the image in pixels.
/// - `options`: The binarization options (e.g., threshold, window size, etc.).
///
/// # Returns
/// A `Vec<u8>` containing raw binary image data (0 or 255 per pixel) for JBIG2 encoding.
///
/// # Panics
/// Panics if the input image size does not match `width * height * 3`.
pub fn binarize_image_raw(
    image: &[u8],
    width: usize,
    height: usize,
    options: &BinarizationOptions,
) -> Vec<u8> {
    binarize_image_raw_into(image, width, height, options)
}

/// Callback variant: produces binarized bytes and hands them to `f` without forcing
/// an intermediate `Vec<u8>` allocation when the GPU path is available. The callback
/// receives a slice of exactly `width * height` bytes.
///
/// On the GPU fast-path, `f` is invoked over the GPU-mapped readback buffer (one
/// memcpy already performed during readback; no further allocation). On the CPU
/// fallback path, a `Vec<u8>` is materialized internally and passed to `f`.
pub fn binarize_image_raw_with<F, R>(
    image: &[u8],
    width: usize,
    height: usize,
    options: &BinarizationOptions,
    f: F,
) -> R
where
    F: FnOnce(&[u8]) -> R,
{
    assert_eq!(
        image.len(),
        width * height * 3,
        "Input image size does not match width*height*3"
    );

    // Hold f in an Option so we can pass it into the GPU closure by &mut Option<F>::take().
    // On GPU success the closure consumes f; on failure f remains and we fall through.
    let mut f_slot: Option<F> = Some(f);

    // Heavy-duty path always materializes (ONNX) — call f on result.
    if !options.use_fixed_threshold && options.use_heavy_duty {
        if let Some(mut processor) = get_or_init_heavy_processor() {
            if let Ok(result) =
                apply_heavy_duty_binarization_raw(&mut processor, image, width, height, options)
            {
                return (f_slot.take().unwrap())(&result);
            }
        }
    }

    // GPU fast-path: feed RGB directly to the GPU. The linearize pre-pass on the GPU
    // applies an sRGB→linear LUT, computes BT.709 luma in linear space, and re-encodes
    // to sRGB gray — preserving perceptual contrast at GPU speed.
    if !options.use_heavy_duty && !options.disable_gpu {
        let n = width * height;
        // For invert_input, we'd need to invert RGB before upload (or extend the shader).
        // The pre-existing CPU fallback handles invert_input via the linearized gray path,
        // so when invert_input is set, prefer the CPU path for now.
        if !options.invert_input {
            let result = try_gpu_binarize_rgb_raw_with(image, width, height, options, |data| {
                (f_slot.take().unwrap())(&data[..n])
            });
            if let Some(r) = result {
                return r;
            }
        }
    }

    // CPU fallback: materialize then call f.
    let result = binarize_image_raw_into(image, width, height, options);
    (f_slot.take().unwrap())(&result)
}

fn try_gpu_binarize_rgb_raw_with<F, R>(
    rgb: &[u8],
    width: usize,
    height: usize,
    options: &BinarizationOptions,
    f: F,
) -> Option<R>
where
    F: FnOnce(&[u8]) -> R,
{
    let mode = if options.use_fixed_threshold {
        lege_gpu::binarization::BinarizationMode::FixedThreshold
    } else {
        lege_gpu::binarization::BinarizationMode::Adaptive
    };
    let adaptive = if options.use_fixed_threshold {
        lege_gpu::binarization::AdaptiveBinarizeGpuConstants {
            sauvola_window: sauvola_window_for(height, width) as u32,
            bg_window: odd_background_window(height) as u32,
            percentile_c: 0,
            otsu_threshold: 0,
        }
    } else {
        // Constants are computed from a histogram of the linearized luma. We approximate
        // with integer luma here for the histogram (cheap), since percentile_c and the
        // Otsu threshold operate on quantized 0..255 space and integer luma is close
        // enough for threshold selection. The actual binarization on GPU uses the
        // proper linear+sRGB gray.
        let n = width * height;
        let mut hist = [0u32; 256];
        for chunk in rgb.chunks_exact(3) {
            let r = chunk[0] as u32;
            let g = chunk[1] as u32;
            let b = chunk[2] as u32;
            let y = ((r * 77 + g * 150 + b * 29) >> 8) as usize;
            hist[y] += 1;
        }
        let percentile_c = percentile_from_hist(&hist, 0.30, n);
        let otsu_threshold = otsu_from_hist(&hist, n);
        lege_gpu::binarization::AdaptiveBinarizeGpuConstants {
            sauvola_window: sauvola_window_for(height, width) as u32,
            bg_window: odd_background_window(height) as u32,
            percentile_c,
            otsu_threshold,
        }
    };
    let params = lege_gpu::binarization::BinarizationParams {
        width: width as u32,
        height: height as u32,
        mode,
        invert_output: options.invert,
        k_factor: options.k_factor,
        fixed_threshold: options.fixed_threshold,
        adaptive,
        debug_mode: 0,
    };
    lege_gpu::binarization::try_binarize_rgb_with(rgb, &params, f)
}

fn binarize_image_raw_into(
    image: &[u8],
    width: usize,
    height: usize,
    options: &BinarizationOptions,
) -> Vec<u8> {
    assert_eq!(
        image.len(),
        width * height * 3,
        "Input image size does not match width*height*3"
    );

    #[cfg(feature = "debug-logging")]
    crate::streamline::log_debug_message(&format!(
        "[BinarizationRaw] Processing {}x{} image, method: {}",
        width,
        height,
        if options.use_fixed_threshold {
            "fixed_threshold"
        } else if options.use_heavy_duty {
            "heavy_duty"
        } else {
            "light_sauvola"
        }
    ));

    if !options.use_fixed_threshold && options.use_heavy_duty {
        if let Some(mut processor) = get_or_init_heavy_processor() {
            match apply_heavy_duty_binarization_raw(&mut processor, image, width, height, options) {
                Ok(result) => {
                    #[cfg(feature = "debug-logging")]
                    crate::streamline::log_debug_message(&format!(
                        "[BinarizationRaw] Heavy-duty completed: {}x{} -> {} binary bytes",
                        width,
                        height,
                        result.len()
                    ));
                    return result;
                }
                Err(_e) => {
                    #[cfg(feature = "debug-logging")]
                    crate::streamline::log_debug_message(&format!(
                        "Heavy-duty binarization failed: {}",
                        _e
                    ));
                }
            }
        }
    }

    // Light binarization: improved Sauvola + adaptive Otsu fusion
    #[cfg(feature = "debug-logging")]
    crate::streamline::log_debug_message(&format!(
        "[BinarizationRaw] Using light binarization for {}x{} image",
        width, height
    ));

    // Integer Rec.601 luma — used both for the GPU fast-path and for fixed-threshold
    // CPU fallback. Avoids the ~12 byte/pixel transient + slow palette conversion of
    // the f32 sRGB-linearize path. For adaptive Sauvola the linearized luma below
    // is preferred (slight quality edge), so we materialize int luma lazily.
    let want_int_luma =
        !options.use_heavy_duty && (options.use_fixed_threshold || !options.disable_gpu);
    let int_luma: Option<Vec<u8>> = if want_int_luma {
        let mut g = vec![0u8; width * height];
        g.par_iter_mut()
            .zip(image.par_chunks_exact(3))
            .for_each(|(out, rgb)| {
                let r = rgb[0] as u32;
                let gv = rgb[1] as u32;
                let b = rgb[2] as u32;
                *out = ((r * 77 + gv * 150 + b * 29) >> 8) as u8;
            });
        if options.invert_input {
            g.par_iter_mut().for_each(|p| *p = 255 - *p);
        }
        Some(g)
    } else {
        None
    };

    // GPU fast-path on integer luma.
    if !options.use_heavy_duty && !options.disable_gpu {
        if let Some(gray_fast) = int_luma.as_deref() {
            if let Some(result) = try_gpu_binarize_gray_raw(gray_fast, width, height, options) {
                return result;
            }
        }
    }

    // Fixed-threshold CPU fallback uses the integer luma directly — no need to pay
    // the f32 linearize cost since fixed thresholding is already a coarse operation.
    if options.use_fixed_threshold {
        let gray = int_luma.expect("int_luma materialized for fixed-threshold path");
        let mut result = vec![0u8; width * height];
        apply_threshold(&gray, options.fixed_threshold, &mut result, width, height);
        if options.invert {
            result.par_iter_mut().for_each(|p| *p = 255 - *p);
        }
        return result;
    }

    // Adaptive CPU fallback: materialize linearized luma for slightly better quality
    // on Sauvola+Otsu fusion. (The transient f32 buffer is the price of accuracy.)
    let linear_rgb = crate::color::linearize::linearize_rgb_bytes_to_f32(image);
    let mut gray = vec![0; width * height];
    crate::color::linearize::linearized_rgb_to_grayscale(&linear_rgb, &mut gray);
    drop(linear_rgb);

    if options.invert_input {
        #[cfg(feature = "debug-logging")]
        crate::streamline::log_debug_message(&format!(
            "[BinarizationRaw] Applying input inversion to {}x{} grayscale",
            width, height
        ));
        gray.par_iter_mut().for_each(|p| *p = 255 - *p);
    }

    // GPU retry with linearized grayscale (reached only if first GPU attempt failed/unavailable).
    if !options.disable_gpu {
        if let Some(result) = try_gpu_binarize_gray_raw(&gray, width, height, options) {
            return result;
        }
    }

    let mut result = improved_binarize(&gray, width, height, options);

    if options.invert {
        result.par_iter_mut().for_each(|p| *p = 255 - *p);
    }

    result
}

fn try_gpu_binarize_gray_raw(
    gray: &[u8],
    width: usize,
    height: usize,
    options: &BinarizationOptions,
) -> Option<Vec<u8>> {
    try_gpu_binarize_gray_raw_with(gray, width, height, options, |data| {
        let mut out = data.to_vec();
        out.truncate(width * height);
        out
    })
}

/// Try GPU binarization with a callback that receives the mapped data directly.
/// This avoids an extra copy from mapped GPU memory to a new Vec.
fn try_gpu_binarize_gray_raw_with<F, R>(
    gray: &[u8],
    width: usize,
    height: usize,
    options: &BinarizationOptions,
    f: F,
) -> Option<R>
where
    F: FnOnce(&[u8]) -> R,
{
    let mode = if options.use_fixed_threshold {
        lege_gpu::binarization::BinarizationMode::FixedThreshold
    } else {
        lege_gpu::binarization::BinarizationMode::Adaptive
    };
    let adaptive = if options.use_fixed_threshold {
        lege_gpu::binarization::AdaptiveBinarizeGpuConstants {
            sauvola_window: sauvola_window_for(height, width) as u32,
            bg_window: odd_background_window(height) as u32,
            percentile_c: 0,
            otsu_threshold: 0,
        }
    } else {
        compute_adaptive_gpu_constants(gray, width, height)
    };
    let params = lege_gpu::binarization::BinarizationParams {
        width: width as u32,
        height: height as u32,
        mode,
        invert_output: options.invert,
        k_factor: options.k_factor,
        fixed_threshold: options.fixed_threshold,
        adaptive,
        debug_mode: 0,
    };
    lege_gpu::binarization::try_binarize_gray_with(gray, &params, f)
}

pub fn compute_adaptive_gpu_constants(
    gray: &[u8],
    width: usize,
    height: usize,
) -> lege_gpu::binarization::AdaptiveBinarizeGpuConstants {
    let n = width * height;
    assert_eq!(gray.len(), n);

    // Build histogram once: used for both percentile_c and Otsu.
    // The GPU recomputes background estimation (bg_max passes) internally, so we skip
    // the CPU dilate_square_reflect + normalize_by_bg that was previously done here.
    // Otsu on raw gray is slightly different from Otsu on bg-normalized gray, but
    // the Sauvola arm of the AND-fusion dominates in practice.
    let hist = hist256(gray);
    let percentile_c = percentile_from_hist(&hist, 0.30, n);
    let otsu_threshold = otsu_from_hist(&hist, n);

    lege_gpu::binarization::AdaptiveBinarizeGpuConstants {
        sauvola_window: sauvola_window_for(height, width) as u32,
        bg_window: odd_background_window(height) as u32,
        percentile_c,
        otsu_threshold,
    }
}

/// Improved binarization matching the Python script: adaptive Sauvola + background-normalized Otsu + AND fusion.
///
/// Buffer reuse: previously allocated 5 separate W*H byte buffers (`sauvola_bin`,
/// `bg`, `normalized`, `otsu_bin`, `fused`); now uses 2. `bg` is overwritten with
/// the normalized image (since `normalize_by_bg` reads `gray`+`bg` and writes
/// per-pixel), then with the Otsu thresholding result, and finally AND'd into
/// `sauvola_bin` which is returned as the fused output.
fn improved_binarize(
    gray: &[u8],
    width: usize,
    height: usize,
    opt: &BinarizationOptions,
) -> Vec<u8> {
    let n = width * height;
    assert!(gray.len() == n);

    let win = sauvola_window_for(height, width);

    let mut sauvola_bin = vec![0u8; n];
    sauvola_via_integral(
        gray,
        width,
        height,
        win,
        opt.k_factor as f32,
        &mut sauvola_bin,
    );

    // `scratch` rotates: dilated bg → normalized → Otsu binary.
    let s = odd_background_window(height);
    let mut scratch = vec![0u8; n];
    dilate_square_reflect(gray, width, height, s, &mut scratch);

    let hist = hist256(gray);
    let c = percentile_from_hist(&hist, 0.30, n);

    // In-place: normalize_by_bg reads gray + scratch(bg) and writes back into scratch.
    normalize_by_bg_inplace(gray, c, &mut scratch);

    let t_otsu = otsu_threshold_u8(&scratch);
    scratch
        .par_iter_mut()
        .for_each(|v| *v = if *v > t_otsu { 255 } else { 0 });

    // Fuse AND into sauvola_bin (in place) — sauvola_bin becomes the fused output.
    sauvola_bin
        .par_iter_mut()
        .zip(scratch.par_iter())
        .for_each(|(s, &o)| {
            *s = if *s != 0 && o != 0 { 255 } else { 0 };
        });

    sauvola_bin
}

/// Adaptive window size for Sauvola
#[inline]
pub fn sauvola_window_for(h: usize, w: usize) -> usize {
    let mut win = (h.min(w) / 40).max(31).min(101);
    if win % 2 == 0 {
        win += 1;
    }
    win
}

#[inline]
pub fn odd_background_window(height: usize) -> usize {
    let mut s = max(3, min(height / 200, 15));
    if s % 2 == 0 {
        s += 1;
    }
    s
}

/// Sauvola via integral images.
///
/// Memory layout: previously materialized a padded gray buffer + two index tables
/// (reflect_x/reflect_y) before building the two u64 integral images. The padded
/// buffer (~W*H bytes for typical windows) and reflection Vecs are now eliminated:
/// the row prefix-sum pass reads reflected source pixels directly from `gray`.
fn sauvola_via_integral(
    gray: &[u8],
    width: usize,
    height: usize,
    win: usize,
    k: f32,
    bin: &mut [u8],
) {
    let r = (win / 2) as isize;
    let pad_w = width + 2 * (r as usize);
    let pad_h = height + 2 * (r as usize);

    let iw = pad_w + 1;
    let mut integ = vec![0u64; iw * (pad_h + 1)];
    let mut integ_sq = vec![0u64; iw * (pad_h + 1)];

    // Row prefix sums (parallelized per row). Each row pulls reflected source
    // pixels directly from `gray`, avoiding a separate padded image allocation.
    let w_isz = width as isize;
    let h_isz = height as isize;
    integ
        .par_chunks_mut(iw)
        .zip(integ_sq.par_chunks_mut(iw))
        .enumerate()
        .skip(1) // Skip row 0 (remains zeros)
        .for_each(|(y, (integ_row, integ_sq_row))| {
            let src_y = reflect_101((y - 1) as isize - r as isize, h_isz) as usize;
            let row_base = src_y * width;
            let mut sum = 0u64;
            let mut sum_sq = 0u64;
            for x in 1..=pad_w {
                let src_x = reflect_101((x - 1) as isize - r as isize, w_isz) as usize;
                let v = gray[row_base + src_x] as u64;
                sum += v;
                sum_sq += v * v;
                integ_row[x] = sum;
                integ_sq_row[x] = sum_sq;
            }
        });

    // Column prefix (sequential, requires cumulative sum)
    for x in 1..=pad_w {
        for y in 2..=pad_h {
            let idx = y * iw + x;
            let prev_idx = (y - 1) * iw + x;
            integ[idx] += integ[prev_idx];
            integ_sq[idx] += integ_sq[prev_idx];
        }
    }

    // Threshold per pixel
    let inv_area = 1.0f64 / (win as f64 * win as f64);
    bin.par_chunks_mut(width)
        .enumerate()
        .for_each(|(y_orig, bin_row)| {
            let gray_row = &gray[y_orig * width..(y_orig + 1) * width];
            for x_orig in 0..width {
                let x_padded_center = x_orig + r as usize;
                let y_padded_center = y_orig + r as usize;
                let x0 = x_padded_center - r as usize;
                let x1 = x_padded_center + r as usize;
                let y0 = y_padded_center - r as usize;
                let y1 = y_padded_center + r as usize;

                let d = integ[(y1 + 1) * iw + (x1 + 1)] as f64;
                let b = integ[(y1 + 1) * iw + x0] as f64;
                let c = integ[y0 * iw + (x1 + 1)] as f64;
                let a = integ[y0 * iw + x0] as f64;
                let sum = d + a - b - c;

                let d_sq = integ_sq[(y1 + 1) * iw + (x1 + 1)] as f64;
                let b_sq = integ_sq[(y1 + 1) * iw + x0] as f64;
                let c_sq = integ_sq[y0 * iw + (x1 + 1)] as f64;
                let a_sq = integ_sq[y0 * iw + x0] as f64;
                let sum_sq = d_sq + a_sq - b_sq - c_sq;

                let mean = sum * inv_area;
                let var = (sum_sq * inv_area) - mean * mean;
                let std = if var > 0.0 { var.sqrt() } else { 0.0 };

                let thr = sauvola_threshold(mean as f32, std as f32, k);
                let px = gray_row[x_orig] as f32;
                bin_row[x_orig] = if px > thr { 255 } else { 0 };
            }
        });
}

#[inline]
fn sauvola_threshold(mu: f32, sigma: f32, k: f32) -> f32 {
    mu * (1.0 + k * (sigma / 127.0 - 1.0))
}

// 1-D sliding max filter (van Herk / Gil-Werman) for a line of u8
fn maxfilter_1d_u8(src: &[u8], k: usize, dst: &mut [u8]) {
    let n = src.len();
    assert!(k >= 1 && k % 2 == 1 && dst.len() == n);
    let r = k / 2;
    let mut g = vec![0u8; n];
    let mut h = vec![0u8; n];

    // forward
    for i in 0..n {
        g[i] = if i % k == 0 {
            src[i]
        } else {
            g[i - 1].max(src[i])
        }
    }
    // backward
    for i in (0..n).rev() {
        h[i] = if i + 1 == n || (i + 1) % k == 0 {
            src[i]
        } else {
            h[i + 1].max(src[i])
        }
    }
    // combine: Van Herk/Gil — h gives suffix max from i-r, g gives prefix max to i+r
    for i in 0..n {
        let a = if i >= r { h[i - r] } else { h[0] };
        let b = if i + r < n { g[i + r] } else { g[n - 1] };
        dst[i] = a.max(b);
    }
}

// 2-D separable max filter with reflect-101 borders (approximates ellipse with square)
fn dilate_square_reflect(gray: &[u8], w: usize, h: usize, k: usize, out: &mut [u8]) {
    let mut tmp = vec![0u8; w * h];
    // horizontal
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let mut line = vec![0u8; w + 2 * (k / 2)];
        // reflect-101 pad into `line`
        for x in 0..line.len() {
            let rx = reflect_101(x as isize - (k / 2) as isize, w as isize) as usize;
            line[x] = gray[y * w + rx];
        }
        let mut maxed = vec![0u8; line.len()];
        maxfilter_1d_u8(&line, k, &mut maxed);
        row.copy_from_slice(&maxed[k / 2..k / 2 + w]);
    });
    // vertical - process columns in parallel (each column writes to disjoint positions)
    let r = k / 2;
    (0..w).into_par_iter().for_each(|x| {
        // build padded column
        let mut col = vec![0u8; h + 2 * r];
        for py in 0..col.len() {
            let ry = reflect_101(py as isize - r as isize, h as isize) as usize;
            col[py] = tmp[ry * w + x];
        }

        // apply 1D max filter on the column
        let mut maxed = vec![0u8; col.len()];
        maxfilter_1d_u8(&col, k, &mut maxed);

        // write back the central region into the output column (safe: each thread writes disjoint x)
        for row_idx in 0..h {
            let val = maxed[r + row_idx];
            // SAFETY: Each thread processes a unique column x, so writes to out[row_idx * w + x] are disjoint
            unsafe {
                let ptr = out.as_ptr() as *mut u8;
                *ptr.add(row_idx * w + x) = val;
            }
        }
    });
}

/// Histogram for u8 data
fn hist256(data: &[u8]) -> [u32; 256] {
    let mut h = [0u32; 256];
    data.iter().for_each(|&v| h[v as usize] += 1);
    h
}

/// Percentile from histogram
fn percentile_from_hist(h: &[u32; 256], p: f32, n: usize) -> u8 {
    let target = (p * (n as f32)).round() as u32;
    let mut acc = 0u32;
    for i in 0..256 {
        acc += h[i];
        if acc >= target {
            return i as u8;
        }
    }
    255
}

/// In-place: `bg_then_out` enters as the dilated background and is overwritten
/// with the normalized result. Reads `gray[i]` and uses `bg_then_out[i]` as the
/// divisor before writing the new value.
fn normalize_by_bg_inplace(gray: &[u8], c: u8, bg_then_out: &mut [u8]) {
    let c = c as f32;
    bg_then_out
        .par_iter_mut()
        .zip(gray.par_iter())
        .for_each(|(slot, &g)| {
            let b = *slot as f32;
            let val = c / (b + 1e-6) * (g as f32);
            *slot = val.clamp(0.0, 255.0) as u8;
        });
}

/// Otsu threshold from a pre-built histogram.
fn otsu_from_hist(h: &[u32; 256], n: usize) -> u8 {
    let total = n as f64;
    let mut sum_all = 0.0f64;
    for i in 0..256 {
        sum_all += (i as f64) * (h[i] as f64);
    }
    let mut sum_b = 0.0f64;
    let mut w_b = 0.0f64;
    let mut max_var = -1.0f64;
    let mut thresh = 0u8;
    for t in 0..256 {
        w_b += h[t] as f64;
        if w_b == 0.0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f == 0.0 {
            break;
        }
        sum_b += (t as f64) * (h[t] as f64);
        let m_b = sum_b / w_b;
        let m_f = (sum_all - sum_b) / w_f;
        let var_between = w_b * w_f * (m_b - m_f).powi(2);
        if var_between > max_var {
            max_var = var_between;
            thresh = t as u8;
        }
    }
    thresh
}

/// Otsu threshold
fn otsu_threshold_u8(data: &[u8]) -> u8 {
    let h = hist256(data);
    otsu_from_hist(&h, data.len())
}

#[inline(always)]
fn reflect_101(idx: isize, len: isize) -> isize {
    if idx < 0 {
        -idx - 1
    } else if idx >= len {
        2 * len - idx - 1
    } else {
        idx
    }
}

/// Heavy-duty ONNX-based binarization returning raw binary data
pub fn apply_heavy_duty_binarization_raw(
    processor: &mut HeavySauvolaProcessor,
    image: &[u8],
    width: usize,
    height: usize,
    options: &BinarizationOptions,
) -> Result<Vec<u8>> {
    let result_img = if options.no_patch {
        #[cfg(feature = "debug-logging")]
        crate::streamline::log_debug_message(&format!(
            "Processing entire {}x{} image as a single patch.",
            width, height
        ));
        processor.process_rgb_region(
            image,
            width as u32,
            height as u32,
            0,
            0,
            width as u32,
            height as u32,
            options,
        )?
    } else {
        process_in_patches(processor, image, width, height, options)?
    };

    // Convert to binary data.
    let mut bin_data = result_img.into_raw();
    // ONNX postprocess uses 255 = model foreground (text). Adaptive Sauvola uses 0 = ink, 255 =
    // paper. Invert so heavy-duty output matches the rest of the pipeline (CCITT, JBIG2, masks).
    bin_data
        .par_iter_mut()
        .for_each(|pixel| *pixel = 255 - *pixel);
    if options.invert {
        bin_data
            .par_iter_mut()
            .for_each(|pixel| *pixel = 255 - *pixel);
    }

    Ok(bin_data)
}

/// Process an image in parallel patches and combine the results
fn process_in_patches(
    processor: &mut HeavySauvolaProcessor,
    image: &[u8],
    width: usize,
    height: usize,
    options: &BinarizationOptions,
) -> Result<GrayImage> {
    let patch_size =
        ((width.min(height) as f32 * options.patch_percentage / 100.0) as u32).max(256);
    let stride = (patch_size as f32 * 0.75) as u32; // 25% overlap for smooth blending
    #[cfg(feature = "debug-logging")]
    crate::streamline::log_debug_message(&format!(
        "Using patch size: {}x{}, stride: {}",
        patch_size, patch_size, stride
    ));

    // Accumulate blended output as (sum, weight) in flat arrays, then normalise once.
    let mut pixel_sum = vec![0.0f32; width * height];
    let mut weight_map = vec![0.0f32; width * height];

    let mut y = 0;
    while y < height as u32 {
        let mut x = 0;
        while x < width as u32 {
            let patch_w = (x + patch_size).min(width as u32) - x;
            let patch_h = (y + patch_size).min(height as u32) - y;

            if patch_w == 0 || patch_h == 0 {
                x += stride;
                continue;
            }

            let patch_result = processor.process_rgb_region(
                image,
                width as u32,
                height as u32,
                x,
                y,
                patch_w,
                patch_h,
                options,
            )?;

            let patch_raw = patch_result.as_raw();
            for py in 0..patch_h as usize {
                let src_row = &patch_raw[py * patch_w as usize..(py + 1) * patch_w as usize];
                let dst_off = (y as usize + py) * width + x as usize;
                for (px, &val) in src_row.iter().enumerate() {
                    pixel_sum[dst_off + px] += val as f32;
                    weight_map[dst_off + px] += 1.0;
                }
            }

            x += stride;
        }
        y += stride;
    }

    // Normalise accumulated sums
    let pixels: Vec<u8> = pixel_sum
        .iter()
        .zip(weight_map.iter())
        .map(|(&s, &w)| if w > 0.0 { (s / w) as u8 } else { 255 })
        .collect();
    let final_image =
        GrayImage::from_raw(width as u32, height as u32, pixels).ok_or_else(|| {
            anyhow!("process_in_patches: failed to construct output image")
        })?;
    Ok(final_image)
}

/// Applies a mask to an already binarized image.
/// Pixels in the binary data corresponding to a 255 in the mask will be set to white (255).
pub fn apply_mask_to_binary(binary_data: &mut [u8], mask: &[u8]) {
    assert_eq!(
        binary_data.len(),
        mask.len(),
        "Binary data and mask must have the same dimensions"
    );

    // This can be parallelized for large images
    binary_data
        .par_iter_mut()
        .zip(mask.par_iter())
        .for_each(|(pixel, mask_val)| {
            if *mask_val == 255 {
                *pixel = 255; // Set to white
            }
        });
}

/// Apply fixed threshold.
///
/// This is safe to parallelize by row because each worker writes to a disjoint
/// output slice. Keep smaller images on the serial path to avoid Rayon overhead.
pub(crate) fn apply_threshold(gray: &[u8], thr: u8, bin: &mut [u8], width: usize, height: usize) {
    const PARALLEL_THRESHOLD_PIXELS: usize = 256 * 256;

    if width.saturating_mul(height) < PARALLEL_THRESHOLD_PIXELS || height < 8 {
        for y in 0..height {
            let start = y * width;
            let row = &mut bin[start..start + width];
            let gray_row = &gray[start..start + width];
            for i in 0..width {
                row[i] = if gray_row[i] > thr { 255 } else { 0 };
            }
        }
        return;
    }

    bin.par_chunks_mut(width)
        .zip(gray.par_chunks(width))
        .for_each(|(row, gray_row)| {
            for i in 0..width {
                row[i] = if gray_row[i] > thr { 255 } else { 0 };
            }
        });
}

/// Invert binary image (parallel using par_chunks_mut)
pub(crate) fn invert_binary(bin: &mut [u8], _height: usize, width: usize) {
    bin.par_chunks_mut(width).for_each(|row| {
        for pixel in row.iter_mut() {
            *pixel = 255 - *pixel;
        }
    });
}

/// PBM (P4) and 1-bit packing
pub mod pbm {
    use rayon::iter::{IndexedParallelIterator, ParallelIterator};
    use rayon::prelude::ParallelSliceMut;

    /// Encode PBM P4 header + data (black=bit1 MSB-first) - parallel over rows
    pub fn make_pbm_p4(bin: &[u8], width: usize, height: usize) -> Vec<u8> {
        let header = format!("P4\n{} {}\n", width, height);
        let row_bytes = (width + 7) >> 3;
        let mut out = vec![0u8; header.len() + row_bytes * height];
        out[0..header.len()].copy_from_slice(header.as_bytes());

        // Parallel processing for PBM packing using par_chunks_mut
        out[header.len()..]
            .par_chunks_mut(row_bytes)
            .enumerate()
            .for_each(|(y, dst)| {
                let src = &bin[y * width..y * width + width];
                for x in 0..width {
                    if src[x] == 0 {
                        dst[x >> 3] |= 0x80 >> (x & 7);
                    }
                }
            });
        out
    }

    /// Pack raw 1-bit data (no header) - parallel over rows
    #[allow(dead_code)]
    pub(crate) fn pack_1bit_data(bin: &[u8], width: usize, height: usize) -> Vec<u8> {
        let row_bytes = (width + 7) >> 3;
        let mut out = vec![0u8; row_bytes * height];

        // Parallel processing using par_chunks_mut
        out.par_chunks_mut(row_bytes)
            .enumerate()
            .for_each(|(y, dst)| {
                let src = &bin[y * width..y * width + width];
                for x in 0..width {
                    if src[x] == 0 {
                        dst[x >> 3] |= 0x80 >> (x & 7);
                    }
                }
            });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_threshold, dilate_square_reflect, improved_binarize, invert_binary};
    use crate::types::BinarizationOptions;

    #[test]
    fn apply_threshold_sets_expected_binary_values() {
        let gray = vec![0u8, 100, 200, 255, 50, 150, 151, 149];
        let mut bin = vec![0u8; gray.len()];

        apply_threshold(&gray, 150, &mut bin, 4, 2);

        assert_eq!(bin, vec![0, 0, 255, 255, 0, 0, 255, 0,]);
    }

    #[test]
    fn invert_binary_flips_all_pixels_by_row() {
        let mut bin = vec![0u8, 255, 128, 1, 254, 42];
        invert_binary(&mut bin, 2, 3);
        assert_eq!(bin, vec![255, 0, 127, 254, 1, 213]);
    }

    #[test]
    fn pbm_pack_handles_odd_width_msb_first() {
        let bin = vec![0u8, 255, 0, 255, 0, 255, 0, 255, 0, 255];
        let packed = super::pbm::pack_1bit_data(&bin, 5, 2);
        assert_eq!(packed, vec![0b1010_1000, 0b0101_0000]);
    }

    #[test]
    fn improved_binarize_preserves_dimensions_for_odd_width() {
        let width = 37usize;
        let height = 29usize;
        let gray: Vec<u8> = (0..width * height)
            .map(|i| ((i * 17 + i / 3) % 256) as u8)
            .collect();
        let out = improved_binarize(&gray, width, height, &BinarizationOptions::default());
        assert_eq!(out.len(), width * height);
        assert!(out.iter().all(|&v| v == 0 || v == 255));
    }

    #[test]
    fn improved_binarize_does_not_apply_output_inversion() {
        let width = 37usize;
        let height = 31usize;
        let gray: Vec<u8> = (0..width * height)
            .map(|i| ((i * 23 + i / 5) % 256) as u8)
            .collect();
        let normal = BinarizationOptions::default();
        let inverted = BinarizationOptions {
            invert: true,
            ..BinarizationOptions::default()
        };

        assert_eq!(
            improved_binarize(&gray, width, height, &normal),
            improved_binarize(&gray, width, height, &inverted)
        );
    }

    // --- GPU algorithm parity tests ---
    // Gated by LEGE_RUN_GPU_TESTS=1 to avoid blocking CI on headless machines.

    fn make_test_image(w: usize, h: usize) -> Vec<u8> {
        (0..w * h)
            .map(|i| {
                let x = i % w;
                let y = i / w;
                // Varied gradient with noise to exercise all regions of the algorithm.
                ((x * 3 + y * 5 + (x ^ y) * 7) % 256) as u8
            })
            .collect()
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn gpu_bg_parity_debug3() {
        if std::env::var("LEGE_RUN_GPU_TESTS").ok().as_deref() != Some("1") {
            return;
        }
        use lege_gpu::binarization::{
            AdaptiveBinarizeGpuConstants, BinarizationMode, BinarizationParams, wgpu::WgpuBinarizer,
        };

        let w = 128usize;
        let h = 96usize;
        let gray = make_test_image(w, h);

        let bg_window = super::odd_background_window(h);
        let mut cpu_bg = vec![0u8; w * h];
        dilate_square_reflect(&gray, w, h, bg_window, &mut cpu_bg);

        let constants = super::compute_adaptive_gpu_constants(&gray, w, h);
        let params = BinarizationParams {
            width: w as u32,
            height: h as u32,
            mode: BinarizationMode::Adaptive,
            invert_output: false,
            k_factor: 0.2,
            fixed_threshold: 128,
            adaptive: AdaptiveBinarizeGpuConstants {
                sauvola_window: constants.sauvola_window,
                bg_window: constants.bg_window,
                percentile_c: constants.percentile_c,
                otsu_threshold: constants.otsu_threshold,
            },
            debug_mode: 3,
        };

        let mut binarizer = WgpuBinarizer::new().expect("WgpuBinarizer::new");
        let gpu_bg = binarizer
            .binarize_gray_raw(&gray, &params)
            .expect("GPU debug_mode=3 (bg)");

        assert_eq!(gpu_bg.len(), w * h, "output length mismatch");
        let mismatches: usize = cpu_bg
            .iter()
            .zip(gpu_bg.iter())
            .filter(|(c, g)| c != g)
            .count();
        assert_eq!(
            mismatches,
            0,
            "bg parity: {} of {} pixels differ (CPU bg vs GPU debug_mode=3)",
            mismatches,
            w * h
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn gpu_local_mean_parity_debug5() {
        if std::env::var("LEGE_RUN_GPU_TESTS").ok().as_deref() != Some("1") {
            return;
        }
        use lege_gpu::binarization::{
            AdaptiveBinarizeGpuConstants, BinarizationMode, BinarizationParams, wgpu::WgpuBinarizer,
        };

        let w = 128usize;
        let h = 96usize;
        let gray = make_test_image(w, h);
        let constants = super::compute_adaptive_gpu_constants(&gray, w, h);
        let win = constants.sauvola_window as usize;
        let r = (win / 2) as isize;

        // CPU: sliding-window mean using the same reflect-101 border handling as the GPU.
        let cpu_mean: Vec<u8> = (0..w * h)
            .map(|i| {
                let px = (i % w) as isize;
                let py = (i / w) as isize;
                let mut sum = 0u32;
                for dy in -r..=r {
                    for dx in -r..=r {
                        let rx = super::reflect_101(px + dx, w as isize) as usize;
                        let ry = super::reflect_101(py + dy, h as isize) as usize;
                        sum += gray[ry * w + rx] as u32;
                    }
                }
                let area = (win * win) as u32;
                (sum / area).min(255) as u8
            })
            .collect();

        let params = BinarizationParams {
            width: w as u32,
            height: h as u32,
            mode: BinarizationMode::Adaptive,
            invert_output: false,
            k_factor: 0.2,
            fixed_threshold: 128,
            adaptive: AdaptiveBinarizeGpuConstants {
                sauvola_window: constants.sauvola_window,
                bg_window: constants.bg_window,
                percentile_c: constants.percentile_c,
                otsu_threshold: constants.otsu_threshold,
            },
            debug_mode: 5,
        };

        let mut binarizer = WgpuBinarizer::new().expect("WgpuBinarizer::new");
        let gpu_mean = binarizer
            .binarize_gray_raw(&gray, &params)
            .expect("GPU debug_mode=5 (local mean)");

        assert_eq!(gpu_mean.len(), w * h);
        let mut max_diff = 0u8;
        let mut bad = 0usize;
        for (i, (&c, &g)) in cpu_mean.iter().zip(gpu_mean.iter()).enumerate() {
            let diff = c.abs_diff(g);
            if diff > max_diff {
                max_diff = diff;
            }
            if diff > 1 {
                bad += 1;
                if bad <= 5 {
                    let x = i % w;
                    let y = i / w;
                    eprintln!("mean mismatch at ({x},{y}): cpu={c} gpu={g}");
                }
            }
        }
        assert_eq!(
            bad, 0,
            "local mean parity: {bad} pixels differ by >1 (max_diff={max_diff}); \
             integral image or padding bug"
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn gpu_fused_parity_debug0() {
        if std::env::var("LEGE_RUN_GPU_TESTS").ok().as_deref() != Some("1") {
            return;
        }
        use lege_gpu::binarization::{
            AdaptiveBinarizeGpuConstants, BinarizationMode, BinarizationParams, wgpu::WgpuBinarizer,
        };

        let w = 128usize;
        let h = 96usize;
        let gray = make_test_image(w, h);
        let constants = super::compute_adaptive_gpu_constants(&gray, w, h);
        let cpu_fused = improved_binarize(&gray, w, h, &BinarizationOptions::default());

        let params = BinarizationParams {
            width: w as u32,
            height: h as u32,
            mode: BinarizationMode::Adaptive,
            invert_output: false,
            k_factor: 0.2,
            fixed_threshold: 128,
            adaptive: AdaptiveBinarizeGpuConstants {
                sauvola_window: constants.sauvola_window,
                bg_window: constants.bg_window,
                percentile_c: constants.percentile_c,
                otsu_threshold: constants.otsu_threshold,
            },
            debug_mode: 0,
        };

        let mut binarizer = WgpuBinarizer::new().expect("WgpuBinarizer::new");
        let gpu_fused = binarizer
            .binarize_gray_raw(&gray, &params)
            .expect("GPU debug_mode=0 (fused)");

        assert_eq!(gpu_fused.len(), w * h);
        let mismatches: usize = cpu_fused
            .iter()
            .zip(gpu_fused.iter())
            .filter(|(c, g)| c != g)
            .count();
        let threshold = (w * h) / 1000; // 0.1%
        assert!(
            mismatches <= threshold,
            "fused parity: {mismatches} of {} pixels differ (>{threshold} allowed); \
             Sauvola or Otsu threshold divergence",
            w * h
        );
    }
}
