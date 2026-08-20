//! Post-NMS correction for PP-DocLayout-M underdetection.
//!
//! DocLayout-M on this corpus misses whole paragraphs and clips text
//! horizontally; image boxes are occasionally tight but almost never too
//! large. OCR-det (DBNet) already finds line boxes independently of layout
//! on the Paddle fast path, so this module borrows those line boxes — plus a
//! cheap ink projection — to:
//!
//! - keep the model's vertical bands
//! - expand text/table boxes horizontally to cover the ink / OCR lines in
//!   that band
//! - insert synthetic text boxes for uncovered vertical gaps that still
//!   contain OCR lines or text-like ink
//! - grow image boxes outward to nearby illustration pixels, and recover
//!   missed illustrations in uncovered high-variance regions
//! - demote line-art image boxes to text so they are not photo-overlaid
//!
//! Paddle page OCR does **not** crop to these boxes. Slow OCR, Tesseract
//! region OCR, reflow, crop, and image-overlay encoding do.

use image::RgbImage;

use crate::engine::Detection;
use crate::types::{ContentCategory, LABEL_CLASSIFIER, class_id_for};

const INK_LUMA: u8 = 200;
const MIN_BOX_PX: f32 = 2.0;
const SYNTHETIC_TEXT_CONFIDENCE: f32 = 0.45;
const SYNTHETIC_IMAGE_CONFIDENCE: f32 = 0.40;
const LINE_BAND_IOY: f32 = 0.35;
const IMAGE_PAD_FRAC: f32 = 0.012;
const IMAGE_GROW_FRAC: f32 = 0.03;
const VARIANCE_BLOCK: u32 = 16;
const ILLUSTRATION_VAR: f32 = 380.0;
const ILLUSTRATION_MIN_PAGE_FRAC: f32 = 0.06;

/// Axis-aligned OCR-det (or test-double) line box in the same pixel space
/// as the layout detections.
#[derive(Debug, Clone, Copy)]
pub struct LineHint {
    pub bbox: [f32; 4],
    pub score: f32,
}

/// Correct raw post-NMS DocLayout boxes in place using the page raster and,
/// when the Paddle detector is available, OCR-det line boxes.
pub fn correct_layout_detections(detections: &mut Vec<Detection>, image: &RgbImage) {
    if image.width() == 0 || image.height() == 0 {
        return;
    }
    let luma = rgb_to_luma(image);
    let lines = collect_ocr_line_hints(image);
    correct_with_signals(detections, &luma, image.width(), image.height(), &lines);
    demote_line_art_images(detections, image);
}

/// Testable core: same correction with caller-supplied luma and line hints.
pub fn correct_with_signals(
    detections: &mut Vec<Detection>,
    luma: &[u8],
    width: u32,
    height: u32,
    lines: &[LineHint],
) {
    if width == 0 || height == 0 || luma.len() < (width as usize) * (height as usize) {
        return;
    }

    expand_text_horizontal(detections, luma, width, height, lines);
    grow_image_boxes(detections, luma, width, height);
    fill_uncovered_text_bands(detections, luma, width, height, lines);
    recover_missed_image_regions(detections, luma, width, height);
}

/// Reclassify image boxes that are line art as text. Needs the RGB page so
/// chroma can veto maps and colour plates. Called after geometry correction.
fn demote_line_art_images(detections: &mut [Detection], image: &RgbImage) {
    let rgb = image.as_raw();
    let width = image.width() as usize;
    let height = image.height() as usize;
    for det in detections.iter_mut() {
        if !LABEL_CLASSIFIER.is_image_label(det) {
            continue;
        }
        if !crate::content_class::region_is_line_art(rgb, width, height, det.bbox) {
            continue;
        }
        // Keep the model class id/name so visualize and traces still say
        // "image"; flip only the category so overlay/premask treat it as text.
        det.category = ContentCategory::Text;
    }
}

/// Invert RGB for a second layout pass (dark engravings, reversed plates).
pub fn invert_rgb(image: &RgbImage) -> RgbImage {
    let mut out = image.clone();
    for px in out.pixels_mut() {
        px.0[0] = 255 - px.0[0];
        px.0[1] = 255 - px.0[1];
        px.0[2] = 255 - px.0[2];
    }
    out
}

/// True when a second, inverted layout pass is worth the extra inference.
///
/// Disabled with `LEGE_LAYOUT_IMAGE_REDETECT=0`. Forced on with `=1`.
/// Otherwise runs only when the page has no image box but still contains a
/// large uncovered high-variance region.
pub fn should_redetect_images(detections: &[Detection], image: &RgbImage) -> bool {
    match std::env::var("LEGE_LAYOUT_IMAGE_REDETECT") {
        Ok(value) if value == "0" || value.eq_ignore_ascii_case("false") => return false,
        Ok(value) if value == "1" || value.eq_ignore_ascii_case("true") => return true,
        _ => {}
    }
    if detections
        .iter()
        .any(|d| LABEL_CLASSIFIER.is_image_label(d))
    {
        return false;
    }
    let luma = rgb_to_luma(image);
    uncovered_illustration_frac(detections, &luma, image.width(), image.height())
        >= ILLUSTRATION_MIN_PAGE_FRAC
}

