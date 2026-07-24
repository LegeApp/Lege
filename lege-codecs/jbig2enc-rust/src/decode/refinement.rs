//! Generic refinement-region decoding, GRTEMPLATE 0 (jbig2decplan.md §17, T.88
//! §6.3).
//!
//! Inverts the encoder's
//! [`crate::encode::arith::Jbig2ArithCoder::encode_refinement_region`]: a 13-bit
//! context is formed from a 3×3 reference neighbourhood, three already-decoded
//! target pixels, and one target adaptive-template pixel (GRAT1, nominally
//! `(-1, -1)`). The reference is offset from the target by `(GRDX, GRDY)`.
//!
//! The context bank is shared across every refined instance of one text region
//! (T.88 §6.4.11 carries the GR statistics across instances) and is *not* reset
//! per instance — the caller resets it once per text-region segment.

use crate::decode::arith::ArithmeticDecoder;
use crate::decode::error::{DecodeError, LimitError};
use crate::shared::bitmap::MonoBitmap;
use crate::shared::limits::DecodeLimits;
use crate::shared::mq_table::MqContext;
use crate::shared::reader::Reader;

/// The number of refinement contexts. GRTEMPLATE-0 uses a 13-bit context;
/// GRTEMPLATE-1 uses a 10-bit context that fits in the same bank.
pub const REFINEMENT_CONTEXT_COUNT: usize = 1 << 13;

// SLTP pseudo-pixel contexts for TPGRON (T.88 §6.3.5.6, Figures 14/15 set the
// centre reference pixel to 1). The SLTP context is the index whose only set
// bit is the centre-reference pixel GRREFERENCE(w, h). In this crate's context
// numbering that pixel is bit 4 for GRTEMPLATE-0 (0x010) and bit 7 for
// GRTEMPLATE-1 (0x080). These match pdfium's CJBig2_GRRDProc, which decodes
// SLTP with grContexts[0x0010] (template 0) and grContexts[0x0008] (template 1)
// — both the centre reference in pdfium's own numbering. The earlier 0x040
// (bit 6 = GRREFERENCE(w+1, h)) desynced every template-1 TPGRON region
// (pdf.js bitmap-refine-template1-tpgron).
const SLTP_CTX_GR0: usize = 0x010;
const SLTP_CTX_GR1: usize = 0x080;

/// A parsed standalone generic-refinement-region segment header (T.88 §7.4.7),
/// plus the arithmetic-coded payload that follows it.
pub struct RefinementRegionSegment<'a> {
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    /// External combination operator (region-info flags low 3 bits).
    pub comb_operator: u8,
    /// GRTEMPLATE (0 or 1).
    pub grtemplate: u8,
    /// TPGRON typical-prediction flag.
    pub tpgron: bool,
    /// GRAT1 (target adaptive-template pixel), nominally `(-1, -1)`. Unused for
    /// GRTEMPLATE-1, which has no adaptive pixel.
    pub grat: (i8, i8),
    /// GRAT2 (reference adaptive-template pixel), nominally `(-1, -1)`. Forms
    /// context bit 0 for GRTEMPLATE-0 (T.88 Fig. 12); unused for GRTEMPLATE-1.
    pub grat2: (i8, i8),
    pub data: &'a [u8],
}

/// Parse a generic refinement region segment data header (T.88 §7.4.7.1–7.4.7.3).
pub fn parse_refinement_region(payload: &[u8]) -> Result<RefinementRegionSegment<'_>, DecodeError> {
    let mut r = Reader::new(payload);
    // §7.4.1 region segment information field.
    let width = r.read_u32_be()?;
    let height = r.read_u32_be()?;
    let x = r.read_u32_be()?;
    let y = r.read_u32_be()?;
    let region_flags = r.read_u8()?;
    let comb_operator = region_flags & 0x07;

    // §7.4.7.2 generic refinement region segment flags.
    let flags = r.read_u8()?;
    let grtemplate = flags & 0x01;
    let tpgron = flags & 0x02 != 0;

    // §7.4.7.3 AT flags: present only when GRTEMPLATE=0 (two AT pairs, 4 bytes).
    // Only GRAT1 is used by the template-0 context; template 1 has no AT pixel.
    let (grat, grat2) = if grtemplate == 0 {
        let g1x = r.read_i8()?;
        let g1y = r.read_i8()?;
        let g2x = r.read_i8()?;
        let g2y = r.read_i8()?;
        ((g1x, g1y), (g2x, g2y))
    } else {
        ((-1, -1), (-1, -1))
    };

    let data = &payload[r.position()..];
    Ok(RefinementRegionSegment {
        width,
        height,
        x,
        y,
        comb_operator,
        grtemplate,
        tpgron,
        grat,
        grat2,
        data,
    })
}

