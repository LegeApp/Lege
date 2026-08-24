//! `/CCITTFaxDecode` — a native, in-house ITU-T T.4 / T.6 (fax Group 3/4)
//! decoder.
//!
//! CCITT Group 3/4 is fax compression, and it is what a great many scanned
//! books embed. Measured with `tools/pdfium-diff` over a real library, pages
//! using it were the single worst class of failure we had: they rendered
//! *blank* (`ours-ink 0.0000` against PDFium's `0.1061`), because the image
//! simply never decoded.
//!
//! This is a from-scratch port of PDFium's fax decoder
//! (`core/fxcodec/fax/faxmodule.cpp`), our differential oracle — the codebase's
//! native-first policy treats every third-party codec as a stopgap, and this
//! replaces the last runtime one (`hayro-ccitt`, now a dev-dependency used only
//! to cross-check this decoder). Line-level provenance is cited throughout, in
//! the style of [`pdf_color::cmyk`]. The Huffman run tables and the
//! `kOneLeadPos` scan table are transcribed **verbatim** from faxmodule.cpp
//! (they are empirical code tables, not derivable).
//!
//! # Polarity
//! CCITT codes runs of white and black; the PDF filter emits bits whose
//! meaning is set by `/BlackIs1` (ISO 32000-1 table 11): **false (default) →
//! a 0 bit is black**, which already matches DeviceGray. `true` inverts.
//!
//! Internally the decoder follows PDFium's convention: a scanline starts all
//! ones (white) and black runs *clear* bits, so a decoded row is `1 = white,
//! 0 = black`; `/BlackIs1` then inverts the whole row. Padding bits past
//! `/Columns` in the last byte of a row are forced to 0 (they are outside the
//! image and the renderer masks to width anyway).

use std::sync::Arc;

use crate::codec::{DecodeLimits, DecodedFormat, DecodedImage, ImageCodec};
use crate::{DecodeParameters, ImageDescriptor, ImageError, StreamFilter};

/// `/DecodeParms` for a CCITT image (ISO 32000-1 table 11).
#[derive(Debug, Clone, Copy)]
pub struct CcittParams {
    /// `/K`: < 0 = Group 4, 0 = Group 3 1-D, > 0 = Group 3 mixed 2-D.
    pub k: i32,
    pub columns: u32,
    pub rows: u32,
    /// `/BlackIs1`: when true a 1 bit is black (default false).
    pub black_is_1: bool,
    /// `/EncodedByteAlign`.
    pub byte_align: bool,
    pub end_of_line: bool,
    pub end_of_block: bool,
}

impl Default for CcittParams {
    fn default() -> Self {
        // The spec's defaults.
        Self {
            k: 0,
            columns: 1728,
            rows: 0,
            black_is_1: false,
            byte_align: false,
            end_of_line: false,
            end_of_block: true,
        }
    }
}

/// The `/CCITTFaxDecode` codec for a [`crate::CodecRegistry`].
#[derive(Debug, Default)]
pub struct CcittCodec;

impl ImageCodec for CcittCodec {
    fn filter(&self) -> StreamFilter {
        StreamFilter::CcittFax
    }

    fn decode(
        &self,
        data: &[u8],
        descriptor: &ImageDescriptor,
        params: &DecodeParameters,
        limits: &DecodeLimits,
    ) -> Result<DecodedImage, ImageError> {
        limits.check_input(data.len())?;
        let p = params.ccitt.unwrap_or_default();
        // /Columns and /Rows describe the encoded data; the image dictionary's
        // /Width and /Height are authoritative for the raster, and producers
        // do omit /Rows.
        let width = if p.columns > 0 {
            p.columns
        } else {
            descriptor.width
        };
        let height = if p.rows > 0 {
            p.rows
        } else {
            descriptor.height
        };
        if width == 0 || height == 0 {
            return Err(ImageError::Decode("CCITT: zero dimension".into()));
        }
        // Bilevel output: one bit per pixel (see `max_pixels_at_bpp`).
        if u64::from(width) * u64::from(height) > limits.max_pixels_at_bpp(1) {
            return Err(ImageError::TooLarge { width, height });
        }
        let stride = (width as usize).div_ceil(8);
        let bytes = stride
            .checked_mul(height as usize)
            .ok_or_else(|| ImageError::Decode("CCITT: size overflow".into()))?;
        if bytes as u64 > limits.max_output_bytes {
            return Err(ImageError::TooLarge { width, height });
        }

        // `/EndOfBlock` is intentionally not consulted: PDFium's FaxDecoder does
        // not take it (decoding is bounded by Rows/height and data exhaustion,
        // and any trailing EOFB/RTC is simply left unread). We match the oracle.
        let out = decode_ccitt(
            data,
            width as usize,
            height as usize,
            stride,
            p.k,
            p.end_of_line,
            p.byte_align,
            p.black_is_1,
            limits,
        )?;

        Ok(DecodedImage {
            width,
            height,
            format: DecodedFormat::Mono1,
            stride,
            data: out,
        })
    }
}

// ===========================================================================
// Native T.4 / T.6 decoder, ported from faxmodule.cpp.
// ===========================================================================

