//! The 14 standard fonts and non-embedded font substitution (fonts.md Font
//! Phase 3).
//!
//! A PDF may name a font without embedding it — always for the standard 14
//! (ISO 32000-1 §9.6.2.2), and often for common system fonts like Arial. The
//! viewer must supply a face. This module answers one question: *given a
//! `/BaseFont` name and a descriptor, which bundled face do we draw?*
//!
//! # Bundled data
//! The faces are PDFium's Foxit fonts (`core/fxge/fontdata/chromefontdata`),
//! distributed by PDFium/Chromium under their BSD-style licence, converted
//! once from bare CFF into OpenType by `tools/foxit-fonts/extract.py` (Skrifa
//! reads SFNT only; PDFium hands bare CFF to FreeType). They are
//! metric-compatible with the standard 14, so their own advances double as
//! the standard-14 metrics when a font supplies no `/Widths`.
//!
//! Using PDFium's own faces is deliberate: this engine is a semantic port of
//! PDFium, so matching its substitution *and* its glyph shapes keeps
//! differential comparison meaningful.
//!
//! # Not bundled
//! The two Multiple Master faces are Type 1, which Skrifa cannot parse (Font
//! Phase 5). PDFium interpolates them for unknown fonts; we fall back to the
//! nearest of the standard 14 instead — deterministic, if less exact.

use std::sync::Arc;

use crate::engine::FontProgram;

/// One of the 14 standard faces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StandardFont {
    Helvetica,
    HelveticaBold,
    HelveticaOblique,
    HelveticaBoldOblique,
    Courier,
    CourierBold,
    CourierOblique,
    CourierBoldOblique,
    TimesRoman,
    TimesBold,
    TimesItalic,
    TimesBoldItalic,
    Symbol,
    ZapfDingbats,
}

/// The typeface family a substitution resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Sans,
    Serif,
    Fixed,
    Symbol,
    Dingbats,
}

impl StandardFont {
    /// Compose a face from a family and a style.
    pub fn new(family: Family, bold: bool, italic: bool) -> StandardFont {
        use StandardFont as F;
        match family {
            Family::Symbol => F::Symbol,
            Family::Dingbats => F::ZapfDingbats,
            Family::Sans => match (bold, italic) {
                (false, false) => F::Helvetica,
                (true, false) => F::HelveticaBold,
                (false, true) => F::HelveticaOblique,
                (true, true) => F::HelveticaBoldOblique,
            },
            Family::Serif => match (bold, italic) {
                (false, false) => F::TimesRoman,
                (true, false) => F::TimesBold,
                (false, true) => F::TimesItalic,
                (true, true) => F::TimesBoldItalic,
            },
            Family::Fixed => match (bold, italic) {
                (false, false) => F::Courier,
                (true, false) => F::CourierBold,
                (false, true) => F::CourierOblique,
                (true, true) => F::CourierBoldOblique,
            },
        }
    }

    pub fn family(self) -> Family {
        use StandardFont as F;
        match self {
            F::Helvetica | F::HelveticaBold | F::HelveticaOblique | F::HelveticaBoldOblique => {
                Family::Sans
            }
            F::TimesRoman | F::TimesBold | F::TimesItalic | F::TimesBoldItalic => Family::Serif,
            F::Courier | F::CourierBold | F::CourierOblique | F::CourierBoldOblique => {
                Family::Fixed
            }
            F::Symbol => Family::Symbol,
            F::ZapfDingbats => Family::Dingbats,
        }
    }

    pub fn is_bold(self) -> bool {
        use StandardFont as F;
        matches!(
            self,
            F::HelveticaBold
                | F::HelveticaBoldOblique
                | F::TimesBold
                | F::TimesBoldItalic
                | F::CourierBold
                | F::CourierBoldOblique
        )
    }

    pub fn is_italic(self) -> bool {
        use StandardFont as F;
        matches!(
            self,
            F::HelveticaOblique
                | F::HelveticaBoldOblique
                | F::TimesItalic
                | F::TimesBoldItalic
                | F::CourierOblique
                | F::CourierBoldOblique
        )
    }

