use anyhow::{Result, bail};
use image::RgbImage;

use crate::nms::DetectionContext;

#[derive(Debug, Clone)]
pub struct Detection {
    pub class_id: i32,
    pub class_name: Option<String>,
    pub confidence: f32,
    pub bbox: [f32; 4],
    pub category: crate::types::ContentCategory,
    pub context: Option<DetectionContext>,
}

impl Detection {
    pub fn scale_bbox(&mut self, sx: f32, sy: f32) {
        self.bbox[0] *= sx;
        self.bbox[1] *= sy;
        self.bbox[2] *= sx;
        self.bbox[3] *= sy;
    }
}

#[derive(Debug, Clone)]
pub struct LayoutEngineConfig {
    pub confidence_threshold: f32,
    pub nms_threshold: f32,
    pub iou_threshold: f32,
    pub batch_size: usize,
}

impl LayoutEngineConfig {
    pub fn new(
        confidence_threshold: f32,
        nms_threshold: f32,
        iou_threshold: f32,
        batch_size: usize,
    ) -> Self {
        Self {
            confidence_threshold,
            nms_threshold,
            iou_threshold,
            batch_size,
        }
    }
}

impl Default for LayoutEngineConfig {
    fn default() -> Self {
        Self::new(0.2, 0.5, 0.5, 1)
    }
}

pub struct LayoutEngine;

impl LayoutEngine {
    pub fn new(_model_path: &str, _config: LayoutEngineConfig) -> Result<Self> {
        bail!("layout detection was not compiled into this Lege build")
    }

    pub fn detect_single_blocking(&mut self, _image: &RgbImage) -> Result<Vec<Detection>> {
        bail!("layout detection was not compiled into this Lege build")
    }

    pub async fn detect_single_async(&mut self, image: &RgbImage) -> Result<Vec<Detection>> {
        self.detect_single_blocking(image)
    }

    pub async fn detect_batch_async(&mut self, images: &[RgbImage]) -> Result<Vec<Vec<Detection>>> {
        self.detect_batch_with_indices_blocking(images, &(0..images.len()).collect::<Vec<_>>())
    }

    pub async fn detect_batch_with_indices_async(
        &mut self,
        images: &[RgbImage],
        page_indices: &[usize],
    ) -> Result<Vec<Vec<Detection>>> {
        self.detect_batch_with_indices_blocking(images, page_indices)
    }

    pub fn detect_batch_with_indices_blocking(
        &mut self,
        _images: &[RgbImage],
        _page_indices: &[usize],
    ) -> Result<Vec<Vec<Detection>>> {
        bail!("layout detection was not compiled into this Lege build")
    }

    pub fn provider_name(&self) -> &str {
        "disabled"
    }
}

pub fn detect_layout_batch(
    _image_paths: &[String],
    _model_path: &str,
) -> Result<Vec<Vec<Detection>>> {
    bail!("layout detection was not compiled into this Lege build")
}
