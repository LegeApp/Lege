//! Halftone-region decoding (jbig2decplan.md §18, T.88 §6.6 / §7.4.5).
//!
//! A halftone region reconstructs a tone image by tiling patterns from a
//! referred pattern dictionary onto a grid. The per-cell pattern index is a
//! gray-scale value decoded as `HBPP = ⌈log2(HNUMPATS)⌉` Gray-coded bitplanes
//! (Annex C.5): the most-significant plane is decoded first and each lower plane
//! is XORed with the next-more-significant to recover the binary value. Both the
//! arithmetic and MMR gray-plane variants are supported (the encoder emits both
//! depending on config). Grid placement uses fixed-point (8-bit fraction)
//! arithmetic with `i64` intermediates, validated before narrowing to page
//! coordinates (T.88 §6.6.5.2).
//!
//! `HENABLESKIP` is parsed but, since the encoder never emits it, surfaces as a
//! typed `Unsupported` error.

use crate::decode::arith::ArithmeticDecoder;
use crate::decode::error::{DecodeError, ParseError};
use crate::decode::generic::{GenericScratch, decode_generic_bitmap};
use crate::decode::mmr::decode_mmr_plane;
use crate::decode::pattern_dictionary::PatternDictionary;
use crate::shared::bitmap::{CombinationOperator, MonoBitmap};
use crate::shared::limits::DecodeLimits;
use crate::shared::mq_table::MqContext;
use crate::shared::reader::Reader;

/// A parsed halftone-region segment: geometry, coding flags, grid parameters,
/// and the coded gray-scale image data.
#[derive(Clone, Debug)]
pub struct HalftoneRegion<'a> {
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    /// External combination operator (region flags low 3 bits).
    pub comb_operator: u8,
    pub mmr: bool,
    pub template: u8,
    pub enable_skip: bool,
    /// Internal (pattern → region) combination operator, HCOMBOP.
    pub halftone_comb_operator: u8,
    pub default_pixel: bool,
    pub grid_width: u32,    // HGW
    pub grid_height: u32,   // HGH
    pub grid_x: i32,        // HGX, 24.8 fixed-point
    pub grid_y: i32,        // HGY, 24.8 fixed-point
    pub grid_vector_x: u16, // HRX, 8.8 fixed-point
    pub grid_vector_y: u16, // HRY, 8.8 fixed-point
    pub data: &'a [u8],
}

/// Parse a halftone-region segment payload (T.88 §7.4.5).
pub fn parse_halftone_region(payload: &[u8]) -> Result<HalftoneRegion<'_>, ParseError> {
    let mut r = Reader::new(payload);
    // §7.4.1 region segment information field.
    let width = r.read_u32_be()?;
    let height = r.read_u32_be()?;
    let x = r.read_u32_be()?;
    let y = r.read_u32_be()?;
    let region_flags = r.read_u8()?;
    let comb_operator = region_flags & 0x07;

    // §7.4.5.1.1 halftone region segment flags.
    let flags = r.read_u8()?;
    let mmr = flags & 0x01 != 0;
    let template = (flags >> 1) & 0x03;
    let enable_skip = flags & 0x08 != 0;
    let halftone_comb_operator = (flags >> 4) & 0x07;
    let default_pixel = flags & 0x80 != 0;

    // §7.4.5.1.2 halftone grid position and vector.
    let grid_width = r.read_u32_be()?;
    let grid_height = r.read_u32_be()?;
    let grid_x = r.read_i32_be()?;
    let grid_y = r.read_i32_be()?;
    let grid_vector_x = r.read_u16_be()?;
    let grid_vector_y = r.read_u16_be()?;

    let data = &payload[r.position()..];
    Ok(HalftoneRegion {
        width,
        height,
        x,
        y,
        comb_operator,
        mmr,
        template,
        enable_skip,
        halftone_comb_operator,
        default_pixel,
        grid_width,
        grid_height,
        grid_x,
        grid_y,
        grid_vector_x,
        grid_vector_y,
        data,
    })
}

