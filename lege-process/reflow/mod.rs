//! RasterReflow — raster-first reflow for scanned / image-only books.
//!
//! This module re-composes rendered page images into new, more readable output
//! pages: it detects columns, rows and graphical words purely from pixels,
//! enlarges the effective text size, removes wasted whitespace, and re-flows the
//! content in reading order. OCR is *optional metadata*; the visual reflow works
//! with no text layer at all.
//!
//! Pipeline (see `RASTER_REFLOW_PLAN.md`):
//! ```text
//! SourcePageImage
//!   -> analyze   (ink mask, projections)
//!   -> regions   (columns / ONNX hints, reading order)
//!   -> rows      (row bands, line-spacing stats, chunk breaks)
//!   -> words     (graphical word spans)
//!   -> [FlowItem stream in reading order]
//!   -> compose   (greedy device-width line flow -> master strip)
//!   -> output    (pagination -> ReflowPage + SourceMap)
//! ```
//!
//! **Status: experimental.** The stages and data model are implemented and the
//! PDF pipeline can invoke them through `--reflow`, but the heuristics are still
//! intentionally isolated here so reflow behavior can evolve without disturbing
//! the normal streaming page pipeline.

pub mod analyze;
pub mod compose;
pub mod config;
pub mod debug;
pub mod output;
pub mod regions;
pub mod rows;
pub mod types;
pub mod words;

pub use config::RasterReflowConfig;
pub use types::{
    FlowItem, InkMask, PlacedItem, PlacedKind, ProjectionProfile, PxRect, ReflowConfidence,
    ReflowDocument, ReflowPage, ReflowRegion, RegionKind, SourceMap, SourcePageImage,
    SourcePageSet, SourceRef, TextRow, WordSpan,
};

use crate::engine::Detection;

#[derive(Debug, Clone, Copy)]
pub struct PageTextMetrics {
    pub body_row_height: u32,
    pub median_gap: u32,
    pub row_height_cv: f32,
}