/// Keep image-class boxes from `extra` that do not land on substantive text
/// and do not largely duplicate an existing image.
pub fn merge_image_redetects(
    detections: &mut Vec<Detection>,
    extra: &[Detection],
    image: &RgbImage,
) {
    let w = image.width();
    let h = image.height();
    for det in extra {
        if !LABEL_CLASSIFIER.is_image_label(det) {
            continue;
        }
        if image_overlaps_substantive_text(det, detections) {
            continue;
        }
        if detections.iter().any(|existing| {
            LABEL_CLASSIFIER.is_image_label(existing) && iou(&existing.bbox, &det.bbox) > 0.35
        }) {
            continue;
        }
        if bbox_is_tiny(&det.bbox) {
            continue;
        }
        let mut kept = det.clone();
        kept.bbox = clamp_bbox(kept.bbox, w, h);
        detections.push(kept);
    }
}

fn collect_ocr_line_hints(image: &RgbImage) -> Vec<LineHint> {
    match std::env::var("LEGE_LAYOUT_OCR_DET") {
        Ok(value) if value == "0" || value.eq_ignore_ascii_case("false") => return Vec::new(),
        _ => {}
    }
    collect_ocr_line_hints_inner(image)
}

#[cfg(lege_paddle_ocr)]
fn collect_ocr_line_hints_inner(image: &RgbImage) -> Vec<LineHint> {
    let engine = match lege_ocr::engine_paddle::PaddleOcrEngine::shared_embedded() {
        Ok(engine) => engine,
        Err(_) => return Vec::new(),
    };
    let gray = match image::GrayImage::from_raw(image.width(), image.height(), rgb_to_luma(image)) {
        Some(gray) => gray,
        None => return Vec::new(),
    };
    match engine.detect_gray_boxes(&gray) {
        Ok(boxes) => boxes
            .into_iter()
            .filter(|b| b.x1 > b.x0 && b.y1 > b.y0)
            .map(|b| LineHint {
                bbox: [b.x0, b.y0, b.x1, b.y1],
                score: b.score,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(not(lege_paddle_ocr))]
fn collect_ocr_line_hints_inner(_image: &RgbImage) -> Vec<LineHint> {
    Vec::new()
}

fn expand_text_horizontal(
    detections: &mut [Detection],
    luma: &[u8],
    width: u32,
    height: u32,
    lines: &[LineHint],
) {
    let images: Vec<[f32; 4]> = detections
        .iter()
        .filter(|d| LABEL_CLASSIFIER.is_image_label(d))
        .map(|d| d.bbox)
        .collect();

    for det in detections.iter_mut() {
        if !LABEL_CLASSIFIER.is_text_label(det) {
            continue;
        }
        let [x0, y0, x1, y1] = det.bbox;
        if y1 <= y0 {
            continue;
        }

        let mut nx0 = x0;
        let mut nx1 = x1;

        if let Some((ix0, ix1)) = ink_x_range(luma, width, height, y0, y1) {
            nx0 = nx0.min(ix0);
            nx1 = nx1.max(ix1);
        }

        for line in lines {
            if vertical_overlap_frac(y0, y1, line.bbox[1], line.bbox[3]) < LINE_BAND_IOY {
                continue;
            }
            // A line that sits entirely in a different column should not pull
            // this box across a gutter. Require some x-proximity or overlap.
            if horizontal_gap(x0, x1, line.bbox[0], line.bbox[2]) > (x1 - x0).max(16.0) * 0.35 {
                continue;
            }
            nx0 = nx0.min(line.bbox[0]);
            nx1 = nx1.max(line.bbox[2]);
        }

        let (nx0, nx1) = clamp_x_away_from_images(nx0, nx1, y0, y1, &images);
        det.bbox[0] = nx0.clamp(0.0, width as f32);
        det.bbox[2] = nx1.clamp(0.0, width as f32).max(det.bbox[0] + MIN_BOX_PX);
        det.bbox[1] = y0.clamp(0.0, height as f32);
        det.bbox[3] = y1.clamp(0.0, height as f32).max(det.bbox[1] + MIN_BOX_PX);
    }
}

fn grow_image_boxes(detections: &mut [Detection], luma: &[u8], width: u32, height: u32) {
    let blockers: Vec<[f32; 4]> = detections
        .iter()
        .filter(|d| !LABEL_CLASSIFIER.is_image_label(d))
        .map(|d| d.bbox)
        .collect();
    let max_grow = ((width.min(height) as f32) * IMAGE_GROW_FRAC).max(4.0);

    for det in detections.iter_mut() {
        if !LABEL_CLASSIFIER.is_image_label(det) {
            continue;
        }
        let original = det.bbox;
        let mut bbox = original;
        bbox = grow_rect_to_ink(bbox, luma, width, height, max_grow, &blockers);
        let pad = ((width.min(height) as f32) * IMAGE_PAD_FRAC)
            .max(2.0)
            .min(6.0);
        bbox[0] -= pad;
        bbox[1] -= pad;
        bbox[2] += pad;
        bbox[3] += pad;
        bbox = clamp_bbox_away(bbox, &blockers);
        // Blockers may limit newly-added padding, but must never collapse an
        // image box the model already detected. Captions and labels are often
        // legitimately inside maps and plates.
        bbox[0] = bbox[0].min(original[0]);
        bbox[1] = bbox[1].min(original[1]);
        bbox[2] = bbox[2].max(original[2]);
        bbox[3] = bbox[3].max(original[3]);
        det.bbox = clamp_bbox(bbox, width, height);
    }
}

fn fill_uncovered_text_bands(
    detections: &mut Vec<Detection>,
    luma: &[u8],
    width: u32,
    height: u32,
    lines: &[LineHint],
) {
    let occupied = occupied_y_intervals(detections);
    let assigned = assigned_line_mask(detections, lines);
    let mut unused_lines: Vec<LineHint> = lines
        .iter()
        .copied()
        .enumerate()
        .filter(|(i, _)| !assigned[*i])
        .map(|(_, line)| line)
        .collect();
    unused_lines.sort_by(|a, b| a.bbox[1].total_cmp(&b.bbox[1]));

    let mut synthetic = Vec::new();
    // OCR lines that landed in no layout band become their own vertical groups.
    for group in cluster_lines(&unused_lines) {
        if let Some(bbox) = union_bboxes(group.iter().map(|l| l.bbox)) {
            if overlaps_any_image(&bbox, detections) {
                continue;
            }
            synthetic.push(synthetic_text(bbox, width, height));
        }
    }

    // Ink-only gaps: recover a missed paragraph when OCR-det was unavailable
    // or also missed the block.
    let content_top = first_ink_row(luma, width, height).unwrap_or(0.0);
    let content_bottom = last_ink_row(luma, width, height).unwrap_or(height as f32);
    let mut cursor = content_top;
    let mut spans = occupied;
    spans.push((content_bottom, content_bottom));
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));

    let min_gap = typical_line_height(lines, detections, height).max(10.0) * 1.15;
    for (y0, y1) in spans {
        let gap0 = cursor;
        let gap1 = y0;
        cursor = cursor.max(y1);
        if gap1 - gap0 < min_gap {
            continue;
        }
        if gap_already_covered(gap0, gap1, &synthetic) {
            continue;
        }
        if !band_has_text_ink(luma, width, height, gap0, gap1) {
            continue;
        }
        let tight = ink_y_range(luma, width, height, gap0, gap1).unwrap_or((gap0, gap1));
        let x = neighbor_x_range(detections, tight.0, tight.1)
            .or_else(|| ink_x_range(luma, width, height, tight.0, tight.1));
        let Some((x0, x1)) = x else {
            continue;
        };
        let bbox = [x0, tight.0, x1, tight.1];
        if overlaps_any_image(&bbox, detections) {
            continue;
        }
        synthetic.push(synthetic_text(bbox, width, height));
    }

    detections.extend(synthetic);
}

fn recover_missed_image_regions(
    detections: &mut Vec<Detection>,
    luma: &[u8],
    width: u32,
    height: u32,
) {
    if detections
        .iter()
        .any(|d| LABEL_CLASSIFIER.is_image_label(d))
    {
        return;
    }
    let Some(bbox) = illustration_bbox(detections, luma, width, height) else {
        return;
    };
    if image_overlaps_substantive_text(&synthetic_image(bbox, width, height), detections) {
        return;
    }
    detections.push(synthetic_image(bbox, width, height));
}

fn collect_occupied_text_like<'a>(
    detections: &'a [Detection],
) -> impl Iterator<Item = &'a Detection> {
    detections
        .iter()
        .filter(|d| LABEL_CLASSIFIER.is_text_label(d) || LABEL_CLASSIFIER.is_image_label(d))
}

