//! Image format definitions and settings structures

/// Supported image formats for encoding
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// JPEG format - lossy compression for photographic images
    Jpeg,
    /// JPEG 2000 (JP2) format - advanced wavelet-based compression
    Jp2,
    /// JBIG2 format - highly efficient compression for bi-level (black/white) images
    Jbig2,
    /// CCITT Group 4 format - fax-standard compression for bi-level images
    Ccitt4,
}

/// JPEG encoding settings
#[derive(Debug, Clone)]
pub struct JpegSettings {
    /// Quality from 1 (worst) to 100 (best)
    pub quality: u8,
    /// Use baseline DCT encoding if true, progressive if false
    pub baseline: bool,
    /// Use optimized Huffman tables
    pub optimized: bool,
    /// Enable 4:2:0 chroma subsampling
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

/// JPEG 2000 encoding settings
#[derive(Debug, Clone)]
pub struct Jp2Settings {
    /// Number of resolution levels (None for default)
    pub num_resolutions: Option<i32>,
    /// Progression order (None for default)
    pub prog_order: Option<i32>,
    /// Compression rate (e.g., 10.0 for 10:1)
    pub rate: Option<f32>,
    /// Use irreversible DWT if true
    pub irreversible: Option<bool>,
}

impl Default for Jp2Settings {
    fn default() -> Self {
        Self {
            num_resolutions: None,
            prog_order: None,
            rate: Some(3.0), // 33% quality compression ratio for pages (e-ink optimized)
            irreversible: Some(true),
        }
    }
}

/// JBIG2 encoding settings
#[derive(Debug, Clone, Default)]
pub struct Jbig2Settings {
    /// Enable symbol mode for text-like images
    pub symbol_mode: bool,
    /// Enable refinement coding
    pub refine: bool,
    /// Remove duplicate lines
    pub duplicate_line_removal: bool,
    /// Auto threshold detection
    pub auto_thresh: bool,
    /// Use hash-based symbol matching
    pub use_hash: bool,
}

/// CCITT4 encoding settings
#[derive(Debug, Clone, Default)]
pub struct Ccitt4Settings {
    /// Whether to use end-of-line markers
    pub end_of_line: bool,
    /// Whether to use end-of-block markers
    pub end_of_block: bool,
}

/// Format-specific encoding settings
#[derive(Debug, Clone)]
pub enum EncodingSettings {
    /// JPEG settings
    Jpeg(JpegSettings),
    /// JPEG 2000 settings
    Jp2(Jp2Settings),
    /// JBIG2 settings
    Jbig2(Jbig2Settings),
    /// CCITT4 settings
    Ccitt4(Ccitt4Settings),
}

impl Default for EncodingSettings {
    fn default() -> Self {
        EncodingSettings::Jpeg(JpegSettings::default())
    }
}

impl EncodingSettings {
    /// Create default settings for a specific format
    pub fn for_format(format: ImageFormat) -> Self {
        match format {
            ImageFormat::Jpeg => EncodingSettings::Jpeg(JpegSettings::default()),
            ImageFormat::Jp2 => EncodingSettings::Jp2(Jp2Settings::default()),
            ImageFormat::Jbig2 => EncodingSettings::Jbig2(Jbig2Settings::default()),
            ImageFormat::Ccitt4 => EncodingSettings::Ccitt4(Ccitt4Settings::default()),
        }
    }
}
