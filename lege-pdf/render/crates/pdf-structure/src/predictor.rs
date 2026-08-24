//! Predictor post-processing for FlateDecode/LZWDecode (`/DecodeParms`).
//!
//! Predictor 1 is the identity, predictor 2 is the TIFF horizontal
//! differencing predictor, and 10–15 select the PNG filter family (ISO
//! 32000-1 §7.4.4.4). For PNG predictors the *per-row filter byte* decides
//! the actual filter — the /Predictor value only announces "PNG"; PDFium
//! and every other reader ignore which of 10–15 was named.

/// Parameters mirrored from `/DecodeParms`.
#[derive(Debug, Clone, Copy)]
pub struct PredictorParms {
    pub predictor: i64,
    pub colors: i64,
    pub bits_per_component: i64,
    pub columns: i64,
}

impl Default for PredictorParms {
    fn default() -> Self {
        // Spec defaults for /Colors, /BitsPerComponent, /Columns.
        Self {
            predictor: 1,
            colors: 1,
            bits_per_component: 8,
            columns: 1,
        }
    }
}

/// Errors from predictor application (all indicate corrupt parameters).
#[derive(Debug, Clone, thiserror::Error)]
pub enum PredictorError {
    #[error("unsupported /Predictor value {0}")]
    UnsupportedPredictor(i64),
    #[error("invalid predictor parameters (colors/bpc/columns)")]
    InvalidParameters,
    #[error("predicted data is not a whole number of rows")]
    TruncatedRows,
}

/// Apply the inverse predictor in place, returning the de-predicted data.
pub fn apply_predictor(data: Vec<u8>, parms: &PredictorParms) -> Result<Vec<u8>, PredictorError> {
    match parms.predictor {
        ..=0 => Err(PredictorError::UnsupportedPredictor(parms.predictor)),
        1 => Ok(data),
        2 => apply_tiff(data, parms),
        10..=15 => apply_png(data, parms),
        other => Err(PredictorError::UnsupportedPredictor(other)),
    }
}

/// Bytes per complete pixel (rounded up to at least 1) and per row.
fn geometry(parms: &PredictorParms) -> Result<(usize, usize), PredictorError> {
    let colors = usize::try_from(parms.colors).map_err(|_| PredictorError::InvalidParameters)?;
    let bpc =
        usize::try_from(parms.bits_per_component).map_err(|_| PredictorError::InvalidParameters)?;
    let columns = usize::try_from(parms.columns).map_err(|_| PredictorError::InvalidParameters)?;
    if colors == 0 || bpc == 0 || columns == 0 || colors > 64 || bpc > 32 {
        return Err(PredictorError::InvalidParameters);
    }
    let bits_per_pixel = colors
        .checked_mul(bpc)
        .ok_or(PredictorError::InvalidParameters)?;
    let row_bits = bits_per_pixel
        .checked_mul(columns)
        .ok_or(PredictorError::InvalidParameters)?;
    let bytes_per_pixel = bits_per_pixel.div_ceil(8).max(1);
    let bytes_per_row = row_bits.div_ceil(8);
    if bytes_per_row == 0 {
        return Err(PredictorError::InvalidParameters);
    }
    Ok((bytes_per_pixel, bytes_per_row))
}

