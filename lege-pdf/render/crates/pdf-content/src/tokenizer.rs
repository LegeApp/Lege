//! Content-stream tokenizer (ISO 32000-1 §7.8.2).
//!
//! A content stream is a flat sequence of *operands* followed by *operators*:
//! `72 720 Td` pushes two numbers then applies `Td`. This reuses the Phase 1
//! [`pdf_syntax::Lexer`] verbatim — the same tokenizer that reads file-level
//! objects reads content operands, so string/name/number semantics never
//! drift between the two.
//!
//! What this layer adds on top of the raw token stream:
//!
//! - Assembling `[...]` and `<<...>>` operands into whole [`Operand`] values
//!   (the lexer only emits open/close markers).
//! - Framing inline images: `BI <dict> ID <raw bytes> EI` is not a normal
//!   operator/operand shape — the bytes between `ID` and `EI` are arbitrary
//!   binary and must be lifted out before the token stream can continue.
//!
//! Operator dispatch (what `Td` *means*) is the interpreter's job, not this
//! layer's; the tokenizer only classifies operands vs. operators.

use pdf_syntax::classify::{is_delimiter, is_whitespace};
use pdf_syntax::{Lexer, SyntaxLimits, Token};

use crate::ContentError;

/// A content-stream operand: a primitive value that precedes an operator.
///
/// Names and strings keep their decoded bytes (the lexer resolves `#xx` and
/// string escapes). Numbers keep PDF's int/real distinction; geometry code
/// coerces through [`Operand::as_f64`].
#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Int(i64),
    Real(f64),
    Bool(bool),
    Null,
    Name(Vec<u8>),
    String(Vec<u8>),
    Array(Vec<Operand>),
    Dict(Vec<(Vec<u8>, Operand)>),
}

impl Operand {
    /// Numeric value with PDF's implicit int→real coercion. `None` for
    /// non-numeric operands.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Operand::Int(i) => Some(*i as f64),
            Operand::Real(r) => Some(*r),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Operand::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_name(&self) -> Option<&[u8]> {
        match self {
            Operand::Name(n) => Some(n),
            _ => None,
        }
    }

    pub fn as_string(&self) -> Option<&[u8]> {
        match self {
            Operand::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Operand]> {
        match self {
            Operand::Array(a) => Some(a),
            _ => None,
        }
    }
}

/// One unit produced by the tokenizer.
#[derive(Debug, Clone, PartialEq)]
pub enum Lexeme {
    /// An operand to be pushed on the interpreter's operand stack.
    Operand(Operand),
    /// An operator keyword (e.g. `q`, `cm`, `Tj`). The interpreter consumes
    /// the pending operand stack.
    Operator(Vec<u8>),
    /// A fully-framed inline image (`BI … ID … EI`): its parameter dictionary
    /// plus the raw (still-encoded) sample bytes.
    InlineImage {
        dict: Vec<(Vec<u8>, Operand)>,
        data: Vec<u8>,
    },
}

/// Streaming content tokenizer over a decoded content-stream byte slice.
#[derive(Debug)]
pub struct ContentLexer<'a> {
    lexer: Lexer<'a>,
    limits: &'a SyntaxLimits,
}

impl<'a> ContentLexer<'a> {
    pub fn new(input: &'a [u8], limits: &'a SyntaxLimits) -> Self {
        Self {
            lexer: Lexer::new(input, 0, limits),
            limits,
        }
    }

    /// Current position in the content byte slice.
    pub fn pos(&self) -> usize {
        self.lexer.pos()
    }

    /// Step the cursor over a single byte, returning `false` at end of input.
    /// The interpreter's recovery loop calls this to guarantee forward progress
    /// past a delimiter the lexer refuses to consume (e.g. a stray `)`), so a
    /// malformed token can never spin the loop forever.
    pub fn skip_byte(&mut self) -> bool {
        let pos = self.lexer.pos();
        if pos >= self.lexer.input().len() {
            return false;
        }
        self.lexer.set_pos(pos + 1);
        true
    }

    /// Produce the next lexeme, or `None` at end of stream.
    pub fn next_lexeme(&mut self) -> Result<Option<Lexeme>, ContentError> {
        let tok = self.lexer.next_token()?;
        match tok.value {
            Token::Eof => Ok(None),
            Token::Keyword(kw) => {
                if kw == b"BI" {
                    self.read_inline_image().map(Some)
                } else {
                    Ok(Some(Lexeme::Operator(kw)))
                }
            }
            other => Ok(Some(Lexeme::Operand(self.operand_from_token(other, 0)?))),
        }
    }

