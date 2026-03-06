// File: src/backend.rs

use anyhow::Result;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::task::spawn;

use crate::models::{
    CompressionType, CoverImageType, ImageProcessingType, OutputFormat, ProcessingOptions,
};

// Import from the main lege crate
pub use Legencode::types::BinarizationConfig;
pub use lege::{CoverFormat, PipelineConfig, target_profiles};

use crate::models::DocumentItem;
use std::collections::VecDeque;
// Re-export settings helpers for convenience
pub use crate::settings::{
    clear_settings as remove_saved_settings, load_settings as load_saved_settings,
    save_settings as persist_settings,
};

/// Information about a spawned processing task
#[derive(Clone, Debug)]
pub struct TrackerInfo {
    pub id: u64,
    pub input_path: PathBuf,
    pub output_path: PathBuf,
}

/// Convert GUI ProcessingOptions to lib.rs PipelineConfig
pub fn gui_options_to_pipeline_config(options: &ProcessingOptions) -> PipelineConfig {
    // Base format logic:
    // - Layout mode: Original → CCITT4, Dithered → JBIG2 (keeps UI simple, avoids bad combinations)
    // - No-layout mode: Use compression_type directly (user sees "Base format" toggle)
    let text_format = if matches!(options.output_format, OutputFormat::Djvu) {
        "djvu".to_string()
    } else if options.layout_analysis {
        // Layout mode: infer from image_processing_type
        match options.image_processing_type {
            ImageProcessingType::Original => "ccitt4".to_string(),
            ImageProcessingType::Dithered => "jbig2".to_string(),
        }
    } else {
        // No-layout mode: use compression_type directly
        match options.compression_type {
            CompressionType::Ccitt4 => "ccitt4".to_string(),
            CompressionType::Jbig2 => "jbig2".to_string(),
        }
    };

    // Image format logic (global for all non-binarized images):
    // - JPEG compatibility => JPEG; otherwise JPEG2000
    // - When None is selected: no special cover page processing
    let cover_format = if matches!(options.output_format, OutputFormat::Djvu) {
        // DjVu branch does not use separate cover image concept
        CoverFormat::None
    } else {
        match options.cover_image_type {
            CoverImageType::None => CoverFormat::None,
            // Force JPEG as the only cover image format in GUI
            _ => CoverFormat::Jpeg,
        }
    };

    // Use layout detection setting from GUI options
    let enable_layout_detection = options.layout_analysis;

    // Dithering logic (global):
    // - If Original image processing is selected: never dither
    // - If Dithered image processing is selected: dither BUT only if layout detection is enabled
    // - Dithering requires layout detection to identify image regions
    let dither_images = matches!(options.image_processing_type, ImageProcessingType::Dithered)
        && enable_layout_detection;

    // Map GUI binarization options to BinarizationConfig
    // Safety layer: invert_input takes precedence over binarization modes
    // When invert is enabled, we use simple binarize-then-invert (no dithering)
    let heavy_enabled = options.use_heavy_binarization && !options.use_fixed_threshold;
    let binarization_config = if options.invert_input {
        // When inverted, use a minimal config - the actual processing uses binarize-then-invert
        BinarizationConfig {
            invert: false,
            invert_input: true,
            k_factor: 0.2,
            use_heavy_duty: false,
            patch_percentage: 0.0,
            no_patch: false,
            use_fixed_threshold: options.use_fixed_threshold,
            fixed_threshold: options.threshold_value,
        }
    } else {
        // Normal binarization when invert is not enabled
        BinarizationConfig {
            invert: false,
            invert_input: false,
            k_factor: options.k_factor,
            use_heavy_duty: heavy_enabled,
            patch_percentage: 0.0,
            no_patch: false,
            use_fixed_threshold: options.use_fixed_threshold,
            fixed_threshold: options.threshold_value,
        }
    };

    let mut config = PipelineConfig::default();

    // Set confidence threshold
    if let Err(e) = config.set_confidence_threshold(0.2) {
        eprintln!("Warning: Failed to set confidence threshold: {}", e);
    }

    // Set NMS threshold
    if let Err(e) = config.set_nms_threshold(0.7) {
        eprintln!("Warning: Failed to set NMS threshold: {}", e);
    }

    match options.target_device.as_deref() {
        Some(device_name) => {
            if let Some(profile) = target_profiles::find_profile(device_name) {
                if let Err(e) = config.set_target_dimensions(profile.width, profile.height) {
                    eprintln!(
                        "Warning: Failed to set target dimensions for {}: {}",
                        device_name, e
                    );
                }
                if let Err(e) = config.set_high_res_render_height(profile.height) {
                    eprintln!("Warning: Failed to set render height: {}", e);
                }
            } else {
                eprintln!(
                    "Warning: Target device '{}' not recognized. Falling back to proportional height.",
                    device_name
                );
                let height = options.target_height.unwrap_or(1200);
                if let Err(e) = config.set_target_height(height) {
                    eprintln!("Warning: Failed to set target height: {}", e);
                }
                if let Err(e) = config.set_high_res_render_height(height) {
                    eprintln!("Warning: Failed to set render height: {}", e);
                }
            }
        }
        None => {
            let height = options.target_height.unwrap_or(1200);
            if let Err(e) = config.set_target_height(height) {
                eprintln!("Warning: Failed to set target height: {}", e);
            }
            if let Err(e) = config.set_high_res_render_height(height) {
                eprintln!("Warning: Failed to set render height: {}", e);
            }
        }
    }

    // Set other options
    config.set_dither_images(dither_images);
    // Ensure "Original" image processing also sets keep_original_images,
    // matching CLI behavior and layout-detection semantics
    let keep_original = matches!(options.image_processing_type, ImageProcessingType::Original);
    config.set_keep_original_images(keep_original);
    config.set_binarization(binarization_config);
    config.set_enable_ocr(options.use_ocr);
    config.set_high_quality_output(options.high_quality_output);
    // Use unified image format alias to keep GUI/CLI consistent
    config.set_image_format(cover_format);

    // Set text format
    if let Err(e) = config.set_text_format(&text_format) {
        eprintln!("Warning: Failed to set text format: {}", e);
        // Fall back to default text format if setting fails
    }

    config.set_enable_layout_detection(enable_layout_detection);
    config.set_enable_deskew(options.deskew_documents);
    if matches!(options.output_format, OutputFormat::Djvu) {
        let iw44_quality = if options.high_quality_output { 100 } else { 75 };
        if let Err(e) = config.set_djvu_iw44_quality(iw44_quality) {
            eprintln!("Warning: Failed to set DjVu quality: {}", e);
        }
    }
    // Ensure pipeline-level invert flag is set so lib.rs uses binarize-then-invert path
    config.set_invert_input(options.invert_input);

    if let Err(e) = config.set_heavy_sauvola_concurrency(5) {
        eprintln!("Warning: Failed to set sauvola concurrency: {}", e);
    }

    config.set_page_range(parse_page_range(&options.page_range));

    // Cover page behavior (PDF only). DjVu ignores cover page processing.
    let page_range_specified = options.page_range.is_some();
    let (enable_cover, no_cover) = if matches!(options.output_format, OutputFormat::Djvu) {
        (false, true)
    } else if page_range_specified {
        (false, true)
    } else if matches!(options.cover_image_type, CoverImageType::None) {
        (false, true)
    } else {
        (true, false)
    };

    config.set_enable_cover_page(enable_cover);
    config.set_no_cover_page(no_cover);
    config.set_pdf_compatibility_mode(options.pdf_compatibility_mode);

    // Set margin processing settings
    let margin_settings = if options.center_margins {
        lege::margin::MarginSettings::StandardizeAndCenter
    } else if options.crop_margins {
        lege::margin::MarginSettings::CropAndResize
    } else {
        lege::margin::MarginSettings::None
    };
    config.set_margin_settings(margin_settings);

    // Pass through crop_footnotes preference
    config.set_crop_footnotes(options.crop_footnotes);

    // Note: quantization option removed - functionality consolidated into invert option

    config
}

