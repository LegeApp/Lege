// config.rs
// Pipeline configuration and related utilities

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, anyhow};

// Import perf_log macro from the crate root
#[allow(unused_imports)]
use crate::perf_log;
use image::RgbImage;

use super::pdf_tokio_pipeline::create_and_run_pdf_tokio_pipeline;
use crate::color::BinarizationConfig;
use crate::encoding::Jbig2Mode;
use crate::pipeline::policies::{InferenceResizeSpec, PaddleResizeConfig};
use crate::types::CoverFormat;

pub use crate::color::ImageRegionDitherMode;

#[derive(Debug)]
pub struct PageTask {
    pub index: usize,
    pub total_pages: usize,
    pub pdf_bytes: Arc<[u8]>,
}
/// Detection results from inference worker
#[derive(Debug, Clone)]
pub struct InferenceResult {
    pub index: usize,
    pub high_res_image: Arc<RgbImage>,
    pub inference_image: Arc<RgbImage>,
    pub detections: Vec<crate::engine::Detection>,
    pub text_layer: Option<String>,
    pub detections_are_page_space: bool,
    // Legacy fields still used by margin mode
    pub original_width_pts: f32,
    pub original_height_pts: f32,
    pub has_no_detections: bool,
}

/// Batch of rendered pages for batch inference
#[derive(Debug)]
pub struct BatchRenderedData {
    pub pages: Vec<RenderedPageData>,
    pub page_indices: Vec<usize>, // Track which page each item corresponds to
}

/// Batch of inference results
#[derive(Debug)]
pub struct BatchInferenceResult {
    pub results: Vec<InferenceResult>,
}

#[derive(Debug, Clone)]
pub struct RenderedPageData {
    pub index: usize,
    pub high_res_image: Arc<RgbImage>,
    pub inference_image: Arc<RgbImage>, // bounded layout-analysis surface for PP-DocLayout
    pub layout_detection_enabled: bool,
    pub original_width_pts: f32,  // Original PDF page width in points
    pub original_height_pts: f32, // Original PDF page height in points
}
impl Default for PipelineConfig {
    fn default() -> Self {
        let mut config = PipelineConfig::new()
            .unwrap_or_else(|e| panic!("Failed to create default config: {}", e));
        config.invert_input = false;
        config.max_retries = 3;
        config.retry_delay_ms = 1000;
        config.batch_size = 2;
        config.batch_timeout_ms = 2000; // Reduced from 5000ms for faster final batch processing
        config
    }
}

#[derive(Clone, Debug)]
pub struct PageRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Default)]
pub struct PageSelection {
    ranges: Vec<PageRange>,
}

pub struct ProcessingPipeline {
    pdf_bytes: Arc<[u8]>,
    config: PipelineConfig,
    page_count: usize,
}

impl ProcessingPipeline {
    pub fn new(pdf_bytes: Vec<u8>, config: PipelineConfig) -> Result<Self> {
        let pdf_bytes: Arc<[u8]> = Arc::from(pdf_bytes.into_boxed_slice());
        let document = lege_pdf_read::RenderSession::open(Arc::clone(&pdf_bytes), None)
            .map_err(|error| anyhow!("Failed to read PDF document: {error}"))?;
        let page_count = document.page_count() as usize;
        Ok(Self {
            pdf_bytes,
            config,
            page_count,
        })
    }

    pub fn from_file(input_path: std::path::PathBuf, config: PipelineConfig) -> Result<Self> {
        // The large-file arm used to map the file and immediately `to_vec()` the
        // mapping, which copies the whole thing onto the heap anyway — a
        // page-fault storm plus an `unsafe` block to reach the same Vec that one
        // sized `read` produces, with a SIGBUS risk if the file is truncated
        // underneath us. `Self::new` takes ownership of the bytes, so there is
        // no borrowed-mapping variant to keep.
        let pdf_bytes = std::fs::read(&input_path)?;

        Self::new(pdf_bytes, config)
    }

    pub fn get_page_count(&self) -> usize {
        self.page_count
    }

    pub fn get_provider_name(&self) -> String {
        "WGPU (initialized by inference actor)".to_string()
    }

    pub async fn process_document_dag<F>(
        &self,
        output_path: &std::path::Path,
        progress_tracker: &crate::progress::ProgressTracker,
        shutdown_rx: tokio::sync::broadcast::Receiver<super::helper_functions::ShutdownSignal>,
        progress_callback: F,
    ) -> Result<()>
    where
        F: Fn(usize, usize) + Send + Sync + 'static,
    {
        let pipeline_start = Instant::now();
        crate::info_log!("Initializing processing pipeline...");

        // Determine which pages to process
        let page_range = match &self.config.page_range {
            Some(range) => {
                let start = range.start.saturating_sub(1);
                let end = range.end.min(self.page_count);
                // An out-of-document range must fail here: passing an inverted
                // range (start > end) downstream underflows page arithmetic in
                // release builds and aborts with a capacity-overflow panic.
                if start >= end {
                    return Err(anyhow!(
                        "Requested page range {}-{} is outside the document ({} pages)",
                        range.start,
                        range.end,
                        self.page_count
                    ));
                }
                Some(start..end)
            }
            None => Some(0..self.page_count),
        };

        let total_pages = page_range
            .as_ref()
            .map(|r| r.len())
            .unwrap_or(self.page_count);
        crate::info_log!("Processing {} pages...", total_pages);

        // Update progress to indicate pipeline start (new progress API)
        progress_tracker.update(crate::progress::ProcessingStatus::Initializing);

        // Route to separate pipelines based on output format
        if self.config.text_format() == "epub" {
            crate::info_log!("Using EPUB pipeline");
            super::epub_pipeline::create_and_run_epub_pipeline(
                self.pdf_bytes.clone(),
                Arc::new(self.config.clone()),
                output_path,
                page_range,
                progress_tracker,
                shutdown_rx,
                progress_callback,
            )
            .await?;
        } else if self.config.text_format() == "djvu" {
            // Use standalone DJVU pipeline (simplified tokio version)
            crate::info_log!("Using standalone DJVU pipeline");
            super::djvu_pipeline::create_and_run_djvu_pipeline(
                self.pdf_bytes.clone(),
                Arc::new(self.config.clone()),
                output_path,
                page_range,
                progress_tracker,
                shutdown_rx,
                progress_callback,
            )
            .await?;
        } else {
            // Use new tokio-based PDF pipeline for all PDF-based formats (jbig2, ccitt4, jpeg)
            crate::info_log!("Using new tokio-based PDF pipeline");
            create_and_run_pdf_tokio_pipeline(
                self.pdf_bytes.clone(),
                Arc::new(self.config.clone()),
                output_path,
                page_range,
                progress_tracker,
                shutdown_rx,
                progress_callback,
            )
            .await?;
        }

        crate::success_log!("Processing completed successfully");
        crate::perf_log!(pipeline_start, "Overall processing completed");
        Ok(())
    }
}

