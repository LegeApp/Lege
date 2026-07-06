use anyhow::Result;
use image::{GrayImage, Luma, RgbImage};
use lege_gpu::binarization::{AdaptiveBinarizeGpuConstants, BinarizationMode, BinarizationParams};

/// Build a segmentation-only binary page mask with guaranteed 0=ink and
/// 255=background values.
///
/// Callers may pass either a binary page mask or a grayscale page buffer. This
/// deliberately normalizes the input instead of trusting output-format buffers,
/// because JPEG text output uses grayscale bytes rather than a binary mask.
pub fn build_analysis_binary(source: &[u8], image: &RgbImage) -> Vec<u8> {
    let expected = image.width() as usize * image.height() as usize;
    let gray;
    let pixels = if source.len() == expected {
        source
    } else {
        gray = rgb_to_gray(image);
        &gray
    };

    if pixels.is_empty() || pixels.iter().all(|&p| p == pixels[0]) {
        return vec![255; expected];
    }

    if pixels.iter().all(|&p| p == 0 || p == 255) {
        let mut binary = pixels.to_vec();
        standardize_polarity(&mut binary);
        return binary;
    }
    if pixels.iter().all(|&p| p <= 1) {
        let mut binary: Vec<u8> = pixels
            .iter()
            .map(|&p| if p == 0 { 0 } else { 255 })
            .collect();
        standardize_polarity(&mut binary);
        return binary;
    }

    let mut binary = otsu_binarize(pixels);
    standardize_polarity(&mut binary);
    binary
}

/// Binarize a full RGB page into a segmentation mask (0=ink, 255=background)
/// using Sauvola adaptive thresholding (GPU) with an Otsu CPU fallback.
///
/// Used when OCR runs on a freshly rendered high-resolution raster that has no
/// accompanying binary buffer from the main pipeline.
pub fn binarize_page(image: &RgbImage) -> Vec<u8> {
    let gray = rgb_to_gray(image);
    let mut binary = binarize_gray_region_with_fallback(&gray, image.width(), image.height());
    standardize_polarity(&mut binary);
    binary
}

fn rgb_to_gray(image: &RgbImage) -> Vec<u8> {
    image
        .pixels()
        .map(|px| {
            (0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32).round() as u8
        })
        .collect()
}

fn standardize_polarity(binary: &mut [u8]) {
    if binary.is_empty() {
        return;
    }
    let ink = binary.iter().filter(|&&p| p == 0).count();
    let background = binary.len() - ink;

    // A document analysis mask should not normally be mostly ink. This also
    // handles white text on a dark page without changing dense normal pages.
    if ink > background.saturating_mul(2) && background > 0 {
        for p in binary {
            *p = if *p == 0 { 255 } else { 0 };
        }
    }
}

/// Extract a grayscale sub-image from a high-res RGB page image.
///
/// Returns a GrayImage of the region, or an error if the bbox is out of bounds
/// or degenerate.
pub fn extract_gray_crop(image: &RgbImage, bbox: [u32; 4]) -> Result<(GrayImage, u32, u32)> {
    let [x1, y1, x2, y2] = bbox;
    let iw = image.width();
    let ih = image.height();

    if x2 <= x1 || y2 <= y1 || x2 > iw || y2 > ih {
        anyhow::bail!("invalid crop bbox [{x1},{y1},{x2},{y2}] for image {iw}x{ih}");
    }

    let w = x2 - x1;
    let h = y2 - y1;
    let mut gray = GrayImage::new(w, h);

    for cy in 0..h {
        for cx in 0..w {
            let px = image.get_pixel(x1 + cx, y1 + cy);
            // BT.709 luma
            let luma = (0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32)
                .round() as u8;
            gray.put_pixel(cx, cy, Luma([luma]));
        }
    }
    Ok((gray, w, h))
}

/// Extract a flat grayscale byte slice from a GrayImage (borrows the underlying buffer).
pub fn gray_image_to_flat(img: &GrayImage) -> &[u8] {
    img.as_raw()
}

/// Binarize a grayscale region using Sauvola adaptive thresholding via lege-gpu.
///
/// `gray`: flat grayscale buffer (1 byte per pixel)
/// Returns a binary buffer (0 = ink, 255 = background) of the same size,
/// or `None` if GPU binarization fails.
pub fn binarize_gray_region(gray: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let params = sauvola_params(width, height);
    lege_gpu::binarization::try_binarize_gray(gray, &params)
}

