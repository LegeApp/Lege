//! Pattern-dictionary decoding (jbig2decplan.md §18, T.88 §6.7 / §7.4.4).
//!
//! A pattern dictionary carries one *collective* bitmap holding `GRAYMAX + 1`
//! fixed-size patterns laid out side by side. The collective bitmap has width
//! `(GRAYMAX + 1) × HDPW` and height `HDPH`; pattern `i` is the `HDPW`-wide
//! sub-bitmap at horizontal offset `i × HDPW` (T.88 §6.7.5). Both MMR and
//! arithmetic collective bitmaps are supported (the encoder's default is MMR).
//!
//! The pattern-dictionary segment does not carry AT pixels: for the arithmetic
//! variant they are fixed by the spec (§6.7.5) — AT1 = `(-HDPW, 0)` and, for
//! template 0, AT2 = `(-3, -1)`, AT3 = `(2, -2)`, AT4 = `(-2, -2)`.
//!
//! All dimension multiplications are checked against [`DecodeLimits`] *before*
//! any allocation so a malformed `GRAYMAX`/`HDPW`/`HDPH` cannot trigger an
//! oversized allocation.

use std::sync::Arc;

use crate::decode::arith::ArithmeticDecoder;
use crate::decode::error::{DecodeError, LimitError};
use crate::decode::generic::{GenericScratch, decode_generic_bitmap};
use crate::decode::mmr::decode_mmr_bitmap;
use crate::shared::bitmap::MonoBitmap;
use crate::shared::limits::DecodeLimits;
use crate::shared::mq_table::MqContext;
use crate::shared::reader::Reader;

/// A decoded pattern dictionary: the fixed-size patterns, shareable across
/// halftone regions and page workers.
pub struct PatternDictionary {
    /// The `GRAYMAX + 1` patterns, indexed by gray-scale value.
    pub patterns: Box<[Arc<MonoBitmap>]>,
    /// Pattern width (HDPW).
    pub pattern_width: u32,
    /// Pattern height (HDPH).
    pub pattern_height: u32,
}

impl PatternDictionary {
    /// The pattern for gray-scale value `index`, if in range.
    #[inline]
    pub fn pattern(&self, index: usize) -> Option<&Arc<MonoBitmap>> {
        self.patterns.get(index)
    }

    /// The number of patterns (`GRAYMAX + 1`).
    #[inline]
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

/// Parsed pattern-dictionary flags and geometry (T.88 §7.4.4.1).
struct PatternDictHeader {
    mmr: bool,
    template: u8,
    hdpw: u32,
    hdph: u32,
    gray_max: u32,
}

fn parse_header(r: &mut Reader<'_>) -> Result<PatternDictHeader, DecodeError> {
    // §7.4.4.1.1 pattern dictionary flags (1 byte): bit0 HDMMR, bits1-2 HDTEMPLATE.
    let flags = r.read_u8()?;
    let mmr = flags & 0x01 != 0;
    let template = (flags >> 1) & 0x03;
    // §7.4.4.1.2/.3 HDPW, HDPH (1 byte each).
    let hdpw = u32::from(r.read_u8()?);
    let hdph = u32::from(r.read_u8()?);
    // §7.4.4.1.4 GRAYMAX (4 bytes).
    let gray_max = r.read_u32_be()?;
    Ok(PatternDictHeader {
        mmr,
        template,
        hdpw,
        hdph,
        gray_max,
    })
}

/// Decode a pattern-dictionary segment payload (T.88 §7.4.4).
///
/// `generic_ctx` is reused, worker-local scratch; it is reset here (a fresh
/// segment starts every arithmetic context at zero). It must be at least
/// `1 << 16` entries. Unused for the MMR variant.
pub fn decode_pattern_dictionary(
    payload: &[u8],
    limits: &DecodeLimits,
    generic_ctx: &mut [MqContext],
    scratch: &mut GenericScratch,
) -> Result<PatternDictionary, DecodeError> {
    let mut r = Reader::new(payload);
    let hdr = parse_header(&mut r)?;

    if hdr.hdpw == 0 || hdr.hdph == 0 {
        return Err(DecodeError::Malformed {
            reason: "pattern dictionary HDPW/HDPH must be non-zero",
        });
    }

    // Number of patterns = GRAYMAX + 1; check overflow before multiplying.
    let num_patterns = (hdr.gray_max as u64)
        .checked_add(1)
        .ok_or(DecodeError::Overflow {
            operation: "pattern count (GRAYMAX + 1)",
        })?;
    if num_patterns > limits.max_symbols as u64 {
        return Err(DecodeError::limit(LimitError::Count {
            what: "patterns",
            value: num_patterns,
            limit: limits.max_symbols as u64,
        }));
    }

    // Collective bitmap width = num_patterns × HDPW (checked before allocation).
    let collective_width =
        num_patterns
            .checked_mul(hdr.hdpw as u64)
            .ok_or(DecodeError::Overflow {
                operation: "collective bitmap width",
            })?;
    if collective_width > limits.max_width as u64 {
        return Err(DecodeError::limit(LimitError::Dimension {
            dimension: "collective bitmap width",
            value: collective_width,
            limit: limits.max_width as u64,
        }));
    }
    let collective_width = collective_width as u32;
    let collective_height = hdr.hdph;

    let data = &payload[r.position()..];

    // Decode the single collective bitmap (MonoBitmap::new checks region pixels).
    let collective = if hdr.mmr {
        decode_mmr_bitmap(data, collective_width, collective_height, limits)?
    } else {
        // §6.7.5 fixed AT pixels for the pattern-dictionary collective bitmap
        // (GBTEMPLATE = HDTEMPLATE). AT1 = (-HDPW, 0); templates 1–3 use AT1 only.
        let hdpw_i8 = i8::try_from(hdr.hdpw).unwrap_or(i8::MIN);
        let at = [(-hdpw_i8, 0i8), (-3, -1), (2, -2), (-2, -2)];
        if generic_ctx.len() < (1usize << 16) {
            return Err(DecodeError::Overflow {
                operation: "pattern dictionary context array too small",
            });
        }
        for c in generic_ctx.iter_mut() {
            *c = MqContext::default();
        }
        let mut dec = ArithmeticDecoder::new(data);
        decode_generic_bitmap(
            &mut dec,
            collective_width,
            collective_height,
            hdr.template,
            at,
            generic_ctx,
            limits,
            scratch,
        )?
    };

    // Split the collective bitmap into individual immutable patterns.
    let num_patterns = num_patterns as usize;
    let mut patterns: Vec<Arc<MonoBitmap>> = Vec::with_capacity(num_patterns.min(4096));
    for i in 0..num_patterns {
        let x0 = (i as u32) * hdr.hdpw;
        let mut pat = MonoBitmap::new(hdr.hdpw, hdr.hdph, false, limits)?;
        for y in 0..hdr.hdph {
            for x in 0..hdr.hdpw {
                if collective.get(x0 + x, y) {
                    pat.set(x, y, true);
                }
            }
        }
        patterns.push(Arc::new(pat));
    }

    Ok(PatternDictionary {
        patterns: patterns.into_boxed_slice(),
        pattern_width: hdr.hdpw,
        pattern_height: hdr.hdph,
    })
}
