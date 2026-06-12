//! Shared types and configuration for lege and lege-ffi
use crate::text_loader::CLI_TEXT;
use Legencode::types::BinarizationConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
// Needed for error macros used below
use crate::engine::Detection;
use anyhow::anyhow;

/// Broad content category assigned to every layout detection.
///
/// Broad content category assigned to every YOLO layout detection.
/// All downstream decisions (dithering, masking, OCR region selection,
/// JBIG2 encoding mode) are driven by this enum — never by raw class IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentCategory {
    /// Textual content: titles, paragraphs, captions, formulas, references, …
    Text,
    /// Photographic / illustrative content: figures.
    Image,
    /// Tabular content.  Treated as text-like for binarization (no dithering).
    Table,
    /// Noise / artifact regions (YOLO "abandon").
    /// Treated as text-like but forces JBIG2 Generic encoding on the page
    /// to avoid Symbol-mode corruption of noisy pixels.
    Abandon,
}

impl ContentCategory {
    /// Is this an image region that should be dithered?
    pub fn is_image(&self) -> bool {
        matches!(self, Self::Image)
    }

    /// Is this a text-like region (binarize normally, no dithering)?
    pub fn is_text_like(&self) -> bool {
        !self.is_image()
    }

    /// Should the presence of this category on a page force
    /// JBIG2 Generic encoding for the base layer?
    pub fn force_generic_jbig2(&self) -> bool {
        matches!(self, Self::Abandon)
    }
}

/// Legacy alias kept for any remaining references.
pub type LabelCategory = ContentCategory;

/// The 10-class DocLayout-YOLO (DocStructBench) label set — the only layout
/// model this program ships. This table is the **single source of truth** for
/// the layout class space: every class name and every `ContentCategory` derives
/// from here via `class_name_for` / `category_for_class`. Index == YOLO class id.
pub const LAYOUT_CLASSES: &[(&str, ContentCategory)] = &[
    ("title", ContentCategory::Text),           // 0
    ("plain_text", ContentCategory::Text),      // 1
    ("abandon", ContentCategory::Abandon),      // 2
    ("figure", ContentCategory::Image),         // 3
    ("figure_caption", ContentCategory::Text),  // 4
    ("table", ContentCategory::Table),          // 5
    ("table_caption", ContentCategory::Text),   // 6
    ("table_footnote", ContentCategory::Text),  // 7
    ("isolate_formula", ContentCategory::Text), // 8
    ("formula_caption", ContentCategory::Text), // 9
];

/// Class name for a YOLO class id (`"unknown"` if out of range).
pub fn class_name_for(class_id: i32) -> &'static str {
    usize::try_from(class_id)
        .ok()
        .and_then(|i| LAYOUT_CLASSES.get(i))
        .map(|(name, _)| *name)
        .unwrap_or("unknown")
}

/// YOLO class id for a canonical DocLayout label.
pub fn class_id_for(class_name: &str) -> Option<i32> {
    LAYOUT_CLASSES
        .iter()
        .position(|(name, _)| *name == class_name)
        .and_then(|i| i32::try_from(i).ok())
}

/// `ContentCategory` for a YOLO class id (`Text` if out of range — the safe
/// default, since unknown regions are binarized normally rather than masked).
pub fn category_for_class(class_id: i32) -> ContentCategory {
    usize::try_from(class_id)
        .ok()
        .and_then(|i| LAYOUT_CLASSES.get(i))
        .map(|(_, cat)| *cat)
        .unwrap_or(ContentCategory::Text)
}

/// True when `class_id` is one of the shipped DocLayout-YOLO classes.
pub fn is_known_layout_class(class_id: i32) -> bool {
    usize::try_from(class_id)
        .ok()
        .is_some_and(|i| i < LAYOUT_CLASSES.len())
}

/// Label name attached to a detection, falling back to the canonical class-id
/// mapping for old/synthetic callers that do not populate `class_name`.
pub fn detection_label(detection: &Detection) -> &str {
    detection
        .class_name
        .as_deref()
        .unwrap_or_else(|| class_name_for(detection.class_id))
}

