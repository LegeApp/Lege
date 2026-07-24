//! Arithmetic generic-region decoding, template 0 (jbig2decplan.md §14, T.88
//! §6.2).
//!
//! This mirrors the encoder's rolling `c1`/`c2`/`c3` context organisation
//! (`encode::arith::Jbig2ArithCoder::encode_generic_region_inner`) so a
//! round-trip is pixel-identical. Two paths share one 16-bit context layout:
//!
//! * a fast rolling path for the nominal template-0 AT positions (the only
//!   thing the encoder emits), reading packed rows directly with no per-pixel
//!   `get()`; and
//! * a general per-pixel path for arbitrary valid AT positions, which reduces
//!   to exactly the same context values for nominal AT (asserted by a test).
//!
//! All four arithmetic templates (0–3) and TPGDON typical prediction are
//! supported (Phase 5a). MMR generic regions are handled by [`crate::decode::mmr`].

use crate::decode::arith::ArithmeticDecoder;
use crate::decode::error::{DecodeError, ParseError};
use crate::shared::bitmap::MonoBitmap;
use crate::shared::limits::DecodeLimits;
use crate::shared::mq_table::MqContext;
use crate::shared::reader::Reader;

/// Nominal template-0 adaptive-template offsets (T.88 §6.2.5.3): AT1 (3,-1),
/// AT2 (-3,-1), AT3 (2,-2), AT4 (-2,-2). Also this crate's encoder default.
pub const NOMINAL_AT0: [(i8, i8); 4] = [(3, -1), (-3, -1), (2, -2), (-2, -2)];

/// Reusable row buffers for the generic decoder: a zero row for the top-boundary
/// rows and the current-row accumulator. Pooled in the [`DecoderContext`] and
/// reused across every region and every dictionary symbol, so the decode of a
/// symbol-heavy page allocates these once instead of twice per symbol.
#[derive(Default)]
pub struct GenericScratch {
    zero: Vec<u32>,
    cur: Vec<u32>,
}

impl GenericScratch {
    /// Return a zeroed zero-row and a zeroed current-row buffer of `stride`
    /// words each, reusing the existing allocations (disjoint fields, so both
    /// may be borrowed at once).
    #[inline]
    fn rows(&mut self, stride: usize) -> (&[u32], &mut [u32]) {
        self.zero.clear();
        self.zero.resize(stride, 0);
        self.cur.clear();
        self.cur.resize(stride, 0);
        (&self.zero, &mut self.cur)
    }
}

/// Nominal AT1 offset for templates 1–3 (T.88 §6.2.5.3 Figures 5–7): template 1
/// uses (3,-1); templates 2 and 3 use (2,-1).
pub const NOMINAL_AT1: [(i8, i8); 3] = [(3, -1), (2, -1), (2, -1)];

// SLTP pseudo-pixel contexts for TPGDON (T.88 §6.2.5.7, Figure 8), the literal
// spec/jbig2dec values. `decode_template0_general`'s bit layout is the pdf.js/
// jbig2dec sorted-template order, so template 0's SLTP context is the spec's
// 0x9B25 (not a re-expressed value). Templates 1–3 likewise use spec numbering.
const SLTP_CTX_T0: usize = 0x9B25;
const SLTP_CTX_T1: usize = 0x0795;
const SLTP_CTX_T2: usize = 0x00E5;
const SLTP_CTX_T3: usize = 0x0195;

/// A parsed generic-region segment: geometry, coding flags, and the coded data.
#[derive(Clone, Debug)]
pub struct GenericRegion<'a> {
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    /// External combination operator (region flags low 3 bits).
    pub comb_operator: u8,
    pub mmr: bool,
    pub template: u8,
    pub tpgdon: bool,
    pub at: [(i8, i8); 4],
    /// Arithmetic (or MMR) coded data.
    pub data: &'a [u8],
}

