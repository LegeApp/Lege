//! JBIG2 Halftone Region Encoder
//!
//! This module implements a high-quality lossy halftone encoding strategy
//! based on the paper "Lossy Compression of Stochastic Halftones with JBIG2"
//! by M. Valliappan, B. L. Evans, D. A. D. Tompkins, and F. Kossentini.
//!
//! The pipeline is as follows:
//! 1. **Prefilter**: A 3x3 smoothing filter reduces noise in the input stochastic halftone.
//! 2. **Decimate**: The filtered image is downsampled by a factor of M, creating a
//!    lower-resolution grayscale representation.
//! 3. **Quantize**: The grayscale image is quantized to N levels using a multi-level
//!    error diffusion dither to preserve quality and prevent contouring.
//! 4. **Standard Encoding**: The resulting pattern dictionary and quantized grayscale
//!    image are then encoded into a JBIG2 compliant bitstream.

use crate::jbig2arith::Jbig2ArithCoder;
use crate::jbig2structs::{GenericRegionParams, HalftoneParams, Jbig2Config, Segment, SegmentType};
use crate::jbig2sym::BitImage;
use anyhow::{anyhow, Result};
use ndarray::{Array2, Axis};

#[allow(unused_imports)]
use crate::{debug, trace};


/// Main entry point for encoding a halftone region.
///
/// This function takes a bi-level halftone image and produces two JBIG2 segments:
/// 1. Pattern Dictionary segment (Type 16) containing the halftone patterns
/// 2. Halftone Region segment (Type 22 for lossy or Type 23 for lossless)
///
/// The halftone region segment will properly reference the pattern dictionary segment.
pub fn encode_halftone_region(
    image: &BitImage,
    config: &Jbig2Config,
    region_params: &GenericRegionParams,
    pattern_dict_segment_number: u32,
    halftone_segment_number: u32,
    page_number: Option<u32>,
) -> Result<(Segment, Segment)> {
    let ht_config = &config.halftone;

    // --- 1. Encoder Pipeline (from the paper) ---

    // Step 1: Prefilter the stochastic halftone to reduce noise.
    trace!("Halftone: Prefiltering...");
    let filtered_image = prefilter(image);

    // Step 2: Decimate the filtered image to create a grayscale representation.
    trace!("Halftone: Decimating with M={}...", ht_config.grid_size_m);
    let decimated_image = decimate(&filtered_image, ht_config.grid_size_m as usize);

    // Step 3: Quantize using multi-level error diffusion.
    trace!("Halftone: Quantizing with N={} and L={}...", ht_config.quant_levels_n, ht_config.sharpening_l);
    let quantized_gray_image = quantize_with_error_diffusion(
        &decimated_image,
        ht_config.quant_levels_n,
        ht_config.sharpening_l,
        ht_config.grid_size_m,
    );

    // --- 2. JBIG2 Standard Compliant Encoding ---

    // Step 4: Generate a pattern dictionary.
    trace!("Halftone: Generating pattern dictionary...");
    let pattern_dictionary = generate_pattern_dictionary(
        ht_config.grid_size_m as usize,
        ht_config.quant_levels_n as usize,
    );

    // Step 5: Encode the pattern dictionary payload.
    // All patterns are concatenated into one large bitmap and encoded as a generic region.
    let dict_payload = encode_pattern_dictionary_payload(&pattern_dictionary, config)?;

    // Step 6: Encode the quantized grayscale image payload according to Annex C.
    let gray_image_payload =
        encode_grayscale_image_annex_c(&quantized_gray_image, ht_config.template)?;

    // Step 7: Assemble the halftone region header.
    // Calculate grid offset based on region position and grid alignment
    let grid_x = (region_params.x % ht_config.grid_size_m) as u16;
    let grid_y = (region_params.y % ht_config.grid_size_m) as u16;
    
    let ht_params = HalftoneParams {
        width: region_params.width,
        height: region_params.height,
        x: region_params.x,
        y: region_params.y,
        grid_width: quantized_gray_image.ncols() as u32,
        grid_height: quantized_gray_image.nrows() as u32,
        grid_x,
        grid_y,
        grid_vector_x: ht_config.grid_size_m as u16 * 256, // Simple non-angled grid
        grid_vector_y: 0,
        pattern_width: ht_config.grid_size_m as u8,
        pattern_height: ht_config.grid_size_m as u8,
        template: ht_config.template,
        ..Default::default()
    };

    let mut halftone_payload = ht_params.to_bytes();
    halftone_payload.extend(gray_image_payload);

    // Create Pattern Dictionary segment (Type 16)
    let pattern_dict_segment = Segment {
        number: pattern_dict_segment_number,
        seg_type: SegmentType::PatternDictionary,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: if page_number.is_some() { 0 } else { 2 }, // explicit or all pages
        referred_to: Vec::new(), // Pattern dictionaries don't refer to other segments
        page: page_number,
        payload: dict_payload,
    };

    // Determine the appropriate halftone segment type based on configuration
    let halftone_seg_type = if ht_config.lossless {
        SegmentType::ImmediateLosslessHalftoneRegion
    } else {
        SegmentType::ImmediateHalftoneRegion
    };

    // Create Halftone Region segment that references the pattern dictionary
    let halftone_segment = Segment {
        number: halftone_segment_number,
        seg_type: halftone_seg_type,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: if page_number.is_some() { 0 } else { 2 }, // explicit or all pages
        referred_to: vec![pattern_dict_segment_number], // Reference the pattern dictionary
        page: page_number,
        payload: halftone_payload,
    };

    Ok((pattern_dict_segment, halftone_segment))
}

