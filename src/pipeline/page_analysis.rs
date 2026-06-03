use image::RgbImage;

use crate::engine::Detection;
use crate::margin::ContentBounds;
use crate::pipeline::config::PipelineConfig;
use crate::pipeline::helper_functions::rounded_clamped_bbox;
use crate::types::LabelClassifier;

pub const BLANK_PAGE_FALLBACK_THRESHOLD: u8 = 96;

#[derive(Debug, Clone, Copy, Default)]
pub struct PageClassification {
    pub content_bounds: Option<ContentBounds>,
    pub is_blank: bool,
    pub is_full_page_image: bool,
}

pub fn compute_pixel_bounds_for_margin(
    image: &RgbImage,
    config: &PipelineConfig,
) -> Option<ContentBounds> {
    let options = crate::pipeline::policies::binarize_options_for(config, false);

    let mut binarized = Legencode::color::binarization::binarize_image_raw(
        image.as_raw(),
        image.width() as usize,
        image.height() as usize,
        &options,
    );

    if binarized.is_empty() {
        return None;
    }

    if binarized.iter().any(|&b| b > 1) {
        for value in &mut binarized {
            *value = if *value > 128 { 1 } else { 0 };
        }
    }

    for value in &mut binarized {
        *value = if *value == 0 { 1 } else { 0 };
    }

    crate::margin::calculate_content_bounds_from_binary_mask(
        &binarized,
        image.width(),
        image.height(),
    )
}

pub fn maybe_expand_sole_image_to_full_page(
    detections: &mut Vec<Detection>,
    page_w: u32,
    page_h: u32,
    classifier: &LabelClassifier,
) {
    let pw = page_w as f32;
    let ph = page_h as f32;
    if pw <= 1.0 || ph <= 1.0 {
        return;
    }

    let image_count = detections
        .iter()
        .filter(|d| classifier.is_image_label(d))
        .count();
    let substantive_text_count = detections
        .iter()
        .filter(|d| classifier.is_substantive_text(d))
        .count();

    if image_count == 1 && substantive_text_count == 0 {
        if let Some(det) = detections.iter_mut().find(|d| {
            classifier.is_image_label(d)
                && !matches!(
                    d.class_name.as_deref(),
                    Some("header_image" | "footer_image")
                )
        }) {
            crate::bbox_trace!(
                "[FULL-PAGE] sole image det, expanding [{:.0},{:.0},{:.0},{:.0}] -> [0,0,{pw},{ph}]",
                det.bbox[0],
                det.bbox[1],
                det.bbox[2],
                det.bbox[3]
            );
            det.bbox = [0.0, 0.0, pw, ph];
        }
    }
}

pub const FULL_BLEED_IMAGE_MIN_HEIGHT_FRAC: f32 = 0.65;
pub const FULL_BLEED_IMAGE_MIN_WIDTH_FRAC: f32 = 0.78;
const FULL_PAGE_BBOX_EPS: f32 = 0.02;
const FULL_BLEED_EDGE_INSET_MAX_FRAC: f32 = 0.045;

#[inline]
fn touches_both_horizontal_edges(b: &[f32; 4], page_w: f32) -> bool {
    if page_w <= 1.0 {
        return false;
    }
    let eps = page_w * FULL_BLEED_EDGE_INSET_MAX_FRAC;
    b[0] <= eps && b[2] >= page_w - eps
}

#[inline]
fn touches_both_vertical_edges(b: &[f32; 4], page_h: f32) -> bool {
    if page_h <= 1.0 {
        return false;
    }
    let eps = page_h * FULL_BLEED_EDGE_INSET_MAX_FRAC;
    b[1] <= eps && b[3] >= page_h - eps
}

fn bbox_covers_almost_full_page(bbox: &[f32; 4], page_w: u32, page_h: u32, eps: f32) -> bool {
    let pw = page_w as f32;
    let ph = page_h as f32;
    if pw <= 1.0 || ph <= 1.0 {
        return false;
    }
    let w = (bbox[2] - bbox[0]).max(0.0);
    let h = (bbox[3] - bbox[1]).max(0.0);
    w >= pw * (1.0 - eps)
        && h >= ph * (1.0 - eps)
        && bbox[0].abs() <= pw * eps
        && bbox[1].abs() <= ph * eps
}

fn dedupe_full_page_image_detections(
    detections: &mut Vec<Detection>,
    classifier: &LabelClassifier,
    page_w: u32,
    page_h: u32,
) {
    let mut best_i: Option<usize> = None;
    let mut best_conf = -1.0f32;
    for (i, d) in detections.iter().enumerate() {
        if classifier.is_image_label(d)
            && bbox_covers_almost_full_page(&d.bbox, page_w, page_h, FULL_PAGE_BBOX_EPS)
            && d.confidence > best_conf
        {
            best_conf = d.confidence;
            best_i = Some(i);
        }
    }

    let Some(keep) = best_i else {
        return;
    };

    let mut idx = 0usize;
    detections.retain(|d| {
        let cur = idx;
        idx += 1;
        if !classifier.is_image_label(d) {
            return true;
        }
        if !bbox_covers_almost_full_page(&d.bbox, page_w, page_h, FULL_PAGE_BBOX_EPS) {
            return true;
        }
        cur == keep
    });
}

