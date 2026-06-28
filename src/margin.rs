// src/margin.rs

//! # Page Margin Processing Module
//!
//! Provides functionality to standardize and re-center the content of scanned pages
//! based on layout detection results. This ensures a consistent and clean appearance
//! on e-readers, eliminating the need for manual cropping and zooming.
//!
//! The two main features are:
//! 1.  **Standardize and Center**: Creates a new page with standard dimensions (typically
//!     from the cover page) and centers the original page's content within it,
//!     creating uniform margins.
//! 2.  **Crop and Resize**: Removes all margins by cropping the page to the content
//!     area, then resizes it to a standard height, maximizing screen real estate on
//!     e-readers.

use crate::engine::Detection;
use crate::pipeline::policies::MarginCorrection;
use anyhow::{Result, anyhow};
use image::{Rgb, RgbImage};
use std::collections::HashMap;

/// Input data for a single page when performing document-wide margin analysis.
#[derive(Debug, Clone)]
pub struct PageMarginInput {
    pub page_index: usize,
    pub page_width: u32,
    pub page_height: u32,
    pub detections: Vec<Detection>,
    pub pixel_bounds: Option<ContentBounds>,
}

/// Defines the user's desired margin processing behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginSettings {
    /// Do not perform any margin adjustments.
    None,
    /// Center content on a standard-sized canvas, creating uniform margins.
    StandardizeAndCenter,
    /// Crop to content and resize to a standard height, removing all margins.
    CropAndResize,
}

/// Stores the standardized dimensions (width, height) for all pages in a document.
/// This is typically derived from the first page (cover) to ensure consistency.
#[derive(Debug, Clone, Copy)]
pub struct StandardPageDimensions {
    pub width: u32,
    pub height: u32,
}

/// Represents the bounding box of all detected content on a single page.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContentBounds {
    pub min_x: u32,
    pub min_y: u32,
    pub max_x: u32,
    pub max_y: u32,
}

/// Margin information for a single page
#[derive(Debug, Clone)]
pub struct PageMarginData {
    pub page_index: usize,
    pub page_width: u32,
    pub page_height: u32,
    pub content_bounds: Option<ContentBounds>,
    pub is_blank: bool,
    pub is_full_page_image: bool,
    pub margin_left: u32,
    pub margin_right: u32,
    pub margin_top: u32,
    pub margin_bottom: u32,
}

#[derive(Debug, Clone)]
struct CropBoundsCandidate {
    page_width: u32,
    page_height: u32,
    bounds: ContentBounds,
}

/// Analysis results for the entire document's margins
#[derive(Debug, Clone)]
pub struct DocumentMarginAnalysis {
    /// Per-page raw analysis data.
    pub pages: HashMap<usize, PageMarginData>,
    /// The statistically robust baseline for content, calculated from stable pages.
    pub baseline_bounds: ContentBounds,
    /// Uniform document crop rectangle, calculated from stable text pages.
    pub crop_bounds: ContentBounds,
    /// The standard aspect ratio derived from the baseline bounds.
    pub standard_aspect_ratio: f32,
    /// The final, effective margin setting to be used for processing.
    pub effective_margin_setting: MarginSettings,
    /// An optional message explaining why the margin setting was changed (e.g., footnotes).
    pub setting_override_reason: Option<String>,
    /// The resolution (width) at which analysis was performed
    pub analysis_width: u32,
    /// The resolution (height) at which analysis was performed
    pub analysis_height: u32,
}

impl ContentBounds {
    /// Returns the width of the content area.
    pub fn width(&self) -> u32 {
        self.max_x - self.min_x
    }

    /// Returns the height of the content area.
    pub fn height(&self) -> u32 {
        self.max_y - self.min_y
    }

    /// Scale bounds from one resolution to another
    ///
    /// This is critical for handling coordinate space transformations when margin analysis
    /// is performed at a different resolution than processing (e.g., analysis at 640px,
    /// processing at 1200px).
    ///
    /// # Arguments
    /// * `from_width` - Original width resolution
    /// * `from_height` - Original height resolution
    /// * `to_width` - Target width resolution
    /// * `to_height` - Target height resolution
    ///
    /// # Returns
    /// A new `ContentBounds` scaled to the target resolution
    pub fn scale_to_resolution(
        &self,
        from_width: u32,
        from_height: u32,
        to_width: u32,
        to_height: u32,
    ) -> ContentBounds {
        let scale_x = to_width as f32 / from_width as f32;
        let scale_y = to_height as f32 / from_height as f32;

        ContentBounds {
            min_x: (self.min_x as f32 * scale_x).round() as u32,
            min_y: (self.min_y as f32 * scale_y).round() as u32,
            max_x: (self.max_x as f32 * scale_x).round() as u32,
            max_y: (self.max_y as f32 * scale_y).round() as u32,
        }
    }
}

/// Computes pixel-level content bounds for a page by binarizing it and analyzing the mask.
///
/// This is the primary method for margin detection when layout detection is disabled or
/// produces no detections. It binarizes the input image using the provided config settings,
/// then calculates content bounds from the resulting binary mask.
///
/// # Arguments
/// * `image` - The high-resolution RGB page image
/// * `config` - Pipeline configuration containing binarization settings
///
/// # Returns
/// `Option<ContentBounds>` - The detected content bounds, or None if the page is blank
pub fn compute_pixel_bounds_for_margin(
    image: &image::RgbImage,
    config: &crate::pipeline::config::PipelineConfig,
) -> Option<ContentBounds> {
    crate::pipeline::page_analysis::compute_pixel_bounds_for_margin(image, config)
}

/// Analyzes a set of detections to find the absolute outer bounds of the page content.
///
/// Iterates through all bounding boxes and finds the minimum and maximum x/y coordinates.
///
/// # Arguments
/// * `detections`: A slice of `Detection` from the layout analysis.
/// * `page_width`, `page_height`: The dimensions of the image the detections belong to.
/// * `filter_for_cropping`: If true, filters out page numbers, headers, and footers
///
/// # Returns
/// An `Option<ContentBounds>`. Returns `None` if no valid detections are found.
pub fn calculate_content_bounds(
    detections: &[Detection],
    page_width: u32,
    page_height: u32,
    filter_for_cropping: bool,
) -> Option<ContentBounds> {
    if detections.is_empty() {
        return None;
    }

    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0;
    let mut max_y = 0;

    for det in detections {
        // Use standardized label classification for margin calculation
        let classifier = &crate::types::LABEL_CLASSIFIER;
        if !classifier.should_include_in_margin_calc(det, filter_for_cropping) {
            continue;
        }

        // Ensure bbox coordinates are within the page dimensions.
        let x1 = (det.bbox[0] as u32).min(page_width);
        let y1 = (det.bbox[1] as u32).min(page_height);
        let x2 = (det.bbox[2] as u32).min(page_width);
        let y2 = (det.bbox[3] as u32).min(page_height);

        min_x = min_x.min(x1);
        min_y = min_y.min(y1);
        max_x = max_x.max(x2);
        max_y = max_y.max(y2);
    }

    // If bounds are still default, it means no valid detections were processed.
    if min_x == u32::MAX || max_x == 0 {
        return None;
    }

    // Add a small padding to avoid cutting off edges of detections
    const PADDING: u32 = 5;
    min_x = min_x.saturating_sub(PADDING);
    min_y = min_y.saturating_sub(PADDING);
    max_x = (max_x + PADDING).min(page_width);
    max_y = (max_y + PADDING).min(page_height);

    let result = Some(ContentBounds {
        min_x,
        min_y,
        max_x,
        max_y,
    });
    #[cfg(feature = "debug-logging")]
    if let Some(b) = result.as_ref() {
        crate::info_log!(
            "[Bounds] detections={} filter_for_cropping={} -> min=({}, {}) max=({}, {}), size={}x{} page={}x{}",
            detections.len(),
            filter_for_cropping,
            b.min_x,
            b.min_y,
            b.max_x,
            b.max_y,
            b.width(),
            b.height(),
            page_width,
            page_height
        );
    }
    result
}

