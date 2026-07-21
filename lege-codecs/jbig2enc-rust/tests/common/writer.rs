//! Test-only JBIG2 stream writer (Phase 5, jbig2dec-phase5-plan.md §5a/§5b).
//!
//! This crate's *encoder* only ever emits generic-region template 0 with no
//! TPGDON, so a self-round-trip cannot cover templates 1–3, TPGDON, or other
//! "wild" forms found in real PDFs. This helper builds minimal valid page
//! streams for those forms by driving the encoder's MQ arithmetic coder
//! (`Jbig2ArithCoder::encode_bit`) directly, with context layouts that mirror
//! `src/decode/generic.rs`.
//!
//! Because the MQ context *numbering* is a pure relabelling of the same pixel
//! partition, a stream produced with the decoder's layout is byte-identical to
//! one produced with jbig2dec's canonical layout — so the same bytes decode
//! correctly under both. The jbig2dec oracle is what actually validates spec
//! conformance; self-round-trip only checks internal consistency.

#![allow(dead_code)]

use jbig2enc_rust::decode::huffman::HuffmanTable;
use jbig2enc_rust::jbig2sym::BitImage;
use jbig2enc_rust::shared::int_proc::IntProc;
use jbig2enc_rust::Jbig2ArithCoder;

/// A simple mutable test bitmap (row-major bool grid, `false` = white).
#[derive(Clone)]
pub struct TestBitmap {
    pub width: u32,
    pub height: u32,
    px: Vec<bool>,
}

impl TestBitmap {
    pub fn new(width: u32, height: u32) -> Self {
        TestBitmap {
            width,
            height,
            px: vec![false; (width as usize) * (height as usize)],
        }
    }

    #[inline]
    pub fn set(&mut self, x: u32, y: u32, v: bool) {
        if x < self.width && y < self.height {
            self.px[(y as usize) * (self.width as usize) + x as usize] = v;
        }
    }

    #[inline]
    pub fn get(&self, x: u32, y: u32) -> bool {
        if x < self.width && y < self.height {
            self.px[(y as usize) * (self.width as usize) + x as usize]
        } else {
            false
        }
    }

    /// Causal sample at `(x+dx, y+dy)` as a `u32` bit, matching the decoder's
    /// convention: previous rows are read directly; the current row is only
    /// visible for `x' < x`; anything else (future / far rows) reads as 0.
    #[inline]
    fn causal(&self, x: i64, y: i64, dx: i64, dy: i64) -> u32 {
        let xx = x + dx;
        let yy = y + dy;
        if xx < 0 || xx >= self.width as i64 || yy < 0 || dy > 0 {
            return 0;
        }
        if dy == 0 {
            // Current row: only already-encoded columns (x' < x) are visible.
            if xx >= x {
                0
            } else {
                self.get(xx as u32, yy as u32) as u32
            }
        } else {
            // Any earlier row — AT pixels may reference further than y-2.
            self.get(xx as u32, yy as u32) as u32
        }
    }

    fn row_equals_prev(&self, y: u32) -> bool {
        // Compare row y to row y-1 (all-zero above row 0), matching the TPGDON
        // duplicate-row test the decoder applies.
        for x in 0..self.width {
            let above = if y >= 1 { self.get(x, y - 1) } else { false };
            if self.get(x, y) != above {
                return false;
            }
        }
        true
    }
}

/// SLTP contexts, mirroring `src/decode/generic.rs`.
const SLTP_CTX_T0: usize = 0x9B25;
const SLTP_CTX_T1: usize = 0x0795;
const SLTP_CTX_T2: usize = 0x00E5;
const SLTP_CTX_T3: usize = 0x0195;

/// Compute the generic-region context index for the pixel at `(x, y)` under the
/// given template, mirroring the decoder's per-template bit layout exactly.
fn context(template: u8, bm: &TestBitmap, x: i64, y: i64, at: &[(i8, i8); 4]) -> usize {
    let (a1x, a1y) = (at[0].0 as i64, at[0].1 as i64);
    let (a2x, a2y) = (at[1].0 as i64, at[1].1 as i64);
    let (a3x, a3y) = (at[2].0 as i64, at[2].1 as i64);
    let (a4x, a4y) = (at[3].0 as i64, at[3].1 as i64);
    let p = |dx: i64, dy: i64| bm.causal(x, y, dx, dy);
    let t = match template {
        0 => {
            (p(a4x, a4y) << 15)
                | (p(-1, -2) << 14)
                | (p(0, -2) << 13)
                | (p(1, -2) << 12)
                | (p(a3x, a3y) << 11)
                | (p(a2x, a2y) << 10)
                | (p(-2, -1) << 9)
                | (p(-1, -1) << 8)
                | (p(0, -1) << 7)
                | (p(1, -1) << 6)
                | (p(2, -1) << 5)
                | (p(a1x, a1y) << 4)
                | (p(-4, 0) << 3)
                | (p(-3, 0) << 2)
                | (p(-2, 0) << 1)
                | p(-1, 0)
        }
        1 => {
            p(-1, 0)
                | (p(-2, 0) << 1)
                | (p(-3, 0) << 2)
                | (p(a1x, a1y) << 3)
                | (p(2, -1) << 4)
                | (p(1, -1) << 5)
                | (p(0, -1) << 6)
                | (p(-1, -1) << 7)
                | (p(-2, -1) << 8)
                | (p(2, -2) << 9)
                | (p(1, -2) << 10)
                | (p(0, -2) << 11)
                | (p(-1, -2) << 12)
        }
        2 => {
            p(-1, 0)
                | (p(-2, 0) << 1)
                | (p(a1x, a1y) << 2)
                | (p(1, -1) << 3)
                | (p(0, -1) << 4)
                | (p(-1, -1) << 5)
                | (p(-2, -1) << 6)
                | (p(1, -2) << 7)
                | (p(0, -2) << 8)
                | (p(-1, -2) << 9)
        }
        _ => {
            p(-1, 0)
                | (p(-2, 0) << 1)
                | (p(-3, 0) << 2)
                | (p(-4, 0) << 3)
                | (p(a1x, a1y) << 4)
                | (p(1, -1) << 5)
                | (p(0, -1) << 6)
                | (p(-1, -1) << 7)
                | (p(-2, -1) << 8)
                | (p(-3, -1) << 9)
        }
    };
    t as usize
}

fn sltp_ctx(template: u8) -> usize {
    match template {
        0 => SLTP_CTX_T0,
        1 => SLTP_CTX_T1,
        2 => SLTP_CTX_T2,
        _ => SLTP_CTX_T3,
    }
}

/// Nominal AT for a template, expanded to 4 slots (only the leading slots are
/// used by templates 1–3).
pub fn nominal_at(template: u8) -> [(i8, i8); 4] {
    match template {
        0 => [(3, -1), (-3, -1), (2, -2), (-2, -2)],
        1 => [(3, -1), (0, 0), (0, 0), (0, 0)],
        _ => [(2, -1), (0, 0), (0, 0), (0, 0)],
    }
}

/// Encode the arithmetic data for a generic region: the pixels of `bm` under
/// `template`/`at`, optionally with TPGDON typical prediction. Returns the raw
/// MQ byte stream (terminated with the standard marker).
pub fn generic_arith_data(
    bm: &TestBitmap,
    template: u8,
    at: &[(i8, i8); 4],
    tpgdon: bool,
) -> Vec<u8> {
    let mut coder = Jbig2ArithCoder::new();
    let mut ltp = false;
    for y in 0..bm.height {
        if tpgdon {
            let dup = bm.row_equals_prev(y);
            let sltp = dup ^ ltp;
            coder.encode_bit(sltp_ctx(template), sltp);
            ltp = dup;
            if dup {
                continue;
            }
        }
        for x in 0..bm.width {
            let ctx = context(template, bm, x as i64, y as i64, at);
            coder.encode_bit(ctx, bm.get(x, y));
        }
    }
    // flush(true) finalizes and appends the FF AC terminator; read the buffer
    // directly rather than via into_vec (which would flush a second time and
    // leave bytes after the terminator — harmless for known-length regions but
    // fatal to the §7.2.7 terminator scan).
    coder.flush(true);
    coder.as_bytes().to_vec()
}

