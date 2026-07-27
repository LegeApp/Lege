//! The native OpenType outline engine (fonts.md Font Phase 2), backed by
//! Fontations/Skrifa. **Skrifa types never leave this module** — callers see
//! only [`FontProgram`], [`Outline`], and [`OutlineVerb`], so the engine is
//! replaceable (a future FreeType compatibility face, a native Type 1 engine)
//! without touching the rest of the renderer.
//!
//! Outlines are emitted in **font design units** (unscaled); the caller scales
//! by `font_size / units_per_em`. This is the unhinted path — correct glyph
//! geometry at document resolutions; hinting is a later font phase.

use std::sync::Arc;

use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, HintingInstance, HintingOptions, OutlinePen};
use skrifa::raw::TableProvider;
use skrifa::{FontRef, GlyphId, MetadataProvider};

/// When to grid-fit glyph outlines (fonts.md Font Phase 4).
///
/// Hinting trades fidelity for crispness: it snaps stems and edges to the
/// pixel grid, which helps at small sizes on screen and *hurts* at document
/// resolutions, where it distorts shapes and breaks the metric agreement
/// between the PDF's `/Widths` and the drawn glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HintingPolicy {
    /// Never hint. Outlines are exactly what the font describes.
    #[default]
    None,
    /// Use the font's own hinting program (TrueType bytecode / CFF hints),
    /// falling back to the auto-hinter when a font carries none.
    Embedded,
    /// Hint only where it pays: below [`AUTO_HINT_MAX_PPEM`], and only for
    /// axis-aligned text (the caller decides the latter — see
    /// `should_hint`). Above the threshold the outline is unhinted.
    Auto,
}

/// The pixels-per-em above which [`HintingPolicy::Auto`] stops hinting.
///
/// Hinting exists to rescue stems that are a pixel or two wide. By ~50px/em
/// a stem spans several pixels and anti-aliasing alone resolves it, while
/// grid-fitting still distorts the shape — so past this size the unhinted
/// outline is both faster and more faithful. Print-resolution rendering
/// (a 10pt glyph at 300dpi is ~42px/em, at 600dpi ~83px/em) therefore sits
/// at or above the threshold by design.
pub const AUTO_HINT_MAX_PPEM: f32 = 50.0;

impl HintingPolicy {
    /// Whether to hint a glyph rendered at `ppem` pixels per em under an
    /// `axis_aligned` transform.
    ///
    /// Rotated or skewed text is never hinted: grid-fitting is defined
    /// against the pixel grid, so it is meaningless once the glyph is not
    /// aligned to it (fonts.md §4 makes the same point about caching).
    pub fn should_hint(self, ppem: f32, axis_aligned: bool) -> bool {
        // A degenerate or non-finite ppem has no grid to fit to.
        if !axis_aligned || !ppem.is_finite() || ppem <= 0.0 {
            return false;
        }
        match self {
            HintingPolicy::None => false,
            HintingPolicy::Embedded => true,
            HintingPolicy::Auto => ppem <= AUTO_HINT_MAX_PPEM,
        }
    }
}

/// A path segment verb in a glyph outline. Quadratics (TrueType) and cubics
/// (CFF) are both preserved; the caller flattens them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlineVerb {
    /// 1 point.
    MoveTo,
    /// 1 point.
    LineTo,
    /// 2 points (control, end) — quadratic Bézier.
    QuadTo,
    /// 3 points (control1, control2, end) — cubic Bézier.
    CurveTo,
    /// 0 points.
    Close,
}

/// A glyph outline in font design units, verb/point split (matching the IR's
/// path layout).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outline {
    pub verbs: Vec<OutlineVerb>,
    pub points: Vec<[f32; 2]>,
}

impl Outline {
    pub fn is_empty(&self) -> bool {
        self.verbs.is_empty()
    }

    /// Axis-aligned control bounds in font design units.
    ///
    /// Font outline tables define their glyph boxes from the outline points;
    /// retaining this accessor keeps text geometry independent of raster
    /// flattening.
    pub fn bounds(&self) -> Option<[f32; 4]> {
        let first = *self.points.first()?;
        let mut bounds = [first[0], first[1], first[0], first[1]];
        for &[x, y] in &self.points[1..] {
            bounds[0] = bounds[0].min(x);
            bounds[1] = bounds[1].min(y);
            bounds[2] = bounds[2].max(x);
            bounds[3] = bounds[3].max(y);
        }
        Some(bounds)
    }

    /// Synthetic oblique: shear every point in font space (`x += shear·y`),
    /// the 12° slant PDFium applies for a missing italic cut.
    pub fn oblique(&mut self, shear: f32) {
        if shear == 0.0 {
            return;
        }
        for p in &mut self.points {
            p[0] += shear * p[1];
        }
    }

