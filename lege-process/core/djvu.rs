// djvu.rs
// DjVu output driver.
//
// Native Rust DjVu encoding through the typed DJVULibrust API.

use anyhow::{Context, Result, anyhow};
use image::RgbImage;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use unicode_normalization::UnicodeNormalization;

use djvu_encoder::doc::{
    Bookmark, DjVmNav, DjvuBuilder, DjvuDocument, EncodedPage, Page, PageBuilder, PageEncodeParams,
};
use djvu_encoder::image::image_formats::{Bitmap, GrayPixel, Pixel, Pixmap};

use crate::app_dirs;
use crate::dbglog;
use crate::engine::Detection;

fn djvu_hidden_text_enabled() -> bool {
    std::env::var("LEGE_DJVU_HIDDEN_TEXT").ok().as_deref() != Some("0")
}

/// Diagnostic string for the active native DjVu acceleration paths.
pub fn active_backend_info() -> String {
    format!(
        "primitives={}, parallel={}",
        djvu_encoder::active_primitives_backend(),
        djvu_encoder::active_parallel_backend()
    )
}

/// Configuration for DJVU encoding
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
    /// Working directory for optional debug dumps (encoding is in memory).
    pub work_dir: PathBuf,
    /// When true, apply PBM (binarized background) as a transparency mask for color layer
    pub pre_mask_color_layer: bool,
    /// If true, process in no-binarization mode (RGB image as IW44, no JB2 mask)
    pub no_binarization_mode: bool,
    /// When true, image regions are dithered directly into the JB2 foreground.
    pub dither_image_regions: bool,
    /// Margin centering (handled upstream in pipeline, not here)
    pub center_margins: bool,
    /// Crop margins mode (handled upstream in pipeline, not here)
    pub crop_margins: bool,
    /// Early page assembly flag (kept for compatibility, unused)
    pub early_page_assembly: bool,
}

