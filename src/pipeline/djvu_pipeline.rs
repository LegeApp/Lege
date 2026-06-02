//! Standalone DJVU pipeline - completely separate from PDF unified pipeline
//!
//! This pipeline handles DJVU document creation with full support for:
//! - Layout detection and region-based processing
//! - Margin processing (standardize/center and crop modes)
//! - Deskewing (rotation and unwarp)
//! - OCR with region-based and tiling modes
//! - All binarization options (heavy sauvola, fixed threshold, etc.)
//! - Page ranges
//! - Different page dimensions support
//! - **Concurrent document assembly** - pages are added to the document as they're processed
use crate::djvu::{DjvuConfig, DjvuOrchestrator, PageData, spawn_djvu_writer_actor}; // Use native encoder + writer actor
use crate::engine::Detection;
use crate::pagerender::prelude::PdfiumRenderer;
use crate::pipeline::config::{InferenceResult, PipelineConfig, RenderedPageData};
use crate::pipeline::helper_functions::{
    build_hocr_from_pdf_text, merge_overlapping_image_detections, rounded_clamped_bbox,
};
use crate::pipeline::page_analysis::{
    BLANK_PAGE_FALLBACK_THRESHOLD, is_visually_blank_page, maybe_apply_yolo_full_page_detection,
    should_force_blank_page_threshold,
};
use crate::pipeline::prepare_shared_deskew_engine;
use crate::pipeline::runtime_limits::PipelineRuntimeLimits;
use crate::pipeline::source::{PageSource, PdfiumPageSource, source_stage};
use crate::progress::ProgressTracker;
use crate::{info_log, warn_log};
use anyhow::{Result, anyhow};
use futures;
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use image::RgbImage;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::task::JoinHandle;
/// Result from inference stage (after layout detection)
#[derive(Debug, Clone)]
pub struct DjvuInferenceData {
    pub rendered: RenderedPageData,
    pub inference_result: InferenceResult,
}
/// Result from binarization stage (ready for DJVU submission)
#[derive(Debug, Clone)]
pub struct DjvuBinarizedData {
    pub index: usize,
    pub adjusted_image: Arc<RgbImage>,
    pub binarized: Vec<u8>,
    pub detections: Vec<Detection>,
    pub original_width_pts: f32,
    pub original_height_pts: f32,
    pub hocr_text: Option<String>,
}

