//! Coarse region detection.
//!
//! Two cooperating paths:
//!   * **Hint path** — ingest ONNX layout [`Detection`]s as high-level region
//!     hints (text / figure / table / caption).
//!   * **Geometry path** — k2pdfopt-style whitespace/projection analysis that
//!     detects columns from vertical gullies. Used as the fallback and as the
//!     corrective path when no usable hints exist.
//!
//! Output is an explicit, reading-order-sorted list of [`ReflowRegion`]s. For
//! the MVP, reading order targets single-column then simple multi-column
//! (left-to-right column, top-to-bottom within a column) layouts.

#![allow(dead_code)]

use super::analyze::{self, blank_threshold};
use super::config::RasterReflowConfig;
use super::types::{InkMask, PxRect, ReflowRegion, RegionKind, SourcePageImage};
use crate::engine::Detection;

/// Detect content columns within `mask` by locating wide vertical whitespace
/// gullies. Returns one full-height rectangle per column (page coordinates),
/// ordered left-to-right, capped to `cfg.max_columns` (widest kept).
pub fn detect_columns(mask: &InkMask, cfg: &RasterReflowConfig) -> Vec<PxRect> {
    if mask.width == 0 || mask.height == 0 {
        return Vec::new();
    }
    let vproj = analyze::vertical_projection(mask);
    let blank = blank_threshold(&vproj, cfg.blank_ink_frac);

    // A gully wide enough to separate columns.
    let gully = ((mask.width as f32) * cfg.column_gap_min_frac).round() as u32;
    // Bridge anything narrower than the gully (intra-column spacing); only break
    // at true column gullies. min_gap bridges gaps strictly shorter than gully.
    let min_gap = gully.saturating_sub(1);
    // Discard slivers narrower than a quarter gully.
    let min_len = (gully / 4).max(2);

    let mut bands = analyze::find_bands(&vproj, blank, min_gap, min_len);
    if bands.is_empty() {
        return vec![mask.to_page_rect(PxRect::new(0, 0, mask.width, mask.height))];
    }

    // Keep widest `max_columns`, then restore left-to-right order.
    if bands.len() > cfg.max_columns {
        bands.sort_by_key(|b| std::cmp::Reverse(b.len()));
        bands.truncate(cfg.max_columns);
        bands.sort_by_key(|b| b.start);
    }

    let top = mask.origin.1;
    let height = mask.height;
    bands
        .into_iter()
        .map(|b| PxRect::from_xyxy(b.start, top, b.end, top + height))
        .collect()
}

/// Build regions from ONNX detections. Boxes are clamped to the page, mapped to
/// [`RegionKind`], and sorted into reading order.
pub fn regions_from_hints(
    detections: &[Detection],
    page: &SourcePageImage,
    _cfg: &RasterReflowConfig,
) -> Vec<ReflowRegion> {
    let (w, h) = page.dimensions();
    let mut regions: Vec<ReflowRegion> = detections
        .iter()
        .map(|d| {
            let rect = PxRect::from_f32_bbox(d.bbox, w, h);
            ReflowRegion {
                id: 0, // assigned after sorting
                rect,
                reading_order: 0,
                kind: RegionKind::from_content_category(d.category),
                confidence: d.confidence,
                onnx_label: Some(d.category),
            }
        })
        .filter(|r| !r.rect.is_empty())
        .collect();

    assign_reading_order(&mut regions, w);
    regions
}

/// Fallback region detection from pure pixel geometry: one text region per
/// detected column.
pub fn regions_from_geometry(
    page: &SourcePageImage,
    cfg: &RasterReflowConfig,
) -> Vec<ReflowRegion> {
    let mask = analyze::build_ink_mask(&page.gray, page.page_rect(), cfg);
    let cols = detect_columns(&mask, cfg);
    let (w, _h) = page.dimensions();

    let mut regions: Vec<ReflowRegion> = cols
        .into_iter()
        .map(|rect| ReflowRegion {
            id: 0,
            rect,
            reading_order: 0,
            kind: RegionKind::Text,
            confidence: 0.5,
            onnx_label: None,
        })
        .collect();

    assign_reading_order(&mut regions, w);
    regions
}