    /// Build a full operand from an already-read token, recursing into
    /// composite `[...]` / `<<...>>` operands. `depth` guards nesting.
    fn operand_from_token(&mut self, tok: Token, depth: usize) -> Result<Operand, ContentError> {
        if depth > self.limits.max_nesting_depth {
            // A DoS guard, not content malformation: surface it on its own
            // variant so the interpreter's recovery loop still treats it as
            // fatal (it must never be skipped-and-continued).
            return Err(ContentError::NestingDepth(depth));
        }
        match tok {
            Token::Boolean(b) => Ok(Operand::Bool(b)),
            Token::Integer(i) => Ok(Operand::Int(i)),
            Token::Real(r) => Ok(Operand::Real(r)),
            Token::Null => Ok(Operand::Null),
            Token::String(s) => Ok(Operand::String(s)),
            Token::Name(n) => Ok(Operand::Name(n)),
            Token::ArrayOpen => self.read_array(depth),
            Token::DictOpen => Ok(Operand::Dict(self.read_dict(depth)?)),
            // A close marker or bare keyword where an operand was required.
            Token::ArrayClose | Token::DictClose => Err(ContentError::Malformed(
                "stray array/dict close in operands".into(),
            )),
            Token::Keyword(k) => Err(ContentError::Malformed(format!(
                "keyword {:?} where operand expected",
                String::from_utf8_lossy(&k)
            ))),
            Token::Eof => Err(ContentError::Malformed(
                "end of stream inside operand".into(),
            )),
        }
    }

    /// Read the body of an array whose `[` was already consumed.
    fn read_array(&mut self, depth: usize) -> Result<Operand, ContentError> {
        let mut items = Vec::new();
        loop {
            let tok = self.lexer.next_token()?;
            match tok.value {
                Token::ArrayClose => return Ok(Operand::Array(items)),
                Token::Eof => {
                    return Err(ContentError::Malformed("unterminated array operand".into()));
                }
                other => items.push(self.operand_from_token(other, depth + 1)?),
            }
        }
    }

    /// Read the body of a dictionary whose `<<` was already consumed.
    fn read_dict(&mut self, depth: usize) -> Result<Vec<(Vec<u8>, Operand)>, ContentError> {
        let mut pairs = Vec::new();
        loop {
            let key_tok = self.lexer.next_token()?;
            let key = match key_tok.value {
                Token::DictClose => return Ok(pairs),
                Token::Name(n) => n,
                Token::Eof => {
                    return Err(ContentError::Malformed("unterminated dict operand".into()));
                }
                _ => return Err(ContentError::Malformed("dict key must be a name".into())),
            };
            let val_tok = self.lexer.next_token()?;
            let value = self.operand_from_token(val_tok.value, depth + 1)?;
            pairs.push((key, value));
        }
    }