pub fn apply_full_bleed_image_bbox_expansion(
    detections: &mut Vec<Detection>,
    page_w: u32,
    page_h: u32,
    classifier: &LabelClassifier,
    enable_yolo_top_fill: bool,
) {
    let pw = page_w as f32;
    let ph = page_h as f32;
    if pw <= 1.0 || ph <= 1.0 {
        return;
    }

    const ONE_SIDED_TOP_FILL_MIN_WIDTH_FRAC: f32 = 0.82;
    const ONE_SIDED_TOP_FILL_MIN_HEIGHT_FRAC: f32 = 0.60;
    const ONE_SIDED_TOP_FILL_MAX_TOP_GAP_FRAC: f32 = 0.25;
    const ONE_SIDED_TOP_FILL_BOTTOM_EDGE_FRAC: f32 = 0.06;
    const ONE_SIDED_TOP_FILL_SIDE_EDGE_FRAC: f32 = 0.06;

    #[inline]
    fn touches_left_right_edges(b: &[f32; 4], page_w: f32) -> bool {
        let eps = page_w * ONE_SIDED_TOP_FILL_SIDE_EDGE_FRAC;
        b[0] <= eps && b[2] >= page_w - eps
    }

    #[inline]
    fn touches_bottom_edge(b: &[f32; 4], page_h: f32) -> bool {
        let eps = page_h * ONE_SIDED_TOP_FILL_BOTTOM_EDGE_FRAC;
        b[3] >= page_h - eps
    }

    #[inline]
    fn looks_like_top_clipped_full_page_image(b: &[f32; 4], page_w: f32, page_h: f32) -> bool {
        let w = (b[2] - b[0]).max(0.0);
        let h = (b[3] - b[1]).max(0.0);
        let wf = w / page_w;
        let hf = h / page_h;

        wf >= ONE_SIDED_TOP_FILL_MIN_WIDTH_FRAC
            && hf >= ONE_SIDED_TOP_FILL_MIN_HEIGHT_FRAC
            && touches_left_right_edges(b, page_w)
            && touches_bottom_edge(b, page_h)
            && b[1] <= page_h * ONE_SIDED_TOP_FILL_MAX_TOP_GAP_FRAC
    }

    for det in detections.iter_mut() {
        if !classifier.is_image_label(det) {
            continue;
        }

        let b = det.bbox;
        let w = (b[2] - b[0]).max(0.0);
        let h = (b[3] - b[1]).max(0.0);
        let wf = w / pw;
        let hf = h / ph;

        let expand_x =
            wf >= FULL_BLEED_IMAGE_MIN_WIDTH_FRAC && touches_both_horizontal_edges(&b, pw);
        let expand_y =
            hf >= FULL_BLEED_IMAGE_MIN_HEIGHT_FRAC && touches_both_vertical_edges(&b, ph);

        if expand_x || expand_y {
            det.bbox = [
                if expand_x { 0.0 } else { b[0] },
                if expand_y { 0.0 } else { b[1] },
                if expand_x { pw } else { b[2] },
                if expand_y { ph } else { b[3] },
            ];
        } else if enable_yolo_top_fill && looks_like_top_clipped_full_page_image(&b, pw, ph) {
            let mut new_b = b;
            new_b[1] = 0.0;

            if touches_left_right_edges(&new_b, pw) {
                new_b[0] = 0.0;
                new_b[2] = pw;
            }

            if touches_bottom_edge(&new_b, ph) {
                new_b[3] = ph;
            }

            crate::bbox_trace!(
                "[FULL-PAGE] yolo top-fill snap {:.0},{:.0},{:.0},{:.0} -> {:.0},{:.0},{:.0},{:.0}",
                b[0],
                b[1],
                b[2],
                b[3],
                new_b[0],
                new_b[1],
                new_b[2],
                new_b[3]
            );

            det.bbox = new_b;
        }
    }

    dedupe_full_page_image_detections(detections, classifier, page_w, page_h);
}

pub fn maybe_apply_yolo_full_page_detection(
    detections: &mut Vec<Detection>,
    page_w: u32,
    page_h: u32,
    config: &PipelineConfig,
    classifier: &LabelClassifier,
) {
    if !cfg!(target_os = "linux") {
        return;
    }
    if !config.enable_layout_detection() || !config.expand_full_bleed_figure_bboxes() {
        return;
    }
    maybe_expand_sole_image_to_full_page(detections, page_w, page_h, classifier);
    apply_full_bleed_image_bbox_expansion(detections, page_w, page_h, classifier, true);
}

