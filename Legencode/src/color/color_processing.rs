use anyhow::{Result, anyhow};
use ndarray::Array4;
#[cfg(target_os = "macos")]
use ort::execution_providers::CoreMLExecutionProvider;
#[cfg(target_os = "windows")]
use ort::execution_providers::DirectMLExecutionProvider;
use ort::session::Session;
use ort::value::Value;
use rayon::iter::ParallelIterator;
use rayon::prelude::*;
use rayon::slice::ParallelSlice;
use std::sync::OnceLock;

// Global guardrails to ensure region operations remain safe and bounded.
const MAX_OVERLAY_SIDE: u32 = 8192; // Prevent pathological overlays (encoder limits, memory safety)
const MAX_OVERLAY_PIXELS: u64 = 8192u64 * 8192u64; // 67Mpx upper cap for regions

/// Grayscale values at or above this floor are treated as pure paper white and
/// forced to 255 before dithering.  Prevents sparse dots in near-white regions
/// (paper texture, scan noise) that create a visible secondary pattern.
const PAPER_WHITE_FLOOR: u8 = 245;

/// How to reduce detected **image** regions to 1-bit before merging into the page mask.
/// Body text / full-page binarization is separate (Sauvola etc.); this applies only where
/// the pipeline passes layout-detected image labels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageRegionDitherMode {
    /// Keep color (crop only); region is masked or overlaid without region dithering.
    #[default]
    None,
    /// `--dither`: **JBIG2 / DjVu** → Stucki error diffusion on image regions.
    /// **CCITT4** → Bayer 8×8 ordered dither on image regions (fax path never uses Stucki here).
    Stucki,
    /// `--cdot`, **CCITT4 only** → clustered 4×4 ordered dither on image regions (invalid with JBIG2/DjVu).
    Ccitt4ClusteredDot4x4,
    /// `--halftone`, **JBIG2 only** → grayscale crops for `jbig2halftone.rs` (invalid with CCITT4/DjVu).
    Halftone,
}

/// Region bounds structure for zero-copy processing
#[derive(Debug, Clone, Copy)]
struct RegionBounds {
    x_start: u32,
    y_start: u32,
    x_end: u32,
    y_end: u32,
}

/// Optimized 8x8 Bayer matrix using integer arithmetic for faster comparison.
/// Provides 65 tonal levels instead of 17 (4x4) for finer gradients.
static BAYER_8X8: [[u8; 8]; 8] = [
    [0, 32, 8, 40, 2, 34, 10, 42],
    [48, 16, 56, 24, 50, 18, 58, 26],
    [12, 44, 4, 36, 14, 46, 6, 38],
    [60, 28, 52, 20, 62, 30, 54, 22],
    [3, 35, 11, 43, 1, 33, 9, 41],
    [51, 19, 59, 27, 49, 17, 57, 25],
    [15, 47, 7, 39, 13, 45, 5, 37],
    [63, 31, 55, 23, 61, 29, 53, 21],
];

/// Bayer 8×8 ordered dithering for image regions (CCITT4 default).
#[inline(always)]
fn process_bayer_dithering(
    original_rgb: &[u8],
    page_width: u32,
    region_bounds: RegionBounds,
    region_width: u32,
    region_height: u32,
) -> Result<(Vec<u8>, u32, u32)> {
    let grayscale_region = rgb_to_grayscale_simd_direct(
        original_rgb,
        page_width,
        region_bounds,
        region_width,
        region_height,
    );

    let dithered_1bit_data = dither_bayer8x8(&grayscale_region, region_width, region_height);

    let pixel_count = dithered_1bit_data.len();
    let mut output = Vec::with_capacity(pixel_count * 3);

    dithered_1bit_data
        .par_chunks(512)
        .map(|chunk: &[u8]| {
            let mut local_output = Vec::with_capacity(chunk.len() * 3);
            for &pixel in chunk {
                local_output.extend_from_slice(&[pixel, pixel, pixel]);
            }
            local_output
        })
        .collect::<Vec<_>>()
        .into_iter()
        .for_each(|chunk| output.extend(chunk));

    Ok((output, region_width, region_height))
}

/// Apply 8×8 Bayer ordered dithering to grayscale data.
fn dither_bayer8x8(grayscale_data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let expected = w * h;
    if expected == 0 || grayscale_data.len() < expected {
        return vec![255; expected];
    }
    let mut out = vec![255u8; expected];
    for y in 0..h {
        for x in 0..w {
            let idx = y * w + x;
            let g = grayscale_data[idx];
            if g >= PAPER_WHITE_FLOOR {
                continue; // already 255 in output
            }
            let threshold = BAYER_8X8[y & 7][x & 7] * 4;
            out[idx] = if g > threshold { 255 } else { 0 };
        }
    }
    out
}

/// Rank order for a clustered-dot screen tile (same construction as ccitt4-dither-tester).
fn clustered_dot_matrix(size: usize, rect_bias_x: i32, rect_bias_y: i32) -> Vec<u16> {
    let mut coords: Vec<(usize, usize, i64, i32, i32, i32)> = Vec::with_capacity(size * size);
    let center = size as i32;
    for y in 0..size {
        for x in 0..size {
            let px = (2 * x + 1) as i32;
            let py = (2 * y + 1) as i32;
            let dx = px - center;
            let dy = py - center;
            let primary =
                (dx as i64 * dx as i64) * rect_bias_x as i64 + (dy as i64 * dy as i64) * rect_bias_y as i64;
            let vertical_bias = dx.abs();
            let chebyshev = dx.abs().max(dy.abs());
            let stable = (y * size + x) as i32;
            coords.push((x, y, primary, vertical_bias, chebyshev, stable));
        }
    }
    coords.sort_by_key(|&(_, _, p, vb, ch, st)| (p, vb, ch, st));
    let mut tile = vec![0u16; size * size];
    for (rank, &(x, y, _, _, _, _)) in coords.iter().enumerate() {
        tile[y * size + x] = rank as u16;
    }
    tile
}

