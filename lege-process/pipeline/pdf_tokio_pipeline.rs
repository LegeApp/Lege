// pdf_tokio_pipeline.rs
use crate::margin::DocumentMarginAnalysis;
use crate::pagerender::NativeTextWord;
use crate::pagerender::prelude::PdfiumRenderer;
use crate::pipeline::config::{
    ImageRegionDitherMode, InferenceResult, PipelineConfig, RenderedPageData,
};
use crate::pipeline::helper_functions::{
    build_hocr_from_pdf_text, build_hocr_from_positioned_words, init_encode_semaphore,
    merge_overlapping_image_detections, rounded_clamped_bbox, should_preserve_cover_page,
    spawn_pdf_writer_actor,
};
use crate::pipeline::margin_pipeline::{
    CachedDetections, adjust_page_with_margin_analysis, cached_inference_result,
    perform_document_margin_analysis,
};
use crate::pipeline::page_analysis::{
    BLANK_PAGE_FALLBACK_THRESHOLD, compute_pixel_bounds_for_margin, is_visually_blank_page,
    maybe_apply_full_page_detection, should_force_blank_page_threshold,
};
use crate::pipeline::policies::{
    LayoutRegions, MarginCorrection, MarginStandardizeAndCenter, NoLayoutFullPage, RegionPolicy,
};
use crate::pipeline::runtime_limits::PipelineRuntimeLimits;
use crate::pipeline::source::{PageSource, PdfiumPageSource, source_stage};
use crate::progress::ProgressTracker;
use crate::progress::ReflowStage;
use crate::{info_log, success_log, warn_log};

use crate::color::BinarizationOptions;
use crate::encoding::Jbig2Mode;
use anyhow::{Result, anyhow};
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use image::RgbImage;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

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
}

/// Inference stage with TRUE concurrency
///
/// Instead of: recv -> infer -> send -> recv -> infer -> send (sequential)
/// Now:        recv -> spawn(infer) -> recv -> spawn(infer) -> collect results -> send
pub(crate) async fn inference_stage_parallel(
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
    detection_cache: Arc<Vec<CachedDetections>>,
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
                            progress.publish_layout_progress(rendered_val, detected_val, encoded_val, total_pages);
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
    detection_cache: Arc<Vec<CachedDetections>>,
    analysis_width: u32,
) -> BoxFuture<'static, Result<PdfInferenceData>> {
    let page_index = rendered.index;

    if analysis_width > 0
        && let Some(inference_result) =
            cached_inference_result(&rendered, detection_cache.as_slice())
    {
        return Box::pin(async move {
            Ok(PdfInferenceData {
                rendered,
                inference_result,
            })
        });
    }

    if !rendered.layout_detection_enabled {
        return Box::pin(async move {
            let inference_result = InferenceResult {
                index: page_index,
                high_res_image: rendered.high_res_image.clone(),
                inference_image: rendered.inference_image.clone(),
                detections: Vec::new(),
                text_layer: None,
                detections_are_page_space: true,
                original_width_pts: rendered.original_width_pts,
                original_height_pts: rendered.original_height_pts,
                has_no_detections: true,
            };

            Ok(PdfInferenceData {
                rendered,
                inference_result,
            })
        });
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
                detections_are_page_space: false,
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
                detections_are_page_space: true,
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
// Stage 3: Processing
//==============================================================================

async fn processing_stage_parallel(
    config: Arc<PipelineConfig>,
    pdf_renderer: Option<Arc<PdfiumRenderer>>,
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
                            progress.publish_layout_progress(rendered_val, detected_val, encoded_val, total_pages);
                        } else {
                            progress.publish_no_layout_progress(encoded_val, total_pages);
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
    pdf_renderer: Option<Arc<PdfiumRenderer>>,
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
        ocr_image,
        binarized,
        cleaned_gray,
        deferred_binarize,
        width,
        height,
        is_cover_page: _is_cover_page,
        cover_encoded_data,
        region_processing_results,
        native_text_transform,
    } = cpu_result;

    // Build elements
    let mut elements: Vec<crate::accumulator::ContentElement> = Vec::new();

    let has_cover_layer = cover_encoded_data.is_some();
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

    // A preserved cover is the complete visible page. Do not add detected image
    // regions on top of it: those crops may be grayscale or dithered depending on
    // the selected body-page mode and would alter the original cover.
    for region_result in region_processing_results
        .into_iter()
        .filter(|_| !has_cover_layer)
    {
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
    let hocr_text = if config.enable_ocr() && config.slow_ocr_enabled() {
        // Recognize on the high-res raster when available, else the page image.
        // Detections and the returned hOCR are in output (page) space.
        let (ocr_src, ocr_binary): (&RgbImage, &[u8]) = match ocr_image.as_ref() {
            Some(hi) => (hi, &[]),
            None => (&adjusted_image, binarized.as_slice()),
        };
        crate::ocr::slow::perform_slow_ocr(
            ocr_src,
            ocr_binary,
            &adjusted_detections,
            width as u32,
            height as u32,
            &config,
            page_index,
        )
        .await?
    } else if config.enable_ocr() {
        perform_ocr(
            &binarized,
            &adjusted_image,
            cleaned_gray.as_deref(),
            width,
            height,
            &adjusted_detections,
            &config,
            page_index,
        )
        .await?
    } else {
        extract_pdf_text(
            pdf_renderer.as_ref(),
            page_index,
            &adjusted_image,
            &native_text_transform,
        )
        .await?
    };

    // If any region on this page is Abandon and we're using JBIG2 Symbol mode,
    // force the base layer to Generic to avoid Symbol-mode corruption of noisy pixels.
    let force_jbig2_generic = config.text_format() == "jbig2"
        && adjusted_detections
            .iter()
            .any(|d| d.category.force_generic_jbig2());

    // Encode base layer.
    // - "jpeg" mode: encode the full RGB image (binarized held only the OCR luma above).
    // - Deferred path: binarize+encode are fused inside one spawn_blocking so GPU mapped
    //   bytes flow directly to the encoder via callback (no intermediate Vec<u8>).
    // - Default path: binarized is moved into the encoder.
    //
    // The binarized/luma buffer is fully consumed by OCR (above) and the base-layer
    // encoder; the PDF writer never reads a per-page binarized buffer (that field exists
    // only for the separate DjVu writer). Keeping it out of `ProcessedPage` avoids towing
    // a full-page grayscale buffer per page through the process→writer channel and the
    // writer's out-of-order reorder buffer — a major OOM source on large OCR jobs.
    let adjusted_image = Arc::new(adjusted_image);

    // Cover preservation is mode-independent. Once the full-color cover has been
    // encoded, it is the page's only visible raster layer; in particular, do not
    // generate a grayscale/MRC background that can replace or tint it.
    if has_cover_layer {
        return Ok(ProcessedPage {
            index: local_index,
            width: width as u32,
            height: height as u32,
            elements,
            hocr_text,
        });
    }

    // Grayscale/MRC base layer: a JP2 grayscale background plus a JBIG2 ink-mask
    // stencil. Two elements — background at the bottom (index 0), mask on top
    // (drawn after image-region overlays). All other modes emit one base element.
    if let (true, Some(cleaned)) = (config.is_grayscale_mode(), cleaned_gray) {
        // Same Abandon-region protection as the bilevel base layer, but not
        // gated on text_format — the MRC mask is always JBIG2.
        let mask_force_generic = adjusted_detections
            .iter()
            .any(|d| d.category.force_generic_jbig2());
        match encode_mrc_base_layer(
            cleaned,
            binarized,
            width,
            height,
            &config,
            page_index,
            mask_force_generic,
        )
        .await
        {
            Ok((bg_content, mask_content)) => {
                elements.insert(
                    0,
                    crate::accumulator::ContentElement {
                        x: 0.0,
                        y: 0.0,
                        width: width as f32,
                        height: height as f32,
                        content: bg_content,
                    },
                );
                elements.push(crate::accumulator::ContentElement {
                    x: 0.0,
                    y: 0.0,
                    width: width as f32,
                    height: height as f32,
                    content: mask_content,
                });
                return Ok(ProcessedPage {
                    index: local_index,
                    width: width as u32,
                    height: height as u32,
                    elements,
                    hocr_text,
                });
            }
            Err(e) => {
                return Err(anyhow!("MRC base layer failed on page {page_index}: {e}"));
            }
        }
    }

    let base_layer = if config.text_format() == "jpeg" {
        let layer =
            encode_base_layer_for_jpeg_mode(Arc::clone(&adjusted_image), &config, page_index)
                .await?;
        drop(binarized);
        layer
    } else if let Some(bin_options) = deferred_binarize {
        encode_base_layer_fused(
            adjusted_image,
            bin_options,
            width,
            height,
            &config,
            page_index,
            force_jbig2_generic,
        )
        .await?
    } else {
        encode_base_layer(
            binarized,
            width,
            height,
            &config,
            page_index,
            force_jbig2_generic,
        )
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
    })
}

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
    /// High-resolution adjusted raster retained for slow OCR ("render high,
    /// resize low"). `Some` only when the page was rendered above `target_height`
    /// for OCR; the encode path always uses the resized-down `adjusted_image`.
    ocr_image: Option<RgbImage>,
    binarized: Vec<u8>,
    /// Cleaned grayscale full page (0-255), `Some` only in `PageMode::Grayscale`.
    /// Feeds the MRC background layer; `binarized` holds its ink-core mask.
    cleaned_gray: Option<Vec<u8>>,
    /// When `Some`, binarization was deferred — `binarized` is empty and the encoder
    /// must binarize from `adjusted_image` using these options. Set only when the
    /// binarized buffer is guaranteed not to require post-binarization mutation
    /// (no cover, no image regions, OCR off, non-jpeg base format).
    deferred_binarize: Option<BinarizationOptions>,
    width: usize,
    height: usize,
    is_cover_page: bool,
    cover_encoded_data: Option<(Vec<u8>, String)>,
    region_processing_results: Vec<RegionProcessingResult>,
    native_text_transform: NativeTextTransform,
}

