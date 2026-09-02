// helper_functions.rs - Shared helper functions for the pipeline
use anyhow::{Result, anyhow};
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::encoding::Jbig2Mode;
use crate::engine::Detection;
#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    feature = "tesseract-ocr",
    not(feature = "paddle-ocr")
))]
use crate::ocr::check_tesseract_availability;
use crate::pagerender::NativeTextWord;
use crate::pipeline::config::PipelineConfig;
use crate::types::CoverFormat;
use crate::{info_log, warn_log};

// Memory monitoring and limits
// Increased from 4GB to 12GB to support layout detection with multiple workers

static ENCODE_SEMAPHORE: std::sync::OnceLock<std::sync::Mutex<std::sync::Arc<Semaphore>>> =
    std::sync::OnceLock::new();

pub fn init_encode_semaphore(permits: usize) {
    let semaphore = ENCODE_SEMAPHORE
        .get_or_init(|| std::sync::Mutex::new(std::sync::Arc::new(Semaphore::new(1))));
    if let Ok(mut guard) = semaphore.lock() {
        *guard = std::sync::Arc::new(Semaphore::new(permits.max(1)));
    }
}

pub fn get_encode_semaphore() -> Option<std::sync::Arc<Semaphore>> {
    ENCODE_SEMAPHORE
        .get()
        .and_then(|semaphore| semaphore.lock().ok().map(|guard| guard.clone()))
}

/// Build the shared layout-inference session pool, with the same hardware-GPU
/// fallback behavior for every output pipeline.
///
/// PDF and DjVu previously carried near-identical initialization/error blocks,
/// which had already drifted in log prefixes. Keeping this at the shared
/// pipeline seam ensures any future adapter policy or warning text applies to
/// both formats.
pub(crate) fn initialize_inference_or_fallback(
    mut config: Arc<PipelineConfig>,
    progress: &crate::progress::ProgressTracker,
    pipeline_label: &str,
) -> Result<(
    Arc<PipelineConfig>,
    Option<Arc<crate::pipeline::inference::InferenceHandle>>,
)> {
    if !config.enable_layout_detection() {
        return Ok((config, None));
    }

    match crate::pipeline::inference::InferenceHandle::new(&config) {
        Ok(handle) => Ok((config, Some(Arc::new(handle)))),
        Err(error)
            if crate::pipeline::inference::is_layout_software_adapter_error(error.as_ref()) =>
        {
            let message = "No usable hardware GPU found — wgpu fell back to a CPU/software \
                           adapter. Layout detection has been disabled for this run. Install or \
                           update your GPU driver to enable hardware acceleration."
                .to_string();
            warn_log!("[{}] {}", pipeline_label, message);
            progress.update(crate::progress::ProcessingStatus::PipelineMessage {
                stage: "GPU Warning".to_string(),
                message,
            });
            Arc::make_mut(&mut config).set_enable_layout_detection(false);
            Ok((config, None))
        }
        Err(error) if crate::pipeline::inference::is_gpu_device_error(error.as_ref()) => {
            let message = format!(
                "GPU initialization failed ({}). Layout detection disabled; processing will \
                 continue without it. Check that your GPU driver supports DX12 (Windows) or \
                 Vulkan (Linux/macOS).",
                error
            );
            warn_log!("[{}] {}", pipeline_label, message);
            progress.update(crate::progress::ProcessingStatus::PipelineMessage {
                stage: "GPU Warning".to_string(),
                message,
            });
            Arc::make_mut(&mut config).set_enable_layout_detection(false);
            Ok((config, None))
        }
        Err(error) => Err(anyhow!(
            "[{}] Failed to create InferenceHandle: {}",
            pipeline_label,
            error
        )),
    }
}

pub(crate) fn jp2_quality(high_quality: bool) -> u8 {
    crate::pipeline::quality_policy::full_page_jp2(high_quality)
}

/// Enum to differentiate between user cancellation and worker abort signals
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    /// User requested cancellation (e.g., GUI cancel button)
    UserCancellation,
    /// Worker encountered an error and needs to abort
    WorkerError,
    /// Normal completion of processing
    Completion,
}

impl std::fmt::Display for ShutdownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShutdownReason::UserCancellation => write!(f, "User Cancellation"),
            ShutdownReason::WorkerError => write!(f, "Worker Error"),
            ShutdownReason::Completion => write!(f, "Completion"),
        }
    }
}

/// Shutdown signal with reason
#[derive(Debug, Clone)]
pub struct ShutdownSignal {
    pub reason: ShutdownReason,
    pub message: Option<String>,
}

impl ShutdownSignal {
    pub fn user_cancellation() -> Self {
        Self {
            reason: ShutdownReason::UserCancellation,
            message: None,
        }
    }

    pub fn worker_error(message: String) -> Self {
        Self {
            reason: ShutdownReason::WorkerError,
            message: Some(message),
        }
    }

    pub fn completion() -> Self {
        Self {
            reason: ShutdownReason::Completion,
            message: None,
        }
    }
}

// Legacy builders removed: unified builder below supersedes individual create_and_run_*_dagrs_pipeline variants

pub fn is_ocr_available() -> bool {
    // The embedded PP-OCR backend needs no external binary or data, so OCR is
    // always available when it is compiled in (Linux/macOS default).
    #[cfg(lege_paddle_ocr)]
    {
        true
    }
    #[cfg(not(lege_paddle_ocr))]
    {
        #[cfg(all(
            any(target_os = "linux", target_os = "macos"),
            feature = "tesseract-ocr"
        ))]
        {
            return check_tesseract_availability().is_ok();
        }
        #[cfg(target_os = "windows")]
        {
            return true;
        }
        #[cfg(not(any(
            target_os = "windows",
            all(
                any(target_os = "linux", target_os = "macos"),
                feature = "tesseract-ocr"
            )
        )))]
        {
            false
        }
    }
}

pub fn get_ocr_status() -> String {
    if is_ocr_available() {
        "OCR is available".to_string()
    } else {
        "OCR is not available".to_string()
    }
}

