// Processing log GUI facade. The persistent log is owned by `lege::processing_log`.

use anyhow::Result;
use std::path::PathBuf;

use crate::models::{
    CompressionType as GuiCompressionType, CoverImageType as GuiCoverImageType,
    ImageProcessingType as GuiImageProcessingType, OcrMode as GuiOcrMode,
    OutputFormat as GuiOutputFormat, ProcessingOptions as GuiProcessingOptions,
};
use lege::processing_log as shared_log;

pub use shared_log::{LogEntry, ProcessingResult};

pub fn load_log_entries() -> Result<Vec<LogEntry>> {
    shared_log::load_log_entries()
}

pub fn reconcile_started_entries() -> Result<Vec<LogEntry>> {
    shared_log::reconcile_started_entries()
}

pub fn clear_log_entries() -> Result<()> {
    shared_log::save_log_entries(&[])
}

pub fn add_failed_log_entry(
    input_path: PathBuf,
    output_path: PathBuf,
    original_size: u64,
    error: &str,
    options: &GuiProcessingOptions,
) -> Result<LogEntry> {
    let log_options = to_shared_options(options);
    shared_log::add_failed_log_entry(input_path, output_path, original_size, error, &log_options)
}

fn to_shared_options(options: &GuiProcessingOptions) -> shared_log::ProcessingOptions {
    let mut shared = shared_log::ProcessingOptions::new();

    shared.input_path = options.input_path.clone();
    shared.output_path = options.output_path.clone();
    shared.output_format = match options.output_format {
        GuiOutputFormat::Pdf => shared_log::OutputFormat::Pdf,
        GuiOutputFormat::Djvu => shared_log::OutputFormat::Djvu,
        GuiOutputFormat::Epub => shared_log::OutputFormat::Epub,
    };
    shared.compression_type = match options.compression_type {
        GuiCompressionType::Ccitt4 => shared_log::CompressionType::Ccitt4,
        GuiCompressionType::Jbig2 => shared_log::CompressionType::Jbig2,
    };
    shared.cover_image_type = match options.cover_image_type {
        GuiCoverImageType::Jpeg => shared_log::CoverImageType::Jpeg,
        GuiCoverImageType::Jpeg2000 => shared_log::CoverImageType::Jpeg2000,
        GuiCoverImageType::None => shared_log::CoverImageType::None,
    };
    shared.image_processing_type = match options.image_processing_type {
        GuiImageProcessingType::Original => shared_log::ImageProcessingType::Original,
        GuiImageProcessingType::Dithered => shared_log::ImageProcessingType::Dithered,
    };

    shared.ccitt4_dithered_images = options.ccitt4_dithered_images;
    shared.original_cover = options.original_cover;
    shared.target_height = options.target_height;
    shared.target_device = options.target_device.clone();
    shared.page_range = options.page_range.clone();
    shared.no_front_cover = options.no_front_cover;
    shared.png_folder_mode = options.png_folder_mode;
    shared.layout_analysis = options.layout_analysis;
    shared.use_ocr = options.use_ocr;
    shared.slow_ocr = matches!(options.ocr_mode, GuiOcrMode::Thorough);
    shared.high_quality_output = options.high_quality_output;
    shared.jpeg_compat = options.jpeg_compat;
    shared.invert_input = options.invert_input;
    shared.center_margins = options.center_margins;
    shared.crop_margins = options.crop_margins;
    shared.crop_footnotes = options.crop_footnotes;
    shared.crop_free_aspect = options.crop_free_aspect;
    shared.use_heavy_binarization = options.use_heavy_binarization;
    shared.k_factor = options.k_factor;
    shared.use_fixed_threshold = options.use_fixed_threshold;
    shared.threshold_value = options.threshold_value;

    shared
}