#[inline]
fn combination_operator(code: u8) -> CombinationOperator {
    match code {
        0 => CombinationOperator::Or,
        1 => CombinationOperator::And,
        2 => CombinationOperator::Xor,
        3 => CombinationOperator::Xnor,
        _ => CombinationOperator::Replace,
    }
}

/// `HBPP = ⌈log2(HNUMPATS)⌉` (T.88 §6.6.5 step 3).
#[inline]
fn bits_per_value(num_patterns: usize) -> u32 {
    if num_patterns <= 1 {
        0
    } else {
        // Number of bits to represent values 0..num_patterns-1.
        32 - ((num_patterns - 1) as u32).leading_zeros()
    }
}

/// Decode a halftone region against its (already resolved) pattern dictionary,
/// returning the region bitmap. `generic_ctx` is reused, worker-local scratch
/// (reset here, must be at least `1 << 16` entries); it is only touched for the
/// arithmetic gray-plane variant.
/// Compute the HSKIP bitmap (T.88 §6.6.5.1): a grid cell is skipped when its
/// pattern, placed by the grid geometry, lies entirely outside the region.
fn compute_skip(
    region: &HalftoneRegion<'_>,
    patterns: &PatternDictionary,
    hgw: u32,
    hgh: u32,
    limits: &DecodeLimits,
) -> Result<MonoBitmap, DecodeError> {
    let mut skip = MonoBitmap::new(hgw, hgh, false, limits)?;
    let hpw = patterns.patterns[0].width() as i64;
    let hph = patterns.patterns[0].height() as i64;
    let hbw = region.width as i64;
    let hbh = region.height as i64;
    let hgx = region.grid_x as i64;
    let hgy = region.grid_y as i64;
    let hrx = region.grid_vector_x as i64;
    let hry = region.grid_vector_y as i64;
    for mg in 0..hgh {
        let mg_i = mg as i64;
        for ng in 0..hgw {
            let ng_i = ng as i64;
            let x = (hgx + mg_i * hry + ng_i * hrx) >> 8;
            let y = (hgy + mg_i * hrx - ng_i * hry) >> 8;
            if x + hpw <= 0 || x >= hbw || y + hph <= 0 || y >= hbh {
                skip.set(ng, mg, true);
            }
        }
    }
    Ok(skip)
}

