//! CJK character-code → CID CMaps (ISO 32000-1 §9.7.5–9.7.6).
//!
//! A Type 0 (composite) font's `/Encoding` is a CMap that both **splits** a
//! show-text byte string into character codes (variable 1–4 byte, per its
//! codespace ranges, §9.7.6.2) and **maps** each code to a CID (§9.7.6.3). Only
//! `Identity-H/V` (code = CID, always two bytes) is handled inline by
//! [`crate::metrics::FontMetrics`]; everything else — the predefined CJK CMaps
//! (`GBK-EUC-H`, `UniGB-UCS2-H`, `90ms-RKSJ-H`, …) and embedded CMap streams —
//! flows through this module.
//!
//! **Predefined tables** are transcribed from PDFium (BSD-licensed, Foxit) by
//! `tools/gen_cmaps.py` into per-charset binary blobs + [`cmap_tables`]; the
//! coding scheme + leading-byte segments come from PDFium's `cpdf_cmap.cpp`
//! `kPredefinedCMaps`, the code→CID records from `core/fpdfapi/cmaps/`.
//!
//! **Writing mode** (`/WMode`, H = 0 / V = 1) is parsed and carried, but
//! vertical *layout* (glyph stacking, V advances) stays deferred per
//! DEFERRED.md — vertical runs render horizontally for now.

use std::sync::Arc;

use crate::cmap_tables::{self, Kind, Scheme};

/// A codespace range (§9.7.6.2): codes of exactly `n` bytes whose byte `i`
/// lies in `[lo[i], hi[i]]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Codespace {
    n: u8,
    lo: [u8; 4],
    hi: [u8; 4],
}

/// The code→CID records of one CMap link (before `usecmap` fallthrough).
/// Codes are held as `u32` so an embedded four-byte codespace fits; predefined
/// records are 16-bit values widened on load.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Records {
    /// Sorted `(low, high, cid)`: `code∈[low,high] → cid + (code-low)`.
    Range(Vec<(u32, u32, u32)>),
    /// Sorted `(code, cid)`.
    Single(Vec<(u32, u32)>),
}

/// A resolved CMap: codespace ranges for byte splitting, code→CID records, an
/// optional `usecmap` base searched on a local miss, and the writing mode.
#[derive(Debug, Clone, PartialEq)]
pub struct CMap {
    name: Vec<u8>,
    /// 0 = horizontal (`-H`), 1 = vertical (`-V`). Carried, not yet laid out.
    wmode: u8,
    codespace: Vec<Codespace>,
    records: Records,
    usecmap: Option<Arc<CMap>>,
}

impl CMap {
    /// Writing mode: 0 horizontal, 1 vertical.
    pub fn wmode(&self) -> u8 {
        self.wmode
    }

    /// Split the next character code off the front of `s`, returning the code
    /// and how many bytes it consumed (`(0, 0)` only for an empty input).
    ///
    /// Implements the §9.7.6.2 length rule against the codespace ranges: the
    /// first range that fully contains the leading bytes fixes the code length.
    /// A leading byte that only *starts* some range but is then truncated or
    /// out of range still consumes that range's length (padding with zero,
    /// matching PDFium's `GetNextChar`), so decoding always makes progress.
    pub fn next_code(&self, s: &[u8]) -> (u32, usize) {
        if s.is_empty() {
            return (0, 0);
        }
        // First: a range every one of whose bytes matches (the common case).
        for r in &self.codespace {
            let n = r.n as usize;
            if s.len() >= n && (0..n).all(|i| r.lo[i] <= s[i] && s[i] <= r.hi[i]) {
                return (be(&s[..n]), n);
            }
        }
        // No full match: pick a range whose *first* byte matches and commit to
        // its length (a partially valid / truncated multibyte code).
        for r in &self.codespace {
            if r.lo[0] <= s[0] && s[0] <= r.hi[0] {
                let n = r.n as usize;
                let mut bytes = [0u8; 4];
                bytes[..n.min(s.len())].copy_from_slice(&s[..n.min(s.len())]);
                return (be(&bytes[..n]), n.min(s.len()).max(1));
            }
        }
        // A byte in no codespace at all: consume one, code = the byte.
        (s[0] as u32, 1)
    }