/// Extract the page-buffer sub-region `[x, y, width, height]` as a fresh bitmap,
/// the `GRREFERENCE` for a standalone refinement region with no referred region
/// segment (T.88 §7.4.7.4). Pixels of the box that fall outside the page are 0.
pub fn page_reference_window(
    page: &MonoBitmap,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    limits: &DecodeLimits,
) -> Result<MonoBitmap, DecodeError> {
    let pixels = (width as u64)
        .checked_mul(height as u64)
        .ok_or(DecodeError::Overflow {
            operation: "refinement window pixels",
        })?;
    if pixels > limits.max_page_pixels {
        return Err(DecodeError::limit(LimitError::Pixels {
            what: "refinement window",
            value: pixels,
            limit: limits.max_page_pixels,
        }));
    }
    let mut window = MonoBitmap::new(width, height, false, limits)?;
    let pw = page.width();
    let ph = page.height();
    for wy in 0..height {
        let py = match y.checked_add(wy) {
            Some(v) if v < ph => v,
            _ => continue,
        };
        for wx in 0..width {
            let px = match x.checked_add(wx) {
                Some(v) if v < pw => v,
                _ => continue,
            };
            if page.get(px, py) {
                window.set(wx, wy, true);
            }
        }
    }
    Ok(window)
}

/// Read a reference/target pixel at signed coordinates, zero outside the bitmap.
#[inline]
fn pget(bm: &MonoBitmap, x: i64, y: i64) -> u32 {
    if x < 0 || y < 0 || x >= bm.width() as i64 || y >= bm.height() as i64 {
        0
    } else {
        bm.get(x as u32, y as u32) as u32
    }
}

/// Decode a GRTEMPLATE-0 generic refinement region (back-compat wrapper).
#[allow(clippy::too_many_arguments)]
pub fn decode_refinement_region(
    dec: &mut ArithmeticDecoder<'_>,
    reference: &MonoBitmap,
    width: u32,
    height: u32,
    grdx: i32,
    grdy: i32,
    grat: (i8, i8),
    contexts: &mut [MqContext],
    limits: &DecodeLimits,
) -> Result<MonoBitmap, DecodeError> {
    decode_refinement_region_templated(
        dec,
        reference,
        width,
        height,
        grdx,
        grdy,
        0,
        false,
        grat,
        (-1, -1),
        contexts,
        limits,
    )
}

/// Decode a generic refinement region into a fresh `width × height` bitmap,
/// using `reference` (offset by `(grdx, grdy)`) as the predictor (T.88 §6.3).
/// `grtemplate` selects the 13-bit (0) or 10-bit (1) context; `tpgron` enables
/// typical prediction (§6.3.5.6). `contexts` must be at least
/// [`REFINEMENT_CONTEXT_COUNT`] entries and is *not* reset here.
#[allow(clippy::too_many_arguments)]
pub fn decode_refinement_region_templated(
    dec: &mut ArithmeticDecoder<'_>,
    reference: &MonoBitmap,
    width: u32,
    height: u32,
    grdx: i32,
    grdy: i32,
    grtemplate: u8,
    tpgron: bool,
    grat: (i8, i8),
    grat2: (i8, i8),
    contexts: &mut [MqContext],
    limits: &DecodeLimits,
) -> Result<MonoBitmap, DecodeError> {
    if contexts.len() < REFINEMENT_CONTEXT_COUNT {
        return Err(DecodeError::Overflow {
            operation: "refinement context array too small",
        });
    }
    let mut target = MonoBitmap::new(width, height, false, limits)?;
    if width == 0 || height == 0 {
        return Ok(target);
    }

    let grat_x = grat.0 as i64;
    let grat_y = grat.1 as i64;
    let grat2_x = grat2.0 as i64;
    let grat2_y = grat2.1 as i64;
    let sltp_ctx = if grtemplate == 0 {
        SLTP_CTX_GR0
    } else {
        SLTP_CTX_GR1
    };
    let mut ltp = false;

    for y in 0..height as i64 {
        if tpgron {
            let sltp = dec.decode_bit(&mut contexts[sltp_ctx]);
            ltp ^= sltp;
        }
        for x in 0..width as i64 {
            let rx = x - grdx as i64;
            let ry = y - grdy as i64;

            // §6.3.5.6 3d): in a typical (LTP) row, a pixel whose 3×3 reference
            // neighbourhood is uniform is set to that value without decoding.
            // Kept nested so `typical_pixel` is skipped entirely when LTP is 0
            // (the common non-TPGRON path).
            #[allow(clippy::collapsible_if)]
            if ltp {
                if let Some(v) = typical_pixel(reference, rx, ry) {
                    if v {
                        target.set(x as u32, y as u32, true);
                    }
                    continue;
                }
            }

            let cx = if grtemplate == 0 {
                context_gr0(
                    &target, reference, x, y, rx, ry, grat_x, grat_y, grat2_x, grat2_y,
                )
            } else {
                context_gr1(&target, reference, x, y, rx, ry)
            };

            let bit = dec.decode_bit(&mut contexts[cx]);
            if bit {
                target.set(x as u32, y as u32, true);
            }
        }
    }

    Ok(target)
}

