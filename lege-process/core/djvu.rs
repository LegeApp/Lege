// djvu.rs
// DjVu output driver.
//
// Lege does not link a DjVu encoder library. It composes each page's layers
// (bilevel ink mask, IW44 background canvas, OCR word boxes), writes them to the
// work directory as ordinary interchange files, and hands a neutral JSON
// manifest to a separate `djvu-encoder` program invoked over the command line.
// This keeps the AGPL encoder at arms length: no FFI, no shared memory, no
// callbacks, and the encoder is user-replaceable via `LEGE_DJVU_ENCODER` or the
// `--djvu-encoder-path` flag.

use anyhow::{Context, Result, anyhow};
use image::{GrayImage, RgbImage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use unicode_normalization::UnicodeNormalization;

use crate::app_dirs;
use crate::dbglog;
use crate::engine::Detection;

/// Manifest schema version understood by this driver. Must match the encoder.
const MANIFEST_SCHEMA_VERSION: u32 = 2;

/// Basename of the encoder executable searched for on `PATH` and next to Lege.
const ENCODER_BIN: &str = "djvu-encoder";

/// Process-wide encoder path override, set once from `--djvu-encoder-path`.
static ENCODER_PATH_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Record the encoder path supplied on the command line (`--djvu-encoder-path`).
/// First value wins; call once during startup before any DJVU job runs.
pub fn set_encoder_path_override(path: PathBuf) {
    let _ = ENCODER_PATH_OVERRIDE.set(path);
}

fn djvu_hidden_text_enabled() -> bool {
    std::env::var("LEGE_DJVU_HIDDEN_TEXT").ok().as_deref() != Some("0")
}

/// Locate the standalone `djvu-encoder` program.
///
/// Search order: explicit path (config/flag) → `LEGE_DJVU_ENCODER` → the macOS
/// app's `Contents/Helpers` directory → next to the running Lege executable →
/// `PATH`. Returns a clear, actionable error if none is found so the DJVU output
/// mode fails fast (PDF output is unaffected).
pub fn resolve_encoder_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        return Err(anyhow!(
            "configured DjVu encoder path does not exist: {}",
            path.display()
        ));
    }

    if let Some(path) = ENCODER_PATH_OVERRIDE.get() {
        if path.exists() {
            return Ok(path.clone());
        }
        return Err(anyhow!(
            "--djvu-encoder-path points to a missing file: {}",
            path.display()
        ));
    }

    if let Some(env_path) = std::env::var_os("LEGE_DJVU_ENCODER") {
        let path = PathBuf::from(env_path);
        if path.exists() {
            return Ok(path);
        }
        return Err(anyhow!(
            "LEGE_DJVU_ENCODER points to a missing file: {}",
            path.display()
        ));
    }

    if let Ok(exe) = std::env::current_exe() {
        for candidate in bundled_encoder_candidates(&exe) {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    if let Some(found) = search_path(ENCODER_BIN) {
        return Ok(found);
    }

    Err(anyhow!(
        "{ENCODER_BIN} wasn't found. Install the AGPL DjVu encoder \
         package, put it next to Lege or on your PATH, or point Lege at it with \
         --djvu-encoder-path or the LEGE_DJVU_ENCODER environment variable. \
         (PDF output does not require it.)"
    ))
}

/// Candidate paths relative to the running executable. A macOS application
/// keeps helper tools in `Contents/Helpers`; loose CLI distributions retain the
/// long-standing next-to-executable lookup.
fn bundled_encoder_candidates(exe: &Path) -> Vec<PathBuf> {
    let Some(exe_dir) = exe.parent() else {
        return Vec::new();
    };

    let mut dirs = Vec::with_capacity(2);
    if exe_dir.file_name().and_then(|name| name.to_str()) == Some("MacOS") {
        if let Some(contents_dir) = exe_dir.parent() {
            dirs.push(contents_dir.join("Helpers"));
        }
    }
    dirs.push(exe_dir.to_path_buf());

    dirs.into_iter()
        .flat_map(|dir| {
            [ENCODER_BIN, "djvu-encoder.exe"]
                .into_iter()
                .map(move |name| dir.join(name))
        })
        .collect()
}

/// Minimal `PATH` search for an executable basename.
fn search_path(bin: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in [bin, "djvu-encoder.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Diagnostic string describing the DjVu encoder that will be used.
pub fn active_backend_info() -> String {
    match resolve_encoder_path(None) {
        Ok(path) => format!("subprocess {}", path.display()),
        Err(_) => format!("subprocess '{ENCODER_BIN}' (not yet resolved)"),
    }
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
    /// Working directory for page-layer files and the manifest.
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
    /// Explicit path to the `djvu-encoder` program (from `--djvu-encoder-path`).
    /// `None` falls back to env / next-to-exe / PATH resolution.
    #[serde(default)]
    pub encoder_path: Option<PathBuf>,
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
            encoder_path: None,
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

// ============================================================================
// Manifest schema (neutral interchange written for the encoder subprocess)
// ============================================================================

/// One page's contribution to the manifest: paths (relative to the manifest
/// directory) of the layer files this driver wrote, plus dimensions. A page
/// with neither `mask` nor `background` is a blank page (encoder fills white).
#[derive(Debug, Clone, Serialize)]
pub struct ManifestPageEntry {
    index: usize,
    width: u32,
    height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    mask: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    background: Option<String>,
    /// Per-page IW44 background subsampling override. Full-page covers use 1
    /// so the document-wide ×3 body-background policy cannot blur them.
    #[serde(skip_serializing_if = "Option::is_none")]
    bg_subsample: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ocr: Option<String>,
}

#[derive(Debug, Serialize)]
struct Manifest {
    version: u32,
    dpi: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    slices: Option<usize>,
    bg_subsample: u8,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    outline: Vec<ManifestOutlineEntry>,
    pages: Vec<ManifestPageEntry>,
}

#[derive(Debug, Serialize)]
struct ManifestOutlineEntry {
    title: String,
    page_index: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<ManifestOutlineEntry>,
}

impl From<lege_pdf_write::outline::OutlineItem> for ManifestOutlineEntry {
    fn from(item: lege_pdf_write::outline::OutlineItem) -> Self {
        Self {
            title: item.title,
            page_index: item.page_index as usize,
            children: item.children.into_iter().map(Self::from).collect(),
        }
    }
}

/// A single OCR word box in page pixel coordinates (neutral interchange form).
#[derive(Serialize)]
struct OcrWord {
    text: String,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}

#[derive(Serialize)]
struct OcrDoc {
    words: Vec<OcrWord>,
}

/// Progress line emitted by the encoder's `--progress-json` output.
#[derive(Deserialize)]
struct ProgressLine {
    event: String,
    #[serde(default)]
    page: Option<usize>,
    #[serde(default)]
    of: Option<usize>,
}

// ============================================================================
// Orchestrator: composes page layers and writes them to the work directory
// ============================================================================

/// Composes DjVu page layers and writes them as interchange files.
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

    /// Directory where page-layer files and the manifest are written.
    pub fn work_dir(&self) -> &Path {
        &self.config.work_dir
    }

    /// Compose a single page's layers, write them to the work directory, and
    /// return the manifest entry describing them. Thread-safe: each page writes
    /// its own uniquely-named files.
    pub fn process_page(&self, page_data: PageData) -> Result<ManifestPageEntry> {
        let page_index = page_data.index;

        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] process_page START: page {}", page_index);

        let result = self.compose_page_entry(page_data);

        #[cfg(feature = "debug-logging")]
        match &result {
            Ok(_) => crate::success_log!("[DJVU] process_page COMPLETE: page {}", page_index),
            Err(e) => crate::error_log!("[DJVU] process_page FAILED: page {} - {}", page_index, e),
        }
        let _ = page_index;

        result
    }

    fn compose_page_entry(&self, page_data: PageData) -> Result<ManifestPageEntry> {
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
    ) -> Result<ManifestPageEntry> {
        let background = self.write_background(page_num, &page_data.rgb_image)?;
        let ocr = self.write_ocr(page_num, page_data, width, height)?;
        Ok(ManifestPageEntry {
            index: page_num,
            width,
            height,
            mask: None,
            background: Some(background),
            bg_subsample: page_data.preserve_full_color.then_some(1),
            ocr,
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
    ) -> Result<ManifestPageEntry> {
        self.maybe_dump_preencode_pbm(page_num, &page_data.binarized, width, height)?;

        // Blank page with no figures: no layer files; encoder synthesizes white.
        if self.is_binarized_blank(&page_data.binarized, width, height) && image_regions.is_empty()
        {
            return Ok(ManifestPageEntry {
                index: page_num,
                width,
                height,
                mask: None,
                background: None,
                bg_subsample: None,
                ocr: None,
            });
        }

        // 1. Prepare the bilevel ink mask (raw Luma8 buffer).
        let mut mask = self.mask_from_binarized(&page_data.binarized, width, height)?;

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
            Some(self.write_background(page_num, &canvas)?)
        } else if use_color_background && !image_regions.is_empty() {
            let canvas = self.compose_color_canvas(page_data, image_regions, width, height);
            dbglog!("[djvu] IW44 color background composed");
            Some(self.write_background(page_num, &canvas)?)
        } else {
            None
        };

        let mask_name = self.write_mask(page_num, &mask, width, height)?;
        let ocr = self.write_ocr(page_num, page_data, width, height)?;

        Ok(ManifestPageEntry {
            index: page_num,
            width,
            height,
            mask: Some(mask_name),
            background,
            bg_subsample: None,
            ocr,
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

    /// Build the bilevel ink mask as a raw Luma8 buffer: ink → 0 (black),
    /// else 255. The encoder thresholds at <128, so this round-trips bit-exactly.
    fn mask_from_binarized(&self, binarized: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
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
            buf.push(if is_black { 0u8 } else { 255u8 });
        }
        Ok(buf)
    }

    /// White out image regions in the ink mask (set to 255/background) so JB2
    /// text does not bleed through color/photo regions.
    fn whiteout_image_regions(
        &self,
        mask: &mut [u8],
        regions: &[ImageRegion],
        width: u32,
        height: u32,
    ) {
        let w = width as usize;
        for region in regions {
            let x1 = region.bbox[0].max(0.0).floor() as usize;
            let y1 = region.bbox[1].max(0.0).floor() as usize;
            let x2 = (region.bbox[2].ceil() as usize).min(width as usize);
            let y2 = (region.bbox[3].ceil() as usize).min(height as usize);
            for row in y1..y2 {
                let start = row * w + x1;
                let end = row * w + x2;
                if let Some(slice) = mask.get_mut(start..end) {
                    slice.fill(255);
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

    // --- File writers ------------------------------------------------------

    fn write_mask(&self, page_num: usize, mask: &[u8], width: u32, height: u32) -> Result<String> {
        let img: GrayImage = GrayImage::from_raw(width, height, mask.to_vec())
            .ok_or_else(|| anyhow!("Failed to build mask image buffer"))?;
        let name = format!("page-{:05}-mask.png", page_num);
        let path = self.config.work_dir.join(&name);
        img.save(&path)
            .with_context(|| format!("Failed to write mask {:?}", path))?;
        Ok(name)
    }

    fn write_background(&self, page_num: usize, bg: &RgbImage) -> Result<String> {
        let name = format!("page-{:05}-bg.png", page_num);
        let path = self.config.work_dir.join(&name);
        bg.save(&path)
            .with_context(|| format!("Failed to write background {:?}", path))?;
        Ok(name)
    }

    fn write_ocr(
        &self,
        page_num: usize,
        page_data: &PageData,
        width: u32,
        height: u32,
    ) -> Result<Option<String>> {
        if !djvu_hidden_text_enabled() {
            return Ok(None);
        }
        let Some(hocr) = page_data.hocr.as_deref() else {
            return Ok(None);
        };
        let words = self.parse_hocr_to_words(hocr, width, height)?;
        if words.is_empty() {
            return Ok(None);
        }

        let doc = OcrDoc {
            words: words
                .into_iter()
                .map(|(text, x, y, w, h)| OcrWord { text, x, y, w, h })
                .collect(),
        };
        let name = format!("page-{:05}-ocr.json", page_num);
        let path = self.config.work_dir.join(&name);
        let json = serde_json::to_vec(&doc).context("Failed to serialize OCR words")?;
        fs::write(&path, json).with_context(|| format!("Failed to write OCR {:?}", path))?;
        dbglog!("[djvu] OCR text layer written for page {}", page_num);
        Ok(Some(name))
    }

    /// Parse HOCR into (text, x, y, width, height) word boxes in page coords.
    fn parse_hocr_to_words(
        &self,
        hocr: &str,
        page_width: u32,
        page_height: u32,
    ) -> Result<Vec<(String, u16, u16, u16, u16)>> {
        use once_cell::sync::Lazy;
        use regex::Regex;

        static WORD_RE: Lazy<Regex> = Lazy::new(|| {
            Regex::new(
                r#"(?is)<span[^>]*class=['"][^'"]*\bocrx_word\b[^'"]*['"][^>]*title=['"]([^'"]*)['"][^>]*>(.*?)</span>"#,
            )
            .expect("valid ocrx_word regex")
        });
        static BBOX_RE: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r#"(?i)\bbbox\s+(-?\d+)\s+(-?\d+)\s+(-?\d+)\s+(-?\d+)"#)
                .expect("valid bbox regex")
        });
        static TAG_RE: Lazy<Regex> =
            Lazy::new(|| Regex::new(r#"(?is)<[^>]+>"#).expect("valid tag regex"));
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

    /// Preflight: verify the work directory is writable and the encoder program
    /// can be located, so DJVU jobs fail fast with a clear message.
    pub fn preflight_check(&self, _require_text_layer: bool) -> Result<()> {
        fs::create_dir_all(&self.config.work_dir)
            .with_context(|| format!("Work directory not writable: {:?}", self.config.work_dir))?;
        resolve_encoder_path(self.config.encoder_path.as_deref())?;
        Ok(())
    }

    /// Clean up the manifest and page-layer files written for this job.
    ///
    /// A job directory below the managed DjVu temp base is removed whole. Any
    /// other work directory — one the caller chose — keeps its directory and
    /// loses only the interchange files this job wrote, so a cleanup can never
    /// delete a location the user owns.
    pub fn cleanup(&self) -> Result<()> {
        let work_dir = self.config.work_dir.as_path();
        dbglog!("[djvu] cleanup() removing {:?}", work_dir);
        if !work_dir.exists() {
            return Ok(());
        }

        let base = app_dirs::djvu_base_dir();
        if work_dir != base && work_dir.starts_with(&base) {
            return fs::remove_dir_all(work_dir)
                .or_else(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
                .with_context(|| format!("Failed to remove DjVu work directory: {:?}", work_dir));
        }

        let entries = fs::read_dir(work_dir)
            .with_context(|| format!("Failed to read DjVu work directory: {:?}", work_dir))?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let is_job_file = name == "manifest.json"
                || (name.starts_with("page-")
                    && (name.ends_with("-mask.png")
                        || name.ends_with("-bg.png")
                        || name.ends_with("-ocr.json")));
            if is_job_file {
                let _ = fs::remove_file(entry.path());
            }
        }
        let _ = fs::remove_dir_all(work_dir.join("preencode-pbm"));
        Ok(())
    }

    pub fn cleanup_work_dir_only(&self) -> Result<()> {
        self.cleanup()
    }
}

/// Job-scoped control channel to the `djvu-encoder` child process.
///
/// The child runs inside the writer actor's blocking task, which a broadcast
/// cancel cannot interrupt. The pipeline holds this control, sets `cancelled`
/// when the job unwinds, and waits for the encoder loop to kill and reap the
/// child. Without it a cancelled DjVu job leaves an orphan encoder.
#[derive(Default)]
pub struct DjvuEncoderControl {
    cancelled: std::sync::atomic::AtomicBool,
    running: std::sync::atomic::AtomicBool,
}

impl DjvuEncoderControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
            || crate::progress::termination_requested()
    }

    fn mark_running(&self, running: bool) {
        self.running
            .store(running, std::sync::atomic::Ordering::Release);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Block until the encoder child is reaped, or the timeout expires.
    /// Returns true when no encoder is running any more.
    pub fn wait_until_stopped(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.is_running() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }
}

/// Ends a DjVu job cleanly when the job ends, whatever the reason — success,
/// failure, or cancellation. It stops the encoder child and then removes the
/// work directory. Phase 8 requires that a cancelled job leave neither behind.
pub struct DjvuWorkDirGuard {
    orchestrator: std::sync::Arc<DjvuOrchestrator>,
    encoder: std::sync::Arc<DjvuEncoderControl>,
}

impl DjvuWorkDirGuard {
    pub fn new(
        orchestrator: std::sync::Arc<DjvuOrchestrator>,
        encoder: std::sync::Arc<DjvuEncoderControl>,
    ) -> Self {
        Self {
            orchestrator,
            encoder,
        }
    }
}

impl Drop for DjvuWorkDirGuard {
    fn drop(&mut self) {
        if self.encoder.is_running() {
            self.encoder.cancel();
            if !self.encoder.wait_until_stopped(Duration::from_secs(5)) {
                crate::warn_log!("[djvu] Encoder did not stop before work-directory cleanup");
            }
        }
        if let Err(_error) = self.orchestrator.cleanup() {
            crate::warn_log!("[djvu] Failed to clean up work directory: {}", _error);
        }
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

//==============================================================================
// DjVu Writer Actor - collects manifest entries, then runs the encoder
//==============================================================================

/// Message types for the DjVu writer actor.
pub enum DjvuWriterMessage {
    /// Record a composed page's manifest entry.
    AppendEntry(ManifestPageEntry),
    /// Set the accepted document navigation tree before finalization.
    SetOutline(Vec<lege_pdf_write::outline::OutlineItem>),
    /// Write the manifest and run the encoder subprocess.
    Finalize,
}

/// Handle for sending composed page entries to the DjVu writer actor.
#[derive(Clone)]
pub struct DjvuWriterHandle {
    sender: mpsc::Sender<DjvuWriterMessage>,
}

impl DjvuWriterHandle {
    /// Record a composed page (its layer files are already on disk).
    pub async fn append_entry(&self, entry: ManifestPageEntry) -> Result<(), anyhow::Error> {
        self.sender
            .send(DjvuWriterMessage::AppendEntry(entry))
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

    /// Finalize the document (write manifest, run encoder).
    pub async fn finalize(&self) -> Result<(), anyhow::Error> {
        self.sender
            .send(DjvuWriterMessage::Finalize)
            .await
            .map_err(|_| anyhow!("DjVu writer actor has stopped"))?;
        Ok(())
    }
}

/// Map IW44 quality (0-100) to a slice count (kept from the previous encoder).
fn quality_to_slices(iw44_quality: u8) -> usize {
    match iw44_quality {
        100 => 97,
        q if q >= 50 => 74 + ((q as usize - 50) * 23 / 50),
        q => 50 + (q as usize * 24 / 50),
    }
}

/// Spawn the DjVu writer actor. It buffers per-page manifest entries and, on
/// `Finalize`, writes the manifest and invokes the `djvu-encoder` subprocess,
/// streaming its progress into `progress_tracker` and surfacing its stderr
/// verbatim on failure.
#[allow(clippy::too_many_arguments)]
pub fn spawn_djvu_writer_actor(
    output_path: PathBuf,
    total_pages: usize,
    dpi: u32,
    iw44_quality: u8,
    bg_subsample: usize,
    work_dir: PathBuf,
    encoder_path: PathBuf,
    progress_tracker: crate::progress::ProgressTracker,
    channel_capacity: usize,
    encoder_control: std::sync::Arc<DjvuEncoderControl>,
) -> (
    DjvuWriterHandle,
    tokio::task::JoinHandle<Result<(), anyhow::Error>>,
) {
    let (tx, mut rx) = mpsc::channel::<DjvuWriterMessage>(channel_capacity.max(1));
    let handle = DjvuWriterHandle { sender: tx };

    let slices = quality_to_slices(iw44_quality);
    let bg_subsample = bg_subsample.clamp(1, 12) as u8;

    let task = tokio::spawn(async move {
        let mut entries: BTreeMap<usize, ManifestPageEntry> = BTreeMap::new();
        let mut outline = Vec::new();

        crate::info_log!("[DjvuWriterActor] Started, collecting page entries...");

        while let Some(msg) = rx.recv().await {
            match msg {
                DjvuWriterMessage::AppendEntry(entry) => {
                    entries.insert(entry.index, entry);
                }
                DjvuWriterMessage::SetOutline(items) => {
                    outline = items.into_iter().map(ManifestOutlineEntry::from).collect();
                }
                DjvuWriterMessage::Finalize => {
                    if entries.len() != total_pages {
                        return Err(anyhow!(
                            "Document incomplete: {} of {} pages composed",
                            entries.len(),
                            total_pages
                        ));
                    }

                    let manifest = Manifest {
                        version: MANIFEST_SCHEMA_VERSION,
                        dpi,
                        slices: Some(slices),
                        bg_subsample,
                        outline,
                        pages: entries.into_values().collect(),
                    };

                    let manifest_path = work_dir.join("manifest.json");
                    let manifest_json = serde_json::to_vec_pretty(&manifest)
                        .context("Failed to serialize DjVu manifest")?;
                    fs::write(&manifest_path, manifest_json).with_context(|| {
                        format!("Failed to write DjVu manifest: {:?}", manifest_path)
                    })?;

                    // Run the encoder off the async runtime; it streams progress
                    // back through the tracker.
                    let progress = progress_tracker.clone();
                    let encoder_path = encoder_path.clone();
                    let output_path = output_path.clone();
                    let control = encoder_control.clone();
                    crate::runtime_stats::spawn_blocking_stage(
                        crate::runtime_stats::Stage::Writer,
                        move || {
                            run_encoder(
                                &encoder_path,
                                &manifest_path,
                                &output_path,
                                total_pages,
                                &progress,
                                &control,
                            )
                        },
                    )
                    .await
                    .map_err(|e| anyhow!("DjVu encoder task panicked: {}", e))??;

                    crate::success_log!("[DjvuWriterActor] Document finalized successfully");
                    break;
                }
            }
        }

        Ok(())
    });

    (handle, task)
}

/// Run the `djvu-encoder` subprocess, streaming NDJSON progress into the
/// tracker and returning its stderr verbatim on failure.
/// Clears the encoder's running flag when `run_encoder` returns, so a waiting
/// [`DjvuWorkDirGuard`] can proceed with cleanup.
struct RunningGuard<'a>(&'a DjvuEncoderControl);

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.0.mark_running(false);
    }
}

fn run_encoder(
    encoder_path: &Path,
    manifest_path: &Path,
    output_path: &Path,
    total_pages: usize,
    progress: &crate::progress::ProgressTracker,
    control: &DjvuEncoderControl,
) -> Result<()> {
    let output_parent = output_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(output_parent)?;
    let temporary_output = tempfile::NamedTempFile::new_in(output_parent)
        .context("Failed to create temporary DjVu output")?
        .into_temp_path();
    let mut child = Command::new(encoder_path)
        .arg("encode-document")
        .arg("--manifest")
        .arg(manifest_path)
        .arg("--output")
        .arg(temporary_output.as_os_str())
        .arg("--progress-json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to launch DjVu encoder: {}", encoder_path.display()))?;
    control.mark_running(true);
    // Report the child as stopped on every exit path of this function.
    let _running = RunningGuard(control);

    // Drain stderr on a separate thread to avoid a full-pipe deadlock.
    let stderr = child.stderr.take();
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_string(&mut buf);
        }
        buf
    });

    let stdout = child.stdout.take();
    let progress_clone = progress.clone();
    let stdout_thread = std::thread::spawn(move || {
        if let Some(stdout) = stdout {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(evt) = serde_json::from_str::<ProgressLine>(&line)
                    && evt.event == "page_done"
                    && let (Some(current), Some(total)) = (evt.page, evt.of)
                {
                    progress_clone
                        .update(crate::progress::ProcessingStatus::PdfAppend { current, total });
                }
            }
        }
    });

    let timeout = std::env::var("LEGE_DJVU_ENCODER_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|&seconds| seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(30 * 60));
    let started = Instant::now();
    let status = loop {
        // On a kill path the drain threads are left detached on purpose: a
        // grandchild of the encoder can hold the pipe open, and joining would
        // then make the cancel latency unbounded.
        if control.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("djvu-encoder cancelled by shutdown request"));
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(
                "djvu-encoder timed out after {} seconds",
                timeout.as_secs()
            ));
        }
        match child
            .try_wait()
            .context("Failed to poll DjVu encoder process")?
        {
            Some(status) => break status,
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    };
    let _ = stdout_thread.join();
    let stderr_str = stderr_thread.join().unwrap_or_default();

    if !status.success() {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        return Err(anyhow!(
            "djvu-encoder failed (exit {code}): {}",
            stderr_str.trim()
        ));
    }
    if control.is_cancelled() {
        return Err(anyhow!("djvu-encoder cancelled before output publish"));
    }
    temporary_output.persist(output_path).map_err(|error| {
        anyhow!(
            "Failed to publish DjVu output {}: {}",
            output_path.display(),
            error.error
        )
    })?;

    // Ensure the writer-side progress reaches 100% even if the encoder batched.
    progress.update(crate::progress::ProcessingStatus::PdfAppend {
        current: total_pages,
        total: total_pages,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    use super::{
        DjvuConfig, DjvuEncoderControl, DjvuOrchestrator, DjvuWorkDirGuard, DjvuWriterHandle,
        DjvuWriterMessage, bundled_encoder_candidates,
    };

    /// A job directory inside the managed DjVu temp base, holding one page's
    /// interchange files and a manifest.
    fn populated_job_dir(name: &str) -> PathBuf {
        let dir = crate::app_dirs::djvu_work_dir_for(Some(Path::new(name)));
        std::fs::create_dir_all(&dir).expect("work dir");
        std::fs::write(dir.join("manifest.json"), b"{}").expect("manifest");
        std::fs::write(dir.join("page-00000-mask.png"), b"x").expect("mask");
        std::fs::write(dir.join("page-00000-bg.png"), b"x").expect("bg");
        dir
    }

    fn orchestrator_for(work_dir: PathBuf) -> std::sync::Arc<DjvuOrchestrator> {
        std::sync::Arc::new(
            DjvuOrchestrator::new(DjvuConfig {
                work_dir,
                ..Default::default()
            })
            .expect("orchestrator"),
        )
    }

    #[test]
    fn cleanup_removes_a_managed_job_directory() {
        let dir = populated_job_dir("cleanup_removes_managed_dir.djvu");
        orchestrator_for(dir.clone()).cleanup().expect("cleanup");
        assert!(!dir.exists(), "job directory {dir:?} was left behind");
    }

    #[test]
    fn cleanup_keeps_an_unmanaged_directory_but_removes_the_job_files() {
        let dir = std::env::temp_dir().join("lege_djvu_unmanaged_cleanup_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("manifest.json"), b"{}").expect("manifest");
        std::fs::write(dir.join("page-00000-mask.png"), b"x").expect("mask");
        std::fs::write(dir.join("keep-me.txt"), b"user file").expect("user file");

        orchestrator_for(dir.clone()).cleanup().expect("cleanup");

        assert!(dir.exists());
        assert!(!dir.join("manifest.json").exists());
        assert!(!dir.join("page-00000-mask.png").exists());
        assert!(dir.join("keep-me.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn work_dir_guard_cleans_up_when_a_job_unwinds() {
        let dir = populated_job_dir("guard_cleans_up_on_unwind.djvu");
        {
            let _guard = DjvuWorkDirGuard::new(
                orchestrator_for(dir.clone()),
                std::sync::Arc::new(DjvuEncoderControl::new()),
            );
            assert!(dir.exists());
        }
        assert!(!dir.exists(), "cancelled job left {dir:?} behind");
    }

    #[test]
    fn macos_bundle_prefers_helpers_then_executable_directory() {
        let candidates =
            bundled_encoder_candidates(Path::new("/Applications/Lege.app/Contents/MacOS/lege"));
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/Applications/Lege.app/Contents/Helpers/djvu-encoder"),
                PathBuf::from("/Applications/Lege.app/Contents/Helpers/djvu-encoder.exe"),
                PathBuf::from("/Applications/Lege.app/Contents/MacOS/djvu-encoder"),
                PathBuf::from("/Applications/Lege.app/Contents/MacOS/djvu-encoder.exe"),
            ]
        );
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