/// Build a generic-region segment payload (T.88 §7.4.6): region info + generic
/// flags + AT pixels + arithmetic data.
pub fn generic_region_payload(
    bm: &TestBitmap,
    template: u8,
    at: &[(i8, i8); 4],
    tpgdon: bool,
    comb_operator: u8,
) -> Vec<u8> {
    let mut v = Vec::new();
    // §7.4.1 region segment information field.
    v.extend_from_slice(&bm.width.to_be_bytes());
    v.extend_from_slice(&bm.height.to_be_bytes());
    v.extend_from_slice(&0u32.to_be_bytes()); // x
    v.extend_from_slice(&0u32.to_be_bytes()); // y
    v.push(comb_operator & 0x07);
    // §7.4.6.2 generic region flags: bit0 MMR, bits1-2 template, bit3 TPGDON.
    let flags = ((template & 0x03) << 1) | ((tpgdon as u8) << 3);
    v.push(flags);
    // AT pixels: template 0 has 4, templates 1–3 have 1.
    let at_count = if template == 0 { 4 } else { 1 };
    for &(ax, ay) in at.iter().take(at_count) {
        v.push(ax as u8);
        v.push(ay as u8);
    }
    v.extend_from_slice(&generic_arith_data(bm, template, at, tpgdon));
    v
}

/// Build a page-information segment payload (T.88 §7.4.8), 19 bytes.
pub fn page_info_payload(width: u32, height: u32) -> Vec<u8> {
    page_info_payload_striped(width, height, 0)
}

/// Page-information payload with an explicit striping field. `striping` bit 15
/// set marks the page as striped; the low 15 bits are the maximum stripe size.
pub fn page_info_payload_striped(width: u32, height: u32, striping: u16) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&width.to_be_bytes());
    v.extend_from_slice(&height.to_be_bytes());
    v.extend_from_slice(&0u32.to_be_bytes()); // x resolution
    v.extend_from_slice(&0u32.to_be_bytes()); // y resolution
    v.push(0x00); // page flags: lossy, default pixel 0, OR combination
    v.extend_from_slice(&striping.to_be_bytes());
    v
}

/// Like [`generic_region_payload`] but with an explicit region origin `(x, y)`.
pub fn generic_region_payload_at(
    bm: &TestBitmap,
    template: u8,
    at: &[(i8, i8); 4],
    tpgdon: bool,
    comb_operator: u8,
    x: u32,
    y: u32,
) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&bm.width.to_be_bytes());
    v.extend_from_slice(&bm.height.to_be_bytes());
    v.extend_from_slice(&x.to_be_bytes());
    v.extend_from_slice(&y.to_be_bytes());
    v.push(comb_operator & 0x07);
    let flags = ((template & 0x03) << 1) | ((tpgdon as u8) << 3);
    v.push(flags);
    let at_count = if template == 0 { 4 } else { 1 };
    for &(ax, ay) in at.iter().take(at_count) {
        v.push(ax as u8);
        v.push(ay as u8);
    }
    v.extend_from_slice(&generic_arith_data(bm, template, at, tpgdon));
    v
}

/// A page whose single immediate generic region has *unknown* segment length
/// (T.88 §7.2.7): the region's arithmetic data ends with the `FF AC` marker
/// (emitted by the encoder's flush) followed by a 4-byte row count, and the
/// segment header's data-length field is the 0xFFFFFFFF sentinel.
pub fn unknown_length_generic_page(bm: &TestBitmap) -> Vec<u8> {
    let mut stream = Vec::new();
    let page_data = page_info_payload(bm.width, bm.height);
    stream.extend_from_slice(&segment_header(0, 48, &[], 1, page_data.len() as u32));
    stream.extend_from_slice(&page_data);

    let mut region = generic_region_payload(bm, 0, &nominal_at(0), false, 0);
    // The arithmetic data already ends with FF AC; append the 4-byte row count.
    region.extend_from_slice(&bm.height.to_be_bytes());
    stream.extend_from_slice(&segment_header(1, 38, &[], 1, 0xFFFF_FFFF));
    stream.extend_from_slice(&region);

    stream.extend_from_slice(&segment_header(2, 49, &[], 1, 0));
    stream
}

/// A striped page of *unknown* height (T.88 §7.4.8.5): page height 0xFFFFFFFF
/// with the striped bit set, `bands` stacked vertically each as its own generic
/// region followed by an end-of-stripe segment, then end-of-page.
pub fn striped_unknown_height_page(width: u32, bands: &[TestBitmap]) -> Vec<u8> {
    let mut stream = Vec::new();
    // Striping info: bit15 set. jbig2dec 0.20 pads an unknown-height page out to
    // the maximum stripe size rather than trimming to the last end-of-stripe row
    // (the native decoder follows §7.4.9 and trims); set the max stripe size to
    // the true total height so both agree on the final page size.
    let total_h: u32 = bands.iter().map(|b| b.height).sum();
    let max_stripe = (total_h & 0x7FFF) as u16;
    let page_data = page_info_payload_striped(width, 0xFFFF_FFFF, 0x8000 | max_stripe);
    stream.extend_from_slice(&segment_header(0, 48, &[], 1, page_data.len() as u32));
    stream.extend_from_slice(&page_data);

    let mut seg = 1u32;
    let mut y = 0u32;
    for band in bands {
        let region = generic_region_payload_at(band, 0, &nominal_at(0), false, 0, 0, y);
        stream.extend_from_slice(&segment_header(seg, 38, &[], 1, region.len() as u32));
        stream.extend_from_slice(&region);
        seg += 1;
        // End of stripe: end row = y + band height - 1.
        let end_row = y + band.height - 1;
        stream.extend_from_slice(&segment_header(seg, 50, &[], 1, 4));
        stream.extend_from_slice(&end_row.to_be_bytes());
        seg += 1;
        y += band.height;
    }
    stream.extend_from_slice(&segment_header(seg, 49, &[], 1, 0));
    stream
}

/// Emit a short-form segment header (T.88 §7.2) followed by nothing — the
/// caller appends the data. `referred` are 1-byte referred numbers (segment
/// numbers must be <= 256 for that to be valid, which all our test streams are).
fn segment_header(number: u32, type_code: u8, referred: &[u32], page: u8, data_len: u32) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&number.to_be_bytes());
    v.push(type_code & 0x3F);
    v.push((referred.len() as u8) << 5);
    for &r in referred {
        v.push(r as u8);
    }
    v.push(page);
    v.extend_from_slice(&data_len.to_be_bytes());
    v
}

/// A minimal embedded page stream (no file header) that jbig2dec `-e` and the
/// native `decode_embedded` both accept: page-info segment, one immediate
/// generic region, end-of-page.
pub fn single_generic_page(bm: &TestBitmap, template: u8, at: &[(i8, i8); 4], tpgdon: bool) -> Vec<u8> {
    let mut stream = Vec::new();

    // Segment 0: page information (type 48).
    let page_data = page_info_payload(bm.width, bm.height);
    stream.extend_from_slice(&segment_header(0, 48, &[], 1, page_data.len() as u32));
    stream.extend_from_slice(&page_data);

    // Segment 1: immediate generic region (type 38).
    let region = generic_region_payload(bm, template, at, tpgdon, 0);
    stream.extend_from_slice(&segment_header(1, 38, &[], 1, region.len() as u32));
    stream.extend_from_slice(&region);

    // Segment 2: end of page (type 49), no data.
    stream.extend_from_slice(&segment_header(2, 49, &[], 1, 0));

    stream
}

/// Wrap an embedded page stream in a standalone sequential file (T.88 Annex D):
/// file magic + flags (sequential, unknown page count) + segments.
pub fn standalone_file(embedded: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&[0x97, 0x4A, 0x42, 0x32, 0x0D, 0x0A, 0x1A, 0x0A]);
    v.push(0x03); // bit0 sequential, bit1 unknown page count
    v.extend_from_slice(embedded);
    v
}

/// A single-generic-region page in the random-access organisation (§D.2): all
/// segment headers first (page-info, generic region, end-of-page, end-of-file),
/// then the data blocks in the same order.
pub fn random_access_generic_file(
    bm: &TestBitmap,
    template: u8,
    at: &[(i8, i8); 4],
    tpgdon: bool,
) -> Vec<u8> {
    // Build (header, data) pairs.
    let page_data = page_info_payload(bm.width, bm.height);
    let region = generic_region_payload(bm, template, at, tpgdon, 0);
    let segs: [(u32, u8, Vec<u32>, Vec<u8>); 4] = [
        (0, 48, vec![], page_data),
        (1, 38, vec![], region),
        (2, 49, vec![], vec![]),
        (3, 51, vec![], vec![]), // end of file (terminates the header section)
    ];

    let mut file = Vec::new();
    file.extend_from_slice(&[0x97, 0x4A, 0x42, 0x32, 0x0D, 0x0A, 0x1A, 0x0A]);
    file.push(0x02); // bit0 = 0 random access, bit1 = 1 page count unknown
    // Header section.
    for (num, ty, refs, data) in &segs {
        file.extend_from_slice(&segment_header(*num, *ty, refs, 1, data.len() as u32));
    }
    // Data section, same order.
    for (_, _, _, data) in &segs {
        file.extend_from_slice(data);
    }
    file
}

// ------------------------------------------------------------------------
// Huffman writer (Phase 5c oracle support)
// ------------------------------------------------------------------------

