// src/arithmetic_coder.rs

//! A pure Rust port of the context-adaptive arithmetic coder from jbig2enc.
//! This module provides low-level bit-level encoding for JBIG2, mirroring the
//! functionality of `jbig2arith.cc` and `jbig2arith.h`. It is designed to be
//! integrated into a larger JBIG2 encoding pipeline where binarization and
//! preprocessing are already handled.

extern crate lazy_static;

use lazy_static::lazy_static;
use anyhow::{Result, anyhow};

#[cfg(feature = "trace_arith")]
use tracing::{debug, trace};

#[cfg(not(feature = "trace_arith"))]
#[macro_use]
mod trace_stubs {
    macro_rules! debug {
        ($($arg:tt)*) => { std::convert::identity(format_args!($($arg)*)) };
    }
    macro_rules! trace {
        ($($arg:tt)*) => { std::convert::identity(format_args!($($arg)*)) };
    }
}

#[cfg(not(feature = "trace_arith"))]
use trace_stubs::*;

const JBIG2_MAX_CTX: usize = 65536;
const TPGD_CTX: u32 = 0x9B25;

const TEST_INPUT: &[u8] = &[
    0, 2, 0, 0x51, 0, 0, 0, 0xc0, 0x03, 0x52, 0x87, 0x2a, 0xaa, 0xaa, 0xaa, 0xaa,
    0x82, 0xc0, 0x20, 0, 0xfc, 0xd7, 0x9e, 0xf6, 0xbf, 0x7f, 0xed, 0x90, 0x4f,
    0x46, 0xa3, 0xbf,
];

/// One probability-estimation state (ISO/IEC 14492 Table E.1)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct State {
    /// Qe value (16 bit)
    pub qe:     u16,
    /// next state if the coded symbol was the **MPS**
    pub nmps:   u8,
    /// next state if the coded symbol was the **LPS**
    pub nlps:   u8,
    /// if 1, toggle the current MPS after coding an LPS
    pub switch: bool,
}

/// Context for arithmetic encoding/decoding
pub type Context = usize;

/// Parameters for encoding integer ranges.
struct IntEncRange {
    bot: i32,    // Lower bound of the range
    top: i32,    // Upper bound of the range
    data: u8,    // Prefix bits to encode
    bits: u8,    // Number of prefix bits
    delta: u32,  // Value to subtract before encoding
    intbits: u8, // Number of bits for the integer part
}

/// Table defining how to encode integers of various ranges.
const INT_ENC_RANGE: [IntEncRange; 13] = [
    IntEncRange { bot: 0, top: 3, data: 0, bits: 2, delta: 0, intbits: 2 },
    IntEncRange { bot: -1, top: -1, data: 9, bits: 4, delta: 0, intbits: 0 },
    IntEncRange { bot: -3, top: -2, data: 5, bits: 3, delta: 2, intbits: 1 },
    IntEncRange { bot: 4, top: 19, data: 2, bits: 3, delta: 4, intbits: 4 },
    IntEncRange { bot: -19, top: -4, data: 3, bits: 3, delta: 4, intbits: 4 },
    IntEncRange { bot: 20, top: 83, data: 6, bits: 4, delta: 20, intbits: 6 },
    IntEncRange { bot: -83, top: -20, data: 7, bits: 4, delta: 20, intbits: 6 },
    IntEncRange { bot: 84, top: 339, data: 14, bits: 5, delta: 84, intbits: 8 },
    IntEncRange { bot: -339, top: -84, data: 15, bits: 5, delta: 84, intbits: 8 },
    IntEncRange { bot: 340, top: 4435, data: 30, bits: 6, delta: 340, intbits: 12 },
    IntEncRange { bot: -4435, top: -340, data: 31, bits: 6, delta: 340, intbits: 12 },
    IntEncRange { bot: 4436, top: 2_000_000_000, data: 62, bits: 6, delta: 4436, intbits: 32 },
    IntEncRange { bot: -2_000_000_000, top: -4436, data: 63, bits: 6, delta: 4436, intbits: 32 },
];

