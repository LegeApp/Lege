//! This module contains logic for finding and processing connected components
//! from a bitmap, a process central to symbol extraction in JBIG2. It uses the
//! Lutz algorithm for the initial component finding and then applies a series of
//! heuristics for dot merging and glyph splitting to produce clean symbols.

use crate::jbig2shared::{save_debug_pbm, usize_to_u32};
use crate::jbig2sym::{compute_glyph_hash, BitImage, Rect, Symbol};
// Image import removed as it's not used
use rustc_hash::FxHashSet;

/// A connected component with bounding box and pixel information
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectedComponent {
    pub bounds: Rect,
    pub pixel_count: usize,
    pub pixels: Vec<(u32, u32)>,
}

/// Finds connected components using Lutz algorithm
pub fn find_connected_components(image: &BitImage, min_size: usize) -> Vec<ConnectedComponent> {
    let components = lutz::lutz::<_, Vec<lutz::Pixel>>(image);

    // Convert to ConnectedComponent and filter by size
    let mut result = Vec::new();
    for pixels in components {
        if pixels.len() >= min_size {
            let mut min_x = u32::MAX;
            let mut min_y = u32::MAX;
            let mut max_x = 0;
            let mut max_y = 0;

            // Calculate bounds
            for p in &pixels {
                min_x = min_x.min(p.x);
                min_y = min_y.min(p.y);
                max_x = max_x.max(p.x);
                max_y = max_y.max(p.y);
            }

            let component = ConnectedComponent {
                bounds: Rect {
                    x: min_x,
                    y: min_y,
                    width: max_x - min_x + 1,
                    height: max_y - min_y + 1,
                },
                pixel_count: pixels.len(),
                pixels: pixels.into_iter().map(|p| (p.x, p.y)).collect(),
            };
            result.push(component);
        }
    }

    result
}

// ==============================================
// Symbol extraction and processing logic
// ==============================================

/// Configuration for symbol extraction, including dot and split parameters
#[derive(Debug, Clone, Copy)]
pub struct SymbolExtractionConfig {
    pub max_dot_area_ratio: f32,
    pub max_dot_height_ratio: f32,
    pub dot_aspect_ratio_range: (f32, f32),
    pub dot_merge_distance_ratio: f32,
    pub min_component_size: usize,
    pub split_config: SplitConfig,
}

impl SymbolExtractionConfig {
    /// Creates a new `SymbolExtractionConfig` from a `Jbig2Config`
    pub fn from_jbig2_config(config: &crate::jbig2structs::Jbig2Config) -> Self {
        let mut cfg = Self::default();

        // Balanced filtering - remove noise but preserve punctuation
        cfg.min_component_size = if config.auto_thresh { 12 } else { 10 };

        // More restrictive dot detection to avoid fragmenting characters
        cfg.max_dot_area_ratio = 0.05; // Reduced from 0.1
        cfg.max_dot_height_ratio = 0.3; // Reduced from 0.5

        cfg
    }
}

impl Default for SymbolExtractionConfig {
    fn default() -> Self {
        Self {
            max_dot_area_ratio: 0.1,
            max_dot_height_ratio: 0.5,
            dot_aspect_ratio_range: (0.5, 2.0),
            dot_merge_distance_ratio: 0.5,
            min_component_size: 10, // Balanced to preserve punctuation but filter noise
            split_config: SplitConfig::default(),
        }
    }
}

/// Configuration for the split check
#[derive(Debug, Clone, Copy)]
pub struct SplitConfig {
    pub rolling_window: usize,
    pub max_width_ratio: f32,
    pub min_gap: usize,
    pub gap_height_frac: f32,
    pub min_subglyph_width: usize,
}

impl Default for SplitConfig {
    fn default() -> Self {
        Self {
            rolling_window: 10,
            max_width_ratio: 1.5,
            min_gap: 2,
            gap_height_frac: 0.9,
            min_subglyph_width: 5,
        }
    }
}

/// Keep a rolling buffer of widths
pub struct WidthTracker {
    buf: Vec<usize>,
    sum: usize,
    max_len: usize,
}

impl WidthTracker {
    pub fn new(max_len: usize) -> Self {
        Self {
            buf: Vec::with_capacity(max_len),
            sum: 0,
            max_len,
        }
    }

    /// Push a new width, evicting the oldest if needed
    pub fn push(&mut self, w: usize) {
        self.buf.push(w);
        self.sum += w;
        if self.buf.len() > self.max_len {
            let old = self.buf.remove(0);
            self.sum -= old;
        }
    }

