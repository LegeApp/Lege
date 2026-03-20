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
/// Both YOLO and PaddleX engines map their raw class IDs to one of these
/// categories.  All downstream decisions (dithering, masking, OCR region
/// selection, JBIG2 encoding mode) are driven by this enum — never by raw
/// class IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentCategory {
    /// Textual content: titles, paragraphs, captions, formulas, references, …
    Text,
    /// Photographic / illustrative content: figures, header/footer images.
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

/// Type of content in a document region (used by `Region` struct)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RegionType {
    Text,
    Image,
    Table,
    Figure,
    Other,
}

use std::collections::HashSet;

/// Centralized label information system that efficiently combines all PaddleX label data
/// Provides O(1) lookups and maintains the complete mapping of class IDs to names
#[derive(Debug, Clone)]
pub struct LabelInfo {
    // Complete mapping of class IDs to names
    class_names: Vec<&'static str>,

    // Precomputed sets for efficient lookups
    image_class_ids: HashSet<i32>,
    text_class_ids: HashSet<i32>,
    margin_crop_excluded_ids: HashSet<i32>,
}

impl LabelInfo {
    pub fn new() -> Self {
        let class_names = vec![
            "paragraph_title", // 0
            "image",           // 1
            "text",            // 2
            "number",          // 3
            "abstract",        // 4
            "content",         // 5
            "figure_title",    // 6
            "formula",         // 7
            "table",           // 8
            "table_title",     // 9
            "reference",       // 10
            "doc_title",       // 11
            "footnote",        // 12
            "header",          // 13
            "algorithm",       // 14
            "footer",          // 15
            "seal",            // 16
            "chart_title",     // 17
            "chart",           // 18
            "formula_number",  // 19
            "header_image",    // 20
            "footer_image",    // 21
            "aside_text",      // 22
        ];

        // Image-like classes: treat charts (18) as text-like (thin lines threshold well),
        // so exclude 18 to avoid unnecessary IW44 regions and background artifacts by default.
        // Allow overriding this behavior via an env var for troubleshooting or special documents.
        let mut image_class_ids: HashSet<i32> = [1, 20, 21].into_iter().collect();
        if std::env::var("LEGE_TREAT_CHART_AS_IMAGE")
            .map(|v| {
                let v = v.to_ascii_lowercase();
                v == "1" || v == "true" || v == "yes"
            })
            .unwrap_or(false)
        {
            image_class_ids.insert(18); // chart
        }
        let text_class_ids: HashSet<i32> = [
            0, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 22,
        ]
        .into_iter()
        .collect();
        let margin_crop_excluded_ids: HashSet<i32> = [3, 12].into_iter().collect();

        Self {
            class_names,
            image_class_ids,
            text_class_ids,
            margin_crop_excluded_ids,
        }
    }

    // === Core lookup methods ===

    pub fn get_class_name(&self, class_id: i32) -> String {
        self.class_names
            .get(class_id as usize)
            .map(|&s| s.to_string())
            .unwrap_or_else(|| format!("unknown_{}", class_id))
    }

    pub fn is_image_class(&self, class_id: i32) -> bool {
        self.image_class_ids.contains(&class_id)
    }

    pub fn is_text_class(&self, class_id: i32) -> bool {
        self.text_class_ids.contains(&class_id)
    }

    pub fn is_margin_crop_excluded(&self, class_id: i32) -> bool {
        self.margin_crop_excluded_ids.contains(&class_id)
    }

    // === Bulk access methods for compatibility ===

    pub fn all_image_class_ids(&self) -> Vec<i32> {
        self.image_class_ids.iter().copied().collect()
    }

    pub fn all_text_class_ids(&self) -> Vec<i32> {
        self.text_class_ids.iter().copied().collect()
    }

    pub fn all_margin_crop_excluded_ids(&self) -> Vec<i32> {
        self.margin_crop_excluded_ids.iter().copied().collect()
    }

    // === Utility methods ===

    pub fn total_classes(&self) -> usize {
        self.class_names.len()
    }

    pub fn class_id_exists(&self, class_id: i32) -> bool {
        (class_id >= 0) && (class_id < self.class_names.len() as i32)
    }

    // === Iterator support ===

    pub fn iter_class_names(&self) -> impl Iterator<Item = (i32, &str)> {
        self.class_names
            .iter()
            .enumerate()
            .map(|(id, &name)| (id as i32, name))
    }

    // === Original const arrays for specific use cases ===
    pub const IMAGE_CLASS_IDS: &'static [i32] = &[1, 20, 21];
    pub const TEXT_CLASS_IDS: &'static [i32] = &[
        0, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 22,
    ];
    pub const MARGIN_CROP_EXCLUDED_CLASS_IDS: &'static [i32] = &[3, 12];
}

impl Default for LabelInfo {
    fn default() -> Self {
        Self::new()
    }
}

/// Standardized label classifier that provides consistent label categorization
/// across all modules (engine, margin, OCR, color_processing)
///
/// This is a compatibility layer that uses the centralized LabelInfo
pub struct LabelClassifier {
    label_info: LabelInfo,
}

impl LabelClassifier {
    pub fn new() -> Self {
        Self {
            label_info: LabelInfo::new(),
        }
    }

    /// Classify a detection for margin calculation purposes
    /// - For StandardizeAndCenter: includes all content
    /// - For CropAndResize: excludes page numbers and footnotes
    pub fn is_margin_calc_label(&self, detection: &Detection, for_cropping: bool) -> bool {
        if for_cropping {
            // Exclude page numbers and footnotes when cropping
            !self.label_info.is_margin_crop_excluded(detection.class_id)
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

    /// Get the underlying label info for direct access to efficient lookups
    pub fn label_info(&self) -> &LabelInfo {
        &self.label_info
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
    pub pdf_compatibility_mode: bool,

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
            pdf_compatibility_mode: false, // Default to disabled (use modern compression)
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

    /// Set PDF compatibility mode (disables object streams and compression for better reader support)
    pub fn with_pdf_compatibility(mut self, enable: bool) -> Self {
        self.pdf_compatibility_mode = enable;
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
        config.pdf_compatibility_mode = self.pdf_compatibility_mode;

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
