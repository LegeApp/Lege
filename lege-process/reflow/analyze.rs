//! Analysis preprocessing: ink masks and projection profiles.
//!
//! This stage turns a rendered grayscale page (or sub-region) into the geometric
//! primitives the rest of the reflow pipeline reasons about: a 1-bit [`InkMask`]
//! and horizontal / vertical [`ProjectionProfile`]s. It contains no semantic
//! understanding — only pixels, thresholds, and whitespace.

use super::config::RasterReflowConfig;
use super::types::{InkMask, ProjectionAxis, ProjectionProfile, PxRect};
use image::GrayImage;

/// A contiguous run of non-blank buckets in a projection profile, expressed in
/// source-page coordinates along the profile's axis as `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Band {
    pub start: u32,
    pub end: u32,
}

impl Band {
    pub fn len(&self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// Compute an Otsu threshold (0..=255) over a region of a grayscale image.
/// Pixels at or below the returned value are considered ink.
pub fn otsu_threshold(gray: &GrayImage, region: PxRect) -> u8 {
    let (img_w, img_h) = gray.dimensions();
    let x0 = region.x.min(img_w);
    let y0 = region.y.min(img_h);
    let x1 = region.right().min(img_w);
    let y1 = region.bottom().min(img_h);
    let stride = img_w as usize;
    let raw = gray.as_raw();

    let mut hist = [0u64; 256];
    let mut total = 0u64;
    for y in y0..y1 {
        let row_start = y as usize * stride + x0 as usize;
        for &v in &raw[row_start..row_start + (x1 - x0) as usize] {
            hist[v as usize] += 1;
            total += 1;
        }
    }
    if total == 0 {
        return 128;
    }

    let sum: f64 = hist
        .iter()
        .enumerate()
        .map(|(i, &c)| i as f64 * c as f64)
        .sum();

    let mut sum_b = 0.0f64;
    let mut w_b = 0.0f64;
    let mut max_var = -1.0f64;
    let mut threshold = 128usize;

    for t in 0..256 {
        w_b += hist[t] as f64;
        if w_b == 0.0 {
            continue;
        }
        let w_f = total as f64 - w_b;
        if w_f == 0.0 {
            break;
        }
        sum_b += t as f64 * hist[t] as f64;
        let m_b = sum_b / w_b;
        let m_f = (sum - sum_b) / w_f;
        let var_between = w_b * w_f * (m_b - m_f) * (m_b - m_f);
        if var_between > max_var {
            max_var = var_between;
            threshold = t;
        }
    }
    threshold as u8
}

/// Build an [`InkMask`] for a region of a grayscale page using the configured
/// thresholding policy, then optionally despeckle it.
pub fn build_ink_mask(gray: &GrayImage, region: PxRect, cfg: &RasterReflowConfig) -> InkMask {
    let (img_w, img_h) = gray.dimensions();
    let x0 = region.x.min(img_w);
    let y0 = region.y.min(img_h);
    let x1 = region.right().min(img_w);
    let y1 = region.bottom().min(img_h);
    let w = x1.saturating_sub(x0);
    let h = y1.saturating_sub(y0);

    let mut mask = InkMask::new(w, h, (x0, y0));
    if w == 0 || h == 0 {
        return mask;
    }

    // Clamp Otsu to `ink_threshold`: real ink is genuinely dark, so a valid text
    // threshold is below it. On a blank / faint page Otsu maximizes between-class
    // variance even with no real contrast and returns a high (~bright) threshold
    // that splits scanner texture ~50/50 into false ink; clamping makes such a
    // page read as ~empty instead.
    let thr = if cfg.adaptive_threshold {
        otsu_threshold(gray, PxRect::from_xyxy(x0, y0, x1, y1)).min(cfg.ink_threshold)
    } else {
        cfg.ink_threshold
    };

    // Threshold row-by-row from the raw buffer: contiguous slice access avoids
    // the bounds-checked `get_pixel`/`set` calls in this per-pixel hot loop.
    let stride = img_w as usize;
    let raw = gray.as_raw();
    let w = w as usize;
    for ly in 0..h as usize {
        let src_start = (y0 as usize + ly) * stride + x0 as usize;
        let dst_start = ly * w;
        for (lx, &v) in raw[src_start..src_start + w].iter().enumerate() {
            mask.bits[dst_start + lx] = v <= thr;
        }
    }

    if cfg.despeckle_min_neighbours > 0 {
        despeckle(&mut mask, cfg.despeckle_min_neighbours);
    }
    mask
}

/// True when `region` carries too little ink to be worth preserving — an empty
/// or scanner-textured area an ONNX detection may have mislabelled as a figure.
/// A region is blank when its ink fraction falls below `cfg.min_region_ink_frac`.
pub fn is_blank_region(gray: &GrayImage, region: PxRect, cfg: &RasterReflowConfig) -> bool {
    let mask = build_ink_mask(gray, region, cfg);
    let area = (mask.width as u64) * (mask.height as u64);
    area == 0 || (mask.ink_count() as f32) < (area as f32) * cfg.min_region_ink_frac
}

/// Remove isolated ink pixels: any ink pixel with fewer than `min_neighbours`
/// 8-connected ink neighbours is cleared. Single pass; conservative by design.
///
/// Reads happen against the original mask and clears are collected and applied
/// afterwards — this gives the same result as scanning a full-buffer snapshot
/// (a clear can never change a not-yet-visited pixel's neighbour count, since
/// it only ever turns ink *off*) without paying for an `O(width*height)` clone.
pub fn despeckle(mask: &mut InkMask, min_neighbours: u8) {
    if mask.width == 0 || mask.height == 0 {
        return;
    }
    let w = mask.width as i64;
    let h = mask.height as i64;
    let mut to_clear: Vec<usize> = Vec::new();
    {
        let bits = &mask.bits;
        let get = |x: i64, y: i64| -> bool {
            if x < 0 || y < 0 || x >= w || y >= h {
                false
            } else {
                bits[(y * w + x) as usize]
            }
        };
        for y in 0..h {
            for x in 0..w {
                if !get(x, y) {
                    continue;
                }
                let mut n = 0u8;
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        if get(x + dx, y + dy) {
                            n += 1;
                        }
                    }
                }
                if n < min_neighbours {
                    to_clear.push((y * w + x) as usize);
                }
            }
        }
    }
    for i in to_clear {
        mask.bits[i] = false;
    }
}

