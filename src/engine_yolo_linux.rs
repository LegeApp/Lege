use anyhow::Result;
use image::RgbImage;
use lege_gpu::vision::{LayoutConfig, LayoutDetection, LayoutDetector};

use crate::nms::{Detection as NmsDetection, DetectionContext, two_pass_nms};
#[allow(unused_imports)]
use crate::{debug_println, info_println};

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
pub struct YoloConfig {
    pub confidence_threshold: f32,
    pub nms_threshold: f32,
    pub iou_threshold: f32,
    pub batch_size: usize,
}

impl YoloConfig {
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

impl Default for YoloConfig {
    fn default() -> Self {
        Self {
            confidence_threshold: 0.2,
            nms_threshold: 0.5,
            iou_threshold: 0.5,
            batch_size: 1,
        }
    }
}

pub struct YoloEngine {
    detector: LayoutDetector,
    config: YoloConfig,
    provider_name: &'static str,
}

// SAFETY: The production pipeline owns this through a serialized inference actor.
unsafe impl Sync for YoloEngine {}

impl YoloEngine {
    pub fn new(model_path: &str, config: YoloConfig) -> Result<Self> {
        let detector = LayoutDetector::new(LayoutConfig {
            model_path: model_path.into(),
            confidence_threshold: config.confidence_threshold,
            iou_threshold: config.iou_threshold,
            max_detections: 300,
        })?;
        info_println!("Using WGPU execution for YOLO layout detection");
        Ok(Self {
            provider_name: detector.provider_name(),
            detector,
            config,
        })
    }

    #[deprecated(note = "This method is deprecated. Use detect_single_async instead.")]
    pub fn detect_single(&mut self, image: &RgbImage) -> Result<Vec<Detection>> {
        self.detect_single_blocking(image)
    }

    pub fn detect_single_blocking(&mut self, image: &RgbImage) -> Result<Vec<Detection>> {
        let detections = self.detector.detect_rgb(image)?;
        Ok(self.finalize_detections(detections, image.width(), image.height()))
    }

    pub async fn detect_single_async(&mut self, image: &RgbImage) -> Result<Vec<Detection>> {
        self.detect_single_blocking(image)
    }

    pub async fn detect_batch_async(&mut self, images: &[RgbImage]) -> Result<Vec<Vec<Detection>>> {
        let page_indices = (0..images.len()).collect::<Vec<_>>();
        self.detect_batch_with_indices_async(images, &page_indices).await
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
        images: &[RgbImage],
        page_indices: &[usize],
    ) -> Result<Vec<Vec<Detection>>> {
        let _ = page_indices;
        images
            .iter()
            .map(|image| self.detect_single_blocking(image))
            .collect()
    }

    pub fn provider_name(&self) -> &str {
        self.provider_name
    }

    fn finalize_detections(
        &self,
        detections: Vec<LayoutDetection>,
        original_width: u32,
        original_height: u32,
    ) -> Vec<Detection> {
        let detections = detections
            .into_iter()
            .filter(|d| {
                d.confidence >= self.config.confidence_threshold
                    && (d.bbox[2] - d.bbox[0]) >= 1.0
                    && (d.bbox[3] - d.bbox[1]) >= 1.0
            })
            .map(|d| Detection {
                class_id: d.class_id,
                class_name: Some(yolo_class_name(d.class_id).to_string()),
                confidence: d.confidence,
                bbox: normalize_bbox(d.bbox, original_width, original_height),
                category: yolo_class_to_category(d.class_id),
                context: Some(DetectionContext {
                    original_width: original_width as f32,
                    original_height: original_height as f32,
                    scale_x: 1.0,
                    scale_y: 1.0,
                }),
            })
            .collect::<Vec<_>>();

        let nms_detections = detections
            .iter()
            .map(|d| NmsDetection {
                bbox: d.bbox,
                confidence: d.confidence,
                class_id: d.class_id,
                class_name: d.class_name.clone(),
                context: d.context.clone(),
            })
            .collect::<Vec<_>>();

        two_pass_nms(
            nms_detections,
            self.config.nms_threshold,
            self.config.iou_threshold,
        )
        .into_iter()
        .map(|d| Detection {
            class_id: d.class_id,
            category: yolo_class_to_category(d.class_id),
            class_name: d.class_name,
            confidence: d.confidence,
            bbox: d.bbox,
            context: d.context,
        })
        .collect()
    }
}

fn normalize_bbox(mut bbox: [f32; 4], width: u32, height: u32) -> [f32; 4] {
    bbox[0] = bbox[0].clamp(0.0, width as f32);
    bbox[1] = bbox[1].clamp(0.0, height as f32);
    bbox[2] = bbox[2].clamp(0.0, width as f32);
    bbox[3] = bbox[3].clamp(0.0, height as f32);
    [
        bbox[0].min(bbox[2]),
        bbox[1].min(bbox[3]),
        bbox[0].max(bbox[2]),
        bbox[1].max(bbox[3]),
    ]
}

fn yolo_class_to_category(class_id: i32) -> crate::types::ContentCategory {
    use crate::types::ContentCategory;
    match class_id {
        0 => ContentCategory::Text,
        1 => ContentCategory::Text,
        2 => ContentCategory::Abandon,
        3 => ContentCategory::Image,
        4 => ContentCategory::Text,
        5 => ContentCategory::Table,
        6 => ContentCategory::Text,
        7 => ContentCategory::Text,
        8 => ContentCategory::Text,
        9 => ContentCategory::Text,
        _ => ContentCategory::Text,
    }
}

fn yolo_class_name(class_id: i32) -> &'static str {
    match class_id {
        0 => "title",
        1 => "plain_text",
        2 => "abandon",
        3 => "figure",
        4 => "figure_caption",
        5 => "table",
        6 => "table_caption",
        7 => "table_footnote",
        8 => "isolate_formula",
        9 => "formula_caption",
        _ => "unknown",
    }
}

pub fn detect_layout_yolo_batch(
    image_paths: &[String],
    model_path: &str,
) -> Result<Vec<Vec<Detection>>> {
    let mut engine = YoloEngine::new(model_path, YoloConfig::default())?;
    image_paths
        .iter()
        .map(|path| {
            let image = image::open(path)?.to_rgb8();
            engine.detect_single_blocking(&image)
        })
        .collect()
}