/// Position (0 = MSB) of the leading 1-bit of a byte; 8 for zero.
/// `kOneLeadPos` (faxmodule.cpp lines 41-53), verbatim.
#[rustfmt::skip]
static ONE_LEAD_POS: [u8; 256] = [
    8, 7, 6, 6, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3,
    3, 3, 3, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// Black run-length Huffman table in PDFium's level-indexed instruction format
/// (`kFaxBlackRunIns`, faxmodule.cpp lines 164-219), verbatim. Run values > 255
/// are stored as `(lo, hi)` bytes (`value = lo + hi*256`); see [`get_run`].
#[rustfmt::skip]
static BLACK_RUN_INS: [u8; 326] = [
    0, 2, 2, 3, 0, 3, 2, 0, 2, 2, 1, 0, 3, 4, 0, 2, 2, 6, 0, 3, 5, 0, 1, 3,
    7, 0, 2, 4, 9, 0, 5, 8, 0, 3, 4, 10, 0, 5, 11, 0, 7, 12, 0, 2, 4, 13, 0, 7,
    14, 0, 1, 24, 15, 0, 5, 8, 18, 0, 15, 64, 0, 23, 16, 0, 24, 17, 0, 55, 0, 0, 10, 8,
    0, 7, 12, 64, 7, 13, 128, 7, 23, 24, 0, 24, 25, 0, 40, 23, 0, 55, 22, 0, 103, 19, 0, 104,
    20, 0, 108, 21, 0, 54, 18, 192, 7, 19, 0, 8, 20, 64, 8, 21, 128, 8, 22, 192, 8, 23, 0, 9,
    28, 64, 9, 29, 128, 9, 30, 192, 9, 31, 0, 10, 36, 52, 0, 39, 55, 0, 40, 56, 0, 43, 59, 0,
    44, 60, 0, 51, 64, 1, 52, 128, 1, 53, 192, 1, 55, 53, 0, 56, 54, 0, 82, 50, 0, 83, 51, 0,
    84, 44, 0, 85, 45, 0, 86, 46, 0, 87, 47, 0, 88, 57, 0, 89, 58, 0, 90, 61, 0, 91, 0, 1,
    100, 48, 0, 101, 49, 0, 102, 62, 0, 103, 63, 0, 104, 30, 0, 105, 31, 0, 106, 32, 0, 107, 33, 0,
    108, 40, 0, 109, 41, 0, 200, 128, 0, 201, 192, 0, 202, 26, 0, 203, 27, 0, 204, 28, 0, 205, 29, 0,
    210, 34, 0, 211, 35, 0, 212, 36, 0, 213, 37, 0, 214, 38, 0, 215, 39, 0, 218, 42, 0, 219, 43, 0,
    20, 74, 128, 2, 75, 192, 2, 76, 0, 3, 77, 64, 3, 82, 0, 5, 83, 64, 5, 84, 128, 5, 85, 192,
    5, 90, 0, 6, 91, 64, 6, 100, 128, 6, 101, 192, 6, 108, 0, 2, 109, 64, 2, 114, 128, 3, 115, 192,
    3, 116, 0, 4, 117, 64, 4, 118, 128, 4, 119, 192, 4, 255,
];

/// White run-length Huffman table (`kFaxWhiteRunIns`, faxmodule.cpp lines
/// 221-277), verbatim.
#[rustfmt::skip]
static WHITE_RUN_INS: [u8; 325] = [
    0, 0, 0, 6, 7, 2, 0, 8, 3, 0, 11, 4, 0, 12, 5, 0, 14, 6, 0, 15, 7, 0, 6, 7,
    10, 0, 8, 11, 0, 18, 128, 0, 19, 8, 0, 20, 9, 0, 27, 64, 0, 9, 3, 13, 0, 7, 1, 0,
    8, 12, 0, 23, 192, 0, 24, 128, 6, 42, 16, 0, 43, 17, 0, 52, 14, 0, 53, 15, 0, 12, 3, 22,
    0, 4, 23, 0, 8, 20, 0, 12, 19, 0, 19, 26, 0, 23, 21, 0, 24, 28, 0, 36, 27, 0, 39, 18,
    0, 40, 24, 0, 43, 25, 0, 55, 0, 1, 42, 2, 29, 0, 3, 30, 0, 4, 45, 0, 5, 46, 0, 10,
    47, 0, 11, 48, 0, 18, 33, 0, 19, 34, 0, 20, 35, 0, 21, 36, 0, 22, 37, 0, 23, 38, 0, 26,
    31, 0, 27, 32, 0, 36, 53, 0, 37, 54, 0, 40, 39, 0, 41, 40, 0, 42, 41, 0, 43, 42, 0, 44,
    43, 0, 45, 44, 0, 50, 61, 0, 51, 62, 0, 52, 63, 0, 53, 0, 0, 54, 64, 1, 55, 128, 1, 74,
    59, 0, 75, 60, 0, 82, 49, 0, 83, 50, 0, 84, 51, 0, 85, 52, 0, 88, 55, 0, 89, 56, 0, 90,
    57, 0, 91, 58, 0, 100, 192, 1, 101, 0, 2, 103, 128, 2, 104, 64, 2, 16, 152, 192, 5, 153, 0, 6,
    154, 64, 6, 155, 192, 6, 204, 192, 2, 205, 0, 3, 210, 64, 3, 211, 128, 3, 212, 192, 3, 213, 0, 4,
    214, 64, 4, 215, 128, 4, 216, 192, 4, 217, 0, 5, 218, 64, 5, 219, 128, 5, 0, 3, 8, 0, 7, 12,
    64, 7, 13, 128, 7, 10, 18, 192, 7, 19, 0, 8, 20, 64, 8, 21, 128, 8, 22, 192, 8, 23, 0, 9,
    28, 64, 9, 29, 128, 9, 30, 192, 9, 31, 0, 10, 255,
];

/// One source bit at `pos` (MSB-first); out of range reads as 0.
#[inline]
fn src_bit(data: &[u8], pos: usize) -> bool {
    data.get(pos / 8)
        .is_some_and(|b| b & (1 << (7 - pos % 8)) != 0)
}

/// Read one bit and advance `bitpos`. Port of `NextBit` (faxmodule.cpp lines
/// 159-162); callers guard `bitpos < bitsize` exactly as PDFium does.
#[inline]
fn next_bit(data: &[u8], bitpos: &mut usize, _bitsize: usize) -> bool {
    let pos = *bitpos;
    *bitpos += 1;
    src_bit(data, pos)
}

/// First position in `[start_pos, max_pos)` whose bit equals `bit` (1 = white),
/// else `max_pos`. Port of `FindBit` (faxmodule.cpp lines 61-108); the 8-byte
/// bulk-skip is a pure performance shortcut and is omitted (identical output).
fn find_bit(data: &[u8], max_pos: usize, start_pos: usize, bit: bool) -> usize {
    if start_pos >= max_pos {
        return max_pos;
    }
    let bit_xor: u8 = if bit { 0x00 } else { 0xff };
    let mut start_pos = start_pos;
    let bit_offset = start_pos % 8;
    if bit_offset != 0 {
        let byte_pos = start_pos / 8;
        let d = (data.get(byte_pos).copied().unwrap_or(0) ^ bit_xor) & (0xff >> bit_offset);
        if d != 0 {
            return byte_pos * 8 + ONE_LEAD_POS[d as usize] as usize;
        }
        start_pos += 7;
    }
    let max_byte = max_pos.div_ceil(8);
    let mut byte_pos = start_pos / 8;
    while byte_pos < max_byte {
        let d = data.get(byte_pos).copied().unwrap_or(0) ^ bit_xor;
        if d != 0 {
            return (byte_pos * 8 + ONE_LEAD_POS[d as usize] as usize).min(max_pos);
        }
        byte_pos += 1;
    }
    max_pos
}

/// Locate the two changing elements `b1`, `b2` on the reference line for the
/// current `a0`/`a0color`. Port of `FaxG4FindB1B2` (faxmodule.cpp lines
/// 110-131). `1 = white` in `ref_buf`.
fn find_b1b2(ref_buf: &[u8], columns: usize, a0: i32, a0color: bool) -> (usize, usize) {
    let mut first_bit = a0 < 0 || {
        let a0u = a0 as usize;
        ref_buf
            .get(a0u / 8)
            .is_some_and(|b| b & (1 << (7 - a0u % 8)) != 0)
    };
    let start = (a0 + 1).max(0) as usize;
    let mut b1 = find_bit(ref_buf, columns, start, !first_bit);
    if b1 >= columns {
        return (columns, columns);
    }
    if first_bit == !a0color {
        b1 = find_bit(ref_buf, columns, b1 + 1, first_bit);
        first_bit = !first_bit;
    }
    if b1 >= columns {
        return (columns, columns);
    }
    let b2 = find_bit(ref_buf, columns, b1 + 1, first_bit);
    (b1, b2)
}

/// Clear (mark black) the bits `[startpos, endpos)` of a scanline that starts
/// all ones. Port of `FaxFillBits` (faxmodule.cpp lines 133-157); uses `&= !m`
/// (idempotent) where PDFium subtracts — identical for the disjoint runs the
/// algorithm produces, and panic-free.
fn fill_bits(dest: &mut [u8], columns: usize, startpos: i32, endpos: i32) {
    let start = startpos.max(0) as usize;
    let end = endpos.clamp(0, columns as i32) as usize;
    if start >= end {
        return;
    }
    let first_byte = start / 8;
    let last_byte = (end - 1) / 8;
    if first_byte == last_byte {
        for i in (start % 8)..=((end - 1) % 8) {
            if let Some(b) = dest.get_mut(first_byte) {
                *b &= !(1u8 << (7 - i));
            }
        }
        return;
    }
    for i in (start % 8)..8 {
        if let Some(b) = dest.get_mut(first_byte) {
            *b &= !(1u8 << (7 - i));
        }
    }
    for i in 0..=((end - 1) % 8) {
        if let Some(b) = dest.get_mut(last_byte) {
            *b &= !(1u8 << (7 - i));
        }
    }
    for byte in (first_byte + 1)..last_byte {
        if let Some(b) = dest.get_mut(byte) {
            *b = 0;
        }
    }
}

/// Decode one run length from `data` using a run-length instruction table.
/// Port of `FaxGetRun` (faxmodule.cpp lines 279-309). Returns the run, or `-1`
/// on a table miss / exhausted input.
fn get_run(ins: &[u8], data: &[u8], bitpos: &mut usize, bitsize: usize) -> i32 {
    let mut code: u32 = 0;
    let mut off = 0usize;
    loop {
        let Some(&insn) = ins.get(off) else {
            return -1;
        };
        off += 1;
        if insn == 0xff {
            return -1;
        }
        if *bitpos >= bitsize {
            return -1;
        }
        code = (code << 1) | u32::from(src_bit(data, *bitpos));
        *bitpos += 1;
        let next_off = off + insn as usize * 3;
        while off < next_off {
            match (ins.get(off), ins.get(off + 1), ins.get(off + 2)) {
                (Some(&c), Some(&lo), Some(&hi)) if u32::from(c) == code => {
                    return i32::from(lo) + i32::from(hi) * 256;
                }
                _ => {}
            }
            off += 3;
        }
    }
}

/// Decode one 2-D (Group 4 / T.6) coded row into `dest` (which starts all
/// ones) using the reference line `ref_buf`. Port of `FaxG4GetRow`
/// (faxmodule.cpp lines 311-473). See TABLE 1/T.6.
fn g4_get_row(
    data: &[u8],
    bitsize: usize,
    bitpos: &mut usize,
    dest: &mut [u8],
    ref_buf: &[u8],
    columns: usize,
) {
    let cols = columns as i32;
    let mut a0: i32 = -1;
    let mut a0color = true; // white
    loop {
        if *bitpos >= bitsize {
            return;
        }
        let (b1, b2) = find_b1b2(ref_buf, columns, a0, a0color);
        let (b1, b2) = (b1 as i32, b2 as i32);

        let mut v_delta = 0i32;
        if !next_bit(data, bitpos, bitsize) {
            if *bitpos >= bitsize {
                return;
            }
            let bit1 = next_bit(data, bitpos, bitsize);
            if *bitpos >= bitsize {
                return;
            }
            let bit2 = next_bit(data, bitpos, bitsize);
            if bit1 {
                // Vertical VR(1)/VL(1).
                v_delta = if bit2 { 1 } else { -1 };
            } else if bit2 {
                // Horizontal: two runs.
                let mut run_len1 = 0i32;
                loop {
                    let table = if a0color {
                        &WHITE_RUN_INS[..]
                    } else {
                        &BLACK_RUN_INS[..]
                    };
                    let run = get_run(table, data, bitpos, bitsize);
                    run_len1 += run;
                    if run < 64 {
                        break;
                    }
                }
                if a0 < 0 {
                    run_len1 += 1;
                }
                if run_len1 < 0 {
                    return;
                }
                let a1 = a0 + run_len1;
                if !a0color {
                    fill_bits(dest, columns, a0, a1);
                }
                let mut run_len2 = 0i32;
                loop {
                    let table = if a0color {
                        &BLACK_RUN_INS[..]
                    } else {
                        &WHITE_RUN_INS[..]
                    };
                    let run = get_run(table, data, bitpos, bitsize);
                    run_len2 += run;
                    if run < 64 {
                        break;
                    }
                }
                if run_len2 < 0 {
                    return;
                }
                let a2 = a1 + run_len2;
                if a0color {
                    fill_bits(dest, columns, a1, a2);
                }
                a0 = a2;
                if a0 < cols {
                    continue;
                }
                return;
            } else {
                if *bitpos >= bitsize {
                    return;
                }
                if next_bit(data, bitpos, bitsize) {
                    // Pass mode.
                    if !a0color {
                        fill_bits(dest, columns, a0, b2);
                    }
                    if b2 >= cols {
                        return;
                    }
                    a0 = b2;
                    continue;
                }
                if *bitpos >= bitsize {
                    return;
                }
                let nb1 = next_bit(data, bitpos, bitsize);
                if *bitpos >= bitsize {
                    return;
                }
                let nb2 = next_bit(data, bitpos, bitsize);
                if nb1 {
                    // Vertical VR(2)/VL(2).
                    v_delta = if nb2 { 2 } else { -2 };
                } else if nb2 {
                    if *bitpos >= bitsize {
                        return;
                    }
                    // Vertical VR(3)/VL(3).
                    v_delta = if next_bit(data, bitpos, bitsize) {
                        3
                    } else {
                        -3
                    };
                } else {
                    if *bitpos >= bitsize {
                        return;
                    }
                    // Extension: skip and continue (or end).
                    if next_bit(data, bitpos, bitsize) {
                        *bitpos += 3;
                        continue;
                    }
                    *bitpos += 5;
                    return;
                }
            }
        }
        // Vertical mode common tail (V0 falls straight through with v_delta 0).
        let a1 = b1 + v_delta;
        if !a0color {
            fill_bits(dest, columns, a0, a1);
        }
        if a1 >= cols {
            return;
        }
        // Picture-element position must be monotonically increasing.
        if a0 >= a1 {
            return;
        }
        a0 = a1;
        a0color = !a0color;
    }
}

/// Decode one 1-D (Group 3 MH / T.4) coded row into `dest`. Port of
/// `FaxGet1DLine` (faxmodule.cpp lines 488-532).
fn get_1d_line(data: &[u8], bitsize: usize, bitpos: &mut usize, dest: &mut [u8], columns: usize) {
    let cols = columns as i32;
    let mut color = true; // white
    let mut startpos: i32 = 0;
    loop {
        if *bitpos >= bitsize {
            return;
        }
        let mut run_len = 0i32;
        loop {
            let table = if color {
                &WHITE_RUN_INS[..]
            } else {
                &BLACK_RUN_INS[..]
            };
            let run = get_run(table, data, bitpos, bitsize);
            if run < 0 {
                // Malformed code: skip to the next 1-bit and give up on the row.
                while *bitpos < bitsize {
                    if next_bit(data, bitpos, bitsize) {
                        return;
                    }
                }
                return;
            }
            run_len += run;
            if run < 64 {
                break;
            }
        }
        if !color {
            fill_bits(dest, columns, startpos, startpos + run_len);
        }
        startpos += run_len;
        if startpos >= cols {
            break;
        }
        color = !color;
    }
}

/// Skip an EOL (end-of-line) code with PDFium's tolerance. Port of `FaxSkipEOL`
/// (faxmodule.cpp lines 475-486): a run of ≥ 11 zero bits terminated by a 1 is
/// consumed as an EOL; anything shorter is left in place (it is data).
fn skip_eol(data: &[u8], bitsize: usize, bitpos: &mut usize) {
    let startbit = *bitpos;
    while *bitpos < bitsize {
        if !next_bit(data, bitpos, bitsize) {
            continue;
        }
        if *bitpos - startbit <= 11 {
            *bitpos = startbit;
        }
        return;
    }
}

/// Force the padding bits past `columns` in the last byte of a row to 0.
fn mask_padding(row: &mut [u8], columns: usize) {
    let rem = columns % 8;
    if rem == 0 {
        return;
    }
    let last = columns / 8;
    if let Some(b) = row.get_mut(last) {
        // Keep the top `rem` bits (the real pixels), zero the rest.
        *b &= !(0xffu8 >> rem);
    }
}

/// Decode a full CCITT image into packed Mono1 rows (see the module polarity
/// note). Mirrors PDFium's `FaxDecoder::GetNextLine` loop, run `rows` times.
///
/// Damage tolerance (matching faxmodule and our Flate/LZW salvage philosophy):
/// a row whose coding runs off the end of the data simply stops, leaving the
/// rest of that row (and any subsequent rows) white; the rows decoded so far
/// are kept. Only a stream from which *no* row decodes at all is a typed error.
#[allow(clippy::too_many_arguments)]
fn decode_ccitt(
    data: &[u8],
    columns: usize,
    rows: usize,
    stride: usize,
    k: i32,
    end_of_line: bool,
    mut byte_align: bool,
    black_is_1: bool,
    limits: &DecodeLimits,
) -> Result<Arc<[u8]>, ImageError> {
    let bitsize = data.len() * 8;
    let mut out = zeroed_arc(stride * rows);
    let Some(out_data) = Arc::get_mut(&mut out) else {
        return Err(ImageError::Decode(
            "CCITT: output allocation unexpectedly became shared".into(),
        ));
    };
    let mut cur = vec![0xffu8; stride];
    // The reference line above row 0 is imaginary all-white (all ones).
    let mut ref_line = vec![0xffu8; stride];
    let mut bitpos = 0usize;
    let mut rows_decoded = 0usize;

    for row in 0..rows {
        // Cooperative cancellation at a row boundary. Cancellation is not
        // truncation salvage: returning the already-decoded prefix with white
        // tail rows would cache and paint a corrupt partial page.
        if limits.is_cancelled() {
            return Err(ImageError::Cancelled);
        }
        skip_eol(data, bitsize, &mut bitpos);
        // Reset the working line to all-white before decoding.
        for b in cur.iter_mut() {
            *b = 0xff;
        }
        if bitpos < bitsize {
            let before = bitpos;
            if k < 0 {
                g4_get_row(data, bitsize, &mut bitpos, &mut cur, &ref_line, columns);
                ref_line.copy_from_slice(&cur);
            } else if k == 0 {
                get_1d_line(data, bitsize, &mut bitpos, &mut cur, columns);
            } else {
                // Group 3 2-D: a per-row tag bit selects 1-D (1) or 2-D (0).
                if next_bit(data, &mut bitpos, bitsize) {
                    get_1d_line(data, bitsize, &mut bitpos, &mut cur, columns);
                } else {
                    g4_get_row(data, bitsize, &mut bitpos, &mut cur, &ref_line, columns);
                }
                ref_line.copy_from_slice(&cur);
            }
            if end_of_line {
                skip_eol(data, bitsize, &mut bitpos);
            }
            // /EncodedByteAlign: advance to the next byte boundary, but only if
            // the skipped padding is all zeros (PDFium turns byte-align off
            // permanently otherwise — some producers set the flag spuriously).
            if byte_align && bitpos < bitsize {
                let bitpos1 = (bitpos + 7) & !7usize;
                let mut p = bitpos;
                while byte_align && p < bitpos1 {
                    if src_bit(data, p) {
                        byte_align = false;
                    } else {
                        p += 1;
                    }
                }
                if byte_align {
                    bitpos = bitpos1;
                }
            }
            if bitpos > before {
                rows_decoded += 1;
            }
        }

        let dst = &mut out_data[row * stride..row * stride + stride];
        dst.copy_from_slice(&cur);
        if black_is_1 {
            for b in dst.iter_mut() {
                *b = !*b;
            }
        }
        mask_padding(dst, columns);
    }

    if rows_decoded == 0 {
        return Err(ImageError::Decode(
            "CCITT: stream produced no decodable rows".into(),
        ));
    }
    Ok(out)
}

#[allow(
    unsafe_code,
    reason = "Arc::new_zeroed_slice hands back MaybeUninit; see SAFETY comment"
)]
fn zeroed_arc(len: usize) -> Arc<[u8]> {
    let data = Arc::<[u8]>::new_zeroed_slice(len);
    // SAFETY: `new_zeroed_slice` initialized every `u8` to a valid zero value.
    unsafe { data.assume_init() }
}