impl Default for DjvuConfig {
    fn default() -> Self {
        Self {
            dpi: 300,
            clean: true,
            lossy: None,
            iw44_quality: 75, // Good quality (85 slices, balanced quality/size)
            work_dir: app_dirs::djvu_base_dir(),
            pre_mask_color_layer: true,
            no_binarization_mode: false,
            dither_image_regions: false,
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
    /// Render this page as one full-color IW44 background. Used for the source
    /// document's first page when cover preservation is enabled.
    pub preserve_full_color: bool,
    /// High-resolution RGB image
    pub rgb_image: RgbImage,
    /// Binarized image data (0 or 255) - always present for DJVU JB2 encoding
    pub binarized: Vec<u8>,
    /// Cleaned grayscale full page (0-255), `Some` only in grayscale/MRC mode;
    /// used as the IW44 background under the JB2 ink mask.
    pub cleaned_gray: Option<Vec<u8>>,
    /// Detected regions from layout detection
    pub detections: Vec<Detection>,
    /// Optional HOCR text (already scaled to page coordinates) if OCR enabled
    pub hocr: Option<String>,
}

/// A typed page ready for DJVULibrust's page encoder. Covers override the
/// document background subsampling so their full-page color remains full-res.
pub struct PreparedDjvuPage {
    pub page: Page,
    pub bg_subsample: Option<u8>,
}

impl PreparedDjvuPage {
    /// Compress this page with the document policy, applying a cover-specific
    /// background-subsampling override when present.
    pub fn encode(
        self,
        document: &DjvuDocument,
        iw44_quality: u8,
        document_bg_subsample: usize,
    ) -> Result<EncodedPage> {
        let mut params = PageEncodeParams::default();
        params.slices = Some(quality_to_slices(iw44_quality));
        params.bg_subsample = self
            .bg_subsample
            .unwrap_or_else(|| document_bg_subsample.clamp(1, 12) as u8);
        document
            .encode_page_with_params(self.page, &params)
            .context("Failed to encode DjVu page")
    }
}

// ============================================================================
// Orchestrator: composes typed page layers in memory
// ============================================================================

/// Composes DjVu page layers using DJVULibrust's typed API.
pub struct DjvuOrchestrator {
    config: DjvuConfig,
}

impl DjvuOrchestrator {
    /// Create a new DJVU orchestrator
    pub fn new(config: DjvuConfig) -> Result<Self> {
        fs::create_dir_all(&config.work_dir)
            .with_context(|| format!("Failed to create work directory: {:?}", config.work_dir))?;

        crate::info_log!("[DJVU] Encoder: {}", active_backend_info());

        Ok(Self { config })
    }

    /// Directory used only for optional PBM debug dumps.
    pub fn work_dir(&self) -> &Path {
        &self.config.work_dir
    }

    /// Compose a single page's typed layers in memory. Thread-safe.
    pub fn process_page(&self, page_data: PageData) -> Result<PreparedDjvuPage> {
        let page_index = page_data.index;

        crate::info_log!("[DJVU] process_page START: page {}", page_index);

        let result = self.compose_page(page_data);

        #[cfg(feature = "debug-logging")]
        match &result {
            Ok(_) => crate::success_log!("[DJVU] process_page COMPLETE: page {}", page_index),
            Err(e) => crate::error_log!("[DJVU] process_page FAILED: page {} - {}", page_index, e),
        }
        let _ = page_index;

        result
    }

    fn compose_page(&self, page_data: PageData) -> Result<PreparedDjvuPage> {
        let page_num = page_data.index;
        let width = page_data.rgb_image.width();
        let height = page_data.rgb_image.height();

        dbglog!("[djvu] Composing page {}: {}x{}", page_num, width, height);

        // Extract image-type regions using centralized LabelClassifier
        let classifier = &crate::types::LABEL_CLASSIFIER;
        let mut image_regions = Vec::new();

        if self.config.no_binarization_mode {
            image_regions.push(ImageRegion {
                bbox: [0.0, 0.0, width as f32, height as f32],
                class_id: crate::types::class_id_for("image").unwrap_or(1),
                class_name: "image".to_string(),
                confidence: 1.0,
            });
            dbglog!("[djvu] NO-BINARIZATION mode - full page as single IW44");
        } else {
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

        if page_data.preserve_full_color {
            dbglog!("[djvu] Cover page - preserving full-page color IW44");
            self.compose_no_binarization_page(page_num, &page_data, width, height)
        } else if self.config.no_binarization_mode {
            self.compose_no_binarization_page(page_num, &page_data, width, height)
        } else {
            self.compose_normal_page(page_num, &page_data, &image_regions, width, height)
        }
    }

    /// No-binarization mode: whole page as a single IW44 background, no mask.
    fn compose_no_binarization_page(
        &self,
        page_num: usize,
        page_data: &PageData,
        width: u32,
        height: u32,
    ) -> Result<PreparedDjvuPage> {
        let pixmap = rgb_image_to_pixmap(&page_data.rgb_image);
        let builder = PageBuilder::new(page_num, width, height)
            .with_background(pixmap)
            .context("Failed to set full-page DjVu background")?;
        let builder = self.add_ocr(builder, page_data, width, height)?;
        Ok(PreparedDjvuPage {
            page: builder
                .build()
                .context("Failed to build full-color DjVu page")?,
            bg_subsample: page_data.preserve_full_color.then_some(1),
        })
    }

    /// Normal mode: JB2 ink mask + optional IW44 background + optional OCR.
    fn compose_normal_page(
        &self,
        page_num: usize,
        page_data: &PageData,
        image_regions: &[ImageRegion],
        width: u32,
        height: u32,
    ) -> Result<PreparedDjvuPage> {
        self.maybe_dump_preencode_pbm(page_num, &page_data.binarized, width, height)?;

        // Preserve the current blank-page semantics with an explicit white page.
        if self.is_binarized_blank(&page_data.binarized, width, height) && image_regions.is_empty()
        {
            let builder = PageBuilder::new(page_num, width, height)
                .with_background(Pixmap::from_pixel(width, height, Pixel::white()))
                .context("Failed to set blank DjVu page background")?;
            return Ok(PreparedDjvuPage {
                page: builder.build().context("Failed to build blank DjVu page")?,
                bg_subsample: None,
            });
        }

        // 1. Prepare the bilevel ink mask (raw Luma8 buffer).
        let mut mask = self.bitmap_from_binarized(&page_data.binarized, width, height)?;

        let use_color_background = !self.config.dither_image_regions;

        // 2. White out image regions in the mask (prevents JB2 bleed-through).
        if use_color_background && !image_regions.is_empty() {
            self.whiteout_image_regions(&mut mask, image_regions, width, height);
        }

        // 3. Compose the IW44 background:
        //    - grayscale/MRC mode (cleaned_gray present): full-page grayscale
        //      canvas under the ink mask, always written;
        //    - otherwise: a colour canvas only when image regions exist.
        let background = if let Some(cleaned) = page_data.cleaned_gray.as_deref() {
            let canvas =
                self.compose_grayscale_canvas(cleaned, page_data, image_regions, width, height);
            dbglog!("[djvu] IW44 grayscale MRC background composed");
            Some(rgb_image_to_pixmap(&canvas))
        } else if use_color_background && !image_regions.is_empty() {
            let canvas = self.compose_color_canvas(page_data, image_regions, width, height);
            dbglog!("[djvu] IW44 color background composed");
            Some(rgb_image_to_pixmap(&canvas))
        } else {
            None
        };

        let mut builder = PageBuilder::new(page_num, width, height).with_foreground(mask, 0, 0);
        if let Some(background) = background {
            builder = builder
                .with_background(background)
                .context("Failed to set DjVu MRC background")?;
        }
        builder = self.add_ocr(builder, page_data, width, height)?;

        Ok(PreparedDjvuPage {
            page: builder.build().context("Failed to build DjVu page")?,
            bg_subsample: None,
        })
    }

    fn is_binarized_blank(&self, binarized: &[u8], width: u32, height: u32) -> bool {
        let len = (width as usize).saturating_mul(height as usize);
        binarized.iter().take(len).all(|&v| v == 1 || v >= 128)
    }

    fn maybe_dump_preencode_pbm(
        &self,
        page_num: usize,
        binarized: &[u8],
        width: u32,
        height: u32,
    ) -> Result<()> {
        let enabled = std::env::var("LEGE_DJVU_DEBUG_PBM")
            .map(|value| value != "0")
            .unwrap_or(false);
        if !enabled {
            return Ok(());
        }

        let out_dir = self.config.work_dir.join("preencode-pbm");
        fs::create_dir_all(&out_dir)
            .with_context(|| format!("Failed to create DjVu PBM debug directory: {:?}", out_dir))?;
        let out_path = out_dir.join(format!("page_{:04}.pbm", page_num + 1));

        let row_bytes = ((width as usize) + 7) / 8;
        let mut bytes = Vec::with_capacity(("P4\n".len() + 32) + row_bytes * height as usize);
        bytes.extend_from_slice(format!("P4\n{} {}\n", width, height).as_bytes());

        for y in 0..height as usize {
            for byte_x in 0..row_bytes {
                let mut packed = 0u8;
                for bit in 0..8 {
                    let x = byte_x * 8 + bit;
                    if x >= width as usize {
                        continue;
                    }
                    let val = binarized[y * width as usize + x];
                    let is_black = if val <= 1 { val == 0 } else { val < 128 };
                    if is_black {
                        packed |= 0x80 >> bit;
                    }
                }
                bytes.push(packed);
            }
        }

        fs::write(&out_path, bytes)
            .with_context(|| format!("Failed to write DjVu PBM debug file: {:?}", out_path))?;
        Ok(())
    }

    /// Build the bilevel ink mask: ink → black, background → white.
    fn bitmap_from_binarized(&self, binarized: &[u8], width: u32, height: u32) -> Result<Bitmap> {
        let n = (width as usize) * (height as usize);
        if binarized.len() < n {
            return Err(anyhow!(
                "Binarized buffer too small: {} < {}",
                binarized.len(),
                n
            ));
        }
        let mut buf = Vec::with_capacity(n);
        for &val in &binarized[..n] {
            let is_black = if val <= 1 { val == 0 } else { val < 128 };
            buf.push(if is_black {
                GrayPixel::black()
            } else {
                GrayPixel::white()
            });
        }
        Ok(Bitmap::from_vec(width, height, buf))
    }

    /// White out image regions in the ink mask (set to 255/background) so JB2
    /// text does not bleed through color/photo regions.
    fn whiteout_image_regions(
        &self,
        mask: &mut Bitmap,
        regions: &[ImageRegion],
        width: u32,
        height: u32,
    ) {
        let w = width as usize;
        let pixels = mask.pixels_mut();
        for region in regions {
            let x1 = region.bbox[0].max(0.0).floor() as usize;
            let y1 = region.bbox[1].max(0.0).floor() as usize;
            let x2 = (region.bbox[2].ceil() as usize).min(width as usize);
            let y2 = (region.bbox[3].ceil() as usize).min(height as usize);
            for row in y1..y2 {
                let start = row * w + x1;
                let end = row * w + x2;
                if let Some(slice) = pixels.get_mut(start..end) {
                    slice.fill(GrayPixel::white());
                }
            }
        }
        dbglog!("[djvu] Whited out {} image regions", regions.len());
    }

    /// Compose the IW44 background for a grayscale/MRC page: the cleaned
    /// grayscale full page with ink cores filled white (the JB2 mask paints
    /// those), and original colour image-region crops pasted on top so figures
    /// keep their tone.
    fn compose_grayscale_canvas(
        &self,
        cleaned_gray: &[u8],
        page_data: &PageData,
        regions: &[ImageRegion],
        width: u32,
        height: u32,
    ) -> RgbImage {
        let n = (width as usize) * (height as usize);
        let bin = &page_data.binarized;
        let mut canvas = Vec::with_capacity(n * 3);
        for i in 0..n {
            let v = if bin.get(i).copied().unwrap_or(255) == 0 {
                255
            } else {
                cleaned_gray.get(i).copied().unwrap_or(255)
            };
            canvas.push(v);
            canvas.push(v);
            canvas.push(v);
        }

        paste_regions(&mut canvas, width, height, &page_data.rgb_image, regions);
        RgbImage::from_raw(width, height, canvas).expect("grayscale canvas buffer sized w*h*3")
    }

    /// Compose a white canvas with the original colour image regions pasted on.
    fn compose_color_canvas(
        &self,
        page_data: &PageData,
        regions: &[ImageRegion],
        width: u32,
        height: u32,
    ) -> RgbImage {
        let n = (width as usize) * (height as usize);
        let mut canvas = vec![255u8; n * 3];
        paste_regions(&mut canvas, width, height, &page_data.rgb_image, regions);
        dbglog!(
            "[djvu] Composed color canvas with {} regions",
            regions.len()
        );
        RgbImage::from_raw(width, height, canvas).expect("color canvas buffer sized w*h*3")
    }

    fn add_ocr(
        &self,
        builder: PageBuilder,
        page_data: &PageData,
        width: u32,
        height: u32,
    ) -> Result<PageBuilder> {
        if !djvu_hidden_text_enabled() {
            return Ok(builder);
        }
        let Some(hocr) = page_data.hocr.as_deref() else {
            return Ok(builder);
        };
        let words = self.parse_hocr_to_words(hocr, width, height)?;
        if words.is_empty() {
            return Ok(builder);
        }
        dbglog!("[djvu] OCR text layer prepared ({} words)", words.len());
        Ok(builder.with_ocr_words(words))
    }

    /// Parse HOCR into (text, x, y, width, height) word boxes in page coords.
    fn parse_hocr_to_words(
        &self,
        hocr: &str,
        page_width: u32,
        page_height: u32,
    ) -> Result<Vec<(String, u16, u16, u16, u16)>> {
        use regex::Regex;
        use std::sync::LazyLock;

        static WORD_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r#"(?is)<span[^>]*class=['"][^'"]*\bocrx_word\b[^'"]*['"][^>]*title=['"]([^'"]*)['"][^>]*>(.*?)</span>"#,
            )
            .expect("valid ocrx_word regex")
        });
        static BBOX_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?i)\bbbox\s+(-?\d+)\s+(-?\d+)\s+(-?\d+)\s+(-?\d+)"#)
                .expect("valid bbox regex")
        });
        static TAG_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#"(?is)<[^>]+>"#).expect("valid tag regex"));
        let word_re = &*WORD_RE;
        let bbox_re = &*BBOX_RE;
        let tag_re = &*TAG_RE;

        let mut words = Vec::new();

        for cap in word_re.captures_iter(hocr) {
            let title = cap.get(1).map_or("", |m| m.as_str());
            let Some(bbox_caps) = bbox_re.captures(title) else {
                continue;
            };

            let x1_i: i32 = bbox_caps
                .get(1)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let y1_i: i32 = bbox_caps
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let x2_i: i32 = bbox_caps
                .get(3)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let y2_i: i32 = bbox_caps
                .get(4)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);

            let x1_raw = x1_i.max(0).min(page_width as i32) as u32;
            let y1_raw = y1_i.max(0).min(page_height as i32) as u32;
            let x2_raw = x2_i.max(0).min(page_width as i32) as u32;
            let y2_raw = y2_i.max(0).min(page_height as i32) as u32;

            if x2_raw <= x1_raw || y2_raw <= y1_raw {
                continue;
            }

            let x1 = x1_raw.min(u16::MAX as u32) as u16;
            let y1 = y1_raw.min(u16::MAX as u32) as u16;
            let x2 = x2_raw.min(u16::MAX as u32) as u16;
            let y2 = y2_raw.min(u16::MAX as u32) as u16;

            let width = x2.saturating_sub(x1);
            let height = y2.saturating_sub(y1);
            if width == 0 || height == 0 {
                continue;
            }

            let raw_text = cap.get(2).map_or("", |m| m.as_str());
            let mut text = tag_re.replace_all(raw_text, "").into_owned();
            text = text
                .replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&#39;", "'");
            text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if text.is_empty() {
                continue;
            }

            text = text.nfkc().collect::<String>();
            text.retain(|c| !c.is_control() || c == '\t' || c == ' ');

            words.push((text, x1, y1, width, height));
        }

        Ok(words)
    }

    /// Preflight: the native encoder has no runtime helper; only debug output
    /// needs a writable work directory.
    pub fn preflight_check(&self, _require_text_layer: bool) -> Result<()> {
        fs::create_dir_all(&self.config.work_dir)
            .with_context(|| format!("Work directory not writable: {:?}", self.config.work_dir))?;
        Ok(())
    }

    /// Native encoding creates no page staging files.
    pub fn cleanup(&self) -> Result<()> {
        dbglog!("[djvu] cleanup() called (native encoder has no staging files)");
        Ok(())
    }

    pub fn cleanup_work_dir_only(&self) -> Result<()> {
        self.cleanup()
    }
}