    /// Synthetic bold: grow the outline by shifting every point (on- and
    /// off-curve alike, as `FT_Outline_Embolden` does) `strength / 2`
    /// outward along its angle-bisector normal, in font units.
    ///
    /// "Outward" is resolved once from the dominant contour's winding: fill
    /// sits on a consistent side of travel in a well-formed font (outer and
    /// hole contours wind oppositely), so a single perpendicular choice
    /// grows the outer boundary *and* shrinks holes — the emboldening
    /// PDFium gets from `FT_Outline_Embolden` (`cfx_face.cpp`,
    /// `kWeightPow`).
    pub fn embolden(&mut self, strength: f32) {
        if strength <= 0.0 || self.points.is_empty() {
            return;
        }
        // Split points into contours at each MoveTo.
        let mut contours: Vec<(usize, usize)> = Vec::new(); // [start, end)
        let mut start = 0usize;
        let mut pt = 0usize;
        for verb in &self.verbs {
            let step = match verb {
                OutlineVerb::MoveTo => {
                    if pt > start {
                        contours.push((start, pt));
                    }
                    start = pt;
                    1
                }
                OutlineVerb::LineTo => 1,
                OutlineVerb::QuadTo => 2,
                OutlineVerb::CurveTo => 3,
                OutlineVerb::Close => 0,
            };
            pt += step;
        }
        if pt > start {
            contours.push((start, pt.min(self.points.len())));
        }

        // Dominant winding: signed area of the largest contour.
        let mut best_area = 0.0f64;
        for &(s, e) in &contours {
            let mut area = 0.0f64;
            for i in s..e {
                let a = self.points[i];
                let b = self.points[if i + 1 == e { s } else { i + 1 }];
                area += a[0] as f64 * b[1] as f64 - b[0] as f64 * a[1] as f64;
            }
            if area.abs() > best_area.abs() {
                best_area = area;
            }
        }
        // CCW outer (area > 0): fill is left of travel → outward is right
        // (rotate direction by -90°). CW outer: the mirror.
        let sign: f32 = if best_area >= 0.0 { 1.0 } else { -1.0 };
        let half = strength * 0.5;

        let old = self.points.clone();
        for &(s, e) in &contours {
            let n = e - s;
            if n < 2 {
                continue;
            }
            for i in 0..n {
                let cur = old[s + i];
                // Nearest distinct neighbors (wrap), so duplicated points do
                // not produce zero-length edge normals.
                let mut prev = cur;
                for k in 1..n {
                    let c = old[s + (i + n - k) % n];
                    if c != cur {
                        prev = c;
                        break;
                    }
                }
                let mut next = cur;
                for k in 1..n {
                    let c = old[s + (i + k) % n];
                    if c != cur {
                        next = c;
                        break;
                    }
                }
                if prev == cur || next == cur {
                    continue; // degenerate contour of identical points
                }
                let norm = |d: [f32; 2]| -> Option<[f32; 2]> {
                    let l = (d[0] * d[0] + d[1] * d[1]).sqrt();
                    (l > 1e-6).then(|| [d[0] / l, d[1] / l])
                };
                let Some(din) = norm([cur[0] - prev[0], cur[1] - prev[1]]) else {
                    continue;
                };
                let Some(dout) = norm([next[0] - cur[0], next[1] - cur[1]]) else {
                    continue;
                };
                // Outward normals of the adjacent edges (right of travel for
                // CCW fill-left, mirrored otherwise).
                let n_in = [sign * din[1], -sign * din[0]];
                let n_out = [sign * dout[1], -sign * dout[0]];
                let bis = [n_in[0] + n_out[0], n_in[1] + n_out[1]];
                let l = (bis[0] * bis[0] + bis[1] * bis[1]).sqrt();
                let (dir, dist) = if l > 1e-3 {
                    // Miter along the bisector: |offset| = half / cos(θ/2),
                    // capped to 2× to keep spikes bounded (FreeType clamps
                    // similarly).
                    ([bis[0] / l, bis[1] / l], (half * 2.0 / l).min(half * 2.0))
                } else {
                    // 180° reversal: fall back to the incoming edge normal.
                    (n_in, half)
                };
                self.points[s + i][0] += dir[0] * dist;
                self.points[s + i][1] += dir[1] * dist;
            }
        }
    }
}

impl OutlinePen for Outline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.verbs.push(OutlineVerb::MoveTo);
        self.points.push([x, y]);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.verbs.push(OutlineVerb::LineTo);
        self.points.push([x, y]);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.verbs.push(OutlineVerb::QuadTo);
        self.points.push([cx0, cy0]);
        self.points.push([x, y]);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.verbs.push(OutlineVerb::CurveTo);
        self.points.push([cx0, cy0]);
        self.points.push([cx1, cy1]);
        self.points.push([x, y]);
    }
    fn close(&mut self) {
        self.verbs.push(OutlineVerb::Close);
    }
}

/// An immutable, shareable parsed font program (fonts.md §11 "Shared
/// FontProgram"). Holds the bytes plus a couple of cached scalars; the Skrifa
/// `FontRef` (which borrows the bytes) is reconstructed transiently per query —
/// a worker-local reusable face is a later optimization.
#[derive(Debug, Clone)]
pub struct FontProgram {
    data: Arc<[u8]>,
    units_per_em: u16,
    num_glyphs: u16,
    kind: ProgramKind,
    /// Lazily-built `post` name → gid index (lowest gid wins), shared across
    /// clones. Replaces the former per-lookup linear scan over every glyph,
    /// which made `/Differences` resolution O(names × glyphs) on large CJK
    /// faces.
    name_index: Arc<std::sync::OnceLock<std::collections::HashMap<Box<[u8]>, u32>>>,
    /// Lazily-built glyph-id -> Unicode scalar map: the font's cmap
    /// reversed. The last-resort code->Unicode fallback for a simple font
    /// that names no usable encoding and has no `/ToUnicode`
    /// (PLAN-TEXT-EXTRACTION §5.2 step 4). Shared across clones.
    gid_to_char: Arc<std::sync::OnceLock<std::collections::HashMap<u32, char>>>,
}

