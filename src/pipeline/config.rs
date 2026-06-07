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
use pdfium_render::prelude::Pdfium;

use super::pdf_tokio_pipeline::create_and_run_pdf_tokio_pipeline;
use crate::pagerender::prelude::{PdfiumRenderer, RasterConfig as PdfRasterConfig};
use crate::pipeline::policies::{InferenceResizeSpec, YoloResizeConfig};
use crate::types::CoverFormat;
use Legencode::streamline::Jbig2Mode;
use Legencode::types::BinarizationConfig;

pub use Legencode::color::ImageRegionDitherMode;

#[derive(Debug)]
pub struct PageTask {
    pub index: usize,
    pub total_pages: usize,
    pub pdf_bytes: Arc<[u8]>,
}
// Specific error types for better error handling
#[derive(thiserror::Error, Debug)]
pub enum ProcessingError {
    #[error("Binarization failed: {0}")]
    Binarization(String),
    #[error("Encoding failed: {0}")]
    Encoding(String),
    #[error("OCR failed: {0}")]
    Ocr(String),
    #[error("Layout detection failed: {0}")]
    LayoutDetection(String),
    #[error("Image processing failed: {0}")]
    ImageProcessing(String),
    #[error("PDF assembly failed: {0}")]
    PdfAssembly(String),
}

/// Detection results from inference worker
#[derive(Debug, Clone)]
pub struct InferenceResult {
    pub index: usize,
    pub high_res_image: Arc<RgbImage>,
    pub inference_image: Arc<RgbImage>,
    pub detections: Vec<crate::engine::Detection>,
    pub text_layer: Option<String>,
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
    pub inference_image: Arc<RgbImage>, // 1024×1024 letterboxed for YOLO
    pub original_width_pts: f32,        // Original PDF page width in points
    pub original_height_pts: f32,       // Original PDF page height in points
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

pub struct ProcessingPipeline {
    pdf_bytes: Arc<[u8]>,
    #[allow(dead_code)]
    input_path: std::path::PathBuf,
    config: PipelineConfig,
    page_count: usize,
}

impl ProcessingPipeline {
    pub fn new(pdf_bytes: Vec<u8>, config: PipelineConfig) -> Result<Self> {
        let pdf_bytes: Arc<[u8]> = Arc::from(pdf_bytes.into_boxed_slice());
        // Use PdfiumRenderer to pre-cache page count without reloading the document elsewhere
        let mut raster_cfg = PdfRasterConfig::default();
        raster_cfg.render_forms = false;
        let renderer = PdfiumRenderer::new_from_bytes(pdf_bytes.clone(), raster_cfg)?;
        let page_count = renderer.page_count() as usize;
        Ok(Self {
            pdf_bytes,
            input_path: std::path::PathBuf::from("unknown.pdf"),
            config,
            page_count,
        })
    }