/// Top-level region detection. Pixel geometry always runs; ONNX detections are
/// treated as coarse semantic hints and supplemented with geometry regions that
/// are not already substantially covered.
pub fn detect_regions(
    page: &SourcePageImage,
    hints: &[Detection],
    cfg: &RasterReflowConfig,
) -> Vec<ReflowRegion> {
    let geom = regions_from_geometry(page, cfg);
    if !cfg.use_onnx_hints || hints.is_empty() {
        return geom;
    }

    let mut regions = regions_from_hints(hints, page, cfg);
    if regions.is_empty() {
        return geom;
    }

    let page_mask = analyze::build_ink_mask(&page.gray, page.page_rect(), cfg);
    if page_mask.ink_count() > 0 {
        for g in geom {
            // Fraction of `g` covered by the *union* of the hint regions,
            // approximated by summed intersection area (capped at `g`'s area).
            // The previous check used the single best `overlap_fraction(g, h)`,
            // but a full-page geometry column is never mostly covered by one
            // small hint box even when the *collection* of hint boxes blankets
            // it — so the whole page was re-added as a second region and every
            // word emitted twice (duplicated text, ~3x page count). Only add a
            // geometry region for area the hints genuinely leave uncovered.
            let g_area = g.rect.area().max(1) as f32;
            let covered_area: u64 = regions
                .iter()
                .filter_map(|h| g.rect.intersection(&h.rect))
                .map(|i| i.area())
                .sum();
            let covered = (covered_area as f32 / g_area).min(1.0);
            if covered < 0.5 {
                regions.push(g);
            }
        }
    }

    let (w, _h) = page.dimensions();
    assign_reading_order(&mut regions, w);
    regions
}