    /// The face's canonical PDF name (`/BaseFont` spelling, Annex D).
    pub fn pdf_name(self) -> &'static str {
        use StandardFont as F;
        match self {
            F::Helvetica => "Helvetica",
            F::HelveticaBold => "Helvetica-Bold",
            F::HelveticaOblique => "Helvetica-Oblique",
            F::HelveticaBoldOblique => "Helvetica-BoldOblique",
            F::Courier => "Courier",
            F::CourierBold => "Courier-Bold",
            F::CourierOblique => "Courier-Oblique",
            F::CourierBoldOblique => "Courier-BoldOblique",
            F::TimesRoman => "Times-Roman",
            F::TimesBold => "Times-Bold",
            F::TimesItalic => "Times-Italic",
            F::TimesBoldItalic => "Times-BoldItalic",
            F::Symbol => "Symbol",
            F::ZapfDingbats => "ZapfDingbats",
        }
    }

    /// True for the two faces whose glyphs are not reachable through a
    /// meaningful Unicode cmap; they resolve by glyph name.
    pub fn is_symbolic(self) -> bool {
        matches!(self, StandardFont::Symbol | StandardFont::ZapfDingbats)
    }

    fn data(self) -> &'static [u8] {
        use StandardFont as F;
        match self {
            F::Helvetica => include_bytes!("../fonts/FoxitSans.otf"),
            F::HelveticaBold => include_bytes!("../fonts/FoxitSansBold.otf"),
            F::HelveticaOblique => include_bytes!("../fonts/FoxitSansItalic.otf"),
            F::HelveticaBoldOblique => include_bytes!("../fonts/FoxitSansBoldItalic.otf"),
            F::Courier => include_bytes!("../fonts/FoxitFixed.otf"),
            F::CourierBold => include_bytes!("../fonts/FoxitFixedBold.otf"),
            F::CourierOblique => include_bytes!("../fonts/FoxitFixedItalic.otf"),
            F::CourierBoldOblique => include_bytes!("../fonts/FoxitFixedBoldItalic.otf"),
            F::TimesRoman => include_bytes!("../fonts/FoxitSerif.otf"),
            F::TimesBold => include_bytes!("../fonts/FoxitSerifBold.otf"),
            F::TimesItalic => include_bytes!("../fonts/FoxitSerifItalic.otf"),
            F::TimesBoldItalic => include_bytes!("../fonts/FoxitSerifBoldItalic.otf"),
            F::Symbol => include_bytes!("../fonts/FoxitSymbol.otf"),
            F::ZapfDingbats => include_bytes!("../fonts/FoxitDingbats.otf"),
        }
    }

    /// The face's font program bytes.
    pub fn program_data(self) -> Arc<[u8]> {
        Arc::from(self.data())
    }

    /// Parse the face. Cheap enough to call per font resource; a worker-local
    /// cache is the documented optimization (advice §11).
    pub fn program(self) -> Option<FontProgram> {
        FontProgram::parse(self.program_data())
    }
}

/// What the PDF says about a font that needs substituting.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubstitutionRequest<'a> {
    /// `/BaseFont`, subset tag included (`ABCDEF+Arial,Bold`).
    pub base_font: &'a [u8],
    /// `/FontDescriptor /Flags`, if present (ISO 32000-1 table 123).
    pub flags: Option<u32>,
    /// `/FontDescriptor /StemV` — PDFium's weight hint.
    pub stem_v: Option<f64>,
    /// `/FontDescriptor /ItalicAngle`.
    pub italic_angle: Option<f64>,
}

/// `/Flags` bits this module reads (ISO 32000-1 table 123).
const FLAG_FIXED_PITCH: u32 = 1 << 0;
const FLAG_SERIF: u32 = 1 << 1;
const FLAG_SYMBOLIC: u32 = 1 << 2;
const FLAG_ITALIC: u32 = 1 << 6;
const FLAG_FORCE_BOLD: u32 = 1 << 18;

/// Strip a subset tag: `ABCDEF+Arial` → `Arial` (§9.6.4).
pub fn strip_subset_tag(name: &[u8]) -> &[u8] {
    if name.len() > 7 && name[6] == b'+' && name[..6].iter().all(|b| b.is_ascii_uppercase()) {
        &name[7..]
    } else {
        name
    }
}