impl PageRange {
    pub fn new(start: usize, end: usize) -> Result<Self> {
        if start == 0 || end == 0 {
            return Err(anyhow!("Page numbers must start from 1"));
        }
        if start > end {
            return Err(anyhow!(
                "Start page ({}) cannot be greater than end page ({})",
                start,
                end
            ));
        }
        Ok(Self { start, end })
    }

    pub fn parse(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        // A single page number is a one-page range. Help advertises `5` as a
        // PAGE-RANGE form; requiring `5-5` made `lege book.pdf 5` and
        // `lege book.pdf 195 2000` fail or process the whole book.
        if let Ok(page) = trimmed.parse::<usize>() {
            return Self::new(page, page);
        }
        let Some((start, end)) = trimmed.split_once('-') else {
            return Err(anyhow!(
                "Page range must be a page number or 'start-end', got '{trimmed}'"
            ));
        };
        if start.is_empty() || end.is_empty() || end.contains('-') {
            return Err(anyhow!(
                "Page range must be a page number or 'start-end', got '{trimmed}'"
            ));
        }
        let start: usize = start
            .parse()
            .map_err(|_| anyhow!("Invalid start page number: {}", start))?;
        let end: usize = end
            .parse()
            .map_err(|_| anyhow!("Invalid end page number: {}", end))?;
        Self::new(start, end)
    }

    pub fn contains(&self, page_index_0based: usize) -> bool {
        let page_num_1based = page_index_0based + 1;
        page_num_1based >= self.start && page_num_1based <= self.end
    }

    pub fn page_count(&self) -> usize {
        self.end - self.start + 1
    }
}

impl PageSelection {
    pub fn parse(s: &str) -> Result<Self> {
        Self::parse_with_page_range(s, None)
    }

    pub fn parse_with_page_range(s: &str, page_range: Option<&PageRange>) -> Result<Self> {
        let mut ranges = Vec::new();
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }

            let (start, end) = if let Some((start, end)) = part.split_once('-') {
                let start: usize = start
                    .trim()
                    .parse()
                    .map_err(|_| anyhow!("Invalid start page number: {}", start.trim()))?;
                let end: usize = end
                    .trim()
                    .parse()
                    .map_err(|_| anyhow!("Invalid end page number: {}", end.trim()))?;
                (start, end)
            } else {
                let page: usize = part
                    .parse()
                    .map_err(|_| anyhow!("Invalid page number: {}", part))?;
                (page, page)
            };
            if start == 0 || end == 0 {
                return Err(anyhow!("Page numbers must start from 1"));
            }
            if start > end {
                return Err(anyhow!(
                    "Start page ({}) cannot be greater than end page ({})",
                    start,
                    end
                ));
            }
            let (start, end) = match page_range {
                Some(selected) if !range_is_inside(start, end, selected) => {
                    let shifted_start = selected.start.saturating_add(start);
                    let shifted_end = selected.start.saturating_add(end);
                    if shifted_start > selected.end || shifted_end > selected.end {
                        return Err(anyhow!(
                            "Layout exclusion pages {}-{} are outside selected page range {}-{}",
                            start,
                            end,
                            selected.start,
                            selected.end
                        ));
                    }
                    (shifted_start, shifted_end)
                }
                _ => (start, end),
            };
            let range = PageRange::new(start, end)?;
            ranges.push(range);
        }

        if ranges.is_empty() {
            return Err(anyhow!("Page selection cannot be empty"));
        }

        Ok(Self { ranges })
    }

    pub fn contains(&self, page_index_0based: usize) -> bool {
        self.ranges
            .iter()
            .any(|range| range.contains(page_index_0based))
    }
}

fn range_is_inside(start: usize, end: usize, page_range: &PageRange) -> bool {
    start >= page_range.start && end <= page_range.end
}

#[cfg(test)]
mod page_selection_tests {
    use super::{PageRange, PageSelection};

    #[test]
    fn layout_exclusion_accepts_relative_pages_for_selected_range() {
        let selected = PageRange::new(300, 400).unwrap();
        let exclusion = PageSelection::parse_with_page_range("30-50", Some(&selected)).unwrap();

        assert!(exclusion.contains(329));
        assert!(exclusion.contains(349));
        assert!(!exclusion.contains(328));
    }

    #[test]
    fn layout_exclusion_keeps_absolute_pages_inside_selected_range() {
        let selected = PageRange::new(300, 400).unwrap();
        let exclusion = PageSelection::parse_with_page_range("330-350", Some(&selected)).unwrap();

        assert!(exclusion.contains(329));
        assert!(exclusion.contains(349));
        assert!(!exclusion.contains(328));
    }

    #[test]
    fn layout_exclusion_rejects_relative_pages_outside_selected_range() {
        let selected = PageRange::new(300, 400).unwrap();

        assert!(PageSelection::parse_with_page_range("120-130", Some(&selected)).is_err());
    }

    #[test]
    fn page_range_parse_accepts_a_single_page_number() {
        let range = PageRange::parse("5").unwrap();
        assert_eq!(range.start, 5);
        assert_eq!(range.end, 5);
        assert!(range.contains(4));
        assert!(!range.contains(5));
    }

    #[test]
    fn page_range_parse_still_accepts_start_end() {
        let range = PageRange::parse("195-195").unwrap();
        assert_eq!((range.start, range.end), (195, 195));
    }
}

/// Smallest render height (pixels) used for `text_format = "truetyping"`
/// unless the caller sets one explicitly. Glyph outlines are traced from this
/// raster, so it is chosen for shape fidelity rather than for a screen.
pub const GLYPHFONT_MIN_TARGET_HEIGHT: u32 = 2400;

/// The text format whose printed text becomes an embedded per-book TrueType
/// font. `glyphfont` was its name while it was being built and stays an
/// accepted spelling of it, so scripted runs keep working.
pub const TRUETYPING: &str = "truetyping";

