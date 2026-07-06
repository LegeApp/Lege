// policies.rs — Detection/region policies and inference resize context
//
// Two logical sections:
//   §1  Resize context: coordinate mapping between inference tensor space and page space,
//       YOLO letterbox resize config and image building.
//   §2  Detection & region policies: pluggable strategies consumed by the processing pipelines.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use anyhow::{Result, anyhow};
use image::RgbImage;

use crate::bbox_trace;
use crate::engine::Detection;
use crate::margin;
use crate::pipeline::config::{InferenceResult, PipelineConfig, RenderedPageData};
use crate::pipeline::page_analysis::compute_pixel_bounds_for_margin;
use crate::resize::{ResizeMethod, ResizeParams};

// ════════════════════════════════════════════════════════════════════════════════
// §1  Resize context
// ════════════════════════════════════════════════════════════════════════════════

/// Supported inference resize policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizePolicy {
    /// Direct stretch to target W×H (bilinear).
    Direct,
    /// Preserve aspect ratio with letterbox padding.
    Letterbox,
}

// ── Model-specific resize configs ────────────────────────────────────────────

/// YOLO model resize configuration — letterbox with light-gray border.
#[derive(Debug, Clone)]
pub struct YoloResizeConfig {
    pub target: u32,
    pub border_value: u8,
}

impl Default for YoloResizeConfig {
    fn default() -> Self {
        Self {
            target: 1024,
            border_value: 192,
        }
    }
}

// ── Unified spec ─────────────────────────────────────────────────────────────

/// Unified inference-resize specification carried through the pipeline.
#[derive(Debug, Clone)]
pub struct InferenceResizeSpec {
    pub target: u32,
    pub policy: ResizePolicy,
    /// Padding fill value for Letterbox mode (ignored for Direct).
    pub border_value: u8,
}

impl Default for InferenceResizeSpec {
    fn default() -> Self {
        YoloResizeConfig::default().into()
    }
}

impl From<YoloResizeConfig> for InferenceResizeSpec {
    fn from(c: YoloResizeConfig) -> Self {
        Self {
            target: c.target,
            policy: ResizePolicy::Letterbox,
            border_value: c.border_value,
        }
    }
}

// ── Letterbox math (private) ─────────────────────────────────────────────────

#[inline]
fn letterbox_scale(page_w: u32, page_h: u32, target: u32) -> f32 {
    let t = target as f32;
    (t / page_w as f32).min(t / page_h as f32)
}

#[inline]
fn letterbox_padding(page_w: u32, page_h: u32, target: u32) -> (f32, f32) {
    let scale = letterbox_scale(page_w, page_h, target);
    let rw = page_w as f32 * scale;
    let rh = page_h as f32 * scale;
    ((target as f32 - rw) * 0.5, (target as f32 - rh) * 0.5)
}

// ── Public coordinate helpers ────────────────────────────────────────────────

/// Compute scale factors (inference → page) given page dimensions and spec.
#[inline]
pub fn scale_factors_infer_to_page(
    page_w: u32,
    page_h: u32,
    spec: &InferenceResizeSpec,
) -> (f32, f32) {
    match spec.policy {
        ResizePolicy::Direct => {
            let t = spec.target as f32;
            (page_w as f32 / t, page_h as f32 / t)
        }
        ResizePolicy::Letterbox => {
            let scale = letterbox_scale(page_w, page_h, spec.target);
            let inv = if scale > 0.0 { 1.0 / scale } else { 1.0 };
            (inv, inv)
        }
    }
}

/// Map bbox from inference (0..target) space to page coordinates.
pub fn map_bbox_infer_to_page(
    b: [f32; 4],
    page_w: u32,
    page_h: u32,
    spec: &InferenceResizeSpec,
) -> [f32; 4] {
    let out = match spec.policy {
        ResizePolicy::Direct => {
            let (sx, sy) = scale_factors_infer_to_page(page_w, page_h, spec);
            [
                (b[0] * sx).clamp(0.0, page_w as f32),
                (b[1] * sy).clamp(0.0, page_h as f32),
                (b[2] * sx).clamp(0.0, page_w as f32),
                (b[3] * sy).clamp(0.0, page_h as f32),
            ]
        }
        ResizePolicy::Letterbox => {
            let (pad_x, pad_y) = letterbox_padding(page_w, page_h, spec.target);
            let scale = letterbox_scale(page_w, page_h, spec.target);
            let inv = if scale > 0.0 { 1.0 / scale } else { 1.0 };
            [
                ((b[0] - pad_x) * inv).clamp(0.0, page_w as f32),
                ((b[1] - pad_y) * inv).clamp(0.0, page_h as f32),
                ((b[2] - pad_x) * inv).clamp(0.0, page_w as f32),
                ((b[3] - pad_y) * inv).clamp(0.0, page_h as f32),
            ]
        }
    };
    bbox_trace!(
        "[REMAP] {:?} infer({b:?}) page={page_w}x{page_h} -> out({out:?})",
        spec.policy
    );
    out
}

