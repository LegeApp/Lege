use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use image::{GrayImage, Luma, RgbImage};

#[cfg(feature = "layout-detection")]
use crate::vision::decode::picodet;
use crate::vision::onnx::{ModelReport, PreparedGraph, load_model, load_model_from_bytes};
use crate::vision::onnx_pb::ModelProto;
use crate::vision::preprocess;
use crate::vision::reference::Tensor;
use crate::vision::runtime::compiled::CompiledGraph;

#[cfg(feature = "layout-detection")]
const DEFAULT_CONFIDENCE_THRESHOLD: f32 = 0.20;
#[cfg(feature = "layout-detection")]
const DEFAULT_IOU_THRESHOLD: f32 = 0.50;
#[cfg(feature = "layout-detection")]
const DEFAULT_MAX_DETECTIONS: usize = 300;

#[cfg(feature = "layout-detection")]
#[derive(Debug, Clone)]
pub struct LayoutConfig {
    pub model_path: PathBuf,
    pub confidence_threshold: f32,
    pub iou_threshold: f32,
    pub max_detections: usize,
}

#[cfg(feature = "layout-detection")]
impl LayoutConfig {
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            ..Self::default()
        }
    }
}

#[cfg(feature = "layout-detection")]
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

#[cfg(feature = "layout-detection")]
#[derive(Debug, Clone)]
pub struct LayoutDetection {
    pub class_id: i32,
    pub class_name: &'static str,
    pub confidence: f32,
    pub bbox: [f32; 4],
}

#[cfg(feature = "layout-detection")]
/// PP-DocLayout-M native input size.
const PICODET_INPUT: u32 = 640;

#[cfg(feature = "layout-detection")]
pub struct LayoutDetector {
    graph: Arc<PreparedGraph>,
    compiled: CompiledGraph,
    config: LayoutConfig,
}