/// Check if a PDF file has OCR/text layers by sampling pages
/// Returns Ok(Some(true)) if OCR found, Ok(Some(false)) if no OCR, Ok(None) if not a PDF, Err on failure
pub async fn check_pdf_has_ocr(pdf_path: &PathBuf) -> Result<Option<bool>> {
    use std::sync::Arc;

    if !is_pdf_file(pdf_path) {
        return Ok(None);
    }

    // Read PDF bytes
    let pdf_bytes = match tokio::fs::read(pdf_path).await {
        Ok(bytes) => Arc::from(bytes.into_boxed_slice()),
        Err(e) => return Err(anyhow::anyhow!("Failed to read PDF: {}", e)),
    };

    // Create a minimal renderer just for checking text layers
    let mut raster_cfg = lege::pagerender::RasterConfig::default();
    raster_cfg.render_forms = false;

    let renderer = lege::pagerender::PdfiumRenderer::new_from_bytes(pdf_bytes, raster_cfg)?;

    // Check for OCR using the has_any_text_layer method
    let has_ocr = renderer.has_any_text_layer().await?;

    Ok(Some(has_ocr))
}

/// Simple processing wrapper that uses the new async progress system from progress.rs
pub fn process_pdf_async(
    input_path: PathBuf,
    output_path: PathBuf,
    options: ProcessingOptions,
) -> u64 {
    let pipeline_config = gui_options_to_pipeline_config(&options);
    lege::progress::spawn_file_processing_task(input_path, output_path, pipeline_config)
}