/// Build the reading-order [`FlowItem`] stream for a single source page, along
/// with a confidence summary describing how prose-like the page looked.
pub fn reflow_page_flow(
    page: &SourcePageImage,
    hints: &[Detection],
    cfg: &RasterReflowConfig,
) -> (Vec<FlowItem>, ReflowConfidence) {
    let regions = regions::detect_regions(page, hints, cfg);
    let mut flow: Vec<FlowItem> = Vec::new();
    let mut all_rows: Vec<TextRow> = Vec::new();
    let mut rows_by_region: Vec<Vec<TextRow>> = Vec::with_capacity(regions.len());
    let mut text_region_count = 0usize;

    for region in &regions {
        let region_rows = if region.kind.is_reflowable() {
            rows::detect_rows(page, region, cfg)
        } else {
            Vec::new()
        };
        if !region_rows.is_empty() {
            text_region_count += 1;
            all_rows.extend(region_rows.iter().cloned());
        }
        rows_by_region.push(region_rows);
    }

    let page_metrics = estimate_page_text_metrics(&all_rows, cfg);
    // Prefer the document-wide calibrated body height so every page scales body
    // text by the *same* factor (uniform glyph size across the whole document).
    // Fall back to this page's own estimate only when no calibration is present.
    let body_h = cfg
        .calibrated_body_height
        .or_else(|| page_metrics.map(|m| m.body_row_height))
        .or_else(|| rows::median_text_height(&all_rows))
        .unwrap_or(cfg.target_text_height);
    let body_scale = cfg.clamp_scale(cfg.target_text_height as f32 / body_h.max(1) as f32);

    for (ri, region) in regions.iter().enumerate() {
        if ri > 0 {
            flow.push(FlowItem::RegionBreak);
        }

        if !region.kind.is_reflowable() {
            // Skip blank atomic regions (e.g. a blank page ONNX mislabels as
            // "image") — compositing an all-white crop adds nothing.
            if !analyze::is_blank_region(&page.gray, region.rect, cfg) {
                flow.push(FlowItem::Figure {
                    src: SourceRef {
                        page_index: page.page_index,
                        rect: region.rect,
                    },
                    kind: region.kind,
                });
            }
            continue;
        }

        let region_rows = &rows_by_region[ri];
        if region_rows.is_empty() {
            // No rows. Either the region is genuinely blank (scanner texture on
            // an empty page) — emit nothing — or it has real ink but no
            // detectable rows (a mislabelled figure) — preserve it atomically.
            if !analyze::is_blank_region(&page.gray, region.rect, cfg) {
                flow.push(FlowItem::Figure {
                    src: SourceRef {
                        page_index: page.page_index,
                        rect: region.rect,
                    },
                    kind: RegionKind::Unknown,
                });
            }
            continue;
        }

        let breaks = rows::chunk_break_indices(&region_rows, cfg);
        let median_gap = page_metrics
            .map(|m| m.median_gap)
            .filter(|&g| g > 0)
            .unwrap_or_else(|| rows::median_line_gap(region_rows).unwrap_or(0));

        for (i, row) in region_rows.iter().enumerate() {
            if breaks.contains(&i) {
                let g = rows::normalize_gap(row.gap_above.unwrap_or(0), median_gap, cfg);
                let g_out = ((g as f32) * body_scale).round() as u32;
                flow.push(FlowItem::Gap { px: g_out });
            }
            for w in words::detect_words(page, row, i, cfg) {
                // Crop each word over the *row's* vertical band rather than the
                // word's tight ink box. Every word in a row then shares one
                // height and one baseline, so uniform-scaled words line up and
                // keep a consistent x-height. Using the tight per-word height
                // instead lets short (x-height-only) words scale up more than
                // tall ones, which produces the "font size changes within a
                // paragraph" artifact.
                let rect =
                    PxRect::from_xyxy(w.rect.x, row.rect.y, w.rect.right(), row.rect.bottom());
                flow.push(FlowItem::WordBitmap {
                    src: SourceRef {
                        page_index: page.page_index,
                        rect,
                    },
                    height: body_h,
                });
            }
            flow.push(FlowItem::LineBreak);
        }
    }

    let confidence = score_confidence(&all_rows, text_region_count, cfg);
    (flow, confidence)
}

pub fn estimate_page_text_metrics(
    rows: &[TextRow],
    cfg: &RasterReflowConfig,
) -> Option<PageTextMetrics> {
    let mut heights: Vec<u32> = rows
        .iter()
        .map(|r| r.text_height)
        .filter(|&h| h >= cfg.min_row_height.max(4))
        .collect();

    if heights.len() < 5 {
        return None;
    }

    heights.sort_unstable();
    let trim = heights.len() / 10;
    let lo = trim.min(heights.len() - 1);
    let hi = (heights.len() - trim).max(lo + 1);
    let trimmed = &heights[lo..hi];
    let body_row_height = trimmed[trimmed.len() / 2].max(1);

    Some(PageTextMetrics {
        body_row_height,
        median_gap: rows::median_line_gap(rows).unwrap_or(0),
        row_height_cv: rows::row_height_cv(rows),
    })
}

/// Calibration: estimate the document's body-text row height **in source
/// pixels** by sampling up to `max_samples` pages evenly across the document,
/// detecting rows, and taking the median of the per-page body-row-height
/// estimates. Returns `None` if no sampled page yields enough rows to judge.
///
/// This is the input to the adaptive `target_text_height`: sizing output text to
/// a controlled *magnification* of the calibrated source height keeps reflow from
/// ballooning the page count (see [`RasterReflowConfig::text_magnification`]).
pub fn estimate_document_body_height(
    pages: &[SourcePageImage],
    hints_per_page: &[Vec<Detection>],
    cfg: &RasterReflowConfig,
    max_samples: usize,
) -> Option<u32> {
    if pages.is_empty() {
        return None;
    }
    let step = body_height_sample_step(pages.len(), max_samples);
    let mut bodies: Vec<u32> = Vec::new();
    for (pi, page) in pages.iter().enumerate().step_by(step) {
        let hints: &[Detection] = hints_per_page.get(pi).map(|v| v.as_slice()).unwrap_or(&[]);
        if let Some(body) = estimate_page_body_height(page, hints, cfg) {
            bodies.push(body);
        }
    }
    combine_body_height_samples(bodies)
}