/// How body-text regions are rendered. Orthogonal to the output container
/// (PDF vs DjVu, chosen by `text_format`) and to image-region handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PageMode {
    /// Bilevel: text is binarized (Sauvola/Otsu/fixed) and encoded as
    /// JBIG2/CCITT4 (PDF) or JB2 (DjVu). The historical default.
    #[default]
    Binarized,
    /// Grayscale clean + MRC: text is cleaned to a flat-white page, then split
    /// into a JBIG2/JB2 ink-core mask painted over a JP2/IW44 grayscale
    /// background, preserving antialiasing. Image regions stay JP2/IW44.
    Grayscale,
}

#[derive(Clone, Debug)]
pub struct PipelineConfig {
    pub(crate) model_path: String,
    pub(crate) confidence_threshold: f32,
    pub(crate) nms_threshold: f32,
    pub(crate) output_dpi: u32,
    pub(crate) enable_ocr_hint: bool,
    pub(crate) target_height: u32,
    pub(crate) target_width: Option<u32>,
    /// The height the caller asked for before glyph-font output raised
    /// `target_height` to its working minimum; `None` when nothing was
    /// raised or an explicit height came later. See
    /// [`Self::glyph_background_subsample`].
    pub(crate) glyph_requested_height: Option<u32>,
    /// Dithering for **layout image regions** only (not body text). Ignored when layout detection is off.
    pub(crate) image_region_dither_mode: ImageRegionDitherMode,
    pub(crate) binarization: BinarizationConfig,
    pub(crate) enable_ocr: bool,
    /// Synthesize a navigable outline from layout title detections and text.
    /// Source-provided outlines are preserved regardless of this setting.
    pub(crate) enable_auto_toc: bool,
    pub(crate) ocr_language: String,
    pub(crate) cover_format: CoverFormat,
    pub(crate) text_format: String,
    pub(crate) enable_layout_detection: bool,
    /// Raster reflow: re-composes rendered page images into reflowed output
    /// pages (see `crate::reflow`). Requires layout detection — region hints
    /// drive reading order and reflow eligibility.
    pub(crate) enable_reflow: bool,
    pub(crate) heavy_sauvola_concurrency: usize,
    pub(crate) page_range: Option<PageRange>,
    pub(crate) layout_exclusion_pages: Option<PageSelection>,
    pub(crate) epub_sidecar_output: Option<PathBuf>,
    pub(crate) enable_cover_page: bool,
    pub(crate) no_cover_page: bool,
    pub(crate) high_quality_output: bool,
    pub(crate) jpeg_compat: bool,
    pub(crate) channel_buffer_size: Option<usize>,
    pub(crate) ocr_binarization_threshold: Option<u8>,
    pub(crate) ocr_preserve_grayscale: bool,
    pub(crate) invert_input: bool,
    pub(crate) jbig2_mode: Jbig2Mode,
    pub(crate) max_retries: u32,
    pub(crate) retry_delay_ms: u64,
    pub(crate) max_parallel_pages: Option<usize>,
    pub(crate) batch_size: usize,
    pub(crate) batch_timeout_ms: u64,
    // Batching behavior knobs
    pub(crate) initial_single_pages: usize,
    pub(crate) max_inference_batch_size: Option<usize>,
    pub(crate) margin_settings: crate::margin::MarginSettings,
    pub(crate) crop_footnotes: bool,
    pub(crate) crop_free_aspect: bool,
    // Rendering and inference sizes
    pub(crate) high_res_render_height: u32,
    pub(crate) inference_size: u32,
    // Keep original image quality for detected image regions
    pub(crate) keep_original_images: bool,
    /// Expand near–full-page figure boxes to the full raster (fixes layout under-segmentation).
    /// Overlap-merge skips full-page vs small inset pairs to avoid collapsing distinct figures.
    pub(crate) expand_full_bleed_figure_bboxes: bool,
    // DjVu IW44 quality (0-100 scale, maps to slices)
    pub(crate) djvu_iw44_quality: u8,
    /// Use the slow line-segmentation OCR pipeline instead of the fast region/tile path.
    pub(crate) slow_ocr: bool,
    /// Body-text rendering mode (bilevel vs grayscale-clean/MRC).
    pub(crate) page_mode: PageMode,
    /// MRC/grayscale tuning (only used when `page_mode == Grayscale`).
    /// JP2 quality for the grayscale background layer. Q45 preserves the
    /// antialiasing halo of the subsampled background (see FINDINGS round 7).
    pub(crate) mrc_bg_quality: u8,
    /// Box-downsample factor for the grayscale background layer.
    /// 0 = resolution-aware auto (see `mrc_bg_subsample_for`): thin structures
    /// (staff lines) smear when the background drops below ~full render
    /// resolution at 1200px, while ×3 is fine at 2600px+.
    pub(crate) mrc_bg_subsample: u8,
    /// Luma threshold (0-255) below which a cleaned pixel becomes an ink-mask
    /// core. Applied to the CLEANED image (paper normalized to 255), so a
    /// fixed value is stable across books. 180 makes the mask carry the same
    /// stroke weight as fixed-180 binarization — the validated look — with
    /// the background adding only the antialiasing skirt above it. Lower
    /// values produce skeletal cores-only masks (thin, weak text); 0 = Otsu
    /// adaptive (kept for experiments; splits within the text after halo
    /// cleaning, so it under-selects).
    pub(crate) mrc_ink_threshold: u8,
    /// Derive the MRC ink mask from the configured binarizer (adaptive Sauvola)
    /// on the RAW render instead of thresholding the cleaned image. Fixed cuts
    /// on the cleaned image have a hard ceiling on faint scans: the auto
    /// contrast stretch maps weak ink near white, so thin staff lines break no
    /// matter the threshold, while local-adaptive Sauvola keeps them contiguous
    /// (measured 0.63 vs 0.30 staff-line continuity at threshold 230 on the
    /// faintest customer songbook). The cleaned gray still supplies the JP2/IW44
    /// background halo.
    pub(crate) mrc_adaptive_mask: bool,
    /// Resolution multiplier applied to `target_height` when slow OCR is enabled,
    /// determining the PDF render height ("render high, resize low"). The
    /// high-res raster feeds OCR; the encode path is resized back down to
    /// `target_height`. Clamped so the render height never exceeds
    /// `MAX_SLOW_OCR_RENDER_HEIGHT`.
    pub(crate) slow_ocr_scale: f32,
}

