//! Row detection and vertical grouping within a text region.
//!
//! Rows are the primary *safe* breakpoints for composition and pagination, so
//! this stage is deliberately conservative: it detects row bands from the
//! horizontal ink projection, bridges faint-scan dropouts, estimates per-row
//! text height and inter-row gaps, and exposes line-spacing statistics plus the
//! "logical chunk" boundaries (large gaps → region breaks) used downstream.

use super::analyze::{self, blank_threshold};
use super::config::RasterReflowConfig;
use super::types::{PxRect, ReflowRegion, SourcePageImage, TextRow};

/// Detect text rows inside a region. Returned rows are in top-to-bottom order
/// with `gap_above` set (the first row's gap is `None`) and `reading_order`
/// numbered sequentially from 0 within this region.
pub fn detect_rows(
    page: &SourcePageImage,
    region: &ReflowRegion,
    cfg: &RasterReflowConfig,
) -> Vec<TextRow> {
    let mask = analyze::build_ink_mask(&page.gray, region.rect, cfg);
    // Reject near-blank regions (scanner texture / faint bleed). Without this,
    // the horizontal projection finds spurious "rows" in the noise and Otsu
    // binarization later floods the page black.
    let area = (mask.width as u64) * (mask.height as u64);
    if area == 0 || (mask.ink_count() as f32) < (area as f32) * cfg.min_region_ink_frac {
        return Vec::new();
    }
    let hproj = analyze::horizontal_projection(&mask);
    let blank = blank_threshold(&hproj, cfg.blank_ink_frac);

    // Bridge tiny vertical dropouts (broken glyphs) but keep real line gaps.
    let min_gap = 1u32;
    let bands = analyze::find_bands(&hproj, blank, min_gap, cfg.min_row_height);

    let mut rows: Vec<TextRow> = Vec::with_capacity(bands.len());
    let mut prev_bottom: Option<u32> = None;
    for (i, b) in bands.iter().enumerate() {
        let rect = PxRect::from_xyxy(region.rect.x, b.start, region.rect.right(), b.end);
        let gap_above = prev_bottom.map(|pb| b.start.saturating_sub(pb));
        rows.push(TextRow {
            rect,
            text_height: b.len(),
            gap_above,
            region_id: region.id,
            reading_order: i,
        });
        prev_bottom = Some(b.end);
    }
    rows
}

/// Upper median of an unsorted iterator. Returns `None` for empty input.
fn median<T: Ord + Copy>(iter: impl Iterator<Item = T>) -> Option<T> {
    let mut vals: Vec<T> = iter.collect();
    if vals.is_empty() {
        return None;
    }
    vals.sort_unstable();
    Some(vals[vals.len() / 2])
}

/// Median of the `gap_above` values across rows (ignoring the leading `None`).
/// Returns `None` if there are fewer than two rows.
pub fn median_line_gap(rows: &[TextRow]) -> Option<u32> {
    median(rows.iter().filter_map(|r| r.gap_above))
}

/// Median text height across rows. Returns `None` for empty input.
pub fn median_text_height(rows: &[TextRow]) -> Option<u32> {
    median(rows.iter().map(|r| r.text_height))
}

/// Coefficient of variation (stddev / mean) of row heights. A low value implies
/// regular prose; a high value suggests mixed content (headings, figures, junk)
/// and lowers reflow confidence. Returns 0.0 for fewer than two rows.
pub fn row_height_cv(rows: &[TextRow]) -> f32 {
    if rows.len() < 2 {
        return 0.0;
    }
    let n = rows.len() as f32;
    let mean = rows.iter().map(|r| r.text_height as f32).sum::<f32>() / n;
    if mean <= 0.0 {
        return 0.0;
    }
    let var = rows
        .iter()
        .map(|r| {
            let d = r.text_height as f32 - mean;
            d * d
        })
        .sum::<f32>()
        / n;
    var.sqrt() / mean
}

/// Indices (into `rows`) at which a new logical chunk begins because the gap
/// above is much larger than the median line gap. These are safe pagination /
/// region-break points (paragraph breaks, section gaps, heading separations).
/// Index 0 is never reported (it has no gap above).
pub fn chunk_break_indices(rows: &[TextRow], cfg: &RasterReflowConfig) -> Vec<usize> {
    let median = match median_line_gap(rows) {
        Some(m) if m > 0 => m as f32,
        _ => return Vec::new(),
    };
    let threshold = median * cfg.row_break_factor;
    rows.iter()
        .enumerate()
        .skip(1)
        .filter_map(|(i, r)| match r.gap_above {
            Some(g) if (g as f32) > threshold => Some(i),
            _ => None,
        })
        .collect()
}