// ===========================================================================
// Tests. `hayro-ccitt` is a dev-dependency, used only as a differential oracle
// against this native decoder; `Mono1Sink` bridges its push-pixel callback to
// packed rows and is what the three pinned polarity tests exercise.
// ===========================================================================

/// Packs a decoder's pixels into 1-bit rows (MSB first).
///
/// The buffer starts all-zero and only 1-bits are written, so whichever colour
/// maps to 0 is free — and long runs of the other cost one `fill`.
#[cfg(test)]
struct Mono1Sink {
    data: Vec<u8>,
    stride: usize,
    row_base: usize,
    x: usize,
    row: usize,
    height: usize,
    /// A set bit means white, unless `/BlackIs1`.
    white_bit: bool,
}

#[cfg(test)]
impl Mono1Sink {
    fn new(stride: usize, height: usize, black_is_1: bool) -> Self {
        // `/BlackIs1` decides which colour is the 1 bit: false (the default)
        // means 0 is black, so white sets the bit; true reverses it.
        Self {
            data: vec![0u8; stride * height],
            stride,
            row_base: 0,
            x: 0,
            row: 0,
            height,
            white_bit: !black_is_1,
        }
    }

    /// Whether this pixel is written as a 1 bit.
    #[inline]
    fn is_one(&self, white: bool) -> bool {
        white == self.white_bit
    }