/// Convert a floating-point bbox into integer pixel bounds using rounding and clamping
pub fn rounded_clamped_bbox(
    bbox: [f32; 4],
    page_width: u32,
    page_height: u32,
) -> (u32, u32, u32, u32) {
    let mut x1 = bbox[0].round() as i64;
    let mut y1 = bbox[1].round() as i64;
    let mut x2 = bbox[2].round() as i64;
    let mut y2 = bbox[3].round() as i64;

    let max_x = page_width as i64;
    let max_y = page_height as i64;

    // Clamp to safe bounds
    x1 = x1.clamp(0, max_x.saturating_sub(1));
    y1 = y1.clamp(0, max_y.saturating_sub(1));
    x2 = x2.clamp(0, max_x);
    y2 = y2.clamp(0, max_y);

    // After clamping, if the region has no area or is invalid, return a valid minimal region
    // or return the original clamped values that indicate an invalid region
    if x2 <= x1 || y2 <= y1 {
        // Return the clamped values as they are, and the calling code should check if the region is valid
        if x1 >= max_x || y1 >= max_y || x2 <= 0 || y2 <= 0 {
            // The entire region is outside the image bounds
            return (0, 0, 0, 0);
        } else {
            // Try to create a minimal valid region
            let adjusted_x1 = x1.max(0).min(max_x.saturating_sub(1)) as u32;
            let adjusted_y1 = y1.max(0).min(max_y.saturating_sub(1)) as u32;
            let adjusted_x2 = (adjusted_x1 + 1).min(page_width);
            let adjusted_y2 = (adjusted_y1 + 1).min(page_height);
            return (adjusted_x1, adjusted_y1, adjusted_x2, adjusted_y2);
        }
    }

    (x1 as u32, y1 as u32, x2 as u32, y2 as u32)
}

/// If intersection area exceeds this fraction of the smaller box, merge image detections.
pub const IMAGE_DET_OVERLAP_MERGE_FRAC: f32 = 0.5;

fn detection_bbox_area(b: &[f32; 4]) -> f32 {
    let w = (b[2] - b[0]).max(0.0);
    let h = (b[3] - b[1]).max(0.0);
    w * h
}

fn skip_merge_full_page_vs_small(
    existing: &Detection,
    det: &Detection,
    classifier: &crate::types::LabelClassifier,
    page_w: u32,
    page_h: u32,
) -> bool {
    if !classifier.is_image_label(existing) || !classifier.is_image_label(det) {
        return false;
    }
    let page_area = (page_w as f32) * (page_h as f32);
    if page_area <= 1.0 {
        return false;
    }
    let a1 = detection_bbox_area(&existing.bbox);
    let a2 = detection_bbox_area(&det.bbox);
    let r1 = a1 / page_area;
    let r2 = a2 / page_area;
    const NEAR_FULL: f32 = 0.72;
    const SMALLISH: f32 = 0.42;
    (r1 >= NEAR_FULL && r2 <= SMALLISH) || (r2 >= NEAR_FULL && r1 <= SMALLISH)
}

fn merge_one_image_detection_into_list(
    out: &mut Vec<Detection>,
    det: Detection,
    classifier: &crate::types::LabelClassifier,
    overlap_frac_min: f32,
    page_w: u32,
    page_h: u32,
) -> bool {
    if !classifier.is_image_label(&det) {
        out.push(det);
        return false;
    }
    let a2 = detection_bbox_area(&det.bbox);
    if a2 <= 0.0 {
        out.push(det);
        return false;
    }
    for existing in out.iter_mut() {
        if !classifier.is_image_label(existing) {
            continue;
        }
        if skip_merge_full_page_vs_small(existing, &det, classifier, page_w, page_h) {
            continue;
        }
        let ix0 = existing.bbox[0].max(det.bbox[0]);
        let iy0 = existing.bbox[1].max(det.bbox[1]);
        let ix1 = existing.bbox[2].min(det.bbox[2]);
        let iy1 = existing.bbox[3].min(det.bbox[3]);
        if ix0 >= ix1 || iy0 >= iy1 {
            continue;
        }
        let overlap_area = (ix1 - ix0) * (iy1 - iy0);
        let a1 = detection_bbox_area(&existing.bbox);
        let min_a = a1.min(a2);
        if min_a <= 0.0 {
            continue;
        }
        if overlap_area > overlap_frac_min * min_a {
            existing.bbox[0] = existing.bbox[0].min(det.bbox[0]);
            existing.bbox[1] = existing.bbox[1].min(det.bbox[1]);
            existing.bbox[2] = existing.bbox[2].max(det.bbox[2]);
            existing.bbox[3] = existing.bbox[3].max(det.bbox[3]);
            existing.confidence = existing.confidence.max(det.confidence);
            return true;
        }
    }
    out.push(det);
    false
}

/// Merge overlapping image layout boxes so each physical illustration is processed once.
pub fn merge_overlapping_image_detections(
    detections: &mut Vec<Detection>,
    classifier: &crate::types::LabelClassifier,
    page_w: u32,
    page_h: u32,
) {
    let overlap_frac_min = IMAGE_DET_OVERLAP_MERGE_FRAC;
    loop {
        let mut out = Vec::with_capacity(detections.len());
        let mut any_merged = false;
        for det in detections.drain(..) {
            if merge_one_image_detection_into_list(
                &mut out,
                det,
                classifier,
                overlap_frac_min,
                page_w,
                page_h,
            ) {
                any_merged = true;
            }
        }
        *detections = out;
        if !any_merged {
            break;
        }
    }
}

/// Return true when an image-class box covers a substantial amount of
/// substantive text. Such boxes are layout false positives: preserving them as
/// color overlays produces the visible "half a text column in color" seam.
pub fn image_detection_overlaps_substantive_text(
    image: &Detection,
    detections: &[Detection],
    classifier: &crate::types::LabelClassifier,
) -> bool {
    if !classifier.is_image_label(image) {
        return false;
    }
    let image_area = detection_bbox_area(&image.bbox);
    if image_area <= 0.0 {
        return true;
    }

    let overlap_area: f32 = detections
        .iter()
        .filter(|detection| classifier.is_substantive_text(detection))
        .map(|text| {
            let x1 = image.bbox[0].max(text.bbox[0]);
            let y1 = image.bbox[1].max(text.bbox[1]);
            let x2 = image.bbox[2].min(text.bbox[2]);
            let y2 = image.bbox[3].min(text.bbox[3]);
            (x2 - x1).max(0.0) * (y2 - y1).max(0.0)
        })
        .sum();

    overlap_area.min(image_area) / image_area >= 0.20
}

/// Keep an image-class box as a raster overlay only when it is continuous-tone
/// (photo, map, engraving). Line art is skipped so the pixels stay in the
/// page binarization or the MRC JBIG2 mask, instead of punching a photo hole.
pub fn should_keep_image_overlay(
    image: &Detection,
    rgb: &[u8],
    width: usize,
    height: usize,
    detections: &[Detection],
    classifier: &crate::types::LabelClassifier,
) -> bool {
    if !classifier.is_image_label(image) {
        return true;
    }
    if image_detection_overlaps_substantive_text(image, detections, classifier) {
        return false;
    }
    !crate::content_class::region_is_line_art(rgb, width, height, image.bbox)
}

