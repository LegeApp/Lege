use crate::{CoverFormat, PipelineConfig, app_dirs, margin::MarginSettings};
use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_FILE_NAME: &str = "log.json";

/// Output container format (PDF or DjVu)
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OutputFormat {
    #[default]
    Pdf,
    Djvu,
}

impl OutputFormat {
    pub fn all() -> Vec<OutputFormat> {
        vec![OutputFormat::Pdf, OutputFormat::Djvu]
    }
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            OutputFormat::Pdf => "PDF",
            OutputFormat::Djvu => "DJVU",
        };
        write!(f, "{label}")
    }
}

/// Compression types (CCITT4/JBIG2)
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CompressionType {
    #[default]
    Ccitt4,
    Jbig2,
}

impl CompressionType {
    pub fn all() -> Vec<CompressionType> {
        vec![CompressionType::Ccitt4, CompressionType::Jbig2]
    }
}

impl std::fmt::Display for CompressionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            CompressionType::Ccitt4 => "CCITT4",
            CompressionType::Jbig2 => "JBIG2",
        };
        write!(f, "{label}")
    }
}

/// Image format types (JPEG/JPEG2000/None) - applies globally to non-binarized images
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CoverImageType {
    Jpeg,
    #[default]
    Jpeg2000,
    None,
}

impl CoverImageType {
    pub fn all() -> Vec<CoverImageType> {
        vec![
            CoverImageType::Jpeg,
            CoverImageType::Jpeg2000,
            CoverImageType::None,
        ]
    }
}

impl std::fmt::Display for CoverImageType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            CoverImageType::Jpeg => "JPEG",
            CoverImageType::Jpeg2000 => "JPEG2000",
            CoverImageType::None => "None",
        };
        write!(f, "{label}")
    }
}

/// Image processing types (Dithered/Original/No Layout Detection)
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ImageProcessingType {
    #[default]
    Dithered,
    Original,
}

impl ImageProcessingType {
    pub fn all() -> Vec<ImageProcessingType> {
        vec![ImageProcessingType::Dithered, ImageProcessingType::Original]
    }
}

impl std::fmt::Display for ImageProcessingType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            ImageProcessingType::Dithered => "Dithered",
            ImageProcessingType::Original => "Original",
        };
        write!(f, "{label}")
    }
}

/// Processing options persisted alongside each log entry.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProcessingOptions {
    pub input_path: Option<PathBuf>,
    pub output_path: Option<PathBuf>,

    // Toggle options (3 groups from top row)
    pub output_format: OutputFormat,
    pub compression_type: CompressionType,
    pub cover_image_type: CoverImageType,
    pub image_processing_type: ImageProcessingType,
    pub ccitt4_dithered_images: bool,
    pub original_cover: bool,

    // Processing options
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

    // Margin processing options
    pub center_margins: bool,
    pub crop_margins: bool,
    pub crop_footnotes: bool,
    pub deskew_documents: bool,

    // Binarization options
    pub use_heavy_binarization: bool,
    pub k_factor: f32,
    pub use_fixed_threshold: bool,
    pub threshold_value: u8,
}

impl ProcessingOptions {
    pub fn new() -> Self {
        Self {
            input_path: None,
            output_path: None,
            output_format: OutputFormat::Pdf,
            compression_type: CompressionType::Ccitt4,
            cover_image_type: CoverImageType::Jpeg,
            image_processing_type: ImageProcessingType::Original,
            ccitt4_dithered_images: false,
            original_cover: true,
            target_height: Some(1200),
            target_device: None,
            page_range: None,
            no_front_cover: false,
            png_folder_mode: false,
            layout_analysis: true,
            use_ocr: false,
            high_quality_output: false,
            jpeg_compat: false,
            invert_input: false,
            center_margins: false,
            crop_margins: false,
            crop_footnotes: false,
            deskew_documents: false,
            use_heavy_binarization: false,
            k_factor: 0.2,
            use_fixed_threshold: false,
            threshold_value: 200,
        }
    }