use jbig2enc_rust::decode::huffman::standard_table;

/// MSB-first bit writer, the mirror of `decode::huffman::BitReader`.
#[derive(Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    nbits: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write_bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | ((b & 1) as u8);
        self.nbits += 1;
        if self.nbits == 8 {
            self.bytes.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }

    pub fn write_bits(&mut self, value: u64, n: u8) {
        for i in (0..n).rev() {
            self.write_bit(((value >> i) & 1) as u32);
        }
    }

    pub fn align(&mut self) {
        while self.nbits != 0 {
            self.write_bit(0);
        }
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        self.align();
        self.bytes
    }
}

fn emit_value(w: &mut BitWriter, table: &HuffmanTable, value: i32) {
    let (code, len, offset, range_len) = table
        .encode_value(value)
        .unwrap_or_else(|| panic!("no Huffman line covers value {value}"));
    w.write_bits(code as u64, len);
    if range_len > 0 {
        w.write_bits(offset, range_len);
    }
}

fn emit_oob(w: &mut BitWriter, table: &HuffmanTable) {
    let (code, len) = table.encode_oob().expect("table has no OOB line");
    w.write_bits(code as u64, len);
}

fn log2_ceil(v: usize) -> u8 {
    if v <= 1 {
        return 0;
    }
    let mut l = 0u8;
    let mut n = 1usize;
    while n < v {
        n <<= 1;
        l += 1;
    }
    l
}

/// Build a Huffman symbol-dictionary segment payload (SDHUFF=1, SDREFAGG=0).
/// All symbols must share one height (one height class); the collective bitmap
/// is emitted uncompressed (BMSIZE=0). Every symbol is exported.
pub fn huffman_symbol_dict_payload(symbols: &[TestBitmap]) -> Vec<u8> {
    assert!(!symbols.is_empty());
    let height = symbols[0].height;
    assert!(symbols.iter().all(|s| s.height == height), "one height class");

    let dh = standard_table(4).unwrap(); // SDHUFFDH
    let dw = standard_table(2).unwrap(); // SDHUFFDW (has OOB)
    let bmsize = standard_table(1).unwrap(); // SDHUFFBMSIZE
    let export = standard_table(1).unwrap(); // §6.5.10 export runs

    let mut w = BitWriter::new();
    // Height class delta height (from HCHEIGHT=0).
    emit_value(&mut w, &dh, height as i32);
    // Delta widths, then OOB.
    let mut prev = 0i32;
    for s in symbols {
        emit_value(&mut w, &dw, s.width as i32 - prev);
        prev = s.width as i32;
    }
    emit_oob(&mut w, &dw);
    // BMSIZE = 0 (uncompressed), then byte-align.
    emit_value(&mut w, &bmsize, 0);
    w.align();
    // Uncompressed collective bitmap: `height` rows of TOTWIDTH pixels, each row
    // byte-padded. Symbols are concatenated left to right.
    for y in 0..height {
        for s in symbols {
            for x in 0..s.width {
                w.write_bit(s.get(x, y) as u32);
            }
        }
        w.align();
    }
    // Export flags: run 0 (not exported, length 0), run N (exported).
    emit_value(&mut w, &export, 0);
    emit_value(&mut w, &export, symbols.len() as i32);
    let data = w.into_bytes();

    let mut payload = Vec::new();
    payload.extend_from_slice(&0x0001u16.to_be_bytes()); // flags: SDHUFF=1, all selections standard
    payload.extend_from_slice(&(symbols.len() as u32).to_be_bytes()); // SDNUMEXSYMS
    payload.extend_from_slice(&(symbols.len() as u32).to_be_bytes()); // SDNUMNEWSYMS
    payload.extend_from_slice(&data);
    payload
}

/// Build a Huffman text-region segment payload (SBHUFF=1, SBREFINE=0, TOPLEFT,
/// SBSTRIPS=1). `placements` are `(symbol_index, s)` in a single strip at
/// `t = 0`, ordered by increasing S. When `transposed`, the S axis is Y.
#[allow(clippy::too_many_arguments)]
pub fn huffman_text_region_payload(
    width: u32,
    height: u32,
    num_symbols: usize,
    symbol_widths: &[u32],
    symbol_heights: &[u32],
    placements: &[(usize, i32)],
    transposed: bool,
) -> Vec<u8> {
    let fs = standard_table(6).unwrap();
    let ds = standard_table(8).unwrap(); // has OOB
    let dt = standard_table(11).unwrap();

    let l = log2_ceil(num_symbols).max(1);
    // Symbol-ID Huffman table: all symbols get code length `l`.
    let mut runcode_lengths = [0u32; 35];
    runcode_lengths[0] = 1;
    runcode_lengths[l as usize] = 1;
    let runcode_table = HuffmanTable::from_code_lengths(&runcode_lengths).unwrap();
    let sym_lengths = vec![l as u32; num_symbols];
    let sym_table = HuffmanTable::from_code_lengths(&sym_lengths).unwrap();

    let mut w = BitWriter::new();
    // §7.4.3.1.7: 35 four-bit runcode lengths.
    for len in runcode_lengths.iter() {
        w.write_bits(*len as u64, 4);
    }
    // One RUNCODE per symbol, each emitting code length `l`.
    let (rc_code, rc_len, _, _) = runcode_table.encode_value(l as i32).unwrap();
    for _ in 0..num_symbols {
        w.write_bits(rc_code as u64, rc_len);
    }
    w.align();

    // Strip data (SBSTRIPS=1 → no T bits; single strip at T=0). Table B.11
    // (SBHUFFDT) codes values >= 1, so use DT0 = 1 and strip DT = 1: initial
    // STRIPT = -(1) then += 1, giving strip T = 0.
    emit_value(&mut w, &dt, 1); // initial STRIPT: DT0 = 1
    emit_value(&mut w, &dt, 1); // strip delta T = 1 -> strip T becomes 0
    // TOPLEFT advances CURS after placement by the glyph extent along the S
    // axis: the width for a normal region, the height when transposed.
    let extent = |sym: usize| {
        if transposed {
            symbol_heights[sym] as i32
        } else {
            symbol_widths[sym] as i32
        }
    };
    // First S.
    let (first_sym, first_s) = placements[0];
    emit_value(&mut w, &fs, first_s);
    emit_symbol_id(&mut w, &sym_table, first_sym);
    let mut cur_s = first_s + extent(first_sym) - 1;
    // Subsequent instances via IDS.
    for &(sym, s) in &placements[1..] {
        let ids = s - cur_s; // SBDSOFFSET = 0
        emit_value(&mut w, &ds, ids);
        emit_symbol_id(&mut w, &sym_table, sym);
        cur_s = s + extent(sym) - 1;
    }
    emit_oob(&mut w, &ds); // end of strip
    let data = w.into_bytes();

    let mut payload = Vec::new();
    // §7.4.1 region info.
    payload.extend_from_slice(&width.to_be_bytes());
    payload.extend_from_slice(&height.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes()); // x
    payload.extend_from_slice(&0u32.to_be_bytes()); // y
    payload.push(0); // region flags: OR
    // §7.4.3.1.1 text flags: SBHUFF=1 (bit0), REFCORNER=TOPLEFT=1 (bits4-5=01),
    // TRANSPOSED (bit6).
    let text_flags: u16 = 0x0001 | (1 << 4) | ((transposed as u16) << 6);
    payload.extend_from_slice(&text_flags.to_be_bytes());
    // §7.4.3.1.2 Huffman flags: all standard (0).
    payload.extend_from_slice(&0u16.to_be_bytes());
    // §7.4.3.1.4 SBNUMINSTANCES.
    payload.extend_from_slice(&(placements.len() as u32).to_be_bytes());
    payload.extend_from_slice(&data);
    payload
}

// ------------------------------------------------------------------------
// Refinement writer (Phase 5e oracle support: GRTEMPLATE-1, TPGRON)
// ------------------------------------------------------------------------

const SLTP_CTX_GR0: usize = 0x010; // T.88 Figure 14 (centre reference), this crate's numbering
const SLTP_CTX_GR1: usize = 0x040; // verified vs jbig2dec (see decode::refinement)

fn ref_get(bm: &TestBitmap, x: i64, y: i64) -> u32 {
    if x < 0 || y < 0 || x >= bm.width as i64 || y >= bm.height as i64 {
        0
    } else {
        bm.get(x as u32, y as u32) as u32
    }
}