/// Sample stride used by the document body-height calibration. A streaming
/// caller uses this to decide which pages to render for the calibration pass.
pub fn body_height_sample_step(page_count: usize, max_samples: usize) -> usize {
    (page_count / max_samples.max(1)).max(1)
}

/// Body-text row height of one page, in that page's own source pixels. This is
/// one calibration sample; see [`combine_body_height_samples`].
pub fn estimate_page_body_height(
    page: &SourcePageImage,
    hints: &[Detection],
    cfg: &RasterReflowConfig,
) -> Option<u32> {
    let regions = regions::detect_regions(page, hints, cfg);
    let mut rows: Vec<TextRow> = Vec::new();
    for region in &regions {
        if region.kind.is_reflowable() {
            rows.extend(rows::detect_rows(page, region, cfg));
        }
    }
    estimate_page_text_metrics(&rows, cfg).map(|m| m.body_row_height)
}

/// Median of the per-page samples from [`estimate_page_body_height`].
pub fn combine_body_height_samples(mut bodies: Vec<u32>) -> Option<u32> {
    if bodies.is_empty() {
        return None;
    }
    bodies.sort_unstable();
    Some(bodies[bodies.len() / 2])
}

/// Conservative fallback for a page that does not look like reflowable prose:
/// preserve the whole page as a single atomic block scaled to fit. (A future
/// version can swap this for margin-cropped resize.)
pub fn page_fallback_flow(page: &SourcePageImage) -> Vec<FlowItem> {
    vec![FlowItem::Figure {
        src: SourceRef {
            page_index: page.page_index,
            rect: page.page_rect(),
        },
        kind: RegionKind::Unknown,
    }]
}

/// Reflow a whole document: produce each source page's flow, compose the
/// combined stream, and paginate into output pages with a source map.
pub fn reflow_document(
    pages: &[SourcePageImage],
    hints_per_page: &[Vec<Detection>],
    cfg: &RasterReflowConfig,
) -> ReflowDocument {
    let mut full_flow: Vec<FlowItem> = Vec::new();
    let mut confidence: Vec<ReflowConfidence> = Vec::with_capacity(pages.len());

    for (pi, page) in pages.iter().enumerate() {
        let hints: &[Detection] = hints_per_page.get(pi).map(|v| v.as_slice()).unwrap_or(&[]);
        let (flow, conf) = reflow_page_flow(page, hints, cfg);
        full_flow.extend(flow);
        // Source-page boundary is a safe break between flows.
        full_flow.push(FlowItem::RegionBreak);
        confidence.push(conf);
    }

    paginate_document_flow(full_flow, confidence, cfg)
}

/// Compose and paginate an already-collected document flow.
///
/// A streaming caller builds `full_flow` one source page at a time — appending
/// each page's [`reflow_page_flow`] output plus a [`FlowItem::RegionBreak`] —
/// and then calls this once. The flow holds only rectangles, so the source
/// images do not have to stay in memory to reach this point.
pub fn paginate_document_flow(
    full_flow: Vec<FlowItem>,
    confidence: Vec<ReflowConfidence>,
    cfg: &RasterReflowConfig,
) -> ReflowDocument {
    let blocks = compose::compose(&full_flow, cfg);
    let (out_pages, source_map) = output::paginate(&blocks, cfg, 0);

    ReflowDocument {
        pages: out_pages,
        source_map,
        confidence,
    }
}