/// Parse the generic-region segment payload (T.88 §7.4.6): region segment info
/// (§7.4.1) + generic flags + AT pixels + coded data.
pub fn parse_generic_region<'a>(payload: &'a [u8]) -> Result<GenericRegion<'a>, ParseError> {
    let mut r = Reader::new(payload);
    // §7.4.1 region segment information field.
    let width = r.read_u32_be()?;
    let height = r.read_u32_be()?;
    let x = r.read_u32_be()?;
    let y = r.read_u32_be()?;
    let region_flags = r.read_u8()?;
    let comb_operator = region_flags & 0x07;

    // §7.4.6.2 generic region segment flags.
    let flags = r.read_u8()?;
    let mmr = flags & 0x01 != 0;
    let template = (flags >> 1) & 0x03;
    let tpgdon = flags & 0x08 != 0;

    // AT pixels (T.88 §7.4.6.3): template 0 has 4, templates 1–3 have 1 each.
    // (Only present for arithmetic coding, i.e. when MMR is off.)
    let mut at = [(0i8, 0i8); 4];
    if !mmr {
        let at_count = if template == 0 { 4 } else { 1 };
        for slot in at.iter_mut().take(at_count) {
            let ax = r.read_i8()?;
            let ay = r.read_i8()?;
            *slot = (ax, ay);
        }
    }

    let data = &payload[r.position()..];
    Ok(GenericRegion {
        width,
        height,
        x,
        y,
        comb_operator,
        mmr,
        template,
        tpgdon,
        at,
        data,
    })
}

/// Decode a parsed generic region into a fresh region-local [`MonoBitmap`].
///
/// `contexts` must be at least `1 << 16` entries; it is not reset here (the
/// caller resets before each independent region — a fresh segment starts with
/// all-zero states).
pub fn decode_generic_region(
    region: &GenericRegion<'_>,
    limits: &DecodeLimits,
    contexts: &mut [MqContext],
    scratch: &mut GenericScratch,
) -> Result<MonoBitmap, DecodeError> {
    if region.mmr {
        // MMR (Group 4) generic region: no arithmetic coder, no AT pixels; the
        // payload is byte-aligned T.6 data for the full region (T.88 §6.2.6).
        return crate::decode::mmr::decode_mmr_bitmap(
            region.data,
            region.width,
            region.height,
            limits,
        );
    }
    if (contexts.len() as u64) < (1u64 << 16) {
        return Err(DecodeError::Overflow {
            operation: "generic region context array too small",
        });
    }

    let mut bitmap = MonoBitmap::new(region.width, region.height, false, limits)?;
    if region.width == 0 || region.height == 0 {
        return Ok(bitmap);
    }

    let mut decoder = ArithmeticDecoder::new(region.data);
    decode_into(
        &mut bitmap,
        &mut decoder,
        region.template,
        region.at,
        region.tpgdon,
        contexts,
        scratch,
    );
    Ok(bitmap)
}

/// Decode a generic region **into** a caller-provided bitmap, reusing its
/// backing allocation (the zero-alloc `decode_embedded_into` path). `dest` must
/// already be sized to `region.width` × `region.height` and all-white, as
/// produced by the context's bitmap pool. Behaviour otherwise matches
/// [`decode_generic_region`].
#[allow(clippy::too_many_arguments)]
pub fn decode_generic_region_into(
    region: &GenericRegion<'_>,
    limits: &DecodeLimits,
    contexts: &mut [MqContext],
    scratch: &mut GenericScratch,
    dest: &mut MonoBitmap,
) -> Result<(), DecodeError> {
    debug_assert_eq!(dest.width(), region.width);
    debug_assert_eq!(dest.height(), region.height);
    if region.mmr {
        // MMR has no reusable-buffer decoder; decode into a temporary and copy
        // into `dest` (reusing `dest`'s allocation). MMR generic regions are not
        // the zero-alloc target.
        let bm = crate::decode::mmr::decode_mmr_bitmap(
            region.data,
            region.width,
            region.height,
            limits,
        )?;
        dest.assign_from(&bm);
        return Ok(());
    }
    if (contexts.len() as u64) < (1u64 << 16) {
        return Err(DecodeError::Overflow {
            operation: "generic region context array too small",
        });
    }
    if region.width == 0 || region.height == 0 {
        return Ok(());
    }
    let mut decoder = ArithmeticDecoder::new(region.data);
    decode_into(
        dest,
        &mut decoder,
        region.template,
        region.at,
        region.tpgdon,
        contexts,
        scratch,
    );
    Ok(())
}

