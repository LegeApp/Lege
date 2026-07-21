use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_FILE_NAME: &str = "log.json";
const STATUS_STARTED: &str = "started";
const STATUS_COMPLETED: &str = "completed";
const STATUS_FAILED: &str = "failed";

/// Output container format.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OutputFormat {
    #[default]
    Pdf,
    Djvu,
    Epub,
}

impl OutputFormat {
    pub fn all() -> Vec<OutputFormat> {
        vec![OutputFormat::Pdf, OutputFormat::Djvu, OutputFormat::Epub]
    }
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            OutputFormat::Pdf => "PDF",
            OutputFormat::Djvu => "DJVU",
            OutputFormat::Epub => "EPUB",
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
    pub slow_ocr: bool,
    pub high_quality_output: bool,
    pub jpeg_compat: bool,
    pub invert_input: bool,

    // Margin processing options
    pub center_margins: bool,
    pub crop_margins: bool,
    pub crop_footnotes: bool,
    pub crop_free_aspect: bool,

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
            slow_ocr: false,
            high_quality_output: false,
            jpeg_compat: false,
            invert_input: false,
            center_margins: false,
            crop_margins: false,
            crop_footnotes: false,
            crop_free_aspect: false,
            use_heavy_binarization: false,
            k_factor: crate::DEFAULT_K_FACTOR,
            use_fixed_threshold: false,
            threshold_value: 200,
        }
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
    #[serde(default = "default_log_status")]
    pub status: String,
    #[serde(default)]
    pub error_kind: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
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
            status: STATUS_COMPLETED.to_string(),
            error_kind: None,
            error_message: None,
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

    pub fn failed(
        input_path: PathBuf,
        output_path: PathBuf,
        original_size: u64,
        error_kind: impl Into<String>,
        error_message: impl Into<String>,
        options: &ProcessingOptions,
    ) -> Self {
        let result = ProcessingResult::new(input_path, output_path, original_size, 0, false);
        let mut entry = Self::new(&result, options);
        entry.status = STATUS_FAILED.to_string();
        entry.error_kind = Some(error_kind.into());
        entry.error_message = Some(error_message.into());
        entry.compressed_size = 0;
        entry.compression_percentage = 0.0;
        entry
    }

    pub fn started(
        input_path: PathBuf,
        output_path: PathBuf,
        original_size: u64,
        options: &ProcessingOptions,
    ) -> Self {
        let result = ProcessingResult::new(input_path, output_path, original_size, 0, false);
        let mut entry = Self::new(&result, options);
        entry.status = STATUS_STARTED.to_string();
        entry
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

fn default_log_status() -> String {
    STATUS_COMPLETED.to_string()
}

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn log_directory() -> PathBuf {
    crate::logs_dir()
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

    ensure_option_paths(&mut entry);

    if let Some(index) = find_started_entry(&entries, &entry.input_path, &entry.output_path) {
        entries[index] = entry.clone();
    } else {
        entries.push(entry.clone());
    }

    save_log_entries(&entries)?;
    Ok(entry)
}

pub fn add_started_log_entry(
    input_path: PathBuf,
    output_path: PathBuf,
    original_size: u64,
    options: &ProcessingOptions,
) -> Result<LogEntry> {
    let mut entries = load_log_entries().unwrap_or_default();
    let mut entry = LogEntry::started(input_path, output_path, original_size, options);

    ensure_option_paths(&mut entry);

    if let Some(index) = find_started_entry(&entries, &entry.input_path, &entry.output_path) {
        entries[index] = entry.clone();
    } else {
        entries.push(entry.clone());
    }

    save_log_entries(&entries)?;
    Ok(entry)
}

pub fn add_failed_log_entry(
    input_path: PathBuf,
    output_path: PathBuf,
    original_size: u64,
    error: &str,
    options: &ProcessingOptions,
) -> Result<LogEntry> {
    let mut entries = load_log_entries().unwrap_or_default();
    let mut entry = LogEntry::failed(
        input_path,
        output_path,
        original_size,
        classify_error(error),
        error,
        options,
    );

    ensure_option_paths(&mut entry);

    if let Some(index) = find_started_entry(&entries, &entry.input_path, &entry.output_path) {
        entries[index] = entry.clone();
    } else {
        entries.push(entry.clone());
    }

    save_log_entries(&entries)?;
    Ok(entry)
}

pub fn reconcile_started_entries() -> Result<Vec<LogEntry>> {
    let mut entries = load_log_entries().unwrap_or_default();
    let mut reconciled = Vec::new();

    for entry in entries.iter_mut() {
        if entry.status == STATUS_STARTED {
            entry.status = STATUS_FAILED.to_string();
            entry.error_kind = Some("worker_exit".to_string());
            entry.error_message = Some(
                "Previous processing job was marked started but did not report completion before the application exited. The process may have been killed or interrupted.".to_string(),
            );
            entry.compressed_size = 0;
            entry.compression_percentage = 0.0;
            reconciled.push(entry.clone());
        }
    }

    if !reconciled.is_empty() {
        save_log_entries(&entries)?;
    }

    Ok(reconciled)
}

fn ensure_option_paths(entry: &mut LogEntry) {
    if entry.options.input_path.is_none() {
        entry.options.input_path = Some(PathBuf::from(&entry.input_path));
    }
    if entry.options.output_path.is_none() {
        entry
            .options
            .output_path
            .replace(PathBuf::from(&entry.output_path));
    }
}

fn find_started_entry(entries: &[LogEntry], input_path: &str, output_path: &str) -> Option<usize> {
    entries.iter().rposition(|entry| {
        entry.status == STATUS_STARTED
            && entry.input_path == input_path
            && entry.output_path == output_path
    })
}

fn classify_error(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if lower.contains("out of memory")
        || lower.contains("memory")
        || lower.contains("vram")
        || lower.contains("wgpu")
        || lower.contains("gpu")
        || lower.contains("adapter")
        || lower.contains("device")
    {
        "gpu_or_memory"
    } else if lower.contains("worker exited before completion")
        || lower.contains("exit status")
        || lower.contains("signal")
        || lower.contains("killed")
        || lower.contains("process")
        || lower.contains("status")
    {
        "worker_exit"
    } else if lower.contains("pdfium")
        || lower.contains("cannot read pdf")
        || lower.contains("cannot open pdf")
        || lower.contains("failed to render page")
        || lower.contains("load_pdf")
        || lower.contains("load pdf")
        || lower.contains("page index")
    {
        "pdf_input"
    } else {
        "processing_error"
    }
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
