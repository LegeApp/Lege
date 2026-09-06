// pdf_tokio_pipeline.rs
use crate::encoding::glyphfont::GlyphFontSession;
use crate::margin::DocumentMarginAnalysis;
use crate::pagerender::NativeTextWord;
use crate::pipeline::config::{
    ImageRegionDitherMode, InferenceResult, PipelineConfig, RenderedPageData,
};
use crate::pipeline::helper_functions::{
    build_hocr_from_positioned_words, image_detection_overlaps_substantive_text,
    init_encode_semaphore, merge_overlapping_image_detections, rounded_clamped_bbox,
    should_keep_image_overlay, should_preserve_cover_page, spawn_pdf_writer_actor,
};
use crate::pipeline::margin_pipeline::{
    CachedDetections, adjust_page_with_margin_analysis, cached_inference_result,
    perform_document_margin_analysis,
};
use crate::pipeline::page_analysis::{
    compute_pixel_bounds_for_margin, is_visually_blank_page, maybe_apply_full_page_detection,
    should_force_blank_page_threshold,
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
use futures_util::future::BoxFuture;
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
    /// Quarter turns clockwise that would make the page's text upright, 0 when
    /// it is upright already or when nothing measured it.
    pub quarter_turns: u8,
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
    glyph_session: Option<Arc<GlyphFontSession>>,
) -> Result<ProcessedPage> {
    checkpoint(&cancellation, "before page processing")?;
    let page_index = inference_data.inference_result.index;
    let local_index = page_index.saturating_sub(page_index_offset);
    // A page whose ink is already one bit keeps it as JBIG2 (see
    // `page_ink_is_bilevel`); truetyping traces only rendered scans.
    let glyph_session =
        glyph_session.filter(|_| !page_ink_is_bilevel(document_session.as_ref(), page_index));

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
    if let Some((encoded_data, format, cover_px_w, cover_px_h)) = cover_encoded_data {
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
                pixel_width: cover_px_w,
                pixel_height: cover_px_h,
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
                    pixel_width: region_result.encoded_w,
                    pixel_height: region_result.encoded_h,
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
    // How the page's text sits, so the recognizer reads it upright. In
    // "jpeg" mode `binarized` holds the OCR luma, which thresholds well
    // enough for this.
    let page_frame = page_frame_for_ocr(&config, &binarized, width, height);
    let quarter_turns = page_frame.map_or(0, |frame| frame.turns);
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
            page_frame,
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
            page_frame,
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

    // If any region on this page is Abandon and we're using JBIG2 Symbol mode,
    // force the base layer to Generic to avoid Symbol-mode corruption of noisy pixels.
    let force_jbig2_generic = matches!(
        config.text_format(),
        "jbig2" | crate::pipeline::config::TRUETYPING
    ) && adjusted_detections
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
            quarter_turns,
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
            glyph_session,
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
                    quarter_turns,
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
            glyph_session,
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
            glyph_session,
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
        quarter_turns,
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
    /// Integer top-left used for extraction, masking, and PDF placement (must stay in sync).
    region_x: u32,
    region_y: u32,
    region_w: u32,
    region_h: u32,
    /// Pixel size of the encoded stream. Equal to `region_w`/`region_h` for
    /// every codec except a JP2 display encode, which emits the box the region
    /// is drawn in. Placement on the page still uses `region_w`/`region_h`.
    encoded_w: u32,
    encoded_h: u32,
    should_dither: bool,
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
    cover_encoded_data: Option<(Vec<u8>, String, u32, u32)>,
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

    // False image detections over substantive text must not become raster
    // overlays. Besides covering a cleaned MRC background, they create the
    // conspicuous "half a text column in color" seam in ordinary bilevel mode.
    // Line art (diagrams, music, schematics) is also dropped so it is
    // binarized with the page or carried by the MRC JBIG2 mask. The
    // variance/orientation classifier from bpg-rs keeps maps, engravings,
    // and photographs as overlays.
    if config.text_format() != "jpeg" {
        let all_detections = adjusted_detections.clone();
        adjusted_detections.retain(|det| {
            if !classifier.is_image_label(det) {
                return true;
            }
            if !should_keep_image_overlay(
                det,
                adjusted_image.as_raw(),
                width,
                height,
                &all_detections,
                classifier,
            ) {
                crate::bbox_trace!(
                    "PAGE {}: dropping image region (text overlap or line art) ({:.0},{:.0},{:.0},{:.0})",
                    page_index,
                    det.bbox[0],
                    det.bbox[1],
                    det.bbox[2],
                    det.bbox[3]
                );
                return false;
            }
            true
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
        // The page raster is already at the device's pixel height, so the
        // cover's display box is its own size: the floor is verified where the
        // reader sees it and nothing is resampled away.
        // A JP2 cover with the high-res raster retained is encoded from it, so
        // the downscale into the device box happens inside the rate loop.
        let hi_cover = ocr_image.as_ref().filter(|hi| {
            crate::pipeline::helper_functions::region_emits_jp2(
                *config.cover_format(),
                true,
                config.jpeg_compat(),
            ) && crate::encoding::Jp2DisplaySettings::resamples_into(
                width as u32,
                height as u32,
                hi.width(),
                hi.height(),
            )
        });
        let (cover_src, cover_w, cover_h) = match hi_cover {
            Some(hi) => (hi.as_raw().as_slice(), hi.width(), hi.height()),
            None => (
                adjusted_image.as_raw().as_slice(),
                width as u32,
                height as u32,
            ),
        };
        let cover_result = encode_region_image_sync(
            cover_src,
            cover_w,
            cover_h,
            *config.cover_format(),
            true,
            config.high_quality_output(),
            config.jpeg_compat(),
            Some((width as u32, height as u32)),
            // A cover is the whole page.
            crate::pipeline::quality_policy::RegionSize::Large,
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
        let (mut encoded_w, mut encoded_h) = (region_w, region_h);

        // Slow OCR kept the page above device resolution. A JP2 region is then
        // cropped from that raster and jp2lam downscales it into the region's
        // device box in linear light inside the rate loop, instead of encoding
        // the Lanczos-resized device crop. Only when the crop is large enough
        // for jp2lam to resample at all (its 1.25x threshold), and only for the
        // two modes whose region ends up JP2; every other codec keeps the
        // device crop. Same luma/RGB layout as `region_data`.
        let hi_region: Option<(Vec<u8>, u32, u32)> = ocr_image
            .as_ref()
            .filter(|_| {
                image_region_mode == ImageRegionDitherMode::GrayJp2
                    || (!should_dither
                        && crate::pipeline::helper_functions::region_emits_jp2(
                            *config.cover_format(),
                            is_cover_page,
                            config.jpeg_compat(),
                        ))
            })
            .and_then(|hi| {
                let sx = hi.width() as f32 / width as f32;
                let sy = hi.height() as f32 / height as f32;
                let hi_bbox = [
                    exact_bbox[0] * sx,
                    exact_bbox[1] * sy,
                    exact_bbox[2] * sx,
                    exact_bbox[3] * sy,
                ];
                let (data, w, h) = crate::color::color_processing::process_image_region(
                    hi.as_raw(),
                    hi.width(),
                    hi.height(),
                    hi_bbox,
                    image_region_mode,
                    config.text_format(),
                    false,
                )
                .ok()?;
                crate::encoding::Jp2DisplaySettings::resamples_into(region_w, region_h, w, h)
                    .then_some((data, w, h))
            });

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

        if should_dither && !is_cover_page {
            let grayscale_data: Vec<u8> = region_data.chunks(3).map(|rgb| rgb[0]).collect();

            if image_region_mode == ImageRegionDitherMode::GrayJp2 {
                // Grayscale JP2 overlay: skip bilevel dithering, encode directly as jp2-gray.
                use crate::encoding::{
                    EncodingManager, EncodingResult, EncodingSettings,
                    ImageBuffer as LegeImageBuffer,
                };
                let hi_gray: Option<(Vec<u8>, u32, u32)> = hi_region
                    .as_ref()
                    .map(|(d, w, h)| (d.chunks(3).map(|px| px[0]).collect(), *w, *h));
                let (gray, gray_w, gray_h): (&[u8], u32, u32) = match &hi_gray {
                    Some((d, w, h)) => (d, *w, *h),
                    None => (&grayscale_data, region_w, region_h),
                };
                let buffer = LegeImageBuffer {
                    data: gray,
                    width: gray_w,
                    height: gray_h,
                    channels: 1,
                };
                let q =
                    crate::pipeline::quality_policy::region_gray_jp2(config.high_quality_output());
                // Grayscale overlay on a grayscale panel: verify the floor as
                // e-ink luminance, in the box the region is drawn in.
                let settings = EncodingSettings::Jp2Display(crate::encoding::Jp2DisplaySettings {
                    max_width: region_w,
                    max_height: region_h,
                    floor: crate::pipeline::quality_policy::region_gray_jp2_floor(
                        config.high_quality_output(),
                    ),
                    fallback_quality: q,
                });
                if let Ok(EncodingResult::Standard(data)) =
                    EncodingManager::encode(&buffer, &settings)
                {
                    (encoded_w, encoded_h) =
                        crate::pipeline::helper_functions::encoded_region_dimensions(
                            &data, "jp2-gray", region_w, region_h,
                        );
                    encoded_data = Some((data, "jp2-gray".to_string()));
                }
            } else if image_region_mode == ImageRegionDitherMode::Halftone {
                // Halftone overlay: grayscale → jbig2halftone.rs (halftone region segments)
                // Invert grayscale so that bright→low pattern index (few dots) and
                // dark→high pattern index (many dots).  Combined with Decode [1, 0]
                // in the PDF this yields black dots on a white default — traditional
                // halftone polarity whose white background blends with the base layer.
                // Near-white rolls off smoothly to paper white rather than being
                // clamped, so photographic highlights keep their gradient.
                let inverted_gray: Vec<u8> = grayscale_data
                    .iter()
                    .map(|&g| crate::color::color_processing::halftone_ink_from_gray(g))
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
                // The overlay is a bilevel raster even when the base layer is
                // not (`truetyping`), so name the codec, not the text format.
                let overlay_fmt = match config.text_format() {
                    "ccitt4" => "ccitt4",
                    _ => "jbig2",
                }
                .to_string();
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
                // The box the region is drawn in is its device-resolution
                // crop size; the source is that crop, or the high-res crop
                // when jp2lam gets to do the downscale.
                let (src, src_w, src_h): (&[u8], u32, u32) = match &hi_region {
                    Some((d, w, h)) => (d, *w, *h),
                    None => (&region_data, region_w, region_h),
                };
                let (data, fmt, enc_w, enc_h) = encode_region_image_sync(
                    src,
                    src_w,
                    src_h,
                    *config.cover_format(),
                    is_cover_page,
                    config.high_quality_output(),
                    config.jpeg_compat(),
                    Some((region_w, region_h)),
                    // Measured on the page the reader sees, not the source crop.
                    crate::pipeline::quality_policy::RegionSize::of(
                        region_w,
                        region_h,
                        width as u32,
                        height as u32,
                    ),
                )
                .map_err(|e| anyhow!("Could not encode image region: {}", e))?;
                encoded_w = enc_w;
                encoded_h = enc_h;
                encoded_data = Some((data, fmt));
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
            region_x: bbox_x1,
            region_y: bbox_y1,
            region_w,
            region_h,
            encoded_w,
            encoded_h,
            should_dither,
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
    display_box: Option<(u32, u32)>,
    region_size: crate::pipeline::quality_policy::RegionSize,
) -> Result<(Vec<u8>, String, u32, u32)> {
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
        display_box,
        region_size,
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

    let data = match encoding_result {
        EncodingResult::Standard(data) => data,
        EncodingResult::Jbig2WithGlobals { page_data, .. } => {
            if fmt_str != "jbig2" {
                return Err(anyhow!(
                    "Encoder returned JBIG2 data but format tag is '{}'",
                    fmt_str
                ));
            }
            // Region overlays do not carry a separate global stream in this path.
            // Return page data only to avoid producing invalid concatenated JBIG2 bytes.
            page_data
        }
    };
    let (pixel_width, pixel_height) = crate::pipeline::helper_functions::encoded_region_dimensions(
        &data, &fmt_str, width, height,
    );
    Ok((data, fmt_str, pixel_width, pixel_height))
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
    frame: Option<crate::ocr::orient::PageFrame>,
) -> Result<Option<String>> {
    // Note: This function is only called when config.enable_ocr() is true

    // PP-OCR (paddle) backend: DBNet needs natural grayscale, not the 1bpp mask,
    // and does its own text-line detection. Run it on the page raster directly.
    #[cfg(lege_paddle_ocr)]
    {
        let _ = (binarized, width, height, detections);
        let result = crate::ocr::fast::perform_page_rgb_ocr(
            page_rgb,
            cleaned_gray,
            config.ocr_language(),
            config.invert_input(),
            frame,
        )
        .await;
        return match result {
            Ok(text) => Ok(Some(text)),
            Err(e) => Err(anyhow!("Page {}: PaddleOCR failed: {e:#}", page_index)),
        };
    }

    #[cfg(not(lege_paddle_ocr))]
    {
        // The legacy mask-based path recognizes the page as scanned.
        let _ = (page_rgb, cleaned_gray, frame);
        perform_ocr_binarized(binarized, width, height, detections, config, page_index).await
    }
}

/// The render height at which a document that is already black and white
/// comes out pixel for pixel, sampled over the range. A book keeps its
/// covers and its plates in continuous tone, so the sample decides by
/// majority; `None` when fewer than half of the sampled pages carry a
/// bilevel ink layer, which leaves the render height as it was.
fn bilevel_source_height(
    session: &Arc<lege_pdf_read::RenderSession>,
    page_start: usize,
    page_end: usize,
) -> Option<u32> {
    const SAMPLES: usize = 8;
    let total = page_end.saturating_sub(page_start);
    if total == 0 {
        return None;
    }
    let step = total.div_ceil(SAMPLES).max(1);
    let (mut sampled, mut bilevel, mut natural) = (0usize, 0usize, 0u32);
    for page in (page_start..page_end).step_by(step).take(SAMPLES) {
        let Ok(compiled) = session.compile(page as u32) else {
            continue;
        };
        sampled += 1;
        if let Some(height) = compiled.bilevel_raster_height() {
            bilevel += 1;
            natural = natural.max(height);
        }
    }
    (natural > 0 && bilevel * 2 >= sampled).then_some(natural)
}

/// Whether a page's ink already comes from a one-bit source (a full-page
/// bilevel image or a bilevel soft mask, see
/// [`lege_pdf_read::CompiledDocumentPage::bilevel_raster_height`]). Such a
/// page is original text: truetyping leaves it as JBIG2 instead of tracing
/// it, and a source mask passes through untouched.
fn page_ink_is_bilevel(
    session: Option<&Arc<lege_pdf_read::RenderSession>>,
    page_index: usize,
) -> bool {
    session
        .and_then(|session| session.compile(page_index as u32).ok())
        .is_some_and(|page| page.bilevel_raster_height().is_some())
}

/// The orientation of a page's text for the recognizer, measured from its
/// binarized raster when OCR is on. `None` leaves the page as it is.
fn page_frame_for_ocr(
    config: &PipelineConfig,
    binarized: &[u8],
    width: usize,
    height: usize,
) -> Option<crate::ocr::orient::PageFrame> {
    if !config.enable_ocr() || binarized.len() < width * height {
        return None;
    }
    let frame = crate::encoding::straighten::detect_frame_of_pixels(
        binarized,
        width,
        height,
        crate::encoding::straighten::analysis_dpi(height),
    );
    (!frame.is_identity()).then_some(frame)
}

/// Tesseract/WinOCR fast path: region- or tile-based OCR over the 1bpp mask.
#[cfg(not(lege_paddle_ocr))]
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
    glyph_session: Option<Arc<GlyphFontSession>>,
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
    // Glyph-font output keeps the JP2 background and draws the ink layer as
    // text instead of a JBIG2 stencil. A truetyping page without a session
    // is one whose ink was already one bit, and it keeps the stencil.
    let glyph_session =
        glyph_session.filter(|_| config.text_format() == crate::pipeline::config::TRUETYPING);
    crate::runtime_stats::spawn_blocking_stage(crate::runtime_stats::Stage::Encode, move || {
        let mask_content = if let Some(session) = glyph_session {
            let runs = session.process_page_pixels(
                &binarized,
                width,
                height,
                crate::encoding::straighten::analysis_dpi(height),
            )?;
            crate::accumulator::ContentType::GlyphText {
                runs: Arc::new(runs),
                pixel_width: width as u32,
                pixel_height: height as u32,
            }
        } else {
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
            crate::accumulator::ContentType::Jbig2Mask {
                page_data: Arc::from(page_data),
                global_data: Arc::from(global_data),
                pixel_width: width as u32,
                pixel_height: height as u32,
                paint_one: false,
            }
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

/// Encode only the cleaned continuous-tone background while passing the
/// qualifying source JBIG2 `/SMask` bytes and globals through unchanged.
/// `working_coverage` affects background cleanup only; it is never encoded as
/// the foreground text plane.
async fn encode_preserved_mrc_base_layer(
    cleaned_gray: Vec<u8>,
    working_coverage: Vec<u8>,
    width: usize,
    height: usize,
    config: &Arc<PipelineConfig>,
    page_index: usize,
    mask: lege_pdf_read::PreservedJbig2Smask,
) -> Result<(
    crate::accumulator::ContentType,
    crate::accumulator::ContentType,
)> {
    let bg_quality = config.mrc_bg_quality();
    let image_mode = if config.keep_original_images() {
        ImageRegionDitherMode::None
    } else {
        config.image_region_dither_mode()
    };
    let text_format = config.text_format().to_string();
    let cover_format = *config.cover_format();
    let high_quality = config.high_quality_output();
    let jpeg_compat = config.jpeg_compat();
    let background_subsample = config.glyph_background_subsample();
    crate::runtime_stats::spawn_blocking_stage(crate::runtime_stats::Stage::Encode, move || {
        use crate::encoding::{
            EncodingManager, EncodingResult, EncodingSettings, ImageBuffer as LegeImageBuffer,
            Jbig2Settings,
        };

        // Preserve all source pixels outside the native text mask at the
        // requested resolution: downsampling below it would make the
        // selected dither/original policy less than global. Glyph-font
        // output renders above that height for its own sake only, and the
        // background comes back down to it here.
        let (bg, ow, oh) = crate::clean_gray::mrc_background_with_coverage(
            &cleaned_gray,
            &working_coverage,
            width,
            height,
            background_subsample,
        );
        let ow = ow as u32;
        let oh = oh as u32;
        let rgb = bg
            .iter()
            .flat_map(|&gray| [gray, gray, gray])
            .collect::<Vec<_>>();

        let background = match image_mode {
            ImageRegionDitherMode::None => {
                // Preservation path: the background is already subsampled
                // beneath a full-resolution source mask, so it stays on the
                // legacy full-resolution encode (`None` display box).
                let (data, format, ow, oh) = encode_region_image_sync(
                    &rgb,
                    ow,
                    oh,
                    cover_format,
                    false,
                    high_quality,
                    jpeg_compat,
                    None,
                    // The MRC background spans the whole page.
                    crate::pipeline::quality_policy::RegionSize::Large,
                )
                .map_err(|error| anyhow!("preserved-mask original background encode: {error}"))?;
                crate::accumulator::ContentType::EncodedImage {
                    data: Arc::from(data),
                    pixel_width: ow,
                    pixel_height: oh,
                    format,
                }
            }
            ImageRegionDitherMode::GrayJp2 => {
                let jp2 = crate::encoding::jp2::encode_gray_document(&bg, ow, oh, bg_quality)
                    .map_err(|error| anyhow!("preserved-mask JP2 background encode: {error}"))?;
                crate::accumulator::ContentType::EncodedImage {
                    data: Arc::from(jp2),
                    pixel_width: ow,
                    pixel_height: oh,
                    format: "jp2-gray".to_string(),
                }
            }
            ImageRegionDitherMode::Halftone => {
                let inverted = bg
                    .iter()
                    .map(|&gray| crate::color::color_processing::halftone_ink_from_gray(gray))
                    .collect::<Vec<_>>();
                let (global_data, page_data) =
                    crate::encoding::encode_halftone_region_grayscale(&inverted, ow, oh)
                        .map_err(|error| anyhow!("preserved-mask halftone encode: {error}"))?;
                crate::accumulator::ContentType::Jbig2ImageWithGlobals {
                    page_data: Arc::from(page_data),
                    global_data: Arc::from(global_data),
                    pixel_width: ow,
                    pixel_height: oh,
                }
            }
            ImageRegionDitherMode::Stucki | ImageRegionDitherMode::Ccitt4ClusteredDot4x4 => {
                let (dithered, dithered_width, dithered_height) =
                    crate::color::color_processing::process_image_region(
                        &rgb,
                        ow,
                        oh,
                        [0.0, 0.0, ow as f32, oh as f32],
                        image_mode,
                        &text_format,
                        false,
                    )?;
                let bilevel = dithered
                    .chunks_exact(3)
                    .map(|pixel| pixel[0])
                    .collect::<Vec<_>>();
                let buffer = LegeImageBuffer {
                    data: &bilevel,
                    width: dithered_width,
                    height: dithered_height,
                    channels: 1,
                };
                let (settings, format) = if text_format == "ccitt4" {
                    (EncodingSettings::Ccitt4, "ccitt")
                } else {
                    (
                        EncodingSettings::Jbig2(Jbig2Settings {
                            pdf_fragment_mode: true,
                            mode: Jbig2Mode::Generic,
                            use_jbig2_halftone_segments: false,
                        }),
                        "jbig2",
                    )
                };
                match EncodingManager::encode(&buffer, &settings)
                    .map_err(|error| anyhow!("preserved-mask dither encode: {error}"))?
                {
                    EncodingResult::Standard(data) => {
                        crate::accumulator::ContentType::EncodedImage {
                            data: Arc::from(data),
                            pixel_width: dithered_width,
                            pixel_height: dithered_height,
                            format: format.to_string(),
                        }
                    }
                    EncodingResult::Jbig2WithGlobals {
                        page_data,
                        global_data,
                    } => crate::accumulator::ContentType::Jbig2ImageWithGlobals {
                        page_data: Arc::from(page_data),
                        global_data: Arc::from(global_data),
                        pixel_width: dithered_width,
                        pixel_height: dithered_height,
                    },
                }
            }
        };
        let foreground = crate::accumulator::ContentType::Jbig2Mask {
            page_data: mask.page_data,
            global_data: mask.global_data,
            pixel_width: mask.native_width,
            pixel_height: mask.native_height,
            paint_one: matches!(
                mask.stencil_polarity,
                lege_pdf_read::Jbig2StencilPolarity::PaintOne
            ),
        };
        crate::bbox_trace!(
            "PAGE {} preserved MRC mode={:?} bg={}x{} bytes={} native_mask={}x{} bytes={}",
            page_index,
            image_mode,
            ow,
            oh,
            background.as_bytes().len(),
            mask.native_width,
            mask.native_height,
            foreground.as_bytes().len()
        );
        Ok((background, foreground))
    })
    .await
    .map_err(|error| anyhow!("preserved MRC encode task panicked: {error}"))?
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
    glyph_session: Option<Arc<GlyphFontSession>>,
) -> Result<crate::accumulator::ContentType> {
    use crate::accumulator::ContentType;
    use crate::encoding::{
        EncodingManager, EncodingResult, EncodingSettings, ImageBuffer as LegeImageBuffer,
        Jbig2Settings, JpegSettings,
    };

    let encoding_start = std::time::Instant::now();

    let text_format = match (config.text_format(), glyph_session) {
        (crate::pipeline::config::TRUETYPING, Some(session)) => {
            return encode_glyph_text(binarized, width, height, page_index, Some(session)).await;
        }
        // A truetyping page whose ink was already one bit (see
        // `page_ink_is_bilevel`) is encoded as the JBIG2 format would.
        (crate::pipeline::config::TRUETYPING, None) => "jbig2",
        (format, _) => format,
    };

    // Determine encoding settings
    let jbig2_mode = if force_jbig2_generic {
        crate::encoding::Jbig2Mode::Generic
    } else {
        config.jbig2_mode()
    };
    let (encoding_settings, base_format) = match text_format {
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
    let text_format = text_format.to_string();
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

/// Glyph-font base layer: segment the binarized page into components, match
/// them against the document-wide glyph dictionary, and hand back the page's
/// glyph placements instead of an encoded raster.
async fn encode_glyph_text(
    binarized: Vec<u8>,
    width: usize,
    height: usize,
    page_index: usize,
    glyph_session: Option<Arc<GlyphFontSession>>,
) -> Result<crate::accumulator::ContentType> {
    let session = glyph_session
        .ok_or_else(|| anyhow!("glyphfont text format selected without a glyph session"))?;
    let start = std::time::Instant::now();
    let encode_sem = crate::pipeline::helper_functions::get_encode_semaphore();
    let permit = match encode_sem {
        Some(sem) => Some(sem.acquire_owned().await.ok()),
        None => None,
    };
    let dpi = crate::encoding::straighten::analysis_dpi(height);
    let runs = crate::runtime_stats::spawn_blocking_stage(
        crate::runtime_stats::Stage::Encode,
        move || session.process_page_pixels(&binarized, width, height, dpi),
    )
    .await
    .map_err(|e| anyhow!("Glyph extraction task panicked: {}", e))??;
    drop(permit);
    crate::perf_log!(
        start,
        "[PROFILING] Page {} glyph extraction completed ({} glyphs)",
        page_index + 1,
        runs.glyph_count
    );
    Ok(crate::accumulator::ContentType::GlyphText {
        runs: Arc::new(runs),
        pixel_width: width as u32,
        pixel_height: height as u32,
    })
}

/// Align a page's hOCR words with its glyph placements and record the votes
/// in the document's glyph dictionary. hOCR and the glyph raster share the
/// page's pixel space; the element's placement covers any scale between.
fn record_glyph_text(
    session: &GlyphFontSession,
    elements: &mut [crate::accumulator::ContentElement],
    hocr: &str,
) -> Result<()> {
    let Some(el) = elements.iter_mut().find(|el| {
        matches!(
            el.content,
            crate::accumulator::ContentType::GlyphText { .. }
        )
    }) else {
        return Ok(());
    };
    let (el_x, el_y, el_w, el_h) = (el.x, el.y, el.width, el.height);
    let crate::accumulator::ContentType::GlyphText {
        runs,
        pixel_width,
        pixel_height,
    } = &mut el.content
    else {
        return Ok(());
    };
    let (pixel_width, pixel_height) = (*pixel_width, *pixel_height);
    if runs.is_empty() || el_w <= 0.0 || el_h <= 0.0 {
        return Ok(());
    }
    let sx = pixel_width as f32 / el_w;
    let sy = pixel_height as f32 / el_h;
    let lines = match crate::hocr::parse_hocr(hocr) {
        Ok(lines) => lines,
        Err(e) => {
            warn_log!("Glyph font: hOCR for text alignment did not parse: {}", e);
            return Ok(());
        }
    };
    let words: Vec<crate::encoding::glyphfont::TextWord> = lines
        .iter()
        .flat_map(|line| line.words.iter())
        .filter(|w| !w.text.trim().is_empty())
        .map(|w| crate::encoding::glyphfont::TextWord {
            text: w.text.clone(),
            x0: (w.x - el_x) * sx,
            y0: (w.y - el_y) * sy,
            x1: (w.x + w.width - el_x) * sx,
            y1: (w.y + w.height - el_y) * sy,
        })
        .collect();
    if words.is_empty() {
        return Ok(());
    }
    // Recording may give occurrences their own CIDs (one outline, two
    // characters), so the page's runs are updated in place before it is
    // handed to the writer.
    session.record_text(Arc::make_mut(runs), &words)
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
    glyph_session: Option<Arc<GlyphFontSession>>,
) -> Result<crate::accumulator::ContentType> {
    use crate::accumulator::ContentType;
    use crate::encoding::{
        EncodingManager, EncodingResult, EncodingSettings, ImageBuffer as LegeImageBuffer,
        Jbig2Settings,
    };

    let encoding_start = std::time::Instant::now();

    if config.text_format() == crate::pipeline::config::TRUETYPING {
        let session = glyph_session
            .ok_or_else(|| anyhow!("glyphfont text format selected without a glyph session"))?;
        let encode_sem = crate::pipeline::helper_functions::get_encode_semaphore();
        let permit = match encode_sem {
            Some(sem) => Some(sem.acquire_owned().await.ok()),
            None => None,
        };
        let dpi = crate::encoding::straighten::analysis_dpi(height);
        let runs = crate::runtime_stats::spawn_blocking_stage(
            crate::runtime_stats::Stage::Encode,
            move || {
                crate::color::binarization::binarize_image_raw_with(
                    rgb_image.as_raw(),
                    width,
                    height,
                    &bin_options,
                    |binarized| session.process_page_pixels(binarized, width, height, dpi),
                )
            },
        )
        .await
        .map_err(|e| anyhow!("Fused glyph extraction task panicked: {}", e))??;
        drop(permit);
        crate::perf_log!(
            encoding_start,
            "[PROFILING] Page {} fused binarize+glyph extraction completed ({} glyphs)",
            page_index + 1,
            runs.glyph_count
        );
        return Ok(ContentType::GlyphText {
            runs: Arc::new(runs),
            pixel_width: width as u32,
            pixel_height: height as u32,
        });
    }

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

/// Width that keeps `width` x `height`'s aspect at the device's target height.
fn dimensions_for_device_height(width: u32, height: u32, target_height: u32) -> u32 {
    if height == 0 {
        return width.max(1);
    }
    ((f64::from(width) * f64::from(target_height.max(1)) / f64::from(height)).round() as u32).max(1)
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

    let encoding_start = std::time::Instant::now();

    let width = image.width();
    let height = image.height();
    let image_data = Arc::clone(&image);
    let jpeg_compat = config.jpeg_compat();
    let high_quality = config.high_quality_output();
    // The device box this page is read in. Usually the raster is already at it
    // (the page was resized before encoding), but a page rendered above
    // `target_height` — no-layout mode, or a high render height for OCR — is
    // encoded straight from the high-res raster, and there the box is what
    // makes the JP2 smaller instead of coding pixels nobody sees.
    let device_box = (
        config
            .target_width()
            .unwrap_or_else(|| dimensions_for_device_height(width, height, config.target_height())),
        config.target_height().max(1),
    );

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
                // The page raster is already at the device height, so the box
                // is its own size: a verified floor, nothing resampled away.
                (
                    EncodingSettings::Jp2Display(crate::encoding::Jp2DisplaySettings {
                        max_width: device_box.0,
                        max_height: device_box.1,
                        floor: crate::pipeline::quality_policy::full_page_jp2_floor(high_quality),
                        fallback_quality: q,
                    }),
                    "jp2",
                )
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

    crate::perf_log!(
        encoding_start,
        "[PROFILING] Page {} full-page encoding completed",
        page_index + 1
    );

    let (pixel_width, pixel_height) =
        crate::pipeline::helper_functions::encoded_region_dimensions(&data, &format, width, height);

    Ok(ContentType::EncodedImage {
        data: std::sync::Arc::from(data),
        pixel_width,
        pixel_height,
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
    preserved_mask: Option<lege_pdf_read::PreservedJbig2Smask>,
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
    if config.text_format() == "jpeg" || should_preserve_cover_page(page_index, &config) {
        return Ok(None);
    }
    crate::runtime_stats::spawn_blocking_stage(crate::runtime_stats::Stage::Render, move || {
        let t_prepare = std::time::Instant::now();
        let geometry = session
            .page_geometry(page_index as u32)
            .map_err(|error| anyhow!("page geometry failed: {error}"))?;
        let (output_width, output_height) =
            dimensions_for_geometry(geometry, config.target_height(), config.target_width());
        let page = session
            .compile(page_index as u32)
            .map_err(|error| anyhow!("page compile failed: {error}"))?;
        crate::perf_log!(
            t_prepare,
            "[PROFILING] Page {} planned compile",
            page_index + 1
        );
        let t_mask = std::time::Instant::now();
        // Native MRC mask preservation is a source property, not a page-mode
        // preference. When it qualifies, use it for both dithered and original
        // image policies so layout detection cannot accidentally threshold the
        // continuous-tone layer.
        let preserved_mask = session
            .preserved_jbig2_smask(&page, output_width, output_height, Some(&cancellation))
            .map_err(|error| anyhow!("preserved JBIG2 SMask preparation failed: {error}"))?;
        crate::perf_log!(
            t_mask,
            "[PROFILING] Page {} planned preserved-mask probe",
            page_index + 1
        );
        if preserved_mask.is_none()
            && (config.is_grayscale_mode()
                || (config.dither_images() && !config.keep_original_images()))
        {
            return Ok(None);
        }
        let mut plan = crate::pipeline::page_output_plan::plan_page_output(
            &config,
            crate::pipeline::page_output_plan::PagePlanInput {
                output_width,
                output_height,
                gray_suitability: if preserved_mask.is_some() {
                    lege_pdf_read::GraySuitability::AcceptableForBilevel
                } else {
                    page.gray_suitability()
                },
            },
        );
        if plan.base.product.format != lege_pdf_read::RasterFormat::Gray8 {
            return Ok(None);
        }
        let t_analysis = std::time::Instant::now();
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
        crate::perf_log!(
            t_analysis,
            "[PROFILING] Page {} planned analysis render",
            page_index + 1
        );
        Ok(Some(PlannedPdfContext {
            page,
            plan,
            preserved_mask,
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
    crate::bbox_trace!(
        "PAGE {} planned raw detections={} inference={}x{} page_space={}",
        inference_data.rendered.index,
        inference_data.inference_result.detections.len(),
        inference_data.rendered.inference_image.width(),
        inference_data.rendered.inference_image.height(),
        inference_data.inference_result.detections_are_page_space
    );
    if crate::bbox_trace::enabled() {
        for detection in &inference_data.inference_result.detections {
            eprintln!(
                "  PAGE {} planned raw {} bbox={:?} conf={:.2}",
                inference_data.rendered.index,
                crate::types::detection_label(detection),
                detection.bbox,
                detection.confidence
            );
        }
    }
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
    // Geometry only. Line-art vs photo is decided later on the output raster
    // (see should_keep_image_overlay), after boxes are scaled.
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

#[cfg(test)]
mod image_detection_policy_tests {
    use super::*;
    use crate::types::ContentCategory;

    fn detection(
        class_id: i32,
        category: ContentCategory,
        bbox: [f32; 4],
    ) -> crate::engine::Detection {
        crate::engine::Detection {
            class_id,
            class_name: None,
            confidence: 0.9,
            bbox,
            category,
            context: None,
        }
    }

    #[test]
    fn planned_path_keeps_model_image_label_even_for_bilevel_pixels() {
        let image = Arc::new(RgbImage::from_fn(100, 100, |x, _| {
            if x % 2 == 0 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            }
        }));
        // Checkerboard hatching is line art (engraving-like). This test only
        // checks that scale_detections_to_output still keeps the model box.
        assert!(crate::content_class::region_is_line_art(
            image.as_raw(),
            100,
            100,
            [0.0, 0.0, 100.0, 100.0]
        ));
        let rendered = RenderedPageData {
            index: 0,
            high_res_image: image.clone(),
            inference_image: image.clone(),
            layout_detection_enabled: true,
            original_width_pts: 100.0,
            original_height_pts: 100.0,
        };
        let inference_result = InferenceResult {
            index: 0,
            high_res_image: image.clone(),
            inference_image: image,
            detections: vec![detection(
                1,
                ContentCategory::Image,
                [10.0, 10.0, 90.0, 90.0],
            )],
            text_layer: None,
            detections_are_page_space: false,
            original_width_pts: 100.0,
            original_height_pts: 100.0,
            has_no_detections: false,
        };
        let detections = scale_detections_to_output(
            &PdfInferenceData {
                rendered,
                inference_result,
            },
            200,
            200,
        );
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].bbox, [20.0, 20.0, 180.0, 180.0]);
    }

    #[test]
    fn planned_path_still_drops_image_box_over_substantive_text() {
        let image = Arc::new(RgbImage::from_pixel(100, 100, image::Rgb([255, 255, 255])));
        let rendered = RenderedPageData {
            index: 0,
            high_res_image: image.clone(),
            inference_image: image.clone(),
            layout_detection_enabled: true,
            original_width_pts: 100.0,
            original_height_pts: 100.0,
        };
        let inference_result = InferenceResult {
            index: 0,
            high_res_image: image.clone(),
            inference_image: image,
            detections: vec![
                detection(1, ContentCategory::Image, [10.0, 10.0, 90.0, 90.0]),
                detection(2, ContentCategory::Text, [20.0, 20.0, 80.0, 80.0]),
            ],
            text_layer: None,
            detections_are_page_space: false,
            original_width_pts: 100.0,
            original_height_pts: 100.0,
            has_no_detections: false,
        };
        let detections = scale_detections_to_output(
            &PdfInferenceData {
                rendered,
                inference_result,
            },
            100,
            100,
        );
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].category, ContentCategory::Text);
    }
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
    preserved_mask: Option<lege_pdf_read::PreservedJbig2Smask>,
    detections: Vec<crate::engine::Detection>,
    cancellation: lege_pdf_read::CancellationToken,
    glyph_session: Option<Arc<GlyphFontSession>>,
) -> Result<ProcessedPage> {
    let lege_pdf_read::RasterPlane::Gray8(base_surface) = products.base else {
        return Err(anyhow!("planned gray path received a non-gray base"));
    };
    let width = base_surface.width;
    let height = base_surface.height;
    let mut gray = compact_gray_surface(base_surface)?;
    let mut working_coverage = preserved_mask
        .as_ref()
        .map(|mask| compact_gray_surface(mask.working_coverage.clone()))
        .transpose()?;
    let uses_preserved_mask = working_coverage.is_some();
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
                if let Some(coverage) = working_coverage.as_mut() {
                    coverage[start..end].fill(0);
                }
            }
        }
    }
    checkpoint(&cancellation, "after Gray8 region masking")?;
    let binarized = if let Some(coverage) = working_coverage.as_ref() {
        // This fixed conversion is only an OCR/collar view of the already
        // decoded source mask. It never thresholds rendered page pixels and
        // never feeds the authoritative output JBIG2 stream.
        let mut clean_options = crate::clean_gray::CleanOptions::production_for_height(
            height as usize,
            config.invert_input(),
        );
        clean_options.halo = None;
        if let Ok(cleaned) = crate::clean_gray::clean_gray_page(
            &gray,
            width as usize,
            height as usize,
            &clean_options,
        ) {
            gray = cleaned.pixels;
        }
        coverage
            .iter()
            .map(|&opacity| if opacity >= 128 { 0 } else { 255 })
            .collect()
    } else {
        let options = crate::pipeline::policies::binarize_options_for(&config, false);
        let t_bin = std::time::Instant::now();
        let b = crate::color::binarization::binarize_gray(
            &gray,
            width as usize,
            height as usize,
            &options,
        );
        crate::perf_log!(
            t_bin,
            "[PROFILING] Page {} planned binarize",
            page_index + 1
        );
        b
    };
    checkpoint(&cancellation, "after planned mask preparation")?;

    let native_text_transform = NativeTextTransform {
        source_width: width,
        source_height: height,
        correction: identity_margin_correction(),
    };
    let page_frame = page_frame_for_ocr(&config, &binarized, width as usize, height as usize);
    let quarter_turns = page_frame.map_or(0, |frame| frame.turns);
    let hocr_text = if config.enable_ocr() && config.slow_ocr_enabled() {
        if uses_preserved_mask {
            // On preserved-mask pages OCR must consume the source-derived
            // binary view instead of re-thresholding a separately rendered
            // high-resolution composite.
            let ocr_image = gray_to_rgb_image(&gray, width, height)?;
            crate::ocr::slow::perform_slow_ocr(
                &ocr_image,
                &binarized,
                &detections,
                width,
                height,
                &config,
                page_index,
                page_frame,
            )
            .await?
        } else {
            let ocr_plane = products
                .ocr
                .ok_or_else(|| anyhow!("slow OCR plan did not render an OCR surface"))?;
            let lege_pdf_read::RasterPlane::Rgb8(surface) = ocr_plane else {
                return Err(anyhow!("slow OCR target returned a non-RGB plane"));
            };
            let ocr_image =
                RgbImage::from_raw(surface.width, surface.height, surface.pixels.to_vec())
                    .ok_or_else(|| anyhow!("OCR RGB surface was truncated"))?;
            crate::ocr::slow::perform_slow_ocr(
                &ocr_image,
                &[],
                &detections,
                width,
                height,
                &config,
                page_index,
                page_frame,
            )
            .await?
        }
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
            page_frame,
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
    let t_regions = std::time::Instant::now();
    let region_count = products.regions.len();
    for region in products.regions {
        checkpoint(&cancellation, "during planned region encoding")?;
        let Some(crop) = region.target.product.crop else {
            continue;
        };
        let lege_pdf_read::RasterPlane::Rgb8(surface) = region.plane else {
            continue;
        };
        // The source raster can be far larger than the box it is drawn in on
        // the output page; encode it at the box and place it unchanged.
        let (encoded, format, pixel_width, pixel_height) =
            crate::pipeline::helper_functions::encode_region_image(
                &surface.pixels,
                surface.width,
                surface.height,
                *config.cover_format(),
                false,
                config.high_quality_output(),
                config.jpeg_compat(),
                Some((crop.width.max(1), crop.height.max(1))),
                // The placement box on the page, not the source surface.
                crate::pipeline::quality_policy::RegionSize::of(
                    crop.width.max(1),
                    crop.height.max(1),
                    width,
                    height,
                ),
            )
            .await?;
        elements.push(crate::accumulator::ContentElement {
            x: crop.x.max(0) as f32,
            y: crop.y.max(0) as f32,
            width: crop.width as f32,
            height: crop.height as f32,
            content: crate::accumulator::ContentType::EncodedImage {
                data: Arc::from(encoded),
                pixel_width,
                pixel_height,
                format,
            },
        });
    }

    crate::perf_log!(
        t_regions,
        "[PROFILING] Page {} planned {} region encodes",
        page_index + 1,
        region_count
    );
    let force_jbig2_generic = matches!(
        config.text_format(),
        "jbig2" | crate::pipeline::config::TRUETYPING
    ) && detections
        .iter()
        .any(|detection| detection.category.force_generic_jbig2());
    if let Some(mask) = preserved_mask {
        let coverage = working_coverage.expect("preserved mask has working coverage");
        let (background, foreground) = encode_preserved_mrc_base_layer(
            gray,
            coverage,
            width as usize,
            height as usize,
            &config,
            page_index,
            mask,
        )
        .await?;
        // The source's own text layer passes through under every text
        // format, truetyping included: it is the original ink, and tracing
        // it could only move its edges.
        elements.insert(
            0,
            crate::accumulator::ContentElement {
                x: 0.0,
                y: 0.0,
                width: width as f32,
                height: height as f32,
                content: background,
            },
        );
        elements.insert(
            1,
            crate::accumulator::ContentElement {
                x: 0.0,
                y: 0.0,
                width: width as f32,
                height: height as f32,
                content: foreground,
            },
        );
    } else {
        let base = encode_base_layer(
            binarized,
            width as usize,
            height as usize,
            &config,
            page_index,
            force_jbig2_generic,
            glyph_session,
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
    }

    let index = page_index.saturating_sub(page_start);
    let toc = if config.enable_auto_toc() {
        crate::toc::capture_page(&detections, hocr_text.as_deref(), index, width, height)
    } else {
        crate::toc::PageTocData::default()
    };

    Ok(ProcessedPage {
        index,
        width,
        height,
        elements,
        hocr_text,
        toc,
        quarter_turns,
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
    glyph_session: Option<Arc<GlyphFontSession>>,
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
    let inference_image = if page_layout_enabled && planned_pdf.is_none() {
        let spec = config.inference_resize_spec();
        Arc::new(
            crate::pipeline::policies::build_inference_image(high_res_image.as_ref(), &spec)
                .unwrap_or_else(|_| (*high_res_image).clone()),
        )
    } else {
        // Planned PDF analysis is already rendered with a bounded long edge.
        // Pass its aspect-preserving surface directly to LayoutDetector; the
        // detector performs the canonical 640x640 model resize itself. An
        // extra square resize here loses low-contrast scanned illustrations.
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

    let text_session = glyph_session.clone();
    let mut processed_page =
        if let (Some(mut planned), Some(session)) = (planned_pdf, document_session.clone()) {
            let detections = scale_detections_to_output(
                &inference_data,
                planned.output_width,
                planned.output_height,
            );
            // JP2 regions render above device size so jp2lam does the downscale
            // in linear light inside its rate loop; every other codec emits the
            // crop as-is and renders at 1.
            let region_render_scale = if crate::pipeline::helper_functions::region_emits_jp2(
                *config.cover_format(),
                false,
                config.jpeg_compat(),
            ) {
                crate::pipeline::page_output_plan::JP2_REGION_RENDER_SCALE
            } else {
                1
            };
            let has_text = detections
                .iter()
                .any(|detection| crate::types::LABEL_CLASSIFIER.is_substantive_text(detection));
            planned.plan.regions = if planned.preserved_mask.is_some() {
                // The source mask identifies text precisely. Process the entire
                // remaining continuous-tone plane with the selected image policy;
                // adding layout-selected crops would duplicate content and make
                // correctness depend on Paddle's labels.
                Vec::new()
            } else {
                detections
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
                            region_render_scale,
                        )
                    })
                    .collect()
            };
            let render_session = session.clone();
            let compiled_page = planned.page.clone();
            // A page whose ink is already one bit keeps it (its own mask, or
            // JBIG2); truetyping traces only rendered scans.
            let glyph_session =
                glyph_session.filter(|_| planned.page.bilevel_raster_height().is_none());
            let plan = planned.plan.clone();
            let render_cancellation = cancellation.clone();
            let products = crate::runtime_stats::spawn_blocking_stage(
                crate::runtime_stats::Stage::Render,
                move || {
                    let t_render = std::time::Instant::now();
                    let products = render_session.render_output_plan(
                        &compiled_page,
                        &plan,
                        Some(&render_cancellation),
                    );
                    crate::perf_log!(
                        t_render,
                        "[PROFILING] Page {} planned output render",
                        page_index + 1
                    );
                    products
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
                planned.preserved_mask,
                detections,
                cancellation.clone(),
                glyph_session,
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
                glyph_session,
            )
            .await?
        };
    checkpoint(&cancellation, "before writer handoff")?;

    // Recognized words teach the glyph font which text its shapes stand for.
    if let (Some(session), Some(hocr)) =
        (text_session.as_ref(), processed_page.hocr_text.as_deref())
    {
        record_glyph_text(session, &mut processed_page.elements, hocr)?;
    }

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
        quarter_turns: processed_page.quarter_turns,
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

    // Truetyping redraws rendered ink as outlines. A book whose ink is
    // already one bit (a JBIG2 or CCITT text layer, an MRC mask) is the
    // original text at its own resolution: tracing it could only move its
    // edges, and rendering it larger only invents them. Such a book keeps its
    // text layer as the JBIG2 format would, at the height that was asked for.
    // Pages are checked one by one as well (`page_ink_is_bilevel`), for a
    // book that mixes scans with bilevel pages.
    let config = match document_session
        .as_ref()
        .filter(|_| config.text_format() == crate::pipeline::config::TRUETYPING)
        .and_then(|session| bilevel_source_height(session, page_start, page_end))
    {
        Some(natural) => {
            let mut kept = (*config).clone();
            kept.set_text_format("jbig2")?;
            info_log!(
                "[PDF-Parallel] The source's ink is already bilevel ({} px): truetyping is skipped and the text stays JBIG2",
                natural
            );
            Arc::new(kept)
        }
        None => config,
    };

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
    let cancellation = lege_pdf_read::CancellationToken::new();

    let (margin_analysis, detection_cache) = if needs_two_pass {
        info_log!("[PDF-Parallel] Margin mode enabled - running 2-pass document analysis");
        let analysis_future = perform_document_margin_analysis(
            source.clone(),
            config.clone(),
            inference_handle.clone(),
            total_pages,
            page_start..page_end,
            page_concurrency,
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
    let memory_budget_mb = page_memory_budget_mb();
    let memory_budget = Arc::new(Semaphore::new(memory_budget_mb));
    info_log!(
        "[PDF-Parallel] Memory admission budget: {} MiB",
        memory_budget_mb
    );
    // One glyph dictionary for the whole document: pages match against it as
    // they finish (any order) and the font is emitted once at finalize.
    let glyph_session = (config.text_format() == crate::pipeline::config::TRUETYPING)
        .then(|| Arc::new(GlyphFontSession::new()));
    let mut jobs = tokio::task::JoinSet::new();
    let mut next_page = page_start;
    let mut hocr_pages = Vec::new();
    let mut toc_candidates: Vec<crate::toc::TocCandidate> = Vec::new();
    let mut toc_stats: Vec<crate::toc::PageTextStats> = Vec::new();
    let mut metadata_candidates: Vec<crate::toc::MetadataCandidate> = Vec::new();
    let mut printed_contents: Vec<String> = Vec::new();

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
                glyph_session.clone(),
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
                metadata_candidates.extend(output.toc.metadata_candidates);
                printed_contents.extend(output.toc.printed_contents);
            }
        }
    }
    info_log!("[PDF-Parallel] Page-owned jobs complete");

    // Await extraction so writer finalization cannot race bookmark delivery.
    let mut source_metadata = lege_pdf_read::DocumentMetadata::default();
    let mut source_outline = Vec::new();
    if let Some(session) = document_session {
        let mut outline_task = crate::runtime_stats::spawn_blocking(move || {
            (
                lege_pdf_read::extract_outline(&session),
                lege_pdf_read::extract_metadata(&session),
            )
        });
        let (bookmarks, metadata) = tokio::select! {
            result = &mut outline_task => {
                result.map_err(|error| anyhow!("Outline extraction task panicked: {error}"))?
            }
            signal = shutdown_rx.recv() => {
                cancellation.cancel();
                outline_task.abort();
                pdf_writer_task.abort();
                let message = signal
                    .ok()
                    .and_then(|signal| signal.message)
                    .unwrap_or_else(|| "User requested cancellation".to_string());
                return Err(anyhow!("Processing cancelled during outline extraction: {message}"));
            }
        };
        source_metadata = metadata;
        if !bookmarks.is_empty() {
            let source_to_output = (page_start..page_end)
                .enumerate()
                .map(|(output, source)| (source, output))
                .collect();
            source_outline = crate::pipeline::helper_functions::bookmarks_to_outline(
                &bookmarks,
                &source_to_output,
            );
            pdf_writer_handle
                .send_bookmarks(bookmarks, source_to_output)
                .await?;
        }
    }

    // The synthesized outline is offered, never imposed: the writer uses it only
    // when the source document had no outline that survived remapping.
    let total_pages = page_end.saturating_sub(page_start);
    let synthetic_outline = crate::toc::build_outline_with_contents(
        toc_candidates,
        &toc_stats,
        total_pages,
        &printed_contents,
    );
    if !synthetic_outline.is_empty() {
        info_log!(
            "[PDF-Parallel] Synthesized a {}-entry table of contents",
            synthetic_outline.len()
        );
        pdf_writer_handle
            .send_synthetic_outline(synthetic_outline.clone())
            .await?;
    }
    let accepted_outline =
        crate::pipeline::helper_functions::merge_outline(source_outline, Some(synthetic_outline));

    let inferred = crate::toc::infer_metadata(&metadata_candidates, &toc_stats, total_pages);
    let title = source_metadata.title.or(inferred.title);
    let author = source_metadata.author.or(inferred.author);
    if title.is_some() || author.is_some() {
        pdf_writer_handle
            .send_document_identity(title, author)
            .await?;
    }

    if let Some(session) = glyph_session.as_ref() {
        let (pages, glyphs, occurrences, residual) = session.stats();
        info_log!(
            "[PDF-Parallel] Glyph font: {} pages, {} distinct glyphs, {} occurrences, {} specks dropped{}",
            pages,
            glyphs,
            occurrences,
            session.specks(),
            if residual > 0 {
                format!(", {} components kept as raster residual", residual)
            } else {
                String::new()
            }
        );
        if occurrences > 0 {
            let builder = Arc::clone(session);
            let fonts = crate::runtime_stats::spawn_blocking_stage(
                crate::runtime_stats::Stage::Encode,
                move || builder.build_embedded_fonts(),
            )
            .await
            .map_err(|e| anyhow!("glyph font build task panicked: {}", e))??;
            // Building the font folds duplicate clusters, so the distinct
            // count is only final now.
            let (_, distinct, _, _) = session.stats();
            let bytes: usize = fonts.iter().map(|font| font.data.len()).sum();
            info_log!(
                "[PDF-Parallel] Glyph font program: {} bytes over {} font(s), {} distinct shapes after merging, {} glyph ids mapped to text",
                bytes,
                fonts.len(),
                distinct,
                session.mapped_glyphs()
            );
            pdf_writer_handle.send_glyph_fonts(fonts).await?;
        }
    }

    info_log!("[PDF-Parallel] Finalizing PDF...");
    pdf_writer_handle.finalize().await?;
    use crate::pipeline::helper_functions::await_stage_or_cancel_with_token;
    await_stage_or_cancel_with_token(
        &mut pdf_writer_task,
        &mut shutdown_rx,
        "PDF writer",
        &[],
        Some(&cancellation),
    )
    .await?;
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
            let sidecar_cancellation = cancellation.clone();
            let packaging_cancellation = sidecar_cancellation.clone();
            let mut sidecar_task = crate::runtime_stats::spawn_blocking_stage(
                crate::runtime_stats::Stage::Writer,
                move || {
                    crate::pipeline::epub_pipeline::build_epub_from_hocr_pages_with_outline_cancellable(
                        &hocr_pages,
                        &title,
                        &epub_path,
                        &accepted_outline,
                        Some(&packaging_cancellation),
                    )
                },
            );
            tokio::select! {
                result = &mut sidecar_task => {
                    result.map_err(|e| anyhow!("EPUB sidecar task panicked: {}", e))??;
                }
                signal = shutdown_rx.recv() => {
                    sidecar_cancellation.cancel();
                    sidecar_task.abort();
                    let message = signal
                        .ok()
                        .and_then(|signal| signal.message)
                        .unwrap_or_else(|| "User requested cancellation".to_string());
                    return Err(anyhow!("Processing cancelled during EPUB sidecar packaging: {message}"));
                }
            }
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

    /// With the high-res raster retained, a JP2 image region is encoded from
    /// that raster, but the stream must still be emitted at the device-space
    /// region size: that is what the PDF XObject declares and what the reader
    /// draws, so the high-res source must never leak into it.
    #[cfg(feature = "jp2-lam")]
    #[test]
    fn slow_ocr_jp2_region_is_encoded_from_high_res_at_device_size() {
        let mut config = (*slow_ocr_config()).clone();
        config.set_enable_layout_detection(true);
        config.set_enable_ocr(false);
        let config = Arc::new(config);
        // A textured colour photo in the page's upper half (noise over a
        // gradient: the overlay classifier drops flat or line-art crops), over
        // a text page.
        let source = Arc::new(RgbImage::from_fn(800, 1200, |x, y| {
            if y < 560 {
                let h = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) >> 8;
                Rgb([
                    ((x / 4) as u8).wrapping_add((h & 0x3f) as u8),
                    ((y / 4) as u8).wrapping_add(((h >> 6) & 0x3f) as u8),
                    (180u8).wrapping_add(((h >> 12) & 0x3f) as u8),
                ])
            } else {
                Rgb([244, 242, 235])
            }
        }));
        let region = crate::engine::Detection {
            class_id: 0,
            class_name: None,
            confidence: 0.9,
            // Device (400x600) space: the top 280 rows.
            bbox: [0.0, 0.0, 400.0, 280.0],
            category: crate::types::ContentCategory::Image,
            context: None,
        };
        let rendered = RenderedPageData {
            index: 1,
            high_res_image: source.clone(),
            inference_image: source.clone(),
            layout_detection_enabled: true,
            original_width_pts: 400.0,
            original_height_pts: 600.0,
        };
        let inference_result = InferenceResult {
            index: 1,
            high_res_image: source.clone(),
            inference_image: source,
            detections: vec![region],
            text_layer: None,
            detections_are_page_space: true,
            original_width_pts: 400.0,
            original_height_pts: 600.0,
            has_no_detections: false,
        };
        let output = process_page_cpu_work(PageProcessingInput {
            rendered,
            inference_result,
            page_index: 1,
            config,
            margin_analysis: None,
            cancellation: lege_pdf_read::CancellationToken::new(),
        })
        .expect("page work with an image region");

        assert!(output.ocr_image.is_some(), "high-res raster retained");
        let result = output
            .region_processing_results
            .iter()
            .find(|r| r.encoded_data.is_some())
            .expect("the image region was encoded");
        let (data, fmt) = result.encoded_data.as_ref().unwrap();
        assert_eq!(fmt, "jp2");
        // Device-space region (page policy may grow the box), never the
        // 800x1200 high-res crop.
        let region = (result.region_w, result.region_h);
        assert!(
            region.0 <= 400 && region.1 <= 600,
            "region {region:?} not device-space"
        );
        assert_eq!((result.encoded_w, result.encoded_h), region);
        assert_eq!(
            crate::encoding::jp2::jp2_dimensions(data).expect("jp2 header"),
            region
        );
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