fn occupied_y_intervals(detections: &[Detection]) -> Vec<(f32, f32)> {
    let mut spans: Vec<(f32, f32)> = collect_occupied_text_like(detections)
        .map(|d| (d.bbox[1], d.bbox[3]))
        .filter(|(a, b)| b > a)
        .collect();
    if spans.is_empty() {
        return spans;
    }
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged = vec![spans[0]];
    for span in spans.into_iter().skip(1) {
        let last = merged.last_mut().unwrap();
        if span.0 <= last.1 + 2.0 {
            last.1 = last.1.max(span.1);
        } else {
            merged.push(span);
        }
    }
    merged
}

fn assigned_line_mask(detections: &[Detection], lines: &[LineHint]) -> Vec<bool> {
    lines
        .iter()
        .map(|line| {
            detections.iter().any(|det| {
                LABEL_CLASSIFIER.is_text_label(det)
                    && vertical_overlap_frac(det.bbox[1], det.bbox[3], line.bbox[1], line.bbox[3])
                        >= LINE_BAND_IOY
                    && horizontal_gap(det.bbox[0], det.bbox[2], line.bbox[0], line.bbox[2])
                        <= (det.bbox[2] - det.bbox[0]).max(16.0) * 0.35
            })
        })
        .collect()
}