/// Refinement context (GRDX=GRDY=0, so reference coords equal target coords),
/// mirroring `decode::refinement::context_gr0`/`context_gr1`.
fn refine_context(
    grtemplate: u8,
    target: &TestBitmap,
    reference: &TestBitmap,
    x: i64,
    y: i64,
) -> usize {
    if grtemplate == 0 {
        let mut cx = 0usize;
        cx |= ref_get(reference, x - 1, y - 1) as usize;
        cx |= (ref_get(reference, x, y - 1) as usize) << 1;
        cx |= (ref_get(reference, x + 1, y - 1) as usize) << 2;
        cx |= (ref_get(reference, x - 1, y) as usize) << 3;
        cx |= (ref_get(reference, x, y) as usize) << 4;
        cx |= (ref_get(reference, x + 1, y) as usize) << 5;
        cx |= (ref_get(target, x - 1, y) as usize) << 6;
        cx |= (ref_get(reference, x - 1, y + 1) as usize) << 7;
        cx |= (ref_get(reference, x, y + 1) as usize) << 8;
        cx |= (ref_get(reference, x + 1, y + 1) as usize) << 9;
        cx |= (ref_get(target, x + 1, y - 1) as usize) << 10;
        cx |= (ref_get(target, x, y - 1) as usize) << 11;
        cx |= (ref_get(target, x - 1, y - 1) as usize) << 12; // GRAT1 nominal (-1,-1)
        cx
    } else {
        let mut cx = 0usize;
        cx |= ref_get(target, x - 1, y) as usize;
        cx |= (ref_get(target, x + 1, y - 1) as usize) << 1;
        cx |= (ref_get(target, x, y - 1) as usize) << 2;
        cx |= (ref_get(target, x - 1, y - 1) as usize) << 3;
        cx |= (ref_get(reference, x + 1, y + 1) as usize) << 4;
        cx |= (ref_get(reference, x, y + 1) as usize) << 5;
        cx |= (ref_get(reference, x + 1, y) as usize) << 6;
        cx |= (ref_get(reference, x, y) as usize) << 7;
        cx |= (ref_get(reference, x - 1, y) as usize) << 8;
        cx |= (ref_get(reference, x, y - 1) as usize) << 9;
        cx
    }
}

/// If the reference 3×3 neighbourhood at `(x, y)` is uniform, its value.
fn ref_typical(reference: &TestBitmap, x: i64, y: i64) -> Option<bool> {
    let mut sum = 0u32;
    for dy in -1..=1 {
        for dx in -1..=1 {
            sum += ref_get(reference, x + dx, y + dy);
        }
    }
    match sum {
        0 => Some(false),
        9 => Some(true),
        _ => None,
    }
}

/// Arithmetic data for a refinement region refining `reference` into `target`
/// (same size, offset 0), with the given template and TPGRON flag.
pub fn refinement_arith_data(
    target: &TestBitmap,
    reference: &TestBitmap,
    grtemplate: u8,
    tpgron: bool,
) -> Vec<u8> {
    let mut coder = Jbig2ArithCoder::new();
    let sltp_ctx = if grtemplate == 0 { SLTP_CTX_GR0 } else { SLTP_CTX_GR1 };
    let mut ltp = false;
    for y in 0..target.height {
        if tpgron {
            // A row can use LTP=1 only if every typical pixel already matches its
            // predicted value in `target`.
            let mut desired = true;
            for x in 0..target.width {
                if let Some(v) = ref_typical(reference, x as i64, y as i64) {
                    if target.get(x, y) != v {
                        desired = false;
                        break;
                    }
                }
            }
            let sltp = desired ^ ltp;
            coder.encode_bit(sltp_ctx, sltp);
            ltp = desired;
        }
        for x in 0..target.width {
            if tpgron && ltp && ref_typical(reference, x as i64, y as i64).is_some() {
                continue; // typical pixel copied by the decoder; not coded
            }
            let ctx = refine_context(grtemplate, target, reference, x as i64, y as i64);
            coder.encode_bit(ctx, target.get(x, y));
        }
    }
    coder.flush(true);
    coder.as_bytes().to_vec()
}

/// A page that first paints `reference` with a generic region, then refines it
/// into `target` with an immediate generic refinement region (type 42).
pub fn refinement_page(
    reference: &TestBitmap,
    target: &TestBitmap,
    grtemplate: u8,
    tpgron: bool,
) -> Vec<u8> {
    assert_eq!(reference.width, target.width);
    assert_eq!(reference.height, target.height);
    let w = reference.width;
    let h = reference.height;
    let mut stream = Vec::new();

    let page_data = page_info_payload(w, h);
    stream.extend_from_slice(&segment_header(0, 48, &[], 1, page_data.len() as u32));
    stream.extend_from_slice(&page_data);

    // Generic region painting the reference.
    let gen_region = generic_region_payload(reference, 0, &nominal_at(0), false, 0);
    stream.extend_from_slice(&segment_header(1, 38, &[], 1, gen_region.len() as u32));
    stream.extend_from_slice(&gen_region);

    // Immediate generic refinement region (type 42), no referred segment.
    let mut region = Vec::new();
    region.extend_from_slice(&w.to_be_bytes());
    region.extend_from_slice(&h.to_be_bytes());
    region.extend_from_slice(&0u32.to_be_bytes()); // x
    region.extend_from_slice(&0u32.to_be_bytes()); // y
    region.push(4); // region flags: external combination operator = REPLACE
    let refine_flags = (grtemplate & 0x01) | ((tpgron as u8) << 1);
    region.push(refine_flags);
    if grtemplate == 0 {
        // AT: GRAT1 nominal (-1,-1), GRAT2 (-1,-1).
        region.push((-1i8) as u8);
        region.push((-1i8) as u8);
        region.push((-1i8) as u8);
        region.push((-1i8) as u8);
    }
    region.extend_from_slice(&refinement_arith_data(target, reference, grtemplate, tpgron));
    stream.extend_from_slice(&segment_header(2, 42, &[], 1, region.len() as u32));
    stream.extend_from_slice(&region);

    stream.extend_from_slice(&segment_header(3, 49, &[], 1, 0));
    stream
}

// ------------------------------------------------------------------------
// SDREFAGG symbol-dictionary writer (Phase 5e oracle support)
// ------------------------------------------------------------------------

fn to_bitimage(bm: &TestBitmap) -> BitImage {
    let mut img = BitImage::new(bm.width, bm.height).unwrap();
    for y in 0..bm.height {
        for x in 0..bm.width {
            if bm.get(x, y) {
                img.set(x, y, true);
            }
        }
    }
    img
}

/// Build an SDREFAGG=1 arithmetic symbol-dictionary payload (SDHUFF=0,
/// SDTEMPLATE=0, SDRTEMPLATE=0, REFAGGNINST=1). Each new symbol `targets[i]`
/// is coded as a refinement (GRREFERENCEDX/DY = 0) of `imported[refs[i]]`, and
/// every new symbol is exported. `imported` are the referred dictionary's
/// exported symbols (the refinement references). All targets share one height
/// class; widths must be non-decreasing.
pub fn arith_refagg_dict_payload(
    imported: &[TestBitmap],
    refs: &[usize],
    targets: &[TestBitmap],
) -> Vec<u8> {
    assert_eq!(refs.len(), targets.len());
    let num_new = targets.len();
    let total = imported.len() + num_new;
    let code_len = log2_ceil(total).max(1);
    let height = targets[0].height;

    let mut coder = Jbig2ArithCoder::new();
    // One height class: HCHEIGHT from 0.
    coder.encode_integer(IntProc::Iadh, height as i32).unwrap();
    let mut prev_w = 0i32;
    for (i, t) in targets.iter().enumerate() {
        coder
            .encode_integer(IntProc::Iadw, t.width as i32 - prev_w)
            .unwrap();
        prev_w = t.width as i32;
        // §6.5.8.2: REFAGGNINST=1, symbol id, RDX=RDY=0, refinement.
        coder.encode_integer(IntProc::Iaai, 1).unwrap();
        coder.encode_iaid(refs[i] as u32, code_len).unwrap();
        coder.encode_integer(IntProc::Iardx, 0).unwrap();
        coder.encode_integer(IntProc::Iardy, 0).unwrap();
        let tbi = to_bitimage(t);
        let rbi = to_bitimage(&imported[refs[i]]);
        coder
            .encode_refinement_region(&tbi, &rbi, 0, 0, 0, &[(-1, -1)])
            .unwrap();
    }
    coder.encode_oob(IntProc::Iadw).unwrap();
    // Export: not-exported run over the imported symbols, exported run over new.
    coder
        .encode_integer(IntProc::Iaex, imported.len() as i32)
        .unwrap();
    coder.encode_integer(IntProc::Iaex, num_new as i32).unwrap();
    coder.flush(true);
    let data = coder.as_bytes().to_vec();

    let mut payload = Vec::new();
    // §7.4.2.1.1 flags: SDREFAGG=1 (bit1), SDHUFF=0, SDTEMPLATE=0, SDRTEMPLATE=0.
    payload.extend_from_slice(&0x0002u16.to_be_bytes());
    // §7.4.2.1.2 SDAT (template 0, 4 pairs) nominal.
    for (ax, ay) in [(3i8, -1i8), (-3, -1), (2, -2), (-2, -2)] {
        payload.push(ax as u8);
        payload.push(ay as u8);
    }
    // §7.4.2.1.3 SDRAT (SDRTEMPLATE=0, 2 pairs) nominal (-1,-1).
    for (ax, ay) in [(-1i8, -1i8), (-1, -1)] {
        payload.push(ax as u8);
        payload.push(ay as u8);
    }
    payload.extend_from_slice(&(num_new as u32).to_be_bytes()); // SDNUMEXSYMS
    payload.extend_from_slice(&(num_new as u32).to_be_bytes()); // SDNUMNEWSYMS
    payload.extend_from_slice(&data);
    payload
}

