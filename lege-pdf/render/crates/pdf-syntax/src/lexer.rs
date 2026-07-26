//! The PDF tokenizer.
//!
//! Operates over a contiguous byte slice (callers obtain slices from
//! `PdfSource::read_range` / `as_contiguous`) with an explicit *base
//! offset*, so spans always report absolute file positions even when the
//! caller lexes a window out of a larger file.
//!
//! Compatibility decisions in this file are ported from PDFium's
//! `CPDF_SyntaxParser` (intent, not structure) and are documented at each
//! site. Deviations from PDFium are flagged with `DEVIATION`.

use crate::classify::{hex_value, is_eol, is_numeric_char, is_regular, is_whitespace, octal_value};
use crate::{Offset, Spanned, SyntaxError, SyntaxLimits, Token};

/// A restartable tokenizer over a byte slice.
#[derive(Debug, Clone)]
pub struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
    base: Offset,
    limits: &'a SyntaxLimits,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a [u8], base_offset: Offset, limits: &'a SyntaxLimits) -> Self {
        Self {
            input,
            pos: 0,
            base: base_offset,
            limits,
        }
    }

    /// Current position, relative to the slice start.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Reposition (relative to the slice start). Positions past the end are
    /// clamped to the end.
    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos.min(self.input.len());
    }

    /// Absolute file offset of the current position.
    pub fn offset(&self) -> Offset {
        self.base + self.pos as Offset
    }

    /// The underlying slice.
    pub fn input(&self) -> &'a [u8] {
        self.input
    }

    /// The absolute offset of the slice start.
    pub fn base_offset(&self) -> Offset {
        self.base
    }

    /// The limits this lexer enforces (shared with parser layers above).
    pub fn limits(&self) -> &'a SyntaxLimits {
        self.limits
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    /// Skip whitespace and `%` comments (comments run to EOL and count as
    /// whitespace, ISO 32000-1 §7.2.4). `%%EOF` and friends are therefore
    /// invisible between tokens, matching every real-world reader.
    pub fn skip_whitespace_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(b) if is_whitespace(b) => {
                    self.pos += 1;
                }
                Some(b'%') => {
                    self.pos += 1;
                    while let Some(b) = self.peek() {
                        if is_eol(b) {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => return,
            }
        }
    }

    /// If the bytes at the current position begin a line ending, consume
    /// exactly one EOL marker (CRLF counts as one) and return true.
    pub fn consume_eol(&mut self) -> bool {
        match self.peek() {
            Some(b'\n') => {
                self.pos += 1;
                true
            }
            Some(b'\r') => {
                self.pos += 1;
                if self.peek() == Some(b'\n') {
                    self.pos += 1;
                }
                true
            }
            _ => false,
        }
    }

    /// Produce the next token.
    pub fn next_token(&mut self) -> Result<Spanned<Token>, SyntaxError> {
        self.skip_whitespace_and_comments();
        let start = self.offset();
        let Some(b) = self.peek() else {
            return Ok(Spanned {
                value: Token::Eof,
                start,
                end: start,
            });
        };

        let value = match b {
            b'[' => {
                self.pos += 1;
                Token::ArrayOpen
            }
            b']' => {
                self.pos += 1;
                Token::ArrayClose
            }
            b'<' => {
                self.pos += 1;
                if self.peek() == Some(b'<') {
                    self.pos += 1;
                    Token::DictOpen
                } else {
                    Token::String(self.read_hex_string(start)?)
                }
            }
            b'>' => {
                self.pos += 1;
                if self.peek() == Some(b'>') {
                    self.pos += 1;
                    Token::DictClose
                } else {
                    return Err(SyntaxError::UnexpectedByte {
                        byte: b'>',
                        offset: start,
                    });
                }
            }
            b'(' => {
                self.pos += 1;
                Token::String(self.read_literal_string(start)?)
            }
            b')' => {
                return Err(SyntaxError::UnexpectedByte {
                    byte: b')',
                    offset: start,
                });
            }
            b'/' => {
                self.pos += 1;
                Token::Name(self.read_name(start)?)
            }
            // `{`/`}` are delimiters with no file-level meaning (they occur
            // in Type 4 function programs). Surface them as one-byte
            // keywords so the content layer can consume them later.
            b'{' | b'}' => {
                self.pos += 1;
                Token::Keyword(vec![b])
            }
            _ => {
                // Regular-character run: number, boolean, null, or keyword.
                let run_start = self.pos;
                while let Some(c) = self.peek() {
                    if !is_regular(c) {
                        break;
                    }
                    self.pos += 1;
                    if self.pos - run_start > self.limits.max_token_bytes {
                        return Err(SyntaxError::TokenTooLong { offset: start });
                    }
                }
                let run = &self.input[run_start..self.pos];
                debug_assert!(!run.is_empty());
                Self::classify_word(run, start)?
            }
        };

        Ok(Spanned {
            value,
            start,
            end: self.offset(),
        })
    }

    /// Look at the next token without consuming it.
    pub fn peek_token(&mut self) -> Result<Spanned<Token>, SyntaxError> {
        let save = self.pos;
        let tok = self.next_token();
        self.pos = save;
        tok
    }

    /// Classify a regular-character run.
    fn classify_word(run: &[u8], start: Offset) -> Result<Token, SyntaxError> {
        match run {
            b"true" => return Ok(Token::Boolean(true)),
            b"false" => return Ok(Token::Boolean(false)),
            b"null" => return Ok(Token::Null),
            _ => {}
        }
        // A run made up entirely of numeric characters is a number token
        // (PDFium `WordType::kNumber`). Runs that merely *start* numeric
        // ("1e10") are keywords; the object layer decides their fate.
        if run.iter().all(|&c| is_numeric_char(c)) {
            return Self::parse_number(run, start);
        }
        Ok(Token::Keyword(run.to_vec()))
    }

    /// Parse a numeric run with PDFium's prefix semantics: one optional
    /// leading sign, integer digits, one optional dot, fraction digits.
    /// Trailing junk *within the numeric character class* ("1-2", "1.2.3")
    /// is ignored — the valid prefix wins, matching `FX_Number`.
    ///
    /// DEVIATION from PDFium: integers are `i64`, and literals that
    /// overflow degrade to `Real` instead of clamping (skeleton `Token`
    /// contract; PDFium clamps to 32-bit with unsigned quirks).
    fn parse_number(run: &[u8], start: Offset) -> Result<Token, SyntaxError> {
        let mut i = 0usize;
        if matches!(run.first(), Some(b'+') | Some(b'-')) {
            i = 1;
        }
        let digits_start = i;
        while i < run.len() && run[i].is_ascii_digit() {
            i += 1;
        }
        let int_digits = i - digits_start;
        let mut frac_digits = 0usize;
        let mut has_dot = false;
        if run.get(i) == Some(&b'.') {
            has_dot = true;
            i += 1;
            let frac_start = i;
            while i < run.len() && run[i].is_ascii_digit() {
                i += 1;
            }
            frac_digits = i - frac_start;
        }
        let prefix = &run[..i];

        if int_digits == 0 && frac_digits == 0 {
            // "-", "+", ".", "-." … PDFium yields zero.
            return Ok(if has_dot {
                Token::Real(0.0)
            } else {
                Token::Integer(0)
            });
        }

        // The prefix is pure ASCII by construction.
        let text = std::str::from_utf8(prefix)
            .map_err(|_| SyntaxError::MalformedNumber { offset: start })?;
        if !has_dot {
            match text.parse::<i64>() {
                Ok(v) => return Ok(Token::Integer(v)),
                // Out-of-range integer degrades to Real (see above).
                Err(_) => {
                    return text
                        .parse::<f64>()
                        .map(Token::Real)
                        .map_err(|_| SyntaxError::MalformedNumber { offset: start });
                }
            }
        }
        text.parse::<f64>()
            .map(Token::Real)
            .map_err(|_| SyntaxError::MalformedNumber { offset: start })
    }

    /// Literal string body; the opening `(` is already consumed.
    ///
    /// Escapes per ISO 32000-1 §7.3.4.2. Unknown escapes drop the
    /// backslash. Balanced nested parentheses are literal.
    ///
    /// DEVIATION from PDFium: an unescaped EOL inside the string is
    /// normalized to a single LF byte, as the spec requires ("an
    /// end-of-line marker … shall be treated as a byte value of 0Ah").
    /// PDFium copies the raw CR/LF bytes through.
    fn read_literal_string(&mut self, start: Offset) -> Result<Vec<u8>, SyntaxError> {
        let mut out: Vec<u8> = Vec::new();
        let mut depth = 0usize;
        loop {
            if out.len() > self.limits.max_token_bytes {
                return Err(SyntaxError::TokenTooLong { offset: start });
            }
            let Some(b) = self.bump() else {
                return Err(SyntaxError::UnterminatedString { offset: start });
            };
            match b {
                b')' => {
                    if depth == 0 {
                        return Ok(out);
                    }
                    depth -= 1;
                    out.push(b')');
                }
                b'(' => {
                    depth += 1;
                    out.push(b'(');
                }
                b'\r' => {
                    // CR or CRLF → LF.
                    if self.peek() == Some(b'\n') {
                        self.pos += 1;
                    }
                    out.push(b'\n');
                }
                b'\\' => {
                    let Some(e) = self.bump() else {
                        return Err(SyntaxError::UnterminatedString { offset: start });
                    };
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'\\' => out.push(b'\\'),
                        b'(' => out.push(b'('),
                        b')' => out.push(b')'),
                        // Line continuation: backslash-EOL vanishes.
                        b'\n' => {}
                        b'\r' => {
                            if self.peek() == Some(b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'0'..=b'7' => {
                            // 1–3 octal digits; overflow wraps to a byte
                            // (PDFium accumulates then truncates).
                            let mut code = u16::from(e - b'0');
                            for _ in 0..2 {
                                match self.peek().and_then(octal_value) {
                                    Some(d) => {
                                        self.pos += 1;
                                        code = code * 8 + u16::from(d);
                                    }
                                    None => break,
                                }
                            }
                            out.push((code & 0xFF) as u8);
                        }
                        // Unknown escape: backslash is dropped, char kept.
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
    }

    /// Hex string body; the opening `<` is already consumed.
    ///
    /// Non-hex bytes are skipped entirely (PDFium `ReadHexString`; the spec
    /// only blesses whitespace, but skipping everything is the tolerated
    /// superset). An odd trailing digit supplies the high nibble, i.e. is
    /// padded with a trailing 0.
    fn read_hex_string(&mut self, start: Offset) -> Result<Vec<u8>, SyntaxError> {
        let mut out: Vec<u8> = Vec::new();
        let mut pending: Option<u8> = None;
        loop {
            if out.len() > self.limits.max_token_bytes {
                return Err(SyntaxError::TokenTooLong { offset: start });
            }
            let Some(b) = self.bump() else {
                return Err(SyntaxError::UnterminatedString { offset: start });
            };
            if b == b'>' {
                if let Some(hi) = pending {
                    out.push(hi << 4);
                }
                return Ok(out);
            }
            if let Some(v) = hex_value(b) {
                match pending.take() {
                    Some(hi) => out.push((hi << 4) | v),
                    None => pending = Some(v),
                }
            }
        }
    }

    /// Name body; the leading `/` is already consumed. An empty name (`/`
    /// followed by a delimiter or whitespace) is valid and yields `vec![]`.
    ///
    /// `#xx` with two valid hex digits decodes to the byte; a `#` NOT
    /// followed by two valid hex digits is kept literally (tolerated
    /// malformation — PDFium never rejects a name over bad `#` escapes).
    fn read_name(&mut self, start: Offset) -> Result<Vec<u8>, SyntaxError> {
        let mut out: Vec<u8> = Vec::new();
        while let Some(b) = self.peek() {
            if !is_regular(b) {
                break;
            }
            self.pos += 1;
            if b == b'#' {
                let hi = self.peek().and_then(hex_value);
                let lo = self.input.get(self.pos + 1).copied().and_then(hex_value);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    self.pos += 2;
                    out.push((hi << 4) | lo);
                    continue;
                }
            }
            out.push(b);
            if out.len() > self.limits.max_token_bytes {
                return Err(SyntaxError::TokenTooLong { offset: start });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn lex_all(input: &[u8]) -> Vec<Token> {
        let limits = SyntaxLimits::default();
        let mut lexer = Lexer::new(input, 0, &limits);
        let mut out = Vec::new();
        loop {
            let t = lexer.next_token().expect("lex error");
            if t.value == Token::Eof {
                return out;
            }
            out.push(t.value);
        }
    }

    fn lex_one(input: &[u8]) -> Token {
        let limits = SyntaxLimits::default();
        let mut lexer = Lexer::new(input, 0, &limits);
        lexer.next_token().expect("lex error").value
    }

    #[test]
    fn booleans_and_null() {
        assert_eq!(
            lex_all(b"true false null"),
            vec![Token::Boolean(true), Token::Boolean(false), Token::Null]
        );
    }

    #[test]
    fn integers() {
        assert_eq!(lex_one(b"123"), Token::Integer(123));
        assert_eq!(lex_one(b"+17"), Token::Integer(17));
        assert_eq!(lex_one(b"-98"), Token::Integer(-98));
        assert_eq!(lex_one(b"0"), Token::Integer(0));
    }

    #[test]
    fn reals_torture() {
        assert_eq!(lex_one(b"4."), Token::Real(4.0));
        assert_eq!(lex_one(b".5"), Token::Real(0.5));
        assert_eq!(lex_one(b"-.002"), Token::Real(-0.002));
        assert_eq!(lex_one(b"34.5"), Token::Real(34.5));
        assert_eq!(lex_one(b"-3.62"), Token::Real(-3.62));
        assert_eq!(lex_one(b"+123.6"), Token::Real(123.6));
        assert_eq!(lex_one(b"005."), Token::Real(5.0));
    }

    #[test]
    fn degenerate_numbers_match_pdfium() {
        // Sign-only / dot-only runs are zero, not errors.
        assert_eq!(lex_one(b"-"), Token::Integer(0));
        assert_eq!(lex_one(b"."), Token::Real(0.0));
        assert_eq!(lex_one(b"-."), Token::Real(0.0));
        // Junk after a valid prefix (still within numeric chars) is ignored.
        assert_eq!(lex_one(b"1-2"), Token::Integer(1));
        assert_eq!(lex_one(b"1.2.3"), Token::Real(1.2));
        assert_eq!(lex_one(b"--3"), Token::Integer(0));
    }

    #[test]
    fn integer_overflow_degrades_to_real() {
        assert_eq!(lex_one(b"9223372036854775807"), Token::Integer(i64::MAX));
        assert_eq!(
            lex_one(b"9223372036854775808"),
            Token::Real(9223372036854775808.0)
        );
        assert_eq!(
            lex_one(b"99999999999999999999999"),
            Token::Real(99999999999999999999999.0)
        );
    }

    #[test]
    fn exponent_notation_is_a_keyword() {
        // PDF has no exponent notation; "1e10" is not a number token. The
        // object layer applies a tolerant fallback (see parser tests).
        assert_eq!(lex_one(b"1e10"), Token::Keyword(b"1e10".to_vec()));
    }

    #[test]
    fn literal_strings() {
        assert_eq!(lex_one(b"()"), Token::String(vec![]));
        assert_eq!(lex_one(b"(hello)"), Token::String(b"hello".to_vec()));
        assert_eq!(lex_one(b"(a(b)c)"), Token::String(b"a(b)c".to_vec()));
        assert_eq!(lex_one(b"(a((b))c)"), Token::String(b"a((b))c".to_vec()));
    }

    #[test]
    fn literal_string_escapes() {
        assert_eq!(
            lex_one(br"(\n\r\t\b\f\\\(\))"),
            Token::String(b"\n\r\t\x08\x0C\\()".to_vec())
        );
        // Octal: 1-3 digits; overflow wraps to byte.
        assert_eq!(lex_one(br"(\053)"), Token::String(b"+".to_vec()));
        assert_eq!(lex_one(br"(\53)"), Token::String(b"+".to_vec()));
        assert_eq!(lex_one(br"(\0533)"), Token::String(b"+3".to_vec()));
        assert_eq!(lex_one(br"(\400)"), Token::String(vec![0x00])); // 0o400 & 0xFF
        // Unknown escape drops the backslash.
        assert_eq!(lex_one(br"(\z)"), Token::String(b"z".to_vec()));
    }

    #[test]
    fn literal_string_line_continuations_and_eol() {
        // Backslash-EOL vanishes (all three EOL forms).
        assert_eq!(lex_one(b"(a\\\nb)"), Token::String(b"ab".to_vec()));
        assert_eq!(lex_one(b"(a\\\rb)"), Token::String(b"ab".to_vec()));
        assert_eq!(lex_one(b"(a\\\r\nb)"), Token::String(b"ab".to_vec()));
        // Unescaped EOL normalizes to LF.
        assert_eq!(lex_one(b"(a\nb)"), Token::String(b"a\nb".to_vec()));
        assert_eq!(lex_one(b"(a\rb)"), Token::String(b"a\nb".to_vec()));
        assert_eq!(lex_one(b"(a\r\nb)"), Token::String(b"a\nb".to_vec()));
    }

    #[test]
    fn unterminated_string_errors() {
        let limits = SyntaxLimits::default();
        let mut lexer = Lexer::new(b"(abc", 0, &limits);
        assert!(matches!(
            lexer.next_token(),
            Err(SyntaxError::UnterminatedString { offset: 0 })
        ));
    }

    #[test]
    fn hex_strings() {
        assert_eq!(lex_one(b"<>"), Token::String(vec![]));
        assert_eq!(lex_one(b"<901FA3>"), Token::String(vec![0x90, 0x1F, 0xA3]));
        // Odd digit count: final digit is the high nibble.
        assert_eq!(lex_one(b"<901FA>"), Token::String(vec![0x90, 0x1F, 0xA0]));
        // Whitespace (and, tolerated, any non-hex byte) is skipped.
        assert_eq!(
            lex_one(b"<90 1F\nA3>"),
            Token::String(vec![0x90, 0x1F, 0xA3])
        );
        assert_eq!(lex_one(b"<90zz1F>"), Token::String(vec![0x90, 0x1F]));
    }

    #[test]
    fn hex_strings_pdfium_ported_cases() {
        // Ported from PDFium SyntaxParserTest.ReadHexString (the `<` is
        // already consumed there; prepended here).
        assert_eq!(lex_one(b"<  >"), Token::String(vec![]));
        assert_eq!(lex_one(b"<z12b>"), Token::String(vec![0x12, 0xB0]));
        assert_eq!(lex_one(b"<*&*#$^&@1>"), Token::String(vec![0x10]));
        assert_eq!(lex_one(b"<\x80zab>"), Token::String(vec![0xAB]));
        assert_eq!(lex_one(b"<\xffzab>"), Token::String(vec![0xAB]));
        assert_eq!(lex_one(b"<1A2b>abcd"), Token::String(vec![0x1A, 0x2B]));
        assert_eq!(lex_one(b"<1A2>asdf"), Token::String(vec![0x1A, 0x20]));
        assert_eq!(
            lex_one(b"<1A2zasdf>"),
            Token::String(vec![0x1A, 0x2A, 0xDF])
        );
        // DEVIATION from PDFium: a hex string missing its `>` at EOF is an
        // error here (PDFium returns the accumulated bytes). A typed error
        // lets recovery decide instead of silently accepting truncation.
        let limits = SyntaxLimits::default();
        assert!(matches!(
            Lexer::new(b"<1A2b", 0, &limits).next_token(),
            Err(SyntaxError::UnterminatedString { .. })
        ));
    }

    #[test]
    fn names() {
        assert_eq!(lex_one(b"/Name"), Token::Name(b"Name".to_vec()));
        assert_eq!(lex_one(b"/"), Token::Name(vec![]));
        assert_eq!(
            lex_one(b"/Name#20With#20Spaces"),
            Token::Name(b"Name With Spaces".to_vec())
        );
        assert_eq!(lex_one(b"/A#42"), Token::Name(b"AB".to_vec()));
        // Invalid hex after '#' is kept literally.
        assert_eq!(lex_one(b"/A#zz"), Token::Name(b"A#zz".to_vec()));
        assert_eq!(lex_one(b"/A#4"), Token::Name(b"A#4".to_vec()));
        assert_eq!(lex_one(b"/A#"), Token::Name(b"A#".to_vec()));
        // Delimiter ends the name.
        assert_eq!(
            lex_all(b"/A/B"),
            vec![Token::Name(b"A".to_vec()), Token::Name(b"B".to_vec())]
        );
    }

    #[test]
    fn structural_tokens() {
        assert_eq!(
            lex_all(b"[<</K 1>>]"),
            vec![
                Token::ArrayOpen,
                Token::DictOpen,
                Token::Name(b"K".to_vec()),
                Token::Integer(1),
                Token::DictClose,
                Token::ArrayClose,
            ]
        );
    }

    #[test]
    fn comments_are_whitespace() {
        assert_eq!(
            lex_all(b"1 % comment ( with ) delims << \n2"),
            vec![Token::Integer(1), Token::Integer(2)]
        );
        assert_eq!(lex_all(b"%only comment"), vec![]);
    }

    #[test]
    fn keywords() {
        assert_eq!(
            lex_all(b"12 0 obj endobj stream R"),
            vec![
                Token::Integer(12),
                Token::Integer(0),
                Token::Keyword(b"obj".to_vec()),
                Token::Keyword(b"endobj".to_vec()),
                Token::Keyword(b"stream".to_vec()),
                Token::Keyword(b"R".to_vec()),
            ]
        );
    }

    #[test]
    fn braces_are_one_byte_keywords() {
        assert_eq!(
            lex_all(b"{ 2 mul }"),
            vec![
                Token::Keyword(b"{".to_vec()),
                Token::Integer(2),
                Token::Keyword(b"mul".to_vec()),
                Token::Keyword(b"}".to_vec()),
            ]
        );
    }

    #[test]
    fn stray_close_delimiters_error() {
        let limits = SyntaxLimits::default();
        assert!(matches!(
            Lexer::new(b")", 0, &limits).next_token(),
            Err(SyntaxError::UnexpectedByte { byte: b')', .. })
        ));
        assert!(matches!(
            Lexer::new(b"> ", 0, &limits).next_token(),
            Err(SyntaxError::UnexpectedByte { byte: b'>', .. })
        ));
    }

    #[test]
    fn token_length_limit_enforced() {
        let limits = SyntaxLimits {
            max_token_bytes: 4,
            max_nesting_depth: 8,
        };
        let mut lexer = Lexer::new(b"(abcdefgh)", 0, &limits);
        assert!(matches!(
            lexer.next_token(),
            Err(SyntaxError::TokenTooLong { .. })
        ));
        let mut lexer = Lexer::new(b"/abcdefgh", 0, &limits);
        assert!(matches!(
            lexer.next_token(),
            Err(SyntaxError::TokenTooLong { .. })
        ));
        let mut lexer = Lexer::new(b"abcdefgh", 0, &limits);
        assert!(matches!(
            lexer.next_token(),
            Err(SyntaxError::TokenTooLong { .. })
        ));
    }

    #[test]
    fn spans_use_base_offset() {
        let limits = SyntaxLimits::default();
        let mut lexer = Lexer::new(b"  42", 1000, &limits);
        let t = lexer.next_token().unwrap();
        assert_eq!(t.start, 1002);
        assert_eq!(t.end, 1004);
        assert_eq!(lexer.offset(), 1004);
    }

    #[test]
    fn peek_does_not_consume() {
        let limits = SyntaxLimits::default();
        let mut lexer = Lexer::new(b"7 8", 0, &limits);
        assert_eq!(lexer.peek_token().unwrap().value, Token::Integer(7));
        assert_eq!(lexer.next_token().unwrap().value, Token::Integer(7));
        assert_eq!(lexer.next_token().unwrap().value, Token::Integer(8));
    }

    #[test]
    fn consume_eol_forms() {
        let limits = SyntaxLimits::default();
        let mut lexer = Lexer::new(b"\r\nX", 0, &limits);
        assert!(lexer.consume_eol());
        assert_eq!(lexer.pos(), 2);
        let mut lexer = Lexer::new(b"\rX", 0, &limits);
        assert!(lexer.consume_eol());
        assert_eq!(lexer.pos(), 1);
        let mut lexer = Lexer::new(b"X", 0, &limits);
        assert!(!lexer.consume_eol());
    }
}