/// Integer encoding procedure types, corresponding to JBIG2_IA* enums.
#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub enum IntProc {
    Iaai = 0,
    Iadh,
    Iads,
    Iadt,
    Iadw,
    Iaex,
    Iafs,
    Iait,
    Iardh,
    Iardw,
    Iardx,
    Iardy,
    Iari,
}

#[allow(clippy::unreadable_literal)]
#[rustfmt::skip]
macro_rules! s { ( $qe:expr , $nmps:expr , $nlps:expr , $sw:expr ) =>
    { State { qe: $qe, nmps: $nmps, nlps: $nlps, switch: $sw != 0 } } }

/// Table E.1 – indices 0 … 46 (MPS = 0 half)
pub const BASE: [State; 47] = [
    s!(0x5601,  1,  1, 1),  s!(0x3401,  2,  6, 0),
    s!(0x1801,  3,  9, 0),  s!(0x0AC1,  4, 12, 0),
    s!(0x0521,  5, 29, 0),  s!(0x0221, 38, 33, 0),
    s!(0x5601,  7,  6, 1),  s!(0x5401,  8, 14, 0),
    s!(0x4801,  9, 14, 0),  s!(0x3801, 10, 14, 0),
    s!(0x3001, 11, 17, 0),  s!(0x2401, 12, 18, 0),
    s!(0x1C01, 13, 20, 0),  s!(0x1601, 29, 21, 0),
    s!(0x5601, 15, 14, 1),  s!(0x5401, 16, 14, 0),
    s!(0x5101, 17, 15, 0),  s!(0x4801, 18, 16, 0),
    s!(0x3801, 19, 17, 0),  s!(0x3401, 20, 18, 0),
    s!(0x3001, 21, 19, 0),  s!(0x2801, 22, 19, 0),
    s!(0x2401, 23, 20, 0),  s!(0x2201, 24, 21, 0),
    s!(0x1C01, 25, 22, 0),  s!(0x1801, 26, 23, 0),
    s!(0x1601, 27, 24, 0),  s!(0x1401, 28, 25, 0),
    s!(0x1201, 29, 26, 0),  s!(0x1101, 30, 27, 0),
    s!(0x0AC1, 31, 28, 0),  s!(0x09C1, 32, 29, 0),
    s!(0x08A1, 33, 30, 0),  s!(0x0521, 34, 31, 0),
    s!(0x0441, 35, 32, 0),  s!(0x02A1, 36, 33, 0),
    s!(0x0221, 37, 34, 0),  s!(0x0141, 38, 35, 0),
    s!(0x0111, 39, 36, 0),  s!(0x0085, 40, 37, 0),
    s!(0x0049, 41, 38, 0),  s!(0x0025, 42, 39, 0),
    s!(0x0015, 43, 40, 0),  s!(0x0009, 44, 41, 0),
    s!(0x0005, 45, 42, 0),  s!(0x0001, 45, 43, 0),
    s!(0x5601, 46, 46, 0),  // dummy “all done” state
];

/// Build the 94-state table at start-up.
lazy_static! {
    pub(crate) static ref FULL: [State; 94] = {
        let mut t = [BASE[0]; 94];
        
        for i in 0..47 {
            let s = BASE[i];
            
            // Lower half: MPS = 0
            t[i] = State {
                qe: s.qe,
                nmps: s.nmps,           // stays in lower half
                nlps: if s.switch { s.nlps + 47 } else { s.nlps },
                switch: s.switch,
            };
            
            // Upper half: MPS = 1
            t[i + 47] = State {
                qe: s.qe,
                nmps: s.nmps + 47,      // stays in upper half
                // If LPS flips the MPS we must leave the upper half
                nlps: if s.switch { s.nlps } else { s.nlps + 47 },
                switch: s.switch,
            };
        }
        
        t
    };
}