/// Heuristic: bbox might still be in square inference tensor space.
#[inline]
pub fn is_in_inference_space(b: &[f32; 4], spec: &InferenceResizeSpec) -> bool {
    b[2] <= spec.target as f32 + 1.0 && b[3] <= spec.target as f32 + 1.0
}

/// Conditionally remap only when the bbox appears to be in inference space.
#[inline]
pub fn maybe_remap_bbox_from_infer(
    b: [f32; 4],
    page_w: u32,
    page_h: u32,
    spec: &InferenceResizeSpec,
) -> [f32; 4] {
    if is_in_inference_space(&b, spec) && (page_w > spec.target || page_h > spec.target) {
        map_bbox_infer_to_page(b, page_w, page_h, spec)
    } else {
        b
    }
}

// ── Inference image builder ──────────────────────────────────────────────────

/// Build the inference-sized image from a high-res page render.
/// `Direct` stretches to target×target.  `Letterbox` preserves aspect ratio
/// with `spec.border_value` padding.
pub fn build_inference_image(high_res: &RgbImage, spec: &InferenceResizeSpec) -> Result<RgbImage> {
    let (src_w, src_h) = (high_res.width(), high_res.height());
    let t = spec.target;

    match spec.policy {
        ResizePolicy::Direct => {
            let params = ResizeParams {
                target_width: t,
                target_height: t,
                method: ResizeMethod::Bilinear,
                letterbox: false,
                border_value: spec.border_value as f32,
                swap_rb: false,
            };
            let resized = crate::resize::resize_bytes(high_res.as_raw(), src_w, src_h, &params, 3)
                .map_err(|e| anyhow!("Inference resize failed: {e}"))?;
            RgbImage::from_raw(t, t, resized)
                .ok_or_else(|| anyhow!("Failed to build inference image buffer"))
        }
        ResizePolicy::Letterbox => {
            let scale = letterbox_scale(src_w, src_h, t);
            let rw = ((src_w as f32 * scale).round() as u32).max(1).min(t);
            let rh = ((src_h as f32 * scale).round() as u32).max(1).min(t);

            let params = ResizeParams {
                target_width: rw,
                target_height: rh,
                method: ResizeMethod::Bilinear,
                letterbox: false,
                border_value: spec.border_value as f32,
                swap_rb: false,
            };
            let resized = crate::resize::resize_bytes(high_res.as_raw(), src_w, src_h, &params, 3)
                .map_err(|e| anyhow!("Letterbox resize failed: {e}"))?;

            let bv = spec.border_value;
            let mut buf = vec![bv; (t * t * 3) as usize];

            let pad_x = (t - rw) / 2;
            let pad_y = (t - rh) / 2;
            let stride_src = (rw * 3) as usize;
            let stride_dst = (t * 3) as usize;
            for row in 0..rh as usize {
                let s = row * stride_src;
                let d = (pad_y as usize + row) * stride_dst + pad_x as usize * 3;
                buf[d..d + stride_src].copy_from_slice(&resized[s..s + stride_src]);
            }

            RgbImage::from_raw(t, t, buf)
                .ok_or_else(|| anyhow!("Failed to build letterboxed inference image"))
        }
    }
}

// ── Page-geometry adjustments ────────────────────────────────────────────────

/// Margin correction: offset + scale for mapping original page space
/// to the margin-corrected page space.
#[derive(Debug, Clone, Default)]
pub struct MarginCorrection {
    pub offset_x: f32,
    pub offset_y: f32,
    pub scale_x: f32,
    pub scale_y: f32,
}

impl MarginCorrection {
    pub fn new(offset_x: f32, offset_y: f32, scale_x: f32, scale_y: f32) -> Self {
        Self {
            offset_x,
            offset_y,
            scale_x,
            scale_y,
        }
    }
}

