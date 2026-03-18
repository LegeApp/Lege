//! The main encoding manager and public API.

// ==============================================================================
// JP2 DYNAMIC SIZE TARGETING CONFIGURATION
// Place this at the top of streamline.rs for easy adjustment
// ==============================================================================

/// JP2 compression configuration based on empirical testing
/// Base target: 30KB for 500px height image (tested on rembrandt2.png)
/// Scales proportionally with image dimensions
pub mod jp2_config {
    use crate::streamline::Jp2Settings;

    /// Base configuration from testing

    // Less aggressive defaults: aim for ~120KB per megapixel (covers a bit higher)
    const BASE_TARGET_KB: f64 = 120.0; // Base target for 1MP image areas
    const COVER_TARGET_KB: f64 = 220.0; // Base target for cover pages (often photo-like)

    // Area reference in pixels (1 megapixel)
    const AREA_REF_PX: f64 = 1_000_000.0;

    // Minimum per-image floors
    // Modest per-image floors to protect tiny images/regions
    const IMAGE_MIN_KB: f64 = 40.0;
    const COVER_MIN_KB: f64 = 80.0;

    // Guardrails on bytes-per-pixel (BPPx) to avoid extreme compression/expansion
    // These are in bytes per pixel (not bits). Example: 0.10 B/px ≈ 0.8 bpp.
    const IMAGE_BPPX_MIN: f64 = 0.08; // ~0.64 bpp minimum for interior images
    const IMAGE_BPPX_MAX: f64 = 0.30; // cap to avoid huge blowups on very large images
    const COVER_BPPX_MIN: f64 = 0.10; // covers/photos get a bit more minimum bitrate
    const COVER_BPPX_MAX: f64 = 0.40; // allow higher ceiling for covers

    /// Scaling factor for size targets (adjust based on testing)
    /// 1.0 = linear scaling; using 1.0 to avoid over-aggressive downscaling of large images
    const SCALING_EXPONENT: f64 = 1.0;

    // Content-aware bias strength (fractional increase for more textured images)
    const IMAGE_BIAS_BETA: f64 = 0.30;
    const COVER_BIAS_BETA: f64 = 0.25;

    fn base_target_kb(area: f64, is_cover: bool) -> f64 {
        let base = if is_cover {
            COVER_TARGET_KB
        } else {
            BASE_TARGET_KB
        };
        base * (area / AREA_REF_PX).powf(SCALING_EXPONENT)
    }

    // Compute a lightweight texture score in [0,1] from image bytes.
    // 0 = very flat/white page, 1 = highly textured.
    pub fn texture_score(image_data: &[u8], width: u32, height: u32, channels: u8) -> f64 {
        if width == 0 || height == 0 {
            return 0.0;
        }
        let w = width as usize;
        let h = height as usize;
        let ch = channels.max(1) as usize;

        // Subsample to roughly ~100k samples to keep it fast
        let area = (w * h) as f64;
        let target_samples: f64 = 100_000.0;
        let stride = (area / target_samples).sqrt().ceil().max(1.0) as usize;
        let xs = (1..w - 1).step_by(stride);
        let ys = (1..h - 1).step_by(stride);

        let mut sum_grad = 0.0_f64;
        let mut count = 0_u64;
        let mut nonwhite = 0_u64;

        // Helper to read luminance at (x,y)
        let lum = |x: usize, y: usize| -> f64 {
            let idx = (y * w + x) * ch;
            if ch == 1 {
                image_data.get(idx).copied().unwrap_or(255) as f64
            } else {
                let r = *image_data.get(idx).unwrap_or(&255) as f64;
                let g = *image_data.get(idx + 1).unwrap_or(&255) as f64;
                let b = *image_data.get(idx + 2).unwrap_or(&255) as f64;
                0.299 * r + 0.587 * g + 0.114 * b
            }
        };

        for y in ys.clone() {
            for x in xs.clone() {
                let c = lum(x, y);
                let rx = lum(x + 1, y);
                let by = lum(x, y + 1);
                let dx = (rx - c).abs();
                let dy = (by - c).abs();
                let g = dx + dy; // 0..510
                sum_grad += g;
                if c < 245.0 {
                    nonwhite += 1;
                }
                count += 1;
            }
        }

        if count == 0 {
            return 0.0;
        }
        let mean_grad = sum_grad / (count as f64); // 0..510
        let edge_density = (mean_grad / 510.0).clamp(0.0, 1.0);
        let nonwhite_ratio = (nonwhite as f64 / count as f64).clamp(0.0, 1.0);
        // Add entropy calculation for better content detection
        let mut histogram = [0u32; 256];
        // Sample histogram with same stride to keep this cheap
        for y in ys.clone() {
            for x in xs.clone() {
                let l = lum(x, y).round().clamp(0.0, 255.0) as usize;
                histogram[l] += 1;
            }
        }
        let total_samples: f64 = (ys.len() * xs.len()) as f64;
        let mut entropy = 0.0;
        if total_samples > 0.0 {
            for &count in &histogram {
                if count > 0 {
                    let p = count as f64 / total_samples;
                    entropy -= p * p.log2();
                }
            }
        }
        let entropy_norm = (entropy / 8.0).clamp(0.0, 1.0);
        // Blend gradient, non-white ratio, and entropy
        (0.4 * edge_density + 0.3 * nonwhite_ratio + 0.3 * entropy_norm).clamp(0.0, 1.0)
    }

