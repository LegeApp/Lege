use crate::types::{SegmentationConfidence, SegmentationResult};

/// Parameters for projection-profile line segmentation.
#[derive(Debug, Clone)]
pub struct SegmentParams {
    /// Fraction of bbox size to expand on each side before analysis
    pub expand_pct: f32,
    /// Minimum line height in pixels (lines shorter than this are merged upward)
    pub min_line_height_px: u32,
    /// Maximum plausible line height in pixels (for confidence scoring)
    pub max_line_height_px: u32,
    /// A row is a valley if its smoothed ink count < valley_threshold * local_max
    pub valley_threshold: f32,
    /// Box-filter smoothing radius (rows on each side)
    pub smooth_radius: usize,
    /// Lines with fewer than this many ink pixels are discarded as noise
    pub min_ink_pixels: u32,
}

impl Default for SegmentParams {
    fn default() -> Self {
        Self {
            expand_pct: 0.03,
            min_line_height_px: 8,
            max_line_height_px: 200,
            valley_threshold: 0.15,
            smooth_radius: 2,
            min_ink_pixels: 5,
        }
    }
}

/// Segment a text region into individual line bounding boxes using horizontal
/// projection profile analysis.
///
/// `binary`: flat buffer, one byte per pixel; 0 = ink, 255 = background. Row-major.
/// `image_width`, `image_height`: dimensions of the full image buffer.
/// `region_bbox`: [x1, y1, x2, y2] of the region in that same pixel space.
///
/// Returns `SegmentationResult` with bboxes in the same pixel space.
pub fn segment_region(
    binary: &[u8],
    image_width: u32,
    image_height: u32,
    region_bbox: [u32; 4],
    params: &SegmentParams,
) -> SegmentationResult {
    let [rx1, ry1, rx2, ry2] = region_bbox;

    // Expand by expand_pct, clamped to image bounds
    let dw = ((rx2.saturating_sub(rx1)) as f32 * params.expand_pct) as u32;
    let dh = ((ry2.saturating_sub(ry1)) as f32 * params.expand_pct) as u32;
    let x1 = rx1.saturating_sub(dw);
    let y1 = ry1.saturating_sub(dh);
    let x2 = (rx2 + dw).min(image_width);
    let y2 = (ry2 + dh).min(image_height);

    if x2 <= x1 || y2 <= y1 {
        return SegmentationResult {
            line_bboxes: vec![[rx1, ry1, rx2, ry2]],
            confidence: SegmentationConfidence::Low,
        };
    }

    let w = (x2 - x1) as usize;
    let h = (y2 - y1) as usize;

    let projection =
        horizontal_projection(binary, image_width as usize, x1 as usize, y1 as usize, w, h);
    let smoothed = smooth_box(&projection, params.smooth_radius);
    let adaptive_min_gap = estimate_ink_band_height(&projection)
        .map(|height| (height / 2).max(params.min_line_height_px as usize))
        .unwrap_or(params.min_line_height_px as usize);
    let valleys = find_valleys(&smoothed, params.valley_threshold, adaptive_min_gap);
    let raw_bboxes = valleys_to_bboxes(&valleys, x1, x2, y1, y2);

    // Drop empty/noise bands only after every interval has had an opportunity
    // to prove that it contains ink. Short intervals may be real captions,
    // page numbers, superscripts, or tightly-spaced formula rows.
    let non_empty: Vec<[u32; 4]> = raw_bboxes
        .into_iter()
        .filter(|bbox| count_ink(binary, image_width as usize, bbox) >= params.min_ink_pixels)
        .collect();

    if non_empty.is_empty() {
        return SegmentationResult {
            line_bboxes: vec![[x1, y1, x2, y2]],
            confidence: SegmentationConfidence::Low,
        };
    }

    let confidence = assess_confidence(&non_empty, params);
    SegmentationResult {
        line_bboxes: non_empty,
        confidence,
    }
}