/// Validate that a path is a PDF file
pub fn is_pdf_file(path: &PathBuf) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase() == "pdf")
        .unwrap_or(false)
}

/// Get all PDF files in a directory
pub fn get_pdf_files_in_directory(dir: &PathBuf) -> Result<Vec<PathBuf>> {
    let mut pdf_files = Vec::new();

    if !dir.is_dir() {
        return Ok(pdf_files);
    }

    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && is_pdf_file(&path) {
            pdf_files.push(path);
        }
    }

    pdf_files.sort();
    Ok(pdf_files)
}

/// Supported image extensions for image folder processing
const SUPPORTED_IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "ppm", "pbm", "pgm", "pnm", "tiff", "tif", "bmp",
];

/// Check if a file is a supported image file
pub fn is_image_file(path: &PathBuf) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|ext| SUPPORTED_IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Get all supported image files in a directory
pub fn get_image_files_in_directory(dir: &PathBuf) -> Result<Vec<PathBuf>> {
    let mut image_files = Vec::new();

    if !dir.is_dir() {
        return Ok(image_files);
    }

    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && is_image_file(&path) {
            image_files.push(path);
        }
    }

    image_files.sort();
    Ok(image_files)
}

/// Calculate total size of a path - handles both files and directories
/// For directories (image folders), sums up all supported image file sizes
pub fn calculate_path_size(path: &PathBuf) -> u64 {
    if path.is_file() {
        // Regular file - just get its size
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    } else if path.is_dir() {
        // Directory (image folder) - sum all image file sizes
        get_image_files_in_directory(path)
            .unwrap_or_default()
            .iter()
            .filter_map(|img_path| std::fs::metadata(img_path).ok())
            .map(|m| m.len())
            .sum()
    } else {
        0
    }
}

/// Parse page range string into PageRange format expected by lib.rs
pub fn parse_page_range(page_range: &Option<String>) -> Option<lege::PageRange> {
    if let Some(range_str) = page_range {
        let range_str = range_str.trim();
        if range_str.is_empty() {
            return None;
        }

        if let Some((start_str, end_str)) = range_str.split_once('-') {
            if let (Ok(start), Ok(end)) = (
                start_str.trim().parse::<usize>(),
                end_str.trim().parse::<usize>(),
            ) {
                if start > 0 && end >= start {
                    return Some(lege::PageRange { start, end });
                }
            }
        } else if let Ok(single_page) = range_str.trim().parse::<usize>() {
            if single_page > 0 {
                return Some(lege::PageRange {
                    start: single_page,
                    end: single_page,
                });
            }
        }
    }
    None
}

/// Generate an appropriate output filename based on the input and processing options
pub fn generate_output_filename(
    input_path: &PathBuf,
    output_base: &PathBuf,
    options: &ProcessingOptions,
) -> PathBuf {
    let file_stem = input_path.file_stem().unwrap_or_default().to_string_lossy();
    // Determine container extension and descriptive parts
    let is_djvu = matches!(options.output_format, OutputFormat::Djvu);
    let text_fmt = if is_djvu {
        "djvu"
    } else {
        match &options.image_processing_type {
            ImageProcessingType::Original => "ccitt4",
            ImageProcessingType::Dithered => {
                if options.ccitt4_dithered_images {
                    "ccitt4"
                } else {
                    "jbig2"
                }
            }
        }
    };
    // Unix timestamp
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let filename = if is_djvu {
        format!("{}_processed_{}_{}.djvu", file_stem, text_fmt, ts)
    } else {
        // PDF format: just include the text format, no cover format suffix
        format!("{}_processed_{}_{}.pdf", file_stem, text_fmt, ts)
    };

    if output_base.is_dir() {
        output_base.join(filename)
    } else if output_base.extension().is_some() {
        // If output_base has an extension, use it as the full output path
        output_base.clone()
    } else {
        // If output_base is a directory path without trailing slash
        output_base.join(filename)
    }
}