/// Decode a single template-0 generic bitmap from an *existing* arithmetic
/// decoder, without resetting `contexts` (jbig2decplan.md §16).
///
/// Symbol-dictionary bitmaps share one arithmetic stream and one generic-context
/// bank across every symbol in the dictionary, so this variant neither creates
/// its own decoder nor zeroes the contexts — the dictionary decoder owns both.
/// `contexts` must be at least `1 << 16` entries.
#[allow(clippy::too_many_arguments)]
pub fn decode_generic_bitmap(
    decoder: &mut ArithmeticDecoder<'_>,
    width: u32,
    height: u32,
    template: u8,
    at: [(i8, i8); 4],
    contexts: &mut [MqContext],
    limits: &DecodeLimits,
    scratch: &mut GenericScratch,
) -> Result<MonoBitmap, DecodeError> {
    if (contexts.len() as u64) < (1u64 << 16) {
        return Err(DecodeError::Overflow {
            operation: "generic bitmap context array too small",
        });
    }
    let mut bitmap = MonoBitmap::new(width, height, false, limits)?;
    if width == 0 || height == 0 {
        return Ok(bitmap);
    }
    // Symbol-dictionary and refinement generic bitmaps never use TPGDON.
    decode_into(&mut bitmap, decoder, template, at, false, contexts, scratch);
    Ok(bitmap)
}

/// Skip-aware generic bitmap decode (T.88 §6.2.5.7 USESKIP / halftone
/// HENABLESKIP §6.6.5.1): pixels where `skip` is set are forced to 0 and *not*
/// arithmetically decoded; their neighbours still contribute to later contexts
/// as 0. Per-pixel (no fast rolling path) — used only for the rare skip case.
/// TPGDON is not combined with skip here (the gray-scale planes never set it).
#[allow(clippy::too_many_arguments)]
pub fn decode_generic_bitmap_skip(
    decoder: &mut ArithmeticDecoder<'_>,
    width: u32,
    height: u32,
    template: u8,
    at: [(i8, i8); 4],
    contexts: &mut [MqContext],
    limits: &DecodeLimits,
    skip: &MonoBitmap,
    scratch: &mut GenericScratch,
) -> Result<MonoBitmap, DecodeError> {
    if (contexts.len() as u64) < (1u64 << 16) {
        return Err(DecodeError::Overflow {
            operation: "generic bitmap context array too small",
        });
    }
    let mut bitmap = MonoBitmap::new(width, height, false, limits)?;
    if width == 0 || height == 0 {
        return Ok(bitmap);
    }
    let stride = bitmap.stride_words() as usize;
    let (zero_row, cur) = scratch.rows(stride);
    for y in 0..height {
        for w in cur.iter_mut() {
            *w = 0;
        }
        let prev1: &[u32] = if y >= 1 { bitmap.row(y - 1) } else { zero_row };
        let prev2: &[u32] = if y >= 2 { bitmap.row(y - 2) } else { zero_row };
        for x in 0..width {
            if skip.get(x, y) {
                continue; // implicitly 0, no arithmetic bit consumed
            }
            let ctx = pixel_context(
                template, &bitmap, &*cur, prev1, prev2, width, y, x as i64, &at,
            );
            let bit = decoder.decode_bit(&mut contexts[ctx]);
            if bit {
                let xu = x as usize;
                cur[xu >> 5] |= 1u32 << (31 - (xu & 31));
            }
        }
        bitmap.row_mut(y).copy_from_slice(cur);
    }
    Ok(bitmap)
}