/// Combine row-count and row-height regularity into a 0.0..=1.0 confidence,
/// blended according to `cfg.confidence_row_norm`/`confidence_row_weight`
/// (regularity gets the complementary `1.0 - confidence_row_weight`).
fn score_confidence(
    rows: &[TextRow],
    text_region_count: usize,
    cfg: &RasterReflowConfig,
) -> ReflowConfidence {
    let row_count = rows.len();
    let cv = rows::row_height_cv(rows);
    let row_factor = (row_count as f32 / cfg.confidence_row_norm).min(1.0);
    let reg_factor = 1.0 - cv.min(1.0);
    let row_weight = cfg.confidence_row_weight;
    let score = if text_region_count == 0 {
        0.0
    } else {
        row_weight * row_factor + (1.0 - row_weight) * reg_factor
    };
    ReflowConfidence {
        row_count,
        text_region_count,
        row_height_cv: cv,
        score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Luma};

    /// Synthetic single-column page: `n` evenly spaced rows, each made of a few
    /// word-like ink runs, on a white background.
    fn synthetic_prose_page(page_index: usize) -> SourcePageImage {
        let (w, h) = (300u32, 400u32);
        let mut img = GrayImage::from_pixel(w, h, Luma([255]));
        let row_h = 10u32;
        let pitch = 24u32;
        let mut y = 20u32;
        while y + row_h < h - 20 {
            // three words per row with wide gaps
            for &(x0, x1) in &[(20u32, 90u32), (110, 180), (200, 270)] {
                for yy in y..y + row_h {
                    for xx in x0..x1 {
                        img.put_pixel(xx, yy, Luma([0]));
                    }
                }
            }
            y += pitch;
        }
        SourcePageImage {
            page_index,
            gray: img,
            rgb: None,
            render_dpi: 300.0,
            page_pts: (w as f32, h as f32),
        }
    }

    fn cfg() -> RasterReflowConfig {
        RasterReflowConfig {
            adaptive_threshold: false,
            ink_threshold: 128,
            despeckle_min_neighbours: 0,
            min_row_height: 3,
            page_width: 400,
            page_height: 600,
            margin: 20,
            target_text_height: 30,
            ..Default::default()
        }
    }

    #[test]
    fn single_page_reflow_produces_flow_and_confidence() {
        let page = synthetic_prose_page(0);
        let (flow, conf) = reflow_page_flow(&page, &[], &cfg());
        // Should detect many rows worth of words.
        let word_count = flow
            .iter()
            .filter(|f| matches!(f, FlowItem::WordBitmap { .. }))
            .count();
        assert!(word_count >= 9, "expected several words, got {word_count}");
        assert!(conf.row_count >= 5, "rows: {}", conf.row_count);
        assert!(conf.score > 0.0);
    }

    #[test]
    fn document_reflow_paginates_with_source_map() {
        let pages = vec![synthetic_prose_page(0), synthetic_prose_page(1)];
        let hints: Vec<Vec<Detection>> = vec![Vec::new(), Vec::new()];
        let doc = reflow_document(&pages, &hints, &cfg());

        assert!(!doc.pages.is_empty(), "should produce output pages");
        assert!(!doc.source_map.placements.is_empty());
        assert_eq!(doc.confidence.len(), 2);

        // Every placement references a valid source page and sits in page bounds.
        for page in &doc.pages {
            let b = page.bounds();
            for it in &page.items {
                assert!(it.out_rect.right() <= b.w);
                assert!(it.out_rect.bottom() <= b.h);
                assert!(it.src.page_index <= 1);
            }
        }
    }

    #[test]
    fn blank_page_produces_no_placements() {
        // A fully blank page must contribute nothing to the document: no spurious
        // rows from scanner texture, no atomic block, no wasted output page.
        let (w, h) = (200u32, 200u32);
        let blank = SourcePageImage {
            page_index: 0,
            gray: GrayImage::from_pixel(w, h, Luma([255])),
            rgb: None,
            render_dpi: 300.0,
            page_pts: (w as f32, h as f32),
        };
        let doc = reflow_document(&[blank], &[Vec::new()], &cfg());
        assert!(doc.confidence[0].score < cfg().fallback_confidence);
        assert!(
            doc.source_map.placements.is_empty(),
            "blank page should place nothing"
        );
        assert!(
            doc.pages.iter().all(|p| p.items.is_empty()),
            "blank page should not fill any output page"
        );
    }

    #[test]
    fn image_hint_is_preserved_as_figure() {
        // ONNX labels an image region → must be preserved atomically from the
        // RGB source, not binarized as word bitmaps. Real photographs always
        // produce row-like pixel structure so running row detection on them
        // causes them to be thresholded to black-and-white, which destroys the image.
        let page = synthetic_prose_page(0);
        let hints = vec![Detection {
            class_id: crate::types::class_id_for("image").unwrap(),
            class_name: Some("image".into()),
            confidence: 0.95,
            bbox: [10.0, 10.0, 290.0, 390.0],
            category: crate::types::ContentCategory::Image,
            context: None,
        }];
        let (flow, _conf) = reflow_page_flow(&page, &hints, &cfg());
        let word_count = flow
            .iter()
            .filter(|f| matches!(f, FlowItem::WordBitmap { .. }))
            .count();
        assert_eq!(
            word_count, 0,
            "image-hinted region must not produce word bitmaps"
        );
        assert!(
            flow.iter().any(|f| matches!(
                f,
                FlowItem::Figure {
                    kind: RegionKind::Figure,
                    ..
                }
            )),
            "image-hinted region must be preserved as an atomic figure"
        );
    }

    #[test]
    fn blank_image_hint_is_skipped() {
        // A blank page that ONNX mislabels as "image" has no ink: it must not be
        // preserved as an atomic block (that block would be Otsu-binarized into a
        // solid-black noise page). It should produce nothing at all.
        let (w, h) = (300u32, 400u32);
        let blank = SourcePageImage {
            page_index: 0,
            gray: GrayImage::from_pixel(w, h, Luma([255])),
            rgb: None,
            render_dpi: 300.0,
            page_pts: (w as f32, h as f32),
        };
        let hints = vec![Detection {
            class_id: crate::types::class_id_for("image").unwrap(),
            class_name: Some("image".into()),
            confidence: 0.95,
            bbox: [10.0, 10.0, 290.0, 390.0],
            category: crate::types::ContentCategory::Image,
            context: None,
        }];
        let (flow, _conf) = reflow_page_flow(&blank, &hints, &cfg());
        assert!(
            !flow.iter().any(|f| matches!(f, FlowItem::Figure { .. })),
            "a blank image-hinted region must be skipped, not preserved as a block"
        );
        assert!(
            !flow
                .iter()
                .any(|f| matches!(f, FlowItem::WordBitmap { .. })),
            "a blank region must not produce words from scanner noise"
        );
    }

    #[test]
    fn page_text_metrics_trim_outlier_row_heights() {
        let rows = vec![
            TextRow {
                rect: PxRect::new(0, 0, 10, 8),
                text_height: 8,
                gap_above: None,
                region_id: 0,
                reading_order: 0,
            },
            TextRow {
                rect: PxRect::new(0, 12, 10, 10),
                text_height: 10,
                gap_above: Some(4),
                region_id: 0,
                reading_order: 1,
            },
            TextRow {
                rect: PxRect::new(0, 40, 10, 22),
                text_height: 22,
                gap_above: Some(18),
                region_id: 0,
                reading_order: 2,
            },
            TextRow {
                rect: PxRect::new(0, 64, 10, 10),
                text_height: 10,
                gap_above: Some(2),
                region_id: 0,
                reading_order: 3,
            },
            TextRow {
                rect: PxRect::new(0, 78, 10, 10),
                text_height: 10,
                gap_above: Some(4),
                region_id: 0,
                reading_order: 4,
            },
            TextRow {
                rect: PxRect::new(0, 92, 10, 9),
                text_height: 9,
                gap_above: Some(4),
                region_id: 0,
                reading_order: 5,
            },
        ];
        let metrics = estimate_page_text_metrics(&rows, &cfg()).expect("metrics");
        assert_eq!(metrics.body_row_height, 10);
        assert_eq!(metrics.median_gap, 4);
    }
}