pub fn decode_halftone_region(
    region: &HalftoneRegion<'_>,
    patterns: &PatternDictionary,
    limits: &DecodeLimits,
    generic_ctx: &mut [MqContext],
    scratch: &mut GenericScratch,
) -> Result<MonoBitmap, DecodeError> {
    // §6.6.5 step 1: fill HTREG with HDEFPIXEL.
    let mut htreg = MonoBitmap::new(region.width, region.height, region.default_pixel, limits)?;
    if region.width == 0 || region.height == 0 {
        return Ok(htreg);
    }

    let num_patterns = patterns.len();
    if num_patterns == 0 {
        return Err(DecodeError::Malformed {
            reason: "halftone region referred an empty pattern dictionary",
        });
    }
    let hbpp = bits_per_value(num_patterns);

    let hgw = region.grid_width;
    let hgh = region.grid_height;
    if hgw == 0 || hgh == 0 {
        return Ok(htreg);
    }

    // §6.6.5.1 HSKIP: a cell whose pattern placement falls entirely outside the
    // region is skipped in the gray-scale decoding. Per Annex C.5 GSUSESKIP
    // applies only to the *arithmetic* gray planes; MMR planes are coded whole
    // (the outside cells are handled by render-time clipping), so no skip bitmap
    // is needed for them.
    let skip = if region.enable_skip && !region.mmr {
        Some(compute_skip(region, patterns, hgw, hgh, limits)?)
    } else {
        None
    };

    // §6.6.5 step 4 + Annex C.5: decode HBPP Gray-coded planes, MSB first.
    // `planes[j]` holds the coded (still Gray-coded) plane for bit position j.
    let mut planes: Vec<MonoBitmap> = Vec::with_capacity(hbpp as usize);
    for _ in 0..hbpp {
        planes.push(MonoBitmap::new(hgw, hgh, false, limits)?);
    }

    if region.mmr {
        // Each plane is a separate byte-aligned EOFB-terminated MMR block.
        let mut off = 0usize;
        for j in (0..hbpp as usize).rev() {
            let (plane, consumed) = decode_mmr_plane(&region.data[off..], hgw, hgh, limits)?;
            planes[j] = plane;
            off = off.checked_add(consumed).ok_or(DecodeError::Overflow {
                operation: "MMR gray-plane offset",
            })?;
            if off > region.data.len() {
                return Err(DecodeError::Malformed {
                    reason: "MMR gray-plane offset past end of data",
                });
            }
        }
    } else {
        if generic_ctx.len() < (1usize << 16) {
            return Err(DecodeError::Overflow {
                operation: "halftone region context array too small",
            });
        }
        for c in generic_ctx.iter_mut() {
            *c = MqContext::default();
        }
        // Table C.4 gray-plane AT pixels (template 0/1 share AT1 = (3, -1),
        // matching this crate's template-0 generic decoder).
        let at = match region.template {
            0 | 1 => [(3i8, -1i8), (-3, -1), (2, -2), (-2, -2)],
            _ => [(2i8, -1i8), (-3, -1), (2, -2), (-2, -2)],
        };
        // All planes share one continuous arithmetic stream and context bank
        // (the encoder codes them with a single coder, contexts carried across).
        let mut dec = ArithmeticDecoder::new(region.data);
        for j in (0..hbpp as usize).rev() {
            planes[j] = match &skip {
                Some(s) => crate::decode::generic::decode_generic_bitmap_skip(
                    &mut dec,
                    hgw,
                    hgh,
                    region.template,
                    at,
                    generic_ctx,
                    limits,
                    s,
                    scratch,
                )?,
                None => decode_generic_bitmap(
                    &mut dec,
                    hgw,
                    hgh,
                    region.template,
                    at,
                    generic_ctx,
                    limits,
                    scratch,
                )?,
            };
        }
    }

    // §6.6.5.2: render patterns. Precompute the per-cell binary gray value from
    // the Gray-coded planes (Annex C.5 step 3/4) as each cell is visited.
    let op = combination_operator(region.halftone_comb_operator);
    let hgx = region.grid_x as i64;
    let hgy = region.grid_y as i64;
    let hrx = region.grid_vector_x as i64;
    let hry = region.grid_vector_y as i64;
    let last_index = num_patterns - 1;

    for mg in 0..hgh {
        let mg_i = mg as i64;
        for ng in 0..hgw {
            let ng_i = ng as i64;
            // Gray-code → binary value for this cell.
            let mut value: u32 = 0;
            if hbpp > 0 {
                let top = hbpp as usize - 1;
                let mut bit = planes[top].get(ng, mg) as u32;
                value |= bit << top;
                for j in (0..top).rev() {
                    bit ^= planes[j].get(ng, mg) as u32;
                    value |= bit << j;
                }
            }
            let idx = (value as usize).min(last_index);
            let pattern = &patterns.patterns[idx];

            // Grid location (fixed-point, 8-bit fraction), i64 intermediates.
            let x = (hgx + mg_i * hry + ng_i * hrx) >> 8;
            let y = (hgy + mg_i * hrx - ng_i * hry) >> 8;
            // Validate before narrowing to i32 for composition.
            let (xi, yi) = match (i32::try_from(x), i32::try_from(y)) {
                (Ok(xi), Ok(yi)) => (xi, yi),
                // Out of the i32 grid entirely: `combine` would clip it away.
                _ => continue,
            };
            htreg.combine(pattern, xi, yi, op);
        }
    }

    Ok(htreg)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn hbpp_matches_spec() {
        assert_eq!(bits_per_value(1), 0);
        assert_eq!(bits_per_value(2), 1);
        assert_eq!(bits_per_value(8), 3);
        assert_eq!(bits_per_value(9), 4);
        assert_eq!(bits_per_value(15), 4);
        assert_eq!(bits_per_value(16), 4);
        assert_eq!(bits_per_value(17), 5);
    }
}
