//! resize_context.rs
//! Centralized resize + coordinate mapping logic for all pipelines (core + djvu + future variants).
//! This is the SINGLE SOURCE OF TRUTH for:
//!   * Building inference images (direct resize or letterbox)
//!   * Mapping detections between inference space and page space
//!   * Future: deskew-adjusted coordinate transforms
//!   * Future: margin-corrected coordinate transforms / normalization
//!
//! All callers should avoid duplicating math and instead use the helpers here.
//!
//! Design principles:
//!   * No async; pure functions or small structs.
//!   * Deterministic, side-effect free (except optional logging via dbglog!).
//!   * Explicit resize policy enum so changes are compile-time visible.

use crate::resize::{ResizeMethod, ResizeParams};
use anyhow::{Result, anyhow};
use image::RgbImage;

/// Enumerates supported resize policies.
/// Supported inference resize policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizePolicy {
    /// Direct stretch to target WxH (bilinear). Caller responsible for aspect choices.
    Direct,
    /// Preserve aspect ratio using letterbox padding.
    Letterbox,
}

/// Configuration describing an inference resize.
#[derive(Debug, Clone)]
pub struct InferenceResizeSpec {
    pub target: u32,          // square target dimension (e.g., 640)
    pub policy: ResizePolicy, // how to map
}

impl Default for InferenceResizeSpec {
    fn default() -> Self {
        Self {
            target: 640,
            policy: ResizePolicy::Direct,
        }
    }
}

/// Result bundling source + inference images.
#[derive(Debug, Clone)]
pub struct InferenceImages {
    pub high_res: RgbImage,  // original (possibly deskewed / margin-corrected)
    pub inference: RgbImage, // resized for model
}

/// Compute scale factors (inference -> page) given page dimensions and spec.
/// For Direct: sx = page_w/target, sy = page_h/target.
#[inline]
pub fn scale_factors_infer_to_page(
    page_w: u32,
    page_h: u32,
    spec: &InferenceResizeSpec,
) -> (f32, f32) {
    match spec.policy {
        ResizePolicy::Direct => {
            let t = spec.target as f32;
            (page_w as f32 / t, page_h as f32 / t)
        }
        ResizePolicy::Letterbox => {
            let scale = letterbox_scale(page_w, page_h, spec.target);
            let inv = if scale > 0.0 { 1.0 / scale } else { 1.0 };
            (inv, inv)
        }
    }
}

#[inline]
fn letterbox_scale(page_w: u32, page_h: u32, target: u32) -> f32 {
    let t = target as f32;
    (t / page_w as f32).min(t / page_h as f32)
}

#[inline]
fn letterbox_padding(page_w: u32, page_h: u32, target: u32) -> (f32, f32) {
    let scale = letterbox_scale(page_w, page_h, target);
    let resized_w = page_w as f32 * scale;
    let resized_h = page_h as f32 * scale;
    (
        (target as f32 - resized_w) * 0.5,
        (target as f32 - resized_h) * 0.5,
    )
}

/// Map bbox from inference (0..target) space to page coordinates.
#[inline]
pub fn map_bbox_infer_to_page(
    b: [f32; 4],
    page_w: u32,
    page_h: u32,
    spec: &InferenceResizeSpec,
) -> [f32; 4] {
    match spec.policy {
        ResizePolicy::Direct => {
            let (sx, sy) = scale_factors_infer_to_page(page_w, page_h, spec);
            [
                (b[0] * sx).clamp(0.0, page_w as f32),
                (b[1] * sy).clamp(0.0, page_h as f32),
                (b[2] * sx).clamp(0.0, page_w as f32),
                (b[3] * sy).clamp(0.0, page_h as f32),
            ]
        }
        ResizePolicy::Letterbox => {
            let (pad_x, pad_y) = letterbox_padding(page_w, page_h, spec.target);
            let scale = letterbox_scale(page_w, page_h, spec.target);
            let inv = if scale > 0.0 { 1.0 / scale } else { 1.0 };
            [
                ((b[0] - pad_x) * inv).clamp(0.0, page_w as f32),
                ((b[1] - pad_y) * inv).clamp(0.0, page_h as f32),
                ((b[2] - pad_x) * inv).clamp(0.0, page_w as f32),
                ((b[3] - pad_y) * inv).clamp(0.0, page_h as f32),
            ]
        }
    }
}

/// Detect whether bbox appears to still be in inference space (heuristic upper bound check).
#[inline]
pub fn is_in_inference_space(b: &[f32; 4], spec: &InferenceResizeSpec) -> bool {
    b[2] <= spec.target as f32 + 1.0 && b[3] <= spec.target as f32 + 1.0
}

