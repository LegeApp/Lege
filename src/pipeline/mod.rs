// pipeline/mod.rs
// DAGRS pipeline module declarations

pub mod config;
pub mod djvu_pipeline;
pub mod epub_pipeline;
pub mod helper_functions;
pub mod inference;
pub mod page_analysis;
pub mod pdf_tokio_pipeline;
pub mod policies;
pub mod reflow_pipeline;
pub mod runtime_limits;
pub mod source;

// Re-export key types
pub use config::{
    ImageRegionDitherMode, PageRange, PageSelection, PageTask, PipelineConfig, ProcessingPipeline,
    RenderedPageData, ensure_pdfium_available, runtime_asset_path, runtime_asset_path_if_exists,
};
pub use djvu_pipeline::{
    DjvuBinarizedData,
    DjvuInferenceData,
    create_and_run_djvu_pipeline, // Tokio-based DJVU pipeline
    create_and_run_djvu_source_pipeline,
    create_djvu_pipeline_config,
};
pub use helper_functions::{
    PdfWriterHandle, ShutdownReason, ShutdownSignal, WriterMessage, build_hocr_from_pdf_text,
    encode_page_data, encode_region_image, get_available_ram_gb, is_ocr_available,
    rounded_clamped_bbox, should_treat_as_cover_page, spawn_pdf_writer_actor,
};
pub use inference::{InferenceActor, InferenceHandle, InferenceJob};
pub use page_analysis::{
    BLANK_PAGE_FALLBACK_THRESHOLD, PageClassification, classify_page,
    compute_pixel_bounds_for_margin, detections_bbox_area_fraction, is_full_page_image,
    is_visually_blank_page, maybe_apply_yolo_full_page_detection,
    should_force_blank_page_threshold,
};
pub use pdf_tokio_pipeline::{
    PdfInferenceData,
    ProcessedPage,
    create_and_run_pdf_source_pipeline,
    create_and_run_pdf_tokio_pipeline, // New simplified tokio-based PDF pipeline
};
pub use policies::{reset_standard_dimensions, set_standard_dimensions_once};
pub use source::{
    ImageFolderPageSource, PageSource, PdfiumPageSource, collect_largest_sequential_image_run,
};