fn cluster_lines(lines: &[LineHint]) -> Vec<Vec<LineHint>> {
    if lines.is_empty() {
        return Vec::new();
    }
    let heights: Vec<f32> = lines
        .iter()
        .map(|l| (l.bbox[3] - l.bbox[1]).max(1.0))
        .collect();
    let mut sorted_h = heights.clone();
    sorted_h.sort_by(|a, b| a.total_cmp(b));
    let median_h = sorted_h[sorted_h.len() / 2];
    let join_gap = (median_h * 1.35).max(8.0);

    let mut groups: Vec<Vec<LineHint>> = Vec::new();
    let mut current = vec![lines[0]];
    for line in lines.iter().skip(1) {
        let prev = current.last().unwrap();
        let gap = line.bbox[1] - prev.bbox[3];
        let same_column = horizontal_gap(prev.bbox[0], prev.bbox[2], line.bbox[0], line.bbox[2])
            < (prev.bbox[2] - prev.bbox[0]).max(line.bbox[2] - line.bbox[0]) * 0.5;
        if gap <= join_gap && same_column {
            current.push(*line);
        } else {
            groups.push(std::mem::take(&mut current));
            current = vec![*line];
        }
    }
    groups.push(current);
    groups
}

fn typical_line_height(lines: &[LineHint], detections: &[Detection], page_h: u32) -> f32 {
    let mut heights: Vec<f32> = lines
        .iter()
        .map(|l| l.bbox[3] - l.bbox[1])
        .filter(|h| *h > 2.0)
        .collect();
    if heights.is_empty() {
        heights = detections
            .iter()
            .filter(|d| LABEL_CLASSIFIER.is_text_label(d))
            .map(|d| (d.bbox[3] - d.bbox[1]).min(page_h as f32 * 0.08))
            .filter(|h| *h > 2.0)
            .collect();
    }
    if heights.is_empty() {
        return (page_h as f32 * 0.018).max(12.0);
    }
    heights.sort_by(|a, b| a.total_cmp(b));
    heights[heights.len() / 2]
}

fn neighbor_x_range(detections: &[Detection], y0: f32, y1: f32) -> Option<(f32, f32)> {
    let mid = (y0 + y1) * 0.5;
    let mut above: Option<&Detection> = None;
    let mut below: Option<&Detection> = None;
    for det in detections {
        if !LABEL_CLASSIFIER.is_substantive_text(det) && !detection_is_plain_text(det) {
            continue;
        }
        if det.bbox[3] <= mid {
            if above.is_none_or(|other| other.bbox[3] < det.bbox[3]) {
                above = Some(det);
            }
        } else if det.bbox[1] >= mid && below.is_none_or(|other| other.bbox[1] > det.bbox[1]) {
            below = Some(det);
        }
    }
    match (above, below) {
        (Some(a), Some(b)) => Some((a.bbox[0].min(b.bbox[0]), a.bbox[2].max(b.bbox[2]))),
        (Some(a), None) => Some((a.bbox[0], a.bbox[2])),
        (None, Some(b)) => Some((b.bbox[0], b.bbox[2])),
        (None, None) => None,
    }
}

fn detection_is_plain_text(det: &Detection) -> bool {
    crate::types::detection_is_class(det, "text")
}

fn gap_already_covered(y0: f32, y1: f32, synthetic: &[Detection]) -> bool {
    synthetic
        .iter()
        .any(|d| vertical_overlap_frac(y0, y1, d.bbox[1], d.bbox[3]) > 0.55)
}

fn overlaps_any_image(bbox: &[f32; 4], detections: &[Detection]) -> bool {
    detections
        .iter()
        .any(|d| LABEL_CLASSIFIER.is_image_label(d) && iou(bbox, &d.bbox) > 0.15)
}

fn band_has_text_ink(luma: &[u8], width: u32, height: u32, y0: f32, y1: f32) -> bool {
    let y0i = y0.floor().clamp(0.0, height as f32) as u32;
    let y1i = y1.ceil().clamp(0.0, height as f32) as u32;
    if y1i <= y0i {
        return false;
    }
    let mut ink_rows = 0u32;
    for y in y0i..y1i {
        let row = &luma[(y * width) as usize..((y + 1) * width) as usize];
        let ink = row.iter().filter(|&&p| p < INK_LUMA).count();
        if ink * 100 >= width as usize * 4 {
            ink_rows += 1;
        }
    }
    let band = (y1i - y0i).max(1);
    ink_rows * 100 >= band * 12
}

fn ink_x_range(luma: &[u8], width: u32, height: u32, y0: f32, y1: f32) -> Option<(f32, f32)> {
    let y0i = y0.floor().clamp(0.0, height as f32) as usize;
    let y1i = y1.ceil().clamp(0.0, height as f32) as usize;
    if y1i <= y0i {
        return None;
    }
    let band_h = y1i - y0i;
    let min_hits = (band_h / 16).max(1);
    let mut hits = vec![0u32; width as usize];
    for y in y0i..y1i {
        let row = &luma[y * width as usize..(y + 1) * width as usize];
        for (x, &px) in row.iter().enumerate() {
            if px < INK_LUMA {
                hits[x] += 1;
            }
        }
    }
    let xs: Vec<usize> = hits
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, c)| (*c as usize) >= min_hits)
        .map(|(x, _)| x)
        .collect();
    let first = *xs.first()?;
    let last = *xs.last()?;
    if last <= first {
        return None;
    }
    Some((first as f32, (last + 1) as f32))
}