/// Horizontal projection: ink count per row (Y bucket). Length == mask height.
/// Buckets are reported with `offset` set to the mask's page-Y origin.
pub fn horizontal_projection(mask: &InkMask) -> ProjectionProfile {
    let mut values = vec![0u32; mask.height as usize];
    for y in 0..mask.height {
        let mut count = 0u32;
        let base = (y as usize) * (mask.width as usize);
        for x in 0..mask.width as usize {
            if mask.bits[base + x] {
                count += 1;
            }
        }
        values[y as usize] = count;
    }
    ProjectionProfile {
        axis: ProjectionAxis::Horizontal,
        values,
        offset: mask.origin.1,
    }
}

/// Vertical projection: ink count per column (X bucket). Length == mask width.
/// Buckets are reported with `offset` set to the mask's page-X origin.
pub fn vertical_projection(mask: &InkMask) -> ProjectionProfile {
    let mut values = vec![0u32; mask.width as usize];
    for y in 0..mask.height {
        let base = (y as usize) * (mask.width as usize);
        for x in 0..mask.width as usize {
            if mask.bits[base + x] {
                values[x] += 1;
            }
        }
    }
    ProjectionProfile {
        axis: ProjectionAxis::Vertical,
        values,
        offset: mask.origin.0,
    }
}

/// Box-smooth a profile in place with a window of `radius` (0 = no-op). Useful
/// to bridge faint-scan dropouts before band detection.
pub fn smooth_profile(profile: &mut ProjectionProfile, radius: usize) {
    if radius == 0 || profile.values.len() < 2 {
        return;
    }
    let src = profile.values.clone();
    let n = src.len();
    for i in 0..n {
        let lo = i.saturating_sub(radius);
        let hi = (i + radius + 1).min(n);
        let sum: u64 = src[lo..hi].iter().map(|&v| v as u64).sum();
        profile.values[i] = (sum / (hi - lo) as u64) as u32;
    }
}