/// Paste original colour image-region crops onto an RGB canvas buffer.
fn paste_regions(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    rgb_image: &RgbImage,
    regions: &[ImageRegion],
) {
    let src = rgb_image.as_raw();
    let src_w = rgb_image.width() as usize;
    let dst_w = width as usize;
    for region in regions {
        let x1 = region.bbox[0].max(0.0).floor() as usize;
        let y1 = region.bbox[1].max(0.0).floor() as usize;
        let x2 = (region.bbox[2].ceil() as usize)
            .min(width as usize)
            .min(src_w);
        let y2 = (region.bbox[3].ceil() as usize)
            .min(height as usize)
            .min(rgb_image.height() as usize);
        if x1 >= x2 || y1 >= y2 {
            continue;
        }
        let span = (x2 - x1) * 3;
        for row in y1..y2 {
            let src_start = (row * src_w + x1) * 3;
            let dst_start = (row * dst_w + x1) * 3;
            canvas[dst_start..dst_start + span].copy_from_slice(&src[src_start..src_start + span]);
        }
    }
}

fn rgb_image_to_pixmap(image: &RgbImage) -> Pixmap {
    let (width, height) = image.dimensions();
    let pixels = image
        .as_raw()
        .chunks_exact(3)
        .map(|rgb| Pixel::new(rgb[0], rgb[1], rgb[2]))
        .collect();
    Pixmap::from_vec(width, height, pixels)
}

