// reflow_pipeline.rs
//
// Raster reflow: whole-document batch pipeline.
//
// This is split out from `pdf_tokio_pipeline.rs` because reflow is a
// fundamentally different execution shape (whole-document batch, single fixed
// output canvas) from the streaming per-page pipeline. Keeping it separate
// makes it possible to tell, by inspection, which logic belongs to the
// streaming path vs. the reflow path — useful while tracking down why layout
// detection behaves differently in reflow mode.

use crate::pipeline::config::PipelineConfig;
use crate::pipeline::deskew_graph::prepare_shared_deskew_engine;
use crate::pipeline::helper_functions::{
    await_stage_or_cancel, init_encode_semaphore, spawn_pdf_writer_actor,
};
use crate::pipeline::pdf_tokio_pipeline::encode_base_layer_for_jpeg_mode;
use crate::pipeline::policies::build_inference_image;
use crate::pipeline::runtime_limits::PipelineRuntimeLimits;
use crate::pipeline::source::PageSource;
use crate::progress::ProgressTracker;
use crate::progress::ReflowStage;
use crate::{info_log, success_log, warn_log};

use anyhow::{Result, anyhow};
use image::RgbImage;
use std::path::Path;
use std::sync::Arc;

//==============================================================================
// Raster reflow: whole-document batch pipeline
//==============================================================================

/// Build a [`crate::reflow::RasterReflowConfig`] sized to the configured output
/// device. Reflow re-paginates the whole document onto one fixed canvas (unlike
/// the streaming pipeline's per-page proportional sizing), so it needs a single
/// concrete `(width, height)` up front: a missing `target_width` falls back to
/// the reference page's aspect ratio. `margin`/`target_text_height` are scaled
/// from the tuned defaults — which assume a ~1072px-tall canvas — so they stay
/// proportionate at other output resolutions.
fn build_raster_reflow_config(
    config: &PipelineConfig,
    reference_page: &crate::reflow::SourcePageImage,
) -> crate::reflow::RasterReflowConfig {
    let defaults = crate::reflow::RasterReflowConfig::default();
    let page_height = config.target_height().max(1);
    let page_width = config.target_width().unwrap_or_else(|| {
        let (sw, sh) = reference_page.dimensions();
        if sh > 0 {
            ((page_height as f32) * (sw as f32 / sh as f32))
                .round()
                .max(1.0) as u32
        } else {
            page_height
        }
    });
    let scale = page_height as f32 / defaults.page_height as f32;
    // Optional tuning override: `LEGE_REFLOW_MAGNIFICATION` sets how much larger
    // body text appears relative to the source (1.0 ≈ original relative size;
    // higher = more dramatic, more pages). Lets the drama be tuned without a
    // rebuild. `0` disables adaptive sizing and uses the fixed target instead.
    let text_magnification = std::env::var("LEGE_REFLOW_MAGNIFICATION")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|m| m.is_finite() && *m >= 0.0)
        .unwrap_or(defaults.text_magnification);
    // Tuning overrides for word segmentation (find the word-gap sweet spot
    // without a rebuild).
    let word_gap_factor = std::env::var("LEGE_REFLOW_WORDGAP")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|m| m.is_finite() && *m > 0.0)
        .unwrap_or(defaults.word_gap_factor);
    let min_word_gap = std::env::var("LEGE_REFLOW_MINWORDGAP")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(defaults.min_word_gap);
    crate::reflow::RasterReflowConfig {
        page_width,
        page_height,
        text_magnification,
        word_gap_factor,
        min_word_gap,
        margin: ((defaults.margin as f32) * scale).round().max(1.0) as u32,
        target_text_height: ((defaults.target_text_height as f32) * scale)
            .round()
            .max(1.0) as u32,
        min_text_height: ((defaults.min_text_height as f32) * scale).round().max(1.0) as u32,
        ..defaults
    }
}

