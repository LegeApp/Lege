// Freya-side copy of the Dioxus GUI models module.
// Keep in sync manually until GUI support code is consolidated.

use std::path::PathBuf;

pub use lege::processing_log::{
    CompressionType, CoverImageType, ImageProcessingType, LogEntry, OutputFormat,
    ProcessingOptions, ProcessingResult,
};

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