/// Process image folder using the existing PNG mode functionality
pub fn process_image_folder(
    folder_path: PathBuf,
    output_path: PathBuf,
    options: &ProcessingOptions,
) -> Result<()> {
    // Use the existing run_png_mode function from the main library
    let enable_deskew = options.deskew_documents;

    // Call the image processing function from the main library
    lege::run_png_mode(
        folder_path,
        Some(output_path),
        lege::AppConfig::default(),
        enable_deskew,
    )?;

    Ok(())
}

/// Start async processing for multiple files and return tracker IDs (no upfront provider detection)
pub async fn start_async_processing(
    queue: VecDeque<DocumentItem>,
    options: &ProcessingOptions,
) -> Result<Vec<TrackerInfo>> {
    if queue.is_empty() {
        return Err(anyhow::anyhow!("No files in queue"));
    }

    let output_path = options
        .output_path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No output path specified"))?;

    // Process files using the proper async progress system (no upfront provider detection)
    let mut tracker_infos = Vec::new();

    for document in queue.iter() {
        // Check if this is an image folder (directory with supported images)
        if document.file_path.is_dir() {
            // Check if it contains supported images
            if let Ok(image_files) = get_image_files_in_directory(&document.file_path) {
                if !image_files.is_empty() {
                    // Process as image folder
                    let output_file_path =
                        generate_output_filename(&document.file_path, output_path, options);

                    // Create a tracker via the unified progress manager
                    let manager = lege::progress::get_progress_manager();
                    let tracker = manager.create_tracker();
                    let tracker_id = tracker.task_id();

                    // Process image folder synchronously in a blocking task
                    let folder_path = document.file_path.clone();
                    let output_file_path_clone = output_file_path.clone();
                    let options_clone = options.clone();

                    spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            process_image_folder(
                                folder_path,
                                output_file_path_clone,
                                &options_clone,
                            )
                        })
                        .await;

                        match result {
                            Ok(Ok(())) => {
                                tracker.finish("Image folder processing completed");
                            }
                            Ok(Err(e)) => {
                                tracker.finish_with_error(e);
                            }
                            Err(e) => {
                                let err = anyhow::anyhow!("Processing task panicked: {:?}", e);
                                tracker.finish_with_error(err);
                            }
                        }
                    });

                    tracker_infos.push(TrackerInfo {
                        id: tracker_id,
                        input_path: document.file_path.clone(),
                        output_path: output_file_path,
                    });
                    continue;
                }
            }
        }

        // Process as regular PDF file
        let output_file_path = generate_output_filename(&document.file_path, output_path, options);

        // Use the proper async processing function from progress.rs
        let tracker_id = process_pdf_async(
            document.file_path.clone(),
            output_file_path.clone(),
            options.clone(),
        );

        tracker_infos.push(TrackerInfo {
            id: tracker_id,
            input_path: document.file_path.clone(),
            output_path: output_file_path,
        });
    }

    Ok(tracker_infos)
}

/// Open folder in system file explorer
pub fn open_folder_in_explorer(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        return Err(anyhow::anyhow!("Path does not exist: {}", path.display()));
    }

    let folder_path = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(folder_path)
            .spawn()?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(folder_path)
            .spawn()?;
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(folder_path)
            .spawn()?;
    }

    Ok(())
}

// A helper function to truncate the path from the beginning.
// This preserves the file/folder name which is often more useful.
pub fn truncate_path(path: &std::path::Path, max_len: usize) -> String {
    let path_str = path.to_string_lossy();
    if path_str.len() <= max_len {
        return path_str.to_string();
    }

    // We'll try to preserve the end of the path
    let mut components: Vec<_> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    let mut result = String::new();

    while let Some(part) = components.pop() {
        let separator = if result.is_empty() {
            ""
        } else {
            std::path::MAIN_SEPARATOR_STR
        };
        let new_result = format!("{}{}{}", part, separator, result);
        if new_result.len() + 4 > max_len {
            // +4 for "..." and a separator
            break;
        }
        result = new_result;
    }

    format!("...{}{}", std::path::MAIN_SEPARATOR_STR, result)
}
