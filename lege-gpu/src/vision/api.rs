use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use image::{GrayImage, Luma, RgbImage};

use crate::vision::decode::yolo;
use crate::vision::onnx::{ModelReport, PreparedGraph, TARGET_INPUT, load_model};
use crate::vision::onnx_pb::ModelProto;
use crate::vision::preprocess;
use crate::vision::reference::Tensor;
use crate::vision::runtime::compiled::CompiledGraph;

const DEFAULT_CONFIDENCE_THRESHOLD: f32 = 0.20;
const DEFAULT_IOU_THRESHOLD: f32 = 0.50;
const DEFAULT_MAX_DETECTIONS: usize = 300;
const ROTATION_INPUT_SIZE: u32 = 224;

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

        let model = load_model(&config.model_path).with_context(|| {
            format!(
                "failed to load layout model {}",
                config.model_path.display()
            )
        })?;
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

        let compiled = pollster::block_on(CompiledGraph::build_layout(&graph))
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

        // One-shot GPU timestamp profile to attribute per-page inference time to
        // kernels vs inter-dispatch barrier/sync idle. Enable with LEGE_INFERENCE_PROFILE=1.
        if std::env::var_os("LEGE_INFERENCE_PROFILE").is_some() {
            static PROFILE_ONCE: std::sync::Once = std::sync::Once::new();
            PROFILE_ONCE.call_once(|| {
                match pollster::block_on(self.compiled.profile_report(&preprocessed.tensor)) {
                    Ok(report) => eprintln!("{report}"),
                    Err(e) => eprintln!("LEGE_INFERENCE_PROFILE failed: {e:#}"),
                }
            });
        }

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

pub fn is_layout_software_adapter_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let marker = CompiledGraph::layout_software_adapter_error();
    let mut current = Some(error);
    while let Some(error) = current {
        if error.to_string().contains(marker) {
            return true;
        }
        current = error.source();
    }
    false
}

#[derive(Debug, Clone)]
pub struct RotationConfig {
    pub model_path: PathBuf,
}

impl RotationConfig {
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RotationPrediction {
    pub class_id: usize,
    pub label_degrees: u16,
    pub correction_degrees: u16,
    pub confidence: f32,
    pub probabilities: [f32; 4],
}

pub struct RotationClassifier {
    graph: PreparedGraph,
    compiled: CompiledGraph,
    config: RotationConfig,
}

impl RotationClassifier {
    pub fn new(config: RotationConfig) -> Result<Self> {
        if config.model_path.as_os_str().is_empty() {
            bail!("rotation model path is empty");
        }

        let (graph, compiled) = load_vision_graph(&config.model_path, "x", "rotation")?;
        Ok(Self {
            graph,
            compiled,
            config,
        })
    }

    pub fn from_model_path(model_path: impl Into<PathBuf>) -> Result<Self> {
        Self::new(RotationConfig::new(model_path))
    }

    pub fn classify_rgb(&self, image: &RgbImage) -> Result<RotationPrediction> {
        let input = rgb_to_rotate_classifier_input(image)?;
        let outputs =
            pollster::block_on(self.compiled.run(&input)).context("rotation inference failed")?;
        let output = graph_output(&self.graph, &outputs, 0)
            .context("rotation output missing from WGPU result")?;

        if output.data.len() < 4 {
            bail!(
                "rotation output must contain at least 4 class scores, got {}",
                output.data.len()
            );
        }

        let mut probabilities = [0.0f32; 4];
        probabilities.copy_from_slice(&output.data[..4]);

        let (class_id, confidence) = probabilities
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(idx, score)| (idx, *score))
            .context("rotation output was empty")?;

        Ok(RotationPrediction {
            class_id,
            label_degrees: rotation_label_degrees(class_id),
            correction_degrees: rotation_correction_degrees(class_id),
            confidence,
            probabilities,
        })
    }

    pub fn provider_name(&self) -> &'static str {
        "WGPU"
    }

    pub fn model_path(&self) -> &Path {
        &self.config.model_path
    }
}

#[derive(Debug, Clone)]
pub struct DeskewConfig {
    pub model_path: PathBuf,
}

impl DeskewConfig {
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
        }
    }
}

/// A deskew graph compiled for one specific page resolution.
struct DeskewGraph {
    width: u32,
    height: u32,
    graph: PreparedGraph,
    compiled: CompiledGraph,
}

