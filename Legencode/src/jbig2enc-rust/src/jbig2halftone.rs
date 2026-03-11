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
use crate::jbig2shared::usize_to_u32;
use crate::jbig2structs::{
    FileHeader, GenericRegionParams, HalftoneParams, Jbig2Config, PageInfo,
    PatternDictionaryParams, Segment, SegmentType,
};
use crate::jbig2sym::BitImage;
use anyhow::{Result, anyhow};
use fax::encoder::Encoder as FaxEncoder;
use fax::{Color as FaxColor, VecWriter as FaxVecWriter};
use ndarray::Array2;

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

    // Step 1: Convert the bilevel input to a grayscale working image without blur.
    trace!("Halftone: Preparing grayscale source...");
    let filtered_image = prefilter(image);

    // Step 2: Decimate the working image to create a grayscale representation.
    trace!("Halftone: Decimating with M={}...", ht_config.grid_size_m);
    let decimated_image = decimate(&filtered_image, ht_config.grid_size_m as usize);

    // Step 3: Quantize using multi-level error diffusion.
    trace!(
        "Halftone: Quantizing with N={} and L={}...",
        ht_config.quant_levels_n, ht_config.sharpening_l
    );
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
    let dict_template = if ht_config.dict_mmr {
        0
    } else {
        ht_config.template
    };
    let dict_payload =
        encode_pattern_dictionary_payload(&pattern_dictionary, dict_template, ht_config.dict_mmr)?;

    // Step 6: Encode the quantized grayscale image payload according to Annex C.
    let effective_template = if ht_config.gray_mmr {
        0
    } else {
        ht_config.template
    };
    let gray_image_payload = if ht_config.gray_mmr {
        encode_grayscale_image_mmr(&quantized_gray_image)?
    } else {
        encode_grayscale_image_annex_c(&quantized_gray_image, effective_template)?
    };

    // Step 7: Assemble the halftone region header.
    // Calculate grid offset based on region position and grid alignment
    let ht_params = HalftoneParams {
        width: region_params.width,
        height: region_params.height,
        x: region_params.x,
        y: region_params.y,
        region_comb_operator: region_params.comb_operator,
        mmr: ht_config.gray_mmr,
        skip_enabled: false,
        halftone_comb_operator: 0,
        default_pixel: false,
        grid_width: quantized_gray_image.ncols() as u32,
        grid_height: quantized_gray_image.nrows() as u32,
        grid_x: (region_params.x as i32) * 256,
        grid_y: (region_params.y as i32) * 256,
        grid_vector_x: ht_config.grid_size_m as u16 * 256, // Simple non-angled grid
        grid_vector_y: 0,
        template: effective_template,
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

/// Encodes a halftone region into a JBIG2 segment stream containing:
/// 1) Pattern Dictionary segment
/// 2) Halftone Region segment
pub fn encode_halftone_region_stream_auto(
    image: &BitImage,
    config: &Jbig2Config,
    region_params: &GenericRegionParams,
    base_segment_number: u32,
    page_number: Option<u32>,
) -> Result<Vec<u8>> {
    let page_num = page_number.unwrap_or(1);
    let page_info = Segment {
        number: base_segment_number,
        seg_type: SegmentType::PageInformation,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 0,
        referred_to: vec![],
        page: Some(page_num),
        payload: PageInfo {
            width: region_params.width,
            height: region_params.height,
            xres: config.generic.dpi,
            yres: config.generic.dpi,
            default_pixel: false,
            ..Default::default()
        }
        .to_bytes(),
    };

    let (pattern_dict, halftone_region) = encode_halftone_region(
        image,
        config,
        region_params,
        base_segment_number + 1,
        base_segment_number + 2,
        Some(page_num),
    )?;

    let end_page = Segment {
        number: base_segment_number + 3,
        seg_type: SegmentType::EndOfPage,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 0,
        referred_to: vec![],
        page: Some(page_num),
        payload: Vec::new(),
    };

    let mut stream = Vec::new();
    page_info.write_into(&mut stream)?;
    pattern_dict.write_into(&mut stream)?;
    halftone_region.write_into(&mut stream)?;
    end_page.write_into(&mut stream)?;
    Ok(stream)
}

/// Encodes a single-page JBIG2 fragment suitable for PDF embedding.
pub fn encode_halftone_pdf_fragment_auto(
    image: &BitImage,
    config: &Jbig2Config,
    region_params: &GenericRegionParams,
    base_segment_number: u32,
    page_number: Option<u32>,
) -> Result<Vec<u8>> {
    let page_num = page_number.unwrap_or(1);
    let page_info = Segment {
        number: base_segment_number,
        seg_type: SegmentType::PageInformation,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 0,
        referred_to: vec![],
        page: Some(page_num),
        payload: PageInfo {
            width: region_params.width,
            height: region_params.height,
            xres: config.generic.dpi,
            yres: config.generic.dpi,
            default_pixel: false,
            ..Default::default()
        }
        .to_bytes(),
    };

    let (pattern_dict, halftone_region) = encode_halftone_region(
        image,
        config,
        region_params,
        base_segment_number + 1,
        base_segment_number + 2,
        Some(page_num),
    )?;

    let mut stream = Vec::new();
    page_info.write_into(&mut stream)?;
    pattern_dict.write_into(&mut stream)?;
    halftone_region.write_into(&mut stream)?;
    Ok(stream)
}

/// Encodes halftone data as a PDF-style split stream:
/// global pattern dictionary plus page-local page info and halftone region.
pub fn encode_halftone_pdf_split_auto(
    image: &BitImage,
    config: &Jbig2Config,
    region_params: &GenericRegionParams,
    base_segment_number: u32,
    page_number: Option<u32>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let page_num = page_number.unwrap_or(1);
    let page_info = Segment {
        number: base_segment_number + 1,
        seg_type: SegmentType::PageInformation,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 0,
        referred_to: vec![],
        page: Some(page_num),
        payload: PageInfo {
            width: region_params.width,
            height: region_params.height,
            xres: config.generic.dpi,
            yres: config.generic.dpi,
            default_pixel: false,
            ..Default::default()
        }
        .to_bytes(),
    };

    let (pattern_dict, halftone_region) = encode_halftone_region(
        image,
        config,
        region_params,
        base_segment_number,
        base_segment_number + 2,
        Some(page_num),
    )?;

    let mut globals = Vec::new();
    let mut dict_segment = pattern_dict;
    dict_segment.page = None;
    dict_segment.page_association_type = 2;
    dict_segment.write_into(&mut globals)?;

    let mut page_stream = Vec::new();
    page_info.write_into(&mut page_stream)?;
    halftone_region.write_into(&mut page_stream)?;

    Ok((globals, page_stream))
}

fn quantized_from_grayscale_input(
    gray: &[u8],
    width: u32,
    height: u32,
    config: &Jbig2Config,
) -> Result<Array2<u8>> {
    let w = width as usize;
    let h = height as usize;
    if gray.len() != w.saturating_mul(h) {
        return Err(anyhow!(
            "grayscale buffer size mismatch: got {}, expected {}",
            gray.len(),
            w.saturating_mul(h)
        ));
    }
    let ht_config = &config.halftone;
    if ht_config.grid_size_m == 0 {
        return Err(anyhow!("halftone grid_size_m must be > 0"));
    }

    let mut src = Array2::<f32>::zeros((h, w));
    for y in 0..h {
        for x in 0..w {
            src[[y, x]] = gray[y * w + x] as f32 / 255.0;
        }
    }

    // Light 3x3 box prefilter for continuous-tone image regions.
    let mut filtered = Array2::<f32>::zeros((h, w));
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0.0f32;
            let mut cnt = 0.0f32;
            let y0 = y.saturating_sub(1);
            let y1 = (y + 1).min(h - 1);
            let x0 = x.saturating_sub(1);
            let x1 = (x + 1).min(w - 1);
            for yy in y0..=y1 {
                for xx in x0..=x1 {
                    sum += src[[yy, xx]];
                    cnt += 1.0;
                }
            }
            filtered[[y, x]] = if cnt > 0.0 { sum / cnt } else { src[[y, x]] };
        }
    }

    let decimated = decimate(&filtered, ht_config.grid_size_m as usize);
    Ok(quantize_with_error_diffusion(
        &decimated,
        ht_config.quant_levels_n,
        ht_config.sharpening_l,
        ht_config.grid_size_m,
    ))
}