/// Composite one [`crate::reflow::ReflowPage`]'s placements into an RGB raster
/// by cropping each [`crate::reflow::PlacedItem::src`] rectangle out of the
/// original rendered source page and resizing it onto a white canvas.
fn compose_reflow_page_raster(
    page: &crate::reflow::ReflowPage,
    sources: &[crate::reflow::SourcePageImage],
    reflow_cfg: &crate::reflow::RasterReflowConfig,
) -> RgbImage {
    let mut canvas = RgbImage::from_pixel(
        page.width.max(1),
        page.height.max(1),
        image::Rgb([255, 255, 255]),
    );
    for item in &page.items {
        if item.out_rect.is_empty() || item.src.rect.is_empty() {
            continue;
        }
        let Some(source) = sources.get(item.src.page_index) else {
            continue;
        };
        let r = item.src.rect;
        if r.right() > source.gray.width() || r.bottom() > source.gray.height() {
            continue;
        }
        let placed = match item.kind {
            crate::reflow::PlacedKind::Word | crate::reflow::PlacedKind::Run => {
                binarized_reflow_text_crop(source, r, item.out_rect.w, item.out_rect.h, reflow_cfg)
            }
            crate::reflow::PlacedKind::Figure | crate::reflow::PlacedKind::Table => {
                let Some(src_rgb) = source.rgb.as_ref() else {
                    continue;
                };
                if r.right() > src_rgb.width() || r.bottom() > src_rgb.height() {
                    continue;
                }
                let crop = image::imageops::crop_imm(src_rgb, r.x, r.y, r.w, r.h).to_image();
                if (crop.width(), crop.height()) == (item.out_rect.w, item.out_rect.h) {
                    crop
                } else {
                    image::imageops::resize(
                        &crop,
                        item.out_rect.w.max(1),
                        item.out_rect.h.max(1),
                        image::imageops::FilterType::Triangle,
                    )
                }
            }
        };
        image::imageops::overlay(
            &mut canvas,
            &placed,
            item.out_rect.x as i64,
            item.out_rect.y as i64,
        );
    }
    canvas
}

fn binarized_reflow_text_crop(
    source: &crate::reflow::SourcePageImage,
    rect: crate::reflow::PxRect,
    out_w: u32,
    out_h: u32,
    reflow_cfg: &crate::reflow::RasterReflowConfig,
) -> RgbImage {
    let crop = image::imageops::crop_imm(&source.gray, rect.x, rect.y, rect.w, rect.h).to_image();
    let resized = if (crop.width(), crop.height()) == (out_w, out_h) {
        crop
    } else {
        image::imageops::resize(
            &crop,
            out_w.max(1),
            out_h.max(1),
            image::imageops::FilterType::Triangle,
        )
    };
    let threshold = if reflow_cfg.adaptive_threshold {
        // Clamp to `ink_threshold` so faint / low-contrast crops are not flooded
        // with false ink by Otsu (see build_ink_mask).
        crate::reflow::analyze::otsu_threshold(
            &resized,
            crate::reflow::PxRect::new(0, 0, resized.width(), resized.height()),
        )
        .min(reflow_cfg.ink_threshold)
    } else {
        reflow_cfg.ink_threshold
    };

    let mut rgb = RgbImage::from_pixel(
        resized.width().max(1),
        resized.height().max(1),
        image::Rgb([255, 255, 255]),
    );
    for (x, y, px) in resized.enumerate_pixels() {
        if px[0] <= threshold {
            rgb.put_pixel(x, y, image::Rgb([0, 0, 0]));
        }
    }
    rgb
}

