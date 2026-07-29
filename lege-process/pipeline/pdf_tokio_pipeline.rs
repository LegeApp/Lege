// pdf_tokio_pipeline.rs
use crate::margin::DocumentMarginAnalysis;
use crate::pagerender::NativeTextWord;
use crate::pipeline::config::{
    ImageRegionDitherMode, InferenceResult, PipelineConfig, RenderedPageData,
};
use crate::pipeline::helper_functions::{
    build_hocr_from_positioned_words, image_detection_overlaps_substantive_text,
    init_encode_semaphore, merge_overlapping_image_detections, rounded_clamped_bbox,
    should_preserve_cover_page, spawn_pdf_writer_actor,
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
use crate::pipeline::source::{PageSource, PdfPageSource};
use crate::progress::ProgressTracker;
use crate::{info_log, success_log, warn_log};

use crate::color::BinarizationOptions;
use crate::encoding::Jbig2Mode;
use anyhow::{Result, anyhow};
use futures::future::BoxFuture;
use image::RgbImage;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

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
    /// Title candidates for the automatic table of contents. Text only; empty
    /// on any page with no title detection.
    pub toc: crate::toc::PageTocData,
}

pub(crate) fn build_inference_future(
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

/// Process a single page (runs in its own task)
async fn process_single_page(
    config: Arc<PipelineConfig>,
    document_session: Option<Arc<lege_pdf_read::RenderSession>>,
    inference_data: PdfInferenceData,
    page_index_offset: usize,
    margin_analysis: Option<Arc<DocumentMarginAnalysis>>,
    cancellation: lege_pdf_read::CancellationToken,
) -> Result<ProcessedPage> {
    checkpoint(&cancellation, "before page processing")?;
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
        cancellation: cancellation.clone(),
    };

    let cpu_result = crate::runtime_stats::spawn_blocking_stage(
        crate::runtime_stats::Stage::Processing,
        move || process_page_cpu_work(input),
    )
    .await
    .map_err(|e| anyhow!("CPU task panicked: {}", e))??;
    checkpoint(&cancellation, "after page processing")?;

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
    checkpoint(&cancellation, "before OCR/text extraction")?;
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
            document_session.as_ref(),
            page_index,
            adjusted_image.width(),
            adjusted_image.height(),
            &native_text_transform,
        )
        .await?
    };
    checkpoint(&cancellation, "after OCR/text extraction")?;

    // Detections and hOCR share the output page's pixel space here, which is the
    // only place the automatic table of contents needs them together.
    let toc = crate::toc::capture_page(
        &adjusted_detections,
        hocr_text.as_deref(),
        local_index,
        width as u32,
        height as u32,
    );

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
    checkpoint(&cancellation, "before page encoding")?;

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
            toc,
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
                    toc,
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
        toc,
    })
}