fn ink_y_range(luma: &[u8], width: u32, height: u32, y0: f32, y1: f32) -> Option<(f32, f32)> {
    let y0i = y0.floor().clamp(0.0, height as f32) as u32;
    let y1i = y1.ceil().clamp(0.0, height as f32) as u32;
    let mut first = None;
    let mut last = None;
    for y in y0i..y1i {
        let row = &luma[(y * width) as usize..((y + 1) * width) as usize];
        if row.iter().filter(|&&p| p < INK_LUMA).count() * 100 >= width as usize * 3 {
            if first.is_none() {
                first = Some(y as f32);
            }
            last = Some((y + 1) as f32);
        }
    }
    Some((first?, last?))
}

fn first_ink_row(luma: &[u8], width: u32, height: u32) -> Option<f32> {
    ink_y_range(luma, width, height, 0.0, height as f32).map(|(y0, _)| y0)
}

fn last_ink_row(luma: &[u8], width: u32, height: u32) -> Option<f32> {
    ink_y_range(luma, width, height, 0.0, height as f32).map(|(_, y1)| y1)
}

fn grow_rect_to_ink(
    mut bbox: [f32; 4],
    luma: &[u8],
    width: u32,
    height: u32,
    max_grow: f32,
    blockers: &[[f32; 4]],
) -> [f32; 4] {
    let steps = max_grow.round().max(1.0) as i32;
    for _ in 0..steps {
        let mut grew = false;
        if strip_has_ink(
            luma,
            width,
            height,
            bbox[0] - 1.0,
            bbox[1],
            bbox[0],
            bbox[3],
        ) && !blocked(&[bbox[0] - 1.0, bbox[1], bbox[2], bbox[3]], blockers)
        {
            bbox[0] -= 1.0;
            grew = true;
        }
        if strip_has_ink(
            luma,
            width,
            height,
            bbox[2],
            bbox[1],
            bbox[2] + 1.0,
            bbox[3],
        ) && !blocked(&[bbox[0], bbox[1], bbox[2] + 1.0, bbox[3]], blockers)
        {
            bbox[2] += 1.0;
            grew = true;
        }
        if strip_has_ink(
            luma,
            width,
            height,
            bbox[0],
            bbox[1] - 1.0,
            bbox[2],
            bbox[1],
        ) && !blocked(&[bbox[0], bbox[1] - 1.0, bbox[2], bbox[3]], blockers)
        {
            bbox[1] -= 1.0;
            grew = true;
        }
        if strip_has_ink(
            luma,
            width,
            height,
            bbox[0],
            bbox[3],
            bbox[2],
            bbox[3] + 1.0,
        ) && !blocked(&[bbox[0], bbox[1], bbox[2], bbox[3] + 1.0], blockers)
        {
            bbox[3] += 1.0;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    bbox
}

fn strip_has_ink(luma: &[u8], width: u32, height: u32, x0: f32, y0: f32, x1: f32, y1: f32) -> bool {
    let x0 = x0.floor().clamp(0.0, width.saturating_sub(1) as f32) as u32;
    let y0 = y0.floor().clamp(0.0, height.saturating_sub(1) as f32) as u32;
    let x1 = x1.ceil().clamp(0.0, width as f32).max(x0 as f32 + 1.0) as u32;
    let y1 = y1.ceil().clamp(0.0, height as f32).max(y0 as f32 + 1.0) as u32;
    if x1 <= x0 || y1 <= y0 {
        return false;
    }
    let mut ink = 0u32;
    let mut total = 0u32;
    for y in y0..y1 {
        let row = &luma[(y * width + x0) as usize..(y * width + x1) as usize];
        ink += row.iter().filter(|&&p| p < INK_LUMA).count() as u32;
        total += x1 - x0;
    }
    total > 0 && ink * 8 >= total
}

fn uncovered_illustration_frac(
    detections: &[Detection],
    luma: &[u8],
    width: u32,
    height: u32,
) -> f32 {
    illustration_bbox(detections, luma, width, height)
        .map(|b| ((b[2] - b[0]) * (b[3] - b[1])) / (width as f32 * height as f32).max(1.0))
        .unwrap_or(0.0)
}

fn illustration_bbox(
    detections: &[Detection],
    luma: &[u8],
    width: u32,
    height: u32,
) -> Option<[f32; 4]> {
    let bw = VARIANCE_BLOCK;
    let blocks_x = width.div_ceil(bw);
    let blocks_y = height.div_ceil(bw);
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut hit = 0u32;
    let mut total = 0u32;
    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let x0 = bx * bw;
            let y0 = by * bw;
            let x1 = (x0 + bw).min(width);
            let y1 = (y0 + bw).min(height);
            total += 1;
            let cx = (x0 + x1) as f32 * 0.5;
            let cy = (y0 + y1) as f32 * 0.5;
            if detections.iter().any(|d| point_in_bbox(cx, cy, &d.bbox)) {
                continue;
            }
            if block_variance(luma, width, x0, y0, x1, y1) < ILLUSTRATION_VAR {
                continue;
            }
            hit += 1;
            min_x = min_x.min(x0);
            min_y = min_y.min(y0);
            max_x = max_x.max(x1);
            max_y = max_y.max(y1);
        }
    }
    if hit == 0 || max_x <= min_x || max_y <= min_y {
        return None;
    }
    // Require the high-variance uncovered region to be a real plate, not
    // scattered speckle or a few decorated initials.
    if (hit as f32) / (total as f32).max(1.0) < ILLUSTRATION_MIN_PAGE_FRAC {
        return None;
    }
    Some([min_x as f32, min_y as f32, max_x as f32, max_y as f32])
}

