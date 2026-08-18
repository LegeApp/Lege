//! Page-representation classification over pdf-read's metrics.
//!
//! The metrics are facts. The label is interpretation: it names the kind of
//! page those facts describe so a report can say "raster-only" instead of
//! leaving the reader to compare coverage numbers. Thresholds are documented
//! next to the constants they live in.

use pdf_read::PageMetrics;

use crate::document::PageRepresentation;

/// Coverage at or above this (basis points of the crop box) counts as a
/// full-page image. 80% leaves room for crop/media mismatch and a thin
/// margin without letting a large photo on a vector page become raster-only.
const FULL_PAGE_BPS: u16 = 8_000;

/// Coverage below this is treated as decorative: a logo or icon does not
/// make a text page Mixed.
const DECORATIVE_BPS: u16 = 500;

/// Classify a compiled page from its metrics.
///
/// A page that failed to compile never reaches this function; the caller
/// labels that [`PageRepresentation::Unknown`].
#[must_use]
pub fn classify(metrics: &PageMetrics) -> PageRepresentation {
    let has_visible_vector =
        metrics.visible_text_runs > 0 || metrics.path_paints > 0 || metrics.shading_paints > 0;
    let has_any_marks = has_visible_vector || metrics.invisible_text_runs > 0 || metrics.images > 0;

    if !has_any_marks {
        return PageRepresentation::Blank;
    }

    if metrics.max_image_coverage_bps >= FULL_PAGE_BPS {
        if !has_visible_vector {
            if metrics.invisible_text_runs > 0 {
                return PageRepresentation::RasterWithTextLayer;
            }
            return PageRepresentation::RasterOnly;
        }
        return PageRepresentation::Mixed;
    }

    if metrics.max_image_coverage_bps <= DECORATIVE_BPS && has_visible_vector {
        return PageRepresentation::Vector;
    }

    if metrics.max_image_coverage_bps > DECORATIVE_BPS && has_visible_vector {
        return PageRepresentation::Mixed;
    }

    // Images that do not dominate and no visible vector: a small photo on an
    // otherwise empty page, or only an invisible text layer. Neither is a
    // full-page raster; Mixed is the honest residual.
    if metrics.images > 0 {
        return PageRepresentation::Mixed;
    }

    PageRepresentation::Vector
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(fill: impl FnOnce(&mut PageMetrics)) -> PageMetrics {
        let mut m = PageMetrics::default();
        fill(&mut m);
        m
    }

    #[test]
    fn empty_is_blank() {
        assert_eq!(classify(&PageMetrics::default()), PageRepresentation::Blank);
    }

    #[test]
    fn text_without_images_is_vector() {
        assert_eq!(
            classify(&metrics(|m| {
                m.text_runs = 1;
                m.visible_text_runs = 1;
                m.fonts = 1;
            })),
            PageRepresentation::Vector
        );
    }

    #[test]
    fn full_page_image_without_text_is_raster_only() {
        assert_eq!(
            classify(&metrics(|m| {
                m.images = 1;
                m.image_coverage_bps = 10_000;
                m.max_image_coverage_bps = 10_000;
            })),
            PageRepresentation::RasterOnly
        );
    }

    #[test]
    fn full_page_image_with_invisible_text_is_ocr_layer() {
        assert_eq!(
            classify(&metrics(|m| {
                m.images = 1;
                m.image_coverage_bps = 10_000;
                m.max_image_coverage_bps = 10_000;
                m.text_runs = 1;
                m.invisible_text_runs = 1;
            })),
            PageRepresentation::RasterWithTextLayer
        );
    }

    #[test]
    fn full_page_image_with_visible_text_is_mixed() {
        assert_eq!(
            classify(&metrics(|m| {
                m.images = 1;
                m.image_coverage_bps = 10_000;
                m.max_image_coverage_bps = 10_000;
                m.text_runs = 1;
                m.visible_text_runs = 1;
            })),
            PageRepresentation::Mixed
        );
    }

    #[test]
    fn a_small_logo_does_not_make_a_text_page_mixed() {
        assert_eq!(
            classify(&metrics(|m| {
                m.text_runs = 4;
                m.visible_text_runs = 4;
                m.images = 1;
                m.image_coverage_bps = 200;
                m.max_image_coverage_bps = 200;
            })),
            PageRepresentation::Vector
        );
    }
}