/// Whole-document batch path used when [`PipelineConfig::enable_reflow`] is set.
///
/// Reflow re-composes the *entire* document into new pages before any of it can
/// be written, so it cannot stream page-by-page like
/// [`crate::pipeline::pdf_tokio_pipeline::create_and_run_pdf_source_pipeline`].
/// This renders every source page and runs layout detection up front
/// (sequentially — `PdfiumPageSource` only supports `source_concurrency() == 1`
/// anyway), builds the reflow plan via [`crate::reflow::reflow_document`], then
/// rasterizes, encodes and writes each output page through the same RGB-capable
/// encoder / writer-actor path the streaming pipeline uses.
pub(crate) async fn run_raster_reflow_pipeline(
    source: Arc<dyn PageSource>,
    config: Arc<PipelineConfig>,
    output_path: &Path,
    page_range: Option<std::ops::Range<usize>>,
    progress_tracker: &ProgressTracker,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
) -> Result<()> {
    info_log!("[Reflow] Starting raster-reflow pipeline");
    crate::pipeline::reset_standard_dimensions();

    if !config.enable_layout_detection() {
        return Err(anyhow!(
            "Raster reflow requires layout detection; it cannot run with layout detection disabled"
        ));
    }

    let inference_handle = crate::pipeline::inference::InferenceHandle::new(&config)?;
    let deskew_engine = prepare_shared_deskew_engine(&config)?;

    let runtime_limits = PipelineRuntimeLimits::from_config(&config);
    init_encode_semaphore(runtime_limits.page_workers);

    let document_pages = source.page_count();
    let page_start = page_range.as_ref().map(|r| r.start).unwrap_or(0);
    let page_end = page_range.as_ref().map(|r| r.end).unwrap_or(document_pages);
    let total_source_pages = page_end.saturating_sub(page_start);
    if total_source_pages == 0 {
        return Err(anyhow!("Raster reflow: no pages to process"));
    }

    info_log!(
        "[Reflow] Rendering and detecting layout for {} source pages",
        total_source_pages
    );

    let spec = config.inference_resize_spec();
    let mut source_pages: Vec<crate::reflow::SourcePageImage> =
        Vec::with_capacity(total_source_pages);
    let mut hints_per_page: Vec<Vec<crate::engine::Detection>> =
        Vec::with_capacity(total_source_pages);

    for page_index in page_start..page_end {
        if let Ok(signal) = shutdown_rx.try_recv() {
            return Err(anyhow!(
                "Processing cancelled: {}",
                signal
                    .message
                    .unwrap_or_else(|| "User requested cancellation".to_string())
            ));
        }

        let crate::pipeline::source::SourcePage {
            mut image,
            original_width_pts,
            original_height_pts,
        } = source.load_page(page_index).await?;

        if let Some(engine) = &deskew_engine {
            let engine = Arc::clone(engine);
            let original = image.clone();
            image = match tokio::task::spawn_blocking(move || engine.process_image(&original)).await
            {
                Ok(Ok(corrected)) => corrected,
                Ok(Err(e)) => {
                    warn_log!("[Reflow] Page {}: deskew failed: {}", page_index, e);
                    image
                }
                Err(e) => {
                    warn_log!("[Reflow] Page {}: deskew task failed: {}", page_index, e);
                    image
                }
            };
        }

        crate::pipeline::set_standard_dimensions_once(image.width(), image.height());
        let render_dpi = (image.width() as f32 / original_width_pts.max(1.0)) * 72.0;
        let high_res = Arc::new(image);

        let inference_image = Arc::new(
            build_inference_image(&high_res, &spec).unwrap_or_else(|_| (*high_res).clone()),
        );
        let mut detections = inference_handle
            .detect(page_index, inference_image)
            .await
            .unwrap_or_else(|e| {
                warn_log!(
                    "[Reflow] Page {}: layout detection failed: {}",
                    page_index,
                    e
                );
                Vec::new()
            });
        let (page_w, page_h) = (high_res.width(), high_res.height());
        for det in detections.iter_mut() {
            if crate::pipeline::policies::is_in_inference_space(&det.bbox, &spec) {
                det.bbox = crate::pipeline::policies::map_bbox_infer_to_page(
                    det.bbox, page_w, page_h, &spec,
                );
            }
        }

        let local_index = page_index - page_start;
        let gray = image::imageops::grayscale(high_res.as_ref());
        source_pages.push(crate::reflow::SourcePageImage {
            page_index: local_index,
            gray,
            rgb: Some((*high_res).clone()),
            render_dpi,
            page_pts: (original_width_pts, original_height_pts),
        });
        hints_per_page.push(detections);

        let done = local_index + 1;
        progress_tracker.publish_reflow_progress(
            ReflowStage::SourceAnalysis,
            done,
            total_source_pages,
        );
    }

    let mut reflow_cfg = build_raster_reflow_config(&config, &source_pages[0]);

    // Adaptive target text height: instead of forcing body text to a fixed
    // absolute size (which balloons page count when the source text is small),
    // size it to a controlled magnification of its *source* relative height.
    // `text_magnification == 1.0` keeps body text at roughly its original
    // relative size; the tuned absolute `target_text_height` becomes the ceiling.
    if reflow_cfg.text_magnification > 0.0 {
        if let Some(body_src) = crate::reflow::estimate_document_body_height(
            &source_pages,
            &hints_per_page,
            &reflow_cfg,
            24,
        ) {
            let mut src_heights: Vec<u32> = source_pages.iter().map(|p| p.gray.height()).collect();
            src_heights.sort_unstable();
            let src_page_h = src_heights[src_heights.len() / 2].max(1);
            let raw =
                reflow_cfg.text_magnification * (reflow_cfg.page_height as f32) * (body_src as f32)
                    / (src_page_h as f32);
            let ceil = reflow_cfg
                .target_text_height
                .max(reflow_cfg.min_text_height);
            let target = (raw.round().max(1.0) as u32).clamp(reflow_cfg.min_text_height, ceil);
            info_log!(
                "[Reflow] Calibrated target text height: {}px (source body {}px in {}px page, m={:.2}, ceiling {}px)",
                target,
                body_src,
                src_page_h,
                reflow_cfg.text_magnification,
                ceil
            );
            reflow_cfg.target_text_height = target;
            // Scale every page by this one body height so glyph size is uniform
            // document-wide (per-page estimates drift and cause visible steps).
            reflow_cfg.calibrated_body_height = Some(body_src);
        } else {
            info_log!(
                "[Reflow] Body-height calibration found no prose rows; using fixed target {}px",
                reflow_cfg.target_text_height
            );
        }
    }

    reflow_cfg
        .validate()
        .map_err(|e| anyhow!("Raster reflow configuration is invalid: {}", e))?;

    info_log!(
        "[Reflow] Composing {} source pages onto a {}x{} canvas",
        source_pages.len(),
        reflow_cfg.page_width,
        reflow_cfg.page_height
    );
    progress_tracker.update(crate::progress::ProcessingStatus::ReflowProgress {
        stage: ReflowStage::Compose,
        current: source_pages.len(),
        total: source_pages.len(),
        eta: None,
    });
    let doc = crate::reflow::reflow_document(&source_pages, &hints_per_page, &reflow_cfg);
    let total_out_pages = doc.pages.len();
    if total_out_pages == 0 {
        return Err(anyhow!("Raster reflow produced no output pages"));
    }
    info_log!(
        "[Reflow] Reflowed {} source pages into {} output pages",
        source_pages.len(),
        total_out_pages
    );
    progress_tracker.publish_reflow_progress(ReflowStage::OutputPages, 0, total_out_pages);

    let (pdf_writer_handle, mut pdf_writer_task) = spawn_pdf_writer_actor(
        output_path.to_path_buf(),
        total_out_pages,
        progress_tracker.clone(),
        false,
        runtime_limits.channel_capacity,
    );

    for reflow_page in &doc.pages {
        if let Ok(signal) = shutdown_rx.try_recv() {
            pdf_writer_task.abort();
            return Err(anyhow!(
                "Processing cancelled: {}",
                signal
                    .message
                    .unwrap_or_else(|| "User requested cancellation".to_string())
            ));
        }

        let canvas = compose_reflow_page_raster(reflow_page, &source_pages, &reflow_cfg);
        let encoded =
            encode_base_layer_for_jpeg_mode(Arc::new(canvas), &config, reflow_page.index).await?;

        // When slow OCR is enabled, overlay a searchable text layer onto the
        // reflowed page by recognizing the reflow-detected text rows and
        // re-projecting the words onto their new output positions.
        let hocr_text = if config.enable_ocr() && config.slow_ocr_enabled() {
            match crate::ocr::slow::perform_reflow_page_ocr(reflow_page, &source_pages, &config)
                .await
            {
                Ok(hocr) => hocr,
                Err(e) => {
                    warn_log!(
                        "[Reflow] Page {}: OCR text layer failed: {}",
                        reflow_page.index,
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

        let page = crate::accumulator::Page {
            width: reflow_page.width as f32,
            height: reflow_page.height as f32,
            elements: vec![crate::accumulator::ContentElement {
                x: 0.0,
                y: 0.0,
                width: reflow_page.width as f32,
                height: reflow_page.height as f32,
                content: encoded,
            }],
            hocr_text,
            index: reflow_page.index,
            binarized: None,
        };
        pdf_writer_handle.send_page(page, reflow_page.index).await?;

        let done = reflow_page.index + 1;
        progress_tracker.publish_reflow_progress(ReflowStage::OutputPages, done, total_out_pages);
    }

    pdf_writer_handle.finalize().await?;
    await_stage_or_cancel(&mut pdf_writer_task, &mut shutdown_rx, "PDF writer", &[]).await?;

    success_log!("Raster reflow pipeline complete: {}", output_path.display());
    Ok(())
}