/// Upper bound on the slow-OCR render height (pixels) to cap memory use.
pub const MAX_SLOW_OCR_RENDER_HEIGHT: u32 = 6000;

impl PipelineConfig {
    pub fn new() -> Result<Self> {
        // A layout-free build does not embed the model or compile its runtime.
        // In normal builds, an adjacent model overrides the embedded payload.
        let model_path = if cfg!(feature = "layout-detection") {
            runtime_asset_path_if_exists("doclayout.onnx")
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let enable_layout_detection = cfg!(feature = "layout-detection");
        let config = Self {
            model_path,
            // Keep marginal but valid scanned illustrations. The shipped
            // model scores some full-page engravings around 0.36-0.42; the
            // substantive-text overlap guard removes the harmful text-column
            // false positives downstream.
            confidence_threshold: 0.35,
            nms_threshold: 0.5,
            output_dpi: 300,
            enable_ocr_hint: true,
            target_height: 1200,
            target_width: None,
            glyph_requested_height: None,
            image_region_dither_mode: ImageRegionDitherMode::None,
            binarization: BinarizationConfig::default(),
            enable_ocr: false,
            enable_auto_toc: false,
            ocr_language: "eng".to_string(),
            cover_format: CoverFormat::Jpeg,
            text_format: "jbig2".to_string(),
            enable_layout_detection,
            enable_reflow: false,
            heavy_sauvola_concurrency: 4,
            page_range: None,
            layout_exclusion_pages: None,
            epub_sidecar_output: None,
            enable_cover_page: true,
            no_cover_page: false,
            high_quality_output: false,
            jpeg_compat: false,
            channel_buffer_size: None,
            ocr_binarization_threshold: None,
            ocr_preserve_grayscale: false,
            invert_input: false,
            jbig2_mode: Jbig2Mode::Symbol,
            max_retries: 3,
            retry_delay_ms: 1000,
            max_parallel_pages: Some(4),
            batch_size: 15,
            batch_timeout_ms: 2000,
            initial_single_pages: 15,
            max_inference_batch_size: Some(16), // Dynamic batching: greedy up to 16 images
            margin_settings: crate::margin::MarginSettings::None,
            crop_footnotes: false,
            crop_free_aspect: false,
            high_res_render_height: 1200,
            // 640 misses large, low-contrast scanned illustrations (including
            // Structures.pdf page 195). 800 retains the model's image label
            // while remaining substantially cheaper than 1024 inference.
            inference_size: 800,
            keep_original_images: true,
            expand_full_bleed_figure_bboxes: true,
            djvu_iw44_quality: 75, // Default to good quality
            slow_ocr: false,
            page_mode: PageMode::Binarized,
            // 45 preserved the antialiasing halo on 290-dpi text but artifacts
            // structured content (sheet music) at lower render resolutions;
            // 55 gives headroom (user floor: at least 50).
            mrc_bg_quality: 55,
            mrc_bg_subsample: 0,    // auto (content-aware)
            mrc_ink_threshold: 180, // matches the validated fixed-180 stroke weight
            mrc_adaptive_mask: false,
            slow_ocr_scale: 2.5,
        };

        config.validate()?;
        Ok(config)
    }

    pub fn simple_cli_defaults() -> Result<Self> {
        let mut cfg = Self::new()?;
        cfg.set_text_format("ccitt4")?;
        cfg.set_image_format(CoverFormat::Jpeg);
        cfg.set_target_height(1200)?;
        cfg.set_high_res_render_height(1200)?;
        let bin = BinarizationConfig {
            k_factor: crate::DEFAULT_K_FACTOR,
            invert: false,
            invert_input: false,
            use_heavy_duty: false,
            patch_percentage: 0.0,
            no_patch: false,
            use_fixed_threshold: true,
            fixed_threshold: 180,
        };
        cfg.set_binarization(bin);
        // Simple mode: keep figure regions as encoded originals (cover format, default JPEG)
        // unless the user passes `--dither` / `--halftone` (handled in main).
        cfg.set_dither_images(false);
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        match self.text_format.as_ref() {
            "jbig2" | "ccitt4" | "jpeg" | "djvu" | TRUETYPING => {}
            _ => {
                return Err(anyhow!(
                    "Invalid text_format: '{}'. Must be one of: jbig2, ccitt4, jpeg, djvu, truetyping",
                    self.text_format
                ));
            }
        }

        if !(0.0..=1.0).contains(&self.confidence_threshold) {
            return Err(anyhow!("confidence_threshold must be between 0.0 and 1.0"));
        }

        if !(0.0..=1.0).contains(&self.nms_threshold) {
            return Err(anyhow!("nms_threshold must be between 0.0 and 1.0"));
        }

        if self.target_height == 0 {
            return Err(anyhow!("target_height must be greater than 0"));
        }
        if let Some(width) = self.target_width {
            if width == 0 {
                return Err(anyhow!("target_width must be greater than 0"));
            }
        }

        if self.high_res_render_height == 0 {
            return Err(anyhow!("high_res_render_height must be greater than 0"));
        }

        if self.inference_size == 0 {
            return Err(anyhow!("inference_size must be greater than 0"));
        }

        if self.output_dpi < 72 || self.output_dpi > 2400 {
            return Err(anyhow!("output_dpi must be between 72 and 2400"));
        }

        if self.heavy_sauvola_concurrency == 0 {
            return Err(anyhow!("heavy_sauvola_concurrency must be greater than 0"));
        }
        Self::validate_ocr_language_code(&self.ocr_language)?;

        if !self.model_path.is_empty() && !std::path::Path::new(&self.model_path).exists() {
            return Err(anyhow!("Model file not found: {}", self.model_path));
        }

        Ok(())
    }

    // Getters
    pub fn model_path(&self) -> &str {
        &self.model_path
    }
    pub fn confidence_threshold(&self) -> f32 {
        self.confidence_threshold
    }
    pub fn nms_threshold(&self) -> f32 {
        self.nms_threshold
    }
    pub fn output_dpi(&self) -> u32 {
        self.output_dpi
    }
    pub fn enable_ocr_hint(&self) -> bool {
        self.enable_ocr_hint
    }
    pub fn target_height(&self) -> u32 {
        self.target_height
    }
    pub fn target_width(&self) -> Option<u32> {
        if self.crop_free_aspect
            && self.margin_settings == crate::margin::MarginSettings::CropAndResize
        {
            return None;
        }
        self.target_width
    }
    pub fn dither_images(&self) -> bool {
        self.image_region_dither_mode != ImageRegionDitherMode::None
    }

    pub fn image_region_dither_mode(&self) -> ImageRegionDitherMode {
        self.image_region_dither_mode
    }
    pub fn binarization(&self) -> &BinarizationConfig {
        &self.binarization
    }
    pub fn enable_ocr(&self) -> bool {
        self.enable_ocr && cfg!(feature = "ocr")
    }
    pub fn enable_auto_toc(&self) -> bool {
        self.enable_auto_toc
    }
    pub fn ocr_language(&self) -> &str {
        &self.ocr_language
    }
    pub fn cover_format(&self) -> &CoverFormat {
        &self.cover_format
    }
    pub fn image_format(&self) -> &CoverFormat {
        &self.cover_format
    }
    pub fn text_format(&self) -> &str {
        &self.text_format
    }
    pub fn page_mode(&self) -> PageMode {
        self.page_mode
    }
    pub fn set_page_mode(&mut self, mode: PageMode) {
        self.page_mode = mode;
    }
    /// True when body text should use the grayscale-clean / MRC path.
    pub fn is_grayscale_mode(&self) -> bool {
        self.page_mode == PageMode::Grayscale
    }
    pub fn mrc_bg_quality(&self) -> u8 {
        self.mrc_bg_quality
    }
    /// Explicit background subsample factor, or `None` for resolution-aware
    /// auto (×1 below 1800px render height, ×2 below 2400px, ×3 above). The
    /// background layer holds the ~1px-per-1200px antialiasing ring around
    /// the threshold-180 mask; subsampling past the render's ring width
    /// averages it into white and the output degenerates to pure bilevel.
    /// Validated on buddhasahibs at 1200px: ×3 left a 99.8%-white background.
    pub fn mrc_bg_subsample_override(&self) -> Option<usize> {
        if self.mrc_bg_subsample != 0 {
            Some(self.mrc_bg_subsample as usize)
        } else {
            None
        }
    }
    pub fn mrc_ink_threshold(&self) -> u8 {
        self.mrc_ink_threshold
    }
    pub fn mrc_adaptive_mask(&self) -> bool {
        self.mrc_adaptive_mask
    }
    pub fn set_mrc_adaptive_mask(&mut self, enabled: bool) {
        self.mrc_adaptive_mask = enabled;
    }
    pub fn set_mrc_bg_quality(&mut self, q: u8) {
        self.mrc_bg_quality = q.clamp(1, 100);
    }
    /// 0 = resolution-aware auto; 1-12 = explicit factor.
    pub fn set_mrc_bg_subsample(&mut self, s: u8) {
        self.mrc_bg_subsample = s.min(12);
    }
    pub fn set_mrc_ink_threshold(&mut self, t: u8) {
        self.mrc_ink_threshold = t;
    }
    pub fn enable_layout_detection(&self) -> bool {
        if self.invert_input || !cfg!(feature = "layout-detection") {
            false
        } else {
            self.enable_layout_detection
        }
    }
    /// Raster reflow is only active while layout detection is actually
    /// available — it is gated at CLI parse time, but `invert_input` (or a
    /// missing model) can disable layout detection afterward, so re-derive
    /// the effective state here rather than trusting the stored flag alone.
    pub fn enable_reflow(&self) -> bool {
        self.enable_reflow && self.enable_layout_detection()
    }
    pub fn heavy_sauvola_concurrency(&self) -> usize {
        self.heavy_sauvola_concurrency
    }
    pub fn page_range(&self) -> Option<&PageRange> {
        self.page_range.as_ref()
    }
    pub fn layout_exclusion_pages(&self) -> Option<&PageSelection> {
        self.layout_exclusion_pages.as_ref()
    }
    pub fn epub_sidecar_output(&self) -> Option<&PathBuf> {
        self.epub_sidecar_output.as_ref()
    }
    pub fn layout_detection_enabled_for_page(&self, page_index_0based: usize) -> bool {
        self.enable_layout_detection()
            && !self
                .layout_exclusion_pages
                .as_ref()
                .is_some_and(|selection| selection.contains(page_index_0based))
    }
    pub fn enable_cover_page(&self) -> bool {
        self.enable_cover_page
    }
    pub fn no_cover_page(&self) -> bool {
        self.no_cover_page
    }
    pub fn high_quality_output(&self) -> bool {
        self.high_quality_output
    }
    pub fn jpeg_compat(&self) -> bool {
        self.jpeg_compat
    }
    pub fn channel_buffer_size(&self) -> usize {
        self.channel_buffer_size
            .unwrap_or_else(|| self.max_parallel_pages.unwrap_or(4).max(1))
    }
    pub fn ocr_binarization_threshold(&self) -> Option<u8> {
        self.ocr_binarization_threshold
    }
    pub fn ocr_preserve_grayscale(&self) -> bool {
        self.ocr_preserve_grayscale
    }
    pub fn invert_input(&self) -> bool {
        self.invert_input
    }
    pub fn jbig2_mode(&self) -> Jbig2Mode {
        self.jbig2_mode.clone()
    }
    pub fn jbig2_symbol_mode(&self) -> bool {
        matches!(self.jbig2_mode, Jbig2Mode::Symbol | Jbig2Mode::SymUnify)
    }
    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }
    pub fn retry_delay_ms(&self) -> u64 {
        self.retry_delay_ms
    }
    pub fn max_parallel_pages(&self) -> Option<usize> {
        self.max_parallel_pages
    }
    pub fn batch_size(&self) -> usize {
        self.batch_size
    }
    pub fn batch_timeout_ms(&self) -> u64 {
        self.batch_timeout_ms
    }
    pub fn initial_single_pages(&self) -> usize {
        self.initial_single_pages
    }
    pub fn max_inference_batch_size(&self) -> Option<usize> {
        self.max_inference_batch_size
    }
    pub fn margin_settings(&self) -> crate::margin::MarginSettings {
        self.margin_settings
    }
    pub fn crop_footnotes(&self) -> bool {
        self.crop_footnotes
    }
    pub fn crop_free_aspect(&self) -> bool {
        self.crop_free_aspect
    }
    pub fn high_res_render_height(&self) -> u32 {
        self.high_res_render_height
    }
    pub fn inference_size(&self) -> u32 {
        self.inference_size
    }
    pub fn inference_resize_spec(&self) -> InferenceResizeSpec {
        PaddleResizeConfig {
            target: self.inference_size,
            ..Default::default()
        }
        .into()
    }
    pub fn keep_original_images(&self) -> bool {
        self.keep_original_images
    }
    pub fn expand_full_bleed_figure_bboxes(&self) -> bool {
        self.expand_full_bleed_figure_bboxes
    }
    pub fn djvu_iw44_quality(&self) -> u8 {
        self.djvu_iw44_quality
    }
    pub fn slow_ocr_enabled(&self) -> bool {
        self.slow_ocr && cfg!(feature = "ocr")
    }