struct EncodedDjvuPage {
    index: usize,
    encoded: djvu_encoder::doc::EncodedPage,
}
/// Helper function to create DJVU pipeline configuration
pub fn create_djvu_pipeline_config(
    output_path: &std::path::Path,
    config: &PipelineConfig,
) -> Result<DjvuConfig> {
    let work_dir = crate::app_dirs::djvu_work_dir_for(Some(output_path));
    let margin_setting = config.margin_settings();
    let center_active = matches!(
        margin_setting,
        crate::margin::MarginSettings::StandardizeAndCenter
    );
    let crop_active = matches!(margin_setting, crate::margin::MarginSettings::CropAndResize);
    let djvu_config = DjvuConfig {
        dpi: config.output_dpi(),
        clean: config.binarization().use_heavy_duty,
        lossy: None,
        iw44_quality: config.djvu_iw44_quality(), // Use quality from pipeline config
        work_dir,
        early_page_assembly: !(center_active || crop_active),
        pre_mask_color_layer: true,
        dither_image_regions: config.dither_images()
            && !config.keep_original_images()
            && config.enable_layout_detection()
            && config.text_format() != "jpeg",
        center_margins: center_active,
        crop_margins: crop_active,
        no_binarization_mode: config.text_format() == "jpeg", // Use "jpeg" text format to indicate no binarization
    };
    Ok(djvu_config)
}
/// Parallel tokio-based DJVU pipeline with TRUE concurrency
/// Uses tokio channels for pipelined async processing with enhanced parallel processing stages
pub async fn create_and_run_djvu_source_pipeline(
    source: Arc<dyn PageSource>,
    config: Arc<PipelineConfig>,
    output_path: &Path,
    page_range: Option<std::ops::Range<usize>>,
    progress_tracker: &ProgressTracker,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
    progress_callback: impl Fn(usize, usize) + Send + Sync + 'static,
) -> Result<()> {
    use tokio::sync::mpsc;
    #[cfg(feature = "debug-logging")]
    {
        info_log!("[DJVU-Parallel] Entering parallel tokio pipeline");
        info_log!(
            "[DJVU-Parallel] Layout detection: {}",
            config.enable_layout_detection()
        );
        info_log!(
            "[DJVU-Parallel] Margin settings: {:?}",
            config.margin_settings()
        );
        info_log!("[DJVU-Parallel] OCR enabled: {}", config.enable_ocr());
    }

    // Check for cancellation before starting
    if let Ok(signal) = shutdown_rx.try_recv() {
        return Err(anyhow::anyhow!(
            "Processing cancelled: {}",
            signal
                .message
                .unwrap_or_else(|| "User requested cancellation".to_string())
        ));
    }

    // Reset standard dimensions at the start of each new document
    crate::pipeline::reset_standard_dimensions();
    // Calculate total pages
    let document_pages = source.page_count();
    let total_pages = match &page_range {
        Some(range) => {
            if range.end > document_pages {
                return Err(anyhow!(
                    "Requested page range {:?} exceeds document length ({})",
                    range,
                    document_pages
                ));
            }
            range.len()
        }
        None => document_pages,
    };
    let page_index_offset: usize = page_range.as_ref().map(|r| r.start).unwrap_or(0);
    let pdf_renderer = source.pdf_renderer();
    // Gate the GPU resize backend by document size: cold-start cost only pays
    // back once we have enough pages to amortize device init.
    const MIN_PAGES_FOR_GPU_RESIZE: usize = 10;
    crate::resize::set_gpu_resize_enabled(total_pages >= MIN_PAGES_FOR_GPU_RESIZE);
    #[cfg(feature = "debug-logging")]
    info_log!(
        "[DJVU-Parallel] Total pages to process: {} (GPU resize: {})",
        total_pages,
        if total_pages >= MIN_PAGES_FOR_GPU_RESIZE {
            "enabled"
        } else {
            "disabled (<10 pages)"
        },
    );
    // Create DJVU orchestrator
    let djvu_config = create_djvu_pipeline_config(output_path, &config)?;
    let dpi = djvu_config.dpi; // Extract DPI before config is moved
    let iw44_quality = djvu_config.iw44_quality; // Extract quality setting (0-100)
    let orchestrator = Arc::new(DjvuOrchestrator::new(djvu_config)?);
    // Create shared inference handle if layout detection is enabled
    let shared_inference_handle: Option<Arc<crate::pipeline::inference::InferenceHandle>> =
        if config.enable_layout_detection() {
            match crate::pipeline::inference::InferenceHandle::new(&config) {
                Ok(handle) => Some(Arc::new(handle)),
                Err(e) => {
                    #[cfg(feature = "debug-logging")]
                    warn_log!(
                        "[DJVU-Parallel] Failed to create InferenceHandle: {}. Layout detection disabled.",
                        e
                    );
                    None
                }
            }
        } else {
            None
        };
    // Pipeline concurrency settings (similar to PDF pipeline)
    let pipeline_config = PipelineRuntimeLimits::djvu_from_config(&config);
    // NOTE: no init_encode_semaphore here. The global encode semaphore is only acquired by
    // encode_region_image/encode_page_data, which the DjVu path never calls — its heavy
    // IW44/JB2 encode runs inside the in_flight-bounded encode stage (capped at
    // djvu_encode_workers), so the semaphore would be redundant.
    let deskew_engine = prepare_shared_deskew_engine(&config)?;
    #[cfg(feature = "debug-logging")]
    info_log!(
        "[DJVU-Parallel] Pipeline configured with: render_buffer={}, inference_buffer={}, page_workers={}, process_workers={}, djvu_encode_workers={}",
        pipeline_config.render_buffer,
        pipeline_config.inference_buffer,
        pipeline_config.page_workers,
        pipeline_config.process_workers,
        pipeline_config.djvu_encode_workers
    );
    // Create channels with larger buffers for better pipelining
    let (render_tx, render_rx) = mpsc::channel::<RenderedPageData>(pipeline_config.render_buffer);
    let (infer_tx, infer_rx) = mpsc::channel::<DjvuInferenceData>(pipeline_config.inference_buffer);
    let (binarize_tx, binarize_rx) =
        mpsc::channel::<DjvuBinarizedData>(pipeline_config.channel_capacity);
    // Setup progress tracking
    let layout_enabled = config.enable_layout_detection();
    let external_cb: Arc<dyn Fn(usize, usize) + Send + Sync + 'static> =
        Arc::new(progress_callback);
    let (render_count, detect_count, encode_count) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    );
    // Spawn source task. PDF input is serialized behind Pdfium; image-folder input
    // can fan out through the same downstream pipeline.
    let mut render_task: JoinHandle<Result<()>> = {
        let config = config.clone();
        let source = source.clone();
        let tracker = progress_tracker.clone();
        let page_start = page_index_offset;
        let page_end = page_start + total_pages;
        let rc = render_count.clone();
        let dc = detect_count.clone();
        let ec = encode_count.clone();
        tokio::spawn(source_stage(
            source,
            config,
            deskew_engine,
            page_start..page_end,
            render_tx,
            rc,
            dc,
            ec,
            tracker,
            total_pages,
            layout_enabled,
        ))
    };
    // Spawn inference stage with TRUE concurrency (similar to PDF pipeline) (mut for tokio::select!)
    let mut infer_task: JoinHandle<Result<()>> = {
        let tracker = progress_tracker.clone();
        let rc = render_count.clone();
        let dc = detect_count.clone();
        let ec = encode_count.clone();
        let mut render_rx = render_rx;
        let handle_clone = shared_inference_handle.clone();
        let total_pages = total_pages;
        let concurrency = pipeline_config.page_workers;
        tokio::spawn(async move {
            #[cfg(feature = "debug-logging")]
            info_log!(
                "[DJVU-Parallel-Infer] Starting inference stage with concurrency={}",
                concurrency
            );
            // Track in-flight inference tasks
            let mut in_flight: FuturesUnordered<BoxFuture<'static, Result<DjvuInferenceData>>> =
                FuturesUnordered::new();
            let mut input_exhausted = false;
            loop {
                tokio::select! {
                    biased; // Prioritize completing work over starting new work
                    // Collect completed inference results
                    Some(result) = in_flight.next(), if !in_flight.is_empty() => {
                        match result {
                            Ok(data) => {
                                let detected_val = dc.fetch_add(1, Ordering::Relaxed) + 1;
                                if layout_enabled {
                                    let rendered_val = rc.load(Ordering::Relaxed);
                                    let encoded_val = ec.load(Ordering::Relaxed);
                                    tracker.publish_layout_progress(rendered_val, detected_val, encoded_val, 0, total_pages);
                                }
                                infer_tx.send(data).await.map_err(|e| anyhow!("Infer send failed: {}", e))?;
                            }
                            Err(e) => {
                                return Err(anyhow!("[DJVU-Parallel-Infer] Inference task failed: {:#}", e));
                            }
                        }
                    }
                    // Accept new work if we have capacity
                    Some(rendered) = render_rx.recv(), if in_flight.len() < concurrency && !input_exhausted => {
                        in_flight.push(build_djvu_inference_future(handle_clone.clone(), rendered));
                    }
                    // Input channel closed
                    else => {
                        if !input_exhausted && render_rx.is_closed() {
                            input_exhausted = true;
                            info_log!("[DJVU-Parallel-Infer] Input exhausted, draining {} in-flight tasks", in_flight.len());
                        }
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
            drop(infer_tx); // Close channel
            #[cfg(feature = "debug-logging")]
            info_log!("[DJVU-Parallel-Infer] Inference stage complete");
            Ok(())
        })
    };
    // Spawn binarization & text extraction stage with TRUE concurrency (similar to PDF pipeline) (mut for tokio::select!)
    let mut binarize_task: JoinHandle<Result<()>> = {
        let config = config.clone();
        let pdf_renderer = pdf_renderer.clone();
        let tracker = progress_tracker.clone();
        let external_cb = external_cb.clone();
        let rc = render_count.clone();
        let dc = detect_count.clone();
        let ec = encode_count.clone();
        let mut infer_rx = infer_rx;
        let total_pages = total_pages;
        let page_index_offset = page_index_offset;
        let layout_enabled = layout_enabled;
        let concurrency = pipeline_config.process_workers;
        tokio::spawn(async move {
            #[cfg(feature = "debug-logging")]
            info_log!(
                "[DJVU-Parallel-Process] Starting binarization & text extraction stage with concurrency={}",
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
                            Ok(Ok(Some(processed_data))) => {
                                let encoded_val = ec.fetch_add(1, Ordering::Relaxed) + 1;
                                if layout_enabled {
                                    let rendered_val = rc.load(Ordering::Relaxed);
                                    let detected_val = dc.load(Ordering::Relaxed);
                                    tracker.publish_layout_progress(rendered_val, detected_val, encoded_val, 0, total_pages);
                                } else {
                                    tracker.publish_no_layout_progress(encoded_val, 0, total_pages);
                                }
                                external_cb(encoded_val, total_pages);
                                binarize_tx.send(processed_data).await.map_err(|e| anyhow!("Binarize send failed: {}", e))?;
                            }
                            Ok(Ok(None)) => {
                                // This means the page processing was skipped, continue
                            }
                            Ok(Err(e)) => {
                                #[cfg(feature = "debug-logging")]
                                crate::error_println!("[DJVU-Parallel-Process] Processing failed (page dropped): {:#}", e);
                                warn_log!("[DJVU-Parallel-Process] Processing failed: {}", e);
                            }
                            Err(e) => {
                                #[cfg(feature = "debug-logging")]
                                crate::error_println!("[DJVU-Parallel-Process] Task join error (page dropped): {:#}", e);
                                warn_log!("[DJVU-Parallel-Process] Task join error: {}", e);
                            }
                        }
                    }
                    // Accept new work if we have capacity
                    Some(inference_data) = infer_rx.recv(), if in_flight.len() < concurrency && !input_exhausted => {
                        let config_clone = config.clone();
                        let pdf_renderer_clone = pdf_renderer.clone();
                        // Spawn processing task
                        let task = tokio::spawn(async move {
                            process_single_djvu_page(
                                config_clone,
                                pdf_renderer_clone,
                                inference_data,
                                page_index_offset,
                            ).await
                        });
                        in_flight.push(task);
                    }
                    else => {
                        if !input_exhausted && infer_rx.is_closed() {
                            input_exhausted = true;
                        }
                        if input_exhausted && in_flight.is_empty() {
                            break;
                        }
                        if in_flight.is_empty() {
                            tokio::task::yield_now().await;
                        }
                    }
                }
            }
            drop(binarize_tx); // Close channel
            #[cfg(feature = "debug-logging")]
            info_log!("[DJVU-Parallel-Process] Binarization & text extraction stage complete");
            Ok(())
        })
    };
    // Spawn DjVu writer actor for concurrent document assembly. The writer
    // owns the assembly side; the shared `Arc<DjvuDocument>` lets the parallel
    // encode stage do the heavy IW44/JB2 encode off-thread.
    let (djvu_writer, djvu_doc, mut writer_task) = spawn_djvu_writer_actor(
        output_path.to_path_buf(),
        total_pages,
        dpi,
        iw44_quality,
        progress_tracker.clone(),
        pipeline_config.channel_capacity,
    );

    // Spawn encoding stage (encodes pages concurrently, then forwards to writer actor)
    let mut encode_task: JoinHandle<Result<()>> = {
        let orchestrator = orchestrator.clone();
        let djvu_writer = djvu_writer.clone();
        let djvu_doc = djvu_doc.clone();
        let mut binarize_rx = binarize_rx;
        let concurrency = pipeline_config.djvu_encode_workers;
        tokio::spawn(async move {
            #[cfg(feature = "debug-logging")]
            info_log!(
                "[DJVU-Parallel-Encode] Starting encoding stage with concurrency={}",
                concurrency
            );
            let mut in_flight: FuturesUnordered<BoxFuture<'static, Result<EncodedDjvuPage>>> =
                FuturesUnordered::new();
            let mut input_exhausted = false;

            loop {
                tokio::select! {
                    biased;
                    Some(result) = in_flight.next(), if !in_flight.is_empty() => {
                        let encoded_page = result?;
                        djvu_writer
                            .append_encoded(encoded_page.encoded, encoded_page.index)
                            .await?;
                    }
                    Some(binarized_data) = binarize_rx.recv(), if in_flight.len() < concurrency && !input_exhausted => {
                        let orchestrator = orchestrator.clone();
                        let djvu_doc = djvu_doc.clone();
                        in_flight.push(Box::pin(async move {
                            let page_index = binarized_data.index;
                            let page_data = PageData {
                                index: binarized_data.index,
                                // The Arc is not shared past this point, so unwrap_or_clone
                                // moves the full-page RgbImage out instead of deep-copying it
                                // (falls back to a clone only if a reference somehow remains).
                                rgb_image: Arc::unwrap_or_clone(binarized_data.adjusted_image),
                                binarized: binarized_data.binarized,
                                detections: binarized_data.detections,
                                hocr: binarized_data.hocr_text,
                            };

                            // Run the full per-page work — page assembly + IW44/JB2
                            // encode — inside one spawn_blocking so the heavy CPU
                            // cost runs concurrently with other pages instead of
                            // serialising in the writer actor.
                            let encoded = tokio::task::spawn_blocking(move || -> Result<_> {
                                let page = orchestrator.process_page(page_data)?;
                                let encoded = djvu_doc
                                    .encode_page(page)
                                    .map_err(|e| anyhow!("DjVu encode failed: {}", e))?;
                                Ok(encoded)
                            })
                            .await
                            .map_err(|e| anyhow!("DjVu encode task panicked: {}", e))??;

                            Ok(EncodedDjvuPage {
                                index: page_index,
                                encoded,
                            })
                        }));
                    }
                    else => {
                        if !input_exhausted && binarize_rx.is_closed() {
                            input_exhausted = true;
                        }
                        if input_exhausted && in_flight.is_empty() {
                            break;
                        }
                        if in_flight.is_empty() {
                            tokio::task::yield_now().await;
                        }
                    }
                }
            }

            djvu_writer.finalize().await?;

            #[cfg(feature = "debug-logging")]
            info_log!("[DJVU-Parallel-Encode] Encoding stage complete");
            Ok(())
        })
    };
    // Wait for all stages to complete with cancellation support
    #[cfg(feature = "debug-logging")]
    info_log!("[DJVU-Parallel] Waiting for pipeline stages to complete...");

    use crate::pipeline::helper_functions::await_stage_or_cancel;
    let h_infer = infer_task.abort_handle();
    let h_binarize = binarize_task.abort_handle();
    let h_encode = encode_task.abort_handle();

    await_stage_or_cancel(
        &mut render_task,
        &mut shutdown_rx,
        "render",
        &[h_infer.clone(), h_binarize.clone(), h_encode.clone()],
    )
    .await?;
    #[cfg(feature = "debug-logging")]
    info_log!("[DJVU-Parallel] Render stage complete");

    await_stage_or_cancel(
        &mut infer_task,
        &mut shutdown_rx,
        "inference",
        &[h_binarize.clone(), h_encode.clone()],
    )
    .await?;
    #[cfg(feature = "debug-logging")]
    info_log!("[DJVU-Parallel] Inference stage complete");

    await_stage_or_cancel(
        &mut binarize_task,
        &mut shutdown_rx,
        "binarization",
        &[h_encode.clone()],
    )
    .await?;
    #[cfg(feature = "debug-logging")]
    info_log!("[DJVU-Parallel] Binarization stage complete");

    await_stage_or_cancel(&mut encode_task, &mut shutdown_rx, "encoding", &[]).await?;
    #[cfg(feature = "debug-logging")]
    info_log!("[DJVU-Parallel] Encoding stage complete");

    #[cfg(feature = "debug-logging")]
    info_log!("[DJVU-Parallel] Waiting for writer actor to complete document assembly...");
    await_stage_or_cancel(&mut writer_task, &mut shutdown_rx, "document assembly", &[]).await?;
    #[cfg(feature = "debug-logging")]
    info_log!(
        "[DJVU-Parallel] Document assembly complete: {}",
        output_path.display()
    );

    // Cleanup
    if let Err(_e) = orchestrator.cleanup_work_dir_only() {
        #[cfg(feature = "debug-logging")]
        warn_log!("[DJVU-Parallel] Failed to clean up work directory: {}", _e);
    }

    #[cfg(feature = "debug-logging")]
    info_log!("[DJVU-Parallel] Pipeline complete");
    Ok(())
}