fn outline_to_navigation(
    outline: Vec<lege_pdf_write::outline::OutlineItem>,
    total_pages: usize,
) -> Result<DjVmNav> {
    fn convert(
        item: lege_pdf_write::outline::OutlineItem,
        total_pages: usize,
        count: &mut usize,
    ) -> Result<Bookmark> {
        *count += 1;
        if *count > u16::MAX as usize {
            return Err(anyhow!("DjVu outline has more than 65535 entries"));
        }
        let title = item.title.trim();
        if title.is_empty() {
            return Err(anyhow!("DjVu outline entry has an empty title"));
        }
        if item.children.len() > u8::MAX as usize {
            return Err(anyhow!(
                "DjVu outline entry {title:?} has more than 255 direct children"
            ));
        }
        let page_index = item.page_index as usize;
        if page_index >= total_pages {
            return Err(anyhow!(
                "DjVu outline entry {title:?} points to page {page_index}, but document has {total_pages} pages"
            ));
        }
        Ok(Bookmark {
            title: title.to_owned(),
            dest: format!("#p{:04}.djvu", page_index + 1),
            children: item
                .children
                .into_iter()
                .map(|child| convert(child, total_pages, count))
                .collect::<Result<Vec<_>>>()?,
        })
    }

    let mut count = 0;
    Ok(DjVmNav {
        bookmarks: outline
            .into_iter()
            .map(|item| convert(item, total_pages, &mut count))
            .collect::<Result<Vec<_>>>()?,
    })
}