/// Match a detection against a canonical DocLayout label. Known class ids win
/// over any stale display name carried by older test fixtures or callers.
pub fn detection_is_class(detection: &Detection, class_name: &str) -> bool {
    if is_known_layout_class(detection.class_id) {
        class_name_for(detection.class_id) == class_name
    } else {
        detection_label(detection) == class_name
    }
}

/// Type of content in a document region (used by `Region` struct)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RegionType {
    Text,
    Image,
    Table,
    Figure,
    Other,
}

/// Standardized label classifier that provides consistent label categorization
/// across all modules (engine, margin, OCR, color_processing).
///
/// All decisions are driven by `ContentCategory` (set from `LAYOUT_CLASSES`),
/// never by raw class-id tables — that is the single source of truth.
pub struct LabelClassifier;

impl LabelClassifier {
    pub fn new() -> Self {
        Self
    }

    /// Classify a detection for margin calculation purposes
    /// - For StandardizeAndCenter: includes all content
    /// - For CropAndResize: excludes margin clutter (Abandon: page numbers,
    ///   headers/footers, scan noise)
    pub fn is_margin_calc_label(&self, detection: &Detection, for_cropping: bool) -> bool {
        if for_cropping {
            // Exclude margin clutter (Abandon) when cropping.
            !matches!(detection.category, ContentCategory::Abandon)
        } else {
            // Include all detected content for centering
            true
        }
    }

    /// Classify a detection as text-like (suitable for OCR)
    pub fn is_text_label(&self, detection: &Detection) -> bool {
        detection.category.is_text_like()
    }

    /// Classify a detection as image-like (suitable for image processing/dithering)
    pub fn is_image_label(&self, detection: &Detection) -> bool {
        detection.category.is_image()
    }

    /// True when a detection is substantive body text that should block
    /// full-page image expansion (paragraphs, tables, formulas, code, etc.).
    /// Returns false for minor peripheral content: titles, captions,
    /// table footnotes, and abandon regions.
    pub fn is_substantive_text(&self, detection: &Detection) -> bool {
        if detection.category.is_image() || matches!(detection.category, ContentCategory::Abandon) {
            return false;
        }
        ![
            "title",
            "figure_caption",
            "table_caption",
            "formula_caption",
            "table_footnote",
            "abandon",
        ]
        .iter()
        .any(|name| detection_is_class(detection, name))
    }

    /// True for the DocLayout class that represents table footnotes.
    pub fn is_footnote_label(&self, detection: &Detection) -> bool {
        detection_is_class(detection, "table_footnote")
    }

    /// True for old furniture labels that may still appear from non-DocLayout
    /// callers. The shipped 10-class model uses `abandon` for clutter; this
    /// method deliberately does not include substantive canonical labels.
    pub fn is_page_furniture_label(&self, detection: &Detection) -> bool {
        matches!(
            detection_label(detection),
            "header" | "footer" | "page" | "page_number" | "number" | "seal" | "stamp"
        )
    }

    /// Get the primary category for a detection
    pub fn get_category(&self, detection: &Detection) -> ContentCategory {
        detection.category
    }

    /// Check if detection should be included in margin calculation
    pub fn should_include_in_margin_calc(&self, detection: &Detection, for_cropping: bool) -> bool {
        self.is_margin_calc_label(detection, for_cropping)
    }

    /// Check if detection should be processed with OCR
    pub fn should_process_with_ocr(&self, detection: &Detection) -> bool {
        detection.category.is_text_like()
    }

    /// Check if detection should be dithered (image processing)
    pub fn should_dither(&self, detection: &Detection) -> bool {
        detection.category.is_image()
    }
}

impl Default for LabelClassifier {
    fn default() -> Self {
        Self::new()
    }
}

/// A region of interest in a document page
#[derive(Debug, Clone)]
pub struct Region {
    /// Unique identifier for the region
    pub id: u32,
    /// Bounding box [x1, y1, x2, y2] in page coordinates
    pub bbox: [f32; 4],
    /// Confidence score (0.0 to 1.0)
    pub confidence: f32,
    /// Extracted text (if any)
    pub text: Option<String>,
    /// Type of region
    pub region_type: RegionType,
    /// Raw binary data (if applicable)
    pub data: Vec<u8>,
}

