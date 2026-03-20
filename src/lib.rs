// lib.rs
// Module declarations
pub mod accumulator;
pub mod app_dirs;
pub mod cli_progress;
pub mod colorquant;
pub mod debug_log;
pub mod deskew;
pub mod djvu; // Native Rust DJVU encoder
#[cfg_attr(target_os = "linux", path = "engine_yolo_linux.rs")]
#[cfg_attr(not(target_os = "linux"), path = "engine.rs")]
pub mod engine;
pub mod errors;
pub mod gpu;
pub mod icon;
pub mod margin;
pub mod nms;
pub mod ocr;
pub mod pagerender;
pub mod pdf_to_png;
pub mod pipeline; // Modular pipeline actions (includes config, inference, processing)
pub mod pnginference;
pub mod preprocess;
pub mod processing_log;
pub mod progress;
pub mod resize;
pub mod resize_context;
pub mod target_profiles;
pub mod text_loader;
pub mod types;
pub mod unicode_font;
pub mod windows_dirs;
pub use pdf_to_png::run_pdf_to_png_mode;
pub use pnginference::{
    DebugCropKind, run_images_to_images_mode, run_pdf_layout_crop_debug, run_pdf_to_images_mode,
    run_png_mode,
}; // Re-export for CLI debug cropping and new image modes

// Re-export TooJPEG functionality for image encoding
pub use Legencode::toojpeg::{EncodeOptions, ImageFormat, encode_jpeg};

// Re-export key types from pipeline submodules for convenience
pub use pipeline::{
    InferenceActor,
    // Inference types
    InferenceHandle,
    InferenceJob,
    PageRange,
    PageTask,
    // Config types
    ImageRegionDitherMode,
    PipelineConfig,
    ProcessingPipeline,
    RenderedPageData,
    ShutdownReason,
    // Helper functions and types
    ShutdownSignal,
    build_hocr_from_pdf_text,
    ensure_pdfium_available,
    get_available_ram_gb,
    is_ocr_available,
    // Other
    prepare_shared_deskew_engine,
    runtime_asset_path,
    runtime_asset_path_if_exists,
    should_treat_as_cover_page,
};

// #[global_allocator]
// static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Profiling log with timing information (enabled only with debug-logging feature)
#[macro_export]
#[cfg(feature = "debug-logging")]
macro_rules! perf_log {
    ($start:expr, $($arg:tt)*) => {{
        use std::time::Instant;
        let duration = $start.elapsed();
        println!(
            "[PERF] {} (took {:?})",
            format_args!($($arg)*),
            duration
        );
    }};
}

/// No-op version of perf_log when debug-logging is disabled
#[macro_export]
#[cfg(not(feature = "debug-logging"))]
macro_rules! perf_log {
    ($start:expr, $($arg:tt)*) => {
        // Prevent unused variable warning for $start
        let _ = $start;
    };
}

// Map legacy logging macros to current helpers
#[cfg(feature = "debug-logging")]
#[macro_export]
macro_rules! info_log {
    ($($arg:tt)*) => { $crate::info_println!($($arg)*) }
}

#[cfg(not(feature = "debug-logging"))]
#[macro_export]
macro_rules! info_log {
    ($($arg:tt)*) => {
        ()
    };
}

#[cfg(feature = "debug-logging")]
#[macro_export]
macro_rules! success_log {
    ($($arg:tt)*) => { $crate::info_println!($($arg)*) }
}

#[cfg(not(feature = "debug-logging"))]
#[macro_export]
macro_rules! success_log {
    ($($arg:tt)*) => {
        ()
    };
}
#[macro_export]
macro_rules! error_log {
    ($($arg:tt)*) => { $crate::error_println!($($arg)*) }
}

#[cfg(feature = "debug-logging")]
#[macro_export]
macro_rules! warn_log {
    ($($arg:tt)*) => {{
        // Don't use colors in macros that may be called from GUI
        println!(
            "[{}] WARNING: {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            format!($($arg)*)
        );
    }};
}

#[cfg(not(feature = "debug-logging"))]
#[macro_export]
macro_rules! warn_log {
    ($($arg:tt)*) => {
        ()
    };
}

#[cfg(feature = "debug-logging")]
#[macro_export]
macro_rules! dprintln {
    ($($arg:tt)*) => {
        /* Temporarily disabled to focus on JP2 debug logs */
    };
}

#[cfg(not(feature = "debug-logging"))]
#[macro_export]
macro_rules! dprintln {
    ($($arg:tt)*) => {
        ()
    };
}

// DAG-related imports
// Added missing imports and type aliases
// brings debug_log!, info_println!, error_println!
// resize params now centralized in resize_context for inference path; keep legacy resize module for other callers
pub use crate::types::{AppConfig, CliConfigBuilder, CoverFormat};
#[allow(unused_imports)]
use fast_image_resize::PixelType;
#[allow(unused_imports)]
use fast_image_resize::images::Image as FirImage;
// Action types are re-exported from the top-level pub use pipeline::...

// ShutdownReason and ShutdownSignal are now in pipeline::helper_functions

/// Returns the external version string (e.g., "1.1.4.0") as set during build time
pub fn get_external_version() -> &'static str {
    option_env!("LEGE_EXTERNAL_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// Returns the internal package version from Cargo.toml
pub fn get_internal_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Configure runtime library environment for dynamically loaded dependencies.
/// This enables wrapper-free Linux packaging when ONNX Runtime is installed
/// under app-private directories such as `/usr/lib/lege`.
pub fn configure_runtime_env() {
    if std::env::var_os("ORT_DYLIB_PATH").is_none() {
        #[cfg(target_os = "windows")]
        let ort_name = "onnxruntime.dll";
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let ort_name = "libonnxruntime.so";
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let ort_name = "libonnxruntime.dylib";

        if let Some(path) = runtime_asset_path_if_exists(ort_name) {
            // Safe: set once at process startup; never touched concurrently.
            unsafe {
                std::env::set_var("ORT_DYLIB_PATH", path);
            }
        }
    }
}
