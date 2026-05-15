// src/ocr/winocr.rs
// Windows OCR implementation using Windows Runtime APIs

use super::OcrResult;
use windows::Graphics::Imaging::{BitmapBufferAccessMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Win32::System::WinRT::IMemoryBufferByteAccess;
use windows_core;

pub fn run_winocr(data: &[u8], width: usize, height: usize, is_binary: bool) -> Option<OcrResult> {
    // Initialize COM if not already done (required for Windows Runtime)
    let _com_guard = match initialize_com() {
        Ok(guard) => guard,
        Err(_e) => {
            return None;
        }
    };

    // Single attempt with immediate success/failure - no retries
    match run_winocr_impl(data, width, height, is_binary) {
        Some(result) => Some(result),
        None => {
            // Return empty OCR result instead of None to avoid failing the pipeline
            Some(create_empty_ocr_result(width, height))
        }
    }
}

fn run_winocr_impl(data: &[u8], width: usize, height: usize, is_binary: bool) -> Option<OcrResult> {
    use windows::Media::Ocr::OcrResult as WinOcrResult;

    // Validate input with bounds checking
    if width == 0 || height == 0 || width > 65535 || height > 65535 {
        return None;
    }

    // Memory optimization: downscale large images by total pixel count
    const MAX_OCR_PIXELS: usize = 1_500_000; // ~1200x1250
    let current_pixels = width * height;

    // Work directly with raw pixel data - no need for image crate conversion

    // Apply memory optimization if needed using existing optimized downscaling
    let (processed_data, final_width, final_height) = if current_pixels > MAX_OCR_PIXELS {
        let scale_factor = (MAX_OCR_PIXELS as f64 / current_pixels as f64).sqrt();
        let new_width = ((width as f64) * scale_factor) as u32;
        let new_height = ((height as f64) * scale_factor) as u32;

        match downscale_image_bell(
            data,
            width as usize,
            height as usize,
            new_width as usize,
            new_height as usize,
            is_binary,
        ) {
            Some(downscaled_data) => (downscaled_data, new_width as u32, new_height as u32),
            None => {
                return None;
            }
        }
    } else {
        (data.to_vec(), width as u32, height as u32)
    };

    // Convert raw data to BGRA format for Windows APIs
    let bgra_bytes = if is_binary {
        // Convert binary (0/1) to BGRA (0/255 for each channel)
        let mut bgra_data = Vec::with_capacity(processed_data.len() * 4);
        for &pixel in &processed_data {
            let value = if pixel == 0 { 0u8 } else { 255u8 };
            bgra_data.extend_from_slice(&[value, value, value, 255]); // BGRA
        }
        bgra_data
    } else if processed_data.len() == (final_width * final_height * 3) as usize {
        // RGB to BGRA conversion
        let mut bgra_data = Vec::with_capacity(processed_data.len() / 3 * 4);
        for chunk in processed_data.chunks_exact(3) {
            bgra_data.extend_from_slice(&[chunk[2], chunk[1], chunk[0], 255]); // BGR + A
        }
        bgra_data
    } else {
        // Assume already BGRA or grayscale
        if processed_data.len() == (final_width * final_height) as usize {
            // Grayscale to BGRA
            let mut bgra_data = Vec::with_capacity(processed_data.len() * 4);
            for &pixel in &processed_data {
                bgra_data.extend_from_slice(&[pixel, pixel, pixel, 255]);
            }
            bgra_data
        } else {
            processed_data
        }
    };

    // Create SoftwareBitmap directly
    let bitmap = match SoftwareBitmap::Create(
        BitmapPixelFormat::Bgra8,
        final_width as i32,
        final_height as i32,
    ) {
        Ok(bitmap) => {
            // Lock the bitmap buffer for writing
            if let Ok(buffer) = bitmap.LockBuffer(BitmapBufferAccessMode::Write) {
                if let Ok(buffer_ref) = buffer.CreateReference() {
                    // Copy BGRA data into the buffer
                    unsafe {
                        let mut ptr: *mut u8 = std::ptr::null_mut();
                        let mut capacity: u32 = 0;

                        // Try to get buffer access
                        if let Ok(byte_access) =
                            windows_core::Interface::cast::<IMemoryBufferByteAccess>(&buffer_ref)
                        {
                            if byte_access.GetBuffer(&mut ptr, &mut capacity).is_ok() {
                                if !ptr.is_null() && capacity >= bgra_bytes.len() as u32 {
                                    std::ptr::copy_nonoverlapping(
                                        bgra_bytes.as_ptr(),
                                        ptr,
                                        bgra_bytes.len(),
                                    );
                                } else {
                                    return None;
                                }
                            } else {
                                return None;
                            }
                        } else {
                            return None;
                        }
                    }
                } else {
                    return None;
                }
            } else {
                return None;
            }

            bitmap
        }
        Err(e) => {
            return None;
        }
    };

    // Create OCR engine with fallback options
    let engine = match create_ocr_engine() {
        Some(e) => e,
        None => {
            return None;
        }
    };

    // Run OCR with immediate success/failure
    let ocr_result: WinOcrResult = match run_ocr_immediate(&engine, &bitmap) {
        Some(result) => result,
        None => {
            return None;
        }
    };

    // Extract lines
    let lines = match ocr_result.Lines() {
        Ok(l) => l,
        Err(e) => {
            return None;
        }
    };

    let line_count = lines.Size().unwrap_or(0);

    // Build HOCR and plain text
    let mut plain_text = String::new();
    let mut word_count = 0;
    // Calculate scale factors for converting back to original coordinates
    let x_scale = width as f32 / final_width as f32;
    let y_scale = height as f32 / final_height as f32;

    let mut hocr = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Transitional//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd">
<html xmlns="http://www.w3.org/1999/xhtml">
<head><meta http-equiv="Content-Type" content="text/html; charset=UTF-8"/></head>
<body>
<div class="ocr_page" title="bbox 0 0 {} {}; image 'embedded'">
<div class="ocr_carea" title="bbox 0 0 {} {}">"#,
        width, height, width, height
    );

    // Process each line with error recovery
    for (line_idx, line) in lines.into_iter().enumerate() {
        let words = match line.Words() {
            Ok(w) => w,
            Err(e) => {
                continue;
            }
        };

        let word_count_in_line = words.Size().unwrap_or(0);
        if word_count_in_line == 0 {
            continue;
        }

        // Start paragraph for this line
        hocr.push_str(r#"<p class="ocr_par">"#);

        // Track a union bounding box for the line while iterating words
        let mut line_min_x: i32 = i32::MAX;
        let mut line_min_y: i32 = i32::MAX;
        let mut line_max_x: i32 = i32::MIN;
        let mut line_max_y: i32 = i32::MIN;

        // Process words in line with safer extraction
        for word in words.into_iter() {
            let text = match word.Text() {
                Ok(t) => {
                    let text_str = t.to_string();
                    // Validate text contains printable characters
                    if text_str
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
                    {
                        continue;
                    }
                    text_str
                }
                Err(e) => {
                    continue;
                }
            };

            let rect = match word.BoundingRect() {
                Ok(r) => {
                    // Validate bounding box is reasonable for the scaled image
                    if r.Width <= 0.0
                        || r.Height <= 0.0
                        || r.X < 0.0
                        || r.Y < 0.0
                        || r.X + r.Width > final_width as f32
                        || r.Y + r.Height > final_height as f32
                    {
                        continue;
                    }
                    r
                }
                Err(e) => {
                    continue;
                }
            };

            if !text.trim().is_empty() {
                word_count += 1;

                // Add to plain text
                plain_text.push_str(&text);
                plain_text.push(' ');

                // Scale bounding box back to original image coordinates
                let scaled_x = (rect.X * x_scale) as i32;
                let scaled_y = (rect.Y * y_scale) as i32;
                let scaled_x2 = ((rect.X + rect.Width) * x_scale) as i32;
                let scaled_y2 = ((rect.Y + rect.Height) * y_scale) as i32;

                // Add to HOCR with scaled bounding box
                hocr.push_str(&format!(
                    r#"<span class="ocrx_word" title="bbox {} {} {} {}">{}</span> "#,
                    scaled_x,
                    scaled_y,
                    scaled_x2,
                    scaled_y2,
                    html_escape(&text)
                ));

                // Update union bbox for the line
                line_min_x = line_min_x.min(scaled_x);
                line_min_y = line_min_y.min(scaled_y);
                line_max_x = line_max_x.max(scaled_x2);
                line_max_y = line_max_y.max(scaled_y2);
            }
        }

        // Emit a line-level span using OcrLine.Text and the union bbox
        if line_min_x != i32::MAX {
            let line_text = match line.Text() {
                Ok(t) => t.to_string(),
                Err(_) => String::new(),
            };
            if !line_text.trim().is_empty() {
                hocr.push_str(&format!(
                    r#"<span class="ocr_line" title="bbox {} {} {} {}">{}</span>"#,
                    line_min_x,
                    line_min_y,
                    line_max_x,
                    line_max_y,
                    html_escape(&line_text)
                ));
            }
        }

        // End paragraph and add line break
        hocr.push_str("</p>\n");
        if !plain_text.is_empty() && !plain_text.ends_with('\n') {
            plain_text.push('\n');
        }
    }

    hocr.push_str("</div></div></body></html>");

    let result = OcrResult {
        hocr,
        plain_text: plain_text.trim().to_string(),
    };

    // Explicit cleanup to help with memory management
    // bgra_bytes was already moved/consumed in bitmap creation

    Some(result)
}

// COM initialization guard
struct ComGuard;

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::System::Com::CoUninitialize();
        }
    }
}