/// Context-adaptive arithmetic encoder for JBIG2.
const NUM_REFINEMENT_CX_STATES: usize = 17; // For GRTEMPLATE=0, contexts 0-16
/// Initial index into CTBL for new contexts, typically 0 (MPS=0, Qe=0x5601).

pub struct Jbig2ArithCoder {
    a: u16,          // Range register A
    c: u32,          // Code register C
    b: u8,           // Current byte being built
    ct: i8,         // Countdown register CT
    bp: i32,         // Byte position in output
    data: Vec<u8>,   // Output data
    context: Vec<usize>,
    int_ctx: [[usize; 256]; 13], // Contexts for integer encoding, storing CTBL indices
    iaid_ctx: [usize; 512],   // Dynamically sized context for IAID symbols, storing CTBL indices
    refinement_contexts: [u8; 16], // Contexts for GRTEMPLATE=0 (16 states)
}

impl Jbig2ArithCoder {
    /// Returns a reference to the internal output buffer as a byte slice.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }
    const INITIAL_STATE: usize = 0;

    /// Creates a new arithmetic encoder with initial state.
    pub fn new() -> Self {
        let mut coder = Self {
            a: 0,
            c: 0,
            b: 0,
            ct: 0,
            bp: -1,
            data: Vec::new(),
            context: vec![Self::INITIAL_STATE; 1 << 14],
            int_ctx: [[0; 256]; 13],
            iaid_ctx: [0; 512],
            refinement_contexts: [0; 16],
        };
        coder.reset();
        coder
    }

    /// Resets the encoder to its initial state, clearing output and contexts.
    pub fn reset(&mut self) {
        self.a = 0x8000;
        self.c = 0;
        self.ct = 12;
        self.b = 0;
        self.bp = -1;
        self.data.clear();
        self.context.fill(Self::INITIAL_STATE);
        for ctx in self.int_ctx.iter_mut() {
            ctx.fill(0);
        }
        self.iaid_ctx.fill(0);
        self.refinement_contexts.fill(0);
    }

    /// Finalizes the arithmetic coding stream.
    pub fn finalize(&mut self, data: &mut Vec<u8>) -> Result<()> {
        self.renorm();
        self.c = self.c.wrapping_add(self.a as u32);
        self.byte_out();
        self.c = self.c.wrapping_add(self.a as u32);
        self.byte_out();



        Ok(())
    }

    /// Renormalizes the arithmetic coder state according to the JBIG2 standard's RENORME procedure (Figure E.8).
    /// This is called when the range register A becomes too small.
    fn renorm(&mut self) {
        loop {
            // Shift A and C left by 1 bit
            self.a <<= 1;
            self.c <<= 1;
            
            // Decrement the countdown register
            self.ct -= 1;
            
            // If CT reaches zero, perform byte out
            if self.ct == 0 {
                self.byte_out();
            }
            
            // Continue until A's high bit is set (A >= 0x8000)
            if (self.a & 0x8000) != 0 {
                break;
            }
        }
    }

    fn byte_out(&mut self) {
        if self.b == 0xFF {
            if self.bp >= 0 {
                self.data.push(self.b);
            }
            self.b = (self.c >> 20) as u8;
            self.bp += 1;
            self.c &= 0x0F_FFFF;
            self.ct = 7;
            return;
        }

        if self.c < 0x800_0000 {
            if self.bp >= 0 {
                self.data.push(self.b);
            }
            self.b = (self.c >> 19) as u8;
            self.bp += 1;
            self.c &= 0x07_FFFF;
            self.ct = 8;
            return;
        }

        self.b = self.b.wrapping_add(1);
        if self.b == 0xFF {
            self.c &= 0x7_FFFF_FF;
            if self.bp >= 0 {
                self.data.push(self.b);
            }
            self.b = (self.c >> 20) as u8;
            self.bp += 1;
            self.c &= 0x0F_FFFF;
            self.ct = 7;
        } else {
            if self.bp >= 0 {
                self.data.push(self.b);
            }
            self.b = (self.c >> 19) as u8;
            self.bp += 1;
            self.c &= 0x07_FFFF;
            self.ct = 8;
        }
    }