/// Convenience function for encoding a halftone region with automatic segment numbering.
/// 
/// This function provides a simpler interface for callers who don't need to manage
/// segment numbers manually. It uses sequential numbering starting from the provided
/// base segment number.
pub fn encode_halftone_region_auto(
    image: &BitImage,
    config: &Jbig2Config,
    region_params: &GenericRegionParams,
    base_segment_number: u32,
    page_number: Option<u32>,
) -> Result<(Segment, Segment)> {
    encode_halftone_region(
        image,
        config,
        region_params,
        base_segment_number,     // Pattern dictionary segment
        base_segment_number + 1, // Halftone region segment
        page_number,
    )
}

/// Step 1: Applies a 3x3 smoothing prefilter with power-of-two coefficients.
/// Kernel: [[1, 2, 1], [2, 4, 2], [1, 2, 1]] / 16
fn prefilter(image: &BitImage) -> Array2<f32> {
    let (w, h) = (image.width, image.height);
    let mut out = Array2::<f32>::zeros((h, w));

    for y in 0..h {
        for x in 0..w {
            let mut sum = 0;
            // Center pixel (weight 4)
            sum += image.get_usize(x, y) as u32 * 4;
            // Orthogonal neighbors (weight 2)
            sum += image.get_usize_safe(x.wrapping_sub(1), y) as u32 * 2;
            sum += image.get_usize_safe(x + 1, y) as u32 * 2;
            sum += image.get_usize_safe(x, y.wrapping_sub(1)) as u32 * 2;
            sum += image.get_usize_safe(x, y + 1) as u32 * 2;
            // Diagonal neighbors (weight 1)
            sum += image.get_usize_safe(x.wrapping_sub(1), y.wrapping_sub(1)) as u32;
            sum += image.get_usize_safe(x + 1, y.wrapping_sub(1)) as u32;
            sum += image.get_usize_safe(x.wrapping_sub(1), y + 1) as u32;
            sum += image.get_usize_safe(x + 1, y + 1) as u32;

            out[[y, x]] = sum as f32 / 16.0;
        }
    }
    out
}

