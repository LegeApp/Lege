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
use crate::resize_context::MarginCorrection;
use anyhow::{Result, anyhow};
use crate::image_types::{Rgb, RgbImage};
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

/// Analysis results for the entire document's margins
#[derive(Debug, Clone)]
pub struct DocumentMarginAnalysis {
    /// Per-page raw analysis data.
    pub pages: HashMap<usize, PageMarginData>,
    /// The statistically robust baseline for content, calculated from stable pages.
    pub baseline_bounds: ContentBounds,
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
    image: &crate::image_types::RgbImage,
    config: &crate::pipeline::config::PipelineConfig,
) -> Option<ContentBounds> {
    use Legencode::types::BinarizationOptions;

    let want_invert_input = config.invert_input();
    let mut want_invert_output = config.binarization().invert;
    if want_invert_input && want_invert_output {
        // Avoid double inversion so black text stays black.
        want_invert_output = false;
    }

    let options = BinarizationOptions {
        invert: want_invert_output,
        invert_input: want_invert_input,
        k_factor: config.binarization().k_factor,
        use_heavy_duty: config.binarization().use_heavy_duty
            && !config.binarization().use_fixed_threshold,
        patch_percentage: config.binarization().patch_percentage,
        no_patch: config.binarization().no_patch,
        use_fixed_threshold: config.binarization().use_fixed_threshold,
        fixed_threshold: config.binarization().fixed_threshold,
    };

    let mut binarized = Legencode::color::binarization::binarize_image_raw(
        image.as_raw(),
        image.width() as usize,
        image.height() as usize,
        &options,
    );

    if binarized.is_empty() {
        return None;
    }

    // Normalize to 0/1 if the output is still 0/255 format
    if binarized.iter().any(|&b| b > 1) {
        for value in binarized.iter_mut() {
            *value = if *value > 128 { 1 } else { 0 };
        }
    }

    // Invert polarity: 0=white (background), 1=black (content) for mask analysis
    for value in binarized.iter_mut() {
        *value = if *value == 0 { 1 } else { 0 };
    }

    calculate_content_bounds_from_binary_mask(
        &binarized,
        image.width(),
        image.height(),
    )
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

    let row_bounds = find_bounds(&row_counts, row_threshold)
        .or_else(|| find_bounds(&row_counts, 1))?;
    let col_bounds = find_bounds(&col_counts, col_threshold)
        .or_else(|| find_bounds(&col_counts, 1))?;

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
    // Derive the standard aspect ratio from the first processed page dimensions
    // These should always be set by the renderer when the first page is seen.
    let standard_aspect_ratio: f32 = standard_dims.width as f32 / standard_dims.height as f32;
    
    #[cfg(feature = "debug-logging")]
    crate::debug_println!("MARGIN PROCESSING: Input image {}x{}, Settings {:?}, Standard dims {}x{} (aspect {:.3}), Target resize {}x{}",
        original_image.width(), original_image.height(),
        settings,
        standard_dims.width, standard_dims.height, standard_aspect_ratio,
        target_width_for_resize.unwrap_or(0), target_height_for_resize);
    
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
            crate::debug_println!("MARGIN PROCESSING: No margin processing, returning original image");
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
    let standard_aspect_ratio: f32 = standard_dims.width as f32 / standard_dims.height as f32;
    let target_width = resolve_target_width(target_width_for_resize, target_height_for_resize, standard_aspect_ratio);

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
            MarginCorrection::new(
                (tx * sx) as f32,
                (ty * sy) as f32,
                sx as f32,
                sy as f32,
            )
        }
        MarginSettings::None => {
            MarginCorrection::default()
        }
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
            let standard_aspect_ratio: f32 =
                standard_dims.width as f32 / standard_dims.height as f32;
            let target_h = target_height_for_resize;
            let target_w = resolve_target_width(
                target_width_for_resize,
                target_h,
                standard_aspect_ratio,
            );

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
            let standard_aspect_ratio: f32 =
                standard_dims.width as f32 / standard_dims.height as f32;

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
    original_margin_setting: MarginSettings,
    crop_footnotes: bool,
) -> DocumentMarginAnalysis {
    let mut pages = HashMap::new();
    let mut baseline_candidates = Vec::new();

    // First pass: analyze each page individually
    for page_input in all_page_data {
        let page_margin_data = analyze_single_page_margins(page_input);

        // Collect pages suitable for baseline calculation (after page 5, not blank, not full-page)
        if page_input.page_index >= 5 && should_use_page_for_baseline(&page_margin_data) {
            baseline_candidates.push(page_margin_data.clone());
        }

        pages.insert(page_input.page_index, page_margin_data);
    }

    // Calculate baseline margins from running average
    let baseline_bounds = calculate_robust_baseline_bounds(&baseline_candidates);

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
        let mut widths: Vec<u32> = all_page_data.iter().map(|p| p.page_width).collect();
        let mut heights: Vec<u32> = all_page_data.iter().map(|p| p.page_height).collect();
        widths.sort();
        heights.sort();
        (
            widths[widths.len() / 2],
            heights[heights.len() / 2]
        )
    } else {
        (640, 800) // fallback default
    };

    DocumentMarginAnalysis {
        pages,
        baseline_bounds,
        standard_aspect_ratio,
        effective_margin_setting,
        setting_override_reason,
        analysis_width,
        analysis_height,
    }
}

