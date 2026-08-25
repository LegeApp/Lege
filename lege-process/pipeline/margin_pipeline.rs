use std::sync::Arc;

use anyhow::{Result, anyhow};
use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use image::RgbImage;

use crate::margin::{
    ContentBounds, DocumentMarginAnalysis, PageMarginInput, StandardPageDimensions,
};
use crate::pipeline::config::{InferenceResult, PipelineConfig, RenderedPageData};
use crate::pipeline::page_analysis::{
    compute_missed_pixel_bounds_for_margin, compute_pixel_bounds_for_margin,
};
use crate::pipeline::policies::{MarginCorrection, build_inference_image};
use crate::pipeline::source::PageSource;
use crate::progress::ProgressTracker;
use crate::{info_log, warn_log};

#[derive(Clone, Debug)]
pub(crate) enum CachedDetections {
    Missing,
    Present {
        detections: Vec<crate::engine::Detection>,
        page_width: u32,
        page_height: u32,
    },
}

pub(crate) fn cached_inference_result(
    rendered: &RenderedPageData,
    detection_cache: &[CachedDetections],
) -> Option<InferenceResult> {
    let CachedDetections::Present {
        detections,
        page_width,
        page_height,
    } = detection_cache.get(rendered.index)?
    else {
        return None;
    };
    if *page_width == 0 || *page_height == 0 {
        return None;
    }

    let scale_x = rendered.high_res_image.width() as f32 / *page_width as f32;
    let scale_y = rendered.high_res_image.height() as f32 / *page_height as f32;
    let mut scaled = detections.clone();
    for det in &mut scaled {
        det.scale_bbox(scale_x, scale_y);
    }
    let has_no_detections = scaled.is_empty();

    Some(InferenceResult {
        index: rendered.index,
        high_res_image: rendered.high_res_image.clone(),
        inference_image: rendered.inference_image.clone(),
        detections: scaled,
        text_layer: None,
        detections_are_page_space: true,
        original_width_pts: rendered.original_width_pts,
        original_height_pts: rendered.original_height_pts,
        has_no_detections,
    })
}

struct AnalysisPreparedPage {
    page_index: usize,
    analysis_image: RgbImage,
}

struct AnalysisPageResult {
    page_index: usize,
    page_width: u32,
    page_height: u32,
    detections: Vec<crate::engine::Detection>,
    pixel_bounds: Option<ContentBounds>,
}

enum AnalysisJobResult {
    Completed(AnalysisPageResult),
    LoadFailed {
        page_index: usize,
        _error: anyhow::Error,
    },
}

fn prepare_analysis_page(page_index: usize, original_image: RgbImage) -> AnalysisPreparedPage {
    const ANALYSIS_WIDTH: u32 = 640;

    let aspect_ratio = original_image.width() as f32 / original_image.height().max(1) as f32;
    let analysis_height = (ANALYSIS_WIDTH as f32 / aspect_ratio).round().max(1.0) as u32;
    let params = crate::resize::ResizeParams {
        target_width: ANALYSIS_WIDTH,
        target_height: analysis_height,
        method: crate::resize::ResizeMethod::Bell,
        letterbox: false,
        border_value: 0.0,
        swap_rb: false,
    };
    let analysis_image = crate::resize::resize_bytes(
        original_image.as_raw(),
        original_image.width(),
        original_image.height(),
        &params,
        3,
    )
    .ok()
    .and_then(|bytes| RgbImage::from_raw(ANALYSIS_WIDTH, analysis_height, bytes))
    .unwrap_or_else(|| {
        warn_log!(
            "Page {}: resize for margin analysis failed; using original.",
            page_index
        );
        original_image
    });
    AnalysisPreparedPage {
        page_index,
        analysis_image,
    }
}