/// Find bands (contiguous non-blank bucket runs) in a profile.
///
/// A bucket is "blank" when its value `<= blank_threshold`. Adjacent non-blank
/// buckets are merged; gaps shorter than `min_gap` between two bands are bridged
/// so that intra-line dropouts don't fragment a row. Bands shorter than
/// `min_len` are discarded. Returned coordinates are absolute (profile.offset
/// applied) along the profile axis.
pub fn find_bands(
    profile: &ProjectionProfile,
    blank_threshold: u32,
    min_gap: u32,
    min_len: u32,
) -> Vec<Band> {
    let mut raw: Vec<Band> = Vec::new();
    let mut run_start: Option<u32> = None;
    for (i, &v) in profile.values.iter().enumerate() {
        let i = i as u32;
        if v > blank_threshold {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else if let Some(s) = run_start.take() {
            raw.push(Band { start: s, end: i });
        }
    }
    if let Some(s) = run_start.take() {
        raw.push(Band {
            start: s,
            end: profile.values.len() as u32,
        });
    }

    // Bridge small gaps.
    let mut merged: Vec<Band> = Vec::new();
    for b in raw {
        match merged.last_mut() {
            Some(prev) if b.start.saturating_sub(prev.end) <= min_gap => {
                prev.end = b.end;
            }
            _ => merged.push(b),
        }
    }

    // Apply offset and length filter.
    merged
        .into_iter()
        .filter(|b| b.len() >= min_len)
        .map(|b| Band {
            start: b.start + profile.offset,
            end: b.end + profile.offset,
        })
        .collect()
}

/// Compute a blank threshold from a profile and the configured blank fraction.
pub fn blank_threshold(profile: &ProjectionProfile, blank_ink_frac: f32) -> u32 {
    ((profile.max() as f32) * blank_ink_frac).floor() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Luma};

    /// Build a white page with black horizontal bars to simulate text rows.
    fn page_with_rows(w: u32, h: u32, rows: &[(u32, u32)]) -> GrayImage {
        let mut img = GrayImage::from_pixel(w, h, Luma([255]));
        for &(y0, y1) in rows {
            for y in y0..y1 {
                for x in 0..w {
                    img.put_pixel(x, y, Luma([0]));
                }
            }
        }
        img
    }

    #[test]
    fn otsu_separates_black_and_white() {
        let mut img = GrayImage::from_pixel(10, 10, Luma([240]));
        for y in 0..5 {
            for x in 0..10 {
                img.put_pixel(x, y, Luma([10]));
            }
        }
        let t = otsu_threshold(&img, PxRect::new(0, 0, 10, 10));
        // Otsu picks the lowest threshold achieving max between-class variance,
        // so a clean bimodal histogram at {10, 240} yields exactly 10 — and
        // `build_ink_mask` classifies ink as `v <= thr`, so 10 still separates
        // the modes correctly.
        assert!(t >= 10 && t < 240, "threshold {t} should split the modes");
    }

    #[test]
    fn ink_mask_marks_dark_pixels() {
        let img = page_with_rows(20, 20, &[(5, 8)]);
        let cfg = RasterReflowConfig {
            adaptive_threshold: false,
            ink_threshold: 128,
            despeckle_min_neighbours: 0,
            ..Default::default()
        };
        let mask = build_ink_mask(&img, PxRect::new(0, 0, 20, 20), &cfg);
        assert!(mask.get(0, 5));
        assert!(!mask.get(0, 0));
        assert_eq!(mask.ink_count(), 20 * 3);
    }

    #[test]
    fn despeckle_removes_isolated_pixels() {
        let mut mask = InkMask::new(5, 5, (0, 0));
        mask.set(2, 2, true); // lone pixel
        despeckle(&mut mask, 1);
        assert_eq!(mask.ink_count(), 0);

        let mut mask2 = InkMask::new(5, 5, (0, 0));
        mask2.set(2, 2, true);
        mask2.set(2, 3, true); // each has one neighbour
        despeckle(&mut mask2, 1);
        assert_eq!(mask2.ink_count(), 2);
    }

    #[test]
    fn horizontal_projection_counts_rows() {
        let img = page_with_rows(10, 10, &[(2, 4)]);
        let cfg = RasterReflowConfig {
            adaptive_threshold: false,
            ink_threshold: 128,
            despeckle_min_neighbours: 0,
            ..Default::default()
        };
        let mask = build_ink_mask(&img, PxRect::new(0, 0, 10, 10), &cfg);
        let prof = horizontal_projection(&mask);
        assert_eq!(prof.values[0], 0);
        assert_eq!(prof.values[2], 10);
        assert_eq!(prof.values[3], 10);
        assert_eq!(prof.values[4], 0);
    }

    #[test]
    fn find_bands_detects_two_rows_with_offset() {
        // profile: blanks, band 3..5, blank, band 7..9
        let profile = ProjectionProfile {
            axis: ProjectionAxis::Horizontal,
            values: vec![0, 0, 0, 10, 10, 0, 0, 9, 9, 0],
            offset: 100,
        };
        let bands = find_bands(&profile, 0, 0, 1);
        assert_eq!(
            bands,
            vec![
                Band {
                    start: 103,
                    end: 105
                },
                Band {
                    start: 107,
                    end: 109
                },
            ]
        );
    }

    #[test]
    fn find_bands_bridges_small_gaps() {
        let profile = ProjectionProfile {
            axis: ProjectionAxis::Horizontal,
            values: vec![5, 5, 0, 5, 5],
            offset: 0,
        };
        // min_gap = 1 bridges the single blank bucket into one band 0..5
        let bands = find_bands(&profile, 0, 1, 1);
        assert_eq!(bands, vec![Band { start: 0, end: 5 }]);
    }

    #[test]
    fn find_bands_filters_short_bands() {
        let profile = ProjectionProfile {
            axis: ProjectionAxis::Vertical,
            values: vec![0, 5, 0, 5, 5, 5],
            offset: 0,
        };
        let bands = find_bands(&profile, 0, 0, 2);
        assert_eq!(bands, vec![Band { start: 3, end: 6 }]);
    }

    #[test]
    fn smooth_profile_box_filters() {
        let mut profile = ProjectionProfile {
            axis: ProjectionAxis::Horizontal,
            values: vec![0, 0, 9, 0, 0],
            offset: 0,
        };
        smooth_profile(&mut profile, 1);
        // center spreads to neighbours
        assert_eq!(profile.values[1], 3);
        assert_eq!(profile.values[2], 3);
        assert_eq!(profile.values[3], 3);
    }
}
