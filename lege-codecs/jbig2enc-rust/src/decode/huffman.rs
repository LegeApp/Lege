//! Huffman table decoding (T.88 Annex B, §6.4/§6.5 Huffman paths).
//!
//! A Huffman table is a list of *table lines*, each assigning a prefix code to a
//! range of values (B.1). Codes are canonical Huffman codes assigned by prefix
//! length (B.3); decoding reads one bit at a time until a code matches, then
//! reads the line's range bits and adds (or, for the lower-range line,
//! subtracts) the offset (B.4).
//!
//! This module provides:
//! * [`BitReader`] — an MSB-first bit reader, distinct from the MQ decoder;
//! * [`HuffmanTable`] — a built table (lines + assigned codes);
//! * the 15 standard tables B.1–B.15 ([`standard_table`]); and
//! * custom-table parsing for segment type 53 ([`parse_custom_table`]).

use crate::decode::error::{DecodeError, LimitError};
use crate::shared::limits::DecodeLimits;

/// MSB-first bit reader over a byte slice. Bit 0 of the first byte is read
/// first (T.88 reads Huffman and MMR data most-significant-bit-first).
#[derive(Clone, Debug)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Absolute bit position from the start of `data`.
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, bit_pos: 0 }
    }

    /// Read a single bit. Reading past the end yields 0 bits (the spec pads the
    /// final byte; a well-formed stream never depends on beyond-end reads).
    #[inline]
    pub fn read_bit(&mut self) -> u32 {
        let byte_index = self.bit_pos >> 3;
        if byte_index >= self.data.len() {
            self.bit_pos += 1;
            return 0;
        }
        let bit = 7 - (self.bit_pos & 7);
        let v = (self.data[byte_index] >> bit) & 1;
        self.bit_pos += 1;
        v as u32
    }

    /// Read `n` bits (0..=32) MSB-first into a `u32`.
    #[inline]
    pub fn read_bits(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.read_bit();
        }
        v
    }

    /// Advance to the next byte boundary (§6.4/§7.4.3.1: some sub-streams are
    /// byte-aligned before a following block).
    #[inline]
    pub fn align_to_byte(&mut self) {
        self.bit_pos = (self.bit_pos + 7) & !7;
    }

    /// Current byte offset (rounded down). Used to hand off to a byte-aligned
    /// sub-stream (e.g. an MMR collective bitmap).
    #[inline]
    pub fn byte_position(&self) -> usize {
        self.bit_pos >> 3
    }

    /// Bits consumed so far.
    #[inline]
    pub fn bit_position(&self) -> usize {
        self.bit_pos
    }

    /// The remaining bytes from the current byte boundary onward. Call
    /// [`align_to_byte`] first if you need alignment.
    #[inline]
    pub fn remaining_from_byte(&self) -> &'a [u8] {
        let byte = self.bit_pos >> 3;
        if byte >= self.data.len() {
            &[]
        } else {
            &self.data[byte..]
        }
    }
}

/// Whether a table line adds or subtracts its offset, or signals OOB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineKind {
    /// `value = range_low + offset` (normal and upper-range lines).
    Normal,
    /// `value = range_low - offset` (the lower-range line).
    Lower,
    /// Out-of-band; no range bits follow.
    Oob,
}

/// One table line before code assignment.
#[derive(Clone, Copy, Debug)]
struct RawLine {
    pref_len: u8,
    range_len: u8,
    range_low: i64,
    kind: LineKind,
}

const fn n(pref_len: u8, range_len: u8, range_low: i64) -> RawLine {
    RawLine {
        pref_len,
        range_len,
        range_low,
        kind: LineKind::Normal,
    }
}
const fn lo(pref_len: u8, range_len: u8, range_low: i64) -> RawLine {
    RawLine {
        pref_len,
        range_len,
        range_low,
        kind: LineKind::Lower,
    }
}
const fn oob(pref_len: u8) -> RawLine {
    RawLine {
        pref_len,
        range_len: 0,
        range_low: 0,
        kind: LineKind::Oob,
    }
}