/// Calculates content bounds from a binary mask created from pixel analysis.
///
/// The mask is expected to be a flat array of length `width * height`
/// where non-zero values mark foreground (content) pixels.
pub fn calculate_content_bounds_from_binary_mask(
    mask: &[u8],
    width: u32,
    height: u32,
) -> Option<ContentBounds> {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || mask.len() != w * h {
        return None;
    }

    let mut row_counts = vec![0usize; h];
    let mut col_counts = vec![0usize; w];

    for y in 0..h {
        let row_offset = y * w;
        let mut row_sum = 0usize;
        for x in 0..w {
            let value = mask[row_offset + x];
            if value != 0 {
                row_sum += 1;
                col_counts[x] += 1;
            }
        }
        row_counts[y] = row_sum;
    }

    let max_row = *row_counts.iter().max().unwrap_or(&0);
    let max_col = *col_counts.iter().max().unwrap_or(&0);
    if max_row == 0 || max_col == 0 {
        return None;
    }

    fn find_bounds(counts: &[usize], threshold: usize) -> Option<(usize, usize)> {
        let mut first = None;
        let mut last = None;
        for (idx, &count) in counts.iter().enumerate() {
            if count >= threshold {
                if first.is_none() {
                    first = Some(idx);
                }
                last = Some(idx);
            }
        }
        match (first, last) {
            (Some(f), Some(l)) if l >= f => Some((f, l)),
            _ => None,
        }
    }

    let mut row_threshold = ((max_row as f32) * 0.05).round() as usize;
    if row_threshold < 4 {
        row_threshold = 4;
    }
    let mut col_threshold = ((max_col as f32) * 0.05).round() as usize;
    if col_threshold < 4 {
        col_threshold = 4;
    }

    let row_bounds =
        find_bounds(&row_counts, row_threshold).or_else(|| find_bounds(&row_counts, 1))?;
    let col_bounds =
        find_bounds(&col_counts, col_threshold).or_else(|| find_bounds(&col_counts, 1))?;

    let (top, bottom) = row_bounds;
    let (left, right) = col_bounds;

    let mut min_y = top as u32;
    let mut max_y = (bottom + 1) as u32;
    let mut min_x = left as u32;
    let mut max_x = (right + 1) as u32;

    const PADDING: u32 = 5;
    min_x = min_x.saturating_sub(PADDING);
    min_y = min_y.saturating_sub(PADDING);
    max_x = (max_x + PADDING).min(width);
    max_y = (max_y + PADDING).min(height);

    if max_x <= min_x || max_y <= min_y {
        return None;
    }

    Some(ContentBounds {
        min_x,
        min_y,
        max_x,
        max_y,
    })
}

/// Main dispatcher for margin processing.
/// Takes an image and its content bounds, and applies the chosen margin strategy.
pub fn process_page_margins(
    original_image: &RgbImage,
    bounds: &ContentBounds,
    settings: MarginSettings,
    standard_dims: &StandardPageDimensions,
    target_width_for_resize: Option<u32>,
    target_height_for_resize: u32,
) -> Result<RgbImage> {
    // Derive the target aspect ratio from explicit output dimensions when
    // present, otherwise from the document-standard dimensions.
    let standard_aspect_ratio = target_aspect_ratio(
        standard_dims,
        target_width_for_resize,
        target_height_for_resize,
    );

    #[cfg(feature = "debug-logging")]
    crate::debug_println!(
        "MARGIN PROCESSING: Input image {}x{}, Settings {:?}, Standard dims {}x{} (aspect {:.3}), Target resize {}x{}",
        original_image.width(),
        original_image.height(),
        settings,
        standard_dims.width,
        standard_dims.height,
        standard_aspect_ratio,
        target_width_for_resize.unwrap_or(0),
        target_height_for_resize
    );

    match settings {
        MarginSettings::StandardizeAndCenter => standardize_and_center_page(
            original_image,
            bounds,
            target_width_for_resize,
            target_height_for_resize,
            standard_aspect_ratio,
        ),
        MarginSettings::CropAndResize => {
            // Enforce the document-wide standard aspect ratio by adjusting bounds before resize
            crop_and_resize_with_standard_aspect_ratio(
                original_image,
                bounds,
                standard_aspect_ratio,
                target_width_for_resize,
                target_height_for_resize,
            )
        }
        MarginSettings::None => {
            // Should not be called if settings are None, but we can gracefully return original.
            #[cfg(feature = "debug-logging")]
            crate::debug_println!(
                "MARGIN PROCESSING: No margin processing, returning original image"
            );
            Ok(original_image.clone())
        }
    }
}

/// Computes margin correction parameters for mapping coordinates from source to destination space.
pub fn compute_margin_correction(
    original_bounds: &ContentBounds,
    settings: MarginSettings,
    standard_dims: &StandardPageDimensions,
    target_width_for_resize: Option<u32>,
    target_height_for_resize: u32,
    orig_image_dims: Option<(u32, u32)>,
) -> MarginCorrection {
    let standard_aspect_ratio = target_aspect_ratio(
        standard_dims,
        target_width_for_resize,
        target_height_for_resize,
    );
    let target_width = resolve_target_width(
        target_width_for_resize,
        target_height_for_resize,
        standard_aspect_ratio,
    );

    match settings {
        MarginSettings::StandardizeAndCenter => {
            let cw = original_bounds.width();
            let ch = original_bounds.height();

            // Calculate scale and offset for standardize and center - use consistent f64 precision
            let sx = target_width as f64 / cw as f64;
            let sy = target_height_for_resize as f64 / ch as f64;
            let s = sx.min(sy);
            let scaled_w = (cw as f64 * s).round();
            let scaled_h = (ch as f64 * s).round();
            let offset_x = ((target_width as f64 - scaled_w) / 2.0).max(0.0);
            let offset_y = ((target_height_for_resize as f64 - scaled_h) / 2.0).max(0.0);

            // The transformation from original bounds to final image:
            // 1. Translate by -original_bounds.min_x, -original_bounds.min_y (to make content start at 0,0)
            // 2. Scale by s
            // 3. Translate by offset_x, offset_y
            // So: final_x = (orig_x - min_x) * s + offset_x
            //     final_y = (orig_y - min_y) * s + offset_y
            // This is equivalent to: final_x = orig_x * s + (-min_x * s + offset_x)
            //                        final_y = orig_y * s + (-min_y * s + offset_y)
            MarginCorrection::new(
                (-(original_bounds.min_x as f64) * s + offset_x) as f32,
                (-(original_bounds.min_y as f64) * s + offset_y) as f32,
                s as f32,
                s as f32,
            )
        }
        MarginSettings::CropAndResize => {
            // Adjust bounds to enforce the standard aspect ratio
            let adjusted = adjust_bounds_to_standard_aspect_ratio(
                original_bounds,
                standard_aspect_ratio,
                orig_image_dims,
            );

            let cw = adjusted.width();
            let ch = adjusted.height();
            if ch == 0 || cw == 0 {
                return MarginCorrection::default();
            }

            let sx = target_width as f64 / cw as f64;
            let sy = target_height_for_resize as f64 / ch as f64;

            let tx = -(adjusted.min_x as f64);
            let ty = -(adjusted.min_y as f64);

            // The transformation is:
            // 1. Translate by tx, ty (to move crop area to origin)
            // 2. Scale by sx, sy
            // So: final_x = (orig_x + tx) * sx
            //     final_y = (orig_y + ty) * sy
            // This is equivalent to: final_x = orig_x * sx + (tx * sx)
            //                        final_y = orig_y * sy + (ty * sy)
            MarginCorrection::new((tx * sx) as f32, (ty * sy) as f32, sx as f32, sy as f32)
        }
        MarginSettings::None => MarginCorrection::default(),
    }
}

