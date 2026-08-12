//! Processing-log types shared with the GUI front-ends.
//!
//! The type/serialization surface now lives in the standalone `lege-ipc` crate
//! (so the GUIs can use it without linking all of `lege`). This module
//! re-exports it and adds `from_pipeline_config`, the one builder that needs the
//! CLI's `PipelineConfig` and therefore stays here.

pub use lege_ipc::processing_log::*;

use crate::{CoverFormat, PipelineConfig, margin::MarginSettings};

/// Build [`ProcessingOptions`] from a live [`PipelineConfig`]. CLI-side only —
/// the GUI builds its options from its own UI state.
pub fn from_pipeline_config(config: &PipelineConfig) -> ProcessingOptions {
    let mut options = ProcessingOptions::new();

    options.output_format = if matches!(config.text_format(), "djvu") {
        OutputFormat::Djvu
    } else if matches!(config.text_format(), "epub") {
        OutputFormat::Epub
    } else {
        OutputFormat::Pdf
    };

    if options.output_format == OutputFormat::Pdf {
        options.compression_type = match config.text_format() {
            "jbig2" => CompressionType::Jbig2,
            "ccitt4" => CompressionType::Ccitt4,
            _ => CompressionType::Ccitt4,
        };
    }

    options.cover_image_type = match *config.cover_format() {
        CoverFormat::Jpeg => CoverImageType::Jpeg,
        CoverFormat::Jp2 => CoverImageType::Jpeg2000,
        CoverFormat::None | CoverFormat::Ccitt4 | CoverFormat::Jbig2 => CoverImageType::None,
    };

    options.image_processing_type = if config.dither_images() {
        ImageProcessingType::Dithered
    } else {
        ImageProcessingType::Original
    };
    // Infer checkbox value: when text is CCITT4 and dithering is enabled, we treat it as "CCITT4 text with dithered images".
    options.ccitt4_dithered_images =
        matches!(options.image_processing_type, ImageProcessingType::Dithered)
            && matches!(config.text_format(), "ccitt4");

    options.original_cover = config.enable_cover_page();
    options.no_front_cover = config.no_cover_page();
    options.target_height = Some(config.target_height());
    options.layout_analysis = config.enable_layout_detection();
    options.use_ocr = config.enable_ocr();
    options.automatic_toc = config.enable_auto_toc();
    options.slow_ocr = config.slow_ocr_enabled();
    options.high_quality_output = config.high_quality_output();
    options.jpeg_compat = config.jpeg_compat();
    options.invert_input = config.invert_input();

    options.center_margins = matches!(
        config.margin_settings(),
        MarginSettings::StandardizeAndCenter
    );
    options.crop_margins = matches!(config.margin_settings(), MarginSettings::CropAndResize);
    options.crop_footnotes = config.crop_footnotes();
    options.crop_free_aspect = config.crop_free_aspect();

    let bin = config.binarization();
    options.use_heavy_binarization = bin.use_heavy_duty;
    options.k_factor = bin.k_factor;
    options.use_fixed_threshold = bin.use_fixed_threshold;
    options.threshold_value = bin.fixed_threshold;

    if let Some(range) = config.page_range() {
        options.page_range = Some(format!("{}-{}", range.start, range.end));
    }

    options
}