/// A built Huffman table: lines with assigned canonical prefix codes.
#[derive(Clone, Debug)]
pub struct HuffmanTable {
    lines: Vec<RawLine>,
    /// Assigned prefix code per line (only meaningful when `pref_len > 0`).
    codes: Vec<u32>,
    pub has_oob: bool,
}

/// The result of decoding one value from a Huffman table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HuffmanValue {
    Value(i32),
    Oob,
}

impl HuffmanTable {
    /// Build a table from its lines, assigning canonical prefix codes (B.3).
    fn build(lines: Vec<RawLine>) -> Result<Self, DecodeError> {
        // Longest prefix length present.
        let len_max = lines.iter().map(|l| l.pref_len).max().unwrap_or(0) as usize;
        if len_max > 32 {
            return Err(DecodeError::Malformed {
                reason: "Huffman prefix length exceeds 32 bits",
            });
        }
        let mut len_count = vec![0u32; len_max + 1];
        for l in &lines {
            len_count[l.pref_len as usize] += 1;
        }
        // B.3 step 2/3: canonical first-code per length.
        let mut codes = vec![0u32; lines.len()];
        if len_max >= 1 {
            let mut first_code = vec![0u32; len_max + 2];
            len_count[0] = 0;
            for cur_len in 1..=len_max {
                first_code[cur_len] = (first_code[cur_len - 1] + len_count[cur_len - 1]) << 1;
                let mut cur_code = first_code[cur_len];
                for (i, l) in lines.iter().enumerate() {
                    if l.pref_len as usize == cur_len {
                        codes[i] = cur_code;
                        cur_code += 1;
                    }
                }
            }
        }
        let has_oob = lines.iter().any(|l| l.kind == LineKind::Oob);
        Ok(HuffmanTable {
            lines,
            codes,
            has_oob,
        })
    }

    /// Build a "pure prefix" table from a list of code lengths, where decoding
    /// yields the *index* of the matched length (T.88 §7.4.3.1.7: the RUNCODE
    /// table and the SBSYMCODES symbol-ID table). A length of 0 means the value
    /// is unused. Each line carries no range bits.
    pub fn from_code_lengths(lengths: &[u32]) -> Result<Self, DecodeError> {
        let mut lines = Vec::with_capacity(lengths.len());
        for (i, &len) in lengths.iter().enumerate() {
            if len > 32 {
                return Err(DecodeError::Malformed {
                    reason: "symbol-ID code length exceeds 32 bits",
                });
            }
            lines.push(RawLine {
                pref_len: len as u8,
                range_len: 0,
                range_low: i as i64,
                kind: LineKind::Normal,
            });
        }
        HuffmanTable::build(lines)
    }

    /// Decode one value (B.4). Returns [`HuffmanValue::Oob`] for the OOB line.
    pub fn decode(&self, r: &mut BitReader<'_>) -> Result<HuffmanValue, DecodeError> {
        // Accumulate bits until (length, code) matches a line. Bounded by the
        // longest prefix present (build() capped it at 32).
        let mut code = 0u32;
        let mut len = 0u32;
        loop {
            code = (code << 1) | r.read_bit();
            len += 1;
            if len > 32 {
                return Err(DecodeError::Malformed {
                    reason: "no Huffman code matched within 32 bits",
                });
            }
            for (i, l) in self.lines.iter().enumerate() {
                if l.pref_len as u32 == len && self.codes[i] == code {
                    return self.emit(l, r);
                }
            }
        }
    }