/// Per-pixel context for any template, matching the per-template decoders'
/// bit layouts. Used by the skip-aware path.
#[allow(clippy::too_many_arguments)]
#[inline]
fn pixel_context(
    template: u8,
    bitmap: &MonoBitmap,
    cur: &[u32],
    prev1: &[u32],
    prev2: &[u32],
    width: u32,
    y: u32,
    xi: i64,
    at: &[(i8, i8); 4],
) -> usize {
    let a1 = (at[0].0 as i64, at[0].1 as i64);
    // AT slots read the bitmap directly so they can reference any earlier row;
    // fixed pixels stay on the cached `prev1`/`prev2` rows.
    let a = |ax: i64, ay: i64| at_pixel(bitmap, cur, width, y, xi, ax, ay);
    match template {
        0 => {
            let a2 = (at[1].0 as i64, at[1].1 as i64);
            let a3 = (at[2].0 as i64, at[2].1 as i64);
            let a4 = (at[3].0 as i64, at[3].1 as i64);
            let p = |dx: i64, dy: i64| -> u32 {
                let xx = xi + dx;
                match dy {
                    0 => {
                        if xx < 0 || xx >= xi || xx >= width as i64 {
                            0
                        } else {
                            sample(cur, width, xx)
                        }
                    }
                    -1 => sample(prev1, width, xx),
                    _ => sample(prev2, width, xx),
                }
            };
            ((a(a4.0, a4.1) << 15)
                | (p(-1, -2) << 14)
                | (p(0, -2) << 13)
                | (p(1, -2) << 12)
                | (a(a3.0, a3.1) << 11)
                | (a(a2.0, a2.1) << 10)
                | (p(-2, -1) << 9)
                | (p(-1, -1) << 8)
                | (p(0, -1) << 7)
                | (p(1, -1) << 6)
                | (p(2, -1) << 5)
                | (a(a1.0, a1.1) << 4)
                | (p(-4, 0) << 3)
                | (p(-3, 0) << 2)
                | (p(-2, 0) << 1)
                | p(-1, 0)) as usize
        }
        1 => {
            let a = a(a1.0, a1.1);
            (sample(cur, width, xi - 1)
                | (sample(cur, width, xi - 2) << 1)
                | (sample(cur, width, xi - 3) << 2)
                | (a << 3)
                | (sample(prev1, width, xi + 2) << 4)
                | (sample(prev1, width, xi + 1) << 5)
                | (sample(prev1, width, xi) << 6)
                | (sample(prev1, width, xi - 1) << 7)
                | (sample(prev1, width, xi - 2) << 8)
                | (sample(prev2, width, xi + 2) << 9)
                | (sample(prev2, width, xi + 1) << 10)
                | (sample(prev2, width, xi) << 11)
                | (sample(prev2, width, xi - 1) << 12)) as usize
        }
        2 => {
            let a = a(a1.0, a1.1);
            (sample(cur, width, xi - 1)
                | (sample(cur, width, xi - 2) << 1)
                | (a << 2)
                | (sample(prev1, width, xi + 1) << 3)
                | (sample(prev1, width, xi) << 4)
                | (sample(prev1, width, xi - 1) << 5)
                | (sample(prev1, width, xi - 2) << 6)
                | (sample(prev2, width, xi + 1) << 7)
                | (sample(prev2, width, xi) << 8)
                | (sample(prev2, width, xi - 1) << 9)) as usize
        }
        _ => {
            let a = a(a1.0, a1.1);
            (sample(cur, width, xi - 1)
                | (sample(cur, width, xi - 2) << 1)
                | (sample(cur, width, xi - 3) << 2)
                | (sample(cur, width, xi - 4) << 3)
                | (a << 4)
                | (sample(prev1, width, xi + 1) << 5)
                | (sample(prev1, width, xi) << 6)
                | (sample(prev1, width, xi - 1) << 7)
                | (sample(prev1, width, xi - 2) << 8)
                | (sample(prev1, width, xi - 3) << 9)) as usize
        }
    }
}

/// Dispatch to the correct template decoder. `template` selects the pixel
/// neighbourhood (0–3); values outside that range are clamped to 3's behaviour
/// by the parser (the flags field is only 2 bits wide, so `template <= 3`
/// always holds).
#[allow(clippy::too_many_arguments)]
fn decode_into(
    bitmap: &mut MonoBitmap,
    decoder: &mut ArithmeticDecoder<'_>,
    template: u8,
    at: [(i8, i8); 4],
    tpgdon: bool,
    contexts: &mut [MqContext],
    scratch: &mut GenericScratch,
) {
    match template {
        0 if at == NOMINAL_AT0 => {
            decode_template0_nominal(bitmap, decoder, contexts, tpgdon, scratch)
        }
        0 => decode_template0_general(bitmap, decoder, contexts, at, tpgdon, scratch),
        1 => decode_template1(bitmap, decoder, contexts, at[0], tpgdon, scratch),
        2 => decode_template2(bitmap, decoder, contexts, at[0], tpgdon, scratch),
        _ => decode_template3(bitmap, decoder, contexts, at[0], tpgdon, scratch),
    }
}

#[inline(always)]
fn sample(row: &[u32], width: u32, x: i64) -> u32 {
    if x < 0 || x >= width as i64 {
        return 0;
    }
    let x = x as usize;
    // row length is stride = ceil(width/32); x < width so x>>5 is in range.
    (row[x >> 5] >> (31 - (x & 31) as u32)) & 1
}