//==============================================================================
// DjVu Writer Actor - deterministic in-memory document assembly
//==============================================================================

/// Message types for the DjVu writer actor.
pub enum DjvuWriterMessage {
    /// Insert an already encoded page into the document.
    AppendEncoded {
        encoded: EncodedPage,
        page_index: usize,
    },
    /// Set the accepted document navigation tree before finalization.
    SetOutline(Vec<lege_pdf_write::outline::OutlineItem>),
    /// Finalize and atomically publish the document.
    Finalize,
}

/// Handle for sending encoded pages to the DjVu writer actor.
#[derive(Clone)]
pub struct DjvuWriterHandle {
    sender: mpsc::Sender<DjvuWriterMessage>,
}

impl DjvuWriterHandle {
    pub async fn append_encoded(
        &self,
        encoded: EncodedPage,
        page_index: usize,
    ) -> Result<(), anyhow::Error> {
        self.sender
            .send(DjvuWriterMessage::AppendEncoded {
                encoded,
                page_index,
            })
            .await
            .map_err(|_| anyhow!("DjVu writer actor has stopped"))?;
        Ok(())
    }

    pub async fn send_outline(
        &self,
        outline: Vec<lege_pdf_write::outline::OutlineItem>,
    ) -> Result<(), anyhow::Error> {
        self.sender
            .send(DjvuWriterMessage::SetOutline(outline))
            .await
            .map_err(|_| anyhow!("DjVu writer actor has stopped"))
    }

