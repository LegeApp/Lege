//! Decoder for PP-DocLayout (PicoDet/GFL) prepared heads.
//!
//! The prepared ONNX (NMS stripped, see `doclayout-m/onnx-work/provenance.json`)
//! emits two tensors already decoded in-graph:
//!   * boxes  `[1, N, 4]` — `[x1,y1,x2,y2]` in the 640×640 input space
//!     (before the `scale_factor` divide, which we strip and redo per-axis here);
//!   * scores `[1, C, N]` — per-class sigmoid confidences.
//! This module thresholds, runs class-aware NMS, and maps boxes back to the
//! original image via the stretch-resize ratio.

use anyhow::{Result, bail};

use crate::vision::preprocess::PreprocessMeta;
use crate::vision::reference;

/// PP-DocLayout-M 23-class label list, in model output order (class id = index).
pub(crate) const CLASS_NAMES: &[&str] = &[
    "paragraph_title",
    "image",
    "text",
    "number",
    "abstract",
    "content",
    "figure_title",
    "formula",
    "table",
    "table_title",
    "reference",
    "doc_title",
    "footnote",
    "header",
    "algorithm",
    "footer",
    "seal",
    "chart_title",
    "chart",
    "formula_number",
    "header_image",
    "footer_image",
    "aside_text",
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

/// Decode the `(boxes, scores)` head pair into original-image detections.
pub(crate) fn decode(
    boxes: &reference::Tensor,
    scores: &reference::Tensor,
    meta: &PreprocessMeta,
    conf_threshold: f32,
    iou_threshold: f32,
    max_detections: usize,
) -> Result<Vec<Detection>> {
    if !conf_threshold.is_finite() || !(0.0..=1.0).contains(&conf_threshold) {
        bail!("PicoDet confidence threshold must be finite and in 0..=1");
    }
    if !iou_threshold.is_finite() || !(0.0..=1.0).contains(&iou_threshold) {
        bail!("PicoDet IoU threshold must be finite and in 0..=1");
    }
    if max_detections == 0 {
        return Ok(Vec::new());
    }

    // boxes: [1, N, 4]; scores: [1, C, N].
    if boxes.shape.len() != 3 || boxes.shape[0] != 1 || boxes.shape[2] != 4 {
        bail!("PicoDet boxes must be [1,N,4], got {:?}", boxes.shape);
    }
    if scores.shape.len() != 3 || scores.shape[0] != 1 {
        bail!("PicoDet scores must be [1,C,N], got {:?}", scores.shape);
    }
    let num_anchors = boxes.shape[1];
    let num_classes = scores.shape[1];
    if num_classes != CLASS_NAMES.len() {
        bail!(
            "PicoDet scores must contain {} classes, got {num_classes}",
            CLASS_NAMES.len()
        );
    }
    if scores.shape[2] != num_anchors {
        bail!(
            "PicoDet boxes/scores anchor mismatch: {} vs {}",
            num_anchors,
            scores.shape[2]
        );
    }

    // Per-axis scale from the stretch resize (input space -> original pixels).
    let sx = meta.orig_w as f32 / meta.input_w as f32;
    let sy = meta.orig_h as f32 / meta.input_h as f32;
    let orig_w = meta.orig_w as f32;
    let orig_h = meta.orig_h as f32;

    let trace = std::env::var_os("LEGE_LAYOUT_TRACE").is_some();
    let mut image_candidates = trace.then(Vec::new);
    let mut detections = Vec::new();
    for anchor in 0..num_anchors {
        // Best class for this anchor. scores are laid out [C, N] row-major.
        let mut best_class = 0usize;
        let mut best_score = 0.0f32;
        for class_id in 0..num_classes {
            let score = scores.data[class_id * num_anchors + anchor];
            if score > best_score {
                best_score = score;
                best_class = class_id;
            }
        }
        if let Some(candidates) = image_candidates.as_mut() {
            let base = anchor * 4;
            candidates.push((
                scores.data[num_anchors + anchor],
                best_class,
                best_score,
                anchor,
                [
                    (boxes.data[base] * sx).clamp(0.0, orig_w),
                    (boxes.data[base + 1] * sy).clamp(0.0, orig_h),
                    (boxes.data[base + 2] * sx).clamp(0.0, orig_w),
                    (boxes.data[base + 3] * sy).clamp(0.0, orig_h),
                ],
            ));
        }
        if best_score < conf_threshold {
            continue;
        }

        let base = anchor * 4;
        let x1 = (boxes.data[base] * sx).clamp(0.0, orig_w);
        let y1 = (boxes.data[base + 1] * sy).clamp(0.0, orig_h);
        let x2 = (boxes.data[base + 2] * sx).clamp(0.0, orig_w);
        let y2 = (boxes.data[base + 3] * sy).clamp(0.0, orig_h);
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

    detections.sort_by(|a, b| b.score.total_cmp(&a.score));
    let kept = class_aware_nms(&detections, iou_threshold, max_detections);
    if let Some(mut candidates) = image_candidates {
        candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
        eprintln!(
            "LEGE_LAYOUT_TRACE PicoDet anchors={} thresholded={} first_nms={} conf={:.3} iou={:.3}",
            num_anchors,
            detections.len(),
            kept.len(),
            conf_threshold,
            iou_threshold
        );
        for (image_score, best_class, best_score, anchor, bbox) in candidates.into_iter().take(12) {
            eprintln!(
                "  image-candidate anchor={} image={:.4} best={}({:.4}) bbox={:?}",
                anchor,
                image_score,
                class_name(best_class),
                best_score,
                bbox
            );
        }
        for detection in &kept {
            eprintln!(
                "  first-nms {} score={:.4} bbox=[{:.1},{:.1},{:.1},{:.1}]",
                class_name(detection.class_id),
                detection.score,
                detection.x1,
                detection.y1,
                detection.x2,
                detection.y2
            );
        }
    }
    Ok(kept)
}

fn class_aware_nms(
    detections: &[Detection],
    iou_threshold: f32,
    max_detections: usize,
) -> Vec<Detection> {
    if max_detections == 0 {
        return Vec::new();
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn one_detection_heads() -> (reference::Tensor, reference::Tensor, PreprocessMeta) {
        let boxes = reference::Tensor::new(vec![1, 1, 4], vec![1.0, 2.0, 5.0, 6.0]).unwrap();
        let mut score_data = vec![0.0; CLASS_NAMES.len()];
        score_data[2] = 0.9;
        let scores = reference::Tensor::new(vec![1, CLASS_NAMES.len(), 1], score_data).unwrap();
        let meta = PreprocessMeta {
            orig_w: 10,
            orig_h: 10,
            input_w: 10,
            input_h: 10,
        };
        (boxes, scores, meta)
    }

    #[test]
    fn zero_detection_limit_returns_no_boxes() {
        let (boxes, scores, meta) = one_detection_heads();
        assert!(
            decode(&boxes, &scores, &meta, 0.2, 0.5, 0)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn invalid_thresholds_are_rejected() {
        let (boxes, scores, meta) = one_detection_heads();
        assert!(decode(&boxes, &scores, &meta, f32::NAN, 0.5, 1).is_err());
        assert!(decode(&boxes, &scores, &meta, 0.2, 1.1, 1).is_err());
    }

    #[test]
    fn class_count_must_match_the_doclayout_dictionary() {
        let (boxes, _, meta) = one_detection_heads();
        let scores = reference::Tensor::new(vec![1, 1, 1], vec![0.9]).unwrap();
        assert!(decode(&boxes, &scores, &meta, 0.2, 0.5, 1).is_err());
    }
}
