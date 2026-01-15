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
    pub interactive: InteractiveText,
    pub processing: ProcessingText,
    pub progress: ProgressText,
    pub system_status: SystemStatusText,
    pub providers: ProviderText,
    pub errors: ErrorText,
    pub indicators: IndicatorText,
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
            prompt: "\x1b[97m".to_string(),
            info: "\x1b[36m".to_string(),
            highlight: "\x1b[35m".to_string(),
            page_start: "\x1b[94m".to_string(),
            page_complete: "\x1b[92m".to_string(),
            ocr: "\x1b[90m".to_string(),
            render: "\x1b[96m".to_string(),
            detect: "\x1b[93m".to_string(),
            encode: "\x1b[95m".to_string(),
            worker: "\x1b[2;37m".to_string(),
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
            target_device_default: "[0] Default ({}px height, proportional)".to_string(),
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