pub fn is_visually_blank_page(image: &RgbImage) -> bool {
    let raw = image.as_raw();
    if raw.len() < 3 {
        return true;
    }

    let pixel_count = (image.width() as usize).saturating_mul(image.height() as usize);
    if pixel_count == 0 {
        return true;
    }

    let step = (pixel_count / 200_000).max(1);
    let mut sampled = 0usize;
    let mut non_white = 0usize;
    let mut dark = 0usize;
    let mut very_dark = 0usize;
    let mut sum = 0f64;
    let mut sum_sq = 0f64;

    for px_index in (0..pixel_count).step_by(step) {
        let base = px_index * 3;
        if base + 2 >= raw.len() {
            break;
        }

        let r = raw[base] as u16;
        let g = raw[base + 1] as u16;
        let b = raw[base + 2] as u16;
        let luma = (299u32 * r as u32 + 587u32 * g as u32 + 114u32 * b as u32) / 1000;

        sampled += 1;
        sum += luma as f64;
        sum_sq += (luma as f64) * (luma as f64);
        if luma < 245 {
            non_white += 1;
        }
        if luma < 180 {
            dark += 1;
        }
        if luma < 120 {
            very_dark += 1;
        }
    }

    if sampled == 0 {
        return true;
    }

    let non_white_ratio = non_white as f32 / sampled as f32;
    if non_white_ratio < 0.003 {
        return true;
    }

    let dark_ratio = dark as f32 / sampled as f32;
    let very_dark_ratio = very_dark as f32 / sampled as f32;
    let mean = sum / sampled as f64;
    let variance = (sum_sq / sampled as f64) - mean * mean;
    let stddev = variance.max(0.0).sqrt();

    mean >= 178.0 && stddev <= 18.0 && dark_ratio < 0.0015 && very_dark_ratio < 0.0002
}

pub fn detections_bbox_area_fraction(detections: &[Detection], width: u32, height: u32) -> f32 {
    let page_area = (width as f64) * (height as f64);
    if page_area <= 0.0 {
        return 0.0;
    }

    let mut sum: f64 = 0.0;
    for det in detections {
        let (x1, y1, x2, y2) = rounded_clamped_bbox(det.bbox, width, height);
        let w = (x2.saturating_sub(x1)) as f64;
        let h = (y2.saturating_sub(y1)) as f64;
        sum += w * h;
    }

    (sum / page_area).min(1.0) as f32
}

#[derive(Debug, Clone, Copy, Default)]
struct LayoutBlankEvidence {
    total_area_fraction: f32,
    text_area_fraction: f32,
    image_area_fraction: f32,
    max_area_fraction: f32,
}

fn layout_blank_evidence(
    detections: &[Detection],
    width: u32,
    height: u32,
    classifier: &LabelClassifier,
) -> LayoutBlankEvidence {
    let page_area = (width as f64) * (height as f64);
    if page_area <= 0.0 {
        return LayoutBlankEvidence::default();
    }

    let mut total = 0.0f64;
    let mut text = 0.0f64;
    let mut image = 0.0f64;
    let mut max_area = 0.0f64;

    for det in detections {
        let (x1, y1, x2, y2) = rounded_clamped_bbox(det.bbox, width, height);
        let area = (x2.saturating_sub(x1) as f64) * (y2.saturating_sub(y1) as f64);
        if area <= 0.0 {
            continue;
        }

        total += area;
        max_area = max_area.max(area);
        if classifier.is_image_label(det) {
            image += area;
        } else if classifier.is_text_label(det)
            && !matches!(det.category, crate::types::ContentCategory::Abandon)
        {
            text += area;
        }
    }

    LayoutBlankEvidence {
        total_area_fraction: (total / page_area).min(1.0) as f32,
        text_area_fraction: (text / page_area).min(1.0) as f32,
        image_area_fraction: (image / page_area).min(1.0) as f32,
        max_area_fraction: (max_area / page_area).min(1.0) as f32,
    }
}