// Config file loading types
use config::{Config, File, FileFormat};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CoverFormat {
    Jpeg,
    Jp2,
    Ccitt4,
    Jbig2,
    None,
}

impl CoverFormat {
    pub fn is_binary_format(&self) -> bool {
        matches!(self, CoverFormat::Ccitt4 | CoverFormat::Jbig2)
    }
}

// NOTE: Local BinarizationOptions removed. Use `Legencode::types::BinarizationOptions` everywhere to avoid duplication.

/// User configuration loaded from TOML files
#[derive(Deserialize, Serialize, Default, Clone)]
pub struct AppConfig {
    /// Default output directory
    pub default_output: Option<PathBuf>,

    /// Default text compression format
    pub default_text_format: Option<String>,

    /// Default cover format
    pub default_cover_format: Option<String>,

    /// Default target height
    pub default_height: Option<u32>,

    /// Enable OCR by default
    pub enable_ocr: Option<bool>,

    /// Use the slower line-segmented OCR pipeline by default
    pub slow_ocr: Option<bool>,

    /// Default binarization method
    pub binarization: Option<String>,

    /// Keep color images by default
    pub keep_color_images: Option<bool>,

    /// Disable layout detection by default
    pub disable_layout: Option<bool>,

    /// Default confidence threshold for detection
    pub confidence_threshold: Option<f32>,

    /// Default NMS threshold
    pub nms_threshold: Option<f32>,
}

impl AppConfig {
    /// Load configuration from file(s)
    pub fn load(config_path: Option<PathBuf>) -> Result<Self, anyhow::Error> {
        let mut builder = Config::builder();

        // Load default config from known locations
        if let Some(config_dir) = dirs::config_dir() {
            let default_path = config_dir.join("lege").join("config.toml");
            if default_path.exists() {
                builder = builder.add_source(File::from(default_path).format(FileFormat::Toml));
            }
        }

        // Also check for config in current directory
        let local_config = PathBuf::from("lege.toml");
        if local_config.exists() {
            builder = builder.add_source(File::from(local_config).format(FileFormat::Toml));
        }

        // Load user-specified config (highest priority)
        if let Some(path) = config_path {
            if path.exists() {
                builder = builder.add_source(File::from(path).format(FileFormat::Toml));
            } else {
                return Err(anyhow::anyhow!(
                    "Configuration file not found: {}",
                    path.display()
                ));
            }
        }

        let config = builder.build()?;
        Ok(config.try_deserialize().unwrap_or_default())
    }

    /// Save configuration to file
    pub fn save(&self, path: &PathBuf) -> Result<(), anyhow::Error> {
        // Create parent directory if it doesn't exist
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let toml_string = toml::to_string_pretty(self)?;
        std::fs::write(path, toml_string)?;
        Ok(())
    }