#[inline(always)]
fn threshold_from_cluster_rank(rank: u16, tile_pixels: usize) -> u8 {
    if tile_pixels <= 1 {
        return 127;
    }
    ((rank as u32 * 255) / (tile_pixels as u32 - 1)) as u8
}

fn clustered_4x4_screen() -> &'static [u16] {
    static SCREEN: OnceLock<Vec<u16>> = OnceLock::new();
    SCREEN.get_or_init(|| clustered_dot_matrix(4, 1, 1))
}

/// Clustered 4×4 ordered dither (CCITT4 dev path via `--cdot`).
fn dither_clustered_4x4(grayscale_data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let expected = w * h;
    if expected == 0 || grayscale_data.len() < expected {
        return vec![255; expected];
    }
    let screen = clustered_4x4_screen();
    const TILE: usize = 4;
    const TILE_PIXELS: usize = TILE * TILE;
    let mut out = vec![255u8; expected];
    for y in 0..h {
        for x in 0..w {
            let idx = y * w + x;
            let g = grayscale_data[idx];
            if g >= PAPER_WHITE_FLOOR {
                continue;
            }
            let rank = screen[(y & (TILE - 1)) * TILE + (x & (TILE - 1))];
            let thr = threshold_from_cluster_rank(rank, TILE_PIXELS);
            out[idx] = if g > thr { 255 } else { 0 };
        }
    }
    out
}

/// Clustered 4×4 ordered dither for image regions (CCITT4 `--cdot`).
#[inline(always)]
fn process_clustered_dot4_dithering(
    original_rgb: &[u8],
    page_width: u32,
    region_bounds: RegionBounds,
    region_width: u32,
    region_height: u32,
) -> Result<(Vec<u8>, u32, u32)> {
    let grayscale_region = rgb_to_grayscale_simd_direct(
        original_rgb,
        page_width,
        region_bounds,
        region_width,
        region_height,
    );

    let dithered_1bit_data = dither_clustered_4x4(&grayscale_region, region_width, region_height);

    let pixel_count = dithered_1bit_data.len();
    let mut output = Vec::with_capacity(pixel_count * 3);

    dithered_1bit_data
        .par_chunks(512)
        .map(|chunk: &[u8]| {
            let mut local_output = Vec::with_capacity(chunk.len() * 3);
            for &pixel in chunk {
                local_output.extend_from_slice(&[pixel, pixel, pixel]);
            }
            local_output
        })
        .collect::<Vec<_>>()
        .into_iter()
        .for_each(|chunk| output.extend(chunk));

    Ok((output, region_width, region_height))
}

