//! API-compatible OCR facade for builds compiled without the `ocr` feature.

use anyhow::{Result, bail};
use image::RgbImage;

#[derive(Debug, Clone)]
pub struct OcrResult {
    pub hocr: String,
    pub plain_text: String,
}

fn unavailable<T>() -> Result<T> {
    bail!("OCR support was not compiled into this build (enable the `ocr` feature)")
}

pub fn run_ocr(
    _image_data: &[u8],
    _width: usize,
    _height: usize,
    _is_binary: bool,
    _language: &str,
) -> Option<OcrResult> {
    None
}

pub fn check_tesseract_availability() -> Result<String, String> {
    Err("OCR support was not compiled into this build".to_string())
}

pub fn check_tesseract_availability_for_language(_language: &str) -> Result<String, String> {
    Err("OCR support was not compiled into this build".to_string())
}

pub fn get_tessdata_path() -> Option<String> {
    None
}

pub fn get_tessdata_path_for_language(_language: &str) -> Option<String> {
    None
}

/// The page-orientation type the pipelines hand to OCR; without OCR only
/// the frame itself is needed.
pub mod orient {
    pub use crate::encoding::straighten::PageFrame;
}

pub mod fast {
    use super::*;
    use crate::engine::Detection;
    use crate::reflow::ReflowPage;

    pub fn should_use_region_ocr(
        _enable_layout_detection: bool,
        _detections: &[Detection],
    ) -> bool {
        false
    }

    pub async fn perform_page_rgb_ocr(
        _page_rgb: &RgbImage,
        _cleaned_gray: Option<&[u8]>,
        _language: &str,
        _invert_input: bool,
        _frame: Option<super::orient::PageFrame>,
    ) -> Result<String> {
        unavailable()
    }

    pub async fn perform_ocr_on_binarized(
        _binarized: Vec<u8>,
        _width: usize,
        _height: usize,
        _language: &str,
    ) -> Result<String> {
        unavailable()
    }

    pub async fn perform_region_based_ocr(
        _binarized: &[u8],
        _page_width: usize,
        _page_height: usize,
        _detections: &[Detection],
        _language: &str,
    ) -> Result<String> {
        unavailable()
    }

    pub async fn perform_tiling_based_ocr(
        _binarized: &[u8],
        _page_width: usize,
        _page_height: usize,
        _language: &str,
    ) -> Result<String> {
        unavailable()
    }

    pub async fn perform_reflow_page_fast_ocr(
        _page: &ReflowPage,
        _composed: &RgbImage,
        _language: &str,
    ) -> Result<Option<String>> {
        unavailable()
    }
}

pub mod slow {
    use super::*;
    use crate::engine::Detection;
    use crate::pipeline::config::PipelineConfig;
    use crate::reflow::{ReflowPage, SourcePageSet};

    #[allow(clippy::too_many_arguments)]
    pub async fn perform_slow_ocr(
        _image: &RgbImage,
        _binarized: &[u8],
        _detections: &[Detection],
        _output_width: u32,
        _output_height: u32,
        _config: &PipelineConfig,
        _page_index: usize,
        _frame: Option<super::orient::PageFrame>,
    ) -> Result<Option<String>> {
        unavailable()
    }

    pub async fn perform_reflow_page_ocr(
        _page: &ReflowPage,
        _sources: &SourcePageSet,
        _config: &PipelineConfig,
    ) -> Result<Option<String>> {
        unavailable()
    }
}
