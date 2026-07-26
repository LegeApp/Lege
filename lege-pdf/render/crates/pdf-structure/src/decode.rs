//! Stream filter decoding: the `/Filter` chain.
//!
//! Phase 1 codecs: FlateDecode (+ predictors), ASCIIHexDecode,
//! ASCII85Decode, RunLengthDecode. Image codecs (DCT, JBIG2, CCITT, JPX)
//! are later-phase work and surface as [`DecodeError::UnsupportedFilter`].
//!
//! Decompression is the classic PDF bomb vector, so every byte produced is
//! charged against a caller-supplied [`DecodeBudget`]. The budget is
//! worker-owned (it lives on / is derived from the worker's parse context)
//! — never global.

use pdf_object::{Dictionary, NameTable, PdfObject};

use crate::predictor::{PredictorError, PredictorParms, apply_predictor};

/// Decompressed-byte budget for one decode context. Constructed from the
/// worker's remaining allowance; the caller settles `used()` back into its
/// own accounting afterwards.
#[derive(Debug)]
pub struct DecodeBudget {
    remaining: usize,
    used: usize,
}

impl DecodeBudget {
    pub fn new(remaining: usize) -> Self {
        Self { remaining, used: 0 }
    }

    /// Total bytes charged so far.
    pub fn used(&self) -> usize {
        self.used
    }

    /// Charge `n` output bytes; fails when the budget is exhausted.
    pub fn charge(&mut self, n: usize) -> Result<(), DecodeError> {
        if n > self.remaining {
            return Err(DecodeError::BudgetExceeded);
        }
        self.remaining -= n;
        self.used += n;
        Ok(())
    }
}