/// Binarize using GPU; fall back to simple Otsu-style fixed-threshold CPU binarization.
pub fn binarize_gray_region_with_fallback(gray: &[u8], width: u32, height: u32) -> Vec<u8> {
    if let Some(result) = binarize_gray_region(gray, width, height) {
        return result;
    }
    // CPU fallback: Otsu threshold
    otsu_binarize(gray)
}

/// Extract and binarize a region from an existing page-level binary buffer.
///
/// This is the fast path for line segmentation: the page is already binarized,
/// so we just extract the sub-region.
pub fn extract_binary_crop(
    binary: &[u8],
    image_width: u32,
    image_height: u32,
    bbox: [u32; 4],
) -> Result<(Vec<u8>, u32, u32)> {
    let [x1, y1, x2, y2] = bbox;
    let iw = image_width as usize;
    let ih = image_height as usize;

    if x2 <= x1 || y2 <= y1 || x2 > image_width || y2 > image_height {
        anyhow::bail!("invalid binary crop bbox [{x1},{y1},{x2},{y2}] for {iw}x{ih}");
    }

    let w = (x2 - x1) as usize;
    let h = (y2 - y1) as usize;
    let mut out = Vec::with_capacity(w * h);

    for row in 0..h {
        let abs_y = y1 as usize + row;
        let start = abs_y * iw + x1 as usize;
        let end = start + w;
        if end > binary.len() {
            break;
        }
        out.extend_from_slice(&binary[start..end]);
    }
    Ok((out, x2 - x1, y2 - y1))
}

