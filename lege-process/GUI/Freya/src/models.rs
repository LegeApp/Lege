// GUI models — all types defined locally; no lege dependency.

use std::path::PathBuf;

// ── Processing option enums ───────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OutputFormat {
    #[default]
    Pdf,
    Djvu,
    Epub,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OcrMode {
    #[default]
    #[serde(alias = "Low")]
    Fast,
    #[serde(alias = "High")]
    Thorough,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CompressionType {
    #[default]
    Ccitt4,
    Jbig2,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CoverImageType {
    #[default]
    Jpeg,
    Jpeg2000,
    None,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ImageProcessingType {
    #[default]
    Original,
    Dithered,
}

impl std::fmt::Display for ImageProcessingType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dithered => write!(f, "Dithered"),
            Self::Original => write!(f, "Original"),
        }
    }
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pdf => write!(f, "PDF"),
            Self::Djvu => write!(f, "DjVu"),
            Self::Epub => write!(f, "EPUB"),
        }
    }
}

impl std::fmt::Display for OcrMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fast => write!(f, "Fast"),
            Self::Thorough => write!(f, "Thorough"),
        }
    }
}

impl std::fmt::Display for CompressionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ccitt4 => write!(f, "CCITT4"),
            Self::Jbig2 => write!(f, "JBIG2"),
        }
    }
}

// ── ProcessingOptions ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ProcessingOptions {
    pub input_path: Option<PathBuf>,
    pub output_path: Option<PathBuf>,
    /// True when `output_path` was auto-derived from an input rather than
    /// picked by the user. Auto output follows each file's own input folder
    /// at job time (mixed-folder queues land next to their sources); an
    /// explicitly chosen folder applies to every file.
    pub output_path_is_auto: bool,

    pub output_format: OutputFormat,
    pub compression_type: CompressionType,
    pub cover_image_type: CoverImageType,
    pub image_processing_type: ImageProcessingType,
    pub ccitt4_dithered_images: bool,
    pub original_cover: bool,

    pub target_height: Option<u32>,
    pub target_width: Option<u32>,
    pub target_device: Option<String>,
    pub page_range: Option<String>,
    pub no_front_cover: bool,
    pub png_folder_mode: bool,
    pub layout_analysis: bool,
    pub layout_exclusion_pages: Option<String>,
    pub use_ocr: bool,
    pub ocr_mode: OcrMode,
    pub make_epub_also: bool,
    pub use_jbig2_halftone: bool,
    pub high_quality_output: bool,
    pub jpeg_compat: bool,
    pub invert_input: bool,

    pub center_margins: bool,
    pub crop_margins: bool,
    pub crop_footnotes: bool,
    pub crop_free_aspect: bool,
    pub reflow: bool,

    pub use_heavy_binarization: bool,
    pub k_factor: f32,
    /// When set, adaptive binarization uses the user-supplied `k_factor` and the
    /// K-factor input is revealed; otherwise the default `k_factor` is used and hidden.
    pub use_custom_adaptive: bool,
    pub use_fixed_threshold: bool,
    pub threshold_value: u8,
    /// Grayscale/MRC rendering: text becomes a JBIG2/JB2 ink mask over a
    /// JP2/IW44 grayscale background (keeps antialiasing). Mutually exclusive
    /// with the bilevel binarization options above; image regions stay JP2/IW44.
    pub grayscale_mode: bool,
}

impl ProcessingOptions {
    pub fn new() -> Self {
        Self {
            output_format: OutputFormat::Pdf,
            // JBIG2 default; CCITT4 is reachable via Compatibility mode.
            compression_type: CompressionType::Jbig2,
            cover_image_type: CoverImageType::Jpeg,
            image_processing_type: ImageProcessingType::Original,
            original_cover: true,
            target_height: Some(1200),
            layout_analysis: true,
            k_factor: lege_ipc::DEFAULT_K_FACTOR,
            // Adaptive is the effective processing default, so represent that
            // default explicitly in the mutually-exclusive GUI controls.
            use_custom_adaptive: true,
            threshold_value: 180,
            ..Default::default()
        }
    }

    pub fn effective_text_format(&self) -> &'static str {
        match self.output_format {
            OutputFormat::Djvu => "djvu",
            OutputFormat::Epub => "epub",
            OutputFormat::Pdf if self.layout_analysis => match self.image_processing_type {
                ImageProcessingType::Original => "ccitt4",
                ImageProcessingType::Dithered => "jbig2",
            },
            OutputFormat::Pdf => match self.compression_type {
                CompressionType::Ccitt4 => "ccitt4",
                CompressionType::Jbig2 => "jbig2",
            },
        }
    }

    /// Keep GUI state aligned with the core pipeline's effective feature
    /// rules. Reflow is a layout mode; inversion disables the layout model.
    pub fn set_layout_analysis(&mut self, enabled: bool) {
        self.layout_analysis = enabled;
        if enabled {
            self.invert_input = false;
        } else {
            self.reflow = false;
        }
    }

    pub fn set_invert_input(&mut self, enabled: bool) {
        self.invert_input = enabled;
        if enabled {
            self.layout_analysis = false;
            self.reflow = false;
        }
    }

    pub fn set_reflow(&mut self, enabled: bool) {
        self.reflow = enabled;
        if enabled {
            self.layout_analysis = true;
            self.invert_input = false;
            self.center_margins = false;
            self.crop_margins = false;
            self.crop_footnotes = false;
            self.crop_free_aspect = false;
        }
    }

    /// Repair stale or hand-edited settings before they become worker
    /// arguments. An explicit reflow selection wins over hidden incompatible
    /// state because it is the most constrained mode.
    pub fn normalize_processing_dependencies(&mut self) {
        if self.reflow {
            self.set_reflow(true);
        } else if self.invert_input {
            self.set_invert_input(true);
        } else if !self.layout_analysis {
            self.set_layout_analysis(false);
        }
    }
}