/// CPU-intensive work for a single page (to be executed in spawn_blocking)
struct PageProcessingInput {
    rendered: RenderedPageData,
    inference_result: InferenceResult,
    page_index: usize,
    config: Arc<PipelineConfig>,
    margin_analysis: Option<Arc<DocumentMarginAnalysis>>,
    cancellation: lege_pdf_read::CancellationToken,
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
        cancellation,
    } = input;
    checkpoint(&cancellation, "before CPU page work")?;

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
    checkpoint(&cancellation, "after page geometry adjustment")?;

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
    checkpoint(&cancellation, "after output resize")?;

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

    // False image detections over text/line art must not become raster
    // overlays. Besides covering a cleaned MRC background, they create the
    // conspicuous "half a text column in color" seam in ordinary bilevel mode.
    if config.text_format() != "jpeg" {
        let all_detections = adjusted_detections.clone();
        adjusted_detections.retain(|det| {
            if !classifier.is_image_label(det) {
                return true;
            }
            if image_detection_overlaps_substantive_text(
                det,
                &all_detections,
                classifier,
            ) {
                crate::bbox_trace!(
                    "PAGE {}: dropping image region overlapping substantive text ({:.0},{:.0},{:.0},{:.0})",
                    page_index,
                    det.bbox[0],
                    det.bbox[1],
                    det.bbox[2],
                    det.bbox[3]
                );
                return false;
            }
            let line_art = crate::clean_gray::region_is_line_art(
                adjusted_image.as_raw(),
                width,
                height,
                det.bbox,
            );
            if line_art {
                crate::bbox_trace!(
                    "PAGE {} bilevel: dropping line-art image region ({:.0},{:.0},{:.0},{:.0})",
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
    checkpoint(&cancellation, "after binarization")?;

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
        checkpoint(&cancellation, "during region processing")?;
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

fn checkpoint(cancellation: &lege_pdf_read::CancellationToken, stage: &'static str) -> Result<()> {
    if cancellation.is_cancelled() {
        Err(anyhow!("Processing cancelled {stage}"))
    } else {
        Ok(())
    }
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
    document_session: Option<&Arc<lege_pdf_read::RenderSession>>,
    page_index: usize,
    output_width: u32,
    output_height: u32,
    text_transform: &NativeTextTransform,
) -> Result<Option<String>> {
    let Some(session) = document_session else {
        return Ok(None);
    };
    let session = Arc::clone(session);
    let source_width = text_transform.source_width;
    let source_height = text_transform.source_height;
    let renderer_words =
        crate::runtime_stats::spawn_blocking_stage(crate::runtime_stats::Stage::Ocr, move || {
            lege_pdf_read::positioned_words(
                &session,
                page_index as u32,
                source_width,
                source_height,
            )
        })
        .await
        .map_err(|error| anyhow!("Renderer text task panicked: {error}"))?;

    match renderer_words {
        Ok(words) if !words.is_empty() => {
            let mapped = map_native_text_words(words, text_transform, output_width, output_height);
            let hocr = build_hocr_from_positioned_words(&mapped, output_width, output_height);
            Ok((!hocr.trim().is_empty()).then_some(hocr))
        }
        Ok(_) => Ok(None),
        Err(error) => {
            warn_log!(
                "Renderer text extraction failed on page {}: {}",
                page_index,
                error
            );
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
/// Symbol default. Generic mode remains the compatibility baseline because
/// symbol-mode `/ImageMask` stencils have historically rendered blank in
/// common readers, while the same streams work as opaque images.
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
    // See the doc comment: Generic is the compatible ImageMask representation.
    // `force_generic` is kept in the signature so the Abandon-region rule stays
    // wired for the day symbol becomes usable here.
    let _ = force_generic;
    let jbig2_mode = crate::encoding::Jbig2Mode::Generic;
    crate::runtime_stats::spawn_blocking_stage(crate::runtime_stats::Stage::Encode, move || {
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
                // Inlining dictionary segments before page segments is legal
                // embedded JBIG2 and avoids reader-specific globals handling.
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

    let encoding_result = crate::runtime_stats::spawn_blocking_stage(
        crate::runtime_stats::Stage::Encode,
        move || {
            let buffer = LegeImageBuffer {
                data: &binarized,
                width: width as u32,
                height: height as u32,
                channels: 1u8, // Grayscale/binary data
            };
            EncodingManager::encode(&buffer, &encoding_settings)
                .map_err(|e| anyhow!("Encoding failed for format {}: {}", text_format, e))
        },
    )
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

    let encoding_result = crate::runtime_stats::spawn_blocking_stage(
        crate::runtime_stats::Stage::Encode,
        move || {
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
        },
    )
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

    let (data, format) = crate::runtime_stats::spawn_blocking_stage(
        crate::runtime_stats::Stage::Encode,
        move || {
            let buffer = LegeImageBuffer {
                data: image_data.as_raw(),
                width,
                height,
                channels: 3u8,
            };
            let (settings, fmt) = if jpeg_compat {
                (
                    EncodingSettings::Jpeg(JpegSettings {
                        quality: crate::pipeline::quality_policy::full_page_jpeg_compat(
                            high_quality,
                        ),
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
        },
    )
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
    // Keep margin pass inference queued through the actor; PageSource handles
    // whether loading is PDF-backed or image-backed.
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
    // PDF sources render through the document session; image-folder sources
    // decode their existing images and enter the same analysis path.
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
        let prepared = crate::runtime_stats::spawn_blocking(move || {
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

struct PageOwnedJobOutput {
    hocr_page: Option<SpilledHocrPage>,
    toc: crate::toc::PageTocData,
}

struct SpilledHocrPage {
    page_index: usize,
    width_px: u32,
    height_px: u32,
    path: std::path::PathBuf,
}

struct PlannedPdfContext {
    page: Arc<lege_pdf_read::CompiledDocumentPage>,
    plan: lege_pdf_read::PageOutputPlan,
    output_width: u32,
    output_height: u32,
    analysis_image: RgbImage,
    original_width_pts: f32,
    original_height_pts: f32,
}

fn dimensions_for_geometry(
    geometry: lege_pdf_read::PageGeometry,
    target_height: u32,
    target_width: Option<u32>,
) -> (u32, u32) {
    if let Some(width) = target_width {
        return (width.max(1), target_height.max(1));
    }
    let width = (geometry.display_width() * f64::from(target_height.max(1))
        / geometry.display_height().max(f64::EPSILON))
    .round()
    .clamp(1.0, f64::from(u32::MAX)) as u32;
    (width, target_height.max(1))
}

async fn prepare_planned_pdf_page(
    session: Arc<lege_pdf_read::RenderSession>,
    config: Arc<PipelineConfig>,
    page_index: usize,
    cancellation: lege_pdf_read::CancellationToken,
) -> Result<Option<PlannedPdfContext>> {
    if config.is_grayscale_mode()
        || config.text_format() == "jpeg"
        || (config.dither_images() && !config.keep_original_images())
        || should_preserve_cover_page(page_index, &config)
    {
        return Ok(None);
    }
    crate::runtime_stats::spawn_blocking_stage(crate::runtime_stats::Stage::Render, move || {
        let geometry = session
            .page_geometry(page_index as u32)
            .map_err(|error| anyhow!("page geometry failed: {error}"))?;
        let (output_width, output_height) =
            dimensions_for_geometry(geometry, config.target_height(), config.target_width());
        let page = session
            .compile(page_index as u32)
            .map_err(|error| anyhow!("page compile failed: {error}"))?;
        let mut plan = crate::pipeline::page_output_plan::plan_page_output(
            &config,
            crate::pipeline::page_output_plan::PagePlanInput {
                output_width,
                output_height,
                gray_suitability: page.gray_suitability(),
            },
        );
        if plan.base.product.format != lege_pdf_read::RasterFormat::Gray8 {
            return Ok(None);
        }
        let analysis_image = if let Some(target) = plan.analysis.take() {
            let plane = session
                .render_cancellable(&page, &target.product, Some(&cancellation))
                .map_err(|error| anyhow!("analysis render failed: {error}"))?;
            let lege_pdf_read::RasterPlane::Rgb8(surface) = plane else {
                return Err(anyhow!("analysis target returned a non-RGB plane"));
            };
            RgbImage::from_raw(surface.width, surface.height, surface.pixels.to_vec())
                .ok_or_else(|| anyhow!("analysis RGB surface was truncated"))?
        } else {
            RgbImage::from_pixel(1, 1, image::Rgb([255, 255, 255]))
        };
        Ok(Some(PlannedPdfContext {
            page,
            plan,
            output_width,
            output_height,
            analysis_image,
            original_width_pts: geometry.display_width() as f32,
            original_height_pts: geometry.display_height() as f32,
        }))
    })
    .await
    .map_err(|error| anyhow!("planned PDF preparation task panicked: {error}"))?
}

fn scale_detections_to_output(
    inference_data: &PdfInferenceData,
    output_width: u32,
    output_height: u32,
) -> Vec<crate::engine::Detection> {
    let (source_width, source_height) = if inference_data.inference_result.detections_are_page_space
    {
        (
            inference_data.rendered.high_res_image.width(),
            inference_data.rendered.high_res_image.height(),
        )
    } else {
        (
            inference_data.rendered.inference_image.width(),
            inference_data.rendered.inference_image.height(),
        )
    };
    let sx = output_width as f32 / source_width.max(1) as f32;
    let sy = output_height as f32 / source_height.max(1) as f32;
    let mut detections = inference_data.inference_result.detections.clone();
    if !inference_data.inference_result.detections_are_page_space {
        let inference = inference_data.rendered.inference_image.as_ref();
        detections.retain(|detection| {
            !crate::types::LABEL_CLASSIFIER.is_image_label(detection)
                || !crate::clean_gray::region_is_line_art(
                    inference.as_raw(),
                    inference.width() as usize,
                    inference.height() as usize,
                    detection.bbox,
                )
        });
    }
    for detection in &mut detections {
        detection.scale_bbox(sx, sy);
    }
    merge_overlapping_image_detections(
        &mut detections,
        &crate::types::LABEL_CLASSIFIER,
        output_width,
        output_height,
    );
    let all_detections = detections.clone();
    detections.retain(|detection| {
        !image_detection_overlaps_substantive_text(
            detection,
            &all_detections,
            &crate::types::LABEL_CLASSIFIER,
        )
    });
    detections
}

fn compact_gray_surface(surface: lege_pdf_read::GraySurface) -> Result<Vec<u8>> {
    let width = surface.width as usize;
    let height = surface.height as usize;
    if surface.stride == width && surface.pixels.len() == width.saturating_mul(height) {
        return Ok(surface.pixels.to_vec());
    }
    let mut compact = Vec::with_capacity(width.saturating_mul(height));
    for row in 0..height {
        let start = row.saturating_mul(surface.stride);
        let end = start.saturating_add(width);
        compact.extend_from_slice(
            surface
                .pixels
                .get(start..end)
                .ok_or_else(|| anyhow!("truncated Gray8 row {row}"))?,
        );
    }
    Ok(compact)
}

fn gray_to_rgb_image(gray: &[u8], width: u32, height: u32) -> Result<RgbImage> {
    let mut rgb = Vec::with_capacity(gray.len().saturating_mul(3));
    for &pixel in gray {
        rgb.extend_from_slice(&[pixel, pixel, pixel]);
    }
    RgbImage::from_raw(width, height, rgb).ok_or_else(|| anyhow!("failed to expand Gray8 to RGB"))
}

#[allow(clippy::too_many_arguments)]
async fn process_planned_pdf_products(
    config: Arc<PipelineConfig>,
    document_session: Arc<lege_pdf_read::RenderSession>,
    page_index: usize,
    page_start: usize,
    products: lege_pdf_read::PageRasterProducts,
    detections: Vec<crate::engine::Detection>,
    cancellation: lege_pdf_read::CancellationToken,
) -> Result<ProcessedPage> {
    let lege_pdf_read::RasterPlane::Gray8(base_surface) = products.base else {
        return Err(anyhow!("planned gray path received a non-gray base"));
    };
    let width = base_surface.width;
    let height = base_surface.height;
    let mut gray = compact_gray_surface(base_surface)?;
    for region in &products.regions {
        if let Some(crop) = region.target.product.crop {
            const PAD: u32 = 3;
            let x1 = (crop.x.max(0) as u32).saturating_sub(PAD);
            let y1 = (crop.y.max(0) as u32).saturating_sub(PAD);
            let x2 = (crop.x.max(0) as u32 + crop.width + PAD).min(width);
            let y2 = (crop.y.max(0) as u32 + crop.height + PAD).min(height);
            for y in y1..y2 {
                let start = y as usize * width as usize + x1 as usize;
                let end = y as usize * width as usize + x2 as usize;
                gray[start..end].fill(255);
            }
        }
    }
    checkpoint(&cancellation, "after Gray8 region masking")?;
    let options = crate::pipeline::policies::binarize_options_for(&config, false);
    let binarized =
        crate::color::binarization::binarize_gray(&gray, width as usize, height as usize, &options);
    checkpoint(&cancellation, "after planned Gray8 binarization")?;

    let native_text_transform = NativeTextTransform {
        source_width: width,
        source_height: height,
        correction: identity_margin_correction(),
    };
    let hocr_text = if config.enable_ocr() && config.slow_ocr_enabled() {
        let ocr_plane = products
            .ocr
            .ok_or_else(|| anyhow!("slow OCR plan did not render an OCR surface"))?;
        let lege_pdf_read::RasterPlane::Rgb8(surface) = ocr_plane else {
            return Err(anyhow!("slow OCR target returned a non-RGB plane"));
        };
        let ocr_image = RgbImage::from_raw(surface.width, surface.height, surface.pixels.to_vec())
            .ok_or_else(|| anyhow!("OCR RGB surface was truncated"))?;
        crate::ocr::slow::perform_slow_ocr(
            &ocr_image,
            &[],
            &detections,
            width,
            height,
            &config,
            page_index,
        )
        .await?
    } else if config.enable_ocr() {
        let ocr_rgb = gray_to_rgb_image(&gray, width, height)?;
        perform_ocr(
            &binarized,
            &ocr_rgb,
            Some(&gray),
            width as usize,
            height as usize,
            &detections,
            &config,
            page_index,
        )
        .await?
    } else {
        extract_pdf_text(
            Some(&document_session),
            page_index,
            width,
            height,
            &native_text_transform,
        )
        .await?
    };

    let mut elements = Vec::new();
    for region in products.regions {
        checkpoint(&cancellation, "during planned region encoding")?;
        let Some(crop) = region.target.product.crop else {
            continue;
        };
        let lege_pdf_read::RasterPlane::Rgb8(surface) = region.plane else {
            continue;
        };
        let (encoded, format) = crate::pipeline::helper_functions::encode_region_image(
            &surface.pixels,
            surface.width,
            surface.height,
            *config.cover_format(),
            false,
            config.high_quality_output(),
            config.jpeg_compat(),
        )
        .await?;
        elements.push(crate::accumulator::ContentElement {
            x: crop.x.max(0) as f32,
            y: crop.y.max(0) as f32,
            width: crop.width as f32,
            height: crop.height as f32,
            content: crate::accumulator::ContentType::EncodedImage {
                data: Arc::from(encoded),
                pixel_width: surface.width,
                pixel_height: surface.height,
                format,
            },
        });
    }

    let force_jbig2_generic = config.text_format() == "jbig2"
        && detections
            .iter()
            .any(|detection| detection.category.force_generic_jbig2());
    let base = encode_base_layer(
        binarized,
        width as usize,
        height as usize,
        &config,
        page_index,
        force_jbig2_generic,
    )
    .await?;
    elements.insert(
        0,
        crate::accumulator::ContentElement {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
            content: base,
        },
    );

    let index = page_index.saturating_sub(page_start);
    let toc = crate::toc::capture_page(&detections, hocr_text.as_deref(), index, width, height);

    Ok(ProcessedPage {
        index,
        width,
        height,
        elements,
        hocr_text,
        toc,
    })
}

fn page_memory_budget_mb() -> usize {
    if let Some(override_mb) = std::env::var("LEGE_MEMORY_BUDGET_MB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value > 0)
    {
        return override_mb;
    }
    crate::pipeline::helper_functions::get_available_ram_gb()
        .saturating_mul(512)
        .clamp(1024, 8192)
}

fn estimated_page_memory_mb(config: &PipelineConfig, budget_mb: usize) -> u32 {
    let height = config.source_render_height() as usize;
    let width = config
        .source_render_width()
        .map(|width| width as usize)
        .unwrap_or_else(|| height.saturating_mul(3).div_ceil(2));
    // Render RGB + inference RGB + adjusted RGB + gray/binary planes + codec
    // scratch. This deliberately overestimates ordinary pages so admission
    // happens before the large allocations, not after them.
    let bytes = width.saturating_mul(height).saturating_mul(16);
    bytes.div_ceil(1024 * 1024).max(1).min(budget_mb.max(1)) as u32
}

#[allow(clippy::too_many_arguments)]
async fn run_page_owned_job(
    source: Arc<dyn PageSource>,
    config: Arc<PipelineConfig>,
    inference_handle: Option<Arc<crate::pipeline::inference::InferenceHandle>>,
    document_session: Option<Arc<lege_pdf_read::RenderSession>>,
    page_index: usize,
    page_start: usize,
    margin_analysis: Option<Arc<DocumentMarginAnalysis>>,
    detection_cache: Arc<Vec<CachedDetections>>,
    analysis_width: u32,
    cancellation: lege_pdf_read::CancellationToken,
    pdf_writer_handle: crate::pipeline::PdfWriterHandle,
    collect_hocr: bool,
    render_count: Arc<AtomicUsize>,
    detect_count: Arc<AtomicUsize>,
    encode_count: Arc<AtomicUsize>,
    progress: ProgressTracker,
    total_pages: usize,
    layout_enabled: bool,
    memory_budget: Arc<Semaphore>,
    memory_budget_mb: usize,
    hocr_spool_dir: Option<Arc<std::path::PathBuf>>,
) -> Result<PageOwnedJobOutput> {
    let estimated_mb = estimated_page_memory_mb(&config, memory_budget_mb);
    let _memory_permit = memory_budget
        .acquire_many_owned(estimated_mb)
        .await
        .map_err(|_| anyhow!("page memory admission semaphore closed"))?;
    checkpoint(&cancellation, "before render")?;
    let planned_pdf = if margin_analysis.is_none() {
        match document_session.clone() {
            Some(session) => {
                prepare_planned_pdf_page(session, config.clone(), page_index, cancellation.clone())
                    .await?
            }
            None => None,
        }
    } else {
        None
    };
    let source_page = if let Some(planned) = &planned_pdf {
        crate::pipeline::source::SourcePage {
            image: planned.analysis_image.clone(),
            original_width_pts: planned.original_width_pts,
            original_height_pts: planned.original_height_pts,
        }
    } else {
        crate::runtime_stats::track_future(
            crate::runtime_stats::Stage::Render,
            source.load_page_cancellable(page_index, cancellation.clone()),
        )
        .await?
    };
    checkpoint(&cancellation, "after render")?;

    crate::pipeline::set_standard_dimensions_once(
        source_page.image.width(),
        source_page.image.height(),
    );
    let high_res_image = Arc::new(source_page.image);
    let page_layout_enabled = config.layout_detection_enabled_for_page(page_index);
    let inference_image = if page_layout_enabled {
        let spec = config.inference_resize_spec();
        Arc::new(
            crate::pipeline::policies::build_inference_image(high_res_image.as_ref(), &spec)
                .unwrap_or_else(|_| (*high_res_image).clone()),
        )
    } else {
        Arc::clone(&high_res_image)
    };
    let rendered = RenderedPageData {
        index: page_index,
        high_res_image,
        inference_image,
        layout_detection_enabled: page_layout_enabled,
        original_width_pts: source_page.original_width_pts,
        original_height_pts: source_page.original_height_pts,
    };

    let rendered_val = render_count.fetch_add(1, Ordering::Relaxed) + 1;
    if layout_enabled {
        progress.publish_layout_progress(
            rendered_val,
            detect_count.load(Ordering::Relaxed),
            encode_count.load(Ordering::Relaxed),
            total_pages,
        );
    } else {
        progress.publish_no_layout_render_progress(rendered_val, total_pages);
    }

    checkpoint(&cancellation, "before layout inference")?;
    let inference_data =
        build_inference_future(inference_handle, rendered, detection_cache, analysis_width).await?;
    checkpoint(&cancellation, "after layout inference")?;

    let detected_val = detect_count.fetch_add(1, Ordering::Relaxed) + 1;
    if layout_enabled {
        progress.publish_layout_progress(
            render_count.load(Ordering::Relaxed),
            detected_val,
            encode_count.load(Ordering::Relaxed),
            total_pages,
        );
    }

    let processed_page = if let (Some(mut planned), Some(session)) =
        (planned_pdf, document_session.clone())
    {
        let detections = scale_detections_to_output(
            &inference_data,
            planned.output_width,
            planned.output_height,
        );
        let has_text = detections
            .iter()
            .any(|detection| crate::types::LABEL_CLASSIFIER.is_substantive_text(detection));
        planned.plan.regions = detections
            .iter()
            .filter(|detection| crate::types::LABEL_CLASSIFIER.is_image_label(detection))
            .filter(|detection| {
                !(has_text
                    && bbox_is_effectively_full_page(
                        detection.bbox,
                        planned.output_width,
                        planned.output_height,
                        0.90,
                    ))
            })
            .filter_map(|detection| {
                crate::pipeline::page_output_plan::region_target(
                    detection.bbox,
                    planned.output_width,
                    planned.output_height,
                )
            })
            .collect();
        let render_session = session.clone();
        let compiled_page = planned.page.clone();
        let plan = planned.plan.clone();
        let render_cancellation = cancellation.clone();
        let products = crate::runtime_stats::spawn_blocking_stage(
            crate::runtime_stats::Stage::Render,
            move || {
                render_session.render_output_plan(&compiled_page, &plan, Some(&render_cancellation))
            },
        )
        .await
        .map_err(|error| anyhow!("page product render task panicked: {error}"))?
        .map_err(|error| anyhow!("page product render failed: {error}"))?;
        process_planned_pdf_products(
            config,
            session,
            page_index,
            page_start,
            products,
            detections,
            cancellation.clone(),
        )
        .await?
    } else {
        process_single_page(
            config,
            document_session,
            inference_data,
            page_start,
            margin_analysis,
            cancellation.clone(),
        )
        .await?
    };
    checkpoint(&cancellation, "before writer handoff")?;

    let encoded_val = encode_count.fetch_add(1, Ordering::Relaxed) + 1;
    if layout_enabled {
        progress.publish_layout_progress(
            render_count.load(Ordering::Relaxed),
            detect_count.load(Ordering::Relaxed),
            encoded_val,
            total_pages,
        );
    } else {
        progress.publish_no_layout_progress(encoded_val, total_pages);
    }

    let hocr_page = if collect_hocr {
        if let (Some(hocr), Some(spool_dir)) = (
            processed_page
                .hocr_text
                .as_ref()
                .filter(|hocr| !hocr.trim().is_empty()),
            hocr_spool_dir,
        ) {
            let path = spool_dir.join(format!("{:08}.hocr", processed_page.index));
            let bytes = hocr.as_bytes().to_vec();
            let write_path = path.clone();
            crate::runtime_stats::spawn_blocking_stage(
                crate::runtime_stats::Stage::Writer,
                move || std::fs::write(&write_path, bytes),
            )
            .await
            .map_err(|error| anyhow!("hOCR spool task panicked: {error}"))?
            .map_err(|error| anyhow!("failed to spool hOCR {}: {error}", path.display()))?;
            Some(SpilledHocrPage {
                page_index: processed_page.index,
                width_px: processed_page.width,
                height_px: processed_page.height,
                path,
            })
        } else {
            None
        }
    } else {
        None
    };
    let toc = processed_page.toc;
    let page = crate::accumulator::Page {
        width: processed_page.width as f32,
        height: processed_page.height as f32,
        elements: processed_page.elements,
        hocr_text: processed_page.hocr_text,
        index: processed_page.index,
        binarized: None,
    };
    let output_index = page.index;
    pdf_writer_handle.send_page(page, output_index).await?;

    Ok(PageOwnedJobOutput { hocr_page, toc })
}

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

    let (config, inference_handle) =
        crate::pipeline::helper_functions::initialize_inference_or_fallback(
            config,
            progress_tracker,
            "PDF-Parallel",
        )?;

    let pipeline_config = PipelineRuntimeLimits::from_config(&config);
    init_encode_semaphore(pipeline_config.page_workers);

    // Calculate pages to process
    let document_pages = source.page_count();
    let page_start = page_range.as_ref().map(|r| r.start).unwrap_or(0);
    let page_end = page_range.as_ref().map(|r| r.end).unwrap_or(document_pages);
    // Saturate: an inverted range must not underflow into a huge page count
    // (vec![None; total_pages] in the PDF writer would abort the process).
    let total_pages = page_end.saturating_sub(page_start);
    if total_pages == 0 {
        return Err(anyhow::anyhow!(
            "No pages selected to process (document has {} pages)",
            document_pages
        ));
    }
    let document_session = source.document_session();

    // Gate the GPU resize backend by document size: cold-start cost only pays
    // back once we have enough pages to amortize device init.
    const MIN_PAGES_FOR_GPU_RESIZE: usize = 10;
    crate::resize::set_gpu_resize_enabled(total_pages >= MIN_PAGES_FOR_GPU_RESIZE);

    let page_concurrency = pipeline_config.page_workers.max(1);

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
    info_log!("  - Page-owned workers: {}", page_concurrency);
    if let Some(handle) = &inference_handle {
        info_log!("  - GPU inference sessions: {}", handle.session_count());
    }

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
            page_concurrency,
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

    // The writer channel is the only stage channel left on the PDF path.
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
    let hocr_spool = if epub_sidecar_output.is_some() {
        let parent = output_path.parent().unwrap_or_else(|| Path::new("."));
        Some(
            tempfile::Builder::new()
                .prefix(".lege-hocr-")
                .tempdir_in(parent)
                .map_err(|error| anyhow!("failed to create hOCR spool directory: {error}"))?,
        )
    } else {
        None
    };
    let hocr_spool_dir = hocr_spool
        .as_ref()
        .map(|directory| Arc::new(directory.path().to_path_buf()));
    let detection_cache_arc = Arc::new(detection_cache);
    let analysis_width = if needs_two_pass { 640 } else { 0 };
    let margin_analysis_arc = margin_analysis.map(Arc::new);
    let cancellation = lege_pdf_read::CancellationToken::new();
    let memory_budget_mb = page_memory_budget_mb();
    let memory_budget = Arc::new(Semaphore::new(memory_budget_mb));
    info_log!(
        "[PDF-Parallel] Memory admission budget: {} MiB",
        memory_budget_mb
    );
    let mut jobs = tokio::task::JoinSet::new();
    let mut next_page = page_start;
    let mut hocr_pages = Vec::new();
    let mut toc_candidates: Vec<crate::toc::TocCandidate> = Vec::new();
    let mut toc_stats: Vec<crate::toc::PageTextStats> = Vec::new();

    while next_page < page_end || !jobs.is_empty() {
        while next_page < page_end && jobs.len() < page_concurrency {
            let page_index = next_page;
            next_page += 1;
            jobs.spawn(run_page_owned_job(
                source.clone(),
                config.clone(),
                inference_handle.clone(),
                document_session.clone(),
                page_index,
                page_start,
                margin_analysis_arc.clone(),
                detection_cache_arc.clone(),
                analysis_width,
                cancellation.clone(),
                pdf_writer_handle.clone(),
                epub_sidecar_output.is_some(),
                render_count.clone(),
                detect_count.clone(),
                encode_count.clone(),
                progress_tracker.clone(),
                total_pages,
                layout_enabled,
                memory_budget.clone(),
                memory_budget_mb,
                hocr_spool_dir.clone(),
            ));
        }

        tokio::select! {
            biased;
            signal = shutdown_rx.recv() => {
                if let Ok(signal) = signal {
                    cancellation.cancel();
                    jobs.abort_all();
                    pdf_writer_task.abort();
                    while jobs.join_next().await.is_some() {}
                    return Err(anyhow!(
                        "Processing cancelled: {}",
                        signal.message.unwrap_or_else(|| "User requested cancellation".to_string())
                    ));
                }
            }
            result = jobs.join_next(), if !jobs.is_empty() => {
                let output = match result {
                    Some(Ok(Ok(output))) => output,
                    Some(Ok(Err(error))) => {
                        cancellation.cancel();
                        jobs.abort_all();
                        pdf_writer_task.abort();
                        while jobs.join_next().await.is_some() {}
                        return Err(error);
                    }
                    Some(Err(error)) => {
                        cancellation.cancel();
                        jobs.abort_all();
                        pdf_writer_task.abort();
                        while jobs.join_next().await.is_some() {}
                        return Err(anyhow!("page job panicked: {error}"));
                    }
                    None => {
                        cancellation.cancel();
                        pdf_writer_task.abort();
                        return Err(anyhow!("page job set ended unexpectedly"));
                    }
                };
                if let Some(hocr_page) = output.hocr_page {
                    hocr_pages.push(hocr_page);
                }
                toc_candidates.extend(output.toc.candidates);
                toc_stats.extend(output.toc.stats);
            }
        }
    }
    info_log!("[PDF-Parallel] Page-owned jobs complete");

    // Await extraction so writer finalization cannot race bookmark delivery.
    if let Some(session) = document_session {
        let bookmarks =
            crate::runtime_stats::spawn_blocking(move || lege_pdf_read::extract_outline(&session))
                .await
                .map_err(|error| anyhow!("Outline extraction task panicked: {error}"))?;
        if !bookmarks.is_empty() {
            let source_to_output = (page_start..page_end)
                .enumerate()
                .map(|(output, source)| (source, output))
                .collect();
            pdf_writer_handle
                .send_bookmarks(bookmarks, source_to_output)
                .await?;
        }
    }

    // The synthesized outline is offered, never imposed: the writer uses it only
    // when the source document had no outline that survived remapping.
    if !toc_candidates.is_empty() {
        let total_pages = page_end.saturating_sub(page_start);
        let outline = crate::toc::build_outline(toc_candidates, &toc_stats, total_pages);
        if !outline.is_empty() {
            info_log!(
                "[PDF-Parallel] Synthesized a {}-entry table of contents",
                outline.len()
            );
            pdf_writer_handle.send_synthetic_outline(outline).await?;
        }
    }

    info_log!("[PDF-Parallel] Finalizing PDF...");
    pdf_writer_handle.finalize().await?;
    use crate::pipeline::helper_functions::await_stage_or_cancel;
    await_stage_or_cancel(&mut pdf_writer_task, &mut shutdown_rx, "PDF writer", &[]).await?;
    info_log!("[PDF-Parallel] PDF writer complete");

    if let Some(epub_path) = epub_sidecar_output {
        if !hocr_pages.is_empty() {
            hocr_pages.sort_by_key(|page| page.page_index);
            let hocr_pages = hocr_pages
                .into_iter()
                .map(|page| {
                    let hocr = std::fs::read_to_string(&page.path).map_err(|error| {
                        anyhow!(
                            "failed to read spooled hOCR {}: {error}",
                            page.path.display()
                        )
                    })?;
                    Ok(crate::pipeline::epub_pipeline::HocrPage {
                        page_index: page.page_index,
                        width_px: page.width_px,
                        height_px: page.height_px,
                        hocr,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let title = epub_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Document")
                .to_string();
            info_log!(
                "[PDF-Parallel] Assembling EPUB sidecar from existing OCR: {}",
                epub_path.display()
            );
            crate::runtime_stats::spawn_blocking_stage(
                crate::runtime_stats::Stage::Writer,
                move || {
                    crate::pipeline::epub_pipeline::build_epub_from_hocr_pages(
                        &hocr_pages,
                        &title,
                        &epub_path,
                    )
                },
            )
            .await
            .map_err(|e| anyhow!("EPUB sidecar task panicked: {}", e))??;
        } else {
            warn_log!("[PDF-Parallel] EPUB sidecar requested, but no OCR text was available");
        }
    }

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
    let source: Arc<dyn PageSource> = Arc::new(PdfPageSource::new(pdf_bytes, config.clone())?);
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

#[cfg(test)]
mod phase4_ocr_baseline_tests {
    use super::*;
    use image::{Rgb, RgbImage};

    fn slow_ocr_config() -> Arc<PipelineConfig> {
        let mut config = PipelineConfig::new().expect("config");
        config.set_target_height(600).expect("target height");
        config
            .set_high_res_render_height(1200)
            .expect("render height");
        config.set_enable_cover_page(false);
        config.set_no_cover_page(true);
        config.set_enable_layout_detection(false);
        config.set_enable_ocr(true);
        config.set_slow_ocr(true);
        config.set_text_format("ccitt4").expect("format");
        let mut bin = config.binarization().clone();
        bin.use_fixed_threshold = true;
        bin.fixed_threshold = 180;
        config.set_binarization(bin);
        Arc::new(config)
    }

    fn synthetic_ocr_page(width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            let line = y % 48;
            if (10..18).contains(&line) && x % 120 < 82 {
                Rgb([24, 24, 24])
            } else {
                Rgb([244, 242, 235])
            }
        })
    }

    #[test]
    fn slow_ocr_baseline_keeps_high_res_surface_but_downscales_encode_surface() {
        let config = slow_ocr_config();
        let source = Arc::new(synthetic_ocr_page(800, 1200));
        let rendered = RenderedPageData {
            index: 1,
            high_res_image: source.clone(),
            inference_image: source.clone(),
            layout_detection_enabled: false,
            original_width_pts: 400.0,
            original_height_pts: 600.0,
        };
        let inference_result = InferenceResult {
            index: 1,
            high_res_image: source.clone(),
            inference_image: source,
            detections: Vec::new(),
            text_layer: None,
            detections_are_page_space: true,
            original_width_pts: 400.0,
            original_height_pts: 600.0,
            has_no_detections: true,
        };
        let output = process_page_cpu_work(PageProcessingInput {
            rendered,
            inference_result,
            page_index: 1,
            config,
            margin_analysis: None,
            cancellation: lege_pdf_read::CancellationToken::new(),
        })
        .expect("OCR-shaped CPU page work");

        assert_eq!((output.width, output.height), (400, 600));
        let ocr = output.ocr_image.expect("high resolution OCR surface");
        assert_eq!(ocr.dimensions(), (800, 1200));
        assert_eq!(output.binarized.len(), 400 * 600);
    }

    #[test]
    fn cancelled_ocr_baseline_stops_before_cpu_allocations() {
        let config = slow_ocr_config();
        let source = Arc::new(synthetic_ocr_page(800, 1200));
        let rendered = RenderedPageData {
            index: 1,
            high_res_image: source.clone(),
            inference_image: source.clone(),
            layout_detection_enabled: false,
            original_width_pts: 400.0,
            original_height_pts: 600.0,
        };
        let inference_result = InferenceResult {
            index: 1,
            high_res_image: source.clone(),
            inference_image: source,
            detections: Vec::new(),
            text_layer: None,
            detections_are_page_space: true,
            original_width_pts: 400.0,
            original_height_pts: 600.0,
            has_no_detections: true,
        };
        let cancellation = lege_pdf_read::CancellationToken::new();
        cancellation.cancel();
        let started = std::time::Instant::now();
        let result = process_page_cpu_work(PageProcessingInput {
            rendered,
            inference_result,
            page_index: 1,
            config,
            margin_analysis: None,
            cancellation,
        });
        assert!(result.is_err());
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ocr_sized_pages_obey_mib_admission_before_render() {
        let config = slow_ocr_config();
        let permits = estimated_page_memory_mb(&config, 1024);
        assert!(permits > 1);
        let budget = Arc::new(Semaphore::new(permits as usize));
        let first = budget
            .clone()
            .acquire_many_owned(permits)
            .await
            .expect("first page admitted");
        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            budget.clone().acquire_many_owned(permits),
        )
        .await;
        assert!(blocked.is_err(), "second OCR-sized page must wait");
        drop(first);
        let _second = budget
            .acquire_many_owned(permits)
            .await
            .expect("page admitted after previous handoff");
    }
}