/// Transforms detection coordinates from the original image space to the new, adjusted image space.
/// This is CRITICAL for ensuring subsequent processing (like image region extraction) works correctly.
pub fn transform_detections(
    original_detections: &[Detection],
    original_bounds: &ContentBounds,
    settings: MarginSettings,
    standard_dims: &StandardPageDimensions,
    target_width_for_resize: Option<u32>,
    target_height_for_resize: u32,
    orig_image_dims: Option<(u32, u32)>,
) -> Vec<Detection> {
    // Fully integer-safe mapping: compute integer scales and offsets using rounding
    // and apply rounding at each step to keep bbox pixel-aligned with image operations.
    let mut new_detections = original_detections.to_vec();

    match settings {
        MarginSettings::StandardizeAndCenter => {
            let cw = original_bounds.width();
            let ch = original_bounds.height();

            // Target canvas from standard aspect ratio and target height
            let standard_aspect_ratio = target_aspect_ratio(
                standard_dims,
                target_width_for_resize,
                target_height_for_resize,
            );
            let target_h = target_height_for_resize;
            let target_w =
                resolve_target_width(target_width_for_resize, target_h, standard_aspect_ratio);

            // Use consistent floating-point precision for calculations
            // Convert to f64 for calculations to maintain precision, then back to f32
            let sx = target_w as f64 / cw as f64;
            let sy = target_h as f64 / ch as f64;
            let s = sx.min(sy);
            let scaled_w = (cw as f64 * s).round() as f64;
            let scaled_h = (ch as f64 * s).round() as f64;
            let off_x = ((target_w as f64 - scaled_w) / 2.0).max(0.0);
            let off_y = ((target_h as f64 - scaled_h) / 2.0).max(0.0);

            for det in &mut new_detections {
                let x1 = det.bbox[0] as f64 - original_bounds.min_x as f64;
                let y1 = det.bbox[1] as f64 - original_bounds.min_y as f64;
                let x2 = det.bbox[2] as f64 - original_bounds.min_x as f64;
                let y2 = det.bbox[3] as f64 - original_bounds.min_y as f64;

                let nx1 = (x1 * s).round() + off_x;
                let ny1 = (y1 * s).round() + off_y;
                let nx2 = (x2 * s).round() + off_x;
                let ny2 = (y2 * s).round() + off_y;

                det.bbox[0] = nx1.max(0.0) as f32;
                det.bbox[1] = ny1.max(0.0) as f32;
                det.bbox[2] = nx2.max(1.0) as f32;
                det.bbox[3] = ny2.max(1.0) as f32;
            }
        }
        MarginSettings::CropAndResize => {
            // Adjust bounds to enforce the standard aspect ratio, mirroring image processing path
            let standard_aspect_ratio = target_aspect_ratio(
                standard_dims,
                target_width_for_resize,
                target_height_for_resize,
            );

            // We cannot clamp without image size here; assume processing used the same adjusted bounds
            let adjusted = adjust_bounds_to_standard_aspect_ratio(
                original_bounds,
                standard_aspect_ratio,
                orig_image_dims,
            );

            let cw = adjusted.width();
            let ch = adjusted.height();
            if ch == 0 || cw == 0 {
                return new_detections;
            }

            // Final size
            let target_w = resolve_target_width(
                target_width_for_resize,
                target_height_for_resize,
                standard_aspect_ratio,
            );
            let sx = target_w as f64 / cw as f64;
            let sy = target_height_for_resize as f64 / ch as f64;

            let tx = -(adjusted.min_x as f64);
            let ty = -(adjusted.min_y as f64);

            for det in &mut new_detections {
                let x1 = det.bbox[0] as f64 + tx;
                let y1 = det.bbox[1] as f64 + ty;
                let x2 = det.bbox[2] as f64 + tx;
                let y2 = det.bbox[3] as f64 + ty;

                let nx1 = (x1 * sx).round();
                let ny1 = (y1 * sy).round();
                let nx2 = (x2 * sx).round();
                let ny2 = (y2 * sy).round();

                // Ensure the transformed coordinates are within the bounds of the new image
                let target_w_f64 = target_w as f64;
                let target_h_f64 = target_height_for_resize as f64;

                det.bbox[0] = nx1.max(0.0).min(target_w_f64 - 1.0) as f32;
                det.bbox[1] = ny1.max(0.0).min(target_h_f64 - 1.0) as f32;
                det.bbox[2] = nx2.max(1.0).min(target_w_f64) as f32;
                det.bbox[3] = ny2.max(1.0).min(target_h_f64) as f32;
            }
        }
        MarginSettings::None => {
            // No transformation needed.
        }
    }

    new_detections
}

/// Analyzes margins across all pages of a document to establish baseline margins
/// and detect edge cases like blank pages, handwriting in margins, etc.
pub fn analyze_document_margins(
    all_page_data: &[PageMarginInput],
    config: &crate::pipeline::config::PipelineConfig,
    original_margin_setting: MarginSettings,
    crop_footnotes: bool,
) -> DocumentMarginAnalysis {
    let mut pages = HashMap::new();
    let mut baseline_candidates = Vec::new();
    let mut fallback_baseline_candidates = Vec::new();

    // First pass: analyze each page individually
    for page_input in all_page_data {
        let page_margin_data = analyze_single_page_margins(page_input, config);

        // Prefer pages after the front matter, but keep an early-page fallback for
        // short documents or selected page ranges.
        if should_use_page_for_baseline(&page_margin_data) {
            if page_input.page_index >= 5 {
                baseline_candidates.push(page_margin_data.clone());
            }
            fallback_baseline_candidates.push(page_margin_data.clone());
        }

        pages.insert(page_input.page_index, page_margin_data);
    }

    if baseline_candidates.is_empty() {
        baseline_candidates = fallback_baseline_candidates;
    }

    // Calculate baseline margins from running average
    let mut baseline_bounds = calculate_robust_baseline_bounds(&baseline_candidates);
    if baseline_bounds.width() == 0 || baseline_bounds.height() == 0 {
        let mut widths: Vec<u32> = all_page_data
            .iter()
            .map(|p| p.page_width)
            .filter(|&w| w > 0)
            .collect();
        let mut heights: Vec<u32> = all_page_data
            .iter()
            .map(|p| p.page_height)
            .filter(|&h| h > 0)
            .collect();
        widths.sort();
        heights.sort();
        let fallback_width = widths
            .get(widths.len().saturating_sub(1) / 2)
            .copied()
            .unwrap_or(640);
        let fallback_height = heights
            .get(heights.len().saturating_sub(1) / 2)
            .copied()
            .unwrap_or(800);
        baseline_bounds = ContentBounds {
            min_x: 0,
            min_y: 0,
            max_x: fallback_width,
            max_y: fallback_height,
        };
    }

    let crop_bounds = calculate_uniform_crop_bounds(all_page_data, config, &baseline_bounds);

    // Calculate standard aspect ratio for consistent cropping
    let standard_aspect_ratio =
        calculate_standard_aspect_ratio(&baseline_candidates, &baseline_bounds);

    // Check for footnotes across all pages and determine effective margin setting
    let has_footnotes = detect_footnotes_in_document(all_page_data);
    let mut setting_override_reason = None;
    let effective_margin_setting = if has_footnotes
        && original_margin_setting == MarginSettings::CropAndResize
        && !crop_footnotes
    {
        setting_override_reason = Some(
            "Footnotes detected. Switched to 'Standardize and Center' to avoid cropping them."
                .to_string(),
        );
        MarginSettings::StandardizeAndCenter
    } else {
        original_margin_setting
    };

    // Get median dimensions from all pages for resolution tracking
    // This captures the resolution at which analysis was performed
    let (analysis_width, analysis_height) = if !all_page_data.is_empty() {
        let mut widths: Vec<u32> = all_page_data
            .iter()
            .map(|p| p.page_width)
            .filter(|&w| w > 0)
            .collect();
        let mut heights: Vec<u32> = all_page_data
            .iter()
            .map(|p| p.page_height)
            .filter(|&h| h > 0)
            .collect();
        widths.sort();
        heights.sort();
        (
            widths.get(widths.len() / 2).copied().unwrap_or(640),
            heights.get(heights.len() / 2).copied().unwrap_or(800),
        )
    } else {
        (640, 800) // fallback default
    };

    DocumentMarginAnalysis {
        pages,
        baseline_bounds,
        crop_bounds,
        standard_aspect_ratio,
        effective_margin_setting,
        setting_override_reason,
        analysis_width,
        analysis_height,
    }
}