/// The primary function to process a detected image region.
///
/// Zero-copy optimized version that processes regions in-place without intermediate allocations.
///
/// # Arguments
/// * `original_rgb`: A slice containing the RGB pixel data for the *entire page*.
/// * `page_width`: The width of the entire page in pixels.
/// * `region_bbox`: The bounding box `[x1, y1, x2, y2]` of the image region to process.
/// * `mode` + `text_format` are validated together: JBIG2 halftone and Stucki never apply to CCITT4;
///   Bayer / clustered-dot never apply to JBIG2; [`ImageRegionDitherMode::Halftone`] requires `jbig2`,
///   [`ImageRegionDitherMode::Ccitt4ClusteredDot4x4`] requires `ccitt4`.
/// * `use_heavy_binarization`: If `true`, uses the ONNX-based heavy binarization for better quality.
///
/// # Returns
/// `(Vec<u8>, u32, u32)` — pixel data, region width, region height.
#[inline(always)]
pub fn process_image_region(
    original_rgb: &[u8],
    page_width: u32,
    page_height: u32,
    region_bbox: [f32; 4],
    mode: ImageRegionDitherMode,
    text_format: &str,
    use_heavy_binarization: bool,
) -> Result<(Vec<u8>, u32, u32)> {
    // 1. Validate and calculate region bounds
    let (region_bounds, region_width, region_height) =
        validate_region_bounds(page_width, page_height, region_bbox)?;

    if mode == ImageRegionDitherMode::None {
        // Zero-copy crop for non-dithered regions
        return crop_rgb_region_zero_copy(
            original_rgb,
            page_width,
            region_bounds,
            region_width,
            region_height,
        );
    }

    // Use heavy binarization if enabled and available
    if use_heavy_binarization {
        if let Ok(mut processor) = HeavyBinarizationProcessor::new() {
            match processor.process_region(
                original_rgb,
                page_width,
                page_height,
                region_bounds,
                region_width,
                region_height,
            ) {
                Ok(result) => return Ok((result, region_width, region_height)),
                Err(_e) => {
                    #[cfg(feature = "debug-logging")]
                    crate::streamline::log_debug_message(&format!(
                        "Heavy binarization failed, falling back to standard dithering: {}",
                        _e
                    ));
                }
            }
        }
    }

    #[cfg(feature = "debug-logging")]
    crate::streamline::log_debug_message(&format!(
        "process_image_region: mode={:?} fmt={} region={}x{} bounds=({},{})→({},{})",
        mode, text_format, region_width, region_height,
        region_bounds.x_start, region_bounds.y_start,
        region_bounds.x_end, region_bounds.y_end,
    ));

    match mode {
        ImageRegionDitherMode::Halftone => {
            if text_format != "jbig2" {
                return Err(anyhow!(
                    "halftone image regions require --text-format jbig2 (got {}). \
                     CCITT4 uses ordered dither (Bayer or --cdot); DjVu uses Stucki with --dither.",
                    text_format
                ));
            }
            // JBIG2 only: grayscale crop for jbig2halftone.rs (not used on CCITT4 / DjVu).
            #[cfg(feature = "debug-logging")]
            crate::streamline::log_debug_message("  → dispatching to grayscale crop (halftone → jbig2halftone.rs encodes later)");
            let grayscale = rgb_to_grayscale_simd_direct(
                original_rgb, page_width, region_bounds, region_width, region_height,
            );
            let mut output = Vec::with_capacity(grayscale.len() * 3);
            for &g in &grayscale {
                output.extend_from_slice(&[g, g, g]);
            }
            Ok((output, region_width, region_height))
        }
        ImageRegionDitherMode::Ccitt4ClusteredDot4x4 => {
            if text_format != "ccitt4" {
                return Err(anyhow!(
                    "--cdot (clustered 4×4) requires --text-format ccitt4 (got {}). \
                     JBIG2/DjVu image regions use Stucki with --dither.",
                    text_format
                ));
            }
            #[cfg(feature = "debug-logging")]
            crate::streamline::log_debug_message("  → dispatching to clustered 4×4 ordered dither (CCITT4, --cdot)");
            process_clustered_dot4_dithering(
                original_rgb,
                page_width,
                region_bounds,
                region_width,
                region_height,
            )
        }
        ImageRegionDitherMode::Stucki => match text_format {
            "ccitt4" => {
                // CCITT4: Bayer only (never Stucki on fax image regions).
                #[cfg(feature = "debug-logging")]
                crate::streamline::log_debug_message("  → dispatching to Bayer 8×8 ordered dither (CCITT4)");
                process_bayer_dithering(
                    original_rgb,
                    page_width,
                    region_bounds,
                    region_width,
                    region_height,
                )
            }
            _ => {
                // JBIG2 / DjVu / … : Stucki only (never Bayer or clustered dot).
                #[cfg(feature = "debug-logging")]
                crate::streamline::log_debug_message("  → dispatching to Stucki error diffusion (JBIG2/DjVu)");
                process_stucki_dither_region(
                    original_rgb,
                    page_width,
                    region_bounds,
                    region_width,
                    region_height,
                )
            }
        },
        ImageRegionDitherMode::None => unreachable!("None mode returns before dither branch"),
    }
}

/// Traditional dithering using custom phase-locked blue-noise method - used for CCITT4
/// This custom mode produces better results for CCITT4 encoding with better run-length
/// characteristics, allowing more efficient compression.
#[inline(always)]
fn process_traditional_dithering(
    original_rgb: &[u8],
    page_width: u32,
    region_bounds: RegionBounds,
    region_width: u32,
    region_height: u32,
) -> Result<(Vec<u8>, u32, u32)> {
    // Convert to grayscale
    let grayscale_region = rgb_to_grayscale_simd_direct(
        original_rgb,
        page_width,
        region_bounds,
        region_width,
        region_height,
    );

    // Phase-locked blue-noise dithering (CCITT4 image regions).
    let dithered_1bit_data =
        dither_phase_locked_blue_noise_rlg(&grayscale_region, region_width, region_height);

    // Convert back to RGB format for compatibility
    let pixel_count = dithered_1bit_data.len();
    let mut output = Vec::with_capacity(pixel_count * 3);

    // Process in larger chunks for better memory bandwidth utilization
    dithered_1bit_data
        .par_chunks(512) // 512 pixels = 1.5KB chunks
        .map(|chunk| {
            let mut local_output = Vec::with_capacity(chunk.len() * 3);
            for &pixel in chunk {
                local_output.extend_from_slice(&[pixel, pixel, pixel]);
            }
            local_output
        })
        .collect::<Vec<_>>()
        .into_iter()
        .for_each(|chunk| output.extend(chunk));

    Ok((output, region_width, region_height))
}

/// Stucki dither for image regions when text format is not CCITT4 (JBIG2, DjVu, etc.).
/// Output is 0/255 packed as RGB triplets for the existing merge pipeline.
#[inline(always)]
fn process_stucki_dither_region(
    original_rgb: &[u8],
    page_width: u32,
    region_bounds: RegionBounds,
    region_width: u32,
    region_height: u32,
) -> Result<(Vec<u8>, u32, u32)> {
    // Convert to grayscale
    let grayscale_region = rgb_to_grayscale_simd_direct(
        original_rgb,
        page_width,
        region_bounds,
        region_width,
        region_height,
    );

    let dithered_1bit_data =
        dither_stucki_error_diffusion(&grayscale_region, region_width, region_height);

    // Convert back to RGB format for compatibility with the existing pipeline
    let pixel_count = dithered_1bit_data.len();
    let mut output = Vec::with_capacity(pixel_count * 3);

    // Process in larger chunks for better memory bandwidth utilization
    dithered_1bit_data
        .par_chunks(512) // 512 pixels = 1.5KB chunks
        .map(|chunk| {
            let mut local_output = Vec::with_capacity(chunk.len() * 3);
            for &pixel in chunk {
                local_output.extend_from_slice(&[pixel, pixel, pixel]);
            }
            local_output
        })
        .collect::<Vec<_>>()
        .into_iter()
        .for_each(|chunk| output.extend(chunk));

    Ok((output, region_width, region_height))
}

