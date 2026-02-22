// djvu.rs
//! Native Rust DJVU encoding using the djvu_encoder crate
//!
//! This module replaces the subprocess-based djvulibre orchestration with
//! direct in-memory encoding. Key improvements:
//! - No external dependencies (djvulibre binaries no longer required)
//! - In-memory processing (no temp file I/O for intermediate steps)
//! - Faster execution (no subprocess overhead)
//! - Simpler deployment (single binary)
//! - Better error handling (Rust Result types)
//! - Thread-safe by design

use anyhow::{anyhow, Context, Result};
use image::{Rgb, RgbImage};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Arc};
use unicode_normalization::UnicodeNormalization;
use tokio::sync::mpsc;

// Import DJVU encoder types
use djvu_encoder::doc::{DjvuBuilder, DjvuDocument, Page, PageBuilder, PageEncodeParams};
use djvu_encoder::image::image_formats::{Bitmap, Pixmap, GrayPixel, Pixel};

use crate::app_dirs;
use crate::engine::Detection;
use crate::dbglog;

fn djvu_hidden_text_enabled() -> bool {
    std::env::var("LEGE_DJVU_HIDDEN_TEXT")
        .ok()
        .as_deref()
        != Some("0")
}

/// Configuration for native DJVU encoding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DjvuConfig {
    /// DPI setting for output (default: 300)
    pub dpi: u32,
    /// Use clean encoding for bilevel (removes flyspecks)
    pub clean: bool,
    /// Lossy compression level for bilevel (0-200, None for lossless)
    pub lossy: Option<u32>,
    /// IW44 quality (0-100, default 100 = high quality, maps to slice count)
    /// 100 = 97 slices (30% more than C44 default)
    /// 50 = 74 slices (C44 default)
    /// 0 = 50 slices (lower quality, smaller files)
    pub iw44_quality: u8,
    /// Working directory for logs/debug (no temp files needed for encoding)
    pub work_dir: PathBuf,
    /// When true, apply PBM (binarized background) as a transparency mask for color layer
    pub pre_mask_color_layer: bool,
    /// If true, process in no-binarization mode (RGB image as IW44, no JB2 mask)
    pub no_binarization_mode: bool,
    /// Margin centering (handled upstream in pipeline, not here)
    pub center_margins: bool,
    /// Crop margins mode (handled upstream in pipeline, not here)
    pub crop_margins: bool,
    /// Early page assembly flag (kept for compatibility, unused in native encoder)
    pub early_page_assembly: bool,
}

impl Default for DjvuConfig {
    fn default() -> Self {
        Self {
            dpi: 300,
            clean: true,
            lossy: None,
            iw44_quality: 75,  // Good quality (85 slices, balanced quality/size)
            work_dir: app_dirs::djvu_base_dir(),
            pre_mask_color_layer: true,
            no_binarization_mode: false,
            center_margins: false,
            crop_margins: false,
            early_page_assembly: false,
        }
    }
}

/// Represents a detected image region with its location and image data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRegion {
    /// Bounding box [x1, y1, x2, y2] in page coordinates
    pub bbox: [f32; 4],
    /// Class ID from detection model
    pub class_id: i32,
    /// Class name (e.g., "image", "chart")
    pub class_name: String,
    /// Confidence score
    pub confidence: f32,
}

/// Data for a single page being processed
pub struct PageData {
    /// Page index (0-based)
    pub index: usize,
    /// High-resolution RGB image
    pub rgb_image: RgbImage,
    /// Binarized image data (0 or 255)
    pub binarized: Vec<u8>,
    /// Detected regions from layout detection
    pub detections: Vec<Detection>,
    /// Optional HOCR text (already scaled to page coordinates) if OCR enabled
    pub hocr: Option<String>,
}



/// Main native DJVU orchestrator
pub struct DjvuOrchestrator {
    config: DjvuConfig,
}

impl DjvuOrchestrator {
    /// Create a new native DJVU orchestrator
    pub fn new(config: DjvuConfig) -> Result<Self> {
        // Create working directory for logs (no temp files needed)
        fs::create_dir_all(&config.work_dir)
            .with_context(|| format!("Failed to create work directory: {:?}", config.work_dir))?;

        Ok(Self {
            config,
        })
    }

    /// Process a single page and encode it to DJVU format
    /// Thread-safe: can be called from multiple threads simultaneously
    /// Returns the encoded Page ready to add to DjvuDocument
    pub fn process_page(&self, page_data: PageData) -> Result<djvu_encoder::doc::Page> {
        let page_index = page_data.index;

        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU-Native] process_page START: page {}", page_index);

