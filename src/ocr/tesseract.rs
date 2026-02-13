use super::OcrResult;
use crate::resize::{ResizeMethod, ResizeParams};
use image::{DynamicImage, GrayImage, RgbImage};
use tesseract::{Tesseract, PageSegMode, OcrEngineMode};

pub fn run_tesseract(
    data: &[u8],
    width: usize,
    height: usize,
    is_binary: bool,
) -> Option<OcrResult> {
    // Input validation with bounds checking (similar to Windows OCR)
    if width == 0 || height == 0 || width > 65535 || height > 65535 {
        #[cfg(feature = "debug-logging")]
        println!("[DEBUG OCR] Invalid dimensions: {}x{}", width, height);
        return None;
    }

    let expected_len = if is_binary {
        width * height
    } else {
        width * height * 3
    };

    if data.len() != expected_len {
        #[cfg(feature = "debug-logging")]
        println!(
            "[DEBUG OCR] Data length mismatch: got {}, expected {} for {}x{} (is_binary: {})",
            data.len(),
            expected_len,
            width,
            height,
            is_binary
        );
        return None;
    }

    // Memory optimization: downscale large images to prevent Tesseract memory issues
    const MAX_OCR_PIXELS: usize = 2_000_000; // ~1400x1400 (more conservative than Windows)
    let current_pixels = width * height;
    let (final_width, final_height, scaled_data) = if current_pixels > MAX_OCR_PIXELS {
        let scale = (MAX_OCR_PIXELS as f64 / current_pixels as f64).sqrt();
        let new_width = ((width as f64) * scale).round().max(1.0) as usize;
        let new_height = ((height as f64) * scale).round().max(1.0) as usize;

        #[cfg(feature = "debug-logging")]
        println!(
            "[DEBUG OCR] Downscaling {}x{} to {}x{} (scale: {:.3})",
            width, height, new_width, new_height, scale
        );

        match downscale_image_lanczos3(data, width, height, new_width, new_height, is_binary) {
            Some(downscaled) => (new_width, new_height, downscaled),
            None => {
                #[cfg(feature = "debug-logging")]
                println!("[DEBUG OCR] Image downscaling failed, skipping OCR");
                return None;
            }
        }
    } else {
        (width, height, data.to_vec())
    };

    // Validate scaled data is not all zeros or all same value (likely corrupted)
    if scaled_data.iter().all(|&b| b == scaled_data[0]) {
        #[cfg(feature = "debug-logging")]
        println!(
            "[DEBUG OCR] Warning: Image data appears uniform (value: {}), OCR may not find text",
            scaled_data[0]
        );
        // Continue anyway - user might have intentionally blank image
    }

    // Configure Tesseract with builder pattern
    // tesseract crate uses set_frame(data, width, height, bytes_per_pixel, bytes_per_line)
    let bytes_per_pixel = if is_binary { 1 } else { 3 };
    let bytes_per_line = final_width * bytes_per_pixel;

    let mut tess = match Tesseract::new_with_oem(None, Some("eng"), OcrEngineMode::LstmOnly) {
        Ok(t) => t,
        Err(e) => {
            #[cfg(feature = "debug-logging")]
            println!("[DEBUG OCR] Failed to initialize Tesseract: {:?}", e);
            return None;
        }
    };

    // Set the image from raw frame data
    tess = match tess.set_frame(
        &scaled_data,
        final_width as i32,
        final_height as i32,
        bytes_per_pixel as i32,
        bytes_per_line as i32,
    ) {
        Ok(t) => t,
        Err(e) => {
            #[cfg(feature = "debug-logging")]
            println!("[DEBUG OCR] Failed to set image frame: {:?}", e);
            return None;
        }
    };

    // Set page segmentation mode (3 = Fully automatic page segmentation, but no OSD)
    tess.set_page_seg_mode(PageSegMode::PsmAuto);

    // Recognize the text
    tess = match tess.recognize() {
        Ok(t) => t,
        Err(e) => {
            #[cfg(feature = "debug-logging")]
            println!("[DEBUG OCR] Tesseract recognition failed: {:?}", e);
            return None;
        }
    };

    // Get HOCR output (page number is typically 0 for single-page images)
    let hocr = match tess.get_hocr_text(0) {
        Ok(h) => {
            #[cfg(feature = "debug-logging")]
            println!(
                "[DEBUG OCR] Tesseract HOCR completed successfully, {} characters",
                h.len()
            );
            h
        }
        Err(e) => {
            #[cfg(feature = "debug-logging")]
            println!("[DEBUG OCR] Tesseract HOCR failed: {:?}", e);
            return None;
        }
    };

    // Get plain text output
    let plain_text = match tess.get_text() {
        Ok(t) => {
            #[cfg(feature = "debug-logging")]
            println!("[DEBUG OCR] Tesseract plain text completed successfully");
            t
        }
        Err(e) => {
            #[cfg(feature = "debug-logging")]
            println!("[DEBUG OCR] Tesseract plain text failed: {:?}", e);
            String::new()
        }
    };

    // Scale HOCR coordinates back to original dimensions if we downscaled
    let final_hocr = if current_pixels > MAX_OCR_PIXELS {
        scale_hocr_coordinates(&hocr, final_width, final_height, width, height)
    } else {
        hocr
    };

    Some(OcrResult {
        hocr: final_hocr,
        plain_text,
    })
}

