// Freya-side GUI localization loader.
// User-facing copy lives in language_service/<locale>/gui_text.json.

use once_cell::sync::Lazy;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct GuiText {
    pub interactive: GuiInteractiveText,
    // Provider-related texts (ONNX providers, hints)
    pub providers: GuiProvidersText,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiInteractiveText {
    pub app: GuiAppText,
    pub buttons: GuiButtonsText,
    pub labels: GuiLabelsText,
    pub controls: GuiControlsText,
    pub tooltips: GuiTooltipsText,
    pub status: GuiStatusText,
    pub messages: GuiMessagesText,
    pub queue: GuiQueueText,
    pub popups: GuiPopupsText,
    pub progress: GuiProgressText,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiMessagesText {
    pub settings_saved: String,
    pub settings_reset: String,
    pub settings_and_queue_reset: String,
    pub queue_cleared_summary: String,
    pub defaulted_output_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiAppText {
    pub title: String,
    pub window_minimize: String,
    pub window_close: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiButtonsText {
    pub debug: String,
    pub add_file: String,
    pub add_folder: String,
    pub output_directory: String,
    pub save: String,
    pub reset: String,
    pub clear_queue: String,
    pub start_processing: String,
    pub cancel: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiLabelsText {
    pub output_format: String,
    pub base_format: String,
    pub image_output_type: String,
    pub cover_format: String,
    pub layout_detection: String,
    pub inverted_colors: String,
    pub jpeg_compatibility: String,
    pub ocr_text_layer: String,
    pub high_quality_output: String,
    pub page_range: String,
    pub page_range_placeholder: String,
    pub target_height: String,
    pub margin_centering: String,
    pub margin_crop_resize: String,
    pub deskew_documents: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiControlsText {
    pub on: String,
    pub off: String,
    pub binarization: String,
    pub threshold: String,
    pub k_factor: String,
    pub fixed_threshold: String,
    pub heavy_model: String,
    pub target_device: String,
    pub target_device_proportional: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiTooltipsText {
    pub add_file_or_folder: String,
    pub output_format: String,
    pub base_format: String,
    pub image_output_type: String,
    pub layout_detection: String,
    pub heavy_model: String,
    pub inverted_colors: String,
    pub jpeg_compatibility: String,
    pub ocr_text_layer: String,
    pub high_quality_output: String,
    pub cover_format_no_cover: String,
    pub cover_format_dithered: String,
    pub cover_format_original: String,
    pub page_range: String,
    pub target_height: String,
    pub sauvola_window_size: String,
    pub sauvola_k_factor: String,
    pub sauvola_r: String,
    pub threshold_value: String,
    pub margin_centering: String,
    pub margin_crop_resize: String,
    pub deskew_documents: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiStatusText {
    pub ready: String,
    pub cancelling: String,
    pub processing: String,
    pub processing_failed: String,
    pub missing_dependency: String, // new: dynamic placeholder {item}
    pub starting: String,
    pub complete: String,
    pub no_files_in_queue: String,
    pub choose_output_directory: String,
    pub queued_files_for_processing: String,
    pub using_hardware_acceleration: String,
    pub processing_file: String,
    pub file_completed_log: String,
    pub files_processed_successfully: String,
    pub files_remaining_in_queue: String,
    pub queue_empty: String,
    pub error: String,
    pub failed_save_settings: String,
    pub failed_clear_saved_settings: String,
    pub failed_start_processing: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiQueueText {
    pub queue_button: String,
    pub log_button: String,
    pub debug_button: String,
    pub about_button: String,
    pub empty_message: String,
    pub empty_short: String,
    pub item_ready_summary: String,
    pub item_queued_summary: String,
    pub pages: String,
    pub images: String,
    pub input: String,
    pub output: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiPopupsText {
    pub close: String,
    pub supported_inputs_filter: String,
    pub queue_items_title: String,
    pub processing_log_title: String,
    pub debug_log_title: String,
    pub email: String,
    pub documentation: String,
    pub licenses: String,
    pub ocr_detected: String,
    pub no_ocr_detected: String,
    pub layout_disabled: String,
    pub zip_no_supported_images: String,
    pub folder_no_supported_images: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiProgressText {
    pub render: String,
    pub infer: String,
    pub encode: String,
    pub margin: String,
    pub deskew: String,
    pub eta: String,
    pub scanning: String,
    pub reduced: String,
    pub increased: String,
    pub completed: String,
    pub completed_with_estimate: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiProvidersInstallHelp {
    pub openvino_linux: String,
    pub directml_windows: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiProvidersText {
    pub cuda_success: String,
    pub secondary_success: String,
    pub using_cpu: String,
    pub install_help: GuiProvidersInstallHelp,
}

pub static GUI_TEXT: Lazy<GuiText> = Lazy::new(|| {
    load_gui_text().unwrap_or_else(|e| {
        eprintln!("Warning: Failed to load GUI text from JSON: {e}. Using fallback text.");
        default_gui_text()
    })
});

fn load_gui_text() -> anyhow::Result<GuiText> {
    #[cfg(feature = "german")]
    let json_content = include_str!("../../../language_service/de/gui_text.json");

    #[cfg(not(feature = "german"))]
    let json_content = include_str!("../../../language_service/en/gui_text.json");

    Ok(serde_json::from_str(json_content)?)
}

fn default_gui_text() -> GuiText {
    GuiText {
        interactive: GuiInteractiveText {
            app: GuiAppText {
                title: "Lege".to_string(),
                window_minimize: "−".to_string(),
                window_close: "×".to_string(),
            },
            buttons: GuiButtonsText {
                debug: "Debug".to_string(),
                add_file: "Add File".to_string(),
                add_folder: "Add Folder".to_string(),
                output_directory: "Output Directory".to_string(),
                save: "Save".to_string(),
                reset: "Reset".to_string(),
                clear_queue: "Clear Queue".to_string(),
                start_processing: "Process".to_string(),
                cancel: "Cancel".to_string(),
            },
            labels: GuiLabelsText {
                output_format: "Output format:".to_string(),
                base_format: "Base format:".to_string(),
                image_output_type: "Image output type:".to_string(),
                cover_format: "Image format:".to_string(),
                layout_detection: "Layout Detection:".to_string(),
                inverted_colors: "Inverted colors".to_string(),
                jpeg_compatibility: "JPEG Compatibility".to_string(),
                ocr_text_layer: "OCR Text Layer".to_string(),
                high_quality_output: "High quality".to_string(),
                page_range: "Page Range:".to_string(),
                page_range_placeholder: "e.g., 1-10".to_string(),
                target_height: "Target Resolution:".to_string(),
                margin_centering: "Center content on standard page".to_string(),
                margin_crop_resize: "Crop to content and resize".to_string(),
                deskew_documents: "Deskew".to_string(),
            },
            controls: GuiControlsText {
                on: "On".to_string(),
                off: "Off".to_string(),
                binarization: "Binarization".to_string(),
                threshold: "Threshold".to_string(),
                k_factor: "K factor".to_string(),
                fixed_threshold: "Fixed threshold".to_string(),
                heavy_model: "Heavy model".to_string(),
                target_device: "Target device".to_string(),
                target_device_proportional: "Set height, width proportional".to_string(),
            },
            tooltips: GuiTooltipsText {
                add_file_or_folder: "Accepts PDF files, image folders, or image ZIPs.".to_string(),
                output_format: "Choose PDF, or DJVU via native Rust encoding".to_string(),
                base_format: "1bit encoding - JBIG2 has better compression and dithering while CCITT4 has wider compatibility".to_string(),
                image_output_type: "Original color with CCITT4 encoding, or dithered with JBIG2 encoding".to_string(),
                layout_detection: "Enable when your document has complex image areas, GPU accelerated layout detection will preserve them from binarization.".to_string(),
                heavy_model: "Heavy Sauvola AI model (ONNX) for degraded pages. benefits from layout detection.".to_string(),
                inverted_colors: "for digitally created documents with dark background and light text. Will convert to white blackground, black text".to_string(),
                jpeg_compatibility: "use JPEG instead of JPEG2000".to_string(),
                ocr_text_layer: "this adds an optical character recognition layer (HOCR) to each page".to_string(),
                high_quality_output: "Use higher-quality image encoding. PDF uses JPEG quality 95; DJVU uses the highest IW44 setting when enabled. Keep unchecked for outputs intended for e-ink readers.".to_string(),
                cover_format_no_cover: "treat first page same as others; image format affects all non-binarized images".to_string(),
                cover_format_dithered: "CCITT4 text with blue-noise dithered image regions where enabled (global)".to_string(),
                cover_format_original: "keep original color images (global)".to_string(),
                page_range: "Specify page range (e.g., 1-10, 5-20)".to_string(),
                target_height: "Select a preset or use proportional scaling".to_string(),
                sauvola_window_size: "Local analysis window size - larger: smoother, smaller: more detail-sensitive".to_string(),
                sauvola_k_factor: "Contrast sensitivity - lower: more text preserved, higher: cleaner backgrounds. benefits from layout detection.".to_string(),
                sauvola_r: "Variance scaling - lower: less aggressive, higher: stronger adaptation to noise".to_string(),
                threshold_value: "Fixed threshold value (0-255) applied after linearization.".to_string(),
                margin_centering: "Centers content area on specified page output dimensions".to_string(),
                margin_crop_resize: "Crop margins to content bounds and resizes to specified output dimensions".to_string(),
                deskew_documents: "Detect rotation and unwarp page by page before further processing".to_string(),
            },
            status: GuiStatusText {
                ready: "Ready".to_string(),
                cancelling: "Cancelling...".to_string(),
                processing: "Processing...".to_string(),
                processing_failed: "Processing failed - check error details above".to_string(),
                missing_dependency: "Missing required component: {item}".to_string(),
                starting: "[Starting]".to_string(),
                complete: "[Complete]".to_string(),
                no_files_in_queue: "No files in queue".to_string(),
                choose_output_directory: "Choose an output directory".to_string(),
                queued_files_for_processing: "Queued {} file(s) for processing.".to_string(),
                using_hardware_acceleration: "Using hardware acceleration".to_string(),
                processing_file: "Processing file {} of {}".to_string(),
                file_completed_log: "{} file(s) completed, {} remaining".to_string(),
                files_processed_successfully: "{} file(s) processed successfully.".to_string(),
                files_remaining_in_queue: "{} file(s) remaining in queue.".to_string(),
                queue_empty: "Queue is now empty.".to_string(),
                error: "Error: {}".to_string(),
                failed_save_settings: "Failed to save settings: {}".to_string(),
                failed_clear_saved_settings: "Failed to clear saved settings: {}".to_string(),
                failed_start_processing: "Failed to start processing: {}".to_string(),
            },
            messages: GuiMessagesText {
                settings_saved: "Settings saved".to_string(),
                settings_reset: "Settings reset to defaults".to_string(),
                settings_and_queue_reset: "Settings and queue reset ({} files cleared)".to_string(),
                queue_cleared_summary: "Cleared {} file{} from queue".to_string(),
                defaulted_output_dir: "output directory defaulted to input file directory".to_string(),
            },
            queue: GuiQueueText {
                queue_button: "Queue: {}".to_string(),
                log_button: "Log".to_string(),
                debug_button: "Debug".to_string(),
                about_button: "About".to_string(),
                empty_message: "No files queued - Add a file or folder to begin.".to_string(),
                empty_short: "Queue is empty.".to_string(),
                item_ready_summary: "{} item(s) ready  -  {} pages total".to_string(),
                item_queued_summary: "{} item(s) queued".to_string(),
                pages: "pages".to_string(),
                images: "images".to_string(),
                input: "Input: {}".to_string(),
                output: "Output: {}".to_string(),
            },
            popups: GuiPopupsText {
                close: "Close".to_string(),
                supported_inputs_filter: "Supported inputs".to_string(),
                queue_items_title: "Queue Items".to_string(),
                processing_log_title: "Processing Log".to_string(),
                debug_log_title: "Debug Log".to_string(),
                email: "Email".to_string(),
                documentation: "Documentation".to_string(),
                licenses: "Licenses".to_string(),
                ocr_detected: "OCR layer detected - Leave OCR off to preserve the existing text layer.".to_string(),
                no_ocr_detected: "No OCR layer found - Enable OCR to add text recognition.".to_string(),
                layout_disabled: "Layout detection disabled. Text/image separation and image-preserving dithering are unavailable.".to_string(),
                zip_no_supported_images: "{}: ZIP contains no JP2 or JPEG images".to_string(),
                folder_no_supported_images: "{}: folder contains no supported images (JP2, JPEG, PNG, ...)".to_string(),
            },
            progress: GuiProgressText {
                render: "Render".to_string(),
                infer: "Infer".to_string(),
                encode: "Encode".to_string(),
                margin: "Margin".to_string(),
                deskew: "Deskew".to_string(),
                eta: "ETA {}".to_string(),
                scanning: "scanning...".to_string(),
                reduced: "Reduced {}% ({} -> {})".to_string(),
                increased: "Increased {}% ({} -> {})".to_string(),
                completed: "Completed: {}\n{}".to_string(),
                completed_with_estimate: "Completed: {}\n{}\nExtrapolated compression % from selected page range.".to_string(),
            },
        },
        providers: GuiProvidersText {
            cuda_success: "CUDA acceleration enabled".to_string(),
            secondary_success: "Hardware acceleration enabled".to_string(),
            using_cpu: "Using CPU".to_string(),
            install_help: GuiProvidersInstallHelp {
                openvino_linux: "For best performance on Linux, install or enable OpenVINO".to_string(),
                directml_windows: "For best performance on Windows, install DirectML and recent GPU drivers".to_string(),
            },
        }
    }
}
