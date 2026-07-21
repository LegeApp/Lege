use std::path::Path;

use anyhow::{Context, Result};
use image::{GrayImage, RgbImage, imageops};

use crate::vision::reference;

const IMAGENET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const IMAGENET_STD: [f32; 3] = [0.229, 0.224, 0.225];
const REC_MEAN: [f32; 3] = [0.5; 3];
const REC_STD: [f32; 3] = [0.5; 3];

fn normalize_rgb_nchw(image: &RgbImage, mean: [f32; 3], std: [f32; 3]) -> Vec<f32> {
    let plane = image.width() as usize * image.height() as usize;
    let mut nchw = vec![0.0f32; 3 * plane];
    for (index, pixel) in image.pixels().enumerate() {
        for channel in 0..3 {
            nchw[channel * plane + index] =
                (pixel[channel] as f32 / 255.0 - mean[channel]) / std[channel];
        }
    }
    nchw
}

/// Normalize a single-channel image as replicated RGB without first allocating
/// an RGB image. This produces the same NCHW values as `gray_to_rgb` followed by
/// [`normalize_rgb_nchw`], but keeps full-page OCR preprocessing at one byte per
/// source pixel.
fn normalize_gray_as_rgb_nchw(image: &GrayImage, mean: [f32; 3], std: [f32; 3]) -> Vec<f32> {
    let plane = image.width() as usize * image.height() as usize;
    let mut nchw = vec![0.0f32; 3 * plane];
    for (index, pixel) in image.pixels().enumerate() {
        let value = pixel[0] as f32 / 255.0;
        for channel in 0..3 {
            nchw[channel * plane + index] = (value - mean[channel]) / std[channel];
        }
    }
    nchw
}

#[derive(Debug, Clone)]
pub(crate) struct PreprocessMeta {
    pub(crate) orig_w: u32,
    pub(crate) orig_h: u32,
    pub(crate) input_w: u32,
    pub(crate) input_h: u32,
    pub(crate) scale: f32,
    pub(crate) pad_x: f32,
    pub(crate) pad_y: f32,
}

pub(crate) struct PreprocessedImage {
    pub(crate) tensor: reference::Tensor,
    pub(crate) letterboxed: RgbImage,
    pub(crate) original: RgbImage,
    pub(crate) meta: PreprocessMeta,
}

/// PP-DocLayout preprocessing: stretch-resize to `size`×`size` (no letterbox,
/// `keep_ratio=false`), then ImageNet normalization on RGB in NCHW order —
/// `(pixel/255 - mean) / std` with the standard ImageNet mean/std. `scale`/`pad`
/// in the returned meta are set for a non-letterbox stretch: the decoder scales
/// boxes back per-axis from `orig_w/size`, `orig_h/size`.
pub(crate) fn stretch_imagenet_rgb(original: RgbImage, size: u32) -> Result<PreprocessedImage> {
    let orig_w = original.width();
    let orig_h = original.height();

    // Stretch to a square; Triangle (bilinear) matches the PIL BILINEAR golden.
    let resized = imageops::resize(&original, size, size, imageops::FilterType::Triangle);

    let nchw = normalize_rgb_nchw(&resized, IMAGENET_MEAN, IMAGENET_STD);

    Ok(PreprocessedImage {
        tensor: reference::Tensor::new(vec![1, 3, size as usize, size as usize], nchw)?,
        letterboxed: resized,
        original,
        meta: PreprocessMeta {
            orig_w,
            orig_h,
            input_w: size,
            input_h: size,
            scale: 1.0,
            pad_x: 0.0,
            pad_y: 0.0,
        },
    })
}

/// PP-OCR recognition preprocessing: resize a text-line crop to height 48
/// (width proportional, min 16), normalize RGB to `[-1, 1]` (`(x/255-0.5)/0.5`),
/// NCHW. Returns the `[1,3,48,W]` tensor and the resized width `W`.
pub(crate) fn rec_line_tensor(line: &RgbImage) -> Result<(reference::Tensor, u32)> {
    let (w, h) = line.dimensions();
    let nw = (((w as f32 * 48.0) / h.max(1) as f32).round() as u32).max(16);
    let resized = imageops::resize(line, nw, 48, imageops::FilterType::Triangle);
    let nchw = normalize_rgb_nchw(&resized, REC_MEAN, REC_STD);
    let tensor = reference::Tensor::new(vec![1, 3, 48, nw as usize], nchw)?;
    Ok((tensor, nw))
}

/// Grayscale equivalent of [`rec_line_tensor`], producing the same replicated
/// three-channel tensor without allocating an RGB line image.
pub(crate) fn rec_line_tensor_gray(line: &GrayImage) -> Result<(reference::Tensor, u32)> {
    let (w, h) = line.dimensions();
    let nw = (((w as f32 * 48.0) / h.max(1) as f32).round() as u32).max(16);
    let resized = imageops::resize(line, nw, 48, imageops::FilterType::Triangle);
    let nchw = normalize_gray_as_rgb_nchw(&resized, REC_MEAN, REC_STD);
    let tensor = reference::Tensor::new(vec![1, 3, 48, nw as usize], nchw)?;
    Ok((tensor, nw))
}

