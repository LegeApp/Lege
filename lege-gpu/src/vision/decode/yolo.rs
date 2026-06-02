use anyhow::{Result, bail};
use image::{Rgb, RgbImage};

use crate::vision::preprocess::PreprocessMeta;
use crate::vision::reference;

const REG_MAX: usize = 16;
const BOX_CHANNELS: usize = 4 * REG_MAX;

// From model metadata: {0:'title',1:'plain text',2:'abandon',3:'figure',
//   4:'figure_caption',5:'table',6:'table_caption',7:'table_footnote',
//   8:'isolate_formula',9:'formula_caption'}
const CLASS_NAMES: &[&str] = &[
    "title",
    "plain text",
    "abandon",
    "figure",
    "figure_caption",
    "table",
    "table_caption",
    "table_footnote",
    "isolate_formula",
    "formula_caption",
];

pub(crate) fn class_name(class_id: usize) -> &'static str {
    CLASS_NAMES.get(class_id).copied().unwrap_or("unknown")
}

#[derive(Debug, Clone)]
pub(crate) struct Detection {
    pub(crate) class_id: usize,
    pub(crate) score: f32,
    pub(crate) x1: f32,
    pub(crate) y1: f32,
    pub(crate) x2: f32,
    pub(crate) y2: f32,
}

pub(crate) fn decode_heads(
    heads: &[reference::Tensor],
    meta: &PreprocessMeta,
    conf_threshold: f32,
    iou_threshold: f32,
    max_detections: usize,
) -> Result<Vec<Detection>> {
    let mut detections = Vec::new();
    for head in heads {
        decode_head(head, meta, conf_threshold, &mut detections)?;
    }
    detections.sort_by(|a, b| b.score.total_cmp(&a.score));
    let kept = class_aware_nms(&detections, iou_threshold, max_detections);
    Ok(kept)
}

fn decode_head(
    head: &reference::Tensor,
    meta: &PreprocessMeta,
    conf_threshold: f32,
    detections: &mut Vec<Detection>,
) -> Result<()> {
    if head.shape.len() != 4 || head.shape[0] != 1 {
        bail!("YOLO head must have shape [1,C,H,W], got {:?}", head.shape);
    }
    let channels = head.shape[1];
    let h = head.shape[2];
    let w = head.shape[3];
    if channels <= BOX_CHANNELS {
        bail!("YOLO head needs box channels plus classes, got {channels}");
    }
    let classes = channels - BOX_CHANNELS;
    let stride_x = meta.input_w as f32 / w as f32;
    let stride_y = meta.input_h as f32 / h as f32;

    for y in 0..h {
        for x in 0..w {
            let mut best_class = 0usize;
            let mut best_score = 0.0f32;
            for class_id in 0..classes {
                let score = sigmoid(value_at(head, BOX_CHANNELS + class_id, y, x));
                if score > best_score {
                    best_score = score;
                    best_class = class_id;
                }
            }
            if best_score < conf_threshold {
                continue;
            }

            let left = dfl_distance(head, 0, y, x) * stride_x;
            let top = dfl_distance(head, 1, y, x) * stride_y;
            let right = dfl_distance(head, 2, y, x) * stride_x;
            let bottom = dfl_distance(head, 3, y, x) * stride_y;
            let cx = (x as f32 + 0.5) * stride_x;
            let cy = (y as f32 + 0.5) * stride_y;

            let x1 = ((cx - left - meta.pad_x) / meta.scale).clamp(0.0, meta.orig_w as f32);
            let y1 = ((cy - top - meta.pad_y) / meta.scale).clamp(0.0, meta.orig_h as f32);
            let x2 = ((cx + right - meta.pad_x) / meta.scale).clamp(0.0, meta.orig_w as f32);
            let y2 = ((cy + bottom - meta.pad_y) / meta.scale).clamp(0.0, meta.orig_h as f32);
            if x2 <= x1 || y2 <= y1 {
                continue;
            }
            detections.push(Detection {
                class_id: best_class,
                score: best_score,
                x1,
                y1,
                x2,
                y2,
            });
        }
    }
    Ok(())
}

fn value_at(head: &reference::Tensor, channel: usize, y: usize, x: usize) -> f32 {
    let h = head.shape[2];
    let w = head.shape[3];
    head.data[(channel * h + y) * w + x]
}

fn dfl_distance(head: &reference::Tensor, side: usize, y: usize, x: usize) -> f32 {
    let base = side * REG_MAX;
    let mut max_logit = f32::NEG_INFINITY;
    for bin in 0..REG_MAX {
        max_logit = max_logit.max(value_at(head, base + bin, y, x));
    }
    let mut sum = 0.0f32;
    let mut weighted = 0.0f32;
    for bin in 0..REG_MAX {
        let p = (value_at(head, base + bin, y, x) - max_logit).exp();
        sum += p;
        weighted += p * bin as f32;
    }
    weighted / sum.max(1e-12)
}

