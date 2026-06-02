use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use image::RgbImage;

use crate::vision::decode::yolo;
use crate::vision::onnx::{ModelReport, PreparedGraph, TARGET_INPUT, load_model};
use crate::vision::preprocess;
use crate::vision::runtime::compiled::CompiledGraph;

const DEFAULT_CONFIDENCE_THRESHOLD: f32 = 0.20;
const DEFAULT_IOU_THRESHOLD: f32 = 0.50;
const DEFAULT_MAX_DETECTIONS: usize = 300;

#[derive(Debug, Clone)]
pub struct LayoutConfig {
    pub model_path: PathBuf,
    pub confidence_threshold: f32,
    pub iou_threshold: f32,
    pub max_detections: usize,
}

impl LayoutConfig {
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            ..Self::default()
        }
    }
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            model_path: PathBuf::new(),
            confidence_threshold: DEFAULT_CONFIDENCE_THRESHOLD,
            iou_threshold: DEFAULT_IOU_THRESHOLD,
            max_detections: DEFAULT_MAX_DETECTIONS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LayoutDetection {
    pub class_id: i32,
    pub class_name: &'static str,
    pub confidence: f32,
    pub bbox: [f32; 4],
}

pub struct LayoutDetector {
    graph: PreparedGraph,
    compiled: CompiledGraph,
    config: LayoutConfig,
}

impl LayoutDetector {
    pub fn new(config: LayoutConfig) -> Result<Self> {
        if config.model_path.as_os_str().is_empty() {
            bail!("layout model path is empty");
        }

        let model = load_model(&config.model_path)
            .with_context(|| format!("failed to load layout model {}", config.model_path.display()))?;
        let report = ModelReport::from_model(&model).context("failed to inspect layout model")?;
        if !report.rejection_reasons.is_empty() {
            bail!(
                "layout model is not compatible with lege-vision: {}",
                report.rejection_reasons.join("; ")
            );
        }

        let graph = PreparedGraph::from_model(&model).context("failed to prepare layout graph")?;
        if graph.inputs.first().map(String::as_str) != Some("images") {
            bail!(
                "layout model must use YOLO input `images`, got {:?}",
                graph.inputs
            );
        }

        let compiled = pollster::block_on(CompiledGraph::build(&graph))
            .context("failed to compile layout graph for WGPU")?;

        Ok(Self {
            graph,
            compiled,
            config,
        })
    }

    pub fn from_model_path(model_path: impl Into<PathBuf>) -> Result<Self> {
        Self::new(LayoutConfig::new(model_path))
    }

    pub fn detect_rgb(&self, image: &RgbImage) -> Result<Vec<LayoutDetection>> {
        let preprocessed = preprocess::letterbox_rgb(image.clone(), TARGET_INPUT[2] as u32)
            .context("failed to preprocess layout input")?;
        let outputs = pollster::block_on(self.compiled.run(&preprocessed.tensor))
            .context("layout inference failed")?;
        let mut heads = Vec::with_capacity(self.graph.outputs.len());
        for name in &self.graph.outputs {
            let tensor = outputs
                .get(name)
                .with_context(|| format!("layout output `{name}` missing from WGPU result"))?;
            heads.push(tensor.clone());
        }

        yolo::decode_heads(
            &heads,
            &preprocessed.meta,
            self.config.confidence_threshold,
            self.config.iou_threshold,
            self.config.max_detections,
        )
        .context("failed to decode YOLO layout heads")
        .map(|detections| {
            detections
                .into_iter()
                .map(|det| LayoutDetection {
                    class_id: det.class_id as i32,
                    class_name: yolo::class_name(det.class_id),
                    confidence: det.score,
                    bbox: [det.x1, det.y1, det.x2, det.y2],
                })
                .collect()
        })
    }

    pub fn detect_path(&self, image_path: impl AsRef<Path>) -> Result<Vec<LayoutDetection>> {
        let image_path = image_path.as_ref();
        let image = image::open(image_path)
            .with_context(|| format!("failed to load layout input {}", image_path.display()))?
            .to_rgb8();
        self.detect_rgb(&image)
    }

    pub fn provider_name(&self) -> &'static str {
        "WGPU"
    }

    pub fn model_path(&self) -> &Path {
        &self.config.model_path
    }
}