pub fn encode_halftone_pdf_split_auto_from_grayscale(
    gray: &[u8],
    width: u32,
    height: u32,
    config: &Jbig2Config,
    region_params: &GenericRegionParams,
    base_segment_number: u32,
    page_number: Option<u32>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let quantized = quantized_from_grayscale_input(gray, width, height, config)?;
    let ht_config = &config.halftone;

    let pattern_dictionary = generate_pattern_dictionary(
        ht_config.grid_size_m as usize,
        ht_config.quant_levels_n as usize,
    );
    let dict_template = if ht_config.dict_mmr {
        0
    } else {
        ht_config.template
    };
    let dict_payload =
        encode_pattern_dictionary_payload(&pattern_dictionary, dict_template, ht_config.dict_mmr)?;

    let effective_template = if ht_config.gray_mmr {
        0
    } else {
        ht_config.template
    };
    let gray_image_payload = if ht_config.gray_mmr {
        encode_grayscale_image_mmr(&quantized)?
    } else {
        encode_grayscale_image_annex_c(&quantized, effective_template)?
    };

    let page_num = page_number.unwrap_or(1);
    let page_info = Segment {
        number: base_segment_number + 1,
        seg_type: SegmentType::PageInformation,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 0,
        referred_to: vec![],
        page: Some(page_num),
        payload: PageInfo {
            width: region_params.width,
            height: region_params.height,
            xres: config.generic.dpi,
            yres: config.generic.dpi,
            default_pixel: false,
            ..Default::default()
        }
        .to_bytes(),
    };

    let ht_params = HalftoneParams {
        width: region_params.width,
        height: region_params.height,
        x: region_params.x,
        y: region_params.y,
        region_comb_operator: region_params.comb_operator,
        mmr: ht_config.gray_mmr,
        skip_enabled: false,
        halftone_comb_operator: 0,
        default_pixel: false,
        grid_width: quantized.ncols() as u32,
        grid_height: quantized.nrows() as u32,
        grid_x: (region_params.x as i32) * 256,
        grid_y: (region_params.y as i32) * 256,
        grid_vector_x: ht_config.grid_size_m as u16 * 256,
        grid_vector_y: 0,
        template: effective_template,
        ..Default::default()
    };

    let mut halftone_payload = ht_params.to_bytes();
    halftone_payload.extend(gray_image_payload);

    let pattern_dict = Segment {
        number: base_segment_number,
        seg_type: SegmentType::PatternDictionary,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 0,
        referred_to: Vec::new(),
        page: Some(page_num),
        payload: dict_payload,
    };
    let halftone_region = Segment {
        number: base_segment_number + 2,
        seg_type: if ht_config.lossless {
            SegmentType::ImmediateLosslessHalftoneRegion
        } else {
            SegmentType::ImmediateHalftoneRegion
        },
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 0,
        referred_to: vec![base_segment_number],
        page: Some(page_num),
        payload: halftone_payload,
    };

    let mut globals = Vec::new();
    let mut dict_segment = pattern_dict;
    dict_segment.page = None;
    dict_segment.page_association_type = 2;
    dict_segment.write_into(&mut globals)?;

    let mut page_stream = Vec::new();
    page_info.write_into(&mut page_stream)?;
    halftone_region.write_into(&mut page_stream)?;

    Ok((globals, page_stream))
}

