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
        Self {
            confidence_threshold: 0.2,
            nms_threshold: 0.5,
            iou_threshold: 0.5,
            batch_size: 1,
        }
    }
}

pub struct LayoutEngine {
    detector: LayoutDetector,
    config: LayoutEngineConfig,
    provider_name: &'static str,
}

impl LayoutEngine {
    pub fn new(model_path: &str, config: LayoutEngineConfig) -> Result<Self> {
        let layout_config = LayoutConfig {
            model_path: model_path.into(),
            confidence_threshold: config.confidence_threshold,
            iou_threshold: config.iou_threshold,
            max_detections: 300,
        };
        // No `doclayout.onnx` on disk → use the model embedded in the binary.
        let detector = if model_path.is_empty() {
            LayoutDetector::from_model_bytes(crate::EMBEDDED_LAYOUT_MODEL, layout_config)?
        } else {
            LayoutDetector::new(layout_config)?
        };
        #[cfg(feature = "debug-logging")]
        info_println!("Using WGPU execution for PP-DocLayout layout detection");
        Ok(Self {
            provider_name: detector.provider_name(),
            detector,
            config,
        })
    }

    /// Create another single-flight layout session on the same WGPU device.
    /// Each session owns activation/readback buffers, while the prepared model
    /// graph and device/queue are shared.
    pub fn build_sibling(&self) -> Result<Self> {
        let detector = self.detector.build_sibling()?;
        Ok(Self {
            provider_name: detector.provider_name(),
            detector,
            config: self.config.clone(),
        })
    }

    #[deprecated(note = "This method is deprecated. Use detect_single_async instead.")]
    pub fn detect_single(&mut self, image: &RgbImage) -> Result<Vec<Detection>> {
        self.detect_single_blocking(image)
    }

    pub fn detect_single_blocking(&mut self, image: &RgbImage) -> Result<Vec<Detection>> {
        let detections = self.detector.detect_rgb(image)?;
        let mut detections = self.finalize_detections(detections, image.width(), image.height());
        if crate::layout_correct::should_redetect_images(&detections, image) {
            let inverted = crate::layout_correct::invert_rgb(image);
            if let Ok(extra) = self.detector.detect_rgb(&inverted) {
                let extra = self.finalize_detections(extra, image.width(), image.height());
                crate::layout_correct::merge_image_redetects(&mut detections, &extra, image);
            }
        }
        crate::layout_correct::correct_layout_detections(&mut detections, image);
        Ok(detections)
    }

    pub async fn detect_single_async(&mut self, image: &RgbImage) -> Result<Vec<Detection>> {
        self.detect_single_blocking(image)
    }

    pub async fn detect_batch_async(&mut self, images: &[RgbImage]) -> Result<Vec<Vec<Detection>>> {
        let page_indices = (0..images.len()).collect::<Vec<_>>();
        self.detect_batch_with_indices_async(images, &page_indices)
            .await
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
                class_name: Some(crate::types::class_name_for(d.class_id).to_string()),
                confidence: d.confidence,
                bbox: normalize_bbox(d.bbox, original_width, original_height),
                category: crate::types::category_for_class(d.class_id),
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

        let finalized = two_pass_nms(
            nms_detections,
            self.config.nms_threshold,
            self.config.iou_threshold,
        );
        if std::env::var_os("LEGE_LAYOUT_TRACE").is_some() {
            eprintln!(
                "LEGE_LAYOUT_TRACE second_nms input={} output={} aware_iou={:.3} agnostic_iou={:.3}",
                detections.len(),
                finalized.len(),
                self.config.nms_threshold,
                self.config.iou_threshold
            );
            for detection in &finalized {
                eprintln!(
                    "  second-nms {} score={:.4} bbox={:?}",
                    detection.class_name.as_deref().unwrap_or("unknown"),
                    detection.confidence,
                    detection.bbox
                );
            }
        }
        finalized
            .into_iter()
            .map(|d| Detection {
                class_id: d.class_id,
                category: crate::types::category_for_class(d.class_id),
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

pub fn detect_layout_batch(
    image_paths: &[String],
    model_path: &str,
) -> Result<Vec<Vec<Detection>>> {
    let mut engine = LayoutEngine::new(model_path, LayoutEngineConfig::default())?;
    image_paths
        .iter()
        .map(|path| {
            let image = image::open(path)?.to_rgb8();
            engine.detect_single_blocking(&image)
        })
        .collect()
}
