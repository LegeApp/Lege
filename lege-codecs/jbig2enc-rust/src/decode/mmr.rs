//! MMR (ITU-T T.6 / Group 4) decoding wrapper (jbig2decplan.md §14, T.88 §6.2.6
//! and Annex C).
//!
//! JBIG2 reuses T.6 (MMR) coding for generic regions, pattern-dictionary
//! collective bitmaps, and halftone gray-code bitplanes. Rather than write a
//! second T.6 implementation, this thin wrapper drives the `fax` crate's Group 4
//! decoder behind a small [`MmrDecoder`] trait so the dependency stays
//! replaceable.
//!
//! Conventions (matching the encoder's `encode_bitmap_mmr`): the input is
//! byte-aligned T.6 data with black = [`fax::Color::Black`], and each decoded
//! pixel maps to a bit in a [`MonoBitmap`] MSB-first with a set bit = black.
//! `fax`'s `decode_g4` pads any missing trailing lines with all-white rows to
//! the requested height and consumes the EOFB marker, so an encoder-emitted MMR
//! block decodes exactly.
//!
//! Malformed input never panics: `fax` returns `None` (or produces a partial
//! bitmap) rather than crashing, and this wrapper turns a hard failure into a
//! typed [`DecodeError`].

use fax::decoder::{decode_g4, pels};
use fax::Color;

use crate::decode::error::DecodeError;
use crate::shared::bitmap::MonoBitmap;
use crate::shared::limits::DecodeLimits;

/// A replaceable MMR (Group 4) decoder (jbig2decplan.md §14).
pub trait MmrDecoder {
    /// Decode `width` × `height` MMR-coded pixels from `data` into `output`
    /// (which must already be sized `width` × `height`). Black pixels become set
    /// bits.
    fn decode_into(
        &mut self,
        data: &[u8],
        width: u32,
        height: u32,
        output: &mut MonoBitmap,
    ) -> Result<(), DecodeError>;
}

/// The default MMR decoder, backed by the `fax` crate's T.6 decoder.
#[derive(Default)]
pub struct FaxMmrDecoder;

impl MmrDecoder for FaxMmrDecoder {
    fn decode_into(
        &mut self,
        data: &[u8],
        width: u32,
        height: u32,
        output: &mut MonoBitmap,
    ) -> Result<(), DecodeError> {
        decode_mmr_into(data, width, height, output)
    }
}

/// Decode an MMR-coded bitmap into an existing, correctly sized `MonoBitmap`.
///
/// The bitmap must have exactly `width` × `height` dimensions; only in-range
/// pixels are written and the caller owns padding. A zero-area bitmap is a
/// no-op. On a hard `fax` failure (`None`) a typed error is returned; the
/// partially filled rows are left in `output`.
pub fn decode_mmr_into(
    data: &[u8],
    width: u32,
    height: u32,
    output: &mut MonoBitmap,
) -> Result<(), DecodeError> {
    debug_assert_eq!(output.width(), width);
    debug_assert_eq!(output.height(), height);
    if width == 0 || height == 0 {
        return Ok(());
    }

    let mut row: u32 = 0;
    let result = decode_g4(data.iter().copied(), width, Some(height), |transitions| {
        // `decode_g4` never emits more than `height` lines, but guard anyway so
        // a future change cannot write out of range.
        if row >= height {
            return;
        }
        let dst = output.row_mut(row);
        for w in dst.iter_mut() {
            *w = 0;
        }
        // `pels` yields exactly `width` items.
        for (x, color) in pels(transitions, width).enumerate() {
            if color == Color::Black {
                let wi = x >> 5;
                dst[wi] |= 1u32 << (31 - (x as u32 & 31));
            }
        }
        row += 1;
    });

    match result {
        Some(()) => Ok(()),
        None => Err(DecodeError::Malformed {
            reason: "MMR (Group 4) decode failed",
        }),
    }
}

/// Allocate a fresh `width` × `height` bitmap (checked against `limits`) and
/// decode `data` (MMR-coded) into it.
pub fn decode_mmr_bitmap(
    data: &[u8],
    width: u32,
    height: u32,
    limits: &DecodeLimits,
) -> Result<MonoBitmap, DecodeError> {
    let mut bitmap = MonoBitmap::new(width, height, false, limits)?;
    decode_mmr_into(data, width, height, &mut bitmap)?;
    Ok(bitmap)
}

/// Whether `decode_g4` fully decodes `data` up to (and including) a terminating
/// EOFB — i.e. `data` contains at least one complete MMR block.
fn decodes_to_eofb(data: &[u8], width: u32) -> bool {
    decode_g4(data.iter().copied(), width, None, |_| {}).is_some()
}