/// Fast rolling path for nominal AT (mirrors the encoder exactly). `tpgdon`
/// enables typical prediction (T.88 §6.2.5.7): an SLTP pseudo-pixel decoded
/// before each row toggles the LTP flag; while LTP is set, the row is copied
/// verbatim from the row above (all-zero for the first row).
fn decode_template0_nominal(
    bitmap: &mut MonoBitmap,
    decoder: &mut ArithmeticDecoder<'_>,
    contexts: &mut [MqContext],
    tpgdon: bool,
    scratch: &mut GenericScratch,
) {
    let width = bitmap.width();
    let height = bitmap.height();
    let stride = bitmap.stride_words() as usize;
    let (zero_row, cur) = scratch.rows(stride);
    let mut ltp = false;

    for y in 0..height {
        if tpgdon {
            let sltp = decoder.decode_bit(&mut contexts[SLTP_CTX_T0]);
            ltp ^= sltp;
            if ltp {
                copy_prev_row(bitmap, y, cur);
                continue;
            }
        }
        for w in cur.iter_mut() {
            *w = 0;
        }
        // Immutable borrows of previously decoded rows.
        let prev1: &[u32] = if y >= 1 { bitmap.row(y - 1) } else { zero_row };
        let prev2: &[u32] = if y >= 2 { bitmap.row(y - 2) } else { zero_row };

        let mut c1 = (sample(prev2, width, 0) << 2)
            | (sample(prev2, width, 1) << 1)
            | sample(prev2, width, 2);
        let mut c2 = (sample(prev1, width, 0) << 3)
            | (sample(prev1, width, 1) << 2)
            | (sample(prev1, width, 2) << 1)
            | sample(prev1, width, 3);
        let mut c3 = 0u32;

        for x in 0..width {
            let tval = ((c1 << 11) | (c2 << 4) | c3) as usize;
            // tval < 2^16 <= contexts.len(); indexing is in range.
            let bit = decoder.decode_bit(&mut contexts[tval]);
            if bit {
                let xi = x as usize;
                cur[xi >> 5] |= 1u32 << (31 - (xi & 31));
            }
            let xi = x as i64;
            c1 = ((c1 << 1) | sample(prev2, width, xi + 3)) & 31;
            c2 = ((c2 << 1) | sample(prev1, width, xi + 4)) & 127;
            c3 = ((c3 << 1) | bit as u32) & 15;
        }

        bitmap.row_mut(y).copy_from_slice(cur);
    }
}

/// Copy row `y-1` into row `y` (a TPGDON duplicated row). `scratch` is a stride
/// sized buffer used to break the aliasing borrow. For `y == 0` the copied row
/// is all-zero.
#[inline]
fn copy_prev_row(bitmap: &mut MonoBitmap, y: u32, scratch: &mut [u32]) {
    if y >= 1 {
        scratch.copy_from_slice(bitmap.row(y - 1));
    } else {
        for w in scratch.iter_mut() {
            *w = 0;
        }
    }
    bitmap.row_mut(y).copy_from_slice(scratch);
}