#[cfg(feature = "layout-detection")]
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
        Self::from_model(model, config)
    }

    /// Build a detector from an in-memory ONNX payload (e.g. a model embedded
    /// in the binary). `config.model_path` may be empty.
    pub fn from_model_bytes(bytes: &[u8], config: LayoutConfig) -> Result<Self> {
        let model = load_model_from_bytes(bytes).context("failed to load embedded layout model")?;
        Self::from_model(model, config)
    }

    fn from_model(model: ModelProto, config: LayoutConfig) -> Result<Self> {
        let report = ModelReport::from_model(&model).context("failed to inspect layout model")?;
        if !report.rejection_reasons.is_empty() {
            bail!(
                "layout model is not compatible with lege-vision: {}",
                report.rejection_reasons.join("; ")
            );
        }

        let graph = PreparedGraph::from_model(&model).context("failed to prepare layout graph")?;
        if graph.inputs.first().map(String::as_str) != Some("pp_image") {
            bail!(
                "layout model must use PP-DocLayout input `pp_image`, got {:?}",
                graph.inputs
            );
        }

        let compiled = pollster::block_on(CompiledGraph::build_layout(&graph))
            .context("failed to compile layout graph for WGPU")?;

        Ok(Self {
            graph: Arc::new(graph),
            compiled,
            config,
        })
    }

    pub fn from_model_path(model_path: impl Into<PathBuf>) -> Result<Self> {
        Self::new(LayoutConfig::new(model_path))
    }

    /// Build a second inference session that shares this detector's GPU device
    /// and immutable prepared graph but owns its own resident activation and
    /// readback buffers, so it can run one page concurrently with the parent
    /// (Phase 5 GPU session pool). Model weights are re-uploaded per session;
    /// PP-DocLayout-M is small, so a handful of sessions stay well inside the
    /// VRAM budget.
    pub fn build_sibling(&self) -> Result<Self> {
        let compiled = self
            .compiled
            .build_sibling(&self.graph)
            .context("failed to build sibling layout session")?;
        Ok(Self {
            graph: Arc::clone(&self.graph),
            compiled,
            config: self.config.clone(),
        })
    }

    pub fn detect_rgb(&self, image: &RgbImage) -> Result<Vec<LayoutDetection>> {
        let preprocessed = preprocess::stretch_imagenet_rgb(image, PICODET_INPUT)
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

        // Prepared PP-DocLayout heads: outputs[0] = boxes [1,N,4],
        // outputs[1] = scores [1,C,N] (see provenance.json).
        let fetch = |idx: usize| -> Result<&Tensor> {
            let name = self
                .graph
                .outputs
                .get(idx)
                .with_context(|| format!("PicoDet output {idx} missing from graph"))?;
            outputs
                .get(name)
                .with_context(|| format!("PicoDet output `{name}` missing from WGPU result"))
        };
        let boxes = fetch(0)?;
        let scores = fetch(1)?;
        let detections = picodet::decode(
            boxes,
            scores,
            &preprocessed.meta,
            self.config.confidence_threshold,
            self.config.iou_threshold,
            self.config.max_detections,
        )
        .context("failed to decode PicoDet layout heads")?;
        Ok(detections
            .into_iter()
            .map(|det| LayoutDetection {
                class_id: det.class_id as i32,
                class_name: picodet::class_name(det.class_id),
                confidence: det.score,
                bbox: [det.x1, det.y1, det.x2, det.y2],
            })
            .collect())
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

#[cfg(feature = "layout-detection")]
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

#[cfg(feature = "layout-detection")]
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

    fn build_for(
        &self,
        width: u32,
        height: u32,
        related: Option<&CompiledGraph>,
    ) -> Result<SauvolaGraph> {
        let dims = [1, height as i64, width as i64, 1];
        let graph = PreparedGraph::from_model_with_input_dims(&self.model, Some(&dims))
            .with_context(|| format!("failed to prepare sauvola graph for {width}x{height}"))?;
        if graph.inputs.first().map(String::as_str) != Some("img01_inp") {
            bail!(
                "sauvola model must use input `img01_inp`, got {:?}",
                graph.inputs
            );
        }
        let compiled = match related {
            Some(parent) => parent.build_related(&graph),
            None => pollster::block_on(CompiledGraph::build(&graph)),
        }
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
                let entry =
                    self.build_for(width, height, cache.first().map(|entry| &entry.compiled))?;
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
        Self::from_model(model, config)
    }

    /// Build from an in-memory ONNX payload, such as a model embedded with
    /// `include_bytes!`.
    pub fn from_model_bytes(bytes: &[u8]) -> Result<Self> {
        let model =
            load_model_from_bytes(bytes).context("failed to load embedded sauvola model")?;
        Self::from_model(model, SauvolaConfig::new(PathBuf::new()))
    }

    fn from_model(model: ModelProto, config: SauvolaConfig) -> Result<Self> {
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

// ── PP-OCR text-line recognition ──────────────────────────────────────────

/// Fixed input height for PP-OCRv5 mobile recognition.
const REC_HEIGHT: i64 = 48;

/// Width buckets a text-line crop is padded up to before recognition. Lines are
/// run one at a time (batching gives no speedup — the graph is dispatch-bound,
/// not submit-bound), so the only cache lever is reusing a compiled graph across
/// lines of similar width. A crop wider than the largest bucket gets its own
/// exact-width graph (rare; still cached and LRU-evicted).
const REC_WIDTH_BUCKETS: [u32; 5] = [160, 320, 480, 640, 960];

/// Distinct rec widths kept compiled at once (the buckets plus a couple of
/// oversize exact-width graphs).
const REC_CACHE_CAP: usize = REC_WIDTH_BUCKETS.len() + 3;

/// A recognized word with its horizontal pixel span in the source line crop.
#[derive(Debug, Clone)]
pub struct RecWord {
    pub text: String,
    /// Left/right x in the ORIGINAL line-crop pixel coordinates.
    pub x0: u32,
    pub x1: u32,
}

/// Recognition result for one text-line crop: the full line text plus per-word
/// spans reconstructed from the CTC timesteps.
#[derive(Debug, Clone)]
pub struct RecLine {
    pub text: String,
    pub words: Vec<RecWord>,
}

/// A rec graph compiled for one specific input width `[1,3,48,W]`.
struct RecGraph {
    width: u32,
    graph: PreparedGraph,
    compiled: CompiledGraph,
}

/// PP-OCRv5 mobile text-line recognizer over the lege-gpu wgpu runtime.
///
/// Feed a tight text-line crop; it resizes to height 48 (width proportional),
/// pads to the next width bucket, runs the SVTR recognition head on the GPU, and
/// CTC-greedy-decodes the logits against the character dictionary. Compiled
/// graphs are cached per bucket width (MRU-first, capped) so a page's ~30-50
/// lines reuse a handful of graphs instead of recompiling each line.
pub struct RecRecognizer {
    model: ModelProto,
    dict: crate::vision::decode::ctc::CtcDict,
    cache: Mutex<Vec<RecGraph>>,
}

impl RecRecognizer {
    /// Build from raw model bytes (a prepared rec `.onnx`) and dictionary text
    /// (one character per line). Suitable for `include_bytes!` embedding.
    pub fn from_bytes(model_bytes: &[u8], dict_text: &str) -> Result<Self> {
        let model = load_model_from_bytes(model_bytes).context("failed to load rec model bytes")?;
        Self::from_model(model, dict_text)
    }

    /// Build from a prepared rec model file and a dictionary file.
    pub fn from_paths(model_path: impl AsRef<Path>, dict_path: impl AsRef<Path>) -> Result<Self> {
        let model = load_model(model_path.as_ref())
            .with_context(|| format!("failed to load rec model {:?}", model_path.as_ref()))?;
        let dict_text = std::fs::read_to_string(dict_path.as_ref())
            .with_context(|| format!("failed to read rec dict {:?}", dict_path.as_ref()))?;
        Self::from_model(model, &dict_text)
    }

    fn from_model(model: ModelProto, dict_text: &str) -> Result<Self> {
        let report = ModelReport::from_model(&model).context("failed to inspect rec model")?;
        if !report.rejection_reasons.is_empty() {
            bail!(
                "rec model is not compatible with lege-vision: {}",
                report.rejection_reasons.join("; ")
            );
        }
        let dict = crate::vision::decode::ctc::CtcDict::from_dict_text(dict_text);
        Ok(Self {
            model,
            dict,
            cache: Mutex::new(Vec::new()),
        })
    }

    /// Number of character classes the dictionary implies (blank + lines + space).
    pub fn num_classes(&self) -> usize {
        self.dict.num_classes()
    }

    /// Smallest bucket width `>= width`, or `width` itself if it exceeds every
    /// bucket (rounded up to an even value so half-width dispatches stay aligned).
    fn bucket_for(width: u32) -> u32 {
        REC_WIDTH_BUCKETS
            .iter()
            .copied()
            .find(|&b| b >= width)
            .unwrap_or_else(|| width + (width & 1))
    }

    fn build_for(&self, width: u32, related: Option<&CompiledGraph>) -> Result<RecGraph> {
        let dims = [1, 3, REC_HEIGHT, width as i64];
        let graph = PreparedGraph::from_model_with_input_dims(&self.model, Some(&dims))
            .with_context(|| format!("failed to prepare rec graph for width {width}"))?;
        let compiled = match related {
            Some(parent) => parent.build_related(&graph),
            None => pollster::block_on(CompiledGraph::build(&graph)),
        }
        .with_context(|| format!("failed to compile rec graph for width {width}"))?;
        Ok(RecGraph {
            width,
            graph,
            compiled,
        })
    }

    /// Run the rec graph for one line crop, returning `(logits, nw, bucket)`
    /// where `nw` is the unpadded resized width and `bucket` the padded width.
    fn run_preprocessed(&self, tensor: Tensor, nw: u32) -> Result<Option<(Tensor, u32, u32)>> {
        let bucket = Self::bucket_for(nw);
        let padded = pad_rec_width(tensor, nw, bucket);

        // Resolve-or-build the bucket graph under the lock; run + read the output
        // name while still holding it (cheap), then release.
        let (outputs, out_name) = {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("rec cache poisoned"))?;
            match cache.iter().position(|e| e.width == bucket) {
                Some(0) => {}
                Some(index) => {
                    let entry = cache.remove(index);
                    cache.insert(0, entry);
                }
                None => {
                    let entry =
                        self.build_for(bucket, cache.first().map(|entry| &entry.compiled))?;
                    cache.insert(0, entry);
                    cache.truncate(REC_CACHE_CAP);
                }
            }
            let out_name = cache[0]
                .graph
                .outputs
                .first()
                .context("rec graph has no output")?
                .clone();
            let outputs = pollster::block_on(cache[0].compiled.run(&padded))
                .context("rec inference failed")?;
            (outputs, out_name)
        };
        let logits = outputs
            .get(&out_name)
            .with_context(|| format!("rec output `{out_name}` missing"))?
            .clone();
        let classes = match logits.shape.as_slice() {
            [1, _, classes] | [_, classes] => *classes,
            shape => bail!("unexpected rec output shape {shape:?}"),
        };
        if classes != self.dict.num_classes() {
            bail!(
                "rec output has {classes} classes but dictionary requires {}",
                self.dict.num_classes()
            );
        }
        Ok(Some((logits, nw, bucket)))
    }

    fn run_line(&self, line: &RgbImage) -> Result<Option<(Tensor, u32, u32)>> {
        if line.width() == 0 || line.height() == 0 {
            return Ok(None);
        }
        let (tensor, nw) = preprocess::rec_line_tensor(line).context("rec preprocess failed")?;
        self.run_preprocessed(tensor, nw)
    }

    fn run_gray_line(&self, line: &GrayImage) -> Result<Option<(Tensor, u32, u32)>> {
        if line.width() == 0 || line.height() == 0 {
            return Ok(None);
        }
        let (tensor, nw) =
            preprocess::rec_line_tensor_gray(line).context("grayscale rec preprocess failed")?;
        self.run_preprocessed(tensor, nw)
    }

    /// Recognize the text on a single tight text-line crop.
    pub fn recognize(&self, line: &RgbImage) -> Result<String> {
        match self.run_line(line)? {
            None => Ok(String::new()),
            Some((logits, _, _)) => Ok(crate::vision::decode::ctc::ctc_greedy_decode(
                &logits, &self.dict,
            )),
        }
    }

    /// Recognize a line crop, returning the full text plus per-word x-spans in
    /// the crop's own pixel coordinates (reconstructed from the CTC timesteps).
    pub fn recognize_words(&self, line: &RgbImage) -> Result<RecLine> {
        let crop_w = line.width();
        self.decode_words(crop_w, self.run_line(line)?)
    }

    /// Grayscale equivalent of [`Self::recognize_words`], avoiding an RGB
    /// intermediate for line crops originating from cleaned grayscale pages.
    pub fn recognize_words_gray(&self, line: &GrayImage) -> Result<RecLine> {
        self.decode_words(line.width(), self.run_gray_line(line)?)
    }

    fn decode_words(&self, crop_w: u32, output: Option<(Tensor, u32, u32)>) -> Result<RecLine> {
        let Some((logits, nw, bucket)) = output else {
            return Ok(RecLine {
                text: String::new(),
                words: Vec::new(),
            });
        };
        let (spans, timesteps) =
            crate::vision::decode::ctc::ctc_greedy_decode_spans(&logits, &self.dict);

        // Content occupies the first `nw/bucket` fraction of the timesteps (the
        // rest is padding). Map a timestep to an x in the ORIGINAL crop width.
        let content_t = (timesteps as f32 * nw as f32 / bucket.max(1) as f32).max(1.0);
        let step_px = crop_w as f32 / content_t;
        let x_of = |t: usize| -> u32 {
            ((t as f32 / content_t) * crop_w as f32)
                .round()
                .clamp(0.0, crop_w as f32) as u32
        };

        let mut text = String::new();
        let mut words = Vec::new();
        let mut cur = String::new();
        let mut cur_start: Option<usize> = None;
        let mut cur_last = 0usize;
        let flush =
            |cur: &mut String, start: Option<usize>, last: usize, words: &mut Vec<RecWord>| {
                if let Some(s) = start {
                    if !cur.is_empty() {
                        let x0 = x_of(s).min(crop_w.saturating_sub(1));
                        let x1 = (x_of(last) + step_px.ceil() as u32)
                            .clamp(x0.saturating_add(1), crop_w);
                        words.push(RecWord {
                            text: std::mem::take(cur),
                            x0,
                            x1,
                        });
                    }
                }
                cur.clear();
            };
        for span in &spans {
            let glyph = self.dict.char_at(span.class_index);
            text.push_str(glyph);
            if glyph == " " {
                flush(&mut cur, cur_start, cur_last, &mut words);
                cur_start = None;
            } else {
                if cur_start.is_none() {
                    cur_start = Some(span.timestep);
                }
                cur_last = span.timestep;
                cur.push_str(glyph);
            }
        }
        flush(&mut cur, cur_start, cur_last, &mut words);

        Ok(RecLine {
            text: text.trim().to_string(),
            words,
        })
    }

    /// Recognize several line crops. Runs them sequentially — line batching does
    /// not amortize the dispatch-bound cost — but reuses the bucket cache.
    pub fn recognize_lines(&self, lines: &[RgbImage]) -> Result<Vec<String>> {
        lines.iter().map(|line| self.recognize(line)).collect()
    }
}

/// Right-pad an NCHW `[1,3,48,nw]` rec tensor to `[1,3,48,bucket]` with `0.0`
/// (PP-OCR pads the normalized image with zeros). No-op when `nw == bucket`.
fn pad_rec_width(tensor: Tensor, nw: u32, bucket: u32) -> Tensor {
    if nw == bucket {
        return tensor;
    }
    let h = REC_HEIGHT as usize;
    let nw = nw as usize;
    let bw = bucket as usize;
    let mut data = vec![0.0f32; 3 * h * bw];
    for c in 0..3 {
        for y in 0..h {
            let src = (c * h + y) * nw;
            let dst = (c * h + y) * bw;
            data[dst..dst + nw].copy_from_slice(&tensor.data[src..src + nw]);
        }
    }
    Tensor {
        shape: vec![1, 3, h, bw],
        data,
    }
}

// ── PP-OCR text detection (DBNet) ──────────────────────────────────────────

/// Longest side the detector resizes a page down to (PP-OCR `limit_side_len`).
const DET_LIMIT_SIDE: u32 = 960;

/// Distinct det input sizes kept compiled at once. Pages in a job are usually a
/// single size, so a tiny cache suffices (a couple of sizes for mixed jobs).
const DET_CACHE_CAP: usize = 3;

/// An axis-aligned detected text-line box in original-image pixel coordinates.
#[derive(Debug, Clone, Copy)]
pub struct TextBox {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub score: f32,
}

impl TextBox {
    /// Integer crop rect `(x, y, w, h)` clamped to an `img_w × img_h` image.
    pub fn crop_rect(&self, img_w: u32, img_h: u32) -> (u32, u32, u32, u32) {
        let x0 = self.x0.floor().clamp(0.0, img_w as f32) as u32;
        let y0 = self.y0.floor().clamp(0.0, img_h as f32) as u32;
        let x1 = self.x1.ceil().clamp(0.0, img_w as f32) as u32;
        let y1 = self.y1.ceil().clamp(0.0, img_h as f32) as u32;
        (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
    }
}

/// A det graph compiled for one specific input size `[1,3,H,W]`.
struct DetGraph {
    in_w: u32,
    in_h: u32,
    graph: PreparedGraph,
    compiled: CompiledGraph,
}

/// PP-OCRv5 mobile DBNet text detector over the lege-gpu wgpu runtime.
///
/// Feed a page image; it resizes to a 32-aligned size (long side ≤ 960),
/// ImageNet-normalizes, runs the detection head on the GPU to get a text
/// probability map, then DB-post-processes (threshold → connected components →
/// unclip) into axis-aligned text-line boxes mapped back to original coords.
pub struct Detector {
    model: ModelProto,
    db: crate::vision::decode::db::DbConfig,
    limit_side: u32,
    cache: Mutex<Vec<DetGraph>>,
}

impl Detector {
    /// Build from raw prepared-det model bytes (for `include_bytes!` embedding).
    pub fn from_bytes(model_bytes: &[u8]) -> Result<Self> {
        let model = load_model_from_bytes(model_bytes).context("failed to load det model bytes")?;
        Self::from_model(model)
    }

    /// Build from a prepared det model file.
    pub fn from_path(model_path: impl AsRef<Path>) -> Result<Self> {
        let model = load_model(model_path.as_ref())
            .with_context(|| format!("failed to load det model {:?}", model_path.as_ref()))?;
        Self::from_model(model)
    }

    fn from_model(model: ModelProto) -> Result<Self> {
        let report = ModelReport::from_model(&model).context("failed to inspect det model")?;
        if !report.rejection_reasons.is_empty() {
            bail!(
                "det model is not compatible with lege-vision: {}",
                report.rejection_reasons.join("; ")
            );
        }
        Ok(Self {
            model,
            db: crate::vision::decode::db::DbConfig::default(),
            limit_side: DET_LIMIT_SIDE,
            cache: Mutex::new(Vec::new()),
        })
    }

    fn build_for(&self, in_w: u32, in_h: u32, related: Option<&CompiledGraph>) -> Result<DetGraph> {
        let dims = [1, 3, in_h as i64, in_w as i64];
        let graph = PreparedGraph::from_model_with_input_dims(&self.model, Some(&dims))
            .with_context(|| format!("failed to prepare det graph for {in_w}x{in_h}"))?;
        let compiled = match related {
            Some(parent) => parent.build_related(&graph),
            None => pollster::block_on(CompiledGraph::build(&graph)),
        }
        .with_context(|| format!("failed to compile det graph for {in_w}x{in_h}"))?;
        Ok(DetGraph {
            in_w,
            in_h,
            graph,
            compiled,
        })
    }

    /// Detect text-line boxes on a page image.
    pub fn detect(&self, image: &RgbImage) -> Result<Vec<TextBox>> {
        let (iw, ih) = image.dimensions();
        if iw == 0 || ih == 0 {
            return Ok(Vec::new());
        }
        let det = preprocess::det_input_tensor(image, self.limit_side)
            .context("det preprocess failed")?;

        self.detect_preprocessed(det)
    }

    /// Detect text-line boxes on a grayscale page without allocating a
    /// replicated full-page RGB image.
    pub fn detect_gray(&self, image: &GrayImage) -> Result<Vec<TextBox>> {
        let (iw, ih) = image.dimensions();
        if iw == 0 || ih == 0 {
            return Ok(Vec::new());
        }
        let det = preprocess::det_input_tensor_gray(image, self.limit_side)
            .context("grayscale det preprocess failed")?;

        self.detect_preprocessed(det)
    }

    fn detect_preprocessed(&self, det: preprocess::DetInput) -> Result<Vec<TextBox>> {
        let (outputs, out_name) = {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("det cache poisoned"))?;
            match cache
                .iter()
                .position(|e| e.in_w == det.in_w && e.in_h == det.in_h)
            {
                Some(0) => {}
                Some(index) => {
                    let entry = cache.remove(index);
                    cache.insert(0, entry);
                }
                None => {
                    let entry = self.build_for(
                        det.in_w,
                        det.in_h,
                        cache.first().map(|entry| &entry.compiled),
                    )?;
                    cache.insert(0, entry);
                    cache.truncate(DET_CACHE_CAP);
                }
            }
            let out_name = cache[0]
                .graph
                .outputs
                .first()
                .context("det graph has no output")?
                .clone();
            let outputs = pollster::block_on(cache[0].compiled.run(&det.tensor))
                .context("det inference failed")?;
            (outputs, out_name)
        };

        let prob = outputs
            .get(&out_name)
            .with_context(|| format!("det output `{out_name}` missing"))?;
        // Prob map is [1,1,H,W]; take the trailing H,W.
        let (pw, ph) = match prob.shape.as_slice() {
            [_, _, h, w] => (*w, *h),
            [h, w] => (*w, *h),
            other => bail!("unexpected det output shape {other:?}"),
        };
        let db_boxes = crate::vision::decode::db::boxes_from_prob(&prob.data, pw, ph, &self.db);
        Ok(db_boxes
            .into_iter()
            .map(|b| TextBox {
                x0: b.x0 * det.scale_x,
                y0: b.y0 * det.scale_y,
                x1: b.x1 * det.scale_x,
                y1: b.y1 * det.scale_y,
                score: b.score,
            })
            .collect())
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

#[cfg(all(test, feature = "layout-detection"))]
mod pp_doclayout_tests {
    //! Conformance of the PP-DocLayout-M prepared graph through the wgpu runtime
    //! against an ORT-computed golden. Opt-in via env vars (needs a GPU + the
    //! prepared artifact); skips cleanly when they are unset.
    //!
    //!   LEGE_PP_MODEL   = path to doclayout-m.prepared.onnx
    //!   LEGE_PP_GOLDEN  = dir with input_1x3x640x640.f32, boxes_1x8500x4.f32,
    //!                     scores_1x23x8500.f32
    use super::*;
    use crate::vision::onnx::load_model;
    use crate::vision::reference::Tensor;
    use crate::vision::runtime::compiled::CompiledGraph;
    use std::path::PathBuf;

    fn read_f32(path: &std::path::Path) -> Vec<f32> {
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn pp_doclayout_matches_ort_golden() {
        let (Some(model), Some(golden)) = (
            std::env::var_os("LEGE_PP_MODEL"),
            std::env::var_os("LEGE_PP_GOLDEN"),
        ) else {
            eprintln!("skipping pp_doclayout conformance: set LEGE_PP_MODEL and LEGE_PP_GOLDEN");
            return;
        };
        let model_path = PathBuf::from(model);
        let golden = PathBuf::from(golden);

        let model = load_model(&model_path).expect("load prepared PP model");
        let report = ModelReport::from_model(&model).expect("inspect PP model");
        assert!(
            report.rejection_reasons.is_empty(),
            "PP model rejected by bridge: {:?}",
            report.rejection_reasons
        );
        let graph = PreparedGraph::from_model(&model).expect("prepare PP graph");
        assert_eq!(graph.inputs.first().map(String::as_str), Some("pp_image"));
        let compiled =
            pollster::block_on(CompiledGraph::build_layout(&graph)).expect("compile PP graph");

        let input = Tensor::new(
            vec![1, 3, 640, 640],
            read_f32(&golden.join("input_1x3x640x640.f32")),
        )
        .expect("golden input");
        let outputs = pollster::block_on(compiled.run(&input)).expect("run PP graph");

        // outputs[0] = boxes [1,8500,4], outputs[1] = scores [1,23,8500]
        let boxes = outputs.get(&graph.outputs[0]).expect("boxes output");
        let scores = outputs.get(&graph.outputs[1]).expect("scores output");
        assert_eq!(boxes.shape, vec![1, 8500, 4], "boxes shape");
        assert_eq!(scores.shape, vec![1, 23, 8500], "scores shape");

        let gold_boxes = read_f32(&golden.join("boxes_1x8500x4.f32"));
        let gold_scores = read_f32(&golden.join("scores_1x23x8500.f32"));

        let score_diff = max_abs_diff(&scores.data, &gold_scores);
        // Box drift only over anchors that carry a real detection: the other
        // ~8500 background anchors hold meaningless coords whose tiny absolute
        // moves blow up in relative terms (and matter especially for fp16
        // weights). Filter by golden score > 0.3, matching the decode threshold.
        const N: usize = 8500;
        const C: usize = 23;
        let mut box_rel = 0.0f32;
        for i in 0..N {
            let best = (0..C)
                .map(|c| gold_scores[c * N + i])
                .fold(0.0f32, f32::max);
            if best < 0.3 {
                continue;
            }
            for k in 0..4 {
                let (g, o) = (boxes.data[i * 4 + k], gold_boxes[i * 4 + k]);
                box_rel = box_rel.max((g - o).abs() / o.abs().max(1.0));
            }
        }
        eprintln!(
            "PP conformance: max score |diff|={score_diff:.5}, max box rel diff (detections)={box_rel:.5}"
        );

        // GPU f32 accumulation vs ORT CPU (plus fp16 weight rounding if the
        // model is quantized): both stay small on real detections.
        assert!(score_diff < 0.02, "score drift too large: {score_diff}");
        assert!(box_rel < 0.02, "detection box drift too large: {box_rel}");
    }

    /// Exercises the new ReduceMean + AveragePool ops through the whole PP-OCRv5
    /// recognition graph (SVTR), on both the CPU reference executor and the GPU
    /// kernels, against an ORT golden. Env-gated:
    ///   LEGE_REC_MODEL  = rec.prepared.onnx
    ///   LEGE_REC_GOLDEN = dir with input_1x3x48x320.f32, output.f32
    #[test]
    fn pp_rec_ops_match_ort_golden() {
        let (Some(model), Some(golden)) = (
            std::env::var_os("LEGE_REC_MODEL"),
            std::env::var_os("LEGE_REC_GOLDEN"),
        ) else {
            eprintln!("skipping pp_rec conformance: set LEGE_REC_MODEL and LEGE_REC_GOLDEN");
            return;
        };
        let golden = PathBuf::from(golden);
        let model = load_model(&PathBuf::from(model)).expect("load prepared rec model");
        assert!(
            ModelReport::from_model(&model)
                .expect("inspect rec model")
                .rejection_reasons
                .is_empty(),
            "rec model rejected — ReduceMean/AveragePool not accepted?"
        );
        // Stamp the dynamic width to the golden's 320 (batch 1, H 48).
        let graph = PreparedGraph::from_model_with_input_dims(&model, Some(&[1, 3, 48, 320]))
            .expect("prepare rec graph");
        assert_eq!(
            graph.inputs.first().map(String::as_str),
            Some("pp_rec_image")
        );

        let input = Tensor::new(
            vec![1, 3, 48, 320],
            read_f32(&golden.join("input_1x3x48x320.f32")),
        )
        .expect("golden input");
        let gold = read_f32(&golden.join("output.f32"));
        let out_name = &graph.outputs[0];

        // CPU reference path (exercises reduce_sum mean + avgpool2d reference).
        let cpu = graph.run_cpu(&input).expect("run rec on CPU reference");
        let cpu_diff = max_abs_diff(&cpu.get(out_name).expect("cpu out").data, &gold);

        // GPU path (exercises the REDUCESUM mean + AVGPOOL2D kernels).
        let compiled = pollster::block_on(CompiledGraph::build(&graph)).expect("compile rec graph");
        let gpu = pollster::block_on(compiled.run(&input)).expect("run rec on GPU");
        let gpu_diff = max_abs_diff(&gpu.get(out_name).expect("gpu out").data, &gold);

        eprintln!("PP-rec conformance: CPU max|diff|={cpu_diff:.5}  GPU max|diff|={gpu_diff:.5}");
        assert!(cpu_diff < 1e-2, "CPU reference drift too large: {cpu_diff}");
        assert!(gpu_diff < 5e-2, "GPU drift too large: {gpu_diff}");
    }

    /// Levenshtein edit distance over characters (for CER).
    fn edit_distance(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        let mut cur = vec![0usize; b.len() + 1];
        for (i, ca) in a.iter().enumerate() {
            cur[0] = i + 1;
            for (j, cb) in b.iter().enumerate() {
                let cost = usize::from(ca != cb);
                cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        prev[b.len()]
    }

    /// End-to-end Rust recognition parity: preprocess a text-line crop, run the
    /// rec graph on the CPU reference executor, CTC-decode it, and compare the
    /// Rust text against the Python/ORT decode. Requires a near-zero character
    /// error rate rather than byte-parity — the two paths use different resize
    /// kernels (PIL BILINEAR golden vs the `image` crate's Triangle), so a
    /// genuinely ambiguous glyph (a wide em-dash decoding as `--` vs `-`) can
    /// differ by one character without any decode defect. Env-gated:
    ///   LEGE_REC_MODEL = rec.prepared.onnx
    ///   LEGE_REC_DICT  = ppocrv5_dict.txt
    ///   LEGE_REC_LINES = dir with line_NN.png + lines.tsv (png\ttext)
    #[test]
    fn pp_rec_ctc_matches_python() {
        let (Some(model), Some(dict), Some(lines_dir)) = (
            std::env::var_os("LEGE_REC_MODEL"),
            std::env::var_os("LEGE_REC_DICT"),
            std::env::var_os("LEGE_REC_LINES"),
        ) else {
            eprintln!(
                "skipping pp_rec ctc parity: set LEGE_REC_MODEL, LEGE_REC_DICT, LEGE_REC_LINES"
            );
            return;
        };
        let lines_dir = PathBuf::from(lines_dir);
        let model = load_model(&PathBuf::from(model)).expect("load rec model");
        let dict_text = std::fs::read_to_string(PathBuf::from(dict)).expect("read dict");
        let dict = crate::vision::decode::ctc::CtcDict::from_dict_text(&dict_text);

        let tsv = std::fs::read_to_string(lines_dir.join("lines.tsv")).expect("read lines.tsv");
        let mut checked = 0usize;
        let mut exact = 0usize;
        let mut total_edits = 0usize;
        let mut total_chars = 0usize;
        for row in tsv.lines() {
            let Some((png, expected)) = row.split_once('\t') else {
                continue;
            };
            let img = image::open(lines_dir.join(png))
                .unwrap_or_else(|e| panic!("open {png}: {e}"))
                .to_rgb8();
            let (tensor, nw) =
                crate::vision::preprocess::rec_line_tensor(&img).expect("rec preprocess");
            let graph =
                PreparedGraph::from_model_with_input_dims(&model, Some(&[1, 3, 48, nw as i64]))
                    .expect("prepare rec graph");
            let outputs = graph.run_cpu(&tensor).expect("run rec CPU");
            let logits = outputs.get(&graph.outputs[0]).expect("rec output");
            let text = crate::vision::decode::ctc::ctc_greedy_decode(logits, &dict);
            let edits = edit_distance(&text, expected);
            if edits == 0 {
                exact += 1;
            } else {
                eprintln!("  {png}: {edits} edit(s)\n    rust: {text:?}\n    py:   {expected:?}");
            }
            total_edits += edits;
            total_chars += expected.chars().count();
            checked += 1;
        }
        assert!(checked > 0, "no golden lines checked");
        let cer = total_edits as f32 / total_chars.max(1) as f32;
        eprintln!(
            "pp_rec ctc parity: {exact}/{checked} exact, {total_edits} edits / {total_chars} chars \
             (CER {cer:.4})"
        );
        // Decode is proven correct on the exact-match lines; the residual is
        // resize-kernel noise on ambiguous glyphs. Require a very low CER.
        assert!(
            cer < 0.01,
            "rec CER {cer:.4} exceeds 1% — likely a decode defect"
        );
    }

    /// End-to-end `RecRecognizer` on the GPU: preprocess + width-bucket pad +
    /// GPU run + CTC decode, over the golden line crops. Proves the padded
    /// bucket path decodes the same text as the exact-width CPU path. Env-gated:
    ///   LEGE_REC_MODEL = rec.prepared.onnx (dynamic width)
    ///   LEGE_REC_DICT  = ppocrv5_dict.txt
    ///   LEGE_REC_LINES = dir with line_NN.png + lines.tsv
    #[test]
    fn pp_rec_recognizer_gpu_matches_golden() {
        let (Some(model), Some(dict), Some(lines_dir)) = (
            std::env::var_os("LEGE_REC_MODEL"),
            std::env::var_os("LEGE_REC_DICT"),
            std::env::var_os("LEGE_REC_LINES"),
        ) else {
            eprintln!(
                "skipping RecRecognizer GPU test: set LEGE_REC_MODEL, LEGE_REC_DICT, LEGE_REC_LINES"
            );
            return;
        };
        let lines_dir = PathBuf::from(lines_dir);
        let rec = RecRecognizer::from_paths(PathBuf::from(model), PathBuf::from(dict))
            .expect("build RecRecognizer");
        let tsv = std::fs::read_to_string(lines_dir.join("lines.tsv")).expect("read lines.tsv");
        let mut checked = 0usize;
        let mut total_edits = 0usize;
        let mut total_chars = 0usize;
        for row in tsv.lines() {
            let Some((png, expected)) = row.split_once('\t') else {
                continue;
            };
            let img = image::open(lines_dir.join(png))
                .unwrap_or_else(|e| panic!("open {png}: {e}"))
                .to_rgb8();
            let text = rec.recognize(&img).expect("recognize");
            let edits = edit_distance(&text, expected);
            if edits != 0 {
                eprintln!("  {png}: {edits} edit(s)\n    gpu: {text:?}\n    py:  {expected:?}");
            }
            total_edits += edits;
            total_chars += expected.chars().count();
            checked += 1;
        }
        assert!(checked > 0, "no golden lines checked");
        let cer = total_edits as f32 / total_chars.max(1) as f32;
        eprintln!("RecRecognizer GPU: {checked} lines, CER {cer:.4}");
        assert!(cer < 0.01, "RecRecognizer GPU CER {cer:.4} exceeds 1%");
    }

    /// Det prob-map parity vs the ORT golden, then DB box-count sanity. Proves
    /// det preprocessing + GPU inference match Python and the pure-Rust DB
    /// post-proc recovers a comparable number of boxes. Env-gated:
    ///   LEGE_DET_MODEL  = det.prepared.onnx (dynamic H/W)
    ///   LEGE_DET_GOLDEN = dir with input.f32, prob.f32, det.json
    #[test]
    fn pp_det_prob_matches_golden() {
        let (Some(model), Some(golden)) = (
            std::env::var_os("LEGE_DET_MODEL"),
            std::env::var_os("LEGE_DET_GOLDEN"),
        ) else {
            eprintln!("skipping det golden: set LEGE_DET_MODEL and LEGE_DET_GOLDEN");
            return;
        };
        let golden = PathBuf::from(golden);
        let meta = std::fs::read_to_string(golden.join("meta.txt")).expect("read meta.txt");
        let nums: Vec<u64> = meta
            .split_whitespace()
            .map(|s| s.parse().expect("meta int"))
            .collect();
        let (in_w, in_h, n_boxes) = (nums[0] as i64, nums[1] as i64, nums[2] as usize);

        let model = load_model(&PathBuf::from(model)).expect("load det model");
        let graph = PreparedGraph::from_model_with_input_dims(&model, Some(&[1, 3, in_h, in_w]))
            .expect("prepare det graph");
        let input = Tensor::new(
            vec![1, 3, in_h as usize, in_w as usize],
            read_f32(&golden.join("input.f32")),
        )
        .expect("golden det input");
        let gold = read_f32(&golden.join("prob.f32"));
        let out_name = &graph.outputs[0];

        let cpu = graph.run_cpu(&input).expect("run det CPU");
        let cpu_diff = max_abs_diff(&cpu.get(out_name).expect("cpu out").data, &gold);
        let compiled = pollster::block_on(CompiledGraph::build(&graph)).expect("compile det graph");
        let gpu = pollster::block_on(compiled.run(&input)).expect("run det GPU");
        let gpu_diff = max_abs_diff(&gpu.get(out_name).expect("gpu out").data, &gold);
        eprintln!("PP-det prob parity: CPU max|diff|={cpu_diff:.5} GPU max|diff|={gpu_diff:.5}");
        assert!(cpu_diff < 1e-2, "CPU det drift too large: {cpu_diff}");
        assert!(gpu_diff < 5e-2, "GPU det drift too large: {gpu_diff}");

        // DB post-proc on the golden prob map: box count should be close to the
        // Python reference (same thresholds, axis-aligned vs. contour differ
        // slightly at merges/splits).
        let db = crate::vision::decode::db::DbConfig::default();
        let boxes =
            crate::vision::decode::db::boxes_from_prob(&gold, in_w as usize, in_h as usize, &db);
        eprintln!("PP-det DB boxes: rust={} python={n_boxes}", boxes.len());
        let lo = n_boxes.saturating_sub(n_boxes / 4 + 3);
        let hi = n_boxes + n_boxes / 4 + 3;
        assert!(
            (lo..=hi).contains(&boxes.len()),
            "rust box count {} outside [{lo},{hi}] of python {n_boxes}",
            boxes.len()
        );
    }

    /// Full-page `Detector` smoke test: run detection on a page and draw the
    /// boxes for visual comparison against the Python DB overlay. Env-gated:
    ///   LEGE_DET_MODEL = det.prepared.onnx
    ///   LEGE_DET_PAGE  = a page PNG
    ///   LEGE_DET_DRAW  = output PNG path (optional)
    #[test]
    fn pp_detector_page_smoke() {
        let (Some(model), Some(page)) = (
            std::env::var_os("LEGE_DET_MODEL"),
            std::env::var_os("LEGE_DET_PAGE"),
        ) else {
            eprintln!("skipping detector smoke: set LEGE_DET_MODEL and LEGE_DET_PAGE");
            return;
        };
        let det = Detector::from_path(PathBuf::from(model)).expect("build Detector");
        let mut img = image::open(PathBuf::from(page))
            .expect("open page")
            .to_rgb8();
        let boxes = det.detect(&img).expect("detect");
        eprintln!(
            "Detector: {} boxes on {}x{}",
            boxes.len(),
            img.width(),
            img.height()
        );
        assert!(!boxes.is_empty(), "detector found no boxes");
        if let Some(out) = std::env::var_os("LEGE_DET_DRAW") {
            for b in &boxes {
                draw_rect(&mut img, b.x0, b.y0, b.x1, b.y1, [255, 0, 0]);
            }
            img.save(PathBuf::from(out)).expect("save overlay");
        }
    }

    fn draw_rect(img: &mut RgbImage, x0: f32, y0: f32, x1: f32, y1: f32, c: [u8; 3]) {
        let (w, h) = img.dimensions();
        let x0 = x0.max(0.0) as u32;
        let y0 = y0.max(0.0) as u32;
        let x1 = (x1 as u32).min(w.saturating_sub(1));
        let y1 = (y1 as u32).min(h.saturating_sub(1));
        for t in 0..3u32 {
            for x in x0..=x1.min(w - 1) {
                if y0 + t < h {
                    img.put_pixel(x, y0 + t, image::Rgb(c));
                }
                if y1 >= t {
                    img.put_pixel(x, y1 - t, image::Rgb(c));
                }
            }
            for y in y0..=y1.min(h - 1) {
                if x0 + t < w {
                    img.put_pixel(x0 + t, y, image::Rgb(c));
                }
                if x1 >= t {
                    img.put_pixel(x1 - t, y, image::Rgb(c));
                }
            }
        }
    }

    /// Per-line GPU recognition latency — the top integration risk. A page has
    /// ~30-50 text lines; if each needs its own GPU submit + readback the
    /// per-page cost may dominate. This benchmarks single-line runs across the
    /// width buckets a real cache would use, then a batched `[B,3,48,W]` run at a
    /// typical width, to quantify how much batching amortizes submit/barrier
    /// overhead. Env-gated (needs a GPU):
    ///   LEGE_REC_MODEL = rec.prepared.onnx
    ///   LEGE_REC_BENCH = 1
    #[test]
    fn pp_rec_gpu_latency_bench() {
        let (Some(model), Some(_)) = (
            std::env::var_os("LEGE_REC_MODEL"),
            std::env::var_os("LEGE_REC_BENCH"),
        ) else {
            eprintln!("skipping pp_rec latency bench: set LEGE_REC_MODEL and LEGE_REC_BENCH=1");
            return;
        };
        let model = load_model(&PathBuf::from(model)).expect("load rec model");

        // Random-ish but deterministic input so we never optimize to a constant.
        let fill = |n: usize| -> Vec<f32> {
            (0..n)
                .map(|i| ((i as f32 * 0.6180339887).fract() - 0.5) * 2.0)
                .collect()
        };

        let buckets = [160u32, 320, 480, 640, 960];
        const WARMUP: usize = 3;
        const ITERS: usize = 20;

        eprintln!("=== PP-rec GPU latency (single line) ===");
        let mut single_line_320_ms = 0.0f64;
        for &w in &buckets {
            let graph =
                PreparedGraph::from_model_with_input_dims(&model, Some(&[1, 3, 48, w as i64]))
                    .expect("prepare rec graph");
            let t_build = std::time::Instant::now();
            let compiled =
                pollster::block_on(CompiledGraph::build(&graph)).expect("compile rec graph");
            let build_ms = t_build.elapsed().as_secs_f64() * 1000.0;

            let input = Tensor::new(vec![1, 3, 48, w as usize], fill(3 * 48 * w as usize))
                .expect("bench input");
            for _ in 0..WARMUP {
                pollster::block_on(compiled.run(&input)).expect("warmup run");
            }
            let mut times = Vec::with_capacity(ITERS);
            for _ in 0..ITERS {
                let t = std::time::Instant::now();
                pollster::block_on(compiled.run(&input)).expect("timed run");
                times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median = times[times.len() / 2];
            let mean = times.iter().sum::<f64>() / times.len() as f64;
            if w == 320 {
                single_line_320_ms = median;
            }
            eprintln!(
                "  W={w:>4}: build={build_ms:>6.0}ms  run median={median:>6.2}ms mean={mean:>6.2}ms  ({} steps)",
                compiled.step_count()
            );
        }

        // Batched: B lines of the same width in one graph. Amortizes the fixed
        // submit + readback cost across B lines.
        eprintln!("=== PP-rec GPU latency (batched, W=320) ===");
        for &b in &[4usize, 8, 16] {
            let graph =
                PreparedGraph::from_model_with_input_dims(&model, Some(&[b as i64, 3, 48, 320]))
                    .expect("prepare batched rec graph");
            let compiled = pollster::block_on(CompiledGraph::build(&graph))
                .expect("compile batched rec graph");
            let input = Tensor::new(vec![b, 3, 48, 320], fill(b * 3 * 48 * 320))
                .expect("batched bench input");
            for _ in 0..WARMUP {
                pollster::block_on(compiled.run(&input)).expect("warmup run");
            }
            let mut times = Vec::with_capacity(ITERS);
            for _ in 0..ITERS {
                let t = std::time::Instant::now();
                pollster::block_on(compiled.run(&input)).expect("timed run");
                times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median = times[times.len() / 2];
            let per_line = median / b as f64;
            let speedup = if per_line > 0.0 {
                single_line_320_ms / per_line
            } else {
                0.0
            };
            eprintln!(
                "  B={b:>2}: batch median={median:>6.2}ms  per-line={per_line:>5.2}ms  ({speedup:.1}x vs single)"
            );
        }
    }
}