/// Encodes a standalone one-page JBIG2 document containing a halftone region.
pub fn encode_halftone_document_auto(
    image: &BitImage,
    config: &Jbig2Config,
    region_params: &GenericRegionParams,
    base_segment_number: u32,
    page_number: Option<u32>,
) -> Result<Vec<u8>> {
    let mut document = FileHeader {
        organisation_type: true,
        unknown_n_pages: false,
        n_pages: 1,
    }
    .to_bytes();
    document.extend(encode_halftone_region_stream_auto(
        image,
        config,
        region_params,
        base_segment_number,
        page_number,
    )?);
    Segment {
        number: base_segment_number + 4,
        seg_type: SegmentType::EndOfFile,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 2,
        referred_to: vec![],
        page: None,
        payload: vec![],
    }
    .write_into(&mut document)?;
    Ok(document)
}

/// Runs the JBIG2 halftone pipeline and reconstructs a bilevel image by tiling
/// generated dictionary patterns according to quantized gray levels.
pub fn halftone_bilevel_preview(image: &BitImage, config: &Jbig2Config) -> Result<BitImage> {
    let ht_config = &config.halftone;
    if ht_config.grid_size_m == 0 {
        return Err(anyhow!("halftone grid_size_m must be > 0"));
    }

    let filtered_image = prefilter(image);
    let decimated_image = decimate(&filtered_image, ht_config.grid_size_m as usize);
    let quantized_gray_image = quantize_with_error_diffusion(
        &decimated_image,
        ht_config.quant_levels_n,
        ht_config.sharpening_l,
        ht_config.grid_size_m,
    );
    let pattern_dictionary = generate_pattern_dictionary(
        ht_config.grid_size_m as usize,
        ht_config.quant_levels_n as usize,
    );

    let cell = ht_config.grid_size_m as usize;
    let mut out = BitImage::new(usize_to_u32(image.width), usize_to_u32(image.height))
        .map_err(|e| anyhow!(e))?;
    for gy in 0..quantized_gray_image.nrows() {
        for gx in 0..quantized_gray_image.ncols() {
            let level = quantized_gray_image[[gy, gx]] as usize;
            let pattern = pattern_dictionary
                .get(level)
                .ok_or_else(|| anyhow!("Quantized level {} out of pattern range", level))?;
            for py in 0..cell {
                for px in 0..cell {
                    let ox = gx * cell + px;
                    let oy = gy * cell + py;
                    if ox < image.width && oy < image.height {
                        out.set(
                            usize_to_u32(ox),
                            usize_to_u32(oy),
                            pattern.get_usize(px, py),
                        );
                    }
                }
            }
        }
    }

    Ok(out)
}