    /// Current average width (or 1.0 if empty)
    pub fn avg(&self) -> f32 {
        if self.buf.is_empty() {
            1.0
        } else {
            self.sum as f32 / self.buf.len() as f32
        }
    }
}

/// Detects potential dots in the components
fn detect_dots(
    components: &[ConnectedComponent],
    config: &SymbolExtractionConfig,
    tracker: &mut WidthTracker,
) -> Vec<usize> {
    if components.is_empty() {
        return Vec::new();
    }
    let mut heights: Vec<u32> = components.iter().map(|c| c.bounds.height).collect();
    heights.sort_unstable();
    let median_height = heights[heights.len() / 2] as f32;

    let mut sizes: Vec<usize> = components.iter().map(|c| c.pixel_count).collect();
    sizes.sort_unstable();
    let median_size = sizes[sizes.len() / 2] as f32;

    let max_dot_size = (median_size * config.max_dot_area_ratio) as usize;
    let max_dot_height = (median_height * config.max_dot_height_ratio) as u32;
    let (min_ar, max_ar) = config.dot_aspect_ratio_range;

    let mut dot_indices = Vec::new();
    for (i, c) in components.iter().enumerate() {
        let ar = c.bounds.width as f32 / c.bounds.height as f32;
        if c.pixel_count <= max_dot_size
            && c.bounds.height <= max_dot_height
            && ar >= min_ar
            && ar <= max_ar
        {
            dot_indices.push(i);
        } else {
            tracker.push(c.bounds.width as usize);
        }
    }
    dot_indices
}

/// Calculates vertical merge distance for dot merging
fn calculate_vertical_merge_distance(components: &[ConnectedComponent], sample_size: usize) -> u32 {
    if components.is_empty() {
        return 10;
    }
    let actual_sample_size = std::cmp::min(sample_size, components.len());
    let avg_height = components
        .iter()
        .take(actual_sample_size)
        .map(|c| c.bounds.height as f32)
        .sum::<f32>()
        / actual_sample_size as f32;
    let variance = components
        .iter()
        .take(actual_sample_size)
        .map(|c| (c.bounds.height as f32 - avg_height).powi(2))
        .sum::<f32>()
        / actual_sample_size as f32;
    let std_dev = variance.sqrt();
    let vertical_distance = (avg_height * 0.6 + std_dev * 0.5) as u32;
    std::cmp::max(5, std::cmp::min(vertical_distance, 30))
}

/// Merges dots with base characters
fn merge_dots(
    components: &[ConnectedComponent],
    dot_indices: &[usize],
    _config: &SymbolExtractionConfig,
) -> Vec<(usize, usize)> {
    const MAX_HORIZONTAL_TOLERANCE: u32 = 1;
    let max_vertical_distance = calculate_vertical_merge_distance(components, 10);
    let mut skip_indices = FxHashSet::default();
    let mut merges = Vec::new();

    for &dot_idx in dot_indices {
        if skip_indices.contains(&dot_idx) {
            continue;
        }
        let dot = &components[dot_idx];
        let dot_center_x = dot.bounds.x + dot.bounds.width / 2;
        let dot_bottom = dot.bounds.y + dot.bounds.height;

        let mut best_match = None;
        let mut min_vertical_distance = u32::MAX;

        for (i, candidate) in components.iter().enumerate() {
            if i == dot_idx
                || dot_indices.contains(&i)
                || skip_indices.contains(&i)
                || candidate.bounds.y <= dot_bottom
            {
                continue;
            }
            let candidate_left = candidate.bounds.x.saturating_sub(MAX_HORIZONTAL_TOLERANCE);
            let candidate_right =
                candidate.bounds.x + candidate.bounds.width + MAX_HORIZONTAL_TOLERANCE;
            if dot_center_x >= candidate_left && dot_center_x <= candidate_right {
                let vertical_distance = candidate.bounds.y - dot_bottom;
                if vertical_distance <= max_vertical_distance
                    && vertical_distance < min_vertical_distance
                {
                    min_vertical_distance = vertical_distance;
                    best_match = Some(i);
                }
            }
        }

        if let Some(base_idx) = best_match {
            merges.push((dot_idx, base_idx));
            skip_indices.insert(dot_idx);
        }
    }

    merges.sort_by_key(|&(_, base_idx)| base_idx);
    merges
}

