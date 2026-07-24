//! CID (composite Type 0) font advance widths (fonts.md §3 "PDF metrics").
//!
//! Covers `/DW` (default width) and `/W` (per-CID overrides) from the
//! descendant CIDFont. Character-code → CID decoding for Identity-H/V (the
//! dominant embedded-subset case) is handled by [`crate::metrics::FontMetrics`];
//! embedded and predefined CJK CMaps are a later font phase.

use std::cmp::Ordering;

/// Advance widths for a CID font, indexed by CID and in glyph space (1000/em).
#[derive(Debug, Clone, PartialEq)]
pub struct CidWidths {
    default_width: f32,
    /// Non-overlapping `[start, end]` CID ranges with a shared width, sorted by
    /// `start`. Both `/W` forms (`c [w…]` and `c_first c_last w`) reduce to
    /// this.
    ranges: Vec<(u32, u32, f32)>,
}

impl CidWidths {
    pub fn new(default_width: f32, mut ranges: Vec<(u32, u32, f32)>) -> Self {
        ranges.sort_by_key(|r| r.0);
        Self { default_width, ranges }
    }

    /// The default (`/DW`, 1000 if absent) with no overrides.
    pub fn uniform(default_width: f32) -> Self {
        Self { default_width, ranges: Vec::new() }
    }

    /// Advance for `cid`, in glyph-space units (1000/em).
    pub fn advance(&self, cid: u32) -> f32 {
        match self.ranges.binary_search_by(|r| {
            if cid < r.0 {
                Ordering::Greater
            } else if cid > r.1 {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        }) {
            Ok(i) => self.ranges[i].2,
            Err(_) => self.default_width,
        }
    }
}

/// Vertical metrics for a CID font in writing mode 1 (ISO 32000-1 §9.7.4.3):
/// `/DW2` defaults plus `/W2` per-CID overrides, mirroring [`CidWidths`].
///
/// Each glyph carries a vertical displacement `w1y` (typically negative — the
/// pen moves down the page) and a position vector `v = (vx, vy)` from the
/// glyph's *horizontal* origin to its *vertical* origin. Defaults: `w1y` =
/// `/DW2[1]` (−1000 absent), `vy` = `/DW2[0]` (880 absent), and `vx` = half
/// the glyph's horizontal advance `w0/2` (supplied by the caller at query
/// time, since it lives in `/W`, not `/W2`).
#[derive(Debug, Clone, PartialEq)]
pub struct CidVerticalMetrics {
    default_vy: f32,
    default_w1y: f32,
    /// Non-overlapping `[start, end]` CID ranges with `[w1y, vx, vy]`, sorted
    /// by `start`. Both `/W2` forms (`c [w1y vx vy …]` and
    /// `c_first c_last w1y vx vy`) reduce to this.
    ranges: Vec<(u32, u32, [f32; 3])>,
}

impl CidVerticalMetrics {
    pub fn new(default_vy: f32, default_w1y: f32, mut ranges: Vec<(u32, u32, [f32; 3])>) -> Self {
        ranges.sort_by_key(|r| r.0);
        Self { default_vy, default_w1y, ranges }
    }

    /// The spec defaults with no `/DW2` and no `/W2`.
    pub fn default_metrics() -> Self {
        Self::new(880.0, -1000.0, Vec::new())
    }