    /// Frame an inline image: the `BI` keyword was already consumed. Reads the
    /// parameter dictionary (name/value pairs) up to `ID`, then lifts the raw
    /// sample bytes, up to and excluding the `EI` terminator.
    ///
    /// Framing is length- and filter-aware (ISO 32000-1 §8.9.7), which is what
    /// makes a false `EI` inside binary sample data harmless:
    ///
    /// - If `/L`/`/Length` is present, that byte count is authoritative.
    /// - Otherwise, if the data is *unfiltered*, the exact length is derived
    ///   from the geometry: `ceil(W·BPC·ncomp / 8)` bytes per row × `H` rows,
    ///   with `ncomp` taken from `/CS` (or 1 for an `/IM` image mask).
    /// - Otherwise (filtered, no length) we fall back to scanning for a
    ///   whitespace-bounded `EI`, made DCT-aware: a `/DCTDecode` payload's true
    ///   terminator follows the JPEG end-of-image marker (`FF D9`), so we anchor
    ///   the scan there rather than stopping at the first `EI`-looking bytes.
    ///
    /// Recovery: if framing still cannot locate the end, we do *not* return a
    /// page-fatal error. Instead the cursor is advanced to a best-effort resync
    /// point and a distinct, recoverable [`ContentError::InlineImage`] is
    /// returned; the interpreter's run loop maps that to a `note_recovery` and
    /// drops just this image. `InlineImage` is used rather than threading a
    /// `ParseContext` into this ctx-free tokenizer (the least-invasive design
    /// that keeps recovery observable).
    ///
    /// Abbreviated inline keys are handled alongside their full spellings
    /// (`/W`=`/Width`, `/H`=`/Height`, `/BPC`=`/BitsPerComponent`,
    /// `/CS`=`/ColorSpace`, `/F`=`/Filter`, `/IM`=`/ImageMask`, `/L`=`/Length`).
    fn read_inline_image(&mut self) -> Result<Lexeme, ContentError> {
        let mut dict = Vec::new();
        loop {
            let tok = self.lexer.next_token()?;
            match tok.value {
                Token::Keyword(k) if k == b"ID" => break,
                Token::Name(key) => {
                    let val_tok = self.lexer.next_token()?;
                    let value = self.operand_from_token(val_tok.value, 0)?;
                    dict.push((key, value));
                }
                Token::Eof => {
                    return Err(ContentError::Malformed("inline image missing ID".into()));
                }
                _ => {
                    return Err(ContentError::Malformed(
                        "inline image key must be a name".into(),
                    ));
                }
            }
        }

        // `ID` is followed by exactly one whitespace byte, then binary data.
        let input = self.lexer.input();
        let mut start = self.lexer.pos();
        if start < input.len() && is_whitespace(input[start]) {
            start += 1;
        }

        // Pull the framing parameters (abbreviated or full spellings).
        let is_mask = matches!(
            dict_get(&dict, &[b"IM", b"ImageMask"]),
            Some(Operand::Bool(true))
        );
        let width = dict_get(&dict, &[b"W", b"Width"])
            .and_then(Operand::as_int)
            .unwrap_or(0);
        let height = dict_get(&dict, &[b"H", b"Height"])
            .and_then(Operand::as_int)
            .unwrap_or(0);
        let mut bpc = dict_get(&dict, &[b"BPC", b"BitsPerComponent"])
            .and_then(Operand::as_int)
            .unwrap_or(0);
        if is_mask {
            bpc = 1;
        }
        let cs = dict_get(&dict, &[b"CS", b"ColorSpace"]);
        let filters = inline_filters(dict_get(&dict, &[b"F", b"Filter"]));
        let declared_len = dict_get(&dict, &[b"L", b"Length"])
            .and_then(Operand::as_int)
            .filter(|&n| n >= 0)
            .map(|n| n as usize);

        // An authoritative byte count: trust `/L`, else derive the exact length
        // of unfiltered sample data from the geometry.
        let exact_len = declared_len.or_else(|| {
            if !filters.is_empty() {
                return None;
            }
            let ncomp = inline_ncomp(cs, is_mask)?;
            if width <= 0 || height <= 0 || bpc <= 0 {
                return None;
            }
            let row_bytes = ((width as u64) * (bpc as u64) * (ncomp as u64)).div_ceil(8);
            usize::try_from(row_bytes.saturating_mul(height as u64)).ok()
        });

        let framed = exact_len
            .and_then(|n| frame_by_length(input, start, n))
            .or_else(|| frame_by_scan(input, start, &filters));

        let (data_end, resync) = match framed {
            Some(fr) => fr,
            None => {
                // Give up locating the end: advance to a best-effort resync so
                // the run loop continues past the image rather than
                // re-tokenizing its binary payload, and surface a recoverable
                // framing error for the loop to note.
                let resync = find_ws_bounded_ei(input, start)
                    .map(|ei| ei + 2)
                    .unwrap_or(input.len());
                self.lexer.set_pos(resync);
                return Err(ContentError::InlineImage(
                    "could not locate EI terminator".into(),
                ));
            }
        };

        let data = input[start..data_end].to_vec();
        self.lexer.set_pos(resync);
        Ok(Lexeme::InlineImage { dict, data })
    }
}

/// First value in `dict` whose key matches any of `keys`.
fn dict_get<'d>(dict: &'d [(Vec<u8>, Operand)], keys: &[&[u8]]) -> Option<&'d Operand> {
    dict.iter()
        .find(|(k, _)| keys.iter().any(|kk| *kk == k.as_slice()))
        .map(|(_, v)| v)
}

