//! Font-file metadata, read through the renderer's font stack.
//!
//! The processor needs a handful of numbers out of a TrueType/OpenType file to
//! fill in a PDF `FontDescriptor`: the design-unit grid, the vertical metrics,
//! the italic angle, the global bounding box and the PostScript/family names.
//! It used to get them from `ttf-parser`, which meant compiling a second font
//! parser purely for those getters -- the renderer already links `skrifa`
//! (`pdf-font` uses it for outlines, metrics and system-font indexing), and
//! `skrifa` answers every one of them.
//!
//! This lives in `lege-pdf-read` rather than in the processor because that is
//! the crate that already owns the seam onto the renderer's dependencies;
//! nothing else in the processor needs to know `skrifa` exists.

use skrifa::MetadataProvider;
use skrifa::instance::{LocationRef, Size};
use skrifa::raw::TableProvider;
use skrifa::raw::types::NameId;

/// A font's global bounding box in design units (`head`'s `xMin`..`yMax`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FaceBBox {
    pub x_min: i16,
    pub y_min: i16,
    pub x_max: i16,
    pub y_max: i16,
}

/// The subset of a face's metadata a PDF font descriptor needs.
///
/// All distances are in font design units (that is, relative to
/// [`Self::units_per_em`]), which is what a `FontDescriptor` wants.
#[derive(Clone, Debug, PartialEq)]
pub struct FaceMetrics {
    pub units_per_em: u16,
    /// `maxp`'s glyph count.
    pub num_glyphs: u16,
    /// Baseline to the top of the alignment box. Positive.
    pub ascent: i16,
    /// Baseline to the bottom of the alignment box. Negative for normal fonts.
    pub descent: i16,
    /// `OS/2`'s cap height, falling back to [`Self::ascent`] when the table
    /// does not carry one (matching what the descriptor previously wrote).
    pub cap_height: i16,
    /// Counter-clockwise degrees from vertical; 0 for upright faces.
    pub italic_angle: f32,
    pub bbox: FaceBBox,
    /// name ID 6, when present.
    pub post_script_name: Option<String>,
    /// name ID 1, when present.
    pub family_name: Option<String>,
}

/// Round a design-unit measurement to the `i16` a font descriptor stores.
///
/// `skrifa` reports metrics as `f32` because it can scale them; at
/// [`Size::unscaled`] they are already integral design units, so this only
/// undoes the float representation. The clamp keeps a corrupt table from
/// producing a saturating cast that silently reads as a real metric.
fn to_design_units(value: f32) -> i16 {
    if !value.is_finite() {
        return 0;
    }
    value.round().clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// Read metadata for face `index` of a font file.
///
/// `index` selects a face inside a TrueType Collection; pass 0 for a plain
/// `.ttf`/`.otf`. Returns `None` when the bytes are not a font, or when the
/// collection has no such face -- a malformed file is never a panic.
pub fn read_face_metrics(data: &[u8], index: u32) -> Option<FaceMetrics> {
    let font = match skrifa::raw::FileRef::new(data).ok()? {
        skrifa::raw::FileRef::Font(font) => (index == 0).then_some(font)?,
        skrifa::raw::FileRef::Collection(collection) => collection.get(index).ok()?,
    };

    // `Size::unscaled` keeps everything in design units. `skrifa` picks the
    // vertical metrics the way FreeType does -- OS/2 typo metrics when the
    // face sets USE_TYPO_METRICS, `hhea` otherwise -- which is what a renderer
    // would use for the same face, and is strictly better than reading `hhea`
    // unconditionally as the old `ttf-parser` path did.
    let metrics = font.metrics(Size::unscaled(), LocationRef::default());

    let ascent = to_design_units(metrics.ascent);
    let bbox = metrics
        .bounds
        .map(|bounds| FaceBBox {
            x_min: to_design_units(bounds.x_min),
            y_min: to_design_units(bounds.y_min),
            x_max: to_design_units(bounds.x_max),
            y_max: to_design_units(bounds.y_max),
        })
        .unwrap_or_default();

    let name_of = |id: NameId| -> Option<String> {
        font.localized_strings(id)
            .english_or_first()
            .map(|name| name.to_string())
    };

    let units_per_em = match font.head() {
        Ok(head) => head.units_per_em(),
        Err(_) => metrics.units_per_em,
    };

    Some(FaceMetrics {
        units_per_em,
        num_glyphs: metrics.glyph_count,
        ascent,
        descent: to_design_units(metrics.descent),
        cap_height: metrics.cap_height.map(to_design_units).unwrap_or(ascent),
        italic_angle: metrics.italic_angle,
        bbox,
        post_script_name: name_of(NameId::POSTSCRIPT_NAME),
        family_name: name_of(NameId::FAMILY_NAME),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_is_rejected_without_panicking() {
        assert!(read_face_metrics(b"", 0).is_none());
        assert!(read_face_metrics(b"not a font at all", 0).is_none());
        assert!(read_face_metrics(&[0u8; 512], 0).is_none());
    }

    #[test]
    fn design_unit_rounding_is_clamped() {
        assert_eq!(to_design_units(0.0), 0);
        assert_eq!(to_design_units(-200.4), -200);
        assert_eq!(to_design_units(1000.6), 1001);
        assert_eq!(to_design_units(f32::NAN), 0);
        assert_eq!(to_design_units(1.0e9), i16::MAX);
        assert_eq!(to_design_units(-1.0e9), i16::MIN);
    }
}
