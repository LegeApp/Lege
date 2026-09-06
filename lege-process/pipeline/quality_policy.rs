//! Centralized image-quality policy.
//!
//! Every encoder quality value used by the pipeline lives here behind a named
//! intent, so tuning or A/B-ing a single surface is a one-line change instead of
//! hunting scattered literals. Each `high_quality` arm is the value used when the
//! user requests HQ output; the other arm is the default (size-first) value.
//!
//! Values were consolidated verbatim from their previous inline call sites — no
//! behavior change was introduced by this module; only the source of truth moved.
//!
//! Quality ranges are libjpeg-style 0..100 for JPEG and jp2lam 0..100 for JP2.

/// Full-page JP2 base layer (`jpeg_compat` off). Aggressive by design: the page
/// is mostly bilevel text handled elsewhere; this layer carries only residual
/// color/gray, so it is the lowest-quality surface.
#[inline]
pub fn full_page_jp2(high_quality: bool) -> u8 {
    // 50 is the program-wide jp2lam floor (see encoding::jp2::JP2_MIN_QUALITY):
    // output below 50 is visibly artifacted.
    if high_quality { 55 } else { 50 }
}

/// Full-page JPEG when `jpeg_compat` forces JPEG for the entire page.
#[inline]
pub fn full_page_jpeg_compat(high_quality: bool) -> u8 {
    if high_quality { 95 } else { 60 }
}

/// Full-page JPEG base layer on the `jpeg` text-format path.
#[inline]
pub fn full_page_jpeg_text(high_quality: bool) -> u8 {
    if high_quality { 95 } else { 40 }
}

/// JPEG for a figure-region overlay, or a cover page. Cover pages get a small
/// bump over ordinary regions since they are the most-looked-at page.
#[inline]
pub fn region_jpeg(high_quality: bool, is_cover: bool) -> u8 {
    if high_quality {
        95
    } else if is_cover {
        50
    } else {
        45
    }
}

/// JP2 for a figure-region overlay, or a cover page.
#[inline]
pub fn region_jp2(high_quality: bool, is_cover: bool) -> u8 {
    if high_quality {
        88
    } else if is_cover {
        80
    } else {
        72
    }
}

/// Grayscale JP2 overlay for a figure region (the `GrayJp2` dither mode).
#[inline]
pub fn region_gray_jp2(high_quality: bool) -> u8 {
    if high_quality { 80 } else { 68 }
}

// ---------------------------------------------------------------------------
// Verified display floors
//
// The functions above are jp2lam's legacy open-loop preset, whose perceptual
// quality drifts with content and resolution. The ones below are SSIMULACRA2
// floors measured at the size the reader actually sees the image, used by
// `EncodingSettings::Jp2Display`. They are NOT the same scale, so a preset of
// 80 does not become a floor of 80 — each value below was picked from a
// measured sweep against the preset it replaces (8 pages of a 12 MP book scan,
// 2663x4506 into a 709x1200 e-ink box). The legacy preset survives only as the
// fallback when the verified encode itself fails (`fallback_quality`); every
// JP2 site takes the verified path, at box == source too, since the product
// renders pages at device height and legacy q50 there measured -25..42.
// ---------------------------------------------------------------------------

/// Display floor for the full-page JP2 base layer (presets 55/50).
///
/// Deliberately low: this layer carries residual colour/gray under the text,
/// and is the one surface where size beats fidelity. Preset 50 measured 47-58
/// at display size (a lottery); floor 55 costs 7-21% more bytes and lifts it to
/// 63-76 on every page. Floor 50 was cheaper still but occasionally scored
/// *below* the preset, so 55 is the lowest floor that never loses.
#[inline]
pub fn full_page_jp2_floor(high_quality: bool) -> u8 {
    if high_quality { 60 } else { 55 }
}