/// Analyzes margins for a single page
fn analyze_single_page_margins(
    page: &PageMarginInput,
    config: &crate::pipeline::config::PipelineConfig,
) -> PageMarginData {
    let mut detections = page.detections.clone();
    crate::pipeline::page_analysis::maybe_apply_yolo_full_page_detection(
        &mut detections,
        page.page_width,
        page.page_height,
        config,
        &crate::types::LABEL_CLASSIFIER,
    );

    let classification = crate::pipeline::page_analysis::classify_page(
        &detections,
        page.page_width,
        page.page_height,
        page.pixel_bounds,
    );

    let content_bounds = classification.content_bounds;
    let is_blank = classification.is_blank;
    let is_full_page_image = classification.is_full_page_image;

    let (margin_left, margin_right, margin_top, margin_bottom) =
        if let Some(bounds) = content_bounds {
            (
                bounds.min_x,
                page.page_width.saturating_sub(bounds.max_x),
                bounds.min_y,
                page.page_height.saturating_sub(bounds.max_y),
            )
        } else {
            (0, 0, 0, 0)
        };

    let data = PageMarginData {
        page_index: page.page_index,
        page_width: page.page_width,
        page_height: page.page_height,
        content_bounds,
        is_blank,
        is_full_page_image,
        margin_left,
        margin_right,
        margin_top,
        margin_bottom,
    };

    #[cfg(feature = "debug-logging")]
    {
        if let Some(cb) = data.content_bounds {
            crate::info_log!(
                "[Margins/Page] p{} blank={} full_image={} margins L{} R{} T{} B{} bounds=({},{})->({}, {})",
                page.page_index + 1,
                data.is_blank,
                data.is_full_page_image,
                data.margin_left,
                data.margin_right,
                data.margin_top,
                data.margin_bottom,
                cb.min_x,
                cb.min_y,
                cb.max_x,
                cb.max_y
            );
        } else {
            crate::info_log!(
                "[Margins/Page] p{} blank={} full_image={} (no bounds computed)",
                page.page_index + 1,
                data.is_blank,
                data.is_full_page_image
            );
        }
    }

    data
}

fn should_use_page_for_baseline(page_data: &PageMarginData) -> bool {
    !page_data.is_blank && !page_data.is_full_page_image && page_data.content_bounds.is_some()
}

pub(crate) fn calculate_text_crop_bounds(
    detections: &[Detection],
    page_width: u32,
    page_height: u32,
) -> Option<ContentBounds> {
    let classifier = &crate::types::LABEL_CLASSIFIER;
    let text_detections: Vec<Detection> = detections
        .iter()
        .filter(|det| classifier.is_substantive_text(det))
        .cloned()
        .collect();
    calculate_content_bounds(&text_detections, page_width, page_height, true)
}

fn calculate_uniform_crop_bounds(
    all_page_data: &[PageMarginInput],
    config: &crate::pipeline::config::PipelineConfig,
    fallback_bounds: &ContentBounds,
) -> ContentBounds {
    let mut text_candidates: Vec<CropBoundsCandidate> = all_page_data
        .iter()
        .filter_map(|page| {
            calculate_text_crop_bounds(&page.detections, page.page_width, page.page_height).map(
                |bounds| CropBoundsCandidate {
                    page_width: page.page_width,
                    page_height: page.page_height,
                    bounds,
                },
            )
        })
        .collect();

    if text_candidates.is_empty() {
        return *fallback_bounds;
    }

    let median_page_width = median_u32(text_candidates.iter().map(|c| c.page_width).collect());
    let median_page_height = median_u32(text_candidates.iter().map(|c| c.page_height).collect());
    if median_page_width == 0 || median_page_height == 0 {
        return *fallback_bounds;
    }

    let dimension_stable: Vec<CropBoundsCandidate> = text_candidates
        .iter()
        .filter(|candidate| {
            within_ratio(candidate.page_width, median_page_width, 0.05)
                && within_ratio(candidate.page_height, median_page_height, 0.05)
        })
        .cloned()
        .collect();
    if !dimension_stable.is_empty() {
        text_candidates = dimension_stable;
    }

    let edge_x = ((median_page_width as f32) * 0.005).round().max(2.0) as u32;
    let edge_y = ((median_page_height as f32) * 0.005).round().max(2.0) as u32;
    let edge_stable: Vec<CropBoundsCandidate> = text_candidates
        .iter()
        .filter(|candidate| {
            candidate.bounds.min_x > edge_x
                && candidate.bounds.min_y > edge_y
                && candidate.page_width.saturating_sub(candidate.bounds.max_x) > edge_x
                && candidate.page_height.saturating_sub(candidate.bounds.max_y) > edge_y
        })
        .cloned()
        .collect();
    if !edge_stable.is_empty() {
        text_candidates = edge_stable;
    }

    let median_content_width =
        median_u32(text_candidates.iter().map(|c| c.bounds.width()).collect());
    let median_content_height =
        median_u32(text_candidates.iter().map(|c| c.bounds.height()).collect());

    let width_stable: Vec<CropBoundsCandidate> = text_candidates
        .iter()
        .filter(|candidate| {
            within_ratio(candidate.bounds.width(), median_content_width, 0.25)
                && within_ratio(candidate.bounds.height(), median_content_height, 0.35)
        })
        .cloned()
        .collect();
    if !width_stable.is_empty() {
        text_candidates = width_stable;
    }

    let vertical_candidates: Vec<CropBoundsCandidate> = text_candidates
        .iter()
        .filter(|candidate| {
            (candidate.bounds.height() as f32) >= (median_content_height as f32 * 0.75)
        })
        .cloned()
        .collect();
    let vertical_source = if vertical_candidates.is_empty() {
        &text_candidates
    } else {
        &vertical_candidates
    };

    let safety_pad = ((median_page_width.min(median_page_height) as f32) * 0.008)
        .round()
        .max(4.0) as u32;
    let safe_left = low_safe_margin(
        text_candidates.iter().map(|c| c.bounds.min_x).collect(),
        safety_pad,
    );
    let safe_right = low_safe_margin(
        text_candidates
            .iter()
            .map(|c| c.page_width.saturating_sub(c.bounds.max_x))
            .collect(),
        safety_pad,
    );
    let safe_top = low_safe_margin(
        vertical_source.iter().map(|c| c.bounds.min_y).collect(),
        safety_pad,
    );
    let safe_bottom = low_safe_margin(
        vertical_source
            .iter()
            .map(|c| c.page_height.saturating_sub(c.bounds.max_y))
            .collect(),
        safety_pad,
    );

    if config.crop_free_aspect() {
        let crop_width =
            max_stable_extent(text_candidates.iter().map(|c| c.bounds.width()).collect())
                .min(median_page_width)
                .max(1);
        let crop_height =
            max_stable_extent(vertical_source.iter().map(|c| c.bounds.height()).collect())
                .min(median_page_height)
                .max(1);

        return ContentBounds {
            min_x: 0,
            min_y: 0,
            max_x: crop_width,
            max_y: crop_height,
        };
    }

    let page_aspect = median_page_width as f32 / median_page_height.max(1) as f32;
    let target_width = config.target_width().unwrap_or_else(|| {
        ((config.target_height().max(1) as f32 * page_aspect)
            .round()
            .max(1.0)) as u32
    });
    let target_aspect = target_width as f32 / config.target_height().max(1) as f32;

    solve_horizontal_first_crop_bounds(
        median_page_width,
        median_page_height,
        safe_left,
        safe_right,
        safe_top,
        safe_bottom,
        target_aspect,
    )
}

