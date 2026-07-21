use rustc_hash::FxHashMap;
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

    let mut class_groups: FxHashMap<i32, Vec<Detection>> = FxHashMap::default();
    for det in detections.drain(..) {
        class_groups.entry(det.class_id).or_default().push(det);
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
