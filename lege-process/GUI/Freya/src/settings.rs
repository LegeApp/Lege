// Freya-side copy of the Dioxus GUI settings module.
// Keep in sync manually until GUI support code is consolidated.

use anyhow::Result;
use std::fs;
use std::path::PathBuf;

use crate::models::{ProcessingOptions, ResolutionPreset};

const SETTINGS_FILE_NAME: &str = "settings.json";
const RESOLUTION_PRESET_FILE_NAME: &str = "resolution_preset.json";

fn settings_directory() -> PathBuf {
    // Single source of truth for the app folder (%LOCALAPPDATA%\Lege on
    // Windows): settings, worker logs, run logs, and the processing log all
    // live in the one consolidated directory.
    lege_ipc::data_dir()
}

/// Get the path to the settings file in the user data directory
pub fn get_settings_file_path() -> PathBuf {
    let path = settings_directory().join(SETTINGS_FILE_NAME);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    path
}

/// Path of the persistent worker diagnostics log (see
/// `worker_process::append_worker_log`).
pub fn get_worker_log_path() -> PathBuf {
    let path = settings_directory().join("worker.log");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    path
}

pub fn get_resolution_preset_file_path() -> PathBuf {
    let path = settings_directory().join(RESOLUTION_PRESET_FILE_NAME);
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

pub fn load_resolution_preset() -> Result<Option<ResolutionPreset>> {
    let preset_path = get_resolution_preset_file_path();

    if !preset_path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&preset_path)?;
    let preset: ResolutionPreset = serde_json::from_str(&content)?;
    Ok(Some(preset))
}

pub fn save_resolution_preset(preset: &ResolutionPreset) -> Result<()> {
    let preset_path = get_resolution_preset_file_path();
    if let Some(parent) = preset_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(preset)?;
    fs::write(&preset_path, content)?;
    Ok(())
}

/// Remove the stored resolution preset (used when a job runs without a
/// target resolution — the cleared field should stay cleared on next launch).
pub fn clear_resolution_preset() -> Result<()> {
    let preset_path = get_resolution_preset_file_path();
    if preset_path.exists() {
        fs::remove_file(&preset_path)?;
    }
    Ok(())
}
