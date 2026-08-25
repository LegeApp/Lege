//! Standalone DJVU pipeline - completely separate from PDF unified pipeline
//!
//! This pipeline handles DJVU document creation with full support for:
//! - Layout detection and region-based processing
//! - Margin processing (standardize/center and crop modes)
//! - OCR with region-based and tiling modes
//! - All binarization options (heavy sauvola, fixed threshold, etc.)
//! - Page ranges
//! - Different page dimensions support
//! - **Concurrent document assembly** - pages are added to the document as they're processed
use crate::djvu::{DjvuConfig, DjvuOrchestrator, PageData, spawn_djvu_writer_actor}; // Use native encoder + writer actor
use crate::engine::Detection;
use crate::margin::DocumentMarginAnalysis;
use crate::pipeline::config::{InferenceResult, PipelineConfig, RenderedPageData};
use crate::pipeline::helper_functions::{
    bookmarks_to_outline, build_hocr_from_pdf_text, merge_outline,
    merge_overlapping_image_detections, rounded_clamped_bbox, should_keep_image_overlay,
    should_preserve_cover_page,
};
use crate::pipeline::margin_pipeline::{
    CachedDetections, adjust_page_with_margin_analysis, cached_inference_result,
    perform_document_margin_analysis,
};
use crate::pipeline::page_analysis::{
    BLANK_PAGE_FALLBACK_THRESHOLD, is_visually_blank_page, maybe_apply_full_page_detection,
    should_force_blank_page_threshold,
};
use crate::pipeline::runtime_limits::PipelineRuntimeLimits;
use crate::pipeline::source::{PageSource, PdfPageSource, source_stage};
use crate::progress::ProgressTracker;
use crate::{info_log, warn_log};
use anyhow::{Result, anyhow};
use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use image::RgbImage;
use lege_pdf_read::RenderSession;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
    /// Preserve this page as one full-color IW44 background, without MRC/JB2
    /// body-page processing.
    pub preserve_full_color: bool,
    pub adjusted_image: Arc<RgbImage>,
    pub binarized: Vec<u8>,
    pub cleaned_gray: Option<Vec<u8>>,
    pub detections: Vec<Detection>,
    pub original_width_pts: f32,
    pub original_height_pts: f32,
    pub hocr_text: Option<String>,
    pub toc: crate::toc::PageTocData,
}