/// Which engine backs a program. Type 1 is not an SFNT, so Skrifa cannot
/// read it; it gets the native interpreter (fonts.md Font Phase 5). Callers
/// never see this — `FontProgram`'s API is identical either way.
#[derive(Debug, Clone)]
enum ProgramKind {
    /// TrueType `glyf`, CFF, CFF2 — anything Skrifa parses. `index` selects
    /// the face inside a collection (0 for a plain font).
    Sfnt { index: u32, wrapped_bare_cff: bool },
    /// A bare Type 1 `/FontFile`.
    Type1(crate::type1::SharedType1),
}

/// Whether an SFNT `head.unitsPerEm` is in the valid range. OpenType 1.8.2
/// requires 16..=16384 and FreeType rejects anything outside it as an invalid
/// `head` table (`sfnt/sfobjs.c`); Skrifa, by contrast, accepts a corrupt value
/// verbatim. A bogus (usually far-too-small) em would scale the design-unit
/// outlines by `font_size / units_per_em` into enormous glyphs — a single
/// character flooding the page. Rejecting the program here routes the font
/// through substitution, matching MuPDF/PDFium (both FreeType-backed), which
/// discard the embedded face and fall back to a system font.
fn valid_units_per_em(upem: u16) -> bool {
    (16..=16384).contains(&upem)
}

impl FontProgram {
    /// Parse an embedded/substitute font program (TrueType `glyf`, CFF, CFF2,
    /// or an OpenType wrapper). Returns `None` if Skrifa cannot parse it.
    pub fn parse(data: Arc<[u8]>) -> Option<Self> {
        Self::parse_indexed(data, 0)
    }

    /// Parse face `index` of a font *collection* (`.ttc`/`.otc`), or a plain
    /// font when `index` is 0.
    ///
    /// Collections are how the big CJK families ship (one file, one face per
    /// language), so a system font provider must be able to name a face
    /// inside one — a collection's bytes are not a font on their own and
    /// `FontRef::new` rejects them.
    pub fn parse_indexed(data: Arc<[u8]>, index: u32) -> Option<Self> {
        // Borrow `data` only inside this scope so it can be moved after.
        let sfnt = Self::font_ref_at(&data, index).and_then(|font| {
            let upem = font
                .metrics(Size::unscaled(), LocationRef::default())
                .units_per_em;
            Some((upem, font.maxp().ok()?.num_glyphs()))
        });
        if let Some((units_per_em, num_glyphs)) = sfnt.filter(|&(upem, _)| valid_units_per_em(upem))
        {
            return Some(Self {
                data,
                units_per_em,
                num_glyphs,
                kind: ProgramKind::Sfnt {
                    index,
                    wrapped_bare_cff: false,
                },
                name_index: Arc::default(),
                gid_to_char: Arc::default(),
            });
        }
        // A bare CFF (`/FontFile3` Type1C / CIDFontType0C) is not an SFNT, so
        // Skrifa cannot read it as-is. Wrap it and use the result — the CFF
        // bytes are unchanged, only described.
        if crate::cff::is_bare_cff(&data)
            && let Some(wrapped) = crate::cff::wrap_bare_cff(&data)
        {
            let wrapped: Arc<[u8]> = Arc::from(wrapped);
            let info = Self::font_ref_at(&wrapped, 0).and_then(|font| {
                let upem = font
                    .metrics(Size::unscaled(), LocationRef::default())
                    .units_per_em;
                Some((upem, font.maxp().ok()?.num_glyphs()))
            });
            if let Some((units_per_em, num_glyphs)) =
                info.filter(|&(upem, _)| valid_units_per_em(upem))
            {
                return Some(Self {
                    data: wrapped,
                    units_per_em,
                    num_glyphs,
                    kind: ProgramKind::Sfnt {
                        index: 0,
                        wrapped_bare_cff: true,
                    },
                    name_index: Arc::default(),
                    gid_to_char: Arc::default(),
                });
            }
        }
        // Not an SFNT: try the native Type 1 engine before giving up.
        let t1 = crate::type1::Type1Font::parse(&data)?;
        Some(Self {
            units_per_em: t1.units_per_em(),
            num_glyphs: t1.num_glyphs(),
            kind: ProgramKind::Type1(Arc::new(t1)),
            data,
            name_index: Arc::default(),
            gid_to_char: Arc::default(),
        })
    }

    /// True when this program is a bare Type 1 font handled by the native
    /// interpreter rather than Skrifa.
    pub fn is_type1(&self) -> bool {
        matches!(self.kind, ProgramKind::Type1(_))
    }