fn block_variance(luma: &[u8], width: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
    let mut sum = 0u32;
    let mut sum_sq = 0u32;
    let mut n = 0u32;
    for y in y0..y1 {
        let row = &luma[(y * width + x0) as usize..(y * width + x1) as usize];
        for &p in row {
            let v = p as u32;
            sum += v;
            sum_sq += v * v;
            n += 1;
        }
    }
    if n == 0 {
        return 0.0;
    }
    let mean = sum as f32 / n as f32;
    (sum_sq as f32 / n as f32) - mean * mean
}

fn rgb_to_luma(image: &RgbImage) -> Vec<u8> {
    image
        .pixels()
        .map(|p| {
            let r = p.0[0] as u32;
            let g = p.0[1] as u32;
            let b = p.0[2] as u32;
            ((77 * r + 150 * g + 29 * b) >> 8) as u8
        })
        .collect()
}

fn synthetic_text(bbox: [f32; 4], width: u32, height: u32) -> Detection {
    let class_id = class_id_for("text").unwrap_or(2);
    Detection {
        class_id,
        class_name: Some("text".to_string()),
        confidence: SYNTHETIC_TEXT_CONFIDENCE,
        bbox: clamp_bbox(bbox, width, height),
        category: ContentCategory::Text,
        context: None,
    }
}

fn synthetic_image(bbox: [f32; 4], width: u32, height: u32) -> Detection {
    let class_id = class_id_for("image").unwrap_or(1);
    Detection {
        class_id,
        class_name: Some("image".to_string()),
        confidence: SYNTHETIC_IMAGE_CONFIDENCE,
        bbox: clamp_bbox(bbox, width, height),
        category: ContentCategory::Image,
        context: None,
    }
}

fn union_bboxes(boxes: impl Iterator<Item = [f32; 4]>) -> Option<[f32; 4]> {
    let mut acc: Option<[f32; 4]> = None;
    for b in boxes {
        acc = Some(match acc {
            None => b,
            Some(a) => [
                a[0].min(b[0]),
                a[1].min(b[1]),
                a[2].max(b[2]),
                a[3].max(b[3]),
            ],
        });
    }
    acc
}

fn clamp_x_away_from_images(
    mut x0: f32,
    mut x1: f32,
    y0: f32,
    y1: f32,
    images: &[[f32; 4]],
) -> (f32, f32) {
    for img in images {
        if img[3] <= y0 || img[1] >= y1 {
            continue;
        }
        // Stop expansion at the image edge when the grown box would enter it.
        if x0 < img[2] && x1 > img[0] {
            let overlap_left = (img[2] - x0).max(0.0);
            let overlap_right = (x1 - img[0]).max(0.0);
            if overlap_left < overlap_right {
                x0 = img[2];
            } else {
                x1 = img[0];
            }
        }
    }
    if x1 < x0 + MIN_BOX_PX {
        x1 = x0 + MIN_BOX_PX;
    }
    (x0, x1)
}

fn clamp_bbox_away(mut bbox: [f32; 4], blockers: &[[f32; 4]]) -> [f32; 4] {
    for other in blockers {
        if iou(&bbox, other) <= 0.0 {
            continue;
        }
        if other[0] >= bbox[0] && other[0] < bbox[2] {
            bbox[2] = bbox[2].min(other[0]);
        }
        if other[2] <= bbox[2] && other[2] > bbox[0] {
            bbox[0] = bbox[0].max(other[2]);
        }
        if other[1] >= bbox[1] && other[1] < bbox[3] {
            bbox[3] = bbox[3].min(other[1]);
        }
        if other[3] <= bbox[3] && other[3] > bbox[1] {
            bbox[1] = bbox[1].max(other[3]);
        }
    }
    bbox
}

fn clamp_bbox(bbox: [f32; 4], width: u32, height: u32) -> [f32; 4] {
    let x0 = bbox[0].clamp(0.0, width as f32);
    let y0 = bbox[1].clamp(0.0, height as f32);
    let x1 = bbox[2].clamp(0.0, width as f32).max(x0 + MIN_BOX_PX);
    let y1 = bbox[3].clamp(0.0, height as f32).max(y0 + MIN_BOX_PX);
    [x0, y0, x1.min(width as f32), y1.min(height as f32)]
}