/// High-quality downscaling using hardware acceleration (HLSL on Windows, CPU fallback)
fn downscale_image_lanczos3(
    data: &[u8],
    orig_width: usize,
    orig_height: usize,
    new_width: usize,
    new_height: usize,
    is_binary: bool,
) -> Option<Vec<u8>> {
    let channels = if is_binary { 1 } else { 3 };
    let src_width = u32::try_from(orig_width).ok()?;
    let src_height = u32::try_from(orig_height).ok()?;
    let dst_width = u32::try_from(new_width).ok()?;
    let dst_height = u32::try_from(new_height).ok()?;

    let params = ResizeParams {
        target_width: dst_width,
        target_height: dst_height,
        method: ResizeMethod::Lanczos3,
        letterbox: false,
        border_value: 0.0,
        swap_rb: false,
    };

    crate::resize::resize_bytes(
        data,
        src_width,
        src_height,
        &params,
        channels as u32,
    )
    .ok()
}

/// Scale HOCR coordinates back to original image dimensions
fn scale_hocr_coordinates(
    hocr: &str,
    scaled_width: usize,
    scaled_height: usize,
    orig_width: usize,
    orig_height: usize,
) -> String {
    let x_scale = orig_width as f32 / scaled_width as f32;
    let y_scale = orig_height as f32 / scaled_height as f32;

    // Replace bbox coordinates using regex-like approach
    let mut result = String::with_capacity(hocr.len());
    let mut chars = hocr.chars();

    while let Some(ch) = chars.next() {
        if ch == 'b' {
            // Look for "bbox " pattern
            let lookahead: String = std::iter::once(ch)
                .chain(chars.as_str().chars().take(4))
                .collect();

            if lookahead == "bbox " {
                result.push_str("bbox ");
                // Skip "box " characters
                for _ in 0..4 {
                    chars.next();
                }

                // Parse and scale the four coordinates
                let remaining = chars.as_str();
                if let Some(end_pos) = remaining.find('"') {
                    let coords_str = &remaining[..end_pos];
                    let coords: Vec<&str> = coords_str.trim().split_whitespace().collect();

                    if coords.len() >= 4 {
                        if let (Ok(x1), Ok(y1), Ok(x2), Ok(y2)) = (
                            coords[0].parse::<f32>(),
                            coords[1].parse::<f32>(),
                            coords[2].parse::<f32>(),
                            coords[3].parse::<f32>(),
                        ) {
                            let scaled_x1 = (x1 * x_scale) as i32;
                            let scaled_y1 = (y1 * y_scale) as i32;
                            let scaled_x2 = (x2 * x_scale) as i32;
                            let scaled_y2 = (y2 * y_scale) as i32;

                            result.push_str(&format!(
                                "{} {} {} {}",
                                scaled_x1, scaled_y1, scaled_x2, scaled_y2
                            ));
                        } else {
                            result.push_str(coords_str);
                        }
                    } else {
                        result.push_str(coords_str);
                    }

                    // Skip to the position after the coordinates
                    for _ in 0..end_pos {
                        chars.next();
                    }
                    continue;
                }
            }
        }
        result.push(ch);
    }

    result
}