/// Errors from filter decoding.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DecodeError {
    #[error("unsupported stream filter /{0}")]
    UnsupportedFilter(String),
    #[error("decompressed data exceeds the decoded-bytes budget")]
    BudgetExceeded,
    #[error("corrupt stream data: {0}")]
    Corrupt(&'static str),
    #[error(transparent)]
    Predictor(#[from] PredictorError),
}

/// Apply the full `/Filter` chain of `dict` to `raw`, in order.
///
/// `/Filter` may be a single name or an array; `/DecodeParms` parallels it
/// (single dict, or array with nulls for filters without parameters).
/// Abbreviated inline-image names (`Fl`, `AHx`, `A85`, `RL`) are tolerated
/// here too — they cost nothing and appear in malformed files.
pub fn decode_stream(
    raw: &[u8],
    dict: &Dictionary,
    names: &NameTable,
    budget: &mut DecodeBudget,
) -> Result<Vec<u8>, DecodeError> {
    match decode_stream_inner(raw, dict, names, budget, false)? {
        (data, None) => Ok(data),
        (_, Some(codec)) => Err(DecodeError::UnsupportedFilter(codec)),
    }
}

/// Like [`decode_stream`], but stop at the first *image-codec* filter
/// (DCT/JPX/JBIG2/CCITTFax): the general-filter prefix is applied and the
/// still-codec-encoded bytes are returned together with the codec filter's
/// canonical name. `None` means the chain contained no codec filter and the
/// bytes are fully decoded.
pub fn decode_stream_to_codec(
    raw: &[u8],
    dict: &Dictionary,
    names: &NameTable,
    budget: &mut DecodeBudget,
) -> Result<(Vec<u8>, Option<String>), DecodeError> {
    decode_stream_inner(raw, dict, names, budget, true)
}

fn decode_stream_inner(
    raw: &[u8],
    dict: &Dictionary,
    names: &NameTable,
    budget: &mut DecodeBudget,
    stop_at_codec: bool,
) -> Result<(Vec<u8>, Option<String>), DecodeError> {
    let filter = dict.get(names.known.filter);
    let parms = dict.get(names.known.decode_parms);

    let filters: Vec<PdfObject> = match filter {
        None | Some(PdfObject::Null) => Vec::new(),
        Some(PdfObject::Array(a)) => a.to_vec(),
        Some(other) => vec![other.clone()],
    };
    let parms_list: Vec<Option<PdfObject>> = match parms {
        None | Some(PdfObject::Null) => vec![None; filters.len()],
        Some(PdfObject::Array(a)) => {
            let mut v: Vec<Option<PdfObject>> = a.iter().map(|p| Some(p.clone())).collect();
            v.resize(filters.len(), None);
            v
        }
        Some(other) => {
            let mut v = vec![Some(other.clone())];
            v.resize(filters.len(), None);
            v
        }
    };

    // Materializing an unfiltered body still allocates: charge it, so the
    // per-context budget covers every byte a stream produces, filtered or
    // not (a 2 GiB "uncompressed" stream is as much a bomb as a flate one).
    if filters.is_empty() {
        budget.charge(raw.len())?;
    }
    let mut data = raw.to_vec();
    for (i, (f, p)) in filters.iter().zip(parms_list).enumerate() {
        let Some(name_id) = f.as_name() else {
            return Err(DecodeError::Corrupt("/Filter entry is not a name"));
        };
        let name = names.resolve(name_id);
        let parms_dict = p.as_ref().and_then(|p| p.as_dict().cloned());
        data = match name.as_ref() {
            b"FlateDecode" | b"Fl" => {
                let inflated = flate_decode(&data, budget)?;
                apply_parms(inflated, parms_dict.as_ref(), names)?
            }
            b"LZWDecode" | b"LZW" => {
                // /EarlyChange (default 1) lives in the same /DecodeParms dict
                // as the predictor keys; read it before applying the predictor.
                let early_change = parms_dict
                    .as_ref()
                    .and_then(|d| names.lookup(b"EarlyChange").and_then(|id| d.get(id)))
                    .and_then(PdfObject::as_int)
                    .unwrap_or(1)
                    != 0;
                let decoded = lzw_decode(&data, budget, early_change)?;
                apply_parms(decoded, parms_dict.as_ref(), names)?
            }
            b"ASCIIHexDecode" | b"AHx" => ascii_hex_decode(&data, budget)?,
            b"ASCII85Decode" | b"A85" => ascii85_decode(&data, budget)?,
            b"RunLengthDecode" | b"RL" => run_length_decode(&data, budget)?,
            codec @ (b"DCTDecode" | b"DCT" | b"JPXDecode" | b"JBIG2Decode" | b"CCITTFaxDecode"
            | b"CCF")
                if stop_at_codec =>
            {
                // Canonicalize the abbreviated inline-image spellings.
                let canonical = match codec {
                    b"DCT" => "DCTDecode",
                    b"CCF" => "CCITTFaxDecode",
                    other => std::str::from_utf8(other).unwrap_or("DCTDecode"),
                };
                // Preceding filters already charged their output; a codec
                // that is the first filter returns the raw body, uncharged
                // so far — charge it like an unfiltered body.
                if i == 0 {
                    budget.charge(data.len())?;
                }
                return Ok((data, Some(canonical.to_owned())));
            }
            other => {
                return Err(DecodeError::UnsupportedFilter(
                    String::from_utf8_lossy(other).into_owned(),
                ));
            }
        };
    }
    Ok((data, None))
}

/// Read predictor parameters out of a `/DecodeParms` dict and apply them.
fn apply_parms(
    data: Vec<u8>,
    parms: Option<&Dictionary>,
    names: &NameTable,
) -> Result<Vec<u8>, DecodeError> {
    let Some(parms) = parms else {
        return Ok(data);
    };
    let get_int = |key: &[u8], default: i64| -> i64 {
        names
            .lookup(key)
            .and_then(|id| parms.get(id))
            .and_then(PdfObject::as_int)
            .unwrap_or(default)
    };
    let p = PredictorParms {
        predictor: get_int(b"Predictor", 1),
        colors: get_int(b"Colors", 1),
        bits_per_component: get_int(b"BitsPerComponent", 8),
        columns: get_int(b"Columns", 1),
    };
    Ok(apply_predictor(data, &p)?)
}

/// FlateDecode. Tries zlib format first, then raw deflate (headerless
/// streams occur in the wild — PDFium's inflate tolerates them the same
/// way). A corrupt tail after some successful output is tolerated: the
/// bytes produced so far are returned, matching viewer behavior of showing
/// what can be salvaged.
fn flate_decode(data: &[u8], budget: &mut DecodeBudget) -> Result<Vec<u8>, DecodeError> {
    match inflate_with(flate2::Decompress::new(true), data, budget) {
        Ok(out) => Ok(out),
        Err(DecodeError::BudgetExceeded) => Err(DecodeError::BudgetExceeded),
        Err(first_err) => match inflate_with(flate2::Decompress::new(false), data, budget) {
            Ok(out) if !out.is_empty() => Ok(out),
            _ => Err(first_err),
        },
    }
}

fn inflate_with(
    mut decompress: flate2::Decompress,
    data: &[u8],
    budget: &mut DecodeBudget,
) -> Result<Vec<u8>, DecodeError> {
    // Inflate straight into the output vector's spare capacity: a scratch
    // buffer would memcpy every byte twice. The initial guess mirrors PDFium's
    // `EstimateFlateUncompressBufferSize` — compressed size scaled by a typical
    // ratio and capped, so a 4:1 text stream lands in one allocation without a
    // small stream reserving megabytes.
    const MIN_RESERVE: usize = 8 * 1024;
    const MAX_GUESS: usize = 4 * 1024 * 1024;
    let guess = data.len().saturating_mul(4).clamp(MIN_RESERVE, MAX_GUESS);
    let mut out: Vec<u8> = Vec::with_capacity(guess);
    loop {
        if out.len() == out.capacity() {
            out.reserve(out.capacity().max(MIN_RESERVE));
        }
        let before_in = decompress.total_in();
        let before_out = decompress.total_out();
        let input = &data[usize::try_from(before_in)
            .unwrap_or(usize::MAX)
            .min(data.len())..];
        let status = decompress
            .decompress_vec(input, &mut out, flate2::FlushDecompress::None)
            .map_err(|_| {
                // Corrupt tail: keep what we already produced, if anything.
                DecodeError::Corrupt("flate stream is corrupt")
            });
        let produced = usize::try_from(decompress.total_out() - before_out)
            .map_err(|_| DecodeError::Corrupt("flate output overflow"))?;
        budget.charge(produced)?;
        match status {
            Ok(flate2::Status::StreamEnd) => return Ok(out),
            Ok(_) => {
                let consumed = decompress.total_in() - before_in;
                if consumed == 0 && produced == 0 {
                    // No forward progress: truncated stream. Tolerate if we
                    // produced output (salvage), else corrupt.
                    if out.is_empty() {
                        return Err(DecodeError::Corrupt("flate stream truncated"));
                    }
                    return Ok(out);
                }
            }
            Err(e) => {
                if out.is_empty() {
                    return Err(e);
                }
                return Ok(out);
            }
        }
    }
}

/// ASCIIHexDecode: hex pairs, whitespace ignored, `>` is EOD, odd trailing
/// digit padded with 0. Other bytes are skipped (tolerated superset of the
/// spec, consistent with the lexer's hex-string handling).
fn ascii_hex_decode(data: &[u8], budget: &mut DecodeBudget) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::new();
    let mut pending: Option<u8> = None;
    for &b in data {
        if b == b'>' {
            break;
        }
        if let Some(v) = pdf_syntax::classify::hex_value(b) {
            match pending.take() {
                Some(hi) => {
                    budget.charge(1)?;
                    out.push((hi << 4) | v);
                }
                None => pending = Some(v),
            }
        }
    }
    if let Some(hi) = pending {
        budget.charge(1)?;
        out.push(hi << 4);
    }
    Ok(out)
}