pub fn should_force_blank_page_threshold(
    config: &PipelineConfig,
    has_no_filtered_detections: bool,
    page_is_visually_blank: bool,
    post_nms_detections: &[Detection],
    width: u32,
    height: u32,
    classifier: &LabelClassifier,
) -> bool {
    if config.binarization().use_fixed_threshold || config.binarization().use_heavy_duty {
        return false;
    }

    // Without layout detection we have no detection boxes to analyse, but a
    // visually blank page should still binarize to white rather than noise.
    if !config.enable_layout_detection() {
        return page_is_visually_blank;
    }

    let evidence = layout_blank_evidence(post_nms_detections, width, height, classifier);

    const MAX_VISUAL_BLANK_SPOOF_FRACTION: f32 = 0.02;
    const MAX_LAYOUT_BLANK_TOTAL_FRACTION: f32 = 0.006;
    const MAX_LAYOUT_BLANK_TEXT_FRACTION: f32 = 0.002;
    const MAX_LAYOUT_BLANK_IMAGE_FRACTION: f32 = 0.001;
    const MAX_LAYOUT_BLANK_SINGLE_BOX_FRACTION: f32 = 0.004;

    let visual_blank_with_no_real_layout = page_is_visually_blank
        && (has_no_filtered_detections
            || evidence.total_area_fraction <= MAX_VISUAL_BLANK_SPOOF_FRACTION);

    let tiny_post_nms_noise = !post_nms_detections.is_empty()
        && evidence.total_area_fraction <= MAX_LAYOUT_BLANK_TOTAL_FRACTION
        && evidence.text_area_fraction <= MAX_LAYOUT_BLANK_TEXT_FRACTION
        && evidence.image_area_fraction <= MAX_LAYOUT_BLANK_IMAGE_FRACTION
        && evidence.max_area_fraction <= MAX_LAYOUT_BLANK_SINGLE_BOX_FRACTION;

    visual_blank_with_no_real_layout || tiny_post_nms_noise
}

pub fn is_full_page_image(
    detections: &[Detection],
    page_width: u32,
    page_height: u32,
    content_bounds: Option<ContentBounds>,
) -> bool {
    if detections.len() == 1 {
        let det = &detections[0];
        let region_width = det.bbox[2] - det.bbox[0];
        let region_height = det.bbox[3] - det.bbox[1];
        let coverage = (region_width * region_height) as f32 / (page_width * page_height) as f32;
        coverage > 0.8
    } else if detections.is_empty() {
        if let Some(bounds) = content_bounds {
            let coverage =
                (bounds.width() * bounds.height()) as f32 / (page_width * page_height) as f32;
            coverage > 0.8
        } else {
            false
        }
    } else {
        false
    }
}

pub fn classify_page(
    detections: &[Detection],
    page_width: u32,
    page_height: u32,
    pixel_bounds: Option<ContentBounds>,
) -> PageClassification {
    let detection_bounds = if detections.is_empty() {
        None
    } else {
        crate::margin::calculate_content_bounds(detections, page_width, page_height, false)
    };

    let content_bounds = detection_bounds.or(pixel_bounds);
    let is_blank = detections.is_empty() && pixel_bounds.is_none();
    let is_full_page_image =
        is_full_page_image(detections, page_width, page_height, content_bounds);

    PageClassification {
        content_bounds,
        is_blank,
        is_full_page_image,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentCategory, LABEL_CLASSIFIER};

    fn test_detection(category: ContentCategory, bbox: [f32; 4]) -> Detection {
        Detection {
            class_id: match category {
                ContentCategory::Text => 2,
                ContentCategory::Image => 1,
                ContentCategory::Table => 8,
                ContentCategory::Abandon => 0,
            },
            class_name: Some(
                match category {
                    ContentCategory::Text => "text",
                    ContentCategory::Image => "image",
                    ContentCategory::Table => "table",
                    ContentCategory::Abandon => "abandon",
                }
                .to_string(),
            ),
            confidence: 0.8,
            bbox,
            category,
            context: None,
        }
    }

    #[test]
    fn aged_low_contrast_page_counts_as_visually_blank() {
        let image = RgbImage::from_pixel(100, 100, image::Rgb([210, 207, 198]));
        assert!(is_visually_blank_page(&image));
    }

    #[test]
    fn post_nms_tiny_noise_boxes_force_blank_fallback() {
        let config = PipelineConfig::default();
        let detections = vec![test_detection(
            ContentCategory::Abandon,
            [10.0, 10.0, 14.0, 16.0],
        )];

        assert!(should_force_blank_page_threshold(
            &config,
            false,
            false,
            &detections,
            1000,
            1000,
            &LABEL_CLASSIFIER,
        ));
    }

    #[test]
    fn substantial_image_box_blocks_blank_fallback() {
        let config = PipelineConfig::default();
        let detections = vec![test_detection(
            ContentCategory::Image,
            [100.0, 100.0, 600.0, 600.0],
        )];

        assert!(!should_force_blank_page_threshold(
            &config,
            false,
            true,
            &detections,
            1000,
            1000,
            &LABEL_CLASSIFIER,
        ));
    }

    #[test]
    fn no_layout_boxes_alone_does_not_force_nonvisual_blank_page() {
        let config = PipelineConfig::default();

        assert!(!should_force_blank_page_threshold(
            &config,
            true,
            false,
            &[],
            1000,
            1000,
            &LABEL_CLASSIFIER,
        ));
    }
}