    /// Calculate target size in bytes for a given image dimension
    /// Uses proportional scaling from the base configuration
    pub fn calculate_jp2_target_bytes(height: u32, width: u32, is_cover: bool) -> usize {
        let area = (height as f64) * (width as f64);

        // Base curve
        let base_kb = base_target_kb(area, is_cover);

        // Dynamic floors: smaller floors for small areas (e.g., overlays), higher for large pages
        let min_kb_floor = if is_cover {
            if area < 250_000.0 {
                12.0
            } else if area < 1_000_000.0 {
                24.0
            } else {
                COVER_MIN_KB
            }
        } else {
            if area < 100_000.0 {
                4.0
            } else if area < 1_000_000.0 {
                10.0
            } else {
                IMAGE_MIN_KB
            }
        };
        let floored_kb = base_kb.max(min_kb_floor);

        // Guardrails in bytes-per-pixel
        let (bppx_min, bppx_max) = if is_cover {
            (COVER_BPPX_MIN, COVER_BPPX_MAX)
        } else {
            (IMAGE_BPPX_MIN, IMAGE_BPPX_MAX)
        };
        let min_kb_by_bppx = (bppx_min * area) / 1024.0;
        let max_kb_by_bppx = (bppx_max * area) / 1024.0;
        let clamped_kb = floored_kb.clamp(min_kb_by_bppx, max_kb_by_bppx);

        (clamped_kb * 1024.0) as usize
    }

    /// Content-aware target size using a lightweight texture score. Biases up
    /// low-texture (mostly white/flat) images to reduce visible artifacts at
    /// low target sizes. Guardrails are applied after biasing.
    pub fn calculate_jp2_target_bytes_content_aware(
        image_data: &[u8],
        width: u32,
        height: u32,
        channels: u8,
        is_cover: bool,
    ) -> usize {
        let area = (height as f64) * (width as f64);
        let mut kb = base_target_kb(area, is_cover);

        // Bias: increase size for higher texture/entropy images to preserve detail
        // t in [0,1]; this applies up to +beta increase at t=1 and no change at t=0
        let t = texture_score(image_data, width, height, channels); // 0..1
        let beta = if is_cover {
            COVER_BIAS_BETA
        } else {
            IMAGE_BIAS_BETA
        };
        kb *= 1.0 + beta * t;

        // Apply dynamic floors and bppx guardrails
        let min_kb_floor = if is_cover {
            if area < 250_000.0 {
                12.0
            } else if area < 1_000_000.0 {
                24.0
            } else {
                COVER_MIN_KB
            }
        } else {
            if area < 100_000.0 {
                4.0
            } else if area < 1_000_000.0 {
                10.0
            } else {
                IMAGE_MIN_KB
            }
        };
        let kb = kb.max(min_kb_floor);
        let (bppx_min, bppx_max) = if is_cover {
            (COVER_BPPX_MIN, COVER_BPPX_MAX)
        } else {
            (IMAGE_BPPX_MIN, IMAGE_BPPX_MAX)
        };
        let min_kb_by_bppx = (bppx_min * area) / 1024.0;
        let max_kb_by_bppx = (bppx_max * area) / 1024.0;
        let clamped_kb = kb.clamp(min_kb_by_bppx, max_kb_by_bppx);
        (clamped_kb * 1024.0) as usize
    }

