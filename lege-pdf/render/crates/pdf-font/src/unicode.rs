//! PDF character-code to Unicode mappings.
//!
//! `/ToUnicode` CMaps are deliberately separate from the code→CID CMaps in
//! [`crate::cmap`]. Both are keyed by the original character code, but one
//! returns UTF-16 text while the other returns a CID used for metrics and
//! glyph selection.

use std::collections::BTreeMap;
use std::sync::Arc;

/// Which step in the PDF font fallback chain produced a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnicodeSource {
    ToUnicode,
    PredefinedCid,
    SimpleEncoding,
    FontProgram,
}

/// One owned mapping. A PDF character code can map to more than one UTF-16
/// code unit (ligatures and supplementary-plane characters are common).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnicodeMapping {
    pub utf16: Arc<[u16]>,
    pub source: UnicodeSource,
}

/// Immutable character-code → UTF-16 map for one semantic font.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnicodeMap {
    entries: BTreeMap<u32, UnicodeMapping>,
}

impl UnicodeMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, char_code: u32) -> Option<&UnicodeMapping> {
        self.entries.get(&char_code)
    }

    pub fn insert(&mut self, char_code: u32, utf16: impl Into<Arc<[u16]>>, source: UnicodeSource) {
        let utf16 = utf16.into();
        if !utf16.is_empty() {
            self.entries
                .insert(char_code, UnicodeMapping { utf16, source });
        }
    }

    /// Insert a fallback without replacing an authoritative earlier mapping
    /// (notably `/ToUnicode`).
    pub fn insert_if_absent(
        &mut self,
        char_code: u32,
        utf16: impl Into<Arc<[u16]>>,
        source: UnicodeSource,
    ) {
        if self.entries.contains_key(&char_code) {
            return;
        }
        self.insert(char_code, utf16, source);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Parse a `/ToUnicode` CMap stream.
///
/// Supports `bfchar`, both forms of `bfrange`, one-to-many mappings, and
/// surrogate pairs. Malformed records are skipped independently. `None`
/// means that the stream contained no usable mapping.
pub fn parse_to_unicode(data: &[u8]) -> Option<UnicodeMap> {
    let mut lexer = Lexer::new(data);
    let mut map = UnicodeMap::new();

    while let Some(token) = lexer.next() {
        let Token::Word(word) = token else {
            continue;
        };
        match word.as_slice() {
            b"beginbfchar" => parse_bfchar(&mut lexer, &mut map),
            b"beginbfrange" => parse_bfrange(&mut lexer, &mut map),
            _ => {}
        }
    }

    (!map.is_empty()).then_some(map)
}

fn parse_bfchar(lexer: &mut Lexer<'_>, map: &mut UnicodeMap) {
    loop {
        let Some(token) = lexer.next() else {
            break;
        };
        match token {
            Token::Word(word) if word == b"endbfchar" => break,
            Token::Hex(src) => {
                let Some(Token::Hex(dst)) = lexer.next() else {
                    continue;
                };
                if let (Some(code), Some(text)) = (source_code(&src), destination_utf16(&dst)) {
                    map.insert(code, text, UnicodeSource::ToUnicode);
                }
            }
            _ => {}
        }
    }
}

fn parse_bfrange(lexer: &mut Lexer<'_>, map: &mut UnicodeMap) {
    loop {
        let Some(token) = lexer.next() else {
            break;
        };
        match token {
            Token::Word(word) if word == b"endbfrange" => break,
            Token::Hex(lo_bytes) => {
                let Some(Token::Hex(hi_bytes)) = lexer.next() else {
                    continue;
                };
                let (Some(lo), Some(hi)) = (source_code(&lo_bytes), source_code(&hi_bytes)) else {
                    continue;
                };
                if hi < lo || hi.saturating_sub(lo) > 0x10_0000 {
                    // Bound work from a malicious range.
                    continue;
                }
                match lexer.next() {
                    Some(Token::Hex(mut dst)) => {
                        for code in lo..=hi {
                            if let Some(text) = destination_utf16(&dst) {
                                map.insert(code, text, UnicodeSource::ToUnicode);
                            }
                            increment_big_endian(&mut dst);
                        }
                    }
                    Some(Token::ArrayStart) => {
                        let mut code = lo;
                        loop {
                            match lexer.next() {
                                Some(Token::Hex(dst)) => {
                                    if code <= hi {
                                        if let Some(text) = destination_utf16(&dst) {
                                            map.insert(code, text, UnicodeSource::ToUnicode);
                                        }
                                        code = code.saturating_add(1);
                                    }
                                }
                                Some(Token::ArrayEnd) | None => break,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn source_code(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || bytes.len() > 4 {
        return None;
    }
    Some(
        bytes
            .iter()
            .fold(0u32, |value, &byte| (value << 8) | byte as u32),
    )
}

fn destination_utf16(bytes: &[u8]) -> Option<Arc<[u16]>> {
    if bytes.is_empty() {
        return None;
    }
    let bytes = bytes.strip_prefix(&[0xfe, 0xff]).unwrap_or(bytes);
    if bytes.is_empty() {
        return None;
    }
    let mut words = Vec::with_capacity(bytes.len().div_ceil(2));
    let mut chunks = bytes.chunks_exact(2);
    for pair in &mut chunks {
        words.push(u16::from_be_bytes([pair[0], pair[1]]));
    }
    if let Some(&last) = chunks.remainder().first() {
        words.push(u16::from_be_bytes([last, 0]));
    }
    (!words.is_empty()).then(|| words.into())
}

fn increment_big_endian(bytes: &mut [u8]) {
    for byte in bytes.iter_mut().rev() {
        let (next, carry) = byte.overflowing_add(1);
        *byte = next;
        if !carry {
            break;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Hex(Vec<u8>),
    ArrayStart,
    ArrayEnd,
    Word(Vec<u8>),
}

struct Lexer<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Lexer<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn next(&mut self) -> Option<Token> {
        loop {
            self.skip_space_and_comments();
            let byte = *self.data.get(self.offset)?;
            match byte {
                b'<' if self.data.get(self.offset + 1) != Some(&b'<') => {
                    self.offset += 1;
                    let start = self.offset;
                    while self.data.get(self.offset).is_some_and(|&b| b != b'>') {
                        self.offset += 1;
                    }
                    let bytes = parse_hex(&self.data[start..self.offset]);
                    self.offset += usize::from(self.offset < self.data.len());
                    return Some(Token::Hex(bytes));
                }
                b'[' => {
                    self.offset += 1;
                    return Some(Token::ArrayStart);
                }
                b']' => {
                    self.offset += 1;
                    return Some(Token::ArrayEnd);
                }
                b'<' | b'>' | b'{' | b'}' | b'(' | b')' => {
                    // Dictionaries and literal strings do not contain records
                    // accepted by this parser. Consume their delimiter.
                    self.offset += 1;
                }
                _ => {
                    let start = self.offset;
                    while self
                        .data
                        .get(self.offset)
                        .is_some_and(|&b| !is_delimiter(b))
                    {
                        self.offset += 1;
                    }
                    if self.offset == start {
                        self.offset += 1;
                        continue;
                    }
                    return Some(Token::Word(self.data[start..self.offset].to_vec()));
                }
            }
        }
    }

    fn skip_space_and_comments(&mut self) {
        loop {
            while self
                .data
                .get(self.offset)
                .is_some_and(|b| b.is_ascii_whitespace())
            {
                self.offset += 1;
            }
            if self.data.get(self.offset) != Some(&b'%') {
                break;
            }
            while self
                .data
                .get(self.offset)
                .is_some_and(|&b| b != b'\r' && b != b'\n')
            {
                self.offset += 1;
            }
        }
    }
}

fn is_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'(' | b')' | b'%' | b'/'
        )
}

fn parse_hex(input: &[u8]) -> Vec<u8> {
    let digits: Vec<u8> = input
        .iter()
        .copied()
        .filter(|b| b.is_ascii_hexdigit())
        .collect();
    let mut out = Vec::with_capacity(digits.len().div_ceil(2));
    for pair in digits.chunks(2) {
        let high = hex_value(pair[0]);
        let low = pair.get(1).copied().map(hex_value).unwrap_or(0);
        out.push((high << 4) | low);
    }
    out
}

fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn units(map: &UnicodeMap, code: u32) -> Vec<u16> {
        map.get(code).unwrap().utf16.to_vec()
    }

    #[test]
    fn parses_bfchar_and_surrogate_pairs() {
        let map = parse_to_unicode(b"2 beginbfchar <01> <0041> <02> <D83DDE00> endbfchar").unwrap();
        assert_eq!(units(&map, 1), vec![0x0041]);
        assert_eq!(units(&map, 2), vec![0xd83d, 0xde00]);
    }

    #[test]
    fn parses_sequential_and_array_bfrange() {
        let map = parse_to_unicode(
            b"2 beginbfrange \
              <10> <12> <0061> \
              <20> <22> [<0066><0069006A><006B>] \
              endbfrange",
        )
        .unwrap();
        assert_eq!(units(&map, 0x10), vec![0x61]);
        assert_eq!(units(&map, 0x12), vec![0x63]);
        assert_eq!(units(&map, 0x20), vec![0x66]);
        assert_eq!(units(&map, 0x21), vec![0x69, 0x6a]);
        assert_eq!(units(&map, 0x22), vec![0x6b]);
    }

    #[test]
    fn later_records_replace_earlier_ones() {
        let map = parse_to_unicode(
            b"1 beginbfchar <01><0041> endbfchar \
              1 beginbfchar <01><0042> endbfchar",
        )
        .unwrap();
        assert_eq!(units(&map, 1), vec![0x42]);
        assert_eq!(map.get(1).unwrap().source, UnicodeSource::ToUnicode);
    }
}