#[derive(Debug, Clone)]
struct NativeTextTransform {
    source_width: u32,
    source_height: u32,
    correction: MarginCorrection,
}

impl NativeTextTransform {
    fn apply(&self, bbox: [f32; 4], output_width: u32, output_height: u32) -> Option<[f32; 4]> {
        let mut mapped =
            crate::pipeline::policies::apply_page_adjustments(bbox, Some(&self.correction));
        let max_x = output_width as f32;
        let max_y = output_height as f32;
        mapped[0] = mapped[0].clamp(0.0, max_x);
        mapped[1] = mapped[1].clamp(0.0, max_y);
        mapped[2] = mapped[2].clamp(0.0, max_x);
        mapped[3] = mapped[3].clamp(0.0, max_y);
        if mapped[2] > mapped[0] && mapped[3] > mapped[1] {
            Some(mapped)
        } else {
            None
        }
    }

    fn compose_scale(&mut self, sx: f32, sy: f32) {
        self.correction.scale_x *= sx;
        self.correction.scale_y *= sy;
        self.correction.offset_x *= sx;
        self.correction.offset_y *= sy;
    }
}

fn identity_margin_correction() -> MarginCorrection {
    MarginCorrection::new(0.0, 0.0, 1.0, 1.0)
}

fn bbox_is_effectively_full_page(
    bbox: [f32; 4],
    page_w: u32,
    page_h: u32,
    min_fraction: f32,
) -> bool {
    if page_w == 0 || page_h == 0 {
        return false;
    }
    let (x1, y1, x2, y2) = rounded_clamped_bbox(bbox, page_w, page_h);
    let area = (x2.saturating_sub(x1) as f32) * (y2.saturating_sub(y1) as f32);
    let page_area = (page_w as f32) * (page_h as f32);
    page_area > 0.0 && area / page_area >= min_fraction
}

fn normalize_crop_binarization_input(image: &RgbImage) -> RgbImage {
    let raw = image.as_raw();
    let pixel_count = (image.width() as usize).saturating_mul(image.height() as usize);
    if pixel_count == 0 || raw.len() < pixel_count.saturating_mul(3) {
        return image.clone();
    }

    let mut luma = Vec::with_capacity(pixel_count);
    let mut hist = [0u32; 256];
    for rgb in raw.chunks_exact(3).take(pixel_count) {
        let y = ((rgb[0] as u32 * 77 + rgb[1] as u32 * 150 + rgb[2] as u32 * 29) >> 8) as u8;
        hist[y as usize] += 1;
        luma.push(y);
    }

    // O(n) percentiles via the 256-bin histogram instead of sorting the whole plane.
    // Matches the previous `sorted[round((n-1)*p)]` rank semantics exactly.
    let low = percentile_from_hist256(&hist, pixel_count, 0.02);
    let high = percentile_from_hist256(&hist, pixel_count, 0.98);
    if high <= low.saturating_add(8) {
        return image.clone();
    }
    // The stretch assumes the 2nd percentile sits on INK. On sparse pages
    // (front matter, chapter openers) less than 2% of pixels are ink, so
    // `low` lands on a paper shade and the stretch maps the whole page to
    // near-black — pages randomly turned solid black in crop mode. Only
    // normalize when the dark anchor is genuinely dark.
    if low > 128 {
        return image.clone();
    }

    let range = (high as u16).saturating_sub(low as u16).max(1);
    let mut normalized = Vec::with_capacity(pixel_count * 3);
    for y in luma {
        let stretched = if y <= low {
            0
        } else if y >= high {
            255
        } else {
            (((y as u16 - low as u16) * 255) / range) as u8
        };
        normalized.extend_from_slice(&[stretched, stretched, stretched]);
    }

    RgbImage::from_raw(image.width(), image.height(), normalized).unwrap_or_else(|| image.clone())
}