// Initialize COM for Windows Runtime
fn initialize_com() -> Result<ComGuard, windows::core::Error> {
    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED
                | windows::Win32::System::Com::COINIT_DISABLE_OLE1DDE,
        )
        .ok()?;
    }
    Ok(ComGuard)
}

// Create OCR engine with fallback options
fn create_ocr_engine() -> Option<windows::Media::Ocr::OcrEngine> {
    use windows::Globalization::Language;
    use windows::Media::Ocr::OcrEngine;

    // Try 1: Create from user profile languages
    if let Ok(engine) = OcrEngine::TryCreateFromUserProfileLanguages() {
        return Some(engine);
    }

    // Try 2: Create for English specifically
    if let Ok(lang) = Language::CreateLanguage(&::windows::core::HSTRING::from("en")) {
        if let Ok(engine) = OcrEngine::TryCreateFromLanguage(&lang) {
            return Some(engine);
        }
    }

    // Try 3: Get available languages and use the first one
    if let Ok(available_languages) = OcrEngine::AvailableRecognizerLanguages() {
        if available_languages.Size().unwrap_or(0) > 0 {
            if let Ok(first_lang) = available_languages.GetAt(0) {
                if let Ok(engine) = OcrEngine::TryCreateFromLanguage(&first_lang) {
                    return Some(engine);
                }
            }
        }
    }

    None
}

