use anyhow::{Result, bail};
use std::path::PathBuf;

pub fn run_layout_visualize_mode(
    _pdf_path: PathBuf,
    _page_range: Option<String>,
    _target_height: u32,
    _config: crate::types::AppConfig,
) -> Result<()> {
    bail!("layout visualization is unavailable because layout detection was not compiled in")
}

pub fn run_layout_visualize_image(_image_path: PathBuf, _target_height: u32) -> Result<()> {
    bail!("layout visualization is unavailable because layout detection was not compiled in")
}

pub fn run_image_debug_mode(
    _image_path: PathBuf,
    _target_height: Option<u32>,
    _binarization: Option<crate::color::BinarizationConfig>,
    _invert_input: bool,
    _output_dir: Option<PathBuf>,
) -> Result<()> {
    bail!("image debug is unavailable because layout detection was not compiled in")
}
