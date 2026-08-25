// Freya-side GUI localization loader.
// User-facing copy lives in language_service/<locale>/gui_text.json.

use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Debug, Clone, Deserialize)]
pub struct GuiText {
    pub interactive: GuiInteractiveText,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiInteractiveText {
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
    pub defaulted_output_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiButtonsText {
    pub add_file: String,
    pub add_folder: String,
    pub output_directory: String,
    pub clear_queue: String,
    pub start_processing: String,
    pub cancel: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiLabelsText {
    pub output_format: String,
    pub image_output_type: String,
    pub layout_detection: String,
    pub inverted_colors: String,
    pub jpeg_compatibility: String,
    pub ocr_text_layer: String,
    pub ocr_fast: String,
    pub ocr_thorough: String,
    pub make_epub_also: String,
    pub jbig2_halftone: String,
    pub high_quality_output: String,
    pub page_range: String,
    pub page_range_placeholder: String,
    pub target_height: String,
    pub reflow: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiControlsText {
    pub on: String,
    pub off: String,
    pub binarization: String,
    pub custom_adaptive: String,
    pub fixed_threshold: String,
    pub heavy_model: String,
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
    pub ocr_fast: String,
    pub ocr_thorough: String,
    pub make_epub_also: String,
    pub jbig2_halftone: String,
    pub high_quality_output: String,
    pub target_height: String,
    pub sauvola_k_factor: String,
    pub threshold_value: String,
    pub margin_centering: String,
    pub margin_crop_resize: String,
    pub reflow: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiStatusText {
    pub ready: String,
    pub cancelling: String,
    pub missing_dependency: String, // new: dynamic placeholder {item}
    pub starting: String,
    pub complete: String,
    pub no_files_in_queue: String,
    pub choose_output_directory: String,
    pub queued_files_for_processing: String,
    pub using_hardware_acceleration: String,
    pub file_completed_log: String,
    pub files_processed_successfully: String,
    pub files_remaining_in_queue: String,
    pub queue_empty: String,
    pub error: String,
    pub failed_start_processing: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiQueueText {
    pub queue_button: String,
    pub log_button: String,
    #[cfg(feature = "debug-logging")]
    pub debug_button: String,
    pub about_button: String,
    pub empty_message: String,
    pub empty_short: String,
    pub item_ready_summary: String,
    pub item_queued_summary: String,
    pub input: String,
    pub output: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiPopupsText {
    pub close: String,
    pub clear_log: String,
    pub supported_inputs_filter: String,
    pub queue_items_title: String,
    pub processing_log_title: String,
    #[cfg(feature = "debug-logging")]
    pub debug_log_title: String,
    pub documentation: String,
    pub licenses: String,
    pub ocr_detected: String,
    pub no_ocr_detected: String,
    pub layout_disabled: String,
    pub folder_no_supported_images: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuiProgressText {
    pub eta: String,
    pub reduced: String,
    pub increased: String,
    pub completed: String,
    pub completed_with_estimate: String,
}

pub static GUI_TEXT: LazyLock<GuiText> = LazyLock::new(|| {
    #[cfg(feature = "german")]
    let json = include_str!("../../../language_service/de/gui_text.json");
    #[cfg(not(feature = "german"))]
    let json = include_str!("../../../language_service/en/gui_text.json");
    serde_json::from_str(json).expect("gui_text.json failed to parse")
});

#[cfg(test)]
mod tests {
    use super::GuiText;

    #[test]
    fn bundled_locales_match_the_gui_schema() {
        for json in [
            include_str!("../../../language_service/en/gui_text.json"),
            include_str!("../../../language_service/de/gui_text.json"),
        ] {
            serde_json::from_str::<GuiText>(json).expect("bundled GUI text should deserialize");
        }
    }
}