    /// Retained backing-byte size of this parsed program, for charging a
    /// bounded document-scoped parse cache. This is the SFNT/wrapped/Type 1
    /// byte buffer the program holds; parsed side structures (e.g. a Type 1
    /// interpreter) are a bounded multiple of it, so it is a stable proxy.
    pub fn retained_bytes(&self) -> usize {
        self.data.len()
    }

    /// Whether reparsing this program is expensive enough to retain for an
    /// immediate repeat render of the same compiled page.
    pub fn benefits_from_parse_cache(&self) -> bool {
        matches!(
            self.kind,
            ProgramKind::Type1(_)
                | ProgramKind::Sfnt {
                    wrapped_bare_cff: true,
                    ..
                }
        )
    }

    /// Resolve face `index` out of `data`, transparently handling both a
    /// plain font and a collection.
    fn font_ref_at(data: &[u8], index: u32) -> Option<FontRef<'_>> {
        match skrifa::raw::FileRef::new(data).ok()? {
            skrifa::raw::FileRef::Font(f) => (index == 0).then_some(f),
            skrifa::raw::FileRef::Collection(c) => c.get(index).ok(),
        }
    }

    /// This program's face.
    fn font_ref(&self) -> Option<FontRef<'_>> {
        let index = match self.kind {
            ProgramKind::Sfnt { index, .. } => index,
            ProgramKind::Type1(_) => return None,
        };
        Self::font_ref_at(&self.data, index)
    }

    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }

    pub fn num_glyphs(&self) -> u16 {
        self.num_glyphs
    }

    /// The unhinted outline of `glyph_id` in font design units, or `None` if
    /// the glyph is absent or empty (e.g. a space).
    pub fn outline(&self, glyph_id: u32) -> Option<Outline> {
        if let ProgramKind::Type1(t1) = &self.kind {
            return t1.outline(glyph_id);
        }
        let font = self.font_ref()?;
        Self::outline_from(&font.outline_glyphs(), glyph_id)
    }

    /// Extract the outlines of many glyph ids in one `FontRef` parse — the
    /// per-glyph-run entry point (amortizes table setup over the run).
    pub fn outlines(&self, glyph_ids: &[u32]) -> Vec<Option<Outline>> {
        if let ProgramKind::Type1(t1) = &self.kind {
            return glyph_ids.iter().map(|&g| t1.outline(g)).collect();
        }
        let Some(font) = self.font_ref() else {
            return vec![None; glyph_ids.len()];
        };
        let glyphs = font.outline_glyphs();
        glyph_ids
            .iter()
            .map(|&g| Self::outline_from(&glyphs, g))
            .collect()
    }

    /// Extract a run's outlines **grid-fitted for `ppem`**, in pixel units.
    ///
    /// Unlike [`Self::outlines`], the result is already scaled: hinting is
    /// defined against a specific pixel grid, so the caller must place these
    /// with translation only and must not scale them again. One
    /// [`HintingInstance`] is built for the whole run — it is the expensive
    /// part (it executes the font's `prep`/`cvt` programs), so it must never
    /// be rebuilt per glyph.
    ///
    /// Returns `None` if a hinting instance cannot be built for this font and
    /// size, so callers can fall back to the unhinted path.
    pub fn outlines_hinted(&self, glyph_ids: &[u32], ppem: f32) -> Option<Vec<Option<Outline>>> {
        // The Type 1 interpreter is unhinted (fonts.md Font Phase 5); `None`
        // sends the caller to the exact outline path.
        if matches!(self.kind, ProgramKind::Type1(_)) {
            return None;
        }
        let font = self.font_ref()?;
        let glyphs = font.outline_glyphs();
        let size = Size::new(ppem);
        let instance = HintingInstance::new(
            &glyphs,
            size,
            LocationRef::default(),
            // AutoFallback: use the font's own bytecode when it has any,
            // else the auto-hinter — which is what `Embedded` means, and
            // what FreeType/PDFium do by default.
            HintingOptions::default(),
        )
        .ok()?;
        Some(
            glyph_ids
                .iter()
                .map(|&g| {
                    let glyph = glyphs.get(GlyphId::new(g))?;
                    let mut outline = Outline::default();
                    glyph
                        .draw(DrawSettings::hinted(&instance, false), &mut outline)
                        .ok()?;
                    (!outline.is_empty()).then_some(outline)
                })
                .collect(),
        )
    }

    /// The glyph id for a Unicode scalar via the font's cmap (simple-font
    /// encoding resolution). When Skrifa's selected Unicode charmap does not
    /// cover the scalar, map Unicode → Mac OS Roman and consult byte-oriented
    /// format 0/6 subtables directly. This covers Macintosh-only Office
    /// subsets while still allowing malformed Microsoft format-6 subsets.
    pub fn gid_for_char(&self, c: char) -> Option<u32> {
        if let ProgramKind::Type1(t1) = &self.kind {
            return t1.gid_for_char(c);
        }
        let font = self.font_ref()?;
        font.charmap().map(c).map(|g| g.to_u32()).or_else(|| {
            let code = crate::encoding::macroman_code_for_char(c)?;
            self.native_byte_cmap_gid(code, true)
        })
    }

    /// The Unicode scalar the font's cmap assigns to `glyph_id` — the cmap
    /// reversed. This is the last-resort code->Unicode fallback for a simple
    /// font that names no usable encoding and carries no `/ToUnicode`
    /// (PLAN-TEXT-EXTRACTION §5.2 step 4): the caller resolves code->gid
    /// through the font's encoding, then this recovers the Unicode identity.
    ///
    /// A `(3,0)` MS-Symbol cmap addresses glyphs in the private-use range
    /// `0xF000..=0xF0FF`; those are folded to their low byte, so an Office
    /// "symbolic" Latin subset (the common `/Flags` bit-3 mislabel) still
    /// recovers ASCII, matching PDFium's `0xF000` stripping.
    pub fn char_for_gid(&self, glyph_id: u32) -> Option<char> {
        if glyph_id == 0 {
            return None;
        }
        let map = self.gid_to_char.get_or_init(|| {
            let mut map: std::collections::HashMap<u32, char> = std::collections::HashMap::new();
            let Some(font) = self.font_ref() else {
                return map;
            };
            for (codepoint, gid) in font.charmap().mappings() {
                let gid = gid.to_u32();
                if gid == 0 {
                    continue;
                }
                let scalar = if (0xF000..=0xF0FF).contains(&codepoint) {
                    codepoint - 0xF000
                } else {
                    codepoint
                };
                // C0/C1 controls are never a real text identity.
                if scalar < 0x20 || (0x7F..=0x9F).contains(&scalar) {
                    continue;
                }
                let Some(ch) = char::from_u32(scalar) else {
                    continue;
                };
                match map.get(&gid) {
                    Some(&existing) if !reverse_char_is_better(ch, existing) => {}
                    _ => {
                        map.insert(gid, ch);
                    }
                }
            }
            map
        });
        map.get(&glyph_id).copied()
    }

    /// The glyph id for a PostScript glyph name, via the `post` table.
    ///
    /// The authoritative route for a `/Differences` entry and for the
    /// symbolic standard faces (Symbol, ZapfDingbats), whose glyphs are
    /// named rather than usefully reachable through Unicode. Only `post`
    /// format 2.0 carries names; other formats yield `None`.
    pub fn gid_for_name(&self, name: &[u8]) -> Option<u32> {
        if let ProgramKind::Type1(t1) = &self.kind {
            return t1.gid_for_name(name);
        }
        // Built once per program (shared across clones), then every lookup
        // is a hash probe. Lowest gid wins on duplicate names, preserving
        // the scan's first-match semantics.
        let index = self.name_index.get_or_init(|| {
            let mut map: std::collections::HashMap<Box<[u8]>, u32> =
                std::collections::HashMap::new();
            if let Some(font) = self.font_ref()
                && let Ok(post) = font.post()
            {
                for gid in 0..self.num_glyphs {
                    if let Some(n) = post.glyph_name(skrifa::raw::types::GlyphId16::new(gid)) {
                        map.entry(n.as_bytes().into()).or_insert(gid as u32);
                    }
                }
            }
            map
        });
        index.get(name).copied()
    }

    /// The advance width of `glyph_id` in font design units.
    ///
    /// The fallback when a simple font supplies no `/Widths` (ISO 32000-1
    /// §9.6.2.1: the standard-14 metrics apply) — the bundled faces are
    /// metric-compatible with the standard fonts, so their own advances
    /// *are* those metrics.
    pub fn advance(&self, glyph_id: u32) -> Option<f32> {
        if let ProgramKind::Type1(t1) = &self.kind {
            return t1.advance(glyph_id);
        }
        let font = self.font_ref()?;
        font.glyph_metrics(Size::unscaled(), LocationRef::default())
            .advance_width(GlyphId::new(glyph_id))
    }

    /// The glyph id for a raw byte code via a symbol cmap: tries the code
    /// directly, then the `0xF000` symbol range.
    pub fn gid_for_code(&self, code: u8) -> Option<u32> {
        // Type 1 carries its own /Encoding: that IS the built-in encoding a
        // symbolic simple font expects.
        if let ProgramKind::Type1(t1) = &self.kind {
            return t1.gid_for_code(code);
        }
        let font = self.font_ref()?;
        let cm = font.charmap();
        cm.map(code as u32)
            .or_else(|| cm.map(0xF000 | code as u32))
            .map(|g| g.to_u32())
            // Some embedded subsets use a byte-oriented format 0/6 cmap even
            // under Microsoft (3,1). The raw PDF code indexes that table.
            .or_else(|| self.native_byte_cmap_gid(code, false))
    }

    /// Native lookup in byte-oriented cmap formats 0 and 6. `prefer_mac` is
    /// used by the Unicode→MacRoman bridge; raw PDF character codes prefer a
    /// Microsoft (3,1) table, as found in pdf.js issue5701.
    fn native_byte_cmap_gid(&self, code: u8, prefer_mac: bool) -> Option<u32> {
        let data: &[u8] = &self.data;
        let u16at = |o: usize| -> Option<u32> {
            Some(((*data.get(o)? as u32) << 8) | *data.get(o + 1)? as u32)
        };
        let u32at = |o: usize| -> Option<u32> { Some((u16at(o)? << 16) | u16at(o + 2)?) };
        // Resolve the sfnt table directory (through a `ttcf` header for the
        // collection case).
        let mut base = 0usize;
        if data.get(0..4)? == b"ttcf" {
            let index = match self.kind {
                ProgramKind::Sfnt { index, .. } => index as usize,
                ProgramKind::Type1(_) => return None,
            };
            base = u32at(12 + 4 * index)? as usize;
        }
        let tag = data.get(base..base + 4)?;
        if tag != [0x00, 0x01, 0x00, 0x00] && tag != *b"true" && tag != *b"OTTO" {
            return None;
        }
        let num_tables = u16at(base + 4)? as usize;
        let mut cmap = None;
        for i in 0..num_tables {
            let rec = base + 12 + 16 * i;
            if data.get(rec..rec + 4)? == b"cmap" {
                cmap = Some(u32at(rec + 8)? as usize);
                break;
            }
        }
        let cmap = cmap?;
        let n_sub = u16at(cmap + 2)? as usize;
        let priorities = if prefer_mac {
            [(1, 0), (3, 1), (3, 10), (0, 3), (0, 4), (0, 0)]
        } else {
            [(3, 1), (3, 10), (1, 0), (0, 3), (0, 4), (0, 0)]
        };
        for (platform, encoding) in priorities {
            for i in 0..n_sub {
                let rec = cmap + 4 + 8 * i;
                if u16at(rec)? != platform || u16at(rec + 2)? != encoding {
                    continue;
                }
                let sub = cmap + u32at(rec + 4)? as usize;
                let gid = match u16at(sub)? {
                    0 => *data.get(sub + 6 + code as usize)? as u32,
                    6 => {
                        let first = u16at(sub + 6)?;
                        let count = u16at(sub + 8)?;
                        let c = code as u32;
                        if c < first || c >= first + count {
                            continue;
                        }
                        u16at(sub + 10 + 2 * (c - first) as usize)?
                    }
                    _ => continue,
                };
                if gid != 0 {
                    return Some(gid);
                }
            }
        }
        None
    }

    fn outline_from(
        glyphs: &skrifa::outline::OutlineGlyphCollection,
        glyph_id: u32,
    ) -> Option<Outline> {
        let glyph = glyphs.get(GlyphId::new(glyph_id))?;
        let mut out = Outline::default();
        glyph
            .draw(
                DrawSettings::unhinted(Size::unscaled(), LocationRef::default()),
                &mut out,
            )
            .ok()?;
        if out.is_empty() { None } else { Some(out) }
    }
}