/// Compute per-row ink pixel count within a sub-rectangle of the image.
///
/// Exported for use in debug output (projection CSV).
pub fn horizontal_projection(
    binary: &[u8],
    image_width: usize,
    x1: usize,
    y1: usize,
    w: usize,
    h: usize,
) -> Vec<u32> {
    let mut proj = vec![0u32; h];
    for (row, row_ink) in proj.iter_mut().enumerate() {
        let abs_y = y1 + row;
        let row_start = abs_y * image_width + x1;
        let row_end = row_start + w;
        if row_start >= binary.len() {
            break;
        }
        let row_end = row_end.min(binary.len());
        for &px in &binary[row_start..row_end] {
            if px == 0 {
                *row_ink += 1;
            }
        }
    }
    proj
}

fn smooth_box(proj: &[u32], radius: usize) -> Vec<f32> {
    let n = proj.len();
    let mut out = vec![0.0f32; n];
    for (i, value) in out.iter_mut().enumerate() {
        let lo = i.saturating_sub(radius);
        let hi = (i + radius + 1).min(n);
        let sum: u32 = proj[lo..hi].iter().sum();
        *value = sum as f32 / (hi - lo) as f32;
    }
    out
}

fn estimate_ink_band_height(projection: &[u32]) -> Option<usize> {
    let mut heights = Vec::new();
    let mut start = None;
    for (row, &ink) in projection.iter().enumerate() {
        match (start, ink > 0) {
            (None, true) => start = Some(row),
            (Some(first), false) => {
                heights.push(row - first);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(first) = start {
        heights.push(projection.len() - first);
    }
    heights.retain(|&height| height > 1);
    if heights.is_empty() {
        return None;
    }
    heights.sort_unstable();
    Some(heights[heights.len() / 2])
}

fn find_valleys(smoothed: &[f32], threshold: f32, min_gap_rows: usize) -> Vec<usize> {
    let global_max = smoothed.iter().cloned().fold(0.0f32, f32::max);
    if global_max == 0.0 {
        return Vec::new();
    }
    let cut = global_max * threshold;
    let is_valley: Vec<bool> = smoothed.iter().map(|&v| v <= cut).collect();

    // Collect midpoints of valley runs
    let mut valleys = Vec::new();
    let mut i = 0;
    while i < is_valley.len() {
        if is_valley[i] {
            let start = i;
            while i < is_valley.len() && is_valley[i] {
                i += 1;
            }
            valleys.push(start + (i - start) / 2);
        } else {
            i += 1;
        }
    }

    // Enforce minimum spacing between split points
    let mut filtered: Vec<usize> = Vec::new();
    for v in valleys {
        if filtered.is_empty() || v - *filtered.last().unwrap() >= min_gap_rows {
            filtered.push(v);
        }
    }
    filtered
}

fn valleys_to_bboxes(valleys: &[usize], x1: u32, x2: u32, y1: u32, y2: u32) -> Vec<[u32; 4]> {
    if valleys.is_empty() {
        return vec![[x1, y1, x2, y2]];
    }

    let splits: Vec<u32> = std::iter::once(y1)
        .chain(valleys.iter().map(|&v| y1 + v as u32))
        .chain(std::iter::once(y2))
        .collect();

    splits
        .windows(2)
        .filter_map(|w| {
            let (a, b) = (w[0], w[1]);
            if b > a { Some([x1, a, x2, b]) } else { None }
        })
        .collect()
}

fn count_ink(binary: &[u8], image_width: usize, bbox: &[u32; 4]) -> u32 {
    let x1 = bbox[0] as usize;
    let y1 = bbox[1] as usize;
    let w = (bbox[2].saturating_sub(bbox[0])) as usize;
    let h = (bbox[3].saturating_sub(bbox[1])) as usize;
    let mut count = 0u32;
    for row in 0..h {
        let abs_y = y1 + row;
        let start = abs_y * image_width + x1;
        let end = (start + w).min(binary.len());
        if start >= binary.len() {
            break;
        }
        for &px in &binary[start..end] {
            if px == 0 {
                count += 1;
            }
        }
    }
    count
}

fn assess_confidence(bboxes: &[[u32; 4]], params: &SegmentParams) -> SegmentationConfidence {
    let n = bboxes.len();
    if n == 0 {
        return SegmentationConfidence::Low;
    }
    let heights: Vec<u32> = bboxes
        .iter()
        .map(|bbox| bbox[3].saturating_sub(bbox[1]))
        .collect();
    if heights.len() == 1 && heights[0] > params.min_line_height_px.saturating_mul(4) {
        return SegmentationConfidence::Low;
    }

    let bad = heights
        .iter()
        .filter(|&&height| height < params.min_line_height_px || height > params.max_line_height_px)
        .count();
    let frac = bad as f32 / n as f32;
    if frac > 0.5 {
        SegmentationConfidence::Low
    } else if frac > 0.2 {
        SegmentationConfidence::Medium
    } else {
        let min_height = *heights.iter().min().unwrap_or(&1);
        let max_height = *heights.iter().max().unwrap_or(&1);
        if min_height == 0 || max_height > min_height.saturating_mul(3) {
            SegmentationConfidence::Medium
        } else {
            SegmentationConfidence::High
        }
    }
}

/// Estimate the baseline y (crop-local) from a binary single-line crop.
/// Returns `None` if the image is empty.
pub fn estimate_baseline(binary: &[u8], width: usize, height: usize) -> Option<u32> {
    if height < 4 || binary.is_empty() {
        return None;
    }
    let proj = horizontal_projection(binary, width, 0, 0, width, height);
    let max_ink = *proj.iter().max()?;
    if max_ink == 0 {
        return None;
    }
    let threshold = (max_ink / 4).max(1);
    let search_start = height / 4;
    proj[search_start..]
        .iter()
        .enumerate()
        .rev()
        .find(|(_, v)| **v >= threshold)
        .map(|(i, _)| (search_start + i) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_binary(width: usize, height: usize, ink_rows: &[usize]) -> Vec<u8> {
        let mut buf = vec![255u8; width * height];
        for &row in ink_rows {
            if row < height {
                for col in 5..(width.saturating_sub(5)) {
                    buf[row * width + col] = 0;
                }
            }
        }
        buf
    }

    #[test]
    fn projection_counts_ink() {
        let width = 100;
        let binary = make_binary(width, 30, &[10, 11, 12]);
        let proj = horizontal_projection(&binary, width, 0, 0, width, 30);
        assert_eq!(proj[10], 90);
        assert_eq!(proj[0], 0);
    }

    #[test]
    fn single_line_no_split() {
        let width = 100;
        let height = 30;
        let binary = make_binary(width, height, &[14, 15]);
        let result = segment_region(
            &binary,
            width as u32,
            height as u32,
            [0, 0, width as u32, height as u32],
            &SegmentParams::default(),
        );
        assert_eq!(result.line_bboxes.len(), 1);
    }

    #[test]
    fn two_lines_split() {
        let width = 100;
        let height = 80;
        let binary = make_binary(width, height, &[10, 11, 50, 51]);
        let result = segment_region(
            &binary,
            width as u32,
            height as u32,
            [0, 0, width as u32, height as u32],
            &SegmentParams::default(),
        );
        assert!(
            result.line_bboxes.len() >= 2,
            "expected >=2 lines, got {} with bboxes {:?}",
            result.line_bboxes.len(),
            result.line_bboxes
        );
    }

    #[test]
    fn estimate_baseline_simple() {
        let width = 50;
        let height = 30;
        let binary = make_binary(width, height, &[20, 21]);
        let baseline = estimate_baseline(&binary, width, height);
        assert!(baseline.is_some());
        assert!(baseline.unwrap() >= 20);
    }

    #[test]
    fn short_intervals_are_not_discarded_before_ink_check() {
        let boxes = valleys_to_bboxes(&[5, 10], 0, 100, 0, 30);
        assert_eq!(
            boxes,
            vec![[0, 0, 100, 5], [0, 5, 100, 10], [0, 10, 100, 30]]
        );
    }

    #[test]
    fn tall_unsplit_region_has_low_confidence() {
        let result = assess_confidence(&[[0, 0, 100, 80]], &SegmentParams::default());
        assert_eq!(result, SegmentationConfidence::Low);
    }

    #[test]
    fn estimates_typical_ink_band_height() {
        let projection = vec![0, 4, 4, 4, 0, 0, 5, 5, 5, 5, 0];
        assert_eq!(estimate_ink_band_height(&projection), Some(4));
    }
}
