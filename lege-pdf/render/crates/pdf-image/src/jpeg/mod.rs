//! In-house JPEG (DCT) codec — the decoder side of `/DCTDecode`.
//!
//! The test encoder uses vstroebel's `jpeg-encoder` crate while this decoder
//! retains the standard zigzag table and AAN DCT constants used by its inverse
//! transform. The decoder is scalar baseline with targeted SIMD paths —
//! optimization seams (fixed-point color convert, SIMD IDCT, MCU-row
//! streaming for baseline scans) are marked where they apply.
//!
//! Coverage, chosen for what PDFs in the wild actually embed:
//! - Baseline (SOF0) and extended sequential (SOF1) Huffman scans,
//!   interleaved and single-component, with restart markers.
//! - Progressive (SOF2): spectral selection + successive approximation,
//!   DC/AC first and refinement scans, EOB runs.
//! - Grayscale, YCbCr (all common subsampling factors), RGB (rare,
//!   component-ID or Adobe-tagged), CMYK and YCCK (Adobe APP14). Four-channel
//!   output is libjpeg's *raw* DeviceCMYK convention: Adobe's inverted-sample
//!   polarity is deliberately left for the consumer's `/Decode` array to undo,
//!   matching PDFium's `cpdf_dib` DCT path byte-for-byte (see [`assemble`]).
//! - Tolerant of: missing EOI, truncated entropy data (zero-bit padding),
//!   stray bytes between markers, dimension-mismatched `/Width`/`/Height`.
//!
//! Not covered (typed errors, see DEFERRED.md): arithmetic coding,
//! lossless/hierarchical JPEG, 12-bit precision.
//!
//! All decoding is bounded by [`DecodeLimits`]: dimensions and buffer
//! allocations use checked arithmetic against `max_pixels` /
//! `max_output_bytes`, and cancellation is probed per block row.

// The AAN IDCT constants are the canonical values from the JPEG
// literature; keep them exactly as published.
#![allow(clippy::excessive_precision, clippy::approx_constant)]

pub mod encoder;

use std::sync::{Arc, OnceLock};

use crate::codec::{DecodeLimits, DecodedFormat, DecodedImage, ImageCodec};
use crate::{DecodeParameters, ImageDescriptor, ImageError, StreamFilter};

use encoder::{AAN_SCALE_FACTORS, ZIGZAG_INV};

/// The `/DCTDecode` codec for a [`crate::CodecRegistry`].
#[derive(Debug, Default)]
pub struct JpegCodec;

impl ImageCodec for JpegCodec {
    fn filter(&self) -> StreamFilter {
        StreamFilter::DctDecode
    }

    fn decode(
        &self,
        data: &[u8],
        descriptor: &ImageDescriptor,
        params: &DecodeParameters,
        limits: &DecodeLimits,
    ) -> Result<DecodedImage, ImageError> {
        decode_jpeg_scaled(data, limits, Some(descriptor), params.target_size)
    }
}

/// Decode a complete JPEG stream. The output dimensions are the stream's
/// own (`SOF`), which take precedence over the PDF dictionary's on mismatch.
pub fn decode_jpeg(data: &[u8], limits: &DecodeLimits) -> Result<DecodedImage, ImageError> {
    decode_jpeg_with_descriptor(data, limits, None)
}

/// As [`decode_jpeg`], with the PDF image dictionary available. The dictionary
/// is not allowed to override the stream in general — a mismatch is the
/// stream's to win — but it is the only way to recover an `SOF` that declares
/// height `0xFFFF` ("height follows in a DNL marker") when the DNL never
/// arrives. See [`Decoder::expected_size`].
pub fn decode_jpeg_with_descriptor(
    data: &[u8],
    limits: &DecodeLimits,
    descriptor: Option<&ImageDescriptor>,
) -> Result<DecodedImage, ImageError> {
    decode_jpeg_scaled(data, limits, descriptor, None)
}

/// As [`decode_jpeg_with_descriptor`], with the draw's device-space footprint.
///
/// A minified draw is decoded at 1/2, 1/4 or 1/8 scale rather than full size,
/// and an image too large to hold at full resolution is reduced until it fits
/// instead of failing outright (see [`Decoder::pick_dct_size`]). The returned
/// raster carries its own dimensions, which the renderer already uses in place
/// of the dictionary's.
pub fn decode_jpeg_scaled(
    data: &[u8],
    limits: &DecodeLimits,
    descriptor: Option<&ImageDescriptor>,
    target_size: Option<(u32, u32)>,
) -> Result<DecodedImage, ImageError> {
    limits.check_input(data.len())?;
    let mut d = Decoder::new(data, limits);
    d.expected_size = descriptor
        .filter(|desc| desc.width > 0 && desc.height > 0)
        .map(|desc| (desc.width as usize, desc.height as usize));
    d.target_size = target_size.map(|(w, h)| (w as usize, h as usize));
    d.run()?;
    d.finish()
}

fn err(msg: &str) -> ImageError {
    ImageError::Decode(msg.into())
}

// --- Huffman tables ----------------------------------------------------------

/// Canonical Huffman decode table built from a DHT segment's 16 length
/// counts + value list — the same code assignment the encoder's
/// `generate_huffman_table` produces.
struct HuffTable {
    /// One-shot lookup on the next 8 bits: `(value, code_length)`;
    /// `code_length == 0` means "longer than 8 bits, take the slow path".
    fast: Box<[(u8, u8); 256]>,
    /// Slow path per code length `l` (1-indexed): the largest code of
    /// length `l` (or -1 when none), the smallest, and where its values
    /// start in `values`.
    max_code: [i32; 17],
    min_code: [i32; 17],
    val_ptr: [usize; 17],
    values: Vec<u8>,
}

impl HuffTable {
    fn build(counts: &[u8; 16], values: Vec<u8>) -> Result<HuffTable, ImageError> {
        let total: usize = counts.iter().map(|&c| c as usize).sum();
        if total != values.len() || total > 256 {
            return Err(err("DHT count/value mismatch"));
        }
        let mut fast = Box::new([(0u8, 0u8); 256]);
        let mut max_code = [-1i32; 17];
        let mut min_code = [0i32; 17];
        let mut val_ptr = [0usize; 17];
        let mut code: i32 = 0;
        let mut k = 0usize;
        for l in 1..=16usize {
            let n = counts[l - 1] as usize;
            if n > 0 {
                val_ptr[l] = k;
                min_code[l] = code;
                if code + n as i32 > (1 << l) {
                    return Err(err("DHT overlong code assignment"));
                }
                if l <= 8 {
                    // Populate every 8-bit prefix of each code.
                    for i in 0..n {
                        let c = (code + i as i32) as usize;
                        let shift = 8 - l;
                        for tail in 0..(1usize << shift) {
                            fast[(c << shift) | tail] = (values[k + i], l as u8);
                        }
                    }
                }
                code += n as i32;
                max_code[l] = code - 1;
                k += n;
            }
            code <<= 1;
        }
        Ok(HuffTable {
            fast,
            max_code,
            min_code,
            val_ptr,
            values,
        })
    }
}

// --- bit reader ---------------------------------------------------------------

/// MSB-first bit reader over entropy-coded data: unstuffs `0xFF 0x00`,
/// stops (and pads with zero bits, libjpeg-style) at any real marker, and
/// resynchronizes across restart markers. The exact inverse of the
/// encoder's `BitWriter`.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    /// MSB-aligned bit buffer: the next bit to deliver is bit 63.
    buf: u64,
    count: u8,
    /// A real marker (or end of data) was reached; reads return zero bits.
    hit_marker: bool,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self {
            data,
            pos,
            buf: 0,
            count: 0,
            hit_marker: false,
        }
    }

    /// Top up the buffer to at least 57 valid bits (zero-padded past any
    /// real marker / end of data). Pass 4: the common case slurps whole
    /// words — when the next 8 bytes contain no `0xFF` there is no byte
    /// stuffing or marker to handle, so they can be inserted in one shot
    /// (libjpeg-turbo's batched refill); the byte-at-a-time path below is
    /// the exact original semantics and remains the arbiter.
    #[inline]
    fn fill(&mut self) {
        if self.count > 56 {
            return;
        }
        if !self.hit_marker
            && let Some(chunk) = self.data.get(self.pos..self.pos + 8)
            && let Ok(arr) = <[u8; 8]>::try_from(chunk)
        {
            let word = u64::from_be_bytes(arr);
            // Zero-byte test on !word == "any byte equals 0xFF".
            let inv = !word;
            if inv.wrapping_sub(0x0101_0101_0101_0101) & !inv & 0x8080_8080_8080_8080 == 0 {
                // Insert as many whole bytes as fit above the valid
                // region; the tail of `word` past that must be
                // masked off so it cannot collide with later fills.
                let take = ((64 - self.count) / 8) as usize; // 1..=8
                let keep = word & (!0u64 << (64 - 8 * take as u32));
                self.buf |= keep >> self.count;
                self.count += (take * 8) as u8;
                self.pos += take;
                return;
            }
        }
        while self.count <= 56 {
            let byte = if self.hit_marker {
                0
            } else {
                match self.data.get(self.pos) {
                    None => {
                        self.hit_marker = true;
                        0
                    }
                    Some(&0xFF) => match self.data.get(self.pos + 1) {
                        Some(&0x00) => {
                            self.pos += 2;
                            0xFF
                        }
                        _ => {
                            // A real marker (or truncation): stop consuming.
                            self.hit_marker = true;
                            0
                        }
                    },
                    Some(&b) => {
                        self.pos += 1;
                        b
                    }
                }
            };
            self.buf |= (byte as u64) << (56 - self.count);
            self.count += 8;
        }
    }

    #[inline]
    fn consume(&mut self, n: u8) {
        self.buf <<= n as u32;
        self.count -= n;
    }

    #[inline]
    fn read_bit(&mut self) -> u32 {
        self.fill();
        let b = (self.buf >> 63) as u32;
        self.consume(1);
        b
    }

    /// The next `n` (0..=16) bits, MSB first. `n == 0` reads nothing.
    #[inline]
    fn read_bits(&mut self, n: u8) -> u32 {
        self.fill();
        self.take_bits(n)
    }

    /// `read_bits` without the refill: valid only after a `fill` this call
    /// site already performed (≥ 57 bits buffered covers a 16-bit code plus
    /// a 16-bit magnitude between refills).
    #[inline]
    fn take_bits(&mut self, n: u8) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = (self.buf >> (64 - n as u32)) as u32;
        self.consume(n);
        v
    }

    /// Decode one Huffman symbol.
    #[inline]
    fn decode(&mut self, table: &HuffTable) -> u8 {
        self.fill();
        self.decode_prefilled(table)
    }

    /// `decode` without the refill — same contract as [`Self::take_bits`]:
    /// the buffer already holds ≥ 57 bits, enough for the 16-bit worst-case
    /// code below without touching the input.
    #[inline]
    fn decode_prefilled(&mut self, table: &HuffTable) -> u8 {
        let peek = (self.buf >> 56) as u8;
        let (value, len) = table.fast[peek as usize];
        if len > 0 {
            self.consume(len);
            return value;
        }
        // Slow path: extend bit by bit from 9 to 16.
        let mut code = peek as i32;
        self.consume(8);
        for l in 9..=16usize {
            code = (code << 1) | (self.buf >> 63) as i32;
            self.consume(1);
            if table.max_code[l] >= 0 && code <= table.max_code[l] {
                let idx = table.val_ptr[l] + (code - table.min_code[l]) as usize;
                return table.values.get(idx).copied().unwrap_or(0);
            }
        }
        // Corrupt data (or the zero-bit padding after truncation): return
        // symbol 0 so the bounded MCU loops run to completion.
        0
    }

    /// Restart-marker resync: drop buffered bits and consume the next RSTn.
    ///
    /// Buffered-but-unread bits never contain marker bytes (`fill` stops at
    /// them), so scanning forward from `pos` cannot land on an
    /// already-consumed marker; a missing RSTn is tolerated (position is
    /// left unchanged and decoding continues).
    fn restart(&mut self) {
        self.buf = 0;
        self.count = 0;
        self.hit_marker = false;
        let limit = (self.pos + 64).min(self.data.len().saturating_sub(1));
        let mut p = self.pos;
        while p < limit {
            if self.data[p] == 0xFF && (0xD0..=0xD7).contains(&self.data[p + 1]) {
                self.pos = p + 2;
                return;
            }
            p += 1;
        }
    }
}

/// Sign-extend an `n`-bit magnitude per ITU T.81 F.2.2.1 (`EXTEND`).
#[inline]
fn extend(v: u32, n: u8) -> i32 {
    if n == 0 {
        0
    } else if (v as i32) < (1 << (n - 1)) {
        v as i32 - (1 << n) + 1
    } else {
        v as i32
    }
}

/// Decode one baseline / extended-sequential block (full DC + AC) from the
/// entropy stream into `block` (natural order, length ≥ 64). The caller owns
/// the DC predictor and — for the streaming scratch path — must present a
/// zeroed `block`; the whole-image path writes into once-zeroed coefficient
/// storage, so only the DC and explicitly-coded AC positions are touched.
///
/// Returns `true` iff at least one nonzero AC coefficient was written (an
/// `s != 0` symbol). `false` means the block is DC-only (EOB or ZRL-only
/// straight after the DC term), which lets the streaming path substitute the
/// exact constant `(dc·dequant[0] + 128.5)` for the whole 8×8 IDCT — the IDCT
/// of a DC-only block is provably that constant at every sample (see
/// `dc_only_tests`).
#[inline]
fn decode_block_sequential(
    reader: &mut BitReader,
    dc: &HuffTable,
    ac: &HuffTable,
    pred: &mut i32,
    block: &mut [i16],
) -> bool {
    // Pass 4 batching: one `fill` covers a worst-case 16-bit code plus its
    // 16-bit magnitude, so each (symbol, magnitude) pair costs exactly one
    // refill check instead of two-plus.
    reader.fill();
    let t = reader.decode_prefilled(dc).min(16);
    let diff = extend(reader.take_bits(t), t);
    *pred += diff;
    block[0] = *pred as i16;
    let mut any_ac = false;
    let mut k = 1usize;
    while k <= 63 {
        reader.fill();
        let rs = reader.decode_prefilled(ac);
        let r = (rs >> 4) as usize;
        let s = rs & 15;
        if s == 0 {
            if r == 15 {
                k += 16; // ZRL
                continue;
            }
            break; // EOB
        }
        k += r;
        if k > 63 {
            break; // corrupt run — tolerate
        }
        block[ZIGZAG_INV[k] as usize] = extend(reader.take_bits(s), s) as i16;
        any_ac = true;
        k += 1;
    }
    any_ac
}

/// Allocate every component's whole-image coefficient storage if it has not
/// been already. Used by the coefficient-path scans (progressive, and
/// non-interleaved sequential); the streaming path skips it entirely.
fn ensure_coeffs(frame: &mut Frame) {
    for c in frame.comps.iter_mut() {
        if c.coeffs.is_empty() {
            c.coeffs = vec![0i16; c.bw * c.bh * 64];
        }
    }
}

// --- frame model --------------------------------------------------------------