    /// Get default config file path
    pub fn default_config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("lege").join("config.toml"))
    }

    /// Create a sample configuration file with optimal defaults
    pub fn create_sample_config() -> Self {
        AppConfig {
            default_output: Some(PathBuf::from("./output")),
            default_text_format: Some("ccitt4".to_string()), // Fast, compatible Group 4 fax compression
            default_cover_format: Some("jpeg".to_string()),  // Fast, compatible JPEG for images
            default_height: Some(1200), // Balanced quality/size for most documents
            enable_ocr: Some(true),     // Enable OCR by default if available
            slow_ocr: Some(false),      // Keep the existing fast OCR path by default
            binarization: Some("adaptive".to_string()), // Good for varied lighting conditions
            keep_color_images: Some(false), // Dither by default for smaller files
            disable_layout: Some(true), // Disable layout detection for faster processing
            confidence_threshold: Some(0.2), // Match PipelineConfig default
            nms_threshold: Some(0.7),   // Match PipelineConfig default
        }
    }

    /// Apply config values to pipeline config, with CLI args taking precedence
    pub fn apply_to_pipeline_config(
        &self,
        mut pipeline_config: crate::PipelineConfig,
    ) -> crate::PipelineConfig {
        if let Some(height) = self.default_height {
            if pipeline_config.target_height == 1200 {
                // Default value
                pipeline_config.target_height = height;
                pipeline_config.target_width = None;
            }
        }

        if let Some(ref format) = self.default_cover_format {
            match format.as_ref() {
                "jpeg" => pipeline_config.cover_format = CoverFormat::Jpeg,
                "jp2" => pipeline_config.cover_format = CoverFormat::Jp2,
                "none" => pipeline_config.cover_format = CoverFormat::None,
                _ => {}
            }
        }

        if let Some(ref text_format) = self.default_text_format {
            pipeline_config.text_format = text_format.clone();
        }

        if let Some(ocr) = self.enable_ocr {
            pipeline_config.enable_ocr = ocr;
        }

        if let Some(slow_ocr) = self.slow_ocr {
            pipeline_config.set_slow_ocr(slow_ocr);
        }

        if let Some(keep_color) = self.keep_color_images {
            pipeline_config.set_dither_images(!keep_color);
        }

        if let Some(disable_layout) = self.disable_layout {
            pipeline_config.enable_layout_detection = !disable_layout;
        }

        if let Some(confidence) = self.confidence_threshold {
            pipeline_config.confidence_threshold = confidence;
        }

        if let Some(nms) = self.nms_threshold {
            pipeline_config.nms_threshold = nms;
        }

        pipeline_config
    }
}

/// Shared CLI configuration builder to eliminate redundancy across cli.rs, interactive.rs, and main.rs
#[derive(Debug, Clone, Default)]
pub struct CliConfigBuilder {
    // Format options
    pub text_format: Option<String>,
    pub cover_format: Option<CoverFormat>,
    pub enable_dithering: bool,
    pub enable_cover: bool,

    // Processing options
    pub enable_layout_detection: bool,
    pub enable_ocr: bool,
    pub target_height: Option<u32>,

    // Binarization
    pub binarization_method: Option<String>,

    // Page range
    pub page_range: Option<String>,

    // Cover page options
    pub enable_cover_page: Option<bool>,
    pub no_cover_page: bool,
}

impl CliConfigBuilder {
    pub fn new() -> Self {
        Self {
            enable_layout_detection: true, // Default to enabled
            enable_cover_page: Some(true), // Default to enabled
            no_cover_page: false,          // Default to disabled
            ..Default::default()
        }
    }

    /// Parse format options from CLI format strings
    pub fn with_format_options(mut self, format_options: &[String]) -> Self {
        for option in format_options {
            match option.as_ref() {
                "1" => self.text_format = Some("ccitt4".to_string()),
                "2" => self.text_format = Some("jbig2".to_string()),
                "d" => self.enable_dithering = true,
                "jpeg" => self.cover_format = Some(CoverFormat::Jpeg),
                "jp2" => self.cover_format = Some(CoverFormat::Jp2),
                "none" | "nocover" => self.cover_format = Some(CoverFormat::None),
                _ => {} // Ignore unknown options
            }
        }
        self
    }

    /// Set layout detection (with automatic dithering dependency)
    pub fn with_layout_detection(mut self, enable: bool) -> Self {
        self.enable_layout_detection = enable;
        // If layout detection is disabled, automatically disable dithering
        // since dithering depends on layout detection to identify image regions
        if !enable {
            self.enable_dithering = false;
        }
        self
    }

    /// Set OCR option
    pub fn with_ocr(mut self, enable: bool) -> Self {
        self.enable_ocr = enable;
        self
    }

    /// Set target height
    pub fn with_height(mut self, height: u32) -> Self {
        self.target_height = Some(height);
        self
    }

    /// Set binarization method
    pub fn with_binarization(mut self, method: String) -> Self {
        self.binarization_method = Some(method);
        self
    }