/// Resolve a `/BaseFont` name that is spelled exactly as one of the standard
/// 14 (or PDFium's accepted aliases for them).
pub fn standard_by_name(name: &[u8]) -> Option<StandardFont> {
    use StandardFont as F;
    let name = strip_subset_tag(name);
    // PDFium's alias table (`core/fxge/cfx_fontmapper.cpp`) accepts the
    // canonical names, the common `,Style` and `-Style` spellings, and the
    // MS core-font names that stand in for them. Normalizing away case and
    // the separators collapses all of those spellings onto one key.
    let lower = name.to_ascii_lowercase();
    let normalized: Vec<u8> = lower
        .iter()
        .copied()
        .filter(|b| !b" -_,".contains(b))
        .collect();
    let s = normalized.as_slice();
    let f = match s {
        b"helvetica" | b"arial" | b"arialmt" | b"helv" => F::Helvetica,
        b"helveticabold" | b"arialbold" | b"arialboldmt" | b"arialbd" | b"helveticabd" => {
            F::HelveticaBold
        }
        b"helveticaoblique" | b"helveticaitalic" | b"arialitalic" | b"arialitalicmt" => {
            F::HelveticaOblique
        }
        b"helveticaboldoblique"
        | b"helveticabolditalic"
        | b"arialbolditalic"
        | b"arialbolditalicmt" => F::HelveticaBoldOblique,

        b"courier" | b"couriernew" | b"couriernewpsmt" | b"couriestd" | b"monospace" => F::Courier,
        b"courierbold" | b"couriernewbold" | b"couriernewpsboldmt" | b"couriernewboldmt" => {
            F::CourierBold
        }
        b"courieroblique" | b"courieritalic" | b"couriernewitalic" | b"couriernewpsitalicmt" => {
            F::CourierOblique
        }
        b"courierboldoblique"
        | b"courierbolditalic"
        | b"couriernewbolditalic"
        | b"couriernewpsbolditalicmt" => F::CourierBoldOblique,

        b"times" | b"timesroman" | b"timesnewroman" | b"timesnewromanpsmt" | b"timesnewromanps"
        | b"serif" => F::TimesRoman,
        b"timesbold" | b"timesnewromanbold" | b"timesnewromanpsboldmt" | b"timesnewromanpsbold" => {
            F::TimesBold
        }
        b"timesitalic"
        | b"timesnewromanitalic"
        | b"timesnewromanpsitalicmt"
        | b"timesnewromanpsitalic" => F::TimesItalic,
        b"timesbolditalic"
        | b"timesnewromanbolditalic"
        | b"timesnewromanpsbolditalicmt"
        | b"timesnewromanpsbolditalic" => F::TimesBoldItalic,

        b"symbol" | b"symbolmt" => F::Symbol,
        b"zapfdingbats" | b"dingbats" | b"wingdings" | b"wingdings2" | b"wingdings3" => {
            F::ZapfDingbats
        }
        _ => return None,
    };
    Some(f)
}

/// Choose a bundled face for a font the document did not embed.
///
/// Always succeeds: an unrecognized font resolves through its descriptor
/// flags and name to the nearest standard face, so text never silently
/// disappears (fonts.md: "deterministic generic fallback"). Mirrors
/// PDFium's `CFX_FontMapper::FindSubstFont` in spirit — exact alias first,
/// then family/style inference — minus the Multiple Master interpolation,
/// which needs Type 1 support.
pub fn substitute(request: SubstitutionRequest<'_>) -> StandardFont {
    substitute_with_style(request).0
}