/// Tighten a page-global bbox around binary ink and retain a conservative
/// amount of surrounding whitespace for OCR.
pub fn tighten_bbox_to_ink(
    binary: &[u8],
    image_width: u32,
    image_height: u32,
    bbox: [u32; 4],
) -> Option<[u32; 4]> {
    let [x1, y1, x2, y2] = bbox;
    if x2 <= x1
        || y2 <= y1
        || x2 > image_width
        || y2 > image_height
        || binary.len() < image_width as usize * image_height as usize
    {
        return None;
    }

    let mut min_x = x2;
    let mut min_y = y2;
    let mut max_x = x1;
    let mut max_y = y1;
    let mut found = false;
    let stride = image_width as usize;

    for y in y1..y2 {
        let row = y as usize * stride;
        for x in x1..x2 {
            if binary[row + x as usize] == 0 {
                found = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }

    if !found {
        return None;
    }

    let ink_height = max_y.saturating_sub(min_y) + 1;
    let pad_x = (ink_height / 2).max(4);
    let pad_y = (ink_height / 4).max(2);
    Some([
        min_x.saturating_sub(pad_x).max(x1),
        min_y.saturating_sub(pad_y).max(y1),
        (max_x + 1 + pad_x).min(x2),
        (max_y + 1 + pad_y).min(y2),
    ])
}

/// Enlarge small line crops before OCR so character strokes occupy enough
/// pixels for the recognizer. Returns the prepared image and factors that map
/// prepared-image coordinates back to the original crop.
pub fn prepare_line_for_ocr(image: &GrayImage, binary: bool) -> (GrayImage, f32, f32) {
    const TARGET_MIN_HEIGHT: u32 = 48;
    const MAX_SCALE: u32 = 4;

    if image.width() == 0 || image.height() == 0 || image.height() >= TARGET_MIN_HEIGHT {
        return (image.clone(), 1.0, 1.0);
    }

    let scale = TARGET_MIN_HEIGHT
        .div_ceil(image.height())
        .clamp(1, MAX_SCALE);
    if scale == 1 {
        return (image.clone(), 1.0, 1.0);
    }

    let new_width = image.width().saturating_mul(scale).max(1);
    let new_height = image.height().saturating_mul(scale).max(1);
    let filter = if binary {
        image::imageops::FilterType::Nearest
    } else {
        image::imageops::FilterType::CatmullRom
    };
    let prepared = image::imageops::resize(image, new_width, new_height, filter);
    (
        prepared,
        image.width() as f32 / new_width as f32,
        image.height() as f32 / new_height as f32,
    )
}

fn sauvola_params(width: u32, height: u32) -> BinarizationParams {
    BinarizationParams {
        width,
        height,
        mode: BinarizationMode::Adaptive,
        invert_output: false,
        k_factor: Legencode::DEFAULT_K_FACTOR,
        fixed_threshold: 128,
        adaptive: AdaptiveBinarizeGpuConstants {
            sauvola_window: 31,
            bg_window: 3,
            percentile_c: 128,
            otsu_threshold: 128,
        },
        debug_mode: 0,
    }
}

/// Simple Otsu CPU binarization (0 = ink, 255 = background).
fn otsu_binarize(gray: &[u8]) -> Vec<u8> {
    if gray.is_empty() {
        return Vec::new();
    }
    let mut hist = [0u64; 256];
    for &p in gray {
        hist[p as usize] += 1;
    }
    let total = gray.len() as f64;
    let mut best_t = 128u8;
    let mut best_var = 0.0f64;
    let mut w0 = 0.0f64;
    let mut sum0 = 0.0f64;
    let sum_all: f64 = (0..256).map(|i| i as f64 * hist[i] as f64).sum();
    for (t, &count) in hist.iter().enumerate() {
        w0 += count as f64 / total;
        sum0 += t as f64 * count as f64;
        let w1 = 1.0 - w0;
        if w0 == 0.0 || w1 == 0.0 {
            continue;
        }
        let mu0 = sum0 / (w0 * total);
        let mu1 = (sum_all - sum0) / (w1 * total);
        let var = w0 * w1 * (mu0 - mu1) * (mu0 - mu1);
        if var > best_var {
            best_var = var;
            best_t = t as u8;
        }
    }
    gray.iter()
        .map(|&p| if p <= best_t { 0 } else { 255 })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_gray_crop_basic() {
        let mut rgb = RgbImage::new(10, 10);
        // Fill with a recognizable value
        for px in rgb.pixels_mut() {
            *px = image::Rgb([200, 200, 200]);
        }
        let (gray, w, h) = extract_gray_crop(&rgb, [2, 2, 8, 8]).unwrap();
        assert_eq!(w, 6);
        assert_eq!(h, 6);
        assert_eq!(gray.width(), 6);
        assert_eq!(gray.height(), 6);
    }

    #[test]
    fn extract_binary_crop_basic() {
        let binary = vec![255u8; 100]; // 10x10, all background
        let (crop, w, h) = extract_binary_crop(&binary, 10, 10, [2, 2, 8, 8]).unwrap();
        assert_eq!(w, 6);
        assert_eq!(h, 6);
        assert_eq!(crop.len(), 36);
    }

    #[test]
    fn otsu_splits_bimodal() {
        let mut gray = vec![0u8; 128]; // dark
        gray.extend(vec![255u8; 128]); // light
        let bin = otsu_binarize(&gray);
        assert!(bin[..128].iter().all(|&v| v == 0));
        assert!(bin[128..].iter().all(|&v| v == 255));
    }

    #[test]
    fn analysis_binary_binarizes_jpeg_style_luma() {
        let image = RgbImage::from_pixel(4, 1, image::Rgb([255, 255, 255]));
        let source = vec![1, 1, 250, 255];
        let binary = build_analysis_binary(&source, &image);
        assert_eq!(binary, vec![0, 0, 255, 255]);
    }

    #[test]
    fn analysis_binary_normalizes_zero_one_mask() {
        let image = RgbImage::from_pixel(4, 1, image::Rgb([255, 255, 255]));
        let binary = build_analysis_binary(&[0, 1, 1, 0], &image);
        assert_eq!(binary, vec![0, 255, 255, 0]);
    }

    #[test]
    fn tighten_bbox_keeps_ink_with_whitespace_padding() {
        let mut binary = vec![255; 20 * 20];
        for y in 8..12 {
            for x in 7..13 {
                binary[y * 20 + x] = 0;
            }
        }
        let bbox = tighten_bbox_to_ink(&binary, 20, 20, [0, 0, 20, 20]).unwrap();
        assert_eq!(bbox, [3, 6, 17, 14]);
    }

    #[test]
    fn line_preparation_upscales_small_crops_and_reports_inverse_scale() {
        let image = GrayImage::from_pixel(20, 12, Luma([255]));
        let (prepared, scale_x, scale_y) = prepare_line_for_ocr(&image, false);
        assert_eq!(prepared.dimensions(), (80, 48));
        assert_eq!(scale_x, 0.25);
        assert_eq!(scale_y, 0.25);
    }
}