struct Component {
    id: u8,
    /// Sampling factors.
    h: usize,
    v: usize,
    /// Quantization table selector.
    tq: usize,
    /// Full-image block dimensions, padded to whole MCUs (the coefficient
    /// storage stride is `bw`).
    bw: usize,
    bh: usize,
    /// Non-padded block dimensions (used by non-interleaved scans).
    bw_used: usize,
    bh_used: usize,
    /// All DCT coefficients, natural (row-major) order within each block.
    coeffs: Vec<i16>,
}

struct Frame {
    progressive: bool,
    width: usize,
    height: usize,
    comps: Vec<Component>,
    hmax: usize,
    vmax: usize,
    mcus_x: usize,
    mcus_y: usize,
    /// Decoded samples per block edge: 8 (full size), or 4/2/1 for a 1/2, 1/4
    /// or 1/8 scaled decode. See [`Decoder::pick_dct_size`].
    dct_size: usize,
}

impl Frame {
    /// Output width after any scaled decode.
    fn out_w(&self) -> usize {
        scaled(self.width, self.dct_size)
    }

    /// Output height after any scaled decode.
    fn out_h(&self) -> usize {
        scaled(self.height, self.dct_size)
    }
}

/// A full-resolution extent reduced to a `dct_size`/8 scaled decode, never
/// below one sample.
fn scaled(n: usize, dct_size: usize) -> usize {
    ((n * dct_size).div_ceil(8)).max(1)
}

/// One component's role in the current scan.
#[derive(Clone, Copy)]
struct ScanComp {
    /// Index into `Frame::comps`.
    comp: usize,
    dc_tbl: usize,
    ac_tbl: usize,
}

/// The block-walk geometry of one scan, snapshotted from the frame so scan
/// loops can mutate coefficient storage without aliasing it.
struct ScanGeom {
    single: bool,
    mcus: usize,
    mcus_x: usize,
    /// Cancellation-probe granularity (one MCU/block row).
    row_len: usize,
    comps: Vec<GeomComp>,
}

#[derive(Clone, Copy)]
struct GeomComp {
    ci: usize,
    h: usize,
    v: usize,
    bw: usize,
    bw_used: usize,
}

/// What the scan-block walk hands to its handler.
#[derive(Clone, Copy)]
enum Event {
    /// A restart marker was consumed — clear DC predictors / EOB runs.
    Restart,
    /// Decode the block at `comps[ci].coeffs[off..off + 64]` (`si` is the
    /// component's index within the scan, for table/predictor selection).
    Block { si: usize, ci: usize, off: usize },
}

impl ScanGeom {
    fn of(frame: &Frame, scan_comps: &[ScanComp]) -> ScanGeom {
        let single = scan_comps.len() == 1;
        let comps: Vec<GeomComp> = scan_comps
            .iter()
            .map(|sc| {
                let c = &frame.comps[sc.comp];
                GeomComp {
                    ci: sc.comp,
                    h: c.h,
                    v: c.v,
                    bw: c.bw,
                    bw_used: c.bw_used.max(1),
                }
            })
            .collect();
        let (mcus, row_len) = if single {
            let c = &frame.comps[scan_comps[0].comp];
            (c.bw_used * c.bh_used, c.bw_used.max(1))
        } else {
            (frame.mcus_x * frame.mcus_y, frame.mcus_x.max(1))
        };
        ScanGeom {
            single,
            mcus,
            mcus_x: frame.mcus_x.max(1),
            row_len,
            comps,
        }
    }
}

struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    limits: &'a DecodeLimits,
    quant: [Option<[u16; 64]>; 4],
    dc_tables: [Option<HuffTable>; 4],
    ac_tables: [Option<HuffTable>; 4],
    frame: Option<Frame>,
    restart_interval: usize,
    /// Adobe APP14 color transform (0/1/2); its presence implies the
    /// Photoshop channel inversion for 4-component images.
    adobe_transform: Option<u8>,
    saw_jfif: bool,
    scans_done: usize,
    /// Output produced eagerly by the baseline MCU-row streaming pipeline
    /// (see [`Decoder::scan_sequential_streaming`]). When present, [`finish`]
    /// returns it directly instead of running the whole-image coefficient →
    /// plane → assemble path.
    streamed: Option<DecodedImage>,
    /// Test hook: when false, the baseline streaming pipeline is disabled and
    /// even eligible scans take the coefficient path, so tests can prove the two
    /// paths produce byte-identical output.
    allow_stream: bool,
    /// `/Width` and `/Height` from the PDF image dictionary, when decoding on
    /// behalf of a PDF draw. Used only to repair a `0xFFFF` SOF height.
    expected_size: Option<(usize, usize)>,
    /// Device-space footprint of this draw, when known. A minified draw is
    /// decoded at 1/2, 1/4 or 1/8 scale instead of full size (see
    /// [`Decoder::pick_dct_size`]).
    target_size: Option<(usize, usize)>,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8], limits: &'a DecodeLimits) -> Self {
        Self {
            data,
            pos: 0,
            limits,
            quant: [None, None, None, None],
            dc_tables: [None, None, None, None],
            ac_tables: [None, None, None, None],
            frame: None,
            restart_interval: 0,
            adobe_transform: None,
            saw_jfif: false,
            scans_done: 0,
            streamed: None,
            allow_stream: true,
            expected_size: None,
            target_size: None,
        }
    }

    fn u16_at(&self, p: usize) -> Result<usize, ImageError> {
        if p + 1 >= self.data.len() {
            return Err(err("truncated segment"));
        }
        Ok(((self.data[p] as usize) << 8) | self.data[p + 1] as usize)
    }

    /// Parse markers and decode every scan until EOI or end of data.
    fn run(&mut self) -> Result<(), ImageError> {
        // Locate SOI, tolerating leading garbage.
        let soi = self
            .data
            .windows(2)
            .take(4096)
            .position(|w| w == [0xFF, 0xD8]);
        self.pos = soi.ok_or_else(|| err("no SOI marker"))? + 2;

        loop {
            // Find the next marker: skip stray non-FF bytes and FF fill bytes.
            while self.pos < self.data.len() && self.data[self.pos] != 0xFF {
                self.pos += 1;
            }
            while self.pos < self.data.len() && self.data[self.pos] == 0xFF {
                self.pos += 1;
            }
            if self.pos >= self.data.len() {
                break; // missing EOI — tolerated
            }
            let marker = self.data[self.pos];
            self.pos += 1;
            match marker {
                0xD8 => {} // stray SOI
                0xD9 => break,
                0x01 | 0xD0..=0xD7 => {} // standalone markers outside a scan
                0xC0..=0xC2 => self.parse_sof(marker == 0xC2)?,
                0xC3 | 0xC5..=0xC7 | 0xCB | 0xCD..=0xCF => {
                    return Err(err(
                        "unsupported JPEG coding process (lossless/hierarchical)",
                    ));
                }
                0xC9 | 0xCA => return Err(err("arithmetic-coded JPEG is unsupported")),
                0xC4 => self.parse_dht()?,
                0xDB => self.parse_dqt()?,
                0xDD => {
                    let len = self.u16_at(self.pos)?;
                    self.restart_interval = self.u16_at(self.pos + 2)?;
                    self.pos += len;
                }
                0xDA => self.parse_sos_and_scan()?,
                _ => {
                    // Length-prefixed segment: APPn / COM / DNL / unknown.
                    let len = self.u16_at(self.pos)?;
                    self.parse_app(marker, self.pos + 2, len.saturating_sub(2));
                    self.pos += len;
                }
            }
        }
        if self.frame.is_none() || self.scans_done == 0 {
            return Err(err("no image data (missing SOF or SOS)"));
        }
        Ok(())
    }

    fn parse_app(&mut self, marker: u8, body: usize, len: usize) {
        let start = body.min(self.data.len());
        let end = body.saturating_add(len).min(self.data.len());
        let body = &self.data[start..end];
        match marker {
            0xE0 if body.starts_with(b"JFIF\0") => self.saw_jfif = true,
            0xEE if body.starts_with(b"Adobe") && body.len() >= 12 => {
                self.adobe_transform = Some(body[11]);
            }
            _ => {}
        }
    }

    fn parse_dqt(&mut self) -> Result<(), ImageError> {
        let len = self.u16_at(self.pos)?;
        let end = self.pos + len;
        let mut p = self.pos + 2;
        while p < end.min(self.data.len()) {
            let pq_tq = self.data[p];
            let pq = pq_tq >> 4; // 0: 8-bit entries, 1: 16-bit
            let tq = (pq_tq & 15) as usize;
            if tq >= 4 {
                return Err(err("DQT table id out of range"));
            }
            p += 1;
            let mut table = [0u16; 64];
            for entry in table.iter_mut() {
                if pq == 1 {
                    *entry = self.u16_at(p)? as u16;
                    p += 2;
                } else {
                    *entry = *self.data.get(p).ok_or_else(|| err("truncated DQT"))? as u16;
                    p += 1;
                }
            }
            self.quant[tq] = Some(table);
        }
        self.pos = end;
        Ok(())
    }

    fn parse_dht(&mut self) -> Result<(), ImageError> {
        let len = self.u16_at(self.pos)?;
        let end = self.pos + len;
        let mut p = self.pos + 2;
        while p < end.min(self.data.len()) {
            let tc_th = self.data[p];
            let tc = tc_th >> 4; // 0 = DC, 1 = AC
            let th = (tc_th & 15) as usize;
            if tc > 1 || th >= 4 {
                return Err(err("DHT table selector out of range"));
            }
            p += 1;
            if p + 16 > self.data.len() {
                return Err(err("truncated DHT"));
            }
            let mut counts = [0u8; 16];
            counts.copy_from_slice(&self.data[p..p + 16]);
            p += 16;
            let total: usize = counts.iter().map(|&c| c as usize).sum();
            if p + total > self.data.len() {
                return Err(err("truncated DHT values"));
            }
            let values = self.data[p..p + total].to_vec();
            p += total;
            let table = HuffTable::build(&counts, values)?;
            if tc == 0 {
                self.dc_tables[th] = Some(table);
            } else {
                self.ac_tables[th] = Some(table);
            }
        }
        self.pos = end;
        Ok(())
    }

    fn parse_sof(&mut self, progressive: bool) -> Result<(), ImageError> {
        if self.frame.is_some() {
            return Err(err("multiple SOF frames"));
        }
        let len = self.u16_at(self.pos)?;
        let p = self.pos + 2;
        if p + 6 > self.data.len() {
            return Err(err("truncated SOF"));
        }
        let precision = self.data[p];
        if precision != 8 {
            return Err(err("only 8-bit JPEG precision is supported"));
        }
        let mut height = self.u16_at(p + 1)?;
        let width = self.u16_at(p + 3)?;
        let ncomp = self.data[p + 5] as usize;
        // `SOF` height 0xFFFF means "the real height arrives in a DNL marker
        // after the first scan" (ISO/IEC 10918-1 B.2.5). Encoders that stream
        // to an unknown length write it, and a stream truncated before the DNL
        // leaves it as the only height available — 65535 rows, of which the
        // real image occupies the top few percent and the rest decodes to flat
        // mid-grey. Take the dictionary's height instead, gated exactly as
        // PDFium's `HasKnownBadHeaderWithInvalidHeight`: the marker really is an
        // SOF, the height really is 0xFFFF, and the widths agree, so a genuine
        // 65535-row image is never rewritten.
        if let Some((expected_width, expected_height)) = self.expected_size
            && height == 0xFFFF
            && width == expected_width
            && (1..0xFFFF).contains(&expected_height)
        {
            height = expected_height;
        }
        // A `SOF` that declares *more* rows than the PDF dictionary is the same
        // defect without the 0xFFFF sentinel: pdfjs issue10989 codes a
        // 10308x6304 image but writes height 60000, so the rows past the real
        // data decode to flat mid-grey and swamp the page. The dictionary is
        // what the placement rectangle is derived from, so it bounds the image;
        // clamp to it. This is deliberately one-directional — a `SOF` *smaller*
        // than the dictionary still wins, which is the usual mismatch and the
        // case the stream is right about.
        if let Some((expected_width, expected_height)) = self.expected_size
            && width == expected_width
            && expected_height > 0
            && height > expected_height
        {
            height = expected_height;
        }
        if width == 0 || height == 0 {
            return Err(err("zero JPEG dimension"));
        }
        if !(1..=4).contains(&ncomp) {
            return Err(err("unsupported component count"));
        }
        // The pixel budget bounds the *output* raster, and a scaled decode
        // shrinks that (see `pick_dct_size`), so the full-resolution extent is
        // checked only against what even a 1/8 decode would produce. The
        // coefficient and plane costs are bounded below, once the sampling
        // factors are known.
        if (scaled(width, 1) as u64) * (scaled(height, 1) as u64) > self.limits.max_pixels {
            return Err(ImageError::TooLarge {
                width: width as u32,
                height: height as u32,
            });
        }

        let mut comps = Vec::with_capacity(ncomp);
        let mut hmax = 1usize;
        let mut vmax = 1usize;
        for i in 0..ncomp {
            let q = p + 6 + i * 3;
            if q + 2 >= self.data.len() {
                return Err(err("truncated SOF components"));
            }
            let id = self.data[q];
            let h = (self.data[q + 1] >> 4) as usize;
            let v = (self.data[q + 1] & 15) as usize;
            let tq = self.data[q + 2] as usize;
            if !(1..=4).contains(&h) || !(1..=4).contains(&v) || tq >= 4 {
                return Err(err("SOF sampling/table selector out of range"));
            }
            hmax = hmax.max(h);
            vmax = vmax.max(v);
            comps.push(Component {
                id,
                h,
                v,
                tq,
                bw: 0,
                bh: 0,
                bw_used: 0,
                bh_used: 0,
                coeffs: Vec::new(),
            });
        }

        let mcus_x = width.div_ceil(8 * hmax);
        let mcus_y = height.div_ceil(8 * vmax);
        let mut coeff_bytes = 0u64;
        for c in comps.iter_mut() {
            c.bw = mcus_x * c.h;
            c.bh = mcus_y * c.v;
            c.bw_used = (width * c.h).div_ceil(hmax).div_ceil(8);
            c.bh_used = (height * c.v).div_ceil(vmax).div_ceil(8);
            coeff_bytes += (c.bw as u64) * (c.bh as u64) * 128;
        }
        // A minified draw, or an image too large to decode whole, is decoded at
        // a reduced DCT size; that shrinks the sample planes and the output but
        // not the coefficients, which are entropy-coded at full resolution.
        let dct_size = self.pick_dct_size(width, height, ncomp, coeff_bytes);

        // Coefficients + sample planes + output, all bounded up front.
        let plane_bytes: u64 = comps
            .iter()
            .map(|c| (c.bw as u64 * dct_size as u64) * (c.bh as u64 * dct_size as u64))
            .sum();
        let out_bytes =
            scaled(width, dct_size) as u64 * scaled(height, dct_size) as u64 * ncomp as u64;
        if coeff_bytes + plane_bytes + out_bytes > self.limits.max_output_bytes {
            return Err(ImageError::TooLarge {
                width: width as u32,
                height: height as u32,
            });
        }
        // Coefficient storage is allocated lazily (see [`ensure_coeffs`]): the
        // baseline MCU-row streaming path never materializes it, saving ~30 MiB
        // of `i16` blocks plus the ~15 MiB of intermediate sample planes on a
        // typical full-page photograph.

        self.frame = Some(Frame {
            progressive,
            width,
            height,
            comps,
            hmax,
            vmax,
            mcus_x,
            mcus_y,
            dct_size,
        });
        self.pos += len;
        Ok(())
    }

    /// Samples per block edge for this frame: 8 (full size) or 4/2/1.
    ///
    /// Two independent reasons to reduce, both bounded by the same set of
    /// power-of-two DCT sizes the AAN kernel can produce exactly:
    ///
    /// * **Minification.** `target_size` is the draw's device-space footprint.
    ///   Decoding a 21770x15399 scan to fill a 4768x6741 page rectangle throws
    ///   away 90% of the samples; pick the smallest scale that still covers the
    ///   target, so the resampler downstream never sees less detail than it
    ///   would draw.
    /// * **Size.** Some scans are simply too large to hold at full resolution
    ///   (`10308x60000` is 618 Mpx). Reducing lets them render at all instead of
    ///   failing the pixel budget and leaving the page blank, which is what
    ///   PDFium does for them too.
    ///
    /// Coefficients are entropy-coded at full resolution and are not reduced by
    /// this, so `coeff_bytes` is the floor on what a decode costs.
    fn pick_dct_size(&self, width: usize, height: usize, ncomp: usize, coeff_bytes: u64) -> usize {
        let fits = |dct: usize| -> bool {
            let w = scaled(width, dct) as u64;
            let h = scaled(height, dct) as u64;
            w * h <= self.limits.max_pixels
                && coeff_bytes + w * h * (ncomp as u64 + 1) <= self.limits.max_output_bytes
        };
        // Minification: the smallest scale whose output still covers the draw.
        let mut dct = 8usize;
        if let Some((tw, th)) = self.target_size {
            while dct > 1
                && scaled(width, dct / 2) >= tw.max(1)
                && scaled(height, dct / 2) >= th.max(1)
            {
                dct /= 2;
            }
        }
        // Size: keep halving while the full-resolution decode cannot fit.
        while dct > 1 && !fits(dct) {
            dct /= 2;
        }
        dct
    }

    // --- scans -----------------------------------------------------------------

    fn parse_sos_and_scan(&mut self) -> Result<(), ImageError> {
        let frame = self.frame.as_ref().ok_or_else(|| err("SOS before SOF"))?;
        let p = self.pos + 2;
        let ns = *self.data.get(p).ok_or_else(|| err("truncated SOS"))? as usize;
        if !(1..=4).contains(&ns) || ns > frame.comps.len() {
            return Err(err("SOS component count out of range"));
        }
        let mut scan_comps = Vec::with_capacity(ns);
        for i in 0..ns {
            let q = p + 1 + i * 2;
            if q + 1 >= self.data.len() {
                return Err(err("truncated SOS components"));
            }
            let cs = self.data[q];
            let comp = frame
                .comps
                .iter()
                .position(|c| c.id == cs)
                .ok_or_else(|| err("SOS references unknown component"))?;
            let dc_tbl = (self.data[q + 1] >> 4) as usize;
            let ac_tbl = (self.data[q + 1] & 15) as usize;
            if dc_tbl >= 4 || ac_tbl >= 4 {
                return Err(err("SOS table selector out of range"));
            }
            scan_comps.push(ScanComp {
                comp,
                dc_tbl,
                ac_tbl,
            });
        }
        let q = p + 1 + ns * 2;
        if q + 2 >= self.data.len() {
            return Err(err("truncated SOS parameters"));
        }
        let ss = self.data[q] as usize;
        let se = self.data[q + 1] as usize;
        let ah = self.data[q + 2] >> 4;
        let al = self.data[q + 2] & 15;
        let entropy_start = q + 3;

        let progressive = frame.progressive;
        if progressive {
            if ss > 63 || se > 63 || ss > se || al > 13 {
                return Err(err("invalid progressive scan parameters"));
            }
            if ss == 0 && se != 0 {
                return Err(err("progressive DC scan with an AC band"));
            }
            if ss != 0 && ns != 1 {
                return Err(err("progressive AC scan must be non-interleaved"));
            }
        }

        // Scans own the frame while running (the decoder keeps the tables).
        // Checked as `Some` at fn entry; a failed take fails the decode.
        let mut frame = self.frame.take().ok_or_else(|| err("SOS before SOF"))?;
        let mut reader = BitReader::new(self.data, entropy_start);
        // Baseline / extended-sequential scans that carry the whole image in one
        // interleaved (or single-component, 1×1) pass are reconstructed one MCU
        // row at a time straight to output — no full coefficient or plane
        // buffers. Progressive scans (multiple passes revisit every block) and
        // non-interleaved sequential scans keep the coefficient path.
        let can_stream = self.allow_stream
            && !progressive
            && self.streamed.is_none()
            && ns == frame.comps.len()
            && (frame.comps.len() > 1 || (frame.comps[0].h == 1 && frame.comps[0].v == 1));
        let result = if can_stream {
            match self.scan_sequential_streaming(&frame, &scan_comps, &mut reader) {
                Ok(img) => {
                    self.streamed = Some(img);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        } else {
            ensure_coeffs(&mut frame);
            if !progressive {
                self.scan_sequential(&mut frame, &scan_comps, &mut reader)
            } else if ss == 0 {
                self.scan_progressive_dc(&mut frame, &scan_comps, &mut reader, ah, al)
            } else {
                self.scan_progressive_ac(&mut frame, scan_comps[0], &mut reader, ss, se, ah, al)
            }
        };
        self.frame = Some(frame);
        result?;
        self.scans_done += 1;
        // Continue marker parsing after the entropy-coded data.
        self.pos = reader.pos.max(entropy_start);
        Ok(())
    }

    /// Walk one scan's blocks in MCU order, calling
    /// `f(reader, Event::Block { .. })` for each block and
    /// `f(reader, Event::Restart)` after every restart-marker resync (the
    /// handler clears its DC predictors / EOB run there). Handles the
    /// interleaved (multi-component) and non-interleaved (single component,
    /// non-padded dimensions) geometries and cancellation. The geometry is
    /// snapshotted so `f` may mutate the frame freely.
    fn walk_scan_blocks(
        &self,
        geom: &ScanGeom,
        reader: &mut BitReader,
        f: &mut dyn FnMut(&mut BitReader, Event) -> Result<(), ImageError>,
    ) -> Result<(), ImageError> {
        let mut to_restart = self.restart_interval;
        for mcu in 0..geom.mcus {
            if mcu % geom.row_len == 0 && self.limits.is_cancelled() {
                return Err(ImageError::Cancelled);
            }
            if self.restart_interval > 0 {
                if to_restart == 0 {
                    reader.restart();
                    f(reader, Event::Restart)?;
                    to_restart = self.restart_interval;
                }
                to_restart -= 1;
            }
            if geom.single {
                let g = &geom.comps[0];
                let row = mcu / g.bw_used;
                let col = mcu % g.bw_used;
                f(
                    reader,
                    Event::Block {
                        si: 0,
                        ci: g.ci,
                        off: (row * g.bw + col) * 64,
                    },
                )?;
            } else {
                let mcu_x = mcu % geom.mcus_x;
                let mcu_y = mcu / geom.mcus_x;
                for (si, g) in geom.comps.iter().enumerate() {
                    for by in 0..g.v {
                        for bx in 0..g.h {
                            let row = mcu_y * g.v + by;
                            let col = mcu_x * g.h + bx;
                            f(
                                reader,
                                Event::Block {
                                    si,
                                    ci: g.ci,
                                    off: (row * g.bw + col) * 64,
                                },
                            )?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Baseline / extended sequential scan: full DC+AC per block.
    fn scan_sequential(
        &self,
        frame: &mut Frame,
        scan_comps: &[ScanComp],
        reader: &mut BitReader,
    ) -> Result<(), ImageError> {
        // Resolve every scan component's tables up front: the loop below then
        // needs no per-block `Option` re-proof (the checked branch is here).
        let mut tables: Vec<(&HuffTable, &HuffTable)> = Vec::with_capacity(scan_comps.len());
        for sc in scan_comps {
            match (
                self.dc_tables[sc.dc_tbl].as_ref(),
                self.ac_tables[sc.ac_tbl].as_ref(),
            ) {
                (Some(dc), Some(ac)) => tables.push((dc, ac)),
                _ => return Err(err("scan references a missing Huffman table")),
            }
        }
        let geom = ScanGeom::of(frame, scan_comps);
        let comps = &mut frame.comps;
        let mut preds = [0i32; 4];
        self.walk_scan_blocks(&geom, reader, &mut |reader, event| {
            let (si, ci, off) = match event {
                Event::Restart => {
                    preds = [0; 4];
                    return Ok(());
                }
                Event::Block { si, ci, off } => (si, ci, off),
            };
            let (dc, ac) = tables[si];
            let coeffs = &mut comps[ci].coeffs;
            if off + 64 > coeffs.len() {
                return Ok(());
            }
            decode_block_sequential(reader, dc, ac, &mut preds[si], &mut coeffs[off..off + 64]);
            Ok(())
        })
    }

    /// Baseline / extended-sequential MCU-row streaming pipeline.
    ///
    /// For a single interleaved (or single-component 1×1) sequential scan the
    /// whole image is one pass over the entropy data, so we never need to keep
    /// every block's coefficients. Instead, for each MCU row we entropy-decode
    /// and IDCT that row's blocks into a small per-component sample *band* (only
    /// `v·8` rows tall), then upsample + colour-convert + emit the output rows
    /// immediately. Peak memory drops from `coefficients + planes + output` to
    /// `bands + output`. Restart markers resync exactly as in the coefficient
    /// path. Produces byte-for-byte the same output as the coefficient path.
    fn scan_sequential_streaming(
        &self,
        frame: &Frame,
        scan_comps: &[ScanComp],
        reader: &mut BitReader,
    ) -> Result<DecodedImage, ImageError> {
        // Resolve every scan component's tables up front (the checked branch
        // that proves the per-MCU lookups below).
        let mut tables: Vec<(&HuffTable, &HuffTable)> = Vec::with_capacity(scan_comps.len());
        for sc in scan_comps {
            match (
                self.dc_tables[sc.dc_tbl].as_ref(),
                self.ac_tables[sc.ac_tbl].as_ref(),
            ) {
                (Some(dc), Some(ac)) => tables.push((dc, ac)),
                _ => return Err(err("scan references a missing Huffman table")),
            }
        }

        let (w, h) = (frame.out_w(), frame.out_h());
        let dct = frame.dct_size;
        let (mcus_x, mcus_y) = (frame.mcus_x, frame.mcus_y);

        // Per-scan-component dequantization tables (natural order, AAN scale and
        // the 1/8 descale folded in — identical to the coefficient path).
        let mut dqs: Vec<[f32; 64]> = Vec::with_capacity(scan_comps.len());
        for sc in scan_comps {
            let c = &frame.comps[sc.comp];
            let raw = self.quant[c.tq].ok_or_else(|| err("missing quantization table"))?;
            let mut dq = [0f32; 64];
            for zig in 0..64 {
                let pos = ZIGZAG_INV[zig] as usize;
                dq[pos] =
                    raw[zig] as f32 * AAN_SCALE_FACTORS[pos / 8] * AAN_SCALE_FACTORS[pos % 8] / 8.0;
            }
            dqs.push(dq);
        }

        // Two reusable sample bands per scan component: one being decoded and
        // one awaiting emission. Fancy 4:2:0 upsampling needs the first source
        // row of the following MCU band for its last output row, so emission is
        // delayed by one band. This is still O(one MCU row), not O(image).
        let mut bands: Vec<Vec<u8>> = scan_comps
            .iter()
            .map(|sc| {
                let c = &frame.comps[sc.comp];
                vec![0u8; (c.v * dct) * (c.bw * dct)]
            })
            .collect();
        let mut previous_bands: Vec<Vec<u8>> =
            bands.iter().map(|band| vec![0u8; band.len()]).collect();
        let mut above_tail: Vec<Vec<u8>> = scan_comps
            .iter()
            .map(|sc| {
                let c = &frame.comps[sc.comp];
                vec![0u8; c.bw * dct]
            })
            .collect();

        let (kind, format, bpp) = output_layout(frame, self.adobe_transform, self.saw_jfif);
        let modes = component_upsampling(frame);
        let stride = w * bpp;
        let mut out = zeroed_arc(stride * h);
        let ncomp = scan_comps.len();
        let mut sampled = vec![vec![0u8; w]; ncomp];

        let mut preds = [0i32; 4];
        let mut to_restart = self.restart_interval;
        let mut scratch = [0i16; 64];
        let mut fbuf = [0f32; 64];

        for my in 0..mcus_y {
            if self.limits.is_cancelled() {
                return Err(ImageError::Cancelled);
            }
            for mx in 0..mcus_x {
                if self.restart_interval > 0 {
                    if to_restart == 0 {
                        reader.restart();
                        preds = [0; 4];
                        to_restart = self.restart_interval;
                    }
                    to_restart -= 1;
                }
                for (si, sc) in scan_comps.iter().enumerate() {
                    let c = &frame.comps[sc.comp];
                    let (dc, ac) = tables[si];
                    let dq = &dqs[si];
                    let band = &mut bands[si];
                    let bstride = c.bw * dct;
                    for by in 0..c.v {
                        for bx in 0..c.h {
                            scratch.fill(0);
                            let any_ac = decode_block_sequential(
                                reader,
                                dc,
                                ac,
                                &mut preds[si],
                                &mut scratch,
                            );
                            let base = (by * dct) * bstride + (mx * c.h + bx) * dct;
                            if any_ac {
                                if dct == 8 {
                                    idct_block(&scratch, dq, &mut fbuf);
                                    store_band_block(&fbuf, band, base, bstride);
                                } else if dct == 1 {
                                    // The block mean is the DC term; no IDCT.
                                    band[base] =
                                        (scratch[0] as f32 * dq[0] + 128.5).clamp(0.0, 255.0) as u8;
                                } else {
                                    idct_block(&scratch, dq, &mut fbuf);
                                    let mut small = [0f32; 64];
                                    reduce_block(&fbuf, dct, &mut small);
                                    store_band_block_scaled(&small, dct, band, base, bstride);
                                }
                            } else {
                                // DC-only block: the IDCT is the same constant at
                                // every sample (= coeffs[0]·dequant[0]); fill the
                                // 8×8 band region directly, skipping IDCT + store.
                                let byte =
                                    (scratch[0] as f32 * dq[0] + 128.5).clamp(0.0, 255.0) as u8;
                                for yy in 0..dct {
                                    band[base + yy * bstride..base + yy * bstride + dct].fill(byte);
                                }
                            }
                        }
                    }
                }
            }

            if my == 0 {
                std::mem::swap(&mut previous_bands, &mut bands);
                continue;
            }

            // The next band is now available as lower context, so the
            // preceding one can be upsampled without replicating its boundary.
            emit_stream_band(
                frame,
                scan_comps,
                my - 1,
                &previous_bands,
                &above_tail,
                Some(&bands),
                &modes,
                &mut sampled,
                kind,
                unique_arc_mut(&mut out),
                stride,
            );
            for si in 0..ncomp {
                let c = &frame.comps[scan_comps[si].comp];
                let bstride = c.bw * dct;
                let start = previous_bands[si].len() - bstride;
                above_tail[si].copy_from_slice(&previous_bands[si][start..]);
            }
            std::mem::swap(&mut previous_bands, &mut bands);
        }

        // The final image edge has no lower context; libjpeg replicates its
        // last real component row.
        emit_stream_band(
            frame,
            scan_comps,
            mcus_y - 1,
            &previous_bands,
            &above_tail,
            None,
            &modes,
            &mut sampled,
            kind,
            unique_arc_mut(&mut out),
            stride,
        );

        Ok(image(w, h, format, stride, out))
    }

    /// Progressive DC scan: first pass (`Ah == 0`) decodes `diff << Al`;
    /// refinement passes contribute one bit per block.
    fn scan_progressive_dc(
        &self,
        frame: &mut Frame,
        scan_comps: &[ScanComp],
        reader: &mut BitReader,
        ah: u8,
        al: u8,
    ) -> Result<(), ImageError> {
        if ah == 0 {
            for sc in scan_comps {
                if self.dc_tables[sc.dc_tbl].is_none() {
                    return Err(err("scan references a missing DC table"));
                }
            }
        }
        // First-pass (`Ah == 0`) DC tables resolved up front (the checked
        // branch above); refinement passes read raw bits and use none.
        let tables: Vec<Option<&HuffTable>> = scan_comps
            .iter()
            .map(|sc| self.dc_tables[sc.dc_tbl].as_ref())
            .collect();
        let geom = ScanGeom::of(frame, scan_comps);
        let comps = &mut frame.comps;
        let mut preds = [0i32; 4];
        self.walk_scan_blocks(&geom, reader, &mut |reader, event| {
            let (si, ci, off) = match event {
                Event::Restart => {
                    preds = [0; 4];
                    return Ok(());
                }
                Event::Block { si, ci, off } => (si, ci, off),
            };
            let coeffs = &mut comps[ci].coeffs;
            if off >= coeffs.len() {
                return Ok(());
            }
            if ah == 0 {
                let Some(dc) = tables[si] else {
                    return Ok(()); // unreachable: verified above for Ah == 0
                };
                let t = reader.decode(dc).min(16);
                let diff = extend(reader.read_bits(t), t);
                preds[si] += diff;
                coeffs[off] = (preds[si] << al) as i16;
            } else if reader.read_bit() != 0 {
                coeffs[off] |= 1i16 << al;
            }
            Ok(())
        })
    }

    /// Progressive AC scan (always single-component): spectral band
    /// `Ss..=Se`, first pass or refinement, with EOB runs.
    #[allow(clippy::too_many_arguments)]
    fn scan_progressive_ac(
        &self,
        frame: &mut Frame,
        sc: ScanComp,
        reader: &mut BitReader,
        ss: usize,
        se: usize,
        ah: u8,
        al: u8,
    ) -> Result<(), ImageError> {
        let ac = self.ac_tables[sc.ac_tbl]
            .as_ref()
            .ok_or_else(|| err("missing AC table"))?;
        let c = &mut frame.comps[sc.comp];
        let (bw, bw_used, bh_used) = (c.bw, c.bw_used.max(1), c.bh_used);
        let coeffs = &mut c.coeffs;
        let mut eob_run: u32 = 0;
        let mut to_restart = self.restart_interval;

        for b in 0..bw_used * bh_used {
            if b % bw_used == 0 && self.limits.is_cancelled() {
                return Err(ImageError::Cancelled);
            }
            if self.restart_interval > 0 {
                if to_restart == 0 {
                    reader.restart();
                    eob_run = 0;
                    to_restart = self.restart_interval;
                }
                to_restart -= 1;
            }
            let row = b / bw_used;
            let col = b % bw_used;
            let off = (row * bw + col) * 64;
            if off + 64 > coeffs.len() {
                break;
            }
            let block = &mut coeffs[off..off + 64];

            if ah == 0 {
                // AC first scan (T.81 G.1.2.2).
                if eob_run > 0 {
                    eob_run -= 1;
                    continue;
                }
                let mut k = ss;
                while k <= se {
                    let rs = reader.decode(ac);
                    let r = (rs >> 4) as usize;
                    let s = rs & 15;
                    if s == 0 {
                        if r < 15 {
                            eob_run = (1u32 << r) - 1;
                            if r > 0 {
                                eob_run += reader.read_bits(r as u8);
                            }
                            break;
                        }
                        k += 16; // ZRL
                        continue;
                    }
                    k += r;
                    if k > se {
                        break; // corrupt run — tolerate
                    }
                    block[ZIGZAG_INV[k] as usize] = (extend(reader.read_bits(s), s) << al) as i16;
                    k += 1;
                }
            } else {
                // AC refinement (T.81 G.1.2.3; the libjpeg
                // decode_mcu_AC_refine structure).
                let p1 = 1i16 << al;
                let m1 = -1i16 << al;
                let mut k = ss;
                if eob_run == 0 {
                    while k <= se {
                        let rs = reader.decode(ac);
                        let mut r = (rs >> 4) as i32;
                        let s = rs & 15;
                        let mut new_val = 0i16;
                        if s == 0 {
                            if r < 15 {
                                eob_run = 1u32 << r;
                                if r > 0 {
                                    eob_run += reader.read_bits(r as u8);
                                }
                                break;
                            }
                            // r == 15: pass over 16 zero-history coefficients.
                        } else {
                            // In a refinement scan the magnitude is one bit.
                            new_val = if reader.read_bit() != 0 { p1 } else { m1 };
                        }
                        while k <= se {
                            let coef = &mut block[ZIGZAG_INV[k] as usize];
                            if *coef != 0 {
                                if reader.read_bit() != 0 && (*coef & p1) == 0 {
                                    *coef += if *coef >= 0 { p1 } else { m1 };
                                }
                            } else {
                                if r == 0 {
                                    if s != 0 {
                                        *coef = new_val;
                                    }
                                    k += 1;
                                    break;
                                }
                                r -= 1;
                            }
                            k += 1;
                        }
                    }
                }
                if eob_run > 0 {
                    // Correction bits for the rest of the band.
                    while k <= se {
                        let coef = &mut block[ZIGZAG_INV[k] as usize];
                        if *coef != 0 && reader.read_bit() != 0 && (*coef & p1) == 0 {
                            *coef += if *coef >= 0 { p1 } else { m1 };
                        }
                        k += 1;
                    }
                    eob_run -= 1;
                }
            }
        }
        Ok(())
    }

    // --- reconstruction ---------------------------------------------------------

    /// IDCT every component, upsample, color-convert, assemble the output.
    ///
    /// When the baseline MCU-row streaming pipeline already produced the output
    /// (the common case for a single-scan sequential JPEG), return it directly.
    fn finish(mut self) -> Result<DecodedImage, ImageError> {
        if let Some(img) = self.streamed.take() {
            return Ok(img);
        }
        let frame = self.frame.take().ok_or_else(|| err("no frame"))?;
        let ncomp = frame.comps.len();
        // Per-component dequantization tables in natural order, with the AAN
        // scale factors and the 1/8 descale folded in (the exact reciprocal
        // of the encoder's forward scale).
        let mut planes: Vec<Vec<u8>> = Vec::with_capacity(ncomp);
        for c in &frame.comps {
            let raw = self.quant[c.tq].ok_or_else(|| err("missing quantization table"))?;
            let mut dq = [0f32; 64];
            for zig in 0..64 {
                let pos = ZIGZAG_INV[zig] as usize;
                dq[pos] =
                    raw[zig] as f32 * AAN_SCALE_FACTORS[pos / 8] * AAN_SCALE_FACTORS[pos % 8] / 8.0;
            }
            let dct = frame.dct_size;
            let pw = c.bw * dct;
            let ph = c.bh * dct;
            let mut plane = vec![0u8; pw * ph];
            let mut block = [0f32; 64];
            for brow in 0..c.bh {
                if self.limits.is_cancelled() {
                    return Err(ImageError::Cancelled);
                }
                for bcol in 0..c.bw {
                    let off = (brow * c.bw + bcol) * 64;
                    // `coeffs` is allocated as `bw * bh * 64`, so the block is
                    // always present; take it as a fixed-size reference so the
                    // AVX2 kernel's 64-element read is guaranteed by the type.
                    let Some(coeff_block) = c.coeffs.get(off..).and_then(<[i16]>::first_chunk)
                    else {
                        continue;
                    };
                    idct_block(coeff_block, &dq, &mut block);
                    let base = brow * dct * pw + bcol * dct;
                    if dct == 8 {
                        for (y, row) in block.chunks_exact(8).enumerate() {
                            let dst = &mut plane[base + y * pw..base + y * pw + 8];
                            for (d, &v) in dst.iter_mut().zip(row) {
                                *d = (v + 128.5).clamp(0.0, 255.0) as u8;
                            }
                        }
                    } else {
                        let mut small = [0f32; 64];
                        reduce_block(&block, dct, &mut small);
                        store_band_block_scaled(&small, dct, &mut plane, base, pw);
                    }
                }
            }
            planes.push(plane);
        }

        assemble(&frame, &planes, self.adobe_transform, self.saw_jfif)
    }
}

// --- IDCT ----------------------------------------------------------------------

/// Inverse AAN DCT of one block: `coeffs` (natural order) × `dequant` →
/// spatial samples (level shift applied by the caller). The mirror of the
/// encoder's forward `dct`; float structure per the classic jidctflt layout.
///
/// Dispatches to an AVX2 kernel when the CPU supports it (the 8 columns / rows
/// become the 8 SIMD lanes, running the identical scalar op sequence per lane,
/// so the result is bit-for-bit identical); otherwise the scalar reference.
// The SIMD dispatch and AVX2 kernels below require `unsafe` for the target-
// feature intrinsics; the crate otherwise warns on `unsafe_code`.
#[allow(unsafe_code)]
#[inline]
fn idct_block(coeffs: &[i16; 64], dequant: &[f32; 64], out: &mut [f32; 64]) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: guarded by the runtime AVX2 feature probe.
            unsafe { idct_block_avx2(coeffs, dequant, out) };
            return;
        }
    }
    idct_block_scalar(coeffs, dequant, out);
}

/// Scalar reference IDCT (also the fallback when AVX2 is unavailable).
fn idct_block_scalar(coeffs: &[i16; 64], dequant: &[f32; 64], out: &mut [f32; 64]) {
    let mut ws = [0f32; 64];

    // Pass 1: columns. All-zero AC columns collapse to a constant.
    for col in 0..8 {
        let mut any_ac = false;
        for row in 1..8 {
            if coeffs[row * 8 + col] != 0 {
                any_ac = true;
                break;
            }
        }
        if !any_ac {
            let dc = coeffs[col] as f32 * dequant[col];
            for row in 0..8 {
                ws[row * 8 + col] = dc;
            }
            continue;
        }
        let at = |row: usize| coeffs[row * 8 + col] as f32 * dequant[row * 8 + col];
        let o = idct_1d([at(0), at(1), at(2), at(3), at(4), at(5), at(6), at(7)]);
        for (row, v) in o.into_iter().enumerate() {
            ws[row * 8 + col] = v;
        }
    }

    // Pass 2: rows.
    for row in 0..8 {
        let r = &ws[row * 8..row * 8 + 8];
        let o = idct_1d([r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]]);
        out[row * 8..row * 8 + 8].copy_from_slice(&o);
    }
}

/// One 8-point inverse AAN pass.
#[inline]
fn idct_1d(i: [f32; 8]) -> [f32; 8] {
    // Even part.
    let tmp10 = i[0] + i[4];
    let tmp11 = i[0] - i[4];
    let tmp13 = i[2] + i[6];
    let tmp12 = (i[2] - i[6]) * 1.414213562 - tmp13;

    let tmp0 = tmp10 + tmp13;
    let tmp3 = tmp10 - tmp13;
    let tmp1 = tmp11 + tmp12;
    let tmp2 = tmp11 - tmp12;

    // Odd part.
    let z13 = i[5] + i[3];
    let z10 = i[5] - i[3];
    let z11 = i[1] + i[7];
    let z12 = i[1] - i[7];

    let tmp7 = z11 + z13;
    let tmp11o = (z11 - z13) * 1.414213562;

    let z5 = (z10 + z12) * 1.847759065;
    let tmp10o = 1.082392200 * z12 - z5;
    let tmp12o = -2.613125930 * z10 + z5;

    let tmp6 = tmp12o - tmp7;
    let tmp5 = tmp11o - tmp6;
    let tmp4 = tmp10o + tmp5;

    [
        tmp0 + tmp7,
        tmp1 + tmp6,
        tmp2 + tmp5,
        tmp3 - tmp4,
        tmp3 + tmp4,
        tmp2 - tmp5,
        tmp1 - tmp6,
        tmp0 - tmp7,
    ]
}

// --- SIMD IDCT (AVX2) ----------------------------------------------------------
//
// The 8 columns (then, after a transpose, the 8 rows) become the 8 lanes of a
// `__m256`. Each lane runs the *exact* scalar `idct_1d` op sequence — same
// multiply/add/subtract order, no fused-multiply-add contraction — so every
// lane is bit-for-bit identical to `idct_block_scalar`. The transposes are pure
// data movement. Verified byte-exact against the scalar path by
// `idct_tests::avx2_matches_scalar_bit_exact`.

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

/// One 8-point inverse AAN pass across 8 lanes (mirrors [`idct_1d`]).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(unsafe_code)]
#[inline]
unsafe fn idct_1d_avx2(i: [__m256; 8]) -> [__m256; 8] {
    let c1414 = _mm256_set1_ps(1.414213562);
    let c1847 = _mm256_set1_ps(1.847759065);
    let c1082 = _mm256_set1_ps(1.082392200);
    let c2613 = _mm256_set1_ps(-2.613125930);

    // Even part.
    let tmp10 = _mm256_add_ps(i[0], i[4]);
    let tmp11 = _mm256_sub_ps(i[0], i[4]);
    let tmp13 = _mm256_add_ps(i[2], i[6]);
    let tmp12 = _mm256_sub_ps(_mm256_mul_ps(_mm256_sub_ps(i[2], i[6]), c1414), tmp13);

    let tmp0 = _mm256_add_ps(tmp10, tmp13);
    let tmp3 = _mm256_sub_ps(tmp10, tmp13);
    let tmp1 = _mm256_add_ps(tmp11, tmp12);
    let tmp2 = _mm256_sub_ps(tmp11, tmp12);

    // Odd part.
    let z13 = _mm256_add_ps(i[5], i[3]);
    let z10 = _mm256_sub_ps(i[5], i[3]);
    let z11 = _mm256_add_ps(i[1], i[7]);
    let z12 = _mm256_sub_ps(i[1], i[7]);

    let tmp7 = _mm256_add_ps(z11, z13);
    let tmp11o = _mm256_mul_ps(_mm256_sub_ps(z11, z13), c1414);

    let z5 = _mm256_mul_ps(_mm256_add_ps(z10, z12), c1847);
    let tmp10o = _mm256_sub_ps(_mm256_mul_ps(c1082, z12), z5);
    let tmp12o = _mm256_add_ps(_mm256_mul_ps(c2613, z10), z5);

    let tmp6 = _mm256_sub_ps(tmp12o, tmp7);
    let tmp5 = _mm256_sub_ps(tmp11o, tmp6);
    let tmp4 = _mm256_add_ps(tmp10o, tmp5);

    [
        _mm256_add_ps(tmp0, tmp7),
        _mm256_add_ps(tmp1, tmp6),
        _mm256_add_ps(tmp2, tmp5),
        _mm256_sub_ps(tmp3, tmp4),
        _mm256_add_ps(tmp3, tmp4),
        _mm256_sub_ps(tmp2, tmp5),
        _mm256_sub_ps(tmp1, tmp6),
        _mm256_sub_ps(tmp0, tmp7),
    ]
}

/// Transpose an 8×8 `f32` matrix held as 8 row vectors (pure data movement).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(unsafe_code)]
#[inline]
unsafe fn transpose8_avx2(r: [__m256; 8]) -> [__m256; 8] {
    let t0 = _mm256_unpacklo_ps(r[0], r[1]);
    let t1 = _mm256_unpackhi_ps(r[0], r[1]);
    let t2 = _mm256_unpacklo_ps(r[2], r[3]);
    let t3 = _mm256_unpackhi_ps(r[2], r[3]);
    let t4 = _mm256_unpacklo_ps(r[4], r[5]);
    let t5 = _mm256_unpackhi_ps(r[4], r[5]);
    let t6 = _mm256_unpacklo_ps(r[6], r[7]);
    let t7 = _mm256_unpackhi_ps(r[6], r[7]);

    let s0 = _mm256_shuffle_ps::<0x44>(t0, t2);
    let s1 = _mm256_shuffle_ps::<0xEE>(t0, t2);
    let s2 = _mm256_shuffle_ps::<0x44>(t1, t3);
    let s3 = _mm256_shuffle_ps::<0xEE>(t1, t3);
    let s4 = _mm256_shuffle_ps::<0x44>(t4, t6);
    let s5 = _mm256_shuffle_ps::<0xEE>(t4, t6);
    let s6 = _mm256_shuffle_ps::<0x44>(t5, t7);
    let s7 = _mm256_shuffle_ps::<0xEE>(t5, t7);

    [
        _mm256_permute2f128_ps::<0x20>(s0, s4),
        _mm256_permute2f128_ps::<0x20>(s1, s5),
        _mm256_permute2f128_ps::<0x20>(s2, s6),
        _mm256_permute2f128_ps::<0x20>(s3, s7),
        _mm256_permute2f128_ps::<0x31>(s0, s4),
        _mm256_permute2f128_ps::<0x31>(s1, s5),
        _mm256_permute2f128_ps::<0x31>(s2, s6),
        _mm256_permute2f128_ps::<0x31>(s3, s7),
    ]
}

/// AVX2 IDCT: column pass (lane = column), transpose, row pass (lane = row),
/// transpose back. Bit-identical to [`idct_block_scalar`].
///
/// # Safety
/// The CPU must support AVX2 (the caller's runtime probe establishes this).
/// The 64-coefficient block is read through raw pointers, so `coeffs` is
/// `&[i16; 64]` rather than a slice: the length precondition is discharged by
/// the type and cannot be violated by a caller.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(unsafe_code)]
unsafe fn idct_block_avx2(coeffs: &[i16; 64], dequant: &[f32; 64], out: &mut [f32; 64]) {
    unsafe {
        // Load and dequantize each of the 8 rows (lane = column).
        let mut reg = [_mm256_setzero_ps(); 8];
        for r in 0..8 {
            // 8 × i16 → i32 → f32, exact.
            let raw = _mm_loadu_si128(coeffs.as_ptr().add(r * 8) as *const __m128i);
            let asf = _mm256_cvtepi32_ps(_mm256_cvtepi16_epi32(raw));
            let dq = _mm256_loadu_ps(dequant.as_ptr().add(r * 8));
            reg[r] = _mm256_mul_ps(asf, dq);
        }

        // Column pass, then transpose so lane = row for the row pass.
        let col = idct_1d_avx2(reg);
        let colt = transpose8_avx2(col);
        let row = idct_1d_avx2(colt);
        let rowt = transpose8_avx2(row);

        for r in 0..8 {
            _mm256_storeu_ps(out.as_mut_ptr().add(r * 8), rowt[r]);
        }
    }
}

// --- band store (IDCT samples → level-shifted bytes) ---------------------------

/// Store one 8×8 IDCT sample block into the streaming sample band as
/// level-shifted, clamped bytes (`(v + 128.5).clamp(0, 255) as u8`), writing
/// `band[base + yy·bstride .. +8]` for each of the 8 rows.
///
/// Dispatches to an AVX2 kernel when available (bit-identical to the scalar
/// path: add 128.5, clamp to `[0, 255]`, truncate toward zero — exactly what
/// the scalar `.clamp(0.0, 255.0) as u8` does over the already-non-negative,
/// already-≤255 clamped value). Scalar reference retained as the fallback.
#[allow(unsafe_code)]
#[inline]
fn store_band_block(fbuf: &[f32; 64], band: &mut [u8], base: usize, bstride: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: guarded by the runtime AVX2 feature probe.
            unsafe { store_band_block_avx2(fbuf, band, base, bstride) };
            return;
        }
    }
    store_band_block_scalar(fbuf, band, base, bstride);
}

/// Reduce a full 8x8 IDCT result to `dct_size` x `dct_size` samples by
/// averaging each `(8/dct_size)` square.
///
/// The DCT is orthogonal, so the mean of a sub-square is exactly the value a
/// correctly band-limited reduced IDCT produces — this is an area average, not
/// point sampling, so no aliasing is introduced. `dct_size == 8` is the
/// identity and never reaches here; `dct_size == 1` is handled by the caller
/// straight from the DC term, which *is* the block mean.
fn reduce_block(fbuf: &[f32; 64], dct_size: usize, out: &mut [f32; 64]) {
    let step = 8 / dct_size;
    let inv = 1.0 / (step * step) as f32;
    for oy in 0..dct_size {
        for ox in 0..dct_size {
            let mut acc = 0.0f32;
            for yy in 0..step {
                let row = (oy * step + yy) * 8 + ox * step;
                for xx in 0..step {
                    acc += fbuf[row + xx];
                }
            }
            out[oy * dct_size + ox] = acc * inv;
        }
    }
}

/// Store a `dct_size` x `dct_size` reduced block into a sample band.
fn store_band_block_scaled(
    fbuf: &[f32; 64],
    dct_size: usize,
    band: &mut [u8],
    base: usize,
    bstride: usize,
) {
    for yy in 0..dct_size {
        let row = base + yy * bstride;
        for xx in 0..dct_size {
            band[row + xx] = (fbuf[yy * dct_size + xx] + 128.5).clamp(0.0, 255.0) as u8;
        }
    }
}

/// Scalar reference band store (also the fallback when AVX2 is unavailable).
fn store_band_block_scalar(fbuf: &[f32; 64], band: &mut [u8], base: usize, bstride: usize) {
    for yy in 0..8 {
        let row = &mut band[base + yy * bstride..base + yy * bstride + 8];
        for (d, &v) in row.iter_mut().zip(&fbuf[yy * 8..yy * 8 + 8]) {
            *d = (v + 128.5).clamp(0.0, 255.0) as u8;
        }
    }
}

/// AVX2 band store: 8 lanes = one row of 8 samples. Bit-identical to
/// [`store_band_block_scalar`].
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(unsafe_code)]
unsafe fn store_band_block_avx2(fbuf: &[f32; 64], band: &mut [u8], base: usize, bstride: usize) {
    unsafe {
        let bias = _mm256_set1_ps(128.5);
        let lo = _mm256_setzero_ps();
        let hi = _mm256_set1_ps(255.0);
        // Gather byte 0 of each 32-bit lane; the clamped values are in 0..=255
        // so the low byte is the whole value. `-1` lanes are zeroed (unused).
        #[rustfmt::skip]
        let shuf = _mm256_setr_epi8(
            0, 4, 8, 12, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
            0, 4, 8, 12, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
        );
        for yy in 0..8 {
            let v = _mm256_loadu_ps(fbuf.as_ptr().add(yy * 8));
            let v = _mm256_add_ps(v, bias);
            let v = _mm256_min_ps(_mm256_max_ps(v, lo), hi);
            let iv = _mm256_cvttps_epi32(v);
            let bytes = _mm256_shuffle_epi8(iv, shuf);
            let lo4 = _mm_cvtsi128_si32(_mm256_castsi256_si128(bytes)) as u32;
            let hi4 = _mm_cvtsi128_si32(_mm256_extracti128_si256::<1>(bytes)) as u32;
            let dst = base + yy * bstride;
            band[dst..dst + 4].copy_from_slice(&lo4.to_le_bytes());
            band[dst + 4..dst + 8].copy_from_slice(&hi4.to_le_bytes());
        }
    }
}