    /// Set page range
    pub fn with_page_range(mut self, range: String) -> Self {
        self.page_range = Some(range);
        self
    }

    /// Set cover format directly
    pub fn with_cover_format(mut self, format: CoverFormat) -> Self {
        self.cover_format = Some(format);
        // Enable cover when setting a cover format (unless explicitly set to None)
        self.enable_cover = !matches!(format, CoverFormat::None);
        self
    }

    /// Set text format directly
    pub fn with_text_format(mut self, format: String) -> Self {
        self.text_format = Some(format);
        self
    }

    /// Set dithering (respects layout detection dependency)
    pub fn with_cover(mut self, enable: bool) -> Self {
        self.enable_cover = enable;
        self
    }

    pub fn with_dithering(mut self, enable: bool) -> Self {
        // Only allow enabling dithering if layout detection is enabled
        if enable && !self.enable_layout_detection {
            // Don't enable dithering if layout detection is disabled
            self.enable_dithering = false;
        } else {
            self.enable_dithering = enable;
        }
        self
    }

    /// Set cover page option
    pub fn with_cover_page(mut self, enable: bool) -> Self {
        self.enable_cover_page = Some(enable);
        self
    }

    /// Set no-cover-page option (forces uniform processing)
    pub fn with_no_cover_page(mut self, enable: bool) -> Self {
        self.no_cover_page = enable;
        if enable {
            // If no_cover_page is enabled, disable cover page processing
            self.enable_cover_page = Some(false);
        }
        self
    }

    /// Set no binarization option
    pub fn with_no_binarization(mut self, no_binarization: bool) -> Self {
        self.binarization_method = if no_binarization {
            None
        } else {
            self.binarization_method
        };
        self
    }

    /// Build a PipelineConfig from the current configuration
    pub fn build_pipeline_config(
        self,
        base_config: crate::PipelineConfig,
    ) -> crate::PipelineConfig {
        let mut config = base_config;

        // Apply text format
        if let Some(text_format) = self.text_format {
            config.text_format = text_format;
        }

        // Apply cover format
        if !self.enable_cover {
            config.cover_format = CoverFormat::None;
        } else if let Some(cover_format) = self.cover_format {
            config.cover_format = cover_format;
        }

        // Apply processing options
        config.set_dither_images(self.enable_dithering);
        config.enable_layout_detection = self.enable_layout_detection;
        config.enable_ocr = self.enable_ocr;

        // Apply height
        if let Some(height) = self.target_height {
            config.target_height = height;
            config.target_width = None;
        }

        // Apply binarization
        if let Some(method) = self.binarization_method {
            config.binarization = Self::parse_binarization_method(&method);
        }

        // Apply page range
        if let Some(range_str) = self.page_range {
            config.page_range = Self::parse_page_range(&range_str).ok().flatten();
        }

        // Apply cover page options
        if let Some(enable_cover) = self.enable_cover_page {
            config.enable_cover_page = enable_cover;
        }
        config.no_cover_page = self.no_cover_page;

        config
    }

    /// Parse binarization method string into BinarizationConfig
    pub fn parse_binarization_method(method: &str) -> BinarizationConfig {
        let trimmed = method.trim();
        if trimmed.is_empty() {
            return BinarizationConfig::default();
        }

        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        let mut config = BinarizationConfig::default();
        let mut fixed_selected = false;

        if let Some(choice) = parts.first() {
            match choice.to_lowercase().as_str() {
                "1" | "adaptive" | "sauvola" | "otsu" => {
                    config.use_heavy_duty = false;
                    config.use_fixed_threshold = false;
                }
                "2" | "fixed" | "threshold" | "thr" => {
                    config.use_heavy_duty = false;
                    config.use_fixed_threshold = true;
                    fixed_selected = true;
                }
                "3" | "heavy" | "sauvola_ai" | "onnx" => {
                    config.use_heavy_duty = true;
                    config.use_fixed_threshold = false;
                }
                _ => {}
            }
        }

        for part in parts.iter().skip(1) {
            if let Some(k_str) = part.strip_prefix("k=") {
                if let Ok(k) = k_str.parse::<f32>() {
                    config.k_factor = k;
                    if !fixed_selected {
                        config.use_fixed_threshold = false;
                    }
                }
            } else if let Some(thr_str) = part.strip_prefix("thr=") {
                if let Ok(threshold) = thr_str.parse::<u8>() {
                    config.fixed_threshold = threshold;
                    config.use_fixed_threshold = true;
                    config.use_heavy_duty = false;
                    fixed_selected = true;
                }
            } else if fixed_selected {
                if let Ok(threshold) = part.parse::<u8>() {
                    config.fixed_threshold = threshold;
                    config.use_fixed_threshold = true;
                    config.use_heavy_duty = false;
                }
            }
        }

        config
    }