    /// Finalizes the arithmetic coding stream.
    pub fn flush(&mut self, with_marker: bool) {
        let temp_c = self.c + self.a as u32;
        self.c |= 0x0000_FFFF;
        if self.c >= temp_c {
            self.c -= 0x8000;
        }
        self.c <<= self.ct as u32;
        self.byte_out();
        self.c <<= self.ct as u32;
        self.byte_out();


        if self.bp >= 0 {
            self.data.push(self.b);
        }
        if with_marker {
            self.data.push(0xFF);
            self.data.push(0xAC);
        }
    }

    /// Encodes a single bit `d` in the given context `ctx`.
    pub fn encode_bit(&mut self, ctx: usize, d: bool) {
        let state_idx = self.context[ctx];
        let state = FULL[state_idx];
        let qe = state.qe;
        let mps_val = state_idx >= 47;

        let mut renorm_needed = false;

        if d != mps_val { // LPS path
            self.a = self.a.wrapping_sub(qe);
            if self.a < qe {
                self.c = self.c.wrapping_add(qe as u32);
            } else {
                self.a = qe;
            }
            
            self.context[ctx] = state.nlps as usize;
            renorm_needed = true;
        } else { // MPS path
            self.a = self.a.wrapping_sub(qe);
            if (self.a & 0x8000) == 0 {
                if self.a < qe {
                    self.a = qe;
                } else {
                    self.c = self.c.wrapping_add(qe as u32);
                }
                self.context[ctx] = state.nmps as usize;
                renorm_needed = true;
            } else {
                self.c = self.c.wrapping_add(qe as u32);
            }
        }

        if renorm_needed {
            while (self.a & 0x8000) == 0 {
                self.a <<= 1;
                self.c <<= 1;
                self.ct -= 1;
                if self.ct == 0 {
                    self.byte_out();
                }
            }
        }
    }

    /// Encodes an integer `v` of `bits` width using a specific context `ctx`.
    pub fn encode_int_with_ctx(&mut self, v: i32, bits: i32, ctx: IntProc) {
        let mut prev = 1usize;
        for i in (0..bits).rev() {
            let bit = ((v >> i) & 1) != 0; // Explicitly make bit a bool
            let state_idx = self.int_ctx[ctx as usize][prev & 0xFF];
            self.encode_bit(state_idx, bit);
            prev = if bit { // Use bool comparison directly
                ((prev << 1) | 1) & 0x1ff | if prev & 0x100 != 0 { 0x100 } else { 0 }
            } else {
                (prev << 1) & 0x1ff | if prev & 0x100 != 0 { 0x100 } else { 0 }
            };
        }
    }

    /// Encodes an integer using the specified procedure.
    pub fn encode_integer(&mut self, proc: IntProc, value: i32) {
        if !(-2_000_000_000..=2_000_000_000).contains(&value) {
            panic!("Integer value out of encodable range");
        }
        let range_info =
            INT_ENC_RANGE.iter().find(|r| r.bot <= value && r.top >= value).expect("Value out of range");
        let val_unsigned = (if value < 0 { -value } else { value }) as u32 - range_info.delta;
        let context_idx = proc as usize;
        let mut prev_ctx = 0u32;

        // Encode integer bits
        for i in 0..range_info.intbits {
            let bit = (val_unsigned & (1 << (range_info.intbits - 1 - i))) != 0;
            let c_usize = (prev_ctx & 0xFF) as usize; // Cast c to usize for indexing
            let state = &self.int_ctx[context_idx][c_usize];
            self.encode_bit(*state, bit);
            prev_ctx = if prev_ctx & 0x100 != 0 {
                ((prev_ctx << 1) | bit as u32) & 0x1ff | 0x100
            } else {
                (prev_ctx << 1) | bit as u32
            };
        }

        // Encode prefix bits
        for i in 0..range_info.bits {
            let bit = (range_info.data & (1 << (range_info.bits - 1 - i))) != 0;
            self.encode_bit(self.iaid_ctx[prev_ctx as usize], bit);
            prev_ctx = if prev_ctx & 0x100 != 0 {
                ((prev_ctx << 1) | bit as u32) & 0x1ff | 0x100
            } else {
                (prev_ctx << 1) | bit as u32
            };
        }
    }