// Helper to encode a small image region; used by image overlays in the page pipeline
/// Build the `(EncodingSettings, format_tag)` pair for a region overlay.
///
/// `jpeg_compat`: when true the `Jpeg` format variant always emits JPEG even for
/// non-cover regions; when false a non-cover Jpeg region is encoded as JP2 instead.
pub fn region_encoding_settings(
    format: CoverFormat,
    is_cover: bool,
    high_quality: bool,
    jpeg_compat: bool,
) -> Result<(crate::encoding::EncodingSettings, &'static str)> {
    use crate::encoding::{EncodingSettings, Jbig2Settings, JpegSettings};
    Ok(match format {
        CoverFormat::Jpeg => {
            if !is_cover && !jpeg_compat {
                let q = jp2_quality(high_quality);
                (EncodingSettings::Jp2Lam { quality: q }, "jp2")
            } else {
                let q = crate::pipeline::quality_policy::region_jpeg(high_quality, is_cover);
                (
                    EncodingSettings::Jpeg(JpegSettings {
                        quality: q,
                        baseline: true,
                        optimized: true,
                        downsample: true,
                    }),
                    "jpeg",
                )
            }
        }
        CoverFormat::Ccitt4 => (EncodingSettings::Ccitt4, "ccitt4"),
        CoverFormat::Jbig2 => (
            EncodingSettings::Jbig2(Jbig2Settings {
                pdf_fragment_mode: true,
                mode: Jbig2Mode::Generic,
                use_jbig2_halftone_segments: false,
            }),
            "jbig2",
        ),
        CoverFormat::Jp2 => {
            let q = crate::pipeline::quality_policy::region_jp2(high_quality, is_cover);
            (EncodingSettings::Jp2Lam { quality: q }, "jp2")
        }
        CoverFormat::None => return Err(anyhow!("No format for region encoding")),
    })
}

pub async fn encode_region_image(
    image_data: &[u8],
    width: u32,
    height: u32,
    format: CoverFormat,
    is_cover: bool,
    high_quality: bool,
    jpeg_compat: bool,
) -> Result<(Vec<u8>, String)> {
    // Guardrails: sanity-check dimensions and buffer length
    const MAX_OVERLAY_SIDE: u32 = 8192;
    const CHANNELS: usize = 3; // regions are RGB buffers
    if width == 0 || height == 0 {
        return Err(anyhow!("Region has zero dimension"));
    }
    if width > MAX_OVERLAY_SIDE || height > MAX_OVERLAY_SIDE {
        return Err(anyhow!(
            "Region exceeds max side limit ({} or {} > {})",
            width,
            height,
            MAX_OVERLAY_SIDE
        ));
    }
    let expected_len = width as usize * height as usize * CHANNELS;
    if image_data.len() < expected_len {
        return Err(anyhow!(
            "Region buffer shorter than expected ({} < {})",
            image_data.len(),
            expected_len
        ));
    }
    if image_data.len() > expected_len {
        // Allow larger buffers if caller provided padded region; slice to expected
        crate::debug_log!(
            "encode_region_image: trimming padded buffer ({} -> {})",
            image_data.len(),
            expected_len
        );
    }
    use crate::encoding::{EncodingManager, EncodingResult, ImageBuffer as LegeImageBuffer};

    let (settings, fmt_str) =
        region_encoding_settings(format, is_cover, high_quality, jpeg_compat)?;

    let image_data_owned = image_data[..expected_len].to_vec();
    let permit = match get_encode_semaphore() {
        Some(sem) => Some(sem.acquire_owned().await.ok()),
        None => None,
    };
    let (encoding_result, fmt_str) = crate::runtime_stats::spawn_blocking_stage(
        crate::runtime_stats::Stage::Encode,
        move || {
            let buffer = LegeImageBuffer {
                data: &image_data_owned,
                width,
                height,
                channels: CHANNELS as u8,
            };
            let result = EncodingManager::encode(&buffer, &settings)
                .map_err(|e| anyhow!("Region encoding failed: {}", e))?;
            Ok::<(EncodingResult, String), anyhow::Error>((result, fmt_str.to_string()))
        },
    )
    .await
    .map_err(|e| anyhow!("Region encoding task panicked: {}", e))??;
    drop(permit);

    match encoding_result {
        EncodingResult::Standard(data) => Ok((data, fmt_str)),
        EncodingResult::Jbig2WithGlobals { page_data, .. } => {
            if fmt_str != "jbig2" {
                return Err(anyhow!(
                    "Encoder returned JBIG2 data but format tag is '{}'",
                    fmt_str
                ));
            }
            // Region overlays do not carry a separate global stream in this path.
            // Return the page stream only to avoid corrupting the JBIG2 payload.
            Ok((page_data, fmt_str))
        }
    }
}

// Helper function to determine if a page should be treated as a cover page
pub fn should_treat_as_cover_page(page_index: usize, config: &PipelineConfig) -> bool {
    // If no_cover_page is enabled, never treat any page as cover
    if config.no_cover_page {
        return false;
    }

    // If cover pages are disabled, never treat any page as cover
    if !config.enable_cover_page {
        return false;
    }

    // If a page range is specified, only treat as cover if the range includes the
    // document's first page (page_range is 1-based user input).
    if let Some(range) = &config.page_range {
        if range.start > 1 {
            return false;
        }
    }

    // First page (index 0) is the cover page
    page_index == 0
}

/// Whether the page should be emitted as a preserved full-color cover layer.
/// A disabled cover format means source page one follows the normal body path.
pub fn should_preserve_cover_page(page_index: usize, config: &PipelineConfig) -> bool {
    should_treat_as_cover_page(page_index, config)
        && *config.cover_format() != crate::types::CoverFormat::None
}

use crate::encoding::{
    EncodingManager, EncodingResult, EncodingSettings, ImageBuffer as LegeImageBuffer,
    Jbig2Settings, JpegSettings,
};

// Minimal HOCR generator for pass-through text when OCR is disabled.
// This creates a single line spanning the page containing the raw text.
/// Convert PDF text layer extraction into HOCR format for preservation
/// This is specifically for preserving existing PDF text layers, NOT for WinOCR output
/// which already comes in proper HOCR format and should not be modified.
pub fn build_hocr_from_pdf_text(text: &str, width: u32, height: u32) -> String {
    fn escape_minimal(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for ch in s.chars() {
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&#39;"),
                _ => out.push(ch),
            }
        }
        out
    }

    let page_w = width.max(1);
    let page_h = height.max(1);

    // Split text into lines and create proper HOCR structure
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let mut hocr = String::with_capacity(text.len() * 2);
    hocr.push_str(&format!(
        "<div class='ocr_page' id='page_1' title='bbox 0 0 {} {}'>",
        page_w, page_h
    ));

    // Distribute lines evenly across the page height
    let line_height = if lines.len() > 1 {
        page_h as f32 / lines.len() as f32
    } else {
        page_h as f32
    };

    for (i, line) in lines.iter().enumerate() {
        let line_text = line.trim();
        if line_text.is_empty() {
            continue;
        }

        let y1 = (i as f32 * line_height) as u32;
        let y2 = ((i as f32 + 1.0) * line_height) as u32;

        // Split line into words for better text layer quality
        let words: Vec<&str> = line_text.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }

        hocr.push_str(&format!(
            "<span class='ocr_line' title='bbox 5 {} {} {}; baseline 0 0'>",
            y1,
            page_w.saturating_sub(10),
            y2
        ));

        // Create individual word spans
        let word_width = if words.len() > 1 {
            (page_w.saturating_sub(20)) as f32 / words.len() as f32
        } else {
            (page_w.saturating_sub(20)) as f32
        };

        for (j, word) in words.iter().enumerate() {
            let x1 = 10 + (j as f32 * word_width) as u32;
            let x2 = 10 + ((j as f32 + 1.0) * word_width) as u32;
            let escaped_word = escape_minimal(word);

            hocr.push_str(&format!(
                "<span class='ocrx_word' title='bbox {} {} {} {}'>{}</span>",
                x1, y1, x2, y2, escaped_word
            ));

            // Add space between words (except for last word)
            if j < words.len() - 1 {
                hocr.push(' ');
            }
        }

        hocr.push_str("</span>");
    }

    hocr.push_str("</div>");
    hocr
}