/// Build an SDREFAGG=1 dictionary payload defining ONE aggregate symbol
/// (REFAGGNINST>1, §6.5.8.2 step 2): the new symbol is a text region of
/// `instances` (base_id, s) placed TOPLEFT, RI=0, SBSTRIPS=1, into a
/// `sym_width`-wide bitmap. Every base symbol shares the aggregate's height.
pub fn arith_aggregate_dict_payload(
    imported: &[TestBitmap],
    instances: &[(usize, i32)],
    sym_width: u32,
) -> Vec<u8> {
    let total = imported.len() + 1;
    let code_len = log2_ceil(total).max(1);
    let height = imported[0].height;

    let mut coder = Jbig2ArithCoder::new();
    coder.encode_integer(IntProc::Iadh, height as i32).unwrap();
    // Single new symbol: DW from 0.
    coder.encode_integer(IntProc::Iadw, sym_width as i32).unwrap();
    // Its bitmap is an aggregate text region of `instances`.
    coder
        .encode_integer(IntProc::Iaai, instances.len() as i32)
        .unwrap();
    coder.encode_integer(IntProc::Iadt, 0).unwrap(); // initial STRIPT (DT0)
    coder.encode_integer(IntProc::Iadt, 0).unwrap(); // strip delta T
    let (id0, s0) = instances[0];
    coder.encode_integer(IntProc::Iafs, s0).unwrap();
    coder.encode_iaid(id0 as u32, code_len).unwrap();
    coder.encode_integer(IntProc::Iari, 0).unwrap(); // RI = 0
    let mut cur_s = s0 + imported[id0].width as i32 - 1;
    for &(id, s) in &instances[1..] {
        coder.encode_integer(IntProc::Iads, s - cur_s).unwrap();
        coder.encode_iaid(id as u32, code_len).unwrap();
        coder.encode_integer(IntProc::Iari, 0).unwrap();
        cur_s = s + imported[id].width as i32 - 1;
    }
    coder.encode_oob(IntProc::Iads).unwrap(); // end of strip
    coder.encode_oob(IntProc::Iadw).unwrap(); // end of height class
    coder
        .encode_integer(IntProc::Iaex, imported.len() as i32)
        .unwrap();
    coder.encode_integer(IntProc::Iaex, 1).unwrap();
    coder.flush(true);
    let data = coder.as_bytes().to_vec();

    let mut payload = Vec::new();
    payload.extend_from_slice(&0x0002u16.to_be_bytes()); // SDREFAGG=1, SDHUFF=0
    for (ax, ay) in [(3i8, -1i8), (-3, -1), (2, -2), (-2, -2)] {
        payload.push(ax as u8);
        payload.push(ay as u8);
    }
    for (ax, ay) in [(-1i8, -1i8), (-1, -1)] {
        payload.push(ax as u8);
        payload.push(ay as u8);
    }
    payload.extend_from_slice(&1u32.to_be_bytes()); // SDNUMEXSYMS
    payload.extend_from_slice(&1u32.to_be_bytes()); // SDNUMNEWSYMS
    payload.extend_from_slice(&data);
    payload
}

/// A page: Huffman base dictionary (seg 1), an SDREFAGG dictionary defining one
/// aggregate symbol (seg 2), and a Huffman text region placing that aggregate
/// symbol (seg 3).
pub fn aggregate_page(
    page_w: u32,
    page_h: u32,
    base: &[TestBitmap],
    instances: &[(usize, i32)],
    sym_width: u32,
    sym_height: u32,
    placements: &[(usize, i32)],
) -> Vec<u8> {
    let mut stream = Vec::new();
    let page_data = page_info_payload(page_w, page_h);
    stream.extend_from_slice(&segment_header(0, 48, &[], 1, page_data.len() as u32));
    stream.extend_from_slice(&page_data);

    let base_dict = huffman_symbol_dict_payload(base);
    stream.extend_from_slice(&segment_header(1, 0, &[], 1, base_dict.len() as u32));
    stream.extend_from_slice(&base_dict);

    let agg = arith_aggregate_dict_payload(base, instances, sym_width);
    stream.extend_from_slice(&segment_header(2, 0, &[1], 1, agg.len() as u32));
    stream.extend_from_slice(&agg);

    // Text region placing the single aggregate symbol.
    let region = huffman_text_region_payload(
        page_w,
        page_h,
        1,
        &[sym_width],
        &[sym_height],
        placements,
        false,
    );
    stream.extend_from_slice(&segment_header(3, 6, &[2], 1, region.len() as u32));
    stream.extend_from_slice(&region);

    stream.extend_from_slice(&segment_header(4, 49, &[], 1, 0));
    stream
}

/// A page: a Huffman symbol dictionary of base symbols (segment 1), an SDREFAGG
/// dictionary refining them (segment 2), and a Huffman text region placing the
/// refined symbols (segment 3).
pub fn refagg_page(
    page_w: u32,
    page_h: u32,
    base: &[TestBitmap],
    refs: &[usize],
    targets: &[TestBitmap],
    placements: &[(usize, i32)],
) -> Vec<u8> {
    let mut stream = Vec::new();
    let page_data = page_info_payload(page_w, page_h);
    stream.extend_from_slice(&segment_header(0, 48, &[], 1, page_data.len() as u32));
    stream.extend_from_slice(&page_data);

    // Segment 1: Huffman symbol dictionary of base symbols.
    let base_dict = huffman_symbol_dict_payload(base);
    stream.extend_from_slice(&segment_header(1, 0, &[], 1, base_dict.len() as u32));
    stream.extend_from_slice(&base_dict);

    // Segment 2: SDREFAGG dictionary refining the base symbols (refers seg 1).
    let refagg = arith_refagg_dict_payload(base, refs, targets);
    stream.extend_from_slice(&segment_header(2, 0, &[1], 1, refagg.len() as u32));
    stream.extend_from_slice(&refagg);

    // Segment 3: Huffman text region placing the refined symbols (refers seg 2).
    let widths: Vec<u32> = targets.iter().map(|s| s.width).collect();
    let heights: Vec<u32> = targets.iter().map(|s| s.height).collect();
    let region =
        huffman_text_region_payload(page_w, page_h, targets.len(), &widths, &heights, placements, false);
    stream.extend_from_slice(&segment_header(3, 6, &[2], 1, region.len() as u32));
    stream.extend_from_slice(&region);

    stream.extend_from_slice(&segment_header(4, 49, &[], 1, 0));
    stream
}

// ------------------------------------------------------------------------
// Halftone HENABLESKIP writer (Phase 5e oracle support)
// ------------------------------------------------------------------------

/// Build a pattern-dictionary payload (arithmetic, HDTEMPLATE=0, T.88 §7.4.4).
/// `patterns` are the HDPW×HDPH patterns, index 0..GRAYMAX, laid out side by
/// side in one collective bitmap coded with AT1 = (-HDPW, 0) per §6.7.5.
pub fn pattern_dict_payload(patterns: &[TestBitmap], hdpw: u32, hdph: u32) -> Vec<u8> {
    let graymax = patterns.len() as u32 - 1;
    // Collective bitmap: patterns concatenated left to right.
    let mut collective = TestBitmap::new((graymax + 1) * hdpw, hdph);
    for (i, p) in patterns.iter().enumerate() {
        for y in 0..hdph {
            for x in 0..hdpw {
                if p.get(x, y) {
                    collective.set(i as u32 * hdpw + x, y, true);
                }
            }
        }
    }
    let at = [(-(hdpw as i32) as i8, 0i8), (-3, -1), (2, -2), (-2, -2)];
    let data = generic_arith_data(&collective, 0, &at, false);

    let mut payload = Vec::new();
    payload.push(0x00); // flags: HDMMR=0, HDTEMPLATE=0
    payload.push(hdpw as u8);
    payload.push(hdph as u8);
    payload.extend_from_slice(&graymax.to_be_bytes());
    payload.extend_from_slice(&data);
    payload
}

