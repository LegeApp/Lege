use anyhow::Result;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

// Import dbglog macro for debug logging
#[allow(unused_imports)]
use crate::dbglog;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CliText {
    pub colors: ColorConfig,
    pub app: AppText,
    pub main: MainCliText,
    pub interactive: InteractiveText,
    pub processing: ProcessingText,
    pub progress: ProgressText,
    pub system_status: SystemStatusText,
    pub errors: ErrorText,
    pub indicators: IndicatorText,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MainCliText {
    pub version_line: String,
    pub internal_version_line: String,
    pub usage_block: String,
    pub debug_help_block: String,
    pub env_variables_help_block: String,
    pub target_profiles_header: String,
    pub target_profiles_custom_note: String,
    pub target_profiles_page_range_note: String,
    pub simple_mode_header: String,
    pub simple_mode_files_queued: String,
    pub simple_mode_file_item: String,
    pub simple_mode_page_range: String,
    pub simple_mode_settings: String,
    pub simple_mode_output_directory: String,
    pub simple_mode_footer: String,
    pub simple_mode_batch_item: String,
    pub simple_mode_batch_output: String,
    pub simple_mode_input: String,
    pub simple_mode_output: String,
    pub simple_mode_batch_completed: String,
    pub simple_mode_error_processing: String,
    pub run_cli_input_prompt: String,
    pub ocr_check_warning: String,
    pub binarization_choices_template: String,
    pub binarization_forced_fixed_note: String,
    pub processing_options_retry: String,
    pub precedence_force_crop_note: String,
    pub precedence_crop_over_center_note: String,
    pub margin_without_layout_note: String,
    pub footnote_override_note: String,
    pub selected_options_text_encoding: String,
    pub selected_options_image_format: String,
    pub selected_options_dithering: String,
    pub selected_options_original_quality: String,
    pub selected_options_high_quality_output: String,
    pub selected_options_target_output: String,
    pub selected_options_layout_detection: String,
    pub selected_options_note: String,
    pub selected_options_reason: String,
    pub selected_options_ocr_enabled: String,
    pub selected_options_no_cover_page: String,
    pub selected_options_invert_input: String,
    pub selected_options_deskew_enabled: String,
    pub selected_options_margin_processing: String,
    pub selected_options_force_crop: String,
    pub selected_options_max_retries: String,
    pub selected_options_retry_delay: String,
    pub selected_options_footer: String,
    pub config_set_text_format_failed: String,
    pub config_set_djvu_quality_failed: String,
    pub target_device_custom_with_hw: String,
    pub target_device_render_height_error: String,
    pub target_device_apply_dimensions_error: String,
    pub target_device_set_target_height_error: String,
    pub target_device_invalid_spec_error: String,
    pub image_folder_mode_title: String,
    pub image_folder_mode_input: String,
    pub image_folder_mode_deskew: String,
    pub image_folder_mode_description: String,
    pub pdf_to_png_mode_title: String,
    pub pdf_to_png_mode_input: String,
    pub pdf_to_png_mode_target_height: String,
    pub pdf_to_png_mode_page_range: String,
    pub pdf_to_png_mode_all_pages: String,
    pub system_storage: String,
    pub system_storage_line: String,
    pub system_hardware_acceleration_label: String,
    pub system_hardware_acceleration_enabled: String,
    pub system_hardware_acceleration_disabled: String,
    pub system_config_file: String,
    pub system_config_found: String,
    pub system_config_missing: String,
    pub system_footer_divider: String,
    pub target_profile_item: String,
    pub target_profile_proportional: String,
    pub progress_complete: String,
    pub progress_error: String,
    pub history_log_warning: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ColorConfig {
    #[serde(default)]
    pub comment: String,
    pub prompt: String,
    pub info: String,
    pub highlight: String,
    pub page_start: String,
    pub page_complete: String,
    pub ocr: String,
    pub render: String,
    pub detect: String,
    pub encode: String,
    pub worker: String,
    pub dag: String,
    pub status_label: String,
    pub detail_label: String,
    pub reset: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppText {
    pub title: String,
    pub welcome: String,
    pub subtitle: String,
    pub main_title: String,
    pub file_prompt: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InteractiveText {
    pub step1_title: String,
    pub step2_title: String,
    pub step3_title: String,
    pub processing_options_title: String,
    pub format_prompt: String,
    pub modifiers_prompt: String,
    pub examples_prompt: String,
    pub default_prompt: String,
    pub binarization_title: String,
    pub binarization_no_layout_note: String,
    pub binarization_advanced: String,
    pub binarization_prompt: String,
    pub target_device_title: String,
    pub target_device_default: String,
    pub target_device_custom: String,
    pub selected_options_title: String,
    pub options: OptionsText,
    pub binarization_methods: Vec<String>,
    pub encoding_formats: EncodingFormatsText,
    pub prompts: PromptsText,
    pub messages: MessagesText,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OptionsText {
    pub cover_images: String,
    pub ocr_text_layer: String,
    pub image_dithering: String,
    pub layout_detection: String,
    pub height_label: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EncodingFormatsText {
    pub ccitt4_jpeg_fastest: String,
    pub jbig2_jpeg_modern: String,
    pub ccitt4_jp2_compression: String,
    pub jbig2_jp2_best: String,
    pub jbig2_jp2_color: String,
    pub ccitt4_jp2_color: String,
    pub ccitt4_text_fastest: String,
    pub jbig2_text_best: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PromptsText {
    pub file_path: String,
    pub select_method: String,
    pub select_format: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessagesText {
    pub starting_processing: String,
    pub page_range_found: String,
    pub page_range_using: String,
    pub ocr_unavailable: String,
    pub file_validation_error: String,
    pub ocr_detected: String,
    pub ocr_detected_msg: String,
    pub ocr_detected_tip: String,
    pub no_ocr_detected: String,
    pub no_ocr_detected_msg: String,
    pub layout_disabled_inverted: String,
    pub layout_disabled_reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProcessingText {
    pub doc_processing: String,
    pub input_size: String,
    pub output_size: String,
    pub processing_completed: String,
    pub output_label: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProgressText {
    pub percentage: String,
    pub complete: String,
    pub status_label: String,
    pub detail_label: String,
    pub page_started: String,
    pub page_finished: String,
    pub encoding_complete: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemStatusText {
    pub title: String,
    pub divider: String,
    pub checking: String,
    pub tesseract_found: String,
    pub tesseract_missing: String,
    pub cuda_available: String,
    pub cuda_unavailable: String,
    pub memory_info: String,
    pub cpu_info: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ErrorText {
    pub file_not_found: String,
    pub invalid_pdf: String,
    pub processing_failed: String,
    pub insufficient_memory: String,
    pub general_error: String,
    pub invalid_format_option: String,
    pub invalid_binarization_method: String,
    pub invalid_start_page: String,
    pub invalid_end_page: String,
    pub invalid_page_range: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IndicatorText {
    pub checked: String,
    pub unchecked: String,
    pub arrow: String,
    pub bullet: String,
}

// Global static instance for easy access
pub static CLI_TEXT: Lazy<CliText> = Lazy::new(|| {
    load_cli_text().unwrap_or_else(|_e| {
        dbglog!(
            "Warning: Failed to load CLI text from JSON: {}. Using fallback text.",
            _e
        );
        default_cli_text()
    })
});

fn load_cli_text() -> Result<CliText> {
    #[cfg(feature = "german")]
    let json_content = include_str!("../language_service/de/cli_text.json");

    #[cfg(not(feature = "german"))]
    let json_content = include_str!("../language_service/en/cli_text.json");

    let cli_text: CliText = serde_json::from_str(json_content)?;
    Ok(cli_text)
}

fn cli_text_asset_error() -> String {
    "CLI text asset error: the compile-time embedded language_service/*/cli_text.json is invalid. Restore or fix that file and rebuild Lege.".to_string()
}

fn default_cli_text() -> CliText {
    let missing = cli_text_asset_error();
    let missing_vec3 = vec![missing.clone(), missing.clone(), missing.clone()];

    CliText {
        colors: ColorConfig {
            comment: String::new(),
            prompt: "\x1b[96m".to_string(),
            info: "\x1b[96m".to_string(),
            highlight: "\x1b[96m".to_string(),
            page_start: "\x1b[94m".to_string(),
            page_complete: "\x1b[92m".to_string(),
            ocr: "\x1b[90m".to_string(),
            render: "\x1b[96m".to_string(),
            detect: "\x1b[93m".to_string(),
            encode: "\x1b[95m".to_string(),
            worker: "\x1b[2;36m".to_string(),
            dag: "\x1b[2;90m".to_string(),
            status_label: "\x1b[2;34m".to_string(),
            detail_label: "\x1b[2;36m".to_string(),
            reset: "\x1b[0m".to_string(),
        },
        app: AppText {
            title: missing.clone(),
            welcome: missing.clone(),
            subtitle: missing.clone(),
            main_title: missing.clone(),
            file_prompt: missing.clone(),
        },
        main: MainCliText {
            version_line: "Lege version {}".to_string(),
            internal_version_line: "Internal version: {}".to_string(),
            usage_block: missing.clone(),
            debug_help_block: missing.clone(),
            env_variables_help_block: missing.clone(),
            target_profiles_header: missing.clone(),
            target_profiles_custom_note: missing.clone(),
            target_profiles_page_range_note: missing.clone(),
            simple_mode_header: missing.clone(),
            simple_mode_files_queued: missing.clone(),
            simple_mode_file_item: missing.clone(),
            simple_mode_page_range: missing.clone(),
            simple_mode_settings: missing.clone(),
            simple_mode_output_directory: missing.clone(),
            simple_mode_footer: missing.clone(),
            simple_mode_batch_item: missing.clone(),
            simple_mode_batch_output: missing.clone(),
            simple_mode_input: missing.clone(),
            simple_mode_output: missing.clone(),
            simple_mode_batch_completed: missing.clone(),
            simple_mode_error_processing: missing.clone(),
            run_cli_input_prompt: ">".to_string(),
            ocr_check_warning: missing.clone(),
            binarization_choices_template: missing.clone(),
            binarization_forced_fixed_note: missing.clone(),
            processing_options_retry: missing.clone(),
            precedence_force_crop_note: missing.clone(),
            precedence_crop_over_center_note: missing.clone(),
            margin_without_layout_note: missing.clone(),
            footnote_override_note: missing.clone(),
            selected_options_text_encoding: missing.clone(),
            selected_options_image_format: missing.clone(),
            selected_options_dithering: missing.clone(),
            selected_options_original_quality: missing.clone(),
            selected_options_high_quality_output: missing.clone(),
            selected_options_target_output: missing.clone(),
            selected_options_layout_detection: missing.clone(),
            selected_options_note: missing.clone(),
            selected_options_reason: missing.clone(),
            selected_options_ocr_enabled: missing.clone(),
            selected_options_no_cover_page: missing.clone(),
            selected_options_invert_input: missing.clone(),
            selected_options_deskew_enabled: missing.clone(),
            selected_options_margin_processing: missing.clone(),
            selected_options_force_crop: missing.clone(),
            selected_options_max_retries: missing.clone(),
            selected_options_retry_delay: missing.clone(),
            selected_options_footer: missing.clone(),
            config_set_text_format_failed: missing.clone(),
            config_set_djvu_quality_failed: missing.clone(),
            target_device_custom_with_hw: missing.clone(),
            target_device_render_height_error: missing.clone(),
            target_device_apply_dimensions_error: missing.clone(),
            target_device_set_target_height_error: missing.clone(),
            target_device_invalid_spec_error: missing.clone(),
            image_folder_mode_title: missing.clone(),
            image_folder_mode_input: missing.clone(),
            image_folder_mode_deskew: missing.clone(),
            image_folder_mode_description: missing.clone(),
            pdf_to_png_mode_title: missing.clone(),
            pdf_to_png_mode_input: missing.clone(),
            pdf_to_png_mode_target_height: missing.clone(),
            pdf_to_png_mode_page_range: missing.clone(),
            pdf_to_png_mode_all_pages: missing.clone(),
            system_storage: missing.clone(),
            system_storage_line: missing.clone(),
            system_hardware_acceleration_label: missing.clone(),
            system_hardware_acceleration_enabled: missing.clone(),
            system_hardware_acceleration_disabled: missing.clone(),
            system_config_file: missing.clone(),
            system_config_found: missing.clone(),
            system_config_missing: missing.clone(),
            system_footer_divider: missing.clone(),
            target_profile_item: missing.clone(),
            target_profile_proportional: missing.clone(),
            progress_complete: missing.clone(),
            progress_error: missing.clone(),
            history_log_warning: missing.clone(),
        },
        interactive: InteractiveText {
            step1_title: missing.clone(),
            step2_title: missing.clone(),
            step3_title: missing.clone(),
            processing_options_title: missing.clone(),
            format_prompt: missing.clone(),
            modifiers_prompt: missing.clone(),
            examples_prompt: missing.clone(),
            default_prompt: missing.clone(),
            binarization_title: missing.clone(),
            binarization_no_layout_note: missing.clone(),
            binarization_advanced: missing.clone(),
            binarization_prompt: missing.clone(),
            target_device_title: missing.clone(),
            target_device_default: missing.clone(),
            target_device_custom: missing.clone(),
            selected_options_title: missing.clone(),
            options: OptionsText {
                cover_images: missing.clone(),
                ocr_text_layer: missing.clone(),
                image_dithering: missing.clone(),
                layout_detection: missing.clone(),
                height_label: missing.clone(),
            },
            binarization_methods: missing_vec3,
            encoding_formats: EncodingFormatsText {
                ccitt4_jpeg_fastest: missing.clone(),
                jbig2_jpeg_modern: missing.clone(),
                ccitt4_jp2_compression: missing.clone(),
                jbig2_jp2_best: missing.clone(),
                jbig2_jp2_color: missing.clone(),
                ccitt4_jp2_color: missing.clone(),
                ccitt4_text_fastest: missing.clone(),
                jbig2_text_best: missing.clone(),
            },
            prompts: PromptsText {
                file_path: missing.clone(),
                select_method: missing.clone(),
                select_format: missing.clone(),
            },
            messages: MessagesText {
                starting_processing: missing.clone(),
                page_range_found: missing.clone(),
                page_range_using: missing.clone(),
                ocr_unavailable: missing.clone(),
                file_validation_error: missing.clone(),
                ocr_detected: missing.clone(),
                ocr_detected_msg: missing.clone(),
                ocr_detected_tip: missing.clone(),
                no_ocr_detected: missing.clone(),
                no_ocr_detected_msg: missing.clone(),
                layout_disabled_inverted: missing.clone(),
                layout_disabled_reason: missing.clone(),
            },
        },
        processing: ProcessingText {
            doc_processing: missing.clone(),
            input_size: missing.clone(),
            output_size: missing.clone(),
            processing_completed: missing.clone(),
            output_label: missing.clone(),
        },
        progress: ProgressText {
            percentage: missing.clone(),
            complete: missing.clone(),
            status_label: missing.clone(),
            detail_label: missing.clone(),
            page_started: missing.clone(),
            page_finished: missing.clone(),
            encoding_complete: missing.clone(),
        },
        system_status: SystemStatusText {
            title: missing.clone(),
            divider: missing.clone(),
            checking: missing.clone(),
            tesseract_found: missing.clone(),
            tesseract_missing: missing.clone(),
            cuda_available: missing.clone(),
            cuda_unavailable: missing.clone(),
            memory_info: missing.clone(),
            cpu_info: missing.clone(),
        },
        errors: ErrorText {
            file_not_found: missing.clone(),
            invalid_pdf: missing.clone(),
            processing_failed: missing.clone(),
            insufficient_memory: missing.clone(),
            general_error: missing.clone(),
            invalid_format_option: missing.clone(),
            invalid_binarization_method: missing.clone(),
            invalid_start_page: missing.clone(),
            invalid_end_page: missing.clone(),
            invalid_page_range: missing.clone(),
        },
        indicators: IndicatorText {
            checked: missing.clone(),
            unchecked: missing.clone(),
            arrow: missing.clone(),
            bullet: missing,
        },
    }
}

// Convenience functions for common formatting patterns
impl CliText {
    pub fn format_page_range_found(&self, range: &str) -> String {
        self.interactive
            .messages
            .page_range_found
            .replace("{}", range)
    }

    pub fn format_page_range_using(&self, range: &str) -> String {
        self.interactive
            .messages
            .page_range_using
            .replace("{}", range)
    }

    pub fn format_doc_processing(&self, filename: &str) -> String {
        self.processing.doc_processing.replace("{}", filename)
    }

    pub fn format_input_size(&self, size: &str) -> String {
        self.processing.input_size.replace("{}", size)
    }

    pub fn format_height_label(&self, height: u32) -> String {
        self.interactive
            .options
            .height_label
            .replace("{}", &height.to_string())
    }

    pub fn format_percentage(&self, pct: i32) -> String {
        self.progress.percentage.replace("{}", &pct.to_string())
    }

}