/// Validates region bounds and returns sanitized coordinates
#[inline(always)]
fn validate_region_bounds(
    page_width: u32,
    page_height: u32,
    bbox: [f32; 4],
) -> Result<(RegionBounds, u32, u32)> {
    let [x1, y1, x2, y2] = bbox;

    #[cfg(feature = "debug-logging")]
    {
        crate::streamline::log_debug_message(&format!(
            "validate_region_bounds: page={}x{}, input_bbox=[{:.1}, {:.1}, {:.1}, {:.1}]",
            page_width, page_height, x1, y1, x2, y2
        ));
    }

    // Sanitize and clamp coordinates to be safely within image bounds.
    let x_start = (x1.round() as u32).min(page_width.saturating_sub(1));
    let y_start = (y1.round() as u32).min(page_height.saturating_sub(1));
    let x_end = (x2.round() as u32).min(page_width);
    let y_end = (y2.round() as u32).min(page_height);

    #[cfg(feature = "debug-logging")]
    {
        crate::streamline::log_debug_message(&format!(
            "validate_region_bounds: clamped to ({},{} to {},{})",
            x_start, y_start, x_end, y_end
        ));
    }

    if x_start >= x_end || y_start >= y_end {
        return Err(anyhow!(
            "Invalid or zero-area bounding box after clamping: {:?}",
            bbox
        ));
    }

    let region_width = x_end - x_start;
    let region_height = y_end - y_start;

    // Hard guardrails: cap side lengths and total pixels
    if region_width == 0 || region_height == 0 {
        return Err(anyhow!("Zero-area region after clamping"));
    }
    if region_width > MAX_OVERLAY_SIDE || region_height > MAX_OVERLAY_SIDE {
        return Err(anyhow!(
            "Region exceeds max side limit ({} or {} > {})",
            region_width,
            region_height,
            MAX_OVERLAY_SIDE
        ));
    }
    let pixels = region_width as u64 * region_height as u64;
    if pixels > MAX_OVERLAY_PIXELS {
        return Err(anyhow!(
            "Region too large ({} px > {} px)",
            pixels,
            MAX_OVERLAY_PIXELS
        ));
    }
    let bounds = RegionBounds {
        x_start,
        y_start,
        x_end,
        y_end,
    };

    #[cfg(feature = "debug-logging")]
    {
        crate::streamline::log_debug_message(&format!(
            "validate_region_bounds: final dimensions {}x{}",
            region_width, region_height
        ));
    }

    Ok((bounds, region_width, region_height))
}

/// Zero-copy crop that only allocates the final output buffer
#[inline(always)]
fn crop_rgb_region_zero_copy(
    source_rgb: &[u8],
    source_width: u32,
    bounds: RegionBounds,
    region_width: u32,
    region_height: u32,
) -> Result<(Vec<u8>, u32, u32)> {
    let mut region_pixels = Vec::with_capacity((region_width * region_height * 3) as usize);
    let source_width = source_width as usize;

    // Sanity: ensure the requested region rows are within the source buffer.
    // `y_end` is exclusive (`for y in y_start..y_end`), so the last row index is `y_end - 1`.
    // Using `y_end` here would address one row past the crop (and fail for full-page crops).
    let last_row = (bounds.y_end - 1) as usize;
    let last_row_end = (last_row * source_width + bounds.x_end as usize) * 3;
    if last_row_end > source_rgb.len() {
        return Err(anyhow!(
            "Crop bounds exceed source buffer (end={} > len={})",
            last_row_end,
            source_rgb.len()
        ));
    }

    #[cfg(feature = "debug-logging")]
    {
        let actual_width = bounds.x_end - bounds.x_start;
        let actual_height = bounds.y_end - bounds.y_start;
        crate::streamline::log_debug_message(&format!(
            "crop_rgb_region_zero_copy: source_width={}, bounds=({},{} to {},{}), region_width={}, region_height={}, actual_width={}, actual_height={}",
            source_width,
            bounds.x_start,
            bounds.y_start,
            bounds.x_end,
            bounds.y_end,
            region_width,
            region_height,
            actual_width,
            actual_height
        ));
        if actual_width != region_width || actual_height != region_height {
            crate::streamline::log_debug_message(&format!(
                "DIMENSION MISMATCH: actual={}x{}, expected={}x{}",
                actual_width, actual_height, region_width, region_height
            ));
        }
    }

    // Process row by row for better cache locality
    for y in bounds.y_start..bounds.y_end {
        let row_start = (y as usize * source_width + bounds.x_start as usize) * 3;
        let row_end = (y as usize * source_width + bounds.x_end as usize) * 3;
        region_pixels.extend_from_slice(&source_rgb[row_start..row_end]);
    }

    #[cfg(feature = "debug-logging")]
    {
        let expected_size = (region_width * region_height * 3) as usize;
        let actual_size = region_pixels.len();
        crate::streamline::log_debug_message(&format!(
            "crop_rgb_region_zero_copy result: expected_size={}, actual_size={}, diff={}",
            expected_size,
            actual_size,
            actual_size as i64 - expected_size as i64
        ));
    }

    Ok((region_pixels, region_width, region_height))
}