/// Value at rank `round((n-1)*p)` in ascending order, read from a 256-bin
/// histogram. Equivalent to `sorted[round((n-1)*p)]` but O(n) (no full sort).
fn percentile_from_hist256(hist: &[u32; 256], n: usize, percentile: f32) -> u8 {
    if n == 0 {
        return 0;
    }
    let max_index = n.saturating_sub(1);
    let target = ((max_index as f32) * percentile.clamp(0.0, 1.0)).round() as usize;
    let mut cum = 0usize;
    for (v, &count) in hist.iter().enumerate() {
        cum += count as usize;
        if cum > target {
            return v as u8;
        }
    }
    255
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

    let source_width = rendered.high_res_image.width();
    let source_height = rendered.high_res_image.height();
    let is_cover_page = should_preserve_cover_page(page_index, &config);
    let (mut adjusted_image, mut adjusted_detections, mut native_text_transform) = if is_cover_page
    {
        // A preserved cover is the full source frame. Body-page margin and
        // region policies must not crop, center, clean, or overlay it.
        (
            (*rendered.high_res_image).clone(),
            Vec::new(),
            NativeTextTransform {
                source_width,
                source_height,
                correction: identity_margin_correction(),
            },
        )
    } else if let Some(analysis) = &margin_analysis {
        // Use document-wide margin analysis (2-pass mode)
        apply_margin_analysis_to_page(
            &rendered,
            inference_result.detections,
            inference_result.detections_are_page_space,
            &config,
            analysis,
            page_index,
        )?
    } else {
        // Use per-page policy (1-pass mode)
        apply_region_policy(&rendered, &inference_result, &config)?
    };
    native_text_transform.source_width = source_width;
    native_text_transform.source_height = source_height;

    // "Render high, resize low": when the page was rendered above target_height
    // (for slow OCR), retain the high-res raster for recognition, then resize the
    // image used by the encode path down to target_height. The slow-OCR gate is
    // OR'd in so the downscale still runs in no-layout mode.
    let mut ocr_image: Option<RgbImage> = None;
    // Free-aspect crop pages have PER-PAGE heights by design (uniform width and
    // scale, height hugging each page's content). Normalizing them to
    // target_height here would stretch short pages back up — the "every page
    // is the same size" complaint — so in that mode unify only the SCALE
    // (render resolution → target resolution).
    let effective_target_h =
        if !is_cover_page && margin_analysis.is_some() && config.crop_free_aspect() {
            // The margin stage already resized the window to its per-page target
            // height (document scale × window height); nothing to normalize.
            adjusted_image.height()
        } else {
            config.target_height()
        };
    if (is_cover_page || config.enable_layout_detection() || config.slow_ocr_enabled())
        && adjusted_image.height() != effective_target_h
    {
        let current_w = adjusted_image.width();
        let current_h = adjusted_image.height();
        let target_h = effective_target_h;
        let aspect_ratio = current_w as f32 / current_h as f32;
        let target_w = config
            .target_width()
            .filter(|_| is_cover_page || !config.crop_free_aspect())
            .unwrap_or_else(|| (target_h as f32 * aspect_ratio).round() as u32);

        if target_w > 0 && target_h > 0 {
            // Scale detection bboxes into target (output) space.
            let sx = target_w as f32 / current_w as f32;
            let sy = target_h as f32 / current_h as f32;
            for det in &mut adjusted_detections {
                det.scale_bbox(sx, sy);
            }
            native_text_transform.compose_scale(sx, sy);

            // Resize image
            let params = crate::resize::ResizeParams {
                target_width: target_w,
                target_height: target_h,
                method: crate::resize::ResizeMethod::Lanczos3,
                letterbox: false,
                border_value: 0.0,
                swap_rb: false,
            };
            let resized = crate::resize::resize_bytes(
                adjusted_image.as_raw(),
                current_w,
                current_h,
                &params,
                3,
            )
            .ok()
            .and_then(|bytes| RgbImage::from_raw(target_w, target_h, bytes));
            let resized = match resized {
                Some(buf) => buf,
                None => image::imageops::resize(
                    &adjusted_image,
                    target_w,
                    target_h,
                    image::imageops::FilterType::Lanczos3,
                ),
            };
            // Move the pre-resize high-res raster out for OCR (no clone) when it
            // is genuinely higher resolution than the output.
            if config.slow_ocr_enabled() && current_h > target_h {
                ocr_image = Some(std::mem::replace(&mut adjusted_image, resized));
            } else {
                adjusted_image = resized;
            }
        }
    }

    let width = adjusted_image.width() as usize;
    let height = adjusted_image.height() as usize;

    // Use raw post-NMS YOLO bboxes (no full-page expansion).
    // We still merge overlaps to prevent double-encodes.
    if config.enable_layout_detection() {
        let classifier = &crate::types::LABEL_CLASSIFIER;
        maybe_apply_full_page_detection(
            &mut adjusted_detections,
            width as u32,
            height as u32,
            &config,
            classifier,
        );
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
            adjusted_detections
                .iter()
                .filter(|d| classifier.is_image_label(d))
                .count()
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

    let classifier = &crate::types::LABEL_CLASSIFIER;

    // Grayscale/MRC mode: image-labeled regions that are actually line art
    // (sheet-music systems, diagrams, ruled tables — a common YOLO
    // misclassification) must NOT be overlaid as original raster crops: the
    // crop carries the un-cleaned gray paper and covers the cleaned page.
    // Dropping the detection routes the region through the ink-mask +
    // cleaned-background path, which renders line art far better. True
    // continuous-tone regions keep their overlay.
    if config.is_grayscale_mode() && config.text_format() != "jpeg" {
        adjusted_detections.retain(|det| {
            if !classifier.is_image_label(det) {
                return true;
            }
            let line_art = crate::clean_gray::region_is_line_art(
                adjusted_image.as_raw(),
                width,
                height,
                det.bbox,
            );
            if line_art {
                crate::bbox_trace!(
                    "PAGE {} grayscale: dropping line-art image region ({:.0},{:.0},{:.0},{:.0})",
                    page_index,
                    det.bbox[0],
                    det.bbox[1],
                    det.bbox[2],
                    det.bbox[3]
                );
            }
            !line_art
        });
    }

    let has_substantive_text_detection = adjusted_detections
        .iter()
        .any(|det| classifier.is_substantive_text(det));
    let has_layout_image_detection = adjusted_detections
        .iter()
        .any(|det| classifier.is_image_label(det));
    let page_is_visually_blank = is_visually_blank_page(&adjusted_image);
    // A crop-mode page without substantive text detections may only be
    // blank-stomped when the PIXELS agree it is blank. YOLO misses text on
    // sparse pages (chapter openers, decorative headings) and detection is
    // skipped entirely under --exclude-layout — erasing real content here
    // was the "cropping deletes my page" failure mode.
    let layout_crop_has_no_text = config.enable_layout_detection()
        && config.crop_free_aspect()
        && !has_substantive_text_detection
        && page_is_visually_blank;
    let force_blank_threshold = layout_crop_has_no_text
        || should_force_blank_page_threshold(
            &config,
            adjusted_detections.is_empty(),
            page_is_visually_blank,
            &adjusted_detections,
            width as u32,
            height as u32,
            classifier,
        );

    crate::debug_println!(
        "CROPBIN page={} force_blank={} layout_crop_no_text={} subst_text={} img_det={} dets={}",
        page_index,
        force_blank_threshold,
        layout_crop_has_no_text,
        has_substantive_text_detection,
        has_layout_image_detection,
        adjusted_detections.len()
    );
    if force_blank_threshold && !is_cover_page {
        if !has_layout_image_detection {
            adjusted_detections.clear();
            adjusted_image =
                RgbImage::from_pixel(width as u32, height as u32, image::Rgb([255, 255, 255]));
        }
    }

    // 2b. Pre-mask image regions before binarization so Sauvola only sees text.
    // Image content would skew the adaptive threshold calculations.
    // We keep the original `adjusted_image` intact for dithering later.
    // White out figure regions before binarization so Sauvola only sees text content.
    // Detected image areas are later overlaid (original, dithered, or re-encoded),
    // so the bilevel base layer must be blank in those areas.
    let premask_images = config.enable_layout_detection() && config.text_format() != "jpeg";
    let has_image_regions = premask_images
        && adjusted_detections
            .iter()
            .any(|det| classifier.is_image_label(det));

    // Crop-mode percentile normalization must run BEFORE the figure premask:
    // premasked regions are pure 255 and hijack the 98th-percentile white
    // anchor, so real paper stretches down to mid-gray — below the default
    // fixed-180 threshold — and the whole base layer binarizes black (the
    // "black chapter page" bug).
    let normalized_page: Option<RgbImage> =
        (config.crop_free_aspect() && config.text_format() != "jpeg" && !force_blank_threshold)
            .then(|| normalize_crop_binarization_input(&adjusted_image));

    let binarization_image: std::borrow::Cow<'_, RgbImage> = if has_image_regions {
        let premask_source = normalized_page.as_ref().unwrap_or(&adjusted_image);
        let mut masked_rgb = premask_source.as_raw().clone();
        let w = width as u32;
        let h = height as u32;
        const MASK_PAD: u32 = 3;
        for det in &adjusted_detections {
            if !classifier.is_image_label(det) {
                continue;
            }
            if has_substantive_text_detection && bbox_is_effectively_full_page(det.bbox, w, h, 0.90)
            {
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
    } else if let Some(normalized) = normalized_page {
        std::borrow::Cow::Owned(normalized)
    } else {
        std::borrow::Cow::Borrowed(&adjusted_image)
    };

    // Defer binarization when no downstream consumer will mutate the result and
    // the encoder can binarize directly from the RGB image with zero intermediate
    // copy. Conditions: not a cover page (no fill mutation), no image regions
    // (no mask/merge mutation), OCR off (no consumer for binarized bytes), and
    // not jpeg base mode (jpeg path uses different luma handling).
    let can_defer_binarize = !is_cover_page
        && !has_image_regions
        && !config.crop_free_aspect()
        && !config.enable_ocr()
        && config.text_format() != "jpeg";

    // Grayscale-clean / MRC mode: clean the page to flat white, then derive an
    // ink-core bilevel buffer (0=ink, 255=paper) that feeds OCR, region masking,
    // and the JBIG2 mask; keep the cleaned grayscale for the MRC background.
    let mut cleaned_gray: Option<Vec<u8>> = None;
    let (mut binarized, deferred_binarize) = if force_blank_threshold {
        (vec![255; width * height], None)
    } else if config.is_grayscale_mode() && !is_cover_page && config.text_format() != "jpeg" {
        let opts =
            crate::clean_gray::CleanOptions::production_for_height(height, config.invert_input());
        // Adaptive-mask mode: the ink mask comes from the Sauvola binarizer
        // on the raw render (keeps faint thin strokes — staff lines —
        // contiguous where any fixed cut on the cleaned image breaks them);
        // the cleaned gray keeps a mask-keyed antialiasing collar for the
        // JP2 background (see clean_page_for_mrc_with_mask).
        let clean_result = if config.mrc_adaptive_mask() {
            let mask = binarize_image(&binarization_image, &config, false);
            crate::clean_gray::clean_page_for_mrc_with_mask(
                binarization_image.as_raw(),
                width,
                height,
                &opts,
                &mask,
            )
            .map(|cleaned| (cleaned, mask))
        } else {
            crate::clean_gray::clean_page_for_mrc(
                binarization_image.as_raw(),
                width,
                height,
                &opts,
                config.mrc_ink_threshold(),
            )
        };
        match clean_result {
            Ok((cleaned, mask)) => {
                cleaned_gray = Some(cleaned);
                (mask, None)
            }
            Err(e) => {
                log::warn!(
                    "clean-gray failed on page {page_index}: {e}; falling back to binarization"
                );
                (
                    binarize_image(&binarization_image, &config, force_blank_threshold),
                    None,
                )
            }
        }
    } else if can_defer_binarize {
        (
            Vec::new(),
            Some(crate::pipeline::policies::binarize_options_for(
                &config,
                force_blank_threshold,
            )),
        )
    } else if config.text_format() == "jpeg" {
        // DEBUG: should not reach here for non-jpeg pages with images
        // Luma-from-RGB pass for jpeg base mode (full-page color encoded later).
        let luma: Vec<u8> = binarization_image
            .as_raw()
            .chunks_exact(3)
            .map(|rgb| {
                let r = rgb[0] as f32;
                let g = rgb[1] as f32;
                let b = rgb[2] as f32;
                ((0.299 * r + 0.587 * g + 0.114 * b) as u8).max(1)
            })
            .collect();
        (luma, None)
    } else {
        let bin = binarize_image(&binarization_image, &config, force_blank_threshold);
        (bin, None)
    };
    drop(binarization_image);

    #[cfg(feature = "debug-logging")]
    if !binarized.is_empty() {
        let ink = binarized.iter().filter(|&&v| v == 0).count();
        crate::debug_println!(
            "CROPBIN page={} binarized ink_frac={:.3}",
            page_index,
            ink as f64 / binarized.len() as f64
        );
    }

    // (Removed the `dark_fraction >= 0.85 → fill white` bandaid: it masked the GPU
    // black-page bug whose root cause — Otsu on the raw instead of bg-normalized
    // histogram — is now fixed. A legitimately dark page must no longer be blanked.)

    // Cover page encoding must happen before region processing (it fills binarized with white).
    let cover_encoded_data = if is_cover_page {
        // Use synchronous version for cover page encoding within the blocking task
        let cover_result = encode_region_image_sync(
            adjusted_image.as_raw(),
            width as u32,
            height as u32,
            *config.cover_format(),
            true,
            config.high_quality_output(),
            config.jpeg_compat(),
        )
        .map_err(|e| anyhow!("Failed to encode cover image: {}", e))?;

        binarized.fill(255); // Fill binarized with white for cover pages
        Some(cover_result)
    } else {
        None
    };

    // JBIG2: Stucki or halftone (`jbig2halftone.rs`) on image labels. CCITT4: clustered-dot 4×4 only.
    // Modes are format-locked in `process_image_region`. Layout must be on — never on full-page text / HOCR.
    let mut region_processing_results = Vec::new();

    for det in &adjusted_detections {
        if !classifier.is_image_label(det) {
            continue;
        }
        if has_substantive_text_detection
            && bbox_is_effectively_full_page(det.bbox, width as u32, height as u32, 0.90)
        {
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

        // Image-region dithering (format-specific; see `process_image_region`) requires layout; if layout is off,
        // `image_region_mode` is None and we only mask/crop — full-page binarization is unchanged.
        let suppress_dither = config.keep_original_images()
            || is_cover_page
            || matches!(config.text_format(), "djvu" | "jpeg")
            || !config.enable_layout_detection();
        let image_region_mode = if suppress_dither {
            ImageRegionDitherMode::None
        } else {
            config.image_region_dither_mode()
        };

        let should_dither = image_region_mode != ImageRegionDitherMode::None;

        // CPU-heavy: Extract and process region (same bounds as masking / placement)
        let (region_data, region_w, region_h) =
            crate::color::color_processing::process_image_region(
                adjusted_image.as_raw(),
                width as u32,
                height as u32,
                exact_bbox,
                image_region_mode,
                config.text_format(),
                false,
            )?;

        let mut encoded_data = None;
        let mut encoded_global_data: Option<Vec<u8>> = None;

        // Mask the region in the base layer with padding so any overlay fully
        // covers remnant binarized pixels at the edges. Cover pages skip the
        // dither/overlay path entirely but still mask via the else branch below.
        let pad: u32 = 3;
        let mx1 = bbox_x1.saturating_sub(pad);
        let my1 = bbox_y1.saturating_sub(pad);
        let mx2 = (bbox_x2 + pad).min(width as u32);
        let my2 = (bbox_y2 + pad).min(height as u32);
        crate::color::color_processing::mask_region(
            &mut binarized,
            width as u32,
            [mx1 as f32, my1 as f32, mx2 as f32, my2 as f32],
        );
        let processed_for_masking = true;

        if should_dither && !is_cover_page {
            let grayscale_data: Vec<u8> = region_data.chunks(3).map(|rgb| rgb[0]).collect();

            if image_region_mode == ImageRegionDitherMode::GrayJp2 {
                // Grayscale JP2 overlay: skip bilevel dithering, encode directly as jp2-gray.
                use crate::encoding::{
                    EncodingManager, EncodingResult, EncodingSettings,
                    ImageBuffer as LegeImageBuffer,
                };
                let buffer = LegeImageBuffer {
                    data: &grayscale_data,
                    width: region_w,
                    height: region_h,
                    channels: 1,
                };
                let q =
                    crate::pipeline::quality_policy::region_gray_jp2(config.high_quality_output());
                match EncodingManager::encode(&buffer, &EncodingSettings::Jp2Lam { quality: q }) {
                    Ok(EncodingResult::Standard(data)) => {
                        encoded_data = Some((data, "jp2-gray".to_string()));
                    }
                    _ => {}
                }
            } else if image_region_mode == ImageRegionDitherMode::Halftone
                && config.text_format() == "jbig2"
            {
                // Halftone overlay: grayscale → jbig2halftone.rs (halftone region segments)
                // Invert grayscale so that bright→low pattern index (few dots) and
                // dark→high pattern index (many dots).  Combined with Decode [1, 0]
                // in the PDF this yields black dots on a white default — traditional
                // halftone polarity whose white background blends with the base layer.
                // Also clamp near-white to pure white before inverting to avoid sparse
                // dots from paper texture / scan noise.
                let inverted_gray: Vec<u8> = grayscale_data
                    .iter()
                    .map(|&g| {
                        if g >= crate::color::color_processing::PAPER_WHITE_FLOOR {
                            0u8
                        } else {
                            // Linearize before inverting so halftone dot coverage tracks
                            // perceived tone (bilevel tone == linear reflectance).
                            255u8 - crate::color::linearize::SRGB_GRAY_TO_LINEAR_U8[g as usize]
                        }
                    })
                    .collect();

                match crate::encoding::encode_halftone_region_grayscale(
                    &inverted_gray,
                    region_w,
                    region_h,
                ) {
                    Ok((global_data, page_data)) => {
                        encoded_global_data = Some(global_data);
                        encoded_data = Some((page_data, "jbig2".to_string()));
                    }
                    Err(_e) => {
                        // Fall back to JBIG2 Generic
                        use crate::encoding::{
                            EncodingManager, EncodingResult, EncodingSettings,
                            ImageBuffer as LegeImageBuffer, Jbig2Settings,
                        };
                        let buffer = LegeImageBuffer {
                            data: &grayscale_data,
                            width: region_w,
                            height: region_h,
                            channels: 1,
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
                // Stucki / clustered-dot overlay: bilevel data, encode matching the page format
                use crate::encoding::{
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
                    Ok(EncodingResult::Jbig2WithGlobals {
                        page_data,
                        global_data: _,
                    }) => {
                        encoded_data = Some((page_data, overlay_fmt));
                    }
                    Err(_e) => {}
                }
            }

            // Masked for overlay but no bitmap produced (encoder failure / halftone fallback miss):
            // merge dithered grayscale into base so the page is not an empty white hole.
            if encoded_data.is_none() {
                crate::color::color_processing::merge_dithered_region(
                    &mut binarized,
                    &grayscale_data,
                    width as u32,
                    exact_bbox,
                );
            }
        } else {
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
                        config.jpeg_compat(),
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
        ocr_image,
        binarized,
        cleaned_gray,
        deferred_binarize,
        width,
        height,
        is_cover_page,
        cover_encoded_data,
        region_processing_results,
        native_text_transform,
    })
}

/// Apply region policy transform (margin correction, layout)
/// Apply document-wide margin analysis to a single page
fn apply_margin_analysis_to_page(
    page: &RenderedPageData,
    detections: Vec<crate::engine::Detection>,
    detections_are_page_space: bool,
    cfg: &PipelineConfig,
    analysis: &DocumentMarginAnalysis,
    page_index: usize,
) -> Result<(RgbImage, Vec<crate::engine::Detection>, NativeTextTransform)> {
    let adjusted = adjust_page_with_margin_analysis(
        page,
        detections,
        detections_are_page_space,
        cfg,
        analysis,
        page_index,
    )?;
    Ok((
        adjusted.image,
        adjusted.detections,
        NativeTextTransform {
            source_width: page.high_res_image.width(),
            source_height: page.high_res_image.height(),
            correction: adjusted.correction,
        },
    ))
}

#[allow(dead_code)]
fn apply_margin_analysis_to_page_legacy(
    page: &RenderedPageData,
    detections: Vec<crate::engine::Detection>,
    detections_are_page_space: bool,
    cfg: &PipelineConfig,
    analysis: &DocumentMarginAnalysis,
    page_index: usize,
) -> Result<(RgbImage, Vec<crate::engine::Detection>, NativeTextTransform)> {
    use crate::pipeline::policies::remap_detections_to_page;

    // Remap detections from inference space to page space unless the analysis
    // cache already scaled them into this rendered page's coordinate system.
    let mut dets = detections;
    let page_w = page.high_res_image.width();
    let page_h = page.high_res_image.height();
    if !detections_are_page_space {
        remap_detections_to_page(&mut dets, page_w, page_h, cfg);
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
    let scaled_crop = analysis.crop_bounds.scale_to_resolution(
        analysis.analysis_width,
        analysis.analysis_height,
        page_w,
        page_h,
    );

    // Get page-specific margin data from analysis
    let page_data = analysis.pages.get(&page_index);

    let page_is_non_crop_content = page_data
        .map(|pd| pd.is_blank || pd.is_full_page_image)
        .unwrap_or(false);
    let full_page_bounds = crate::margin::ContentBounds {
        min_x: 0,
        min_y: 0,
        max_x: page_w,
        max_y: page_h,
    };

    let bounds =
        if analysis.effective_margin_setting == crate::margin::MarginSettings::CropAndResize {
            Some(if page_is_non_crop_content {
                full_page_bounds
            } else if cfg.crop_free_aspect() {
                // Window bounds must cover ALL content — text AND figures. Text-only
                // bounds cropped chapter woodcuts (and any illustration above or
                // below the text block) out of the page.
                let page_text_bounds =
                    crate::margin::calculate_content_bounds(&dets, page_w, page_h, true)
                        .or_else(|| {
                            page_data.and_then(|pd| {
                                pd.content_bounds.map(|cb| {
                                    cb.scale_to_resolution(
                                        analysis.analysis_width,
                                        analysis.analysis_height,
                                        page_w,
                                        page_h,
                                    )
                                })
                            })
                        })
                        // Layout-off crop: no detections anywhere — measure the page's
                        // ink directly so per-page heights work there too.
                        .or_else(|| compute_pixel_bounds_for_margin(&page.high_res_image, cfg))
                        .unwrap_or(scaled_crop);
                // Free-aspect: uniform document WIDTH (stable text scale and
                // zoom-to-width across pages) but per-page HEIGHT hugging this
                // page's own content — sparse pages (chapter ends, part titles)
                // no longer carry the tallest page's blank space. Floor keeps
                // near-empty pages from collapsing into slivers.
                let pad_y = ((scaled_crop.height() as f32) * 0.015).round().max(4.0) as u32;
                let min_h = ((scaled_crop.height() as f32) * 0.35).round().max(1.0) as u32;
                let page_window_h = page_text_bounds
                    .height()
                    .saturating_add(pad_y * 2)
                    .clamp(min_h, scaled_crop.height().max(1));
                crate::margin::fit_crop_window_to_content(
                    &page_text_bounds,
                    scaled_crop.width().max(1),
                    page_window_h,
                    page_w,
                    page_h,
                )
            } else {
                scaled_crop
            })
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
                compute_pixel_bounds_for_margin(&page.high_res_image, cfg)
            }
        };

    if let Some(bounds) = bounds {
        // Apply effective margin setting (may have been overridden due to footnotes)
        let effective_setting = if analysis.effective_margin_setting
            == crate::margin::MarginSettings::CropAndResize
            && page_is_non_crop_content
        {
            crate::margin::MarginSettings::StandardizeAndCenter
        } else {
            analysis.effective_margin_setting
        };

        // Free-aspect crop pages resize to a PER-PAGE height (uniform width,
        // uniform scale): the window already hugs this page's content, so the
        // output height is the document target scaled by this window's share
        // of the uniform window height.
        let free_aspect_crop = analysis.effective_margin_setting
            == crate::margin::MarginSettings::CropAndResize
            && cfg.crop_free_aspect()
            && !page_is_non_crop_content;

        let standard_dims = if free_aspect_crop {
            crate::margin::StandardPageDimensions {
                width: bounds.width().max(1),
                height: bounds.height().max(1),
            }
        } else if analysis.effective_margin_setting == crate::margin::MarginSettings::CropAndResize
        {
            crate::margin::StandardPageDimensions {
                width: scaled_crop.width().max(1),
                height: scaled_crop.height().max(1),
            }
        } else {
            // CRITICAL: Use SCALED baseline for standard dimensions (processing resolution, not analysis resolution)
            crate::margin::StandardPageDimensions {
                width: scaled_baseline.width().max(1),
                height: scaled_baseline.height().max(1),
            }
        };

        let (resize_target_width, resize_target_height) = if free_aspect_crop {
            let th = ((cfg.target_height().max(1) as f32) * (bounds.height().max(1) as f32)
                / (scaled_crop.height().max(1) as f32))
                .round()
                .max(1.0) as u32;
            (None, th)
        } else {
            (cfg.target_width(), cfg.target_height())
        };

        // Crop mode uses a uniform document crop; center/no-margin modes keep
        // their page-specific content bounds.
        let effective_bounds = bounds;

        // Process page with document-wide baseline
        match crate::margin::process_page_margins(
            &page.high_res_image,
            &effective_bounds,
            effective_setting,
            &standard_dims,
            resize_target_width,
            resize_target_height,
        ) {
            Ok(img) => {
                let mut new_dets = crate::margin::transform_detections(
                    &dets,
                    &effective_bounds,
                    effective_setting,
                    &standard_dims,
                    resize_target_width,
                    resize_target_height,
                    Some((page_w, page_h)),
                );

                // Clamp to new image bounds and drop degenerate boxes.
                let iw = img.width() as f32;
                let ih = img.height() as f32;
                new_dets.retain_mut(|det| {
                    det.bbox[0] = det.bbox[0].clamp(0.0, iw);
                    det.bbox[1] = det.bbox[1].clamp(0.0, ih);
                    det.bbox[2] = det.bbox[2].clamp(0.0, iw);
                    det.bbox[3] = det.bbox[3].clamp(0.0, ih);
                    det.bbox[0] < det.bbox[2] && det.bbox[1] < det.bbox[3]
                });

                let correction = crate::margin::compute_margin_correction(
                    &effective_bounds,
                    effective_setting,
                    &standard_dims,
                    resize_target_width,
                    resize_target_height,
                    Some((page_w, page_h)),
                );
                return Ok((
                    img,
                    new_dets,
                    NativeTextTransform {
                        source_width: page_w,
                        source_height: page_h,
                        correction,
                    },
                ));
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
    Ok((
        (*page.high_res_image).clone(),
        dets,
        NativeTextTransform {
            source_width: page.high_res_image.width(),
            source_height: page.high_res_image.height(),
            correction: identity_margin_correction(),
        },
    ))
}

fn apply_region_policy(
    rendered: &RenderedPageData,
    inference_result: &InferenceResult,
    config: &PipelineConfig,
) -> Result<(RgbImage, Vec<crate::engine::Detection>, NativeTextTransform)> {
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

    let page_w = rendered.high_res_image.width();
    let page_h = rendered.high_res_image.height();

    let mut dets_for_bounds = inference_result.detections.clone();
    crate::pipeline::policies::remap_detections_to_page(
        &mut dets_for_bounds,
        page_w,
        page_h,
        config,
    );

    let transform = match config.margin_settings() {
        crate::margin::MarginSettings::StandardizeAndCenter
        | crate::margin::MarginSettings::CropAndResize => {
            let bounds = if !dets_for_bounds.is_empty() {
                crate::margin::calculate_content_bounds(&dets_for_bounds, page_w, page_h, true)
            } else {
                compute_pixel_bounds_for_margin(&rendered.high_res_image, config)
            };
            if let Some(bounds) = bounds {
                let (standard_w, standard_h) = crate::pipeline::policies::standard_dimensions();
                let dims = crate::margin::StandardPageDimensions {
                    width: standard_w,
                    height: standard_h,
                };
                if dims.width > 0 && dims.height > 0 {
                    let setting = match config.margin_settings() {
                        crate::margin::MarginSettings::StandardizeAndCenter
                        | crate::margin::MarginSettings::CropAndResize => {
                            crate::margin::MarginSettings::StandardizeAndCenter
                        }
                        crate::margin::MarginSettings::None => crate::margin::MarginSettings::None,
                    };
                    crate::margin::compute_margin_correction(
                        &bounds,
                        setting,
                        &dims,
                        config.target_width(),
                        config.target_height(),
                        Some((page_w, page_h)),
                    )
                } else {
                    identity_margin_correction()
                }
            } else {
                identity_margin_correction()
            }
        }
        crate::margin::MarginSettings::None => identity_margin_correction(),
    };

    let (img, dets) = policy.transform(rendered, inference_result, config);
    Ok((
        img,
        dets,
        NativeTextTransform {
            source_width: page_w,
            source_height: page_h,
            correction: transform,
        },
    ))
}

/// Binarize image with special handling for blank pages in adaptive mode.
fn binarize_image(
    image: &RgbImage,
    config: &PipelineConfig,
    force_blank_threshold: bool,
) -> Vec<u8> {
    let options = crate::pipeline::policies::binarize_options_for(config, force_blank_threshold);
    crate::color::binarization::binarize_image_raw(
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
    jpeg_compat: bool,
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
    use crate::encoding::{EncodingManager, EncodingResult, ImageBuffer as LegeImageBuffer};

    let (settings, fmt_str) = crate::pipeline::helper_functions::region_encoding_settings(
        format,
        is_cover,
        high_quality,
        jpeg_compat,
    )?;

    let buffer = LegeImageBuffer {
        data: &image_data[..expected_len],
        width,
        height,
        channels: CHANNELS as u8,
    };
    let (encoding_result, fmt_str) = {
        (
            EncodingManager::encode(&buffer, &settings)
                .map_err(|e| anyhow!("Region encoding failed: {}", e))?,
            fmt_str.to_string(),
        )
    };

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
    page_rgb: &RgbImage,
    cleaned_gray: Option<&[u8]>,
    width: usize,
    height: usize,
    detections: &[crate::engine::Detection],
    config: &PipelineConfig,
    page_index: usize,
) -> Result<Option<String>> {
    // Note: This function is only called when config.enable_ocr() is true

    // PP-OCR (paddle) backend: DBNet needs natural grayscale, not the 1bpp mask,
    // and does its own text-line detection. Run it on the page raster directly.
    #[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "paddle-ocr"))]
    {
        let _ = (binarized, width, height, detections);
        let result = crate::ocr::fast::perform_page_rgb_ocr(
            page_rgb,
            cleaned_gray,
            config.ocr_language(),
            config.invert_input(),
        )
        .await;
        return match result {
            Ok(text) => Ok(Some(text)),
            Err(e) => Err(anyhow!("Page {}: PaddleOCR failed: {e:#}", page_index)),
        };
    }

    #[cfg(not(all(any(target_os = "linux", target_os = "macos"), feature = "paddle-ocr")))]
    {
        let _ = (page_rgb, cleaned_gray);
        perform_ocr_binarized(binarized, width, height, detections, config, page_index).await
    }
}

/// Tesseract/WinOCR fast path: region- or tile-based OCR over the 1bpp mask.
#[cfg(not(all(any(target_os = "linux", target_os = "macos"), feature = "paddle-ocr")))]
async fn perform_ocr_binarized(
    binarized: &[u8],
    width: usize,
    height: usize,
    detections: &[crate::engine::Detection],
    config: &PipelineConfig,
    page_index: usize,
) -> Result<Option<String>> {
    let use_regions =
        crate::ocr::fast::should_use_region_ocr(config.enable_layout_detection(), detections);

    let result = if use_regions {
        crate::ocr::fast::perform_region_based_ocr(
            binarized,
            width,
            height,
            detections,
            config.ocr_language(),
        )
        .await
    } else {
        crate::ocr::fast::perform_tiling_based_ocr(binarized, width, height, config.ocr_language())
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
    pdf_renderer: Option<&Arc<PdfiumRenderer>>,
    page_index: usize,
    image: &RgbImage,
    text_transform: &NativeTextTransform,
) -> Result<Option<String>> {
    let Some(pdf_renderer) = pdf_renderer else {
        return Ok(None);
    };

    match pdf_renderer.has_text_layer(page_index as u32).await {
        Ok(true) => {
            match pdf_renderer
                .extract_positioned_text_words(
                    page_index as u32,
                    text_transform.source_width,
                    text_transform.source_height,
                )
                .await
            {
                Ok(words) => {
                    let mapped =
                        map_native_text_words(words, text_transform, image.width(), image.height());
                    let hocr =
                        build_hocr_from_positioned_words(&mapped, image.width(), image.height());
                    if !hocr.trim().is_empty() {
                        return Ok(Some(hocr));
                    }
                }
                Err(e) => {
                    warn_log!(
                        "Failed to extract positioned text from page {}: {}",
                        page_index,
                        e
                    );
                }
            }

            match pdf_renderer.extract_page_text(page_index as u32).await {
                Ok(raw_text) => Ok(Some(build_hocr_from_pdf_text(
                    &raw_text,
                    image.width(),
                    image.height(),
                ))),
                Err(e) => {
                    warn_log!("Failed to extract text from page {}: {}", page_index, e);
                    Ok(None)
                }
            }
        }
        Ok(false) => Ok(None),
        Err(e) => {
            warn_log!("Failed to check text layer for page {}: {}", page_index, e);
            Ok(None)
        }
    }
}

fn map_native_text_words(
    words: Vec<NativeTextWord>,
    text_transform: &NativeTextTransform,
    output_width: u32,
    output_height: u32,
) -> Vec<NativeTextWord> {
    words
        .into_iter()
        .filter_map(|mut word| {
            word.bbox = text_transform.apply(word.bbox, output_width, output_height)?;
            Some(word)
        })
        .collect()
}

/// Encode the MRC (grayscale) base layer: a JP2 grayscale background plus a
/// JBIG2 ink-mask stencil. Returns `(background, mask)` — the caller draws the
/// background first and the mask last so ink paints over the antialiased gray.
///
/// `binarized` is 0=ink / 255=paper (its ink pixels become the mask; image
/// regions were already whited out to 255 by the region loop).
///
/// MASK MODE: pinned to JBIG2 **Generic**, unlike the bilevel base layer's
/// Symbol default. This is a renderer constraint, not an oversight: pdfium
/// renders a symbol-mode `/ImageMask` stencil BLANK — with the dictionary in
/// a JBIG2Globals stream or inlined ahead of the page segments — while the
/// same symbol streams render fine as opaque images (Lege's normal JBIG2
/// output). Verified 2026-07 on crusades p22: generic mask 14.0 KB renders,
/// symbol mask 7.8 KB renders blank both ways. Revisit if pdfium gains
/// symbol support in its ImageMask path (flip `jbig2_mode` below).
async fn encode_mrc_base_layer(
    cleaned_gray: Vec<u8>,
    binarized: Vec<u8>,
    width: usize,
    height: usize,
    config: &Arc<PipelineConfig>,
    page_index: usize,
    force_generic: bool,
) -> Result<(
    crate::accumulator::ContentType,
    crate::accumulator::ContentType,
)> {
    use crate::encoding::{
        EncodingManager, EncodingResult, EncodingSettings, ImageBuffer as LegeImageBuffer,
        Jbig2Settings,
    };
    let bg_quality = config.mrc_bg_quality();
    let subsample_override = config.mrc_bg_subsample_override();
    let adaptive_mask = config.mrc_adaptive_mask();
    // See the doc comment: Generic is required for pdfium ImageMask rendering.
    // `force_generic` is kept in the signature so the Abandon-region rule stays
    // wired for the day symbol becomes usable here.
    let _ = force_generic;
    let jbig2_mode = crate::encoding::Jbig2Mode::Generic;
    tokio::task::spawn_blocking(move || {
        let buffer = LegeImageBuffer {
            data: &binarized,
            width: width as u32,
            height: height as u32,
            channels: 1u8,
        };
        let settings = EncodingSettings::Jbig2(Jbig2Settings {
            pdf_fragment_mode: true,
            mode: jbig2_mode,
            use_jbig2_halftone_segments: false,
        });
        let (page_data, global_data) = match EncodingManager::encode(&buffer, &settings)
            .map_err(|e| anyhow!("jbig2 mask encode: {e}"))?
        {
            EncodingResult::Standard(data) => (data, Vec::new()),
            EncodingResult::Jbig2WithGlobals {
                page_data,
                global_data,
            } => {
                // pdfium renders a symbol-mode /ImageMask blank when the
                // dictionary arrives via a JBIG2Globals DecodeParms stream
                // (the opaque-image path handles it fine). Inlining the
                // dictionary segments ahead of the page segments is equally
                // legal embedded JBIG2 and renders correctly.
                let mut inline = global_data;
                inline.extend_from_slice(&page_data);
                (inline, Vec::new())
            }
        };
        let mask_content = crate::accumulator::ContentType::Jbig2Mask {
            page_data: Arc::from(page_data),
            global_data: Arc::from(global_data),
            pixel_width: width as u32,
            pixel_height: height as u32,
        };

        // Background: cleaned gray with ink filled white, box-downsampled, then
        // encoded as document-profile grayscale JP2. Auto subsample follows
        // RENDER RESOLUTION: the residual layer is the ~1px-per-1200px
        // antialiasing ring around the mask, and downsampling below full
        // resolution at 1200px averages that ring into pure white — deleting
        // the antialiasing that grayscale mode exists to preserve. At 2600px
        // the ring is ~3px, so ×3 is safe. Size stays modest at ×1 because
        // the threshold-180 mask leaves only the ring in this layer.
        let subsample = subsample_override.unwrap_or_else(|| {
            let auto = match height {
                0..=1799 => 1,
                1800..=2399 => 2,
                _ => 3,
            };
            if adaptive_mask {
                // Adaptive-mask mode: the background holds only the synthetic
                // antialiasing rings (no real structure to smear), and ×1
                // encodes their hard steps expensively with visible wavelet
                // ringing at high zoom. ×2 box-downsampling turns the rings
                // into smooth gradients — cleaner AND ~2.5× smaller.
                auto.max(2)
            } else {
                auto
            }
        });
        let (bg, ow, oh) =
            crate::clean_gray::mrc_background(&cleaned_gray, &binarized, width, height, subsample);
        let jp2 = crate::encoding::jp2::encode_gray_document(&bg, ow as u32, oh as u32, bg_quality)
            .map_err(|e| anyhow!("jp2 background encode: {e}"))?;
        let bg_content = crate::accumulator::ContentType::EncodedImage {
            data: Arc::from(jp2),
            pixel_width: ow as u32,
            pixel_height: oh as u32,
            format: "jp2-gray".to_string(),
        };

        crate::bbox_trace!(
            "PAGE {} MRC base bg={}x{} bytes={} mask_bytes={}",
            page_index,
            ow,
            oh,
            bg_content.as_bytes().len(),
            mask_content.as_bytes().len()
        );
        Ok((bg_content, mask_content))
    })
    .await
    .map_err(|e| anyhow!("MRC encode task panicked: {e}"))?
}

/// Encode base layer (binarized image) using the full encoding pipeline
/// Supports JBIG2, CCITT4, and JPEG formats with proper settings.
/// `force_jbig2_generic` overrides Symbol/SymUnify → Generic when abandon regions are present.
async fn encode_base_layer(
    binarized: Vec<u8>,
    width: usize,
    height: usize,
    config: &PipelineConfig,
    page_index: usize,
    force_jbig2_generic: bool,
) -> Result<crate::accumulator::ContentType> {
    use crate::accumulator::ContentType;
    use crate::encoding::{
        EncodingManager, EncodingResult, EncodingSettings, ImageBuffer as LegeImageBuffer,
        Jbig2Settings, JpegSettings,
    };

    #[cfg(feature = "debug-logging")]
    let encoding_start = std::time::Instant::now();

    // Determine encoding settings
    let jbig2_mode = if force_jbig2_generic {
        crate::encoding::Jbig2Mode::Generic
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
                quality: crate::pipeline::quality_policy::full_page_jpeg_text(
                    config.high_quality_output(),
                ),
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
    let text_format = config.text_format().to_string();
    let encode_sem = crate::pipeline::helper_functions::get_encode_semaphore();
    let permit = match encode_sem {
        Some(sem) => Some(sem.acquire_owned().await.ok()),
        None => None,
    };

    let encoding_result = tokio::task::spawn_blocking(move || {
        let buffer = LegeImageBuffer {
            data: &binarized,
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

/// Fused binarize + base-layer encode in a single spawn_blocking task.
///
/// Used when the binarized buffer needs no post-binarization mutation (no cover, no image
/// regions, OCR off). The GPU binarizer's mapped readback bytes flow directly into the
/// encoder via callback — no intermediate `Vec<u8>` allocation between binarize and encode.
async fn encode_base_layer_fused(
    rgb_image: Arc<RgbImage>,
    bin_options: BinarizationOptions,
    width: usize,
    height: usize,
    config: &PipelineConfig,
    page_index: usize,
    force_jbig2_generic: bool,
) -> Result<crate::accumulator::ContentType> {
    use crate::accumulator::ContentType;
    use crate::encoding::{
        EncodingManager, EncodingResult, EncodingSettings, ImageBuffer as LegeImageBuffer,
        Jbig2Settings,
    };

    #[cfg(feature = "debug-logging")]
    let encoding_start = std::time::Instant::now();

    let jbig2_mode = if force_jbig2_generic {
        crate::encoding::Jbig2Mode::Generic
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
        other => {
            return Err(anyhow!(
                "fused encode path does not support text format '{}'",
                other
            ));
        }
    };

    let encode_sem = crate::pipeline::helper_functions::get_encode_semaphore();
    let permit = match encode_sem {
        Some(sem) => Some(sem.acquire_owned().await.ok()),
        None => None,
    };

    let text_format = config.text_format().to_string();

    let encoding_result = tokio::task::spawn_blocking(move || {
        crate::color::binarization::binarize_image_raw_with(
            rgb_image.as_raw(),
            width,
            height,
            &bin_options,
            |binarized| {
                if crate::bbox_trace::enabled() {
                    let dark = binarized.iter().filter(|&&b| b <= 128).count();
                    let light = binarized.len().saturating_sub(dark);
                    eprintln!(
                        "PAGE {} fused_binarized dark={} light={} total={}",
                        page_index,
                        dark,
                        light,
                        binarized.len()
                    );
                }
                let buffer = LegeImageBuffer {
                    data: binarized,
                    width: width as u32,
                    height: height as u32,
                    channels: 1u8,
                };
                EncodingManager::encode(&buffer, &encoding_settings)
                    .map_err(|e| anyhow!("Encoding failed for format {}: {}", text_format, e))
            },
        )
    })
    .await
    .map_err(|e| anyhow!("Fused encoding task panicked: {}", e))??;

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
                "[PROFILING] Page {} {} fused binarize+encode completed",
                page_index + 1,
                base_format
            );

            Ok(ContentType::EncodedImage {
                data: Arc::from(data),
                pixel_width: width as u32,
                pixel_height: height as u32,
                format: base_format.to_string(),
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
            Ok(ContentType::Jbig2ImageWithGlobals {
                page_data: Arc::from(page_data),
                global_data: Arc::from(global_data),
                pixel_width: width as u32,
                pixel_height: height as u32,
            })
        }
    }
}

/// Encode base layer as full-color image for full-page mode (JP2 by default, JPEG in compat mode)
pub(crate) async fn encode_base_layer_for_jpeg_mode(
    image: Arc<RgbImage>,
    config: &PipelineConfig,
    page_index: usize,
) -> Result<crate::accumulator::ContentType> {
    use crate::accumulator::ContentType;
    use crate::encoding::{
        EncodingManager, EncodingResult, EncodingSettings, ImageBuffer as LegeImageBuffer,
        JpegSettings,
    };

    #[cfg(feature = "debug-logging")]
    let encoding_start = std::time::Instant::now();

    let width = image.width();
    let height = image.height();
    let image_data = Arc::clone(&image);
    let jpeg_compat = config.jpeg_compat();
    let high_quality = config.high_quality_output();

    let encode_sem = crate::pipeline::helper_functions::get_encode_semaphore();
    let permit = match encode_sem {
        Some(sem) => Some(sem.acquire_owned().await.ok()),
        None => None,
    };

    let (data, format) = tokio::task::spawn_blocking(move || {
        let buffer = LegeImageBuffer {
            data: image_data.as_raw(),
            width,
            height,
            channels: 3u8,
        };
        let (settings, fmt) = if jpeg_compat {
            (
                EncodingSettings::Jpeg(JpegSettings {
                    quality: crate::pipeline::quality_policy::full_page_jpeg_compat(high_quality),
                    baseline: true,
                    optimized: true,
                    downsample: false,
                }),
                "jpeg",
            )
        } else {
            let q = crate::pipeline::helper_functions::jp2_quality(high_quality);
            (EncodingSettings::Jp2Lam { quality: q }, "jp2")
        };
        let result = EncodingManager::encode(&buffer, &settings)
            .map_err(|e| anyhow!("Full-page encoding failed: {}", e))?;
        match result {
            EncodingResult::Standard(data) => Ok((data, fmt.to_string())),
            _ => Err(anyhow!("Unexpected encoding result type for full-page")),
        }
    })
    .await
    .map_err(|e| anyhow!("Full-page encoding task panicked: {}", e))??;

    drop(permit);

    if data.is_empty() {
        return Err(anyhow!(
            "Full-page encoder returned empty data for {}x{} image",
            width,
            height
        ));
    }

    #[cfg(feature = "debug-logging")]
    crate::perf_log!(
        encoding_start,
        "[PROFILING] Page {} full-page encoding completed",
        page_index + 1
    );

    Ok(ContentType::EncodedImage {
        data: std::sync::Arc::from(data),
        pixel_width: width,
        pixel_height: height,
        format,
    })
}

//==============================================================================
// Phase 1: Document-Wide Margin Analysis (Low-Res Pass)
//==============================================================================

#[cfg(any())]
struct AnalysisPreparedPage {
    page_index: usize,
    analysis_image: RgbImage,
    pixel_bounds: Option<crate::margin::ContentBounds>,
}

#[cfg(any())]
struct AnalysisPageResult {
    page_index: usize,
    page_width: u32,
    page_height: u32,
    detections: Vec<crate::engine::Detection>,
    pixel_bounds: Option<crate::margin::ContentBounds>,
}

#[cfg(any())]
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
        method: crate::resize::ResizeMethod::Bell,
        letterbox: false,
        border_value: 0.0,
        swap_rb: false,
    };

    let analysis_image = match crate::resize::resize_bytes(
        original_image.as_raw(),
        original_image.width(),
        original_image.height(),
        &params,
        3,
    )
    .ok()
    .and_then(|bytes| RgbImage::from_raw(ANALYSIS_WIDTH, analysis_height, bytes))
    {
        Some(img) => img,
        None => {
            warn_log!(
                "Page {}: resize for margin analysis failed or produced wrong dimensions; using original.",
                page_idx
            );
            original_image
        }
    };

    let pixel_bounds = compute_pixel_bounds_for_margin(&analysis_image, &config);

    AnalysisPreparedPage {
        page_index: page_idx,
        analysis_image,
        pixel_bounds,
    }
}

#[cfg(any())]
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

        let mut detections = if config.layout_detection_enabled_for_page(page_index) {
            if let Some(handle) = inference_handle {
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
            }
        } else {
            Vec::new()
        };

        crate::pipeline::policies::remap_detections_to_page(
            &mut detections,
            analysis_image.width(),
            analysis_image.height(),
            &config,
        );

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
#[cfg(any())]
async fn perform_document_analysis(
    source: Arc<dyn PageSource>,
    config: Arc<PipelineConfig>,
    inference_handle: Option<Arc<crate::pipeline::inference::InferenceHandle>>,
    total_pages: usize,
    page_range: std::ops::Range<usize>,
    progress_tracker: &ProgressTracker,
) -> Result<(DocumentMarginAnalysis, Vec<CachedDetections>)> {
    info_log!("[Margin-Analysis] Phase 1: Analyzing document margins (Low-Res Pass)...");
    progress_tracker.update(crate::progress::ProcessingStatus::MarginPass1Analyzing);

    let mut margin_inputs = Vec::new();
    let mut detection_cache = vec![CachedDetections::Missing; source.page_count()];
    // Keep margin pass inference queued through the actor; PageSource handles whether
    // loading is Pdfium-backed or image-backed.
    let analysis_infer_concurrency = 1usize;
    let mut pending: FuturesUnordered<BoxFuture<'static, Result<AnalysisPageResult>>> =
        FuturesUnordered::new();

    let push_completed = |result: AnalysisPageResult,
                          margin_inputs: &mut Vec<PageMarginInput>,
                          detection_cache: &mut Vec<CachedDetections>| {
        margin_inputs.push(PageMarginInput {
            page_index: result.page_index,
            page_width: result.page_width,
            page_height: result.page_height,
            detections: result.detections.clone(),
            pixel_bounds: result.pixel_bounds,
        });

        if result.page_index < detection_cache.len() {
            detection_cache[result.page_index] = CachedDetections::Present {
                detections: result.detections,
                page_width: result.page_width,
                page_height: result.page_height,
            };
        }
    };

    // Load pages at source resolution first, then resize in memory for analysis.
    // For Pdfium sources this is the same render operation as before; for image-folder
    // sources the already-rendered image is decoded and fed through the same analysis path.
    for (idx, page_idx) in page_range.enumerate() {
        let source_page = match source.load_page(page_idx).await {
            Ok(source_page) => source_page,
            Err(e) => {
                warn_log!(
                    "Page {}: Failed to load during margin analysis: {}. Skipping page for margin analysis.",
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
                progress_tracker.publish_margin_progress(
                    margin_inputs.len().min(total_pages),
                    margin_inputs.len().min(total_pages),
                    0,
                    total_pages,
                );
                continue;
            }
        };

        let config_clone = config.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_analysis_page(page_idx, source_page.image, config_clone)
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
                progress_tracker.publish_margin_progress(
                    margin_inputs.len().min(total_pages),
                    margin_inputs.len().min(total_pages),
                    0,
                    total_pages,
                );
            }
        }

        if idx % 2 == 0 || idx == total_pages - 1 {
            let analyzed = margin_inputs.len() + pending.len();
            progress_tracker.publish_margin_progress(
                analyzed.min(total_pages),
                margin_inputs.len().min(total_pages),
                0,
                total_pages,
            );
        }
    }

    while let Some(result) = pending.next().await {
        push_completed(result?, &mut margin_inputs, &mut detection_cache);
        progress_tracker.publish_margin_progress(
            margin_inputs.len().min(total_pages),
            margin_inputs.len().min(total_pages),
            0,
            total_pages,
        );
    }

    margin_inputs.sort_by_key(|input| input.page_index);

    // Analyze margins across entire document
    info_log!(
        "[Margin-Analysis] Calculating document-wide baseline from {} pages...",
        margin_inputs.len()
    );
    let analysis = crate::margin::analyze_document_margins(
        &margin_inputs,
        &config,
        config.margin_settings(),
        config.crop_footnotes(),
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

pub async fn create_and_run_pdf_source_pipeline(
    source: Arc<dyn PageSource>,
    config: Arc<PipelineConfig>,
    output_path: &Path,
    page_range: Option<std::ops::Range<usize>>,
    progress_tracker: &ProgressTracker,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
    _progress_callback: impl Fn(usize, usize) + Send + Sync + 'static,
) -> Result<()> {
    info_log!("[PDF-Parallel] Starting parallel PDF pipeline");
    if config.text_format() == "jbig2" {
        info_log!(
            "[PDF-Parallel] JBIG2 encoder backend: {}",
            crate::encoding::active_jbig2_backend_info()
        );
    }

    if config.enable_reflow() {
        return crate::pipeline::reflow_pipeline::run_raster_reflow_pipeline(
            source,
            config,
            output_path,
            page_range,
            progress_tracker,
            shutdown_rx,
        )
        .await;
    }

    // Reset standard dimensions at the start of each new document
    crate::pipeline::reset_standard_dimensions();

    let mut config = config;
    let inference_handle = if config.enable_layout_detection() {
        match crate::pipeline::inference::InferenceHandle::new(&config) {
            Ok(handle) => Some(Arc::new(handle)),
            Err(e) if crate::pipeline::inference::is_layout_software_adapter_error(e.as_ref()) => {
                let msg = format!(
                    "No usable hardware GPU found — wgpu fell back to a CPU/software adapter. \
                     Layout detection has been disabled for this run. \
                     Install or update your GPU driver to enable hardware acceleration."
                );
                warn_log!("[PDF-Parallel] {msg}");
                progress_tracker.update(crate::progress::ProcessingStatus::PipelineMessage {
                    stage: "GPU Warning".to_string(),
                    message: msg,
                });
                let mut fallback = (*config).clone();
                fallback.set_enable_layout_detection(false);
                config = Arc::new(fallback);
                None
            }
            Err(e) if crate::pipeline::inference::is_gpu_device_error(e.as_ref()) => {
                let msg = format!(
                    "GPU initialization failed ({}). Layout detection disabled; \
                     processing will continue without it. \
                     Check that your GPU driver supports DX12 (Windows) or Vulkan (Linux/macOS).",
                    e
                );
                warn_log!("[PDF-Parallel] {msg}");
                progress_tracker.update(crate::progress::ProcessingStatus::PipelineMessage {
                    stage: "GPU Warning".to_string(),
                    message: msg,
                });
                let mut fallback = (*config).clone();
                fallback.set_enable_layout_detection(false);
                config = Arc::new(fallback);
                None
            }
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

    let pipeline_config = PipelineRuntimeLimits::from_config(&config);
    init_encode_semaphore(pipeline_config.page_workers);

    // Calculate pages to process
    let document_pages = source.page_count();
    let page_start = page_range.as_ref().map(|r| r.start).unwrap_or(0);
    let page_end = page_range.as_ref().map(|r| r.end).unwrap_or(document_pages);
    let total_pages = page_end - page_start;
    let pdf_renderer = source.pdf_renderer();

    // Gate the GPU resize backend by document size: cold-start cost only pays
    // back once we have enough pages to amortize device init.
    const MIN_PAGES_FOR_GPU_RESIZE: usize = 10;
    crate::resize::set_gpu_resize_enabled(total_pages >= MIN_PAGES_FOR_GPU_RESIZE);

    // InferenceActor owns the resident WGPU layout graph; >1 here lets prep/postproc
    // overlap with the actor queue without creating extra GPU model instances.
    let infer_concurrency = pipeline_config.page_workers.max(1);
    let process_concurrency = pipeline_config.page_workers.max(1);

    info_log!(
        "[PDF-Parallel] Processing {} pages (GPU resize: {})",
        total_pages,
        if total_pages >= MIN_PAGES_FOR_GPU_RESIZE {
            "enabled"
        } else {
            "disabled (<10 pages)"
        },
    );
    info_log!("[PDF-Parallel] Processing {} pages with:", total_pages);
    info_log!("  - Render buffer: {}", pipeline_config.render_buffer);
    info_log!("  - Inference concurrency: {}", infer_concurrency);
    info_log!("  - Process concurrency: {}", process_concurrency);

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
        let (analysis, cache) = perform_document_margin_analysis(
            source.clone(),
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
        progress_tracker.clone(),
        use_margin_label,
        pipeline_config.channel_capacity,
    );

    let epub_sidecar_output = config.epub_sidecar_output().cloned();

    // Forward processed pages to PDF writer (mut for tokio::select!)
    let mut writer_forwarder = {
        let mut process_rx: mpsc::Receiver<ProcessedPage> = process_rx;
        let pdf_writer_handle = pdf_writer_handle.clone();
        let epub_sidecar_output = epub_sidecar_output.clone();

        tokio::spawn(async move {
            let mut hocr_pages = Vec::new();
            while let Some(processed_page) = process_rx.recv().await {
                if epub_sidecar_output.is_some()
                    && let Some(hocr) = processed_page.hocr_text.clone()
                    && !hocr.trim().is_empty()
                {
                    hocr_pages.push(crate::pipeline::epub_pipeline::HocrPage {
                        page_index: processed_page.index,
                        width_px: processed_page.width,
                        height_px: processed_page.height,
                        hocr,
                    });
                }

                // Convert ProcessedPage to accumulator::Page format
                let page = crate::accumulator::Page {
                    width: processed_page.width as f32,
                    height: processed_page.height as f32,
                    elements: processed_page.elements,
                    hocr_text: processed_page.hocr_text,
                    index: processed_page.index,
                    // PDF assembly never reads Page::binarized (it's for the DjVu writer);
                    // leaving it None keeps full-page grayscale buffers out of the writer's
                    // reorder buffer.
                    binarized: None,
                };

                pdf_writer_handle
                    .send_page(page, processed_page.index)
                    .await?;
            }

            // CRITICAL: Finalize the PDF after all pages are sent
            info_log!("[PDF-Parallel] Finalizing PDF...");
            pdf_writer_handle.finalize().await?;

            if let Some(epub_path) = epub_sidecar_output {
                if !hocr_pages.is_empty() {
                    let title = epub_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("Document")
                        .to_string();
                    info_log!(
                        "[PDF-Parallel] Assembling EPUB sidecar from existing OCR: {}",
                        epub_path.display()
                    );
                    tokio::task::spawn_blocking(move || {
                        crate::pipeline::epub_pipeline::build_epub_from_hocr_pages(
                            &hocr_pages,
                            &title,
                            &epub_path,
                        )
                    })
                    .await
                    .map_err(|e| anyhow!("EPUB sidecar task panicked: {}", e))??;
                } else {
                    warn_log!(
                        "[PDF-Parallel] EPUB sidecar requested, but no OCR text was available"
                    );
                }
            }

            Ok::<(), anyhow::Error>(())
        })
    };

    // Spawn pipeline stages (mut for tokio::select!)
    let mut render_task = tokio::spawn(source_stage(
        source.clone(),
        config.clone(),
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

    // Wait for all stages in pipeline order; abort remaining on cancellation.
    info_log!("[PDF-Parallel] Waiting for pipeline stages to complete...");
    use crate::pipeline::helper_functions::await_stage_or_cancel;
    let h_infer = infer_task.abort_handle();
    let h_process = process_task.abort_handle();
    let h_forwarder = writer_forwarder.abort_handle();
    let h_writer = pdf_writer_task.abort_handle();

    await_stage_or_cancel(
        &mut render_task,
        &mut shutdown_rx,
        "render",
        &[
            h_infer.clone(),
            h_process.clone(),
            h_forwarder.clone(),
            h_writer.clone(),
        ],
    )
    .await?;
    info_log!("[PDF-Parallel] Render stage complete");

    // Rendering is done; extract bookmarks while encoding pipeline is still humming.
    // Runs in a blocking task so it doesn't block the async executor.
    if let Some(renderer) = &pdf_renderer {
        let renderer = renderer.clone();
        let handle = pdf_writer_handle.clone();
        tokio::spawn(async move {
            let bookmarks = tokio::task::spawn_blocking(move || renderer.extract_bookmarks())
                .await
                .unwrap_or_default();
            if !bookmarks.is_empty() {
                let _ = handle
                    .send_bookmarks(bookmarks, std::collections::HashMap::new())
                    .await;
            }
        });
    }

    await_stage_or_cancel(
        &mut infer_task,
        &mut shutdown_rx,
        "inference",
        &[h_process.clone(), h_forwarder.clone(), h_writer.clone()],
    )
    .await?;
    info_log!("[PDF-Parallel] Inference stage complete");

    await_stage_or_cancel(
        &mut process_task,
        &mut shutdown_rx,
        "processing",
        &[h_forwarder.clone(), h_writer.clone()],
    )
    .await?;
    info_log!("[PDF-Parallel] Processing stage complete");

    await_stage_or_cancel(
        &mut writer_forwarder,
        &mut shutdown_rx,
        "writer forwarder",
        &[h_writer.clone()],
    )
    .await?;
    info_log!("[PDF-Parallel] Writer forwarder complete");

    await_stage_or_cancel(&mut pdf_writer_task, &mut shutdown_rx, "PDF writer", &[]).await?;
    info_log!("[PDF-Parallel] PDF writer complete");

    success_log!("PDF pipeline complete: {}", output_path.display());
    Ok(())
}

pub async fn create_and_run_pdf_parallel_pipeline(
    pdf_bytes: Arc<[u8]>,
    config: Arc<PipelineConfig>,
    output_path: &Path,
    page_range: Option<std::ops::Range<usize>>,
    progress_tracker: &ProgressTracker,
    shutdown_rx: tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
    progress_callback: impl Fn(usize, usize) + Send + Sync + 'static,
) -> Result<()> {
    let source: Arc<dyn PageSource> = Arc::new(PdfiumPageSource::new(pdf_bytes, config.clone())?);
    create_and_run_pdf_source_pipeline(
        source,
        config,
        output_path,
        page_range,
        progress_tracker,
        shutdown_rx,
        progress_callback,
    )
    .await
}

// Re-export the original function name for compatibility
pub use create_and_run_pdf_parallel_pipeline as create_and_run_pdf_tokio_pipeline;