    /// Test/encoder helper: the bits to emit for `value` — `(prefix_code,
    /// prefix_len, offset, range_len)`. Returns `None` if no line covers it.
    #[allow(dead_code)]
    pub fn encode_value(&self, value: i32) -> Option<(u32, u8, u64, u8)> {
        let v = value as i64;
        // Bounded normal lines first (disjoint ascending ranges below HTHIGH).
        for (i, l) in self.lines.iter().enumerate() {
            if l.pref_len == 0 || l.kind != LineKind::Normal || l.range_len >= 32 {
                continue;
            }
            let lo = l.range_low;
            let hi = lo + (1i64 << l.range_len) - 1;
            if v >= lo && v <= hi {
                return Some((self.codes[i], l.pref_len, (v - lo) as u64, l.range_len));
            }
        }
        // Upper range line (Normal, 32-bit): covers value >= range_low.
        for (i, l) in self.lines.iter().enumerate() {
            if l.pref_len == 0 || l.kind != LineKind::Normal || l.range_len < 32 {
                continue;
            }
            if v >= l.range_low {
                return Some((self.codes[i], l.pref_len, (v - l.range_low) as u64, 32));
            }
        }
        // Lower range line: covers value <= range_low.
        for (i, l) in self.lines.iter().enumerate() {
            if l.pref_len == 0 || l.kind != LineKind::Lower {
                continue;
            }
            if v <= l.range_low {
                return Some((
                    self.codes[i],
                    l.pref_len,
                    (l.range_low - v) as u64,
                    l.range_len,
                ));
            }
        }
        None
    }

    /// Test/encoder helper: the OOB prefix `(code, len)`, if this table has one.
    #[allow(dead_code)]
    pub fn encode_oob(&self) -> Option<(u32, u8)> {
        for (i, l) in self.lines.iter().enumerate() {
            if l.kind == LineKind::Oob && l.pref_len > 0 {
                return Some((self.codes[i], l.pref_len));
            }
        }
        None
    }

    fn emit(&self, l: &RawLine, r: &mut BitReader<'_>) -> Result<HuffmanValue, DecodeError> {
        match l.kind {
            LineKind::Oob => Ok(HuffmanValue::Oob),
            LineKind::Normal => {
                let offset = read_offset(r, l.range_len);
                let v = l.range_low + offset;
                to_i32(v)
            }
            LineKind::Lower => {
                let offset = read_offset(r, l.range_len);
                let v = l.range_low - offset;
                to_i32(v)
            }
        }
    }
}

#[inline]
fn read_offset(r: &mut BitReader<'_>, range_len: u8) -> i64 {
    if range_len == 0 {
        0
    } else {
        r.read_bits(range_len as u32) as i64
    }
}

#[inline]
fn to_i32(v: i64) -> Result<HuffmanValue, DecodeError> {
    i32::try_from(v)
        .map(HuffmanValue::Value)
        .map_err(|_| DecodeError::Overflow {
            operation: "Huffman value out of i32 range",
        })
}