pub async fn create_and_run_djvu_pipeline(
    pdf_bytes: Arc<[u8]>,
    config: Arc<PipelineConfig>,
    output_path: &Path,
    page_range: Option<std::ops::Range<usize>>,
    progress_tracker: &ProgressTracker,
    shutdown_rx: tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
    progress_callback: impl Fn(usize, usize) + Send + Sync + 'static,
) -> Result<()> {
    let source: Arc<dyn PageSource> = Arc::new(PdfiumPageSource::new(pdf_bytes, config.clone())?);
    create_and_run_djvu_source_pipeline(
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

fn build_djvu_inference_future(
    shared_handle: Option<Arc<crate::pipeline::inference::InferenceHandle>>,
    rendered: RenderedPageData,
) -> BoxFuture<'static, Result<DjvuInferenceData>> {
    match shared_handle {
        Some(handle) => Box::pin(async move {
            let page_index = rendered.index;
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
                index: rendered.index,
                high_res_image: rendered.high_res_image.clone(),
                inference_image: rendered.inference_image.clone(),
                detections: detections.clone(),
                text_layer: None,
                original_width_pts: rendered.original_width_pts,
                original_height_pts: rendered.original_height_pts,
                has_no_detections: detections.is_empty(),
            };
            Ok(DjvuInferenceData {
                rendered,
                inference_result,
            })
        }) as BoxFuture<'static, Result<DjvuInferenceData>>,
        None => Box::pin(async move {
            let inference_result = InferenceResult {
                index: rendered.index,
                high_res_image: rendered.high_res_image.clone(),
                inference_image: rendered.inference_image.clone(),
                detections: Vec::new(),
                text_layer: None,
                original_width_pts: rendered.original_width_pts,
                original_height_pts: rendered.original_height_pts,
                has_no_detections: true,
            };
            Ok(DjvuInferenceData {
                rendered,
                inference_result,
            })
        }),
    }
}
/// Process a single DJVU page with OCR/text extraction in the async part
async fn process_single_djvu_page(
    config: Arc<PipelineConfig>,
    pdf_renderer: Option<Arc<PdfiumRenderer>>,
    inference_data: DjvuInferenceData,
    page_index_offset: usize,
) -> Result<Option<DjvuBinarizedData>> {
    let page_index = inference_data.rendered.index;
    let local_index = page_index.saturating_sub(page_index_offset);
    // CPU-heavy work in spawn_blocking (binarization and image processing only)
    let config_clone = config.clone();
    let input = DjvuPageProcessingInput {
        rendered: inference_data.rendered,
        inference_result: inference_data.inference_result,
        config: config_clone,
    };
    let cpu_result = tokio::task::spawn_blocking(move || process_djvu_cpu_intensive_work(input))
        .await
        .map_err(|e| anyhow!("CPU task panicked: {}", e))??;
    let DjvuPageProcessingOutput {
        adjusted_image,
        adjusted_detections,
        binarized,
        width,
        height,
        original_width_pts,
        original_height_pts,
    } = cpu_result;
    // OCR and text extraction in async part (not CPU-intensive, involves I/O and API calls)
    let hocr_text = extract_djvu_text_layer(
        &config,
        pdf_renderer.as_ref(),
        &binarized,
        width,
        height,
        &adjusted_detections,
        page_index,
    )
    .await;
    Ok(Some(DjvuBinarizedData {
        index: local_index,
        adjusted_image: Arc::new(adjusted_image),
        binarized,
        detections: adjusted_detections,
        original_width_pts,
        original_height_pts,
        hocr_text,
    }))
}
/// CPU-intensive work for a single DJVU page (to be executed in spawn_blocking)
struct DjvuPageProcessingInput {
    rendered: RenderedPageData,
    inference_result: InferenceResult,
    config: Arc<PipelineConfig>,
}
struct DjvuPageProcessingOutput {
    adjusted_image: RgbImage,
    adjusted_detections: Vec<crate::engine::Detection>,
    binarized: Vec<u8>,
    width: usize,
    height: usize,
    original_width_pts: f32,
    original_height_pts: f32,
}
/// Consolidated CPU-intensive work for a single DJVU page (binarization only)
fn process_djvu_cpu_intensive_work(
    input: DjvuPageProcessingInput,
) -> Result<DjvuPageProcessingOutput> {
    let DjvuPageProcessingInput {
        rendered,
        inference_result,
        config,
    } = input;
    // 1. Apply region policy (CPU-heavy: image resizing/cropping)
    let (mut adjusted_image, mut adjusted_detections) =
        apply_djvu_region_policy(&rendered, &inference_result, &config)?;
    // 2. Resize to target height if needed (CPU-heavy: Lanczos3 filtering).
    // Always normalize page dimensions regardless of layout mode — PDF source already
    // renders at target_height so this is a no-op there, but folder source supplies
    // images at native resolution and viewers reject DjVus built at those sizes.
    if adjusted_image.height() != config.target_height() {
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
            let mut scaled_detections = adjusted_detections.clone();
            for det in &mut scaled_detections {
                det.scale_bbox(sx, sy);
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
            let resized = crate::resize::resize_bytes(
                adjusted_image.as_raw(),
                current_w,
                current_h,
                &params,
                3,
            )
            .ok()
            .and_then(|bytes| RgbImage::from_raw(target_w, target_h, bytes));
            adjusted_image = match resized {
                Some(buf) => buf,
                None => image::imageops::resize(
                    &adjusted_image,
                    target_w,
                    target_h,
                    image::imageops::FilterType::Lanczos3,
                ),
            };
            adjusted_detections = scaled_detections;
        }
    }
    let width = adjusted_image.width() as usize;
    let height = adjusted_image.height() as usize;
    let classifier = &crate::types::LABEL_CLASSIFIER;

    if config.enable_layout_detection() {
        maybe_apply_yolo_full_page_detection(
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
    }

    // 2b. Pre-mask image regions before binarization so Sauvola only sees text.
    // Mirrors the PDF pipeline: image content skews adaptive threshold calculations for
    // adjacent text. The original adjusted_image is kept for dithering/encoding later.
    let has_image_regions = config.enable_layout_detection()
        && adjusted_detections
            .iter()
            .any(|det| classifier.is_image_label(det));

    let binarization_image: std::borrow::Cow<'_, RgbImage> = if has_image_regions {
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
    // In "jpeg" text format mode, skip binarization and use grayscale version for OCR/text layer
    let mut binarized = if config.text_format() == "jpeg" {
        // For JPEG-only mode in DJVU: create grayscale representation of the full RGB image
        // This is used for OCR purposes but the RGB image will be encoded as IW44
        adjusted_image
            .as_raw()
            .chunks_exact(3)
            .map(|rgb| {
                // Standard luminance conversion: 0.299*R + 0.587*G + 0.114*B
                let r = rgb[0] as f32;
                let g = rgb[1] as f32;
                let b = rgb[2] as f32;
                ((0.299 * r + 0.587 * g + 0.114 * b) as u8).max(1) // Ensure non-zero to avoid issues
            })
            .collect()
    } else {
        let force_blank_threshold = should_force_blank_page_threshold(
            &config,
            inference_result.has_no_detections,
            is_visually_blank_page(&adjusted_image),
            &adjusted_detections,
            width as u32,
            height as u32,
            classifier,
        );
        binarize_djvu_image(&binarization_image, &config, force_blank_threshold)
    };

    let should_dither_regions = config.dither_images()
        && !config.keep_original_images()
        && config.enable_layout_detection()
        && config.text_format() != "jpeg";

    if should_dither_regions {
        for det in &adjusted_detections {
            if !classifier.is_image_label(det) {
                continue;
            }

            let (bbox_x1, bbox_y1, bbox_x2, bbox_y2) =
                rounded_clamped_bbox(det.bbox, width as u32, height as u32);
            if bbox_x2 <= bbox_x1 || bbox_y2 <= bbox_y1 {
                continue;
            }

            let (region_data, region_w, region_h) =
                Legencode::color::color_processing::process_image_region(
                    adjusted_image.as_raw(),
                    width as u32,
                    height as u32,
                    det.bbox,
                    config.image_region_dither_mode(),
                    "djvu",
                    false,
                )?;

            let grayscale_data: Vec<u8> = region_data.chunks(3).map(|rgb| rgb[0]).collect();
            Legencode::color::color_processing::merge_dithered_region(
                &mut binarized,
                &grayscale_data,
                width as u32,
                det.bbox,
            );
        }
    }

    Ok(DjvuPageProcessingOutput {
        adjusted_image,
        adjusted_detections,
        binarized,
        width,
        height,
        original_width_pts: inference_result.original_width_pts,
        original_height_pts: inference_result.original_height_pts,
    })
}
/// Extract text layer via OCR or PDF text extraction (runs in async context)
async fn extract_djvu_text_layer(
    config: &PipelineConfig,
    pdf_renderer: Option<&Arc<PdfiumRenderer>>,
    binarized: &[u8],
    width: usize,
    height: usize,
    detections: &[crate::engine::Detection],
    page_index: usize,
) -> Option<String> {
    if config.enable_ocr() {
        let use_regions =
            crate::ocr::ocr::should_use_region_ocr(config.enable_layout_detection(), detections);
        #[cfg(feature = "debug-logging")]
        info_log!(
            "[extract_djvu_text_layer] Page {}: OCR enabled, use_regions={}, detections={}",
            page_index,
            use_regions,
            detections.len()
        );
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
            crate::ocr::ocr::perform_tiling_based_ocr(
                binarized,
                width,
                height,
                config.ocr_language(),
            )
            .await
        };
        match result {
            Ok(text) => {
                #[cfg(feature = "debug-logging")]
                info_log!(
                    "[extract_djvu_text_layer] Page {}: OCR returned {} chars",
                    page_index,
                    text.len()
                );
                Some(text)
            }
            Err(e) => {
                warn_log!("Page {}: OCR failed: {}", page_index, e);
                Some(String::new())
            }
        }
    } else {
        let Some(pdf_renderer) = pdf_renderer else {
            return None;
        };
        match pdf_renderer.has_text_layer(page_index as u32).await {
            Ok(true) => match pdf_renderer.extract_page_text(page_index as u32).await {
                Ok(raw_text) => {
                    let hocr = build_hocr_from_pdf_text(&raw_text, width as u32, height as u32);
                    #[cfg(feature = "debug-logging")]
                    info_log!(
                        "[extract_djvu_text_layer] Page {}: PDF text extracted, HOCR {} chars",
                        page_index,
                        hocr.len()
                    );
                    Some(hocr)
                }
                Err(e) => {
                    warn_log!("Failed to extract text from page {}: {}", page_index, e);
                    None
                }
            },
            Ok(false) => None,
            Err(e) => {
                warn_log!("Failed to check text layer for page {}: {}", page_index, e);
                None
            }
        }
    }
}
/// Apply region policy transform (margins, layout) for DJVU
fn apply_djvu_region_policy(
    rendered: &RenderedPageData,
    inference_result: &InferenceResult,
    config: &PipelineConfig,
) -> Result<(RgbImage, Vec<crate::engine::Detection>)> {
    let policy: Arc<dyn crate::pipeline::policies::RegionPolicy> = match config.margin_settings() {
        crate::margin::MarginSettings::StandardizeAndCenter
        | crate::margin::MarginSettings::CropAndResize => {
            Arc::new(crate::pipeline::policies::MarginStandardizeAndCenter)
        }
        crate::margin::MarginSettings::None => {
            if config.enable_layout_detection() {
                Arc::new(crate::pipeline::policies::LayoutRegions)
            } else {
                Arc::new(crate::pipeline::policies::NoLayoutFullPage)
            }
        }
    };
    Ok(policy.transform(rendered, inference_result, config))
}
/// Binarize DJVU image — delegates to the shared pipeline helper.
fn binarize_djvu_image(
    image: &RgbImage,
    config: &PipelineConfig,
    force_blank_threshold: bool,
) -> Vec<u8> {
    #[cfg(feature = "debug-logging")]
    if force_blank_threshold {
        crate::debug_log!(
            "Blank page detected via filtered detections, forcing fixed threshold {}",
            BLANK_PAGE_FALLBACK_THRESHOLD
        );
    }
    let options = crate::pipeline::policies::binarize_options_for(config, force_blank_threshold);
    Legencode::color::binarization::binarize_image_raw(
        image.as_raw(),
        image.width() as usize,
        image.height() as usize,
        &options,
    )
}