/// Detection preprocessing result: the stamped input plus the per-axis scale
/// factors to map prob-map coordinates back to the original image.
pub(crate) struct DetInput {
    pub(crate) tensor: reference::Tensor,
    pub(crate) in_w: u32,
    pub(crate) in_h: u32,
    pub(crate) scale_x: f32,
    pub(crate) scale_y: f32,
}

/// PP-OCR DBNet detection preprocessing: limit the long side to `limit` (only
/// shrinking, never upscaling), round both sides to multiples of 32, then
/// ImageNet-normalize RGB in NCHW. Matches PaddleOCR `DetResizeForTest`
/// (`limit_type='max'`) followed by `NormalizeImage`.
pub(crate) fn det_input_tensor(image: &RgbImage, limit: u32) -> Result<DetInput> {
    let (w, h) = image.dimensions();
    let scale = (limit as f32 / w.max(h) as f32).min(1.0);
    let round32 = |v: f32| -> u32 { (((v / 32.0).round() as u32) * 32).max(32) };
    let in_w = round32(w as f32 * scale);
    let in_h = round32(h as f32 * scale);

    let resized = imageops::resize(image, in_w, in_h, imageops::FilterType::Triangle);
    let nchw = normalize_rgb_nchw(&resized, IMAGENET_MEAN, IMAGENET_STD);
    Ok(DetInput {
        tensor: reference::Tensor::new(vec![1, 3, in_h as usize, in_w as usize], nchw)?,
        in_w,
        in_h,
        scale_x: w as f32 / in_w as f32,
        scale_y: h as f32 / in_h as f32,
    })
}

/// Grayscale equivalent of [`det_input_tensor`]. The model still receives
/// three ImageNet-normalized channels, generated directly from the resized
/// luma plane to avoid a full-page RGB intermediate.
pub(crate) fn det_input_tensor_gray(image: &GrayImage, limit: u32) -> Result<DetInput> {
    let (w, h) = image.dimensions();
    let scale = (limit as f32 / w.max(h) as f32).min(1.0);
    let round32 = |v: f32| -> u32 { (((v / 32.0).round() as u32) * 32).max(32) };
    let in_w = round32(w as f32 * scale);
    let in_h = round32(h as f32 * scale);

    let resized = imageops::resize(image, in_w, in_h, imageops::FilterType::Triangle);
    let nchw = normalize_gray_as_rgb_nchw(&resized, IMAGENET_MEAN, IMAGENET_STD);
    Ok(DetInput {
        tensor: reference::Tensor::new(vec![1, 3, in_h as usize, in_w as usize], nchw)?,
        in_w,
        in_h,
        scale_x: w as f32 / in_w as f32,
        scale_y: h as f32 / in_h as f32,
    })
}

pub(crate) fn write_rgb(path: &Path, image: &RgbImage) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create image output dir {}", parent.display()))?;
    }
    image
        .save(path)
        .with_context(|| format!("failed to write image {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Luma, Rgb};

    fn sample_gray() -> GrayImage {
        GrayImage::from_fn(47, 23, |x, y| {
            Luma([((x * 17 + y * 29 + x * y) % 256) as u8])
        })
    }

    fn replicated_rgb(gray: &GrayImage) -> RgbImage {
        RgbImage::from_fn(gray.width(), gray.height(), |x, y| {
            let value = gray.get_pixel(x, y)[0];
            Rgb([value; 3])
        })
    }

    #[test]
    fn grayscale_rec_preprocess_matches_replicated_rgb() {
        let gray = sample_gray();
        let rgb = replicated_rgb(&gray);
        let (gray_tensor, gray_width) = rec_line_tensor_gray(&gray).unwrap();
        let (rgb_tensor, rgb_width) = rec_line_tensor(&rgb).unwrap();
        assert_eq!(gray_width, rgb_width);
        assert_eq!(gray_tensor.shape, rgb_tensor.shape);
        assert_eq!(gray_tensor.data, rgb_tensor.data);
    }

    #[test]
    fn grayscale_det_preprocess_matches_replicated_rgb() {
        let gray = sample_gray();
        let rgb = replicated_rgb(&gray);
        let gray_det = det_input_tensor_gray(&gray, 960).unwrap();
        let rgb_det = det_input_tensor(&rgb, 960).unwrap();
        assert_eq!((gray_det.in_w, gray_det.in_h), (rgb_det.in_w, rgb_det.in_h));
        assert_eq!(gray_det.tensor.shape, rgb_det.tensor.shape);
        assert_eq!(gray_det.tensor.data, rgb_det.tensor.data);
        assert_eq!(gray_det.scale_x, rgb_det.scale_x);
        assert_eq!(gray_det.scale_y, rgb_det.scale_y);
    }
}