fn low_safe_margin(mut margins: Vec<u32>, safety_pad: u32) -> u32 {
    percentile_u32(&mut margins, 0.20).saturating_sub(safety_pad)
}

fn max_stable_extent(extents: Vec<u32>) -> u32 {
    filter_outliers_iqr(extents).into_iter().max().unwrap_or(0)
}

pub(crate) fn fit_crop_window_to_content(
    content: &ContentBounds,
    crop_width: u32,
    crop_height: u32,
    page_width: u32,
    page_height: u32,
) -> ContentBounds {
    let crop_width = crop_width.max(1).min(page_width.max(1));
    let crop_height = crop_height.max(1).min(page_height.max(1));

    let center_x_twice = content.min_x as u64 + content.max_x as u64;
    let center_y_twice = content.min_y as u64 + content.max_y as u64;

    let min_x = place_axis_around_center(center_x_twice, crop_width, page_width);
    let min_y = place_axis_around_center(center_y_twice, crop_height, page_height);

    ContentBounds {
        min_x,
        min_y,
        max_x: min_x.saturating_add(crop_width).min(page_width),
        max_y: min_y.saturating_add(crop_height).min(page_height),
    }
}

fn place_axis_around_center(center_twice: u64, length: u32, limit: u32) -> u32 {
    if limit <= length {
        return 0;
    }
    let length_u64 = length as u64;
    let limit_u64 = limit as u64;
    let max_min = limit_u64.saturating_sub(length_u64);
    center_twice
        .saturating_sub(length_u64)
        .saturating_div(2)
        .min(max_min) as u32
}

fn solve_horizontal_first_crop_bounds(
    page_width: u32,
    page_height: u32,
    safe_left: u32,
    safe_right: u32,
    safe_top: u32,
    safe_bottom: u32,
    target_aspect: f32,
) -> ContentBounds {
    if page_width == 0 || page_height == 0 || !target_aspect.is_finite() || target_aspect <= 0.0 {
        return ContentBounds {
            min_x: 0,
            min_y: 0,
            max_x: page_width,
            max_y: page_height,
        };
    }

    let max_horizontal_crop = page_width.saturating_sub(1);
    let mut left = safe_left.min(max_horizontal_crop);
    let mut right = safe_right.min(max_horizontal_crop.saturating_sub(left));
    let top_capacity = safe_top.min(page_height.saturating_sub(1));
    let bottom_capacity =
        safe_bottom.min(page_height.saturating_sub(1).saturating_sub(top_capacity));

    let mut new_width = page_width.saturating_sub(left + right).max(1);
    let mut vertical_needed =
        page_height.saturating_sub((new_width as f32 / target_aspect).round().max(1.0) as u32);
    let vertical_capacity = top_capacity.saturating_add(bottom_capacity);

    if vertical_needed > vertical_capacity {
        let min_height = page_height.saturating_sub(vertical_capacity).max(1);
        let min_width_for_safe_vertical =
            ((min_height as f32 * target_aspect).round().max(1.0) as u32).min(page_width);
        let allowed_horizontal_crop = page_width.saturating_sub(min_width_for_safe_vertical);
        let requested_horizontal_crop = left.saturating_add(right);
        if requested_horizontal_crop > allowed_horizontal_crop {
            let (scaled_left, scaled_right) =
                split_crop_proportionally(allowed_horizontal_crop, left, right);
            left = scaled_left;
            right = scaled_right;
            new_width = page_width.saturating_sub(left + right).max(1);
            vertical_needed = page_height
                .saturating_sub((new_width as f32 / target_aspect).round().max(1.0) as u32);
        }
    }

    let vertical_needed = vertical_needed.min(vertical_capacity);
    let (top, bottom) = split_crop_with_caps(
        vertical_needed,
        top_capacity,
        bottom_capacity,
        safe_top,
        safe_bottom,
    );

    ContentBounds {
        min_x: left,
        min_y: top,
        max_x: page_width.saturating_sub(right).max(left.saturating_add(1)),
        max_y: page_height
            .saturating_sub(bottom)
            .max(top.saturating_add(1)),
    }
}

fn split_crop_proportionally(total: u32, left_weight: u32, right_weight: u32) -> (u32, u32) {
    let weight_sum = left_weight.saturating_add(right_weight);
    if total == 0 || weight_sum == 0 {
        return (0, 0);
    }
    let left = ((total as u64 * left_weight as u64) / weight_sum as u64) as u32;
    (left, total.saturating_sub(left))
}

fn split_crop_with_caps(
    total: u32,
    top_cap: u32,
    bottom_cap: u32,
    top_weight: u32,
    bottom_weight: u32,
) -> (u32, u32) {
    let (mut top, mut bottom) = split_crop_proportionally(total, top_weight, bottom_weight);
    if top > top_cap {
        let overflow = top - top_cap;
        top = top_cap;
        bottom = bottom.saturating_add(overflow).min(bottom_cap);
    }
    if bottom > bottom_cap {
        let overflow = bottom - bottom_cap;
        bottom = bottom_cap;
        top = top.saturating_add(overflow).min(top_cap);
    }
    (top, bottom)
}

fn within_ratio(value: u32, reference: u32, tolerance: f32) -> bool {
    if reference == 0 {
        return value == 0;
    }
    let ratio = value as f32 / reference as f32;
    ratio >= 1.0 - tolerance && ratio <= 1.0 + tolerance
}

/// Calculates a robust baseline using median and IQR to reject outliers.
fn calculate_robust_baseline_bounds(candidates: &[PageMarginData]) -> ContentBounds {
    if candidates.is_empty() {
        return ContentBounds::default(); // No data, no bounds.
    }

    let mut left_margins = Vec::new();
    let mut right_margins = Vec::new();
    let mut top_margins = Vec::new();
    let mut bottom_margins = Vec::new();
    let mut widths = Vec::new();
    let mut heights = Vec::new();

    for page in candidates {
        if let Some(bounds) = page.content_bounds {
            left_margins.push(bounds.min_x);
            right_margins.push(page.page_width.saturating_sub(bounds.max_x));
            top_margins.push(bounds.min_y);
            bottom_margins.push(page.page_height.saturating_sub(bounds.max_y));
            widths.push(page.page_width);
            heights.push(page.page_height);
        }
    }

    // Filter outliers from each margin set using IQR.
    let stable_left = filter_outliers_iqr(left_margins);
    let stable_right = filter_outliers_iqr(right_margins);
    let stable_top = filter_outliers_iqr(top_margins);
    let stable_bottom = filter_outliers_iqr(bottom_margins);

    // Calculate the average of the stable (non-outlier) margins.
    let avg_left = average_u32(&stable_left);
    let avg_right = average_u32(&stable_right);
    let avg_top = average_u32(&stable_top);
    let avg_bottom = average_u32(&stable_bottom);

    // Use the median page dimension for consistency.
    let median_width = median_u32(widths);
    let median_height = median_u32(heights);

    ContentBounds {
        min_x: avg_left,
        min_y: avg_top,
        max_x: median_width.saturating_sub(avg_right),
        max_y: median_height.saturating_sub(avg_bottom),
    }
}

/// Detects footnotes across all pages in the document
fn detect_footnotes_in_document(all_page_data: &[PageMarginInput]) -> bool {
    let classifier = &crate::types::LABEL_CLASSIFIER;
    for page in all_page_data {
        for detection in &page.detections {
            if classifier.is_footnote_label(detection) {
                return true; // Found at least one footnote
            }
        }
    }
    false
}

/// Calculates standard aspect ratio for consistent cropping
fn calculate_standard_aspect_ratio(
    baseline_candidates: &[PageMarginData],
    baseline_bounds: &ContentBounds,
) -> f32 {
    if baseline_candidates.is_empty() {
        return 0.75; // Default aspect ratio (3:4)
    }

    // Calculate aspect ratio based on baseline content area
    let content_width = baseline_bounds.max_x - baseline_bounds.min_x;
    let content_height = baseline_bounds.max_y - baseline_bounds.min_y;

    if content_height == 0 {
        return 0.75;
    }

    content_width as f32 / content_height as f32
}