        let result = self.encode_page_internal(page_data);

        #[cfg(feature = "debug-logging")]
        match &result {
            Ok(_) => crate::success_log!("[DJVU-Native] process_page COMPLETE: page {}", page_index),
            Err(e) => crate::error_log!("[DJVU-Native] process_page FAILED: page {} - {}", page_index, e),
        }

        result
    }

    /// Internal page encoding logic
    fn encode_page_internal(&self, page_data: PageData) -> Result<djvu_encoder::doc::Page> {
        let page_num = page_data.index;
        let width = page_data.rgb_image.width();
        let height = page_data.rgb_image.height();

        dbglog!("[djvu-native] Encoding page {}: {}x{}", page_num, width, height);

        // Extract image-type regions using centralized LabelClassifier
        let classifier = &crate::types::LABEL_CLASSIFIER;
        let mut image_regions = Vec::new();
        
        if self.config.no_binarization_mode {
            // In no-binarization mode: treat the entire page as one IW44 image, no text layer
            image_regions.push(ImageRegion {
                bbox: [0.0, 0.0, width as f32, height as f32],
                class_id: 1,
                class_name: "page".to_string(),
                confidence: 1.0,
            });
            dbglog!("[djvu-native] NO-BINARIZATION mode - full page as single IW44");
        } else {
            // Normal mode: extract image regions from detections
            for detection in &page_data.detections {
                if !classifier.is_image_label(detection) {
                    continue;
                }
                image_regions.push(ImageRegion {
                    bbox: detection.bbox,
                    class_id: detection.class_id,
                    class_name: detection.class_name.clone().unwrap_or_default(),
                    confidence: detection.confidence,
                });
            }
        }

        // Build and return the page using the new encoder API
        let page = if self.config.no_binarization_mode {
            // No-binarization mode: encode entire page as IW44, no JB2 layer
            self.encode_no_binarization_page(page_num, &page_data, width, height)?
        } else {
            // Normal mode: JB2 text layer + optional IW44 color layer
            self.encode_normal_page(page_num, &page_data, &image_regions, width, height)?
        };

        dbglog!("[djvu-native] Page {} encoded successfully", page_num);
        Ok(page)
    }

    /// Encode a page in no-binarization mode (full IW44, no JB2)
    fn encode_no_binarization_page(
        &self,
        page_num: usize,
        page_data: &PageData,
        width: u32,
        height: u32,
    ) -> Result<djvu_encoder::doc::Page> {
        // Convert RGB image to Pixmap
        let pixmap = self.image_buffer_to_pixmap(&page_data.rgb_image)?;

        // Build page with IW44 background layer
        let page_builder = PageBuilder::new(page_num, width, height)
            .with_background(pixmap)
            .context("Failed to set background")?;

        // Add OCR hidden text layer when enabled. Skip empty word sets to avoid
        // emitting degenerate text chunks that some viewers can't parse.
        let page_builder = if djvu_hidden_text_enabled() {
            if let Some(ref hocr) = page_data.hocr {
                let words = self.parse_hocr_to_words(hocr, width, height)?;
                if words.is_empty() {
                    page_builder
                } else {
                    page_builder.with_ocr_words(words)
                }
            } else {
                page_builder
            }
        } else {
            page_builder
        };

        // Build page
        page_builder.build()
            .context("Failed to build no-binarization page")
    }

    /// Encode a page in normal mode (JB2 + optional IW44)
    fn encode_normal_page(
        &self,
        page_num: usize,
        page_data: &PageData,
        image_regions: &[ImageRegion],
        width: u32,
        height: u32,
    ) -> Result<djvu_encoder::doc::Page> {
        // 1. Prepare binary bitmap (with whiteout if needed)
        let mut bitmap = self.create_bitmap_from_binarized(
            &page_data.binarized,
            width as usize,
            height as usize,
        )?;

        // 2. White out image regions in the bitmap (CRITICAL: prevents JB2 bleed-through)
        if !image_regions.is_empty() {
            self.whiteout_image_regions(&mut bitmap, image_regions, width, height);
        }

        // 3. Build page with JB2 foreground (text layer)
        let mut page_builder = PageBuilder::new(page_num, width, height)
            .with_foreground(bitmap, 0, 0);

        // 4. Add IW44 color background if image regions exist
        if !image_regions.is_empty() {
            let color_canvas = self.compose_color_canvas(page_data, image_regions, width, height)?;
            page_builder = page_builder.with_background(color_canvas)
                .context("Failed to add background")?;
            dbglog!("[djvu-native] IW44 color background added");
        }

        // 5. Add OCR text layer when enabled; skip empty word sets.
        if djvu_hidden_text_enabled() {
            if let Some(ref hocr) = page_data.hocr {
                let words = self.parse_hocr_to_words(hocr, width, height)?;
                if !words.is_empty() {
                    let word_count = words.len();
                    page_builder = page_builder.with_ocr_words(words);
                    dbglog!("[djvu-native] OCR text layer added ({} words)", word_count);
                }
            }
        }

        // 6. Build the final page
        page_builder.build()
            .context("Failed to build page")
    }

    /// White out image regions in the binary bitmap
    /// CRITICAL: This prevents JB2 text from bleeding through color images
    fn whiteout_image_regions(
        &self,
        bitmap: &mut Bitmap,
        regions: &[ImageRegion],
        width: u32,
        height: u32,
    ) {
        for region in regions {
            let x1 = region.bbox[0].max(0.0).floor() as u32;
            let y1 = region.bbox[1].max(0.0).floor() as u32;
            let x2 = region.bbox[2].min(width as f32).ceil() as u32;
            let y2 = region.bbox[3].min(height as f32).ceil() as u32;

            // Set all pixels in this region to white
            for y in y1..y2.min(height) {
                for x in x1..x2.min(width) {
                    bitmap.put_pixel(x, y, GrayPixel::white());
                }
            }
        }

        dbglog!("[djvu-native] Whited out {} image regions", regions.len());
    }

    /// Compose a white canvas with image regions pasted onto it
    /// CRITICAL: Paste full color data (NO masking during composition)
    /// Returns a Pixmap ready to use as background layer
    fn compose_color_canvas(
        &self,
        page_data: &PageData,
        regions: &[ImageRegion],
        width: u32,
        height: u32,
    ) -> Result<Pixmap> {
        let rgb_image = &page_data.rgb_image;
        
        // Create white canvas
        let mut canvas_vec = vec![Pixel::new(255, 255, 255); (width * height) as usize];

        // Paste each image region
        for region in regions {
            let x1 = region.bbox[0].max(0.0).floor() as u32;
            let y1 = region.bbox[1].max(0.0).floor() as u32;
            let x2 = region.bbox[2].min(width as f32).ceil() as u32;
            let y2 = region.bbox[3].min(height as f32).ceil() as u32;

            if x1 >= width || y1 >= height || x2 <= x1 || y2 <= y1 {
                continue;
            }

            // Copy pixels from source to canvas
            for y in y1..y2.min(height) {
                for x in x1..x2.min(width) {
                    if x < rgb_image.width() && y < rgb_image.height() {
                        let pixel = rgb_image.get_pixel(x, y);
                        canvas_vec[(y * width + x) as usize] = Pixel::new(pixel[0], pixel[1], pixel[2]);
                    }
                }
            }
        }

        dbglog!("[djvu-native] Composed color canvas with {} regions", regions.len());
        Ok(Pixmap::from_vec(width, height, canvas_vec))
    }

    /// Convert ImageBuffer to Pixmap
    fn image_buffer_to_pixmap(&self, img: &RgbImage) -> Result<Pixmap> {
        let (width, height) = img.dimensions();
        let mut pixels = Vec::with_capacity((width * height) as usize);

        for y in 0..height {
            for x in 0..width {
                let pixel = img.get_pixel(x, y);
                pixels.push(Pixel::new(pixel[0], pixel[1], pixel[2]));
            }
        }

        Ok(Pixmap::from_vec(width, height, pixels))
    }

    /// Create a Bitmap from binarized data (0 or 255 grayscale)
    fn create_bitmap_from_binarized(
        &self,
        binarized: &[u8],
        width: usize,
        height: usize,
    ) -> Result<Bitmap> {
        if binarized.len() < width * height {
            return Err(anyhow!(
                "Binarized buffer too small: {} < {}",
                binarized.len(),
                width * height
            ));
        }

        let mut pixels = Vec::with_capacity(width * height);
        for &val in &binarized[..width * height] {
            // Convert 0/255 to black/white
            let is_black = if val <= 1 { val == 0 } else { val < 128 };
            pixels.push(if is_black {
                GrayPixel::black()
            } else {
                GrayPixel::white()
            });
        }

        Ok(Bitmap::from_vec(width as u32, height as u32, pixels))
    }

    /// Parse HOCR to word list format expected by djvu_encoder
    /// Returns: Vec<(text, x, y, width, height)>
    fn parse_hocr_to_words(
        &self,
        hocr: &str,
        page_width: u32,
        page_height: u32,
    ) -> Result<Vec<(String, u16, u16, u16, u16)>> {
        use regex::Regex;

        // Debug: log HOCR length and first 500 chars
        #[cfg(feature = "debug-logging")]
        {
            crate::info_log!("[parse_hocr_to_words] HOCR length: {} chars", hocr.len());
            if !hocr.is_empty() {
                let preview: String = hocr.chars().take(500).collect();
                crate::info_log!("[parse_hocr_to_words] HOCR preview: {}", preview);
            }
        }

        let word_re = Regex::new(
            r#"<span class=['"]ocrx_word['"].*?title=['"]bbox\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)['"].*?>(.*?)</span>"#
        )?;

        let mut words = Vec::new();

        for cap in word_re.captures_iter(hocr) {
            // Parse as u32 first to avoid truncation on high-DPI pages
            let x1_raw: u32 = cap[1].parse().unwrap_or(0);
            let y1_raw: u32 = cap[2].parse().unwrap_or(0);
            let x2_raw: u32 = cap[3].parse().unwrap_or(0);
            let y2_raw: u32 = cap[4].parse().unwrap_or(0);

            if x2_raw <= x1_raw || y2_raw <= y1_raw {
                continue;
            }

            // Clamp coordinates to u16 range (DjVu max dimension: 65535)
            // This matches the u16 limit in from_word_boxes()
            let x1 = x1_raw.min(u16::MAX as u32) as u16;
            let y1 = y1_raw.min(u16::MAX as u32) as u16;
            let x2 = x2_raw.min(u16::MAX as u32) as u16;
            let y2 = y2_raw.min(u16::MAX as u32) as u16;

            // Recalculate width/height after clamping
            let width = x2.saturating_sub(x1);
            let height = y2.saturating_sub(y1);

            // Skip invalid boxes (zero or negative dimensions after clamping)
            if width == 0 || height == 0 {
                continue;
            }

            let mut text = cap[5].trim().to_string();
            if text.is_empty() {
                continue;
            }

            // Normalize and clean text
            text = text.nfkc().collect::<String>();
            text.retain(|c| !c.is_control() || c == '\t' || c == ' ');

            words.push((text, x1, y1, width, height));
        }

        #[cfg(feature = "debug-logging")]
        crate::info_log!("[parse_hocr_to_words] Found {} words from HOCR", words.len());

        Ok(words)
    }

    /// Finalize a DjvuDocument and write to file
    /// This is called by the writer actor after all pages have been added
    pub fn finalize_document(
        doc: &djvu_encoder::doc::DjvuDocument,
        output_path: &Path,
    ) -> Result<()> {
        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU-Native] Finalizing document");

        // Finalize and get bytes
        let djvu_bytes = doc.finalize()
            .context("Failed to finalize DJVU document")?;

        dbglog!("[djvu-native] Document built: {} bytes", djvu_bytes.len());

        // Write to file
        fs::write(output_path, &djvu_bytes)
            .with_context(|| format!("Failed to write DJVU file: {:?}", output_path))?;

        #[cfg(feature = "debug-logging")]
        crate::success_log!(
            "[DJVU-Native] Document finalized: {} bytes",
            djvu_bytes.len()
        );

        Ok(())
    }



    /// Preflight check (no external dependencies needed for native encoder)
    pub fn preflight_check(&self, _require_text_layer: bool) -> Result<()> {
        // Native encoder has no external dependencies
        // Just verify work directory is writable
        fs::create_dir_all(&self.config.work_dir)
            .with_context(|| format!("Work directory not writable: {:?}", self.config.work_dir))?;
        
        Ok(())
    }

    /// Clean up temporary files (native encoder doesn't create temp files)
    pub fn cleanup(&self) -> Result<()> {
        dbglog!("[djvu-native] cleanup() called (no temp files to remove)");
        // Native encoder doesn't create temp files, so nothing to clean
        Ok(())
    }

    /// Fast cleanup (same as cleanup for native encoder)
    pub fn cleanup_work_dir_only(&self) -> Result<()> {
        self.cleanup()
    }
}