    pub fn slow_ocr_scale(&self) -> f32 {
        self.slow_ocr_scale
    }

    /// Height (pixels) the PDF source should render at. When slow OCR is enabled
    /// this is `target_height * slow_ocr_scale` (clamped), so a high-resolution
    /// raster is available for recognition; otherwise it equals `target_height`.
    pub fn source_render_height(&self) -> u32 {
        if self.slow_ocr_enabled() && self.slow_ocr_scale > 1.0 {
            let scaled = (self.target_height as f32 * self.slow_ocr_scale).round() as u32;
            scaled
                .min(MAX_SLOW_OCR_RENDER_HEIGHT)
                .max(self.target_height)
        } else {
            self.target_height
        }
    }

    /// Width (pixels) the PDF source should render at, scaled in proportion to
    /// `source_render_height`. `None` derives width from the page
    /// aspect ratio (mirrors `target_width`).
    pub fn source_render_width(&self) -> Option<u32> {
        let render_h = self.source_render_height();
        match self.target_width {
            Some(w) if self.target_height > 0 && render_h != self.target_height => Some(
                ((w as f32 * render_h as f32 / self.target_height as f32).round() as u32).max(1),
            ),
            other => other,
        }
    }

    // Setters
    pub fn set_text_format(&mut self, format: &str) -> Result<()> {
        // `glyphfont` was the name this format was built under.
        let format = if format == "glyphfont" {
            TRUETYPING
        } else {
            format
        };
        match format {
            "jbig2" | "ccitt4" | "jpeg" | "djvu" | "epub" | TRUETYPING => {
                self.text_format = format.to_string();
                // EPUB is a text-only reflowable format: it needs the slow,
                // structured OCR path and has no image-encoding stage.
                if format == "epub" {
                    self.set_slow_ocr(true);
                }
                // Truetyping is resolution independent on the reader, so the
                // source raster is chosen for outline fidelity rather than for
                // a screen; the requested height is kept for the background.
                self.raise_render_height_for_truetyping();
                Ok(())
            }
            _ => Err(anyhow!(
                "Invalid text_format: '{}'. Must be one of: jbig2, ccitt4, jpeg, djvu, epub, truetyping",
                format
            )),
        }
    }
    pub fn set_confidence_threshold(&mut self, threshold: f32) -> Result<()> {
        if !(0.0..=1.0).contains(&threshold) {
            return Err(anyhow!("confidence_threshold must be between 0.0 and 1.0"));
        }
        self.confidence_threshold = threshold;
        Ok(())
    }
    pub fn set_nms_threshold(&mut self, threshold: f32) -> Result<()> {
        if !(0.0..=1.0).contains(&threshold) {
            return Err(anyhow!("nms_threshold must be between 0.0 and 1.0"));
        }
        self.nms_threshold = threshold;
        Ok(())
    }
    pub fn set_target_height(&mut self, height: u32) -> Result<()> {
        if height == 0 {
            return Err(anyhow!("target_height must be greater than 0"));
        }
        self.target_height = height;
        self.target_width = None;
        self.glyph_requested_height = None;
        self.raise_render_height_for_truetyping();
        Ok(())
    }