/// Document unwarp via the dynamic-resolution paddle-deskew model.
///
/// The model declares symbolic H/W; we stamp the page's native dimensions into
/// the graph at preparation time so the shape-plumbing subgraph folds and the
/// graph compiles natively at that size. No resize to a fixed working size, so
/// the warp is applied at full document resolution. The compiled graph is cached
/// for the most recent page size and rebuilt only when the size changes — the
/// rebuild is a fast CPU shape-pass plus one `CompiledGraph::build()`.
pub struct DocumentDeskewer {
    model: ModelProto,
    config: DeskewConfig,
    cache: Mutex<Option<DeskewGraph>>,
}

impl DocumentDeskewer {
    pub fn new(config: DeskewConfig) -> Result<Self> {
        if config.model_path.as_os_str().is_empty() {
            bail!("deskew model path is empty");
        }

        let model = load_model(&config.model_path).with_context(|| {
            format!(
                "failed to load deskew model {}",
                config.model_path.display()
            )
        })?;
        let report = ModelReport::from_model(&model).context("failed to inspect deskew model")?;
        if !report.rejection_reasons.is_empty() {
            bail!(
                "deskew model is not compatible with lege-vision: {}",
                report.rejection_reasons.join("; ")
            );
        }

        Ok(Self {
            model,
            config,
            cache: Mutex::new(None),
        })
    }

    pub fn from_model_path(model_path: impl Into<PathBuf>) -> Result<Self> {
        Self::new(DeskewConfig::new(model_path))
    }

    fn build_for(&self, width: u32, height: u32) -> Result<DeskewGraph> {
        let dims = [1, 3, height as i64, width as i64];
        let graph = PreparedGraph::from_model_with_input_dims(&self.model, Some(&dims))
            .with_context(|| format!("failed to prepare deskew graph for {width}x{height}"))?;
        if graph.inputs.first().map(String::as_str) != Some("image") {
            bail!(
                "deskew model must use input `image`, got {:?}",
                graph.inputs
            );
        }
        let compiled = pollster::block_on(CompiledGraph::build(&graph))
            .with_context(|| format!("failed to compile deskew graph for {width}x{height}"))?;
        Ok(DeskewGraph {
            width,
            height,
            graph,
            compiled,
        })
    }

    pub fn unwarp_rgb(&self, image: &RgbImage) -> Result<RgbImage> {
        let (width, height) = image.dimensions();
        if width == 0 || height == 0 {
            bail!("deskew input image must be non-empty");
        }
        let mut guard = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("deskew cache poisoned"))?;
        if guard.as_ref().map(|entry| (entry.width, entry.height)) != Some((width, height)) {
            *guard = Some(self.build_for(width, height)?);
        }
        let entry = guard.as_ref().expect("deskew graph built above");
        let input = rgb_to_nchw_native(image);
        let outputs =
            pollster::block_on(entry.compiled.run(&input)).context("deskew inference failed")?;
        let output = graph_output(&entry.graph, &outputs, 0)
            .context("deskew output missing from WGPU result")?;
        nchw_tensor_to_rgb(output)
    }

    pub fn provider_name(&self) -> &'static str {
        "WGPU"
    }

    pub fn model_path(&self) -> &Path {
        &self.config.model_path
    }
}

#[derive(Debug, Clone)]
pub struct SauvolaConfig {
    pub model_path: PathBuf,
}

impl SauvolaConfig {
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
        }
    }
}

/// A sauvola graph compiled for one specific page resolution.
struct SauvolaGraph {
    width: u32,
    height: u32,
    graph: PreparedGraph,
    compiled: CompiledGraph,
}

/// Number of distinct tile sizes to keep compiled at once. The caller binarizes
/// in overlapping patches (see `process_in_patches`), so a page yields only a
/// handful of sizes — the full patch plus the clamped right/bottom/corner tiles.
/// A small LRU keeps each built once per job instead of thrashing the ~0.5 s
/// graph compile as tile sizes alternate.
const SAUVOLA_CACHE_CAP: usize = 8;

/// sauvola.onnx adaptive binarization via the dynamic-resolution model.
///
/// The model is NHWC `[1,H,W,1]` with symbolic H/W. Because the channel is 1,
/// NHWC and NCHW share memory layout, so the NCHW bridge runs it unchanged; the
/// only thing that needed solving was the shape-plumbing that computes the
/// window pixel-count integral images from the input size. We stamp the tile's
/// native dims at preparation time so that plumbing folds (the all-ones
/// `ConstantOfShape` tensors become initializers feeding `CumSum`), and the
/// graph compiles natively at the tile size — no resize. Compiled graphs are
/// cached per tile size (most-recently-used first, capped) since the caller
/// feeds a few recurring patch sizes.
pub struct SauvolaProcessor {
    model: ModelProto,
    config: SauvolaConfig,
    cache: Mutex<Vec<SauvolaGraph>>,
}