/// ASCII85Decode per ISO 32000-1 §7.4.3.
fn ascii85_decode(data: &[u8], budget: &mut DecodeBudget) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::new();
    let mut group = [0u8; 5];
    let mut n = 0usize;
    let mut i = 0usize;
    // Optional `<~` prefix (Adobe convention) is tolerated.
    if data.starts_with(b"<~") {
        i = 2;
    }
    while i < data.len() {
        let b = data[i];
        i += 1;
        if b == b'~' {
            break; // `~>` EOD
        }
        if pdf_syntax::classify::is_whitespace(b) {
            continue;
        }
        if b == b'z' && n == 0 {
            budget.charge(4)?;
            out.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        if !(b'!'..=b'u').contains(&b) {
            return Err(DecodeError::Corrupt("invalid ASCII85 character"));
        }
        group[n] = b - b'!';
        n += 1;
        if n == 5 {
            let mut v: u32 = 0;
            for &d in &group {
                v = v
                    .checked_mul(85)
                    .and_then(|v| v.checked_add(u32::from(d)))
                    .ok_or(DecodeError::Corrupt("ASCII85 group overflow"))?;
            }
            budget.charge(4)?;
            out.extend_from_slice(&v.to_be_bytes());
            n = 0;
        }
    }
    if n == 1 {
        return Err(DecodeError::Corrupt("ASCII85 partial group of one digit"));
    }
    if n > 1 {
        // Pad with 'u' (84) and emit n-1 bytes.
        let mut v: u32 = 0;
        for (k, &digit) in group.iter().enumerate() {
            let d = if k < n { u32::from(digit) } else { 84 };
            v = v
                .checked_mul(85)
                .and_then(|v| v.checked_add(d))
                .ok_or(DecodeError::Corrupt("ASCII85 group overflow"))?;
        }
        budget.charge(n - 1)?;
        out.extend_from_slice(&v.to_be_bytes()[..n - 1]);
    }
    Ok(out)
}