/// TIFF predictor 2: horizontal differencing. Each sample is the running sum
/// of the raw deltas of the same colour component along the row, modulo
/// 2^bpc. The 8-bit case (one sample per byte) is a byte-wise fast path; other
/// depths — including the sub-byte 1/2/4-bit scans real scanners emit (a 1-bpc
/// DeviceGray page differenced bit-by-bit) and 16-bit samples — unpack the
/// row's packed samples, difference per component, and repack. The old code
/// fell back to identity for `bpc != 8`, leaving the deltas un-summed: a
/// bilevel page came out near-solid ink (issue6071/flate_predictor_bpc_1).
fn apply_tiff(mut data: Vec<u8>, parms: &PredictorParms) -> Result<Vec<u8>, PredictorError> {
    let (bpp, row_len) = geometry(parms)?;
    if parms.bits_per_component == 8 {
        for row in data.chunks_exact_mut(row_len) {
            for i in bpp..row.len() {
                row[i] = row[i].wrapping_add(row[i - bpp]);
            }
        }
        return Ok(data);
    }
    let bpc = parms.bits_per_component as usize;
    let colors = parms.colors as usize;
    let columns = parms.columns as usize;
    let samples = colors * columns;
    let mask: u32 = if bpc >= 32 {
        u32::MAX
    } else {
        (1u32 << bpc) - 1
    };
    let mut acc = vec![0u32; samples];
    for row in data.chunks_exact_mut(row_len) {
        // Unpack `samples` values of `bpc` bits, MSB-first.
        for (j, slot) in acc.iter_mut().enumerate() {
            let mut v = 0u32;
            let base = j * bpc;
            for k in 0..bpc {
                let bit = base + k;
                let b = (row[bit / 8] >> (7 - (bit % 8))) & 1;
                v = (v << 1) | u32::from(b);
            }
            *slot = v;
        }
        // Horizontal differencing per colour component.
        for j in colors..samples {
            acc[j] = (acc[j].wrapping_add(acc[j - colors])) & mask;
        }
        // Repack MSB-first over the row (only the sample bits; any padding
        // bits in the final byte stay zero, as when they were unpacked).
        row.fill(0);
        for (j, &v) in acc.iter().enumerate() {
            let base = j * bpc;
            for k in 0..bpc {
                let bit = base + k;
                let b = ((v >> (bpc - 1 - k)) & 1) as u8;
                row[bit / 8] |= b << (7 - (bit % 8));
            }
        }
    }
    Ok(data)
}

/// PNG filter family: each row is `filter_byte || filtered_bytes`.
fn apply_png(data: Vec<u8>, parms: &PredictorParms) -> Result<Vec<u8>, PredictorError> {
    let (bpp, row_len) = geometry(parms)?;
    let stride = row_len + 1; // +1 for the per-row filter byte
    if !data.len().is_multiple_of(stride) {
        // Tolerate a truncated trailing row only if nothing else fits;
        // xref streams with sloppy trailing bytes exist. Complete rows are
        // recovered, the partial tail is dropped.
        if data.len() < stride {
            return Err(PredictorError::TruncatedRows);
        }
    }
    let rows = data.len() / stride;
    // Decode straight into the output. Every filter branch writes all
    // `row_len` bytes of its row, so `out` needs no clearing between rows,
    // and splitting it gives the previous row as a borrow — no per-row
    // scratch allocation and no per-row copy into the output.
    let mut out: Vec<u8> = vec![0u8; rows * row_len];
    // The imaginary row above row 0 is all zero (RFC 2083 §6.3). One
    // allocation for the whole image rather than one per row.
    let zero_row = vec![0u8; row_len];

    for r in 0..rows {
        let row = &data[r * stride..(r + 1) * stride];
        let filter = row[0];
        let src = &row[1..];
        // `done` holds every row already decoded, `cur` the row being
        // decoded — disjoint borrows into the same buffer.
        let (done, rest) = out.split_at_mut(r * row_len);
        let cur = &mut rest[..row_len];
        let prev: &[u8] = if r == 0 {
            &zero_row
        } else {
            &done[(r - 1) * row_len..r * row_len]
        };
        match filter {
            0 => cur.copy_from_slice(src),
            1 => {
                // Sub: left neighbour.
                for i in 0..row_len {
                    let left = if i >= bpp { cur[i - bpp] } else { 0 };
                    cur[i] = src[i].wrapping_add(left);
                }
            }
            2 => {
                // Up: previous row.
                for i in 0..row_len {
                    cur[i] = src[i].wrapping_add(prev[i]);
                }
            }
            3 => {
                // Average of left and up.
                for i in 0..row_len {
                    let left = if i >= bpp { cur[i - bpp] } else { 0 };
                    let up = prev[i];
                    let avg = ((u16::from(left) + u16::from(up)) / 2) as u8;
                    cur[i] = src[i].wrapping_add(avg);
                }
            }
            4 => {
                // Paeth.
                for i in 0..row_len {
                    let left = if i >= bpp { cur[i - bpp] } else { 0 };
                    let up = prev[i];
                    let up_left = if i >= bpp { prev[i - bpp] } else { 0 };
                    cur[i] = src[i].wrapping_add(paeth(left, up, up_left));
                }
            }
            // Unknown filter byte: treat the row as unfiltered rather than
            // failing the whole stream (tolerated deviation).
            _ => cur.copy_from_slice(src),
        }
    }
    Ok(out)
}