/// Prefer a non-private-use, lower codepoint when several map to one glyph.
fn reverse_char_is_better(candidate: char, current: char) -> bool {
    let pua = |c: char| ('\u{E000}'..='\u{F8FF}').contains(&c);
    match (pua(candidate), pua(current)) {
        (false, true) => true,
        (true, false) => false,
        _ => (candidate as u32) < (current as u32),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use pdf_test_support::fonts::minimal_ttf;

    #[test]
    fn parses_and_reports_metrics() {
        let prog = FontProgram::parse(minimal_ttf().into()).expect("parse");
        assert_eq!(prog.units_per_em(), 1000);
        assert_eq!(prog.num_glyphs(), 2);
    }

    #[test]
    fn oblique_shears_points_by_y() {
        let mut o = Outline {
            verbs: vec![OutlineVerb::MoveTo, OutlineVerb::LineTo, OutlineVerb::Close],
            points: vec![[0.0, 0.0], [100.0, 500.0]],
        };
        o.oblique(0.2);
        assert_eq!(o.points[0], [0.0, 0.0]);
        assert_eq!(o.points[1], [200.0, 500.0]);
    }

    #[test]
    fn embolden_grows_outer_contours_and_shrinks_holes() {
        // CCW outer square [100,900]^2 with a CW inner hole [400,600]^2 —
        // the nonzero convention (fill left of travel). Embolden by 70:
        // outer must expand ~35 outward, the hole contract ~35 inward.
        let sq = |a: f32, b: f32, ccw: bool| -> Vec<[f32; 2]> {
            let pts = vec![[a, a], [b, a], [b, b], [a, b]];
            if ccw {
                pts
            } else {
                pts.into_iter().rev().collect()
            }
        };
        let mut verbs = Vec::new();
        let mut points = Vec::new();
        for contour in [sq(100.0, 900.0, true), sq(400.0, 600.0, false)] {
            verbs.push(OutlineVerb::MoveTo);
            verbs.extend(std::iter::repeat_n(OutlineVerb::LineTo, contour.len() - 1));
            verbs.push(OutlineVerb::Close);
            points.extend(contour);
        }
        let mut o = Outline { verbs, points };
        o.embolden(70.0);
        // Outer corner (100,100) moves out along the corner bisector.
        assert!(
            o.points[0][0] < 100.0 && o.points[0][1] < 100.0,
            "{:?}",
            o.points[0]
        );
        assert!(
            o.points[0][0] > 100.0 - 71.0,
            "bounded miter: {:?}",
            o.points[0]
        );
        // Hole corner (400,400) — reversed order puts (400,600)-ish first;
        // check every hole point moved toward the hole centroid (500,500).
        for p in &o.points[4..8] {
            let (dx, dy) = (p[0] - 500.0, p[1] - 500.0);
            assert!(
                dx.abs() < 100.0 - 20.0 && dy.abs() < 100.0 - 20.0,
                "hole must shrink: {p:?}"
            );
        }
    }

    #[test]
    fn mac_only_cmap_resolves_through_the_native_fallback() {
        // A face whose only cmap is a Macintosh (1,0) format-6 subtable
        // (the DLIFLC Calibri-subset shape): Skrifa's Charmap ignores it,
        // so both the raw-code and the Unicode routes must fall back to the
        // native Mac lookup instead of resolving everything to notdef.
        let prog = FontProgram::parse(pdf_test_support::fonts::minimal_ttf_mac_cmap_only().into())
            .expect("parse");
        assert_eq!(prog.gid_for_code(0x41), Some(1), "raw byte through (1,0)");
        assert_eq!(
            prog.gid_for_char('A'),
            Some(1),
            "unicode -> MacRoman -> (1,0)"
        );
        assert_eq!(prog.gid_for_code(0x42), None, "unmapped code stays notdef");
    }

    #[test]
    fn microsoft_format_6_cmap_resolves_raw_codes() {
        let mut ttf = pdf_test_support::fonts::minimal_ttf_mac_cmap_only();
        patch_only_cmap_platform(&mut ttf, 3, 1);
        let prog = FontProgram::parse(ttf.into()).expect("parse");
        assert_eq!(
            prog.gid_for_code(0x41),
            Some(1),
            "raw byte through Microsoft (3,1) format 6"
        );
    }

    fn patch_only_cmap_platform(ttf: &mut [u8], platform: u16, encoding: u16) {
        let num_tables = u16::from_be_bytes([ttf[4], ttf[5]]) as usize;
        for i in 0..num_tables {
            let rec = 12 + i * 16;
            if &ttf[rec..rec + 4] == b"cmap" {
                let off = u32::from_be_bytes(ttf[rec + 8..rec + 12].try_into().unwrap()) as usize;
                ttf[off + 4..off + 6].copy_from_slice(&platform.to_be_bytes());
                ttf[off + 6..off + 8].copy_from_slice(&encoding.to_be_bytes());
                return;
            }
        }
        panic!("no cmap table");
    }

    #[test]
    fn extracts_triangle_glyph_outline() {
        let prog = FontProgram::parse(minimal_ttf().into()).expect("parse");
        // Glyph 0 (.notdef) is empty.
        assert!(prog.outline(0).is_none());
        // Glyph 1 is the triangle: a MoveTo, a Close, and points spanning the
        // declared x-bounds [100, 900].
        let o = prog.outline(1).expect("glyph 1 outline");
        assert_eq!(o.verbs[0], OutlineVerb::MoveTo);
        assert!(o.verbs.contains(&OutlineVerb::Close));
        let xs: Vec<f32> = o.points.iter().map(|p| p[0]).collect();
        assert!(xs.iter().cloned().fold(f32::MAX, f32::min) <= 100.5);
        assert!(xs.iter().cloned().fold(f32::MIN, f32::max) >= 899.5);
    }

    #[test]
    fn garbage_bytes_do_not_parse() {
        assert!(FontProgram::parse(vec![0u8; 32].into()).is_none());
    }

    #[test]
    fn valid_units_per_em_matches_freetype_range() {
        // OpenType 1.8.2 / FreeType `sfnt/sfobjs.c`: 16..=16384.
        assert!(!valid_units_per_em(0));
        assert!(!valid_units_per_em(14));
        assert!(!valid_units_per_em(15));
        assert!(valid_units_per_em(16));
        assert!(valid_units_per_em(1000));
        assert!(valid_units_per_em(2048));
        assert!(valid_units_per_em(16384));
        assert!(!valid_units_per_em(16385));
    }

    #[test]
    fn out_of_range_units_per_em_is_rejected() {
        // An SFNT with a corrupt `head.unitsPerEm` (here 14, as shipped by the
        // XFIPTR+Copperplate subset in pdfjs/issue9462) must be rejected so the
        // caller falls back to substitution — Skrifa would otherwise accept the
        // bogus em and scale the 1000-unit outlines into page-flooding glyphs.
        let mut ttf = minimal_ttf();
        patch_units_per_em(&mut ttf, 14);
        assert!(
            FontProgram::parse(ttf.into()).is_none(),
            "unitsPerEm=14 must be rejected"
        );
        // The unpatched face (unitsPerEm=1000) still parses.
        assert!(FontProgram::parse(minimal_ttf().into()).is_some());
    }

    /// Patch the `head` table's `unitsPerEm` (offset +18) in an SFNT, locating
    /// the table through the directory. Checksums are not revalidated — Skrifa
    /// (like FreeType for embedded PDF fonts) does not check them.
    fn patch_units_per_em(ttf: &mut [u8], upem: u16) {
        let num_tables = u16::from_be_bytes([ttf[4], ttf[5]]) as usize;
        for i in 0..num_tables {
            let rec = 12 + i * 16;
            if &ttf[rec..rec + 4] == b"head" {
                let off = u32::from_be_bytes(ttf[rec + 8..rec + 12].try_into().unwrap()) as usize;
                ttf[off + 18..off + 20].copy_from_slice(&upem.to_be_bytes());
                return;
            }
        }
        panic!("no head table");
    }

    #[test]
    fn char_for_gid_reverses_the_cmap() {
        let prog = FontProgram::parse(minimal_ttf().into()).expect("parse");
        // minimal_ttf's (3,1) cmap maps 'A' (0x41) -> GID 1.
        let gid = prog.gid_for_char('A').expect("'A' resolves");
        assert_eq!(prog.char_for_gid(gid), Some('A'));
        assert_eq!(prog.char_for_gid(0), None, "notdef has no identity");
    }
}

#[cfg(test)]
mod hinting_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::standard::StandardFont;

    fn helvetica() -> FontProgram {
        StandardFont::Helvetica
            .program()
            .expect("bundled face parses")
    }

    #[test]
    fn policy_gates_on_ppem_and_alignment() {
        // Rotated/skewed text is never hinted: the pixel grid hinting is
        // defined against no longer aligns with the glyph.
        for p in [
            HintingPolicy::None,
            HintingPolicy::Embedded,
            HintingPolicy::Auto,
        ] {
            assert!(
                !p.should_hint(12.0, false),
                "{p:?} must not hint skewed text"
            );
            assert!(!p.should_hint(0.0, true), "{p:?} must not hint a zero ppem");
        }
        assert!(!HintingPolicy::None.should_hint(12.0, true));
        // Embedded honours the font at any size; Auto backs off once
        // anti-aliasing alone resolves the stems.
        assert!(HintingPolicy::Embedded.should_hint(400.0, true));
        assert!(HintingPolicy::Auto.should_hint(12.0, true));
        assert!(HintingPolicy::Auto.should_hint(AUTO_HINT_MAX_PPEM, true));
        assert!(!HintingPolicy::Auto.should_hint(AUTO_HINT_MAX_PPEM + 0.1, true));
        // A 10pt glyph at 300dpi (~42 ppem) still hints; at 600dpi it does not.
        assert!(HintingPolicy::Auto.should_hint(10.0 * 300.0 / 72.0, true));
        assert!(!HintingPolicy::Auto.should_hint(10.0 * 600.0 / 72.0, true));
    }

    #[test]
    fn hinted_outlines_are_in_pixels_and_differ_from_unhinted() {
        let f = helvetica();
        let gid = f.gid_for_char('H').unwrap();
        let ppem = 12.0;

        let hinted = f
            .outlines_hinted(&[gid], ppem)
            .expect("hinting instance builds")[0]
            .clone()
            .expect("'H' hints to an outline");
        let unhinted = f.outline(gid).expect("'H' has an outline");

        // Design units (1000/em) vs pixels (12/em): the hinted outline lives
        // in a completely different, much smaller coordinate space.
        let extent = |o: &Outline| {
            o.points
                .iter()
                .fold(0f32, |m, p| m.max(p[0].abs()).max(p[1].abs()))
        };
        assert!(extent(&unhinted) > 100.0, "design units");
        assert!(
            extent(&hinted) < 30.0,
            "pixels at 12ppem: {}",
            extent(&hinted)
        );

        // Same topology, moved points: hinting grid-fits, it does not redraw.
        assert_eq!(hinted.verbs, unhinted.verbs, "hinting preserves contours");
    }

    #[test]
    fn hinting_snaps_stems_to_the_grid() {
        // The whole point: at a small ppem the hinted outline's extremes sit
        // on (or much nearer) integer pixel boundaries than the plain scaled
        // outline does.
        let f = helvetica();
        let gid = f.gid_for_char('H').unwrap();
        let ppem = 16.0;
        let hinted = f.outlines_hinted(&[gid], ppem).unwrap()[0].clone().unwrap();

        let off_grid = |o: &Outline| {
            let n = o.points.len() as f32;
            o.points
                .iter()
                .map(|p| (p[1] - p[1].round()).abs())
                .sum::<f32>()
                / n
        };
        let scaled = {
            let mut o = f.outline(gid).unwrap();
            let s = ppem / f.units_per_em() as f32;
            for p in &mut o.points {
                p[0] *= s;
                p[1] *= s;
            }
            o
        };
        assert!(
            off_grid(&hinted) < off_grid(&scaled),
            "hinted y {} should sit closer to the grid than scaled y {}",
            off_grid(&hinted),
            off_grid(&scaled)
        );
    }

    #[test]
    fn hinting_is_deterministic() {
        let f = helvetica();
        let gids: Vec<u32> = "Hamburg"
            .chars()
            .filter_map(|c| f.gid_for_char(c))
            .collect();
        let a = f.outlines_hinted(&gids, 14.0).unwrap();
        let b = f.outlines_hinted(&gids, 14.0).unwrap();
        assert_eq!(a, b, "same font, same ppem => identical outlines");
    }
}