struct ComposedDjvuPage {
    encoded: djvu_encoder::doc::EncodedPage,
    page_index: usize,
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
    let config = config;
    #[cfg(feature = "debug-logging")]
    {
        info_log!("[DJVU-Parallel] Entering parallel tokio pipeline");
        info_log!(
            "[DJVU-Parallel] Native encoder backend: {}",
            crate::djvu::active_backend_info()
        );
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

    // Dispatch to the reflow variant when enabled
    if config.enable_reflow() {
        return crate::pipeline::reflow_pipeline::run_raster_reflow_djvu_pipeline(
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
    let document_session = source.document_session();
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
    let (config, shared_inference_handle) =
        crate::pipeline::helper_functions::initialize_inference_or_fallback(
            config,
            progress_tracker,
            "DJVU-Parallel",
        )?;

    // Use the same adaptive worker limit for the margin pre-pass and the main
    // page pipeline. Inference itself is additionally bounded by the shared
    // GPU session pool.
    let pipeline_config = PipelineRuntimeLimits::djvu_from_config(&config);
    let cancellation = lege_pdf_read::CancellationToken::new();
    let needs_margin_pass = matches!(
        config.margin_settings(),
        crate::margin::MarginSettings::StandardizeAndCenter
            | crate::margin::MarginSettings::CropAndResize
    );
    let (margin_analysis, detection_cache) = if needs_margin_pass {
        let analysis_future = perform_document_margin_analysis(
            source.clone(),
            config.clone(),
            shared_inference_handle.clone(),
            total_pages,
            page_index_offset..page_index_offset + total_pages,
            pipeline_config.page_workers,
            progress_tracker,
            cancellation.clone(),
        );
        let (analysis, cache) = tokio::select! {
            result = analysis_future => result?,
            signal = shutdown_rx.recv() => {
                cancellation.cancel();
                let message = signal
                    .ok()
                    .and_then(|signal| signal.message)
                    .unwrap_or_else(|| "User requested cancellation".to_string());
                return Err(anyhow!("Processing cancelled during margin analysis: {message}"));
            }
        };
        (Some(Arc::new(analysis)), Arc::new(cache))
    } else {
        (
            None,
            Arc::new(vec![CachedDetections::Missing; source.page_count()]),
        )
    };

    // Pipeline concurrency settings (similar to PDF pipeline)
    // NOTE: no init_encode_semaphore here. The global encode semaphore is only acquired by
    // encode_region_image/encode_page_data, which the DjVu path never calls — its heavy
    // IW44/JB2 encode runs inside the in_flight-bounded encode stage (capped at
    // djvu_encode_workers), so the semaphore would be redundant.
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
    // Spawn source task. PDF and image-folder inputs advertise their own
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
            page_start..page_end,
            cancellation.clone(),
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
        let detection_cache = detection_cache.clone();
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
                                    tracker.publish_layout_progress(rendered_val, detected_val, encoded_val, total_pages);
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
                        in_flight.push(Box::pin(crate::runtime_stats::track_future(
                            crate::runtime_stats::Stage::Inference,
                            build_djvu_inference_future(
                                handle_clone.clone(),
                                rendered,
                                detection_cache.clone(),
                            ),
                        )));
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
        let document_session = document_session.clone();
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
        let margin_analysis = margin_analysis.clone();
        let binarize_cancellation = cancellation.clone();
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
                                    tracker.publish_layout_progress(rendered_val, detected_val, encoded_val, total_pages);
                                } else {
                                    tracker.publish_no_layout_progress(encoded_val, total_pages);
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
                        let document_session_clone = document_session.clone();
                        let margin_analysis = margin_analysis.clone();
                        let page_cancellation = binarize_cancellation.clone();
                        // Spawn processing task
                        let task = tokio::spawn(async move {
                            process_single_djvu_page(
                                config_clone,
                                document_session_clone,
                                inference_data,
                                page_index_offset,
                                margin_analysis,
                                page_cancellation,
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
    // Background subsample: binarized mode keeps c44's ×3 default. Grayscale
    // mode is resolution-aware — the IW44 background carries the ~1px
    // antialiasing ring around the mask, which ×3 erases at low renders.
    let bg_subsample = if config.is_grayscale_mode() {
        config.mrc_bg_subsample_override().unwrap_or_else(|| {
            let auto = match config.target_height() {
                0..=1799 => 1,
                1800..=2399 => 2,
                _ => 3,
            };
            // Adaptive-mask mode: synthetic rings only — ×2 floor smooths
            // them into gradients (see pdf_tokio_pipeline).
            if config.mrc_adaptive_mask() {
                auto.max(2)
            } else {
                auto
            }
        })
    } else {
        3
    };
    let (djvu_writer, djvu_document, mut writer_task) = spawn_djvu_writer_actor(
        output_path.to_path_buf(),
        total_pages,
        dpi,
        iw44_quality,
        bg_subsample,
        progress_tracker.clone(),
        pipeline_config.channel_capacity,
        cancellation.clone(),
    );

    let epub_sidecar_output = config.epub_sidecar_output().cloned();
    let epub_hocr_pages = Arc::new(Mutex::new(Vec::new()));
    let document_toc = Arc::new(Mutex::new(Vec::new()));

    // Spawn encoding stage (encodes pages concurrently, then forwards to writer actor)
    let mut encode_task: JoinHandle<Result<()>> = {
        let orchestrator = orchestrator.clone();
        let djvu_document = djvu_document.clone();
        let djvu_writer = djvu_writer.clone();
        let mut binarize_rx = binarize_rx;
        let concurrency = pipeline_config.djvu_encode_workers;
        let epub_sidecar_output = epub_sidecar_output.clone();
        let epub_hocr_pages = epub_hocr_pages.clone();
        let document_toc = document_toc.clone();
        let encode_cancellation = cancellation.clone();
        tokio::spawn(async move {
            #[cfg(feature = "debug-logging")]
            info_log!(
                "[DJVU-Parallel-Compose] Starting compose stage with concurrency={}",
                concurrency
            );
            let mut in_flight: FuturesUnordered<BoxFuture<'static, Result<ComposedDjvuPage>>> =
                FuturesUnordered::new();
            let mut input_exhausted = false;

            loop {
                tokio::select! {
                    biased;
                    Some(result) = in_flight.next(), if !in_flight.is_empty() => {
                        let composed_page = result?;
                        djvu_writer
                            .append_encoded(composed_page.encoded, composed_page.page_index)
                            .await?;
                    }
                    Some(binarized_data) = binarize_rx.recv(), if in_flight.len() < concurrency && !input_exhausted => {
                        if let Ok(mut pages) = document_toc.lock() {
                            pages.push(binarized_data.toc.clone());
                        }
                        if epub_sidecar_output.is_some()
                            && let Some(hocr) = binarized_data.hocr_text.clone()
                            && !hocr.trim().is_empty()
                        {
                            let width_px = binarized_data.adjusted_image.width();
                            let height_px = binarized_data.adjusted_image.height();
                            if let Ok(mut pages) = epub_hocr_pages.lock() {
                                pages.push(crate::pipeline::epub_pipeline::HocrPage {
                                    page_index: binarized_data.index,
                                    width_px,
                                    height_px,
                                    hocr,
                                });
                            }
                        }
                        let orchestrator = orchestrator.clone();
                        let djvu_document = djvu_document.clone();
                        let page_cancellation = encode_cancellation.clone();
                        in_flight.push(Box::pin(async move {
                            if page_cancellation.is_cancelled() {
                                return Err(anyhow!("DjVu composition cancelled before page encode"));
                            }
                            let page_data = PageData {
                                index: binarized_data.index,
                                preserve_full_color: binarized_data.preserve_full_color,
                                // The Arc is not shared past this point, so unwrap_or_clone
                                // moves the full-page RgbImage out instead of deep-copying it
                                // (falls back to a clone only if a reference somehow remains).
                                rgb_image: Arc::unwrap_or_clone(binarized_data.adjusted_image),
                                binarized: binarized_data.binarized,
                                cleaned_gray: binarized_data.cleaned_gray,
                                detections: binarized_data.detections,
                                hocr: binarized_data.hocr_text,
                            };

                            // Compose and compress the typed page inside one blocking
                            // worker so IW44/JB2 encoding remains page-parallel.
                            let page_index = page_data.index;
                            let encoded = crate::runtime_stats::spawn_blocking_stage(
                                crate::runtime_stats::Stage::Encode,
                                move || -> Result<_> {
                                    let prepared = orchestrator.process_page(page_data)?;
                                    prepared.encode(&djvu_document, iw44_quality, bg_subsample)
                                },
                            )
                            .await
                            .map_err(|e| anyhow!("DjVu compose task panicked: {}", e))??;
                            if page_cancellation.is_cancelled() {
                                return Err(anyhow!("DjVu composition cancelled after page encode"));
                            }

                            Ok(ComposedDjvuPage {
                                encoded,
                                page_index,
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

            #[cfg(feature = "debug-logging")]
            info_log!("[DJVU-Parallel-Encode] Encoding stage complete");
            Ok(())
        })
    };
    // Wait for all stages to complete with cancellation support
    #[cfg(feature = "debug-logging")]
    info_log!("[DJVU-Parallel] Waiting for pipeline stages to complete...");

    use crate::pipeline::helper_functions::await_stage_or_cancel_with_token;
    let h_infer = infer_task.abort_handle();
    let h_binarize = binarize_task.abort_handle();
    let h_encode = encode_task.abort_handle();

    await_stage_or_cancel_with_token(
        &mut render_task,
        &mut shutdown_rx,
        "render",
        &[h_infer.clone(), h_binarize.clone(), h_encode.clone()],
        Some(&cancellation),
    )
    .await?;
    #[cfg(feature = "debug-logging")]
    info_log!("[DJVU-Parallel] Render stage complete");

    await_stage_or_cancel_with_token(
        &mut infer_task,
        &mut shutdown_rx,
        "inference",
        &[h_binarize.clone(), h_encode.clone()],
        Some(&cancellation),
    )
    .await?;
    #[cfg(feature = "debug-logging")]
    info_log!("[DJVU-Parallel] Inference stage complete");

    await_stage_or_cancel_with_token(
        &mut binarize_task,
        &mut shutdown_rx,
        "binarization",
        &[h_encode.clone()],
        Some(&cancellation),
    )
    .await?;
    #[cfg(feature = "debug-logging")]
    info_log!("[DJVU-Parallel] Binarization stage complete");

    await_stage_or_cancel_with_token(
        &mut encode_task,
        &mut shutdown_rx,
        "encoding",
        &[],
        Some(&cancellation),
    )
    .await?;
    #[cfg(feature = "debug-logging")]
    info_log!("[DJVU-Parallel] Encoding stage complete");

    let toc_pages = document_toc
        .lock()
        .map(|pages| pages.clone())
        .unwrap_or_default();
    let candidates = toc_pages
        .iter()
        .flat_map(|page| page.candidates.clone())
        .collect();
    let stats = toc_pages
        .iter()
        .filter_map(|page| page.stats)
        .collect::<Vec<_>>();
    let printed_contents = toc_pages
        .iter()
        .flat_map(|page| page.printed_contents.clone())
        .collect::<Vec<_>>();
    let synthetic =
        crate::toc::build_outline_with_contents(candidates, &stats, total_pages, &printed_contents);
    let source_outline = if let Some(session) = document_session.clone() {
        let bookmarks =
            crate::runtime_stats::spawn_blocking(move || lege_pdf_read::extract_outline(&session))
                .await?;
        let source_to_output = (0..total_pages)
            .map(|local| (page_index_offset + local, local))
            .collect::<std::collections::HashMap<_, _>>();
        bookmarks_to_outline(&bookmarks, &source_to_output)
    } else {
        Vec::new()
    };
    let accepted_outline = merge_outline(source_outline, Some(synthetic));
    if !accepted_outline.is_empty() {
        djvu_writer.send_outline(accepted_outline.clone()).await?;
    }
    djvu_writer.finalize().await?;

    if let Some(epub_path) = epub_sidecar_output {
        let hocr_pages = epub_hocr_pages
            .lock()
            .map(|pages| pages.clone())
            .unwrap_or_default();
        if !hocr_pages.is_empty() {
            let title = epub_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Document")
                .to_string();
            info_log!(
                "[DJVU-Parallel] Assembling EPUB sidecar from existing OCR: {}",
                epub_path.display()
            );
            let epub_cancellation = cancellation.clone();
            crate::runtime_stats::spawn_blocking_stage(
                crate::runtime_stats::Stage::Writer,
                move || {
                    crate::pipeline::epub_pipeline::build_epub_from_hocr_pages_with_outline_cancellable(
                        &hocr_pages,
                        &title,
                        &epub_path,
                        &accepted_outline,
                        Some(&epub_cancellation),
                    )
                },
            )
            .await
            .map_err(|e| anyhow!("EPUB sidecar task panicked: {}", e))??;
        } else {
            warn_log!("[DJVU-Parallel] EPUB sidecar requested, but no OCR text was available");
        }
    }

    #[cfg(feature = "debug-logging")]
    info_log!("[DJVU-Parallel] Waiting for writer actor to complete document assembly...");
    await_stage_or_cancel_with_token(
        &mut writer_task,
        &mut shutdown_rx,
        "document assembly",
        &[],
        Some(&cancellation),
    )
    .await?;
    #[cfg(feature = "debug-logging")]
    info_log!(
        "[DJVU-Parallel] Document assembly complete: {}",
        output_path.display()
    );

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
    let source: Arc<dyn PageSource> = Arc::new(PdfPageSource::new(pdf_bytes, config.clone())?);
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
    detection_cache: Arc<Vec<CachedDetections>>,
) -> BoxFuture<'static, Result<DjvuInferenceData>> {
    if let Some(inference_result) = cached_inference_result(&rendered, detection_cache.as_slice()) {
        return Box::pin(async move {
            Ok(DjvuInferenceData {
                rendered,
                inference_result,
            })
        });
    }

    if !rendered.layout_detection_enabled {
        return Box::pin(async move {
            let inference_result = InferenceResult {
                index: rendered.index,
                high_res_image: rendered.high_res_image.clone(),
                inference_image: rendered.inference_image.clone(),
                detections: Vec::new(),
                text_layer: None,
                detections_are_page_space: true,
                original_width_pts: rendered.original_width_pts,
                original_height_pts: rendered.original_height_pts,
                has_no_detections: true,
            };
            Ok(DjvuInferenceData {
                rendered,
                inference_result,
            })
        });
    }

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
                detections_are_page_space: false,
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
                detections_are_page_space: true,
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
    document_session: Option<Arc<RenderSession>>,
    inference_data: DjvuInferenceData,
    page_index_offset: usize,
    margin_analysis: Option<Arc<DocumentMarginAnalysis>>,
    cancellation: lege_pdf_read::CancellationToken,
) -> Result<Option<DjvuBinarizedData>> {
    if cancellation.is_cancelled() {
        return Err(anyhow!("DjVu page processing cancelled before CPU work"));
    }
    let page_index = inference_data.rendered.index;
    let local_index = page_index.saturating_sub(page_index_offset);
    let preserve_full_color = should_preserve_cover_page(page_index, &config);
    // CPU-heavy work in spawn_blocking (binarization and image processing only)
    let config_clone = config.clone();
    let input = DjvuPageProcessingInput {
        rendered: inference_data.rendered,
        inference_result: inference_data.inference_result,
        config: config_clone,
        margin_analysis,
    };
    let cpu_result = crate::runtime_stats::spawn_blocking_stage(
        crate::runtime_stats::Stage::Processing,
        move || process_djvu_cpu_intensive_work(input),
    )
    .await
    .map_err(|e| anyhow!("CPU task panicked: {}", e))??;
    if cancellation.is_cancelled() {
        return Err(anyhow!("DjVu page processing cancelled after CPU work"));
    }
    let DjvuPageProcessingOutput {
        adjusted_image,
        adjusted_detections,
        ocr_image,
        binarized,
        cleaned_gray,
        width,
        height,
        original_width_pts,
        original_height_pts,
    } = cpu_result;
    // OCR and text extraction in async part (not CPU-intensive, involves I/O and API calls)
    let hocr_text = extract_djvu_text_layer(
        &config,
        document_session.as_ref(),
        &adjusted_image,
        ocr_image.as_ref(),
        &binarized,
        cleaned_gray.as_deref(),
        width,
        height,
        &adjusted_detections,
        page_index,
    )
    .await?;
    if cancellation.is_cancelled() {
        return Err(anyhow!("DjVu page processing cancelled after OCR"));
    }
    let toc = if config.enable_auto_toc() {
        crate::toc::capture_page(
            &adjusted_detections,
            hocr_text.as_deref(),
            local_index,
            width as u32,
            height as u32,
        )
    } else {
        crate::toc::PageTocData::default()
    };
    Ok(Some(DjvuBinarizedData {
        index: local_index,
        preserve_full_color,
        adjusted_image: Arc::new(adjusted_image),
        binarized,
        cleaned_gray,
        detections: adjusted_detections,
        original_width_pts,
        original_height_pts,
        hocr_text,
        toc,
    }))
}
/// CPU-intensive work for a single DJVU page (to be executed in spawn_blocking)
struct DjvuPageProcessingInput {
    rendered: RenderedPageData,
    inference_result: InferenceResult,
    config: Arc<PipelineConfig>,
    margin_analysis: Option<Arc<DocumentMarginAnalysis>>,
}
struct DjvuPageProcessingOutput {
    adjusted_image: RgbImage,
    adjusted_detections: Vec<crate::engine::Detection>,
    /// High-resolution adjusted raster retained for slow OCR ("render high,
    /// resize low"); `Some` only when rendered above `target_height`.
    ocr_image: Option<RgbImage>,
    binarized: Vec<u8>,
    /// Cleaned grayscale full page, `Some` only in grayscale/MRC mode.
    cleaned_gray: Option<Vec<u8>>,
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
        margin_analysis,
    } = input;
    let preserve_full_color = should_preserve_cover_page(rendered.index, &config);
    // 1. Apply region policy (CPU-heavy: image resizing/cropping)
    let (mut adjusted_image, mut adjusted_detections, free_aspect_crop) = if preserve_full_color {
        // Preserve the complete source frame. The cover still follows requested
        // output scaling below, but never body-page crop/center/region policies.
        ((*rendered.high_res_image).clone(), Vec::new(), false)
    } else if let Some(analysis) = margin_analysis.as_deref() {
        let adjusted = adjust_page_with_margin_analysis(
            &rendered,
            inference_result.detections.clone(),
            inference_result.detections_are_page_space,
            &config,
            analysis,
            rendered.index,
        )?;
        (
            adjusted.image,
            adjusted.detections,
            adjusted.free_aspect_crop,
        )
    } else {
        let (image, detections) = apply_djvu_region_policy(&rendered, &inference_result, &config)?;
        (image, detections, false)
    };
    // 2. Resize to target height if needed (CPU-heavy: Lanczos3 filtering).
    // Always normalize page dimensions regardless of layout mode — PDF source already
    // renders at target_height so this is a no-op there, but folder source supplies
    // images at native resolution and viewers reject DjVus built at those sizes.
    //
    // "Render high, resize low": when slow OCR rendered the page above
    // target_height, retain the high-res raster for recognition (moved out, not
    // cloned) before downscaling the encode-path image.
    let mut ocr_image: Option<RgbImage> = None;
    let effective_target_height = if free_aspect_crop {
        adjusted_image.height()
    } else {
        config.target_height()
    };
    if adjusted_image.height() != effective_target_height {
        let current_w = adjusted_image.width();
        let current_h = adjusted_image.height();
        let target_h = effective_target_height;
        let aspect_ratio = current_w as f32 / current_h as f32;
        let target_w = config
            .target_width()
            .unwrap_or_else(|| (target_h as f32 * aspect_ratio).round() as u32);
        if target_w > 0 && target_h > 0 {
            // Scale detection bboxes into target (output) space
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
            let resized = match resized {
                Some(buf) => buf,
                None => image::imageops::resize(
                    &adjusted_image,
                    target_w,
                    target_h,
                    image::imageops::FilterType::Lanczos3,
                ),
            };
            if config.slow_ocr_enabled() && current_h > target_h {
                ocr_image = Some(std::mem::replace(&mut adjusted_image, resized));
            } else {
                adjusted_image = resized;
            }
            adjusted_detections = scaled_detections;
        }
    }
    let width = adjusted_image.width() as usize;
    let height = adjusted_image.height() as usize;
    let classifier = &crate::types::LABEL_CLASSIFIER;

    if config.enable_layout_detection() {
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
    }

    // Drop false image boxes over substantive text, and line-art boxes that
    // should stay in the bilevel / MRC mask path (mirrors the PDF pipeline).
    if config.text_format() != "jpeg" {
        let all_detections = adjusted_detections.clone();
        adjusted_detections.retain(|det| {
            if !classifier.is_image_label(det) {
                return true;
            }
            should_keep_image_overlay(
                det,
                adjusted_image.as_raw(),
                width,
                height,
                &all_detections,
                classifier,
            )
        });
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
    // In grayscale/MRC mode, clean the page and derive an ink-core bilevel buffer
    // (the JB2 mask); keep the cleaned gray for the IW44 background.
    let mut cleaned_gray: Option<Vec<u8>> = None;
    let mut binarized = if config.is_grayscale_mode() && config.text_format() != "jpeg" {
        let opts =
            crate::clean_gray::CleanOptions::production_for_height(height, config.invert_input());
        // Adaptive-mask mode: Sauvola mask over the cleaned-gray background
        // with a mask-keyed antialiasing collar (see pdf_tokio_pipeline and
        // clean_page_for_mrc_with_mask; keeps faint staff lines contiguous
        // in the JB2 layer without emptying the IW44 background).
        let clean_result = if config.mrc_adaptive_mask() {
            let mask = binarize_djvu_image(&binarization_image, &config, false);
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
                mask
            }
            Err(e) => {
                log::warn!("clean-gray failed on DjVu page: {e}; binarizing");
                binarize_djvu_image(&binarization_image, &config, false)
            }
        }
    } else if config.text_format() == "jpeg" {
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
                crate::color::color_processing::process_image_region(
                    adjusted_image.as_raw(),
                    width as u32,
                    height as u32,
                    det.bbox,
                    config.image_region_dither_mode(),
                    "djvu",
                    false,
                )?;

            let grayscale_data: Vec<u8> = region_data.chunks(3).map(|rgb| rgb[0]).collect();
            crate::color::color_processing::merge_dithered_region(
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
        ocr_image,
        binarized,
        cleaned_gray,
        width,
        height,
        original_width_pts: inference_result.original_width_pts,
        original_height_pts: inference_result.original_height_pts,
    })
}
/// Extract text layer via OCR or PDF text extraction (runs in async context)
#[allow(clippy::too_many_arguments)]
async fn extract_djvu_text_layer(
    config: &PipelineConfig,
    document_session: Option<&Arc<RenderSession>>,
    adjusted_image: &RgbImage,
    ocr_image: Option<&RgbImage>,
    binarized: &[u8],
    cleaned_gray: Option<&[u8]>,
    width: usize,
    height: usize,
    detections: &[crate::engine::Detection],
    page_index: usize,
) -> Result<Option<String>> {
    if config.enable_ocr() && config.slow_ocr_enabled() {
        // Recognize on the high-res raster when available; detections and the
        // returned hOCR are in output (page) space.
        let (ocr_src, ocr_binary): (&RgbImage, &[u8]) = match ocr_image {
            Some(hi) => (hi, &[]),
            None => (adjusted_image, binarized),
        };
        match crate::ocr::slow::perform_slow_ocr(
            ocr_src,
            ocr_binary,
            detections,
            width as u32,
            height as u32,
            config,
            page_index,
        )
        .await
        {
            Ok(text) => Ok(text),
            Err(e) => Err(anyhow!("Page {}: PaddleOCR failed: {e:#}", page_index)),
        }
    } else if config.enable_ocr() {
        // PP-OCR (paddle) backend runs on the page raster (DBNet needs grayscale,
        // not the 1bpp mask) and does its own line detection.
        #[cfg(lege_paddle_ocr)]
        let result = {
            let _ = (binarized, detections);
            let page_rgb = ocr_image.unwrap_or(adjusted_image);
            // A separately rendered OCR image can be higher resolution than the
            // output page. Reuse the cleaner result only when the rasters match.
            let reusable_cleaned = cleaned_gray.filter(|gray| {
                (page_rgb.width() as usize).checked_mul(page_rgb.height() as usize)
                    == Some(gray.len())
            });
            crate::ocr::fast::perform_page_rgb_ocr(
                page_rgb,
                reusable_cleaned,
                config.ocr_language(),
                config.invert_input(),
            )
            .await
        };

        #[cfg(not(lege_paddle_ocr))]
        let result = {
            let _ = cleaned_gray;
            let use_regions = crate::ocr::fast::should_use_region_ocr(
                config.enable_layout_detection(),
                detections,
            );
            #[cfg(feature = "debug-logging")]
            info_log!(
                "[extract_djvu_text_layer] Page {}: OCR enabled, use_regions={}, detections={}",
                page_index,
                use_regions,
                detections.len()
            );
            if use_regions {
                crate::ocr::fast::perform_region_based_ocr(
                    binarized,
                    width,
                    height,
                    detections,
                    config.ocr_language(),
                )
                .await
            } else {
                crate::ocr::fast::perform_tiling_based_ocr(
                    binarized,
                    width,
                    height,
                    config.ocr_language(),
                )
                .await
            }
        };
        match result {
            Ok(text) => {
                #[cfg(feature = "debug-logging")]
                info_log!(
                    "[extract_djvu_text_layer] Page {}: OCR returned {} chars",
                    page_index,
                    text.len()
                );
                Ok(Some(text))
            }
            Err(e) => Err(anyhow!("Page {}: OCR failed: {e:#}", page_index)),
        }
    } else {
        let Some(session) = document_session else {
            return Ok(None);
        };
        let session = Arc::clone(session);
        let native_text = crate::runtime_stats::spawn_blocking_stage(
            crate::runtime_stats::Stage::Ocr,
            move || lege_pdf_read::page_text(&session, page_index as u32),
        )
        .await
        .map_err(|e| anyhow!("Renderer text task panicked: {e}"))?;

        match native_text {
            Ok(raw_text) if !raw_text.trim().is_empty() => {
                let hocr = build_hocr_from_pdf_text(&raw_text, width as u32, height as u32);
                #[cfg(feature = "debug-logging")]
                info_log!(
                    "[extract_djvu_text_layer] Page {}: renderer text extracted, HOCR {} chars",
                    page_index,
                    hocr.len()
                );
                Ok(Some(hocr))
            }
            Ok(_) => Ok(None),
            Err(e) => {
                warn_log!(
                    "Renderer text extraction failed on page {}: {}",
                    page_index,
                    e
                );
                Ok(None)
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
pub(crate) fn binarize_djvu_image(
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
    crate::color::binarization::binarize_image_raw(
        image.as_raw(),
        image.width() as usize,
        image.height() as usize,
        &options,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use image::{Rgb, RgbImage};

    use super::*;
    use crate::margin::{ContentBounds, MarginSettings, PageMarginData};

    #[test]
    fn djvu_crop_uses_shared_free_aspect_adjustment() {
        let source = Arc::new(RgbImage::from_pixel(640, 900, Rgb([255, 255, 255])));
        let rendered = RenderedPageData {
            index: 1,
            high_res_image: source.clone(),
            inference_image: source.clone(),
            layout_detection_enabled: false,
            original_width_pts: 640.0,
            original_height_pts: 900.0,
        };
        let inference_result = InferenceResult {
            index: 1,
            high_res_image: source.clone(),
            inference_image: source,
            detections: Vec::new(),
            text_layer: None,
            detections_are_page_space: true,
            original_width_pts: 640.0,
            original_height_pts: 900.0,
            has_no_detections: true,
        };
        let bounds = ContentBounds {
            min_x: 120,
            min_y: 160,
            max_x: 500,
            max_y: 660,
        };
        let mut pages = HashMap::new();
        pages.insert(
            1,
            PageMarginData {
                page_index: 1,
                page_width: 640,
                page_height: 900,
                content_bounds: Some(bounds),
                is_blank: false,
                is_full_page_image: false,
                margin_left: bounds.min_x,
                margin_right: 640 - bounds.max_x,
                margin_top: bounds.min_y,
                margin_bottom: 900 - bounds.max_y,
            },
        );
        let analysis = DocumentMarginAnalysis {
            pages,
            baseline_bounds: ContentBounds {
                min_x: 100,
                min_y: 100,
                max_x: 500,
                max_y: 700,
            },
            crop_bounds: ContentBounds {
                min_x: 0,
                min_y: 0,
                max_x: 400,
                max_y: 600,
            },
            standard_aspect_ratio: 2.0 / 3.0,
            effective_margin_setting: MarginSettings::CropAndResize,
            setting_override_reason: None,
            analysis_width: 640,
            analysis_height: 900,
        };
        let mut config = PipelineConfig::default();
        config.set_margin_settings(MarginSettings::CropAndResize);
        config.set_crop_free_aspect(true);
        config.set_target_height(900).expect("valid target height");

        let output = process_djvu_cpu_intensive_work(DjvuPageProcessingInput {
            rendered,
            inference_result,
            config: Arc::new(config),
            margin_analysis: Some(Arc::new(analysis)),
        })
        .expect("DjVu crop processing");

        assert!(
            output.adjusted_image.height() < 900,
            "free-aspect crop was stretched back to the configured height"
        );
        assert_eq!(output.adjusted_image.height(), output.height as u32);
    }
}
