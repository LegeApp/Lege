//! Configuration for RasterReflow.
//!
//! Values here are *behavioral references* tuned for typical scanned books, not
//! constants copied from any other tool. They are expressed relative to measured
//! page geometry (line spacing, text height) wherever possible so they scale
//! with render DPI.

#![allow(dead_code)]

/// Target output device / content-area description plus all reflow thresholds.
#[derive(Debug, Clone)]
pub struct RasterReflowConfig {
    // --- Output canvas ---
    /// Full output page width in pixels.
    pub page_width: u32,
    /// Full output page height in pixels.
    pub page_height: u32,
    /// Margin (px) applied on every side; content area is page minus margins.
    pub margin: u32,

    // --- Ink / binarization ---
    /// Luma value (0..=255) at or below which a pixel counts as ink. Used by the
    /// fixed-threshold path; an adaptive (Otsu) threshold can override it.
    pub ink_threshold: u8,
    /// Use an Otsu-derived threshold instead of `ink_threshold`.
    pub adaptive_threshold: bool,
    /// Despeckle: drop isolated ink pixels with fewer than this many ink
    /// neighbours (8-connected). 0 disables despeckling.
    pub despeckle_min_neighbours: u8,

    // --- Column / region detection ---
    /// Maximum columns to consider per page (k2pdfopt-style default of 2).
    pub max_columns: usize,
    /// A vertical whitespace gully must be at least this fraction of the region
    /// width to split columns.
    pub column_gap_min_frac: f32,
    /// A row/column bucket counts as "blank" if its ink is <= this fraction of
    /// the profile's max.
    pub blank_ink_frac: f32,
    /// Minimum fraction of a region that must be ink for it to be treated as
    /// text. Below this the region is considered blank (scanner texture, faint
    /// bleed) and produces no rows — otherwise Otsu binarization amplifies the
    /// noise into a solid black page.
    pub min_region_ink_frac: f32,

    // --- Row detection / vertical grouping ---
    /// Minimum row band height in pixels (filters specks / rule lines).
    pub min_row_height: u32,
    /// Rows separated by a gap larger than `row_break_factor * median_line_gap`
    /// start a new logical chunk (region break).
    pub row_break_factor: f32,
    /// Preserved vertical gaps are capped at `max_gap_factor * median_line_gap`
    /// so large source whitespace does not waste output space.
    pub max_gap_factor: f32,

    // --- Word segmentation ---
    /// A blank-column run inside a row is a word gap if it is at least
    /// `word_gap_factor * text_height` wide.
    pub word_gap_factor: f32,
    /// Absolute floor for a word gap, in pixels.
    pub min_word_gap: u32,

    // --- Composition / scaling ---
    /// Desired body-text height in output pixels. When `text_magnification > 0`
    /// this is set adaptively per document from the measured source body height
    /// (and acts as the upper ceiling); otherwise it is used directly as a fixed
    /// absolute target.
    pub target_text_height: u32,
    /// How much larger body text should appear **relative to the source**,
    /// measured device-independently as the fraction of page height a line of
    /// body text occupies. `1.0` keeps body text at roughly its original
    /// relative size — output page count stays close to the source (plus some
    /// re-wrapping) — while `>1.0` enlarges. This is the primary "how dramatic is
    /// the reflow" knob; it drives `target_text_height` adaptively from the
    /// calibrated source body height. Set to `0.0` to use a fixed
    /// `target_text_height` instead.
    pub text_magnification: f32,
    /// Floor (output px) for the adaptively-derived target text height, so very
    /// small source text is still enlarged to a readable size.
    pub min_text_height: u32,
    /// Document-wide source body-row height (source px) from calibration. When
    /// set, every page scales its body text by the *same* `target_text_height /
    /// calibrated_body_height` factor, so glyph size is uniform across the whole
    /// document. When `None`, each page falls back to its own measured body
    /// height (only correct in isolation — different pages then render at
    /// different sizes).
    pub calibrated_body_height: Option<u32>,
    /// Clamp on the source→output scale factor.
    pub scale_min: f32,
    pub scale_max: f32,
    /// Output spacing between packed words, as a fraction of text height.
    pub word_spacing_frac: f32,
    /// Output line spacing as a fraction of text height (line pitch = h * this).
    pub line_spacing_frac: f32,