    /// Calculate JP2 rate parameter from target bytes and original size
    /// The rate parameter is approximately original_size / compressed_size
    pub fn calculate_jp2_rate(original_bytes: usize, target_bytes: usize) -> f32 {
        let rate = (original_bytes as f64 / target_bytes as f64) as f32;
        // Clamp to reasonable bounds for stability
        rate.clamp(2.0, 100.0)
    }

    /// Get optimized JP2 settings for the target compression
    /// Based on testing with 30KB target at 500px height
    pub fn get_jp2_settings_for_target(
        image_data: &[u8],
        width: u32,
        height: u32,
        channels: u8,
        is_cover: bool,
    ) -> Jp2Settings {
        let original_bytes = image_data.len();
        let target_bytes =
            calculate_jp2_target_bytes_content_aware(image_data, width, height, channels, is_cover);
        let rate = calculate_jp2_rate(original_bytes, target_bytes);
        // Adaptive codeblock sizing helper
        fn adaptive_codeblock_size(width: u32, height: u32) -> Option<(i32, i32)> {
            let area = (width as u64) * (height as u64);
            if area < 250_000 {
                Some((32, 32))
            } else if area > 4_000_000 {
                Some((64, 64))
            } else {
                None
            }
        }

        Jp2Settings {
            num_resolutions: Some(6), // Optimal for multi-resolution
            prog_order: None,         // Use default LRCP
            rate: Some(rate),         // Dynamic rate based on target
            rates: None,              // Single layer for simplicity
            psnrs: None,              // Rate-based, not quality-based
            irreversible: Some(true), // Lossy for better compression
            tile_size: None,          // No tiling for small images
            codeblock: adaptive_codeblock_size(width, height),
        }
    }

    /// Enhanced function that also logs parameters when debug logging is enabled
    pub fn get_jp2_settings_with_logging(
        image_data: &[u8],
        width: u32,
        height: u32,
        channels: u8,
        is_cover: bool,
    ) -> Jp2Settings {
        let settings = get_jp2_settings_for_target(image_data, width, height, channels, is_cover);

        #[cfg(feature = "debug-logging")]
        {
            let target_bytes = calculate_jp2_target_bytes(height, width, is_cover);
            crate::streamline::log_debug_message(&format!(
                "JP2 settings created: {}x{} -> target: {}KB, rate: {:.1}, num_resolutions: {:?}, irreversible: {:?}, codeblock: {:?}",
                width,
                height,
                target_bytes / 1024,
                settings.rate.unwrap_or(0.0),
                settings.num_resolutions,
                settings.irreversible,
                settings.codeblock
            ));
        }

        settings
    }
}

use std::error::Error;

#[cfg(feature = "debug-logging")]
use std::collections::VecDeque;
#[cfg(feature = "debug-logging")]
use std::sync::{Arc, Mutex};
#[cfg(feature = "debug-logging")]
use std::time::SystemTime;

use crate::encoders;

#[cfg(feature = "debug-logging")]
static DEBUG_LOG_BUFFER: std::sync::OnceLock<Arc<Mutex<VecDeque<String>>>> =
    std::sync::OnceLock::new();

#[cfg(feature = "debug-logging")]
const MAX_DEBUG_LOG_ENTRIES: usize = 1000;

#[cfg(feature = "debug-logging")]
pub fn log_debug_message(message: &str) {
    let buffer = DEBUG_LOG_BUFFER.get_or_init(|| Arc::new(Mutex::new(VecDeque::new())));

    if let Ok(mut log) = buffer.lock() {
        // Format time using SystemTime
        let timestamp = match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
            Ok(duration) => {
                let total_secs = duration.as_secs();
                let hours = (total_secs / 3600) % 24;
                let minutes = (total_secs / 60) % 60;
                let seconds = total_secs % 60;
                format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
            }
            Err(_) => "00:00:00".to_string(),
        };
        let formatted_message = format!("[{}] {}", timestamp, message);

        log.push_back(formatted_message);

        // Keep buffer size manageable
        if log.len() > MAX_DEBUG_LOG_ENTRIES {
            log.pop_front();
        }
    }
}