/// Integer-only grayscale conversion using BT.709 weights (faster than floating point)
/// Uses fixed-point arithmetic: (54*R + 183*G + 19*B) >> 8
#[inline(always)]
fn rgb_to_grayscale_simd(rgb_data: &[u8]) -> Vec<u8> {
    rgb_data
        .par_chunks_exact(3)
        .map(|rgb| {
            // BT.709 coefficients in fixed-point (8-bit fractional part)
            // 0.2126 * 256 ≈ 54, 0.7152 * 256 ≈ 183, 0.0722 * 256 ≈ 19
            let r = rgb[0] as u16;
            let g = rgb[1] as u16;
            let b = rgb[2] as u16;

            ((54 * r + 183 * g + 19 * b) >> 8) as u8
        })
        .collect()
}

/// Public helper: Convert interleaved RGB bytes to 8-bit grayscale.
/// Uses the same fixed-point BT.709 weights for speed and quality.
#[inline(always)]
pub fn rgb_to_grayscale_u8(rgb_data: &[u8]) -> Vec<u8> {
    rgb_to_grayscale_simd(rgb_data)
}
/// Compute histogram for an 8-bit grayscale image
pub fn histogram_u8(gray: &[u8]) -> [u32; 256] {
    let mut hist = [0u32; 256];
    for &v in gray {
        hist[v as usize] += 1;
    }
    hist
}

/// Heuristic: detect dark-background pages (e.g., inverted documents)
/// Returns true if background appears dark; caller may set invert_input=true.
pub fn auto_dark_background(gray: &[u8], width: usize, height: usize) -> bool {
    if gray.is_empty() || width == 0 || height == 0 {
        return false;
    }
    let hist = histogram_u8(gray);
    // median
    let half = (width * height / 2) as u32;
    let mut cum = 0u32;
    let mut median = 0u8;
    for i in 0..256 {
        cum += hist[i];
        if cum >= half {
            median = i as u8;
            break;
        }
    }
    let bright_mass: u32 = hist[200..].iter().sum();
    median < 64 && bright_mass < (width as u32 * height as u32) / 4
}

/// Invert grayscale buffer in-place
pub fn invert_gray_in_place(gray: &mut [u8]) {
    gray.par_iter_mut().for_each(|p| *p = 255 - *p);
}

/// Zero-copy grayscale conversion that processes directly from source region
#[inline(always)]
fn rgb_to_grayscale_simd_direct(
    source_rgb: &[u8],
    source_width: u32,
    bounds: RegionBounds,
    region_width: u32,
    region_height: u32,
) -> Vec<u8> {
    let mut grayscale = Vec::with_capacity((region_width * region_height) as usize);
    let source_width = source_width as usize;

    // Defensive check: ensure the last accessed byte is in-bounds
    let last_row_end = ((bounds.y_end as usize * source_width) + bounds.x_end as usize) * 3;
    if last_row_end > source_rgb.len() {
        // Return empty buffer; caller will treat as failure downstream
        return Vec::new();
    }

    // Process region row by row directly from source
    for y in bounds.y_start..bounds.y_end {
        let row_start = (y as usize * source_width + bounds.x_start as usize) * 3;

        for x in 0..(region_width as usize) {
            let pixel_idx = row_start + x * 3;
            let r = source_rgb[pixel_idx] as u16;
            let g = source_rgb[pixel_idx + 1] as u16;
            let b = source_rgb[pixel_idx + 2] as u16;

            // BT.709 integer coefficients
            let luminance = ((54 * r + 183 * g + 19 * b) >> 8) as u8;
            grayscale.push(luminance);
        }
    }

    grayscale
}

/// Heavy binarization processor using ONNX model for high-quality document binarization
struct HeavyBinarizationProcessor {
    session: Session,
}

impl HeavyBinarizationProcessor {
    /// Create a new instance of the heavy binarization processor
    fn new() -> Result<Self> {
        let model_path = crate::runtime_asset_path_if_exists("sauvola.onnx").ok_or_else(|| {
            anyhow!(
                "Heavy binarization model not found (expected sauvola.onnx near the executable or under share/lege/models; set LEGE_ASSET_DIR to override)."
            )
        })?;

        let mut builder = Session::builder()
            .map_err(|e| anyhow!("failed to create ORT session builder: {e}"))?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow!("failed to set ORT optimization level: {e}"))?;

        // Prefer platform-appropriate execution providers:
        // - Windows: DirectML
        // - macOS: CoreML
        // - Linux/other: WebGPU if available via build feature, else CPU
        let mut session = {
            #[cfg(target_os = "windows")]
            {
                if let Ok(mut dml_builder) = builder
                    .clone()
                    .with_execution_providers([DirectMLExecutionProvider::default().build()])
                {
                    if let Ok(sess) = dml_builder.commit_from_file(&model_path) {
                        #[cfg(feature = "debug-logging")]
                        crate::streamline::log_debug_message("Heavy binarization: Using DirectML");
                        sess
                    } else {
                        #[cfg(feature = "debug-logging")]
                        crate::streamline::log_debug_message(
                            "Heavy binarization: DirectML unavailable, falling back to CPU",
                        );
                        builder.commit_from_file(&model_path)?
                    }
                } else {
                    builder.commit_from_file(&model_path)?
                }
            }

            #[cfg(target_os = "macos")]
            {
                if let Ok(coreml_builder) = builder
                    .clone()
                    .with_execution_providers([CoreMLExecutionProvider::default().build()])
                {
                    if let Ok(sess) = coreml_builder.commit_from_file(&model_path) {
                        #[cfg(feature = "debug-logging")]
                        crate::streamline::log_debug_message("Heavy binarization: Using CoreML");
                        sess
                    } else {
                        #[cfg(feature = "debug-logging")]
                        crate::streamline::log_debug_message(
                            "Heavy binarization: CoreML unavailable, falling back to CPU",
                        );
                        builder.commit_from_file(&model_path)?
                    }
                } else {
                    builder.commit_from_file(&model_path)?
                }
            }

            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            {
                // With the local ort fork built with the `webgpu` feature on Linux,
                // not specifying a provider lets ORT select WebGPU automatically when available.
                #[cfg(feature = "debug-logging")]
                crate::streamline::log_debug_message(
                    "Heavy binarization: Using WebGPU if available, else CPU",
                );
                builder.commit_from_file(&model_path)?
            }
        };