fn bbox_is_tiny(bbox: &[f32; 4]) -> bool {
    (bbox[2] - bbox[0]) < 8.0 || (bbox[3] - bbox[1]) < 8.0
}

fn image_overlaps_substantive_text(image: &Detection, detections: &[Detection]) -> bool {
    if !LABEL_CLASSIFIER.is_image_label(image) {
        return false;
    }
    let image_area =
        (image.bbox[2] - image.bbox[0]).max(0.0) * (image.bbox[3] - image.bbox[1]).max(0.0);
    if image_area <= 0.0 {
        return true;
    }
    let overlap: f32 = detections
        .iter()
        .filter(|d| LABEL_CLASSIFIER.is_substantive_text(d))
        .map(|text| {
            let x0 = image.bbox[0].max(text.bbox[0]);
            let y0 = image.bbox[1].max(text.bbox[1]);
            let x1 = image.bbox[2].min(text.bbox[2]);
            let y1 = image.bbox[3].min(text.bbox[3]);
            (x1 - x0).max(0.0) * (y1 - y0).max(0.0)
        })
        .sum();
    overlap.min(image_area) / image_area >= 0.20
}

fn blocked(bbox: &[f32; 4], blockers: &[[f32; 4]]) -> bool {
    blockers.iter().any(|other| iou(bbox, other) > 0.02)
}

fn point_in_bbox(x: f32, y: f32, bbox: &[f32; 4]) -> bool {
    x >= bbox[0] && x <= bbox[2] && y >= bbox[1] && y <= bbox[3]
}

fn vertical_overlap_frac(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    let overlap = (a1.min(b1) - a0.max(b0)).max(0.0);
    let denom = (a1 - a0).min(b1 - b0).max(1.0);
    overlap / denom
}