/// Component count implied by an inline image's `/CS`, or 1 for an image mask.
/// `None` when the space is a named resource or otherwise unknown here (the
/// caller then cannot derive an exact length and falls back to scanning).
fn inline_ncomp(cs: Option<&Operand>, is_mask: bool) -> Option<u32> {
    if is_mask {
        return Some(1);
    }
    match cs {
        Some(Operand::Name(n)) => match n.as_slice() {
            b"G" | b"DeviceGray" | b"CalGray" | b"Gray" => Some(1),
            b"RGB" | b"DeviceRGB" | b"CalRGB" => Some(3),
            b"CMYK" | b"DeviceCMYK" => Some(4),
            b"I" | b"Indexed" => Some(1),
            _ => None,
        },
        // A written-out space, most often `[/I /RGB <hival> <palette>]`. Only
        // the head matters here: the component count of the *samples*, not of
        // the base space. Getting this wrong is not a colour bug but a framing
        // one — without an exact length the caller scans for a whitespace-
        // bounded `EI`, and 262 KB of palette indices (pdfbox/2385_1) contains
        // such a sequence long before the real end, truncating the image to its
        // first few rows.
        Some(Operand::Array(items)) => match items.first() {
            Some(Operand::Name(n)) => match n.as_slice() {
                b"I" | b"Indexed" => Some(1),
                b"CalGray" => Some(1),
                b"CalRGB" | b"Lab" => Some(3),
                // `/ICCBased` needs the stream's `/N`, `/Separation` and
                // `/DeviceN` their tint transform — none of which an inline
                // image can carry (no indirect references). Leave them to the
                // scan rather than guess.
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// Filter names on an inline image's `/F` (single name or array of names).
fn inline_filters(f: Option<&Operand>) -> Vec<Vec<u8>> {
    match f {
        Some(Operand::Name(n)) => vec![n.clone()],
        Some(Operand::Array(items)) => items
            .iter()
            .filter_map(|o| match o {
                Operand::Name(n) => Some(n.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn is_dct(name: &[u8]) -> bool {
    matches!(name, b"DCT" | b"DCTDecode")
}

/// Frame `n` bytes of sample data starting at `start`. Returns
/// `(data_end, resync)` where `resync` is the cursor position just past the
/// `EI`. Returns `None` when `n` overshoots the input (truncated), so the
/// caller can fall back to scanning.
fn frame_by_length(input: &[u8], start: usize, n: usize) -> Option<(usize, usize)> {
    let data_end = start.checked_add(n)?;
    if data_end > input.len() {
        return None;
    }
    // The `n` bytes are the data verbatim (a false `EI` inside them is kept);
    // `EI` should follow after optional whitespace.
    let mut j = data_end;
    while j < input.len() && is_whitespace(input[j]) {
        j += 1;
    }
    let resync = if input.get(j) == Some(&b'E') && input.get(j + 1) == Some(&b'I') {
        j + 2
    } else {
        // Length is authoritative but `EI` is not where expected: locate the
        // next whitespace-bounded `EI`, else resume right after the data.
        find_ws_bounded_ei(input, data_end)
            .map(|ei| ei + 2)
            .unwrap_or(data_end)
    };
    Some((data_end, resync))
}

/// Frame filtered/unknown-length data by scanning for `EI`. Returns
/// `(data_end, resync)`, or `None` when no plausible `EI` exists.
fn frame_by_scan(input: &[u8], start: usize, filters: &[Vec<u8>]) -> Option<(usize, usize)> {
    // DCT payloads embed bytes that can look like a whitespace-bounded `EI`;
    // the true terminator follows the JPEG end-of-image marker (`FF D9`).
    if filters.iter().any(|f| is_dct(f)) {
        if let Some(eoi) = rfind2(input, [0xFF, 0xD9], start) {
            if let Some(ei) = find_ws_bounded_ei(input, eoi + 2) {
                return Some((trim_trailing_ws(input, start, ei), ei + 2));
            }
        }
    }
    // An ASCII-armoured payload is *text*, so a whitespace-bounded `EI` can
    // occur inside it by chance — and does: bug1077808 carries 31 inline images
    // filtered `[/A85 /Fl]`, and the first one's base-85 body contains "\r\nEI("
    // about 3.5 KB in. The scan stopped there and the rest of a 10 MB content
    // stream was tokenized as operators, which is what produced its flood of
    // "keyword where operand expected" recoveries.
    //
    // These encodings carry their own end-of-data marker, so use it: `~>` for
    // ASCII85, `>` for ASCII-hex. PDFium reaches the same place from the other
    // side, by running the decoder and taking the input bytes it consumed
    // (`CPDF_StreamParser::ReadInlineImage` -> `DecodeInlineStream`).
    if let Some(first) = filters.first() {
        // RunLength is binary, so its literal packets may contain a perfectly
        // whitespace-bounded `EI`. Unlike Flate/CCITT, however, its framing is
        // cheap to parse without decoding the image: walk packet headers until
        // the 128 end-of-data marker. The marker is meaningful only in header
        // position; a raw byte search would mistake literal 0x80 sample data
        // for the end.
        if is_rle(first)
            && let Some(end) = run_length_eod(input, start)
            && find_ws_bounded_bi(input, start).is_none_or(|bi| bi >= end)
            && let Some(ei) = find_ws_bounded_ei(input, end)
        {
            return Some((end, ei + 2));
        }

        let eod: &[u8] = if is_a85(first) {
            b"~>"
        } else if is_ahx(first) {
            b">"
        } else {
            b""
        };
        if !eod.is_empty()
            && let Some(end) = find_bytes(input, eod, start)
            // Only trust the marker if it belongs to *this* image. A writer
            // that omits the end-of-data marker (issue11385 has two `[/A85/Fl]`
            // masks and only one `~>` between them) would otherwise hand us the
            // *next* image's marker, and the frame would swallow every operator
            // in between — that page lost all of its text. An intervening `BI`
            // is the giveaway: no second image can start inside this one's
            // payload.
            && find_ws_bounded_bi(input, start).is_none_or(|bi| bi > end)
            && let Some(ei) = find_ws_bounded_ei(input, end + eod.len())
        {
            return Some((trim_trailing_ws(input, start, ei), ei + 2));
        }
    }
    let ei = find_ws_bounded_ei(input, start)?;
    Some((trim_trailing_ws(input, start, ei), ei + 2))
}

/// `/ASCII85Decode` or its inline abbreviation.
fn is_a85(f: &[u8]) -> bool {
    f == b"A85" || f == b"ASCII85Decode"
}

/// `/ASCIIHexDecode` or its inline abbreviation.
fn is_ahx(f: &[u8]) -> bool {
    f == b"AHx" || f == b"ASCIIHexDecode"
}

/// `/RunLengthDecode` or its inline abbreviation.
fn is_rle(f: &[u8]) -> bool {
    f == b"RL" || f == b"RunLengthDecode"
}

/// End-exclusive position of a structurally valid RunLength stream, including
/// its 128 end-of-data byte. Literal packet contents are skipped rather than
/// inspected, so an embedded 0x80 or `EI` sequence cannot terminate framing.
fn run_length_eod(input: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    loop {
        let header = *input.get(pos)?;
        pos += 1;
        match header {
            128 => return Some(pos),
            0..=127 => {
                let literal_len = usize::from(header) + 1;
                pos = pos.checked_add(literal_len)?;
                if pos > input.len() {
                    return None;
                }
            }
            129..=255 => {
                // A repeat packet carries one source byte.
                input.get(pos)?;
                pos += 1;
            }
        }
    }
}

/// First whitespace-bounded `BI` keyword at or after `from` — the start of the
/// *next* inline image, which bounds how far the current one's payload can run.
fn find_ws_bounded_bi(input: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < input.len() {
        let before_ok = i == 0 || is_whitespace(input[i - 1]);
        let after = input.get(i + 2).copied();
        let after_ok = after.is_none_or(|b| is_whitespace(b) || is_delimiter(b));
        if input[i] == b'B' && input[i + 1] == b'I' && before_ok && after_ok {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// First occurrence of `needle` at or after `from`.
fn find_bytes(input: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= input.len() || needle.is_empty() {
        return None;
    }
    input[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

/// Index of the `E` of the first whitespace-bounded `EI` at or after `from`.
fn find_ws_bounded_ei(input: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < input.len() {
        let before_ok = i == 0 || is_whitespace(input[i - 1]);
        let after = input.get(i + 2).copied();
        let after_ok = after.is_none_or(|b| is_whitespace(b) || is_delimiter(b));
        if input[i] == b'E' && input[i + 1] == b'I' && before_ok && after_ok {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Last index at or after `from` where the two-byte `needle` occurs.
fn rfind2(hay: &[u8], needle: [u8; 2], from: usize) -> Option<usize> {
    if from + 2 > hay.len() {
        return None;
    }
    let mut i = hay.len() - 2;
    loop {
        if i >= from && hay[i] == needle[0] && hay[i + 1] == needle[1] {
            return Some(i);
        }
        if i == from {
            return None;
        }
        i -= 1;
    }
}

/// Drop the single delimiting whitespace byte (if any) that precedes `EI`.
fn trim_trailing_ws(input: &[u8], start: usize, ei: usize) -> usize {
    let mut data_end = ei;
    if data_end > start && is_whitespace(input[data_end - 1]) {
        data_end -= 1;
    }
    data_end
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn lex_all(input: &[u8]) -> Vec<Lexeme> {
        let limits = SyntaxLimits::default();
        let mut lx = ContentLexer::new(input, &limits);
        let mut out = Vec::new();
        while let Some(l) = lx.next_lexeme().expect("lex error") {
            out.push(l);
        }
        out
    }

    fn op(s: &[u8]) -> Lexeme {
        Lexeme::Operator(s.to_vec())
    }

    #[test]
    fn operands_then_operator() {
        assert_eq!(
            lex_all(b"72 720 Td"),
            vec![
                Lexeme::Operand(Operand::Int(72)),
                Lexeme::Operand(Operand::Int(720)),
                op(b"Td"),
            ]
        );
    }

    #[test]
    fn reals_and_names() {
        assert_eq!(
            lex_all(b"/DeviceRGB cs 0.2 0.3 0.4 sc"),
            vec![
                Lexeme::Operand(Operand::Name(b"DeviceRGB".to_vec())),
                op(b"cs"),
                Lexeme::Operand(Operand::Real(0.2)),
                Lexeme::Operand(Operand::Real(0.3)),
                Lexeme::Operand(Operand::Real(0.4)),
                op(b"sc"),
            ]
        );
    }

    #[test]
    fn tj_array_operand() {
        // `[(A) -120 (B)] TJ`
        let out = lex_all(b"[(A) -120 (B)] TJ");
        assert_eq!(
            out,
            vec![
                Lexeme::Operand(Operand::Array(vec![
                    Operand::String(b"A".to_vec()),
                    Operand::Int(-120),
                    Operand::String(b"B".to_vec()),
                ])),
                op(b"TJ"),
            ]
        );
    }

    #[test]
    fn booleans_are_operands_not_operators() {
        assert_eq!(
            lex_all(b"true false BX"),
            vec![
                Lexeme::Operand(Operand::Bool(true)),
                Lexeme::Operand(Operand::Bool(false)),
                op(b"BX"),
            ]
        );
    }

    #[test]
    fn text_show_sequence() {
        assert_eq!(
            lex_all(b"BT /F1 12 Tf 72 720 Td (Page 0) Tj ET"),
            vec![
                op(b"BT"),
                Lexeme::Operand(Operand::Name(b"F1".to_vec())),
                Lexeme::Operand(Operand::Int(12)),
                op(b"Tf"),
                Lexeme::Operand(Operand::Int(72)),
                Lexeme::Operand(Operand::Int(720)),
                op(b"Td"),
                Lexeme::Operand(Operand::String(b"Page 0".to_vec())),
                op(b"Tj"),
                op(b"ET"),
            ]
        );
    }

    #[test]
    fn inline_image_framing() {
        // BI /W 2 /H 2 /BPC 8 /CS /RGB ID <12 raw bytes> EI
        let data: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let mut src = b"q BI /W 2 /H 2 /BPC 8 /CS /RGB ID ".to_vec();
        src.extend_from_slice(&data);
        src.extend_from_slice(b"\nEI Q");
        let out = lex_all(&src);
        assert_eq!(out.first(), Some(&op(b"q")));
        assert_eq!(out.last(), Some(&op(b"Q")));
        let Lexeme::InlineImage { dict, data: got } = &out[1] else {
            panic!("expected inline image, got {:?}", out[1]);
        };
        assert_eq!(got, &data);
        assert_eq!(dict.len(), 4);
        assert_eq!(dict[0], (b"W".to_vec(), Operand::Int(2)));
        assert_eq!(dict[3], (b"CS".to_vec(), Operand::Name(b"RGB".to_vec())));
    }

    #[test]
    fn stray_close_is_malformed() {
        let limits = SyntaxLimits::default();
        let mut lx = ContentLexer::new(b"] Q", &limits);
        assert!(matches!(lx.next_lexeme(), Err(ContentError::Malformed(_))));
    }

    /// Length-aware framing must consume the exact geometric byte count and NOT
    /// stop at a false `EI` sitting inside the raw sample data (A1).
    #[test]
    fn inline_image_length_frames_past_false_ei() {
        // Unfiltered 4×1 DeviceGray, 8bpc ⇒ exactly 4 data bytes. The middle two
        // bytes are `E` `I`, whitespace-bounded — the old scanner would have
        // stopped there and truncated the image to one byte.
        let data: [u8; 4] = [0x20, b'E', b'I', 0x20];
        let mut src = b"q BI /W 4 /H 1 /BPC 8 /CS /G ID ".to_vec();
        src.extend_from_slice(&data);
        src.extend_from_slice(b"\nEI Q");
        let out = lex_all(&src);
        let Lexeme::InlineImage { data: got, .. } = &out[1] else {
            panic!("expected inline image, got {:?}", out[1]);
        };
        assert_eq!(got, &data, "length framing must keep all 4 bytes");
        assert_eq!(
            out.last(),
            Some(&op(b"Q")),
            "cursor must resync past EI to Q"
        );
    }

    /// A filtered `/DCTDecode` inline image whose payload contains a false
    /// whitespace-bounded `EI` frames on the JPEG EOI marker (`FF D9`), not the
    /// first `EI`-looking bytes (A1).
    #[test]
    fn inline_image_filtered_dct_frames_on_eoi() {
        // Payload: a false " EI " then the JPEG EOI (FF D9). The true EI follows.
        let payload: [u8; 6] = [0x20, b'E', b'I', 0x20, 0xFF, 0xD9];
        let mut src = b"BI /W 2 /H 2 /CS /RGB /F /DCTDecode ID ".to_vec();
        src.extend_from_slice(&payload);
        src.extend_from_slice(b"\nEI Q");
        let out = lex_all(&src);
        let Lexeme::InlineImage { data: got, .. } = &out[0] else {
            panic!("expected inline image, got {:?}", out[0]);
        };
        assert_eq!(
            got, &payload,
            "DCT framing must span the whole payload to FF D9"
        );
        assert_eq!(out.last(), Some(&op(b"Q")));
    }

    /// RunLength literal packets can contain a false whitespace-bounded `EI`.
    /// Framing must parse packet lengths through the 128 EOD marker.
    #[test]
    fn inline_image_run_length_frames_on_structural_eod() {
        // One six-byte literal packet, including " EI ", followed by EOD.
        let payload: [u8; 8] = [5, 0x20, b'E', b'I', 0x20, 0x11, 0x22, 128];
        let mut src = b"BI /W 6 /H 1 /BPC 8 /CS /G /F /RL ID ".to_vec();
        src.extend_from_slice(&payload);
        src.extend_from_slice(b"\nEI Q");
        let out = lex_all(&src);
        let Lexeme::InlineImage { data: got, .. } = &out[0] else {
            panic!("expected inline image, got {:?}", out[0]);
        };
        assert_eq!(
            got, &payload,
            "RunLength framing must keep the literal false EI and EOD"
        );
        assert_eq!(out.last(), Some(&op(b"Q")));
    }

    #[test]
    fn run_length_eod_ignores_literal_0x80_and_handles_repeat_packets() {
        // literal(0x80), repeat(three 0x41), EOD
        let encoded = [0, 0x80, 254, 0x41, 128, b' ', b'E', b'I'];
        assert_eq!(run_length_eod(&encoded, 0), Some(5));
        assert_eq!(run_length_eod(&encoded[..4], 0), None);
    }

    /// When no `EI` can be located at all, framing returns the recoverable
    /// [`ContentError::InlineImage`] (not a hard `Malformed`), and advances the
    /// cursor so the run loop can continue (A1 recovery).
    #[test]
    fn inline_image_missing_ei_is_recoverable() {
        let limits = SyntaxLimits::default();
        let mut src = b"BI /W 2 /H 2 /CS /RGB /F /DCTDecode ID ".to_vec();
        src.extend_from_slice(&[0x00, 0x01, 0x02, 0x03]); // no EI, no FF D9
        let mut lx = ContentLexer::new(&src, &limits);
        let first = lx.next_lexeme();
        assert!(
            matches!(first, Err(ContentError::InlineImage(_))),
            "missing EI must be a recoverable InlineImage error, got {first:?}"
        );
        // Cursor advanced to end: the next pull is a clean end-of-stream.
        assert!(matches!(lx.next_lexeme(), Ok(None)));
    }
}
