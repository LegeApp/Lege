//! Token stream → `PdfObject` tree.
//!
//! This is the primitive-object parser from skeleton-blueprint.md §7. It
//! lives in `pdf-object` (not `pdf-syntax`) because the workspace dependency
//! direction is `syntax ← object`: the lexer cannot name the object model.
//!
//! Tolerance decisions are ported from PDFium's
//! `CPDF_SyntaxParser::GetObjectBodyInternal` / `ReadStream` (intent, not
//! structure); each is documented at its site. Fatal problems are typed
//! [`SyntaxError`]s; deviations we *repair* are reported to the caller as
//! [`IndirectRepair`]s so upper layers can log them as recovery events —
//! repairs must be observable, never silent.

use std::sync::Arc;

use pdf_syntax::{Lexer, Offset, SyntaxError, SyntaxLimits, Token};

use crate::{Dictionary, NameTable, ObjectId, PdfObject, PdfStream, PdfString, StreamData};

/// Callback resolving an indirect `/Length` value. Returns `None` when the
/// length cannot be resolved (e.g. during structural recovery before an
/// xref exists); the parser then falls back to scanning for `endstream`.
pub type LengthResolver<'r> = dyn FnMut(ObjectId) -> Option<i64> + 'r;

/// A tolerated deviation repaired while parsing an indirect object.
/// The caller (structure/document layer) maps these onto its recovery log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndirectRepair {
    /// `/Length` was missing, unresolvable, or did not land on `endstream`;
    /// the actual body length was determined by scanning.
    StreamLengthRepaired { declared: Option<i64>, actual: u64 },
    /// The closing `endobj` keyword was absent.
    MissingEndObj,
}

/// A parsed `N G obj … endobj` wrapper.
#[derive(Debug)]
pub struct ParsedIndirect {
    pub id: ObjectId,
    pub object: PdfObject,
    pub repairs: Vec<IndirectRepair>,
}

/// Parser over a byte window. Positions/spans are absolute file offsets via
/// the lexer's base offset; stream bodies are recorded as
/// [`StreamData::InSource`] ranges — body bytes are never tokenized.
#[derive(Debug)]
pub struct ObjectParser<'a> {
    lexer: Lexer<'a>,
    names: &'a NameTable,
}

impl<'a> ObjectParser<'a> {
    pub fn new(
        input: &'a [u8],
        base_offset: Offset,
        limits: &'a SyntaxLimits,
        names: &'a NameTable,
    ) -> Self {
        Self { lexer: Lexer::new(input, base_offset, limits), names }
    }

    /// Build directly on an existing lexer (used by the structure layer,
    /// which interleaves token-level and object-level reads).
    pub fn from_lexer(lexer: Lexer<'a>, names: &'a NameTable) -> Self {
        Self { lexer, names }
    }

    /// Position relative to the window start.
    pub fn pos(&self) -> usize {
        self.lexer.pos()
    }

    pub fn set_pos(&mut self, pos: usize) {
        self.lexer.set_pos(pos);
    }

    /// Absolute offset of the current position.
    pub fn offset(&self) -> Offset {
        self.lexer.offset()
    }