impl SauvolaProcessor {
    pub fn new(config: SauvolaConfig) -> Result<Self> {
        if config.model_path.as_os_str().is_empty() {
            bail!("sauvola model path is empty");
        }

        let model = load_model(&config.model_path).with_context(|| {
            format!(
                "failed to load sauvola model {}",
                config.model_path.display()
            )
        })?;
        let report = ModelReport::from_model(&model).context("failed to inspect sauvola model")?;
        if !report.rejection_reasons.is_empty() {
            bail!(
                "sauvola model is not compatible with lege-vision: {}",
                report.rejection_reasons.join("; ")
            );
        }

        Ok(Self {
            model,
            config,
            cache: Mutex::new(Vec::new()),
        })
    }

    pub fn from_model_path(model_path: impl Into<PathBuf>) -> Result<Self> {
        Self::new(SauvolaConfig::new(model_path))
    }

    /// Returns the model path used by this processor.
    pub fn model_path(&self) -> &Path {
        &self.config.model_path
    }

    fn build_for(&self, width: u32, height: u32) -> Result<SauvolaGraph> {
        let dims = [1, height as i64, width as i64, 1];
        let graph = PreparedGraph::from_model_with_input_dims(&self.model, Some(&dims))
            .with_context(|| format!("failed to prepare sauvola graph for {width}x{height}"))?;
        if graph.inputs.first().map(String::as_str) != Some("img01_inp") {
            bail!(
                "sauvola model must use input `img01_inp`, got {:?}",
                graph.inputs
            );
        }
        let compiled = pollster::block_on(CompiledGraph::build(&graph))
            .with_context(|| format!("failed to compile sauvola graph for {width}x{height}"))?;
        Ok(SauvolaGraph {
            width,
            height,
            graph,
            compiled,
        })
    }

    /// Process an RGB image through the sauvola model at its native resolution.
    /// Preprocesses to grayscale, NHWC `[1,H,W,1]`, no resize. Returns a binary
    /// grayscale mask (255 = foreground text, 0 = background).
    pub fn binarize_rgb(&self, image: &RgbImage) -> Result<GrayImage> {
        let (width, height) = image.dimensions();
        if width == 0 || height == 0 {
            bail!("sauvola input image must be non-empty");
        }
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("sauvola cache poisoned"))?;
        // Move a cached graph for this size to the front, or build and insert it.
        match cache
            .iter()
            .position(|e| e.width == width && e.height == height)
        {
            Some(0) => {}
            Some(index) => {
                let entry = cache.remove(index);
                cache.insert(0, entry);
            }
            None => {
                let entry = self.build_for(width, height)?;
                cache.insert(0, entry);
                cache.truncate(SAUVOLA_CACHE_CAP);
            }
        }
        let entry = &cache[0];
        let input = rgb_to_nhwc_gray_native(image);
        let outputs =
            pollster::block_on(entry.compiled.run(&input)).context("sauvola inference failed")?;
        let output = graph_output(&entry.graph, &outputs, 0)
            .context("sauvola output missing from WGPU result")?;
        nhwc_gray_tensor_to_image(output)
    }
}

/// CPU-only sauvola binarization.
///
/// Heavy Sauvola runs whole-image on the CPU reference executor rather than the
/// GPU: the model is dominated by integral images, slices and global instance
/// normalization (sequential, memory-bound work) where the GPU offers no
/// speedup but costs VRAM and forces tiling. Tiling is unacceptable here because
/// the global instance norm makes per-tile statistics inconsistent. Running the
/// whole page on CPU is RAM-bounded, needs no resize, and keeps the global
/// statistics correct — matching the original onnxruntime behavior.
pub struct SauvolaCpuProcessor {
    model: ModelProto,
    config: SauvolaConfig,
    cache: Mutex<Option<(u32, u32, Arc<PreparedGraph>)>>,
}

impl SauvolaCpuProcessor {
    pub fn new(config: SauvolaConfig) -> Result<Self> {
        if config.model_path.as_os_str().is_empty() {
            bail!("sauvola model path is empty");
        }
        let model = load_model(&config.model_path).with_context(|| {
            format!(
                "failed to load sauvola model {}",
                config.model_path.display()
            )
        })?;
        let report = ModelReport::from_model(&model).context("failed to inspect sauvola model")?;
        if !report.rejection_reasons.is_empty() {
            bail!(
                "sauvola model is not compatible with lege-vision: {}",
                report.rejection_reasons.join("; ")
            );
        }
        Ok(Self {
            model,
            config,
            cache: Mutex::new(None),
        })
    }