    /// Finalize and atomically publish the document.
    pub async fn finalize(&self) -> Result<(), anyhow::Error> {
        self.sender
            .send(DjvuWriterMessage::Finalize)
            .await
            .map_err(|_| anyhow!("DjVu writer actor has stopped"))?;
        Ok(())
    }
}

fn drain_ready_pages<T>(buffer: &mut BTreeMap<usize, T>, next_expected: &mut usize) -> Vec<T> {
    let mut ready = Vec::new();
    while let Some(page) = buffer.remove(next_expected) {
        ready.push(page);
        *next_expected += 1;
    }
    ready
}

/// Map IW44 quality (0-100) to a slice count (kept from the previous encoder).
fn quality_to_slices(iw44_quality: u8) -> usize {
    match iw44_quality {
        100 => 97,
        q if q >= 50 => 74 + ((q as usize - 50) * 23 / 50),
        q => 50 + (q as usize * 24 / 50),
    }
}

/// Spawn the DjVu writer actor. Page compression happens concurrently in the
/// pipeline; this actor inserts completed pages in deterministic order and
/// performs the final atomic publication.
pub fn spawn_djvu_writer_actor(
    output_path: PathBuf,
    total_pages: usize,
    dpi: u32,
    iw44_quality: u8,
    bg_subsample: usize,
    progress_tracker: crate::progress::ProgressTracker,
    channel_capacity: usize,
    cancel: lege_pdf_read::CancellationToken,
) -> (
    DjvuWriterHandle,
    Arc<DjvuDocument>,
    tokio::task::JoinHandle<Result<(), anyhow::Error>>,
) {
    let (tx, mut rx) = mpsc::channel::<DjvuWriterMessage>(channel_capacity.max(1));
    let handle = DjvuWriterHandle { sender: tx };

    let mut params = PageEncodeParams::default();
    params.slices = Some(quality_to_slices(iw44_quality));
    params.bg_subsample = bg_subsample.clamp(1, 12) as u8;
    let doc = Arc::new(
        DjvuBuilder::new(total_pages)
            .with_dpi(dpi)
            .with_params(params)
            .build(),
    );
    let writer_doc = Arc::clone(&doc);

    let task = tokio::spawn(async move {
        let mut page_buffer: BTreeMap<usize, EncodedPage> = BTreeMap::new();
        let mut next_expected = 0usize;
        let mut pages_written = 0usize;
        let mut navigation = DjVmNav::default();

        crate::info_log!("[DjvuWriterActor] Started, waiting for encoded pages...");

        while let Some(msg) = rx.recv().await {
            match msg {
                DjvuWriterMessage::AppendEncoded {
                    encoded,
                    page_index,
                } => {
                    page_buffer.insert(page_index, encoded);
                    for encoded in drain_ready_pages(&mut page_buffer, &mut next_expected) {
                        writer_doc.add_encoded_page(encoded).with_context(|| {
                            format!(
                                "Failed to append DjVu page {}",
                                next_expected.saturating_sub(1)
                            )
                        })?;
                        pages_written += 1;
                        progress_tracker.update(crate::progress::ProcessingStatus::PdfAppend {
                            current: pages_written,
                            total: total_pages,
                        });
                    }
                }
                DjvuWriterMessage::SetOutline(items) => {
                    navigation = outline_to_navigation(items, total_pages)?;
                }
                DjvuWriterMessage::Finalize => {
                    if pages_written != total_pages {
                        return Err(anyhow!(
                            "Document incomplete: {} of {} pages encoded",
                            pages_written,
                            total_pages
                        ));
                    }
                    let writer_doc = Arc::clone(&writer_doc);
                    let navigation = navigation.clone();
                    let output_path = output_path.clone();
                    let cancel = cancel.clone();
                    crate::runtime_stats::spawn_blocking_stage(
                        crate::runtime_stats::Stage::Writer,
                        move || {
                            finalize_document_atomic(
                                &writer_doc,
                                &navigation,
                                &output_path,
                                &cancel,
                            )
                        },
                    )
                    .await
                    .map_err(|e| anyhow!("DjVu finalizer task panicked: {}", e))??;

                    progress_tracker.update(crate::progress::ProcessingStatus::PdfAppend {
                        current: total_pages,
                        total: total_pages,
                    });

                    crate::success_log!("[DjvuWriterActor] Document finalized successfully");
                    break;
                }
            }
        }

        Ok(())
    });

    (handle, doc, task)
}