    /// Box-downsample factor that takes a raster rendered at the glyph-font
    /// working height back to the height the caller asked for. Glyph-font
    /// output raises the render height for clean glyph shapes; the
    /// continuous-tone background gains nothing from that and costs four
    /// times as much to encode, so it is encoded at the requested
    /// resolution (never below it). 1 when nothing was raised.
    /// The height the caller asked for, when truetyping is rendering above it.
    pub fn glyph_requested_height(&self) -> Option<u32> {
        self.glyph_requested_height
    }

    pub fn glyph_background_subsample(&self) -> usize {
        match self.glyph_requested_height {
            Some(requested) if requested > 0 && self.target_height > requested => {
                (self.target_height / requested).max(1) as usize
            }
            _ => 1,
        }
    }
    pub fn set_target_dimensions(&mut self, width: u32, height: u32) -> Result<()> {
        if self.crop_free_aspect {
            return Err(anyhow!(
                "fixed target width cannot be used with free-aspect margin crop"
            ));
        }
        if height == 0 {
            return Err(anyhow!("target_height must be greater than 0"));
        }
        if width == 0 {
            return Err(anyhow!("target_width must be greater than 0"));
        }
        let (mut w, mut h) = (width, height);
        if w > h {
            std::mem::swap(&mut w, &mut h);
        }
        self.target_height = h;
        self.target_width = Some(w);
        self.glyph_requested_height = None;
        self.raise_render_height_for_truetyping();
        Ok(())
    }

    /// Truetyping traces its outlines from the rendered page, so the page is
    /// rendered at least [`GLYPHFONT_MIN_TARGET_HEIGHT`] tall whatever height
    /// was asked for; the asked-for height is remembered and the background
    /// comes back down to it at encode time (see
    /// [`Self::glyph_background_subsample`]). Order-independent: it applies
    /// whether the format or the height was set first.
    fn raise_render_height_for_truetyping(&mut self) {
        if self.text_format != TRUETYPING || self.target_height >= GLYPHFONT_MIN_TARGET_HEIGHT {
            return;
        }
        self.set_truetyping_working_height(GLYPHFONT_MIN_TARGET_HEIGHT);
    }

    /// Lower the working height a scan is traced at to what the scan itself
    /// holds ([`lege_pdf_read::CompiledDocumentPage::natural_raster_height`]),
    /// never below the height the caller asked for. Rendering a scan above
    /// its own resolution invents detail: the interpolated edges differ from
    /// one impression of a letter to the next, so the dictionary fills with
    /// near-duplicates and every stage pays for pixels that carry nothing.
    pub fn clamp_truetyping_render_height(&mut self, natural: u32) {
        let Some(requested) = self.glyph_requested_height else {
            return;
        };
        let working = natural.max(requested);
        if working >= self.target_height {
            return;
        }
        self.set_truetyping_working_height(working);
    }