/// Whether grid cell `(ng, mg)` is skipped (its pattern lies outside the
/// region) — mirrors `decode::halftone_region::compute_skip`.
fn cell_skipped(
    ng: u32,
    mg: u32,
    hgx: i64,
    hgy: i64,
    hrx: i64,
    hry: i64,
    hpw: i64,
    hph: i64,
    hbw: i64,
    hbh: i64,
) -> bool {
    let x = (hgx + mg as i64 * hry + ng as i64 * hrx) >> 8;
    let y = (hgy + mg as i64 * hrx - ng as i64 * hry) >> 8;
    x + hpw <= 0 || x >= hbw || y + hph <= 0 || y >= hbh
}

/// A page: a pattern dictionary (segment 1) plus a halftone region (segment 2)
/// with HENABLESKIP=1 and a single gray-plane (2 patterns). `cell_values` gives
/// the pattern index (0/1) for each non-skipped cell at `[mg][ng]`; the grid is
/// axis-aligned with HRX = HDPW<<8, HRY = 0 (so square patterns, HDPW=HDPH).
pub fn halftone_skip_page(
    page_w: u32,
    page_h: u32,
    patterns: &[TestBitmap],
    hdpw: u32,
    hgw: u32,
    hgh: u32,
    cell_values: &[Vec<bool>],
) -> Vec<u8> {
    let hrx = (hdpw as i64) << 8;
    let hry = 0i64;
    let (hgx, hgy) = (0i64, 0i64);
    let hpw = hdpw as i64;
    let hph = hdpw as i64;
    let hbw = page_w as i64;
    let hbh = page_h as i64;

    // Gray plane (HBPP=1): pixel (ng, mg) = pattern index; skipped cells omitted.
    let mut coder = Jbig2ArithCoder::new();
    // Build the plane bitmap so causal context reads see the coded values.
    let mut plane = TestBitmap::new(hgw, hgh);
    for mg in 0..hgh {
        for ng in 0..hgw {
            if cell_values[mg as usize][ng as usize] {
                plane.set(ng, mg, true);
            }
        }
    }
    let at = [(3i8, -1i8), (-3, -1), (2, -2), (-2, -2)]; // gray-plane AT (template 0)
    for mg in 0..hgh {
        for ng in 0..hgw {
            if cell_skipped(ng, mg, hgx, hgy, hrx, hry, hpw, hph, hbw, hbh) {
                continue;
            }
            let ctx = context(0, &plane, ng as i64, mg as i64, &at);
            coder.encode_bit(ctx, plane.get(ng, mg));
        }
    }
    coder.flush(true);
    let gray_data = coder.as_bytes().to_vec();

    let mut stream = Vec::new();
    let page_data = page_info_payload(page_w, page_h);
    stream.extend_from_slice(&segment_header(0, 48, &[], 1, page_data.len() as u32));
    stream.extend_from_slice(&page_data);

    // Segment 1: pattern dictionary (type 16).
    let pd = pattern_dict_payload(patterns, hdpw, hdpw);
    stream.extend_from_slice(&segment_header(1, 16, &[], 1, pd.len() as u32));
    stream.extend_from_slice(&pd);

    // Segment 2: immediate halftone region (type 22/23) referring to seg 1.
    let mut region = Vec::new();
    region.extend_from_slice(&page_w.to_be_bytes());
    region.extend_from_slice(&page_h.to_be_bytes());
    region.extend_from_slice(&0u32.to_be_bytes()); // x
    region.extend_from_slice(&0u32.to_be_bytes()); // y
    region.push(0); // region flags: OR
    // §7.4.5.1.1 halftone flags: HMMR=0, HTEMPLATE=0, HENABLESKIP=1 (bit3),
    // HCOMBOP=OR (bits4-6=0), HDEFPIXEL=0.
    region.push(0x08);
    region.extend_from_slice(&hgw.to_be_bytes());
    region.extend_from_slice(&hgh.to_be_bytes());
    region.extend_from_slice(&(hgx as i32).to_be_bytes());
    region.extend_from_slice(&(hgy as i32).to_be_bytes());
    region.extend_from_slice(&(hrx as u16).to_be_bytes());
    region.extend_from_slice(&(hry as u16).to_be_bytes());
    region.extend_from_slice(&gray_data);
    stream.extend_from_slice(&segment_header(2, 23, &[1], 1, region.len() as u32));
    stream.extend_from_slice(&region);

    stream.extend_from_slice(&segment_header(3, 49, &[], 1, 0));
    stream
}

/// Build a Huffman refinement/aggregate dictionary payload (SDHUFF=1,
/// SDREFAGG=1, REFAGGNINST=1): each new symbol `targets[i]` is coded as a
/// refinement (RDX=RDY=0) of `imported[refs[i]]`. One height class; widths
/// non-decreasing. Every new symbol is exported.
pub fn huffman_refagg_dict_payload(
    imported: &[TestBitmap],
    refs: &[usize],
    targets: &[TestBitmap],
) -> Vec<u8> {
    let num_new = targets.len();
    let total = imported.len() + num_new;
    let code_len = log2_ceil(total).max(1);
    let height = targets[0].height;

    let dh = standard_table(4).unwrap();
    let dw = standard_table(2).unwrap();
    let agg = standard_table(1).unwrap();
    let b15 = standard_table(15).unwrap();
    let b1 = standard_table(1).unwrap();

    let mut w = BitWriter::new();
    emit_value(&mut w, &dh, height as i32); // HCDH
    let mut prev = 0i32;
    for (i, t) in targets.iter().enumerate() {
        emit_value(&mut w, &dw, t.width as i32 - prev);
        prev = t.width as i32;
        emit_value(&mut w, &agg, 1); // REFAGGNINST = 1
        w.write_bits(refs[i] as u64, code_len); // symbol id (equal-length code)
        emit_value(&mut w, &b15, 0); // RDX
        emit_value(&mut w, &b15, 0); // RDY
        let refine = refinement_arith_data(t, &imported[refs[i]], 0, false);
        emit_value(&mut w, &b1, refine.len() as i32); // BMSIZE
        w.align();
        for b in &refine {
            w.write_bits(*b as u64, 8);
        }
    }
    emit_oob(&mut w, &dw); // end of height class
    emit_value(&mut w, &b1, imported.len() as i32); // export: not-exported run
    emit_value(&mut w, &b1, num_new as i32); // export: exported run
    let data = w.into_bytes();

    let mut payload = Vec::new();
    // flags: SDHUFF=1, SDREFAGG=1, SDTEMPLATE=0, SDRTEMPLATE=0, all tables std.
    payload.extend_from_slice(&0x0003u16.to_be_bytes());
    // SDRAT (SDREFAGG=1, SDRTEMPLATE=0): nominal (-1,-1),(-1,-1).
    for b in [(-1i8) as u8; 4] {
        payload.push(b);
    }
    payload.extend_from_slice(&(num_new as u32).to_be_bytes()); // SDNUMEXSYMS
    payload.extend_from_slice(&(num_new as u32).to_be_bytes()); // SDNUMNEWSYMS
    payload.extend_from_slice(&data);
    payload
}

/// Build a Huffman aggregate dictionary payload (SDHUFF=1, SDREFAGG=1) defining
/// ONE aggregate symbol (REFAGGNINST>1): an internal Huffman text region of
/// `instances` (base_id, s), RI=0, TOPLEFT, SBSTRIPS=1, into a `sym_width`
/// bitmap.
pub fn huffman_aggregate_dict_payload(
    imported: &[TestBitmap],
    instances: &[(usize, i32)],
    sym_width: u32,
) -> Vec<u8> {
    let total = imported.len() + 1;
    let code_len = log2_ceil(total).max(1);
    let height = imported[0].height;

    let dh = standard_table(4).unwrap();
    let dw = standard_table(2).unwrap();
    let agg = standard_table(1).unwrap();
    let fs = standard_table(6).unwrap();
    let ds = standard_table(8).unwrap();
    let dt = standard_table(11).unwrap();
    let b1 = standard_table(1).unwrap();

    let mut w = BitWriter::new();
    emit_value(&mut w, &dh, height as i32); // HCDH
    emit_value(&mut w, &dw, sym_width as i32); // DW (single new symbol)
    emit_value(&mut w, &agg, instances.len() as i32); // REFAGGNINST
    // Internal text region (SBSTRIPS=1): DT0=1, DT=1 -> strip T = 0.
    emit_value(&mut w, &dt, 1);
    emit_value(&mut w, &dt, 1);
    let (id0, s0) = instances[0];
    emit_value(&mut w, &fs, s0);
    w.write_bits(id0 as u64, code_len);
    w.write_bit(0); // RI = 0
    let mut cur_s = s0 + imported[id0].width as i32 - 1;
    for &(id, s) in &instances[1..] {
        emit_value(&mut w, &ds, s - cur_s);
        w.write_bits(id as u64, code_len);
        w.write_bit(0);
        cur_s = s + imported[id].width as i32 - 1;
    }
    emit_oob(&mut w, &ds); // end of strip
    emit_oob(&mut w, &dw); // end of height class
    emit_value(&mut w, &b1, imported.len() as i32);
    emit_value(&mut w, &b1, 1);
    let data = w.into_bytes();

    let mut payload = Vec::new();
    payload.extend_from_slice(&0x0003u16.to_be_bytes()); // SDHUFF=1, SDREFAGG=1
    for b in [(-1i8) as u8; 4] {
        payload.push(b);
    }
    payload.extend_from_slice(&1u32.to_be_bytes()); // SDNUMEXSYMS
    payload.extend_from_slice(&1u32.to_be_bytes()); // SDNUMNEWSYMS
    payload.extend_from_slice(&data);
    payload
}

