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

/// Returns true when `error` was caused by every wgpu adapter rejecting
/// `request_device` (bad or absent drivers). Distinct from the software-adapter
/// case: here wgpu *found* an adapter but the driver refused to create a device.
pub fn is_layout_gpu_device_error(error: &(dyn std::error::Error + 'static)) -> bool {
    use crate::vision::runtime::device::GpuContext;
    let marker = GpuContext::gpu_device_unavailable_marker();
    let msg = format!("{error:#}");
    msg.contains(marker)
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
