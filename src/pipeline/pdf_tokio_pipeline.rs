// pdf_tokio_pipeline.rs
//! Parallel tokio-based PDF pipeline with TRUE concurrency
//!
//! Key changes from original:
//! 1. Inference stage spawns concurrent inference tasks (not sequential)
//! 2. Processing stage spawns concurrent workers (not sequential)
//! 3. Larger channel buffers to enable pipelining
//! 4. FuturesUnordered for concurrent async task management
//!
//! Architecture:
//!   [Render] ──┬──> [Inference Pool] ──┬──> [Process Pool] ──> [PDF Writer]
//!              │    (N concurrent)     │    (M concurrent)
//!              └────────────────────────────────────────────────────────────
//!                          Backpressure via bounded channels

use crate::margin::{DocumentMarginAnalysis, PageMarginInput};
use crate::pagerender::prelude::{PdfiumRenderer, RasterConfig as PdfRasterConfig};
use crate::pipeline::config::{ImageRegionDitherMode, InferenceResult, PipelineConfig, RenderedPageData};
use crate::pipeline::helper_functions::{
    BLANK_PAGE_FALLBACK_THRESHOLD, build_hocr_from_pdf_text, init_encode_semaphore,
    apply_full_bleed_image_bbox_expansion, maybe_expand_sole_image_to_full_page,
    merge_overlapping_image_detections,
    rounded_clamped_bbox, should_force_blank_page_threshold, should_treat_as_cover_page,
    spawn_pdf_writer_actor, wait_for_memory_relief,
};
use crate::pipeline::policies::{
    LayoutRegions, MarginStandardizeAndCenter, NoLayoutFullPage, RegionPolicy,
};
use crate::pipeline::prepare_shared_deskew_engine;
use crate::pipeline::runtime_limits::PipelineRuntimeLimits;
use crate::progress::ProgressTracker;
use crate::pipeline::policies::build_inference_image;
use crate::{info_log, success_log, warn_log};

use Legencode::streamline::Jbig2Mode;
use Legencode::types::BinarizationOptions;
use anyhow::{Result, anyhow};
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use image::RgbImage;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

//==============================================================================
// Data Structures
//==============================================================================

/// Result from inference stage
#[derive(Clone)]
pub struct PdfInferenceData {
    pub rendered: RenderedPageData,
    pub inference_result: InferenceResult,
}

/// Fully processed page ready for PDF writing
#[derive(Clone)]
pub struct ProcessedPage {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub elements: Vec<crate::accumulator::ContentElement>,
    pub hocr_text: Option<String>,
    pub binarized: Vec<u8>,
}

//==============================================================================
// Stage 1: Rendering (mostly unchanged - PDFium is single-threaded)
//==============================================================================

async fn render_stage(
    pdf_renderer: Arc<PdfiumRenderer>,
    config: Arc<PipelineConfig>,
    deskew_engine: Option<Arc<crate::deskew::DeskewEngine>>,
    page_range: std::ops::Range<usize>,
    tx: mpsc::Sender<RenderedPageData>,
    render_count: Arc<AtomicUsize>,
    detect_count: Arc<AtomicUsize>,
    encode_count: Arc<AtomicUsize>,
    progress: ProgressTracker,
    total_pages: usize,
    layout_enabled: bool,
) -> Result<()> {
    info_log!(
        "[PDF-Parallel-Render] Starting render stage for {} pages",
        total_pages
    );

    for page_index in page_range {
        // Memory check - but don't over-check
        if page_index % 10 == 0 {
            wait_for_memory_relief().await;
        }

        // Render page at target resolution
        let rgb_page = pdf_renderer
            .render_page_rgb(
                page_index as u32,
                config.target_height(),
                config.target_width(),
            )
            .await
            .map_err(|e| anyhow!("Failed to render page {}: {}", page_index, e))?;

        // Convert to ImageBuffer
        let mut rendered_image = RgbImage::from_raw(rgb_page.width, rgb_page.height, rgb_page.data)
            .ok_or_else(|| {
                anyhow!(
                    "Failed to create ImageBuffer for page {} ({}x{})",
                    page_index,
                    rgb_page.width,
                    rgb_page.height
                )
            })?;

        // Apply deskew if enabled
        if let Some(engine) = &deskew_engine {
            let engine = engine.clone();
            let image_for_deskew = rendered_image.clone();
            if let Ok(Ok(corrected)) =
                tokio::task::spawn_blocking(move || engine.process_image(&image_for_deskew)).await
            {
                rendered_image = corrected;
            }
        }

        // Set standard dimensions from first page for margin processing
        crate::pipeline::set_standard_dimensions_once(
            rendered_image.width(),
            rendered_image.height(),
        );

        let high_res_arc = Arc::new(rendered_image);

        // Build inference-sized image
        let inference_img = if config.enable_layout_detection() {
            let spec = config.inference_resize_spec();
            build_inference_image(high_res_arc.as_ref(), &spec)
                .unwrap_or_else(|_| (*high_res_arc).clone())
        } else {
            (*high_res_arc).clone()
        };

        let rendered = RenderedPageData {
            index: page_index,
            high_res_image: high_res_arc,
            inference_image: Arc::new(inference_img),
            original_width_pts: rgb_page.original_width_pts,
            original_height_pts: rgb_page.original_height_pts,
        };

        // Send to next stage - this will backpressure if buffer is full
        if tx.send(rendered).await.is_err() {
            break; // Downstream closed
        }

        // Update progress tracking
        let rendered_val = render_count.fetch_add(1, Ordering::Relaxed) + 1;
        // Deskew happens inline during rendering, so deskew count == render count when deskew is enabled
        let deskewed_val = if deskew_engine.is_some() {
            rendered_val
        } else {
            0
        };
        if layout_enabled {
            let detected_val = detect_count.load(Ordering::Relaxed);
            let encoded_val = encode_count.load(Ordering::Relaxed);
            progress.publish_layout_progress(
                rendered_val,
                detected_val,
                encoded_val,
                deskewed_val,
                total_pages,
            );
        } else {
            progress.publish_no_layout_progress(rendered_val, deskewed_val, total_pages);
        }
    }

    info_log!("[PDF-Parallel-Render] Render stage complete");
    Ok(())
}

//==============================================================================
// Stage 2: Inference (PARALLEL - key fix!)
//==============================================================================