pub fn build_hocr_from_positioned_words(
    words: &[NativeTextWord],
    width: u32,
    height: u32,
) -> String {
    fn escape_minimal(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for ch in s.chars() {
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&#39;"),
                _ => out.push(ch),
            }
        }
        out
    }

    let mut positioned: Vec<NativeTextWord> = words
        .iter()
        .filter(|w| !w.text.trim().is_empty())
        .filter(|w| w.bbox[2] > w.bbox[0] && w.bbox[3] > w.bbox[1])
        .cloned()
        .collect();
    if positioned.is_empty() {
        return String::new();
    }

    positioned.sort_by(|a, b| {
        a.bbox[1]
            .partial_cmp(&b.bbox[1])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.bbox[0]
                    .partial_cmp(&b.bbox[0])
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    let mut lines: Vec<Vec<NativeTextWord>> = Vec::new();
    for word in positioned {
        let cy = (word.bbox[1] + word.bbox[3]) * 0.5;
        let h = (word.bbox[3] - word.bbox[1]).max(1.0);
        if let Some(line) = lines.last_mut() {
            let line_top = line.iter().map(|w| w.bbox[1]).fold(f32::MAX, f32::min);
            let line_bottom = line.iter().map(|w| w.bbox[3]).fold(0.0f32, f32::max);
            let line_cy = (line_top + line_bottom) * 0.5;
            let line_h = (line_bottom - line_top).max(1.0);
            if (cy - line_cy).abs() <= h.max(line_h) * 0.6 {
                line.push(word);
                continue;
            }
        }
        lines.push(vec![word]);
    }

    let mut hocr = String::new();
    hocr.push_str(&format!(
        "<div class='ocr_page' id='page_1' title='bbox 0 0 {} {}'>",
        width.max(1),
        height.max(1)
    ));

    for mut line in lines {
        line.sort_by(|a, b| {
            a.bbox[0]
                .partial_cmp(&b.bbox[0])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let x1 = line.iter().map(|w| w.bbox[0]).fold(f32::MAX, f32::min);
        let y1 = line.iter().map(|w| w.bbox[1]).fold(f32::MAX, f32::min);
        let x2 = line.iter().map(|w| w.bbox[2]).fold(0.0f32, f32::max);
        let y2 = line.iter().map(|w| w.bbox[3]).fold(0.0f32, f32::max);
        hocr.push_str(&format!(
            "<span class='ocr_line' title='bbox {} {} {} {}; baseline 0 0'>",
            x1.round().max(0.0) as u32,
            y1.round().max(0.0) as u32,
            x2.round().max(1.0) as u32,
            y2.round().max(1.0) as u32
        ));

        for (i, word) in line.iter().enumerate() {
            if i > 0 {
                hocr.push(' ');
            }
            hocr.push_str(&format!(
                "<span class='ocrx_word' title='bbox {} {} {} {}'>{}</span>",
                word.bbox[0].round().max(0.0) as u32,
                word.bbox[1].round().max(0.0) as u32,
                word.bbox[2].round().max(1.0) as u32,
                word.bbox[3].round().max(1.0) as u32,
                escape_minimal(&word.text)
            ));
        }
        hocr.push_str("</span>");
    }

    hocr.push_str("</div>");
    hocr
}
/// Get available system RAM in GB
// The Android arm returns early, making the desktop detection below dead code
// on that target only — scoped so genuine dead code still warns on desktop.
#[cfg_attr(feature = "android", allow(unreachable_code))]
pub fn get_available_ram_gb() -> usize {
    // Android must answer before the fallthrough below: an app is entitled to
    // its heap class, not total device RAM, and the 8 GB tail would size
    // AdaptiveConcurrency for a machine that does not exist.
    #[cfg(feature = "android")]
    return crate::android::available_ram_gb();

    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

        let mut mem_status = MEMORYSTATUSEX {
            dwLength: core::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };

        // SAFETY: `GlobalMemoryStatusEx` requires a writable `MEMORYSTATUSEX`
        // whose `dwLength` is set to the struct's own size, which is exactly
        // what is constructed above. The pointer is to a live local that
        // outlives the call, and the API writes only within that struct.
        if unsafe { GlobalMemoryStatusEx(&mut mem_status) }.is_ok() {
            let total_ram_gb = mem_status.ullTotalPhys / (1024 * 1024 * 1024);
            return total_ram_gb as usize;
        }
    }

    #[cfg(target_os = "linux")]
    {
        // sysinfo 0.39 accounts for parent cgroup limits. That prevents the
        // adaptive image/JP2 workers from sizing themselves for host RAM when
        // Lege is running inside a memory-limited container.
        //
        // Refresh RAM only: `System::new_all()` walks every process in /proc to
        // answer a question about two numbers.
        let system = sysinfo::System::new_with_specifics(
            sysinfo::RefreshKind::nothing()
                .with_memory(sysinfo::MemoryRefreshKind::nothing().with_ram()),
        );
        let host_bytes = system.total_memory();
        let effective_bytes = system
            .cgroup_limits()
            .map(|limits| limits.total_memory)
            .filter(|&bytes| bytes > 0)
            .map(|bytes| bytes.min(host_bytes))
            .unwrap_or(host_bytes);
        if effective_bytes > 0 {
            return (effective_bytes / (1024 * 1024 * 1024)) as usize;
        }
    }

    // macOS support removed in this codebase (maintained separately); skip macOS RAM detection.

    // Fallback to 8GB if detection fails
    8
}

/// Await a pipeline stage task, aborting remaining tasks on shutdown signal.
///
/// Each stage call conveys the tasks that are still alive and should be cancelled
/// together — pass them as `AbortHandle`s so the caller retains ownership of the
/// `JoinHandle`s for the subsequent `await` calls.
pub async fn await_stage_or_cancel(
    task: &mut tokio::task::JoinHandle<anyhow::Result<()>>,
    shutdown_rx: &mut tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
    stage_name: &str,
    abort_remaining: &[tokio::task::AbortHandle],
) -> anyhow::Result<()> {
    await_stage_or_cancel_with_token(task, shutdown_rx, stage_name, abort_remaining, None).await
}

/// Variant of [`await_stage_or_cancel`] that also trips the renderer/source
/// cancellation token before aborting async stage tasks. This matters because
/// aborting a Tokio task alone cannot stop blocking work it has already
/// dispatched.
pub async fn await_stage_or_cancel_with_token(
    task: &mut tokio::task::JoinHandle<anyhow::Result<()>>,
    shutdown_rx: &mut tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
    stage_name: &str,
    abort_remaining: &[tokio::task::AbortHandle],
    cancellation: Option<&lege_pdf_read::CancellationToken>,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            result = &mut *task => {
                return result.map_err(|e| anyhow!("Stage '{}' panicked: {}", stage_name, e))?;
            }
            signal = shutdown_rx.recv() => {
                if let Ok(sig) = signal {
                    if let Some(cancellation) = cancellation {
                        cancellation.cancel();
                    }
                    task.abort();
                    for handle in abort_remaining {
                        handle.abort();
                    }
                    return Err(anyhow::anyhow!(
                        "Processing cancelled during {}: {}",
                        stage_name,
                        sig.message.unwrap_or_else(|| "User requested cancellation".to_string())
                    ));
                }
            }
        }
    }
}