    /// Set the height the page is rendered at while keeping the caller's own
    /// height as the background's (see [`Self::glyph_background_subsample`]).
    fn set_truetyping_working_height(&mut self, working: u32) {
        let requested = self.glyph_requested_height.unwrap_or(self.target_height);
        if working <= requested {
            self.glyph_requested_height = None;
            self.target_height = requested;
            return;
        }
        if let Some(width) = self.target_width.as_mut() {
            let scaled =
                (u64::from(*width) * u64::from(working) / u64::from(self.target_height)) as u32;
            *width = scaled.max(1);
        }
        self.glyph_requested_height = Some(requested);
        self.target_height = working;
    }
    pub fn set_dither_images(&mut self, dither: bool) {
        self.image_region_dither_mode = if dither {
            if self.text_format == "ccitt4" {
                ImageRegionDitherMode::Ccitt4ClusteredDot4x4
            } else {
                ImageRegionDitherMode::Stucki
            }
        } else {
            ImageRegionDitherMode::None
        };
    }

    pub fn set_image_region_dither_mode(&mut self, mode: ImageRegionDitherMode) {
        self.image_region_dither_mode = mode;
    }
    pub fn set_enable_ocr(&mut self, enable: bool) {
        self.enable_ocr = enable && cfg!(feature = "ocr");
    }
    pub fn set_enable_auto_toc(&mut self, enable: bool) {
        self.enable_auto_toc = enable;
    }
    pub fn set_ocr_language(&mut self, language: &str) -> Result<()> {
        let normalized = language.trim().to_ascii_lowercase();
        Self::validate_ocr_language_code(&normalized)?;
        self.ocr_language = normalized;
        Ok(())
    }
    pub fn set_enable_layout_detection(&mut self, enable: bool) {
        self.enable_layout_detection = enable && cfg!(feature = "layout-detection");
    }
    /// Enable raster reflow. Has no effect unless layout detection is also
    /// enabled — see [`PipelineConfig::enable_reflow`].
    pub fn set_enable_reflow(&mut self, enable: bool) {
        self.enable_reflow = enable;
    }
    pub fn set_slow_ocr(&mut self, enable: bool) {
        self.slow_ocr = enable && cfg!(feature = "ocr");
        if enable {
            self.enable_ocr = cfg!(feature = "ocr");
        }
    }
    /// Set the slow-OCR render-resolution multiplier. Values <= 1.0 disable the
    /// high-resolution render (OCR then runs at `target_height`).
    pub fn set_slow_ocr_scale(&mut self, scale: f32) {
        if scale.is_finite() && scale >= 1.0 {
            self.slow_ocr_scale = scale;
        }
    }
    pub fn set_keep_original_images(&mut self, keep: bool) {
        self.keep_original_images = keep;
    }
    pub fn set_expand_full_bleed_figure_bboxes(&mut self, enable: bool) {
        self.expand_full_bleed_figure_bboxes = enable;
    }
    pub fn set_heavy_sauvola_concurrency(&mut self, concurrency: usize) -> Result<()> {
        if concurrency == 0 {
            return Err(anyhow!("heavy_sauvola_concurrency must be greater than 0"));
        }
        self.heavy_sauvola_concurrency = concurrency;
        Ok(())
    }
    pub fn set_channel_buffer_size(&mut self, size: Option<usize>) -> Result<()> {
        if matches!(size, Some(0)) {
            return Err(anyhow!("channel_buffer_size must be greater than 0"));
        }
        self.channel_buffer_size = size;
        Ok(())
    }
    pub fn set_max_parallel_pages(&mut self, pages: Option<usize>) -> Result<()> {
        if matches!(pages, Some(0)) {
            return Err(anyhow!("max_parallel_pages must be greater than 0"));
        }
        self.max_parallel_pages = pages;
        Ok(())
    }
    pub fn set_page_range(&mut self, range: Option<PageRange>) {
        self.page_range = range;
    }
    pub fn set_layout_exclusion_pages(&mut self, pages: Option<PageSelection>) {
        self.layout_exclusion_pages = pages;
    }
    pub fn set_epub_sidecar_output(&mut self, output: Option<PathBuf>) {
        self.epub_sidecar_output = output;
    }
    pub fn set_enable_cover_page(&mut self, enable: bool) {
        self.enable_cover_page = enable;
    }
    pub fn set_no_cover_page(&mut self, no_cover: bool) {
        self.no_cover_page = no_cover;
    }
    pub fn set_high_quality_output(&mut self, high_quality: bool) {
        self.high_quality_output = high_quality;
    }
    pub fn set_jpeg_compat(&mut self, jpeg_compat: bool) {
        self.jpeg_compat = jpeg_compat;
    }
    pub fn set_binarization(&mut self, config: BinarizationConfig) {
        self.binarization = config;
    }
    pub fn set_cover_format(&mut self, format: CoverFormat) {
        self.cover_format = format;
    }
    pub fn set_image_format(&mut self, format: CoverFormat) {
        self.cover_format = format;
    }
    pub fn set_invert_input(&mut self, invert: bool) {
        self.invert_input = invert;
    }
    pub fn set_jbig2_mode(&mut self, mode: Jbig2Mode) {
        self.jbig2_mode = mode;
    }
    pub fn set_jbig2_symbol_mode(&mut self, symbol_mode: bool) {
        self.jbig2_mode = if symbol_mode {
            Jbig2Mode::Symbol
        } else {
            Jbig2Mode::Generic
        };
    }
    pub fn set_initial_single_pages(&mut self, n: usize) {
        self.initial_single_pages = n;
    }
    pub fn set_max_inference_batch_size(&mut self, n: Option<usize>) {
        self.max_inference_batch_size = n;
    }
    pub fn set_margin_settings(&mut self, settings: crate::margin::MarginSettings) {
        self.margin_settings = settings;
    }
    pub fn set_crop_footnotes(&mut self, v: bool) {
        self.crop_footnotes = v;
    }
    pub fn set_crop_free_aspect(&mut self, v: bool) {
        self.crop_free_aspect = v;
        if v {
            self.target_width = None;
        }
    }
    pub fn set_high_res_render_height(&mut self, h: u32) -> Result<()> {
        if h == 0 {
            return Err(anyhow!("high_res_render_height must be > 0"));
        }
        self.high_res_render_height = h;
        Ok(())
    }
    pub fn set_inference_size(&mut self, s: u32) -> Result<()> {
        if s == 0 {
            return Err(anyhow!("inference_size must be > 0"));
        }
        self.inference_size = s;
        Ok(())
    }
    pub fn set_djvu_iw44_quality(&mut self, quality: u8) -> Result<()> {
        if quality > 100 {
            return Err(anyhow!("djvu_iw44_quality must be 0-100"));
        }
        self.djvu_iw44_quality = quality;
        Ok(())
    }

