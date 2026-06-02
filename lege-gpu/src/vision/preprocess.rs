use std::path::Path;

use anyhow::{Context, Result};
use image::{DynamicImage, ImageBuffer, Rgb, RgbImage, imageops};

use crate::vision::reference;

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

pub(crate) fn load_letterbox(path: &Path, size: u32) -> Result<PreprocessedImage> {
    let image = image::open(path)
        .with_context(|| format!("failed to load image {}", path.display()))?
        .to_rgb8();
    Ok(letterbox_rgb(
        DynamicImage::ImageRgb8(image).to_rgb8(),
        size,
    )?)
}

pub(crate) fn letterbox_rgb(original: RgbImage, size: u32) -> Result<PreprocessedImage> {
    let orig_w = original.width();
    let orig_h = original.height();
    let scale = (size as f32 / orig_w as f32).min(size as f32 / orig_h as f32);
    let resized_w = ((orig_w as f32) * scale).round().max(1.0) as u32;
    let resized_h = ((orig_h as f32) * scale).round().max(1.0) as u32;
    let pad_x = ((size - resized_w) / 2) as f32;
    let pad_y = ((size - resized_h) / 2) as f32;

    let resized = imageops::resize(
        &original,
        resized_w,
        resized_h,
        imageops::FilterType::Triangle,
    );
    let mut letterboxed = ImageBuffer::from_pixel(size, size, Rgb([114, 114, 114]));
    imageops::replace(
        &mut letterboxed,
        &resized,
        pad_x.round() as i64,
        pad_y.round() as i64,
    );

    let mut nchw = vec![0.0f32; 3 * size as usize * size as usize];
    let plane = size as usize * size as usize;
    for y in 0..size as usize {
        for x in 0..size as usize {
            let pixel = letterboxed.get_pixel(x as u32, y as u32).0;
            let index = y * size as usize + x;
            nchw[index] = pixel[0] as f32 / 255.0;
            nchw[plane + index] = pixel[1] as f32 / 255.0;
            nchw[2 * plane + index] = pixel[2] as f32 / 255.0;
        }
    }

    Ok(PreprocessedImage {
        tensor: reference::Tensor::new(vec![1, 3, size as usize, size as usize], nchw)?,
        letterboxed,
        original,
        meta: PreprocessMeta {
            orig_w,
            orig_h,
            input_w: size,
            input_h: size,
            scale,
            pad_x,
            pad_y,
        },
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