        // Optional warmup to reduce first-inference latency on Windows (DirectML)
        #[cfg(target_os = "windows")]
        {
            use ndarray::Array4;
            let arr = Array4::<f32>::zeros((1, 8, 8, 1));
            if let Ok(inp) = Value::from_array(arr) {
                let _ = session.run(ort::inputs!["img01_inp" => inp]);
            }
        }

        Ok(Self { session })
    }

    /// Process a region using the heavy binarization model
    fn process_region(
        &mut self,
        rgb_data: &[u8],
        page_width: u32,
        _page_height: u32,
        bounds: RegionBounds,
        region_width: u32,
        region_height: u32,
    ) -> Result<Vec<u8>> {
        // Convert RGB to grayscale and normalize to [0,1] in a single pass
        let img_array = Array4::from_shape_fn(
            (1, region_height as usize, region_width as usize, 1),
            |(_, y, x, _)| {
                let src_x = bounds.x_start as usize + x;
                let src_y = bounds.y_start as usize + y;
                let idx = (src_y * page_width as usize + src_x) * 3;

                // Standard sRGB -> Grayscale conversion with fixed-point math
                let r = rgb_data[idx] as u32;
                let g = rgb_data[idx + 1] as u32;
                let b = rgb_data[idx + 2] as u32;
                let gray = (r * 77 + g * 150 + b * 29) >> 8; // ~= 0.299R + 0.587G + 0.114B

                // Normalize to [0.0, 1.0] for the model
                (gray as f32) / 255.0
            },
        );

        // Run inference
        let inputs = ort::inputs!["img01_inp" => Value::from_array(img_array)
            .map_err(|e| anyhow!("failed to create ORT input tensor: {e}"))?];
        let outputs = self.session.run(inputs)?;

        // Process output
        let output = outputs
            .values()
            .next()
            .ok_or_else(|| anyhow!("Model produced no output"))?;
        let (_shape, data) = output.try_extract_tensor::<f32>()?;

        // Convert output to binary image
        let mut result = vec![0u8; (region_width * region_height) as usize];
        for i in 0..result.len() {
            result[i] = if data[i] > 0.5 { 255 } else { 0 };
        }

        Ok(result)
    }
}

/// Deterministic hash-based blue-noise-like threshold sampler (0..255).
#[inline]
fn blue_noise_hash_u8(x: i32, y: i32) -> u8 {
    let mut v =
        (x as u32).wrapping_mul(0x1f123bb5) ^ (y as u32).wrapping_mul(0x05491333) ^ 0x9e3779b9;
    v ^= v >> 16;
    v = v.wrapping_mul(0x7feb352d);
    v ^= v >> 15;
    v = v.wrapping_mul(0x846ca68b);
    v ^= v >> 16;
    (v & 0xFF) as u8
}

#[inline]
fn blue_noise_tile_64() -> &'static [u8; 64 * 64] {
    static TILE: OnceLock<[u8; 64 * 64]> = OnceLock::new();
    TILE.get_or_init(|| {
        let mut t = [0u8; 64 * 64];
        for y in 0..64usize {
            for x in 0..64usize {
                t[y * 64 + x] = blue_noise_hash_u8(x as i32, y as i32);
            }
        }
        t
    })
}

#[inline]
fn run_length_governor(line: &mut [u8], min_run: u32) {
    if line.len() < 3 || min_run < 2 {
        return;
    }
    let min_run = min_run as usize;
    let mut i = 0;
    while i < line.len() {
        let v = line[i];
        let mut j = i + 1;
        while j < line.len() && line[j] == v {
            j += 1;
        }
        let run_len = j - i;
        if run_len < min_run && i > 0 && j < line.len() {
            let left = line[i - 1];
            let right = line[j];
            if left == right && left != v {
                line[i..j].fill(left);
            }
        }
        i = j;
    }
}

#[inline]
fn edge_alignment_score(line: &[u8], prev_edges: &[u8], curr_edges: &mut [u8]) -> u32 {
    if line.len() < 2 {
        curr_edges.fill(0);
        return 0;
    }

    curr_edges.fill(0);
    let mut score = 0u32;
    let mut edge_count = 0u32;
    let mut last = line[0];

    for x in 1..line.len() {
        let edge = (line[x] != last) as u8;
        last = line[x];
        curr_edges[x] = edge;
        if edge != 0 {
            edge_count += 1;
            let lo = x.saturating_sub(3);
            let hi = (x + 3).min(line.len() - 1);
            let mut aligned = false;
            for k in lo..=hi {
                if prev_edges[k] != 0 {
                    aligned = true;
                    break;
                }
            }
            score += if aligned { 1 } else { 8 };
        }
    }

    // Base transition penalty to discourage noisy rows.
    score + edge_count.saturating_mul(4)
}

