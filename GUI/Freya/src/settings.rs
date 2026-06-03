// Freya-side copy of the Dioxus GUI settings module.
// Keep in sync manually until GUI support code is consolidated.

use anyhow::Result;
use std::fs;
use std::path::PathBuf;

use crate::models::ProcessingOptions;

const SETTINGS_FILE_NAME: &str = "settings.json";

fn settings_directory() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Lege")
}

/// Get the path to the settings file in the user data directory
pub fn get_settings_file_path() -> PathBuf {
    let path = settings_directory().join(SETTINGS_FILE_NAME);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    path
}

/// Load saved settings from settings.json; returns None if file doesn't exist, Some(options) if loaded
pub fn load_settings() -> Result<Option<ProcessingOptions>> {
    let settings_path = get_settings_file_path();

    if !settings_path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&settings_path)?;
    let options: ProcessingOptions = serde_json::from_str(&content)?;
    Ok(Some(options))
}

/// Save settings to settings.json
pub fn save_settings(options: &ProcessingOptions) -> Result<()> {
    let settings_path = get_settings_file_path();
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(options)?;
    fs::write(&settings_path, content)?;
    Ok(())
}

/// Clear saved settings by deleting settings.json if present
pub fn clear_settings() -> Result<()> {
    let settings_path = get_settings_file_path();
    if settings_path.exists() {
        fs::remove_file(settings_path)?;
    }
    Ok(())
}
