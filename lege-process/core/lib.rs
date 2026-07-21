// lib.rs
// Module declarations

/// PP-DocLayout-M layout model (fp16), embedded in the binary as a payload so
/// layout detection works with no external asset. A `doclayout.onnx` found next
/// to the executable (or via `LEGE_LAYOUT_MODEL`) overrides this. The runtime
/// upconverts the fp16 weights to f32 at load, so inference speed is unchanged.
#[cfg(feature = "layout-detection")]
pub static EMBEDDED_LAYOUT_MODEL: &[u8] = include_bytes!("../models/doclayout.onnx");

pub mod accumulator;
pub mod app_dirs;
pub mod bbox_trace;
pub mod clean_gray; // Grayscale scanned-page cleanup + MRC split (rounds 1-7)
pub mod cli_progress;
#[path = "../color/mod.rs"]
pub mod color;
#[path = "../colorquant/mod.rs"]
pub mod colorquant;
pub mod debug_log;
pub mod djvu; // Native Rust DJVU encoder
#[path = "../encoding/mod.rs"]
pub mod encoding;
#[cfg(feature = "layout-detection")]
#[cfg_attr(target_os = "linux", path = "engine_impl.rs")]
#[cfg_attr(not(target_os = "linux"), path = "engine.rs")]
pub mod engine;
#[cfg(not(feature = "layout-detection"))]
#[path = "engine_disabled.rs"]
pub mod engine;
pub mod glyphless_font; // Minimal glyphless TTF for the invisible OCR text layer
pub mod hocr; // hOCR parsing + line grouping (moved out of accumulator)
pub mod icon;
#[cfg(feature = "layout-detection")]
pub mod layout_visualize;
#[cfg(not(feature = "layout-detection"))]
#[path = "layout_visualize_disabled.rs"]
pub mod layout_visualize;
pub mod margin;
pub mod nms;
#[cfg(feature = "ocr")]
#[path = "../ocr/mod.rs"]
pub mod ocr;
#[cfg(not(feature = "ocr"))]
#[path = "ocr_disabled.rs"]
pub mod ocr;
pub mod pagerender;
pub mod pdf_artifact; // accumulator::Page -> lege_pdf_write::PdfPageArtifact adapter
pub mod pdf_to_png;
#[path = "../pipeline/mod.rs"]
pub mod pipeline; // Modular pipeline actions (includes config, inference, processing)
pub mod pnginference;
pub mod preprocess;
pub mod processing_log;
pub mod progress;
#[path = "../reflow/mod.rs"]
pub mod reflow; // RasterReflow: raster-first reflow for scanned books (scaffolding)
#[path = "../resize/mod.rs"]
pub mod resize;
pub mod resolution_preset;
// resize context is now merged into pipeline::policies
pub mod text_loader;
pub mod types;
pub mod unicode_font;
pub mod windows_dirs;
pub use layout_visualize::run_layout_visualize_mode;
pub use pdf_to_png::{run_pdf_to_jp2_debug_mode, run_pdf_to_png_mode};
pub use pnginference::{
    DebugCropKind, run_folder_layout_crop_debug, run_images_to_images_mode,
    run_pdf_layout_crop_debug, run_pdf_to_images_mode, run_png_mode, run_png_mode_with_config,
    run_zip_mode_with_config,
}; // Re-export for CLI debug cropping and new image modes

// Re-export common image-encoding functionality.
pub use color::DEFAULT_K_FACTOR;
pub use encoding::toojpeg::{EncodeOptions, ImageFormat, encode_jpeg};

// Re-export key types from pipeline submodules for convenience
pub use pipeline::{
    // Config types
    ImageRegionDitherMode,
    InferenceActor,
    // Inference types
    InferenceHandle,
    InferenceJob,
    PageRange,
    PageSelection,
    PageTask,
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

/// `LEGE_BBOX_TRACE=1` → stderr lines for layout bboxes, region encodes, PDF `Do` placement.
#[macro_export]
macro_rules! bbox_trace {
    ($($arg:tt)*) => {{
        if $crate::bbox_trace::enabled() {
            eprintln!($($arg)*);
        }
    }};
}

// DAG-related imports
// Added missing imports and type aliases
// brings debug_log!, info_println!, error_println!
// resize params now centralized in pipeline::policies for inference path; keep legacy resize module for other callers
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
pub fn configure_runtime_env() {
    configure_rayon_runtime();
}

fn configure_rayon_runtime() {
    static INIT: std::sync::Once = std::sync::Once::new();

    INIT.call_once(|| {
        let threads = rayon_thread_count();
        let mut builder =
            rayon::ThreadPoolBuilder::new().thread_name(|idx| format!("lege-rayon-{idx}"));
        if let Some(threads) = threads {
            builder = builder.num_threads(threads);
        }

        let _ = builder.build_global();
    });
}

fn rayon_thread_count() -> Option<usize> {
    if let Some(threads) = parse_positive_env_usize("LEGE_RAYON_THREADS") {
        return Some(threads);
    }

    if std::env::var_os("RAYON_NUM_THREADS").is_some() {
        return None;
    }

    let cores = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
        .max(1);
    Some(cores.saturating_sub(1).max(1))
}

fn parse_positive_env_usize(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
}