/// General per-pixel path for arbitrary AT positions. Produces identical
/// context values to the rolling path when `at == NOMINAL_AT0`.
fn decode_template0_general(
    bitmap: &mut MonoBitmap,
    decoder: &mut ArithmeticDecoder<'_>,
    contexts: &mut [MqContext],
    at: [(i8, i8); 4],
    tpgdon: bool,
    scratch: &mut GenericScratch,
) {
    let width = bitmap.width();
    let height = bitmap.height();
    let stride = bitmap.stride_words() as usize;
    let (zero_row, cur) = scratch.rows(stride);
    let mut ltp = false;

    // Context bit layout (MSB..LSB), matching the encoder's tval arrangement.
    // Fixed pixels and the four AT slots (nominal offsets in comments):
    //   bit15 AT4 (-2,-2)   bit14 (-1,-2)  bit13 (0,-2)  bit12 (1,-2)
    //   bit11 AT3 (2,-2)    bit10 AT2 (-3,-1) bit9 (-2,-1) bit8 (-1,-1)
    //   bit7  (0,-1)        bit6  (1,-1)   bit5 (2,-1)   bit4 AT1 (3,-1)
    //   bit3  (-4,0)        bit2  (-3,0)   bit1 (-2,0)   bit0 (-1,0)
    let (a1x, a1y) = (at[0].0 as i64, at[0].1 as i64);
    let (a2x, a2y) = (at[1].0 as i64, at[1].1 as i64);
    let (a3x, a3y) = (at[2].0 as i64, at[2].1 as i64);
    let (a4x, a4y) = (at[3].0 as i64, at[3].1 as i64);

    for y in 0..height {
        if tpgdon {
            let sltp = decoder.decode_bit(&mut contexts[SLTP_CTX_T0]);
            ltp ^= sltp;
            if ltp {
                copy_prev_row(bitmap, y, &mut *cur);
                continue;
            }
        }
        for w in cur.iter_mut() {
            *w = 0;
        }
        let prev1: &[u32] = if y >= 1 { bitmap.row(y - 1) } else { zero_row };
        let prev2: &[u32] = if y >= 2 { bitmap.row(y - 2) } else { zero_row };

        // Read a fixed template pixel (dy in {-2,-1,0}) from already-decoded
        // data. The four AT slots use `at_pixel`, which reads the bitmap so an
        // AT pixel may reference any earlier row, not just `y-1`/`y-2`.
        let px = |cur: &[u32], x: i64, dx: i64, dy: i64| -> u32 {
            let xx = x + dx;
            match dy {
                0 => {
                    // Current row: only x' < x is decoded.
                    if xx < 0 || xx >= x || xx >= width as i64 {
                        0
                    } else {
                        sample(cur, width, xx)
                    }
                }
                -1 => sample(prev1, width, xx),
                _ => sample(prev2, width, xx),
            }
        };

        for x in 0..width {
            let x = x as i64;
            let mut t = 0u32;
            t |= at_pixel(bitmap, &*cur, width, y, x, a4x, a4y) << 15;
            t |= px(&*cur, x, -1, -2) << 14;
            t |= px(&*cur, x, 0, -2) << 13;
            t |= px(&*cur, x, 1, -2) << 12;
            t |= at_pixel(bitmap, &*cur, width, y, x, a3x, a3y) << 11;
            t |= at_pixel(bitmap, &*cur, width, y, x, a2x, a2y) << 10;
            t |= px(&*cur, x, -2, -1) << 9;
            t |= px(&*cur, x, -1, -1) << 8;
            t |= px(&*cur, x, 0, -1) << 7;
            t |= px(&*cur, x, 1, -1) << 6;
            t |= px(&*cur, x, 2, -1) << 5;
            t |= at_pixel(bitmap, &*cur, width, y, x, a1x, a1y) << 4;
            t |= px(&*cur, x, -4, 0) << 3;
            t |= px(&*cur, x, -3, 0) << 2;
            t |= px(&*cur, x, -2, 0) << 1;
            t |= px(&*cur, x, -1, 0);

            let bit = decoder.decode_bit(&mut contexts[t as usize]);
            if bit {
                let xi = x as usize;
                cur[xi >> 5] |= 1u32 << (31 - (xi & 31));
            }
        }
        bitmap.row_mut(y).copy_from_slice(cur);
    }
}

/// Sample an adaptive-template pixel at `(x+dx, y+dy)` relative to the pixel
/// being decoded at row `y`, reading only causal (already-decoded) data.
///
/// A generic-region AT pixel may reference any earlier row (`Ay` down to −128,
/// T.88 §7.4.6.3), so — unlike the fixed template pixels, which never reach past
/// `y−2` — this reads the destination bitmap directly rather than a pair of
/// cached `prev` rows. `dy == 0` is the current row (only columns `< x` are
/// decoded); `dy < 0` reads row `y+dy` (all-zero above the top edge); `dy > 0`
/// (never emitted for a valid AT pixel) reads nothing.
#[inline(always)]
fn at_pixel(bitmap: &MonoBitmap, cur: &[u32], width: u32, y: u32, x: i64, dx: i64, dy: i64) -> u32 {
    let xx = x + dx;
    if xx < 0 || xx >= width as i64 {
        return 0;
    }
    if dy == 0 {
        if xx >= x { 0 } else { sample(cur, width, xx) }
    } else {
        let yy = y as i64 + dy;
        if yy < 0 || dy > 0 {
            0
        } else {
            sample(bitmap.row(yy as u32), width, xx)
        }
    }
}