#[inline]
fn cheap_distortion_proxy(candidate_line: &[u8], grayscale_data: &[u8], row_offset: usize) -> u32 {
    if candidate_line.is_empty() {
        return 0;
    }

    let mut sum = 0u32;
    // Subsample for speed: good enough as a tie-breaker proxy.
    for x in (0..candidate_line.len()).step_by(2) {
        let cand = candidate_line[x] as i32;
        let src = grayscale_data[row_offset + x] as i32;
        sum = sum.saturating_add((cand - src).unsigned_abs());
    }
    sum
}

/// Dithers an image using phase-locked blue-noise thresholding and run-length governance.
/// Optimised for CCITT4 output: minimises transitions between lines and enforces minimum
/// run lengths so the encoder can use long vertical and horizontal runs.
pub fn dither_phase_locked_blue_noise_rlg(
    grayscale_data: &[u8],
    width: u32,
    height: u32,
) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let expected = w * h;
    if expected == 0 || grayscale_data.len() < expected {
        return Vec::new();
    }

    let tile = blue_noise_tile_64();
    let mut out = vec![255; expected];
    let mut prev_edges = vec![0u8; w];
    let mut curr_edges = vec![0u8; w];
    let mut best_edges = vec![0u8; w];
    let mut phase_x: i32 = 0;
    const SEARCH_EVERY_ROWS: usize = 4;
    const SHIFT_CANDIDATES: [i32; 3] = [-1, 0, 1];
    const MIN_RUN: u32 = 2;
    let mut best_line = vec![255u8; w];
    let mut candidate_line = vec![255u8; w];

    for y in 0..h {
        #[cfg(feature = "debug-logging")]
        if (y & 127) == 0 {
            crate::streamline::log_debug_message(&format!(
                "Blue-noise dithering: row {} / {}",
                y + 1,
                h
            ));
        }

        let row_off = y * w;
        let mut best_cost = u32::MAX;
        let mut best_shift = 0;

        // Re-evaluate phase only periodically; reuse current phase on intermediate rows.
        let shifts: &[i32] = if y % SEARCH_EVERY_ROWS == 0 {
            &SHIFT_CANDIDATES
        } else {
            &[0]
        };

        for &shift in shifts {
            let px = phase_x + shift;
            for x in 0..w {
                let src = grayscale_data[row_off + x];
                let tx = ((x as i32 + px) & 63) as usize;
                let thr = tile[(y & 63) * 64 + tx];
                candidate_line[x] = if src > thr { 255 } else { 0 };
            }

            run_length_governor(&mut candidate_line, MIN_RUN);

            let edge_score = edge_alignment_score(&candidate_line, &prev_edges, &mut curr_edges);
            let distortion = cheap_distortion_proxy(&candidate_line, grayscale_data, row_off);
            let cost = edge_score.saturating_add(distortion / 8);

            if cost < best_cost {
                best_cost = cost;
                best_shift = shift;
                best_line.copy_from_slice(&candidate_line);
                best_edges.copy_from_slice(&curr_edges);
            }
        }

        phase_x += best_shift;
        out[row_off..row_off + w].copy_from_slice(&best_line);
        prev_edges.copy_from_slice(&best_edges);
    }

    out
}

/// Stucki error diffusion dithering - higher quality than Floyd-Steinberg
/// Uses a wider error distribution pattern for better visual results
#[inline(always)]
pub fn dither_stucki_error_diffusion(grayscale_data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let width = width as usize;
    let height = height as usize;

    let expected = width.saturating_mul(height);
    if expected == 0 {
        return Vec::new();
    }

    // Create a mutable buffer of length width*height; copy available pixels and default the rest to white.
    // Clamp near-white pixels to pure white so paper/scan noise doesn't produce sparse dots.
    let mut working_image: Vec<f32> = vec![255.0; expected];
    for (i, &v) in grayscale_data.iter().take(expected).enumerate() {
        working_image[i] = if v >= PAPER_WHITE_FLOOR { 255.0 } else { v as f32 };
    }
    let mut output = Vec::with_capacity(expected);

    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            let old_pixel = working_image[idx];

            // Quantize to binary (0 or 255)
            let new_pixel = if old_pixel >= 127.5 { 255.0 } else { 0.0 };
            output.push(new_pixel as u8);

            // Calculate quantization error
            let error = old_pixel - new_pixel;

            // Stucki error diffusion matrix:
            //       X   8   4
            //   2   4   8   4   2
            //   1   2   4   2   1
            // All divided by 42

            // Current row (right neighbors)
            if x + 1 < width {
                working_image[idx + 1] += error * 8.0 / 42.0;
            }
            if x + 2 < width {
                working_image[idx + 2] += error * 4.0 / 42.0;
            }

            // Next row (y + 1)
            if y + 1 < height {
                let next_row_base = (y + 1) * width;
                if x >= 2 {
                    working_image[next_row_base + x - 2] += error * 2.0 / 42.0;
                }
                if x >= 1 {
                    working_image[next_row_base + x - 1] += error * 4.0 / 42.0;
                }
                working_image[next_row_base + x] += error * 8.0 / 42.0;
                if x + 1 < width {
                    working_image[next_row_base + x + 1] += error * 4.0 / 42.0;
                }
                if x + 2 < width {
                    working_image[next_row_base + x + 2] += error * 2.0 / 42.0;
                }
            }

            // Row y + 2
            if y + 2 < height {
                let next2_row_base = (y + 2) * width;
                if x >= 2 {
                    working_image[next2_row_base + x - 2] += error * 1.0 / 42.0;
                }
                if x >= 1 {
                    working_image[next2_row_base + x - 1] += error * 2.0 / 42.0;
                }
                working_image[next2_row_base + x] += error * 4.0 / 42.0;
                if x + 1 < width {
                    working_image[next2_row_base + x + 1] += error * 2.0 / 42.0;
                }
                if x + 2 < width {
                    working_image[next2_row_base + x + 2] += error * 1.0 / 42.0;
                }
            }
        }
    }

    output
}