    fn validate_ocr_language_code(language: &str) -> Result<()> {
        if language.is_empty() {
            return Err(anyhow!("ocr_language cannot be empty"));
        }
        if language
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        {
            Ok(())
        } else {
            Err(anyhow!(
                "Invalid ocr_language '{}'. Allowed characters: a-z, 0-9, underscore",
                language
            ))
        }
    }
}

// Runtime asset path utilities
pub fn runtime_search_directories() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    let mut push_unique = |path: PathBuf| {
        if !dirs.iter().any(|p| p == &path) {
            dirs.push(path);
        }
    };

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(dir) = exe_path.parent() {
            push_unique(dir.to_path_buf());
            push_unique(dir.join("models"));
            push_unique(dir.join("tessdata"));
            push_unique(dir.join("installer/linux64"));
            push_unique(dir.join("installer"));

            if let Some(parent) = dir.parent() {
                #[cfg(target_os = "macos")]
                if parent.file_name().is_some_and(|name| name == "Contents") {
                    push_unique(parent.join("Frameworks"));
                    push_unique(parent.join("Resources"));
                    push_unique(parent.join("Resources/models"));
                    push_unique(parent.join("Resources/tessdata"));
                }

                push_unique(parent.to_path_buf());
                push_unique(parent.join("share/lege"));
                push_unique(parent.join("share/lege/models"));
                push_unique(parent.join("share/lege/tessdata"));
                push_unique(parent.join("lib/lege"));
                push_unique(parent.join("installer/linux64"));
                push_unique(parent.join("installer"));
            }
        }
    }

    for env_key in ["LEGE_DATA_DIR", "LEGE_ASSET_DIR"] {
        if let Ok(value) = std::env::var(env_key) {
            push_unique(PathBuf::from(value));
        }
    }

    if let Some(ld_path) = std::env::var_os("LD_LIBRARY_PATH") {
        for segment in std::env::split_paths(&ld_path) {
            push_unique(segment);
        }
    }

    #[cfg(target_os = "macos")]
    if let Some(dyld_path) = std::env::var_os("DYLD_LIBRARY_PATH") {
        for segment in std::env::split_paths(&dyld_path) {
            push_unique(segment);
        }
    }

    #[cfg(target_os = "macos")]
    if let Some(dyld_fallback) = std::env::var_os("DYLD_FALLBACK_LIBRARY_PATH") {
        for segment in std::env::split_paths(&dyld_fallback) {
            push_unique(segment);
        }
    }

    for fallback in [
        "/usr/lib/lege",
        "/usr/local/lib/lege",
        "/usr/share/lege",
        "/usr/share/lege/models",
    ] {
        push_unique(PathBuf::from(fallback));
    }

    if let Ok(cwd) = std::env::current_dir() {
        push_unique(cwd);
    }

    push_unique(PathBuf::from("."));
    dirs
}

pub fn runtime_asset_path_if_exists(file_name: &str) -> Option<PathBuf> {
    runtime_search_directories()
        .into_iter()
        .map(|dir| dir.join(file_name))
        .find(|candidate| candidate.is_file())
}

pub fn runtime_asset_path(file_name: &str) -> PathBuf {
    runtime_asset_path_if_exists(file_name).unwrap_or_else(|| PathBuf::from(file_name))
}

#[cfg(all(test, not(feature = "ocr")))]
mod no_ocr_feature_tests {
    use super::PipelineConfig;

    #[test]
    fn ocr_setters_remain_disabled_without_the_feature() {
        let mut config = PipelineConfig::default();
        config.set_enable_ocr(true);
        config.set_slow_ocr_scale(2.5);
        config.set_slow_ocr(true);

        assert!(!config.enable_ocr());
        assert!(!config.slow_ocr_enabled());
        assert_eq!(config.source_render_height(), config.target_height());
    }
}

#[cfg(all(test, not(feature = "layout-detection")))]
mod no_layout_feature_tests {
    use super::PipelineConfig;

    #[test]
    fn layout_setter_remains_disabled_without_the_feature() {
        let mut config = PipelineConfig::default();
        config.set_enable_layout_detection(true);

        assert!(!config.enable_layout_detection());
    }
}

#[cfg(test)]
mod truetyping_height_tests {
    use super::{GLYPHFONT_MIN_TARGET_HEIGHT, PipelineConfig, TRUETYPING};

    #[test]
    fn the_working_height_is_raised_whichever_was_asked_for_first() {
        let mut format_first = PipelineConfig::default();
        format_first.set_text_format(TRUETYPING).unwrap();
        format_first.set_target_height(1200).unwrap();

        let mut height_first = PipelineConfig::default();
        height_first.set_target_height(1200).unwrap();
        height_first.set_text_format(TRUETYPING).unwrap();

        for config in [&format_first, &height_first] {
            assert_eq!(config.target_height(), GLYPHFONT_MIN_TARGET_HEIGHT);
            assert_eq!(config.glyph_background_subsample(), 2);
        }
    }

    #[test]
    fn a_fixed_width_is_scaled_with_the_raised_height() {
        let mut config = PipelineConfig::default();
        config.set_text_format(TRUETYPING).unwrap();
        config.set_target_dimensions(900, 1200).unwrap();

        assert_eq!(config.target_height(), GLYPHFONT_MIN_TARGET_HEIGHT);
        assert_eq!(config.target_width(), Some(1800));
    }

    #[test]
    fn glyphfont_is_still_accepted_as_a_spelling_of_truetyping() {
        let mut config = PipelineConfig::default();
        config.set_text_format("glyphfont").unwrap();
        assert_eq!(config.text_format(), TRUETYPING);
        config.validate().unwrap();
    }

    #[test]
    fn other_formats_keep_the_height_they_were_given() {
        let mut config = PipelineConfig::default();
        config.set_text_format("jbig2").unwrap();
        config.set_target_height(1200).unwrap();

        assert_eq!(config.target_height(), 1200);
        assert_eq!(config.glyph_background_subsample(), 1);
    }
}
