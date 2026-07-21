//! EPUB compatibility surface for builds without OCR support.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::pipeline::config::PipelineConfig;
use crate::progress::ProgressTracker;

#[derive(Debug, Clone)]
pub struct HocrPage {
    pub page_index: usize,
    pub width_px: u32,
    pub height_px: u32,
    pub hocr: String,
}

fn unavailable<T>() -> Result<T> {
    bail!("EPUB output requires OCR support (enable the `ocr` feature)")
}

#[allow(clippy::too_many_arguments)]
pub async fn create_and_run_epub_pipeline(
    _pdf_bytes: Arc<[u8]>,
    _config: Arc<PipelineConfig>,
    _output_path: &Path,
    _page_range: Option<std::ops::Range<usize>>,
    _progress_tracker: &ProgressTracker,
    _shutdown_rx: tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
    _progress_callback: impl Fn(usize, usize) + Send + Sync + 'static,
) -> Result<()> {
    unavailable()
}

pub fn build_epub_from_hocr_pages(
    _hocr_pages: &[HocrPage],
    _title: &str,
    _output_path: &Path,
) -> Result<()> {
    unavailable()
}