    #[inline]
    fn set_bit(&mut self, x: usize) {
        let byte = self.row_base + x / 8;
        if let Some(b) = self.data.get_mut(byte) {
            *b |= 0x80 >> (x % 8);
        }
    }
}

#[cfg(test)]
impl hayro_ccitt::Decoder for Mono1Sink {
    fn push_pixel(&mut self, white: bool) {
        if self.row < self.height && self.is_one(white) {
            self.set_bit(self.x);
        }
        self.x += 1;
    }

    fn push_pixel_chunk(&mut self, white: bool, chunk_count: u32) {
        if self.row < self.height && self.is_one(white) {
            let start = self.row_base + self.x / 8;
            let end = (start + chunk_count as usize).min(self.data.len());
            if start < end {
                self.data[start..end].fill(0xFF);
            }
        }
        self.x += chunk_count as usize * 8;
    }

    fn next_line(&mut self) {
        self.row += 1;
        self.row_base = self.row * self.stride;
        self.x = 0;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use hayro_ccitt::Decoder;

    // ------------------------------------------------------------------
    // Pinned adapter polarity tests (unchanged).
    // ------------------------------------------------------------------

    #[test]
    fn default_polarity_makes_white_the_one_bit() {
        // /BlackIs1 false: 0 = black, so a white pixel sets its bit.
        let mut s = Mono1Sink::new(1, 1, false);
        s.push_pixel(true);
        for _ in 0..7 {
            s.push_pixel(false);
        }
        assert_eq!(s.data, vec![0b1000_0000], "white first, then black");
    }

    #[test]
    fn black_is_1_inverts() {
        let mut s = Mono1Sink::new(1, 1, true);
        s.push_pixel(true); // white -> 0 bit
        for _ in 0..7 {
            s.push_pixel(false); // black -> 1 bit
        }
        assert_eq!(s.data, vec![0b0111_1111]);
    }

    #[test]
    fn chunks_and_rows_advance() {
        let mut s = Mono1Sink::new(2, 2, false);
        s.push_pixel_chunk(true, 2); // two all-white bytes
        s.next_line();
        s.push_pixel_chunk(false, 2); // all black -> stays 0
        assert_eq!(s.data, vec![0xFF, 0xFF, 0x00, 0x00]);
    }

    // ------------------------------------------------------------------
    // A tiny, test-only T.4/T.6 encoder to synthesise valid streams.
    // MH run codes and the 2-D encoder are ported from faxmodule.cpp's
    // (Windows-only) FaxEncoder (lines 715-897).
    // ------------------------------------------------------------------

    #[rustfmt::skip]
    static WHITE_TERM: [u8; 128] = [
        53, 8, 7, 6, 7, 4, 8, 4, 11, 4, 12, 4, 14, 4, 15, 4, 19, 5, 20, 5, 7, 5, 8, 5,
        8, 6, 3, 6, 52, 6, 53, 6, 42, 6, 43, 6, 39, 7, 12, 7, 8, 7, 23, 7, 3, 7, 4, 7,
        40, 7, 43, 7, 19, 7, 36, 7, 24, 7, 2, 8, 3, 8, 26, 8, 27, 8, 18, 8, 19, 8, 20, 8,
        21, 8, 22, 8, 23, 8, 40, 8, 41, 8, 42, 8, 43, 8, 44, 8, 45, 8, 4, 8, 5, 8, 10, 8,
        11, 8, 82, 8, 83, 8, 84, 8, 85, 8, 36, 8, 37, 8, 88, 8, 89, 8, 90, 8, 91, 8, 74, 8,
        75, 8, 50, 8, 51, 8, 52, 8,
    ];
    #[rustfmt::skip]
    static WHITE_MK: [u8; 80] = [
        27, 5, 18, 5, 23, 6, 55, 7, 54, 8, 55, 8, 100, 8, 101, 8, 104, 8, 103, 8, 204, 9, 205, 9,
        210, 9, 211, 9, 212, 9, 213, 9, 214, 9, 215, 9, 216, 9, 217, 9, 218, 9, 219, 9, 152, 9, 153, 9,
        154, 9, 24, 6, 155, 9, 8, 11, 12, 11, 13, 11, 18, 12, 19, 12, 20, 12, 21, 12, 22, 12, 23, 12,
        28, 12, 29, 12, 30, 12, 31, 12,
    ];
    #[rustfmt::skip]
    static BLACK_TERM: [u8; 128] = [
        55, 10, 2, 3, 3, 2, 2, 2, 3, 3, 3, 4, 2, 4, 3, 5, 5, 6, 4, 6, 4, 7, 5, 7,
        7, 7, 4, 8, 7, 8, 24, 9, 23, 10, 24, 10, 8, 10, 103, 11, 104, 11, 108, 11, 55, 11, 40, 11,
        23, 11, 24, 11, 202, 12, 203, 12, 204, 12, 205, 12, 104, 12, 105, 12, 106, 12, 107, 12, 210, 12, 211, 12,
        212, 12, 213, 12, 214, 12, 215, 12, 108, 12, 109, 12, 218, 12, 219, 12, 84, 12, 85, 12, 86, 12, 87, 12,
        100, 12, 101, 12, 82, 12, 83, 12, 36, 12, 55, 12, 56, 12, 39, 12, 40, 12, 88, 12, 89, 12, 43, 12,
        44, 12, 90, 12, 102, 12, 103, 12,
    ];
    #[rustfmt::skip]
    static BLACK_MK: [u8; 80] = [
        15, 10, 200, 12, 201, 12, 91, 12, 51, 12, 52, 12, 53, 12, 108, 13, 109, 13, 74, 13, 75, 13, 76, 13,
        77, 13, 114, 13, 115, 13, 116, 13, 117, 13, 118, 13, 119, 13, 82, 13, 83, 13, 84, 13, 85, 13, 90, 13,
        91, 13, 100, 13, 101, 13, 8, 11, 12, 11, 13, 11, 18, 12, 19, 12, 20, 12, 21, 12, 22, 12, 23, 12,
        28, 12, 29, 12, 30, 12, 31, 12,
    ];

    struct Enc {
        buf: Vec<u8>,
        bitpos: usize,
    }
    impl Enc {
        fn new() -> Self {
            Self {
                buf: Vec::new(),
                bitpos: 0,
            }
        }
        fn add_bits(&mut self, data: u32, len: u32) {
            for i in (0..len).rev() {
                let byte = self.bitpos / 8;
                if byte >= self.buf.len() {
                    self.buf.push(0);
                }
                if (data >> i) & 1 != 0 {
                    self.buf[byte] |= 1 << (7 - self.bitpos % 8);
                }
                self.bitpos += 1;
            }
        }
        fn align_byte(&mut self) {
            while self.bitpos % 8 != 0 {
                if self.bitpos / 8 >= self.buf.len() {
                    self.buf.push(0);
                }
                self.bitpos += 1;
            }
        }
    }

    fn encode_run(enc: &mut Enc, mut run: u32, white: bool) {
        while run >= 2560 {
            enc.add_bits(0x1f, 12);
            run -= 2560;
        }
        if run >= 64 {
            let markup = run - run % 64;
            let idx = (markup / 64 - 1) as usize * 2;
            let t = if white { &WHITE_MK[..] } else { &BLACK_MK[..] };
            enc.add_bits(u32::from(t[idx]), u32::from(t[idx + 1]));
        }
        run %= 64;
        let t = if white {
            &WHITE_TERM[..]
        } else {
            &BLACK_TERM[..]
        };
        let idx = run as usize * 2;
        enc.add_bits(u32::from(t[idx]), u32::from(t[idx + 1]));
    }

    fn encode_1d_line(enc: &mut Enc, src: &[u8], cols: usize) {
        let mut color = true;
        let mut pos = 0usize;
        loop {
            let next = find_bit(src, cols, pos, !color);
            encode_run(enc, (next - pos) as u32, color);
            pos = next;
            if pos >= cols {
                break;
            }
            color = !color;
        }
    }

    /// Port of `FaxEncode2DLine` (faxmodule.cpp lines 822-877).
    fn encode_2d_line(enc: &mut Enc, src: &[u8], cols: usize, refl: &[u8]) {
        let mut a0: i32 = -1;
        let mut a0color = true;
        loop {
            let a1 = find_bit(src, cols, (a0 + 1).max(0) as usize, !a0color) as i32;
            let (b1, b2) = find_b1b2(refl, cols, a0, a0color);
            let (b1, b2) = (b1 as i32, b2 as i32);
            if b2 < a1 {
                enc.add_bits(0b0001, 4); // pass
                a0 = b2;
            } else if (a1 - b1).abs() <= 3 {
                match a1 - b1 {
                    0 => enc.add_bits(0b1, 1),
                    1 => enc.add_bits(0b011, 3),
                    2 => enc.add_bits(0b000011, 6),
                    3 => enc.add_bits(0b0000011, 7),
                    -1 => enc.add_bits(0b010, 3),
                    -2 => enc.add_bits(0b000010, 6),
                    -3 => enc.add_bits(0b0000010, 7),
                    _ => unreachable!(),
                }
                a0 = a1;
                a0color = !a0color;
            } else {
                let a2 = find_bit(src, cols, (a1 + 1) as usize, a0color) as i32;
                enc.add_bits(0b001, 3); // horizontal
                let start = if a0 < 0 { 0 } else { a0 };
                encode_run(enc, (a1 - start) as u32, a0color);
                encode_run(enc, (a2 - a1) as u32, !a0color);
                a0 = a2;
            }
            if a0 >= cols as i32 {
                return;
            }
        }
    }

    /// Pack a row into the `1 = white` convention the encoder/decoder share.
    fn pack_row(cols: usize, black: impl Fn(usize) -> bool) -> Vec<u8> {
        let stride = cols.div_ceil(8);
        let mut v = vec![0xffu8; stride];
        for x in 0..cols {
            if black(x) {
                v[x / 8] &= !(1u8 << (7 - x % 8));
            }
        }
        v
    }

    /// Encode a whole bitmap for a given `k`, with optional byte alignment.
    fn encode(
        k: i32,
        cols: usize,
        rows: usize,
        byte_align: bool,
        black: impl Fn(usize, usize) -> bool,
    ) -> Vec<u8> {
        let stride = cols.div_ceil(8);
        let mut enc = Enc::new();
        let mut refl = vec![0xffu8; stride];
        for y in 0..rows {
            let src = pack_row(cols, |x| black(x, y));
            if k < 0 {
                encode_2d_line(&mut enc, &src, cols, &refl);
            } else if k == 0 {
                encode_1d_line(&mut enc, &src, cols);
            } else if y == 0 {
                enc.add_bits(1, 1); // tag: 1-D
                encode_1d_line(&mut enc, &src, cols);
            } else {
                enc.add_bits(0, 1); // tag: 2-D
                encode_2d_line(&mut enc, &src, cols, &refl);
            }
            refl = src;
            if byte_align {
                enc.align_byte();
            }
        }
        enc.buf
    }

    /// Read a decoded pixel's colour back from packed Mono1 output.
    fn decoded_black(out: &[u8], stride: usize, x: usize, y: usize, black_is_1: bool) -> bool {
        let bit = (out[y * stride + x / 8] >> (7 - x % 8)) & 1;
        if black_is_1 { bit == 1 } else { bit == 0 }
    }

    fn native(
        stream: &[u8],
        cols: usize,
        rows: usize,
        k: i32,
        byte_align: bool,
        black_is_1: bool,
    ) -> Vec<u8> {
        let stride = cols.div_ceil(8);
        decode_ccitt(
            stream,
            cols,
            rows,
            stride,
            k,
            false,
            byte_align,
            black_is_1,
            &DecodeLimits::default(),
        )
        .expect("native decode")
        .to_vec()
    }

    /// Drive `hayro-ccitt` over the same stream (the differential oracle).
    fn hayro(
        stream: &[u8],
        cols: usize,
        rows: usize,
        k: i32,
        byte_align: bool,
        black_is_1: bool,
    ) -> Vec<u8> {
        use hayro_ccitt::{DecodeSettings, DecoderContext, EncodingMode};
        let stride = cols.div_ceil(8);
        let settings = DecodeSettings {
            columns: cols as u32,
            rows: rows as u32,
            end_of_block: false,
            end_of_line: false,
            rows_are_byte_aligned: byte_align,
            encoding: match k {
                x if x < 0 => EncodingMode::Group4,
                0 => EncodingMode::Group3_1D,
                x => EncodingMode::Group3_2D { k: x as u32 },
            },
            invert_black: false,
        };
        let mut ctx = DecoderContext::new(settings);
        let mut sink = Mono1Sink::new(stride, rows, black_is_1);
        let _ = hayro_ccitt::decode(stream, &mut sink, &mut ctx);
        // Match the native decoder: force padding bits past `cols` to 0.
        for y in 0..rows {
            mask_padding(&mut sink.data[y * stride..(y + 1) * stride], cols);
        }
        sink.data
    }

    /// The synthetic bitmaps that make up the differential/round-trip matrix.
    fn patterns() -> Vec<(&'static str, fn(usize, usize) -> bool)> {
        vec![
            ("all_white", |_, _| false),
            ("all_black", |_, _| true),
            ("checker", |x, y| (x + y) % 2 == 0),
            ("v_stripes", |x, _| x % 3 == 0),
            ("long_runs", |x, _| x < 5),
            ("single_pixels", |x, y| x == y),
            ("edges", |x, y| x == 0 || y == 0),
        ]
    }

    #[test]
    fn roundtrip_matrix_is_pixel_exact() {
        // encode → native decode → assert the bitmap comes back bit-for-bit.
        let mut cases = 0;
        for &k in &[-1i32, 0, 4] {
            for &cols in &[8usize, 13, 17, 24, 31] {
                for &rows in &[1usize, 4, 9] {
                    for &byte_align in &[false, true] {
                        for &black_is_1 in &[false, true] {
                            for (name, pat) in patterns() {
                                let stream = encode(k, cols, rows, byte_align, pat);
                                let out = native(&stream, cols, rows, k, byte_align, black_is_1);
                                let stride = cols.div_ceil(8);
                                for y in 0..rows {
                                    for x in 0..cols {
                                        let got = decoded_black(&out, stride, x, y, black_is_1);
                                        assert_eq!(
                                            got,
                                            pat(x, y),
                                            "k={k} cols={cols} rows={rows} ba={byte_align} \
                                             b1={black_is_1} pat={name} at ({x},{y})"
                                        );
                                    }
                                }
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(cases >= 600, "expected a broad matrix, ran {cases}");
    }

    #[test]
    fn differential_native_matches_hayro() {
        // Same crafted streams through BOTH decoders → identical Mono1 output.
        let mut cases = 0;
        for &k in &[-1i32, 0, 4] {
            for &cols in &[8usize, 13, 17, 24, 31] {
                for &rows in &[1usize, 4, 9] {
                    for &byte_align in &[false, true] {
                        for &black_is_1 in &[false, true] {
                            for (name, pat) in patterns() {
                                let stream = encode(k, cols, rows, byte_align, pat);
                                let mine = native(&stream, cols, rows, k, byte_align, black_is_1);
                                let theirs = hayro(&stream, cols, rows, k, byte_align, black_is_1);
                                assert_eq!(
                                    mine, theirs,
                                    "native vs hayro differ: k={k} cols={cols} rows={rows} \
                                     ba={byte_align} b1={black_is_1} pat={name}"
                                );
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(cases >= 600, "ran {cases}");
    }

    #[test]
    fn fully_undecodable_stream_is_a_typed_error() {
        // Empty input: no row can decode → typed error, not a panic or blank Ok.
        let err = CcittCodec.decode(
            &[],
            &descriptor(24, 4),
            &params(-1, 24, 4),
            &DecodeLimits::default(),
        );
        assert!(matches!(err, Err(ImageError::Decode(_))), "{err:?}");

        // All-zero bytes are consumed as EOL fill and never form a row.
        let err = CcittCodec.decode(
            &[0u8; 4],
            &descriptor(24, 4),
            &params(-1, 24, 4),
            &DecodeLimits::default(),
        );
        assert!(matches!(err, Err(ImageError::Decode(_))), "{err:?}");
    }

    #[test]
    fn cancellation_does_not_return_a_partial_white_raster() {
        let limits = DecodeLimits {
            should_cancel: Some(Arc::new(|| true)),
            ..DecodeLimits::default()
        };
        let result = CcittCodec.decode(&[0xff; 8], &descriptor(24, 4), &params(-1, 24, 4), &limits);
        assert!(matches!(result, Err(ImageError::Cancelled)));
    }

    #[test]
    fn truncated_stream_salvages_decoded_rows() {
        // A valid 6-row G4 image truncated mid-stream: the rows that decoded
        // survive, later rows fall back to white, and it never errors/panics.
        let cols = 24;
        let rows = 6;
        let full = encode(-1, cols, rows, false, |x, y| (x + y) % 2 == 0);
        let cut = full.len() / 2;
        let img = CcittCodec
            .decode(
                &full[..cut],
                &descriptor(cols as u32, rows as u32),
                &params(-1, cols as u32, rows as u32),
                &DecodeLimits::default(),
            )
            .expect("salvage should yield a partial image, not an error");
        assert_eq!((img.width, img.height), (cols as u32, rows as u32));
        assert_eq!(img.format, DecodedFormat::Mono1);
        // The first row is fully coded early in the stream, so it must match.
        let stride = cols.div_ceil(8);
        for x in 0..cols {
            let got = decoded_black(&img.data, stride, x, 0, false);
            assert_eq!(got, (x) % 2 == 0, "first salvaged row wrong at x={x}");
        }
    }

    #[test]
    fn pixel_limit_is_enforced_before_alloc() {
        let tight = DecodeLimits {
            max_pixels: 8,
            ..DecodeLimits::default()
        };
        let err = CcittCodec.decode(
            &encode(-1, 24, 4, false, |_, _| false),
            &descriptor(24, 4),
            &params(-1, 24, 4),
            &tight,
        );
        // 24×4 = 96 px > 8: rejected before decoding.
        assert!(matches!(err, Err(ImageError::TooLarge { .. })), "{err:?}");
    }

    fn descriptor(width: u32, height: u32) -> ImageDescriptor {
        ImageDescriptor {
            width,
            height,
            bits_per_component: 1,
            color_space: None,
            is_mask: false,
            interpolate: false,
            filters: vec![StreamFilter::CcittFax],
            object: None,
        }
    }

    fn params(k: i32, columns: u32, rows: u32) -> DecodeParameters {
        DecodeParameters {
            ccitt: Some(CcittParams {
                k,
                columns,
                rows,
                ..CcittParams::default()
            }),
            ..DecodeParameters::default()
        }
    }
}