    /// Parse page range string into PageRange
    pub fn parse_page_range(range_str: &str) -> Result<Option<crate::PageRange>, anyhow::Error> {
        // Handle simple ranges like "30-50" or single pages like "5"
        if range_str.contains('-') {
            let parts: Vec<&str> = range_str.split('-').collect();
            if parts.len() == 2 {
                let start: usize = parts[0].trim().parse().map_err(|_| {
                    anyhow!(CLI_TEXT.errors.invalid_start_page.replace("{}", parts[0]))
                })?;
                let end: usize = parts[1].trim().parse().map_err(|_| {
                    anyhow!(CLI_TEXT.errors.invalid_end_page.replace("{}", parts[1]))
                })?;
                return Ok(Some(crate::PageRange::new(start, end)?));
            }
        } else if let Ok(single_page) = range_str.trim().parse::<usize>() {
            // Single page becomes a range of one page
            return Ok(Some(crate::PageRange::new(single_page, single_page)?));
        }

        // For more complex ranges like "1,3,5-8", return None for now
        // This is a limitation but covers the main use case
        Ok(None)
    }

    /// Validate format options
    pub fn validate_format_options(format_options: &[String]) -> Result<(), anyhow::Error> {
        for option in format_options {
            match option.as_ref() {
                "1" | "2" | "d" | "jpeg" | "jp2" | "none" | "nocover" => {}
                _ => {
                    return Err(anyhow!(
                        CLI_TEXT.errors.invalid_format_option.replace("{}", option)
                    ));
                }
            }
        }
        Ok(())
    }

    /// Validate binarization method
    pub fn validate_binarization_method(method: &str) -> Result<(), anyhow::Error> {
        let trimmed = method.trim();
        if trimmed.is_empty() {
            return Ok(());
        }

        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        let choice = parts
            .first()
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let is_fixed_choice = matches!(choice.as_str(), "2" | "fixed" | "threshold" | "thr");
        let valid_choice = matches!(
            choice.as_str(),
            "1" | "adaptive"
                | "sauvola"
                | "otsu"
                | "2"
                | "fixed"
                | "threshold"
                | "thr"
                | "3"
                | "heavy"
                | "sauvola_ai"
                | "onnx"
        );
        if !valid_choice {
            return Err(anyhow!(
                CLI_TEXT
                    .errors
                    .invalid_binarization_method
                    .replace("{}", method)
            ));
        }

        let mut positional_threshold_seen = false;
        for part in parts.iter().skip(1) {
            if part.starts_with("k=") {
                let raw = &part[2..];
                let val = raw
                    .parse::<f32>()
                    .map_err(|_| anyhow!("Invalid k value: {}", raw))?;
                if !(0.0..=1.0).contains(&val) {
                    return Err(anyhow!("k must be between 0.0 and 1.0"));
                }
            } else if part.starts_with("thr=") {
                let raw = &part[4..];
                raw.parse::<u8>()
                    .map_err(|_| anyhow!("Invalid threshold value: {}", raw))?;
                positional_threshold_seen = true;
            } else if is_fixed_choice && !positional_threshold_seen {
                part.parse::<u8>()
                    .map_err(|_| anyhow!("Invalid fixed threshold value: {}", part))?;
                positional_threshold_seen = true;
            } else {
                return Err(anyhow!(
                    CLI_TEXT
                        .errors
                        .invalid_binarization_method
                        .replace("{}", method)
                ));
            }
        }

        Ok(())
    }

