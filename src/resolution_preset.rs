use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::app_dirs;

const RESOLUTION_PRESET_FILE_NAME: &str = "resolution_preset.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionPreset {
    pub height: u32,
    pub width: Option<u32>,
}

pub fn preset_path() -> PathBuf {
    app_dirs::data_dir().join(RESOLUTION_PRESET_FILE_NAME)
}

pub fn load() -> Result<Option<ResolutionPreset>> {
    let path = preset_path();
    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(path)?;
    let preset = serde_json::from_str(&content)?;
    Ok(Some(preset))
}

pub fn save(preset: ResolutionPreset) -> Result<()> {
    let path = preset_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(&preset)?;
    fs::write(path, content)?;
    Ok(())
}