    /// Encodes a generic region payload, returning the raw arithmetically coded data.
    /// This is the high-level entry point for generic region encoding.
    pub fn encode_generic_region(
        &mut self,
        packed_data: &[u32],
        width: usize,
        height: usize,
        template: u8,
        at_pixels: &[(i8, i8)],
    ) -> Result<()> {
        self.encode_generic_region_inner(packed_data, width, height, template, at_pixels)
    }

    pub fn encode_generic_payload(
        image: &crate::jbig2sym::BitImage,
        template: u8,
        at_pixels: &[(i8, i8)],
    ) -> Result<Vec<u8>> {
        let packed_data = image.to_packed_words();
        let mut coder = Jbig2ArithCoder::new();

        // Correctly size the context buffer before encoding.
        let gbats = &at_pixels[..at_pixels.len().min(4)];
        let n_ctx = 1 << (10 + gbats.len());
        coder.context.resize(n_ctx, Jbig2ArithCoder::INITIAL_STATE);

        coder.encode_generic_region_inner(
            &packed_data,
            image.width,
            image.height,
            template,
            at_pixels,
        )?;
        coder.flush(true);
        Ok(coder.as_bytes().to_vec())
    }

    fn encode_generic_region_inner(&mut self, packed_data: &[u32], width: usize, height: usize, template: u8, at_pixels: &[(i8, i8)]) -> Result<()> {
        anyhow::ensure!(template == 0, "only template 0 is supported");

        /// Return the bit at *(x, y)*, respecting arbitrary image widths.
        #[inline(always)]
        fn sample(img: &[u32], w: usize, h: usize, x: i32, y: i32) -> u32 {
            if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
                return 0;
            }
            let words_per_row = ((w as usize) + 31) >> 5; // ceiling(w / 32)
            let idx_word = y as usize * words_per_row + (x as usize >> 5);
            let bit_pos = 31 - (x as usize & 31);
            (img[idx_word] >> bit_pos) & 1
        }

        // Annex A.3.5 table – MSB→LSB order for template 0
        const STATIC_OFFSETS: [(i8, i8); 10] = [
            (-1, -2), (0, -2), (1, -2), // row y-2
            (-2, -1), (-1, -1), (0, -1), (1, -1), // row y-1
            (-2, 0), (-1, 0), (0, 0), // current row (already coded pixels)
        ];

        let gbats = &at_pixels[..at_pixels.len().min(4)];

        for y in 0..height as i32 {
            for x in 0..width as i32 {
                let mut cx: usize = 0;

                for (bit, (dx, dy)) in STATIC_OFFSETS.iter().enumerate() {
                    let sample_val = sample(packed_data, width, height, x + *dx as i32, y + *dy as i32);
                    let shift = STATIC_OFFSETS.len() - 1 - bit;
                    cx |= (sample_val as usize) << shift;
                }

                for (bit, (dx, dy)) in gbats.iter().enumerate() {
                    let sample_val = sample(packed_data, width, height, x + *dx as i32, y + *dy as i32);
                    cx |= (sample_val as usize) << (10 + bit);
                }

                let pixel_val = sample(packed_data, width, height, x, y) != 0;
                self.encode_bit(cx, pixel_val);
            }
        }
        Ok(())
    }

    /// Returns the output buffer as a Vec<u8>.
    pub fn into_vec(mut self) -> Vec<u8> {
        self.flush(false); // Flush any pending data, do not add JBIG2 marker
        std::mem::take(&mut self.data) // Assuming data field; ensure it's correct
    }
}