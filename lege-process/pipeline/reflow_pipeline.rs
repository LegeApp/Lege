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
use crate::pipeline::helper_functions::{
    await_stage_or_cancel_with_token, bookmarks_to_outline, init_encode_semaphore, merge_outline,
    spawn_pdf_writer_actor,
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
    sources: &crate::reflow::SourcePageSet,
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

/// How many source pages the compose pass may hold at one time. An output page
/// normally draws from one or two source pages, so a small window keeps every
/// page it needs resident and still bounds memory to a constant.
const SOURCE_PAGE_WINDOW: usize = 4;

/// Number of pages the body-height calibration samples across the document.
const BODY_HEIGHT_SAMPLES: usize = 24;

/// One analyzed source page: the grayscale render plus the layout hints. This
/// is what the analysis passes keep; the RGB render is dropped at once because
/// only the compose pass reads color.
struct AnalyzedPage {
    page: crate::reflow::SourcePageImage,
    hints: Vec<crate::engine::Detection>,
}

/// The reflow plan: the calibrated config and the paginated document. It holds
/// rectangles only, never pixels, so it is small for any document size.
struct ReflowPlan {
    cfg: crate::reflow::RasterReflowConfig,
    doc: crate::reflow::ReflowDocument,
    total_source_pages: usize,
    /// Source-space title/content boxes retained only long enough to project
    /// them onto the repaginated output pages.
    toc_hints: Vec<(usize, crate::engine::Detection)>,
}

fn mapped_toc_detections(
    page: &crate::reflow::ReflowPage,
    hints: &[(usize, crate::engine::Detection)],
) -> Vec<crate::engine::Detection> {
    hints
        .iter()
        .filter_map(|(source_page, hint)| {
            let mut bounds: Option<[u32; 4]> = None;
            for item in &page.items {
                if item.src.page_index != *source_page {
                    continue;
                }
                let rect = item.src.rect;
                let cx = rect.x as f32 + rect.w as f32 * 0.5;
                let cy = rect.y as f32 + rect.h as f32 * 0.5;
                if cx < hint.bbox[0] || cx > hint.bbox[2] || cy < hint.bbox[1] || cy > hint.bbox[3]
                {
                    continue;
                }
                let out = item.out_rect;
                bounds = Some(match bounds {
                    None => [out.x, out.y, out.x + out.w, out.y + out.h],
                    Some(old) => [
                        old[0].min(out.x),
                        old[1].min(out.y),
                        old[2].max(out.x + out.w),
                        old[3].max(out.y + out.h),
                    ],
                });
            }
            bounds.map(|bbox| crate::engine::Detection {
                class_id: hint.class_id,
                class_name: hint.class_name.clone(),
                confidence: hint.confidence,
                bbox: bbox.map(|value| value as f32),
                category: hint.category.clone(),
                context: hint.context.clone(),
            })
        })
        .collect()
}

fn cancelled_if_signalled(
    shutdown_rx: &mut tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
    cancellation: &lege_pdf_read::CancellationToken,
) -> Result<()> {
    if let Ok(signal) = shutdown_rx.try_recv() {
        cancellation.cancel();
        return Err(anyhow!(
            "Processing cancelled: {}",
            signal
                .message
                .unwrap_or_else(|| "User requested cancellation".to_string())
        ));
    }
    Ok(())
}

async fn await_reflow_step<T>(
    future: impl std::future::Future<Output = Result<T>>,
    shutdown_rx: &mut tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
    cancellation: &lege_pdf_read::CancellationToken,
    stage: &str,
) -> Result<T> {
    tokio::select! {
        result = future => result,
        signal = shutdown_rx.recv() => {
            cancellation.cancel();
            let message = signal
                .ok()
                .and_then(|signal| signal.message)
                .unwrap_or_else(|| "User requested cancellation".to_string());
            Err(anyhow!("Processing cancelled during {stage}: {message}"))
        }
    }
}

/// Render one source page, run layout detection on it, and map the detections
/// into page space. `want_rgb` keeps the color render for the compose pass;
/// the analysis passes set it to `false` and keep the grayscale plane only.
async fn analyze_source_page(
    source: &Arc<dyn PageSource>,
    inference: Option<(
        &crate::pipeline::inference::InferenceHandle,
        &crate::pipeline::policies::InferenceResizeSpec,
    )>,
    page_index: usize,
    local_index: usize,
    want_rgb: bool,
    cancellation: lege_pdf_read::CancellationToken,
) -> Result<AnalyzedPage> {
    let crate::pipeline::source::SourcePage {
        image,
        original_width_pts,
        original_height_pts,
    } = source
        .load_page_cancellable(page_index, cancellation.clone())
        .await?;
    if cancellation.is_cancelled() {
        return Err(anyhow!("Raster reflow cancelled after source render"));
    }

    crate::pipeline::set_standard_dimensions_once(image.width(), image.height());
    let render_dpi = (image.width() as f32 / original_width_pts.max(1.0)) * 72.0;
    let high_res = Arc::new(image);

    let mut hints = Vec::new();
    if let Some((inference_handle, spec)) = inference {
        let inference_image = Arc::new(
            build_inference_image(&high_res, spec).unwrap_or_else(|_| (*high_res).clone()),
        );
        hints = inference_handle
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
        for det in hints.iter_mut() {
            if crate::pipeline::policies::is_in_inference_space(&det.bbox, spec) {
                det.bbox = crate::pipeline::policies::map_bbox_infer_to_page(
                    det.bbox, page_w, page_h, spec,
                );
            }
        }
    }

    let gray = image::imageops::grayscale(high_res.as_ref());
    // The page Arc is uniquely held here, so reclaim the RGB buffer instead of a
    // whole-page deep clone (Phase 8 reflow memory reduction).
    let rgb = if want_rgb {
        Some(match Arc::try_unwrap(high_res) {
            Ok(image) => image,
            Err(shared) => (*shared).clone(),
        })
    } else {
        drop(high_res);
        None
    };

    Ok(AnalyzedPage {
        page: crate::reflow::SourcePageImage {
            page_index: local_index,
            gray,
            rgb,
            render_dpi,
            page_pts: (original_width_pts, original_height_pts),
        },
        hints,
    })
}

/// A bounded window of loaded source pages for the compose pass.
///
/// Output pages are produced in reading order, so consecutive output pages ask
/// for the same or the next source page. Keeping the last few pages therefore
/// re-renders each source page about one time while the resident set stays at
/// `capacity` pages instead of the whole document.
struct SourcePageWindow {
    source: Arc<dyn PageSource>,
    page_start: usize,
    /// Source pages that carry a figure or table placement somewhere in the
    /// document. Only these need their color render kept.
    color_pages: std::collections::HashSet<usize>,
    capacity: usize,
    /// Least-recently-used order; the front is the next eviction candidate.
    order: std::collections::VecDeque<usize>,
    pages: std::collections::HashMap<usize, Arc<crate::reflow::SourcePageImage>>,
    renders: usize,
}

impl SourcePageWindow {
    fn new(
        source: Arc<dyn PageSource>,
        page_start: usize,
        doc: &crate::reflow::ReflowDocument,
    ) -> Self {
        let mut color_pages = std::collections::HashSet::new();
        for page in &doc.pages {
            for item in &page.items {
                if matches!(
                    item.kind,
                    crate::reflow::PlacedKind::Figure | crate::reflow::PlacedKind::Table
                ) {
                    color_pages.insert(item.src.page_index);
                }
            }
        }
        Self {
            source,
            page_start,
            color_pages,
            capacity: SOURCE_PAGE_WINDOW,
            order: std::collections::VecDeque::new(),
            pages: std::collections::HashMap::new(),
            renders: 0,
        }
    }

    fn touch(&mut self, local_index: usize) {
        if let Some(pos) = self.order.iter().position(|&i| i == local_index) {
            self.order.remove(pos);
        }
        self.order.push_back(local_index);
    }

    /// Drop least-recently-used pages until the window fits, never dropping a
    /// page the current output page still needs.
    fn evict_down_to(&mut self, keep: &std::collections::BTreeSet<usize>) {
        let limit = self.capacity.max(keep.len());
        let mut retained: std::collections::VecDeque<usize> =
            std::collections::VecDeque::with_capacity(self.order.len());
        while self.pages.len() > limit {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if keep.contains(&oldest) {
                retained.push_back(oldest);
                continue;
            }
            self.pages.remove(&oldest);
        }
        while let Some(index) = retained.pop_back() {
            self.order.push_front(index);
        }
    }

    /// Make every page in `needed` resident and return them as a set.
    async fn load(
        &mut self,
        needed: &std::collections::BTreeSet<usize>,
        cancellation: lege_pdf_read::CancellationToken,
    ) -> Result<crate::reflow::SourcePageSet> {
        for &local_index in needed {
            if !self.pages.contains_key(&local_index) {
                let analyzed = analyze_source_page(
                    &self.source,
                    None,
                    self.page_start + local_index,
                    local_index,
                    self.color_pages.contains(&local_index),
                    cancellation.clone(),
                )
                .await?;
                self.renders += 1;
                self.pages.insert(local_index, Arc::new(analyzed.page));
            }
            self.touch(local_index);
        }
        self.evict_down_to(needed);

        let mut set = crate::reflow::SourcePageSet::new();
        for &local_index in needed {
            if let Some(page) = self.pages.get(&local_index) {
                set.insert(Arc::clone(page));
            }
        }
        Ok(set)
    }
}

/// Source pages one output page draws from, in ascending order.
fn source_pages_for(page: &crate::reflow::ReflowPage) -> std::collections::BTreeSet<usize> {
    page.items.iter().map(|item| item.src.page_index).collect()
}

/// Shared analysis+plan setup used by both the PDF and DJVU reflow variants.
///
/// This is the analysis half of the two-pass reflow design. It never holds more
/// than one source page at a time:
///
/// 1. Calibration pass — render the sampled pages, measure the document body
///    text height, and build the calibrated config.
/// 2. Flow pass — render every page once more, extract its reading-order flow,
///    and drop the pixels. The flow holds rectangles only.
///
/// Composition is the second half and streams the source pages again through a
/// bounded window (see [`SourcePageWindow`]).
async fn analyze_and_plan(
    source: &Arc<dyn PageSource>,
    config: &Arc<PipelineConfig>,
    page_range: &Option<std::ops::Range<usize>>,
    progress_tracker: &ProgressTracker,
    shutdown_rx: &mut tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
    cancellation: &lege_pdf_read::CancellationToken,
) -> Result<ReflowPlan> {
    if !config.enable_layout_detection() {
        return Err(anyhow!(
            "Raster reflow requires layout detection; it cannot run with layout detection disabled"
        ));
    }

    let inference_handle = crate::pipeline::inference::InferenceHandle::new(config)?;

    let document_pages = source.page_count();
    let page_start = page_range.as_ref().map(|r| r.start).unwrap_or(0);
    let page_end = page_range.as_ref().map(|r| r.end).unwrap_or(document_pages);
    let total_source_pages = page_end.saturating_sub(page_start);
    if total_source_pages == 0 {
        return Err(anyhow!("Raster reflow: no pages to process"));
    }

    info_log!(
        "[Reflow] Analyzing {} source pages (bounded two-pass)",
        total_source_pages
    );

    let spec = config.inference_resize_spec();
    let sample_step =
        crate::reflow::body_height_sample_step(total_source_pages, BODY_HEIGHT_SAMPLES);
    let sample_indices: Vec<usize> = (0..total_source_pages).step_by(sample_step).collect();
    // The analysis passes report one progress unit per rendered page.
    let analysis_units = sample_indices.len() + total_source_pages;
    let mut analysis_done = 0usize;

    // Pass 1: calibration samples.
    let mut reflow_cfg: Option<crate::reflow::RasterReflowConfig> = None;
    let mut body_samples: Vec<u32> = Vec::with_capacity(sample_indices.len());
    let mut sampled_heights: Vec<u32> = Vec::with_capacity(sample_indices.len());
    for &local_index in &sample_indices {
        cancelled_if_signalled(shutdown_rx, cancellation)?;
        let analyzed = await_reflow_step(
            analyze_source_page(
                source,
                Some((&inference_handle, &spec)),
                page_start + local_index,
                local_index,
                false,
                cancellation.clone(),
            ),
            shutdown_rx,
            cancellation,
            "reflow calibration",
        )
        .await?;
        let cfg =
            reflow_cfg.get_or_insert_with(|| build_raster_reflow_config(config, &analyzed.page));
        sampled_heights.push(analyzed.page.gray.height());
        if cfg.text_magnification > 0.0
            && let Some(body) =
                crate::reflow::estimate_page_body_height(&analyzed.page, &analyzed.hints, cfg)
        {
            body_samples.push(body);
        }
        analysis_done += 1;
        progress_tracker.publish_reflow_progress(
            ReflowStage::SourceAnalysis,
            analysis_done,
            analysis_units,
        );
    }

    let mut reflow_cfg =
        reflow_cfg.ok_or_else(|| anyhow!("Raster reflow: no source page could be analyzed"))?;

    // Adaptive target text height calibration
    if reflow_cfg.text_magnification > 0.0 {
        if let Some(body_src) = crate::reflow::combine_body_height_samples(body_samples) {
            sampled_heights.sort_unstable();
            let src_page_h = sampled_heights[sampled_heights.len() / 2].max(1);
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

    // Pass 2: per-page reading-order flow. Only the flow survives each page.
    let mut full_flow: Vec<crate::reflow::FlowItem> = Vec::new();
    let mut confidence: Vec<crate::reflow::ReflowConfidence> =
        Vec::with_capacity(total_source_pages);
    let mut toc_hints = Vec::new();
    for local_index in 0..total_source_pages {
        cancelled_if_signalled(shutdown_rx, cancellation)?;
        let analyzed = await_reflow_step(
            analyze_source_page(
                source,
                Some((&inference_handle, &spec)),
                page_start + local_index,
                local_index,
                false,
                cancellation.clone(),
            ),
            shutdown_rx,
            cancellation,
            "reflow flow analysis",
        )
        .await?;
        let (flow, conf) =
            crate::reflow::reflow_page_flow(&analyzed.page, &analyzed.hints, &reflow_cfg);
        toc_hints.extend(analyzed.hints.iter().filter_map(|hint| {
            let name = hint.class_name.as_deref()?;
            matches!(name, "doc_title" | "paragraph_title" | "content")
                .then(|| (local_index, hint.clone()))
        }));
        full_flow.extend(flow);
        // Source-page boundary is a safe break between flows.
        full_flow.push(crate::reflow::FlowItem::RegionBreak);
        confidence.push(conf);

        analysis_done += 1;
        progress_tracker.publish_reflow_progress(
            ReflowStage::SourceAnalysis,
            analysis_done,
            analysis_units,
        );
    }

    info_log!(
        "[Reflow] Composing {} source pages onto a {}x{} canvas",
        total_source_pages,
        reflow_cfg.page_width,
        reflow_cfg.page_height
    );
    progress_tracker.update(crate::progress::ProcessingStatus::ReflowProgress {
        stage: ReflowStage::Compose,
        current: total_source_pages,
        total: total_source_pages,
        eta: None,
    });
    let doc = crate::reflow::paginate_document_flow(full_flow, confidence, &reflow_cfg);
    if doc.pages.is_empty() {
        return Err(anyhow!("Raster reflow produced no output pages"));
    }
    info_log!(
        "[Reflow] Reflowed {} source pages into {} output pages",
        total_source_pages,
        doc.pages.len()
    );

    Ok(ReflowPlan {
        cfg: reflow_cfg,
        doc,
        total_source_pages,
        toc_hints,
    })
}

/// Whole-document batch path used when [`PipelineConfig::enable_reflow`] is set.
///
/// Reflow re-composes the *entire* document into new pages before any of it can
/// be written, so it cannot stream page-by-page like
/// [`crate::pipeline::pdf_tokio_pipeline::create_and_run_pdf_source_pipeline`].
/// This renders every source page and runs layout detection up front
/// (concurrently, bounded by the PDF source's advertised source concurrency,
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
    info_log!("[Reflow] Starting raster-reflow PDF pipeline");
    crate::pipeline::reset_standard_dimensions();

    let runtime_limits = PipelineRuntimeLimits::from_config(&config);
    init_encode_semaphore(runtime_limits.page_workers);
    let cancellation = lege_pdf_read::CancellationToken::new();

    let ReflowPlan {
        cfg: reflow_cfg,
        doc,
        total_source_pages,
        toc_hints,
    } = analyze_and_plan(
        &source,
        &config,
        &page_range,
        progress_tracker,
        &mut shutdown_rx,
        &cancellation,
    )
    .await?;
    let page_start = page_range.as_ref().map(|r| r.start).unwrap_or(0);
    let mut window = SourcePageWindow::new(source.clone(), page_start, &doc);

    let total_out_pages = doc.pages.len();
    progress_tracker.publish_reflow_progress(ReflowStage::OutputPages, 0, total_out_pages);

    let (pdf_writer_handle, mut pdf_writer_task) = spawn_pdf_writer_actor(
        output_path.to_path_buf(),
        total_out_pages,
        progress_tracker.clone(),
        false,
        runtime_limits.channel_capacity,
    );
    let mut toc_pages = Vec::new();

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

        // Load only the source pages this output page draws from, then compose.
        let source_pages = await_reflow_step(
            window.load(&source_pages_for(reflow_page), cancellation.clone()),
            &mut shutdown_rx,
            &cancellation,
            "reflow source loading",
        )
        .await?;
        let canvas = Arc::new(compose_reflow_page_raster(
            reflow_page,
            &source_pages,
            &reflow_cfg,
        ));

        // Overlay a searchable text layer onto the reflowed page, reusing the
        // text rows / words reflow already detected. Slow OCR recognizes the
        // source crops and re-projects each word; fast OCR recognizes the
        // composed reflow raster block-by-block. Either way the hOCR lands in
        // output-page coordinates so the text overlays the reflowed bitmaps.
        let hocr_text = if config.enable_ocr() {
            let result = await_reflow_step(
                async {
                    if config.slow_ocr_enabled() {
                        crate::ocr::slow::perform_reflow_page_ocr(
                            reflow_page,
                            &source_pages,
                            &config,
                        )
                        .await
                    } else {
                        crate::ocr::fast::perform_reflow_page_fast_ocr(
                            reflow_page,
                            &canvas,
                            config.ocr_language(),
                        )
                        .await
                    }
                },
                &mut shutdown_rx,
                &cancellation,
                "reflow OCR",
            )
            .await;
            match result {
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

        let encoded = await_reflow_step(
            encode_base_layer_for_jpeg_mode(canvas, &config, reflow_page.index),
            &mut shutdown_rx,
            &cancellation,
            "reflow page encoding",
        )
        .await?;

        let toc_detections = mapped_toc_detections(reflow_page, &toc_hints);
        if config.enable_auto_toc() {
            toc_pages.push(crate::toc::capture_page(
                &toc_detections,
                hocr_text.as_deref(),
                reflow_page.index,
                reflow_page.width,
                reflow_page.height,
            ));
        }

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

    info_log!(
        "[Reflow] Composed {} output pages from {} source pages with {} streamed page renders (window {})",
        total_out_pages,
        total_source_pages,
        window.renders,
        SOURCE_PAGE_WINDOW
    );

    // After all pages are composed, extract bookmarks and send them before finalize.
    // Reflow re-paginates, so we build source-page → first-output-page mapping from SourceMap.
    let mut source_metadata = lege_pdf_read::DocumentMetadata::default();
    if let Some(session) = source.document_session() {
        let source_map = &doc.source_map;
        let mut src_to_out: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        for placement in &source_map.placements {
            src_to_out
                .entry(page_start + placement.src.page_index)
                .and_modify(|e| *e = (*e).min(placement.out_page))
                .or_insert(placement.out_page);
        }
        let mut outline_task = crate::runtime_stats::spawn_blocking(move || {
            (
                lege_pdf_read::extract_outline(&session),
                lege_pdf_read::extract_metadata(&session),
            )
        });
        let (bookmarks, metadata) = tokio::select! {
            result = &mut outline_task => result?,
            signal = shutdown_rx.recv() => {
                cancellation.cancel();
                outline_task.abort();
                pdf_writer_task.abort();
                let message = signal
                    .ok()
                    .and_then(|signal| signal.message)
                    .unwrap_or_else(|| "User requested cancellation".to_string());
                return Err(anyhow!("Processing cancelled during reflow outline extraction: {message}"));
            }
        };
        source_metadata = metadata;
        if !bookmarks.is_empty() {
            pdf_writer_handle
                .send_bookmarks(bookmarks, src_to_out)
                .await?;
        }
    }

    let candidates = toc_pages
        .iter()
        .flat_map(|page| page.candidates.clone())
        .collect();
    let stats = toc_pages
        .iter()
        .filter_map(|page| page.stats)
        .collect::<Vec<_>>();
    let contents = toc_pages
        .iter()
        .flat_map(|page| page.printed_contents.clone())
        .collect::<Vec<_>>();
    let synthetic =
        crate::toc::build_outline_with_contents(candidates, &stats, total_out_pages, &contents);
    pdf_writer_handle.send_synthetic_outline(synthetic).await?;
    let metadata_candidates = toc_pages
        .iter()
        .flat_map(|page| page.metadata_candidates.clone())
        .collect::<Vec<_>>();
    let inferred = crate::toc::infer_metadata(&metadata_candidates, &stats, total_out_pages);
    let title = source_metadata.title.or(inferred.title);
    let author = source_metadata.author.or(inferred.author);
    if title.is_some() || author.is_some() {
        pdf_writer_handle
            .send_document_identity(title, author)
            .await?;
    }

    pdf_writer_handle.finalize().await?;
    await_stage_or_cancel_with_token(
        &mut pdf_writer_task,
        &mut shutdown_rx,
        "PDF writer",
        &[],
        Some(&cancellation),
    )
    .await?;

    success_log!("Raster reflow pipeline complete: {}", output_path.display());
    Ok(())
}

/// DJVU variant of the raster reflow pipeline.
///
/// Shares the render+detect+reflow setup with the PDF variant. For each output
/// page the composed RGB canvas is binarized and figure placements are mapped to
/// `ContentCategory::Image` detections so the DJVU orchestrator can route them
/// to the IW44 color background layer while text stays in the JB2 foreground.
pub(crate) async fn run_raster_reflow_djvu_pipeline(
    source: Arc<dyn PageSource>,
    config: Arc<PipelineConfig>,
    output_path: &Path,
    page_range: Option<std::ops::Range<usize>>,
    progress_tracker: &ProgressTracker,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
) -> Result<()> {
    info_log!("[Reflow] Starting raster-reflow DJVU pipeline");
    info_log!(
        "[Reflow] Native DjVu encoder backend: {}",
        crate::djvu::active_backend_info()
    );
    crate::pipeline::reset_standard_dimensions();

    let runtime_limits = PipelineRuntimeLimits::djvu_from_config(&config);
    let cancellation = lege_pdf_read::CancellationToken::new();

    let ReflowPlan {
        cfg: reflow_cfg,
        doc,
        total_source_pages,
        toc_hints,
    } = analyze_and_plan(
        &source,
        &config,
        &page_range,
        progress_tracker,
        &mut shutdown_rx,
        &cancellation,
    )
    .await?;
    let page_start = page_range.as_ref().map(|r| r.start).unwrap_or(0);
    let mut window = SourcePageWindow::new(source.clone(), page_start, &doc);

    let total_out_pages = doc.pages.len();
    progress_tracker.publish_reflow_progress(ReflowStage::OutputPages, 0, total_out_pages);

    let djvu_config =
        crate::pipeline::djvu_pipeline::create_djvu_pipeline_config(output_path, &config)?;
    let dpi = djvu_config.dpi;
    let iw44_quality = djvu_config.iw44_quality;
    let orchestrator = Arc::new(crate::djvu::DjvuOrchestrator::new(djvu_config)?);
    let (djvu_writer, djvu_document, mut writer_task) = crate::djvu::spawn_djvu_writer_actor(
        output_path.to_path_buf(),
        total_out_pages,
        dpi,
        iw44_quality,
        3, // reflow output is bilevel text over figures; c44's default subsample
        progress_tracker.clone(),
        runtime_limits.channel_capacity,
        cancellation.clone(),
    );
    let mut toc_pages = Vec::new();

    for reflow_page in &doc.pages {
        if let Ok(signal) = shutdown_rx.try_recv() {
            cancellation.cancel();
            writer_task.abort();
            return Err(anyhow!(
                "Processing cancelled: {}",
                signal
                    .message
                    .unwrap_or_else(|| "User requested cancellation".to_string())
            ));
        }

        // Load only the source pages this output page draws from, then compose.
        let source_pages = await_reflow_step(
            window.load(&source_pages_for(reflow_page), cancellation.clone()),
            &mut shutdown_rx,
            &cancellation,
            "reflow source loading",
        )
        .await?;
        let canvas = compose_reflow_page_raster(reflow_page, &source_pages, &reflow_cfg);
        let hocr_text = if config.enable_ocr() {
            let canvas_ref = Arc::new(canvas.clone());
            if config.slow_ocr_enabled() {
                crate::ocr::slow::perform_reflow_page_ocr(reflow_page, &source_pages, &config)
                    .await?
            } else {
                crate::ocr::fast::perform_reflow_page_fast_ocr(
                    reflow_page,
                    &canvas_ref,
                    config.ocr_language(),
                )
                .await?
            }
        } else {
            None
        };
        let toc_detections = mapped_toc_detections(reflow_page, &toc_hints);
        if config.enable_auto_toc() {
            toc_pages.push(crate::toc::capture_page(
                &toc_detections,
                hocr_text.as_deref(),
                reflow_page.index,
                reflow_page.width,
                reflow_page.height,
            ));
        }
        let binarize_config = config.clone();
        let binarize_cancellation = cancellation.clone();
        let (canvas, binarized) = await_reflow_step(
            async move {
                crate::runtime_stats::spawn_blocking_stage(
                    crate::runtime_stats::Stage::Processing,
                    move || -> Result<_> {
                        if binarize_cancellation.is_cancelled() {
                            return Err(anyhow!("Reflow binarization cancelled before CPU work"));
                        }
                        let binarized = crate::pipeline::djvu_pipeline::binarize_djvu_image(
                            &canvas,
                            &binarize_config,
                            false,
                        );
                        if binarize_cancellation.is_cancelled() {
                            return Err(anyhow!("Reflow binarization cancelled after CPU work"));
                        }
                        Ok((canvas, binarized))
                    },
                )
                .await
                .map_err(|error| anyhow!("DjVu binarization task panicked: {error}"))?
            },
            &mut shutdown_rx,
            &cancellation,
            "reflow binarization",
        )
        .await?;

        // Map figure placements to image-category detections so the DJVU
        // orchestrator routes them to the IW44 color background layer.
        let detections: Vec<crate::engine::Detection> = reflow_page
            .items
            .iter()
            .filter(|item| matches!(item.kind, crate::reflow::PlacedKind::Figure))
            .map(|item| crate::engine::Detection {
                class_id: crate::types::class_id_for("image").unwrap_or(1),
                class_name: Some("image".to_string()),
                confidence: 1.0,
                bbox: [
                    item.out_rect.x as f32,
                    item.out_rect.y as f32,
                    (item.out_rect.x + item.out_rect.w) as f32,
                    (item.out_rect.y + item.out_rect.h) as f32,
                ],
                category: crate::types::ContentCategory::Image,
                context: None,
            })
            .collect();

        let page_data = crate::djvu::PageData {
            index: reflow_page.index,
            preserve_full_color: false,
            rgb_image: canvas,
            binarized,
            cleaned_gray: None,
            detections,
            hocr: hocr_text,
        };

        let orchestrator = orchestrator.clone();
        let djvu_document = djvu_document.clone();
        let compose_cancellation = cancellation.clone();
        let page_index = page_data.index;
        let encoded = await_reflow_step(
            async move {
                crate::runtime_stats::spawn_blocking_stage(
                    crate::runtime_stats::Stage::Encode,
                    move || -> Result<_> {
                        if compose_cancellation.is_cancelled() {
                            return Err(anyhow!("DjVu composition cancelled before CPU work"));
                        }
                        let prepared = orchestrator.process_page(page_data)?;
                        let encoded = prepared.encode(&djvu_document, iw44_quality, 3)?;
                        if compose_cancellation.is_cancelled() {
                            return Err(anyhow!("DjVu composition cancelled after CPU work"));
                        }
                        Ok(encoded)
                    },
                )
                .await
                .map_err(|error| anyhow!("DjVu compose task panicked: {error}"))?
            },
            &mut shutdown_rx,
            &cancellation,
            "reflow DjVu composition",
        )
        .await?;

        djvu_writer.append_encoded(encoded, page_index).await?;

        let done = reflow_page.index + 1;
        progress_tracker.publish_reflow_progress(ReflowStage::OutputPages, done, total_out_pages);
    }

    info_log!(
        "[Reflow] Composed {} output pages from {} source pages with {} streamed page renders (window {})",
        total_out_pages,
        total_source_pages,
        window.renders,
        SOURCE_PAGE_WINDOW
    );

    let candidates = toc_pages
        .iter()
        .flat_map(|page| page.candidates.clone())
        .collect();
    let stats = toc_pages
        .iter()
        .filter_map(|page| page.stats)
        .collect::<Vec<_>>();
    let contents = toc_pages
        .iter()
        .flat_map(|page| page.printed_contents.clone())
        .collect::<Vec<_>>();
    let synthetic =
        crate::toc::build_outline_with_contents(candidates, &stats, total_out_pages, &contents);
    let source_outline = if let Some(session) = source.document_session() {
        let bookmarks =
            crate::runtime_stats::spawn_blocking(move || lege_pdf_read::extract_outline(&session))
                .await?;
        let mut source_to_output = std::collections::HashMap::new();
        for placement in &doc.source_map.placements {
            source_to_output
                .entry(page_start + placement.src.page_index)
                .and_modify(|page: &mut usize| *page = (*page).min(placement.out_page))
                .or_insert(placement.out_page);
        }
        bookmarks_to_outline(&bookmarks, &source_to_output)
    } else {
        Vec::new()
    };
    let accepted = merge_outline(source_outline, Some(synthetic));
    if !accepted.is_empty() {
        djvu_writer.send_outline(accepted).await?;
    }
    djvu_writer.finalize().await?;
    await_stage_or_cancel_with_token(
        &mut writer_task,
        &mut shutdown_rx,
        "DJVU writer",
        &[],
        Some(&cancellation),
    )
    .await?;

    success_log!(
        "Raster reflow DJVU pipeline complete: {}",
        output_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::source::SourcePage;
    use crate::reflow::{PlacedItem, PlacedKind, PxRect, ReflowPage, SourceRef};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A page source that counts renders, so a test can prove the compose pass
    /// loads each source page about one time.
    struct CountingSource {
        pages: usize,
        loads: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl PageSource for CountingSource {
        fn page_count(&self) -> usize {
            self.pages
        }

        fn source_concurrency(&self) -> usize {
            1
        }

        async fn load_page(&self, _page_index: usize) -> Result<SourcePage> {
            self.loads.fetch_add(1, Ordering::Relaxed);
            Ok(SourcePage {
                image: RgbImage::from_pixel(32, 40, image::Rgb([255, 255, 255])),
                original_width_pts: 32.0,
                original_height_pts: 40.0,
            })
        }
    }

    fn item(src_page: usize, kind: PlacedKind) -> PlacedItem {
        PlacedItem {
            out_rect: PxRect::new(0, 0, 10, 10),
            src: SourceRef {
                page_index: src_page,
                rect: PxRect::new(0, 0, 10, 10),
            },
            scale: 1.0,
            kind,
        }
    }

    /// One output page per source page, `n` of them.
    fn document(source_pages: &[Vec<usize>], kind: PlacedKind) -> crate::reflow::ReflowDocument {
        let mut doc = crate::reflow::ReflowDocument::default();
        for (index, sources) in source_pages.iter().enumerate() {
            let mut page = ReflowPage::new(index, 100, 100);
            page.items = sources.iter().map(|&s| item(s, kind)).collect();
            doc.pages.push(page);
        }
        doc
    }

    fn window_for(
        doc: &crate::reflow::ReflowDocument,
        loads: Arc<AtomicUsize>,
        pages: usize,
    ) -> SourcePageWindow {
        let source: Arc<dyn PageSource> = Arc::new(CountingSource { pages, loads });
        SourcePageWindow::new(source, 0, doc)
    }

    fn active_cancellation() -> lege_pdf_read::CancellationToken {
        lege_pdf_read::CancellationToken::new()
    }

    #[test]
    fn title_detection_is_projected_into_output_space() {
        let mut page = ReflowPage::new(0, 200, 200);
        page.items.push(PlacedItem {
            out_rect: PxRect::new(40, 60, 80, 20),
            src: SourceRef {
                page_index: 2,
                rect: PxRect::new(100, 120, 160, 40),
            },
            scale: 0.5,
            kind: PlacedKind::Run,
        });
        let hint = crate::engine::Detection {
            class_id: crate::types::class_id_for("paragraph_title").unwrap_or(0),
            class_name: Some("paragraph_title".to_string()),
            confidence: 0.91,
            bbox: [90.0, 110.0, 280.0, 180.0],
            category: crate::types::ContentCategory::Text,
            context: None,
        };
        let mapped = mapped_toc_detections(&page, &[(2, hint)]);
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].bbox, [40.0, 60.0, 120.0, 80.0]);
        assert_eq!(mapped[0].class_name.as_deref(), Some("paragraph_title"));
    }

    #[tokio::test]
    async fn compose_window_stays_bounded_and_renders_each_page_once() {
        let doc = document(
            &[vec![0], vec![1], vec![2], vec![3], vec![4], vec![5]],
            PlacedKind::Word,
        );
        let loads = Arc::new(AtomicUsize::new(0));
        let mut window = window_for(&doc, loads.clone(), 6);

        for page in &doc.pages {
            let set = window
                .load(&source_pages_for(page), active_cancellation())
                .await
                .expect("load");
            assert_eq!(set.len(), 1);
            assert!(
                window.pages.len() <= SOURCE_PAGE_WINDOW,
                "resident pages {} exceeded the window",
                window.pages.len()
            );
        }
        assert_eq!(loads.load(Ordering::Relaxed), 6);
        assert_eq!(window.renders, 6);
    }

    #[tokio::test]
    async fn output_page_spanning_two_source_pages_gets_both() {
        let doc = document(&[vec![0, 1]], PlacedKind::Word);
        let loads = Arc::new(AtomicUsize::new(0));
        let mut window = window_for(&doc, loads, 2);

        let set = window
            .load(&source_pages_for(&doc.pages[0]), active_cancellation())
            .await
            .expect("load");
        assert!(set.get(0).is_some() && set.get(1).is_some());
    }

    #[tokio::test]
    async fn a_resident_page_is_not_rendered_again() {
        let doc = document(&[vec![0], vec![0], vec![1], vec![0]], PlacedKind::Word);
        let loads = Arc::new(AtomicUsize::new(0));
        let mut window = window_for(&doc, loads.clone(), 2);

        for page in &doc.pages {
            window
                .load(&source_pages_for(page), active_cancellation())
                .await
                .expect("load");
        }
        assert_eq!(loads.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn color_is_kept_only_for_pages_that_carry_a_figure() {
        let text_doc = document(&[vec![0]], PlacedKind::Word);
        let mut text_window = window_for(&text_doc, Arc::new(AtomicUsize::new(0)), 1);
        let text_set = text_window
            .load(&source_pages_for(&text_doc.pages[0]), active_cancellation())
            .await
            .expect("load");
        assert!(text_set.get(0).expect("page").rgb.is_none());

        let figure_doc = document(&[vec![0]], PlacedKind::Figure);
        let mut figure_window = window_for(&figure_doc, Arc::new(AtomicUsize::new(0)), 1);
        let figure_set = figure_window
            .load(
                &source_pages_for(&figure_doc.pages[0]),
                active_cancellation(),
            )
            .await
            .expect("load");
        assert!(figure_set.get(0).expect("page").rgb.is_some());
    }
}