    /// Map a character code to a CID, following the `usecmap` chain on a miss.
    /// Unmapped codes are CID 0 (`.notdef`), as PDFium's `CIDFromCharCode`.
    pub fn cid(&self, code: u32) -> u32 {
        let mut cur = Some(self);
        while let Some(c) = cur {
            match &c.records {
                Records::Range(v) => {
                    if let Ok(i) = v.binary_search_by(|&(lo, hi, _)| {
                        if code < lo {
                            std::cmp::Ordering::Greater
                        } else if code > hi {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Equal
                        }
                    }) {
                        let (lo, _, cid) = v[i];
                        return cid + (code - lo);
                    }
                }
                Records::Single(v) => {
                    if let Ok(i) = v.binary_search_by(|&(c0, _)| c0.cmp(&code)) {
                        return v[i].1;
                    }
                }
            }
            cur = c.usecmap.as_deref();
        }
        0
    }
}

/// Big-endian integer of up to four bytes.
fn be(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |acc, &b| (acc << 8) | b as u32)
}

/// Synthesize codespace ranges from a predefined CMap's coding scheme and
/// leading-byte segments. PDFium models byte splitting with a coding scheme
/// (`cpdf_cmap.cpp`), not explicit codespace ranges; we translate that into the
/// §9.7.6.2 range model so one `next_code` serves predefined and embedded CMaps
/// alike.
fn synth_codespace(scheme: Scheme, lead: &[(u8, u8); 2]) -> Vec<Codespace> {
    let full = |n: u8| Codespace {
        n,
        lo: [0; 4],
        hi: [0xff; 4],
    };
    match scheme {
        Scheme::OneByte => vec![full(1)],
        Scheme::TwoBytes => vec![full(2)],
        Scheme::MixedTwoBytes => {
            // A leading byte starts a 2-byte code; every other byte is a 1-byte
            // code. Build non-overlapping-first-byte ranges so the length rule
            // is unambiguous.
            let mut leading = [false; 256];
            let mut ranges = Vec::new();
            for &(a, b) in lead {
                if a == 0 && b == 0 {
                    continue; // sentinel: unused segment
                }
                let (lo2, hi2) = (a.min(b), a.max(b));
                for x in lo2..=hi2 {
                    leading[x as usize] = true;
                }
                ranges.push(Codespace {
                    n: 2,
                    lo: [lo2, 0x00, 0, 0],
                    hi: [hi2, 0xff, 0, 0],
                });
            }
            // One-byte ranges over each maximal run of non-leading first bytes.
            let mut x = 0usize;
            while x < 256 {
                if leading[x] {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < 256 && !leading[x] {
                    x += 1;
                }
                ranges.push(Codespace {
                    n: 1,
                    lo: [start as u8, 0, 0, 0],
                    hi: [(x - 1) as u8, 0, 0, 0],
                });
            }
            ranges
        }
    }
}

/// A predefined CMap's static header (fields populated by `gen_cmaps.py`).
/// `off`/`records` index the per-charset blob (`off` in u16 units); `use_index`
/// is the entry this CMap `usecmap`s within the same charset (`-1` = none).
#[derive(Debug)]
pub(crate) struct RawCMap {
    pub name: &'static str,
    pub wmode: u8,
    pub scheme: Scheme,
    pub lead: [(u8, u8); 2],
    pub kind: Kind,
    pub off: u32,
    pub records: u32,
    pub use_index: i16,
}

/// Read one u16 (LE) at record-word offset `w` in a charset blob.
fn word(blob: &[u8], w: usize) -> u32 {
    u16::from_le_bytes([blob[w * 2], blob[w * 2 + 1]]) as u32
}

/// Build a [`CMap`] from a predefined entry, resolving its `usecmap` chain.
fn build_predefined(entries: &'static [RawCMap], blob: &'static [u8], idx: usize) -> Arc<CMap> {
    let e = &entries[idx];
    let base = e.off as usize;
    let records = match e.kind {
        Kind::Range => {
            let mut v = Vec::with_capacity(e.records as usize);
            for r in 0..e.records as usize {
                let o = base + r * 3;
                v.push((word(blob, o), word(blob, o + 1), word(blob, o + 2)));
            }
            Records::Range(v)
        }
        Kind::Single => {
            let mut v = Vec::with_capacity(e.records as usize);
            for r in 0..e.records as usize {
                let o = base + r * 2;
                v.push((word(blob, o), word(blob, o + 1)));
            }
            Records::Single(v)
        }
    };
    let usecmap = (e.use_index >= 0).then(|| build_predefined(entries, blob, e.use_index as usize));
    Arc::new(CMap {
        name: e.name.as_bytes().to_vec(),
        wmode: e.wmode,
        codespace: synth_codespace(e.scheme, &e.lead),
        records,
        usecmap,
    })
}

/// Look up a predefined CMap by name (e.g. `b"GBK-EUC-H"`, `b"UniGB-UCS2-H"`),
/// building it (and its `usecmap` base) from the transcribed PDFium tables.
/// `None` for an unknown name — the caller then falls back (Identity / charset).
pub fn predefined_cmap(name: &[u8]) -> Option<Arc<CMap>> {
    for &(entries, blob) in cmap_tables::CHARSETS {
        if let Some(i) = entries.iter().position(|e| e.name.as_bytes() == name) {
            return Some(build_predefined(entries, blob, i));
        }
    }
    None
}

// ---- Embedded CMap stream parser (ISO 32000-1 §9.7.5.3) --------------------

/// Parse an embedded CMap stream: the PostScript-ish
/// `begincodespacerange` / `begincidrange` / `begincidchar` / `usecmap`
/// syntax. Tolerant — a malformed entry is skipped, never fatal. `None` only if
/// nothing usable (no codespace and no records and no `usecmap`) was found.
pub fn parse_embedded_cmap(data: &[u8]) -> Option<CMap> {
    let mut lex = Lexer { s: data, i: 0 };
    let mut codespace: Vec<Codespace> = Vec::new();
    let mut ranges: Vec<(u32, u32, u32)> = Vec::new();
    let mut singles: Vec<(u32, u32)> = Vec::new();
    let mut wmode = 0u8;
    let mut usecmap: Option<Arc<CMap>> = None;
    let mut last_name: Option<Vec<u8>> = None;

    while let Some(tok) = lex.next() {
        match tok {
            Token::Name(n) => last_name = Some(n),
            Token::Keyword(kw) => match kw.as_slice() {
                b"usecmap" => {
                    if let Some(n) = last_name.take() {
                        usecmap = predefined_cmap(&n);
                    }
                }
                b"def" => {
                    // `/WMode <n> def`: the number precedes `def`; we captured it
                    // as `last_number` via the Number arm below.
                }
                b"begincodespacerange" => {
                    while let Some(t) = lex.next() {
                        let Token::Hex(lo) = t else { break }; // endcodespacerange
                        let Some(Token::Hex(hi)) = lex.next() else {
                            break;
                        };
                        if !lo.is_empty() && lo.len() == hi.len() && lo.len() <= 4 {
                            let mut l = [0u8; 4];
                            let mut h = [0xffu8; 4];
                            l[..lo.len()].copy_from_slice(&lo);
                            h[..hi.len()].copy_from_slice(&hi);
                            codespace.push(Codespace {
                                n: lo.len() as u8,
                                lo: l,
                                hi: h,
                            });
                        }
                    }
                }
                b"begincidrange" => {
                    // Triples: <lo> <hi> cid.
                    loop {
                        let Some(Token::Hex(lo)) = lex.next() else {
                            break;
                        };
                        let Some(Token::Hex(hi)) = lex.next() else {
                            break;
                        };
                        let Some(Token::Number(cid)) = lex.next() else {
                            break;
                        };
                        if lo.len() <= 4 && hi.len() <= 4 {
                            ranges.push((be(&lo), be(&hi), cid));
                        }
                    }
                }
                b"begincidchar" => {
                    // Pairs: <code> cid.
                    loop {
                        let Some(Token::Hex(code)) = lex.next() else {
                            break;
                        };
                        let Some(Token::Number(cid)) = lex.next() else {
                            break;
                        };
                        if code.len() <= 4 {
                            singles.push((be(&code), cid));
                        }
                    }
                }
                _ => {}
            },
            Token::Number(n) => {
                // `/WMode n def`. Consume the pending name so a later bare
                // count (e.g. `1 begincidrange`) is not mistaken for it.
                if last_name.as_deref() == Some(b"WMode".as_slice()) {
                    wmode = (n & 1) as u8;
                }
                last_name = None;
            }
            Token::Hex(_) => {}
        }
    }

    if codespace.is_empty() && ranges.is_empty() && singles.is_empty() && usecmap.is_none() {
        return None;
    }
    // If the stream declared no codespace of its own, borrow the base's so byte
    // splitting still works (common for a thin `usecmap` override CMap).
    if codespace.is_empty() {
        if let Some(base) = &usecmap {
            codespace = base.codespace.clone();
        } else {
            codespace.push(Codespace {
                n: 2,
                lo: [0; 4],
                hi: [0xff; 4],
            });
        }
    }
    ranges.sort_by_key(|r| r.0);
    singles.sort_by_key(|r| r.0);
    // Prefer ranges as the primary record set; fold singles into ranges as
    // degenerate `[c,c]` spans so one lookup structure serves both.
    for (c, cid) in singles {
        ranges.push((c, c, cid));
    }
    ranges.sort_by_key(|r| r.0);
    Some(CMap {
        name: b"(embedded)".to_vec(),
        wmode,
        codespace,
        records: Records::Range(ranges),
        usecmap,
    })
}

/// Minimal token stream for the embedded-CMap syntax.
enum Token {
    Hex(Vec<u8>),
    Number(u32),
    Name(Vec<u8>),
    Keyword(Vec<u8>),
}

struct Lexer<'a> {
    s: &'a [u8],
    i: usize,
}