/// Step 1: Convert the input bilevel image to a grayscale working image.
/// We intentionally avoid blurring here because this encoder is also used
/// on sharp non-halftone artwork where a prefilter destroys edge fidelity.
fn prefilter(image: &BitImage) -> Array2<f32> {
    let (w, h) = (image.width, image.height);
    let mut out = Array2::<f32>::zeros((h, w));

    for y in 0..h {
        for x in 0..w {
            out[[y, x]] = if image.get_usize(x, y) { 1.0 } else { 0.0 };
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

/// Step 3: Quantizes to N levels using a serpentine Floyd-Steinberg pass.
/// This avoids the large directional noise introduced by the previous global
/// sharpening term while still preserving tone transitions.
fn quantize_with_error_diffusion(
    decimated_image: &Array2<f32>,
    n: u32,
    _l: f32,
    m: u32,
) -> Array2<u8> {
    let (h, w) = decimated_image.dim();
    let mut out = Array2::<u8>::zeros((h, w));
    let mut temp_image = decimated_image.mapv(|v| v * (n - 1) as f32 / (m * m) as f32);

    for y in 0..h {
        let left_to_right = y % 2 == 0;
        let xs: Box<dyn Iterator<Item = usize>> = if left_to_right {
            Box::new(0..w)
        } else {
            Box::new((0..w).rev())
        };

        for x in xs {
            let pixel_val = temp_image[[y, x]];
            let quantized_val = pixel_val.round().clamp(0.0, (n - 1) as f32);
            out[[y, x]] = quantized_val as u8;

            let error = pixel_val - quantized_val;

            if left_to_right {
                if x + 1 < w {
                    temp_image[[y, x + 1]] += error * 7.0 / 16.0;
                }
                if y + 1 < h {
                    if x > 0 {
                        temp_image[[y + 1, x - 1]] += error * 3.0 / 16.0;
                    }
                    temp_image[[y + 1, x]] += error * 5.0 / 16.0;
                    if x + 1 < w {
                        temp_image[[y + 1, x + 1]] += error * 1.0 / 16.0;
                    }
                }
            } else {
                if x > 0 {
                    temp_image[[y, x - 1]] += error * 7.0 / 16.0;
                }
                if y + 1 < h {
                    if x + 1 < w {
                        temp_image[[y + 1, x + 1]] += error * 3.0 / 16.0;
                    }
                    temp_image[[y + 1, x]] += error * 5.0 / 16.0;
                    if x > 0 {
                        temp_image[[y + 1, x - 1]] += error * 1.0 / 16.0;
                    }
                }
            }
        }
    }
    out
}

/// Step 4: Generates an ideal pattern dictionary with N patterns of size M x M.
/// The spec only requires a fixed-size dictionary of patterns; it does not
/// require Bayer ordering. We use a clustered, center-weighted fill order to
/// reduce visible screen texture and keep shapes more contiguous on e-ink.
fn generate_pattern_dictionary(m: usize, n: usize) -> Vec<BitImage> {
    let mut dict = Vec::with_capacity(n);
    let fill_order = clustered_fill_order(m);

    for i in 0..n {
        let mut pattern = BitImage::new(usize_to_u32(m), usize_to_u32(m)).unwrap();

        let on_pixels = if n <= 1 {
            0
        } else {
            (i * m * m + (n - 2)) / (n - 1)
        };
        for &(x, y) in fill_order.iter().take(on_pixels.min(fill_order.len())) {
            pattern.set(usize_to_u32(x), usize_to_u32(y), true);
        }
        dict.push(pattern);
    }
    dict
}

fn clustered_fill_order(m: usize) -> Vec<(usize, usize)> {
    let center = (m as f32 - 1.0) / 2.0;
    let mut coords = Vec::with_capacity(m * m);
    for y in 0..m {
        for x in 0..m {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let radius = dx * dx + dy * dy;
            let angle = dy.atan2(dx);
            coords.push((x, y, radius, angle));
        }
    }

    coords.sort_by(|a, b| {
        a.2.partial_cmp(&b.2)
            .unwrap()
            .then_with(|| a.3.partial_cmp(&b.3).unwrap())
    });

    coords.into_iter().map(|(x, y, _, _)| (x, y)).collect()
}

/// Encodes the pattern dictionary by concatenating all patterns into a single
/// large bitmap and using the generic region encoder.
fn encode_pattern_dictionary_payload(
    patterns: &[BitImage],
    template: u8,
    mmr: bool,
) -> Result<Vec<u8>> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let p_h = patterns[0].height;
    let p_w = patterns[0].width;
    let total_width = p_w * patterns.len();

    // Create a single large bitmap to hold all patterns
    let mut collective_bitmap =
        BitImage::new(usize_to_u32(total_width), usize_to_u32(p_h)).map_err(|e| anyhow!(e))?;

    for (i, pattern) in patterns.iter().enumerate() {
        let x_offset = i * p_w;
        for y in 0..p_h {
            for x in 0..p_w {
                if pattern.get_usize(x, y) {
                    collective_bitmap.set(usize_to_u32(x_offset + x), usize_to_u32(y), true);
                }
            }
        }
    }

    let params = PatternDictionaryParams {
        mmr,
        template,
        pattern_width: p_w as u8,
        pattern_height: p_h as u8,
        gray_max: (patterns.len() - 1) as u32,
    };

    let mut payload = params.to_bytes();
    if mmr {
        payload.extend(encode_bitmap_mmr(&collective_bitmap)?);
    } else {
        let at_pixels = if template == 0 {
            vec![(-(p_w as i8), 0), (-3, -1), (2, -2), (-2, -2)]
        } else {
            vec![(-(p_w as i8), 0)]
        };
        payload.extend(Jbig2ArithCoder::encode_generic_payload(
            &collective_bitmap,
            template,
            &at_pixels,
        )?);
    }
    Ok(payload)
}

/// Encodes the grayscale image according to JBIG2 Annex C.
/// This involves bitplane decomposition, Gray coding, and arithmetic coding of each plane.
fn encode_grayscale_image_annex_c(gray_image: &Array2<u8>, template: u8) -> Result<Vec<u8>> {
    let bitplanes = gray_coded_bitplanes(gray_image);
    let (h, w) = gray_image.dim();

    let mut coder = Jbig2ArithCoder::new();
    let mut encoded_payload = Vec::new();

    // 2. Gray-code and encode each bitplane
    for plane_to_encode in bitplanes.iter().rev() {
        // Encode the (potentially modified) bitplane using the generic region coder
        // The AT pixels for halftone bitplanes are fixed by the spec.
        let at_pixels = match template {
            0 | 1 => [(3, -1), (-3, -1), (2, -2), (-2, -2)],
            _ => [(2, -1), (-3, -1), (2, -2), (-2, -2)],
        };
        let packed_data = plane_to_encode.packed_words();
        coder.encode_generic_region_inner(packed_data, w, h, template, &at_pixels)?;

        // NOTE: The standard implies one continuous arithmetic stream for all bitplanes.
        // We will flush once at the end.
    }

    coder.flush(true);
    encoded_payload.extend(coder.as_bytes());

    Ok(encoded_payload)
}

fn encode_grayscale_image_mmr(gray_image: &Array2<u8>) -> Result<Vec<u8>> {
    let bitplanes = gray_coded_bitplanes(gray_image);
    let mut out = Vec::new();

    for plane in bitplanes.iter().rev() {
        out.extend(encode_bitmap_mmr(plane)?);
    }

    Ok(out)
}

fn encode_bitmap_mmr(image: &BitImage) -> Result<Vec<u8>> {
    let width = u16::try_from(image.width)
        .map_err(|_| anyhow!("bitmap width too large for fax encoder"))?;
    let mut encoder = FaxEncoder::new(FaxVecWriter::new());

    for y in 0..image.height {
        let row = (0..image.width).map(|x| {
            if image.get_usize(x, y) {
                FaxColor::Black
            } else {
                FaxColor::White
            }
        });
        encoder
            .encode_line(row, width)
            .map_err(|e| anyhow!("fax encoder failed: {:?}", e))?;
    }

    let writer = encoder
        .finish()
        .map_err(|e| anyhow!("fax encoder finalization failed: {:?}", e))?;
    Ok(writer.finish())
}

fn gray_coded_bitplanes(gray_image: &Array2<u8>) -> Vec<BitImage> {
    let (h, w) = gray_image.dim();
    let max_val = gray_image.iter().max().copied().unwrap_or(0);
    let num_bits = if max_val == 0 {
        1
    } else {
        (max_val as f32 + 1.0).log2().ceil() as usize
    };

    let mut raw_bitplanes: Vec<BitImage> = (0..num_bits)
        .map(|_| BitImage::new(usize_to_u32(w), usize_to_u32(h)).unwrap())
        .collect();

    for y in 0..h {
        for x in 0..w {
            let val = gray_image[[y, x]];
            for b in 0..num_bits {
                if (val >> b) & 1 == 1 {
                    raw_bitplanes[b].set(usize_to_u32(x), usize_to_u32(y), true);
                }
            }
        }
    }

    let mut gray_planes: Vec<BitImage> = raw_bitplanes.clone();
    for i in 0..num_bits.saturating_sub(1) {
        for y in 0..h {
            for x in 0..w {
                let current_bit = raw_bitplanes[i].get_usize(x, y);
                let higher_bit = raw_bitplanes[i + 1].get_usize(x, y);
                gray_planes[i].set(
                    usize_to_u32(x),
                    usize_to_u32(y),
                    current_bit ^ higher_bit,
                );
            }
        }
    }

    gray_planes
}