/// Applies the document-wide analysis to process a single page's image.
///
/// This function selects the correct content bounds for the page (either its own or the
/// document baseline) and then calls the appropriate image transformation function.
pub fn apply_margin_analysis_to_page(
    original_image: &RgbImage,
    page_index: usize,
    analysis: &DocumentMarginAnalysis,
    standard_dims: &StandardPageDimensions,
    target_width_for_resize: Option<u32>,
    target_height_for_resize: u32,
) -> Result<RgbImage> {
    if analysis.effective_margin_setting == MarginSettings::None {
        return Ok(original_image.clone());
    }

    let page_data = analysis
        .pages
        .get(&page_index)
        .ok_or_else(|| anyhow!("Margin data not found for page {}", page_index))?;

    let full_page_bounds = ContentBounds {
        min_x: 0,
        min_y: 0,
        max_x: page_data.page_width,
        max_y: page_data.page_height,
    };

    // Determine the effective bounds to use for processing this page.
    let (effective_bounds, effective_setting) =
        if analysis.effective_margin_setting == MarginSettings::CropAndResize {
            if page_data.is_blank || page_data.is_full_page_image {
                (full_page_bounds, MarginSettings::StandardizeAndCenter)
            } else {
                (analysis.crop_bounds, MarginSettings::CropAndResize)
            }
        } else if page_data.is_blank || page_data.is_full_page_image {
            // For blank/full-image pages, use the full page so they are not cropped.
            (full_page_bounds, analysis.effective_margin_setting)
        } else if let Some(bounds) = page_data.content_bounds {
            (bounds, analysis.effective_margin_setting)
        } else {
            // No content detected, use baseline.
            (analysis.baseline_bounds, analysis.effective_margin_setting)
        };

    let crop_standard_dims = StandardPageDimensions {
        width: analysis.crop_bounds.width().max(1),
        height: analysis.crop_bounds.height().max(1),
    };
    let effective_standard_dims =
        if analysis.effective_margin_setting == MarginSettings::CropAndResize {
            &crop_standard_dims
        } else {
            standard_dims
        };

    // Dispatch to the core image processing functions.
    process_page_margins(
        original_image,
        &effective_bounds,
        effective_setting,
        effective_standard_dims,
        target_width_for_resize,
        target_height_for_resize,
    )
}

/// Crops and resizes while maintaining standard aspect ratio for consistency
fn crop_and_resize_with_standard_aspect_ratio(
    original_image: &RgbImage,
    bounds: &ContentBounds,
    standard_aspect_ratio: f32,
    target_width: Option<u32>,
    target_height: u32,
) -> Result<RgbImage> {
    #[cfg(feature = "debug-logging")]
    crate::debug_println!(
        "RESIZE CROP_AND_RESIZE: Original image: {}x{}, Content bounds: ({},{}) to ({},{}), Standard aspect ratio: {:.3}",
        original_image.width(),
        original_image.height(),
        bounds.min_x,
        bounds.min_y,
        bounds.max_x,
        bounds.max_y,
        standard_aspect_ratio
    );

    let adjusted_bounds = adjust_bounds_to_standard_aspect_ratio(
        bounds,
        standard_aspect_ratio,
        Some((original_image.width(), original_image.height())),
    );
    let final_bounds = adjusted_bounds;

    #[cfg(feature = "debug-logging")]
    crate::debug_println!(
        "RESIZE CROP_AND_RESIZE: Adjusted bounds: ({},{}) to ({},{}), Size: {}x{}",
        final_bounds.min_x,
        final_bounds.min_y,
        final_bounds.max_x,
        final_bounds.max_y,
        final_bounds.width(),
        final_bounds.height()
    );

    // Crop the content area from the original image using the final bounds
    let content_crop = image::imageops::crop_imm(
        original_image,
        final_bounds.min_x,
        final_bounds.min_y,
        final_bounds.width(),
        final_bounds.height(),
    );

    // Compute exact target dimensions from standard aspect ratio and height
    let target_width = resolve_target_width(target_width, target_height, standard_aspect_ratio);
    if target_width == 0 || target_height == 0 {
        return Err(anyhow!("Invalid target dimensions for resizing"));
    }

    #[cfg(feature = "debug-logging")]
    crate::debug_println!(
        "RESIZE CROP_AND_RESIZE: Target dimensions: {}x{}, Crop dimensions: {}x{}",
        target_width,
        target_height,
        final_bounds.width(),
        final_bounds.height()
    );

    // Use hardware-accelerated resize (HLSL on Windows, CPU fallback)
    let (crop_width, crop_height) = (final_bounds.width(), final_bounds.height());
    let src_data = content_crop.to_image().into_raw();

    let params = crate::resize::ResizeParams {
        target_width,
        target_height,
        method: crate::resize::ResizeMethod::Lanczos3,
        letterbox: false,
        border_value: 255.0,
        swap_rb: false,
    };

    let dst_data = crate::resize::resize_bytes(&src_data, crop_width, crop_height, &params, 3)
        .map_err(|e| anyhow!("Margin crop resize failed: {}", e))?;

    #[cfg(feature = "debug-logging")]
    crate::debug_println!(
        "RESIZE CROP_AND_RESIZE: Resize completed from {}x{} to {}x{}",
        crop_width,
        crop_height,
        target_width,
        target_height
    );

    // Create the final RgbImage from the resized data
    RgbImage::from_raw(target_width, target_height, dst_data)
        .ok_or_else(|| anyhow!("Failed to create final image from resized buffer"))
}

/// Adjusts content bounds to match the given standard aspect ratio, optionally clamping to image size
fn adjust_bounds_to_standard_aspect_ratio(
    bounds: &ContentBounds,
    standard_aspect_ratio: f32,
    image_dims: Option<(u32, u32)>,
) -> ContentBounds {
    // Calculate current crop dimensions
    let crop_width = bounds.width();
    let crop_height = bounds.height();
    if crop_width == 0 || crop_height == 0 {
        return *bounds;
    }
    let current_aspect_ratio = crop_width as f32 / crop_height as f32;

    // Adjust bounds to match standard aspect ratio while preserving content. Crop mode
    // may expand one axis to avoid slicing text, but it should shift that expansion
    // inside the source image instead of relying on a final clamp that makes the crop
    // visibly lopsided on facing-page scans.
    if let Some((img_w, img_h)) = image_dims {
        return if (current_aspect_ratio - standard_aspect_ratio).abs() < 0.01 {
            clamp_bounds_to_image(bounds, img_w, img_h)
        } else if current_aspect_ratio > standard_aspect_ratio {
            // Too wide -> increase height.
            let needed_height = (crop_width as f32 / standard_aspect_ratio).round() as u32;
            let (min_y, max_y) =
                expand_axis_to_length(bounds.min_y, bounds.max_y, needed_height, img_h);
            let (min_x, max_x) = clamp_axis_to_limit(bounds.min_x, bounds.max_x, img_w);
            ContentBounds {
                min_x,
                max_x,
                min_y,
                max_y,
            }
        } else {
            // Too tall -> increase width.
            let needed_width = (crop_height as f32 * standard_aspect_ratio).round() as u32;
            let (min_x, max_x) =
                expand_axis_to_length(bounds.min_x, bounds.max_x, needed_width, img_w);
            let (min_y, max_y) = clamp_axis_to_limit(bounds.min_y, bounds.max_y, img_h);
            ContentBounds {
                min_x,
                max_x,
                min_y,
                max_y,
            }
        };
    }

    if (current_aspect_ratio - standard_aspect_ratio).abs() < 0.01 {
        *bounds
    } else if current_aspect_ratio > standard_aspect_ratio {
        // Too wide -> increase height
        let needed_height = (crop_width as f32 / standard_aspect_ratio).round() as u32;
        let height_increase = needed_height.saturating_sub(crop_height);
        let height_expand_per_side = height_increase / 2;
        ContentBounds {
            min_x: bounds.min_x,
            max_x: bounds.max_x,
            min_y: bounds.min_y.saturating_sub(height_expand_per_side),
            max_y: bounds.max_y.saturating_add(height_expand_per_side),
        }
    } else {
        // Too tall -> increase width
        let needed_width = (crop_height as f32 * standard_aspect_ratio).round() as u32;
        let width_increase = needed_width.saturating_sub(crop_width);
        let width_expand_per_side = width_increase / 2;
        ContentBounds {
            min_x: bounds.min_x.saturating_sub(width_expand_per_side),
            max_x: bounds.max_x.saturating_add(width_expand_per_side),
            min_y: bounds.min_y,
            max_y: bounds.max_y,
        }
    }
}