fn build_margin_analysis_future(
    inference_handle: Option<Arc<crate::pipeline::inference::InferenceHandle>>,
    prepared: AnalysisPreparedPage,
    config: Arc<PipelineConfig>,
    cancellation: lege_pdf_read::CancellationToken,
) -> BoxFuture<'static, Result<AnalysisPageResult>> {
    Box::pin(async move {
        let AnalysisPreparedPage {
            page_index,
            analysis_image,
        } = prepared;
        if cancellation.is_cancelled() {
            return Err(anyhow!("Margin analysis cancelled before inference"));
        }

        let mut detections = if config.layout_detection_enabled_for_page(page_index) {
            if let Some(handle) = inference_handle {
                let spec = config.inference_resize_spec();
                let inference_image = build_inference_image(&analysis_image, &spec)
                    .unwrap_or_else(|_| analysis_image.clone());
                handle
                    .submit(page_index, Arc::new(inference_image))
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
        if cancellation.is_cancelled() {
            return Err(anyhow!("Margin analysis cancelled after inference"));
        }

        crate::pipeline::policies::remap_detections_to_page(
            &mut detections,
            analysis_image.width(),
            analysis_image.height(),
            &config,
        );

        let pixel_bounds =
            crate::runtime_stats::spawn_blocking_stage(crate::runtime_stats::Stage::Processing, {
                let analysis_image = analysis_image.clone();
                let detections = detections.clone();
                let config = config.clone();
                move || {
                    if detections.is_empty() {
                        compute_pixel_bounds_for_margin(&analysis_image, &config)
                    } else {
                        compute_missed_pixel_bounds_for_margin(
                            &analysis_image,
                            &detections,
                            &config,
                        )
                    }
                }
            })
            .await
            .map_err(|error| anyhow!("Margin-analysis pixel guard task panicked: {}", error))?;
        if cancellation.is_cancelled() {
            return Err(anyhow!("Margin analysis cancelled after pixel bounds"));
        }

        Ok(AnalysisPageResult {
            page_index,
            page_width: analysis_image.width(),
            page_height: analysis_image.height(),
            detections,
            pixel_bounds,
        })
    })
}

pub(crate) async fn perform_document_margin_analysis(
    source: Arc<dyn PageSource>,
    config: Arc<PipelineConfig>,
    inference_handle: Option<Arc<crate::pipeline::inference::InferenceHandle>>,
    total_pages: usize,
    page_range: std::ops::Range<usize>,
    max_in_flight: usize,
    progress: &ProgressTracker,
    cancellation: lege_pdf_read::CancellationToken,
) -> Result<(DocumentMarginAnalysis, Vec<CachedDetections>)> {
    info_log!("[Margin-Analysis] Phase 1: Analyzing document margins (Low-Res Pass)...");
    progress.update(crate::progress::ProcessingStatus::MarginPass1Analyzing);

    let mut margin_inputs = Vec::new();
    let mut detection_cache = vec![CachedDetections::Missing; source.page_count()];
    let mut pending: FuturesUnordered<BoxFuture<'static, Result<AnalysisJobResult>>> =
        FuturesUnordered::new();
    let analysis_concurrency = max_in_flight
        .max(
            inference_handle
                .as_ref()
                .map_or(1, |handle| handle.session_count()),
        )
        .max(1);
    info_log!(
        "[Margin-Analysis] Low-res page workers: {} (GPU sessions: {})",
        analysis_concurrency,
        inference_handle
            .as_ref()
            .map_or(0, |handle| handle.session_count())
    );

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

    let mut next_page = page_range.start;
    let page_end = page_range.end;
    while next_page < page_end || !pending.is_empty() {
        if cancellation.is_cancelled() {
            return Err(anyhow!("Margin analysis cancelled"));
        }
        while next_page < page_end && pending.len() < analysis_concurrency {
            let page_index = next_page;
            next_page += 1;
            let source = source.clone();
            let config = config.clone();
            let inference_handle = inference_handle.clone();
            let cancellation = cancellation.clone();
            pending.push(Box::pin(async move {
                let source_page = match source
                    .load_page_cancellable(page_index, cancellation.clone())
                    .await
                {
                    Ok(page) => page,
                    Err(error) => {
                        return Ok(AnalysisJobResult::LoadFailed {
                            page_index,
                            _error: error,
                        });
                    }
                };
                let prepared = crate::runtime_stats::spawn_blocking_stage(
                    crate::runtime_stats::Stage::Processing,
                    move || prepare_analysis_page(page_index, source_page.image),
                )
                .await
                .map_err(|error| anyhow!("Margin-analysis prep task panicked: {}", error))?;
                if cancellation.is_cancelled() {
                    return Err(anyhow!("Margin analysis cancelled after page preparation"));
                }
                let result = crate::runtime_stats::track_future(
                    crate::runtime_stats::Stage::Inference,
                    build_margin_analysis_future(inference_handle, prepared, config, cancellation),
                )
                .await?;
                Ok(AnalysisJobResult::Completed(result))
            }));
        }

        let Some(result) = pending.next().await else {
            break;
        };
        match result? {
            AnalysisJobResult::Completed(result) => {
                push_completed(result, &mut margin_inputs, &mut detection_cache);
            }
            AnalysisJobResult::LoadFailed { page_index, _error } => {
                warn_log!(
                    "Page {}: margin-analysis load failed: {}. Preserving page.",
                    page_index,
                    _error
                );
                margin_inputs.push(PageMarginInput {
                    page_index,
                    page_width: 0,
                    page_height: 0,
                    detections: Vec::new(),
                    pixel_bounds: None,
                });
            }
        }
        progress.publish_margin_progress(
            margin_inputs.len().min(total_pages),
            margin_inputs.len().min(total_pages),
            0,
            total_pages,
        );
    }
    margin_inputs.sort_by_key(|input| input.page_index);

    let analysis = crate::margin::analyze_document_margins(
        &margin_inputs,
        &config,
        config.margin_settings(),
        config.crop_footnotes(),
    );
    if let Some(reason) = &analysis.setting_override_reason {
        progress.update(crate::progress::ProcessingStatus::FootnotesDetected {
            message: reason.clone(),
        });
    }
    progress.update(crate::progress::ProcessingStatus::MarginAnalysisSummary {
        summary: format!(
            "Baseline margins established from {} pages. Effective setting: {:?}",
            margin_inputs.len(),
            analysis.effective_margin_setting
        ),
    });

    Ok((analysis, detection_cache))
}

#[derive(Debug)]
pub(crate) struct AdjustedMarginPage {
    pub image: RgbImage,
    pub detections: Vec<crate::engine::Detection>,
    pub correction: MarginCorrection,
    pub free_aspect_crop: bool,
    #[allow(dead_code)]
    pub centered_exception: bool,
}

fn identity_correction() -> MarginCorrection {
    MarginCorrection::new(0.0, 0.0, 1.0, 1.0)
}

fn scale_page_bounds(
    bounds: ContentBounds,
    page_data: &crate::margin::PageMarginData,
    page_width: u32,
    page_height: u32,
) -> ContentBounds {
    bounds.scale_to_resolution(
        page_data.page_width.max(1),
        page_data.page_height.max(1),
        page_width,
        page_height,
    )
}

/// Ignore an oversized crop axis when its bounds also hug a page edge.
///
/// A detected header or scanner-edge shadow can span the full width/height and
/// contaminate the per-page safety union. Treating that as genuine content
/// switches the page to the larger centered fallback, which adds margins in a
/// crop job. Interior oversized content remains protected by that fallback.
fn constrain_edge_spanning_crop_axes(
    safety: ContentBounds,
    template: ContentBounds,
    page_width: u32,
    page_height: u32,
) -> ContentBounds {
    let edge_x = ((page_width as f32) * 0.01).round().max(6.0) as u32;
    let edge_y = ((page_height as f32) * 0.01).round().max(6.0) as u32;
    let constrain_x = safety.width() > template.width()
        && (safety.min_x <= edge_x || page_width.saturating_sub(safety.max_x) <= edge_x);
    let constrain_y = safety.height() > template.height()
        && (safety.min_y <= edge_y || page_height.saturating_sub(safety.max_y) <= edge_y);

    if !constrain_x && !constrain_y {
        return safety;
    }

    let fitted = crate::margin::fit_crop_window_to_content(
        &safety,
        template.width().max(1),
        template.height().max(1),
        page_width,
        page_height,
    );
    ContentBounds {
        min_x: if constrain_x {
            fitted.min_x
        } else {
            safety.min_x
        },
        min_y: if constrain_y {
            fitted.min_y
        } else {
            safety.min_y
        },
        max_x: if constrain_x {
            fitted.max_x
        } else {
            safety.max_x
        },
        max_y: if constrain_y {
            fitted.max_y
        } else {
            safety.max_y
        },
    }
}

pub(crate) fn adjust_page_with_margin_analysis(
    page: &RenderedPageData,
    mut detections: Vec<crate::engine::Detection>,
    detections_are_page_space: bool,
    config: &PipelineConfig,
    analysis: &DocumentMarginAnalysis,
    page_index: usize,
) -> Result<AdjustedMarginPage> {
    let page_width = page.high_res_image.width();
    let page_height = page.high_res_image.height();
    if !detections_are_page_space {
        crate::pipeline::policies::remap_detections_to_page(
            &mut detections,
            page_width,
            page_height,
            config,
        );
    }

    let scaled_baseline = analysis.baseline_bounds.scale_to_resolution(
        analysis.analysis_width.max(1),
        analysis.analysis_height.max(1),
        page_width,
        page_height,
    );
    let scaled_crop = analysis.crop_bounds.scale_to_resolution(
        analysis.analysis_width.max(1),
        analysis.analysis_height.max(1),
        page_width,
        page_height,
    );
    let page_data = analysis.pages.get(&page_index);
    let full_page = ContentBounds {
        min_x: 0,
        min_y: 0,
        max_x: page_width,
        max_y: page_height,
    };
    let non_crop_content = page_data.is_some_and(|data| data.is_blank || data.is_full_page_image);

    let detection_bounds =
        crate::margin::calculate_content_bounds(&detections, page_width, page_height, true);
    let cached_safety = page_data.and_then(|data| {
        data.content_bounds
            .map(|bounds| scale_page_bounds(bounds, data, page_width, page_height))
    });
    let safety_bounds = match (detection_bounds, cached_safety) {
        (Some(detection), Some(cached)) => Some(detection.union(&cached)),
        (Some(bounds), None) | (None, Some(bounds)) => Some(bounds),
        (None, None) => compute_pixel_bounds_for_margin(&page.high_res_image, config),
    };

    let mut effective_setting = analysis.effective_margin_setting;
    let mut centered_exception = false;
    let bounds = if effective_setting == crate::margin::MarginSettings::CropAndResize {
        if non_crop_content {
            effective_setting = crate::margin::MarginSettings::StandardizeAndCenter;
            full_page
        } else {
            let safety = constrain_edge_spanning_crop_axes(
                safety_bounds.unwrap_or(scaled_crop),
                scaled_crop,
                page_width,
                page_height,
            );
            let pad_y = ((scaled_crop.height() as f32) * 0.015).round().max(4.0) as u32;
            let required_height = safety.height().saturating_add(pad_y.saturating_mul(2));
            let exceptional =
                safety.width() > scaled_crop.width() || safety.height() > scaled_crop.height();
            if exceptional {
                effective_setting = crate::margin::MarginSettings::StandardizeAndCenter;
                centered_exception = true;
                safety
            } else if config.crop_free_aspect() {
                let min_height = ((scaled_crop.height() as f32) * 0.35).round().max(1.0) as u32;
                let window_height = required_height.clamp(min_height, scaled_crop.height().max(1));
                crate::margin::fit_crop_window_to_content(
                    &safety,
                    scaled_crop.width().max(1),
                    window_height,
                    page_width,
                    page_height,
                )
            } else {
                crate::margin::fit_crop_window_to_content(
                    &safety,
                    scaled_crop.width().max(1),
                    scaled_crop.height().max(1),
                    page_width,
                    page_height,
                )
            }
        }
    } else if non_crop_content {
        full_page
    } else {
        safety_bounds.unwrap_or(scaled_baseline)
    };

    let free_aspect_crop = effective_setting == crate::margin::MarginSettings::CropAndResize
        && config.crop_free_aspect()
        && !non_crop_content;
    let standard_dimensions =
        if analysis.effective_margin_setting == crate::margin::MarginSettings::CropAndResize {
            if effective_setting == crate::margin::MarginSettings::StandardizeAndCenter
                && config.target_width().is_none()
            {
                // Fallback page inside a crop job (blank page, full-page image,
                // or content larger than the document crop window). With no
                // fixed output width the canvas aspect is free — letterboxing
                // onto the crop-window aspect would ADD white margin around
                // the page, the opposite of what cropping promises. Use the
                // page's own bounds aspect so the scaled content fills the
                // canvas edge-to-edge.
                StandardPageDimensions {
                    width: bounds.width().max(1),
                    height: bounds.height().max(1),
                }
            } else {
                StandardPageDimensions {
                    width: scaled_crop.width().max(1),
                    height: scaled_crop.height().max(1),
                }
            }
        } else {
            StandardPageDimensions {
                width: scaled_baseline.width().max(1),
                height: scaled_baseline.height().max(1),
            }
        };
    let (target_width, target_height) = if free_aspect_crop {
        let height = ((config.target_height().max(1) as f32) * bounds.height().max(1) as f32
            / scaled_crop.height().max(1) as f32)
            .round()
            .max(1.0) as u32;
        (None, height)
    } else {
        (config.target_width(), config.target_height())
    };

    match crate::margin::process_page_margins(
        &page.high_res_image,
        &bounds,
        effective_setting,
        &standard_dimensions,
        target_width,
        target_height,
    ) {
        Ok(image) => {
            let mut transformed = crate::margin::transform_detections(
                &detections,
                &bounds,
                effective_setting,
                &standard_dimensions,
                target_width,
                target_height,
                Some((page_width, page_height)),
            );
            let output_width = image.width() as f32;
            let output_height = image.height() as f32;
            transformed.retain_mut(|det| {
                det.bbox[0] = det.bbox[0].clamp(0.0, output_width);
                det.bbox[1] = det.bbox[1].clamp(0.0, output_height);
                det.bbox[2] = det.bbox[2].clamp(0.0, output_width);
                det.bbox[3] = det.bbox[3].clamp(0.0, output_height);
                det.bbox[0] < det.bbox[2] && det.bbox[1] < det.bbox[3]
            });
            let correction = crate::margin::compute_margin_correction(
                &bounds,
                effective_setting,
                &standard_dimensions,
                target_width,
                target_height,
                Some((page_width, page_height)),
            );
            Ok(AdjustedMarginPage {
                image,
                detections: transformed,
                correction,
                free_aspect_crop,
                centered_exception,
            })
        }
        Err(_error) => {
            warn_log!(
                "Page {}: margin adjustment failed: {}. Preserving original.",
                page_index,
                _error
            );
            Ok(AdjustedMarginPage {
                image: (*page.high_res_image).clone(),
                detections,
                correction: identity_correction(),
                free_aspect_crop: false,
                centered_exception: false,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use image::{Rgb, RgbImage};

    use super::*;
    use crate::margin::{MarginSettings, PageMarginData};

    fn rendered_page(width: u32, height: u32) -> RenderedPageData {
        let image = Arc::new(RgbImage::from_pixel(width, height, Rgb([255, 255, 255])));
        RenderedPageData {
            index: 0,
            high_res_image: image.clone(),
            inference_image: image,
            layout_detection_enabled: false,
            original_width_pts: width as f32,
            original_height_pts: height as f32,
        }
    }

    fn page_data(bounds: ContentBounds, width: u32, height: u32) -> PageMarginData {
        PageMarginData {
            page_index: 0,
            page_width: width,
            page_height: height,
            content_bounds: Some(bounds),
            is_blank: false,
            is_full_page_image: false,
            margin_left: bounds.min_x,
            margin_right: width.saturating_sub(bounds.max_x),
            margin_top: bounds.min_y,
            margin_bottom: height.saturating_sub(bounds.max_y),
        }
    }

    fn analysis(page: PageMarginData) -> DocumentMarginAnalysis {
        let mut pages = HashMap::new();
        pages.insert(0, page);
        DocumentMarginAnalysis {
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
        }
    }

    fn crop_config() -> PipelineConfig {
        let mut config = PipelineConfig::default();
        config.set_margin_settings(MarginSettings::CropAndResize);
        config.set_crop_free_aspect(true);
        config
    }

    struct NeverLoadedSource;

    #[async_trait::async_trait]
    impl PageSource for NeverLoadedSource {
        fn page_count(&self) -> usize {
            1
        }

        fn source_concurrency(&self) -> usize {
            1
        }

        async fn load_page(
            &self,
            _page_index: usize,
        ) -> Result<crate::pipeline::source::SourcePage> {
            panic!("pre-cancelled margin pass must not load a page");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pre_cancelled_margin_pass_stops_before_page_loading() {
        let cancellation = lege_pdf_read::CancellationToken::new();
        cancellation.cancel();
        let progress = crate::progress::ProgressManager::new().create_tracker();

        let result = perform_document_margin_analysis(
            Arc::new(NeverLoadedSource),
            Arc::new(PipelineConfig::default()),
            None,
            1,
            0..1,
            1,
            &progress,
            cancellation,
        )
        .await;

        assert!(result.is_err());
    }

    #[test]
    fn normal_page_keeps_free_aspect_crop() {
        let bounds = ContentBounds {
            min_x: 120,
            min_y: 160,
            max_x: 500,
            max_y: 660,
        };
        let adjusted = adjust_page_with_margin_analysis(
            &rendered_page(640, 900),
            Vec::new(),
            true,
            &crop_config(),
            &analysis(page_data(bounds, 640, 900)),
            0,
        )
        .expect("normal crop");

        assert!(adjusted.free_aspect_crop);
        assert!(!adjusted.centered_exception);
    }

    #[test]
    fn wider_page_is_centered_instead_of_clipped() {
        let bounds = ContentBounds {
            min_x: 70,
            min_y: 140,
            max_x: 570,
            max_y: 700,
        };
        let adjusted = adjust_page_with_margin_analysis(
            &rendered_page(640, 900),
            Vec::new(),
            true,
            &crop_config(),
            &analysis(page_data(bounds, 640, 900)),
            0,
        )
        .expect("exceptional page should center");

        assert!(!adjusted.free_aspect_crop);
        assert!(adjusted.centered_exception);
        assert_eq!(adjusted.image.height(), crop_config().target_height());
    }

    #[test]
    fn page_edge_span_does_not_expand_the_document_crop() {
        let bounds = ContentBounds {
            min_x: 0,
            min_y: 140,
            max_x: 640,
            max_y: 700,
        };
        let adjusted = adjust_page_with_margin_analysis(
            &rendered_page(640, 900),
            Vec::new(),
            true,
            &crop_config(),
            &analysis(page_data(bounds, 640, 900)),
            0,
        )
        .expect("edge-spanning artifact should use the stable crop");

        assert!(adjusted.free_aspect_crop);
        assert!(!adjusted.centered_exception);
    }

    #[test]
    fn interior_oversized_content_still_uses_the_safe_fallback() {
        let safety = ContentBounds {
            min_x: 70,
            min_y: 140,
            max_x: 570,
            max_y: 700,
        };
        let template = ContentBounds {
            min_x: 0,
            min_y: 0,
            max_x: 400,
            max_y: 600,
        };

        let constrained = constrain_edge_spanning_crop_axes(safety, template, 640, 900);
        assert_eq!(constrained.min_x, safety.min_x);
        assert_eq!(constrained.min_y, safety.min_y);
        assert_eq!(constrained.max_x, safety.max_x);
        assert_eq!(constrained.max_y, safety.max_y);
    }

    #[test]
    fn fixed_aspect_outlier_is_centered_instead_of_clipped() {
        let bounds = ContentBounds {
            min_x: 70,
            min_y: 140,
            max_x: 570,
            max_y: 700,
        };
        let mut config = crop_config();
        config.set_crop_free_aspect(false);
        let adjusted = adjust_page_with_margin_analysis(
            &rendered_page(640, 900),
            Vec::new(),
            true,
            &config,
            &analysis(page_data(bounds, 640, 900)),
            0,
        )
        .expect("fixed-aspect exception should center");

        assert!(!adjusted.free_aspect_crop);
        assert!(adjusted.centered_exception);
        assert_eq!(adjusted.image.height(), config.target_height());
    }

    #[test]
    fn cached_bounds_scale_from_their_own_page_dimensions() {
        let bounds = ContentBounds {
            min_x: 64,
            min_y: 120,
            max_x: 576,
            max_y: 1080,
        };
        let data = page_data(bounds, 640, 1200);

        let scaled = scale_page_bounds(bounds, &data, 1280, 2400);

        assert_eq!(scaled.min_x, 128);
        assert_eq!(scaled.max_x, 1152);
        assert_eq!(scaled.min_y, 240);
        assert_eq!(scaled.max_y, 2160);
    }
}
