// policies.rs - Pluggable detection and region policies for processing pipelines
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, AtomicU32, Ordering};

use image::{ImageBuffer, Rgb};

use crate::engine::Detection;
use crate::pipeline::config::{PipelineConfig, RenderedPageData, InferenceResult};
use crate::resize_context::{InferenceResizeSpec, is_in_inference_space, map_bbox_infer_to_page};
use crate::margin;

// Global standard dimensions for margin processing (typically set from first/cover page)
static STANDARD_WIDTH: AtomicU32 = AtomicU32::new(0);
static STANDARD_HEIGHT: AtomicU32 = AtomicU32::new(0);
static STANDARD_DIMS_INITIALIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
            bbox: [0.0, 0.0, page.high_res_image.width() as f32, page.high_res_image.height() as f32],
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
    /// Default behavior is identity transform with remapped detections to page space.
    fn transform(&self, page: &RenderedPageData, inf: &InferenceResult, cfg: &PipelineConfig) -> (ImageBuffer<Rgb<u8>, Vec<u8>>, Vec<Detection>) {
        let mut dets = inf.detections.clone();
        remap_detections_to_page(&mut dets, page.high_res_image.width(), page.high_res_image.height(), cfg);
        ((*inf.high_res_image).clone(), dets)
    }

    /// Map (page + detections + config) into an ordered list of region tasks to process/encode.
    /// Default: if there are detections, create regions per detection; otherwise, whole page.
    fn to_regions(&self, page: &RenderedPageData, inf: &InferenceResult, cfg: &PipelineConfig) -> Vec<RegionTask> {
        if inf.detections.is_empty() {
            vec![RegionTask::whole_page(page, cfg)]
        } else {
            inf.detections.iter().map(|d| RegionTask { page_index: page.index, bbox: d.bbox, binarize: false, prefer_original_quality: true }).collect()
        }
    }
}

/// No-op detection provider for no-layout mode: yields empty detections (or marks as no-detections)
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
            original_width_pts: page.original_width_pts,
            original_height_pts: page.original_height_pts,
            has_no_detections: true,
        }
    }
}

/// Layout-based regions: placeholder implementation for now; uses detections to form regions
pub struct LayoutRegions;
impl RegionPolicy for LayoutRegions {
    fn transform(
        &self,
        page: &RenderedPageData,
        inf: &InferenceResult,
        cfg: &PipelineConfig,
    ) -> (ImageBuffer<Rgb<u8>, Vec<u8>>, Vec<Detection>) {
        // Start with original page image and remap detections to page space
        let mut dets = inf.detections.clone();
        let page_w = page.high_res_image.width();
        let page_h = page.high_res_image.height();
        remap_detections_to_page(&mut dets, page_w, page_h, cfg);

        // Clamp any slightly out-of-bounds boxes and drop degenerate regions
        for d in dets.iter_mut() {
            d.bbox[0] = d.bbox[0].clamp(0.0, page_w.max(1) as f32);
            d.bbox[1] = d.bbox[1].clamp(0.0, page_h.max(1) as f32);
            d.bbox[2] = d.bbox[2].clamp(0.0, page_w.max(1) as f32);
            d.bbox[3] = d.bbox[3].clamp(0.0, page_h.max(1) as f32);
            // Normalize ordering if needed
            if d.bbox[2] < d.bbox[0] {
                let x1 = d.bbox[0];
                let x2 = d.bbox[2];
                d.bbox[0] = x2;
                d.bbox[2] = x1;
            }
            if d.bbox[3] < d.bbox[1] {
                let y1 = d.bbox[1];
                let y2 = d.bbox[3];
                d.bbox[1] = y2;
                d.bbox[3] = y1;
            }
        }

        // Drop zero-area regions after clamping
        dets.retain(|d| (d.bbox[2] - d.bbox[0]) >= 1.0 && (d.bbox[3] - d.bbox[1]) >= 1.0);

        ((*page.high_res_image).clone(), dets)
    }
}

/// No-layout policy: just one full-page region
pub struct NoLayoutFullPage;

impl RegionPolicy for NoLayoutFullPage {}

/// Margin policy: skeleton; real logic should call into crate::margin helpers
pub struct MarginStandardizeAndCenter;