/// Normalize an inter-row gap for output: capped at `max_gap_factor *
/// median_line_gap` so large source whitespace becomes a bounded break rather
/// than wasted output space.
pub fn normalize_gap(gap: u32, median_line_gap: u32, cfg: &RasterReflowConfig) -> u32 {
    if median_line_gap == 0 {
        return gap;
    }
    let cap = ((median_line_gap as f32) * cfg.max_gap_factor).round() as u32;
    gap.min(cap)
}

#[cfg(test)]
mod tests {
    use super::super::types::RegionKind;
    use super::*;
    use image::{GrayImage, Luma};

    fn page_with_text_rows(w: u32, h: u32, bars: &[(u32, u32)]) -> SourcePageImage {
        let mut img = GrayImage::from_pixel(w, h, Luma([255]));
        for &(y0, y1) in bars {
            for y in y0..y1 {
                for x in 5..w - 5 {
                    img.put_pixel(x, y, Luma([0]));
                }
            }
        }
        SourcePageImage {
            page_index: 0,
            gray: img,
            rgb: None,
            render_dpi: 300.0,
            page_pts: (w as f32, h as f32),
        }
    }

    fn full_region(w: u32, h: u32) -> ReflowRegion {
        ReflowRegion {
            id: 0,
            rect: PxRect::new(0, 0, w, h),
            reading_order: 0,
            kind: RegionKind::Text,
            confidence: 1.0,
            onnx_label: None,
        }
    }

    fn cfg() -> RasterReflowConfig {
        RasterReflowConfig {
            adaptive_threshold: false,
            ink_threshold: 128,
            despeckle_min_neighbours: 0,
            min_row_height: 2,
            ..Default::default()
        }
    }

    #[test]
    fn detects_three_evenly_spaced_rows() {
        // rows of height 6 at y=10,30,50 → gaps of 14
        let page = page_with_text_rows(100, 70, &[(10, 16), (30, 36), (50, 56)]);
        let rows = detect_rows(&page, &full_region(100, 70), &cfg());
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].gap_above, None);
        assert_eq!(rows[1].gap_above, Some(14));
        assert_eq!(rows[2].gap_above, Some(14));
        assert_eq!(rows[0].text_height, 6);
        assert_eq!(rows[0].reading_order, 0);
        assert_eq!(rows[2].reading_order, 2);
    }

    #[test]
    fn median_line_gap_and_height() {
        let page = page_with_text_rows(100, 80, &[(10, 16), (30, 36), (50, 56)]);
        let rows = detect_rows(&page, &full_region(100, 80), &cfg());
        assert_eq!(median_line_gap(&rows), Some(14));
        assert_eq!(median_text_height(&rows), Some(6));
    }

    #[test]
    fn chunk_break_on_large_gap() {
        // tight rows then a big gap before the last row (paragraph break)
        let page = page_with_text_rows(100, 140, &[(10, 16), (26, 32), (42, 48), (100, 106)]);
        let rows = detect_rows(&page, &full_region(100, 140), &cfg());
        assert_eq!(rows.len(), 4);
        let breaks = chunk_break_indices(&rows, &cfg());
        assert_eq!(
            breaks,
            vec![3],
            "the large gap before row 3 is a chunk break"
        );
    }

    #[test]
    fn normalize_gap_caps_large_whitespace() {
        let c = cfg();
        // median 10, factor 1.5 → cap 15
        assert_eq!(normalize_gap(100, 10, &c), 15);
        assert_eq!(normalize_gap(8, 10, &c), 8);
    }

    #[test]
    fn row_height_cv_zero_for_uniform() {
        let rows = vec![
            TextRow {
                rect: PxRect::new(0, 0, 10, 6),
                text_height: 6,
                gap_above: None,
                region_id: 0,
                reading_order: 0,
            },
            TextRow {
                rect: PxRect::new(0, 10, 10, 6),
                text_height: 6,
                gap_above: Some(4),
                region_id: 0,
                reading_order: 1,
            },
        ];
        assert!(row_height_cv(&rows) < 1e-6);
    }
}