/// Gather byte 0 of each of the 8 i32 lanes (values in `0..=255`) into 8
/// contiguous bytes, returned little-endian in a `u64` (`byte i` = `lane i`).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(unsafe_code)]
#[inline]
unsafe fn pack_low_bytes_avx2(v: __m256i) -> u64 {
    // No `unsafe` block: every intrinsic below is a safe `target_feature` fn
    // callable from this equally-`target_feature`d function.
    #[rustfmt::skip]
    let shuf = _mm256_setr_epi8(
        0, 4, 8, 12, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
        0, 4, 8, 12, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
    );
    let bytes = _mm256_shuffle_epi8(v, shuf);
    let lo4 = _mm_cvtsi128_si32(_mm256_castsi256_si128(bytes)) as u32 as u64;
    let hi4 = _mm_cvtsi128_si32(_mm256_extracti128_si256::<1>(bytes)) as u32 as u64;
    lo4 | (hi4 << 32)
}

/// Vectorized YCbCr→RGB for one row, for the SIMD-eligible layouts
/// ([`ChromaX`]): luma 1:1, chroma 1:1 or 2:1 horizontally. Processes whole
/// 8-pixel groups and returns the number of output columns written (a multiple
/// of 8); the caller finishes the tail with the scalar path.
///
/// Each channel is recomputed arithmetically (the scalar path's 64 KiB green
/// table cannot be gathered lane-wise). The float sub-expressions use the exact
/// operation order and constants of [`ycc_tables`] with no FMA contraction, and
/// the integer add/level-shift/clamp mirror [`ycc_to_rgb`] — including its two
/// saturated-edge `−1` green corrections — so the result is byte-for-byte
/// identical to the scalar table path. Proven exhaustively over all
/// 16,777,216 `(Y, Cb, Cr)` triples by `rgb_simd_tests`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(unsafe_code)]
unsafe fn rgb_ycc_row_avx2(
    cx: ChromaX,
    out_row: &mut [u8],
    w: usize,
    y_row: &[u8],
    cb_row: &[u8],
    cr_row: &[u8],
) -> usize {
    unsafe {
        // Largest multiple of 8 whose every load stays in bounds.
        let mut lim = w & !7;
        while lim >= 8 {
            let last = lim - 8;
            let y_ok = last + 8 <= y_row.len();
            let c_ok = match cx {
                ChromaX::Identity => last + 8 <= cb_row.len() && last + 8 <= cr_row.len(),
                ChromaX::Half => last / 2 + 4 <= cb_row.len() && last / 2 + 4 <= cr_row.len(),
            };
            if y_ok && c_ok {
                break;
            }
            lim -= 8;
        }
        if lim == 0 {
            return 0;
        }

        let c1402 = _mm256_set1_ps(1.402);
        let c1772 = _mm256_set1_ps(1.772);
        let c0344 = _mm256_set1_ps(0.344136);
        let c0714 = _mm256_set1_ps(0.714136);
        let half = _mm256_set1_ps(0.5);
        let c128f = _mm256_set1_ps(128.0);
        let c128i = _mm256_set1_epi32(128);
        let zero = _mm256_setzero_si256();
        let c255 = _mm256_set1_epi32(255);
        let one = _mm256_set1_epi32(1);
        // Byte-duplication shuffle for 2:1 chroma: [c0,c0,c1,c1,c2,c2,c3,c3].
        #[rustfmt::skip]
        let dup_shuf = _mm_setr_epi8(0, 0, 1, 1, 2, 2, 3, 3, -1, -1, -1, -1, -1, -1, -1, -1);

        let mut x = 0;
        while x < lim {
            let y = _mm256_cvtepu8_epi32(_mm_loadl_epi64(y_row.as_ptr().add(x) as *const __m128i));
            let (cb, cr) = match cx {
                ChromaX::Identity => (
                    _mm256_cvtepu8_epi32(_mm_loadl_epi64(cb_row.as_ptr().add(x) as *const __m128i)),
                    _mm256_cvtepu8_epi32(_mm_loadl_epi64(cr_row.as_ptr().add(x) as *const __m128i)),
                ),
                ChromaX::Half => {
                    let xc = x / 2;
                    let cbraw = _mm_cvtsi32_si128(
                        (cb_row.as_ptr().add(xc) as *const u32).read_unaligned() as i32,
                    );
                    let crraw = _mm_cvtsi32_si128(
                        (cr_row.as_ptr().add(xc) as *const u32).read_unaligned() as i32,
                    );
                    (
                        _mm256_cvtepu8_epi32(_mm_shuffle_epi8(cbraw, dup_shuf)),
                        _mm256_cvtepu8_epi32(_mm_shuffle_epi8(crraw, dup_shuf)),
                    )
                }
            };

            let cbf = _mm256_sub_ps(_mm256_cvtepi32_ps(cb), c128f);
            let crf = _mm256_sub_ps(_mm256_cvtepi32_ps(cr), c128f);

            // R = Y + floor(1.402·crf + 0.5); B = Y + floor(1.772·cbf + 0.5).
            let rc = _mm256_cvttps_epi32(_mm256_floor_ps(_mm256_add_ps(
                _mm256_mul_ps(c1402, crf),
                half,
            )));
            let bc = _mm256_cvttps_epi32(_mm256_floor_ps(_mm256_add_ps(
                _mm256_mul_ps(c1772, cbf),
                half,
            )));
            // G table term: floor(128 − 0.344136·cbf − 0.714136·crf + 0.5) − 128,
            // in the exact scalar op order.
            let gt2 = _mm256_sub_ps(c128f, _mm256_mul_ps(c0344, cbf));
            let gt4 = _mm256_sub_ps(gt2, _mm256_mul_ps(c0714, crf));
            let gt5 = _mm256_add_ps(gt4, half);
            let gc = _mm256_sub_epi32(_mm256_cvttps_epi32(_mm256_floor_ps(gt5)), c128i);

            let r = _mm256_add_epi32(y, rc);
            let b = _mm256_add_epi32(y, bc);
            let mut g = _mm256_add_epi32(y, gc);

            // The two saturated-edge corrections from `ycc_to_rgb`:
            //   (cb==78 && cr==178 && y>=239) || (cb==178 && cr==78 && y<=13) → g−1.
            let m1 = _mm256_and_si256(
                _mm256_and_si256(
                    _mm256_cmpeq_epi32(cb, _mm256_set1_epi32(78)),
                    _mm256_cmpeq_epi32(cr, _mm256_set1_epi32(178)),
                ),
                _mm256_cmpgt_epi32(y, _mm256_set1_epi32(238)),
            );
            let m2 = _mm256_and_si256(
                _mm256_and_si256(
                    _mm256_cmpeq_epi32(cb, _mm256_set1_epi32(178)),
                    _mm256_cmpeq_epi32(cr, _mm256_set1_epi32(78)),
                ),
                _mm256_cmpgt_epi32(_mm256_set1_epi32(14), y),
            );
            g = _mm256_sub_epi32(g, _mm256_and_si256(_mm256_or_si256(m1, m2), one));

            // Clamp each channel to [0, 255] and gather the low byte of each lane.
            let clamp = |v| _mm256_min_epi32(_mm256_max_epi32(v, zero), c255);
            let ra = pack_low_bytes_avx2(clamp(r)).to_le_bytes();
            let ga = pack_low_bytes_avx2(clamp(g)).to_le_bytes();
            let ba = pack_low_bytes_avx2(clamp(b)).to_le_bytes();

            let mut dst = x * 3;
            for i in 0..8 {
                out_row[dst] = ra[i];
                out_row[dst + 1] = ga[i];
                out_row[dst + 2] = ba[i];
                dst += 3;
            }
            x += 8;
        }
        lim
    }
}