    pub fn from_model_path(model_path: impl Into<PathBuf>) -> Result<Self> {
        Self::new(SauvolaConfig::new(model_path))
    }

    pub fn model_path(&self) -> &Path {
        &self.config.model_path
    }

    /// Binarize an RGB image whole, on CPU, at its native resolution.
    /// Returns a binary mask (255 = foreground text, 0 = background).
    pub fn binarize_rgb(&self, image: &RgbImage) -> Result<GrayImage> {
        let (width, height) = image.dimensions();
        if width == 0 || height == 0 {
            bail!("sauvola input image must be non-empty");
        }
        // Resolve (and lazily build) the per-size prepared graph under the lock,
        // then clone the Arc and release the lock before running inference. The
        // graph's run_cpu takes &self and is reentrant, so concurrent pages of the
        // same size run in parallel instead of serializing on this mutex.
        let graph = {
            let mut guard = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("sauvola cache poisoned"))?;
            if guard.as_ref().map(|(w, h, _)| (*w, *h)) != Some((width, height)) {
                let graph = PreparedGraph::from_model_with_input_dims(
                    &self.model,
                    Some(&[1, height as i64, width as i64, 1]),
                )
                .with_context(|| format!("failed to prepare sauvola graph for {width}x{height}"))?;
                if graph.inputs.first().map(String::as_str) != Some("img01_inp") {
                    bail!(
                        "sauvola model must use input `img01_inp`, got {:?}",
                        graph.inputs
                    );
                }
                *guard = Some((width, height, Arc::new(graph)));
            }
            Arc::clone(&guard.as_ref().expect("sauvola graph prepared above").2)
        };
        let input = rgb_to_nhwc_gray_native(image);
        let outputs = graph
            .run_cpu(&input)
            .context("sauvola CPU inference failed")?;
        let out_name = graph
            .outputs
            .first()
            .context("sauvola graph has no output")?;
        let output = outputs
            .get(out_name)
            .with_context(|| format!("sauvola output `{out_name}` missing"))?;
        nhwc_gray_tensor_to_image(output)
    }
}

/// Preprocess: grayscale, normalize to [0,1], NHWC `[1,H,W,1]` at native size.
fn rgb_to_nhwc_gray_native(image: &RgbImage) -> Tensor {
    let w = image.width() as usize;
    let h = image.height() as usize;
    let mut data = vec![0.0f32; h * w];
    for (x, y, pixel) in image.enumerate_pixels() {
        let p = pixel.0;
        let luma = 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32;
        data[y as usize * w + x as usize] = luma / 255.0;
    }
    Tensor {
        shape: vec![1, h, w, 1],
        data,
    }
}

/// Postprocess: read NHWC [1, H, W, 1] tensor as GrayImage (threshold at 0.5).
fn nhwc_gray_tensor_to_image(tensor: &Tensor) -> Result<GrayImage> {
    if tensor.shape.len() != 4 || tensor.shape[0] != 1 || tensor.shape[3] != 1 {
        bail!(
            "expected NHWC grayscale tensor [1,H,W,1], got {:?}",
            tensor.shape
        );
    }
    let h = tensor.shape[1];
    let w = tensor.shape[2];
    let mut image = GrayImage::new(w as u32, h as u32);
    for y in 0..h {
        for x in 0..w {
            // Threshold the model output at 0.5; 255 = foreground
            let value = if tensor.data[y * w + x] > 0.5 {
                255u8
            } else {
                0u8
            };
            image.put_pixel(x as u32, y as u32, Luma([value]));
        }
    }
    Ok(image)
}

fn load_vision_graph(
    model_path: &Path,
    expected_input: &str,
    model_name: &str,
) -> Result<(PreparedGraph, CompiledGraph)> {
    let model = load_model(model_path)
        .with_context(|| format!("failed to load {model_name} model {}", model_path.display()))?;
    let report = ModelReport::from_model(&model)
        .with_context(|| format!("failed to inspect {model_name} model"))?;
    if !report.rejection_reasons.is_empty() {
        bail!(
            "{model_name} model is not compatible with lege-vision: {}",
            report.rejection_reasons.join("; ")
        );
    }

    let graph = PreparedGraph::from_model(&model)
        .with_context(|| format!("failed to prepare {model_name} graph"))?;
    if graph.inputs.first().map(String::as_str) != Some(expected_input) {
        bail!(
            "{model_name} model must use input `{expected_input}`, got {:?}",
            graph.inputs
        );
    }

    let compiled = pollster::block_on(CompiledGraph::build(&graph))
        .with_context(|| format!("failed to compile {model_name} graph for WGPU"))?;

    Ok((graph, compiled))
}