/// Inference stage with TRUE concurrency
///
/// Instead of: recv -> infer -> send -> recv -> infer -> send (sequential)
/// Now:        recv -> spawn(infer) -> recv -> spawn(infer) -> collect results -> send
async fn inference_stage_parallel(
    inference_handle: Option<Arc<crate::pipeline::inference::InferenceHandle>>,
    mut rx: mpsc::Receiver<RenderedPageData>,
    tx: mpsc::Sender<PdfInferenceData>,
    detect_count: Arc<AtomicUsize>,
    concurrency: usize,
    render_count: Arc<AtomicUsize>,
    encode_count: Arc<AtomicUsize>,
    progress: ProgressTracker,
    total_pages: usize,
    layout_enabled: bool,
    detection_cache: Arc<Vec<Vec<crate::engine::Detection>>>,
    analysis_width: u32,
) -> Result<()> {
    info_log!(
        "[PDF-Parallel-Infer] Starting inference stage with concurrency={}",
        concurrency
    );

    // Track in-flight inference tasks
    let mut in_flight: FuturesUnordered<BoxFuture<'static, Result<PdfInferenceData>>> =
        FuturesUnordered::new();
    let mut input_exhausted = false;

    loop {
        tokio::select! {
            biased;  // Prioritize completing work over starting new work

            // Collect completed inference results
            Some(result) = in_flight.next(), if !in_flight.is_empty() => {
                match result {
                    Ok(data) => {
                        let detected_val = detect_count.fetch_add(1, Ordering::Relaxed) + 1;
                        if layout_enabled {
                            let rendered_val = render_count.load(Ordering::Relaxed);
                            let encoded_val = encode_count.load(Ordering::Relaxed);
                            progress.publish_layout_progress(rendered_val, detected_val, encoded_val, 0, total_pages);
                        }
                        if tx.send(data).await.is_err() {
                            info_log!("[PDF-Parallel-Infer] Downstream closed, stopping");
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        return Err(anyhow!(
                            "[PDF-Parallel-Infer] Inference task failed: {}",
                            e
                        ));
                    }
                }
            }

            // Accept new work if we have capacity
            recv_result = rx.recv(), if !input_exhausted && in_flight.len() < concurrency => {
                match recv_result {
                    Some(rendered) => {
                        let handle_clone = inference_handle.clone();
                        let cache_clone = detection_cache.clone();
                        let analysis_w = analysis_width;

                        in_flight.push(build_inference_future(
                            handle_clone,
                            rendered,
                            cache_clone,
                            analysis_w,
                        ));
                    }
                    None => {
                        input_exhausted = true;
                        info_log!("[PDF-Parallel-Infer] Input exhausted, draining {} in-flight tasks", in_flight.len());
                    }
                }
            }

            // Input channel closed
            else => {
                // Exit when all work is done
                if input_exhausted && in_flight.is_empty() {
                    break;
                }

                // If we can't make progress, yield
                if in_flight.is_empty() {
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    info_log!("[PDF-Parallel-Infer] Inference stage complete");
    Ok(())
}

fn build_inference_future(
    inference_handle: Option<Arc<crate::pipeline::inference::InferenceHandle>>,
    rendered: RenderedPageData,
    detection_cache: Arc<Vec<Vec<crate::engine::Detection>>>,
    analysis_width: u32,
) -> BoxFuture<'static, Result<PdfInferenceData>> {
    let page_index = rendered.index;

    if let Some(cached) = detection_cache.get(page_index) {
        if !cached.is_empty() && analysis_width > 0 {
            let high_res_width = rendered.high_res_image.width();
            let scale_factor = high_res_width as f32 / analysis_width as f32;
            let mut scaled = cached.clone();
            for det in &mut scaled {
                det.bbox[0] *= scale_factor;
                det.bbox[1] *= scale_factor;
                det.bbox[2] *= scale_factor;
                det.bbox[3] *= scale_factor;
            }

            let inference_result = InferenceResult {
                index: page_index,
                high_res_image: rendered.high_res_image.clone(),
                inference_image: rendered.inference_image.clone(),
                detections: scaled.clone(),
                text_layer: None,
                original_width_pts: rendered.original_width_pts,
                original_height_pts: rendered.original_height_pts,
                has_no_detections: scaled.is_empty(),
            };

            return Box::pin(async move {
                Ok(PdfInferenceData {
                    rendered,
                    inference_result,
                })
            });
        }
    }

    match inference_handle {
        Some(handle) => Box::pin(async move {
            let detections = handle
                .submit(page_index, rendered.inference_image.clone())
                .await?
                .await
                .map_err(|_| anyhow!("Inference actor dropped response"))?
                .unwrap_or_else(|e| {
                    warn_log!("Page {}: inference failed: {}", page_index, e);
                    Vec::new()
                });

            let inference_result = InferenceResult {
                index: page_index,
                high_res_image: rendered.high_res_image.clone(),
                inference_image: rendered.inference_image.clone(),
                detections: detections.clone(),
                text_layer: None,
                original_width_pts: rendered.original_width_pts,
                original_height_pts: rendered.original_height_pts,
                has_no_detections: detections.is_empty(),
            };

            Ok(PdfInferenceData {
                rendered,
                inference_result,
            })
        }) as BoxFuture<'static, Result<PdfInferenceData>>,
        None => Box::pin(async move {
            let inference_result = InferenceResult {
                index: page_index,
                high_res_image: rendered.high_res_image.clone(),
                inference_image: rendered.inference_image.clone(),
                detections: Vec::new(),
                text_layer: None,
                original_width_pts: rendered.original_width_pts,
                original_height_pts: rendered.original_height_pts,
                has_no_detections: true,
            };

            Ok(PdfInferenceData {
                rendered,
                inference_result,
            })
        }),
    }
}

//==============================================================================
// Stage 3: Processing (PARALLEL - key fix!)
//==============================================================================

/// Processing stage with TRUE concurrency
async fn processing_stage_parallel(
    config: Arc<PipelineConfig>,
    pdf_renderer: Arc<PdfiumRenderer>,
    mut rx: mpsc::Receiver<PdfInferenceData>,
    tx: mpsc::Sender<ProcessedPage>,
    encode_count: Arc<AtomicUsize>,
    page_index_offset: usize,
    concurrency: usize,
    render_count: Arc<AtomicUsize>,
    detect_count: Arc<AtomicUsize>,
    progress: ProgressTracker,
    total_pages: usize,
    layout_enabled: bool,
    margin_analysis: Option<Arc<DocumentMarginAnalysis>>,
) -> Result<()> {
    info_log!(
        "[PDF-Parallel-Process] Starting processing stage with concurrency={}",
        concurrency
    );

    let mut in_flight: FuturesUnordered<_> = FuturesUnordered::new();
    let mut input_exhausted = false;

    loop {
        tokio::select! {
            biased;

            // Collect completed processing results
            Some(result) = in_flight.next(), if !in_flight.is_empty() => {
                match result {
                    Ok(Ok(processed_page)) => {
                        let encoded_val = encode_count.fetch_add(1, Ordering::Relaxed) + 1;
                        if layout_enabled {
                            let rendered_val = render_count.load(Ordering::Relaxed);
                            let detected_val = detect_count.load(Ordering::Relaxed);
                            progress.publish_layout_progress(rendered_val, detected_val, encoded_val, 0, total_pages);
                        } else {
                            progress.publish_no_layout_progress(encoded_val, 0, total_pages);
                        }
                        if tx.send(processed_page).await.is_err() {
                            return Ok(());
                        }
                    }
                    Ok(Err(e)) => {
                        return Err(anyhow!("[PDF-Parallel-Process] Processing failed: {}", e));
                    }
                    Err(e) => {
                        return Err(anyhow!("[PDF-Parallel-Process] Task join error: {}", e));
                    }
                }
            }

            // Accept new work if we have capacity
            recv_result = rx.recv(), if !input_exhausted && in_flight.len() < concurrency => {
                match recv_result {
                    Some(inference_data) => {
                        let config_clone = config.clone();
                        let pdf_renderer_clone = pdf_renderer.clone();
                        let margin_analysis_clone = margin_analysis.clone();

                        // Spawn processing task
                        let task = tokio::spawn(async move {
                            process_single_page(
                                config_clone,
                                pdf_renderer_clone,
                                inference_data,
                                page_index_offset,
                                margin_analysis_clone,
                            ).await
                        });

                        in_flight.push(task);
                    }
                    None => {
                        input_exhausted = true;
                    }
                }
            }

            else => {
                if input_exhausted && in_flight.is_empty() {
                    break;
                }
                if in_flight.is_empty() {
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    info_log!("[PDF-Parallel-Process] Processing stage complete");
    Ok(())
}

/// Process a single page (runs in its own task)
async fn process_single_page(
    config: Arc<PipelineConfig>,
    pdf_renderer: Arc<PdfiumRenderer>,
    inference_data: PdfInferenceData,
    page_index_offset: usize,
    margin_analysis: Option<Arc<DocumentMarginAnalysis>>,
) -> Result<ProcessedPage> {
    let page_index = inference_data.inference_result.index;
    let local_index = page_index.saturating_sub(page_index_offset);

    // CPU-heavy work in spawn_blocking
    let config_clone = config.clone();
    let input = PageProcessingInput {
        rendered: inference_data.rendered,
        inference_result: inference_data.inference_result,
        page_index,
        config: config_clone,
        margin_analysis,
    };

    let cpu_result = tokio::task::spawn_blocking(move || process_page_cpu_work(input))
        .await
        .map_err(|e| anyhow!("CPU task panicked: {}", e))??;

    let PageProcessingOutput {
        adjusted_image,
        adjusted_detections,
        binarized,
        width,
        height,
        is_cover_page,
        cover_encoded_data,
        region_processing_results,
    } = cpu_result;

    // Build elements
    let mut elements: Vec<crate::accumulator::ContentElement> = Vec::new();

    if let Some((encoded_data, format)) = cover_encoded_data {
        crate::bbox_trace!(
            "PAGE {} PDF queue cover fullpage fmt={} bytes={}",
            page_index,
            format,
            encoded_data.len()
        );
        elements.push(crate::accumulator::ContentElement {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
            content: crate::accumulator::ContentType::EncodedImage {
                data: Arc::from(encoded_data),
                pixel_width: width as u32,
                pixel_height: height as u32,
                format,
            },
        });
    }

    for region_result in region_processing_results {
        if let Some((encoded_data, format)) = region_result.encoded_data {
            crate::bbox_trace!(
                "PAGE {} PDF queue overlay tl=({},{}) wh=({}x{}) fmt={} bytes={} dither_path={}",
                page_index,
                region_result.region_x,
                region_result.region_y,
                region_result.region_w,
                region_result.region_h,
                format,
                encoded_data.len(),
                region_result.should_dither
            );
            let content = if let Some(global_data) = region_result.encoded_global_data {
                crate::accumulator::ContentType::Jbig2ImageWithGlobals {
                    page_data: Arc::from(encoded_data),
                    global_data: Arc::from(global_data),
                    pixel_width: region_result.region_w,
                    pixel_height: region_result.region_h,
                }
            } else {
                crate::accumulator::ContentType::EncodedImage {
                    data: Arc::from(encoded_data),
                    pixel_width: region_result.region_w,
                    pixel_height: region_result.region_h,
                    format,
                }
            };

            elements.push(crate::accumulator::ContentElement {
                x: region_result.region_x as f32,
                y: region_result.region_y as f32,
                width: region_result.region_w as f32,
                height: region_result.region_h as f32,
                content,
            });
        }
    }

    // OCR or text extraction (can run concurrently with other pages)
    let hocr_text = if config.enable_ocr() {
        perform_ocr(
            &binarized,
            width,
            height,
            &adjusted_detections,
            &config,
            page_index,
        )
        .await?
    } else {
        extract_pdf_text(&pdf_renderer, page_index, &adjusted_image).await?
    };

    // If any region on this page is Abandon and we're using JBIG2 Symbol mode,
    // force the base layer to Generic to avoid Symbol-mode corruption of noisy pixels.
    let force_jbig2_generic = config.text_format() == "jbig2"
        && adjusted_detections
            .iter()
            .any(|d| d.category.force_generic_jbig2());

    // Encode base layer - in "jpeg" mode, encode the full RGB image instead of grayscale
    let base_layer = if config.text_format() == "jpeg" {
        encode_base_layer_for_jpeg_mode(&adjusted_image, &config, page_index).await?
    } else {
        encode_base_layer(&binarized, width, height, &config, page_index, force_jbig2_generic)
            .await?
    };

    elements.insert(
        0,
        crate::accumulator::ContentElement {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
            content: base_layer,
        },
    );

    Ok(ProcessedPage {
        index: local_index,
        width: width as u32,
        height: height as u32,
        elements,
        hocr_text,
        binarized,
    })
}

//==============================================================================
// Helper Functions (copied from original)
//==============================================================================

/// CPU-intensive work for a single page (to be executed in spawn_blocking)
struct PageProcessingInput {
    rendered: RenderedPageData,
    inference_result: InferenceResult,
    page_index: usize,
    config: Arc<PipelineConfig>,
    margin_analysis: Option<Arc<DocumentMarginAnalysis>>,
}

struct RegionProcessingResult {
    detection: crate::engine::Detection,
    region_data: Vec<u8>,
    /// Integer top-left used for extraction, masking, and PDF placement (must stay in sync).
    region_x: u32,
    region_y: u32,
    region_w: u32,
    region_h: u32,
    should_dither: bool,
    processed_for_masking: bool,
    encoded_data: Option<(Vec<u8>, String)>,
    /// JBIG2 globals (pattern dictionary for halftone, symbol dict for Symbol mode).
    /// When present, the overlay must use `Jbig2ImageWithGlobals` in the PDF.
    encoded_global_data: Option<Vec<u8>>,
}

struct PageProcessingOutput {
    adjusted_image: RgbImage,
    adjusted_detections: Vec<crate::engine::Detection>,
    binarized: Vec<u8>,
    width: usize,
    height: usize,
    is_cover_page: bool,
    cover_encoded_data: Option<(Vec<u8>, String)>,
    region_processing_results: Vec<RegionProcessingResult>,
}

/// Consolidated CPU-intensive work for a single page
fn process_page_cpu_work(input: PageProcessingInput) -> Result<PageProcessingOutput> {
    let PageProcessingInput {
        rendered,
        inference_result,
        page_index,
        config,
        margin_analysis,
    } = input;

    // 1. Apply region policy OR document-wide margin analysis
    let (mut adjusted_image, mut adjusted_detections) = if let Some(analysis) = &margin_analysis {
        // Use document-wide margin analysis (2-pass mode)
        apply_margin_analysis_to_page(&rendered, &inference_result, &config, analysis, page_index)?
    } else {
        // Use per-page policy (1-pass mode)
        apply_region_policy(&rendered, &inference_result, &config)?
    };

    // 2. Resize to target height (CPU-heavy: Lanczos3 filtering)
    if config.enable_layout_detection() && adjusted_image.height() != config.target_height() {
        let current_w = adjusted_image.width();
        let current_h = adjusted_image.height();
        let target_h = config.target_height();
        let aspect_ratio = current_w as f32 / current_h as f32;
        let target_w = config
            .target_width()
            .unwrap_or_else(|| (target_h as f32 * aspect_ratio).round() as u32);

        if target_w > 0 && target_h > 0 {
            // Scale detection bboxes
            let sx = target_w as f32 / current_w as f32;
            let sy = target_h as f32 / current_h as f32;
            for det in &mut adjusted_detections {
                det.bbox[0] *= sx;
                det.bbox[1] *= sy;
                det.bbox[2] *= sx;
                det.bbox[3] *= sy;
            }

            // Resize image
            let params = crate::resize::ResizeParams {
                target_width: target_w,
                target_height: target_h,
                method: crate::resize::ResizeMethod::Lanczos3,
                letterbox: false,
                border_value: 0.0,
                swap_rb: false,
            };
            match crate::resize::resize_bytes(
                adjusted_image.as_raw(),
                current_w,
                current_h,
                &params,
                3,
            ) {
                Ok(bytes) => {
                    if let Some(buf) = RgbImage::from_raw(target_w, target_h, bytes) {
                        adjusted_image = buf;
                    }
                }
                Err(e) => {
                    // Fallback resize
                    adjusted_image = image::imageops::resize(
                        &adjusted_image,
                        target_w,
                        target_h,
                        image::imageops::FilterType::Lanczos3,
                    );
                }
            }
        }
    }

    let width = adjusted_image.width() as usize;
    let height = adjusted_image.height() as usize;

    // Use raw post-NMS YOLO bboxes (no full-page expansion).
    // We still merge overlaps to prevent double-encodes.
    if config.enable_layout_detection() {
        let classifier = &crate::types::LABEL_CLASSIFIER;
        merge_overlapping_image_detections(
            &mut adjusted_detections,
            classifier,
            width as u32,
            height as u32,
        );
        crate::bbox_trace!(
            "PAGE {} [layout] page={}x{} expand_full_bleed={} keep_original={} premask_figures={} image_labels={}",
            page_index,
            width,
            height,
            config.expand_full_bleed_figure_bboxes(),
            config.keep_original_images(),
            config.enable_layout_detection()
                && config.text_format() != "jpeg"
                && (config.dither_images() || config.keep_original_images()),
            adjusted_detections.iter().filter(|d| classifier.is_image_label(d)).count()
        );
        if crate::bbox_trace::enabled() {
            for (i, det) in adjusted_detections.iter().enumerate() {
                if classifier.is_image_label(det) {
                    eprintln!(
                        "  PAGE {} img_det[{}] bbox=({:.1},{:.1},{:.1},{:.1}) conf={:.2}",
                        page_index,
                        i,
                        det.bbox[0],
                        det.bbox[1],
                        det.bbox[2],
                        det.bbox[3],
                        det.confidence
                    );
                }
            }
        }
    }

    let force_blank_threshold = should_force_blank_page_threshold(
        &config,
        inference_result.has_no_detections,
        crate::pipeline::helper_functions::is_visually_blank_page(&adjusted_image),
    );

    // 2b. Pre-mask image regions before binarization so Sauvola only sees text.
    // Image content would skew the adaptive threshold calculations.
    // We keep the original `adjusted_image` intact for dithering later.
    let classifier = &crate::types::LABEL_CLASSIFIER;
    // White out figure regions before binarization so Sauvola only sees text content.
    // Detected image areas are later overlaid (original, dithered, or re-encoded),
    // so the bilevel base layer must be blank in those areas.
    let premask_images = config.enable_layout_detection() && config.text_format() != "jpeg";

    let binarization_image: std::borrow::Cow<'_, RgbImage> = if premask_images {
        let mut masked_rgb = adjusted_image.as_raw().clone();
        let w = width as u32;
        let h = height as u32;
        const MASK_PAD: u32 = 3;
        for det in &adjusted_detections {
            if !classifier.is_image_label(det) {
                continue;
            }
            let (ix1, iy1, ix2, iy2) = rounded_clamped_bbox(det.bbox, w, h);
            let mx1 = ix1.saturating_sub(MASK_PAD);
            let my1 = iy1.saturating_sub(MASK_PAD);
            let mx2 = (ix2 + MASK_PAD).min(w);
            let my2 = (iy2 + MASK_PAD).min(h);
            for y in my1..my2 {
                let row_start = (y as usize * width + mx1 as usize) * 3;
                let row_end = (y as usize * width + mx2 as usize) * 3;
                if row_end <= masked_rgb.len() {
                    masked_rgb[row_start..row_end].fill(255);
                }
            }
        }
        std::borrow::Cow::Owned(RgbImage::from_raw(w, h, masked_rgb).unwrap())
    } else {
        std::borrow::Cow::Borrowed(&adjusted_image)
    };

    // 3. Binarize image (CPU-heavy: Sauvola on millions of pixels)
    // In "jpeg" text format mode, skip binarization and use the RGB image directly for base encoding
    let mut binarized = if config.text_format() == "jpeg" {
        binarization_image
            .as_raw()
            .chunks_exact(3)
            .map(|rgb| {
                let r = rgb[0] as f32;
                let g = rgb[1] as f32;
                let b = rgb[2] as f32;
                ((0.299 * r + 0.587 * g + 0.114 * b) as u8).max(1)
            })
            .collect()
    } else {
        binarize_image(&binarization_image, &config, force_blank_threshold)
    };
    drop(binarization_image);

    let is_cover_page = should_treat_as_cover_page(page_index, &config);

    // 4. Handle cover page encoding (before processing regions since it modifies binarized)
    let cover_encoded_data =
        if is_cover_page && *config.cover_format() != crate::types::CoverFormat::None {
            // Use synchronous version for cover page encoding within the blocking task
            let cover_result = encode_region_image_sync(
                adjusted_image.as_raw(),
                width as u32,
                height as u32,
                *config.cover_format(),
                true,
                config.high_quality_output(),
            )
            .map_err(|e| anyhow!("Failed to encode cover image: {}", e))?;

            binarized.fill(255); // Fill binarized with white for cover pages
            Some(cover_result)
        } else {
            None
        };

    // 5. Process all image regions (consolidate all region work into this single blocking call).
    // Stucki (JBIG2) / blue-noise (CCITT4) on image labels only when layout detection is on — never
    // on full-page text / HOCR. JBIG2 `--halftone` selects jbig2halftone.rs at encode time.
    let mut region_processing_results = Vec::new();

    for det in &adjusted_detections {
        if !classifier.is_image_label(det) {
            continue;
        }

        // Check if the detection bbox is valid (not completely outside image bounds)
        let (bbox_x1, bbox_y1, bbox_x2, bbox_y2) =
            rounded_clamped_bbox(det.bbox, width as u32, height as u32);

        // Skip regions that are completely outside the new image bounds after margin correction
        if bbox_x2 <= bbox_x1 || bbox_y2 <= bbox_y1 {
            continue;
        }

        // Single integer-aligned bbox for crop, mask, merge, and PDF placement (avoids 1px drift).
        let exact_bbox = [
            bbox_x1 as f32,
            bbox_y1 as f32,
            bbox_x2 as f32,
            bbox_y2 as f32,
        ];

        // Image-region dithering (Stucki vs halftone) requires layout detection; if layout is off,
        // `image_region_mode` is None and we only mask/crop — full-page binarization is unchanged.
        let image_region_mode = if config.keep_original_images() {
            ImageRegionDitherMode::None
        } else if is_cover_page {
            ImageRegionDitherMode::None
        } else if config.text_format() == "djvu" || config.text_format() == "jpeg" {
            ImageRegionDitherMode::None
        } else if !config.enable_layout_detection() {
            ImageRegionDitherMode::None
        } else {
            config.image_region_dither_mode()
        };

        let should_dither = image_region_mode != ImageRegionDitherMode::None;

        // CPU-heavy: Extract and process region (same bounds as masking / placement)
        let (region_data, region_w, region_h) =
            Legencode::color::color_processing::process_image_region(
                adjusted_image.as_raw(),
                width as u32,
                height as u32,
                exact_bbox,
                image_region_mode,
                config.text_format(),
                false,
            )?;

        let mut processed_for_masking = false;
        let mut encoded_data = None;
        let mut encoded_global_data: Option<Vec<u8>> = None;

        if should_dither && !is_cover_page {
            // Mask the region in the base layer with padding so the overlay fully
            // covers any remnant binarized pixels at the edges.
            let pad: u32 = 3;
            let mx1 = bbox_x1.saturating_sub(pad);
            let my1 = bbox_y1.saturating_sub(pad);
            let mx2 = (bbox_x2 + pad).min(width as u32);
            let my2 = (bbox_y2 + pad).min(height as u32);
            Legencode::color::color_processing::mask_region(
                &mut binarized,
                width as u32,
                [mx1 as f32, my1 as f32, mx2 as f32, my2 as f32],
            );
            processed_for_masking = true;

            let grayscale_data: Vec<u8> = region_data.chunks(3).map(|rgb| rgb[0]).collect();

            if image_region_mode == ImageRegionDitherMode::Halftone
                && config.text_format() == "jbig2"
            {
                // Halftone overlay: grayscale → jbig2halftone.rs (halftone region segments)
                // Invert grayscale so that bright→low pattern index (few dots) and
                // dark→high pattern index (many dots).  Combined with Decode [1, 0]
                // in the PDF this yields black dots on a white default — traditional
                // halftone polarity whose white background blends with the base layer.
                // Also clamp near-white to pure white before inverting to avoid sparse
                // dots from paper texture / scan noise.
                let inverted_gray: Vec<u8> = grayscale_data.iter().map(|&g| {
                    if g >= 245 { 0u8 } else { 255u8 - g }
                }).collect();

                match Legencode::streamline::encode_halftone_region_grayscale(
                    &inverted_gray, region_w, region_h,
                ) {
                    Ok((global_data, page_data)) => {
                        encoded_global_data = Some(global_data);
                        encoded_data = Some((page_data, "jbig2".to_string()));
                    }
                    Err(_e) => {
                        // Fall back to JBIG2 Generic
                        use Legencode::streamline::{
                            EncodingManager, EncodingResult, EncodingSettings,
                            ImageBuffer as LegeImageBuffer, Jbig2Settings,
                        };
                        let buffer = LegeImageBuffer {
                            data: &grayscale_data, width: region_w, height: region_h, channels: 1,
                        };
                        let settings = EncodingSettings::Jbig2(Jbig2Settings {
                            pdf_fragment_mode: true,
                            mode: Jbig2Mode::Generic,
                            use_jbig2_halftone_segments: false,
                        });
                        if let Ok(result) = EncodingManager::encode(&buffer, &settings) {
                            match result {
                                EncodingResult::Standard(data) => {
                                    encoded_data = Some((data, "jbig2".to_string()));
                                }
                                EncodingResult::Jbig2WithGlobals { page_data, .. } => {
                                    encoded_data = Some((page_data, "jbig2".to_string()));
                                }
                            }
                        }
                        // If halftone + generic JBIG2 both fail, fall through to merge below
                    }
                }
            } else {
                // Stucki / Bayer overlay: bilevel data, encode matching the page format
                use Legencode::streamline::{
                    EncodingManager, EncodingResult, EncodingSettings,
                    ImageBuffer as LegeImageBuffer, Jbig2Settings,
                };

                let buffer = LegeImageBuffer {
                    data: &grayscale_data,
                    width: region_w,
                    height: region_h,
                    channels: 1,
                };
                let overlay_settings = match config.text_format() {
                    "ccitt4" => EncodingSettings::Ccitt4,
                    _ => EncodingSettings::Jbig2(Jbig2Settings {
                        pdf_fragment_mode: true,
                        mode: Jbig2Mode::Generic,
                        use_jbig2_halftone_segments: false,
                    }),
                };
                let overlay_fmt = config.text_format().to_string();
                match EncodingManager::encode(&buffer, &overlay_settings) {
                    Ok(EncodingResult::Standard(data)) => {
                        encoded_data = Some((data, overlay_fmt));
                    }
                    Ok(EncodingResult::Jbig2WithGlobals { page_data, global_data: _ }) => {
                        encoded_data = Some((page_data, overlay_fmt));
                    }
                    Err(_e) => {}
                }
            }

            // Masked for overlay but no bitmap produced (encoder failure / halftone fallback miss):
            // merge dithered grayscale into base so the page is not an empty white hole.
            if encoded_data.is_none() {
                Legencode::color::color_processing::merge_dithered_region(
                    &mut binarized,
                    &grayscale_data,
                    width as u32,
                    exact_bbox,
                );
            }
        } else {
            // Mask the region in the base layer with padding so the overlay fully
            // covers any remnant binarized pixels at the edges.
            let pad: u32 = 3;
            let mx1 = bbox_x1.saturating_sub(pad);
            let my1 = bbox_y1.saturating_sub(pad);
            let mx2 = (bbox_x2 + pad).min(width as u32);
            let my2 = (bbox_y2 + pad).min(height as u32);
            Legencode::color::color_processing::mask_region(
                &mut binarized,
                width as u32,
                [mx1 as f32, my1 as f32, mx2 as f32, my2 as f32],
            );
            processed_for_masking = true;

            // For non-dithered regions that need overlay encoding
            if !should_dither {
                encoded_data = Some(
                    encode_region_image_sync(
                        &region_data,
                        region_w,
                        region_h,
                        *config.cover_format(),
                        is_cover_page,
                        config.high_quality_output(),
                    )
                    .map_err(|e| anyhow!("Could not encode image region: {}", e))?,
                );
            }
        }

        if let Some((ref enc, ref fmt)) = encoded_data {
            crate::bbox_trace!(
                "PAGE {} ENCODE xywh=({},{},{},{}) fmt={} bytes={} dither={} halftone_globals={}",
                page_index,
                bbox_x1,
                bbox_y1,
                region_w,
                region_h,
                fmt,
                enc.len(),
                should_dither,
                encoded_global_data.is_some()
            );
        }

        region_processing_results.push(RegionProcessingResult {
            detection: det.clone(),
            region_data,
            region_x: bbox_x1,
            region_y: bbox_y1,
            region_w,
            region_h,
            should_dither,
            processed_for_masking,
            encoded_data,
            encoded_global_data,
        });
    }

    Ok(PageProcessingOutput {
        adjusted_image,
        adjusted_detections,
        binarized,
        width,
        height,
        is_cover_page,
        cover_encoded_data,
        region_processing_results,
    })
}

/// Apply region policy transform (margin correction, layout)
/// Apply document-wide margin analysis to a single page
fn apply_margin_analysis_to_page(
    page: &RenderedPageData,
    inf: &InferenceResult,
    cfg: &PipelineConfig,
    analysis: &DocumentMarginAnalysis,
    page_index: usize,
) -> Result<(RgbImage, Vec<crate::engine::Detection>)> {
    use crate::pipeline::policies::{is_in_inference_space, map_bbox_infer_to_page};

    // Remap detections to page space
    let mut dets = inf.detections.clone();
    let page_w = page.high_res_image.width();
    let page_h = page.high_res_image.height();
    let spec = cfg.inference_resize_spec();
    for d in dets.iter_mut() {
        if is_in_inference_space(&d.bbox, &spec) {
            d.bbox = map_bbox_infer_to_page(d.bbox, page_w, page_h, &spec);
        }
    }

    // CRITICAL FIX: Scale baseline bounds from analysis resolution to processing resolution
    // Analysis runs at 640px width, but processing happens at high resolution.
    // Without this scaling, coordinates are misaligned causing ~20% offset errors.
    let scaled_baseline = analysis.baseline_bounds.scale_to_resolution(
        analysis.analysis_width,
        analysis.analysis_height,
        page_w,
        page_h,
    );

    // Get page-specific margin data from analysis
    let page_data = analysis.pages.get(&page_index);

    // For consistent cropping, use document-wide baseline consistently across all pages
    // - All pages use the same baseline to ensure consistency
    // - This prevents pages with equal margins from being processed differently
    let bounds = if analysis.effective_margin_setting
        == crate::margin::MarginSettings::CropAndResize
    {
        // Calculate per-page bounds for content preservation but only use it to validate against baseline
        let page_bounds = if !dets.is_empty() {
            crate::margin::calculate_content_bounds(&dets, page_w, page_h, true)
        } else {
            crate::margin::compute_pixel_bounds_for_margin(&page.high_res_image, cfg)
        };

        // For consistency across pages, always use the document-wide baseline
        // Only expand if page content significantly exceeds baseline (e.g., page has extra content)
        match page_bounds {
            Some(pb) => {
                // Check if page content significantly exceeds the baseline bounds
                // Use a tolerance to avoid minor variations causing different processing
                const BOUND_TOLERANCE: u32 = 10; // pixels tolerance

                let exceeds_left = pb.min_x < scaled_baseline.min_x.saturating_sub(BOUND_TOLERANCE);
                let exceeds_top = pb.min_y < scaled_baseline.min_y.saturating_sub(BOUND_TOLERANCE);
                let exceeds_right =
                    pb.max_x > scaled_baseline.max_x.saturating_add(BOUND_TOLERANCE);
                let exceeds_bottom =
                    pb.max_y > scaled_baseline.max_y.saturating_add(BOUND_TOLERANCE);

                let final_bounds = if exceeds_left || exceeds_top || exceeds_right || exceeds_bottom
                {
                    // Expand baseline to accommodate content that genuinely exceeds it
                    crate::margin::ContentBounds {
                        min_x: pb.min_x.min(scaled_baseline.min_x),
                        min_y: pb.min_y.min(scaled_baseline.min_y),
                        max_x: pb.max_x.max(scaled_baseline.max_x),
                        max_y: pb.max_y.max(scaled_baseline.max_y),
                    }
                } else {
                    // Use consistent baseline for pages with similar content placement
                    scaled_baseline
                };

                Some(final_bounds)
            }
            None => {
                Some(scaled_baseline)
            }
        }
    } else {
        // For StandardizeAndCenter and None, use the original per-page logic
        if !dets.is_empty() {
            crate::margin::calculate_content_bounds(&dets, page_w, page_h, true)
        } else if let Some(pd) = page_data {
            // CRITICAL: Also scale per-page cached bounds if using them
            pd.content_bounds.map(|cb| {
                cb.scale_to_resolution(
                    analysis.analysis_width,
                    analysis.analysis_height,
                    page_w,
                    page_h,
                )
            })
        } else {
            crate::margin::compute_pixel_bounds_for_margin(&page.high_res_image, cfg)
        }
    };

    if let Some(bounds) = bounds {
        // CRITICAL: Use SCALED baseline for standard dimensions (processing resolution, not analysis resolution)
        let standard_dims = crate::margin::StandardPageDimensions {
            width: scaled_baseline.width(),
            height: scaled_baseline.height(),
        };

        // Apply effective margin setting (may have been overridden due to footnotes)
        let effective_setting = analysis.effective_margin_setting;

        // For CropAndResize mode, use the calculated bounds but ensure more consistent processing
        // This means each page will crop to its own content area but with uniform processing
        let effective_bounds = bounds;

        // Compute margin correction parameters for potential use in coordinate mapping
        let _margin_correction = crate::margin::compute_margin_correction(
            &effective_bounds,
            effective_setting,
            &standard_dims,
            cfg.target_width(),
            cfg.target_height(),
            Some((page_w, page_h)),
        );

        // Process page with document-wide baseline
        match crate::margin::process_page_margins(
            &page.high_res_image,
            &effective_bounds,
            effective_setting,
            &standard_dims,
            cfg.target_width(),
            cfg.target_height(),
        ) {
            Ok(img) => {
                let new_dets = crate::margin::transform_detections(
                    &dets,
                    &effective_bounds,
                    effective_setting,
                    &standard_dims,
                    cfg.target_width(),
                    cfg.target_height(),
                    Some((page_w, page_h)),
                );

                // Additional validation: ensure transformed detections are within new image bounds
                let mut validated_detections = Vec::new();
                for mut det in new_dets {
                    // Clamp the detection coordinates to the new image dimensions
                    det.bbox[0] = det.bbox[0].max(0.0).min(img.width() as f32);
                    det.bbox[1] = det.bbox[1].max(0.0).min(img.height() as f32);
                    det.bbox[2] = det.bbox[2].max(0.0).min(img.width() as f32);
                    det.bbox[3] = det.bbox[3].max(0.0).min(img.height() as f32);

                    // Only keep detections that have valid bounds (not completely outside image)
                    if det.bbox[0] < det.bbox[2] && det.bbox[1] < det.bbox[3] {
                        validated_detections.push(det);
                    }
                }

                return Ok((img, validated_detections));
            }
            Err(e) => {
                warn_log!(
                    "Page {}: Failed to apply margin analysis: {}. Falling back to original.",
                    page_index,
                    e
                );
            }
        }
    }

    // Fallback: return original image
    Ok(((*page.high_res_image).clone(), dets))
}

fn apply_region_policy(
    rendered: &RenderedPageData,
    inference_result: &InferenceResult,
    config: &PipelineConfig,
) -> Result<(RgbImage, Vec<crate::engine::Detection>)> {
    let policy: Arc<dyn RegionPolicy> = match config.margin_settings() {
        crate::margin::MarginSettings::StandardizeAndCenter
        | crate::margin::MarginSettings::CropAndResize => Arc::new(MarginStandardizeAndCenter),
        crate::margin::MarginSettings::None => {
            if config.enable_layout_detection() {
                Arc::new(LayoutRegions)
            } else {
                Arc::new(NoLayoutFullPage)
            }
        }
    };

    Ok(policy.transform(rendered, inference_result, config))
}

/// Binarize image with special handling for blank pages in adaptive mode.
fn binarize_image(
    image: &RgbImage,
    config: &PipelineConfig,
    force_blank_threshold: bool,
) -> Vec<u8> {
    let want_invert_input = config.invert_input();
    let mut want_invert_output = config.binarization().invert;
    if want_invert_input && want_invert_output {
        want_invert_output = false;
    }

    // Special handling for blank pages in adaptive + layout mode:
    // use fixed threshold to avoid static/noise artifacts.
    let (use_fixed_threshold, fixed_threshold) = if force_blank_threshold {
        (true, BLANK_PAGE_FALLBACK_THRESHOLD)
    } else {
        (
            config.binarization().use_fixed_threshold,
            config.binarization().fixed_threshold,
        )
    };

    let options = BinarizationOptions {
        invert: want_invert_output,
        invert_input: want_invert_input,
        k_factor: config.binarization().k_factor,
        use_heavy_duty: config.binarization().use_heavy_duty && !use_fixed_threshold,
        patch_percentage: config.binarization().patch_percentage,
        no_patch: config.binarization().no_patch,
        use_fixed_threshold,
        fixed_threshold,
    };

    // binarize_image_raw handles heavy-duty mode internally
    Legencode::color::binarization::binarize_image_raw(
        image.as_raw(),
        image.width() as usize,
        image.height() as usize,
        &options,
    )
}

/// Synchronous version of encode_region_image for use in blocking tasks
fn encode_region_image_sync(
    image_data: &[u8],
    width: u32,
    height: u32,
    format: crate::types::CoverFormat,
    is_cover: bool,
    high_quality: bool,
) -> Result<(Vec<u8>, String)> {
    // Guardrails: sanity-check dimensions and buffer length
    const MAX_OVERLAY_SIDE: u32 = 8192;
    const CHANNELS: usize = 3; // regions are RGB buffers
    if width == 0 || height == 0 {
        return Err(anyhow!("Region has zero dimension"));
    }
    if width > MAX_OVERLAY_SIDE || height > MAX_OVERLAY_SIDE {
        return Err(anyhow!(
            "Region exceeds max side limit ({} or {} > {})",
            width,
            height,
            MAX_OVERLAY_SIDE
        ));
    }
    let expected_len = width as usize * height as usize * CHANNELS;
    if image_data.len() < expected_len {
        return Err(anyhow!(
            "Region buffer shorter than expected ({} < {})",
            image_data.len(),
            expected_len
        ));
    }
    use Legencode::streamline::{
        EncodingManager, EncodingResult, EncodingSettings, ImageBuffer as LegeImageBuffer,
        Jbig2Settings, JpegSettings,
    };

    let channels = CHANNELS as u32; // regions are RGB buffers
    if image_data.len() < expected_len {
        return Err(anyhow!(
            "Region buffer shorter than expected ({} < {})",
            image_data.len(),
            expected_len
        ));
    }

    let (settings, fmt_str) = match format {
        crate::types::CoverFormat::Jpeg => {
            let q = if high_quality {
                95
            } else if is_cover {
                50
            } else {
                40
            };
            (
                EncodingSettings::Jpeg(JpegSettings {
                    quality: q,
                    baseline: true,
                    optimized: true,
                    downsample: true,
                }),
                "jpeg".to_string(),
            )
        }
        crate::types::CoverFormat::Ccitt4 => (EncodingSettings::Ccitt4, "ccitt4".to_string()),
        crate::types::CoverFormat::Jbig2 => (
            EncodingSettings::Jbig2(Jbig2Settings {
                pdf_fragment_mode: true,
                mode: Jbig2Mode::Generic,
                use_jbig2_halftone_segments: false,
            }),
            "jbig2".to_string(),
        ),
        crate::types::CoverFormat::None => return Err(anyhow!("No format for region encoding")),
        _ => {
            // Treat any other formats as JPEG fallback
            let q = if high_quality {
                95
            } else if is_cover {
                50
            } else {
                40
            };
            (
                EncodingSettings::Jpeg(JpegSettings {
                    quality: q,
                    baseline: true,
                    optimized: true,
                    downsample: true,
                }),
                "jpeg".to_string(),
            )
        }
    };

    let image_data_owned = image_data[..expected_len].to_vec();
    let buffer = LegeImageBuffer {
        data: &image_data_owned,
        width,
        height,
        channels: CHANNELS as u8,
    };
    let encoding_result = EncodingManager::encode(&buffer, &settings)
        .map_err(|e| anyhow!("Region encoding failed: {}", e))?;

    match encoding_result {
        EncodingResult::Standard(data) => Ok((data, fmt_str)),
        EncodingResult::Jbig2WithGlobals { page_data, .. } => {
            if fmt_str != "jbig2" {
                return Err(anyhow!(
                    "Encoder returned JBIG2 data but format tag is '{}'",
                    fmt_str
                ));
            }
            // Region overlays do not carry a separate global stream in this path.
            // Return page data only to avoid producing invalid concatenated JBIG2 bytes.
            Ok((page_data, fmt_str))
        }
    }
}

/// Perform OCR on binarized image
async fn perform_ocr(
    binarized: &[u8],
    width: usize,
    height: usize,
    detections: &[crate::engine::Detection],
    config: &PipelineConfig,
    page_index: usize,
) -> Result<Option<String>> {
    // Note: This function is only called when config.enable_ocr() is true
    let use_regions =
        crate::ocr::ocr::should_use_region_ocr(config.enable_layout_detection(), detections);

    let result = if use_regions {
        crate::ocr::ocr::perform_region_based_ocr(
            binarized,
            width,
            height,
            detections,
            config.ocr_language(),
        )
        .await
    } else {
        crate::ocr::ocr::perform_tiling_based_ocr(binarized, width, height, config.ocr_language())
            .await
    };

    match result {
        Ok(text) => Ok(Some(text)),
        Err(e) => {
            warn_log!("Page {}: OCR failed: {}", page_index, e);
            Ok(Some(String::new()))
        }
    }
}

/// Extract PDF text layer
async fn extract_pdf_text(
    pdf_renderer: &Arc<PdfiumRenderer>,
    page_index: usize,
    image: &RgbImage,
) -> Result<Option<String>> {
    match pdf_renderer.has_text_layer(page_index as u32).await {
        Ok(true) => match pdf_renderer.extract_page_text(page_index as u32).await {
            Ok(raw_text) => Ok(Some(build_hocr_from_pdf_text(
                &raw_text,
                image.width(),
                image.height(),
            ))),
            Err(e) => {
                warn_log!("Failed to extract text from page {}: {}", page_index, e);
                Ok(None)
            }
        },
        Ok(false) => Ok(None),
        Err(e) => {
            warn_log!("Failed to check text layer for page {}: {}", page_index, e);
            Ok(None)
        }
    }
}

/// Encode base layer (binarized image) using the full encoding pipeline
/// Supports JBIG2, CCITT4, and JPEG formats with proper settings.
/// `force_jbig2_generic` overrides Symbol/SymUnify → Generic when abandon regions are present.
async fn encode_base_layer(
    binarized: &[u8],
    width: usize,
    height: usize,
    config: &PipelineConfig,
    page_index: usize,
    force_jbig2_generic: bool,
) -> Result<crate::accumulator::ContentType> {
    use crate::accumulator::ContentType;
    use Legencode::streamline::{
        EncodingManager, EncodingResult, EncodingSettings, ImageBuffer as LegeImageBuffer,
        Jbig2Settings, JpegSettings,
    };

    #[cfg(feature = "debug-logging")]
    let encoding_start = std::time::Instant::now();

    // Determine encoding settings
    let jbig2_mode = if force_jbig2_generic {
        Legencode::streamline::Jbig2Mode::Generic
    } else {
        config.jbig2_mode()
    };
    let (encoding_settings, base_format) = match config.text_format() {
        "jbig2" => (
            EncodingSettings::Jbig2(Jbig2Settings {
                pdf_fragment_mode: true,
                mode: jbig2_mode,
                use_jbig2_halftone_segments: false,
            }),
            "jbig2",
        ),
        "ccitt4" => (EncodingSettings::Ccitt4, "ccitt"),
        "jpeg" => (
            EncodingSettings::Jpeg(JpegSettings {
                quality: if config.high_quality_output() { 95 } else { 40 },
                baseline: true,
                optimized: true,
                downsample: false,
            }),
            "jpeg",
        ),
        _ => {
            return Err(anyhow!(
                "No valid text encoding format specified: {}",
                config.text_format()
            ));
        }
    };

    // Spawn blocking task for encoding with semaphore backpressure
    let binarized_owned = binarized.to_vec();
    let text_format = config.text_format().to_string();
    let encode_sem = crate::pipeline::helper_functions::get_encode_semaphore();
    let permit = match encode_sem {
        Some(sem) => Some(sem.acquire_owned().await.ok()),
        None => None,
    };

    let encoding_result = tokio::task::spawn_blocking(move || {
        let buffer = LegeImageBuffer {
            data: &binarized_owned,
            width: width as u32,
            height: height as u32,
            channels: 1u8, // Grayscale/binary data
        };
        EncodingManager::encode(&buffer, &encoding_settings)
            .map_err(|e| anyhow!("Encoding failed for format {}: {}", text_format, e))
    })
    .await
    .map_err(|e| anyhow!("Encoding task panicked: {}", e))??;

    drop(permit);

    match encoding_result {
        EncodingResult::Standard(data) => {
            if data.is_empty() {
                return Err(anyhow!(
                    "Encoder returned empty data for {}x{} image",
                    width,
                    height
                ));
            }

            #[cfg(feature = "debug-logging")]
            crate::perf_log!(
                encoding_start,
                "[PROFILING] Page {} {} encoding completed",
                page_index + 1,
                base_format
            );

            let final_format = if base_format == "jpeg" {
                "jpeg-gray".to_string()
            } else {
                base_format.to_string()
            };

            // JBIG2 generic (lossless) has no global dictionary → EncodingManager uses Standard.
            Ok(ContentType::EncodedImage {
                data: Arc::from(data),
                pixel_width: width as u32,
                pixel_height: height as u32,
                format: final_format,
            })
        }
        EncodingResult::Jbig2WithGlobals {
            page_data,
            global_data,
        } => {
            if base_format != "jbig2" {
                return Err(anyhow!(
                    "Non-JBIG2 text mode returned JBIG2 payload (Jbig2WithGlobals variant)"
                ));
            }
            if page_data.is_empty() {
                return Err(anyhow!(
                    "Encoder returned empty data for {}x{} image",
                    width,
                    height
                ));
            }

            #[cfg(feature = "debug-logging")]
            crate::perf_log!(
                encoding_start,
                "[PROFILING] Page {} JBIG2 encoding completed",
                page_index + 1
            );

            Ok(ContentType::Jbig2ImageWithGlobals {
                page_data: Arc::from(page_data),
                global_data: Arc::from(global_data),
                pixel_width: width as u32,
                pixel_height: height as u32,
            })
        }
    }
}

/// Encode base layer as full-color JPEG for JPEG-only mode
async fn encode_base_layer_for_jpeg_mode(
    image: &RgbImage,
    config: &PipelineConfig,
    page_index: usize,
) -> Result<crate::accumulator::ContentType> {
    use crate::accumulator::ContentType;
    use Legencode::streamline::{
        EncodingManager, EncodingResult, EncodingSettings, ImageBuffer as LegeImageBuffer,
        JpegSettings,
    };

    #[cfg(feature = "debug-logging")]
    let encoding_start = std::time::Instant::now();

    // For JPEG-only mode, use higher quality setting since we're preserving original image
    let jpeg_settings = JpegSettings {
        quality: if config.high_quality_output() { 95 } else { 60 },
        baseline: true,
        optimized: true,
        downsample: false, // Don't downsample to preserve quality
    };

    let encoding_settings = EncodingSettings::Jpeg(jpeg_settings);

    // Clone the image data for the blocking task
    let width = image.width();
    let height = image.height();
    let image_data = image.as_raw().to_vec();

    let encode_sem = crate::pipeline::helper_functions::get_encode_semaphore();
    let permit = match encode_sem {
        Some(sem) => Some(sem.acquire_owned().await.ok()),
        None => None,
    };

    let encoding_result = tokio::task::spawn_blocking(move || {
        let buffer = LegeImageBuffer {
            data: &image_data,
            width,
            height,
            channels: 3u8, // RGB data
        };
        EncodingManager::encode(&buffer, &encoding_settings)
            .map_err(|e| anyhow!("JPEG encoding failed: {}", e))
    })
    .await
    .map_err(|e| anyhow!("JPEG encoding task panicked: {}", e))??;

    drop(permit);

    match encoding_result {
        EncodingResult::Standard(data) => {
            if data.is_empty() {
                return Err(anyhow!(
                    "JPEG encoder returned empty data for {}x{} image",
                    width,
                    height
                ));
            }

            #[cfg(feature = "debug-logging")]
            crate::perf_log!(
                encoding_start,
                "[PROFILING] Page {} JPEG encoding completed",
                page_index + 1
            );

            Ok(ContentType::EncodedImage {
                data: std::sync::Arc::from(data),
                pixel_width: width,
                pixel_height: height,
                format: "jpeg-rgb".to_string(),
            })
        }
        _ => {
            // JPEG encoding should only return Standard result
            Err(anyhow!("Unexpected JPEG encoding result type"))
        }
    }
}

//==============================================================================
// Phase 1: Document-Wide Margin Analysis (Low-Res Pass)
//==============================================================================

struct AnalysisPreparedPage {
    page_index: usize,
    analysis_image: RgbImage,
    pixel_bounds: Option<crate::margin::ContentBounds>,
}

struct AnalysisPageResult {
    page_index: usize,
    page_width: u32,
    page_height: u32,
    detections: Vec<crate::engine::Detection>,
    pixel_bounds: Option<crate::margin::ContentBounds>,
}

fn prepare_analysis_page(
    page_idx: usize,
    original_image: RgbImage,
    config: Arc<PipelineConfig>,
) -> AnalysisPreparedPage {
    const ANALYSIS_WIDTH: u32 = 640;

    let aspect_ratio = original_image.width() as f32 / original_image.height() as f32;
    let analysis_height = (ANALYSIS_WIDTH as f32 / aspect_ratio).round() as u32;
    let params = crate::resize::ResizeParams {
        target_width: ANALYSIS_WIDTH,
        target_height: analysis_height,
        method: crate::resize::ResizeMethod::Lanczos3,
        letterbox: false,
        border_value: 0.0,
        swap_rb: false,
    };

    let analysis_image_data = match crate::resize::resize_bytes(
        original_image.as_raw(),
        original_image.width(),
        original_image.height(),
        &params,
        3,
    ) {
        Ok(bytes) => bytes,
        Err(e) => {
            warn_log!(
                "Page {}: Failed to resize for margin analysis: {}. Using original image.",
                page_idx,
                e
            );
            original_image.as_raw().to_vec()
        }
    };

    let analysis_image =
        if analysis_image_data.len() == (ANALYSIS_WIDTH * analysis_height * 3) as usize {
            RgbImage::from_raw(ANALYSIS_WIDTH, analysis_height, analysis_image_data)
                .unwrap_or(original_image)
        } else {
            original_image
        };

    let pixel_bounds = crate::margin::compute_pixel_bounds_for_margin(&analysis_image, &config);

    AnalysisPreparedPage {
        page_index: page_idx,
        analysis_image,
        pixel_bounds,
    }
}

fn build_margin_analysis_future(
    inference_handle: Option<Arc<crate::pipeline::inference::InferenceHandle>>,
    prepared: AnalysisPreparedPage,
    config: Arc<PipelineConfig>,
) -> BoxFuture<'static, Result<AnalysisPageResult>> {
    Box::pin(async move {
        let AnalysisPreparedPage {
            page_index,
            analysis_image,
            pixel_bounds,
        } = prepared;

        let detections = if let Some(handle) = inference_handle {
            let spec = config.inference_resize_spec();
            let inference_img = build_inference_image(&analysis_image, &spec)
                .unwrap_or_else(|_| analysis_image.clone());

            handle
                .submit(page_index, Arc::new(inference_img))
                .await?
                .await
                .map_err(|_| anyhow!("Inference actor dropped margin-analysis response"))?
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        Ok(AnalysisPageResult {
            page_index,
            page_width: analysis_image.width(),
            page_height: analysis_image.height(),
            detections,
            pixel_bounds,
        })
    })
}

/// Performs document-wide margin analysis using low-resolution rendering
/// Returns the analysis and cached detections for reuse in Phase 2
async fn perform_document_analysis(
    pdf_renderer: Arc<PdfiumRenderer>,
    config: Arc<PipelineConfig>,
    inference_handle: Option<Arc<crate::pipeline::inference::InferenceHandle>>,
    total_pages: usize,
    page_range: std::ops::Range<usize>,
    progress_tracker: &ProgressTracker,
) -> Result<(DocumentMarginAnalysis, Vec<Vec<crate::engine::Detection>>)> {
    info_log!("[Margin-Analysis] Phase 1: Analyzing document margins (Low-Res Pass)...");
    progress_tracker.update(crate::progress::ProcessingStatus::MarginPass1Analyzing);

    let mut margin_inputs = Vec::new();
    let mut detection_cache = vec![Vec::new(); total_pages];
    // ORT + PDFium are intentionally single-worker for stability.
    let analysis_infer_concurrency = 1usize;
    let mut pending: FuturesUnordered<BoxFuture<'static, Result<AnalysisPageResult>>> =
        FuturesUnordered::new();

    let push_completed =
        |result: AnalysisPageResult,
         margin_inputs: &mut Vec<PageMarginInput>,
         detection_cache: &mut Vec<Vec<crate::engine::Detection>>| {
            margin_inputs.push(PageMarginInput {
                page_index: result.page_index,
                page_width: result.page_width,
                page_height: result.page_height,
                detections: result.detections.clone(),
                pixel_bounds: result.pixel_bounds,
            });

            if result.page_index < detection_cache.len() {
                detection_cache[result.page_index] = result.detections;
            }
        };

    // Render pages at original target resolution first, then resize in memory for analysis
    for (idx, page_idx) in page_range.enumerate() {
        // Render at target resolution (this is what pdfium is most stable with)
        let target_height = config.target_height();
        let target_width = config.target_width();

        let rgb_page = match pdf_renderer
            .render_page_rgb(page_idx as u32, target_height, target_width)
            .await
        {
            Ok(rgb_page) => rgb_page,
            Err(e) => {
                warn_log!(
                    "Page {}: Failed to render during margin analysis: {}. Skipping page for margin analysis.",
                    page_idx,
                    e
                );
                // Create a placeholder with empty detections to avoid breaking the analysis
                margin_inputs.push(PageMarginInput {
                    page_index: page_idx,
                    page_width: 0,
                    page_height: 0,
                    detections: Vec::new(),
                    pixel_bounds: None,
                });
                if page_idx < detection_cache.len() {
                    detection_cache[page_idx] = Vec::new();
                }
                continue;
            }
        };

        let original_image = RgbImage::from_raw(rgb_page.width, rgb_page.height, rgb_page.data)
            .ok_or_else(|| anyhow!("Failed to convert image for page {}", page_idx))?;
        let config_clone = config.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_analysis_page(page_idx, original_image, config_clone)
        })
        .await
        .map_err(|e| anyhow!("Margin-analysis prep task panicked: {}", e))?;

        pending.push(build_margin_analysis_future(
            inference_handle.clone(),
            prepared,
            config.clone(),
        ));

        while pending.len() >= analysis_infer_concurrency {
            if let Some(result) = pending.next().await {
                push_completed(result?, &mut margin_inputs, &mut detection_cache);
            }
        }

        // Update progress
        if idx % 10 == 0 || idx == total_pages - 1 {
            progress_tracker.update(crate::progress::ProcessingStatus::MarginPass1Analyzing);
        }
    }

    while let Some(result) = pending.next().await {
        push_completed(result?, &mut margin_inputs, &mut detection_cache);
    }

    margin_inputs.sort_by_key(|input| input.page_index);

    // Analyze margins across entire document
    info_log!(
        "[Margin-Analysis] Calculating document-wide baseline from {} pages...",
        margin_inputs.len()
    );
    let analysis = crate::margin::analyze_document_margins(
        &margin_inputs,
        config.margin_settings(),
        false, // crop_footnotes
    );

    // Show summary
    if let Some(reason) = &analysis.setting_override_reason {
        progress_tracker.update(crate::progress::ProcessingStatus::FootnotesDetected {
            message: reason.clone(),
        });
    }

    let summary = format!(
        "Baseline margins established from {} pages. Effective setting: {:?}",
        margin_inputs.len(),
        analysis.effective_margin_setting
    );
    progress_tracker.update(crate::progress::ProcessingStatus::MarginAnalysisSummary { summary });

    Ok((analysis, detection_cache))
}

//==============================================================================
// Main Pipeline Entry Point
//==============================================================================

pub async fn create_and_run_pdf_parallel_pipeline(
    pdf_bytes: Arc<[u8]>,
    config: Arc<PipelineConfig>,
    output_path: &Path,
    page_range: Option<std::ops::Range<usize>>,
    progress_tracker: &ProgressTracker,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
    _progress_callback: impl Fn(usize, usize) + Send + Sync + 'static,
) -> Result<()> {
    info_log!("[PDF-Parallel] Starting parallel PDF pipeline");

    // Reset standard dimensions at the start of each new document
    crate::pipeline::reset_standard_dimensions();

    let pipeline_config = PipelineRuntimeLimits::from_config(&config);
    init_encode_semaphore(pipeline_config.page_workers);

    // Initialize PDF renderer
    let mut raster_cfg = PdfRasterConfig::default();
    raster_cfg.render_forms = false;
    raster_cfg.target_width = config.target_width();
    let pdf_renderer = Arc::new(PdfiumRenderer::new_from_bytes(
        pdf_bytes.clone(),
        raster_cfg,
    )?);

    // Calculate pages to process
    let document_pages = pdf_renderer.page_count() as usize;
    let page_start = page_range.as_ref().map(|r| r.start).unwrap_or(0);
    let page_end = page_range.as_ref().map(|r| r.end).unwrap_or(document_pages);
    let total_pages = page_end - page_start;

    let infer_concurrency = 1usize;
    let process_concurrency = pipeline_config.page_workers.max(1);

    info_log!("[PDF-Parallel] Processing {} pages with:", total_pages);
    info_log!("  - Render buffer: {}", pipeline_config.render_buffer);
    info_log!("  - Inference concurrency: {}", infer_concurrency);
    info_log!("  - Process concurrency: {}", process_concurrency);

    // Initialize shared resources
    let deskew_engine = prepare_shared_deskew_engine(&config)?;
    let inference_handle = if config.enable_layout_detection() {
        match crate::pipeline::inference::InferenceHandle::new(&config) {
            Ok(handle) => Some(Arc::new(handle)),
            Err(e) => {
                return Err(anyhow!(
                    "[PDF-Parallel] Failed to create InferenceHandle: {}",
                    e
                ));
            }
        }
    } else {
        None
    };

    // Check for cancellation before starting processing
    if let Ok(signal) = shutdown_rx.try_recv() {
        return Err(anyhow::anyhow!(
            "Processing cancelled: {}",
            signal
                .message
                .unwrap_or_else(|| "User requested cancellation".to_string())
        ));
    }

    // Phase 1: Document-wide margin analysis (if margin mode is enabled)
    let needs_two_pass = matches!(
        config.margin_settings(),
        crate::margin::MarginSettings::StandardizeAndCenter
            | crate::margin::MarginSettings::CropAndResize
    );

    let (margin_analysis, detection_cache) = if needs_two_pass {
        info_log!("[PDF-Parallel] Margin mode enabled - running 2-pass document analysis");
        let (analysis, cache) = perform_document_analysis(
            pdf_renderer.clone(),
            config.clone(),
            inference_handle.clone(),
            total_pages,
            page_start..page_end,
            progress_tracker,
        )
        .await?;
        (Some(analysis), cache)
    } else {
        (None, Vec::new())
    };

    // Check for cancellation after margin analysis
    if let Ok(signal) = shutdown_rx.try_recv() {
        return Err(anyhow::anyhow!(
            "Processing cancelled: {}",
            signal
                .message
                .unwrap_or_else(|| "User requested cancellation".to_string())
        ));
    }

    // Create progress counters
    let layout_enabled = config.enable_layout_detection();
    let render_count = Arc::new(AtomicUsize::new(0));
    let detect_count = Arc::new(AtomicUsize::new(0));
    let encode_count = Arc::new(AtomicUsize::new(0));

    // Create pipeline channels with larger buffers for better pipelining
    let (render_tx, render_rx) = mpsc::channel(pipeline_config.render_buffer);
    let (infer_tx, infer_rx) = mpsc::channel(pipeline_config.inference_buffer);
    let (process_tx, process_rx) = mpsc::channel(pipeline_config.channel_capacity);

    // Spawn PDF writer actor (already exists and works well)
    let use_margin_label = !matches!(
        config.margin_settings(),
        crate::margin::MarginSettings::None
    );
    let (pdf_writer_handle, mut pdf_writer_task) = spawn_pdf_writer_actor(
        output_path.to_path_buf(),
        total_pages,
        config.pdf_compatibility_mode,
        progress_tracker.clone(),
        use_margin_label,
        pipeline_config.channel_capacity,
    );

    // Forward processed pages to PDF writer (mut for tokio::select!)
    let mut writer_forwarder = {
        let mut process_rx: mpsc::Receiver<ProcessedPage> = process_rx;
        let pdf_writer_handle = pdf_writer_handle.clone();

        tokio::spawn(async move {
            while let Some(processed_page) = process_rx.recv().await {
                // Convert ProcessedPage to accumulator::Page format
                let page = crate::accumulator::Page {
                    width: processed_page.width as f32,
                    height: processed_page.height as f32,
                    elements: processed_page.elements,
                    hocr_text: processed_page.hocr_text,
                    index: processed_page.index,
                    binarized: Some(processed_page.binarized),
                };

                pdf_writer_handle
                    .send_page(page, processed_page.index)
                    .await?;
            }

            // CRITICAL: Finalize the PDF after all pages are sent
            info_log!("[PDF-Parallel] Finalizing PDF...");
            pdf_writer_handle.finalize().await?;

            Ok::<(), anyhow::Error>(())
        })
    };

    // Spawn pipeline stages (mut for tokio::select!)
    let mut render_task = tokio::spawn(render_stage(
        pdf_renderer.clone(),
        config.clone(),
        deskew_engine,
        page_start..page_end,
        render_tx,
        render_count.clone(),
        detect_count.clone(),
        encode_count.clone(),
        progress_tracker.clone(),
        total_pages,
        layout_enabled,
    ));

    // Prepare detection cache for inference stage
    let detection_cache_arc = Arc::new(detection_cache);
    let analysis_width = if needs_two_pass { 640 } else { 0 };

    let mut infer_task = tokio::spawn(inference_stage_parallel(
        inference_handle,
        render_rx,
        infer_tx,
        detect_count.clone(),
        infer_concurrency,
        render_count.clone(),
        encode_count.clone(),
        progress_tracker.clone(),
        total_pages,
        layout_enabled,
        detection_cache_arc,
        analysis_width,
    ));

    // Prepare margin analysis for processing stage
    let margin_analysis_arc = margin_analysis.map(Arc::new);

    let mut process_task = tokio::spawn(processing_stage_parallel(
        config.clone(),
        pdf_renderer.clone(),
        infer_rx,
        process_tx,
        encode_count.clone(),
        page_start,
        process_concurrency,
        render_count.clone(),
        detect_count.clone(),
        progress_tracker.clone(),
        total_pages,
        layout_enabled,
        margin_analysis_arc,
    ));

    // Wait for all stages to complete with cancellation support
    info_log!("[PDF-Parallel] Waiting for pipeline stages to complete...");

    // Poll for cancellation while waiting for tasks
    loop {
        tokio::select! {
            result = &mut render_task => {
                result??;
                info_log!("[PDF-Parallel] Render stage complete");
                break;
            }
            signal = shutdown_rx.recv() => {
                if let Ok(sig) = signal {
                    // Abort all tasks
                    render_task.abort();
                    infer_task.abort();
                    process_task.abort();
                    writer_forwarder.abort();
                    pdf_writer_task.abort();

                    return Err(anyhow::anyhow!("Processing cancelled during render stage: {}",
                        sig.message.unwrap_or_else(|| "User requested cancellation".to_string())));
                }
            }
        }
    }

    // Wait for remaining stages with cancellation
    loop {
        tokio::select! {
            result = &mut infer_task => {
                result??;
                info_log!("[PDF-Parallel] Inference stage complete");
                break;
            }
            signal = shutdown_rx.recv() => {
                if let Ok(sig) = signal {
                    infer_task.abort();
                    process_task.abort();
                    writer_forwarder.abort();
                    pdf_writer_task.abort();

                    return Err(anyhow::anyhow!("Processing cancelled during inference stage: {}",
                        sig.message.unwrap_or_else(|| "User requested cancellation".to_string())));
                }
            }
        }
    }

    loop {
        tokio::select! {
            result = &mut process_task => {
                result??;
                info_log!("[PDF-Parallel] Processing stage complete");
                break;
            }
            signal = shutdown_rx.recv() => {
                if let Ok(sig) = signal {
                    process_task.abort();
                    writer_forwarder.abort();
                    pdf_writer_task.abort();

                    return Err(anyhow::anyhow!("Processing cancelled during processing stage: {}",
                        sig.message.unwrap_or_else(|| "User requested cancellation".to_string())));
                }
            }
        }
    }

    loop {
        tokio::select! {
            result = &mut writer_forwarder => {
                result??;
                info_log!("[PDF-Parallel] Writer forwarder complete");
                break;
            }
            signal = shutdown_rx.recv() => {
                if let Ok(sig) = signal {
                    writer_forwarder.abort();
                    pdf_writer_task.abort();

                    return Err(anyhow::anyhow!("Processing cancelled during writer forwarding: {}",
                        sig.message.unwrap_or_else(|| "User requested cancellation".to_string())));
                }
            }
        }
    }

    // Wait for PDF writer to finish
    loop {
        tokio::select! {
            result = &mut pdf_writer_task => {
                result.map_err(|e| anyhow!("PDF writer task failed: {:?}", e))??;
                info_log!("[PDF-Parallel] PDF writer complete");
                break;
            }
            signal = shutdown_rx.recv() => {
                if let Ok(sig) = signal {
                    pdf_writer_task.abort();

                    return Err(anyhow::anyhow!("Processing cancelled during PDF writing: {}",
                        sig.message.unwrap_or_else(|| "User requested cancellation".to_string())));
                }
            }
        }
    }

    success_log!("PDF pipeline complete: {}", output_path.display());
    Ok(())
}

// Re-export the original function name for compatibility
pub use create_and_run_pdf_parallel_pipeline as create_and_run_pdf_tokio_pipeline;