/// How much of the output page a figure region covers.
///
/// The rule: a region is [`RegionSize::Large`] when its pixel area is at least
/// **one quarter** of the page's pixel area, and [`RegionSize::Small`]
/// otherwise. Both areas are measured in the same pixel space — the box the
/// region occupies on the output page versus that page's own pixel size — so
/// the classification is about how much of the reader's screen the figure
/// takes, not about the source scan's resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionSize {
    /// Less than a quarter of the page's pixel area.
    Small,
    /// At least a quarter of the page's pixel area.
    Large,
}

impl RegionSize {
    /// Classify a region by its share of the page area (see [`RegionSize`]).
    ///
    /// A zero-area page cannot be divided into, so it is treated as `Large`:
    /// the conservative (higher-quality) arm.
    #[inline]
    pub fn of(region_w: u32, region_h: u32, page_w: u32, page_h: u32) -> Self {
        let region_area = region_w as u64 * region_h as u64;
        let page_area = page_w as u64 * page_h as u64;
        if page_area == 0 || region_area * 4 >= page_area {
            RegionSize::Large
        } else {
            RegionSize::Small
        }
    }
}

/// Display floor for a figure-region or cover JP2 (presets 88/80/72).
///
/// Floor 70 replacing preset 72 measured 0.64-0.74x the bytes at an equal or
/// better display score, so this is the tier where the display target pays on
/// both axes. Cover and HQ keep the preset ladder's relative lift.
///
/// Ordinary (non-HQ, non-cover) regions are additionally split by size. After
/// the jp2lam perceptual work this floor is no longer an open-loop preset: it
/// is a *guaranteed* SSIMULACRA2-style score measured at display size, so the
/// same number buys the same visible fidelity regardless of the region's pixel
/// count. That makes it safe to spend less on a figure that only occupies a
/// corner of the page — a small figure gets 65, while a large plate, where a
/// reader's eye lingers and artifacts have room to show, keeps 70. This is a
/// deliberate size-first tuning made on 2026-09-06; HQ and cover are unchanged.
#[inline]
pub fn region_jp2_floor(high_quality: bool, is_cover: bool, size: RegionSize) -> u8 {
    if high_quality {
        78
    } else if is_cover {
        75
    } else {
        match size {
            RegionSize::Small => 65,
            RegionSize::Large => 70,
        }
    }
}

/// Display floor for the grayscale JP2 figure overlay (presets 80/68).
///
/// Floor 65 replacing preset 68 measured 0.96-1.00x the bytes while lifting the
/// display score from 57-73 to 84-86: the same size, but guaranteed.
#[inline]
pub fn region_gray_jp2_floor(high_quality: bool) -> u8 {
    if high_quality { 70 } else { 65 }
}

#[cfg(test)]
mod tests {
    use super::{RegionSize, region_jp2_floor};

    #[test]
    fn region_size_boundary_is_a_quarter_of_the_page() {
        // Page is 1000x1000 = 1_000_000 px; the boundary is 250_000 px.
        assert_eq!(RegionSize::of(500, 500, 1000, 1000), RegionSize::Large);
        assert_eq!(RegionSize::of(500, 499, 1000, 1000), RegionSize::Small);
        // Exactly one quarter counts as Large.
        assert_eq!(RegionSize::of(250, 1000, 1000, 1000), RegionSize::Large);
        assert_eq!(RegionSize::of(249, 1000, 1000, 1000), RegionSize::Small);
        // Degenerate page: no division possible, take the safe arm.
        assert_eq!(RegionSize::of(10, 10, 0, 0), RegionSize::Large);
    }

    #[test]
    fn ordinary_region_floor_is_size_dependent() {
        assert_eq!(region_jp2_floor(false, false, RegionSize::Small), 65);
        assert_eq!(region_jp2_floor(false, false, RegionSize::Large), 70);
    }

    #[test]
    fn hq_and_cover_floors_ignore_size() {
        for size in [RegionSize::Small, RegionSize::Large] {
            assert_eq!(region_jp2_floor(true, false, size), 78);
            assert_eq!(region_jp2_floor(false, true, size), 75);
            // high_quality wins over is_cover, as before.
            assert_eq!(region_jp2_floor(true, true, size), 78);
        }
    }
}