    fn lookup(&self, cid: u32) -> Option<&[f32; 3]> {
        self.ranges
            .binary_search_by(|r| {
                if cid < r.0 {
                    Ordering::Greater
                } else if cid > r.1 {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            })
            .ok()
            .map(|i| &self.ranges[i].2)
    }

    /// Vertical displacement `w1y` for `cid`, glyph space (1000/em); the pen
    /// advances by this along y (negative = downward).
    pub fn advance(&self, cid: u32) -> f32 {
        self.lookup(cid).map_or(self.default_w1y, |m| m[0])
    }

    /// The `v` vector `(vx, vy)` for `cid`, glyph space. `w0` is the glyph's
    /// horizontal advance (for the `vx = w0/2` default).
    pub fn origin(&self, cid: u32, w0: f32) -> (f32, f32) {
        self.lookup(cid).map_or((w0 / 2.0, self.default_vy), |m| (m[1], m[2]))
    }
}

/// Resolves a decoded code (simple) or CID (composite) to a glyph id in the
/// embedded font program — the last step before outline extraction.
#[derive(Debug, Clone, PartialEq)]
pub enum GlyphMap {
    /// No reliable mapping (no embedded program): the glyph field stays the
    /// raw code, and the backend renders a placement box.
    Identity,
    /// Composite font: CID → GID.
    Cid(CidToGid),
    /// Simple font: a precomputed 256-entry code → GID table.
    Simple(Box<[u32; 256]>),
}

impl GlyphMap {
    /// The glyph id for a decoded code/CID.
    pub fn gid(&self, code: u32) -> u32 {
        match self {
            GlyphMap::Identity => code,
            GlyphMap::Cid(c) => c.gid(code),
            GlyphMap::Simple(t) => t.get((code & 0xFF) as usize).copied().unwrap_or(0),
        }
    }
}

/// Maps a CID to a glyph id in the embedded font program (ISO 32000-1 §9.7.4.3).
#[derive(Debug, Clone, PartialEq)]
pub enum CidToGid {
    /// `/CIDToGIDMap /Identity` (or absent): GID = CID.
    Identity,
    /// A stream of big-endian `u16` GIDs indexed by CID.
    Map(Vec<u16>),
}

impl CidToGid {
    /// Build from a decoded `/CIDToGIDMap` stream (2 bytes per CID, big-endian).
    pub fn from_stream(bytes: &[u8]) -> Self {
        CidToGid::Map(bytes.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect())
    }

    /// Bridge a *substitute* face for a non-embedded CJK CID font: the face
    /// indexes glyphs by its own `cmap`, not by the document's CIDs, so map
    /// CID → Unicode (the transcribed Adobe-*-UCS2 tables) → the face's
    /// charmap. `None` when the charset has no table or the face covers none
    /// of it (the caller then keeps its previous mapping) — this is what
    /// PDFium does for non-embedded CID fonts resolved against installed
    /// fonts (CPDF_CIDFont::GlyphFromCharCode's unicode path).
    pub fn from_unicode_bridge(
        charset: crate::system::Charset,
        program: &crate::engine::FontProgram,
    ) -> Option<Self> {
        let table = crate::cmap_tables::cid2uni_table(charset)?;
        let n = table.len() / 2;
        let mut gids = vec![0u16; n];
        let mut covered = 0usize;
        for (cid, slot) in gids.iter_mut().enumerate() {
            if let Some(c) = crate::cmap_tables::cid_to_unicode(charset, cid as u32)
                && let Some(gid) = program.gid_for_char(c)
                && gid != 0
                && gid <= u16::MAX as u32
            {
                *slot = gid as u16;
                covered += 1;
            }
        }
        (covered > 0).then_some(CidToGid::Map(gids))
    }

    /// The glyph id for `cid` (0 = `.notdef` for out-of-range CIDs).
    pub fn gid(&self, cid: u32) -> u32 {
        match self {
            CidToGid::Identity => cid,
            CidToGid::Map(m) => m.get(cid as usize).copied().unwrap_or(0) as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn cid_to_gid_maps() {
        assert_eq!(CidToGid::Identity.gid(7), 7);
        let m = CidToGid::from_stream(&[0x00, 0x00, 0x00, 0x05, 0x00, 0x09]);
        assert_eq!(m.gid(1), 5);
        assert_eq!(m.gid(2), 9);
        assert_eq!(m.gid(99), 0, "out of range → .notdef");
    }

    #[test]
    fn ranges_and_default() {
        // CIDs 1..=3 → 500; CID 10 → 800; default 1000.
        let w = CidWidths::new(1000.0, vec![(1, 3, 500.0), (10, 10, 800.0)]);
        assert_eq!(w.advance(0), 1000.0);
        assert_eq!(w.advance(2), 500.0);
        assert_eq!(w.advance(3), 500.0);
        assert_eq!(w.advance(4), 1000.0);
        assert_eq!(w.advance(10), 800.0);
    }
}