impl Lexer<'_> {
    fn next(&mut self) -> Option<Token> {
        loop {
            while self.i < self.s.len() && is_ws(self.s[self.i]) {
                self.i += 1;
            }
            if self.i >= self.s.len() {
                return None;
            }
            let c = self.s[self.i];
            match c {
                b'%' => {
                    // Comment to end of line.
                    while self.i < self.s.len()
                        && self.s[self.i] != b'\n'
                        && self.s[self.i] != b'\r'
                    {
                        self.i += 1;
                    }
                }
                b'<' => {
                    self.i += 1;
                    let start = self.i;
                    while self.i < self.s.len() && self.s[self.i] != b'>' {
                        self.i += 1;
                    }
                    let hex = &self.s[start..self.i];
                    if self.i < self.s.len() {
                        self.i += 1; // skip '>'
                    }
                    return Some(Token::Hex(parse_hex(hex)));
                }
                b'/' => {
                    self.i += 1;
                    let start = self.i;
                    while self.i < self.s.len() && !is_delim(self.s[self.i]) {
                        self.i += 1;
                    }
                    return Some(Token::Name(self.s[start..self.i].to_vec()));
                }
                b'[' | b']' | b'{' | b'}' | b'(' | b')' => {
                    // Arrays/strings are not part of the records we read; skip
                    // the single delimiter and continue.
                    self.i += 1;
                }
                _ => {
                    let start = self.i;
                    while self.i < self.s.len() && !is_delim(self.s[self.i]) {
                        self.i += 1;
                    }
                    let word = &self.s[start..self.i];
                    if let Some(n) = parse_dec(word) {
                        return Some(Token::Number(n));
                    }
                    return Some(Token::Keyword(word.to_vec()));
                }
            }
        }
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0c | 0x00)
}
fn is_delim(b: u8) -> bool {
    is_ws(b)
        || matches!(
            b,
            b'<' | b'>' | b'/' | b'[' | b']' | b'{' | b'}' | b'(' | b')' | b'%'
        )
}