fn class_aware_nms(
    detections: &[Detection],
    iou_threshold: f32,
    max_detections: usize,
) -> Vec<Detection> {
    let mut kept: Vec<Detection> = Vec::new();
    'candidate: for detection in detections {
        for existing in &kept {
            if existing.class_id == detection.class_id && iou(existing, detection) > iou_threshold {
                continue 'candidate;
            }
        }
        kept.push(detection.clone());
        if kept.len() >= max_detections {
            break;
        }
    }
    kept
}

fn iou(a: &Detection, b: &Detection) -> f32 {
    let x1 = a.x1.max(b.x1);
    let y1 = a.y1.max(b.y1);
    let x2 = a.x2.min(b.x2);
    let y2 = a.y2.min(b.y2);
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let area_a = (a.x2 - a.x1).max(0.0) * (a.y2 - a.y1).max(0.0);
    let area_b = (b.x2 - b.x1).max(0.0) * (b.y2 - b.y1).max(0.0);
    inter / (area_a + area_b - inter).max(1e-12)
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

// Per-class colors matching CLASS_NAMES order
const CLASS_COLORS: &[Rgb<u8>] = &[
    Rgb([255, 100, 100]), // title        — red-ish
    Rgb([60, 180, 75]),   // plain text   — green
    Rgb([180, 180, 180]), // abandon      — grey
    Rgb([0, 130, 200]),   // figure       — blue
    Rgb([0, 200, 200]),   // figure_caption — cyan
    Rgb([245, 130, 48]),  // table        — orange
    Rgb([255, 210, 80]),  // table_caption — yellow
    Rgb([200, 160, 255]), // table_footnote — lavender
    Rgb([240, 50, 230]),  // isolate_formula — magenta
    Rgb([210, 245, 60]),  // formula_caption — lime
];

pub(crate) fn draw_overlay(original: &RgbImage, detections: &[Detection]) -> RgbImage {
    let mut out = original.clone();
    for det in detections {
        let color = CLASS_COLORS
            .get(det.class_id)
            .copied()
            .unwrap_or(Rgb([230, 25, 75]));
        draw_rect(&mut out, det, color);
        draw_label(&mut out, det, color);
    }
    out
}

fn draw_rect(image: &mut RgbImage, det: &Detection, color: Rgb<u8>) {
    let x1 = det.x1.round().max(0.0) as u32;
    let y1 = det.y1.round().max(0.0) as u32;
    let x2 = det.x2.round().min((image.width().saturating_sub(1)) as f32) as u32;
    let y2 = det
        .y2
        .round()
        .min((image.height().saturating_sub(1)) as f32) as u32;
    for thickness in 0..3 {
        let t = thickness as u32;
        for x in x1.saturating_sub(t)..=x2.saturating_add(t).min(image.width().saturating_sub(1)) {
            if y1 >= t {
                image.put_pixel(x, y1 - t, color);
            }
            let yb = y2.saturating_add(t).min(image.height().saturating_sub(1));
            image.put_pixel(x, yb, color);
        }
        for y in y1.saturating_sub(t)..=y2.saturating_add(t).min(image.height().saturating_sub(1)) {
            if x1 >= t {
                image.put_pixel(x1 - t, y, color);
            }
            let xb = x2.saturating_add(t).min(image.width().saturating_sub(1));
            image.put_pixel(xb, y, color);
        }
    }
}

// Draws a small pixel-font label at the top-left corner of the bounding box.
// Each character is rendered as a 5×7 dot pattern at 1px per dot.
fn draw_label(image: &mut RgbImage, det: &Detection, color: Rgb<u8>) {
    let label = format!("{} {:.0}%", class_name(det.class_id), det.score * 100.0);
    let x0 = det.x1.round().max(0.0) as u32 + 4;
    let y0 = det.y1.round().max(0.0) as u32 + 4;
    let char_w = 6u32;
    let char_h = 8u32;
    let bg = Rgb([0u8, 0, 0]);
    // draw background bar
    let bar_w = label.len() as u32 * char_w + 2;
    for dy in 0..char_h + 2 {
        for dx in 0..bar_w {
            let px = x0.saturating_sub(1) + dx;
            let py = y0.saturating_sub(1) + dy;
            if px < image.width() && py < image.height() {
                image.put_pixel(px, py, bg);
            }
        }
    }
    for (i, ch) in label.chars().enumerate() {
        let cx = x0 + i as u32 * char_w;
        draw_char(image, ch, cx, y0, color);
    }
}

fn draw_char(image: &mut RgbImage, ch: char, x0: u32, y0: u32, color: Rgb<u8>) {
    // Minimal 5×7 bitmap font for ASCII printable chars.
    // Each u8 encodes one row (bits 7..3, MSB = leftmost pixel).
    let bitmap: [u8; 7] = char_bitmap(ch);
    for (row, &bits) in bitmap.iter().enumerate() {
        for col in 0..5u32 {
            if bits & (0x80 >> col) != 0 {
                let px = x0 + col;
                let py = y0 + row as u32;
                if px < image.width() && py < image.height() {
                    image.put_pixel(px, py, color);
                }
            }
        }
    }
}

fn char_bitmap(ch: char) -> [u8; 7] {
    match ch {
        ' ' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        '%' => [0xC8, 0xD0, 0x20, 0x40, 0x98, 0x18, 0x00],
        '0' => [0x70, 0x88, 0x98, 0xA8, 0xC8, 0x88, 0x70],
        '1' => [0x20, 0x60, 0x20, 0x20, 0x20, 0x20, 0x70],
        '2' => [0x70, 0x88, 0x08, 0x30, 0x40, 0x80, 0xF8],
        '3' => [0x70, 0x88, 0x08, 0x30, 0x08, 0x88, 0x70],
        '4' => [0x10, 0x30, 0x50, 0x90, 0xF8, 0x10, 0x10],
        '5' => [0xF8, 0x80, 0xF0, 0x08, 0x08, 0x88, 0x70],
        '6' => [0x30, 0x40, 0x80, 0xF0, 0x88, 0x88, 0x70],
        '7' => [0xF8, 0x08, 0x10, 0x20, 0x40, 0x40, 0x40],
        '8' => [0x70, 0x88, 0x88, 0x70, 0x88, 0x88, 0x70],
        '9' => [0x70, 0x88, 0x88, 0x78, 0x08, 0x10, 0x60],
        'a' => [0x00, 0x00, 0x70, 0x08, 0x78, 0x88, 0x78],
        'b' => [0x80, 0x80, 0xB0, 0xC8, 0x88, 0xC8, 0xB0],
        'c' => [0x00, 0x00, 0x70, 0x88, 0x80, 0x88, 0x70],
        'd' => [0x08, 0x08, 0x68, 0x98, 0x88, 0x98, 0x68],
        'e' => [0x00, 0x00, 0x70, 0x88, 0xF8, 0x80, 0x70],
        'f' => [0x18, 0x28, 0x20, 0xF8, 0x20, 0x20, 0x20],
        'g' => [0x00, 0x68, 0x98, 0x88, 0x78, 0x08, 0x70],
        'h' => [0x80, 0x80, 0xB0, 0xC8, 0x88, 0x88, 0x88],
        'i' => [0x20, 0x00, 0x60, 0x20, 0x20, 0x20, 0x70],
        'j' => [0x10, 0x00, 0x30, 0x10, 0x10, 0x90, 0x60],
        'k' => [0x80, 0x80, 0x90, 0xA0, 0xC0, 0xA0, 0x90],
        'l' => [0x60, 0x20, 0x20, 0x20, 0x20, 0x20, 0x70],
        'm' => [0x00, 0x00, 0xD0, 0xA8, 0xA8, 0x88, 0x88],
        'n' => [0x00, 0x00, 0xB0, 0xC8, 0x88, 0x88, 0x88],
        'o' => [0x00, 0x00, 0x70, 0x88, 0x88, 0x88, 0x70],
        'p' => [0x00, 0xB0, 0xC8, 0x88, 0xC8, 0xB0, 0x80],
        'q' => [0x00, 0x68, 0x98, 0x88, 0x98, 0x68, 0x08],
        'r' => [0x00, 0x00, 0xB0, 0xC8, 0x80, 0x80, 0x80],
        's' => [0x00, 0x00, 0x78, 0x80, 0x70, 0x08, 0xF0],
        't' => [0x20, 0x20, 0xF8, 0x20, 0x20, 0x28, 0x10],
        'u' => [0x00, 0x00, 0x88, 0x88, 0x88, 0x98, 0x68],
        'v' => [0x00, 0x00, 0x88, 0x88, 0x88, 0x50, 0x20],
        'w' => [0x00, 0x00, 0x88, 0xA8, 0xA8, 0xA8, 0x50],
        'x' => [0x00, 0x00, 0x88, 0x50, 0x20, 0x50, 0x88],
        'y' => [0x00, 0x88, 0x88, 0x78, 0x08, 0x88, 0x70],
        'z' => [0x00, 0x00, 0xF8, 0x10, 0x20, 0x40, 0xF8],
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF8],
        '-' => [0x00, 0x00, 0x00, 0xF8, 0x00, 0x00, 0x00],
        _ => [0x00, 0x00, 0x70, 0x20, 0x20, 0x00, 0x20], // '?' fallback
    }
}