/// A page: Huffman base dict (seg 1), a Huffman aggregate dict defining one
/// aggregate symbol (seg 2), and a Huffman text region placing it (seg 3).
pub fn huffman_aggregate_page(
    page_w: u32,
    page_h: u32,
    base: &[TestBitmap],
    instances: &[(usize, i32)],
    sym_width: u32,
    sym_height: u32,
    placements: &[(usize, i32)],
) -> Vec<u8> {
    let mut stream = Vec::new();
    let page_data = page_info_payload(page_w, page_h);
    stream.extend_from_slice(&segment_header(0, 48, &[], 1, page_data.len() as u32));
    stream.extend_from_slice(&page_data);

    let base_dict = huffman_symbol_dict_payload(base);
    stream.extend_from_slice(&segment_header(1, 0, &[], 1, base_dict.len() as u32));
    stream.extend_from_slice(&base_dict);

    let agg = huffman_aggregate_dict_payload(base, instances, sym_width);
    stream.extend_from_slice(&segment_header(2, 0, &[1], 1, agg.len() as u32));
    stream.extend_from_slice(&agg);

    let region = huffman_text_region_payload(
        page_w,
        page_h,
        1,
        &[sym_width],
        &[sym_height],
        placements,
        false,
    );
    stream.extend_from_slice(&segment_header(3, 6, &[2], 1, region.len() as u32));
    stream.extend_from_slice(&region);

    stream.extend_from_slice(&segment_header(4, 49, &[], 1, 0));
    stream
}

/// A page: Huffman base dictionary (seg 1), a Huffman refinement/aggregate
/// dictionary refining the base symbols (seg 2), and a Huffman text region
/// placing the refined symbols (seg 3).
pub fn huffman_refagg_page(
    page_w: u32,
    page_h: u32,
    base: &[TestBitmap],
    refs: &[usize],
    targets: &[TestBitmap],
    placements: &[(usize, i32)],
) -> Vec<u8> {
    let mut stream = Vec::new();
    let page_data = page_info_payload(page_w, page_h);
    stream.extend_from_slice(&segment_header(0, 48, &[], 1, page_data.len() as u32));
    stream.extend_from_slice(&page_data);

    let base_dict = huffman_symbol_dict_payload(base);
    stream.extend_from_slice(&segment_header(1, 0, &[], 1, base_dict.len() as u32));
    stream.extend_from_slice(&base_dict);

    let refagg = huffman_refagg_dict_payload(base, refs, targets);
    stream.extend_from_slice(&segment_header(2, 0, &[1], 1, refagg.len() as u32));
    stream.extend_from_slice(&refagg);

    let widths: Vec<u32> = targets.iter().map(|s| s.width).collect();
    let heights: Vec<u32> = targets.iter().map(|s| s.height).collect();
    let region =
        huffman_text_region_payload(page_w, page_h, targets.len(), &widths, &heights, placements, false);
    stream.extend_from_slice(&segment_header(3, 6, &[2], 1, region.len() as u32));
    stream.extend_from_slice(&region);

    stream.extend_from_slice(&segment_header(4, 49, &[], 1, 0));
    stream
}

/// A page with a Huffman base dictionary (seg 1) and a Huffman text region
/// (seg 2, SBHUFF=1 ∧ SBREFINE=1) placing each instance as a refinement of a
/// base symbol. `instances` are `(base_id, s, target)`; each target has the same
/// size as its base (RDW=RDH=RDX=RDY=0, GRDX=GRDY=0). TOPLEFT, SBSTRIPS=1.
pub fn huffman_refine_text_page(
    page_w: u32,
    page_h: u32,
    base: &[TestBitmap],
    instances: &[(usize, i32, TestBitmap)],
) -> Vec<u8> {
    let mut stream = Vec::new();
    let page_data = page_info_payload(page_w, page_h);
    stream.extend_from_slice(&segment_header(0, 48, &[], 1, page_data.len() as u32));
    stream.extend_from_slice(&page_data);

    let base_dict = huffman_symbol_dict_payload(base);
    stream.extend_from_slice(&segment_header(1, 0, &[], 1, base_dict.len() as u32));
    stream.extend_from_slice(&base_dict);

    // Build the text-region bit stream.
    let num_syms = base.len();
    let l = log2_ceil(num_syms).max(1);
    let mut runcode_lengths = [0u32; 35];
    runcode_lengths[0] = 1;
    runcode_lengths[l as usize] = 1;
    let runcode_table = HuffmanTable::from_code_lengths(&runcode_lengths).unwrap();
    let sym_table = HuffmanTable::from_code_lengths(&vec![l as u32; num_syms]).unwrap();

    let fs = standard_table(6).unwrap();
    let ds = standard_table(8).unwrap();
    let dt = standard_table(11).unwrap();
    let b14 = standard_table(14).unwrap();
    let b1 = standard_table(1).unwrap();

    let mut w = BitWriter::new();
    for len in runcode_lengths.iter() {
        w.write_bits(*len as u64, 4);
    }
    let (rc_code, rc_len, _, _) = runcode_table.encode_value(l as i32).unwrap();
    for _ in 0..num_syms {
        w.write_bits(rc_code as u64, rc_len);
    }
    w.align();

    emit_value(&mut w, &dt, 1); // DT0 -> STRIPT = -1
    emit_value(&mut w, &dt, 1); // strip DT -> T = 0

    let (_id0, s0, _) = &instances[0];
    emit_value(&mut w, &fs, *s0);
    let mut cur_s = *s0;
    for (i, (id, s, target)) in instances.iter().enumerate() {
        if i > 0 {
            emit_value(&mut w, &ds, *s - cur_s);
        }
        // symbol ID.
        let (code, len, _, _) = sym_table.encode_value(*id as i32).unwrap();
        w.write_bits(code as u64, len);
        // RI = 1.
        w.write_bit(1);
        // RDW=RDH=RDX=RDY=0 (Table B.14).
        emit_value(&mut w, &b14, 0);
        emit_value(&mut w, &b14, 0);
        emit_value(&mut w, &b14, 0);
        emit_value(&mut w, &b14, 0);
        // Refinement block (fresh coder, offset 0), then BMSIZE (Table B.1).
        let refine = refinement_arith_data(target, &base[*id], 0, false);
        emit_value(&mut w, &b1, refine.len() as i32);
        w.align();
        for b in &refine {
            w.write_bits(*b as u64, 8);
        }
        // Already byte-aligned; CURS advances by the placed width.
        cur_s = *s + target.width as i32 - 1;
    }
    emit_oob(&mut w, &ds);
    let data = w.into_bytes();

    let mut region = Vec::new();
    region.extend_from_slice(&page_w.to_be_bytes());
    region.extend_from_slice(&page_h.to_be_bytes());
    region.extend_from_slice(&0u32.to_be_bytes());
    region.extend_from_slice(&0u32.to_be_bytes());
    region.push(0);
    // Text flags: SBHUFF=1, SBREFINE=1, REFCORNER=TOPLEFT.
    let text_flags: u16 = 0x0001 | 0x0002 | (1 << 4);
    region.extend_from_slice(&text_flags.to_be_bytes());
    region.extend_from_slice(&0u16.to_be_bytes()); // Huffman flags: all standard
    // SBRAT (SBREFINE=1, SBRTEMPLATE=0): nominal (-1,-1),(-1,-1).
    region.push((-1i8) as u8);
    region.push((-1i8) as u8);
    region.push((-1i8) as u8);
    region.push((-1i8) as u8);
    region.extend_from_slice(&(instances.len() as u32).to_be_bytes());
    region.extend_from_slice(&data);
    stream.extend_from_slice(&segment_header(2, 6, &[1], 1, region.len() as u32));
    stream.extend_from_slice(&region);

    stream.extend_from_slice(&segment_header(3, 49, &[], 1, 0));
    stream
}

// ------------------------------------------------------------------------
// Retained arithmetic contexts writer (Phase 5e oracle support, §6.5.5)
// ------------------------------------------------------------------------

