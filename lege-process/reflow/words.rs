//! Word segmentation within a row.
//!
//! Splits a row into "graphical words" — horizontal slices separated by
//! blank-column gaps wide enough to be word breaks rather than inter-character
//! spacing. The word-gap threshold is derived from the row's text height so it
//! scales with the rendered glyph size. No recognized text is required; each
//! span is a bitmap slice carrying only its source rectangle.

use super::analyze::{self, blank_threshold};
use super::config::RasterReflowConfig;
use super::types::{PxRect, SourcePageImage, TextRow, WordSpan};

/// The minimum blank-column run (px) that separates two words in a row of the
/// given text height.
pub fn word_gap_threshold(text_height: u32, cfg: &RasterReflowConfig) -> u32 {
    let rel = ((text_height as f32) * cfg.word_gap_factor).round() as u32;
    rel.max(cfg.min_word_gap)
}

/// Segment a row into word spans, left-to-right. Punctuation and accents stay
/// attached to their neighbour because only gaps `>= word_gap_threshold` break a
/// word; narrower (character) gaps are bridged.
pub fn detect_words(
    page: &SourcePageImage,
    row: &TextRow,
    row_index: usize,
    cfg: &RasterReflowConfig,
) -> Vec<WordSpan> {
    let mask = analyze::build_ink_mask(&page.gray, row.rect, cfg);
    let vproj = analyze::vertical_projection(&mask);
    let blank = blank_threshold(&vproj, cfg.blank_ink_frac);

    let gap = word_gap_threshold(row.text_height, cfg);
    // Bridge gaps strictly narrower than a word gap (i.e. character spacing).
    let min_gap = gap.saturating_sub(1);
    // Keep even single-glyph words (e.g. "I", "a").
    let min_len = 1u32;

    let bands = analyze::find_bands(&vproj, blank, min_gap, min_len);
    bands
        .into_iter()
        .enumerate()
        .map(|(i, b)| WordSpan {
            rect: tight_word_rect(&mask, row, b.start, b.end, 1),
            row_index,
            order_in_row: i,
        })
        .collect()
}

fn tight_word_rect(
    mask: &super::types::InkMask,
    row: &TextRow,
    abs_x0: u32,
    abs_x1: u32,
    pad_y: u32,
) -> PxRect {
    let local_x0 = abs_x0.saturating_sub(mask.origin.0).min(mask.width);
    let local_x1 = abs_x1.saturating_sub(mask.origin.0).min(mask.width);
    if local_x0 >= local_x1 || mask.height == 0 {
        return PxRect::from_xyxy(abs_x0, row.rect.y, abs_x1, row.rect.bottom());
    }

    let mut top: Option<u32> = None;
    let mut bottom = 0u32;
    for y in 0..mask.height {
        let mut has_ink = false;
        for x in local_x0..local_x1 {
            if mask.get(x, y) {
                has_ink = true;
                break;
            }
        }
        if has_ink {
            top.get_or_insert(y);
            bottom = y + 1;
        }
    }

    match top {
        Some(y0) => {
            let y0 = y0.saturating_sub(pad_y);
            let y1 = (bottom + pad_y).min(mask.height);
            PxRect::from_xyxy(abs_x0, row.rect.y + y0, abs_x1, row.rect.y + y1)
        }
        None => PxRect::from_xyxy(abs_x0, row.rect.y, abs_x1, row.rect.bottom()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Luma};

    /// Build a single-row page where ink columns are given as `[start,end)` runs.
    fn row_page(w: u32, h: u32, runs: &[(u32, u32)]) -> SourcePageImage {
        let mut img = GrayImage::from_pixel(w, h, Luma([255]));
        for &(x0, x1) in runs {
            for x in x0..x1 {
                for y in 2..h - 2 {
                    img.put_pixel(x, y, Luma([0]));
                }
            }
        }
        SourcePageImage {
            page_index: 0,
            gray: img,
            rgb: None,
            render_dpi: 300.0,
            page_pts: (w as f32, h as f32),
        }
    }

    fn cfg() -> RasterReflowConfig {
        RasterReflowConfig {
            adaptive_threshold: false,
            ink_threshold: 128,
            despeckle_min_neighbours: 0,
            min_word_gap: 3,
            word_gap_factor: 0.45,
            ..Default::default()
        }
    }

    fn row(w: u32, h: u32) -> TextRow {
        TextRow {
            rect: PxRect::new(0, 0, w, h),
            text_height: h,
            gap_above: None,
            region_id: 0,
            reading_order: 0,
        }
    }

    #[test]
    fn word_gap_threshold_scales_with_height() {
        let c = cfg();
        // 0.45 * 20 = 9
        assert_eq!(word_gap_threshold(20, &c), 9);
        // floor at min_word_gap
        assert_eq!(word_gap_threshold(2, &c), 3);
    }

    #[test]
    fn splits_words_on_wide_gaps_bridges_char_gaps() {
        // text height 20 → word gap threshold 9. Glyphs separated by 2px (chars)
        // form a word; a 12px gap separates two words.
        // word A: 0..4, 6..10, 12..16  (char gaps of 2 bridged)
        // gap of 12
        // word B: 28..32, 34..38
        let page = row_page(50, 20, &[(0, 4), (6, 10), (12, 16), (28, 32), (34, 38)]);
        let words = detect_words(&page, &row(50, 20), 0, &cfg());
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].rect.x, 0);
        assert_eq!(words[0].rect.right(), 16);
        assert_eq!(words[1].rect.x, 28);
        assert_eq!(words[1].rect.right(), 38);
        assert_eq!(words[1].order_in_row, 1);
    }

    #[test]
    fn single_glyph_word_kept() {
        let page = row_page(30, 20, &[(0, 2)]);
        let words = detect_words(&page, &row(30, 20), 0, &cfg());
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].rect.x, 0);
    }

    #[test]
    fn empty_row_yields_no_words() {
        let page = row_page(30, 20, &[]);
        let words = detect_words(&page, &row(30, 20), 0, &cfg());
        assert!(words.is_empty());
    }

    #[test]
    fn word_rect_is_tight_to_ink_vertically() {
        let page = row_page(30, 24, &[(5, 15)]);
        let words = detect_words(&page, &row(30, 24), 0, &cfg());
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].rect.y, 1);
        assert_eq!(words[0].rect.bottom(), 23);
    }
}