/// Apply margin corrections to a page-space bbox.
#[inline]
pub fn apply_page_adjustments(b: [f32; 4], margin: Option<&MarginCorrection>) -> [f32; 4] {
    let mut r = b;
    if let Some(m) = margin {
        r[0] = b[0] * m.scale_x + m.offset_x;
        r[1] = b[1] * m.scale_y + m.offset_y;
        r[2] = b[2] * m.scale_x + m.offset_x;
        r[3] = b[3] * m.scale_y + m.offset_y;
    }
    r
}

/// Full transform pipeline: inference bbox → remap → page adjustments → final page bbox.
#[inline]
pub fn full_infer_bbox_to_final_page(
    b: [f32; 4],
    page_w: u32,
    page_h: u32,
    spec: &InferenceResizeSpec,
    margin: Option<&MarginCorrection>,
) -> [f32; 4] {
    let bb = map_bbox_infer_to_page(b, page_w, page_h, spec);
    apply_page_adjustments(bb, margin)
}

// ════════════════════════════════════════════════════════════════════════════════
// §2  Detection & region policies
// ════════════════════════════════════════════════════════════════════════════════

// Global standard dimensions for margin processing (typically set from first/cover page)
static STANDARD_WIDTH: AtomicU32 = AtomicU32::new(0);
static STANDARD_HEIGHT: AtomicU32 = AtomicU32::new(0);
static STANDARD_DIMS_INITIALIZED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Reset standard dimensions (called at start of new document processing)
pub fn reset_standard_dimensions() {
    STANDARD_WIDTH.store(0, Ordering::Relaxed);
    STANDARD_HEIGHT.store(0, Ordering::Relaxed);
    STANDARD_DIMS_INITIALIZED.store(false, Ordering::Relaxed);
}

/// Set standard dimensions from the first rendered page (thread-safe, only sets once)
pub fn set_standard_dimensions_once(width: u32, height: u32) {
    if !STANDARD_DIMS_INITIALIZED.load(Ordering::Relaxed) {
        if STANDARD_DIMS_INITIALIZED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            STANDARD_WIDTH.store(width, Ordering::Relaxed);
            STANDARD_HEIGHT.store(height, Ordering::Relaxed);
            #[cfg(feature = "debug-logging")]
            crate::info_log!("Standard dimensions set: {}x{}", width, height);
        }
    }
}

pub fn standard_dimensions() -> (u32, u32) {
    (
        STANDARD_WIDTH.load(Ordering::Relaxed),
        STANDARD_HEIGHT.load(Ordering::Relaxed),
    )
}

/// Neutral unit of work for the processor: what area to render/encode and how.
#[derive(Debug, Clone)]
pub struct RegionTask {
    pub page_index: usize,
    /// Bounding box in rendered pixel space: [x1, y1, x2, y2]
    pub bbox: [f32; 4],
    /// Whether this region should be binarized for the text base layer
    pub binarize: bool,
    /// Optional hint to prefer original-quality color encoding (e.g., images/covers)
    pub prefer_original_quality: bool,
}

impl RegionTask {
    /// Convenience: full-page region for no-layout flows
    pub fn whole_page(page: &RenderedPageData, _cfg: &PipelineConfig) -> Self {
        RegionTask {
            page_index: page.index,
            bbox: [
                0.0,
                0.0,
                page.high_res_image.width() as f32,
                page.high_res_image.height() as f32,
            ],
            binarize: true,
            prefer_original_quality: false,
        }
    }
}

#[async_trait::async_trait]
pub trait DetectionProvider: Send + Sync {
    /// Process a single page and return its InferenceResult.
    async fn run_detection(&self, page: RenderedPageData) -> InferenceResult;
}

pub trait RegionPolicy: Send + Sync {
    /// Transform the page image and detections (e.g., margin crop/center) and return adjusted outputs.
    fn transform(
        &self,
        page: &RenderedPageData,
        inf: &InferenceResult,
        cfg: &PipelineConfig,
    ) -> (RgbImage, Vec<Detection>) {
        let mut dets = inf.detections.clone();
        remap_detections_to_page(
            &mut dets,
            page.high_res_image.width(),
            page.high_res_image.height(),
            cfg,
        );
        ((*inf.high_res_image).clone(), dets)
    }

    /// Map (page + detections + config) into an ordered list of region tasks to process/encode.
    fn to_regions(
        &self,
        page: &RenderedPageData,
        inf: &InferenceResult,
        cfg: &PipelineConfig,
    ) -> Vec<RegionTask> {
        if inf.detections.is_empty() {
            vec![RegionTask::whole_page(page, cfg)]
        } else {
            inf.detections
                .iter()
                .map(|d| RegionTask {
                    page_index: page.index,
                    bbox: d.bbox,
                    binarize: false,
                    prefer_original_quality: true,
                })
                .collect()
        }
    }
}