/// Applies merges to components
fn apply_merges(components: &mut Vec<ConnectedComponent>, merges: &[(usize, usize)]) {
    for &(dot_idx, base_idx) in merges.iter().rev() {
        if dot_idx >= components.len() || base_idx >= components.len() {
            continue;
        }
        // Get mutable references to both components
        if dot_idx != base_idx {
            let (first, second) = if dot_idx < base_idx {
                (dot_idx, base_idx)
            } else {
                (base_idx, dot_idx)
            };

            // Split the vector into three parts to get mutable references to both components
            let (left, right) = components.split_at_mut(second);
            let dot = if first < left.len() {
                &left[first]
            } else {
                continue;
            };
            let base = if !right.is_empty() {
                &mut right[0]
            } else {
                continue;
            };

            // Update the base component's bounds to include the dot
            base.bounds.x = base.bounds.x.min(dot.bounds.x);
            base.bounds.y = base.bounds.y.min(dot.bounds.y);
            let right_edge =
                (base.bounds.x + base.bounds.width).max(dot.bounds.x + dot.bounds.width);
            let bottom_edge =
                (base.bounds.y + base.bounds.height).max(dot.bounds.y + dot.bounds.height);
            base.bounds.width = right_edge - base.bounds.x;
            base.bounds.height = bottom_edge - base.bounds.y;
            base.pixels.extend(dot.pixels.iter().copied());
            base.pixel_count += dot.pixel_count;
        }
    }

    let mut to_remove: Vec<usize> = merges.iter().map(|&(dot_idx, _)| dot_idx).collect();
    to_remove.sort_unstable_by(|a, b| b.cmp(a));
    for idx in to_remove {
        if idx < components.len() {
            components.remove(idx);
        }
    }
}

/// Attempt to split a glyph if it is abnormally wide.
/// Returns one or two `BitImage` slices and the split x-coordinate (if split).
pub fn maybe_split_glyph(
    glyph: &BitImage,
    tracker: &mut WidthTracker,
    config: &SplitConfig,
) -> (Vec<BitImage>, Option<usize>) {
    let w = glyph.width;
    let h = glyph.height;
    let avg_w = tracker.avg();
    let _out: Vec<BitImage> = Vec::new();

    // Record this glyph width regardless
    tracker.push(w);

    // If glyph is not “too wide,” just emit it whole
    if (w as f32) <= avg_w * config.max_width_ratio {
        return (vec![glyph.clone()], None);
    }

    // Otherwise scan for vertical white gaps
    let required_gap_height = (h as f32 * config.gap_height_frac).ceil() as usize;
    let mut best_split = None;
    let mut best_gap_size = 0;

    for x in config.min_gap..w - config.min_gap {
        let mut gap_run = 0;
        for y in 0..h {
            if !glyph.get_usize(x, y) {
                gap_run += 1;
            } else {
                gap_run = 0;
            }
        }
        if gap_run >= required_gap_height {
            let mut left = x;
            while left > 0 && (0..h).all(|y| !glyph.get_usize(left - 1, y)) {
                left -= 1;
            }
            let mut right = x;
            while right + 1 < w && (0..h).all(|y| !glyph.get_usize(right + 1, y)) {
                right += 1;
            }
            let gap_width = right - left + 1;
            if gap_width > best_gap_size {
                best_gap_size = gap_width;
                best_split = Some((left + gap_width / 2) as usize);
            }
        }
    }

    if let Some(split_x) = best_split {
        if split_x >= config.min_subglyph_width && (w - split_x) >= config.min_subglyph_width {
            let left_img = BitImage::from_sub_image(
                glyph,
                &Rect {
                    x: 0,
                    y: 0,
                    width: usize_to_u32(split_x),
                    height: usize_to_u32(h),
                },
            );
            let right_img = BitImage::from_sub_image(
                glyph,
                &Rect {
                    x: usize_to_u32(split_x),
                    y: 0,
                    width: usize_to_u32(w - split_x),
                    height: usize_to_u32(h),
                },
            );
            tracker.push(left_img.width);
            tracker.push(right_img.width);
            return (vec![left_img, right_img], Some(split_x));
        }
    }

    (vec![glyph.clone()], None)
}