// --- color conversion & assembly -------------------------------------------------

/// YCbCr → RGB (BT.601 full range). Optimization seam: fixed-point + SIMD.
#[inline]
fn ycc_to_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let tables = ycc_tables();
    let mut g = y as i16 + tables.g[cb as usize * 256 + cr as usize];
    // These are the only two chroma pairs where the original sequential f32
    // evaluation rounds one lower at a saturated edge. Keep the optimized
    // integer path byte-for-byte identical to it.
    if (cb == 78 && cr == 178 && y >= 239) || (cb == 178 && cr == 78 && y <= 13) {
        g -= 1;
    }
    [
        add_clamped(y, tables.r[cr as usize]),
        g.clamp(0, 255) as u8,
        add_clamped(y, tables.b[cb as usize]),
    ]
}

struct YccTables {
    r: [i16; 256],
    g: Box<[i16]>,
    b: [i16; 256],
}

fn ycc_tables() -> &'static YccTables {
    static TABLES: OnceLock<YccTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut tables = YccTables {
            r: [0; 256],
            g: vec![0; 256 * 256].into_boxed_slice(),
            b: [0; 256],
        };
        for chroma in 0..256 {
            let centered = chroma as f32 - 128.0;
            tables.r[chroma] = (1.402 * centered + 0.5).floor() as i16;
            tables.b[chroma] = (1.772 * centered + 0.5).floor() as i16;
        }
        for cb in 0..256 {
            let cbf = cb as f32 - 128.0;
            for cr in 0..256 {
                let crf = cr as f32 - 128.0;
                tables.g[cb * 256 + cr] =
                    (128.0 - 0.344136 * cbf - 0.714136 * crf + 0.5).floor() as i16 - 128;
            }
        }
        tables
    })
}