/// RunLengthDecode per ISO 32000-1 §7.4.5.
fn run_length_decode(data: &[u8], budget: &mut DecodeBudget) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let l = data[i];
        i += 1;
        match l {
            128 => break, // EOD
            0..=127 => {
                let count = usize::from(l) + 1;
                if i + count > data.len() {
                    return Err(DecodeError::Corrupt("run-length literal overruns data"));
                }
                budget.charge(count)?;
                out.extend_from_slice(&data[i..i + count]);
                i += count;
            }
            129..=255 => {
                let count = 257 - usize::from(l);
                let Some(&b) = data.get(i) else {
                    return Err(DecodeError::Corrupt("run-length repeat missing byte"));
                };
                i += 1;
                budget.charge(count)?;
                out.extend(std::iter::repeat_n(b, count));
            }
        }
    }
    Ok(out)
}

/// LZWDecode per ISO 32000-1 §7.4.4 (the TIFF/PDF variable-width LZW variant).
///
/// Codes are packed most-significant-bit first and grow from 9 to 12 bits as
/// the string table fills. Code 256 clears the table, 257 is end-of-data.
/// `early_change` (PDF `/EarlyChange`, default 1) makes the code width step up
/// one code sooner, matching every Acrobat-produced stream; a `0` stream turns
/// it off. A truncated stream that simply runs out of bits before EOD yields
/// the bytes decoded so far, matching a viewer showing a partial image.
///
/// The table stores each code as `(prefix, suffix)` and strings are rebuilt
/// through a scratch stack, so a pathological stream cannot cost O(n²) copying.
fn lzw_decode(
    data: &[u8],
    budget: &mut DecodeBudget,
    early_change: bool,
) -> Result<Vec<u8>, DecodeError> {
    const CLEAR: usize = 256;
    const EOD: usize = 257;
    const FIRST: usize = 258;
    const MAX: usize = 4096;

    let early = usize::from(early_change);
    let mut prefix = [0u16; MAX];
    let mut suffix = [0u8; MAX];
    let mut stack: Vec<u8> = Vec::with_capacity(MAX);
    let mut out = Vec::new();

    let mut bit_buf: u32 = 0;
    let mut bit_count: u32 = 0;
    let mut pos = 0usize;
    let mut width: u32 = 9;
    let mut next = FIRST;
    let mut old_code: i32 = -1;
    let mut first_byte: u8 = 0;

    loop {
        // Refill to at least `width` bits, MSB-first. Running dry before EOD is
        // tolerated: emit what we have (truncated-stream salvage).
        while bit_count < width && pos < data.len() {
            bit_buf = (bit_buf << 8) | u32::from(data[pos]);
            pos += 1;
            bit_count += 8;
        }
        if bit_count < width {
            break;
        }
        bit_count -= width;
        let code = ((bit_buf >> bit_count) & ((1 << width) - 1)) as usize;

        if code == EOD {
            break;
        }
        if code == CLEAR {
            width = 9;
            next = FIRST;
            old_code = -1;
            continue;
        }
        if code > next || (code == next && old_code < 0) {
            return Err(DecodeError::Corrupt("LZW code out of range"));
        }

        // Rebuild this code's string onto the stack (reversed).
        stack.clear();
        let mut in_code = if code == next {
            // KwKwK: the not-yet-defined code is prev's string + its first byte.
            stack.push(first_byte);
            old_code as usize
        } else {
            code
        };
        while in_code >= FIRST {
            stack.push(suffix[in_code]);
            in_code = usize::from(prefix[in_code]);
        }
        first_byte = in_code as u8; // in_code is now a root byte (0..=255)
        stack.push(first_byte);

        budget.charge(stack.len())?;
        out.extend(stack.iter().rev());

        // Add prev + first_byte as the next table entry (§7.4.4.2).
        if old_code >= 0 && next < MAX {
            prefix[next] = old_code as u16;
            suffix[next] = first_byte;
            next += 1;
            if width < 12 && next + early >= (1 << width) {
                width += 1;
            }
        }
        old_code = code as i32;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use pdf_object::NameTable;
    use std::io::Write;

    fn budget() -> DecodeBudget {
        DecodeBudget::new(1 << 20)
    }

    fn dict_with_filter(names: &NameTable, filter: &[u8]) -> Dictionary {
        Dictionary::from_pairs([(names.known.filter, PdfObject::Name(names.intern(filter)))])
    }

    fn zlib_compress(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn no_filter_is_identity_but_still_budgeted() {
        let names = NameTable::new();
        let dict = Dictionary::new();
        let mut b = budget();
        assert_eq!(
            decode_stream(b"plain", &dict, &names, &mut b).unwrap(),
            b"plain"
        );
        assert_eq!(b.used(), 5);
        let mut zero = DecodeBudget::new(0);
        assert!(matches!(
            decode_stream(b"plain", &dict, &names, &mut zero),
            Err(DecodeError::BudgetExceeded)
        ));
    }

    #[test]
    fn flate_roundtrip() {
        let names = NameTable::new();
        let dict = dict_with_filter(&names, b"FlateDecode");
        let raw = zlib_compress(b"the quick brown fox");
        assert_eq!(
            decode_stream(&raw, &dict, &names, &mut budget()).unwrap(),
            b"the quick brown fox"
        );
    }

    #[test]
    fn flate_headerless_raw_deflate_tolerated() {
        let names = NameTable::new();
        let dict = dict_with_filter(&names, b"FlateDecode");
        let mut enc =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"headerless").unwrap();
        let raw = enc.finish().unwrap();
        assert_eq!(
            decode_stream(&raw, &dict, &names, &mut budget()).unwrap(),
            b"headerless"
        );
    }

    #[test]
    fn flate_budget_enforced() {
        let names = NameTable::new();
        let dict = dict_with_filter(&names, b"FlateDecode");
        // 1 MiB of zeros compresses tiny but must still exceed a 1 KiB budget.
        let raw = zlib_compress(&vec![0u8; 1 << 20]);
        let mut small = DecodeBudget::new(1024);
        assert!(matches!(
            decode_stream(&raw, &dict, &names, &mut small),
            Err(DecodeError::BudgetExceeded)
        ));
    }

    #[test]
    fn flate_with_png_predictor() {
        let names = NameTable::new();
        // Up-filtered rows: [1,2,3], [4,5,6] → deltas [1,2,3],[3,3,3].
        let filtered: Vec<u8> = vec![2, 1, 2, 3, 2, 3, 3, 3];
        let raw = zlib_compress(&filtered);
        let parms = Dictionary::from_pairs([
            (names.intern(b"Predictor"), PdfObject::Integer(12)),
            (names.intern(b"Columns"), PdfObject::Integer(3)),
        ]);
        let dict = Dictionary::from_pairs([
            (
                names.known.filter,
                PdfObject::Name(names.intern(b"FlateDecode")),
            ),
            (
                names.known.decode_parms,
                PdfObject::Dictionary(parms.into()),
            ),
        ]);
        assert_eq!(
            decode_stream(&raw, &dict, &names, &mut budget()).unwrap(),
            vec![1, 2, 3, 4, 5, 6]
        );
    }

    #[test]
    fn ascii_hex() {
        let names = NameTable::new();
        let dict = dict_with_filter(&names, b"ASCIIHexDecode");
        assert_eq!(
            decode_stream(b"48 65 6C 6C 6F>", &dict, &names, &mut budget()).unwrap(),
            b"Hello"
        );
        // Odd digit → padded with zero.
        assert_eq!(
            decode_stream(b"7>", &dict, &names, &mut budget()).unwrap(),
            vec![0x70]
        );
    }

    #[test]
    fn ascii85() {
        let names = NameTable::new();
        let dict = dict_with_filter(&names, b"ASCII85Decode");
        // "Man " encodes to 9jqo^ ; 'z' is four zero bytes.
        assert_eq!(
            decode_stream(b"9jqo^~>", &dict, &names, &mut budget()).unwrap(),
            b"Man "
        );
        assert_eq!(
            decode_stream(b"z~>", &dict, &names, &mut budget()).unwrap(),
            vec![0, 0, 0, 0]
        );
        // Partial group: "Man" → 3 bytes from 4 digits.
        assert_eq!(
            decode_stream(b"9jqo~>", &dict, &names, &mut budget()).unwrap(),
            b"Man"
        );
        // Whitespace ignored.
        assert_eq!(
            decode_stream(b"9j qo\n^~>", &dict, &names, &mut budget()).unwrap(),
            b"Man "
        );
    }

    #[test]
    fn run_length() {
        let names = NameTable::new();
        let dict = dict_with_filter(&names, b"RunLengthDecode");
        // literal "AB", repeat 'C' x4 (257-253), EOD.
        assert_eq!(
            decode_stream(
                &[1, b'A', b'B', 253, b'C', 128],
                &dict,
                &names,
                &mut budget()
            )
            .unwrap(),
            b"ABCCCC"
        );
    }

    #[test]
    fn filter_chain_applies_in_order() {
        let names = NameTable::new();
        // Flate-compress, then hex-encode; /Filter [AHx Fl] decodes
        // hex first, flate second.
        let deflated = zlib_compress(b"chained");
        let hex: Vec<u8> = deflated
            .iter()
            .flat_map(|b| format!("{b:02X}").into_bytes())
            .collect();
        let dict = Dictionary::from_pairs([(
            names.known.filter,
            PdfObject::Array(
                vec![
                    PdfObject::Name(names.intern(b"ASCIIHexDecode")),
                    PdfObject::Name(names.intern(b"FlateDecode")),
                ]
                .into(),
            ),
        )]);
        assert_eq!(
            decode_stream(&hex, &dict, &names, &mut budget()).unwrap(),
            b"chained"
        );
    }

    #[test]
    fn unsupported_filter_is_typed_error() {
        let names = NameTable::new();
        let dict = dict_with_filter(&names, b"DCTDecode");
        assert!(matches!(
            decode_stream(b"...", &dict, &names, &mut budget()),
            Err(DecodeError::UnsupportedFilter(f)) if f == "DCTDecode"
        ));
    }

    #[test]
    fn corrupt_flate_with_no_output_errors() {
        let names = NameTable::new();
        let dict = dict_with_filter(&names, b"FlateDecode");
        assert!(matches!(
            decode_stream(b"\xff\xff\xff\xff", &dict, &names, &mut budget()),
            Err(DecodeError::Corrupt(_))
        ));
    }

    #[test]
    fn truncated_flate_salvages_its_inflated_prefix() {
        // PDFium renders what it can from a truncated /FlateDecode stream; we
        // must return the successfully-inflated prefix rather than dropping the
        // whole page. Compress a long payload, then cut the compressed bytes
        // short of the stream end.
        let names = NameTable::new();
        let dict = dict_with_filter(&names, b"FlateDecode");
        let original: Vec<u8> = (0..8192u32)
            .map(|i| b"the quick brown fox "[(i % 20) as usize])
            .collect();
        let full = zlib_compress(&original);
        let truncated = &full[..full.len() * 3 / 4];

        let out = decode_stream(truncated, &dict, &names, &mut budget())
            .expect("a truncated flate stream should salvage its prefix, not error");
        assert!(!out.is_empty(), "expected a salvaged prefix");
        assert!(
            out.len() < original.len(),
            "a truncated stream cannot yield the whole payload"
        );
        assert_eq!(
            &out[..],
            &original[..out.len()],
            "salvaged bytes must be a correct prefix"
        );
    }

    // ── LZWDecode ───────────────────────────────────────────────────────────

    /// An MSB-first bit sink mirroring `lzw_decode`'s reader, for the test
    /// encoder below.
    struct BitWriter {
        out: Vec<u8>,
        buf: u32,
        cnt: u32,
    }
    impl BitWriter {
        fn new() -> Self {
            Self {
                out: Vec::new(),
                buf: 0,
                cnt: 0,
            }
        }
        fn write(&mut self, code: u32, width: u32) {
            self.buf = (self.buf << width) | (code & ((1 << width) - 1));
            self.cnt += width;
            while self.cnt >= 8 {
                self.cnt -= 8;
                self.out.push((self.buf >> self.cnt) as u8);
            }
        }
        fn finish(mut self) -> Vec<u8> {
            if self.cnt > 0 {
                self.out.push((self.buf << (8 - self.cnt)) as u8);
            }
            self.out
        }
    }

    /// An independent LZW encoder (separate logic from the decoder), so a
    /// round trip is a real cross-check rather than a shared bug. Width-step
    /// timing uses the same `next + early` rule as the decoder — the two must
    /// agree on it to interoperate; that they match Adobe for `early = 1` is
    /// what the corpus/pdfium pass validates.
    fn lzw_encode(data: &[u8], early_change: bool) -> Vec<u8> {
        use std::collections::HashMap;
        const CLEAR: u32 = 256;
        const EOD: u32 = 257;
        const FIRST: u32 = 258;
        const MAX: u32 = 4096;
        let early = u32::from(early_change);

        let seed =
            || -> HashMap<Vec<u8>, u32> { (0..256u32).map(|i| (vec![i as u8], i)).collect() };
        let mut dict = seed();
        let mut next = FIRST;
        let mut width = 9u32;
        let mut bw = BitWriter::new();
        bw.write(CLEAR, width);

        let mut w: Vec<u8> = Vec::new();
        for &b in data {
            let mut wc = w.clone();
            wc.push(b);
            if dict.contains_key(&wc) {
                w = wc;
            } else {
                bw.write(dict[&w], width);
                // Freeze the table when full rather than emitting a CLEAR: this
                // mirrors the decoder's `next < MAX` freeze, so the round trip
                // exercises the shared 9→12-bit width-step logic without also
                // depending on encoder/decoder agreement on the CLEAR-reset
                // boundary. (Real Adobe streams do emit CLEAR at table-full;
                // the decoder's handling of that is validated against PDFium on
                // real corpus files, not by this self-round-trip.)
                if next < MAX {
                    dict.insert(wc, next);
                    next += 1;
                    // One entry LATER than the decoder's `>=` bump: the decoder
                    // builds its table one entry behind the encoder (it needs
                    // the next code to complete an entry), so to emit each code
                    // at the width the decoder will read it, the encoder must
                    // defer its width step by exactly one assigned code.
                    if width < 12 && next + early > (1 << width) {
                        width += 1;
                    }
                }
                w = vec![b];
            }
        }
        if !w.is_empty() {
            bw.write(dict[&w], width);
        }
        bw.write(EOD, width);
        bw.finish()
    }

    fn lzw_round_trip(data: &[u8]) {
        for early in [true, false] {
            let encoded = lzw_encode(data, early);
            let decoded = lzw_decode(&encoded, &mut budget(), early).unwrap();
            assert_eq!(
                decoded, data,
                "LZW round trip failed (early_change={early})"
            );
        }
    }

    #[test]
    fn lzw_round_trips_text_and_kwkwk() {
        lzw_round_trip(b"");
        lzw_round_trip(b"A");
        lzw_round_trip(b"TOBEORNOTTOBEORTOBEORNOT"); // the classic worked example
        lzw_round_trip(&[0x41u8; 4096]); // long run -> exercises the KwKwK case
    }

    #[test]
    fn lzw_round_trips_across_width_steps() {
        // A long, high-entropy input forces the table past 512, 1024 and 2048
        // entries, exercising every 9→10→11→12-bit width transition and the
        // table-full reset.
        let mut data = Vec::with_capacity(20_000);
        let mut x: u32 = 0x1234_5678;
        for _ in 0..20_000 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            data.push((x >> 24) as u8);
        }
        lzw_round_trip(&data);
    }

    #[test]
    fn lzw_through_decode_stream_with_early_change_param() {
        let names = NameTable::new();
        // Default /EarlyChange (1): a bare /Filter /LZWDecode.
        let dict = dict_with_filter(&names, b"LZWDecode");
        let encoded = lzw_encode(b"hello hello hello world", true);
        assert_eq!(
            decode_stream(&encoded, &dict, &names, &mut budget()).unwrap(),
            b"hello hello hello world"
        );

        // /EarlyChange 0 via /DecodeParms must be honored.
        let parms = Dictionary::from_pairs([(names.intern(b"EarlyChange"), PdfObject::Integer(0))]);
        let dict0 = Dictionary::from_pairs([
            (
                names.known.filter,
                PdfObject::Name(names.intern(b"LZWDecode")),
            ),
            (
                names.known.decode_parms,
                PdfObject::Dictionary(std::sync::Arc::new(parms)),
            ),
        ]);
        let encoded0 = lzw_encode(b"hello hello hello world", false);
        assert_eq!(
            decode_stream(&encoded0, &dict0, &names, &mut budget()).unwrap(),
            b"hello hello hello world"
        );
    }

    #[test]
    fn lzw_truncated_stream_salvages_prefix() {
        // Chop the encoded stream before EOD: the decoder returns the bytes it
        // managed to decode rather than erroring the whole page away.
        let encoded = lzw_encode(b"TOBEORNOTTOBEORTOBEORNOT", true);
        let truncated = &encoded[..encoded.len() / 2];
        let out = lzw_decode(truncated, &mut budget(), true).unwrap();
        assert!(!out.is_empty(), "expected a salvaged prefix");
        assert!(b"TOBEORNOTTOBEORTOBEORNOT".starts_with(&out[..out.len().min(4)]));
    }

    #[test]
    fn lzw_budget_is_enforced() {
        let encoded = lzw_encode(&[0x41u8; 1024], true);
        let mut tiny = DecodeBudget::new(16);
        assert!(matches!(
            lzw_decode(&encoded, &mut tiny, true),
            Err(DecodeError::BudgetExceeded)
        ));
    }
}