/// GRTEMPLATE-0 context (T.88 Figure 12), matching the encoder exactly.
#[allow(clippy::too_many_arguments)]
#[inline]
fn context_gr0(
    target: &MonoBitmap,
    reference: &MonoBitmap,
    x: i64,
    y: i64,
    rx: i64,
    ry: i64,
    grat_x: i64,
    grat_y: i64,
    grat2_x: i64,
    grat2_y: i64,
) -> usize {
    let mut cx: usize = 0;
    // Bit 0 is the GRAT2 adaptive *reference* pixel (T.88 Fig. 12), nominally
    // (rx-1, ry-1). pdfium's DecodeTemplate0UnoptCalculateContext reads
    // GRREFERENCE(w + GRAT[2], h + GRAT[3]) for this slot — there is no fixed
    // (rx-1, ry-1) reference pixel in the template — so a custom GRAT2 must be
    // honoured or every custom-AT refinement region desyncs (pdf.js
    // bitmap-refine-customat).
    cx |= pget(reference, rx + grat2_x, ry + grat2_y) as usize;
    cx |= (pget(reference, rx, ry - 1) as usize) << 1;
    cx |= (pget(reference, rx + 1, ry - 1) as usize) << 2;
    cx |= (pget(reference, rx - 1, ry) as usize) << 3;
    cx |= (pget(reference, rx, ry) as usize) << 4;
    cx |= (pget(reference, rx + 1, ry) as usize) << 5;
    cx |= (pget(target, x - 1, y) as usize) << 6;
    cx |= (pget(reference, rx - 1, ry + 1) as usize) << 7;
    cx |= (pget(reference, rx, ry + 1) as usize) << 8;
    cx |= (pget(reference, rx + 1, ry + 1) as usize) << 9;
    cx |= (pget(target, x + 1, y - 1) as usize) << 10;
    cx |= (pget(target, x, y - 1) as usize) << 11;
    cx |= (pget(target, x + grat_x, y + grat_y) as usize) << 12;
    cx
}

/// GRTEMPLATE-1 context (T.88 Figure 13), 10 bits, no AT pixel. Target pixels
/// `(-1,-1),(0,-1),(1,-1),(-1,0)`; reference pixels
/// `(0,-1),(-1,0),(0,0),(1,0),(0,1),(1,1)`.
#[inline]
fn context_gr1(
    target: &MonoBitmap,
    reference: &MonoBitmap,
    x: i64,
    y: i64,
    rx: i64,
    ry: i64,
) -> usize {
    let mut cx: usize = 0;
    cx |= pget(target, x - 1, y) as usize;
    cx |= (pget(target, x + 1, y - 1) as usize) << 1;
    cx |= (pget(target, x, y - 1) as usize) << 2;
    cx |= (pget(target, x - 1, y - 1) as usize) << 3;
    cx |= (pget(reference, rx + 1, ry + 1) as usize) << 4;
    cx |= (pget(reference, rx, ry + 1) as usize) << 5;
    cx |= (pget(reference, rx + 1, ry) as usize) << 6;
    cx |= (pget(reference, rx, ry) as usize) << 7;
    cx |= (pget(reference, rx - 1, ry) as usize) << 8;
    cx |= (pget(reference, rx, ry - 1) as usize) << 9;
    cx
}