/// The 15 standard Huffman tables (T.88 Annex B.5). `n` is 1..=15 for B.1..B.15.
pub fn standard_table(index: u8) -> Result<HuffmanTable, DecodeError> {
    let lines: Vec<RawLine> = match index {
        1 => vec![n(1, 4, 0), n(2, 8, 16), n(3, 16, 272), n(3, 32, 65808)],
        2 => vec![
            n(1, 0, 0),
            n(2, 0, 1),
            n(3, 0, 2),
            n(4, 3, 3),
            n(5, 6, 11),
            n(6, 32, 75),
            oob(6),
        ],
        3 => vec![
            n(8, 8, -256),
            n(1, 0, 0),
            n(2, 0, 1),
            n(3, 0, 2),
            n(4, 3, 3),
            n(5, 6, 11),
            lo(8, 32, -257),
            n(7, 32, 75),
            oob(6),
        ],
        4 => vec![
            n(1, 0, 1),
            n(2, 0, 2),
            n(3, 0, 3),
            n(4, 3, 4),
            n(5, 6, 12),
            n(5, 32, 76),
        ],
        5 => vec![
            n(7, 8, -255),
            n(1, 0, 1),
            n(2, 0, 2),
            n(3, 0, 3),
            n(4, 3, 4),
            n(5, 6, 12),
            lo(7, 32, -256),
            n(6, 32, 76),
        ],
        6 => vec![
            n(5, 10, -2048),
            n(4, 9, -1024),
            n(4, 8, -512),
            n(4, 7, -256),
            n(5, 6, -128),
            n(5, 5, -64),
            n(4, 5, -32),
            n(2, 7, 0),
            n(3, 7, 128),
            n(3, 8, 256),
            n(4, 9, 512),
            n(4, 10, 1024),
            lo(6, 32, -2049),
            n(6, 32, 2048),
        ],
        7 => vec![
            n(4, 9, -1024),
            n(3, 8, -512),
            n(4, 7, -256),
            n(5, 6, -128),
            n(5, 5, -64),
            n(4, 5, -32),
            n(4, 5, 0),
            n(5, 5, 32),
            n(5, 6, 64),
            n(4, 7, 128),
            n(3, 8, 256),
            n(3, 9, 512),
            n(3, 10, 1024),
            lo(5, 32, -1025),
            n(5, 32, 2048),
        ],
        8 => vec![
            n(8, 3, -15),
            n(9, 1, -7),
            n(8, 1, -5),
            n(9, 0, -3),
            n(7, 0, -2),
            n(4, 0, -1),
            n(2, 1, 0),
            n(5, 0, 2),
            n(6, 0, 3),
            n(3, 4, 4),
            n(6, 1, 20),
            n(4, 4, 22),
            n(4, 5, 38),
            n(5, 6, 70),
            n(5, 7, 134),
            n(6, 7, 262),
            n(7, 8, 390),
            n(6, 10, 646),
            lo(9, 32, -16),
            n(9, 32, 1670),
            oob(2),
        ],
        9 => vec![
            n(8, 4, -31),
            n(9, 2, -15),
            n(8, 2, -11),
            n(9, 1, -7),
            n(7, 1, -5),
            n(4, 1, -3),
            n(3, 1, -1),
            n(3, 1, 1),
            n(5, 1, 3),
            n(6, 1, 5),
            n(3, 5, 7),
            n(6, 2, 39),
            n(4, 5, 43),
            n(4, 6, 75),
            n(5, 7, 139),
            n(5, 8, 267),
            n(6, 8, 523),
            n(7, 9, 779),
            n(6, 11, 1291),
            lo(9, 32, -32),
            n(9, 32, 3339),
            oob(2),
        ],
        10 => vec![
            n(7, 4, -21),
            n(8, 0, -5),
            n(7, 0, -4),
            n(5, 0, -3),
            n(2, 2, -2),
            n(5, 0, 2),
            n(6, 0, 3),
            n(7, 0, 4),
            n(8, 0, 5),
            n(2, 6, 6),
            n(5, 5, 70),
            n(6, 5, 102),
            n(6, 6, 134),
            n(6, 7, 198),
            n(6, 8, 326),
            n(6, 9, 582),
            n(6, 10, 1094),
            n(7, 11, 2118),
            lo(8, 32, -22),
            n(8, 32, 4166),
            oob(2),
        ],
        11 => vec![
            n(1, 0, 1),
            n(2, 1, 2),
            n(4, 0, 4),
            n(4, 1, 5),
            n(5, 1, 7),
            n(5, 2, 9),
            n(6, 2, 13),
            n(7, 2, 17),
            n(7, 3, 21),
            n(7, 4, 29),
            n(7, 5, 45),
            n(7, 6, 77),
            n(7, 32, 141),
        ],
        12 => vec![
            n(1, 0, 1),
            n(2, 0, 2),
            n(3, 1, 3),
            n(5, 0, 5),
            n(5, 1, 6),
            n(6, 1, 8),
            n(7, 0, 10),
            n(7, 1, 11),
            n(7, 2, 13),
            n(7, 3, 17),
            n(7, 4, 25),
            n(8, 5, 41),
            n(8, 32, 73),
        ],
        13 => vec![
            n(1, 0, 1),
            n(3, 0, 2),
            n(4, 0, 3),
            n(5, 0, 4),
            n(4, 1, 5),
            n(3, 3, 7),
            n(6, 1, 15),
            n(6, 2, 17),
            n(6, 3, 21),
            n(6, 4, 29),
            n(6, 5, 45),
            n(7, 6, 77),
            n(7, 32, 141),
        ],
        14 => vec![n(3, 0, -2), n(3, 0, -1), n(1, 0, 0), n(3, 0, 1), n(3, 0, 2)],
        15 => vec![
            n(7, 4, -24),
            n(6, 2, -8),
            n(5, 1, -4),
            n(4, 0, -2),
            n(3, 0, -1),
            n(1, 0, 0),
            n(3, 0, 1),
            n(4, 0, 2),
            n(5, 1, 3),
            n(6, 2, 5),
            n(7, 4, 9),
            lo(7, 32, -25),
            n(7, 32, 25),
        ],
        _ => {
            return Err(DecodeError::Malformed {
                reason: "invalid standard Huffman table index",
            });
        }
    };
    HuffmanTable::build(lines)
}