    pub fn from_pipeline_config(config: &PipelineConfig) -> Self {
        let mut options = ProcessingOptions::new();

        options.output_format = if matches!(config.text_format(), "djvu") {
            OutputFormat::Djvu
        } else {
            OutputFormat::Pdf
        };

        if options.output_format == OutputFormat::Pdf {
            options.compression_type = match config.text_format() {
                "jbig2" => CompressionType::Jbig2,
                "ccitt4" => CompressionType::Ccitt4,
                _ => CompressionType::Ccitt4,
            };
        }

        options.cover_image_type = match *config.cover_format() {
            CoverFormat::Jpeg => CoverImageType::Jpeg,
            CoverFormat::Jp2 => CoverImageType::Jpeg2000,
            CoverFormat::None | CoverFormat::Ccitt4 | CoverFormat::Jbig2 => CoverImageType::None,
        };

        options.image_processing_type = if config.dither_images() {
            ImageProcessingType::Dithered
        } else {
            ImageProcessingType::Original
        };
        // Infer checkbox value: when text is CCITT4 and dithering is enabled, we treat it as "CCITT4 text with dithered images".
        options.ccitt4_dithered_images =
            matches!(options.image_processing_type, ImageProcessingType::Dithered)
                && matches!(config.text_format(), "ccitt4");

        options.original_cover = config.enable_cover_page();
        options.no_front_cover = config.no_cover_page();
        options.target_height = Some(config.target_height());
        options.layout_analysis = config.enable_layout_detection();
        options.use_ocr = config.enable_ocr();
        options.high_quality_output = config.high_quality_output();
        options.jpeg_compat = config.jpeg_compat();
        options.invert_input = config.invert_input();

        options.center_margins = matches!(
            config.margin_settings(),
            MarginSettings::StandardizeAndCenter
        );
        options.crop_margins = matches!(config.margin_settings(), MarginSettings::CropAndResize);
        options.crop_footnotes = config.crop_footnotes();
        options.deskew_documents = config.enable_deskew();

        let bin = config.binarization();
        options.use_heavy_binarization = bin.use_heavy_duty;
        options.k_factor = bin.k_factor;
        options.use_fixed_threshold = bin.use_fixed_threshold;
        options.threshold_value = bin.fixed_threshold;

        if let Some(range) = config.page_range() {
            options.page_range = Some(format!("{}-{}", range.start, range.end));
        }

        options
    }
}

/// Result metadata captured after processing completes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
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
    #[allow(clippy::too_many_arguments)]
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

/// Persisted log entry combining processing metadata and options.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LogEntry {
    pub timestamp: u64, // Unix timestamp
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
            timestamp: current_timestamp(),
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
        let datetime = chrono::DateTime::from_timestamp(self.timestamp as i64, 0)
            .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).unwrap());
        datetime
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
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

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn log_directory() -> PathBuf {
    app_dirs::logs_dir()
}

/// Get the path to the log file in the user data directory.
pub fn get_log_file_path() -> PathBuf {
    let path = log_directory().join(LOG_FILE_NAME);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    path
}

/// Load existing log entries from the JSON file.
pub fn load_log_entries() -> Result<Vec<LogEntry>> {
    let log_path = get_log_file_path();

    if !log_path.exists() {
        return Ok(Vec::new());
    }

    let content =
        fs::read_to_string(&log_path).with_context(|| format!("Reading {}", log_path.display()))?;
    let entries: Vec<LogEntry> = serde_json::from_str(&content)
        .with_context(|| format!("Parsing {}", log_path.display()))?;
    Ok(entries)
}

/// Save log entries to the JSON file.
pub fn save_log_entries(entries: &[LogEntry]) -> Result<()> {
    let log_path = get_log_file_path();
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("Creating {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(entries)?;
    fs::write(&log_path, content).with_context(|| format!("Writing {}", log_path.display()))?;
    Ok(())
}

/// Add a new log entry and save to file.
pub fn add_log_entry(result: &ProcessingResult, options: &ProcessingOptions) -> Result<LogEntry> {
    let mut entries = load_log_entries().unwrap_or_default();
    let mut entry = LogEntry::new(result, options);

    // Ensure paths are recorded for diagnostics.
    if entry.options.input_path.is_none() {
        entry.options.input_path = Some(PathBuf::from(&entry.input_path));
    }
    if entry.options.output_path.is_none() {
        entry
            .options
            .output_path
            .replace(PathBuf::from(&entry.output_path));
    }

    entries.push(entry.clone());
    save_log_entries(&entries)?;
    Ok(entry)
}

/// Get recent log entries (up to specified count).
pub fn get_recent_entries(count: usize) -> Vec<LogEntry> {
    load_log_entries()
        .unwrap_or_default()
        .into_iter()
        .rev()
        .take(count)
        .collect()
}