/// If the 3×3 reference neighbourhood centred at `(rx, ry)` is uniform, return
/// `Some(common_value)`; otherwise `None` (the pixel must be decoded).
#[inline]
fn typical_pixel(reference: &MonoBitmap, rx: i64, ry: i64) -> Option<bool> {
    let mut sum = 0u32;
    for dy in -1..=1 {
        for dx in -1..=1 {
            sum += pget(reference, rx + dx, ry + dy);
        }
    }
    match sum {
        0 => Some(false),
        9 => Some(true),
        _ => None,
    }
}

#[cfg(all(test, feature = "encode"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::encode::arith::Jbig2ArithCoder;
    use crate::encode::sym::BitImage;

    /// Round-trip: encode the difference between a target and a reference glyph
    /// with the encoder's refinement coder, then decode it back exactly.
    #[test]
    fn refinement_roundtrip() {
        let limits = DecodeLimits::default();
        // Reference glyph.
        let mut refimg = BitImage::new(12, 16).unwrap();
        for y in 0..16 {
            for x in 0..12 {
                if x == 0 || x == 11 || y == 0 || y == 15 {
                    refimg.set(x, y, true);
                }
            }
        }
        // Target: reference with a few pixels toggled (same size, GRDX=GRDY=0).
        let mut target = refimg.clone();
        target.set(5, 5, true);
        target.set(6, 8, true);
        target.set(2, 10, false);

        let mut enc = Jbig2ArithCoder::new();
        enc.encode_refinement_region(&target, &refimg, 0, 0, 0, &[(-1, -1)])
            .unwrap();
        let data = enc.into_vec();

        let ref_mono = MonoBitmap::from_bit_image(&refimg, &limits).unwrap();
        let mut dec = ArithmeticDecoder::new(&data);
        let mut ctx = vec![MqContext::default(); REFINEMENT_CONTEXT_COUNT];
        let out = decode_refinement_region(
            &mut dec,
            &ref_mono,
            12,
            16,
            0,
            0,
            (-1, -1),
            &mut ctx,
            &limits,
        )
        .unwrap();

        for y in 0..16u32 {
            for x in 0..12u32 {
                assert_eq!(out.get(x, y), target.get(x, y), "pixel ({x},{y})");
            }
        }
    }
}

// Decoder-only regression: the GRTEMPLATE-0 refinement context must read the
// *reference* adaptive pixel at the parsed GRAT2 offset (T.88 Fig. 12, bit 8 in
// pdfium's numbering / bit 0 here), not a fixed (rx-1, ry-1). A custom SDRAT2 /
// SBRAT2 that is dropped desyncs every custom-AT refinement region — the
// pdf.js bitmap-symbol-symbolrefineone-customat fixture blanked because of it.
#[cfg(test)]
mod grat2_tests {
    use super::*;

    #[test]
    fn context_gr0_reads_the_custom_grat2_reference_pixel() {
        let limits = DecodeLimits::default();
        let target = MonoBitmap::new(8, 8, false, &limits).unwrap();
        let mut reference = MonoBitmap::new(8, 8, false, &limits).unwrap();
        // A lone black pixel far from the fixed neighbourhood window around
        // (rx±1, ry±1) = (4..=6, 4..=6), so only the GRAT2 tap can see it.
        reference.set(2, 3, true);

        let (x, y, rx, ry) = (5i64, 5i64, 5i64, 5i64);
        // Nominal GRAT1 target pixel, so bit 12 stays 0.
        let (grat_x, grat_y) = (-1i64, -1i64);

        // Nominal GRAT2 = (-1,-1) reads reference(4,4) = white → context 0.
        let nominal = context_gr0(&target, &reference, x, y, rx, ry, grat_x, grat_y, -1, -1);
        assert_eq!(nominal, 0, "nominal GRAT2 must not see the (2,3) pixel");

        // Custom GRAT2 pointing at (2,3): rx+dx=2, ry+dy=3 → dx=-3, dy=-2.
        let custom = context_gr0(&target, &reference, x, y, rx, ry, grat_x, grat_y, -3, -2);
        assert_eq!(
            custom, 1,
            "custom GRAT2 must read the reference pixel into bit 0"
        );
    }
}