/// No-op detection provider for no-layout mode: yields empty detections.
pub struct NoOpDetectionProvider;

#[async_trait::async_trait]
impl DetectionProvider for NoOpDetectionProvider {
    async fn run_detection(&self, page: RenderedPageData) -> InferenceResult {
        InferenceResult {
            index: page.index,
            high_res_image: page.high_res_image.clone(),
            inference_image: page.inference_image.clone(),
            detections: Vec::new(),
            text_layer: None,
            detections_are_page_space: false,
            original_width_pts: page.original_width_pts,
            original_height_pts: page.original_height_pts,
            has_no_detections: true,
        }
    }
}

/// Layout-based regions: uses detections to form regions.
pub struct LayoutRegions;
impl RegionPolicy for LayoutRegions {
    fn transform(
        &self,
        page: &RenderedPageData,
        inf: &InferenceResult,
        cfg: &PipelineConfig,
    ) -> (RgbImage, Vec<Detection>) {
        let mut dets = inf.detections.clone();
        let page_w = page.high_res_image.width();
        let page_h = page.high_res_image.height();
        remap_detections_to_page(&mut dets, page_w, page_h, cfg);

        for d in dets.iter_mut() {
            d.bbox[0] = d.bbox[0].clamp(0.0, page_w.max(1) as f32);
            d.bbox[1] = d.bbox[1].clamp(0.0, page_h.max(1) as f32);
            d.bbox[2] = d.bbox[2].clamp(0.0, page_w.max(1) as f32);
            d.bbox[3] = d.bbox[3].clamp(0.0, page_h.max(1) as f32);
            if d.bbox[2] < d.bbox[0] {
                d.bbox.swap(0, 2);
            }
            if d.bbox[3] < d.bbox[1] {
                d.bbox.swap(1, 3);
            }
        }

        dets.retain(|d| (d.bbox[2] - d.bbox[0]) >= 1.0 && (d.bbox[3] - d.bbox[1]) >= 1.0);

        ((*page.high_res_image).clone(), dets)
    }
}

/// No-layout policy: just one full-page region.
pub struct NoLayoutFullPage;
impl RegionPolicy for NoLayoutFullPage {}

/// Margin policy: skeleton; real logic calls into crate::margin helpers.
pub struct MarginStandardizeAndCenter;

impl RegionPolicy for MarginStandardizeAndCenter {
    fn transform(
        &self,
        page: &RenderedPageData,
        inf: &InferenceResult,
        cfg: &PipelineConfig,
    ) -> (RgbImage, Vec<Detection>) {
        let mut dets = inf.detections.clone();
        let page_w = page.high_res_image.width();
        let page_h = page.high_res_image.height();
        remap_detections_to_page(&mut dets, page_w, page_h, cfg);

        let bounds = if !dets.is_empty() {
            margin::calculate_content_bounds(&dets, page_w, page_h, true)
        } else {
            compute_pixel_bounds_for_margin(&page.high_res_image, cfg)
        };

        if let Some(bounds) = bounds {
            let dims = margin::StandardPageDimensions {
                width: STANDARD_WIDTH.load(Ordering::Relaxed),
                height: STANDARD_HEIGHT.load(Ordering::Relaxed),
            };
            if dims.width > 0 && dims.height > 0 {
                let setting = match cfg.margin_settings() {
                    margin::MarginSettings::StandardizeAndCenter
                    | margin::MarginSettings::CropAndResize => {
                        margin::MarginSettings::StandardizeAndCenter
                    }
                    margin::MarginSettings::None => margin::MarginSettings::None,
                };
                match margin::process_page_margins(
                    &page.high_res_image,
                    &bounds,
                    setting,
                    &dims,
                    cfg.target_width(),
                    cfg.target_height(),
                ) {
                    Ok(img) => {
                        let new_dets = margin::transform_detections(
                            &dets,
                            &bounds,
                            setting,
                            &dims,
                            cfg.target_width(),
                            cfg.target_height(),
                            Some((page_w, page_h)),
                        );
                        return (img, new_dets);
                    }
                    Err(_) => {}
                }
            }
        }
        ((*page.high_res_image).clone(), dets)
    }
}

/// YOLO-backed detection provider using existing InferenceHandle.
pub struct YoloDetectionProvider {
    pub config: Arc<PipelineConfig>,
    pub inference_callback: Option<Arc<dyn Fn(usize, usize) + Send + Sync + 'static>>,
    pub total_pages: usize,
    pub completed_detections: Arc<AtomicUsize>,
    pub inference_handle: Arc<crate::pipeline::inference::InferenceHandle>,
}