fn finalize_document_atomic(
    doc: &DjvuDocument,
    navigation: &DjVmNav,
    output_path: &Path,
    cancel: &lege_pdf_read::CancellationToken,
) -> Result<()> {
    if cancel.is_cancelled() || crate::progress::termination_requested() {
        return Err(anyhow!(
            "DjVu finalization cancelled before output publication"
        ));
    }

    let nav = (!navigation.bookmarks.is_empty()).then_some(navigation);
    let bytes = doc
        .finalize_with_navigation(nav)
        .context("Failed to finalize DjVu document")?;

    let output_parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(output_parent).with_context(|| {
        format!(
            "Failed to create DjVu output directory {}",
            output_parent.display()
        )
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(output_parent)
        .context("Failed to create temporary DjVu output")?;
    temporary
        .write_all(&bytes)
        .context("Failed to write temporary DjVu output")?;

    if cancel.is_cancelled() || crate::progress::termination_requested() {
        return Err(anyhow!(
            "DjVu finalization cancelled before output publication"
        ));
    }

    temporary.persist(output_path).map_err(|error| {
        anyhow!(
            "Failed to publish DjVu output {}: {}",
            output_path.display(),
            error.error
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use djvu_encoder::doc::{DjvuBuilder, PageBuilder, PageEncodeParams};
    use djvu_encoder::image::image_formats::{Pixel, Pixmap};
    use image::{Rgb, RgbImage};
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    use super::{
        DjVmNav, DjvuConfig, DjvuOrchestrator, DjvuWriterHandle, DjvuWriterMessage, PageData,
        drain_ready_pages, finalize_document_atomic,
    };

    fn contains_chunk(bytes: &[u8], chunk: &[u8; 4]) -> bool {
        bytes.windows(4).any(|window| window == chunk)
    }

    fn one_page_document() -> djvu_encoder::doc::DjvuDocument {
        let doc = DjvuBuilder::new(1).with_dpi(300).build();
        let page = PageBuilder::new(0, 8, 8)
            .with_background(Pixmap::from_pixel(8, 8, Pixel::white()))
            .expect("background")
            .build()
            .expect("page");
        doc.add_page(page).expect("encode page");
        doc
    }

    #[test]
    fn out_of_order_djvu_pages_drain_in_order() {
        let mut buffer = std::collections::BTreeMap::new();
        let mut next_expected = 0usize;
        buffer.insert(2usize, "two");
        buffer.insert(0usize, "zero");
        buffer.insert(1usize, "one");

        let ready = drain_ready_pages(&mut buffer, &mut next_expected);

        assert_eq!(ready, vec!["zero", "one", "two"]);
        assert!(buffer.is_empty());
        assert_eq!(next_expected, 3);
    }

    #[test]
    fn typed_pages_preserve_cover_mrc_mask_and_hidden_text() {
        let work_dir = tempfile::tempdir().expect("tempdir");
        let orchestrator = DjvuOrchestrator::new(DjvuConfig {
            work_dir: work_dir.path().to_path_buf(),
            ..Default::default()
        })
        .expect("orchestrator");

        let cover = orchestrator
            .process_page(PageData {
                index: 0,
                preserve_full_color: true,
                rgb_image: RgbImage::from_pixel(8, 8, Rgb([20, 80, 140])),
                binarized: vec![255; 64],
                cleaned_gray: None,
                detections: Vec::new(),
                hocr: None,
            })
            .expect("cover");
        assert_eq!(cover.bg_subsample, Some(1));

        let mut mask = vec![255; 64];
        mask[9] = 0;
        mask[10] = 0;
        let mrc = orchestrator
            .process_page(PageData {
                index: 1,
                preserve_full_color: false,
                rgb_image: RgbImage::from_pixel(8, 8, Rgb([180, 180, 180])),
                binarized: mask,
                cleaned_gray: Some(vec![190; 64]),
                detections: Vec::new(),
                hocr: Some(
                    "<span class='ocrx_word' title='bbox 1 1 6 6'>caf&amp;eacute;</span>"
                        .to_string(),
                ),
            })
            .expect("MRC page");
        assert_eq!(mrc.bg_subsample, None);

        let mut params = PageEncodeParams::default();
        params.bg_subsample = 3;
        let doc = Arc::new(
            DjvuBuilder::new(2)
                .with_dpi(300)
                .with_params(params)
                .build(),
        );
        let encoded_cover = cover.encode(&doc, 75, 3).expect("encode cover");
        let encoded_mrc = mrc.encode(&doc, 75, 3).expect("encode MRC");

        // The document collection accepts out-of-order completion while final
        // assembly remains page-index ordered.
        doc.add_encoded_page(encoded_mrc).expect("add MRC");
        doc.add_encoded_page(encoded_cover).expect("add cover");
        let bytes = doc.finalize().expect("finalize");

        assert!(bytes.starts_with(b"AT&TFORM"));
        assert!(contains_chunk(&bytes, b"BG44"));
        assert!(contains_chunk(&bytes, b"Sjbz"));
        assert!(contains_chunk(&bytes, b"TXTz"));
    }

    #[test]
    fn cancellation_before_publication_preserves_existing_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("existing.djvu");
        std::fs::write(&output, b"old output").expect("seed output");
        let cancel = lege_pdf_read::CancellationToken::new();
        cancel.cancel();

        let error =
            finalize_document_atomic(&one_page_document(), &DjVmNav::default(), &output, &cancel)
                .expect_err("cancelled finalization");

        assert!(error.to_string().contains("cancelled"));
        assert_eq!(std::fs::read(&output).expect("output"), b"old output");
    }

    #[test]
    fn atomic_publication_writes_a_complete_document() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("published.djvu");
        finalize_document_atomic(
            &one_page_document(),
            &DjVmNav::default(),
            &output,
            &lege_pdf_read::CancellationToken::new(),
        )
        .expect("publish");

        let bytes = std::fs::read(output).expect("output");
        assert!(bytes.starts_with(b"AT&TFORM"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bounded_djvu_writer_handle_applies_backpressure() {
        let (tx, mut rx) = mpsc::channel::<DjvuWriterMessage>(1);
        let handle = DjvuWriterHandle { sender: tx };

        handle.finalize().await.expect("first send");

        let second_send = tokio::spawn({
            let handle = handle;
            async move { handle.finalize().await }
        });
        let mut second_send = Box::pin(second_send);

        assert!(
            timeout(Duration::from_millis(50), &mut second_send)
                .await
                .is_err()
        );

        let _ = rx.recv().await.expect("message");

        let completed = timeout(Duration::from_millis(200), &mut second_send)
            .await
            .expect("send should finish")
            .expect("join");
        completed.expect("send ok");
    }
}