    /// Get available binarization methods for interactive selection
    pub fn get_binarization_methods() -> Vec<(String, BinarizationConfig)> {
        vec![
            (
                CLI_TEXT.interactive.binarization_methods[0].clone(),
                BinarizationConfig {
                    k_factor: 0.2,
                    use_heavy_duty: false,
                    use_fixed_threshold: false,
                    ..Default::default()
                },
            ),
            (
                CLI_TEXT.interactive.binarization_methods[1].clone(),
                BinarizationConfig {
                    use_fixed_threshold: true,
                    fixed_threshold: 200,
                    ..Default::default()
                },
            ),
            (
                CLI_TEXT.interactive.binarization_methods[2].clone(),
                BinarizationConfig {
                    k_factor: 0.2,
                    use_heavy_duty: true,
                    ..Default::default()
                },
            ),
        ]
    }

    /// Get available encoding formats based on options
    pub fn get_encoding_formats(
        enable_cover: bool,
        enable_dithering: bool,
    ) -> Vec<(String, String, CoverFormat)> {
        let mut formats = Vec::new();

        if enable_cover {
            if enable_dithering {
                // With dithering, we can use binary formats (CCITT4/JBIG2) since we have 1-bit data
                formats.push((
                    CLI_TEXT
                        .interactive
                        .encoding_formats
                        .ccitt4_jpeg_fastest
                        .clone(),
                    "ccitt4".to_string(),
                    CoverFormat::Ccitt4,
                ));
                formats.push((
                    CLI_TEXT
                        .interactive
                        .encoding_formats
                        .jbig2_jpeg_modern
                        .clone(),
                    "jbig2".to_string(),
                    CoverFormat::Jbig2,
                ));

                // Also include non-binary formats as fallbacks
                formats.push((
                    CLI_TEXT
                        .interactive
                        .encoding_formats
                        .ccitt4_jp2_compression
                        .clone(),
                    "ccitt4".to_string(),
                    CoverFormat::Jp2,
                ));
                formats.push((
                    CLI_TEXT.interactive.encoding_formats.jbig2_jp2_best.clone(),
                    "jbig2".to_string(),
                    CoverFormat::Jp2,
                ));
            } else {
                // Without dithering, only use non-binary formats for color images
                formats.push((
                    CLI_TEXT
                        .interactive
                        .encoding_formats
                        .jbig2_jp2_color
                        .clone(),
                    "jbig2".to_string(),
                    CoverFormat::Jp2,
                ));
                formats.push((
                    CLI_TEXT
                        .interactive
                        .encoding_formats
                        .ccitt4_jp2_color
                        .clone(),
                    "ccitt4".to_string(),
                    CoverFormat::Jp2,
                ));
                formats.push((
                    CLI_TEXT
                        .interactive
                        .encoding_formats
                        .ccitt4_jpeg_fastest
                        .clone(),
                    "ccitt4".to_string(),
                    CoverFormat::Jpeg,
                ));
            }
        } else {
            // For text-only pages, we can use binary formats since the content is already binarized
            formats.push((
                CLI_TEXT
                    .interactive
                    .encoding_formats
                    .ccitt4_text_fastest
                    .clone(),
                "ccitt4".to_string(),
                CoverFormat::Ccitt4,
            ));
            formats.push((
                CLI_TEXT
                    .interactive
                    .encoding_formats
                    .jbig2_text_best
                    .clone(),
                "jbig2".to_string(),
                CoverFormat::Jbig2,
            ));

            // Also include the None format as a fallback (will use the default format)
            formats.push((
                "Default format".to_string(),
                "default".to_string(),
                CoverFormat::None,
            ));
        }

        formats
    }
}

use once_cell::sync::Lazy;

/// Global instance for efficient label classification
/// This ensures we don't repeatedly create LabelClassifier instances
pub static LABEL_CLASSIFIER: Lazy<LabelClassifier> = Lazy::new(|| LabelClassifier::new());