// Run OCR with immediate success/failure (no timeouts)
fn run_ocr_immediate(
    engine: &windows::Media::Ocr::OcrEngine,
    bitmap: &windows::Graphics::Imaging::SoftwareBitmap,
) -> Option<windows::Media::Ocr::OcrResult> {
    // Start OCR operation
    let async_op = match engine.RecognizeAsync(bitmap) {
        Ok(op) => op,
        Err(e) => {
            return None;
        }
    };

    // Try to complete immediately - if it doesn't work right away, it likely won't work at all
    match async_op.join() {
        Ok(result) => Some(result),
        Err(e) => {
            let _ = async_op.Cancel();
            None
        }
    }
}

// (Removed Completed-handler helpers; using status polling above.)

// Create empty OCR result as fallback
fn create_empty_ocr_result(width: usize, height: usize) -> OcrResult {
    let hocr = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Transitional//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd">
<html xmlns="http://www.w3.org/1999/xhtml">
<head><meta http-equiv="Content-Type" content="text/html; charset=UTF-8"/></head>
<body>
<div class="ocr_page" title="bbox 0 0 {} {}; image 'embedded'">
<div class="ocr_carea" title="bbox 0 0 {} {}">
<p class="ocr_par"><!-- OCR failed, no text extracted --></p>
</div></div></body></html>"#,
        width, height, width, height
    );

    OcrResult {
        hocr,
        plain_text: String::new(),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// Temporary OCR downscale using hardware acceleration when available (HLSL/WGPU, CPU fallback).
fn downscale_image_bell(
    data: &[u8],
    orig_width: usize,
    orig_height: usize,
    new_width: usize,
    new_height: usize,
    is_binary: bool,
) -> Option<Vec<u8>> {
    let channels = if is_binary { 1 } else { 3 };

    // Use hardware-accelerated resize (HLSL on Windows, CPU fallback)
    let params = crate::resize::ResizeParams {
        target_width: new_width as u32,
        target_height: new_height as u32,
        method: crate::resize::ResizeMethod::Bell,
        letterbox: false,
        border_value: 0.0,
        swap_rb: false,
    };

    crate::resize::resize_bytes(
        data,
        orig_width as u32,
        orig_height as u32,
        &params,
        channels as u32,
    )
    .ok()
}
