//! The main encoding manager and public API.

#[cfg(feature = "jp2-lam")]
pub use crate::encoding::jp2::{Jp2Settings, jp2_config};

use std::error::Error;

#[cfg(feature = "debug-logging")]
use std::collections::VecDeque;
#[cfg(feature = "debug-logging")]
use std::sync::{Arc, Mutex};
#[cfg(feature = "debug-logging")]
use std::time::SystemTime;

use crate::encoding::encoders;

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
    /// When true, encode the bilevel page with `jbig2enc_rust::jbig2halftone` (pattern dictionary +
    /// halftone region per ISO/JBIG2). Used when pipeline `ImageRegionDitherMode::Halftone` is set
    /// for JBIG2 text output; symbol/generic modes are not used in this path.
    pub use_jbig2_halftone_segments: bool,
}

impl Default for Jbig2Settings {
    fn default() -> Self {
        Self {
            pdf_fragment_mode: true, // Default to PDF fragment mode for PDF assembly
            mode: Jbig2Mode::Symbol,
            use_jbig2_halftone_segments: false,
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
    /// JPEG 2000 via jp2lam (pure Rust). Channels inferred from `ImageBuffer.channels`:
    /// 1 → grayscale JP2, 3 → RGB JP2.
    Jp2Lam {
        /// Quality 0–100; 100 = lossless.
        quality: u8,
    },
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
            EncodingSettings::Jp2Lam { quality: _quality } => {
                #[cfg(feature = "jp2-lam")]
                {
                    let data = if buffer.channels == 1 {
                        encoders::jp2::encode_gray(
                            buffer.data,
                            buffer.width,
                            buffer.height,
                            *_quality,
                        )
                    } else {
                        encoders::jp2::encode_rgb(
                            buffer.data,
                            buffer.width,
                            buffer.height,
                            *_quality,
                        )
                    }
                    .map_err(|e| Box::new(e) as Box<dyn Error>)?;
                    Ok::<EncodingResult, Box<dyn Error>>(EncodingResult::Standard(data))
                }
                #[cfg(not(feature = "jp2-lam"))]
                Err(Box::new(crate::encoding::EncodingError::EncoderError(
                    "jp2-lam feature not enabled; rebuild with --features jp2-lam".to_string(),
                )) as Box<dyn Error>)
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

/// Encode grayscale image data as a JBIG2 halftone region (for PDF embedding).
///
/// Returns `(global_data, page_data)` — the caller stores `global_data` as a
/// JBIG2 Globals stream and `page_data` as the XObject stream, matching
/// the existing split pattern used by Symbol/Generic JBIG2 encoding.
pub fn encode_halftone_region_grayscale(
    grayscale: &[u8],
    width: u32,
    height: u32,
) -> std::result::Result<(Vec<u8>, Vec<u8>), Box<dyn std::error::Error>> {
    use jbig2enc_rust::Jbig2Config;
    use jbig2enc_rust::jbig2halftone::encode_halftone_pdf_split_auto_from_grayscale;
    use jbig2enc_rust::jbig2structs::GenericRegionParams;

    let mut jcfg = Jbig2Config::default();
    jcfg.want_full_headers = false;
    let region_params = GenericRegionParams::new(width, height, jcfg.dpi);

    let (global_data, page_data) = encode_halftone_pdf_split_auto_from_grayscale(
        grayscale,
        width,
        height,
        &jcfg,
        &region_params,
        1,
        Some(1),
    )?;

    Ok((global_data, page_data))
}