/// [`substitute`], also reporting the *requested* style `(bold, italic)` —
/// which may exceed what the chosen face offers (the symbolic faces have no
/// bold/italic cuts), in which case [`synthesis`] says what to fake.
pub fn substitute_with_style(request: SubstitutionRequest<'_>) -> (StandardFont, bool, bool) {
    let name = strip_subset_tag(request.base_font);
    let lower = name.to_ascii_lowercase();
    let flags = request.flags.unwrap_or(0);
    let has = |needle: &[u8]| lower.windows(needle.len()).any(|w| w == needle);
    let want_bold = has(b"bold")
        || has(b"black")
        || has(b"heavy")
        || flags & FLAG_FORCE_BOLD != 0
        || request.stem_v.is_some_and(|v| v > 120.0);
    let want_italic = has(b"italic")
        || has(b"oblique")
        || flags & FLAG_ITALIC != 0
        || request.italic_angle.is_some_and(|a| a != 0.0);

    // 1. An exact standard name (or PDFium alias) settles both family and style.
    if let Some(exact) = standard_by_name(name) {
        return (
            exact,
            want_bold || exact.is_bold(),
            want_italic || exact.is_italic(),
        );
    }

    // 2. Style from the name's style words, then the descriptor
    //    (`want_bold`/`want_italic` above — PDFium treats a heavy stem as
    //    bold when the name is silent).
    let (bold, italic) = (want_bold, want_italic);

    // 3. Family from the name, then the flags. A symbolic font with no other
    //    signal keeps Helvetica rather than a symbol face: `/Symbolic` is set
    //    by countless ordinary subset fonts, and guessing Dingbats there
    //    would replace text with ornaments.
    let family = if has(b"courier") || has(b"mono") || flags & FLAG_FIXED_PITCH != 0 {
        Family::Fixed
    } else if has(b"zapf") || has(b"dingbat") {
        Family::Dingbats
    } else if has(b"symbol") {
        Family::Symbol
    } else if has(b"times")
        || has(b"georgia")
        || has(b"book")
        || has(b"roman")
        || has(b"garamond")
        || has(b"minion")
        || has(b"serif") && !has(b"sans")
        || (flags & FLAG_SERIF != 0 && flags & FLAG_SYMBOLIC == 0)
    {
        Family::Serif
    } else {
        Family::Sans
    };

    (StandardFont::new(family, bold, italic), bold, italic)
}

/// Synthetic styling the chosen face cannot supply itself.
///
/// The bundled 12 text faces cover all four styles, so this only arises for
/// the symbolic faces (no bold/italic cut exists) — PDFium slants and
/// emboldens them synthetically.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Synthesis {
    /// Shear to apply in text space for a missing italic cut, as `tan(angle)`.
    pub oblique_shear: f32,
    /// A missing bold cut wants emboldening (stroke-widen the outline).
    pub embolden: bool,
}

/// The standard oblique angle PDFium uses when slanting a face (12°).
pub const SYNTHETIC_OBLIQUE_TAN: f32 = 0.212_556_6;

