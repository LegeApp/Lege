// GUI models — all types defined locally; no lege dependency.

use std::path::PathBuf;

// ── Processing option enums ───────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OutputFormat {
    #[default]
    Pdf,
    Djvu,
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

    pub output_format: OutputFormat,
    pub compression_type: CompressionType,
    pub cover_image_type: CoverImageType,
    pub image_processing_type: ImageProcessingType,
    pub ccitt4_dithered_images: bool,
    pub original_cover: bool,

    pub target_height: Option<u32>,
    pub target_device: Option<String>,
    pub page_range: Option<String>,
    pub no_front_cover: bool,
    pub png_folder_mode: bool,
    pub layout_analysis: bool,
    pub use_ocr: bool,
    pub high_quality_output: bool,
    pub jpeg_compat: bool,
    pub invert_input: bool,

    pub center_margins: bool,
    pub crop_margins: bool,
    pub crop_footnotes: bool,
    pub reflow: bool,

    pub use_heavy_binarization: bool,
    pub k_factor: f32,
    pub use_fixed_threshold: bool,
    pub threshold_value: u8,
}

impl ProcessingOptions {
    pub fn new() -> Self {
        Self {
            output_format: OutputFormat::Pdf,
            compression_type: CompressionType::Ccitt4,
            cover_image_type: CoverImageType::Jpeg,
            image_processing_type: ImageProcessingType::Original,
            original_cover: true,
            target_height: Some(1200),
            layout_analysis: true,
            k_factor: 0.2,
            threshold_value: 180,
            ..Default::default()
        }
    }
}

// ── ProcessingResult ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProcessingResult {
    pub input_filename: String,
    pub input_path: PathBuf,
    pub output_filename: String,
    pub output_path: PathBuf,
    pub original_size: u64,
    pub compressed_size: u64,
    pub compression_percentage: f64,
    pub page_range_used: bool,
}

impl ProcessingResult {
    pub fn new(
        input_path: PathBuf,
        output_path: PathBuf,
        original_size: u64,
        compressed_size: u64,
        page_range_used: bool,
    ) -> Self {
        let input_filename = input_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let output_filename = output_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let compression_percentage = if original_size > 0 {
            (compressed_size as f64 / original_size as f64) * 100.0
        } else {
            0.0
        };
        Self {
            input_filename,
            input_path,
            output_filename,
            output_path,
            original_size,
            compressed_size,
            compression_percentage,
            page_range_used,
        }
    }
}

// ── LogEntry ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    pub timestamp: u64,
    pub input_filename: String,
    pub input_path: String,
    pub output_filename: String,
    pub output_path: String,
    pub original_size: u64,
    pub compressed_size: u64,
    pub compression_percentage: f64,
    pub options: ProcessingOptions,
}

impl LogEntry {
    pub fn new(result: &ProcessingResult, options: &ProcessingOptions) -> Self {
        Self {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            input_filename: result.input_filename.clone(),
            input_path: result.input_path.to_string_lossy().to_string(),
            output_filename: result.output_filename.clone(),
            output_path: result.output_path.to_string_lossy().to_string(),
            original_size: result.original_size,
            compressed_size: result.compressed_size,
            compression_percentage: result.compression_percentage,
            options: options.clone(),
        }
    }

    pub fn format_timestamp(&self) -> String {
        use chrono::TimeZone;
        chrono::Local
            .timestamp_opt(self.timestamp as i64, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }

    pub fn format_size(bytes: u64) -> String {
        const KIB: f64 = 1024.0;
        const MIB: f64 = KIB * 1024.0;
        const GIB: f64 = MIB * 1024.0;
        match bytes {
            0..=1023 => format!("{bytes} B"),
            1024..=1_048_575 => format!("{:.1} KiB", bytes as f64 / KIB),
            1_048_576..=1_073_741_823 => format!("{:.1} MiB", bytes as f64 / MIB),
            _ => format!("{:.1} GiB", bytes as f64 / GIB),
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