fn clamp_bounds_to_image(bounds: &ContentBounds, img_w: u32, img_h: u32) -> ContentBounds {
    let (min_x, max_x) = clamp_axis_to_limit(bounds.min_x, bounds.max_x, img_w);
    let (min_y, max_y) = clamp_axis_to_limit(bounds.min_y, bounds.max_y, img_h);
    ContentBounds {
        min_x,
        max_x,
        min_y,
        max_y,
    }
}

fn clamp_axis_to_limit(min: u32, max: u32, limit: u32) -> (u32, u32) {
    if limit == 0 {
        return (0, 0);
    }
    let clamped_min = min.min(limit.saturating_sub(1));
    let clamped_max = max.min(limit).max(clamped_min.saturating_add(1).min(limit));
    (clamped_min, clamped_max)
}

fn expand_axis_to_length(min: u32, max: u32, desired_len: u32, limit: u32) -> (u32, u32) {
    if limit == 0 {
        return (0, 0);
    }

    let current_len = max.saturating_sub(min).max(1);
    let desired_len = desired_len.max(current_len).min(limit);
    let center_twice = min as u64 + max as u64;
    let desired_len_u64 = desired_len as u64;
    let limit_u64 = limit as u64;

    let mut new_min = center_twice.saturating_sub(desired_len_u64) / 2;
    let mut new_max = new_min.saturating_add(desired_len_u64);
    if new_max > limit_u64 {
        new_max = limit_u64;
        new_min = new_max.saturating_sub(desired_len_u64);
    }

    (new_min as u32, new_max as u32)
}

// --- Statistical Helper Functions ---

/// Calculates the average of a slice of u32 values
fn average_u32(data: &[u32]) -> u32 {
    if data.is_empty() {
        return 0;
    }
    (data.iter().map(|&x| x as u64).sum::<u64>() / data.len() as u64) as u32
}

/// Calculates the median of a vector of u32 values
fn median_u32(mut data: Vec<u32>) -> u32 {
    if data.is_empty() {
        return 0;
    }
    data.sort_unstable();
    let mid = data.len() / 2;
    if data.len() % 2 == 0 {
        (data[mid - 1] + data[mid]) / 2
    } else {
        data[mid]
    }
}

fn percentile_u32(data: &mut [u32], percentile: f32) -> u32 {
    if data.is_empty() {
        return 0;
    }
    data.sort_unstable();
    let p = percentile.clamp(0.0, 1.0);
    let idx = ((data.len().saturating_sub(1) as f32) * p).round() as usize;
    data[idx.min(data.len() - 1)]
}

/// A robust statistical outlier filter using the Interquartile Range (IQR) method.
/// This effectively ignores anomalous values, such as pages with heavy handwriting in margins.
fn filter_outliers_iqr(mut data: Vec<u32>) -> Vec<u32> {
    if data.len() < 4 {
        // Need enough data for quartiles.
        return data;
    }
    data.sort_unstable();
    let q1 = data[data.len() / 4];
    let q3 = data[data.len() * 3 / 4];
    let iqr = q3.saturating_sub(q1) as f32;

    // The standard definition of an outlier is anything outside 1.5 * IQR from the quartiles.
    let lower_bound = q1 as f32 - 1.5 * iqr;
    let upper_bound = q3 as f32 + 1.5 * iqr;

    data.into_iter()
        .filter(|&x| x as f32 >= lower_bound && x as f32 <= upper_bound)
        .collect()
}

#[inline]
fn resolve_target_width(target_width: Option<u32>, target_height: u32, aspect_ratio: f32) -> u32 {
    match target_width {
        Some(width) if width > 0 => width,
        _ => ((target_height as f32 * aspect_ratio).round().max(1.0)) as u32,
    }
}

#[inline]
fn target_aspect_ratio(
    standard_dims: &StandardPageDimensions,
    target_width: Option<u32>,
    target_height: u32,
) -> f32 {
    if let Some(width) = target_width.filter(|&w| w > 0) {
        width as f32 / target_height.max(1) as f32
    } else {
        standard_dims.width.max(1) as f32 / standard_dims.height.max(1) as f32
    }
}

// --- Private Implementation Functions ---

