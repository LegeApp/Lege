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