/// Encode page data using the appropriate encoding format
/// - JBIG2: Accepts 0/255 (channels=1) or 0/1 (channels=0)
/// - CCITT4: Accepts 0/255 (channels=1) or bit-packed (channels=0)
/// - JPEG: Accepts grayscale/RGB data
pub async fn encode_page_data(
    binarized: &[u8],
    width: usize,
    height: usize,
    page_index: usize,
    config: &PipelineConfig,
) -> Result<crate::accumulator::ContentType> {
    let encoding_start = std::time::Instant::now();

    // Determine encoding settings
    let (encoding_settings, base_format) = match config.text_format() {
        "jbig2" => (
            EncodingSettings::Jbig2(Jbig2Settings {
                pdf_fragment_mode: true,
                mode: config.jbig2_mode(),
                use_jbig2_halftone_segments: false,
            }),
            "jbig2",
        ),
        "ccitt4" => (EncodingSettings::Ccitt4, "ccitt"),
        "jpeg" => (
            EncodingSettings::Jpeg(JpegSettings {
                quality: if config.high_quality_output() { 95 } else { 40 },
                baseline: true,
                optimized: true,
                downsample: false,
            }),
            "jpeg",
        ),
        "djvu" => {
            // DJVU encoding is handled by DjvuOrchestrator via accept_full_page_data()
            // Return a placeholder that won't be used - the sink handles encoding
            return Ok(crate::accumulator::ContentType::EncodedImage {
                data: std::sync::Arc::from(vec![]),
                pixel_width: width as u32,
                pixel_height: height as u32,
                format: "djvu-placeholder".to_string(),
            });
        }
        _ => {
            return Err(anyhow::anyhow!(
                "No valid text encoding format specified: {}",
                config.text_format()
            ));
        }
    };

    let binarized_owned = binarized.to_vec();
    let text_format = config.text_format().to_string();
    let encode_sem = get_encode_semaphore();
    let permit = match encode_sem {
        Some(sem) => Some(sem.acquire_owned().await.ok()),
        None => None,
    };
    let encoding_result = crate::runtime_stats::spawn_blocking_stage(
        crate::runtime_stats::Stage::Encode,
        move || {
            let buffer = LegeImageBuffer {
                data: &binarized_owned,
                width: width as u32,
                height: height as u32,
                channels: 1u8,
            };
            EncodingManager::encode(&buffer, &encoding_settings)
                .map_err(|e| anyhow::anyhow!("Encoding failed for format {}: {}", text_format, e))
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("Encoding task panicked: {}", e))??;
    drop(permit);

    match encoding_result {
        EncodingResult::Standard(data) => {
            if data.is_empty() {
                return Err(anyhow::anyhow!(
                    "Encoder returned empty data for {}x{} image",
                    width,
                    height
                ));
            }

            crate::perf_log!(
                encoding_start,
                "[PROFILING] Page {} {} encoding completed",
                page_index + 1,
                base_format
            );

            let final_format = if base_format == "jpeg" {
                "jpeg-gray".to_string()
            } else {
                base_format.to_string()
            };
            // JBIG2 generic (lossless) has no global dictionary → Standard variant is valid.
            Ok(crate::accumulator::ContentType::EncodedImage {
                data: std::sync::Arc::from(data),
                pixel_width: width as u32,
                pixel_height: height as u32,
                format: final_format,
            })
        }
        EncodingResult::Jbig2WithGlobals {
            page_data,
            global_data,
        } => {
            if base_format != "jbig2" {
                return Err(anyhow::anyhow!(
                    "Non-JBIG2 text mode returned JBIG2 payload (Jbig2WithGlobals variant)"
                ));
            }
            if page_data.is_empty() {
                return Err(anyhow::anyhow!(
                    "Encoder returned empty data for {}x{} image",
                    width,
                    height
                ));
            }

            crate::perf_log!(
                encoding_start,
                "[PROFILING] Page {} JBIG2 encoding completed",
                page_index + 1
            );

            Ok(crate::accumulator::ContentType::Jbig2ImageWithGlobals {
                page_data: std::sync::Arc::from(page_data),
                global_data: std::sync::Arc::from(global_data),
                pixel_width: width as u32,
                pixel_height: height as u32,
            })
        }
    }
}

// ============================================================================
// Writer Actor Pattern - Replaces AsyncMutex contention with dedicated writer
// ============================================================================

use std::path::PathBuf;
use tokio::sync::mpsc;

// Retained for its unit test; the writer actor no longer reorders (it writes
// pages in arrival order and restores logical order via the page tree).
#[cfg(test)]
fn drain_ready_values<T>(
    buffer: &mut std::collections::BTreeMap<usize, T>,
    next_expected: &mut usize,
) -> Vec<T> {
    let mut ready = Vec::new();
    while let Some(value) = buffer.remove(next_expected) {
        ready.push(value);
        *next_expected += 1;
    }
    ready
}

/// Message sent to the PDF writer actor
#[derive(Clone, Debug)]
pub enum WriterMessage {
    /// Append a page to the PDF
    AppendPage {
        page: crate::accumulator::Page,
        page_index: usize,
    },
    /// Supply the *source* document's bookmarks, to be resolved and written at
    /// finalize time (must arrive before Finalize).
    SetBookmarks {
        bookmarks: Vec<crate::pagerender::OwnedBookmarkNode>,
        /// For reflow: source-page-index → output-page-index mapping. Empty = identity.
        source_to_output: std::collections::HashMap<usize, usize>,
    },
    /// Supply a synthesized outline, already in output page space. It is used
    /// only when the source document had no outline that survived remapping.
    SetSyntheticOutline(Vec<lege_pdf_write::outline::OutlineItem>),
    /// Supply document identity metadata. Empty fields are omitted from Info.
    SetDocumentIdentity {
        title: Option<String>,
        author: Option<String>,
    },
    /// Supply the document-wide glyph font (`text_format = "glyphfont"`).
    /// Must arrive before Finalize; pages reference it by a reserved id.
    SetGlyphFont(lege_pdf_write::font::EmbeddedFont),
    /// Signal that all pages have been sent and PDF should be finalized
    Finalize,
}

/// Handle for sending pages to the dedicated PDF writer actor
#[derive(Clone)]
pub struct PdfWriterHandle {
    sender: mpsc::Sender<WriterMessage>,
}

impl PdfWriterHandle {
    /// Send a page to be written to the PDF
    pub async fn send_page(
        &self,
        page: crate::accumulator::Page,
        page_index: usize,
    ) -> Result<(), anyhow::Error> {
        self.sender
            .send(WriterMessage::AppendPage { page, page_index })
            .await
            .map_err(|_| anyhow::anyhow!("PDF writer actor has stopped"))?;
        Ok(())
    }

    /// Send bookmarks to be written at finalize time. Must be called before finalize().
    pub async fn send_bookmarks(
        &self,
        bookmarks: Vec<crate::pagerender::OwnedBookmarkNode>,
        source_to_output: std::collections::HashMap<usize, usize>,
    ) -> Result<(), anyhow::Error> {
        self.sender
            .send(WriterMessage::SetBookmarks {
                bookmarks,
                source_to_output,
            })
            .await
            .map_err(|_| anyhow::anyhow!("PDF writer actor has stopped"))?;
        Ok(())
    }

    /// Send a synthesized outline (output page space). Must be called before
    /// finalize(). A source outline that resolves takes precedence over this.
    pub async fn send_synthetic_outline(
        &self,
        items: Vec<lege_pdf_write::outline::OutlineItem>,
    ) -> Result<(), anyhow::Error> {
        self.sender
            .send(WriterMessage::SetSyntheticOutline(items))
            .await
            .map_err(|_| anyhow::anyhow!("PDF writer actor has stopped"))?;
        Ok(())
    }

    pub async fn send_document_identity(
        &self,
        title: Option<String>,
        author: Option<String>,
    ) -> Result<(), anyhow::Error> {
        self.sender
            .send(WriterMessage::SetDocumentIdentity { title, author })
            .await
            .map_err(|_| anyhow::anyhow!("PDF writer actor has stopped"))?;
        Ok(())
    }

    /// Send the document-wide glyph font. Must be called before finalize()
    /// whenever any page carried glyph text.
    pub async fn send_glyph_font(
        &self,
        font: lege_pdf_write::font::EmbeddedFont,
    ) -> Result<(), anyhow::Error> {
        self.sender
            .send(WriterMessage::SetGlyphFont(font))
            .await
            .map_err(|_| anyhow::anyhow!("PDF writer actor has stopped"))?;
        Ok(())
    }

    /// Signal the writer to finalize the PDF
    pub async fn finalize(&self) -> Result<(), anyhow::Error> {
        self.sender
            .send(WriterMessage::Finalize)
            .await
            .map_err(|_| anyhow::anyhow!("PDF writer actor has stopped"))?;
        Ok(())
    }
}

/// Spawn a dedicated PDF writer actor that owns the StreamingPdfBuilder
/// Returns a handle for sending pages and a JoinHandle for the actor task
pub fn spawn_pdf_writer_actor(
    output_path: PathBuf,
    total_pages: usize,
    progress_tracker: crate::progress::ProgressTracker,
    use_margin_label: bool,
    channel_capacity: usize,
) -> (
    PdfWriterHandle,
    tokio::task::JoinHandle<Result<(), anyhow::Error>>,
) {
    let (tx, mut rx) = mpsc::channel::<WriterMessage>(channel_capacity.max(1));

    let handle = PdfWriterHandle { sender: tx };

    let task = tokio::spawn(async move {
        use lege_pdf_write::meta::PdfProfile;
        use lege_pdf_write::writer::DocumentWriter;
        use std::io::{BufWriter, Write};

        let parent = output_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        let temporary = tempfile::NamedTempFile::new_in(parent).map_err(|e| {
            anyhow::anyhow!(
                "Failed to create temporary output for {}: {}",
                output_path.display(),
                e
            )
        })?;
        let file = temporary.reopen().map_err(|e| {
            anyhow::anyhow!(
                "Failed to open temporary output for {}: {}",
                output_path.display(),
                e
            )
        })?;
        // Pdf17 preserves the current live behavior (per-page text layers, no
        // PDF/A metadata). Switching to PdfProfile::PdfA1b would emit the
        // OutputIntent/Info the old finalize intended but never applied.
        let mut writer =
            DocumentWriter::with_profile(BufWriter::new(file), total_pages, PdfProfile::Pdf17)
                .map_err(|e| anyhow::anyhow!("Failed to init PDF writer: {}", e))?;

        // Embed Lege's glyphless OCR font once, if available.
        let embedded = crate::unicode_font::get_unicode_font();
        let embedded_available = embedded.is_some();
        if let Some(ref u) = embedded {
            writer.set_embedded_font(crate::pdf_artifact::embedded_font_from(u));
        }

        let mut pages_written = 0usize;
        let mut pending_bookmarks: Option<(
            Vec<crate::pagerender::OwnedBookmarkNode>,
            std::collections::HashMap<usize, usize>,
        )> = None;
        let mut pending_synthetic: Option<Vec<lege_pdf_write::outline::OutlineItem>> = None;
        let mut do_finalize = false;

        info_log!("[PdfWriterActor] Started (lege-pdf-write), waiting for pages...");

        while let Some(msg) = rx.recv().await {
            // The writer FREES memory by flushing pages to disk on arrival, so
            // it never blocks its own recv loop. Channel capacity is the
            // backpressure. Pages are written in arrival order; logical order
            // is restored by the page tree at finalize.
            match msg {
                WriterMessage::AppendPage { page, page_index } => {
                    let _writer_stage =
                        crate::runtime_stats::enter_stage(crate::runtime_stats::Stage::Writer);
                    let (artifact, globals) =
                        match crate::pdf_artifact::page_to_artifact(&page, embedded_available) {
                            Ok(v) => v,
                            Err(e) => {
                                crate::warn_log!(
                                    "[PdfWriterActor] Failed to convert page {}: {}",
                                    page_index,
                                    e
                                );
                                return Err(anyhow::anyhow!(
                                    "Failed to convert page {}: {}",
                                    page_index,
                                    e
                                ));
                            }
                        };
                    for (id, bytes) in globals {
                        writer.register_shared(id, bytes);
                    }
                    if let Err(e) = writer.add_page(&artifact) {
                        crate::warn_log!(
                            "[PdfWriterActor] Failed to write page {}: {}",
                            page_index,
                            e
                        );
                        return Err(anyhow::anyhow!(
                            "Failed to write page {}: {}",
                            page_index,
                            e
                        ));
                    }

                    pages_written += 1;
                    let is_last_page = pages_written == total_pages;
                    if is_last_page || pages_written % 5 == 0 {
                        if use_margin_label {
                            progress_tracker.update(
                                crate::progress::ProcessingStatus::PdfAppendMargin {
                                    current: pages_written,
                                    total: total_pages,
                                },
                            );
                        } else {
                            progress_tracker.update(crate::progress::ProcessingStatus::PdfAppend {
                                current: pages_written,
                                total: total_pages,
                            });
                        }
                    }
                }
                WriterMessage::SetBookmarks {
                    bookmarks,
                    source_to_output,
                } => {
                    pending_bookmarks = Some((bookmarks, source_to_output));
                }
                WriterMessage::SetSyntheticOutline(items) => {
                    pending_synthetic = Some(items);
                }
                WriterMessage::SetGlyphFont(font) => {
                    writer.set_glyph_font(font);
                }
                WriterMessage::SetDocumentIdentity { title, author } => {
                    writer.set_metadata(lege_pdf_write::meta::DocumentMeta {
                        title: title.unwrap_or_default(),
                        author: author.unwrap_or_default(),
                        subject: String::new(),
                        keywords: String::new(),
                        ..Default::default()
                    });
                }
                WriterMessage::Finalize => {
                    info_log!(
                        "[PdfWriterActor] Finalize requested, written {} of {} pages",
                        pages_written,
                        total_pages
                    );
                    if pages_written != total_pages {
                        return Err(anyhow::anyhow!(
                            "[PdfWriterActor] Refusing to finalize incomplete PDF: wrote {} of {} pages",
                            pages_written,
                            total_pages
                        ));
                    }
                    do_finalize = true;
                    break;
                }
            }
        }

        // finalize() consumes the writer, so it must happen after the loop.
        if do_finalize {
            let _writer_stage =
                crate::runtime_stats::enter_stage(crate::runtime_stats::Stage::Writer);
            // Outline precedence is decided here because "did the source outline
            // survive remapping" is only knowable once the pages are resolved.
            let source_outline = pending_bookmarks
                .take()
                .map(|(bookmarks, source_to_output)| {
                    bookmarks_to_outline(&bookmarks, &source_to_output)
                })
                .unwrap_or_default();
            let outline = merge_outline(source_outline, pending_synthetic.take());
            if !outline.is_empty() {
                writer.set_bookmarks(outline);
            }
            let mut inner = writer
                .finalize()
                .map_err(|e| anyhow::anyhow!("Finalize failed: {}", e))?;
            inner
                .flush()
                .map_err(|e| anyhow::anyhow!("Failed to flush output: {}", e))?;
            inner
                .get_ref()
                .sync_all()
                .map_err(|e| anyhow::anyhow!("Failed to sync output: {}", e))?;
            drop(inner);
            temporary.persist(&output_path).map_err(|e| {
                anyhow::anyhow!(
                    "Failed to publish output {}: {}",
                    output_path.display(),
                    e.error
                )
            })?;
            crate::success_log!("[PdfWriterActor] PDF written to: {}", output_path.display());
        }

        info_log!("[PdfWriterActor] Shutting down");
        Ok(())
    });

    (handle, task)
}

/// Resolve document bookmarks (source page indices, optional source→output map)
/// into writer outline items keyed by output page index.
///
/// A node whose page falls outside the run — a page range that cuts a part
/// heading, or a destination the reader could not resolve — is dropped, but its
/// resolvable children are promoted into its place instead of disappearing with
/// it. One unreachable parent must not cost a whole subtree.
///
/// Preserved nodes keep the page-level `/Fit` destination. Their `top` is in the
/// *source* page's user space, and the output page is re-rendered at a different
/// scale, so reusing it would land somewhere arbitrary.
pub(crate) fn bookmarks_to_outline(
    nodes: &[crate::pagerender::OwnedBookmarkNode],
    source_to_output: &std::collections::HashMap<usize, usize>,
) -> Vec<lege_pdf_write::outline::OutlineItem> {
    let mut out = Vec::new();
    for node in nodes {
        let out_idx = if node.source_page == usize::MAX {
            None
        } else if source_to_output.is_empty() {
            Some(node.source_page)
        } else {
            source_to_output.get(&node.source_page).copied()
        };
        let children = bookmarks_to_outline(&node.children, source_to_output);
        match out_idx {
            Some(out_idx) => out.push(lege_pdf_write::outline::OutlineItem {
                title: node.title.clone(),
                page_index: out_idx as u32,
                top: None,
                children,
            }),
            None => out.extend(children),
        }
    }
    out
}

/// Outline precedence. An author's outline is ground truth: when it resolves to
/// at least one page it wins outright, and nothing is synthesized. A synthesized
/// outline only ever fills a gap, so the feature's failure mode is absence.
pub(crate) fn merge_outline(
    source: Vec<lege_pdf_write::outline::OutlineItem>,
    synthetic: Option<Vec<lege_pdf_write::outline::OutlineItem>>,
) -> Vec<lege_pdf_write::outline::OutlineItem> {
    if source.is_empty() {
        synthetic.unwrap_or_default()
    } else {
        source
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    use super::{
        PdfWriterHandle, WriterMessage, bookmarks_to_outline, drain_ready_values,
        image_detection_overlaps_substantive_text, merge_outline, should_keep_image_overlay,
        should_preserve_cover_page, should_treat_as_cover_page, spawn_pdf_writer_actor,
    };
    use crate::engine::Detection;
    use crate::pipeline::config::{PageRange, PipelineConfig};
    use crate::types::{CoverFormat, LABEL_CLASSIFIER, category_for_class, class_id_for};

    fn detection(class_name: &str, bbox: [f32; 4]) -> Detection {
        let class_id = class_id_for(class_name).expect("known layout class");
        Detection {
            class_id,
            class_name: Some(class_name.to_string()),
            confidence: 0.9,
            bbox,
            category: category_for_class(class_id),
            context: None,
        }
    }

    fn empty_page(index: usize) -> crate::accumulator::Page {
        crate::accumulator::Page {
            width: 1.0,
            height: 1.0,
            elements: Vec::new(),
            hocr_text: None,
            index,
            binarized: None,
        }
    }

    fn node(
        title: &str,
        source_page: usize,
        children: Vec<crate::pagerender::OwnedBookmarkNode>,
    ) -> crate::pagerender::OwnedBookmarkNode {
        crate::pagerender::OwnedBookmarkNode {
            title: title.to_string(),
            source_page,
            top: None,
            children,
        }
    }

    fn item(title: &str, page_index: u32) -> lege_pdf_write::outline::OutlineItem {
        lege_pdf_write::outline::OutlineItem {
            title: title.to_string(),
            page_index,
            top: None,
            children: Vec::new(),
        }
    }

    #[test]
    fn a_source_outline_beats_a_synthesized_one() {
        let merged = merge_outline(
            vec![item("Preface", 0), item("Chapter I", 4)],
            Some(vec![item("Synthesized", 2)]),
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].title, "Preface");
    }

    #[test]
    fn image_box_covering_text_is_not_kept_as_a_color_overlay() {
        let image = detection("image", [0.0, 0.0, 100.0, 100.0]);
        let text = detection("text", [50.0, 0.0, 100.0, 100.0]);
        assert!(image_detection_overlaps_substantive_text(
            &image,
            &[image.clone(), text],
            &LABEL_CLASSIFIER,
        ));
    }

    #[test]
    fn staff_line_image_is_not_kept_as_a_photo_overlay() {
        let (w, h) = (256usize, 256usize);
        let mut rgb = vec![245u8; w * h * 3];
        for y in (8..h).step_by(8) {
            for x in 8..(w - 8) {
                let i = (y * w + x) * 3;
                rgb[i] = 20;
                rgb[i + 1] = 20;
                rgb[i + 2] = 20;
            }
        }
        let image = detection("image", [0.0, 0.0, 256.0, 256.0]);
        assert!(!should_keep_image_overlay(
            &image,
            &rgb,
            w,
            h,
            &[image.clone()],
            &LABEL_CLASSIFIER,
        ));
    }

    #[test]
    fn gradient_photo_is_kept_as_a_photo_overlay() {
        let (w, h) = (256usize, 128usize);
        let mut rgb = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                let v = x as u8;
                rgb[i] = v;
                rgb[i + 1] = v;
                rgb[i + 2] = v;
            }
        }
        let image = detection("image", [0.0, 0.0, 256.0, 128.0]);
        assert!(should_keep_image_overlay(
            &image,
            &rgb,
            w,
            h,
            &[image.clone()],
            &LABEL_CLASSIFIER,
        ));
    }

    #[test]
    fn small_caption_overlap_does_not_discard_a_real_image() {
        let image = detection("image", [0.0, 0.0, 100.0, 100.0]);
        let caption = detection("text", [0.0, 95.0, 100.0, 105.0]);
        assert!(!image_detection_overlaps_substantive_text(
            &image,
            &[image.clone(), caption],
            &LABEL_CLASSIFIER,
        ));
    }

    #[test]
    fn a_synthesized_outline_fills_the_gap_and_nothing_fills_neither() {
        let merged = merge_outline(Vec::new(), Some(vec![item("Chapter I", 3)]));
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].title, "Chapter I");

        assert!(merge_outline(Vec::new(), None).is_empty());
        assert!(merge_outline(Vec::new(), Some(Vec::new())).is_empty());
    }

    #[test]
    fn a_page_range_shifts_indexes_and_promotes_orphaned_children() {
        // Pages 10..14 of the source become output pages 0..4. The part heading
        // on source page 3 falls outside the run; its chapters must survive.
        let map: std::collections::HashMap<usize, usize> =
            (10..14).enumerate().map(|(out, src)| (src, out)).collect();
        let bookmarks = vec![node(
            "Part One",
            3,
            vec![
                node("Chapter I", 10, vec![]),
                node("Chapter II", 12, vec![]),
            ],
        )];

        let outline = bookmarks_to_outline(&bookmarks, &map);

        assert_eq!(
            outline.len(),
            2,
            "the unreachable parent does not take its children with it"
        );
        assert_eq!(outline[0].title, "Chapter I");
        assert_eq!(outline[0].page_index, 0);
        assert_eq!(outline[1].page_index, 2);
        assert!(outline[0].top.is_none(), "preserved nodes keep /Fit");
    }

    #[test]
    fn a_full_document_run_maps_source_pages_through_unchanged() {
        let outline = bookmarks_to_outline(
            &[node("Chapter I", 7, vec![node("Section 1.1", 9, vec![])])],
            &std::collections::HashMap::new(),
        );
        assert_eq!(outline.len(), 1);
        assert_eq!(outline[0].page_index, 7);
        assert_eq!(outline[0].children[0].page_index, 9);
    }

    #[test]
    fn out_of_order_pages_drain_in_order() {
        let mut buffer = std::collections::BTreeMap::new();
        let mut next_expected = 0usize;
        buffer.insert(2usize, "two");
        buffer.insert(0usize, "zero");
        buffer.insert(1usize, "one");

        let ready = drain_ready_values(&mut buffer, &mut next_expected);

        assert_eq!(ready, vec!["zero", "one", "two"]);
        assert!(buffer.is_empty());
        assert_eq!(next_expected, 3);
    }

    #[test]
    fn cover_is_only_source_page_one_when_it_is_first_in_the_job() {
        let mut config = PipelineConfig::new().expect("pipeline config");

        assert!(should_treat_as_cover_page(0, &config));
        assert!(!should_treat_as_cover_page(1, &config));

        config.set_page_range(Some(PageRange::new(1, 10).unwrap()));
        assert!(should_treat_as_cover_page(0, &config));

        config.set_page_range(Some(PageRange::new(2, 10).unwrap()));
        assert!(!should_treat_as_cover_page(0, &config));
        assert!(!should_treat_as_cover_page(1, &config));
    }

    #[test]
    fn disabled_cover_format_routes_page_one_through_body_processing() {
        let mut config = PipelineConfig::new().expect("pipeline config");
        assert!(should_preserve_cover_page(0, &config));

        config.set_cover_format(CoverFormat::None);
        assert!(!should_preserve_cover_page(0, &config));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bounded_writer_handle_applies_backpressure() {
        let (tx, mut rx) = mpsc::channel::<WriterMessage>(1);
        let handle = PdfWriterHandle { sender: tx };

        handle
            .send_page(empty_page(0), 0)
            .await
            .expect("first send");

        let second_send = tokio::spawn({
            let handle = handle.clone();
            async move { handle.send_page(empty_page(1), 1).await }
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

    #[tokio::test(flavor = "current_thread")]
    async fn aborted_writer_never_truncates_the_destination() {
        let directory = tempfile::tempdir().expect("temporary output directory");
        let output = directory.path().join("result.pdf");
        std::fs::write(&output, b"last-good-output").expect("seed destination");
        let manager = crate::progress::ProgressManager::new();
        let tracker = manager.create_tracker();

        let (_handle, task) = spawn_pdf_writer_actor(output.clone(), 1, tracker, false, 1);
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;

        assert_eq!(
            std::fs::read(output).expect("preserved destination"),
            b"last-good-output"
        );
    }
}