/// Generic template 1 (T.88 §6.2.5.3, Figure 5): 13-bit context, one AT pixel.
/// Context numbering matches T.88/jbig2dec so the SLTP constant is the literal
/// spec value.
fn decode_template1(
    bitmap: &mut MonoBitmap,
    decoder: &mut ArithmeticDecoder<'_>,
    contexts: &mut [MqContext],
    at1: (i8, i8),
    tpgdon: bool,
    scratch: &mut GenericScratch,
) {
    let width = bitmap.width();
    let height = bitmap.height();
    let stride = bitmap.stride_words() as usize;
    let (zero_row, cur) = scratch.rows(stride);
    let mut ltp = false;
    let (a1x, a1y) = (at1.0 as i64, at1.1 as i64);

    for y in 0..height {
        if tpgdon {
            let sltp = decoder.decode_bit(&mut contexts[SLTP_CTX_T1]);
            ltp ^= sltp;
            if ltp {
                copy_prev_row(bitmap, y, &mut *cur);
                continue;
            }
        }
        for w in cur.iter_mut() {
            *w = 0;
        }
        let prev1: &[u32] = if y >= 1 { bitmap.row(y - 1) } else { zero_row };
        let prev2: &[u32] = if y >= 2 { bitmap.row(y - 2) } else { zero_row };

        for x in 0..width {
            let xi = x as i64;
            let mut t = 0u32;
            t |= sample(&*cur, width, xi - 1);
            t |= sample(&*cur, width, xi - 2) << 1;
            t |= sample(&*cur, width, xi - 3) << 2;
            t |= at_pixel(bitmap, &*cur, width, y, xi, a1x, a1y) << 3;
            t |= sample(prev1, width, xi + 2) << 4;
            t |= sample(prev1, width, xi + 1) << 5;
            t |= sample(prev1, width, xi) << 6;
            t |= sample(prev1, width, xi - 1) << 7;
            t |= sample(prev1, width, xi - 2) << 8;
            t |= sample(prev2, width, xi + 2) << 9;
            t |= sample(prev2, width, xi + 1) << 10;
            t |= sample(prev2, width, xi) << 11;
            t |= sample(prev2, width, xi - 1) << 12;

            let bit = decoder.decode_bit(&mut contexts[t as usize]);
            if bit {
                let xu = x as usize;
                cur[xu >> 5] |= 1u32 << (31 - (xu & 31));
            }
        }
        bitmap.row_mut(y).copy_from_slice(cur);
    }
}

/// Generic template 2 (T.88 §6.2.5.3, Figure 6): 10-bit context, one AT pixel.
fn decode_template2(
    bitmap: &mut MonoBitmap,
    decoder: &mut ArithmeticDecoder<'_>,
    contexts: &mut [MqContext],
    at1: (i8, i8),
    tpgdon: bool,
    scratch: &mut GenericScratch,
) {
    let width = bitmap.width();
    let height = bitmap.height();
    let stride = bitmap.stride_words() as usize;
    let (zero_row, cur) = scratch.rows(stride);
    let mut ltp = false;
    let (a1x, a1y) = (at1.0 as i64, at1.1 as i64);

    for y in 0..height {
        if tpgdon {
            let sltp = decoder.decode_bit(&mut contexts[SLTP_CTX_T2]);
            ltp ^= sltp;
            if ltp {
                copy_prev_row(bitmap, y, &mut *cur);
                continue;
            }
        }
        for w in cur.iter_mut() {
            *w = 0;
        }
        let prev1: &[u32] = if y >= 1 { bitmap.row(y - 1) } else { zero_row };
        let prev2: &[u32] = if y >= 2 { bitmap.row(y - 2) } else { zero_row };

        for x in 0..width {
            let xi = x as i64;
            let mut t = 0u32;
            t |= sample(&*cur, width, xi - 1);
            t |= sample(&*cur, width, xi - 2) << 1;
            t |= at_pixel(bitmap, &*cur, width, y, xi, a1x, a1y) << 2;
            t |= sample(prev1, width, xi + 1) << 3;
            t |= sample(prev1, width, xi) << 4;
            t |= sample(prev1, width, xi - 1) << 5;
            t |= sample(prev1, width, xi - 2) << 6;
            t |= sample(prev2, width, xi + 1) << 7;
            t |= sample(prev2, width, xi) << 8;
            t |= sample(prev2, width, xi - 1) << 9;

            let bit = decoder.decode_bit(&mut contexts[t as usize]);
            if bit {
                let xu = x as usize;
                cur[xu >> 5] |= 1u32 << (31 - (xu & 31));
            }
        }
        bitmap.row_mut(y).copy_from_slice(cur);
    }
}