#[cfg(test)]
mod reflow_tests {
    use super::ProcessingOptions;

    #[test]
    fn reflow_enables_layout_and_clears_incompatible_geometry_state() {
        let mut options = ProcessingOptions::new();
        options.layout_analysis = false;
        options.invert_input = true;
        options.center_margins = true;
        options.crop_margins = true;
        options.crop_footnotes = true;
        options.crop_free_aspect = true;

        options.set_reflow(true);

        assert!(options.reflow);
        assert!(options.layout_analysis);
        assert!(!options.invert_input);
        assert!(!options.center_margins);
        assert!(!options.crop_margins);
        assert!(!options.crop_footnotes);
        assert!(!options.crop_free_aspect);
    }

    #[test]
    fn disabling_layout_or_enabling_inversion_disables_reflow() {
        let mut options = ProcessingOptions::new();
        options.set_reflow(true);
        options.set_layout_analysis(false);
        assert!(!options.reflow);

        options.set_reflow(true);
        options.set_invert_input(true);
        assert!(!options.reflow);
        assert!(!options.layout_analysis);
    }

    #[test]
    fn normalization_repairs_stale_reflow_settings() {
        let mut options = ProcessingOptions::new();
        options.reflow = true;
        options.layout_analysis = false;
        options.invert_input = true;
        options.crop_margins = true;

        options.normalize_processing_dependencies();

        assert!(options.reflow);
        assert!(options.layout_analysis);
        assert!(!options.invert_input);
        assert!(!options.crop_margins);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResolutionPreset {
    pub height: u32,
    pub width: Option<u32>,
}

impl ResolutionPreset {
    pub fn from_options(options: &ProcessingOptions) -> Self {
        Self {
            height: options.target_height.unwrap_or(1200),
            width: options.target_width,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum InputKind {
    Pdf,
    ImageFolder,
    ZipArchive,
    #[default]
    Unknown,
}

impl InputKind {
    pub fn detect(path: &PathBuf) -> Self {
        if path.is_dir() {
            return InputKind::ImageFolder;
        }
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("pdf") => InputKind::Pdf,
            Some("zip") => InputKind::ZipArchive,
            _ => InputKind::Unknown,
        }
    }

    pub fn badge(&self) -> &'static str {
        match self {
            InputKind::Pdf => "PDF",
            InputKind::ImageFolder => "DIR",
            InputKind::ZipArchive => "ZIP",
            InputKind::Unknown => "???",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocumentItem {
    pub id: String,
    pub file_path: PathBuf,
    pub file_name: String,
    pub input_kind: InputKind,
    pub page_count: Option<u32>,
    pub status: DocumentStatus,
    pub output_path: Option<PathBuf>,
    pub progress: f32,
    pub error_message: Option<String>,
}

impl DocumentItem {
    pub fn new(file_path: PathBuf) -> Self {
        let file_name = file_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let input_kind = InputKind::detect(&file_path);

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            file_path,
            file_name,
            input_kind,
            page_count: None,
            status: DocumentStatus::Queued,
            output_path: None,
            progress: 0.0,
            error_message: None,
        }
    }

    pub fn count_label(&self) -> &'static str {
        match self.input_kind {
            InputKind::Pdf => "pages",
            InputKind::ImageFolder | InputKind::ZipArchive => "images",
            InputKind::Unknown => "pages",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum DocumentStatus {
    #[default]
    Queued,
    Processing(f32),
    Completed,
    Failed(String),
    Cancelled,
}

impl std::fmt::Display for DocumentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocumentStatus::Queued => write!(f, "Queued"),
            DocumentStatus::Processing(progress) => {
                write!(f, "Processing: {:.0}%", progress * 100.0)
            }
            DocumentStatus::Completed => write!(f, "Completed"),
            DocumentStatus::Failed(err) => write!(f, "Failed: {}", err),
            DocumentStatus::Cancelled => write!(f, "Cancelled"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_layout_text_format_uses_base_compression_setting() {
        let mut options = ProcessingOptions::new();
        options.layout_analysis = false;
        options.image_processing_type = ImageProcessingType::Dithered;

        options.compression_type = CompressionType::Ccitt4;
        assert_eq!(options.effective_text_format(), "ccitt4");

        options.compression_type = CompressionType::Jbig2;
        assert_eq!(options.effective_text_format(), "jbig2");
    }

    #[test]
    fn layout_text_format_uses_image_processing_setting() {
        let mut options = ProcessingOptions::new();
        options.layout_analysis = true;
        options.compression_type = CompressionType::Jbig2;

        options.image_processing_type = ImageProcessingType::Original;
        assert_eq!(options.effective_text_format(), "ccitt4");

        options.image_processing_type = ImageProcessingType::Dithered;
        assert_eq!(options.effective_text_format(), "jbig2");
    }
}