/// Parse a custom Huffman table from a segment-type-53 payload (T.88 §7.4.13 /
/// B.2). The payload is: flags byte, 4-byte HTLOW, 4-byte HTHIGH, then the
/// table lines as HTPS/HTRS-bit fields.
pub fn parse_custom_table(
    payload: &[u8],
    limits: &DecodeLimits,
) -> Result<HuffmanTable, DecodeError> {
    let mut r = BitReader::new(payload);
    // B.2.1 code table flags.
    let flags = r.read_bits(8);
    let htoob = flags & 0x01 != 0;
    let htps = ((flags >> 1) & 0x07) + 1; // prefix-size field width
    let htrs = ((flags >> 4) & 0x07) + 1; // range-size field width

    // B.2.2/B.2.3 low and high, signed 32-bit.
    let htlow = r.read_bits(32) as i32 as i64;
    let hthigh = r.read_bits(32) as i32 as i64;
    if hthigh <= htlow {
        return Err(DecodeError::Malformed {
            reason: "custom Huffman table HTHIGH <= HTLOW",
        });
    }

    let mut lines: Vec<RawLine> = Vec::new();
    let mut cur_low = htlow;
    // B.2 step 5: normal table lines until CURRANGELOW >= HTHIGH.
    while cur_low < hthigh {
        if lines.len() >= limits.max_huffman_table_entries {
            return Err(DecodeError::limit(LimitError::Count {
                what: "Huffman table lines",
                value: lines.len() as u64,
                limit: limits.max_huffman_table_entries as u64,
            }));
        }
        let pref_len = r.read_bits(htps) as u8;
        let range_len = r.read_bits(htrs) as u8;
        if range_len > 32 {
            return Err(DecodeError::Malformed {
                reason: "custom Huffman range length exceeds 32",
            });
        }
        lines.push(n(pref_len, range_len, cur_low));
        cur_low = cur_low.saturating_add(1i64 << range_len.min(62));
    }

    // B.2 step 6/7: lower range table line.
    let low_pref = r.read_bits(htps) as u8;
    lines.push(lo(low_pref, 32, htlow - 1));
    // B.2 step 8/9: upper range table line.
    let high_pref = r.read_bits(htps) as u8;
    lines.push(n(high_pref, 32, hthigh));
    // B.2 step 10: optional OOB line.
    if htoob {
        let oob_pref = r.read_bits(htps) as u8;
        lines.push(oob(oob_pref));
    }

    HuffmanTable::build(lines)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn limits() -> DecodeLimits {
        DecodeLimits::default()
    }

    #[test]
    fn table_b1_codes_match_spec_example() {
        // B.5 example: PREFLEN [1,2,3,3], codes 0, 10, 110, 111.
        let t = standard_table(1).unwrap();
        assert_eq!(t.codes[0], 0b0);
        assert_eq!(t.codes[1], 0b10);
        assert_eq!(t.codes[2], 0b110);
        assert_eq!(t.codes[3], 0b111);
        assert!(!t.has_oob);
    }

    /// Encode a value with a known table line by hand and decode it back.
    fn bits_to_bytes(bits: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; bits.len().div_ceil(8)];
        for (i, &b) in bits.iter().enumerate() {
            if b != 0 {
                out[i >> 3] |= 1 << (7 - (i & 7));
            }
        }
        out
    }

    #[test]
    fn table_b1_decode_small_value() {
        // Value 5 (range 0..15): prefix "0" then 4-bit 0101.
        let bytes = bits_to_bytes(&[0, 0, 1, 0, 1]);
        let t = standard_table(1).unwrap();
        let mut r = BitReader::new(&bytes);
        assert_eq!(t.decode(&mut r).unwrap(), HuffmanValue::Value(5));
    }

    #[test]
    fn table_b1_decode_mid_value() {
        // Value 20 (range 16..271): prefix "10" then 8-bit (20-16)=4 = 00000100.
        let bytes = bits_to_bytes(&[1, 0, 0, 0, 0, 0, 0, 1, 0, 0]);
        let t = standard_table(1).unwrap();
        let mut r = BitReader::new(&bytes);
        assert_eq!(t.decode(&mut r).unwrap(), HuffmanValue::Value(20));
    }

    #[test]
    fn table_b2_oob() {
        // B.2 OOB code is "111111" (prefix length 6).
        let bytes = bits_to_bytes(&[1, 1, 1, 1, 1, 1]);
        let t = standard_table(2).unwrap();
        assert!(t.has_oob);
        let mut r = BitReader::new(&bytes);
        assert_eq!(t.decode(&mut r).unwrap(), HuffmanValue::Oob);
    }

    #[test]
    fn all_standard_tables_build() {
        for i in 1u8..=15 {
            let t = standard_table(i).unwrap();
            // Codes must be prefix-free: no code is a prefix of another.
            for (a, la) in t.lines.iter().enumerate() {
                if la.pref_len == 0 {
                    continue;
                }
                for (b, lb) in t.lines.iter().enumerate() {
                    if a == b || lb.pref_len == 0 || la.pref_len > lb.pref_len {
                        continue;
                    }
                    // Is code[a] a prefix of code[b]?
                    let shift = lb.pref_len - la.pref_len;
                    if t.codes[b] >> shift == t.codes[a] && la.pref_len <= lb.pref_len {
                        assert!(
                            !(la.pref_len < lb.pref_len || a == b),
                            "table B.{i}: code {a} is a prefix of {b}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn table_b14_symmetric() {
        // B.14 codes: -2..2 with the value 0 having the 1-bit code "0".
        let t = standard_table(14).unwrap();
        // value 0 is line index 2, prefix length 1.
        assert_eq!(t.lines[2].pref_len, 1);
        assert_eq!(t.codes[2], 0);
        let bytes = bits_to_bytes(&[0]);
        let mut r = BitReader::new(&bytes);
        assert_eq!(t.decode(&mut r).unwrap(), HuffmanValue::Value(0));
    }

    #[test]
    fn custom_table_roundtrip_via_b1_encoding() {
        // The B.5 example byte encoding of Table B.1: flags 0x42, low 0,
        // high 0x00010110, then table lines 0x49 0x23 0x81 0x80.
        let payload = [
            0x42, // flags: HTOOB=0 HTPS=2 HTRS=5
            0x00, 0x00, 0x00, 0x00, // HTLOW = 0
            0x00, 0x01, 0x01, 0x10, // HTHIGH = 65808
            0x49, 0x23, 0x81, 0x80, // three lines + lower(unused) + upper
        ];
        let t = parse_custom_table(&payload, &limits()).unwrap();
        // Decode value 5 (prefix "0" + 4-bit 0101) as with the standard table.
        let bytes = bits_to_bytes(&[0, 0, 1, 0, 1]);
        let mut r = BitReader::new(&bytes);
        assert_eq!(t.decode(&mut r).unwrap(), HuffmanValue::Value(5));
    }
}