/// Build inference image according to spec (Direct policy only for now).
/// Caller passes a cloneable reference to the high_res image.
pub fn build_inference_image(high_res: &RgbImage, spec: &InferenceResizeSpec) -> Result<RgbImage> {
    #[cfg(feature = "debug-logging")]
    crate::debug_println!(
        "RESIZE INFERENCE: Input image: {}x{}, Target: {}x{}, Policy: {:?}",
        high_res.width(),
        high_res.height(),
        spec.target,
        spec.target,
        spec.policy
    );

    match spec.policy {
        ResizePolicy::Direct | ResizePolicy::Letterbox => {
            let params = ResizeParams {
                target_width: spec.target,
                target_height: spec.target,
                method: ResizeMethod::Bilinear,
                letterbox: matches!(spec.policy, ResizePolicy::Letterbox),
                border_value: 114.0,
                swap_rb: false,
            };

            #[cfg(feature = "debug-logging")]
            crate::debug_println!(
                "RESIZE INFERENCE: Using resize params - Method: {:?}, Letterbox: {}, Border value: {}",
                params.method,
                params.letterbox,
                params.border_value
            );

            let resized = crate::resize::resize_bytes(
                high_res.as_raw(),
                high_res.width(),
                high_res.height(),
                &params,
                3,
            )
            .map_err(|e| anyhow!("Inference resize failed: {e}"))?;

            #[cfg(feature = "debug-logging")]
            crate::debug_println!(
                "RESIZE INFERENCE: Resize completed from {}x{} to {}x{}, Output buffer size: {} bytes",
                high_res.width(),
                high_res.height(),
                spec.target,
                spec.target,
                resized.len()
            );

            RgbImage::from_raw(spec.target, spec.target, resized)
                .ok_or_else(|| anyhow!("Failed to build inference image buffer"))
        }
    }
}

/// Optional unified remap used by djvu masking to avoid duplicated heuristics.
#[inline]
pub fn maybe_remap_bbox_from_infer(
    b: [f32; 4],
    page_w: u32,
    page_h: u32,
    spec: &InferenceResizeSpec,
) -> [f32; 4] {
    if is_in_inference_space(&b, spec) && (page_w > spec.target || page_h > spec.target) {
        map_bbox_infer_to_page(b, page_w, page_h, spec)
    } else {
        b
    }
}

/// Future placeholders for deskew / margin transforms.
#[derive(Debug, Clone, Default)]
pub struct DeskewTransform {/* rotation matrix, etc. */}

/// Represents margin correction transformations applied to a page.
/// Contains the offset and scale factors needed to map coordinates
/// from the original page space to the margin-corrected page space.
#[derive(Debug, Clone, Default)]
pub struct MarginCorrection {
    pub offset_x: f32,
    pub offset_y: f32,
    pub scale_x: f32,
    pub scale_y: f32,
}

impl MarginCorrection {
    pub fn new(offset_x: f32, offset_y: f32, scale_x: f32, scale_y: f32) -> Self {
        Self {
            offset_x,
            offset_y,
            scale_x,
            scale_y,
        }
    }
}

/// Apply deskew + margin corrections to a page-space bbox.
#[inline]
pub fn apply_page_adjustments(
    b: [f32; 4],
    _deskew: Option<&DeskewTransform>,
    margin: Option<&MarginCorrection>,
) -> [f32; 4] {
    let mut result = b;

    if let Some(margin_correction) = margin {
        // Apply margin correction transformation
        // Scale coordinates first, then apply offset
        result[0] = b[0] * margin_correction.scale_x + margin_correction.offset_x; // x1
        result[1] = b[1] * margin_correction.scale_y + margin_correction.offset_y; // y1
        result[2] = b[2] * margin_correction.scale_x + margin_correction.offset_x; // x2
        result[3] = b[3] * margin_correction.scale_y + margin_correction.offset_y; // y2
    }

    result
}

/// High-level pipeline step: full transform from inference bbox -> final page bbox with adjustments.
#[inline]
pub fn full_infer_bbox_to_final_page(
    b: [f32; 4],
    page_w: u32,
    page_h: u32,
    spec: &InferenceResizeSpec,
    deskew: Option<&DeskewTransform>,
    margin: Option<&MarginCorrection>,
) -> [f32; 4] {
    let mut bb = map_bbox_infer_to_page(b, page_w, page_h, spec);
    bb = apply_page_adjustments(bb, deskew, margin);
    bb
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_scale_and_map() {
        let spec = InferenceResizeSpec::default();
        let mapped = map_bbox_infer_to_page([0.0, 0.0, 640.0, 640.0], 1280, 1920, &spec);
        assert_eq!(mapped, [0.0, 0.0, 1280.0, 1920.0]);
    }
    #[test]
    fn test_maybe_remap() {
        let spec = InferenceResizeSpec::default();
        let b = maybe_remap_bbox_from_infer([10.0, 20.0, 30.0, 40.0], 2560, 1600, &spec);
        assert!(b[0] > 10.0 && b[1] > 20.0); // scaled up
    }

    #[test]
    fn test_letterbox_mapping() {
        let spec = InferenceResizeSpec {
            target: 1024,
            policy: ResizePolicy::Letterbox,
        };
        let mapped = map_bbox_infer_to_page([0.0, 128.0, 1024.0, 896.0], 1000, 750, &spec);
        assert_eq!(mapped, [0.0, 0.0, 1000.0, 750.0]);
    }
}
