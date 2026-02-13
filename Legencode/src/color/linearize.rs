// linearize.rs
use ndarray::Array3;
use rayon::prelude::*;
use std::error::Error;

// Use once_cell to initialize the LUT at runtime
use once_cell::sync::Lazy;

static SRGB_TO_LINEAR: Lazy<[f32; 256]> = Lazy::new(|| {
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

static LINEAR_TO_SRGB: Lazy<[u8; 1025]> = Lazy::new(|| {
    let mut table = [0u8; 1025];
    for i in 0..1025 {
        let x = (i as f32) / 1024.0;
        let y = if x <= 0.0031308 {
            x * 12.92
        } else {
            1.055 * x.powf(1.0 / 2.4) - 0.055
        };
        table[i] = (y * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
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

/// Shared CPU function: Load an image and linearize sRGB to linear RGB
pub fn load_and_linearize<'a>(
    input: impl Into<LinearizeInput<'a>>,
    width: usize,
    height: usize,
) -> Result<Array3<f32>, Box<dyn Error>> {
    let input = input.into();

    // Load or prepare RGB data
    let (rgb_bytes, actual_width, actual_height) = match input {
        LinearizeInput::Path(_path) => {
            // Path loading removed with image crate dependency
            // For path-based loading, use external tools or pass bytes directly
            return Err("Path-based image loading not supported. Use LinearizeInput::Bytes.".into());
        }
        LinearizeInput::Bytes(bytes) => {
            if width == 0 || height == 0 || bytes.len() != width * height * 3 {
                return Err("Invalid dimensions or byte length".into());
            }
            (bytes.to_vec(), width, height)
        }
    };

    // Convert to linear RGB using optimized parallel processing
    let mut linear_rgb = vec![0.0f32; rgb_bytes.len()];
    srgb_to_linear_wide(&rgb_bytes, &mut linear_rgb);

    // Shape into Array3<f32>
    let array = Array3::from_shape_vec((actual_height, actual_width, 3), linear_rgb)?;
    Ok(array)
}

/// Helper enum to accept either a file path or raw bytes.
pub enum LinearizeInput<'a> {
    Path(&'a str),
    Bytes(&'a [u8]),
}

impl<'a> From<&'a str> for LinearizeInput<'a> {
    fn from(path: &'a str) -> Self {
        LinearizeInput::Path(path)
    }
}

impl<'a> From<&'a [u8]> for LinearizeInput<'a> {
    fn from(bytes: &'a [u8]) -> Self {
        LinearizeInput::Bytes(bytes)
    }
}

/// Convert linearized RGB (0.0-1.0 range) to grayscale (8-bit) using parallel processing
/// Uses BT.709 luminance coefficients: Y = 0.2126*R + 0.7152*G + 0.0722*B
pub fn linearized_rgb_to_grayscale(linear_rgb: &[f32], gray: &mut [u8]) {
    assert!(
        linear_rgb.len() == gray.len() * 3,
        "Input length must be 3x output length"
    );

    gray.par_iter_mut().enumerate().for_each(|(i, chunk)| {
        let r = linear_rgb[3 * i].clamp(0.0, 1.0);
        let g = linear_rgb[3 * i + 1].clamp(0.0, 1.0);
        let b = linear_rgb[3 * i + 2].clamp(0.0, 1.0);
        
        // BT.709 luminance formula for linear RGB
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        
        // Convert back to sRGB gamma space for grayscale output
        *chunk = linear_to_srgb(luma);
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