/// The minimal prefix length of `data` that still decodes to a complete EOFB.
///
/// `decode_g4` stops at the EOFB (it treats the first EOL of the two-EOL EOFB as
/// end-of-block), so this "decode point" *undershoots* the true byte-aligned
/// block boundary — the caller rounds it up with [`next_block_offset`].
/// `decodes_to_eofb` is monotone in the prefix length, so a binary search finds
/// the decode point.
fn decode_point(data: &[u8], width: u32) -> Option<usize> {
    if !decodes_to_eofb(data, width) {
        return None;
    }
    let mut lo = 1usize;
    let mut hi = data.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if decodes_to_eofb(&data[..mid], width) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Some(lo)
}

/// Whether `data` decodes as a fresh MMR block of exactly `height` lines ending
/// in an EOFB — used to recognise the byte-aligned start of the next plane.
fn is_block_start(data: &[u8], width: u32, height: u32) -> bool {
    let mut lines: u32 = 0;
    let r = decode_g4(data.iter().copied(), width, None, |_| lines += 1);
    r.is_some() && lines == height
}

/// The byte offset (relative to the start of the current block in `data`) where
/// the *next* MMR plane begins. The encoder emits each plane as an independent
/// byte-aligned, EOFB-terminated block, so the next block starts at the first
/// byte-aligned offset at or after the current block's decode point that itself
/// decodes as a full `height`-line block.
fn next_block_offset(data: &[u8], width: u32, height: u32, from: usize) -> Option<usize> {
    for off in from..=data.len() {
        if off == data.len() {
            return Some(off);
        }
        if is_block_start(&data[off..], width, height) {
            return Some(off);
        }
    }
    None
}

/// Decode one MMR gray-code plane from the front of `data`, returning the
/// decoded bitmap and the byte offset at which the *next* plane's block begins
/// (so the caller can advance). Missing trailing rows are padded white.
pub fn decode_mmr_plane(
    data: &[u8],
    width: u32,
    height: u32,
    limits: &DecodeLimits,
) -> Result<(MonoBitmap, usize), DecodeError> {
    let mut bitmap = MonoBitmap::new(width, height, false, limits)?;
    let point = decode_point(data, width).ok_or(DecodeError::Malformed {
        reason: "MMR gray-code plane has no terminating EOFB",
    })?;
    if width == 0 || height == 0 {
        return Ok((bitmap, point));
    }

    // Decode exactly `height` lines (padding any missing trailing rows white);
    // `Some(height)` stops after the plane without needing to parse the EOFB.
    let mut row: u32 = 0;
    let result = decode_g4(data.iter().copied(), width, Some(height), |transitions| {
        if row >= height {
            return;
        }
        let dst = bitmap.row_mut(row);
        for w in dst.iter_mut() {
            *w = 0;
        }
        // `pels` yields exactly `width` items.
        for (x, color) in pels(transitions, width).enumerate() {
            if color == Color::Black {
                let wi = x >> 5;
                dst[wi] |= 1u32 << (31 - (x as u32 & 31));
            }
        }
        row += 1;
    });
    if result.is_none() {
        return Err(DecodeError::Malformed {
            reason: "MMR gray-code plane decode failed",
        });
    }

    let next = next_block_offset(data, width, height, point).ok_or(DecodeError::Malformed {
        reason: "MMR gray-code plane boundary not found",
    })?;
    Ok((bitmap, next))
}

#[cfg(all(test, feature = "encode"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use fax::encoder::Encoder as FaxEncoder;
    use fax::VecWriter;

    /// Encode a bitmap with the same `fax` path the encoder's halftone module
    /// uses, then decode it back and assert a pixel-exact round-trip.
    fn roundtrip(width: u32, height: u32, black: impl Fn(u32, u32) -> bool) {
        let mut encoder = FaxEncoder::new(VecWriter::new());
        for y in 0..height {
            let line = (0..width).map(|x| {
                if black(x, y) {
                    Color::Black
                } else {
                    Color::White
                }
            });
            encoder.encode_line(line, width).unwrap();
        }
        let data = encoder.finish().unwrap().finish();

        let limits = DecodeLimits::default();
        let bm = decode_mmr_bitmap(&data, width, height, &limits).unwrap();
        for y in 0..height {
            for x in 0..width {
                assert_eq!(bm.get(x, y), black(x, y), "pixel ({x},{y}) w={width} h={height}");
            }
        }
    }

    #[test]
    fn roundtrip_odd_widths() {
        for &w in &[1u32, 2, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65] {
            for &h in &[1u32, 3, 8, 17] {
                roundtrip(w, h, |x, y| (x.wrapping_mul(7) ^ y.wrapping_mul(13)) & 1 == 0);
            }
        }
    }

    #[test]
    fn roundtrip_solid_and_empty() {
        roundtrip(40, 12, |_, _| true);
        roundtrip(40, 12, |_, _| false);
        roundtrip(33, 9, |x, _| x < 10);
    }

    #[test]
    fn malformed_data_is_typed_error_not_panic() {
        let limits = DecodeLimits::default();
        // Random bytes must never panic; either a bounded bitmap or a typed err.
        let junk: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(37).wrapping_add(3)).collect();
        let _ = decode_mmr_bitmap(&junk, 50, 20, &limits);
    }
}