/// Generic template 3 (T.88 §6.2.5.3, Figure 7): 10-bit context, one AT pixel,
/// two rows only (no `y-2` line).
fn decode_template3(
    bitmap: &mut MonoBitmap,
    decoder: &mut ArithmeticDecoder<'_>,
    contexts: &mut [MqContext],
    at1: (i8, i8),
    tpgdon: bool,
    scratch: &mut GenericScratch,
) {
    let width = bitmap.width();
    let height = bitmap.height();
    let stride = bitmap.stride_words() as usize;
    let (zero_row, cur) = scratch.rows(stride);
    let mut ltp = false;
    let (a1x, a1y) = (at1.0 as i64, at1.1 as i64);

    for y in 0..height {
        if tpgdon {
            let sltp = decoder.decode_bit(&mut contexts[SLTP_CTX_T3]);
            ltp ^= sltp;
            if ltp {
                copy_prev_row(bitmap, y, &mut *cur);
                continue;
            }
        }
        for w in cur.iter_mut() {
            *w = 0;
        }
        let prev1: &[u32] = if y >= 1 { bitmap.row(y - 1) } else { zero_row };
        // Template 3's fixed pixels use only rows y and y-1. Its AT pixel may
        // still reference an earlier row, so `at_pixel` reads the bitmap.

        for x in 0..width {
            let xi = x as i64;
            let mut t = 0u32;
            t |= sample(&*cur, width, xi - 1);
            t |= sample(&*cur, width, xi - 2) << 1;
            t |= sample(&*cur, width, xi - 3) << 2;
            t |= sample(&*cur, width, xi - 4) << 3;
            t |= at_pixel(bitmap, &*cur, width, y, xi, a1x, a1y) << 4;
            t |= sample(prev1, width, xi + 1) << 5;
            t |= sample(prev1, width, xi) << 6;
            t |= sample(prev1, width, xi - 1) << 7;
            t |= sample(prev1, width, xi - 2) << 8;
            t |= sample(prev1, width, xi - 3) << 9;

            let bit = decoder.decode_bit(&mut contexts[t as usize]);
            if bit {
                let xu = x as usize;
                cur[xu >> 5] |= 1u32 << (31 - (xu & 31));
            }
        }
        bitmap.row_mut(y).copy_from_slice(cur);
    }
}

#[cfg(all(test, feature = "encode"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::decode::context::GENERIC_CONTEXT_COUNT;
    use crate::encode::arith::Jbig2ArithCoder;
    use crate::encode::sym::BitImage;

    fn contexts() -> Vec<MqContext> {
        vec![MqContext::default(); GENERIC_CONTEXT_COUNT]
    }

    /// Encode `img` as a raw generic payload and decode it back, asserting a
    /// pixel-exact round-trip.
    fn roundtrip_bitimage(img: &BitImage) {
        let data = Jbig2ArithCoder::encode_generic_payload(img, 0, &nominal_at0_i8()).unwrap();
        let (w, h) = (img.width as u32, img.height as u32);
        let region = GenericRegion {
            width: w,
            height: h,
            x: 0,
            y: 0,
            comb_operator: 0,
            mmr: false,
            template: 0,
            tpgdon: false,
            at: NOMINAL_AT0,
            data: &data,
        };
        let limits = DecodeLimits::default();
        let mut ctx = contexts();
        let mut scratch = GenericScratch::default();
        let bm = decode_generic_region(&region, &limits, &mut ctx, &mut scratch).unwrap();
        for y in 0..h {
            for x in 0..w {
                assert_eq!(bm.get(x, y), img.get(x, y), "pixel ({x},{y}) w={w} h={h}");
            }
        }
    }

    fn nominal_at0_i8() -> Vec<(i8, i8)> {
        NOMINAL_AT0.to_vec()
    }

    fn make_image(w: u32, h: u32, seed: u32) -> BitImage {
        let mut img = BitImage::new(w, h).unwrap();
        let mut s = seed.wrapping_add(1);
        for y in 0..h {
            for x in 0..w {
                s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                if (s >> 16) & 1 == 1 {
                    img.set(x, y, true);
                }
            }
        }
        img
    }

    #[test]
    fn roundtrip_odd_widths() {
        for &w in &[1u32, 2, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65] {
            for &h in &[1u32, 3, 8, 17] {
                roundtrip_bitimage(&make_image(w, h, w * 31 + h));
            }
        }
    }

    #[test]
    fn nominal_and_general_paths_agree() {
        // Decode the same stream with both paths and compare.
        let img = make_image(37, 20, 7);
        let data = Jbig2ArithCoder::encode_generic_payload(&img, 0, &nominal_at0_i8()).unwrap();
        let limits = DecodeLimits::default();

        let mut scratch = GenericScratch::default();
        let mut bm_fast = MonoBitmap::new(37, 20, false, &limits).unwrap();
        let mut d1 = ArithmeticDecoder::new(&data);
        let mut c1 = contexts();
        decode_template0_nominal(&mut bm_fast, &mut d1, &mut c1, false, &mut scratch);

        let mut bm_gen = MonoBitmap::new(37, 20, false, &limits).unwrap();
        let mut d2 = ArithmeticDecoder::new(&data);
        let mut c2 = contexts();
        decode_template0_general(
            &mut bm_gen,
            &mut d2,
            &mut c2,
            NOMINAL_AT0,
            false,
            &mut scratch,
        );

        assert_eq!(bm_fast, bm_gen);
    }
}
