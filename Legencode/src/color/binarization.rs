//! Pure-CPU document binarization module in Rust
//! Replicates OpenCV Sauvola-based precision binarization and PBM encoding

use crate::types::BinarizationOptions;
use anyhow::{anyhow, Result};
use image::{GrayImage, Luma};
use ndarray::Array4;
use ort::execution_providers::DirectMLExecutionProvider;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Value;
use rayon::prelude::*;
use rayon::slice::ParallelSliceMut;
use std::cmp::{max, min};

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

        let builder =
            Session::builder()?.with_optimization_level(GraphOptimizationLevel::Level3)?;

        // Try different execution providers with fallback to CPU
        let session = 'provider_search: loop {
            #[cfg(target_os = "macos")]
            if let Ok(coreml_builder) = builder
                .clone()
                .with_execution_providers([CoreMLExecutionProvider::default().build()])
            {
                if let Ok(session) = coreml_builder.commit_from_file(&model_path) {
                    #[cfg(feature = "debug-logging")]
                    crate::streamline::log_debug_message(
                        "Heavy-duty Sauvola: Using CoreML execution provider.",
                    );
                    break 'provider_search session;
                }
            }

            #[cfg(target_os = "windows")]
            if let Ok(directml_builder) = builder
                .clone()
                .with_execution_providers([DirectMLExecutionProvider::default().build()])
            {
                if let Ok(session) = directml_builder.commit_from_file(&model_path) {
                    #[cfg(feature = "debug-logging")]
                    crate::streamline::log_debug_message(
                        "Heavy-duty Sauvola: Using DirectML execution provider",
                    );
                    break 'provider_search session;
                }
            }

            // Fallback to CPU
            #[cfg(feature = "debug-logging")]
            crate::streamline::log_debug_message(
                "Heavy-duty Sauvola: Using CPU execution provider",
            );
            break 'provider_search builder.commit_from_file(&model_path)?;
        };

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

        let inputs = ort::inputs!["img01_inp" => Value::from_array(img_array)?];
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
        let img_value = Value::from_array(img_array)?;
        let inputs = ort::inputs!["img01_inp" => img_value];
        let outputs = self.session.run(inputs)?;

        // Extract output
        let output = outputs.values().next().unwrap();
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
        width, height, image.len(), options.use_heavy_duty, options.use_fixed_threshold, options.invert_input, options.invert
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
        match HeavySauvolaProcessor::new() {
            Ok(mut processor) => {
                match apply_heavy_duty_binarization_raw(
                    &mut processor,
                    image,
                    width,
                    height,
                    options,
                ) {
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
                        // Fall through to light binarization
                    }
                }
            }
            Err(_e) => {
                #[cfg(feature = "debug-logging")]
                crate::streamline::log_debug_message(&format!(
                    "Failed to initialize heavy-duty processor: {}",
                    _e
                ));
                // Fall through to light binarization
            }
        }
    }

    // Light binarization: improved Sauvola + adaptive Otsu fusion
    #[cfg(feature = "debug-logging")]
    crate::streamline::log_debug_message(&format!(
        "[BinarizationRaw] Using light binarization for {}x{} image",
        width, height
    ));

    let linear_rgb = crate::color::linearize::linearize_rgb_bytes_to_f32(image);
    let mut gray = vec![0; width * height];
    crate::color::linearize::linearized_rgb_to_grayscale(&linear_rgb, &mut gray);

    // Handle input inversion
    if options.invert_input {
        #[cfg(feature = "debug-logging")]
        crate::streamline::log_debug_message(&format!(
            "[BinarizationRaw] Applying input inversion to {}x{} grayscale",
            width, height
        ));
        gray.par_iter_mut().for_each(|p| *p = 255 - *p);
    }

    if options.use_fixed_threshold {
        let mut result = vec![0u8; width * height];
        apply_threshold(&gray, options.fixed_threshold, &mut result, width, height);
        if options.invert {
            result.par_iter_mut().for_each(|p| *p = 255 - *p);
        }
        return result;
    }

    // Always use adaptive improved binarization
    let mut result = improved_binarize(&gray, width, height, options);

    // Handle output inversion
    if options.invert {
        result.par_iter_mut().for_each(|p| *p = 255 - *p);
    }

    result
}

/// Improved binarization matching the Python script: adaptive Sauvola + background-normalized Otsu + AND fusion
fn improved_binarize(
    gray: &[u8],
    width: usize,
    height: usize,
    opt: &BinarizationOptions,
) -> Vec<u8> {
    let n = width * height;
    assert!(gray.len() == n);

    // Adaptive window for Sauvola
    let win = sauvola_window_for(height, width);

    // Compute Sauvola binarization
    let mut sauvola_bin = vec![0u8; n];
    sauvola_via_integral(
        gray,
        width,
        height,
        win,
        opt.k_factor as f32,
        &mut sauvola_bin,
    );

    // Background estimate
    let mut s = max(3, min(height / 200, 15)); // Clamp to reasonable max for speed
    if s % 2 == 0 {
        s += 1; // Ensure odd
    }
    let mut bg = vec![0u8; n];
    dilate_square_reflect(gray, width, height, s, &mut bg);

    // Percentile C (30th)
    let hist = hist256(gray);
    let c = percentile_from_hist(&hist, 0.30, n);

    // Normalize
    let mut normalized = vec![0u8; n];
    normalize_by_bg(gray, &bg, c, &mut normalized);

    // Otsu on normalized
    let t_otsu = otsu_threshold_u8(&normalized);
    let mut otsu_bin = vec![0u8; n];
    otsu_bin
        .par_iter_mut()
        .zip(normalized.par_iter())
        .for_each(|(o, &v)| *o = if v > t_otsu { 255 } else { 0 });

    // Fuse with AND
    let mut fused = vec![0u8; n];
    bitand_binaries(&sauvola_bin, &otsu_bin, &mut fused);

    // Handle output inversion
    if opt.invert {
        fused.par_iter_mut().for_each(|p| *p = 255 - *p);
    }

    fused
}