/// Assign reading order and stable ids. Heuristic: group regions into columns by
/// horizontal band, order columns left-to-right, and within a column order
/// top-to-bottom. `page_width` calibrates the column-grouping tolerance.
pub fn assign_reading_order(regions: &mut [ReflowRegion], page_width: u32) {
    if regions.is_empty() {
        return;
    }
    // Column key: bucket center-x into coarse columns. Tolerance ~ 15% of page.
    let tol = (page_width as f32 * 0.15).max(1.0);
    regions.sort_by(|a, b| {
        let ca = a.rect.center_x() as f32;
        let cb = b.rect.center_x() as f32;
        if (ca - cb).abs() > tol {
            ca.partial_cmp(&cb).unwrap()
        } else {
            a.rect.y.cmp(&b.rect.y)
        }
    });
    for (ro, region) in regions.iter_mut().enumerate() {
        region.reading_order = ro;
        region.id = ro as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ContentCategory;
    use image::{GrayImage, Luma};

    fn blank_page(w: u32, h: u32) -> GrayImage {
        GrayImage::from_pixel(w, h, Luma([255]))
    }

    /// Fill a rectangle with ink (black).
    fn fill(img: &mut GrayImage, r: PxRect) {
        for y in r.y..r.bottom() {
            for x in r.x..r.right() {
                img.put_pixel(x, y, Luma([0]));
            }
        }
    }

    fn page(gray: GrayImage) -> SourcePageImage {
        let (w, h) = gray.dimensions();
        SourcePageImage {
            page_index: 0,
            gray,
            rgb: None,
            render_dpi: 300.0,
            page_pts: (w as f32, h as f32),
        }
    }

    #[test]
    fn single_column_detected_as_one() {
        let mut img = blank_page(200, 100);
        fill(&mut img, PxRect::new(20, 10, 160, 80));
        let cfg = RasterReflowConfig {
            adaptive_threshold: false,
            ink_threshold: 128,
            despeckle_min_neighbours: 0,
            ..Default::default()
        };
        let mask = analyze::build_ink_mask(&img, PxRect::new(0, 0, 200, 100), &cfg);
        let cols = detect_columns(&mask, &cfg);
        assert_eq!(cols.len(), 1);
    }

    #[test]
    fn two_columns_detected_with_wide_gully() {
        let mut img = blank_page(200, 100);
        // left block 10..80, wide gully, right block 120..190
        fill(&mut img, PxRect::new(10, 10, 70, 80));
        fill(&mut img, PxRect::new(120, 10, 70, 80));
        let cfg = RasterReflowConfig {
            adaptive_threshold: false,
            ink_threshold: 128,
            despeckle_min_neighbours: 0,
            max_columns: 2,
            column_gap_min_frac: 0.1, // 20px gully threshold
            ..Default::default()
        };
        let mask = analyze::build_ink_mask(&img, PxRect::new(0, 0, 200, 100), &cfg);
        let cols = detect_columns(&mask, &cfg);
        assert_eq!(cols.len(), 2);
        assert!(cols[0].x < cols[1].x, "columns ordered left-to-right");
    }

    #[test]
    fn hints_take_priority_and_sort_reading_order() {
        let detections = vec![
            Detection {
                class_id: crate::types::class_id_for("text").unwrap(),
                class_name: Some("text".into()),
                confidence: 0.9,
                bbox: [10.0, 60.0, 90.0, 95.0], // lower-left
                category: ContentCategory::Text,
                context: None,
            },
            Detection {
                class_id: crate::types::class_id_for("text").unwrap(),
                class_name: Some("text".into()),
                confidence: 0.9,
                bbox: [10.0, 10.0, 90.0, 40.0], // upper-left
                category: ContentCategory::Text,
                context: None,
            },
        ];
        let p = page(blank_page(100, 100));
        let cfg = RasterReflowConfig::default();
        let regions = detect_regions(&p, &detections, &cfg);
        assert_eq!(regions.len(), 2);
        // upper region should read first
        assert_eq!(regions[0].reading_order, 0);
        assert!(regions[0].rect.y < regions[1].rect.y);
        assert_eq!(regions[0].id, 0);
        assert_eq!(regions[1].id, 1);
    }

    #[test]
    fn reading_order_left_column_before_right() {
        let mut regions = vec![
            ReflowRegion {
                id: 0,
                rect: PxRect::new(60, 0, 30, 30), // right col, top
                reading_order: 0,
                kind: RegionKind::Text,
                confidence: 1.0,
                onnx_label: None,
            },
            ReflowRegion {
                id: 0,
                rect: PxRect::new(0, 40, 30, 30), // left col, bottom
                reading_order: 0,
                kind: RegionKind::Text,
                confidence: 1.0,
                onnx_label: None,
            },
            ReflowRegion {
                id: 0,
                rect: PxRect::new(0, 0, 30, 30), // left col, top
                reading_order: 0,
                kind: RegionKind::Text,
                confidence: 1.0,
                onnx_label: None,
            },
        ];
        assign_reading_order(&mut regions, 100);
        // Expect: left-top, left-bottom, right-top
        assert_eq!(regions[0].rect, PxRect::new(0, 0, 30, 30));
        assert_eq!(regions[1].rect, PxRect::new(0, 40, 30, 30));
        assert_eq!(regions[2].rect, PxRect::new(60, 0, 30, 30));
        // monotonic
        for w in regions.windows(2) {
            assert!(w[0].reading_order < w[1].reading_order);
        }
    }

    #[test]
    fn geometry_fallback_when_no_hints() {
        let mut img = blank_page(120, 80);
        fill(&mut img, PxRect::new(10, 10, 100, 60));
        let p = page(img);
        let cfg = RasterReflowConfig {
            adaptive_threshold: false,
            ink_threshold: 128,
            despeckle_min_neighbours: 0,
            ..Default::default()
        };
        let regions = detect_regions(&p, &[], &cfg);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].kind, RegionKind::Text);
        assert!(regions[0].onnx_label.is_none());
    }
}