/// Step 2: Decimates (downsamples) the grayscale image by summing M x M blocks.
fn decimate(gray_image: &Array2<f32>, m: usize) -> Array2<f32> {
    let (h, w) = gray_image.dim();
    let (out_h, out_w) = ((h + m - 1) / m, (w + m - 1) / m);
    let mut out = Array2::<f32>::zeros((out_h, out_w));

    for y in 0..out_h {
        for x in 0..out_w {
            let start_y = y * m;
            let start_x = x * m;
            let end_y = (start_y + m).min(h);
            let end_x = (start_x + m).min(w);

            let window = gray_image.slice(ndarray::s![start_y..end_y, start_x..end_x]);
            out[[y, x]] = window.sum();
        }
    }
    out
}

/// Step 3: Quantizes to N levels using modified Stucki error diffusion.
/// Stucki dithering provides higher quality output than Floyd-Steinberg by
/// distributing error across a wider neighborhood.
fn quantize_with_error_diffusion(
    decimated_image: &Array2<f32>,
    n: u32,
    l: f32,
    m: u32,
) -> Array2<u8> {
    let (h, w) = decimated_image.dim();
    let mut out = Array2::<u8>::zeros((h, w));
    // Create a temporary float array to accumulate errors
    let mut temp_image = decimated_image.mapv(|v| v * (n - 1) as f32 / (m * m) as f32);

    let mut last_quant_error = 0.0;

    for y in 0..h {
        for x in 0..w {
            // Apply sharpening as per the paper's "modified error diffusion"
            // u[m,n] = y[m,n] - L * qe[m,n]
            let pixel_val = temp_image[[y, x]] - l * last_quant_error;

            // Quantize the pixel
            let quantized_val = pixel_val.round().max(0.0).min((n - 1) as f32);
            out[[y, x]] = quantized_val as u8;

            // Calculate the error and diffuse it using Stucki weights
            let error = pixel_val - quantized_val;
            last_quant_error = error; // Update for next pixel

            // Stucki error diffusion matrix:
            //       X   8   4
            //   2   4   8   4   2
            //   1   2   4   2   1
            // All divided by 42

            // Current row (y), right neighbors
            if x + 1 < w { temp_image[[y, x + 1]] += error * 8.0 / 42.0; }
            if x + 2 < w { temp_image[[y, x + 2]] += error * 4.0 / 42.0; }

            // Next row (y + 1)
            if y + 1 < h {
                if x >= 2 { temp_image[[y + 1, x - 2]] += error * 2.0 / 42.0; }
                if x >= 1 { temp_image[[y + 1, x - 1]] += error * 4.0 / 42.0; }
                temp_image[[y + 1, x]] += error * 8.0 / 42.0;
                if x + 1 < w { temp_image[[y + 1, x + 1]] += error * 4.0 / 42.0; }
                if x + 2 < w { temp_image[[y + 1, x + 2]] += error * 2.0 / 42.0; }
            }

            // Row y + 2
            if y + 2 < h {
                if x >= 2 { temp_image[[y + 2, x - 2]] += error * 1.0 / 42.0; }
                if x >= 1 { temp_image[[y + 2, x - 1]] += error * 2.0 / 42.0; }
                temp_image[[y + 2, x]] += error * 4.0 / 42.0;
                if x + 1 < w { temp_image[[y + 2, x + 1]] += error * 2.0 / 42.0; }
                if x + 2 < w { temp_image[[y + 2, x + 2]] += error * 1.0 / 42.0; }
            }
        }
    }
    out
}


/// Bayer matrix for clustered-dot pattern generation
/// Using 8x8 Bayer matrix for high-quality halftone patterns
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