/// Adaptive window size for Sauvola
#[inline]
fn sauvola_window_for(h: usize, w: usize) -> usize {
    let mut win = (h.min(w) / 40).max(31).min(101);
    if win % 2 == 0 {
        win += 1;
    }
    win
}

/// Sauvola via integral images
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

    // Precompute reflection tables
    let reflect_x: Vec<usize> = (-r..(width as isize) + r)
        .map(|i| reflect_101(i, width as isize) as usize)
        .collect();
    let reflect_y: Vec<usize> = (-r..(height as isize) + r)
        .map(|i| reflect_101(i, height as isize) as usize)
        .collect();

    // Padded image
    let mut padded = vec![0u8; pad_w * pad_h];
    padded
        .par_chunks_mut(pad_w)
        .enumerate()
        .for_each(|(py, row)| {
            let sy = reflect_y[py] * width;
            for px in 0..pad_w {
                let sx = reflect_x[px];
                row[px] = gray[sy + sx];
            }
        });

    // Integral images
    let iw = pad_w + 1;
    let ih = pad_h + 1;
    let mut integ = vec![0u64; iw * ih];
    let mut integ_sq = vec![0u64; iw * ih];

    // Row prefix sums (parallelized per row)
    integ
        .par_chunks_mut(iw)
        .zip(integ_sq.par_chunks_mut(iw))
        .enumerate()
        .skip(1) // Skip row 0 (remains zeros)
        .for_each(|(y, (integ_row, integ_sq_row))| {
            let src = &padded[(y - 1) * pad_w..(y - 1) * pad_w + pad_w];
            let mut sum = 0u64;
            let mut sum_sq = 0u64;
            for x in 1..=pad_w {
                let v = src[x - 1] as u64;
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
    // combine
    for i in 0..n {
        let a = if i + r < n { h[i + r] } else { h[n - 1] };
        let b = if i >= r { g[i - r] } else { g[0] };
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

/// Normalize by background
fn normalize_by_bg(gray: &[u8], bg: &[u8], c: u8, out: &mut [u8]) {
    let c = c as f32;
    out.par_iter_mut()
        .zip(gray.par_iter())
        .zip(bg.par_iter())
        .for_each(|((o, &g), &b)| {
            let val = c / (b as f32 + 1e-6) * (g as f32);
            *o = val.clamp(0.0, 255.0) as u8;
        });
}

/// Otsu threshold
fn otsu_threshold_u8(data: &[u8]) -> u8 {
    let h = hist256(data);
    let total = data.len() as f64;
    let mut sum_all = 0.0;
    for i in 0..256 {
        sum_all += (i as f64) * (h[i] as f64);
    }

    let mut sum_b = 0.0;
    let mut w_b = 0.0;
    let mut max_var = -1.0;
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

/// Bitwise AND for binaries (0/255)
fn bitand_binaries(a: &[u8], b: &[u8], out: &mut [u8]) {
    out.par_iter_mut()
        .zip(a.par_iter())
        .zip(b.par_iter())
        .for_each(|((o, &x), &y)| {
            *o = if x != 0 && y != 0 { 255 } else { 0 };
        });
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

    // Convert to binary data and apply inversion if needed
    let mut bin_data = result_img.into_raw();
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

    let mut final_image = GrayImage::new(width as u32, height as u32);
    let mut weight_map = vec![0.0f32; width * height];

    let mut y = 0;
    while y < height as u32 {
        let mut x = 0;
        while x < width as u32 {
            let patch_w = (x + patch_size).min(width as u32) - x;
            let patch_h = (y + patch_size).min(height as u32) - y;

            if patch_w == 0 || patch_h == 0 {
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

            // Blend the patch into the final image
            for py in 0..patch_h {
                for px in 0..patch_w {
                    let final_x = x + px;
                    let final_y = y + py;
                    let weight = 1.0; // Simple average for now, can be improved with blending

                    let old_pixel = final_image.get_pixel(final_x, final_y).0[0] as f32;
                    let new_pixel = patch_result.get_pixel(px, py).0[0] as f32;
                    let old_weight = weight_map[(final_y as usize * width) + final_x as usize];

                    let blended_pixel =
                        ((old_pixel * old_weight) + (new_pixel * weight)) / (old_weight + weight);
                    final_image.put_pixel(final_x, final_y, Luma([blended_pixel as u8]));
                    weight_map[(final_y as usize * width) + final_x as usize] += weight;
                }
            }

            x += stride;
        }
        y += stride;
    }

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

/// Apply fixed threshold (sequential processing)
pub(crate) fn apply_threshold(gray: &[u8], thr: u8, bin: &mut [u8], width: usize, height: usize) {
    for y in 0..height {
        let start = y * width;
        let row = &mut bin[start..start + width];
        let gray_row = &gray[start..start + width];
        for i in 0..width {
            row[i] = if gray_row[i] > thr { 255 } else { 0 };
        }
    }
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