impl RegionPolicy for MarginStandardizeAndCenter {
    fn transform(&self, page: &RenderedPageData, inf: &InferenceResult, cfg: &PipelineConfig) -> (ImageBuffer<Rgb<u8>, Vec<u8>>, Vec<Detection>) {
        // Start from original image and remapped detections
        let mut dets = inf.detections.clone();
        let page_w = page.high_res_image.width();
        let page_h = page.high_res_image.height();
        remap_detections_to_page(&mut dets, page_w, page_h, cfg);

        // Compute content bounds: prefer detection-derived, else pixel-derived
        let bounds = if !dets.is_empty() {
            margin::calculate_content_bounds(&dets, page_w, page_h, /*filter_for_cropping*/ true)
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
                    margin::MarginSettings::StandardizeAndCenter | margin::MarginSettings::CropAndResize => margin::MarginSettings::StandardizeAndCenter,
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
                    Err(_e) => {
                        // Fall through to identity on failure
                    }
                }
            }
        }
        ((*page.high_res_image).clone(), dets)
    }
}


/// PaddleX-backed detection provider using existing InferenceHandle.
pub struct PaddleDetectionProvider {
    pub config: Arc<PipelineConfig>,
    /// Optional inference callback to update progress bar when detection completes for each page
    pub inference_callback: Option<Arc<dyn Fn(usize, usize) + Send + Sync + 'static>>,
    /// Total number of pages being processed
    pub total_pages: usize,
    /// Counter for completed detections (to track progress correctly)
    pub completed_detections: Arc<AtomicUsize>,
    /// Reusable inference handle backed by a long-lived actor thread
    pub inference_handle: Arc<crate::pipeline::inference::InferenceHandle>,
}

#[async_trait::async_trait]
impl DetectionProvider for PaddleDetectionProvider {
    async fn run_detection(&self, page: RenderedPageData) -> InferenceResult {
        // Run detection directly
        let dets = match self.inference_handle.detect(page.index, page.inference_image.clone()).await {
            Ok(d) => d,
            Err(_) => Vec::new(),
        };

        let has_no_detections = dets.is_empty();

        // Update progress tracking
        if let Some(callback) = &self.inference_callback {
            let completed = self.completed_detections.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            callback(completed, self.total_pages);
        }

        InferenceResult {
            index: page.index,
            high_res_image: page.high_res_image.clone(),
            inference_image: page.inference_image.clone(),
            detections: dets,
            text_layer: None,
            original_width_pts: page.original_width_pts,
            original_height_pts: page.original_height_pts,
            has_no_detections,
        }
    }
}

// --- Helpers ---

fn remap_detections_to_page(dets: &mut Vec<Detection>, page_w: u32, page_h: u32, cfg: &PipelineConfig) {
    let spec = InferenceResizeSpec { target: cfg.inference_size(), ..Default::default() };
    for d in dets.iter_mut() {
        if is_in_inference_space(&d.bbox, &spec) {
            d.bbox = map_bbox_infer_to_page(d.bbox, page_w, page_h, &spec);
        }
    }
}

fn compute_pixel_bounds_for_margin(
    image: &image::ImageBuffer<image::Rgb<u8>, Vec<u8>>,
    config: &PipelineConfig,
) -> Option<margin::ContentBounds> {
    use Legencode::types::BinarizationOptions;
    let want_invert_input = config.invert_input();
    let mut want_invert_output = config.binarization().invert;
    if want_invert_input && want_invert_output { want_invert_output = false; }
    let options = BinarizationOptions {
        invert: want_invert_output,
        invert_input: want_invert_input,
        k_factor: config.binarization().k_factor,
        use_heavy_duty: config.binarization().use_heavy_duty && !config.binarization().use_fixed_threshold,
        patch_percentage: config.binarization().patch_percentage,
        no_patch: config.binarization().no_patch,
        use_fixed_threshold: config.binarization().use_fixed_threshold,
        fixed_threshold: config.binarization().fixed_threshold,
    };
    let mut binarized = Legencode::color::binarization::binarize_image_raw(
        image.as_raw(), image.width() as usize, image.height() as usize, &options,
    );
    if binarized.is_empty() { return None; }
    if binarized.iter().any(|&b| b > 1) {
        for v in binarized.iter_mut() { *v = if *v > 128 { 1 } else { 0 }; }
    }
    // Foreground black as 1 for content-bounds function
    for v in binarized.iter_mut() { *v = if *v == 0 { 1 } else { 0 }; }
    margin::calculate_content_bounds_from_binary_mask(&binarized, image.width(), image.height())
}