#[inline]
fn add_clamped(value: u8, delta: i16) -> u8 {
    (value as i16 + delta).clamp(0, 255) as u8
}

/// True when a 4-component frame must be treated as YCCK rather than direct
/// CMYK. libjpeg (`jdinput.c::default_decompress_parms`) selects `JCS_YCCK`
/// for a 4-component file **iff** an Adobe APP14 marker is present with a
/// non-zero transform (2 = YCCK; any other non-zero value is assumed YCCK with
/// a warning). Absent the marker, or with transform 0, it is `JCS_CMYK`.
#[inline]
fn four_comp_is_ycck(adobe: Option<u8>) -> bool {
    adobe.is_some_and(|t| t != 0)
}

/// One 4-component JPEG pixel → the *raw* libjpeg `jdcolor` CMYK bytes.
///
/// This reproduces libjpeg's decoded-scanline convention exactly, which is what
/// PDFium's DCT path (`CPDF_DIB` → `CPDF_DeviceCS::TranslateImageLine` →
/// `AdobeCMYK_to_sRGB1`) consumes:
///
/// - **YCCK** (`ycck == true`): libjpeg's `ycck_cmyk_convert` sets
///   `CMY = 255 - (YCbCr→RGB)` and passes K through unchanged.
/// - **direct CMYK** (`ycck == false`): `JCS_CMYK` passthrough — the stored
///   samples are the output.
///
/// No Adobe "inverted samples" un-inversion is applied here. Adobe/Photoshop
/// CMYK JPEGs store ink inverted (≈255 = no ink) and signal it in the PDF image
/// dictionary with `/Decode [1 0 1 0 1 0 1 0]`; the image consumer applies that
/// `/Decode` (default `[0 1 …]`) to reach true DeviceCMYK before the frozen
/// CMYK→sRGB table — exactly as PDFium does. Un-inverting here as well would
/// double-invert those files (the *Eighteen Songs of a Nomad Flute* near-black
/// regression).
#[inline]
fn cmyk_libjpeg_pixel(c0: u8, c1: u8, c2: u8, k: u8, ycck: bool) -> [u8; 4] {
    if ycck {
        let [r, g, b] = ycc_to_rgb(c0, c1, c2);
        [255 - r, 255 - g, 255 - b, k]
    } else {
        [c0, c1, c2, k]
    }
}

/// The separable upsampling policy libjpeg uses for the common integral
/// sampling layouts. The "fancy" cases are a triangle filter: each generated
/// sample is 3/4 of the nearer source sample plus 1/4 of its neighbour, with
/// alternating integer-rounding biases to avoid a systematic colour shift.
///
/// Other integral ratios retain JPEG's generic box-filter (sample replication)
/// behavior. libjpeg also deliberately uses that fallback for very narrow
/// horizontally-downsampled components (`downsampled_width <= 2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Upsample {
    Box,
    FancyH2V1,
    FancyH1V2,
    FancyH2V2,
}

impl Upsample {
    #[inline]
    fn needs_vertical_context(self) -> bool {
        matches!(self, Self::FancyH1V2 | Self::FancyH2V2)
    }
}

#[inline]
fn component_sample_width(frame: &Frame, ci: usize) -> usize {
    (frame.out_w() * frame.comps[ci].h)
        .div_ceil(frame.hmax)
        .max(1)
}

#[inline]
fn component_sample_height(frame: &Frame, ci: usize) -> usize {
    (frame.out_h() * frame.comps[ci].v)
        .div_ceil(frame.vmax)
        .max(1)
}

fn component_upsampling(frame: &Frame) -> Vec<Upsample> {
    frame
        .comps
        .iter()
        .enumerate()
        .map(|(ci, c)| {
            let h2 = c.h * 2 == frame.hmax;
            let v2 = c.v * 2 == frame.vmax;
            let h1 = c.h == frame.hmax;
            let v1 = c.v == frame.vmax;
            let wide = component_sample_width(frame, ci) > 2;
            match (h2, v2, h1, v1, wide) {
                (true, true, _, _, true) => Upsample::FancyH2V2,
                (true, _, _, true, true) => Upsample::FancyH2V1,
                (_, true, true, _, _) => Upsample::FancyH1V2,
                _ => Upsample::Box,
            }
        })
        .collect()
}

/// Expand one component row to the frame's output width.
///
/// `near` is the source row selected by box upsampling. For a vertical fancy
/// mode, `far` is its preceding row on even output rows and following row on
/// odd output rows (edge rows are replicated by the caller). The formulae and
/// asymmetric `+1/+2` and `+8/+7` biases match libjpeg-turbo's
/// `h2v1_fancy_upsample`, `h1v2_fancy_upsample`, and
/// `h2v2_fancy_upsample`.
fn upsample_component_row(
    mode: Upsample,
    dst: &mut [u8],
    output_y: usize,
    near: &[u8],
    far: &[u8],
    source_w: usize,
    h: usize,
    hmax: usize,
) {
    match mode {
        Upsample::Box => {
            if h == hmax {
                dst.copy_from_slice(&near[..dst.len()]);
            } else {
                for (x, d) in dst.iter_mut().enumerate() {
                    *d = near[(x * h / hmax).min(source_w - 1)];
                }
            }
        }
        Upsample::FancyH2V1 => {
            for (x, d) in dst.iter_mut().enumerate() {
                let sx = (x / 2).min(source_w - 1);
                let v = near[sx] as u16;
                *d = if x & 1 == 0 {
                    if sx == 0 {
                        v as u8
                    } else {
                        ((v * 3 + near[sx - 1] as u16 + 1) >> 2) as u8
                    }
                } else if sx + 1 == source_w {
                    v as u8
                } else {
                    ((v * 3 + near[sx + 1] as u16 + 2) >> 2) as u8
                };
            }
        }
        Upsample::FancyH1V2 => {
            let bias = 1 + (output_y & 1) as u16;
            for (x, d) in dst.iter_mut().enumerate() {
                *d = ((near[x] as u16 * 3 + far[x] as u16 + bias) >> 2) as u8;
            }
        }
        Upsample::FancyH2V2 => {
            for (x, d) in dst.iter_mut().enumerate() {
                let sx = (x / 2).min(source_w - 1);
                let this = near[sx] as u16 * 3 + far[sx] as u16;
                *d = if x & 1 == 0 {
                    if sx == 0 {
                        ((this * 4 + 8) >> 4) as u8
                    } else {
                        let prev = near[sx - 1] as u16 * 3 + far[sx - 1] as u16;
                        ((this * 3 + prev + 8) >> 4) as u8
                    }
                } else if sx + 1 == source_w {
                    ((this * 4 + 7) >> 4) as u8
                } else {
                    let next = near[sx + 1] as u16 * 3 + far[sx + 1] as u16;
                    ((this * 3 + next + 7) >> 4) as u8
                };
            }
        }
    }
}

/// How one output row is produced from the per-component sample rows. Shared
/// by the whole-image [`assemble`] path and the [`Decoder::scan_sequential_streaming`]
/// path so both emit byte-for-byte identical output.
#[derive(Clone, Copy)]
enum RowKind {
    /// One component, no upsampling: copy the first `w` samples directly.
    Gray,
    /// Two components, interleaved with horizontal upsampling and no colour
    /// transform (a 2-component JPEG is `JCS_UNKNOWN` — two independent
    /// components, never YCbCr). Both channels are handed over as
    /// [`DecodedFormat::Multi2`] so a 2-colorant `/DeviceN` duotone routes
    /// through the same tint table as the JPX path, instead of dropping
    /// component 1 and mis-reading component 0 as gray.
    Multi2,
    /// Three components. `true` = the samples are already RGB (Adobe transform
    /// 0 or component ids `RGB`); `false` = YCbCr → RGB.
    Rgb(bool),
    /// Four components → raw libjpeg DeviceCMYK. `true` = YCCK (Adobe transform
    /// ≠ 0, `CMY = 255 − (YCbCr→RGB)`); `false` = direct CMYK passthrough.
    Cmyk(bool),
}

/// The output row kind, [`DecodedFormat`], and bytes-per-pixel for a frame.
/// `parse_sof` already restricts the component count to `1..=4`.
fn output_layout(
    frame: &Frame,
    adobe: Option<u8>,
    saw_jfif: bool,
) -> (RowKind, DecodedFormat, usize) {
    match frame.comps.len() {
        1 => (RowKind::Gray, DecodedFormat::Gray8, 1),
        2 => (RowKind::Multi2, DecodedFormat::Multi2, 2),
        3 => {
            // Adobe transform 0 = plain RGB; JFIF or Adobe 1 = YCbCr; untagged
            // files use YCbCr unless the component ids spell "RGB".
            let ids: Vec<u8> = frame.comps.iter().map(|c| c.id).collect();
            let is_rgb =
                adobe == Some(0) || (!saw_jfif && adobe.is_none() && ids == [b'R', b'G', b'B']);
            (RowKind::Rgb(is_rgb), DecodedFormat::Rgb8, 3)
        }
        // Four channels → DeviceCMYK. We emit exactly the bytes libjpeg's
        // `jdcolor` produces (YCCK runs `ycck_cmyk_convert`; direct CMYK passes
        // through) and leave Adobe's inverted-sample polarity for the PDF
        // `/Decode` array, matching PDFium's `CPDF_DIB` path byte-for-byte.
        _ => (
            RowKind::Cmyk(four_comp_is_ycck(adobe)),
            DecodedFormat::Cmyk8,
            4,
        ),
    }
}

/// Horizontal chroma addressing for the two SIMD-vectorizable YCbCr→RGB
/// layouts: luma is 1:1 and chroma is either 1:1 (4:4:4) or 2:1 horizontally
/// (4:2:2 / 4:2:0 — each chroma sample spans two output columns). Any other
/// sampling (e.g. 4:1:1, 3:1) falls back to the scalar per-pixel path.
// Only `rgb_ycc_row_avx2` and its call site take this, and both are
// `#[cfg(target_arch = "x86_64")]`; on other targets (e.g. aarch64-android)
// there is no AVX2 path and nothing names this type.
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChromaX {
    /// `xm_chroma[x] == x` (no horizontal upsampling).
    Identity,
    /// `xm_chroma[x] == x / 2` (2× horizontal upsampling).
    #[cfg_attr(not(test), allow(dead_code))]
    Half,
}

/// Write one output row from already-upsampled component rows.
#[allow(unsafe_code)]
#[inline]
fn assemble_row(kind: RowKind, out_row: &mut [u8], w: usize, rows: &[&[u8]]) {
    match kind {
        RowKind::Gray => out_row[..w].copy_from_slice(&rows[0][..w]),
        RowKind::Multi2 => {
            let (r0, r1) = (rows[0], rows[1]);
            for x in 0..w {
                let dst = x * 2;
                out_row[dst] = r0[x];
                out_row[dst + 1] = r1[x];
            }
        }
        RowKind::Rgb(is_rgb) => {
            let (r0, r1, r2) = (rows[0], rows[1], rows[2]);
            #[cfg(target_arch = "x86_64")]
            if !is_rgb {
                if is_x86_feature_detected!("avx2") {
                    // SAFETY: guarded by the runtime AVX2 feature probe; every
                    // component row has already been expanded to `w` samples.
                    let done =
                        unsafe { rgb_ycc_row_avx2(ChromaX::Identity, out_row, w, r0, r1, r2) };
                    for x in done..w {
                        store_three(out_row, x, r0[x], r1[x], r2[x], false);
                    }
                    return;
                }
            }
            for x in 0..w {
                store_three(out_row, x, r0[x], r1[x], r2[x], is_rgb);
            }
        }
        RowKind::Cmyk(ycck) => {
            let (r0, r1, r2, r3) = (rows[0], rows[1], rows[2], rows[3]);
            for x in 0..w {
                let px = cmyk_libjpeg_pixel(r0[x], r1[x], r2[x], r3[x], ycck);
                let dst = x * 4;
                out_row[dst..dst + 4].copy_from_slice(&px);
            }
        }
    }
}

