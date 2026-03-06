use once_cell::sync::Lazy;

#[derive(Debug, Clone)]
pub struct GuiText {
    pub interactive: GuiInteractiveText,
    // Provider-related texts (ONNX providers, hints)
    pub providers: GuiProvidersText,
}

#[derive(Debug, Clone)]
pub struct GuiInteractiveText {
    pub app: GuiAppText,
    pub buttons: GuiButtonsText,
    pub labels: GuiLabelsText,
    pub tooltips: GuiTooltipsText,
    pub status: GuiStatusText,
    pub messages: GuiMessagesText,
}

#[derive(Debug, Clone)]
pub struct GuiMessagesText {
    pub settings_saved: String,
    pub settings_reset: String,
    pub settings_and_queue_reset: String,
    pub queue_cleared_summary: String,
    pub defaulted_output_dir: String,
}

#[derive(Debug, Clone)]
pub struct GuiAppText {
    pub title: String,
    pub window_minimize: String,
    pub window_close: String,
}

#[derive(Debug, Clone)]
pub struct GuiButtonsText {
    pub debug: String,
    pub add_files: String,
    pub add_folder: String,
    pub output_directory: String,
    pub save: String,
    pub reset: String,
    pub clear_queue: String,
    pub start_processing: String,
    pub cancel: String,
}

#[derive(Debug, Clone)]
pub struct GuiLabelsText {
    pub output_format: String,
    pub base_format: String,
    pub image_output_type: String,
    pub cover_format: String,
    pub layout_detection: String,
    pub inverted_colors: String,
    pub jpeg_compatibility: String,
    pub ocr_text_layer: String,
    pub pdf_compatibility_mode: String,
    pub high_quality_output: String,
    pub page_range: String,
    pub page_range_placeholder: String,
    pub target_height: String,
    pub margin_centering: String,
    pub margin_crop_resize: String,
    pub deskew_documents: String,
}

#[derive(Debug, Clone)]
pub struct GuiTooltipsText {
    pub output_format: String,
    pub base_format: String,
    pub image_output_type: String,
    pub layout_detection: String,
    pub inverted_colors: String,
    pub jpeg_compatibility: String,
    pub ocr_text_layer: String,
    pub pdf_compatibility_mode: String,
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

#[derive(Debug, Clone)]
pub struct GuiStatusText {
    pub ready: String,
    pub cancelling: String,
    pub processing: String,
    pub processing_failed: String,
    pub missing_dependency: String, // new: dynamic placeholder {item}
}

#[derive(Debug, Clone)]
pub struct GuiProvidersInstallHelp {
    pub openvino_linux: String,
    pub directml_windows: String,
}

#[derive(Debug, Clone)]
pub struct GuiProvidersText {
    pub cuda_success: String,
    pub secondary_success: String,
    pub using_cpu: String,
    pub install_help: GuiProvidersInstallHelp,
}

pub static GUI_TEXT: Lazy<GuiText> = Lazy::new(|| default_gui_text());

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
                add_files: "Add PDF".to_string(),
                add_folder: "Add Page Folder".to_string(),
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
                layout_detection: "Layout Detection".to_string(),
                inverted_colors: "Inverted colors".to_string(),
                jpeg_compatibility: "JPEG Compatibility".to_string(),
                ocr_text_layer: "OCR Text Layer".to_string(),
                pdf_compatibility_mode: "PDF Compatibility Mode".to_string(),
                high_quality_output: "High quality".to_string(),
                page_range: "Page Range:".to_string(),
                page_range_placeholder: "e.g., 1-10".to_string(),
                target_height: "Target Resolution:".to_string(),
                margin_centering: "Center content on standard page".to_string(),
                margin_crop_resize: "Crop to content and resize".to_string(),
                deskew_documents: "Deskew".to_string(),
            },
            tooltips: GuiTooltipsText {
                output_format: "Choose PDF, or DJVU via DJVUlibre".to_string(),
                base_format: "1bit encoding - JBIG2 has better compression and dithering while CCITT4 has wider compatibility".to_string(),
                image_output_type: "Original color with CCITT4 encoding, or dithered with JBIG2 encoding".to_string(),
                layout_detection: "Enable when your document has complex image areas, GPU accelerated layout detection will preserve them from binarization.".to_string(),
                inverted_colors: "for digitally created documents with dark background and light text. Will convert to white blackground, black text".to_string(),
                jpeg_compatibility: "use JPEG instead of JPEG2000".to_string(),
                ocr_text_layer: "this adds an optical character recognition layer (HOCR) to each page".to_string(),
                pdf_compatibility_mode: "disable object streams and compression for better reader support (e.g., Okular)".to_string(),
                high_quality_output: "Use higher-quality image encoding. PDF uses JPEG quality 95; DJVU uses the highest IW44 setting and keeps dithered image areas inside JB2 when enabled. Keep unchecked for outputs intended for e-ink readers.".to_string(),
                cover_format_no_cover: "treat first page same as others; image format affects all non-binarized images".to_string(),
                cover_format_dithered: "CCITT4 text with Bayer 8x8 dithered images (global)".to_string(),
                cover_format_original: "keep original color images (global)".to_string(),
                page_range: "Specify page range (e.g., 1-10, 5-20)".to_string(),
                target_height: "Select a preset or use proportional scaling".to_string(),
                sauvola_window_size: "Local analysis window size - larger: smoother, smaller: more detail-sensitive".to_string(),
                sauvola_k_factor: "Contrast sensitivity - lower: more text preserved, higher: cleaner backgrounds".to_string(),
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
            },
            messages: GuiMessagesText {
                settings_saved: "Settings saved".to_string(),
                settings_reset: "Settings reset to defaults".to_string(),
                settings_and_queue_reset: "Settings and queue reset ({} files cleared)".to_string(),
                queue_cleared_summary: "Cleared {} file{} from queue".to_string(),
                defaulted_output_dir: "output directory defaulted to input file directory".to_string(),
            }
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
