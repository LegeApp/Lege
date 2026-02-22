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
    pub providers: ProviderText,
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
    pub processing_options_retry: String,
    pub precedence_force_crop_note: String,
    pub precedence_crop_over_center_note: String,
    pub margin_without_layout_note: String,
    pub footnote_override_note: String,
    pub selected_options_text_encoding: String,
    pub selected_options_image_format: String,
    pub selected_options_dithering: String,
    pub selected_options_original_quality: String,
    pub selected_options_target_output: String,
    pub selected_options_layout_detection: String,
    pub selected_options_note: String,
    pub selected_options_reason: String,
    pub selected_options_ocr_enabled: String,
    pub selected_options_no_cover_page: String,
    pub selected_options_pdf_compatibility: String,
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
pub struct ProviderText {
    pub linux: LinuxProviderText,
    pub windows: WindowsProviderText,
    pub install_help: InstallHelpText,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LinuxProviderText {
    pub trying_cuda: String,
    pub cuda_success: String,
    pub cuda_failed: String,
    pub trying_openvino: String,
    pub openvino_success: String,
    pub openvino_failed: String,
    pub trying_webgpu: String,
    pub webgpu_success: String,
    pub webgpu_failed: String,
    pub using_cpu: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WindowsProviderText {
    pub trying_cuda: String,
    pub cuda_success: String,
    pub cuda_failed: String,
    pub trying_directml: String,
    pub directml_success: String,
    pub directml_failed: String,
    pub using_cpu: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InstallHelpText {
    pub nvidia_drivers: String,
    pub openvino_linux: String,
    pub directml_windows: String,
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
    let json_content = include_str!("cli_text.json");
    let cli_text: CliText = serde_json::from_str(json_content)?;
    Ok(cli_text)
}

fn default_cli_text() -> CliText {
    // Fallback text in case JSON loading fails
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
            title: "Lege PDF Processor".to_string(),
            welcome: "Welcome to Lege Interactive Mode".to_string(),
            subtitle: "Let's get your PDF processed in just 3 steps!".to_string(),
            main_title: "=== Lege PDF Processing ===".to_string(),
            file_prompt: "Enter PDF file path with optional page range (e.g., book.pdf 1-10):".to_string(),
        },
        main: MainCliText {
            version_line: "Lege version {}".to_string(),
            internal_version_line: "Internal version: {}".to_string(),
            usage_block: "Lege - Document Processing CLI\n\nInteractive mode: run with no arguments\n".to_string(),
            debug_help_block: "Lege - Debug Modes\n".to_string(),
            env_variables_help_block: "Lege - Environment Variables".to_string(),
            target_profiles_header: "Available target device presets:".to_string(),
            target_profiles_custom_note: "You can also pass custom values like 1440x1920 or a single height (e.g., 1600).".to_string(),
            target_profiles_page_range_note: "Use 'all' as the page-range placeholder when you want to target the entire document.".to_string(),
            simple_mode_header: "=== Simple Processing Mode ===".to_string(),
            simple_mode_files_queued: "Files queued: {}".to_string(),
            simple_mode_file_item: "  - {}".to_string(),
            simple_mode_page_range: "Page range: {}".to_string(),
            simple_mode_settings: "Settings: CCITT4, JPEG cover, {}, threshold 200".to_string(),
            simple_mode_output_directory: "Output directory: {}".to_string(),
            simple_mode_footer: "===============================".to_string(),
            simple_mode_batch_item: "[Batch] {}/{} -> {}".to_string(),
            simple_mode_batch_output: "[Batch] Output -> {}".to_string(),
            simple_mode_input: "Input: {}".to_string(),
            simple_mode_output: "Output: {}".to_string(),
            simple_mode_batch_completed: "[Batch] Completed {}/{} | {} remaining".to_string(),
            simple_mode_error_processing: "Error processing {}: {}".to_string(),
            run_cli_input_prompt: ">".to_string(),
            ocr_check_warning: "Warning: Failed to check for OCR layer: {}".to_string(),
            binarization_choices_template: "[1] {} | [2] {} | [3] {}".to_string(),
            processing_options_retry: "{}\nPlease try again.\n".to_string(),
            precedence_force_crop_note: "Note: 'force-crop' (f) selected. This overrides 'crop-margins' (w) and 'center-margins' (m).".to_string(),
            precedence_crop_over_center_note: "Note: Both 'crop-margins' (w) and 'center-margins' (m) selected. Applying precedence: crop wins.".to_string(),
            margin_without_layout_note: "Margin processing will run without layout detection; using pixel-based margin analysis.".to_string(),
            footnote_override_note: "Footnote-aware overrides will be unavailable without layout detection.".to_string(),
            selected_options_text_encoding: "Text Encoding".to_string(),
            selected_options_image_format: "Image Format".to_string(),
            selected_options_dithering: "Dithering".to_string(),
            selected_options_original_quality: "Original Quality (images)".to_string(),
            selected_options_target_output: "Target Output".to_string(),
            selected_options_layout_detection: "Layout Detection".to_string(),
            selected_options_note: "Note".to_string(),
            selected_options_reason: "Reason".to_string(),
            selected_options_ocr_enabled: "OCR Enabled".to_string(),
            selected_options_no_cover_page: "No Cover Page".to_string(),
            selected_options_pdf_compatibility: "PDF Compatibility".to_string(),
            selected_options_invert_input: "Invert Input".to_string(),
            selected_options_deskew_enabled: "Deskew Enabled".to_string(),
            selected_options_margin_processing: "Margin Processing".to_string(),
            selected_options_force_crop: "Force crop (ignore footnotes)".to_string(),
            selected_options_max_retries: "Max Retries".to_string(),
            selected_options_retry_delay: "Retry Delay".to_string(),
            selected_options_footer: "=====================".to_string(),
            config_set_text_format_failed: "Failed to set text format: {}".to_string(),
            config_set_djvu_quality_failed: "Failed to set DjVu quality: {}".to_string(),
            target_device_custom_with_hw: "Or enter: custom height (e.g., 1600) | WxH (e.g., 1440x1920) | H W (e.g., 1600 1200) | blank for default".to_string(),
            target_device_render_height_error: "Failed to set render height: {}. Try again.".to_string(),
            target_device_apply_dimensions_error: "Failed to apply target dimensions: {}. Try again.".to_string(),
            target_device_set_target_height_error: "Failed to set target height: {}. Try again.".to_string(),
            target_device_invalid_spec_error: "Invalid target specification '{}': {}. Try again.".to_string(),
            image_folder_mode_title: "Image Folder Mode".to_string(),
            image_folder_mode_input: "Input folder: {}".to_string(),
            image_folder_mode_deskew: "Deskew: ENABLED (rotation correction + document unwarping)".to_string(),
            image_folder_mode_description: "This mode processes image files and performs layout detection inference.".to_string(),
            pdf_to_png_mode_title: "PDF to PNG Mode".to_string(),
            pdf_to_png_mode_input: "Input PDF: {}".to_string(),
            pdf_to_png_mode_target_height: "Target height: {}px".to_string(),
            pdf_to_png_mode_page_range: "Page range: {}".to_string(),
            pdf_to_png_mode_all_pages: "Processing all pages".to_string(),
            system_storage: "Storage:".to_string(),
            system_storage_line: "   {}: {} GB available / {} GB total ({}% used)".to_string(),
            system_hardware_acceleration_label: "Hardware acceleration: {}".to_string(),
            system_hardware_acceleration_enabled: "enabled ({})".to_string(),
            system_hardware_acceleration_disabled: "disabled ({})".to_string(),
            system_config_file: "Config file: {} ({})".to_string(),
            system_config_found: "Found".to_string(),
            system_config_missing: "Not found".to_string(),
            system_footer_divider: "========================================".to_string(),
            target_profile_item: "  - {} ({}x{} px)".to_string(),
            target_profile_proportional: "  - {} (keeps proportional width using custom height)".to_string(),
            progress_complete: "[Complete]\n{}".to_string(),
            progress_error: "[Error]\n{}".to_string(),
            history_log_warning: "Warning: failed to write processing log entry: {}".to_string(),
        },
        interactive: InteractiveText {
            step1_title: "Step 1: Processing Options".to_string(),
            step2_title: "Step 2: Binarization Method".to_string(),
            step3_title: "Step 3: Encoding Format".to_string(),
            processing_options_title: "=== Processing Options ===".to_string(),
            format_prompt: "Format: [1] CCITT4 | [2] JBIG2 | [3] DJVU".to_string(),
            modifiers_prompt: "Modifiers: c=Dithered | a=No-layout | b=OCR | d=No-cover | e=PDF-compat | f=Force-crop | g=Invert | h=Deskew | m=Center | w=Crop".to_string(),
            examples_prompt: "Examples: '1' (CCITT4) | '1c' (CCITT4+dither) | '2b' (JBIG2+OCR) | '2cs' (JBIG2+dither+symbol) | '3' (DJVU)".to_string(),
            default_prompt: "Default: 1 (CCITT4, original quality, layout detection on)".to_string(),
            binarization_title: "Binarization method:".to_string(),
            binarization_advanced: "Advanced: Add k=<value> for sensitivity (e.g., '1 k=0.25') | thr=<0-255> for threshold (e.g., '2 thr=200')".to_string(),
            binarization_prompt: "Choose [1-3] (default: 1):".to_string(),
            target_device_title: "Target Device / Resolution:".to_string(),
            target_device_default: "Default ({}px height, proportional)".to_string(),
            target_device_custom: "Or enter: custom height (e.g., 1600) | WxH (e.g., 1440x1920) | blank for default".to_string(),
            selected_options_title: "=== Selected Options ===".to_string(),
            options: OptionsText {
                cover_images: "Cover images".to_string(),
                ocr_text_layer: "OCR text layer".to_string(),
                image_dithering: "Image dithering".to_string(),
                layout_detection: "Layout detection".to_string(),
                height_label: "Height: {}px".to_string(),
            },
            binarization_methods: vec![
                "Adaptive binarization (Sauvola/Otsu fusion)".to_string(),
                "Fixed threshold (manual cutoff, use thr=<value>)".to_string(),
                "Heavy AI model (Sauvola ONNX for degraded docs)".to_string(),
            ],
            encoding_formats: EncodingFormatsText {
                ccitt4_jpeg_fastest: "CCITT4 + JPEG - Fastest".to_string(),
                jbig2_jpeg_modern: "JBIG2 + JPEG - Modern".to_string(),
                ccitt4_jp2_compression: "CCITT4 + JP2 - Good compression".to_string(),
                jbig2_jp2_best: "JBIG2 + JP2 - Best compression".to_string(),
                jbig2_jp2_color: "JBIG2 + JP2 - Best for color".to_string(),
                ccitt4_jp2_color: "CCITT4 + JP2 - Fast + good color".to_string(),
                ccitt4_text_fastest: "CCITT4 Text Only - Fastest".to_string(),
                jbig2_text_best: "JBIG2 Text Only - Best compression".to_string(),
            },
            prompts: PromptsText {
                file_path: "Enter PDF file path".to_string(),
                select_method: "Select method".to_string(),
                select_format: "Select format".to_string(),
            },
            messages: MessagesText {
                starting_processing: "Starting processing...".to_string(),
                page_range_found: "Found page range: {}".to_string(),
                page_range_using: "Using page range: {}".to_string(),
                ocr_unavailable: "OCR not available".to_string(),
                file_validation_error: "File validation error".to_string(),
                ocr_detected: "✓ [OCR Layer Detected]".to_string(),
                ocr_detected_msg: "This PDF contains an existing OCR text layer.".to_string(),
                ocr_detected_tip: "Leave OCR disabled to preserve the existing text layer.".to_string(),
                no_ocr_detected: "⚠ [No OCR Layer Found]".to_string(),
                no_ocr_detected_msg: "Enable OCR if you want to add text recognition.".to_string(),
                layout_disabled_inverted: "Layout detection temporarily disabled for inverted documents".to_string(),
                layout_disabled_reason: "Inverted backgrounds confuse the model, creating large files".to_string(),
            },
        },
        processing: ProcessingText {
            doc_processing: "Processing: {}".to_string(),
            input_size: "Input size: {}".to_string(),
            output_size: "Output size: {} ({:.1}% of original)".to_string(),
            processing_completed: "Processing completed in {:.2}s".to_string(),
            output_label: "Output: {}".to_string(),
        },
        progress: ProgressText {
            percentage: "{}%".to_string(),
            complete: "Processing complete".to_string(),
            status_label: "Status".to_string(),
            detail_label: "Detail".to_string(),
            page_started: "Started page {}".to_string(),
            page_finished: "Finished page {}".to_string(),
            encoding_complete: "Encoding complete".to_string(),
        },
        system_status: SystemStatusText {
            title: "System Status".to_string(),
            divider: "========================================".to_string(),
            checking: "Checking system...".to_string(),
            tesseract_found: "Tesseract OCR: Available".to_string(),
            tesseract_missing: "Tesseract OCR: Not found".to_string(),
            cuda_available: "CUDA: Available".to_string(),
            cuda_unavailable: "CUDA: Not available".to_string(),
            memory_info: "Memory: {} MB available".to_string(),
            cpu_info: "CPU cores: {} available".to_string(),
        },
        providers: ProviderText {
            linux: LinuxProviderText {
                trying_cuda: "Checking NVIDIA GPU...".to_string(),
                cuda_success: "NVIDIA GPU detected".to_string(),
                cuda_failed: "Nvidia CUDA tried, not detected...".to_string(),
                trying_openvino: "Checking OpenVINO...".to_string(),
                openvino_success: "OpenVINO detected".to_string(),
                openvino_failed: "OpenVINO not found, using CPU".to_string(),
                trying_webgpu: "Checking WebGPU acceleration...".to_string(),
                webgpu_success: "Using WebGPU acceleration for ONNX Runtime".to_string(),
                webgpu_failed: "WebGPU acceleration not available, falling back to CPU".to_string(),
                using_cpu: "CPU mode".to_string(),
            },
            windows: WindowsProviderText {
                trying_cuda: "".to_string(),
                cuda_success: "".to_string(),
                cuda_failed: "".to_string(),
                trying_directml: "Checking for hardware acceleration...".to_string(),
                directml_success: "Using hardware acceleration for ONNX Runtime".to_string(),
                directml_failed: "Hardware acceleration not available, falling back to CPU"
                    .to_string(),
                using_cpu: "Using CPU execution for ONNX Runtime".to_string(),
            },
            install_help: InstallHelpText {
                nvidia_drivers: "For NVIDIA: Update drivers".to_string(),
                openvino_linux: "For Intel: Install OpenVINO".to_string(),
                directml_windows: "For acceleration: Update Windows".to_string(),
            },
        },
        errors: ErrorText {
            file_not_found: "Error: File not found - {}".to_string(),
            invalid_pdf: "Error: Invalid PDF file - {}".to_string(),
            processing_failed: "Error: Processing failed - {}".to_string(),
            insufficient_memory: "Error: Insufficient memory".to_string(),
            general_error: "Error: {}".to_string(),
            invalid_format_option:
                "Invalid format option: {}. Use: 1, 2, d, jpeg, jp2, none, nocover".to_string(),
            invalid_binarization_method:
                "Invalid binarization method: {}. Use 1 (adaptive), 2 (fixed threshold), or 3 (heavy Sauvola)".to_string(),
            invalid_start_page: "Invalid start page: {}".to_string(),
            invalid_end_page: "Invalid end page: {}".to_string(),
            invalid_page_range: "Invalid page range format: {}".to_string(),
        },
        indicators: IndicatorText {
            checked: "[X]".to_string(),
            unchecked: "[ ]".to_string(),
            arrow: ">".to_string(),
            bullet: "*".to_string(),
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

    // Helper to get platform-specific provider messages
    pub fn get_provider_messages(&self) -> &dyn ProviderMessages {
        #[cfg(target_os = "linux")]
        return &self.providers.linux;

        #[cfg(target_os = "windows")]
        return &self.providers.windows;

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        return &self.providers.linux; // fallback to linux messages
    }
}

// Trait to abstract platform-specific provider messages
pub trait ProviderMessages {
    fn trying_cuda(&self) -> &str;
    fn cuda_success(&self) -> &str;
    fn cuda_failed(&self) -> &str;
    fn trying_secondary(&self) -> &str;
    fn secondary_success(&self) -> &str;
    fn secondary_failed(&self) -> &str;
    fn using_cpu(&self) -> &str;
}

impl ProviderMessages for LinuxProviderText {
    fn trying_cuda(&self) -> &str {
        &self.trying_cuda
    }
    fn cuda_success(&self) -> &str {
        &self.cuda_success
    }
    fn cuda_failed(&self) -> &str {
        &self.cuda_failed
    }
    fn trying_secondary(&self) -> &str {
        &self.trying_openvino
    }
    fn secondary_success(&self) -> &str {
        &self.openvino_success
    }
    fn secondary_failed(&self) -> &str {
        &self.openvino_failed
    }
    fn using_cpu(&self) -> &str {
        &self.using_cpu
    }
}

impl ProviderMessages for WindowsProviderText {
    fn trying_cuda(&self) -> &str {
        &self.trying_cuda
    }
    fn cuda_success(&self) -> &str {
        &self.cuda_success
    }
    fn cuda_failed(&self) -> &str {
        &self.cuda_failed
    }
    fn trying_secondary(&self) -> &str {
        &self.trying_directml
    }
    fn secondary_success(&self) -> &str {
        &self.directml_success
    }
    fn secondary_failed(&self) -> &str {
        &self.directml_failed
    }
    fn using_cpu(&self) -> &str {
        &self.using_cpu
    }
}
