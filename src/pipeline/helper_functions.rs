// helper_functions.rs - Shared helper functions for the pipeline
use anyhow::{Result, anyhow};
use image::RgbImage;
use tokio::sync::Semaphore;

use crate::bbox_trace;
use crate::engine::Detection;
use crate::ocr::check_tesseract_availability;
use crate::pagerender::NativeTextWord;
use crate::pipeline::config::PipelineConfig;
use crate::types::CoverFormat;
use crate::{info_log, warn_log};
use Legencode::streamline::Jbig2Mode;

// Memory monitoring and limits
// Increased from 4GB to 12GB to support layout detection with multiple workers
const DEFAULT_MEMORY_LIMIT_GB: usize = 12; // 12GB default limit for modern systems

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

pub(crate) fn jp2_quality(high_quality: bool) -> u8 {
    if high_quality { 55 } else { 45 }
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
    check_tesseract_availability().is_ok()
}

pub fn get_ocr_status() -> String {
    if is_ocr_available() {
        "OCR is available (Tesseract found)".to_string()
    } else {
        "OCR is not available (Tesseract not found)".to_string()
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
) -> Result<(Legencode::streamline::EncodingSettings, &'static str)> {
    use Legencode::streamline::{EncodingSettings, Jbig2Settings, JpegSettings};
    Ok(match format {
        CoverFormat::Jpeg => {
            if !is_cover && !jpeg_compat {
                let q = jp2_quality(high_quality);
                (EncodingSettings::Jp2Lam { quality: q }, "jp2")
            } else {
                let q: u8 = if high_quality {
                    95
                } else if is_cover {
                    50
                } else {
                    45
                };
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
            let q: u8 = if high_quality {
                88
            } else if is_cover {
                80
            } else {
                72
            };
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
        #[cfg(feature = "debug-logging")]
        crate::debug_log!(
            "encode_region_image: trimming padded buffer ({} -> {})",
            image_data.len(),
            expected_len
        );
    }
    use Legencode::streamline::{EncodingManager, EncodingResult, ImageBuffer as LegeImageBuffer};

    let (settings, fmt_str) =
        region_encoding_settings(format, is_cover, high_quality, jpeg_compat)?;

    let image_data_owned = image_data[..expected_len].to_vec();
    let permit = match get_encode_semaphore() {
        Some(sem) => Some(sem.acquire_owned().await.ok()),
        None => None,
    };
    let (encoding_result, fmt_str) = tokio::task::spawn_blocking(move || {
        let buffer = LegeImageBuffer {
            data: &image_data_owned,
            width,
            height,
            channels: CHANNELS as u8,
        };
        let result = EncodingManager::encode(&buffer, &settings)
            .map_err(|e| anyhow!("Region encoding failed: {}", e))?;
        Ok::<(EncodingResult, String), anyhow::Error>((result, fmt_str.to_string()))
    })
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

use Legencode::streamline::{
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
        let escaped = escape_minimal(line_text);

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
pub fn get_available_ram_gb() -> usize {
    #[cfg(target_os = "windows")]
    {
        use std::mem;
        use winapi::um::sysinfoapi::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

        let mut mem_status = MEMORYSTATUSEX {
            dwLength: mem::size_of::<MEMORYSTATUSEX>() as u32,
            dwMemoryLoad: 0,
            ullTotalPhys: 0,
            ullAvailPhys: 0,
            ullTotalPageFile: 0,
            ullAvailPageFile: 0,
            ullTotalVirtual: 0,
            ullAvailVirtual: 0,
            ullAvailExtendedVirtual: 0,
        };

        unsafe {
            if GlobalMemoryStatusEx(&mut mem_status) != 0 {
                let total_ram_gb = mem_status.ullTotalPhys / (1024 * 1024 * 1024);
                return total_ram_gb as usize;
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
            for line in contents.lines() {
                if line.starts_with("MemTotal:") {
                    if let Some(kb_str) = line.split_whitespace().nth(1) {
                        if let Ok(kb) = kb_str.parse::<u64>() {
                            let gb = kb / (1024 * 1024);
                            return gb as usize;
                        }
                    }
                }
            }
        }
    }

    // macOS support removed in this codebase (maintained separately); skip macOS RAM detection.

    // Fallback to 8GB if detection fails
    8
}

// Memory usage monitor
static MEMORY_MONITOR: std::sync::OnceLock<MemoryMonitor> = std::sync::OnceLock::new();

pub struct MemoryMonitor {
    limit_bytes: u64,
}

impl MemoryMonitor {
    pub fn new(limit_gb: usize) -> Self {
        Self {
            limit_bytes: (limit_gb as u64) * 1024 * 1024 * 1024,
        }
    }

    pub fn get_current_usage_bytes(&self) -> u64 {
        #[cfg(target_os = "windows")]
        {
            use winapi::shared::minwindef::DWORD;
            use winapi::um::processthreadsapi::GetCurrentProcess;
            use winapi::um::psapi::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};

            let mut pmc: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
            pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as DWORD;

            unsafe {
                if GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) != 0 {
                    return pmc.WorkingSetSize as u64;
                }
            }
        }

        #[cfg(target_os = "linux")]
        {
            if let Ok(contents) = std::fs::read_to_string("/proc/self/status") {
                for line in contents.lines() {
                    if line.starts_with("VmRSS:") {
                        if let Some(kb_str) = line.split_whitespace().nth(1) {
                            if let Ok(kb) = kb_str.parse::<u64>() {
                                return kb * 1024; // Convert KB to bytes
                            }
                        }
                    }
                }
            }
        }

        // Fallback: return 0 if unable to determine
        0
    }

    pub fn get_current_usage_gb(&self) -> f64 {
        self.get_current_usage_bytes() as f64 / (1024.0 * 1024.0 * 1024.0)
    }

    pub fn is_approaching_limit(&self, safety_margin_gb: usize) -> bool {
        let current_gb = self.get_current_usage_gb();
        let limit_gb = self.limit_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        current_gb >= (limit_gb - safety_margin_gb as f64)
    }

    pub fn get_memory_pressure(&self) -> MemoryPressure {
        // Disable memory management for systems with 16GB+ RAM
        static LOGGED_DISABLE: std::sync::Once = std::sync::Once::new();
        let system_ram_gb = get_available_ram_gb();
        if system_ram_gb >= 16 {
            LOGGED_DISABLE.call_once(|| {
                info_log!(
                    "Memory management disabled - system has {}GB RAM (â‰¥16GB)",
                    system_ram_gb
                );
            });
            return MemoryPressure::Low;
        }

        let current_gb = self.get_current_usage_gb();
        let limit_gb = self.limit_bytes as f64 / (1024.0 * 1024.0 * 1024.0);

        let pressure = current_gb / limit_gb;
        if pressure >= 0.9 {
            MemoryPressure::Critical
        } else if pressure >= 0.7 {
            MemoryPressure::High
        } else if pressure >= 0.5 {
            MemoryPressure::Medium
        } else {
            MemoryPressure::Low
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPressure {
    Low,
    Medium,
    High,
    Critical,
}

pub fn get_memory_monitor() -> &'static MemoryMonitor {
    MEMORY_MONITOR.get_or_init(|| MemoryMonitor::new(adaptive_memory_limit_gb()))
}

/// Derive a memory limit that is meaningful on the machine actually running.
///
/// The pressure ratio used for backpressure is `current_usage / limit`, so a fixed
/// 12 GB limit is useless on the 4–8 GB machines that actually run out of memory:
/// `Critical` (ratio ≥ 0.9) would require ~10.8 GB of working set, which those
/// machines can never reach before the OS kills the process. Scaling the limit to a
/// fraction of physical RAM keeps backpressure (`wait_for_memory_relief`) engaging
/// with headroom for the OS and other apps, while never exceeding the original
/// ceiling on large machines.
fn adaptive_memory_limit_gb() -> usize {
    let total_ram_gb = get_available_ram_gb();
    // Use ~70% of physical RAM as the working-set ceiling, clamped to a small floor so
    // tiny/undetected configs still trip backpressure rather than overflowing, and to
    // the historical default as the upper bound.
    ((total_ram_gb * 7) / 10).clamp(3, DEFAULT_MEMORY_LIMIT_GB)
}

/// Wait for memory pressure to decrease below critical level.
/// This implements backpressure by pausing processing when memory is too high.
/// Returns immediately if memory pressure is not critical.
///
/// Unlike a simple time-based wait, this checks memory frequently (every 50ms)
/// expecting that as other pages complete processing and get written to disk,
/// memory will be freed naturally.
pub async fn wait_for_memory_relief() {
    let monitor = get_memory_monitor();
    let mut wait_count = 0;
    let initial_usage = monitor.get_current_usage_gb();

    loop {
        let pressure = monitor.get_memory_pressure();

        if pressure != MemoryPressure::Critical {
            if wait_count > 0 {
                let current_usage = monitor.get_current_usage_gb();
                info_log!(
                    "[MemoryBackpressure] Memory pressure relieved after {} checks ({:.2} GB -> {:.2} GB)",
                    wait_count,
                    initial_usage,
                    current_usage
                );
            }
            break;
        }

        if wait_count == 0 {
            warn_log!(
                "[MemoryBackpressure] Critical memory pressure detected ({:.2} GB / {:.2} GB), pausing until other pages complete",
                monitor.get_current_usage_gb(),
                monitor.limit_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
            );
        } else if wait_count % 20 == 0 {
            // Log every 20 checks (1 second) to show we're waiting for other work to complete
            warn_log!(
                "[MemoryBackpressure] Still waiting for memory relief ({:.2} GB used, waiting for other pages to finish)",
                monitor.get_current_usage_gb()
            );
        }

        wait_count += 1;

        // Wait 50ms before checking again - short enough to respond quickly when memory frees
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
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
    loop {
        tokio::select! {
            result = &mut *task => {
                return result.map_err(|e| anyhow!("Stage '{}' panicked: {}", stage_name, e))?;
            }
            signal = shutdown_rx.recv() => {
                if let Ok(sig) = signal {
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
    #[cfg(feature = "debug-logging")]
    let encoding_start = std::time::Instant::now();

    // Create buffer - use channels=1 for binary formats so Legencode handles normalization
    let buffer = LegeImageBuffer {
        data: binarized,
        width: width as u32,
        height: height as u32,
        channels: 1u8, // Grayscale/binary data (0/255 per byte)
    };

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
    let encoding_result = tokio::task::spawn_blocking(move || {
        let buffer = LegeImageBuffer {
            data: &binarized_owned,
            width: width as u32,
            height: height as u32,
            channels: 1u8,
        };
        EncodingManager::encode(&buffer, &encoding_settings)
            .map_err(|e| anyhow::anyhow!("Encoding failed for format {}: {}", text_format, e))
    })
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

            #[cfg(feature = "debug-logging")]
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

            #[cfg(feature = "debug-logging")]
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
    /// Supply bookmarks to be written at finalize time (must arrive before Finalize)
    SetBookmarks {
        bookmarks: Vec<crate::pagerender::OwnedBookmarkNode>,
        /// For reflow: source-page-index → output-page-index mapping. Empty = identity.
        source_to_output: std::collections::HashMap<usize, usize>,
    },
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
            .send(WriterMessage::SetBookmarks { bookmarks, source_to_output })
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
        let mut builder = crate::accumulator::StreamingPdfBuilder::new();
        let mut pages_written = 0usize;
        let mut page_buffer: std::collections::BTreeMap<usize, crate::accumulator::Page> =
            std::collections::BTreeMap::new();
        let mut next_expected = 0usize;

        crate::info_log!("[PdfWriterActor] Started, waiting for pages...");

        while let Some(msg) = rx.recv().await {
            // NOTE: Do NOT block the writer for memory relief here.
            // The writer is the component that FREES memory by flushing pages to disk.
            // Blocking it makes memory pressure worse, not better.

            match msg {
                WriterMessage::AppendPage { page, page_index } => {
                    // Always accept the incoming page immediately — never block the recv loop.
                    // The channel capacity already provides backpressure to senders.
                    // Blocking here before inserting causes a deadlock: the writer spins
                    // waiting for `next_expected` to appear, but it can't receive it
                    // because it stopped reading from the channel.
                    page_buffer.insert(page_index, page);

                    // Log if buffer is growing large (informational only, no blocking)
                    if page_buffer.len() > 50 && page_buffer.len() % 25 == 0 {
                        crate::warn_log!(
                            "[PdfWriterActor] Page buffer at {} pages (next_expected: {}, waiting for out-of-order page)",
                            page_buffer.len(),
                            next_expected
                        );
                    }

                    // Drain all consecutive pages starting from next_expected
                    for page in drain_ready_values(&mut page_buffer, &mut next_expected) {
                        if let Err(e) = builder.add_page(&page) {
                            let failed_page = next_expected.saturating_sub(1);
                            crate::warn_log!(
                                "[PdfWriterActor] Failed to append page {}: {}",
                                failed_page,
                                e
                            );
                            return Err(anyhow::anyhow!(
                                "Failed to append page {}: {}",
                                failed_page,
                                e
                            ));
                        }

                        pages_written += 1;

                        // Throttle progress updates - every 5 pages or on last page
                        let is_last_page = pages_written == total_pages;
                        let should_update = is_last_page || pages_written % 5 == 0;
                        if should_update {
                            if use_margin_label {
                                progress_tracker.update(
                                    crate::progress::ProcessingStatus::PdfAppendMargin {
                                        current: pages_written,
                                        total: total_pages,
                                    },
                                );
                            } else {
                                progress_tracker.update(
                                    crate::progress::ProcessingStatus::PdfAppend {
                                        current: pages_written,
                                        total: total_pages,
                                    },
                                );
                            }
                        }
                    }
                }
                WriterMessage::SetBookmarks { bookmarks, source_to_output } => {
                    builder.set_bookmarks(bookmarks);
                    if !source_to_output.is_empty() {
                        builder.set_source_to_output_map(source_to_output);
                    }
                }
                WriterMessage::Finalize => {
                    crate::info_log!(
                        "[PdfWriterActor] Finalize requested, written {} of {} pages",
                        pages_written,
                        total_pages
                    );

                    // Check memory before finalizing if it's a large document
                    if total_pages > 50 {
                        // Only check for documents with more than 50 pages
                        let memory_monitor = get_memory_monitor();
                        if memory_monitor.get_memory_pressure() == MemoryPressure::High
                            || memory_monitor.get_memory_pressure() == MemoryPressure::Critical
                        {
                            crate::warn_log!(
                                "[PdfWriterActor] High memory pressure before finalizing large document ({} pages)",
                                total_pages
                            );
                        }
                    }

                    // Check if we have all pages
                    if pages_written != total_pages {
                        return Err(anyhow::anyhow!(
                            "[PdfWriterActor] Refusing to finalize incomplete PDF: wrote {} of {} pages",
                            pages_written,
                            total_pages
                        ));
                    }

                    // Determine if any page has OCR text
                    let has_ocr = false; // We don't have access to hocr_text here anymore
                    // This is acceptable since finalize() will check during assembly

                    // Finalize the PDF
                    let output_path_str = output_path.to_str().ok_or_else(|| {
                        anyhow::anyhow!("Output path is not valid UTF-8: {}", output_path.display())
                    })?;
                    if let Err(e) = builder.finalize(output_path_str, has_ocr) {
                        crate::warn_log!("[PdfWriterActor] Finalize failed: {}", e);
                        return Err(anyhow::anyhow!("Finalize failed: {}", e));
                    }

                    crate::success_log!(
                        "[PdfWriterActor] PDF written to: {}",
                        output_path.display()
                    );
                    break;
                }
            }
        }

        crate::info_log!("[PdfWriterActor] Shutting down");
        Ok(())
    });

    (handle, task)
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    use super::{PdfWriterHandle, WriterMessage, drain_ready_values};

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
}