fn horizontal_gap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    if a1 < b0 {
        b0 - a1
    } else if b1 < a0 {
        a0 - b1
    } else {
        0.0
    }
}

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let x0 = a[0].max(b[0]);
    let y0 = a[1].max(b[1]);
    let x1 = a[2].min(b[2]);
    let y1 = a[3].min(b[3]);
    let inter = (x1 - x0).max(0.0) * (y1 - y0).max(0.0);
    let aa = (a[2] - a[0]).max(0.0) * (a[3] - a[1]).max(0.0);
    let ba = (b[2] - b[0]).max(0.0) * (b[3] - b[1]).max(0.0);
    if aa + ba <= inter {
        0.0
    } else {
        inter / (aa + ba - inter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{category_for_class, class_id_for};

    fn det(class: &str, bbox: [f32; 4]) -> Detection {
        let class_id = class_id_for(class).expect("known class");
        Detection {
            class_id,
            class_name: Some(class.to_string()),
            confidence: 0.9,
            bbox,
            category: category_for_class(class_id),
            context: None,
        }
    }

    fn page(width: u32, height: u32, ink: &[[u32; 4]]) -> (Vec<u8>, u32, u32) {
        let mut luma = vec![240u8; (width * height) as usize];
        for &[x0, y0, x1, y1] in ink {
            for y in y0..y1.min(height) {
                for x in x0..x1.min(width) {
                    luma[(y * width + x) as usize] = 40;
                }
            }
        }
        (luma, width, height)
    }

    #[test]
    fn existing_text_keeps_vertical_bounds_and_expands_to_ink() {
        let (luma, w, h) = page(200, 120, &[[20, 30, 180, 70]]);
        let mut detections = vec![det("text", [60.0, 30.0, 120.0, 70.0])];
        correct_with_signals(&mut detections, &luma, w, h, &[]);
        assert_eq!(detections.len(), 1);
        assert!((detections[0].bbox[1] - 30.0).abs() < 0.1);
        assert!((detections[0].bbox[3] - 70.0).abs() < 0.1);
        assert!(detections[0].bbox[0] <= 22.0, "left should grow toward ink");
        assert!(
            detections[0].bbox[2] >= 178.0,
            "right should grow toward ink"
        );
    }

    #[test]
    fn ocr_lines_expand_clipped_caption_horizontally() {
        let (luma, w, h) = page(200, 80, &[[10, 40, 190, 55]]);
        let mut detections = vec![det("figure_title", [70.0, 40.0, 140.0, 55.0])];
        let lines = [LineHint {
            bbox: [12.0, 41.0, 188.0, 54.0],
            score: 0.9,
        }];
        correct_with_signals(&mut detections, &luma, w, h, &lines);
        assert!(detections[0].bbox[0] <= 13.0);
        assert!(detections[0].bbox[2] >= 187.0);
        assert!((detections[0].bbox[1] - 40.0).abs() < 0.1);
    }

    #[test]
    fn missed_middle_paragraph_is_synthesized_from_ink() {
        let (luma, w, h) = page(
            200,
            300,
            &[[20, 20, 180, 70], [20, 110, 180, 190], [20, 220, 180, 280]],
        );
        let mut detections = vec![
            det("text", [20.0, 20.0, 180.0, 70.0]),
            det("text", [20.0, 220.0, 180.0, 280.0]),
        ];
        correct_with_signals(&mut detections, &luma, w, h, &[]);
        assert!(
            detections.len() >= 3,
            "expected a synthetic box for the missed middle paragraph, got {}",
            detections.len()
        );
        let recovered = detections.iter().any(|d| {
            d.bbox[1] < 130.0 && d.bbox[3] > 170.0 && d.bbox[0] < 40.0 && d.bbox[2] > 160.0
        });
        assert!(recovered, "middle ink block should become a text box");
    }

    #[test]
    fn whitespace_gap_is_not_filled() {
        let (luma, w, h) = page(200, 200, &[[20, 20, 180, 60], [20, 140, 180, 180]]);
        let mut detections = vec![
            det("text", [20.0, 20.0, 180.0, 60.0]),
            det("text", [20.0, 140.0, 180.0, 180.0]),
        ];
        correct_with_signals(&mut detections, &luma, w, h, &[]);
        assert_eq!(detections.len(), 2);
    }

    #[test]
    fn unused_ocr_lines_become_text_bands() {
        let (luma, w, h) = page(200, 200, &[[20, 20, 180, 40], [20, 100, 180, 160]]);
        let mut detections = vec![det("text", [20.0, 20.0, 180.0, 40.0])];
        let lines = [
            LineHint {
                bbox: [22.0, 102.0, 178.0, 118.0],
                score: 0.8,
            },
            LineHint {
                bbox: [22.0, 122.0, 178.0, 138.0],
                score: 0.8,
            },
            LineHint {
                bbox: [22.0, 142.0, 178.0, 158.0],
                score: 0.8,
            },
        ];
        correct_with_signals(&mut detections, &luma, w, h, &lines);
        assert!(
            detections
                .iter()
                .any(|d| d.bbox[1] <= 104.0 && d.bbox[3] >= 156.0),
            "clustered unused OCR lines should form one vertical text band"
        );
    }

    #[test]
    fn text_expansion_stops_at_an_image() {
        let (luma, w, h) = page(200, 80, &[[10, 10, 190, 70]]);
        let mut detections = vec![
            det("text", [80.0, 10.0, 120.0, 70.0]),
            det("image", [10.0, 10.0, 50.0, 70.0]),
        ];
        correct_with_signals(&mut detections, &luma, w, h, &[]);
        let text = detections
            .iter()
            .find(|d| LABEL_CLASSIFIER.is_text_label(d))
            .unwrap();
        assert!(
            text.bbox[0] >= 48.0,
            "text must not grow into the image, got x0={}",
            text.bbox[0]
        );
    }

    #[test]
    fn image_box_grows_toward_nearby_ink() {
        let (luma, w, h) = page(120, 120, &[[10, 10, 100, 100]]);
        let mut detections = vec![det("image", [20.0, 20.0, 80.0, 80.0])];
        correct_with_signals(&mut detections, &luma, w, h, &[]);
        let img = &detections[0];
        assert!(img.bbox[0] < 20.0);
        assert!(img.bbox[1] < 20.0);
        assert!(img.bbox[2] > 80.0);
        assert!(img.bbox[3] > 80.0);
    }

    #[test]
    fn overlapping_caption_cannot_collapse_detected_image() {
        let (luma, w, h) = page(800, 800, &[[0, 40, 768, 634]]);
        let original = [0.2, 38.4, 767.8, 633.1];
        let mut detections = vec![
            det("image", original),
            det("figure_title", [40.0, 560.0, 730.0, 620.0]),
        ];
        correct_with_signals(&mut detections, &luma, w, h, &[]);
        let image = detections
            .iter()
            .find(|detection| LABEL_CLASSIFIER.is_image_label(detection))
            .expect("image detection survives correction");
        assert!(image.bbox[0] <= original[0]);
        assert!(image.bbox[1] <= original[1]);
        assert!(image.bbox[2] >= original[2]);
        assert!(image.bbox[3] >= original[3]);
    }

    #[test]
    fn staff_line_image_box_is_demoted_to_text() {
        let (w, h) = (256u32, 256u32);
        let mut img = RgbImage::from_pixel(w, h, image::Rgb([245, 245, 245]));
        for y in (8..h).step_by(8) {
            for x in 8..(w - 8) {
                img.put_pixel(x, y, image::Rgb([20, 20, 20]));
            }
        }
        let mut detections = vec![det("image", [0.0, 0.0, 256.0, 256.0])];
        demote_line_art_images(&mut detections, &img);
        assert!(
            LABEL_CLASSIFIER.is_text_label(&detections[0]),
            "staff-line crop must leave the photo-overlay path"
        );
    }

    #[test]
    fn gradient_photo_image_box_is_kept() {
        let img = RgbImage::from_fn(256, 128, |x, _| image::Rgb([x as u8, x as u8, x as u8]));
        let mut detections = vec![det("image", [0.0, 0.0, 256.0, 128.0])];
        demote_line_art_images(&mut detections, &img);
        assert!(LABEL_CLASSIFIER.is_image_label(&detections[0]));
    }
}