    pub fn lexer_mut(&mut self) -> &mut Lexer<'a> {
        &mut self.lexer
    }

    fn limits(&self) -> &'a SyntaxLimits {
        self.lexer.limits()
    }

    /// Parse one direct object (no `N G obj` wrapper, no streams).
    pub fn parse_object(&mut self) -> Result<PdfObject, SyntaxError> {
        self.parse_object_at_depth(0)
    }

    fn parse_object_at_depth(&mut self, depth: usize) -> Result<PdfObject, SyntaxError> {
        if depth > self.limits().max_nesting_depth {
            return Err(SyntaxError::TooDeep { offset: self.lexer.offset() });
        }
        let tok = self.lexer.next_token()?;
        match tok.value {
            Token::Null => Ok(PdfObject::Null),
            Token::Boolean(b) => Ok(PdfObject::Boolean(b)),
            Token::Real(r) => Ok(PdfObject::Real(r)),
            Token::String(s) => Ok(PdfObject::String(PdfString::new(s))),
            Token::Name(n) => Ok(PdfObject::Name(self.names.intern(&n))),
            Token::Integer(i) => Ok(self.integer_or_reference(i)?),
            Token::ArrayOpen => self.parse_array_body(depth),
            Token::DictOpen => self.parse_dict_body(depth).map(|d| PdfObject::Dictionary(Arc::new(d))),
            Token::Keyword(kw) => {
                // PDF has no exponent notation, but keywords like `1e10`
                // occur in the wild. DECISION (documented): a keyword that
                // starts numerically and parses as a float is tolerated as
                // a Real. PDFium instead fails the enclosing object; we
                // preserve the value, which is strictly more tolerant and
                // deterministic.
                if kw.first().is_some_and(|&c| pdf_syntax::classify::is_numeric_char(c))
                    && let Ok(text) = std::str::from_utf8(&kw)
                    && let Ok(v) = text.parse::<f64>()
                    && v.is_finite()
                {
                    return Ok(PdfObject::Real(v));
                }
                Err(SyntaxError::UnexpectedToken { expected: "object", offset: tok.start })
            }
            Token::ArrayClose | Token::DictClose => {
                Err(SyntaxError::UnexpectedToken { expected: "object", offset: tok.start })
            }
            Token::Eof => Err(SyntaxError::UnexpectedEof { offset: tok.start }),
        }
    }

    /// `N` was read; decide `N` vs `N G R` with two-token lookahead.
    fn integer_or_reference(&mut self, value: i64) -> Result<PdfObject, SyntaxError> {
        let save = self.lexer.pos();
        // Both lookahead reads tolerate lex errors by restoring: a binary
        // blob after an integer must not fail the integer itself.
        let ok = (|| -> Result<Option<PdfObject>, SyntaxError> {
            let t2 = match self.lexer.next_token() {
                Ok(t) => t,
                Err(_) => return Ok(None),
            };
            let Token::Integer(generation) = t2.value else {
                return Ok(None);
            };
            let t3 = match self.lexer.next_token() {
                Ok(t) => t,
                Err(_) => return Ok(None),
            };
            let Token::Keyword(kw) = t3.value else {
                return Ok(None);
            };
            if kw != b"R" {
                return Ok(None);
            }
            let (Ok(number), Ok(generation)) = (u32::try_from(value), u16::try_from(generation))
            else {
                // Out-of-range object ids: not a plausible reference —
                // fall back to the plain integer.
                return Ok(None);
            };
            if number == u32::MAX {
                // PDFium's kInvalidObjNum: never a valid reference.
                return Ok(None);
            }
            Ok(Some(PdfObject::Reference(ObjectId::new(number, generation))))
        })()?;
        match ok {
            Some(reference) => Ok(reference),
            None => {
                self.lexer.set_pos(save);
                Ok(PdfObject::Integer(value))
            }
        }
    }

    fn parse_array_body(&mut self, depth: usize) -> Result<PdfObject, SyntaxError> {
        let mut items = Vec::new();
        loop {
            let peeked = self.lexer.peek_token()?;
            match peeked.value {
                Token::ArrayClose => {
                    self.lexer.next_token()?;
                    return Ok(PdfObject::Array(items.into()));
                }
                Token::Eof => {
                    return Err(SyntaxError::UnexpectedEof { offset: peeked.start });
                }
                _ => items.push(self.parse_object_at_depth(depth + 1)?),
            }
        }
    }

    /// Dictionary body after `<<`.
    ///
    /// Tolerances ported from PDFium: non-name tokens in key position are
    /// skipped; an `endobj`/`stream` keyword in key position terminates the
    /// dictionary (pushed back for the caller); empty-name keys parse but
    /// drop their value; duplicate keys resolve to the last occurrence.
    fn parse_dict_body(&mut self, depth: usize) -> Result<Dictionary, SyntaxError> {
        let mut pairs: Vec<(crate::NameId, PdfObject)> = Vec::new();
        loop {
            let save = self.lexer.pos();
            let tok = self.lexer.next_token()?;
            match tok.value {
                Token::DictClose => return Ok(Dictionary::from_pairs(pairs)),
                Token::Eof => return Err(SyntaxError::UnexpectedEof { offset: tok.start }),
                Token::Name(key) => {
                    let value = self.parse_object_at_depth(depth + 1)?;
                    if key.is_empty() {
                        // `/ <value>` — tolerated, pair dropped (PDFium
                        // requires a key of at least one byte).
                        continue;
                    }
                    pairs.push((self.names.intern(&key), value));
                }
                Token::Keyword(kw) if kw == b"endobj" || kw == b"stream" => {
                    // Unterminated dictionary running into the object
                    // trailer keywords: stop here and let the caller see
                    // the keyword (PDFium pushes `endobj` back too).
                    self.lexer.set_pos(save);
                    return Ok(Dictionary::from_pairs(pairs));
                }
                // Anything else in key position is skipped (tolerated).
                _ => {}
            }
        }
    }

    /// Parse an indirect object `N G obj <body> endobj`, including the
    /// `stream … endstream` protocol when the body is a stream dictionary.
    ///
    /// `resolve_length` supplies values for indirect `/Length` entries; it
    /// may return `None`, in which case (like any wrong length) the body is
    /// delimited by scanning for `endstream` and a repair is reported.
    pub fn parse_indirect(
        &mut self,
        resolve_length: &mut LengthResolver<'_>,
    ) -> Result<ParsedIndirect, SyntaxError> {
        let t1 = self.lexer.next_token()?;
        let Token::Integer(number) = t1.value else {
            return Err(SyntaxError::UnexpectedToken { expected: "object number", offset: t1.start });
        };
        let t2 = self.lexer.next_token()?;
        let Token::Integer(generation) = t2.value else {
            return Err(SyntaxError::UnexpectedToken {
                expected: "generation number",
                offset: t2.start,
            });
        };
        let (Ok(number), Ok(generation)) = (u32::try_from(number), u16::try_from(generation))
        else {
            return Err(SyntaxError::UnexpectedToken { expected: "valid object id", offset: t1.start });
        };
        let t3 = self.lexer.next_token()?;
        if !matches!(&t3.value, Token::Keyword(kw) if kw == b"obj") {
            return Err(SyntaxError::UnexpectedToken { expected: "obj keyword", offset: t3.start });
        }
        let id = ObjectId::new(number, generation);

        let body = self.parse_object_at_depth(0)?;
        let mut repairs = Vec::new();

        // Decide between `endobj` and the stream protocol.
        let save = self.lexer.pos();
        let after = self.lexer.next_token()?;
        let object = match (&after.value, body) {
            (Token::Keyword(kw), PdfObject::Dictionary(dict)) if kw == b"stream" => {
                let stream = self.read_stream_body(&dict, id, resolve_length, &mut repairs)?;
                // Consume a trailing `endobj` if present.
                let save2 = self.lexer.pos();
                match self.lexer.next_token() {
                    Ok(t) if matches!(&t.value, Token::Keyword(kw) if kw == b"endobj") => {}
                    _ => {
                        self.lexer.set_pos(save2);
                        repairs.push(IndirectRepair::MissingEndObj);
                    }
                }
                PdfObject::Stream(Arc::new(PdfStream {
                    dict: Arc::unwrap_or_clone(dict),
                    data: stream,
                }))
            }
            (Token::Keyword(kw), body) if kw == b"endobj" => body,
            (_, body) => {
                // Missing `endobj` — tolerated (PDFium never checks), but
                // reported so callers can log it. Token is pushed back.
                self.lexer.set_pos(save);
                repairs.push(IndirectRepair::MissingEndObj);
                body
            }
        };

        Ok(ParsedIndirect { id, object, repairs })
    }

    /// The `stream` keyword was just consumed. Delimit the body, validate
    /// `/Length`, repair by scanning when needed, and leave the lexer
    /// positioned after `endstream`.
    fn read_stream_body(
        &mut self,
        dict: &Dictionary,
        id: ObjectId,
        resolve_length: &mut LengthResolver<'_>,
        repairs: &mut Vec<IndirectRepair>,
    ) -> Result<StreamData, SyntaxError> {
        let input = self.lexer.input();

        // Per ISO 32000-1 §7.3.8.1 the `stream` keyword is followed by
        // CRLF or LF. Tolerated extras: trailing spaces/tabs before the
        // EOL, and a lone CR (sloppy writers). We deliberately do NOT skip
        // a whole line like PDFium's ToNextLine — a missing EOL must not
        // eat body bytes.
        let mut pos = self.lexer.pos();
        while matches!(input.get(pos), Some(b' ') | Some(b'\t')) {
            pos += 1;
        }
        if input.get(pos) == Some(&b'\r') {
            pos += 1;
            if input.get(pos) == Some(&b'\n') {
                pos += 1;
            }
        } else if input.get(pos) == Some(&b'\n') {
            pos += 1;
        }
        let data_start = pos;

        let declared: Option<i64> = match dict.get(self.names.known.length) {
            Some(PdfObject::Integer(n)) => Some(*n),
            Some(PdfObject::Real(r)) => Some(*r as i64),
            Some(PdfObject::Reference(len_id)) => resolve_length(*len_id),
            _ => None,
        };
        let _ = id; // Currently only used by callers mapping repairs.

        // Happy path: /Length is sane and `endstream` follows the body.
        if let Some(len) = declared
            && len >= 0
            && let Ok(len_usize) = usize::try_from(len)
            && data_start.checked_add(len_usize).is_some_and(|end| end <= input.len())
        {
            let end = data_start + len_usize;
            let mut check = self.lexer.clone();
            check.set_pos(end);
            if matches!(check.next_token(),
                Ok(t) if matches!(&t.value, Token::Keyword(kw) if kw == b"endstream"))
            {
                self.lexer.set_pos(check.pos());
                return Ok(StreamData::InSource {
                    offset: self.lexer.base_offset() + data_start as u64,
                    len: len_usize as u64,
                });
            }
        }

        // Repair path: scan for the first `endstream` (or, failing that,
        // `endobj` — streams truncated before their trailer keyword), and
        // trim one EOL marker immediately preceding it (PDFium intent).
        let tail = &input[data_start..];
        let end_rel = find_subslice(tail, b"endstream")
            .or_else(|| find_subslice(tail, b"endobj"))
            .ok_or(SyntaxError::UnexpectedEof {
                offset: self.lexer.base_offset() + input.len() as u64,
            })?;
        let mut body_len = end_rel;
        if body_len > 0 && tail[body_len - 1] == b'\n' {
            body_len -= 1;
            if body_len > 0 && tail[body_len - 1] == b'\r' {
                body_len -= 1;
            }
        } else if body_len > 0 && tail[body_len - 1] == b'\r' {
            body_len -= 1;
        }
        repairs.push(IndirectRepair::StreamLengthRepaired { declared, actual: body_len as u64 });

        // Position after the delimiting keyword; consume `endstream` if
        // that is what we found (do not consume a delimiting `endobj` —
        // parse_indirect handles it).
        let keyword_pos = data_start + end_rel;
        self.lexer.set_pos(keyword_pos);
        if tail[end_rel..].starts_with(b"endstream") {
            let _ = self.lexer.next_token();
        }
        Ok(StreamData::InSource {
            offset: self.lexer.base_offset() + data_start as u64,
            len: body_len as u64,
        })
    }
}

