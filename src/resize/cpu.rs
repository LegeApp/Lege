use crate::resize::{ResizeError, ResizeMethod, ResizeParams};
use fast_image_resize::{
    FilterType, IntoImageView, PixelType, ResizeAlg, ResizeOptions, Resizer, images::Image,
};
use image::{Rgb, RgbImage};
use rayon::prelude::*;

type Result<T> = std::result::Result<T, ResizeError>;

pub fn resize_batch_cpu<T>(
    batch: &[T],
    params: &ResizeParams,
) -> Result<Vec<RgbImage>>
where
    T: IntoImageView + Sync + Clone,
{
    if batch.is_empty() {
        return Err(ResizeError::EmptyInput);
    }

    let tw = params.target_width;
    let th = params.target_height;

    batch
        .par_iter()
        .map(|src| resize_single(src.clone(), params, tw, th))
        .collect()
}

fn fir_algorithm(m: ResizeMethod) -> ResizeAlg {
    match m {
        ResizeMethod::Nearest => ResizeAlg::Nearest,
        ResizeMethod::Bilinear => ResizeAlg::Convolution(FilterType::Bilinear),
        ResizeMethod::Bicubic => ResizeAlg::Convolution(FilterType::CatmullRom),
        ResizeMethod::Lanczos3 => ResizeAlg::Convolution(FilterType::Lanczos3),
    }
}

// Removed compute_letterbox function - no longer needed for PaddleX direct resize

pub fn resize_single<T>(
    src: T,
    params: &ResizeParams,
    tw: u32,
    th: u32,
) -> Result<RgbImage>
where
    T: IntoImageView,
{
    let _sw = src.width();
    let _sh = src.height();

    // Always do direct resize for PaddleX, ignore aspect ratio and padding
    let rw = tw;
    let rh = th;

    // Allocate destination buffer
    let mut dst = vec![0u8; (rw * rh * 3) as usize];
    let mut dst_img = Image::from_slice_u8(rw, rh, &mut dst, PixelType::U8x3)
        .map_err(|e| ResizeError::BackendError(format!("Failed to create dest image: {e:?}")))?;

    let mut resizer = Resizer::new();
    let resize_options = ResizeOptions::new().resize_alg(fir_algorithm(params.method));
    resizer
        .resize(&src, &mut dst_img, Some(&resize_options))
        .map_err(|e| ResizeError::BackendError(format!("Resize error: {e:?}")))?;

    // Direct resize - no letterboxing logic needed
    let mut out = dst.to_vec();

    if params.swap_rb {
        for px in out.chunks_exact_mut(3) {
            px.swap(0, 2);
        }
    }

    RgbImage::from_raw(tw, th, out).ok_or(ResizeError::InvalidDimensions)
}

/// Specialized resize function for OCR and Margin modules
///
/// This function provides a simple, direct interface for resizing images
/// specifically for OCR processing and margin analysis, using high-quality
/// filtering and optimized for text clarity.
///
/// # Arguments
/// * `src_data` - Source image data as RGB bytes
/// * `src_width` - Source image width
/// * `src_height` - Source image height  
/// * `target_width` - Target width for resizing
/// * `target_height` - Target height for resizing
/// * `preserve_aspect` - If true, maintains aspect ratio with letterboxing
///
/// # Returns
/// A Vec<u8> containing the resized RGB image data, or ResizeError on failure
pub fn resize_for_ocr(
    src_data: &[u8],
    src_width: u32,
    src_height: u32,
    target_width: u32,
    target_height: u32,
    preserve_aspect: bool,
) -> Result<Vec<u8>> {
    // Create a mutable copy for FIR to work with
    let mut src_copy = src_data.to_vec();

    // Create source image view
    let src_view = Image::from_slice_u8(src_width, src_height, &mut src_copy, PixelType::U8x3)
        .map_err(|e| ResizeError::BackendError(format!("Failed to create source image: {e:?}")))?;

    let (final_width, final_height) = if preserve_aspect {
        // Calculate dimensions preserving aspect ratio
        let src_aspect = src_width as f32 / src_height as f32;
        let target_aspect = target_width as f32 / target_height as f32;

        if src_aspect > target_aspect {
            // Source is wider, fit to width
            (target_width, (target_width as f32 / src_aspect) as u32)
        } else {
            // Source is taller, fit to height
            ((target_height as f32 * src_aspect) as u32, target_height)
        }
    } else {
        (target_width, target_height)
    };

    // Allocate destination buffer
    let mut dst = vec![0u8; (final_width * final_height * 3) as usize];
    let mut dst_img = Image::from_slice_u8(final_width, final_height, &mut dst, PixelType::U8x3)
        .map_err(|e| ResizeError::BackendError(format!("Failed to create dest image: {e:?}")))?;

    // Use Lanczos3 for high-quality text-oriented resizing
    let mut resizer = Resizer::new();
    let resize_options =
        ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));

    resizer
        .resize(&src_view, &mut dst_img, Some(&resize_options))
        .map_err(|e| ResizeError::BackendError(format!("Resize error: {e:?}")))?;

    if preserve_aspect && (final_width != target_width || final_height != target_height) {
        // Add letterboxing with white background for OCR clarity
        let mut letterboxed = vec![255u8; (target_width * target_height * 3) as usize]; // White background

        let offset_x = (target_width - final_width) / 2;
        let offset_y = (target_height - final_height) / 2;

        // Copy resized image to center of letterboxed buffer
        for y in 0..final_height {
            let src_row_start = (y * final_width * 3) as usize;
            let src_row_end = src_row_start + (final_width * 3) as usize;

            let dst_y = y + offset_y;
            let dst_row_start = ((dst_y * target_width + offset_x) * 3) as usize;
            let dst_row_end = dst_row_start + (final_width * 3) as usize;

            letterboxed[dst_row_start..dst_row_end]
                .copy_from_slice(&dst[src_row_start..src_row_end]);
        }

        Ok(letterboxed)
    } else {
        Ok(dst)
    }
}

/// Resize function specifically optimized for margin analysis
///
/// This variant uses a faster algorithm suitable for margin detection
/// where pixel-perfect accuracy is less critical than performance.
pub fn resize_for_margin_analysis(
    src_data: &[u8],
    src_width: u32,
    src_height: u32,
    target_width: u32,
    target_height: u32,
) -> Result<Vec<u8>> {
    // Create a mutable copy for FIR to work with
    let mut src_copy = src_data.to_vec();

    // Create source image view
    let src_view = Image::from_slice_u8(src_width, src_height, &mut src_copy, PixelType::U8x3)
        .map_err(|e| ResizeError::BackendError(format!("Failed to create source image: {e:?}")))?;

    // Allocate destination buffer
    let mut dst = vec![0u8; (target_width * target_height * 3) as usize];
    let mut dst_img = Image::from_slice_u8(target_width, target_height, &mut dst, PixelType::U8x3)
        .map_err(|e| ResizeError::BackendError(format!("Failed to create dest image: {e:?}")))?;

    // Use Bilinear for faster margin analysis (good enough quality for edge detection)
    let mut resizer = Resizer::new();
    let resize_options =
        ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));

    resizer
        .resize(&src_view, &mut dst_img, Some(&resize_options))
        .map_err(|e| ResizeError::BackendError(format!("Resize error: {e:?}")))?;

    Ok(dst)
}
