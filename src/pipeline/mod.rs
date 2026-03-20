// pipeline/mod.rs
// DAGRS pipeline module declarations

pub mod config;
pub mod deskew_graph;
pub mod djvu_pipeline;
pub mod helper_functions;
pub mod inference;
pub mod pdf_tokio_pipeline;
pub mod policies;
pub mod runtime_limits;

// Re-export key types
pub use config::{
    ImageRegionDitherMode, PageRange, PageTask, PipelineConfig, ProcessingPipeline,
    RenderedPageData, ensure_pdfium_available, runtime_asset_path, runtime_asset_path_if_exists,
};
pub use deskew_graph::prepare_shared_deskew_engine;
pub use djvu_pipeline::{
    DjvuBinarizedData,
    DjvuInferenceData,
    create_and_run_djvu_pipeline, // Tokio-based DJVU pipeline
    create_djvu_pipeline_config,
};
pub use helper_functions::{
    PdfWriterHandle, ShutdownReason, ShutdownSignal, WriterMessage, build_hocr_from_pdf_text,
    encode_page_data, encode_region_image, get_available_ram_gb, is_ocr_available,
    rounded_clamped_bbox, should_treat_as_cover_page, spawn_pdf_writer_actor,
};
pub use inference::{InferenceActor, InferenceHandle, InferenceJob};
pub use pdf_tokio_pipeline::{
    PdfInferenceData,
    ProcessedPage,
    create_and_run_pdf_tokio_pipeline, // New simplified tokio-based PDF pipeline
};
pub use policies::{reset_standard_dimensions, set_standard_dimensions_once};
