// lib.rs
// Module declarations
pub mod accumulator;
pub mod app_dirs;
pub mod cli_progress;
pub mod debug_log;
pub mod deskew;
pub mod djvu;  // Native Rust DJVU encoder
pub mod engine;
pub mod gpu;
pub mod errors;
pub mod icon;
pub mod margin;
pub mod nms;
pub mod ocr;
pub mod pagerender;
pub mod pdf_to_png;
pub mod pipeline;         // Modular pipeline actions (includes config, inference, processing)
pub mod pnginference;
pub mod preprocess;
pub mod progress;
pub mod resize;
pub mod resize_context;
pub mod processing_log;
pub mod target_profiles;
pub mod text_loader;
pub mod types;
pub mod unicode_font;
pub mod windows_dirs;
pub use pdf_to_png::run_pdf_to_png_mode;
pub use pnginference::{run_png_mode, run_pdf_layout_crop_debug, DebugCropKind}; // Re-export for CLI debug cropping

// Re-export key types from pipeline submodules for convenience
pub use pipeline::{
    // Config types
    PipelineConfig, PageRange, PageTask, RenderedPageData, ProcessingPipeline,
    runtime_asset_path, runtime_asset_path_if_exists, ensure_pdfium_available,
    // Inference types
    InferenceHandle, InferenceActor, InferenceJob,
    // Helper functions and types
    ShutdownSignal, ShutdownReason, get_available_ram_gb,
    should_treat_as_cover_page, build_hocr_from_pdf_text, is_ocr_available,
    // Other
    prepare_shared_deskew_engine,
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
#[allow(unused_imports)] use fast_image_resize::PixelType;
#[allow(unused_imports)] use fast_image_resize::images::Image as FirImage;
// Action types are re-exported from the top-level pub use pipeline::...
use log::warn;

// ShutdownReason and ShutdownSignal are now in pipeline::helper_functions

/// Returns the external version string (e.g., "1.1.4.0") as set during build time
pub fn get_external_version() -> &'static str {
    option_env!("LEGE_EXTERNAL_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// Returns the internal package version from Cargo.toml
pub fn get_internal_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}










