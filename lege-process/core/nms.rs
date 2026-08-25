use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Detection {
    pub class_id: i32,
    pub class_name: Option<String>,
    pub confidence: f32,
    pub bbox: [f32; 4],
    pub context: Option<DetectionContext>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DetectionContext {
    pub original_width: f32,
    pub original_height: f32,
    pub scale_x: f32,
    pub scale_y: f32,
}

pub fn two_pass_nms(
    mut detections: Vec<Detection>,
    aware_iou: f32,
    agnostic_iou: f32,
) -> Vec<Detection> {
    if detections.is_empty() {
        return Vec::new();
    }

    detections.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(Ordering::Equal)
    });

    // Group by class id. A layout page yields on the order of ten distinct
    // classes, so a linear scan over an association list beats hashing — and,
    // unlike a hash map, it groups in first-seen order, which makes the output
    // deterministic all the way down to confidence ties.
    let mut class_groups: Vec<(i32, Vec<Detection>)> = Vec::new();
    for det in detections.drain(..) {
        match class_groups.iter_mut().find(|(id, _)| *id == det.class_id) {
            Some((_, group)) => group.push(det),
            None => class_groups.push((det.class_id, vec![det])),
        }
    }

    let mut results = Vec::new();
    for (_, group) in class_groups {
        let nms_group = class_aware_nms(group, aware_iou);
        results.extend(nms_group);
    }

    results.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(Ordering::Equal)
    });

    let mut final_results: Vec<Detection> = Vec::new();
    for det in results {
        let mut keep = true;
        for kept in &final_results {
            if det.class_id != kept.class_id {
                let iou = calculate_iou(&det, kept);
                if iou > agnostic_iou && det.confidence < kept.confidence * 0.9 {
                    keep = false;
                    break;
                }
            }
        }
        if keep {
            final_results.push(det);
        }
    }
    final_results
}

fn class_aware_nms(mut detections: Vec<Detection>, iou_threshold: f32) -> Vec<Detection> {
    detections.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(Ordering::Equal)
    });
    let mut current_index = 0;
    for index in 0..detections.len() {
        let mut keep = true;
        for prev_index in 0..current_index {
            let iou = calculate_iou(&detections[prev_index], &detections[index]);
            if iou > iou_threshold {
                keep = false;
                break;
            }
        }
        if keep {
            detections.swap(current_index, index);
            current_index += 1;
        }
    }
    detections.truncate(current_index);
    detections
}

fn calculate_iou(a: &Detection, b: &Detection) -> f32 {
    let [a_x1, a_y1, a_x2, a_y2] = a.bbox;
    let [b_x1, b_y1, b_x2, b_y2] = b.bbox;

    let x1 = a_x1.max(b_x1);
    let y1 = a_y1.max(b_y1);
    let x2 = a_x2.min(b_x2);
    let y2 = a_y2.min(b_y2);

    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let area_a = (a_x2 - a_x1) * (a_y2 - a_y1);
    let area_b = (b_x2 - b_x1) * (b_y2 - b_y1);
    let union = area_a + area_b - inter;

    if union > 0.0 { inter / union } else { 0.0 }
}

#[cfg(test)]
mod class_grouping_tests {
    use super::*;

    fn det(class_id: i32, confidence: f32, bbox: [f32; 4]) -> Detection {
        Detection {
            class_id,
            class_name: None,
            confidence,
            bbox,
            context: None,
        }
    }

    /// Grouping moved from `FxHashMap` to an association list. Detections of
    /// different classes must still be grouped separately, so a class-aware
    /// pass never suppresses across classes.
    #[test]
    fn detections_are_grouped_per_class() {
        // Two heavily overlapping boxes in each of two classes.
        let detections = vec![
            det(0, 0.90, [0.0, 0.0, 10.0, 10.0]),
            det(0, 0.80, [0.5, 0.5, 10.5, 10.5]),
            det(8, 0.70, [0.0, 0.0, 10.0, 10.0]),
            det(8, 0.60, [0.5, 0.5, 10.5, 10.5]),
        ];

        // Class-aware NMS collapses each pair; agnostic pass is disabled (1.0).
        let kept = two_pass_nms(detections, 0.5, 1.0);

        assert_eq!(kept.len(), 2, "one survivor per class expected");
        let mut classes: Vec<i32> = kept.iter().map(|d| d.class_id).collect();
        classes.sort_unstable();
        assert_eq!(classes, vec![0, 8]);
        // The higher-confidence member of each pair is the survivor.
        for d in &kept {
            assert!(d.confidence >= 0.70, "kept the weaker detection: {d:?}");
        }
    }

    /// Non-overlapping detections of the same class must all survive.
    #[test]
    fn disjoint_same_class_detections_all_survive() {
        let detections = vec![
            det(2, 0.9, [0.0, 0.0, 10.0, 10.0]),
            det(2, 0.8, [100.0, 100.0, 110.0, 110.0]),
            det(2, 0.7, [200.0, 200.0, 210.0, 210.0]),
        ];
        let kept = two_pass_nms(detections, 0.5, 1.0);
        assert_eq!(kept.len(), 3);
    }

    /// Output order is confidence-descending regardless of grouping order.
    #[test]
    fn results_are_sorted_by_confidence() {
        let detections = vec![
            det(1, 0.30, [0.0, 0.0, 10.0, 10.0]),
            det(2, 0.95, [100.0, 0.0, 110.0, 10.0]),
            det(3, 0.60, [200.0, 0.0, 210.0, 10.0]),
        ];
        let kept = two_pass_nms(detections, 0.5, 1.0);
        let confidences: Vec<f32> = kept.iter().map(|d| d.confidence).collect();
        assert_eq!(confidences, vec![0.95, 0.60, 0.30]);
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert!(two_pass_nms(Vec::new(), 0.5, 0.5).is_empty());
    }
}
