use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::Result;
use image::RgbImage;

use lege_ocr::{
    OcrPipeline, SlowOcrConfig, coordinate::CoordinateMap, coordinate::scale_lines, hocr,
    normalize, types::TextRegion,
};

use crate::engine::Detection;
use crate::pipeline::config::PipelineConfig;
use crate::types::ContentCategory;

thread_local! {
    /// Per-worker cache of slow-OCR pipelines, keyed by language. Constructing
    /// an `OcrPipeline` initializes the platform OCR engine, which is expensive;
    /// reusing one per blocking-pool worker thread avoids paying that cost on
    /// every page.
    static PIPELINES: RefCell<HashMap<String, OcrPipeline>> = RefCell::new(HashMap::new());
}

/// Convert a lege `Detection` slice to `lege_ocr::TextRegion`s in the OCR
/// image's pixel space, keeping only text-like detections.
///
/// `detections` are expressed in *output* space (`output_w` × `output_h`); they
/// are scaled by (`image_w`/`output_w`, `image_h`/`output_h`) so they line up
/// with the (possibly higher-resolution) OCR raster.
fn detections_to_regions(
    detections: &[Detection],
    page_index: usize,
    image_w: u32,
    image_h: u32,
    output_w: u32,
    output_h: u32,
) -> Vec<TextRegion> {
    let sx = image_w as f32 / output_w.max(1) as f32;
    let sy = image_h as f32 / output_h.max(1) as f32;
    let classifier = crate::types::LabelClassifier::default();
    detections
        .iter()
        .filter(|d| {
            classifier.should_process_with_ocr(d) && !matches!(d.category, ContentCategory::Abandon)
        })
        .enumerate()
        .filter_map(|(i, d)| {
            let bbox = [
                (d.bbox[0] * sx).floor().clamp(0.0, image_w as f32) as u32,
                (d.bbox[1] * sy).floor().clamp(0.0, image_h as f32) as u32,
                (d.bbox[2] * sx).ceil().clamp(0.0, image_w as f32) as u32,
                (d.bbox[3] * sy).ceil().clamp(0.0, image_h as f32) as u32,
            ];
            (bbox[2] > bbox[0] && bbox[3] > bbox[1]).then(|| TextRegion {
                page_index,
                region_id: i,
                class_name: d.class_name.clone(),
                bbox_highres: bbox,
                confidence: d.confidence,
            })
        })
        .collect()
}

/// Run the slow OCR pipeline on one page and return a page-level hOCR string.
///
/// `image` is the raster to recognize — the high-resolution OCR raster when
/// "render high, resize low" is active, otherwise the page-resolution image.
/// `binarized` is a binary mask matching `image`'s dimensions; when it does not
/// match (e.g. only a page-resolution mask is available) the OCR raster is
/// re-binarized internally.
///
/// `detections` and the produced hOCR are both expressed in *output* space
/// (`output_width` × `output_height`) — the page-pixel space the PDF/DJVU writer
/// maps 1:1 onto the page. Recognition happens at `image`'s resolution and the
/// results are scaled back down to output space.
///
/// This is a drop-in replacement for the fast OCR path when
/// `config.slow_ocr_enabled()` is true.
#[allow(clippy::too_many_arguments)]
pub async fn perform_slow_ocr(
    image: &RgbImage,
    binarized: &[u8],
    detections: &[Detection],
    output_width: u32,
    output_height: u32,
    config: &PipelineConfig,
    page_index: usize,
) -> Result<Option<String>> {
    let image_w = image.width();
    let image_h = image.height();
    if image_w == 0 || image_h == 0 {
        return Ok(None);
    }

    let regions = detections_to_regions(
        detections,
        page_index,
        image_w,
        image_h,
        output_width,
        output_height,
    );

    if regions.is_empty() {
        return Ok(None);
    }

    let language = config.ocr_language().to_string();
    let coord_map = CoordinateMap::identity(image_w, image_h, image_w as f32, image_h as f32);

    // Run in spawn_blocking because OCR engine calls (and GPU binarization) are
    // synchronous. The pipeline (and its OCR engine) is cached per worker thread.
    let image_clone = image.clone();
    let has_matching_binary = binarized.len() == image_w as usize * image_h as usize;
    let binarized_clone = if has_matching_binary {
        binarized.to_vec()
    } else {
        Vec::new()
    };

    let mut slow_page = tokio::task::spawn_blocking(move || {
        // Binarize the OCR raster ourselves when the caller could not supply a
        // mask at this resolution (the high-res render has none).
        let binary = if binarized_clone.is_empty() {
            normalize::binarize_page(&image_clone)
        } else {
            binarized_clone
        };
        PIPELINES.with(|cell| {
            let mut pipelines = cell.borrow_mut();
            let pipeline = pipelines.entry(language.clone()).or_insert_with(|| {
                OcrPipeline::new(SlowOcrConfig {
                    language: language.clone(),
                    debug: false,
                    debug_out_dir: None,
                    ..Default::default()
                })
            });
            pipeline.process_page(&image_clone, &binary, &regions, &coord_map, page_index)
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("slow OCR task panicked: {e}"))??;

    // Scale recognition results from the OCR raster's space back to output space
    // and rebuild the page hOCR there (the writer treats hOCR coordinates as
    // page-pixel == PDF units).
    let sx = output_width as f32 / image_w as f32;
    let sy = output_height as f32 / image_h as f32;
    scale_lines(&mut slow_page.lines, sx, sy, output_width, output_height);
    let hocr = hocr::build_page_hocr(&slow_page.lines, output_width, output_height);

    Ok(Some(hocr))
}