/// PNG Paeth prediction function (RFC 2083 §6.6).
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i32::from(a) + i32::from(b) - i32::from(c);
    let pa = (p - i32::from(a)).abs();
    let pb = (p - i32::from(b)).abs();
    let pc = (p - i32::from(c)).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn png_parms(columns: i64) -> PredictorParms {
        PredictorParms {
            predictor: 12,
            colors: 1,
            bits_per_component: 8,
            columns,
        }
    }

    #[test]
    fn identity_predictor_passthrough() {
        let parms = PredictorParms {
            predictor: 1,
            ..Default::default()
        };
        assert_eq!(
            apply_predictor(vec![1, 2, 3], &parms).unwrap(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn png_none_filter() {
        // Two rows of 3 columns, filter byte 0.
        let data = vec![0, 10, 20, 30, 0, 40, 50, 60];
        assert_eq!(
            apply_predictor(data, &png_parms(3)).unwrap(),
            vec![10, 20, 30, 40, 50, 60]
        );
    }

    #[test]
    fn png_sub_filter() {
        // Sub: out[i] = src[i] + out[i-1]. Row: 1, 1+2=3, 3+3=6.
        let data = vec![1, 1, 2, 3];
        assert_eq!(apply_predictor(data, &png_parms(3)).unwrap(), vec![1, 3, 6]);
    }

    #[test]
    fn png_up_filter() {
        // Row 1 raw: 5 5 5. Row 2 Up: +1 +2 +3 → 6 7 8.
        let data = vec![0, 5, 5, 5, 2, 1, 2, 3];
        assert_eq!(
            apply_predictor(data, &png_parms(3)).unwrap(),
            vec![5, 5, 5, 6, 7, 8]
        );
    }

    #[test]
    fn png_average_filter() {
        // Row 1 raw: 2 4. Row 2 Average: src 3 with avg(left,up):
        //   i=0: left=0, up=2 → avg 1 → 3+1=4
        //   i=1: left=4, up=4 → avg 4 → 3+4=7
        let data = vec![0, 2, 4, 3, 3, 3];
        assert_eq!(
            apply_predictor(data, &png_parms(2)).unwrap(),
            vec![2, 4, 4, 7]
        );
    }

    #[test]
    fn png_paeth_filter_known_vector() {
        // Paeth chooses among left/up/up-left; verify against hand-computed
        // values. Row 1 raw: 10 20. Row 2 Paeth with src 1 1:
        //   i=0: a=0,b=10,c=0 → p=10 → pa=10,pb=0 → b=10 → 1+10=11
        //   i=1: a=11,b=20,c=10 → p=21 → pa=10,pb=1,pc=11 → b=20 → 1+20=21
        let data = vec![0, 10, 20, 4, 1, 1];
        assert_eq!(
            apply_predictor(data, &png_parms(2)).unwrap(),
            vec![10, 20, 11, 21]
        );
    }

    #[test]
    fn paeth_function_prefers_left_on_ties() {
        assert_eq!(paeth(1, 1, 1), 1);
        assert_eq!(paeth(0, 0, 0), 0);
        // Classic asymmetry check.
        assert_eq!(paeth(3, 10, 12), 3);
    }

    #[test]
    fn tiff_predictor_8bit() {
        let parms = PredictorParms {
            predictor: 2,
            colors: 1,
            bits_per_component: 8,
            columns: 4,
        };
        // Row deltas 1,1,1,1 → cumulative 1,2,3,4.
        assert_eq!(
            apply_predictor(vec![1, 1, 1, 1], &parms).unwrap(),
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn tiff_predictor_1bit() {
        // 1-bpc DeviceGray, 8 columns. Encode a known bilevel row (pixels
        // 1 0 0 1 1 0 1 0) by forward horizontal differencing, then verify the
        // inverse predictor recovers it. Forward: d[0]=p[0], d[i]=p[i]-p[i-1]
        // mod 2 = p[i] XOR p[i-1].
        let pixels = [1u8, 0, 0, 1, 1, 0, 1, 0];
        let mut deltas = [0u8; 8];
        deltas[0] = pixels[0];
        for i in 1..8 {
            deltas[i] = pixels[i] ^ pixels[i - 1];
        }
        let pack = |bits: &[u8; 8]| {
            let mut b = 0u8;
            for (i, &v) in bits.iter().enumerate() {
                b |= v << (7 - i);
            }
            b
        };
        let parms = PredictorParms {
            predictor: 2,
            colors: 1,
            bits_per_component: 1,
            columns: 8,
        };
        let decoded = apply_predictor(vec![pack(&deltas)], &parms).unwrap();
        assert_eq!(decoded, vec![pack(&pixels)]);
    }

    #[test]
    fn tiff_predictor_4bit_two_colors() {
        // bpc=4, colors=2, columns=2 → 4 samples/row, 2 bytes/row. Samples
        // interleaved by colour: differencing references j-colors. Pixels
        // (c0,c1) = (1,2),(3,4): deltas c0: 1, 3-1=2 ; c1: 2, 4-2=2.
        let deltas = [1u8, 2, 2, 2]; // s0..s3 nibbles
        let byte = |hi: u8, lo: u8| (hi << 4) | lo;
        let parms = PredictorParms {
            predictor: 2,
            colors: 2,
            bits_per_component: 4,
            columns: 2,
        };
        let decoded = apply_predictor(
            vec![byte(deltas[0], deltas[1]), byte(deltas[2], deltas[3])],
            &parms,
        )
        .unwrap();
        assert_eq!(decoded, vec![byte(1, 2), byte(3, 4)]);
    }

    #[test]
    fn multi_byte_pixels_use_pixel_stride() {
        // colors=3 → bpp=3; Sub filter must reference 3 bytes back.
        let parms = PredictorParms {
            predictor: 12,
            colors: 3,
            bits_per_component: 8,
            columns: 2,
        };
        let data = vec![1, 10, 20, 30, 5, 5, 5];
        assert_eq!(
            apply_predictor(data, &parms).unwrap(),
            vec![10, 20, 30, 15, 25, 35]
        );
    }

    #[test]
    fn invalid_parameters_rejected() {
        let parms = PredictorParms {
            predictor: 12,
            colors: 0,
            bits_per_component: 8,
            columns: 5,
        };
        assert!(apply_predictor(vec![0; 12], &parms).is_err());
        let parms = PredictorParms {
            predictor: 7,
            ..Default::default()
        };
        assert!(matches!(
            apply_predictor(vec![0; 4], &parms),
            Err(PredictorError::UnsupportedPredictor(7))
        ));
    }

    #[test]
    fn xref_style_up_predictor_roundtrip() {
        // The exact shape used by real xref streams: W [1 2 1], predictor
        // 12 (Up), columns 4. Encode by forward-filtering, then verify
        // decode reproduces the original entries.
        let rows: Vec<[u8; 4]> = vec![[1, 0, 15, 0], [1, 0, 90, 0], [2, 0, 3, 1]];
        let mut encoded = Vec::new();
        let mut prev = [0u8; 4];
        for row in &rows {
            encoded.push(2u8); // Up filter
            for i in 0..4 {
                encoded.push(row[i].wrapping_sub(prev[i]));
            }
            prev = *row;
        }
        let parms = PredictorParms {
            predictor: 12,
            colors: 1,
            bits_per_component: 8,
            columns: 4,
        };
        let decoded = apply_predictor(encoded, &parms).unwrap();
        let flat: Vec<u8> = rows.iter().flatten().copied().collect();
        assert_eq!(decoded, flat);
    }
}