#[cfg(feature = "debug-logging")]
pub fn get_debug_log_messages() -> Vec<String> {
    let buffer = DEBUG_LOG_BUFFER.get_or_init(|| Arc::new(Mutex::new(VecDeque::new())));

    if let Ok(log) = buffer.lock() {
        log.iter().cloned().collect()
    } else {
        Vec::new()
    }
}

#[cfg(feature = "debug-logging")]
pub fn clear_debug_log() {
    let buffer = DEBUG_LOG_BUFFER.get_or_init(|| Arc::new(Mutex::new(VecDeque::new())));

    if let Ok(mut log) = buffer.lock() {
        log.clear();
    }
}

#[cfg(not(feature = "debug-logging"))]
pub fn log_debug_message(_message: &str) {}

#[cfg(not(feature = "debug-logging"))]
pub fn get_debug_log_messages() -> Vec<String> {
    Vec::new()
}

#[cfg(not(feature = "debug-logging"))]
pub fn clear_debug_log() {}

/// Result of encoding that may include global dictionary data for JBIG2
#[derive(Debug, Clone)]
pub enum EncodingResult {
    /// Regular encoded data (JPEG, JP2, CCITT4, etc.)
    Standard(Vec<u8>),
    /// JBIG2 with global dictionary (page data, global data)
    Jbig2WithGlobals {
        page_data: Vec<u8>,
        global_data: Vec<u8>,
    },
}

impl EncodingResult {
    /// Check if the result is empty
    pub fn is_empty(&self) -> bool {
        match self {
            EncodingResult::Standard(data) => data.is_empty(),
            EncodingResult::Jbig2WithGlobals {
                page_data,
                global_data,
            } => page_data.is_empty() && global_data.is_empty(),
        }
    }

    /// Get the main data as a slice (for Standard variant or page_data for JBIG2)
    pub fn as_slice(&self) -> &[u8] {
        match self {
            EncodingResult::Standard(data) => data,
            EncodingResult::Jbig2WithGlobals { page_data, .. } => page_data,
        }
    }

    /// Get the main data as Vec<u8> (for Standard variant or page_data for JBIG2)
    pub fn into_vec(self) -> Vec<u8> {
        match self {
            EncodingResult::Standard(data) => data,
            EncodingResult::Jbig2WithGlobals { page_data, .. } => page_data,
        }
    }
}

/// Represents raw image data for encoding.
pub struct ImageBuffer<'a> {
    /// Raw pixel data.
    pub data: &'a [u8],
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Number of channels (e.g., 1 for grayscale, 3 for RGB, 0 for binary).
    pub channels: u8,
}

#[derive(Debug, Clone)]
/// Settings for JPEG encoding.
pub struct JpegSettings {
    pub quality: u8,
    pub baseline: bool,
    pub optimized: bool,
    pub downsample: bool,
}

#[derive(Debug, Clone, Default)]
/// Stub settings for JP2 (disabled). Kept to satisfy legacy references.
pub struct Jp2Settings {
    /// Number of resolution levels (unused)
    pub num_resolutions: Option<i32>,
    /// Progression order (unused)
    pub prog_order: Option<i32>,
    /// Compression rate (unused)
    pub rate: Option<f32>,
    /// Optional list of compression rates (unused)
    pub rates: Option<Vec<f32>>,
    /// Optional list of target PSNR per layer (unused)
    pub psnrs: Option<Vec<f32>>,
    /// Use irreversible DWT if true (unused)
    pub irreversible: Option<bool>,
    /// Optional tiling (unused)
    pub tile_size: Option<(i32, i32)>,
    /// Optional codeblock size (unused)
    pub codeblock: Option<(i32, i32)>,
}
impl Default for JpegSettings {
    fn default() -> Self {
        Self {
            quality: 50,
            baseline: true,
            optimized: true,
            downsample: true,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Jbig2Mode {
    Generic,
    Symbol,
    SymUnify,
}

impl Jbig2Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Symbol => "symbol",
            Self::SymUnify => "sym-unify",
        }
    }
}

#[derive(Debug, Clone)]
/// Settings for JBIG2 encoding.
pub struct Jbig2Settings {
    /// Use PDF fragment mode (true) or standalone mode (false).
    /// PDF fragment mode creates data suitable for embedding in PDF XObjects.
    /// Standalone mode creates complete JBIG2 files with headers.
    pub pdf_fragment_mode: bool,
    /// Selects the JBIG2 encoding strategy.
    pub mode: Jbig2Mode,
}