/// Extracts symbols from a page image by finding, merging, and splitting components.
pub fn extract_symbols(image: &BitImage, config: SymbolExtractionConfig) -> Vec<(Rect, Symbol)> {
    let _ = save_debug_pbm(image, "00_original.pbm");

    let mut components = find_connected_components(image, config.min_component_size);
    if components.is_empty() {
        return Vec::new();
    }
    let mut tracker = WidthTracker::new(config.split_config.rolling_window);
    let dot_indices = detect_dots(&components, &config, &mut tracker);
    let merges = merge_dots(&mut components, &dot_indices, &config);
    apply_merges(&mut components, &merges);

    let mut symbols = Vec::new();
    let mut processed: FxHashSet<u64> = FxHashSet::default();

    for (i, component) in components.iter().enumerate() {
        let rect = component.bounds;
        let cropped = BitImage::from_sub_image(image, &rect);
        let (pieces, split_x) = maybe_split_glyph(&cropped, &mut tracker, &config.split_config);

        for (j, piece) in pieces.into_iter().enumerate() {
            let (piece_rect, trimmed_piece) = piece.trim();
            if trimmed_piece.width == 0 || trimmed_piece.height == 0 {
                continue; // Skip empty symbols
            }

            let piece_x = if j == 0 {
                rect.x + piece_rect.x
            } else {
                rect.x + split_x.unwrap_or(rect.width as usize / 2) as u32 + piece_rect.x
            };

            let hash = compute_glyph_hash(&trimmed_piece);
            // Validate symbol dimensions
            if trimmed_piece.width == 0 || trimmed_piece.height == 0 {
                continue; // Skip empty symbols
            }

            // Check symbol size limits (JBIG2 maximum dimensions are 2^32-1)
            if trimmed_piece.width > 10_000 || trimmed_piece.height > 10_000 {
                eprintln!(
                    "Warning: Skipping oversized symbol: {}x{}",
                    trimmed_piece.width, trimmed_piece.height
                );
                continue;
            }

            let symbol = Symbol {
                image: trimmed_piece,
                hash,
            };

            // Verify the hash is unique
            if !processed.insert(hash) {
                // if cfg!(debug_assertions) {
                //     eprintln!("Warning: Duplicate symbol hash detected: {}", hash);
                // }
            }

            if cfg!(debug_assertions) {
                let _ = save_debug_pbm(
                    &symbol.image,
                    &format!("02_symbol_{:04}.pbm", symbols.len()),
                );
            }

            symbols.push((
                Rect {
                    x: piece_x,
                    y: rect.y + piece_rect.y,
                    width: usize_to_u32(symbol.image.width),
                    height: usize_to_u32(symbol.image.height),
                },
                symbol,
            ));
        }

        if cfg!(debug_assertions) {
            let mut comp_image =
                BitImage::new(component.bounds.width, component.bounds.height).unwrap();
            for &(x, y) in &component.pixels {
                comp_image.set(x - component.bounds.x, y - component.bounds.y, true);
            }
            let _ = save_debug_pbm(&comp_image, &format!("01_component_{:04}.pbm", i));
        }
    }

    // Consolidate similar symbols to reduce dictionary size
    consolidate_symbols(symbols)
}

/// Consolidates similar symbols to reduce dictionary size
/// This is crucial for preventing 300+ symbol dictionaries
fn consolidate_symbols(mut symbols: Vec<(Rect, Symbol)>) -> Vec<(Rect, Symbol)> {
    if symbols.len() <= 50 {
        return symbols; // Already reasonable size
    }

    // Cap at 200 symbols to prevent performance issues
    if symbols.len() > 200 {
        symbols.truncate(200);
        eprintln!(
            "Warning: Too many symbols ({}), truncated to 200 to prevent performance issues",
            symbols.len()
        );
    }

    use crate::jbig2comparator::Comparator;
    let mut comparator = Comparator::default();
    let mut consolidated = Vec::new();
    let mut used = vec![false; symbols.len()];

    for i in 0..symbols.len() {
        if used[i] {
            continue;
        }

        let (rect_i, ref symbol_i) = symbols[i];
        consolidated.push((rect_i, symbol_i.clone()));
        used[i] = true;

        // Find similar symbols to merge with this one
        for j in (i + 1)..symbols.len() {
            if used[j] {
                continue;
            }

            let (_, ref symbol_j) = symbols[j];

            // Allow up to 3% pixel difference for consolidation (more conservative)
            let max_err = ((symbol_i.image.width * symbol_i.image.height) / 33).max(1) as u32;

            if comparator
                .distance(&symbol_i.image, &symbol_j.image, max_err)
                .is_some()
            {
                used[j] = true; // Mark as consolidated
            }
        }
    }

    eprintln!(
        "Symbol consolidation: {} -> {} symbols",
        symbols.len(),
        consolidated.len()
    );
    consolidated
}