/// Decode a run of hex nibbles into bytes (odd trailing nibble padded low,
/// per PDF string rules); non-hex characters are ignored.
fn parse_hex(s: &[u8]) -> Vec<u8> {
    let mut nibbles: Vec<u8> = Vec::with_capacity(s.len());
    for &b in s {
        let v = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => continue,
        };
        nibbles.push(v);
    }
    if nibbles.len() % 2 == 1 {
        nibbles.push(0);
    }
    nibbles
        .chunks_exact(2)
        .map(|c| (c[0] << 4) | c[1])
        .collect()
}

fn parse_dec(s: &[u8]) -> Option<u32> {
    if s.is_empty() || !s.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut n: u32 = 0;
    for &b in s {
        n = n.saturating_mul(10).saturating_add((b - b'0') as u32);
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // Decode one code and its CID in one call.
    fn one(cm: &CMap, bytes: &[u8]) -> (u32, usize, u32) {
        let (code, n) = cm.next_code(bytes);
        (code, n, cm.cid(code))
    }

    #[test]
    fn gbk_euc_h_mixed_width_and_known_cids() {
        // Predefined GBK-EUC-H (GB1, MixedTwoBytes, leading 0x81..=0xFE).
        let cm = predefined_cmap(b"GBK-EUC-H").expect("GBK-EUC-H present");
        // Row 1 of kGBK_EUC_H_2: {0x0020, 0x0020, 0x1E24} — a 1-byte code.
        assert_eq!(one(&cm, &[0x20]), (0x20, 1, 0x1E24));
        // Row 2: {0x0021, 0x007E, 0x032E} — code 0x22 → 0x032E + (0x22-0x21).
        assert_eq!(one(&cm, &[0x22]), (0x22, 1, 0x032F));
        // Row 3: {0x8140, 0x8178, 0x2758} — a 2-byte code (0x81 is a lead byte).
        assert_eq!(one(&cm, &[0x81, 0x40]), (0x8140, 2, 0x2758));
        assert_eq!(one(&cm, &[0x81, 0x41]), (0x8141, 2, 0x2759));
        // A lead byte splits two bytes even mid-string; a non-lead splits one.
        assert_eq!(cm.next_code(&[0x81, 0x40, 0x20]), (0x8140, 2));
        assert_eq!(cm.next_code(&[0x20, 0x81, 0x40]), (0x20, 1));
    }

    #[test]
    fn unigb_ucs2_h_is_always_two_bytes() {
        // UniGB-UCS2-H (GB1, TwoBytes): every code is two bytes.
        let cm = predefined_cmap(b"UniGB-UCS2-H").expect("UniGB-UCS2-H present");
        assert_eq!(cm.wmode(), 0);
        // Row 1 of kUniGB_UCS2_H_4: {0x0020, 0x007E, 0x0001} — code 0x0020→CID 1.
        assert_eq!(one(&cm, &[0x00, 0x20]), (0x0020, 2, 0x0001));
        // Row 3: {0x00A5, 0x00A5, 0x5752}.
        assert_eq!(one(&cm, &[0x00, 0xA5]), (0x00A5, 2, 0x5752));
        // A byte < 0x20 is still consumed two at a time (TwoBytes coding).
        assert_eq!(cm.next_code(&[0x41, 0x42]).1, 2);
    }

    #[test]
    fn rksj_ksc_eten_known_cids() {
        // 90ms-RKSJ-H (Japan1): 1-byte {0x0020,0x007D,0x00E7}; 2-byte lead 0x81.
        let jp = predefined_cmap(b"90ms-RKSJ-H").unwrap();
        assert_eq!(one(&jp, &[0x20]), (0x20, 1, 0x00E7));
        assert_eq!(one(&jp, &[0x81, 0x40]), (0x8140, 2, 0x0279)); // {0x8140,0x817E,0x0279}
        // KSC-EUC-H (Korea1): {0xA1A1,0xA1FE,0x0065}.
        let ks = predefined_cmap(b"KSC-EUC-H").unwrap();
        assert_eq!(one(&ks, &[0xA1, 0xA1]), (0xA1A1, 2, 0x0065));
        assert_eq!(one(&ks, &[0xA1, 0xA2]), (0xA1A2, 2, 0x0066));
        // ETen-B5-H (CNS1): {0xA140,0xA158,0x0063}.
        let cns = predefined_cmap(b"ETen-B5-H").unwrap();
        assert_eq!(one(&cns, &[0xA1, 0x40]), (0xA140, 2, 0x0063));
    }

    #[test]
    fn vertical_cmap_chains_to_horizontal_via_usecmap() {
        // GB-EUC-V uses GB-EUC-H: a code the V table overrides resolves to the
        // vertical CID; one it does not falls through to the horizontal table.
        let v = predefined_cmap(b"GB-EUC-V").unwrap();
        assert_eq!(v.wmode(), 1, "V is vertical writing mode");
        // kGB_EUC_V_0 row 1: {0xA1A2,0xA1A2,0x023F} — vertical override.
        assert_eq!(v.cid(0xA1A2), 0x023F);
        // 0xA1A1 is absent from V → chains to GB-EUC-H {0xA1A1,0xA1FE,0x0060}.
        assert_eq!(v.cid(0xA1A1), 0x0060);
    }

    #[test]
    fn unknown_predefined_name_is_none() {
        assert!(predefined_cmap(b"Not-A-CMap").is_none());
        assert!(
            predefined_cmap(b"Identity-H").is_none(),
            "Identity is not a table"
        );
    }

    #[test]
    fn embedded_cmap_parse_codespace_ranges_and_chars() {
        // A hand-written embedded CMap: one-byte and two-byte codespaces, a
        // cidrange and a cidchar, honoring the §9.7.6.2 length rule.
        let src = br#"
            begincmap
            /WMode 0 def
            2 begincodespacerange
            <00> <80>
            <8100> <9FFF>
            endcodespacerange
            1 begincidrange
            <8140> <8143> 100
            endcidrange
            1 begincidchar
            <41> 7
            endcidchar
            endcmap
        "#;
        let cm = parse_embedded_cmap(src).expect("parses");
        assert_eq!(cm.wmode(), 0);
        // 0x41 is a 1-byte code (matches <00>-<80>) → cidchar 7.
        assert_eq!(one(&cm, &[0x41]), (0x41, 1, 7));
        // 0x8142 is a 2-byte code (matches <8100>-<9FFF>) → 100 + (0x8142-0x8140).
        assert_eq!(one(&cm, &[0x81, 0x42]), (0x8142, 2, 102));
        // A code in no cidrange/char → CID 0.
        assert_eq!(cm.cid(0x8150), 0);
    }

    #[test]
    fn embedded_usecmap_chains_to_predefined_base() {
        // `/GBK-EUC-H usecmap` then a small override: the override wins where it
        // maps; everything else falls through to the predefined base.
        let src = br#"
            begincmap
            /GBK-EUC-H usecmap
            1 begincidchar
            <8140> 9999
            endcidchar
            endcmap
        "#;
        let cm = parse_embedded_cmap(src).expect("parses");
        // Override: 0x8140 → 9999 (base would give 0x2758).
        assert_eq!(cm.cid(0x8140), 9999);
        // Fallthrough to GBK-EUC-H for a code the override omits.
        assert_eq!(cm.cid(0x8141), 0x2759);
        // Codespace borrowed from the base: 0x20 is a 1-byte code.
        assert_eq!(cm.next_code(&[0x20, 0x81]), (0x20, 1));
    }

    #[test]
    fn malformed_entries_are_skipped_not_fatal() {
        // Truncated hex, a missing cid, and stray tokens: parse must not panic
        // and must still pick up the well-formed record.
        let src = br#"
            begincmap
            1 begincodespacerange <00> <FF> endcodespacerange
            2 begincidchar
            <41> 5
            <42>
            endcidchar
            garbage 123 /Name
            endcmap
        "#;
        let cm = parse_embedded_cmap(src).expect("parses despite noise");
        assert_eq!(cm.cid(0x41), 5);
        // The malformed <42> entry (no cid) is dropped → CID 0.
        assert_eq!(cm.cid(0x42), 0);
    }

    #[test]
    fn empty_input_makes_no_progress() {
        let cm = predefined_cmap(b"GBK-EUC-H").unwrap();
        assert_eq!(cm.next_code(&[]), (0, 0));
    }
}