/// Encode one arithmetic symbol-dictionary's inner data onto `coder`: a single
/// height class of generic (template-0) symbols, then the export runs. The
/// generic contexts are whatever `coder` currently holds (fresh, or imported
/// from a previous dictionary via `reinit_registers_keep_bitmap_contexts`).
fn encode_generic_dict_inner(
    coder: &mut Jbig2ArithCoder,
    new_symbols: &[TestBitmap],
    imported_count: usize,
) {
    let height = new_symbols[0].height;
    let at = nominal_at(0);
    coder.encode_integer(IntProc::Iadh, height as i32).unwrap();
    let mut prev = 0i32;
    for sym in new_symbols {
        coder
            .encode_integer(IntProc::Iadw, sym.width as i32 - prev)
            .unwrap();
        prev = sym.width as i32;
        for y in 0..height {
            for x in 0..sym.width {
                let ctx = context(0, sym, x as i64, y as i64, &at);
                coder.encode_bit(ctx, sym.get(x, y));
            }
        }
    }
    coder.encode_oob(IntProc::Iadw).unwrap();
    coder
        .encode_integer(IntProc::Iaex, imported_count as i32)
        .unwrap();
    coder
        .encode_integer(IntProc::Iaex, new_symbols.len() as i32)
        .unwrap();
}

fn arith_dict_payload(data: &[u8], num_ex: usize, num_new: usize, used: bool, retained: bool) -> Vec<u8> {
    let mut flags: u16 = 0; // SDHUFF=0, SDREFAGG=0, SDTEMPLATE=0
    if used {
        flags |= 0x0100;
    }
    if retained {
        flags |= 0x0200;
    }
    let mut payload = Vec::new();
    payload.extend_from_slice(&flags.to_be_bytes());
    for (ax, ay) in [(3i8, -1i8), (-3, -1), (2, -2), (-2, -2)] {
        payload.push(ax as u8);
        payload.push(ay as u8);
    }
    payload.extend_from_slice(&(num_ex as u32).to_be_bytes());
    payload.extend_from_slice(&(num_new as u32).to_be_bytes());
    payload.extend_from_slice(data);
    payload
}

/// A page with two arithmetic symbol dictionaries where the second imports the
/// first's bitmap-coding contexts (T.88 §6.5.5): dict A (seg 1, retained=1),
/// dict B (seg 2, refers A, used=1), then a Huffman text region placing dict B's
/// symbols. All symbols share one height and have non-decreasing widths.
pub fn retained_context_page(
    page_w: u32,
    page_h: u32,
    syms_a: &[TestBitmap],
    syms_b: &[TestBitmap],
    placements: &[(usize, i32)],
) -> Vec<u8> {
    let mut coder = Jbig2ArithCoder::new();
    encode_generic_dict_inner(&mut coder, syms_a, 0);
    coder.flush(true);
    let data_a = coder.as_bytes().to_vec();
    let dict_a = arith_dict_payload(&data_a, syms_a.len(), syms_a.len(), false, true);

    // Dict B: keep A's bitmap contexts, reset the MQ registers, code B.
    coder.reinit_registers_keep_bitmap_contexts();
    encode_generic_dict_inner(&mut coder, syms_b, syms_a.len());
    coder.flush(true);
    let data_b = coder.as_bytes().to_vec();
    let dict_b = arith_dict_payload(&data_b, syms_b.len(), syms_b.len(), true, false);

    let mut stream = Vec::new();
    let page_data = page_info_payload(page_w, page_h);
    stream.extend_from_slice(&segment_header(0, 48, &[], 1, page_data.len() as u32));
    stream.extend_from_slice(&page_data);
    stream.extend_from_slice(&segment_header(1, 0, &[], 1, dict_a.len() as u32));
    stream.extend_from_slice(&dict_a);
    stream.extend_from_slice(&segment_header(2, 0, &[1], 1, dict_b.len() as u32));
    stream.extend_from_slice(&dict_b);

    let widths: Vec<u32> = syms_b.iter().map(|s| s.width).collect();
    let heights: Vec<u32> = syms_b.iter().map(|s| s.height).collect();
    let region = huffman_text_region_payload(
        page_w,
        page_h,
        syms_b.len(),
        &widths,
        &heights,
        placements,
        false,
    );
    stream.extend_from_slice(&segment_header(3, 6, &[2], 1, region.len() as u32));
    stream.extend_from_slice(&region);

    stream.extend_from_slice(&segment_header(4, 49, &[], 1, 0));
    stream
}

/// A page with an intermediate generic region (seg 1, type 36 — stored as an
/// auxiliary buffer, not drawn) and an immediate generic refinement region
/// (seg 2, type 42, referring seg 1) that refines it and composites onto the
/// page (T.88 §7.4.7.4: GRREFERENCE = the referred region's buffer, GRREFDX/DY
/// = 0). `reference` and `target` are the same size.
pub fn intermediate_refine_page(reference: &TestBitmap, target: &TestBitmap) -> Vec<u8> {
    assert_eq!(reference.width, target.width);
    assert_eq!(reference.height, target.height);
    let w = reference.width;
    let h = reference.height;
    let mut stream = Vec::new();

    let page_data = page_info_payload(w, h);
    stream.extend_from_slice(&segment_header(0, 48, &[], 1, page_data.len() as u32));
    stream.extend_from_slice(&page_data);

    // Intermediate generic region (type 36): defines the reference bitmap.
    let gen_region = generic_region_payload(reference, 0, &nominal_at(0), false, 0);
    stream.extend_from_slice(&segment_header(1, 36, &[], 1, gen_region.len() as u32));
    stream.extend_from_slice(&gen_region);

    // Immediate generic refinement region (type 42) referring to segment 1.
    let mut region = Vec::new();
    region.extend_from_slice(&w.to_be_bytes());
    region.extend_from_slice(&h.to_be_bytes());
    region.extend_from_slice(&0u32.to_be_bytes()); // x
    region.extend_from_slice(&0u32.to_be_bytes()); // y
    region.push(0); // region flags: external combination operator = OR
    region.push(0); // refinement flags: GRTEMPLATE=0, TPGRON=0
    // AT: GRAT1 nominal (-1,-1), GRAT2 (-1,-1).
    region.push((-1i8) as u8);
    region.push((-1i8) as u8);
    region.push((-1i8) as u8);
    region.push((-1i8) as u8);
    region.extend_from_slice(&refinement_arith_data(target, reference, 0, false));
    stream.extend_from_slice(&segment_header(2, 42, &[1], 1, region.len() as u32));
    stream.extend_from_slice(&region);

    stream.extend_from_slice(&segment_header(3, 49, &[], 1, 0));
    stream
}

fn emit_symbol_id(w: &mut BitWriter, sym_table: &HuffmanTable, id: usize) {
    let (code, len, _, _) = sym_table
        .encode_value(id as i32)
        .unwrap_or_else(|| panic!("no symbol-ID code for {id}"));
    w.write_bits(code as u64, len);
}

/// Assemble a page with a Huffman symbol dictionary (segment 1) and a Huffman
/// text region (segment 2) referring to it, plus page info and end-of-page.
pub fn huffman_symbol_text_page(
    page_w: u32,
    page_h: u32,
    symbols: &[TestBitmap],
    placements: &[(usize, i32)],
) -> Vec<u8> {
    huffman_symbol_text_page_ex(page_w, page_h, symbols, placements, false)
}

/// As [`huffman_symbol_text_page`], with an explicit `transposed` flag.
pub fn huffman_symbol_text_page_ex(
    page_w: u32,
    page_h: u32,
    symbols: &[TestBitmap],
    placements: &[(usize, i32)],
    transposed: bool,
) -> Vec<u8> {
    let mut stream = Vec::new();
    // Segment 0: page info.
    let page_data = page_info_payload(page_w, page_h);
    stream.extend_from_slice(&segment_header(0, 48, &[], 1, page_data.len() as u32));
    stream.extend_from_slice(&page_data);
    // Segment 1: Huffman symbol dictionary (type 0).
    let dict = huffman_symbol_dict_payload(symbols);
    stream.extend_from_slice(&segment_header(1, 0, &[], 1, dict.len() as u32));
    stream.extend_from_slice(&dict);
    // Segment 2: Huffman text region (type 6, immediate) referring to segment 1.
    let widths: Vec<u32> = symbols.iter().map(|s| s.width).collect();
    let heights: Vec<u32> = symbols.iter().map(|s| s.height).collect();
    let region = huffman_text_region_payload(
        page_w,
        page_h,
        symbols.len(),
        &widths,
        &heights,
        placements,
        transposed,
    );
    stream.extend_from_slice(&segment_header(2, 6, &[1], 1, region.len() as u32));
    stream.extend_from_slice(&region);
    // Segment 3: end of page.
    stream.extend_from_slice(&segment_header(3, 49, &[], 1, 0));
    stream
}