//==============================================================================
// DjVu Writer Actor - Concurrent Document Assembly
//==============================================================================

/// Message types for DjVu writer actor
pub enum DjvuWriterMessage {
    /// Add a page to the document
    AppendPage { page: Page, page_index: usize },
    /// Finalize the document
    Finalize,
}

/// Handle for sending pages to the DjVu writer actor
pub struct DjvuWriterHandle {
    sender: mpsc::UnboundedSender<DjvuWriterMessage>,
}

impl DjvuWriterHandle {
    /// Send a page to be added to the document
    pub fn append_page(&self, page: Page, page_index: usize) -> Result<(), anyhow::Error> {
        self.sender
            .send(DjvuWriterMessage::AppendPage { page, page_index })
            .map_err(|_| anyhow::anyhow!("DjVu writer actor has stopped"))?;
        Ok(())
    }

    /// Finalize the document
    pub fn finalize(&self) -> Result<(), anyhow::Error> {
        self.sender
            .send(DjvuWriterMessage::Finalize)
            .map_err(|_| anyhow::anyhow!("DjVu writer actor has stopped"))?;
        Ok(())
    }
}

/// Spawn a dedicated DjVu writer actor that owns the DjvuDocument
/// Returns a handle for sending pages and a JoinHandle for the actor task
pub fn spawn_djvu_writer_actor(
    output_path: PathBuf,
    total_pages: usize,
    dpi: u32,
    iw44_quality: u8,
    progress_tracker: crate::progress::ProgressTracker,
) -> (DjvuWriterHandle, tokio::task::JoinHandle<Result<(), anyhow::Error>>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<DjvuWriterMessage>();

    let handle = DjvuWriterHandle { sender: tx };

    // Map quality (0-100) to slice count:
    // 100 = 97 slices (30% above C44 default, high quality)
    // 50 = 74 slices (C44 default)
    // 0 = 50 slices (lower quality, smaller files)
    let slices = match iw44_quality {
        100 => 97,  // High quality
        q if q >= 50 => 74 + ((q as usize - 50) * 23 / 50),  // 74-97 range
        q => 50 + (q as usize * 24 / 50),  // 50-74 range
    };

    let task = tokio::spawn(async move {
        // Build the DjVu document with explicit IW44 slice configuration.
        let mut params = PageEncodeParams::default();
        params.slices = Some(slices);
        let doc = DjvuBuilder::new(total_pages)
            .with_dpi(dpi)
            .with_params(params)
            .build();

        let mut pages_written = 0usize;
        let mut page_buffer: std::collections::BTreeMap<usize, Page> = std::collections::BTreeMap::new();
        let mut next_expected = 0usize;

        crate::info_log!("[DjvuWriterActor] Started, waiting for pages...");

        while let Some(msg) = rx.recv().await {
            match msg {
                DjvuWriterMessage::AppendPage { page, page_index } => {
                    // Buffer the incoming page
                    page_buffer.insert(page_index, page);

                    // Add all consecutive pages starting from next_expected
                    while let Some(page) = page_buffer.remove(&next_expected) {
                        if let Err(e) = doc.add_page(page) {
                            crate::warn_log!("[DjvuWriterActor] Failed to append page {}: {}", next_expected, e);
                            return Err(anyhow::anyhow!("Failed to append page {}: {}", next_expected, e));
                        }

                        pages_written += 1;
                        next_expected += 1;
                        // Writer-side progress reflects successful document assembly.
                        progress_tracker.update(crate::progress::ProcessingStatus::PdfAppend {
                            current: pages_written,
                            total: total_pages,
                        });
                    }
                }
                DjvuWriterMessage::Finalize => {
                    crate::info_log!("[DjvuWriterActor] Finalize requested, written {} of {} pages", pages_written, total_pages);

                    // Check if all pages were received
                    if pages_written != total_pages {
                        crate::warn_log!(
                            "[DjvuWriterActor] Not all pages received: {} of {} pages",
                            pages_written,
                            total_pages
                        );
                        return Err(anyhow::anyhow!(
                            "Document incomplete: {} of {} pages",
                            pages_written,
                            total_pages
                        ));
                    }

                    // Finalize and write to file
                    DjvuOrchestrator::finalize_document(&doc, &output_path)?;

                    crate::success_log!("[DjvuWriterActor] Document finalized successfully");
                    break;
                }
            }
        }

        Ok(())
    });

    (handle, task)
}
