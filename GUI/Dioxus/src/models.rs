// File: src/models.rs

use std::path::PathBuf;

pub use lege::processing_log::{
    CompressionType, CoverImageType, ImageProcessingType, LogEntry, OutputFormat,
    ProcessingOptions, ProcessingResult,
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocumentItem {
    pub id: String,
    pub file_path: PathBuf,
    pub file_name: String,
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

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            file_path,
            file_name,
            status: DocumentStatus::Queued,
            output_path: None,
            progress: 0.0,
            error_message: None,
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
