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
