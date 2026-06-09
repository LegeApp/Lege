// Freya-side GUI localization loader.
// User-facing copy lives in language_service/<locale>/gui_text.json.

use once_cell::sync::Lazy;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct GuiText {
    pub interactive: GuiInteractiveText,
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
    pub output_directory: String,
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
    pub output_format: String,
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
    pub copy_email: String,
    pub docs_not_found: String,
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

pub static GUI_TEXT: Lazy<GuiText> = Lazy::new(|| {
    #[cfg(feature = "german")]
    let json = include_str!("../../../language_service/de/gui_text.json");
    #[cfg(not(feature = "german"))]
    let json = include_str!("../../../language_service/en/gui_text.json");
    serde_json::from_str(json).expect("gui_text.json failed to parse")
});
