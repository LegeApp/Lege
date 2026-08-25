// linearize.rs
use rayon::prelude::*;
use std::sync::LazyLock;

// Runtime-initialized LUTs. `powf` is not const, so these stay lazy statics —
// `std::sync::LazyLock`, no external crate required.
static SRGB_TO_LINEAR: LazyLock<[f32; 256]> = LazyLock::new(|| {
    let mut table = [0f32; 256];
    for (i, val) in table.iter_mut().enumerate() {
        let x = i as f32 / 255.0;
        *val = if x <= 0.04045 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        };
    }
    table
});

static LINEAR_TO_SRGB: LazyLock<[u8; 1025]> = LazyLock::new(|| {
    let mut table = [0u8; 1025];
    for (i, slot) in table.iter_mut().enumerate() {
        let x = (i as f32) / 1024.0;
        let y = if x <= 0.0031308 {
            x * 12.92
        } else {
            1.055 * x.powf(1.0 / 2.4) - 0.055
        };
        *slot = (y * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
    }
    table
});

/// sRGB-encoded gray (0..255) → linear-reflectance-coded gray (0..255).
/// On a bilevel output, perceived tone is dot *coverage*, which is linear
/// reflectance — so dithering must diffuse/threshold linear values, else an
/// sRGB 128 (~21% luminance) renders as ~50% coverage and midtones come out
/// far too light. Mapping the input through this LUT first fixes that.
pub static SRGB_GRAY_TO_LINEAR_U8: LazyLock<[u8; 256]> = LazyLock::new(|| {
    let mut table = [0u8; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        *slot = (SRGB_TO_LINEAR[i] * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
    }
    table
});

// Convert sRGB value to linear RGB (LUT version)
#[inline(always)]
fn srgb_to_linear(c: u8) -> f32 {
    SRGB_TO_LINEAR[c as usize]
}

// Convert linear RGB value to sRGB (LUT version for better performance)
#[inline(always)]
fn linear_to_srgb(c: f32) -> u8 {
    let idx = (c.clamp(0.0, 1.0) * 1024.0) as usize;
    LINEAR_TO_SRGB[idx]
}

// Parallel sRGB to linear RGB conversion
fn srgb_to_linear_wide(srgb: &[u8], linear: &mut [f32]) {
    // Process in parallel using Rayon's parallel iterators
    linear
        .par_chunks_mut(3)
        .enumerate()
        .for_each(|(i, linear_chunk)| {
            let src_idx = i * 3;
            if src_idx + 2 < srgb.len() {
                linear_chunk[0] = srgb_to_linear(srgb[src_idx]);
                linear_chunk[1] = srgb_to_linear(srgb[src_idx + 1]);
                linear_chunk[2] = srgb_to_linear(srgb[src_idx + 2]);
            }
        });
}

// Parallel linear RGB to sRGB conversion
fn linear_to_srgb_wide(linear: &[f32], srgb: &mut [u8]) {
    // Process in parallel using Rayon's parallel iterators
    srgb.par_chunks_mut(3)
        .enumerate()
        .for_each(|(i, srgb_chunk)| {
            let src_idx = i * 3;
            if src_idx + 2 < linear.len() {
                srgb_chunk[0] = linear_to_srgb(linear[src_idx]);
                srgb_chunk[1] = linear_to_srgb(linear[src_idx + 1]);
                srgb_chunk[2] = linear_to_srgb(linear[src_idx + 2]);
            }
        });
}

// `load_and_linearize` and its `LinearizeInput` enum were removed: nothing in
// the workspace called them, and they were the crate's only use of `ndarray`
// (and of `image::open`). The live entry points are
// `linearize_rgb_bytes_to_f32` / `linearized_rgb_to_grayscale` below.

/// Convert linearized RGB (0.0-1.0 range) to grayscale (8-bit) using parallel processing.
///
/// Computes BT.709 luma in linear space then re-encodes to sRGB via the precomputed
/// LINEAR_TO_SRGB LUT. Matches the previous palette-crate path to within ±1 LSB
/// (LUT quantization at 1024 steps) and avoids a per-pixel `powf`.
pub fn linearized_rgb_to_grayscale(linear_rgb: &[f32], gray: &mut [u8]) {
    assert!(
        linear_rgb.len() == gray.len() * 3,
        "Input length must be 3x output length"
    );

    gray.par_iter_mut()
        .zip(linear_rgb.par_chunks_exact(3))
        .for_each(|(out, rgb)| {
            let r = rgb[0].clamp(0.0, 1.0);
            let g = rgb[1].clamp(0.0, 1.0);
            let b = rgb[2].clamp(0.0, 1.0);
            // BT.709 (D65) luma in linear space.
            let y_lin = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            *out = linear_to_srgb(y_lin);
        });
}

/// Convert RGB bytes to linearized f32 values using optimized parallel processing
pub fn linearize_rgb_bytes_to_f32(rgb_bytes: &[u8]) -> Vec<f32> {
    let mut linear_rgb = vec![0.0f32; rgb_bytes.len()];
    srgb_to_linear_wide(rgb_bytes, &mut linear_rgb);
    linear_rgb
}

/// Convert linear RGB to sRGB bytes using optimized parallel processing
pub fn linear_to_srgb_bytes(linear: &[f32]) -> Vec<u8> {
    let mut srgb_bytes = vec![0u8; linear.len()];
    linear_to_srgb_wide(linear, &mut srgb_bytes);
    srgb_bytes
}