impl Default for Jbig2Settings {
    fn default() -> Self {
        Self {
            pdf_fragment_mode: true, // Default to PDF fragment mode for PDF assembly
            mode: Jbig2Mode::Symbol,
        }
    }
}

// Indexed8Settings removed - indexed8 encoder deleted

#[derive(Debug, Clone)]
/// Encoding settings that determine the output format and its parameters.
pub enum EncodingSettings {
    /// JPEG encoding settings.
    Jpeg(JpegSettings),
    /// JBIG2 encoding settings.
    Jbig2(Jbig2Settings),
    /// CCITT4 encoding (no configurable settings).
    Ccitt4,
    // /// Indexed 8-bit paletted image encoding.
    // Indexed8(Indexed8Settings),  // Removed - indexed8 encoder deleted
}

/// Main encoding manager that provides a unified API for all image formats.
pub struct EncodingManager;

impl EncodingManager {
    /// Encode based on provided settings, delegating to format-specific encoders.
    pub fn encode(
        buffer: &ImageBuffer,
        settings: &EncodingSettings,
    ) -> Result<EncodingResult, Box<dyn Error>> {
        #[cfg(feature = "debug-logging")]
        let _start_time = std::time::Instant::now();

        let result = match settings {
            EncodingSettings::Jpeg(s) => {
                let data = encoders::jpeg::encode(
                    buffer.data,
                    buffer.width,
                    buffer.height,
                    buffer.channels,
                    s,
                )
                .map_err(|e| Box::new(e) as Box<dyn Error>)?;
                Ok::<EncodingResult, Box<dyn Error>>(EncodingResult::Standard(data))
            }
            EncodingSettings::Jbig2(s) => {
                let result = encoders::jbig2::encode(
                    buffer.data,
                    buffer.width,
                    buffer.height,
                    buffer.channels,
                    s,
                )
                .map_err(|e| Box::new(e) as Box<dyn Error>)?;

                if let Some(global_data) = result.global_data {
                    Ok::<EncodingResult, Box<dyn Error>>(EncodingResult::Jbig2WithGlobals {
                        page_data: result.page_data,
                        global_data,
                    })
                } else {
                    Ok::<EncodingResult, Box<dyn Error>>(EncodingResult::Standard(result.page_data))
                }
            }
            EncodingSettings::Ccitt4 => {
                let data = encoders::ccitt4::encode(
                    buffer.data,
                    buffer.width,
                    buffer.height,
                    buffer.channels,
                )
                .map_err(|e| Box::new(e) as Box<dyn Error>)?;
                Ok::<EncodingResult, Box<dyn Error>>(EncodingResult::Standard(data))
            }
        }?;

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jpeg_rgb() {
        let pixels = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]; // 2x2 RGB
        let buffer = ImageBuffer {
            data: &pixels,
            width: 2,
            height: 2,
            channels: 3,
        };
        let settings = EncodingSettings::Jpeg(JpegSettings {
            quality: 90,
            baseline: true,
            optimized: true,
            downsample: true,
        });
        let result = EncodingManager::encode(&buffer, &settings);
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(!output.is_empty());
        assert_eq!(&output.as_slice()[0..2], &[0xFF, 0xD8]); // JPEG SOI marker
    }

    #[test]
    fn test_jpeg_grayscale() {
        let pixels = vec![128, 64, 192, 255]; // 2x2 grayscale
        let buffer = ImageBuffer {
            data: &pixels,
            width: 2,
            height: 2,
            channels: 1,
        };
        let settings = EncodingSettings::Jpeg(JpegSettings {
            quality: 90,
            baseline: true,
            optimized: true,
            downsample: true,
        });
        let result = EncodingManager::encode(&buffer, &settings);
        assert!(result.is_ok());
    }

    #[test]
    fn test_jbig2() {
        let pixels = vec![0, 1, 1, 0]; // 2x2 binary, using 0/1 for clarity
        let buffer = ImageBuffer {
            data: &pixels,
            width: 2,
            height: 2,
            channels: 0, // Binary
        };
        let settings = EncodingSettings::Jbig2(Jbig2Settings::default());
        let result = EncodingManager::encode(&buffer, &settings);
        assert!(result.is_ok());
    }
}