/// Creates a new canvas with target height and proportional width, centering the page's content.
fn standardize_and_center_page(
    original_image: &RgbImage,
    bounds: &ContentBounds,
    target_width: Option<u32>,
    target_height: u32,
    standard_aspect_ratio: f32,
) -> Result<RgbImage> {
    // Target canvas dimensions from cover-derived standard aspect ratio
    let target_width = resolve_target_width(target_width, target_height, standard_aspect_ratio);
    let target_height = target_height;

    #[cfg(feature = "debug-logging")]
    crate::debug_println!(
        "MARGIN STANDARDIZE_AND_CENTER: Original image {}x{}, Content bounds ({},{}) to ({},{}), Target canvas {}x{}, Aspect ratio {:.3}",
        original_image.width(),
        original_image.height(),
        bounds.min_x,
        bounds.min_y,
        bounds.max_x,
        bounds.max_y,
        target_width,
        target_height,
        standard_aspect_ratio
    );

    // Create a new blank (white) image with target dimensions
    let mut new_image = RgbImage::from_pixel(target_width, target_height, Rgb([255, 255, 255]));

    // Crop the content area from the original image.
    let content_crop = image::imageops::crop_imm(
        original_image,
        bounds.min_x,
        bounds.min_y,
        bounds.width(),
        bounds.height(),
    )
    .to_image();

    // Compute scale to fit content within target canvas while preserving aspect ratio
    let (cw, ch) = (bounds.width(), bounds.height());
    let scale_x = target_width as f32 / cw as f32;
    let scale_y = target_height as f32 / ch as f32;
    let scale = scale_x.min(scale_y);
    let resized_w = (cw as f32 * scale).round() as u32;
    let resized_h = (ch as f32 * scale).round() as u32;

    #[cfg(feature = "debug-logging")]
    crate::debug_println!(
        "MARGIN SCALING: Content crop {}x{}, Scale factors X={:.3} Y={:.3}, Using scale={:.3}, Resized to {}x{}",
        cw,
        ch,
        scale_x,
        scale_y,
        scale,
        resized_w,
        resized_h
    );

    // Resize the content crop using hardware acceleration (HLSL on Windows, CPU fallback)
    let src_data = content_crop.into_raw();

    let params = crate::resize::ResizeParams {
        target_width: resized_w,
        target_height: resized_h,
        method: crate::resize::ResizeMethod::Lanczos3,
        letterbox: false,
        border_value: 255.0,
        swap_rb: false,
    };

    let dst_data = crate::resize::resize_bytes(&src_data, cw, ch, &params, 3)
        .map_err(|e| anyhow!("Margin center resize failed: {}", e))?;

    let resized = RgbImage::from_raw(resized_w, resized_h, dst_data)
        .ok_or_else(|| anyhow!("Failed to create resized image for centering"))?;

    // Center the resized content onto the new canvas
    let target_x = ((target_width as i64 - resized_w as i64) / 2).max(0);
    let target_y = ((target_height as i64 - resized_h as i64) / 2).max(0);

    #[cfg(feature = "debug-logging")]
    crate::debug_println!(
        "MARGIN CENTERING: Placing resized content at position ({},{}) on {}x{} canvas",
        target_x,
        target_y,
        target_width,
        target_height
    );

    image::imageops::overlay(&mut new_image, &resized, target_x, target_y);

    Ok(new_image)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detection(class_name: &str, bbox: [f32; 4]) -> Detection {
        let class_id = crate::types::class_id_for(class_name).unwrap();
        Detection {
            class_id,
            class_name: Some(class_name.to_string()),
            confidence: 0.9,
            bbox,
            category: crate::types::category_for_class(class_id),
            context: None,
        }
    }

    #[test]
    fn table_footnote_switches_crop_to_center_when_not_forced() {
        let pages = vec![PageMarginInput {
            page_index: 0,
            page_width: 1000,
            page_height: 1400,
            detections: vec![detection("table_footnote", [100.0, 1200.0, 900.0, 1300.0])],
            pixel_bounds: None,
        }];

        let analysis = analyze_document_margins(
            &pages,
            &crate::pipeline::config::PipelineConfig::default(),
            MarginSettings::CropAndResize,
            false,
        );

        assert_eq!(
            analysis.effective_margin_setting,
            MarginSettings::StandardizeAndCenter
        );
        assert!(analysis.setting_override_reason.is_some());
    }

    #[test]
    fn table_footnote_keeps_crop_when_forced() {
        let pages = vec![PageMarginInput {
            page_index: 0,
            page_width: 1000,
            page_height: 1400,
            detections: vec![detection("table_footnote", [100.0, 1200.0, 900.0, 1300.0])],
            pixel_bounds: None,
        }];

        let analysis = analyze_document_margins(
            &pages,
            &crate::pipeline::config::PipelineConfig::default(),
            MarginSettings::CropAndResize,
            true,
        );

        assert_eq!(
            analysis.effective_margin_setting,
            MarginSettings::CropAndResize
        );
        assert!(analysis.setting_override_reason.is_none());
    }

    #[test]
    fn early_pages_can_establish_nonzero_crop_baseline() {
        let pages = vec![
            PageMarginInput {
                page_index: 0,
                page_width: 640,
                page_height: 900,
                detections: vec![detection("plain_text", [90.0, 110.0, 520.0, 760.0])],
                pixel_bounds: None,
            },
            PageMarginInput {
                page_index: 1,
                page_width: 640,
                page_height: 900,
                detections: vec![detection("plain_text", [95.0, 115.0, 515.0, 755.0])],
                pixel_bounds: None,
            },
        ];

        let analysis = analyze_document_margins(
            &pages,
            &crate::pipeline::config::PipelineConfig::default(),
            MarginSettings::CropAndResize,
            true,
        );

        assert!(analysis.baseline_bounds.width() > 0);
        assert!(analysis.baseline_bounds.height() > 0);
        assert_eq!(analysis.analysis_width, 640);
        assert_eq!(analysis.analysis_height, 900);
    }

    #[test]
    fn crop_bounds_are_uniform_for_alternating_facing_page_offsets() {
        let pages = vec![
            PageMarginInput {
                page_index: 0,
                page_width: 640,
                page_height: 900,
                detections: vec![detection("plain_text", [80.0, 100.0, 500.0, 760.0])],
                pixel_bounds: None,
            },
            PageMarginInput {
                page_index: 1,
                page_width: 640,
                page_height: 900,
                detections: vec![detection("plain_text", [140.0, 100.0, 560.0, 760.0])],
                pixel_bounds: None,
            },
            PageMarginInput {
                page_index: 2,
                page_width: 640,
                page_height: 900,
                detections: vec![detection("figure", [0.0, 0.0, 640.0, 900.0])],
                pixel_bounds: None,
            },
        ];

        let analysis = analyze_document_margins(
            &pages,
            &crate::pipeline::config::PipelineConfig::default(),
            MarginSettings::CropAndResize,
            true,
        );

        assert!(analysis.crop_bounds.min_x > 0);
        assert!(analysis.crop_bounds.max_x < 640);
        assert!(analysis.crop_bounds.min_y > 0);
        assert!(analysis.crop_bounds.max_y < 900);
        assert!(analysis.crop_bounds.width() < 640);
        assert_eq!(analysis.crop_bounds.width(), 500);
        assert_eq!(analysis.crop_bounds.height(), 703);
    }

    #[test]
    fn free_aspect_crop_uses_uniform_stable_text_box_size() {
        let pages = vec![
            PageMarginInput {
                page_index: 0,
                page_width: 640,
                page_height: 900,
                detections: vec![detection("plain_text", [80.0, 100.0, 500.0, 760.0])],
                pixel_bounds: None,
            },
            PageMarginInput {
                page_index: 1,
                page_width: 640,
                page_height: 900,
                detections: vec![detection("plain_text", [140.0, 100.0, 560.0, 760.0])],
                pixel_bounds: None,
            },
            PageMarginInput {
                page_index: 2,
                page_width: 640,
                page_height: 900,
                detections: vec![detection("title", [200.0, 120.0, 440.0, 180.0])],
                pixel_bounds: None,
            },
        ];
        let mut config = crate::pipeline::config::PipelineConfig::default();
        config.set_enable_layout_detection(true);
        config.set_crop_free_aspect(true);

        let analysis =
            analyze_document_margins(&pages, &config, MarginSettings::CropAndResize, true);

        assert_eq!(analysis.crop_bounds.min_x, 0);
        assert_eq!(analysis.crop_bounds.max_x, 430);
        assert_eq!(analysis.crop_bounds.min_y, 0);
        assert_eq!(analysis.crop_bounds.max_y, 670);
    }

    #[test]
    fn free_aspect_crop_window_follows_alternating_text_position() {
        let crop_width = 430;
        let crop_height = 670;
        let left_page = ContentBounds {
            min_x: 75,
            min_y: 95,
            max_x: 505,
            max_y: 765,
        };
        let right_page = ContentBounds {
            min_x: 135,
            min_y: 95,
            max_x: 565,
            max_y: 765,
        };

        let left_window = fit_crop_window_to_content(&left_page, crop_width, crop_height, 640, 900);
        let right_window =
            fit_crop_window_to_content(&right_page, crop_width, crop_height, 640, 900);

        assert_eq!(left_window.width(), right_window.width());
        assert_eq!(left_window.height(), right_window.height());
        assert_eq!(left_window.min_x, 75);
        assert_eq!(right_window.min_x, 135);
        assert!(left_window.min_x <= left_page.min_x && left_window.max_x >= left_page.max_x);
        assert!(right_window.min_x <= right_page.min_x && right_window.max_x >= right_page.max_x);
    }

    #[test]
    fn crop_bounds_fall_back_when_layout_has_no_text_pages() {
        let pages = vec![PageMarginInput {
            page_index: 0,
            page_width: 640,
            page_height: 900,
            detections: vec![detection("figure", [0.0, 0.0, 640.0, 900.0])],
            pixel_bounds: None,
        }];

        let analysis = analyze_document_margins(
            &pages,
            &crate::pipeline::config::PipelineConfig::default(),
            MarginSettings::CropAndResize,
            true,
        );

        assert_eq!(analysis.crop_bounds.min_x, analysis.baseline_bounds.min_x);
        assert_eq!(analysis.crop_bounds.min_y, analysis.baseline_bounds.min_y);
        assert_eq!(analysis.crop_bounds.max_x, analysis.baseline_bounds.max_x);
        assert_eq!(analysis.crop_bounds.max_y, analysis.baseline_bounds.max_y);
    }

    #[test]
    fn crop_aspect_expansion_shifts_inside_image_bounds() {
        let bounds = ContentBounds {
            min_x: 760,
            min_y: 100,
            max_x: 960,
            max_y: 700,
        };

        let adjusted = adjust_bounds_to_standard_aspect_ratio(&bounds, 0.75, Some((1000, 1000)));

        assert_eq!(adjusted.width(), 450);
        assert_eq!(adjusted.height(), 600);
        assert_eq!(adjusted.max_x, 1000);
        assert!(adjusted.min_x <= bounds.min_x);
        assert!(adjusted.max_x >= bounds.max_x);
    }
}