/// What must be synthesized to present `chosen` as the requested style.
pub fn synthesis(chosen: StandardFont, want_bold: bool, want_italic: bool) -> Synthesis {
    Synthesis {
        oblique_shear: if want_italic && !chosen.is_italic() {
            SYNTHETIC_OBLIQUE_TAN
        } else {
            0.0
        },
        embolden: want_bold && !chosen.is_bold(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn subset_tags_are_stripped() {
        assert_eq!(strip_subset_tag(b"ABCDEF+Arial"), b"Arial");
        assert_eq!(strip_subset_tag(b"Arial"), b"Arial");
        // Only six uppercase letters then '+' is a subset tag.
        assert_eq!(strip_subset_tag(b"Abc+Arial"), b"Abc+Arial");
    }

    #[test]
    fn standard_names_and_pdfium_aliases_resolve() {
        for (name, want) in [
            (&b"Helvetica"[..], StandardFont::Helvetica),
            (b"Arial", StandardFont::Helvetica),
            (b"Arial,Bold", StandardFont::HelveticaBold),
            (b"Arial-BoldMT", StandardFont::HelveticaBold),
            (b"ArialMT", StandardFont::Helvetica),
            (b"Times-Roman", StandardFont::TimesRoman),
            (b"TimesNewRomanPSMT", StandardFont::TimesRoman),
            (
                b"TimesNewRomanPS-BoldItalicMT",
                StandardFont::TimesBoldItalic,
            ),
            (b"Courier", StandardFont::Courier),
            (b"CourierNewPS-BoldMT", StandardFont::CourierBold),
            (b"Symbol", StandardFont::Symbol),
            (b"ZapfDingbats", StandardFont::ZapfDingbats),
            (
                b"ABCDEF+Arial,BoldItalic",
                StandardFont::HelveticaBoldOblique,
            ),
        ] {
            assert_eq!(
                standard_by_name(name),
                Some(want),
                "{}",
                String::from_utf8_lossy(name)
            );
        }
        assert_eq!(standard_by_name(b"NotAFont"), None);
    }

    #[test]
    fn unknown_fonts_fall_back_by_name_and_flags() {
        let by_name = |n: &[u8]| {
            substitute(SubstitutionRequest {
                base_font: n,
                ..Default::default()
            })
        };
        assert_eq!(by_name(b"Verdana"), StandardFont::Helvetica);
        assert_eq!(by_name(b"Verdana-Bold"), StandardFont::HelveticaBold);
        assert_eq!(by_name(b"Garamond-Italic"), StandardFont::TimesItalic);
        assert_eq!(by_name(b"Consolas"), StandardFont::Helvetica); // no fixed signal in the name
        assert_eq!(
            by_name(b"SomeMono-BoldItalic"),
            StandardFont::CourierBoldOblique
        );

        // Flags carry the family when the name is opaque.
        let fixed = substitute(SubstitutionRequest {
            base_font: b"XYZQ",
            flags: Some(FLAG_FIXED_PITCH),
            ..Default::default()
        });
        assert_eq!(fixed, StandardFont::Courier);
        let serif = substitute(SubstitutionRequest {
            base_font: b"XYZQ",
            flags: Some(FLAG_SERIF),
            ..Default::default()
        });
        assert_eq!(serif, StandardFont::TimesRoman);
    }

    #[test]
    fn descriptor_supplies_style_when_the_name_does_not() {
        let f = substitute(SubstitutionRequest {
            base_font: b"OpaqueName",
            flags: None,
            stem_v: Some(160.0),
            italic_angle: Some(-12.0),
        });
        assert_eq!(f, StandardFont::HelveticaBoldOblique);
    }

    #[test]
    fn symbolic_flag_alone_does_not_pick_a_symbol_face() {
        // Subset fonts set /Symbolic constantly; picking Dingbats here would
        // turn text into ornaments.
        let f = substitute(SubstitutionRequest {
            base_font: b"KLMNOP+SomeSubsetFont",
            flags: Some(FLAG_SYMBOLIC),
            ..Default::default()
        });
        assert_eq!(f.family(), Family::Sans);
    }

    #[test]
    fn every_bundled_face_parses_and_has_glyphs() {
        for f in [
            StandardFont::Helvetica,
            StandardFont::HelveticaBold,
            StandardFont::HelveticaOblique,
            StandardFont::HelveticaBoldOblique,
            StandardFont::Courier,
            StandardFont::CourierBold,
            StandardFont::CourierOblique,
            StandardFont::CourierBoldOblique,
            StandardFont::TimesRoman,
            StandardFont::TimesBold,
            StandardFont::TimesItalic,
            StandardFont::TimesBoldItalic,
            StandardFont::Symbol,
            StandardFont::ZapfDingbats,
        ] {
            let prog = f
                .program()
                .unwrap_or_else(|| panic!("{} must parse", f.pdf_name()));
            assert_eq!(prog.units_per_em(), 1000, "{}", f.pdf_name());
            assert!(prog.num_glyphs() > 100, "{}", f.pdf_name());
        }
    }

    #[test]
    fn text_faces_resolve_letters_and_symbolic_faces_resolve_names() {
        let helv = StandardFont::Helvetica.program().unwrap();
        let gid = helv.gid_for_char('A').expect("Helvetica has 'A'");
        assert!(helv.outline(gid).is_some(), "'A' has an outline");
        // Metric-compatible with the standard 14: Helvetica 'A' is 667/1000.
        assert_eq!(helv.advance(gid), Some(667.0));

        // Symbol has no Latin 'A'; its glyphs answer to names.
        let symbol = StandardFont::Symbol.program().unwrap();
        let alpha = symbol.gid_for_name(b"alpha").expect("Symbol has 'alpha'");
        assert!(symbol.outline(alpha).is_some());
    }

    #[test]
    fn synthesis_is_only_needed_for_the_symbolic_faces() {
        // The text families ship all four cuts.
        assert_eq!(
            synthesis(StandardFont::HelveticaBoldOblique, true, true),
            Synthesis::default()
        );
        assert_eq!(
            synthesis(StandardFont::TimesRoman, false, false),
            Synthesis::default()
        );
        // Symbol has no italic cut, so slant it.
        let s = synthesis(StandardFont::Symbol, false, true);
        assert!(s.oblique_shear > 0.0 && !s.embolden);
        let b = synthesis(StandardFont::ZapfDingbats, true, false);
        assert!(b.embolden);
    }
}