    // --- Behavior / fallback ---
    /// Use ONNX layout detections as coarse region hints when available.
    pub use_onnx_hints: bool,
    /// Confidence below which a page falls back to conservative crop/resize.
    pub fallback_confidence: f32,
    /// Row count that fully saturates the confidence "enough rows to judge"
    /// factor — `min(row_count / this, 1.0)`.
    pub confidence_row_norm: f32,
    /// Weight given to row count vs. row-height regularity when blending the
    /// page confidence score (`regularity_weight = 1.0 - this`).
    pub confidence_row_weight: f32,
    /// Emit debug overlays (columns, regions, rows, words, placements).
    pub debug_overlays: bool,
}

impl Default for RasterReflowConfig {
    fn default() -> Self {
        // Defaults sized for a ~1448px-wide e-ink content area (common 6"+ E-Ink
        // panels rendered around 300 DPI). All thresholds are relative factors.
        Self {
            page_width: 1448,
            page_height: 1072,
            margin: 24,

            ink_threshold: 160,
            adaptive_threshold: true,
            despeckle_min_neighbours: 1,

            max_columns: 2,
            column_gap_min_frac: 0.04,
            blank_ink_frac: 0.02,
            min_region_ink_frac: 0.004,

            min_row_height: 4,
            row_break_factor: 1.8,
            max_gap_factor: 1.5,

            // Word-break gap as a fraction of text height. Justified book body
            // text has word spaces well under half the line height; 0.45 left
            // most rows unsplit (whole line became one over-wide "word" that got
            // clamped and squished). ~0.30 cleanly separates words without
            // splitting inside them.
            word_gap_factor: 0.30,
            min_word_gap: 3,

            target_text_height: 40,
            text_magnification: 1.15,
            min_text_height: 22,
            calibrated_body_height: None,
            scale_min: 0.5,
            scale_max: 6.0,
            word_spacing_frac: 0.33,
            line_spacing_frac: 1.30,

            use_onnx_hints: true,
            fallback_confidence: 0.45,
            confidence_row_norm: 5.0,
            confidence_row_weight: 0.4,
            debug_overlays: false,
        }
    }
}

impl RasterReflowConfig {
    /// Width of the usable content area (page width minus both margins).
    pub fn content_width(&self) -> u32 {
        self.page_width
            .saturating_sub(self.margin.saturating_mul(2))
    }

    /// Height of the usable content area (page height minus both margins).
    pub fn content_height(&self) -> u32 {
        self.page_height
            .saturating_sub(self.margin.saturating_mul(2))
    }

    /// Output word spacing in pixels for a given text height.
    pub fn word_spacing_px(&self, text_height: u32) -> u32 {
        ((text_height as f32) * self.word_spacing_frac).round() as u32
    }

    /// Output line pitch in pixels for a given text height.
    pub fn line_pitch_px(&self, text_height: u32) -> u32 {
        ((text_height as f32) * self.line_spacing_frac)
            .round()
            .max(1.0) as u32
    }

    /// Clamp a raw scale factor into the configured range.
    pub fn clamp_scale(&self, scale: f32) -> f32 {
        scale.clamp(self.scale_min, self.scale_max)
    }

    /// Basic sanity validation; returns an error string on misconfiguration.
    pub fn validate(&self) -> Result<(), String> {
        if self.page_width == 0 || self.page_height == 0 {
            return Err("page dimensions must be non-zero".into());
        }
        if self.content_width() == 0 || self.content_height() == 0 {
            return Err("margins leave no content area".into());
        }
        if self.target_text_height == 0 {
            return Err("target_text_height must be non-zero".into());
        }
        if self.scale_min <= 0.0 || self.scale_max < self.scale_min {
            return Err("invalid scale range".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_validates() {
        assert!(RasterReflowConfig::default().validate().is_ok());
    }

    #[test]
    fn content_area_accounts_for_margins() {
        let cfg = RasterReflowConfig {
            page_width: 1000,
            page_height: 800,
            margin: 50,
            ..Default::default()
        };
        assert_eq!(cfg.content_width(), 900);
        assert_eq!(cfg.content_height(), 700);
    }

    #[test]
    fn scale_clamped() {
        let cfg = RasterReflowConfig::default();
        assert_eq!(cfg.clamp_scale(0.01), cfg.scale_min);
        assert_eq!(cfg.clamp_scale(100.0), cfg.scale_max);
        assert!((cfg.clamp_scale(2.0) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn derived_spacing() {
        let cfg = RasterReflowConfig {
            word_spacing_frac: 0.5,
            line_spacing_frac: 1.2,
            ..Default::default()
        };
        assert_eq!(cfg.word_spacing_px(40), 20);
        assert_eq!(cfg.line_pitch_px(40), 48);
    }

    #[test]
    fn zero_content_area_rejected() {
        let cfg = RasterReflowConfig {
            page_width: 100,
            margin: 60,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }
}