/// Create an inversion mask for a given bounding box on a page
pub fn create_inversion_mask(mask: &mut [bool], page_width: u32, bbox: [f32; 4]) {
    if mask.is_empty() || page_width == 0 {
        return;
    }

    let (mut x1, mut y1, mut x2, mut y2) = (
        bbox[0].max(0.0) as u32,
        bbox[1].max(0.0) as u32,
        bbox[2].max(0.0) as u32,
        bbox[3].max(0.0) as u32,
    );

    if x1 >= x2 || y1 >= y2 {
        return;
    }

    let page_height = mask.len() as u32 / page_width;
    // Clamp to page bounds
    x1 = x1.min(page_width.saturating_sub(1));
    y1 = y1.min(page_height.saturating_sub(1));
    x2 = x2.min(page_width);
    y2 = y2.min(page_height);

    for y in y1..y2 {
        let row_start = (y * page_width + x1) as usize;
        let row_end = (y * page_width + x2) as usize;
        if row_end > mask.len() {
            break;
        }
        mask[row_start..row_end].fill(true);
    }
}

/// Mask a binary image region by setting pixels to white (255) within bbox
pub fn mask_region(binarized: &mut [u8], page_width: u32, bbox: [f32; 4]) {
    if binarized.is_empty() || page_width == 0 {
        return;
    }

    let (mut x1, mut y1, mut x2, mut y2) = (
        bbox[0].max(0.0) as u32,
        bbox[1].max(0.0) as u32,
        bbox[2].max(0.0) as u32,
        bbox[3].max(0.0) as u32,
    );

    if x1 >= x2 || y1 >= y2 {
        return;
    }

    let page_height = binarized.len() as u32 / page_width;
    // Clamp to page bounds
    x1 = x1.min(page_width.saturating_sub(1));
    y1 = y1.min(page_height.saturating_sub(1));
    x2 = x2.min(page_width);
    y2 = y2.min(page_height);

    for y in y1..y2 {
        let row_start = (y * page_width + x1) as usize;
        let row_end = (y * page_width + x2) as usize;
        if row_end > binarized.len() {
            break;
        }
        binarized[row_start..row_end].fill(255);
    }
}

/// Pack 0/1 binarized rows into MSB-first bit-packed bytes
pub fn pack_bits(binarized: &[u8], width: usize, height: usize) -> Vec<u8> {
    let bytes_per_row = (width + 7) / 8;
    let mut packed = vec![0u8; bytes_per_row * height];
    for y in 0..height {
        for x in 0..width {
            let src_idx = y * width + x;
            let dst_byte = y * bytes_per_row + (x / 8);
            let dst_bit = 7 - (x % 8); // MSB first
            if src_idx < binarized.len() && binarized[src_idx] != 0 {
                packed[dst_byte] |= 1 << dst_bit;
            }
        }
    }
    packed
}

/// Merge a dithered region back into the page-sized binarized image buffer.
///
/// **Placement must match `validate_region_bounds`** (used by `process_image_region`):
/// origin is `(round(x1), round(y1))`, size `(round(x2), round(y2))` clamped to the page.
/// The previous implementation used `bbox[0] as u32` (truncate) instead of `round`, which
/// misaligned merged pixels and looked like blown-out / “few lines” halos.
pub fn merge_dithered_region(
    binarized: &mut [u8],
    grayscale_data: &[u8],
    page_width: u32,
    bbox: [f32; 4],
) {
    let page_height = (binarized.len() as u32 / page_width).max(1);
    let [x1, y1, x2, y2] = bbox;
    let x_start = (x1.round() as u32).min(page_width.saturating_sub(1));
    let y_start = (y1.round() as u32).min(page_height.saturating_sub(1));
    let x_end = (x2.round() as u32).min(page_width);
    let y_end = (y2.round() as u32).min(page_height);
    if x_start >= x_end || y_start >= y_end {
        return;
    }
    let rw = x_end - x_start;
    let rh = y_end - y_start;
    let expected = (rw * rh) as usize;
    if grayscale_data.len() < expected {
        #[cfg(feature = "debug-logging")]
        crate::streamline::log_debug_message(&format!(
            "merge_dithered_region: buffer too short (have {}, need {} for {}x{} bbox {:?})",
            grayscale_data.len(),
            expected,
            rw,
            rh,
            bbox
        ));
        return;
    }

    for y in 0..rh {
        let src_row = y as usize * rw as usize;
        for x in 0..rw {
            let src_idx = src_row + x as usize;
            let dst_idx = (y_start as usize + y as usize) * page_width as usize
                + (x_start as usize + x as usize);
            if dst_idx < binarized.len() {
                binarized[dst_idx] = grayscale_data[src_idx];
            }
        }
    }
}
