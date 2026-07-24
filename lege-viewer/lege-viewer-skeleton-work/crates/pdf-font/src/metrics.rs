//! The prepared advance/decoding model a page compiler uses to place text.
//!
//! This unifies the two character-code shapes so the caller does not special-
//! case them: **simple** fonts consume one byte per code; **Identity** Type 0
//! fonts consume two big-endian bytes per code (CID = code). Both yield a
//! glyph identity placeholder (code/CID — real glyph-id resolution is a later
//! font phase), the PDF advance in glyph space, and whether word spacing (`Tw`)
//! applies (single-byte code 32 only).

use std::sync::Arc;

use crate::cid::{CidVerticalMetrics, CidWidths};
use crate::cmap::CMap;
use crate::widths::SimpleWidths;

/// How a font's strings decode into positioned glyphs.
#[derive(Debug, Clone, PartialEq)]
pub enum FontMetrics {
    /// Single-byte simple font.
    Simple(SimpleWidths),
    /// Two-byte Identity-H composite font (CID = code).
    CidIdentity(CidWidths),
    /// Composite font with a predefined or embedded CJK CMap: the CMap splits
    /// the byte string into variable-width codes and maps each to a CID; `/W`
    /// widths are indexed by that CID (ISO 32000-1 §9.7.5–9.7.6).
    CidCMap { widths: CidWidths, cmap: Arc<CMap> },
    /// Composite font in vertical writing mode (`Identity-V`, or a CMap whose
    /// `/WMode` is 1). Decodes exactly like the horizontal forms (`cmap:
    /// None` = two-byte identity), but layout advances along −y using the
    /// `/W2`//`/DW2` vertical metrics, with the `v`-vector glyph-origin
    /// displacement (ISO 32000-1 §9.7.4.3).
    CidVertical {
        widths: CidWidths,
        vertical: CidVerticalMetrics,
        cmap: Option<Arc<CMap>>,
    },
}

/// Vertical placement of one CID (§9.7.4.3), glyph space (1000/em): the pen
/// displacement `w1y` (negative = down) and the `v` vector from the
/// horizontal to the vertical glyph origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerticalPlacement {
    pub advance: f32,
    pub origin: (f32, f32),
}

/// One losslessly decoded PDF character code.
///
/// A source character code, a CID, and a font-program glyph id are different
/// namespaces. This type deliberately carries the first two; [`crate::GlyphMap`]
/// performs the final CID/code → GID mapping. Keeping `char_code` is essential
/// for `/ToUnicode`, whose keys are the original bytes rather than the CID.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecodedCode {
    /// Big-endian value of the original 1–4 source bytes.
    pub char_code: u32,
    /// Number of source bytes consumed for this code.
    pub byte_len: u8,
    /// CID for a composite font; equal to `char_code` for a simple font.
    pub cid: u32,
    pub advance: f32,
    pub word_space: bool,
}

impl FontMetrics {
    /// Decode a show-text string into its codes with (horizontal) advances.
    pub fn decode(&self, s: &[u8]) -> Vec<DecodedCode> {
        match self {
            FontMetrics::Simple(w) => s
                .iter()
                .map(|&b| DecodedCode {
                    char_code: b as u32,
                    byte_len: 1,
                    cid: b as u32,
                    advance: w.advance(b as u32),
                    word_space: b == 32,
                })
                .collect(),
            FontMetrics::CidIdentity(w) => decode_identity(w, s),
            FontMetrics::CidCMap { widths, cmap } => decode_cmap(widths, cmap, s),
            FontMetrics::CidVertical { widths, cmap, .. } => match cmap {
                Some(cmap) => decode_cmap(widths, cmap, s),
                None => decode_identity(widths, s),
            },
        }
    }

    /// Whether this font lays out along −y (writing mode 1).
    pub fn is_vertical(&self) -> bool {
        matches!(self, FontMetrics::CidVertical { .. })
    }

    /// Vertical placement for a decoded CID; `None` for horizontal fonts.
    pub fn vertical(&self, cid: u32) -> Option<VerticalPlacement> {
        let FontMetrics::CidVertical {
            widths, vertical, ..
        } = self
        else {
            return None;
        };
        Some(VerticalPlacement {
            advance: vertical.advance(cid),
            origin: vertical.origin(cid, widths.advance(cid)),
        })
    }
}

/// Two-byte big-endian identity decoding (CID = code).
fn decode_identity(w: &CidWidths, s: &[u8]) -> Vec<DecodedCode> {
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i + 1 < s.len() {
        let code = ((s[i] as u32) << 8) | (s[i + 1] as u32);
        out.push(DecodedCode {
            char_code: code,
            byte_len: 2,
            cid: code,
            advance: w.advance(code),
            word_space: false,
        });
        i += 2;
    }
    // A trailing odd byte is padded (tolerant of malformed input).
    if i < s.len() {
        let code = (s[i] as u32) << 8;
        out.push(DecodedCode {
            char_code: code,
            byte_len: 1,
            cid: code,
            advance: w.advance(code),
            word_space: false,
        });
    }
    out
}

/// CMap decoding: the CMap splits the string into variable-width codes and
/// maps each to a CID; the advance is the CID's `/W` width. Word spacing
/// (`Tw`) applies only to a single-byte code 32 (§9.3.3), which for a CJK
/// CMap means a one-byte code equal to 32 — never a two-byte code whose low
/// byte is 0x20.
fn decode_cmap(widths: &CidWidths, cmap: &CMap, s: &[u8]) -> Vec<DecodedCode> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let (code, n) = cmap.next_code(&s[i..]);
        if n == 0 {
            break;
        }
        let cid = cmap.cid(code);
        out.push(DecodedCode {
            char_code: code,
            byte_len: n as u8,
            cid,
            advance: widths.advance(cid),
            word_space: n == 1 && code == 32,
        });
        i += n;
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn simple_is_one_byte_per_code() {
        let m = FontMetrics::Simple(SimpleWidths::new(65, vec![600.0], 250.0));
        let d = m.decode(b"AB");
        assert_eq!(d.len(), 2);
        assert_eq!(
            d[0],
            DecodedCode {
                char_code: 65,
                byte_len: 1,
                cid: 65,
                advance: 600.0,
                word_space: false,
            }
        );
        assert_eq!(
            d[1],
            DecodedCode {
                char_code: 66,
                byte_len: 1,
                cid: 66,
                advance: 250.0,
                word_space: false,
            }
        );
    }

    #[test]
    fn space_flags_word_spacing() {
        let m = FontMetrics::Simple(SimpleWidths::new(32, vec![300.0], 0.0));
        assert!(m.decode(b" ")[0].word_space);
    }

    #[test]
    fn identity_is_two_bytes_big_endian() {
        let m = FontMetrics::CidIdentity(CidWidths::new(1000.0, vec![(0x0041, 0x0041, 700.0)]));
        // Bytes 00 41 → CID 0x41 (65).
        let d = m.decode(&[0x00, 0x41, 0x00, 0x10]);
        assert_eq!(d.len(), 2);
        assert_eq!(
            d[0],
            DecodedCode {
                char_code: 0x41,
                byte_len: 2,
                cid: 0x41,
                advance: 700.0,
                word_space: false,
            }
        );
        assert_eq!(
            d[1],
            DecodedCode {
                char_code: 0x10,
                byte_len: 2,
                cid: 0x10,
                advance: 1000.0,
                word_space: false,
            }
        );
    }
}