    pub fn from_file(input_path: std::path::PathBuf, config: PipelineConfig) -> Result<Self> {
        use memmap2::Mmap;
        use std::fs::File;

        let metadata = std::fs::metadata(&input_path)?;
        let file_size = metadata.len();

        let pdf_bytes = if file_size > 50 * 1024 * 1024 {
            let file = File::open(&input_path)?;
            let mmap = unsafe { Mmap::map(&file)? };
            mmap.to_vec()
        } else {
            std::fs::read(&input_path)?
        };

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
            Some(range) => Some((range.start.saturating_sub(1))..range.end.min(self.page_count)),
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
        #[cfg(feature = "debug-logging")]
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
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 2 {
            return Err(anyhow!("Page range must be in format 'start-end'"));
        }
        let start: usize = parts[0]
            .parse()
            .map_err(|_| anyhow!("Invalid start page number: {}", parts[0]))?;
        let end: usize = parts[1]
            .parse()
            .map_err(|_| anyhow!("Invalid end page number: {}", parts[1]))?;
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

#[derive(Clone, Debug)]
pub struct PipelineConfig {
    pub(crate) model_path: String,
    pub(crate) confidence_threshold: f32,
    pub(crate) nms_threshold: f32,
    pub(crate) output_dpi: u32,
    pub(crate) enable_ocr_hint: bool,
    pub(crate) target_height: u32,
    pub(crate) target_width: Option<u32>,
    /// Dithering for **layout image regions** only (not body text). Ignored when layout detection is off.
    pub(crate) image_region_dither_mode: ImageRegionDitherMode,
    pub(crate) binarization: BinarizationConfig,
    pub(crate) enable_ocr: bool,
    pub(crate) ocr_language: String,
    pub(crate) cover_format: CoverFormat,
    pub(crate) text_format: String,
    pub(crate) enable_layout_detection: bool,
    pub(crate) heavy_sauvola_concurrency: usize,
    pub(crate) page_range: Option<PageRange>,
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
    // Deskew settings
    pub(crate) enable_deskew: bool,
    pub(crate) deskew_rotation_model: Option<PathBuf>,
    pub(crate) deskew_unwarp_model: Option<PathBuf>,
    pub(crate) deskew_config: crate::deskew::DeskewConfig,
    // Rendering and inference sizes
    pub(crate) high_res_render_height: u32,
    pub(crate) inference_size: u32,
    // Keep original image quality for detected image regions
    pub(crate) keep_original_images: bool,
    /// Expand near–full-page figure boxes to the full raster (fixes YOLO under-segmentation).
    /// Overlap-merge skips full-page vs small inset pairs to avoid collapsing distinct figures.
    pub(crate) expand_full_bleed_figure_bboxes: bool,
    // DjVu IW44 quality (0-100 scale, maps to slices)
    pub(crate) djvu_iw44_quality: u8,
    /// Use the slow line-segmentation OCR pipeline instead of the fast region/tile path.
    pub(crate) slow_ocr: bool,
    /// Resolution multiplier applied to `target_height` when slow OCR is enabled,
    /// determining the height pdfium renders at ("render high, resize low"). The
    /// high-res raster feeds OCR; the encode path is resized back down to
    /// `target_height`. Clamped so the render height never exceeds
    /// `MAX_SLOW_OCR_RENDER_HEIGHT`.
    pub(crate) slow_ocr_scale: f32,
}

/// Upper bound on the slow-OCR render height (pixels) to cap memory use.
pub const MAX_SLOW_OCR_RENDER_HEIGHT: u32 = 6000;

impl PipelineConfig {
    pub fn new() -> Result<Self> {
        let model_path = runtime_asset_path_if_exists("yolo-layout.onnx")
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default();
        let enable_layout_detection = !model_path.is_empty();
        let config = Self {
            model_path,
            confidence_threshold: 0.4,
            nms_threshold: 0.5,
            output_dpi: 300,
            enable_ocr_hint: true,
            target_height: 1200,
            target_width: None,
            image_region_dither_mode: ImageRegionDitherMode::None,
            binarization: BinarizationConfig::default(),
            enable_ocr: false,
            ocr_language: "eng".to_string(),
            cover_format: CoverFormat::Jpeg,
            text_format: "jbig2".to_string(),
            enable_layout_detection,
            heavy_sauvola_concurrency: 4,
            page_range: None,
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
            enable_deskew: false,
            deskew_rotation_model: None,
            deskew_unwarp_model: None,
            deskew_config: crate::deskew::DeskewConfig::default(),
            high_res_render_height: 1200,
            inference_size: 1024,
            keep_original_images: true,
            expand_full_bleed_figure_bboxes: true,
            djvu_iw44_quality: 75, // Default to good quality
            slow_ocr: false,
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
            k_factor: 0.2,
            invert: false,
            invert_input: false,
            use_heavy_duty: false,
            patch_percentage: 0.0,
            no_patch: false,
            use_fixed_threshold: true,
            fixed_threshold: 200,
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
            "jbig2" | "ccitt4" | "jpeg" | "djvu" => {}
            _ => {
                return Err(anyhow!(
                    "Invalid text_format: '{}'. Must be one of: jbig2, ccitt4, jpeg, djvu",
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

        if self.enable_deskew {
            if let Some(path) = &self.deskew_rotation_model {
                if !path.exists() {
                    return Err(anyhow!(
                        "Deskew rotation model not found: {}",
                        path.display()
                    ));
                }
            }
            if let Some(path) = &self.deskew_unwarp_model {
                if !path.exists() {
                    return Err(anyhow!("Deskew unwarp model not found: {}", path.display()));
                }
            }
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
        self.enable_ocr
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
    pub fn enable_layout_detection(&self) -> bool {
        if self.invert_input {
            false
        } else {
            self.enable_layout_detection
        }
    }
    pub fn heavy_sauvola_concurrency(&self) -> usize {
        self.heavy_sauvola_concurrency
    }
    pub fn page_range(&self) -> Option<&PageRange> {
        self.page_range.as_ref()
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
            .unwrap_or_else(|| std::cmp::max(self.heavy_sauvola_concurrency * 2, 8))
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
    pub fn enable_deskew(&self) -> bool {
        self.enable_deskew
    }
    pub fn deskew_rotation_model(&self) -> Option<&PathBuf> {
        self.deskew_rotation_model.as_ref()
    }
    pub fn deskew_unwarp_model(&self) -> Option<&PathBuf> {
        self.deskew_unwarp_model.as_ref()
    }
    pub fn deskew_config(&self) -> &crate::deskew::DeskewConfig {
        &self.deskew_config
    }
    pub fn high_res_render_height(&self) -> u32 {
        self.high_res_render_height
    }
    pub fn inference_size(&self) -> u32 {
        self.inference_size
    }
    pub fn inference_resize_spec(&self) -> InferenceResizeSpec {
        YoloResizeConfig {
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
        self.slow_ocr
    }

    pub fn slow_ocr_scale(&self) -> f32 {
        self.slow_ocr_scale
    }

    /// Height (pixels) pdfium should render pages at. When slow OCR is enabled
    /// this is `target_height * slow_ocr_scale` (clamped), so a high-resolution
    /// raster is available for recognition; otherwise it equals `target_height`.
    pub fn source_render_height(&self) -> u32 {
        if self.slow_ocr && self.slow_ocr_scale > 1.0 {
            let scaled = (self.target_height as f32 * self.slow_ocr_scale).round() as u32;
            scaled
                .min(MAX_SLOW_OCR_RENDER_HEIGHT)
                .max(self.target_height)
        } else {
            self.target_height
        }
    }

    /// Width (pixels) pdfium should render at, scaled in proportion to
    /// `source_render_height`. `None` lets pdfium derive width from the page
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
        match format {
            "jbig2" | "ccitt4" | "jpeg" | "djvu" | "epub" => {
                self.text_format = format.to_string();
                // EPUB is a text-only reflowable format: it needs the slow,
                // structured OCR path and has no image-encoding stage.
                if format == "epub" {
                    self.set_slow_ocr(true);
                }
                Ok(())
            }
            _ => Err(anyhow!(
                "Invalid text_format: '{}'. Must be one of: jbig2, ccitt4, jpeg, djvu, epub",
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
        Ok(())
    }
    pub fn set_target_dimensions(&mut self, width: u32, height: u32) -> Result<()> {
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
        Ok(())
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
        self.enable_ocr = enable;
    }
    pub fn set_ocr_language(&mut self, language: &str) -> Result<()> {
        let normalized = language.trim().to_ascii_lowercase();
        Self::validate_ocr_language_code(&normalized)?;
        self.ocr_language = normalized;
        Ok(())
    }
    pub fn set_enable_layout_detection(&mut self, enable: bool) {
        self.enable_layout_detection = enable
            && !self.model_path.is_empty()
            && std::path::Path::new(&self.model_path).exists();
    }
    pub fn set_slow_ocr(&mut self, enable: bool) {
        self.slow_ocr = enable;
        if enable {
            self.enable_ocr = true;
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
    pub fn set_enable_deskew(&mut self, enable: bool) {
        self.enable_deskew = enable;
    }
    pub fn set_deskew_model_paths(&mut self, rotation: Option<PathBuf>, unwarp: Option<PathBuf>) {
        self.deskew_rotation_model = rotation;
        self.deskew_unwarp_model = unwarp;
    }
    pub fn set_deskew_config(&mut self, config: crate::deskew::DeskewConfig) {
        self.deskew_config = config;
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

    if let Ok(ld_path) = std::env::var("LD_LIBRARY_PATH") {
        for segment in ld_path.split(':').filter(|s| !s.is_empty()) {
            push_unique(PathBuf::from(segment));
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

pub fn locate_pdfium_in_runtime_dirs() -> Option<PathBuf> {
    runtime_search_directories()
        .into_iter()
        .map(|dir| PathBuf::from(Pdfium::pdfium_platform_library_name_at_path(&dir)))
        .find(|candidate| candidate.exists())
}

pub fn ensure_pdfium_available() -> Result<()> {
    if let Some(env_path) = std::env::var_os("PDFIUM_PATH") {
        let path = PathBuf::from(&env_path);
        crate::info_log!("Checking PDFIUM_PATH: {:?}", path);
        if path.exists() {
            crate::info_log!("Found pdfium at PDFIUM_PATH");
            return Ok(());
        }
    }

    crate::dbglog!("Searching for pdfium in runtime directories...");
    if let Some(found_path) = locate_pdfium_in_runtime_dirs() {
        crate::dbglog!("Found pdfium at: {:?}", found_path);
        return Ok(());
    }

    crate::dbglog!("Trying to bind to system library...");

    #[cfg(not(target_arch = "wasm32"))]
    #[cfg(not(feature = "static"))]
    if Pdfium::bind_to_system_library().is_ok() {
        return Ok(());
    }

    let library_name = Pdfium::pdfium_platform_library_name();
    Err(anyhow!(
        "Missing Pdfium library ({}). Place it next to the Lege executable or set PDFIUM_PATH to its full path before starting.",
        library_name.to_string_lossy()
    ))
}

pub fn locate_deskew_model_path(
    hint: Option<&PathBuf>,
    fallback_filename: &str,
) -> Option<PathBuf> {
    if let Some(path) = hint {
        if path.exists() {
            return Some(path.clone());
        }
    }

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            let candidate = parent.join(fallback_filename);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(fallback_filename);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    runtime_asset_path_if_exists(fallback_filename)
}
