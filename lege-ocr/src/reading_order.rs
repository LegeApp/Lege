//! Column-aware reading-order reconstruction for text regions.
//!
//! Layout detection emits regions in detection order, not reading order. A
//! naive top-to-bottom, left-to-right sort interleaves the columns of a
//! multi-column page (and weaves sidebars into the body text). This module
//! orders regions the way a human reads them: full-width bands (titles,
//! headers, footers, figures spanning the page) split the page into vertical
//! sections; within each section the remaining regions are grouped into
//! columns ordered left-to-right, each column read top-to-bottom.

use crate::types::TextRegion;

/// A region is treated as a full-width band (a horizontal separator between
/// column groups) when it spans at least this fraction of the page width.
const FULL_WIDTH_FRACTION: f32 = 0.65;

/// Two regions belong to the same column when their horizontal spans overlap
/// by at least this fraction of the narrower span.
const COLUMN_OVERLAP_FRACTION: f32 = 0.30;

/// Compute a reading order over `regions`, returning region indices in the
/// order they should be read. Handles single-column, multi-column, and mixed
/// (full-width header over columns) layouts.
pub fn order_regions(regions: &[TextRegion], page_width: u32) -> Vec<usize> {
    let bboxes: Vec<_> = regions.iter().map(|region| region.bbox_highres).collect();
    order_bboxes(&bboxes, page_width)
}

/// Compute reading order directly from page-global bounding boxes.
///
/// This is shared by region-driven slow OCR and detector-driven PP-OCR so both
/// paths handle columns and full-width separators consistently.
pub fn order_bboxes(bboxes: &[[u32; 4]], page_width: u32) -> Vec<usize> {
    if bboxes.len() <= 1 {
        return (0..bboxes.len()).collect();
    }

    let full_width_threshold = page_width as f32 * FULL_WIDTH_FRACTION;

    // Sort top-to-bottom (ties broken left-to-right) so full-width bands are
    // encountered in vertical order and partition the page cleanly.
    let mut by_y: Vec<usize> = (0..bboxes.len()).collect();
    by_y.sort_by_key(|&i| (bboxes[i][1], bboxes[i][0]));

    let mut order = Vec::with_capacity(bboxes.len());
    let mut band: Vec<usize> = Vec::new();

    for &idx in &by_y {
        let bbox = bboxes[idx];
        let width = bbox[2].saturating_sub(bbox[0]) as f32;
        if width >= full_width_threshold {
            // Flush the accumulated column group before the separator, then
            // emit the full-width region itself.
            flush_band(&mut band, bboxes, &mut order);
            order.push(idx);
        } else {
            band.push(idx);
        }
    }
    flush_band(&mut band, bboxes, &mut order);

    order
}

/// Order one band of (non-full-width) regions into columns and append the
/// result to `order`, clearing `band`.
fn flush_band(band: &mut Vec<usize>, bboxes: &[[u32; 4]], order: &mut Vec<usize>) {
    if band.is_empty() {
        return;
    }
    if band.len() == 1 {
        order.push(band.remove(0));
        return;
    }

    // Greedily cluster into columns by horizontal overlap. Process left-to-right
    // so a column's x-range grows from its leftmost member outward.
    band.sort_by_key(|&i| (bboxes[i][0], bboxes[i][1]));

    struct Column {
        x_min: u32,
        x_max: u32,
        members: Vec<usize>,
    }
    let mut columns: Vec<Column> = Vec::new();

    for &idx in band.iter() {
        let bbox = bboxes[idx];
        let (x1, x2) = (bbox[0], bbox[2]);
        let mut placed = false;
        for col in columns.iter_mut() {
            if horizontal_overlap_ratio(col.x_min, col.x_max, x1, x2) >= COLUMN_OVERLAP_FRACTION {
                col.x_min = col.x_min.min(x1);
                col.x_max = col.x_max.max(x2);
                col.members.push(idx);
                placed = true;
                break;
            }
        }
        if !placed {
            columns.push(Column {
                x_min: x1,
                x_max: x2,
                members: vec![idx],
            });
        }
    }

    // Columns left-to-right; members within a column top-to-bottom.
    columns.sort_by_key(|c| c.x_min);
    for mut col in columns {
        col.members.sort_by_key(|&i| (bboxes[i][1], bboxes[i][0]));
        order.extend(col.members);
    }
    band.clear();
}

/// Overlap of two horizontal spans relative to the narrower span. Returns 0.0
/// when either span is empty or they do not overlap.
fn horizontal_overlap_ratio(a_min: u32, a_max: u32, b_min: u32, b_max: u32) -> f32 {
    let overlap = a_max.min(b_max).saturating_sub(a_min.max(b_min));
    let narrower = a_max.saturating_sub(a_min).min(b_max.saturating_sub(b_min));
    if narrower == 0 {
        0.0
    } else {
        overlap as f32 / narrower as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(id: usize, bbox: [u32; 4]) -> TextRegion {
        TextRegion {
            page_index: 0,
            region_id: id,
            class_name: Some("plain_text".to_string()),
            bbox_highres: bbox,
            confidence: 1.0,
        }
    }

    #[test]
    fn single_region_is_identity() {
        let regions = vec![region(0, [0, 0, 100, 50])];
        assert_eq!(order_regions(&regions, 100), vec![0]);
    }

    #[test]
    fn two_column_page_reads_left_column_first() {
        // Left column: regions 0 (top) and 2 (bottom).
        // Right column: regions 1 (top) and 3 (bottom).
        // Detection order interleaves them; reading order must not.
        let page_w = 1000;
        let regions = vec![
            region(0, [0, 0, 480, 100]),      // left-top
            region(1, [520, 0, 1000, 100]),   // right-top
            region(2, [0, 120, 480, 220]),    // left-bottom
            region(3, [520, 120, 1000, 220]), // right-bottom
        ];
        // Expect: left column top→bottom, then right column top→bottom.
        assert_eq!(order_regions(&regions, page_w), vec![0, 2, 1, 3]);
    }

    #[test]
    fn full_width_header_precedes_columns() {
        let page_w = 1000;
        let regions = vec![
            region(0, [0, 200, 480, 300]),    // left column body
            region(1, [520, 200, 1000, 300]), // right column body
            region(2, [0, 0, 1000, 80]),      // full-width title (detected last)
        ];
        // Title first, then left column, then right column.
        assert_eq!(order_regions(&regions, page_w), vec![2, 0, 1]);
    }

    #[test]
    fn full_width_footer_follows_columns() {
        let page_w = 1000;
        let regions = vec![
            region(0, [0, 0, 480, 300]),    // left column
            region(1, [520, 0, 1000, 300]), // right column
            region(2, [0, 320, 1000, 380]), // full-width footer
        ];
        assert_eq!(order_regions(&regions, page_w), vec![0, 1, 2]);
    }

    #[test]
    fn single_column_preserves_top_to_bottom() {
        let page_w = 1000;
        let regions = vec![
            region(0, [100, 300, 900, 400]),
            region(1, [100, 0, 900, 100]),
            region(2, [100, 150, 900, 250]),
        ];
        assert_eq!(order_regions(&regions, page_w), vec![1, 2, 0]);
    }
}
