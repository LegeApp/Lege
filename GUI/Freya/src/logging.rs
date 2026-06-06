// Processing log: read/write a JSON file in the user's log directory.

use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

use crate::models::{LogEntry, ProcessingOptions, ProcessingResult};

const LOG_FILE_NAME: &str = "processing_log.json";

fn log_directory() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Lege")
        .join("logs")
}

pub fn get_log_file_path() -> PathBuf {
    let path = log_directory().join(LOG_FILE_NAME);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    path
}

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

fn save_log_entries(entries: &[LogEntry]) -> Result<()> {
    let log_path = get_log_file_path();
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("Creating {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(entries)?;
    fs::write(&log_path, content).with_context(|| format!("Writing {}", log_path.display()))?;
    Ok(())
}

pub fn add_log_entry(result: &ProcessingResult, options: &ProcessingOptions) -> Result<LogEntry> {
    let mut entries = load_log_entries().unwrap_or_default();
    let mut entry = LogEntry::new(result, options);

    if entry.options.input_path.is_none() {
        entry.options.input_path = Some(PathBuf::from(&entry.input_path));
    }
    if entry.options.output_path.is_none() {
        entry.options.output_path = Some(PathBuf::from(&entry.output_path));
    }

    entries.push(entry.clone());
    save_log_entries(&entries)?;
    Ok(entry)
}