/// Analyzes margins for a single page
fn analyze_single_page_margins(page: &PageMarginInput) -> PageMarginData {
    let detection_bounds = if page.detections.is_empty() {
        None
    } else {
        calculate_content_bounds(
            &page.detections,
            page.page_width,
            page.page_height,
            false,
        )
    };

    let content_bounds = detection_bounds.or(page.pixel_bounds);
    let is_blank = page.detections.is_empty() && page.pixel_bounds.is_none();

    let is_full_page_image = is_full_page_image(
        &page.detections,
        page.page_width,
        page.page_height,
        content_bounds,
    );

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

/// Detects if a page contains a full-page image that should use standard margins
fn is_full_page_image(
    detections: &[Detection],
    page_width: u32,
    page_height: u32,
    content_bounds: Option<ContentBounds>,
) -> bool {
    if detections.len() == 1 {
        let det = &detections[0];
        let region_width = det.bbox[2] - det.bbox[0];
        let region_height = det.bbox[3] - det.bbox[1];

        // Consider it a full-page image if it covers more than 80% of the page
        let coverage = (region_width * region_height) as f32 / (page_width * page_height) as f32;
        coverage > 0.8
    } else if detections.is_empty() {
        if let Some(bounds) = content_bounds {
            let coverage = (bounds.width() * bounds.height()) as f32
                / (page_width * page_height) as f32;
            coverage > 0.8
        } else {
            false
        }
    } else {
        false
    }
}

/// Determines if a page should be used for baseline margin calculation
fn should_use_page_for_baseline(page_data: &PageMarginData) -> bool {
    !page_data.is_blank && !page_data.is_full_page_image && page_data.content_bounds.is_some()
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
    const FOOTNOTE_CLASS_ID: i32 = 12; // Based on engine.rs footnote detection

    for page in all_page_data {
        for detection in &page.detections {
            if detection.class_id == FOOTNOTE_CLASS_ID {
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

    // Determine the effective bounds to use for processing this page.
    let effective_bounds = if page_data.is_blank {
        // For blank pages, use the document's baseline to give them standard margins.
        analysis.baseline_bounds
    } else if page_data.is_full_page_image {
        // For full-page images, use the document's baseline to give them standard margins.
        analysis.baseline_bounds
    } else if let Some(bounds) = page_data.content_bounds {
        bounds
    } else {
        // No content detected, use baseline.
        analysis.baseline_bounds
    };

    // Dispatch to the core image processing functions.
    process_page_margins(
        original_image,
        &effective_bounds,
        analysis.effective_margin_setting,
        standard_dims,
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
    crate::debug_println!("RESIZE CROP_AND_RESIZE: Original image: {}x{}, Content bounds: ({},{}) to ({},{}), Standard aspect ratio: {:.3}", 
        original_image.width(), original_image.height(),
        bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y,
        standard_aspect_ratio);

    let adjusted_bounds = adjust_bounds_to_standard_aspect_ratio(
        bounds,
        standard_aspect_ratio,
        Some((original_image.width(), original_image.height())),
    );
    let final_bounds = adjusted_bounds;

    #[cfg(feature = "debug-logging")]
    crate::debug_println!("RESIZE CROP_AND_RESIZE: Adjusted bounds: ({},{}) to ({},{}), Size: {}x{}", 
        final_bounds.min_x, final_bounds.min_y, final_bounds.max_x, final_bounds.max_y,
        final_bounds.width(), final_bounds.height());

    // Crop the content area from the original image using the final bounds
    let content_crop = crate::image_types::imageops::crop_imm(
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
    crate::debug_println!("RESIZE CROP_AND_RESIZE: Target dimensions: {}x{}, Crop dimensions: {}x{}", 
        target_width, target_height,
        final_bounds.width(), final_bounds.height());

    // Use hardware-accelerated resize (HLSL on Windows, CPU fallback)
    let (crop_width, crop_height) = (final_bounds.width(), final_bounds.height());
    let src_data = content_crop.to_image().into_raw();

    let params = crate::resize::ResizeParams {
        target_width,
        target_height,
        method: crate::resize::ResizeMethod::Lanczos3,
        letterbox: false,
        border_value: 0.0,
        swap_rb: false,
    };

    let dst_data = crate::resize::resize_bytes(
        &src_data,
        crop_width,
        crop_height,
        &params,
        3,
    )
    .map_err(|e| anyhow!("Margin crop resize failed: {}", e))?;

    #[cfg(feature = "debug-logging")]
    crate::debug_println!("RESIZE CROP_AND_RESIZE: Resize completed from {}x{} to {}x{}",
        crop_width, crop_height, target_width, target_height);

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

    // Adjust bounds to match standard aspect ratio while preserving content (prefer expansion)
    let mut adjusted = if (current_aspect_ratio - standard_aspect_ratio).abs() < 0.01 {
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
    };

    if let Some((img_w, img_h)) = image_dims {
        adjusted = ContentBounds {
            min_x: adjusted.min_x.min(img_w.saturating_sub(1)),
            max_x: adjusted.max_x.min(img_w),
            min_y: adjusted.min_y.min(img_h.saturating_sub(1)),
            max_y: adjusted.max_y.min(img_h),
        };
    }

    adjusted
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
    crate::debug_println!("MARGIN STANDARDIZE_AND_CENTER: Original image {}x{}, Content bounds ({},{}) to ({},{}), Target canvas {}x{}, Aspect ratio {:.3}",
        original_image.width(), original_image.height(),
        bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y,
        target_width, target_height, standard_aspect_ratio);

    // Create a new blank (white) image with target dimensions
    let mut new_image = RgbImage::from_pixel(target_width, target_height, Rgb::new(255, 255, 255));

    // Crop the content area from the original image.
    let content_crop = crate::image_types::imageops::crop_imm(
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
    crate::debug_println!("MARGIN SCALING: Content crop {}x{}, Scale factors X={:.3} Y={:.3}, Using scale={:.3}, Resized to {}x{}",
        cw, ch, scale_x, scale_y, scale, resized_w, resized_h);

    // Resize the content crop using hardware acceleration (HLSL on Windows, CPU fallback)
    let src_data = content_crop.into_raw();

    let params = crate::resize::ResizeParams {
        target_width: resized_w,
        target_height: resized_h,
        method: crate::resize::ResizeMethod::Lanczos3,
        letterbox: false,
        border_value: 0.0,
        swap_rb: false,
    };

    let dst_data = crate::resize::resize_bytes(
        &src_data,
        cw,
        ch,
        &params,
        3,
    )
    .map_err(|e| anyhow!("Margin center resize failed: {}", e))?;

    let resized = RgbImage::from_raw(resized_w, resized_h, dst_data)
        .ok_or_else(|| anyhow!("Failed to create resized image for centering"))?;

    // Center the resized content onto the new canvas
    let target_x = ((target_width as i64 - resized_w as i64) / 2).max(0);
    let target_y = ((target_height as i64 - resized_h as i64) / 2).max(0);
    
    #[cfg(feature = "debug-logging")]
    crate::debug_println!("MARGIN CENTERING: Placing resized content at position ({},{}) on {}x{} canvas",
        target_x, target_y, target_width, target_height);
    
    crate::image_types::imageops::overlay(&mut new_image, &resized, target_x, target_y);

    Ok(new_image)
}

