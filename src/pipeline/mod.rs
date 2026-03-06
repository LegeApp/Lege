// pipeline/mod.rs
// DAGRS pipeline module declarations

pub mod config;
pub mod inference;
pub mod helper_functions;
pub mod pdf_tokio_pipeline;
pub mod deskew_graph;
pub mod djvu_pipeline;
pub mod policies;
pub mod runtime_limits;

// Re-export key types
pub use config::{PipelineConfig, PageRange, PageTask, RenderedPageData, ProcessingPipeline, runtime_asset_path, runtime_asset_path_if_exists, ensure_pdfium_available};
pub use inference::{InferenceHandle, InferenceActor, InferenceJob};
pub use helper_functions::{ShutdownSignal, ShutdownReason, should_treat_as_cover_page, build_hocr_from_pdf_text, encode_region_image, get_available_ram_gb, is_ocr_available, rounded_clamped_bbox, encode_page_data, spawn_pdf_writer_actor, PdfWriterHandle, WriterMessage};
pub use deskew_graph::prepare_shared_deskew_engine;
pub use djvu_pipeline::{
    DjvuInferenceData, DjvuBinarizedData,
    create_djvu_pipeline_config,
    create_and_run_djvu_pipeline, // Tokio-based DJVU pipeline
};
pub use policies::{reset_standard_dimensions, set_standard_dimensions_once};
pub use pdf_tokio_pipeline::{
    create_and_run_pdf_tokio_pipeline, // New simplified tokio-based PDF pipeline
    PdfInferenceData, ProcessedPage,
};