#[async_trait::async_trait]
impl DetectionProvider for YoloDetectionProvider {
    async fn run_detection(&self, page: RenderedPageData) -> InferenceResult {
        let dets = match self
            .inference_handle
            .detect(page.index, page.inference_image.clone())
            .await
        {
            Ok(d) => d,
            Err(_) => Vec::new(),
        };

        let has_no_detections = dets.is_empty();

        if let Some(callback) = &self.inference_callback {
            let completed = self
                .completed_detections
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            callback(completed, self.total_pages);
        }

        InferenceResult {
            index: page.index,
            high_res_image: page.high_res_image.clone(),
            inference_image: page.inference_image.clone(),
            detections: dets,
            text_layer: None,
            detections_are_page_space: false,
            original_width_pts: page.original_width_pts,
            original_height_pts: page.original_height_pts,
            has_no_detections,
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

pub(crate) fn remap_detections_to_page(
    dets: &mut [Detection],
    page_w: u32,
    page_h: u32,
    cfg: &PipelineConfig,
) {
    let spec = cfg.inference_resize_spec();
    for d in dets.iter_mut() {
        if is_in_inference_space(&d.bbox, &spec) {
            d.bbox = map_bbox_infer_to_page(d.bbox, page_w, page_h, &spec);
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// Binarization
// ════════════════════════════════════════════════════════════════════════════════

/// Build `BinarizationOptions` from the pipeline config.
///
/// When `force_blank_threshold` is set, overrides to a fixed high threshold
/// so blank pages binarize to all-white rather than all-noise.
pub fn binarize_options_for(
    config: &PipelineConfig,
    force_blank_threshold: bool,
) -> Legencode::types::BinarizationOptions {
    let want_invert_input = config.invert_input();
    let mut want_invert_output = config.binarization().invert;
    if want_invert_input && want_invert_output {
        want_invert_output = false;
    }
    let (use_fixed_threshold, fixed_threshold) = if force_blank_threshold {
        (
            true,
            crate::pipeline::page_analysis::BLANK_PAGE_FALLBACK_THRESHOLD,
        )
    } else {
        (
            config.binarization().use_fixed_threshold,
            config.binarization().fixed_threshold,
        )
    };
    Legencode::types::BinarizationOptions {
        invert: want_invert_output,
        invert_input: want_invert_input,
        k_factor: config.binarization().k_factor,
        use_heavy_duty: config.binarization().use_heavy_duty && !use_fixed_threshold,
        patch_percentage: config.binarization().patch_percentage,
        no_patch: config.binarization().no_patch,
        use_fixed_threshold,
        fixed_threshold,
        // GPU binarization is safe in crop mode again: the black-page cause (Otsu
        // computed on the raw rather than bg-normalized histogram) is fixed, and the
        // GPU fused output is now bit-exact to the CPU path in the interior (see
        // gpu_fused_parity_debug0). Previously forced CPU via `config.crop_free_aspect()`.
        disable_gpu: false,
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_direct_scale_and_map() {
        let spec = InferenceResizeSpec {
            target: 640,
            policy: ResizePolicy::Direct,
            border_value: 0,
        };
        let mapped = map_bbox_infer_to_page([0.0, 0.0, 640.0, 640.0], 1280, 1920, &spec);
        assert_eq!(mapped, [0.0, 0.0, 1280.0, 1920.0]);
    }

    #[test]
    fn test_maybe_remap() {
        let spec = InferenceResizeSpec {
            target: 640,
            policy: ResizePolicy::Direct,
            border_value: 0,
        };
        let b = maybe_remap_bbox_from_infer([10.0, 20.0, 30.0, 40.0], 2560, 1600, &spec);
        assert!(b[0] > 10.0 && b[1] > 20.0);
    }

    #[test]
    fn test_letterbox_mapping() {
        let spec: InferenceResizeSpec = YoloResizeConfig {
            target: 1024,
            ..Default::default()
        }
        .into();
        let mapped = map_bbox_infer_to_page([0.0, 128.0, 1024.0, 896.0], 1000, 750, &spec);
        let expected = [0.0f32, 0.0, 1000.0, 750.0];
        for (a, b) in mapped.iter().zip(expected.iter()) {
            assert!(
                (a - b).abs() < 0.01,
                "mapped={mapped:?} expected={expected:?}"
            );
        }
    }
}