/// Step 4: Generates an ideal pattern dictionary with N patterns of size M x M.
/// Uses Bayer matrix thresholding for clustered-dot patterns that provide
/// better visual quality than simple raster fill.
fn generate_pattern_dictionary(m: usize, n: usize) -> Vec<BitImage> {
    let mut dict = Vec::with_capacity(n);

    for i in 0..n {
        let mut pattern = BitImage::new(m, m).unwrap();
        
        // Calculate the threshold level for this gray level
        // Scale from [0, n-1] to [0, 255] for comparison with Bayer matrix
        let gray_level = if n > 1 { (i * 255) / (n - 1) } else { 0 };
        
        // Generate pattern using Bayer matrix thresholding
        for y in 0..m {
            for x in 0..m {
                // Use Bayer matrix coordinates modulo the matrix size
                let bayer_x = x % 8;
                let bayer_y = y % 8;
                let threshold = BAYER_8X8[bayer_y][bayer_x];
                
                // Scale threshold from [0, 63] to [0, 255] for comparison
                let scaled_threshold = (threshold as usize * 255) / 63;
                
                // Set pixel based on threshold comparison
                if gray_level > scaled_threshold {
                    pattern.set(x, y, true);
                }
            }
        }
        dict.push(pattern);
    }
    dict
}

/// Encodes the pattern dictionary by concatenating all patterns into a single
/// large bitmap and using the generic region encoder.
fn encode_pattern_dictionary_payload(patterns: &[BitImage], config: &Jbig2Config) -> Result<Vec<u8>> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let p_h = patterns[0].height;
    let p_w = patterns[0].width;
    let total_width = p_w * patterns.len();

    // Create a single large bitmap to hold all patterns
    let mut collective_bitmap = BitImage::new(total_width, p_h)?;

    for (i, pattern) in patterns.iter().enumerate() {
        collective_bitmap.blit(pattern, i * p_w, 0);
    }

    // Encode this collective bitmap using the generic arithmetic coder
    Jbig2ArithCoder::encode_generic_payload(
        &collective_bitmap,
        config.generic.template,
        &config.generic.at_pixels,
    )
}

/// Encodes the grayscale image according to JBIG2 Annex C.
/// This involves bitplane decomposition, Gray coding, and arithmetic coding of each plane.
fn encode_grayscale_image_annex_c(gray_image: &Array2<u8>, template: u8) -> Result<Vec<u8>> {
    let (h, w) = gray_image.dim();
    let max_val = gray_image.iter().max().copied().unwrap_or(0);
    let num_bits = if max_val == 0 { 1 } else { (max_val as f32 + 1.0).log2().ceil() as usize };

    // 1. Decompose into bitplanes
    let mut bitplanes: Vec<BitImage> = (0..num_bits)
        .map(|_| BitImage::new(w, h).unwrap())
        .collect();

    for y in 0..h {
        for x in 0..w {
            let val = gray_image[[y, x]];
            for b in 0..num_bits {
                if (val >> b) & 1 == 1 {
                    bitplanes[b].set(x, y, true);
                }
            }
        }
    }

    let mut coder = Jbig2ArithCoder::new();
    let mut encoded_payload = Vec::new();

    // 2. Gray-code and encode each bitplane
    for i in (0..num_bits).rev() {
        let mut plane_to_encode = bitplanes[i].clone();

        // Gray coding: current plane is XORed with the original version of the next higher plane
        if i < num_bits - 1 {
            for y in 0..h {
                for x in 0..w {
                    let current_bit = plane_to_encode.get_usize(x, y);
                    let higher_bit = bitplanes[i + 1].get_usize(x, y);
                    plane_to_encode.set(x, y, current_bit ^ higher_bit);
                }
            }
        }

        // Encode the (potentially modified) bitplane using the generic region coder
        // The AT pixels for halftone bitplanes are fixed by the spec.
        let at_pixels = [ (3, -1), (-3, -1), (2, -2), (-2, -2) ];
        let packed_data = plane_to_encode.to_packed_words();
        coder.encode_generic_region_inner(&packed_data, w, h, template, &at_pixels)?;

        // NOTE: The standard implies one continuous arithmetic stream for all bitplanes.
        // We will flush once at the end.
    }

    coder.flush(true);
    encoded_payload.extend(coder.as_bytes());

    Ok(encoded_payload)
}