/// First occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn parse(names: &NameTable, input: &[u8]) -> PdfObject {
        let limits = SyntaxLimits::default();
        let mut p = ObjectParser::new(input, 0, &limits, names);
        p.parse_object().expect("parse error")
    }

    fn parse_err(names: &NameTable, input: &[u8]) -> SyntaxError {
        let limits = SyntaxLimits::default();
        let mut p = ObjectParser::new(input, 0, &limits, names);
        p.parse_object().expect_err("expected parse error")
    }

    #[test]
    fn primitives() {
        let names = NameTable::new();
        assert!(matches!(parse(&names, b"null"), PdfObject::Null));
        assert!(matches!(parse(&names, b"true"), PdfObject::Boolean(true)));
        assert!(matches!(parse(&names, b"42"), PdfObject::Integer(42)));
        assert!(matches!(parse(&names, b"-1.5"), PdfObject::Real(v) if v == -1.5));
        assert!(
            matches!(parse(&names, b"(bytes)"), PdfObject::String(s) if s.as_bytes() == b"bytes")
        );
        let obj = parse(&names, b"/MediaBox");
        assert_eq!(obj.as_name(), Some(names.known.media_box));
    }

    #[test]
    fn references_need_two_ints_and_r() {
        let names = NameTable::new();
        assert!(matches!(
            parse(&names, b"12 0 R"),
            PdfObject::Reference(id) if id == ObjectId::new(12, 0)
        ));
        // Not references: missing generation, non-R keyword, real numbers.
        assert!(matches!(parse(&names, b"12 R"), PdfObject::Integer(12)));
        assert!(matches!(parse(&names, b"12 0 RR"), PdfObject::Integer(12)));
        assert!(matches!(parse(&names, b"12 1.0 R"), PdfObject::Integer(12)));
        // Out-of-range ids degrade to plain integers.
        assert!(matches!(parse(&names, b"5000000000 0 R"), PdfObject::Integer(5000000000)));
        assert!(matches!(parse(&names, b"12 99999 R"), PdfObject::Integer(12)));
        // PDFium kInvalidObjNum (ported from SyntaxParserTest.GetInvalidReference).
        assert!(matches!(parse(&names, b"4294967295 0 R"), PdfObject::Integer(4294967295)));
    }

    #[test]
    fn arrays_nested() {
        let names = NameTable::new();
        let PdfObject::Array(items) = parse(&names, b"[/a[/b]]") else {
            panic!("expected array");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].as_name(), Some(names.intern(b"a")));
        let PdfObject::Array(inner) = &items[1] else {
            panic!("expected inner array");
        };
        assert_eq!(inner[0].as_name(), Some(names.intern(b"b")));
    }

    #[test]
    fn array_with_references_and_comments() {
        let names = NameTable::new();
        let PdfObject::Array(items) = parse(&names, b"[1 0 R % note\n 2 0 R 7]") else {
            panic!("expected array");
        };
        assert_eq!(items.len(), 3);
        assert!(matches!(items[0], PdfObject::Reference(id) if id.number == 1));
        assert!(matches!(items[1], PdfObject::Reference(id) if id.number == 2));
        assert!(matches!(items[2], PdfObject::Integer(7)));
    }

    #[test]
    fn dictionaries() {
        let names = NameTable::new();
        let obj = parse(&names, b"<</Type/Page/Count 3/Kids[4 0 R]>>");
        let dict = obj.as_dict().unwrap();
        assert_eq!(dict.get(names.known.type_).and_then(PdfObject::as_name), Some(names.known.page));
        assert_eq!(dict.get(names.known.count).and_then(PdfObject::as_int), Some(3));
        assert!(matches!(dict.get(names.known.kids), Some(PdfObject::Array(_))));
    }

    #[test]
    fn dict_duplicate_keys_last_wins() {
        let names = NameTable::new();
        let obj = parse(&names, b"<</N 1/N 2>>");
        let dict = obj.as_dict().unwrap();
        assert_eq!(dict.len(), 1);
        assert_eq!(dict.get(names.known.n).and_then(PdfObject::as_int), Some(2));
    }

    #[test]
    fn dict_tolerates_junk_keys_and_empty_names() {
        let names = NameTable::new();
        // Integer in key position skipped; `/ 9` empty-name pair dropped.
        let obj = parse(&names, b"<< 42 /Count 3 / 9 >>");
        let dict = obj.as_dict().unwrap();
        assert_eq!(dict.len(), 1);
        assert_eq!(dict.get(names.known.count).and_then(PdfObject::as_int), Some(3));
    }

    #[test]
    fn dict_unterminated_stops_at_endobj() {
        let names = NameTable::new();
        let limits = SyntaxLimits::default();
        let mut p = ObjectParser::new(b"<</Count 3 endobj", 0, &limits, &names);
        let obj = p.parse_object().unwrap();
        assert_eq!(obj.as_dict().unwrap().get(names.known.count).and_then(PdfObject::as_int), Some(3));
        // endobj is still there for the caller.
        let t = p.lexer_mut().next_token().unwrap();
        assert!(matches!(&t.value, Token::Keyword(kw) if kw == b"endobj"));
    }

    #[test]
    fn exponent_keyword_falls_back_to_real() {
        let names = NameTable::new();
        assert!(matches!(parse(&names, b"1e10"), PdfObject::Real(v) if v == 1e10));
        assert!(matches!(parse(&names, b"-2.5e-3"), PdfObject::Real(v) if v == -2.5e-3));
        // Non-numeric keywords still fail.
        assert!(matches!(
            parse_err(&names, b"bogus"),
            SyntaxError::UnexpectedToken { .. }
        ));
    }

    #[test]
    fn depth_limit_enforced() {
        let names = NameTable::new();
        let limits = SyntaxLimits { max_token_bytes: 1024, max_nesting_depth: 4 };
        let deep = b"[[[[[[1]]]]]]";
        let mut p = ObjectParser::new(deep, 0, &limits, &names);
        assert!(matches!(p.parse_object(), Err(SyntaxError::TooDeep { .. })));
    }

    fn parse_indirect_ok(names: &NameTable, input: &[u8]) -> ParsedIndirect {
        let limits = SyntaxLimits::default();
        let mut p = ObjectParser::new(input, 0, &limits, names);
        p.parse_indirect(&mut |_| None).expect("parse_indirect failed")
    }

    #[test]
    fn indirect_simple() {
        let names = NameTable::new();
        let parsed = parse_indirect_ok(&names, b"7 0 obj (hi) endobj");
        assert_eq!(parsed.id, ObjectId::new(7, 0));
        assert!(parsed.repairs.is_empty());
        assert!(matches!(parsed.object, PdfObject::String(s) if s.as_bytes() == b"hi"));
    }

    #[test]
    fn indirect_missing_endobj_reported() {
        let names = NameTable::new();
        let parsed = parse_indirect_ok(&names, b"7 0 obj 42 ");
        assert!(matches!(parsed.object, PdfObject::Integer(42)));
        assert_eq!(parsed.repairs, vec![IndirectRepair::MissingEndObj]);
    }

    #[test]
    fn stream_with_correct_length() {
        let names = NameTable::new();
        let input = b"5 0 obj <</Length 11>> stream\nhello world\nendstream endobj";
        let parsed = parse_indirect_ok(&names, input);
        assert!(parsed.repairs.is_empty());
        let PdfObject::Stream(s) = &parsed.object else { panic!("expected stream") };
        let StreamData::InSource { offset, len } = s.data else { panic!("expected InSource") };
        assert_eq!(&input[offset as usize..(offset + len) as usize], b"hello world");
    }

    #[test]
    fn stream_crlf_after_keyword() {
        let names = NameTable::new();
        let input = b"5 0 obj <</Length 5>> stream\r\nABCDE\r\nendstream endobj";
        let parsed = parse_indirect_ok(&names, input);
        let PdfObject::Stream(s) = &parsed.object else { panic!("expected stream") };
        let StreamData::InSource { offset, len } = s.data else { panic!("expected InSource") };
        assert_eq!(&input[offset as usize..(offset + len) as usize], b"ABCDE");
        assert!(parsed.repairs.is_empty());
    }

    #[test]
    fn stream_wrong_length_repaired_by_scan() {
        let names = NameTable::new();
        // /Length claims 3 but the body is 11 bytes.
        let input = b"5 0 obj <</Length 3>> stream\nhello world\nendstream endobj";
        let parsed = parse_indirect_ok(&names, input);
        assert_eq!(
            parsed.repairs,
            vec![IndirectRepair::StreamLengthRepaired { declared: Some(3), actual: 11 }]
        );
        let PdfObject::Stream(s) = &parsed.object else { panic!("expected stream") };
        let StreamData::InSource { offset, len } = s.data else { panic!("expected InSource") };
        assert_eq!(&input[offset as usize..(offset + len) as usize], b"hello world");
    }

    #[test]
    fn stream_missing_length_repaired() {
        let names = NameTable::new();
        let input = b"5 0 obj <<>> stream\nDATA\nendstream endobj";
        let parsed = parse_indirect_ok(&names, input);
        assert_eq!(
            parsed.repairs,
            vec![IndirectRepair::StreamLengthRepaired { declared: None, actual: 4 }]
        );
    }

    #[test]
    fn stream_indirect_length_resolved() {
        let names = NameTable::new();
        let limits = SyntaxLimits::default();
        let input = b"5 0 obj <</Length 6 0 R>> stream\nDATA\nendstream endobj";
        let mut p = ObjectParser::new(input, 0, &limits, &names);
        let parsed = p
            .parse_indirect(&mut |id| (id == ObjectId::new(6, 0)).then_some(4))
            .unwrap();
        assert!(parsed.repairs.is_empty());
        let PdfObject::Stream(s) = &parsed.object else { panic!("expected stream") };
        let StreamData::InSource { len, .. } = s.data else { panic!("expected InSource") };
        assert_eq!(len, 4);
    }

    #[test]
    fn stream_binary_body_with_delimiters_not_tokenized() {
        let names = NameTable::new();
        // Body contains bytes that would be lexer errors — must not matter.
        let body: &[u8] = b")))>>>\x00\xff(((";
        let mut input = b"9 0 obj <</Length 11>> stream\n".to_vec();
        input.extend_from_slice(body);
        input.extend_from_slice(b"\nendstream endobj");
        let limits = SyntaxLimits::default();
        let mut p = ObjectParser::new(&input, 0, &limits, &names);
        let parsed = p.parse_indirect(&mut |_| None).unwrap();
        let PdfObject::Stream(s) = &parsed.object else { panic!("expected stream") };
        let StreamData::InSource { offset, len } = s.data else { panic!("expected InSource") };
        assert_eq!(&input[offset as usize..(offset + len) as usize], body);
    }

    #[test]
    fn stream_truncated_without_endstream_errors() {
        let names = NameTable::new();
        let limits = SyntaxLimits::default();
        let input = b"5 0 obj <</Length 99>> stream\nshort body";
        let mut p = ObjectParser::new(input, 0, &limits, &names);
        assert!(matches!(p.parse_indirect(&mut |_| None), Err(SyntaxError::UnexpectedEof { .. })));
    }

    #[test]
    fn base_offset_flows_into_stream_data() {
        let names = NameTable::new();
        let limits = SyntaxLimits::default();
        let input = b"5 0 obj <</Length 4>> stream\nDATA\nendstream endobj";
        let mut p = ObjectParser::new(input, 5000, &limits, &names);
        let parsed = p.parse_indirect(&mut |_| None).unwrap();
        let PdfObject::Stream(s) = &parsed.object else { panic!("expected stream") };
        let StreamData::InSource { offset, .. } = s.data else { panic!("expected InSource") };
        assert_eq!(offset, 5000 + 29);
    }
}