#[inline]
fn stream_source_row<'a>(
    sy: usize,
    band_start: usize,
    band_rows: usize,
    stride: usize,
    band: &'a [u8],
    above: &'a [u8],
    below: Option<&'a [u8]>,
) -> &'a [u8] {
    if sy < band_start {
        &above[..stride]
    } else if sy >= band_start + band_rows {
        match below {
            Some(next) => &next[..stride],
            // If lookahead is unexpectedly absent, use image-edge behavior
            // instead of panicking: replicate the band's final source row.
            None => {
                let last = band_rows.saturating_sub(1);
                &band[last * stride..last * stride + stride]
            }
        }
    } else {
        let local = sy - band_start;
        &band[local * stride..(local + 1) * stride]
    }
}

/// Upsample and emit one decoded MCU band. `below_bands` supplies the one-row
/// lookahead required by 2:1 vertical fancy upsampling; `above_tail` supplies
/// the matching lookbehind without retaining a third full band.
#[allow(clippy::too_many_arguments)]
fn emit_stream_band(
    frame: &Frame,
    scan_comps: &[ScanComp],
    my: usize,
    bands: &[Vec<u8>],
    above_tail: &[Vec<u8>],
    below_bands: Option<&[Vec<u8>]>,
    modes: &[Upsample],
    sampled: &mut [Vec<u8>],
    kind: RowKind,
    out: &mut [u8],
    output_stride: usize,
) {
    let (w, h) = (frame.out_w(), frame.out_h());
    let dct = frame.dct_size;
    let y0 = my * dct * frame.vmax;
    let y1 = ((my + 1) * dct * frame.vmax).min(h);

    for y in y0..y1 {
        for (si, sc) in scan_comps.iter().enumerate() {
            let ci = sc.comp;
            let c = &frame.comps[ci];
            let source_w = component_sample_width(frame, ci);
            let source_h = component_sample_height(frame, ci);
            let source_stride = c.bw * dct;
            let band_rows = c.v * dct;
            let band_start = my * band_rows;
            let sy = (y * c.v / frame.vmax).min(source_h - 1);
            let far_sy = if modes[ci].needs_vertical_context() {
                if y & 1 == 0 {
                    sy.saturating_sub(1)
                } else {
                    (sy + 1).min(source_h - 1)
                }
            } else {
                sy
            };
            let below = below_bands.map(|next| next[si].as_slice());
            let near = stream_source_row(
                sy,
                band_start,
                band_rows,
                source_stride,
                &bands[si],
                &above_tail[si],
                below,
            );
            let far = stream_source_row(
                far_sy,
                band_start,
                band_rows,
                source_stride,
                &bands[si],
                &above_tail[si],
                below,
            );
            upsample_component_row(
                modes[ci],
                &mut sampled[si],
                y,
                near,
                far,
                source_w,
                c.h,
                frame.hmax,
            );
        }
        let mut rows: [&[u8]; 4] = [&[]; 4];
        for (si, row) in sampled.iter().enumerate() {
            rows[si] = row;
        }
        assemble_row(
            kind,
            &mut out[y * output_stride..(y + 1) * output_stride],
            w,
            &rows[..scan_comps.len()],
        );
    }
}

fn assemble(
    frame: &Frame,
    planes: &[Vec<u8>],
    adobe: Option<u8>,
    saw_jfif: bool,
) -> Result<DecodedImage, ImageError> {
    let (w, h) = (frame.out_w(), frame.out_h());
    let n = frame.comps.len();
    // Unreachable in practice: `parse_sof` rejects component counts outside
    // 1..=4. Kept as a typed, named error (never a silent blank).
    if !(1..=4).contains(&n) {
        return Err(err(&format!("unsupported JPEG component count: {n}")));
    }
    let (kind, format, bpp) = output_layout(frame, adobe, saw_jfif);
    let modes = component_upsampling(frame);
    let stride = w * bpp;
    let mut out = zeroed_arc(stride * h);
    let mut sampled = vec![vec![0u8; w]; n];
    // Per-component sample geometry depends only on the component, not on the
    // row, but sits inside the row loop if derived inline — `h * n` redundant
    // `div_ceil` chains on a tall scan. Derive it once.
    let geom: Vec<(usize, usize, usize)> = (0..n)
        .map(|ci| {
            (
                component_sample_width(frame, ci),
                component_sample_height(frame, ci),
                frame.comps[ci].bw * frame.dct_size,
            )
        })
        .collect();
    for y in 0..h {
        for ci in 0..n {
            let c = &frame.comps[ci];
            let (source_w, source_h, source_stride) = geom[ci];
            let sy = (y * c.v / frame.vmax).min(source_h - 1);
            let far_sy = if modes[ci].needs_vertical_context() {
                if y & 1 == 0 {
                    sy.saturating_sub(1)
                } else {
                    (sy + 1).min(source_h - 1)
                }
            } else {
                sy
            };
            let near = &planes[ci][sy * source_stride..(sy + 1) * source_stride];
            let far = &planes[ci][far_sy * source_stride..(far_sy + 1) * source_stride];
            upsample_component_row(
                modes[ci],
                &mut sampled[ci],
                y,
                near,
                far,
                source_w,
                c.h,
                frame.hmax,
            );
        }
        let mut rows: [&[u8]; 4] = [&[]; 4];
        for (ci, row) in sampled.iter().enumerate() {
            rows[ci] = row;
        }
        assemble_row(
            kind,
            &mut unique_arc_mut(&mut out)[y * stride..y * stride + stride],
            w,
            &rows[..n],
        );
    }
    Ok(image(w, h, format, stride, out))
}

#[inline]
fn store_three(out: &mut [u8], pixel: usize, c0: u8, c1: u8, c2: u8, is_rgb: bool) {
    let rgb = if is_rgb {
        [c0, c1, c2]
    } else {
        ycc_to_rgb(c0, c1, c2)
    };
    let dst = pixel * 3;
    out[dst] = rgb[0];
    out[dst + 1] = rgb[1];
    out[dst + 2] = rgb[2];
}

fn image(
    w: usize,
    h: usize,
    format: DecodedFormat,
    stride: usize,
    data: Arc<[u8]>,
) -> DecodedImage {
    DecodedImage {
        width: w as u32,
        height: h as u32,
        format,
        stride,
        data,
    }
}

/// Allocate the decoder's final shared output directly. A `Vec<u8>` cannot be
/// rehomed into `Arc<[u8]>` without a full allocation and copy because the Arc
/// refcounts need adjacent storage; scan assembly writes into this uniquely
/// owned allocation and then hands it to `DecodedImage` unchanged.
#[allow(
    unsafe_code,
    reason = "Arc::new_zeroed_slice hands back MaybeUninit; see SAFETY comment"
)]
fn zeroed_arc(len: usize) -> Arc<[u8]> {
    let data = Arc::<[u8]>::new_zeroed_slice(len);
    // SAFETY: `new_zeroed_slice` initialized every `u8` to a valid zero value.
    unsafe { data.assume_init() }
}

#[inline]
fn unique_arc_mut<T>(data: &mut Arc<[T]>) -> &mut [T] {
    let Some(data) = Arc::get_mut(data) else {
        unreachable!("JPEG output is never shared while the decoder writes it")
    };
    data
}

#[cfg(test)]
mod idct_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    //! The AVX2 IDCT kernel must be bit-for-bit identical to the scalar
    //! reference (its correctness argument: same op sequence per lane, no FMA).
    use super::encoder::AAN_SCALE_FACTORS;
    use super::{
        Component, DecodedFormat, Frame, RowKind, ZIGZAG_INV, assemble_row, idct_block_scalar,
        output_layout,
    };

    fn dequant_from(q: &[u16; 64]) -> [f32; 64] {
        let mut dq = [0f32; 64];
        for zig in 0..64 {
            let pos = ZIGZAG_INV[zig] as usize;
            dq[pos] = q[zig] as f32 * AAN_SCALE_FACTORS[pos / 8] * AAN_SCALE_FACTORS[pos % 8] / 8.0;
        }
        dq
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    #[allow(unsafe_code)]
    fn avx2_matches_scalar_bit_exact() {
        if !is_x86_feature_detected!("avx2") {
            return; // nothing to compare against on this host
        }
        // A small xorshift PRNG keeps this deterministic and dependency-free.
        let mut state = 0x9E3779B97F4A7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for trial in 0..5000 {
            // Random quant table (1..=255) and coefficients, biased toward the
            // sparse low-frequency blocks that dominate real scans.
            let mut q = [1u16; 64];
            for e in q.iter_mut() {
                *e = 1 + (next() % 255) as u16;
            }
            let dq = dequant_from(&q);
            let mut coeffs = [0i16; 64];
            let nz = if trial % 3 == 0 {
                64
            } else {
                1 + (next() % 12) as usize
            };
            for _ in 0..nz {
                let idx = (next() % 64) as usize;
                coeffs[idx] = (next() % 2048) as i16 - 1024;
            }
            let mut a = [0f32; 64];
            let mut b = [0f32; 64];
            idct_block_scalar(&coeffs, &dq, &mut a);
            // SAFETY: guarded by the avx2 feature check above.
            unsafe { super::idct_block_avx2(&coeffs, &dq, &mut b) };
            for k in 0..64 {
                assert_eq!(
                    a[k].to_bits(),
                    b[k].to_bits(),
                    "trial {trial} lane {k}: scalar {} avx2 {}",
                    a[k],
                    b[k]
                );
            }
        }
    }

    fn comp(id: u8) -> Component {
        Component {
            id,
            h: 1,
            v: 1,
            tq: 0,
            bw: 1,
            bh: 1,
            bw_used: 1,
            bh_used: 1,
            coeffs: vec![0; 64],
        }
    }

    fn frame_with(n: u8) -> Frame {
        Frame {
            progressive: false,
            width: 8,
            height: 8,
            comps: (0..n).map(|i| comp(i + 1)).collect(),
            hmax: 1,
            vmax: 1,
            mcus_x: 1,
            mcus_y: 1,
            dct_size: 8,
        }
    }

    #[test]
    fn two_component_frame_is_multi2_not_dropped_to_gray() {
        // A 2-component JPEG is JCS_UNKNOWN: two independent colorants (a spot +
        // black `/DeviceN` duotone, e.g. pdfjs issue6364). Both channels must
        // survive as Multi2 so the tint table interprets them; dropping
        // component 1 and reading component 0 as gray tone-inverted the cover.
        let (kind, format, bpp) = output_layout(&frame_with(2), None, false);
        assert!(matches!(kind, RowKind::Multi2));
        assert_eq!(format, DecodedFormat::Multi2);
        assert_eq!(bpp, 2);
    }

    #[test]
    fn multi2_row_interleaves_both_components() {
        let r0 = [10u8, 20, 30, 40];
        let r1 = [50u8, 60, 70, 80];
        let mut out = [0u8; 8];
        assemble_row(RowKind::Multi2, &mut out, 4, &[&r0, &r1]);
        assert_eq!(out, [10, 50, 20, 60, 30, 70, 40, 80]);
    }
}

#[cfg(test)]
mod upsample_tests {
    use super::{Upsample, upsample_component_row};

    #[test]
    fn h2v1_matches_libjpeg_triangle_and_edge_replication() {
        let near = [10u8, 50, 90];
        let mut out = [0u8; 6];
        upsample_component_row(
            Upsample::FancyH2V1,
            &mut out,
            0,
            &near,
            &near,
            near.len(),
            1,
            2,
        );
        assert_eq!(out, [10, 20, 40, 60, 80, 90]);
    }

    #[test]
    fn h1v2_uses_alternating_vertical_rounding_bias() {
        let near = [10u8, 50];
        let far = [32u8, 72];
        let mut even = [0u8; 2];
        let mut odd = [0u8; 2];
        upsample_component_row(
            Upsample::FancyH1V2,
            &mut even,
            2,
            &near,
            &far,
            near.len(),
            1,
            1,
        );
        upsample_component_row(
            Upsample::FancyH1V2,
            &mut odd,
            3,
            &near,
            &far,
            near.len(),
            1,
            1,
        );
        assert_eq!(even, [15, 55]);
        assert_eq!(odd, [16, 56]);
    }

    #[test]
    fn h2v2_matches_libjpeg_combined_integer_filter() {
        let near = [10u8, 50, 90];
        let far = [30u8, 70, 110];
        let mut out = [0u8; 6];
        upsample_component_row(
            Upsample::FancyH2V2,
            &mut out,
            2,
            &near,
            &far,
            near.len(),
            1,
            2,
        );
        assert_eq!(out, [15, 25, 45, 65, 85, 95]);
    }

    #[test]
    fn box_handles_nonstandard_integral_ratios() {
        let near = [7u8, 17];
        let mut out = [0u8; 6];
        upsample_component_row(Upsample::Box, &mut out, 0, &near, &near, near.len(), 1, 3);
        assert_eq!(out, [7, 7, 7, 17, 17, 17]);
    }
}

#[cfg(test)]
mod dc_only_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    //! The streaming DC-only fast path substitutes one constant byte for a full
    //! 8×8 IDCT + band store when a block has no AC coefficients. This is exact:
    //! the inverse DCT of a DC-only block is `coeffs[0]·dequant[0]` at every
    //! sample. Prove — across the full baseline DC range and several quant[0]
    //! values — that both IDCT kernels produce that flat block and that the
    //! fast-path byte equals what the band store would emit.
    use super::encoder::AAN_SCALE_FACTORS;
    use super::{ZIGZAG_INV, idct_block, idct_block_scalar, store_band_block_scalar};

    fn dequant_from(q: &[u16; 64]) -> [f32; 64] {
        let mut dq = [0f32; 64];
        for zig in 0..64 {
            let pos = ZIGZAG_INV[zig] as usize;
            dq[pos] = q[zig] as f32 * AAN_SCALE_FACTORS[pos / 8] * AAN_SCALE_FACTORS[pos % 8] / 8.0;
        }
        dq
    }

    fn check(out: &[f32; 64], dc: i16, dq: &[f32; 64], q0: u16) {
        let expected = dc as f32 * dq[0];
        for (i, &v) in out.iter().enumerate() {
            assert_eq!(
                v.to_bits(),
                expected.to_bits(),
                "dc {dc} q0 {q0} sample {i}: IDCT of DC-only block is not the flat constant"
            );
        }
        // The fast-path byte must equal the byte the band store would write.
        let fast = (dc as f32 * dq[0] + 128.5).clamp(0.0, 255.0) as u8;
        let mut band = [0u8; 64];
        store_band_block_scalar(out, &mut band, 0, 8);
        for (i, &b) in band.iter().enumerate() {
            assert_eq!(
                b, fast,
                "dc {dc} q0 {q0} sample {i}: fast byte != band-store byte"
            );
        }
    }

    #[test]
    fn dc_only_idct_is_flat_constant_and_matches_fast_path() {
        for &q0 in &[1u16, 2, 3, 8, 16, 37, 99, 255] {
            let mut q = [1u16; 64];
            q[0] = q0;
            let dq = dequant_from(&q);
            for dc in -1024..=1023i16 {
                let mut coeffs = [0i16; 64];
                coeffs[0] = dc;
                // Dispatched kernel (AVX2 when present) and the scalar reference
                // must both yield the exact flat constant.
                let mut a = [0f32; 64];
                let mut b = [0f32; 64];
                idct_block(&coeffs, &dq, &mut a);
                idct_block_scalar(&coeffs, &dq, &mut b);
                check(&a, dc, &dq, q0);
                check(&b, dc, &dq, q0);
            }
        }
    }
}