fn graph_output<'a>(
    graph: &PreparedGraph,
    outputs: &'a std::collections::HashMap<String, Tensor>,
    index: usize,
) -> Result<&'a Tensor> {
    let name = graph
        .outputs
        .get(index)
        .with_context(|| format!("graph has no output at index {index}"))?;
    outputs
        .get(name)
        .with_context(|| format!("missing model output tensor `{name}`"))
}

fn rgb_to_rotate_classifier_input(image: &RgbImage) -> Result<Tensor> {
    let resized = resize_to_fill_rgb(image, ROTATION_INPUT_SIZE, ROTATION_INPUT_SIZE);
    let mean = [0.485f32, 0.456, 0.406];
    let std = [0.229f32, 0.224, 0.225];
    let w = ROTATION_INPUT_SIZE as usize;
    let h = ROTATION_INPUT_SIZE as usize;
    let plane = w * h;
    let mut data = vec![0.0f32; 3 * plane];
    for y in 0..h {
        for x in 0..w {
            let pixel = resized.get_pixel(x as u32, y as u32).0;
            let index = y * w + x;
            for channel in 0..3 {
                let value = pixel[channel] as f32 / 255.0;
                data[channel * plane + index] = (value - mean[channel]) / std[channel];
            }
        }
    }
    Tensor::new(vec![1, 3, h, w], data)
}

fn resize_to_fill_rgb(image: &RgbImage, width: u32, height: u32) -> RgbImage {
    let scale = (width as f32 / image.width() as f32).max(height as f32 / image.height() as f32);
    let resized_width = (image.width() as f32 * scale).round().max(1.0) as u32;
    let resized_height = (image.height() as f32 * scale).round().max(1.0) as u32;
    let resized = image::imageops::resize(
        image,
        resized_width,
        resized_height,
        image::imageops::FilterType::Triangle,
    );
    let crop_x = resized_width.saturating_sub(width) / 2;
    let crop_y = resized_height.saturating_sub(height) / 2;
    image::imageops::crop_imm(&resized, crop_x, crop_y, width, height).to_image()
}

/// Converts an RGB image to a normalized NCHW `[1,3,H,W]` tensor at its native
/// resolution (no resize — the deskew graph is compiled for this exact size).
fn rgb_to_nchw_native(image: &RgbImage) -> Tensor {
    let w = image.width() as usize;
    let h = image.height() as usize;
    let plane = w * h;
    let mut data = vec![0.0f32; 3 * plane];
    for (x, y, pixel) in image.enumerate_pixels() {
        let index = y as usize * w + x as usize;
        data[index] = pixel.0[0] as f32 / 255.0;
        data[plane + index] = pixel.0[1] as f32 / 255.0;
        data[2 * plane + index] = pixel.0[2] as f32 / 255.0;
    }
    Tensor {
        shape: vec![1, 3, h, w],
        data,
    }
}

fn nchw_tensor_to_rgb(tensor: &Tensor) -> Result<RgbImage> {
    if tensor.shape.len() != 4 || tensor.shape[0] != 1 || tensor.shape[1] != 3 {
        bail!("expected NCHW RGB tensor [1,3,H,W], got {:?}", tensor.shape);
    }
    let h = tensor.shape[2];
    let w = tensor.shape[3];
    let plane = h * w;
    let mut image = RgbImage::new(w as u32, h as u32);
    for y in 0..h {
        for x in 0..w {
            let index = y * w + x;
            image.put_pixel(
                x as u32,
                y as u32,
                image::Rgb([
                    f32_to_u8(tensor.data[index]),
                    f32_to_u8(tensor.data[plane + index]),
                    f32_to_u8(tensor.data[2 * plane + index]),
                ]),
            );
        }
    }
    Ok(image)
}

fn f32_to_u8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn rotation_label_degrees(class_id: usize) -> u16 {
    match class_id {
        0 => 0,
        1 => 90,
        2 => 180,
        3 => 270,
        _ => 0,
    }
}

fn rotation_correction_degrees(class_id: usize) -> u16 {
    (360 - rotation_label_degrees(class_id)) % 360
}