#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod band_store_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    //! The AVX2 band store must be byte-identical to the scalar reference over
    //! IDCT-shaped inputs, including out-of-range values that exercise the clamp.
    use super::{store_band_block_avx2, store_band_block_scalar};

    #[test]
    #[allow(unsafe_code)]
    fn avx2_band_store_matches_scalar() {
        if !is_x86_feature_detected!("avx2") {
            return;
        }
        let mut state = 0x243F6A8885A308D3u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let mut fbuf = [0f32; 64];
            for v in fbuf.iter_mut() {
                // Span well past [-128.5, 126.5] so both clamp edges fire, with
                // fractional parts to exercise the truncation.
                let r = (next() % 1_000_000) as f32 / 1000.0 - 500.0;
                *v = r;
            }
            // Non-trivial stride so base/stride addressing is covered too.
            let bstride = 11;
            let mut a = vec![0u8; bstride * 8];
            let mut b = vec![0u8; bstride * 8];
            store_band_block_scalar(&fbuf, &mut a, 0, bstride);
            // SAFETY: guarded by the avx2 feature check above.
            unsafe { store_band_block_avx2(&fbuf, &mut b, 0, bstride) };
            assert_eq!(a, b, "avx2 band store differs from scalar");
        }
    }
}

#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod rgb_simd_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    //! The AVX2 YCbCr→RGB row kernel must be byte-for-byte identical to the
    //! scalar table path `ycc_to_rgb`, including its two saturated-edge green
    //! corrections. The identity-chroma test is exhaustive over all 16,777,216
    //! `(Y, Cb, Cr)` triples; a second test covers the 2:1 chroma duplication.
    use super::{ChromaX, rgb_ycc_row_avx2, ycc_to_rgb};

    #[test]
    #[allow(unsafe_code)]
    fn avx2_ycc_rgb_is_exhaustively_byte_exact() {
        if !is_x86_feature_detected!("avx2") {
            return;
        }
        let w = 256usize;
        let y_row: Vec<u8> = (0..w).map(|x| x as u8).collect();
        let mut cb_row = vec![0u8; w];
        let mut cr_row = vec![0u8; w];
        let mut out = vec![0u8; w * 3];
        for cb in 0..=255u8 {
            cb_row.iter_mut().for_each(|v| *v = cb);
            for cr in 0..=255u8 {
                cr_row.iter_mut().for_each(|v| *v = cr);
                // SAFETY: guarded by the avx2 feature check above.
                let done = unsafe {
                    rgb_ycc_row_avx2(ChromaX::Identity, &mut out, w, &y_row, &cb_row, &cr_row)
                };
                assert_eq!(done, w, "whole row should vectorize");
                for x in 0..w {
                    let exp = ycc_to_rgb(y_row[x], cb, cr);
                    let got = [out[x * 3], out[x * 3 + 1], out[x * 3 + 2]];
                    assert_eq!(got, exp, "y={} cb={cb} cr={cr}", y_row[x]);
                }
            }
        }
    }

    #[test]
    #[allow(unsafe_code)]
    fn avx2_half_chroma_matches_scalar() {
        if !is_x86_feature_detected!("avx2") {
            return;
        }
        let mut state = 0xDEAD_BEEF_1234_5678u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..300 {
            let w = 8 + (next() % 400) as usize;
            let cw = w / 2 + 4; // padded like a real chroma band row
            let y_row: Vec<u8> = (0..w).map(|_| (next() & 0xff) as u8).collect();
            let cb_row: Vec<u8> = (0..cw).map(|_| (next() & 0xff) as u8).collect();
            let cr_row: Vec<u8> = (0..cw).map(|_| (next() & 0xff) as u8).collect();
            let mut out = vec![0u8; w * 3];
            // SAFETY: guarded by the avx2 feature check above.
            let done =
                unsafe { rgb_ycc_row_avx2(ChromaX::Half, &mut out, w, &y_row, &cb_row, &cr_row) };
            for x in 0..done {
                let exp = ycc_to_rgb(y_row[x], cb_row[x / 2], cr_row[x / 2]);
                let got = [out[x * 3], out[x * 3 + 1], out[x * 3 + 2]];
                assert_eq!(got, exp, "half x={x} w={w}");
            }
        }
    }
}

#[cfg(test)]
mod streaming_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    //! The baseline MCU-row streaming pipeline must produce byte-for-byte the
    //! same output as the whole-image coefficient path. Verified on a baseline
    //! 4:2:0 stream *with restart markers* (mid-MCU-row, interval 4 over
    //! `mcus_x == 5`), plus the wild-format fixtures.
    use super::{DecodeLimits, DecodedImage, Decoder, ImageError};

    fn decode_stream(data: &[u8]) -> Result<DecodedImage, ImageError> {
        let limits = DecodeLimits::default();
        let mut d = Decoder::new(data, &limits);
        d.run()?;
        d.finish()
    }

    fn decode_coeff(data: &[u8]) -> Result<DecodedImage, ImageError> {
        let limits = DecodeLimits::default();
        let mut d = Decoder::new(data, &limits);
        d.allow_stream = false;
        d.run()?;
        d.finish()
    }

    fn assert_paths_identical(name: &str, data: &[u8]) {
        let s = decode_stream(data).unwrap_or_else(|e| panic!("{name} stream: {e}"));
        let c = decode_coeff(data).unwrap_or_else(|e| panic!("{name} coeff: {e}"));
        assert_eq!((s.width, s.height), (c.width, c.height), "{name} dims");
        assert_eq!(s.format, c.format, "{name} format");
        assert_eq!(s.stride, c.stride, "{name} stride");
        assert_eq!(
            &*s.data, &*c.data,
            "{name}: streaming vs coefficient bytes differ"
        );
    }

    #[test]
    fn baseline_420_restart_paths_match() {
        assert_paths_identical(
            "base420_restart",
            include_bytes!("../../tests/fixtures/base420_restart.jpg"),
        );
    }

    #[test]
    fn baseline_422_paths_match() {
        assert_paths_identical(
            "base_rgb422",
            include_bytes!("../../tests/fixtures/base_rgb422.jpg"),
        );
    }

    #[test]
    fn baseline_restart_gray_paths_match() {
        assert_paths_identical(
            "restart_gray",
            include_bytes!("../../tests/fixtures/restart_gray.jpg"),
        );
    }

    #[test]
    fn cmyk_paths_match() {
        assert_paths_identical("cmyk", include_bytes!("../../tests/fixtures/cmyk.jpg"));
        assert_paths_identical("ycck", include_bytes!("../../tests/fixtures/ycck.jpg"));
        assert_paths_identical(
            "nomad_flute_p0",
            include_bytes!("../../tests/fixtures/nomad_flute_p0.jpg"),
        );
    }
}

#[cfg(test)]
mod cmyk_polarity_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    //! Unit tests for the 4-component (CMYK / YCCK) polarity math.
    //!
    //! The decoder emits **raw libjpeg** DeviceCMYK bytes (Adobe's inverted
    //! polarity is left for the PDF `/Decode` array to undo, matching PDFium's
    //! `CPDF_DIB` path). Each case therefore asserts the raw codec output *and*
    //! the true DeviceCMYK it becomes once the appropriate `/Decode` is applied
    //! (identity `[0 1 …]` for non-Adobe files, inverting `[1 0 …]` for Adobe).
    use super::{cmyk_libjpeg_pixel, four_comp_is_ycck, ycc_to_rgb};

    /// Apply `/Decode [1 0 1 0 1 0 1 0]` (Adobe inversion) to a raw pixel: the
    /// per-component remap `min + sample/255 * (max - min)` with `[min,max]=[1,0]`
    /// is `255 - sample` in 8-bit space.
    fn decode_1_0(px: [u8; 4]) -> [u8; 4] {
        [255 - px[0], 255 - px[1], 255 - px[2], 255 - px[3]]
    }

    fn ycc_to_rgb_reference(y: u8, cb: u8, cr: u8) -> [u8; 3] {
        let clamp = |v: f32| v.clamp(0.0, 255.0) as u8;
        let y = y as f32;
        let cb = cb as f32 - 128.0;
        let cr = cr as f32 - 128.0;
        [
            clamp(y + 1.402 * cr + 0.5),
            clamp(y - 0.344136 * cb - 0.714136 * cr + 0.5),
            clamp(y + 1.772 * cb + 0.5),
        ]
    }

    #[test]
    fn optimized_ycc_conversion_is_exhaustively_byte_exact() {
        for cb in 0..=255u8 {
            for cr in 0..=255u8 {
                for y in 0..=255u8 {
                    assert_eq!(
                        ycc_to_rgb(y, cb, cr),
                        ycc_to_rgb_reference(y, cb, cr),
                        "Y={y} Cb={cb} Cr={cr}"
                    );
                }
            }
        }
    }

    /// libjpeg selects YCCK only for an Adobe marker with a non-zero transform.
    #[test]
    fn ycck_selection_matches_libjpeg() {
        assert!(!four_comp_is_ycck(None), "no marker → CMYK");
        assert!(!four_comp_is_ycck(Some(0)), "transform 0 → CMYK");
        assert!(four_comp_is_ycck(Some(2)), "transform 2 → YCCK");
        assert!(
            four_comp_is_ycck(Some(1)),
            "non-zero transform → assumed YCCK"
        );
    }

    /// (a) YCCK + Adobe, white pixel. A conforming Adobe YCCK file stores white
    /// (no ink) as luma 0 / neutral chroma and inverted K = 255. libjpeg
    /// decodes that to raw CMYK [255,255,255,255]; `/Decode [1 0 …]` normalizes
    /// it to true DeviceCMYK [0,0,0,0] → white (the PDFium ~2.1%-ink result).
    #[test]
    fn ycck_adobe_white() {
        let raw = cmyk_libjpeg_pixel(0, 128, 128, 255, /*ycck=*/ true);
        assert_eq!(raw, [255, 255, 255, 255], "raw libjpeg YCCK white");
        assert_eq!(
            decode_1_0(raw),
            [0, 0, 0, 0],
            "true DeviceCMYK white after /Decode"
        );
    }

    /// (b) YCCK + Adobe, solid (K) black. True CMYK [0,0,0,255]; the conforming
    /// file stores it inverted, libjpeg yields raw [255,255,255,0], and
    /// `/Decode [1 0 …]` recovers [0,0,0,255].
    #[test]
    fn ycck_adobe_solid_black() {
        let raw = cmyk_libjpeg_pixel(0, 128, 128, 0, /*ycck=*/ true);
        assert_eq!(raw, [255, 255, 255, 0], "raw libjpeg YCCK black");
        assert_eq!(
            decode_1_0(raw),
            [0, 0, 0, 255],
            "true DeviceCMYK K-black after /Decode"
        );
    }

    /// (c) Direct CMYK + Adobe (transform 0). libjpeg passes the stored samples
    /// through untouched; the Adobe inversion lives entirely in `/Decode`.
    /// Stored inverted cyan [0,255,255,255] → raw passthrough → /Decode → true
    /// cyan [255,0,0,0].
    #[test]
    fn direct_cmyk_adobe_passthrough() {
        let raw = cmyk_libjpeg_pixel(0, 255, 255, 255, /*ycck=*/ false);
        assert_eq!(raw, [0, 255, 255, 255], "direct CMYK is a raw passthrough");
        assert_eq!(
            decode_1_0(raw),
            [255, 0, 0, 0],
            "true DeviceCMYK cyan after /Decode"
        );
    }

    /// (d) Direct CMYK, no Adobe marker. libjpeg still passes samples through;
    /// such files carry no inverting `/Decode`, so the raw output *is* the true
    /// DeviceCMYK. Stored magenta [0,255,0,0] stays [0,255,0,0].
    #[test]
    fn direct_cmyk_no_adobe_is_true_devicecmyk() {
        let raw = cmyk_libjpeg_pixel(0, 255, 0, 0, /*ycck=*/ false);
        assert_eq!(
            raw,
            [0, 255, 0, 0],
            "no-marker CMYK passes through as true DeviceCMYK"
        );
    }
}

#[cfg(test)]
mod assemble_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    //! `assemble()` channel-shape test: the 2-component (JCS_UNKNOWN) path
    //! (B1 / East Asia textbook duotone plates).
    use super::{Component, DecodedFormat, Frame, assemble};

    /// A minimal 1×1-sampled frame of `ncomp` components over `w×h`; the caller
    /// supplies matching 8-wide planes.
    fn frame(w: usize, h: usize, ncomp: usize) -> Frame {
        let comps = (0..ncomp)
            .map(|i| Component {
                id: i as u8,
                h: 1,
                v: 1,
                tq: 0,
                bw: 1,
                bh: 1,
                bw_used: 1,
                bh_used: 1,
                coeffs: Vec::new(),
            })
            .collect();
        Frame {
            progressive: false,
            width: w,
            height: h,
            comps,
            hmax: 1,
            vmax: 1,
            mcus_x: 1,
            mcus_y: 1,
            dct_size: 8,
        }
    }

    #[test]
    fn two_component_keeps_both_channels_as_multi2() {
        // Two channels have no JFIF/Adobe colour meaning (JCS_UNKNOWN): they are
        // two independent colorants a 2-colorant `/DeviceN` duotone interprets
        // through its tint table. Both must survive, interleaved, as Multi2 —
        // dropping component 1 and reading component 0 as gray tone-inverted the
        // pdfjs/issue6364 cover.
        let (w, h) = (2usize, 2usize);
        let mut p0 = vec![0u8; 64]; // one 8×8 block plane
        let mut p1 = vec![0u8; 64];
        for y in 0..h {
            for x in 0..w {
                p0[y * 8 + x] = (10 + y * 2 + x) as u8;
                p1[y * 8 + x] = (200 + y * 2 + x) as u8;
            }
        }
        let img = assemble(&frame(w, h, 2), &[p0, p1], None, false).unwrap();
        assert_eq!(
            img.format,
            DecodedFormat::Multi2,
            "2-comp decodes to Multi2"
        );
        assert_eq!((img.width, img.height), (2, 2));
        // Interleaved comp0,comp1 per pixel across the 2×2 image.
        assert_eq!(
            &img.data[..8],
            &[10u8, 200, 11, 201, 12, 202, 13, 203],
            "both channels interleaved"
        );
    }
}

#[cfg(test)]
mod streaming_row_tests {
    use super::stream_source_row;

    #[test]
    fn missing_lower_lookahead_replicates_final_band_row() {
        let band = [10, 11, 20, 21];
        let above = [1, 2];
        assert_eq!(
            stream_source_row(4, 2, 2, 2, &band, &above, None),
            &[20, 21]
        );
    }
}
