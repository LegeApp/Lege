// src/arithmetic_coder.rs

//! A pure Rust port of the context-adaptive arithmetic coder from jbig2enc.
//! This module provides low-level bit-level encoding for JBIG2, mirroring the
//! functionality of `jbig2arith.cc` and `jbig2arith.h`. It is designed to be
//! integrated into a larger JBIG2 encoding pipeline where binarization and
//! preprocessing are already handled.

use anyhow::Result;
use anyhow::anyhow;
use once_cell::sync::Lazy;
#[cfg(debug_assertions)]
use std::collections::HashMap;

#[cfg(not(feature = "trace_arith"))]
#[macro_use]
mod trace_stubs {
    #[allow(unused_macros)]
    macro_rules! debug {
        ($($arg:tt)*) => { std::convert::identity(format_args!($($arg)*)); };
    }
    #[allow(unused_macros)]
    macro_rules! trace {
        ($($arg:tt)*) => { std::convert::identity(format_args!($($arg)*)) };
    }
}

const JBIG2_MAX_CTX: usize = 65536;
const TPGD_CTX: u32 = 0x9B25;

const TEST_INPUT: &[u8] = &[
    0, 2, 0, 0x51, 0, 0, 0, 0xc0, 0x03, 0x52, 0x87, 0x2a, 0xaa, 0xaa, 0xaa, 0xaa, 0x82, 0xc0, 0x20,
    0, 0xfc, 0xd7, 0x9e, 0xf6, 0xbf, 0x7f, 0xed, 0x90, 0x4f, 0x46, 0xa3, 0xbf,
];

/// One probability-estimation state (ISO/IEC 14492 Table E.1)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct State {
    /// Qe value (16 bit)
    pub qe: u16,
    /// next state if the coded symbol was the **MPS**
    pub nmps: u8,
    /// next state if the coded symbol was the **LPS**
    pub nlps: u8,
    /// if 1, toggle the current MPS after coding an LPS
    pub switch: bool,
}

/// Context for arithmetic encoding/decoding
pub type ArithContext = usize;

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
    IntEncRange {
        bot: 0,
        top: 3,
        data: 0,
        bits: 2,
        delta: 0,
        intbits: 2,
    },
    IntEncRange {
        bot: -1,
        top: -1,
        data: 9,
        bits: 4,
        delta: 0,
        intbits: 0,
    },
    IntEncRange {
        bot: -3,
        top: -2,
        data: 5,
        bits: 3,
        delta: 2,
        intbits: 1,
    },
    IntEncRange {
        bot: 4,
        top: 19,
        data: 2,
        bits: 3,
        delta: 4,
        intbits: 4,
    },
    IntEncRange {
        bot: -19,
        top: -4,
        data: 3,
        bits: 3,
        delta: 4,
        intbits: 4,
    },
    IntEncRange {
        bot: 20,
        top: 83,
        data: 6,
        bits: 4,
        delta: 20,
        intbits: 6,
    },
    IntEncRange {
        bot: -83,
        top: -20,
        data: 7,
        bits: 4,
        delta: 20,
        intbits: 6,
    },
    IntEncRange {
        bot: 84,
        top: 339,
        data: 14,
        bits: 5,
        delta: 84,
        intbits: 8,
    },
    IntEncRange {
        bot: -339,
        top: -84,
        data: 15,
        bits: 5,
        delta: 84,
        intbits: 8,
    },
    IntEncRange {
        bot: 340,
        top: 4435,
        data: 30,
        bits: 6,
        delta: 340,
        intbits: 12,
    },
    IntEncRange {
        bot: -4435,
        top: -340,
        data: 31,
        bits: 6,
        delta: 340,
        intbits: 12,
    },
    IntEncRange {
        bot: 4436,
        top: 2_000_000_000,
        data: 62,
        bits: 6,
        delta: 4436,
        intbits: 32,
    },
    IntEncRange {
        bot: -2_000_000_000,
        top: -4436,
        data: 63,
        bits: 6,
        delta: 4436,
        intbits: 32,
    },
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
macro_rules! s {
    ( $qe:expr, $nmps:expr, $nlps:expr, $sw:expr ) => {
        State { qe: $qe, nmps: $nmps, nlps: $nlps, switch: $sw != 0 }
    };
}

/// Table E.1 – indices 0 … 46 (MPS = 0 half)
pub const BASE: [State; 47] = [
    s!(0x5601, 1, 1, 1),
    s!(0x3401, 2, 6, 0),
    s!(0x1801, 3, 9, 0),
    s!(0x0AC1, 4, 12, 0),
    s!(0x0521, 5, 29, 0),
    s!(0x0221, 38, 33, 0),
    s!(0x5601, 7, 6, 1),
    s!(0x5401, 8, 14, 0),
    s!(0x4801, 9, 14, 0),
    s!(0x3801, 10, 14, 0),
    s!(0x3001, 11, 17, 0),
    s!(0x2401, 12, 18, 0),
    s!(0x1C01, 13, 20, 0),
    s!(0x1601, 29, 21, 0),
    s!(0x5601, 15, 14, 1),
    s!(0x5401, 16, 14, 0),
    s!(0x5101, 17, 15, 0),
    s!(0x4801, 18, 16, 0),
    s!(0x3801, 19, 17, 0),
    s!(0x3401, 20, 18, 0),
    s!(0x3001, 21, 19, 0),
    s!(0x2801, 22, 19, 0),
    s!(0x2401, 23, 20, 0),
    s!(0x2201, 24, 21, 0),
    s!(0x1C01, 25, 22, 0),
    s!(0x1801, 26, 23, 0),
    s!(0x1601, 27, 24, 0),
    s!(0x1401, 28, 25, 0),
    s!(0x1201, 29, 26, 0),
    s!(0x1101, 30, 27, 0),
    s!(0x0AC1, 31, 28, 0),
    s!(0x09C1, 32, 29, 0),
    s!(0x08A1, 33, 30, 0),
    s!(0x0521, 34, 31, 0),
    s!(0x0441, 35, 32, 0),
    s!(0x02A1, 36, 33, 0),
    s!(0x0221, 37, 34, 0),
    s!(0x0141, 38, 35, 0),
    s!(0x0111, 39, 36, 0),
    s!(0x0085, 40, 37, 0),
    s!(0x0049, 41, 38, 0),
    s!(0x0025, 42, 39, 0),
    s!(0x0015, 43, 40, 0),
    s!(0x0009, 44, 41, 0),
    s!(0x0005, 45, 42, 0),
    s!(0x0001, 45, 43, 0),
    s!(0x5601, 46, 46, 0), // dummy “all done” state
];

/// Build the 94-state table at start-up.
pub(crate) static FULL: Lazy<[State; 94]> = Lazy::new(|| {
    let mut t = [BASE[0]; 94];

    for i in 0..47 {
        let s = BASE[i];

        // Lower half: MPS = 0
        t[i] = State {
            qe: s.qe,
            nmps: s.nmps, // stays in lower half
            nlps: if s.switch { s.nlps + 47 } else { s.nlps },
            switch: s.switch,
        };

        // Upper half: MPS = 1
        t[i + 47] = State {
            qe: s.qe,
            nmps: s.nmps + 47, // stays in upper half
            // If LPS flips the MPS we must leave the upper half
            nlps: if s.switch { s.nlps } else { s.nlps + 47 },
            switch: s.switch,
        };
    }

    t
});

/// Context-adaptive arithmetic encoder for JBIG2.
const NUM_REFINEMENT_CONTEXTS: usize = 1 << 13; // 8192 for GRTEMPLATE=0 (13-bit context)
/// Initial index into CTBL for new contexts, typically 0 (MPS=0, Qe=0x5601).

pub struct Jbig2ArithCoder {
    a: u16,        // Range register A
    c: u32,        // Code register C
    b: u8,         // Current byte being built
    ct: i8,        // Countdown register CT
    bp: isize,     // Byte position in output
    data: Vec<u8>, // Output data
    context: Vec<usize>,
    int_ctx: [[usize; 512]; 13], // Contexts for integer encoding, storing CTBL indices
    iaid_ctx: Vec<usize>,        // Dynamically sized context tree for IAID symbols
    refinement_contexts: Vec<usize>, // Contexts for refinement region (GRTEMPLATE=0: 8192 states)
}

impl Jbig2ArithCoder {
    /// Encodes a generic region payload using high-level config, returning the raw arithmetically coded data.
    pub fn encode_generic_payload_cfg(
        image: &crate::jbig2sym::BitImage,
        cfg: &crate::jbig2structs::GenericRegionConfig,
    ) -> Result<Vec<u8>> {
        // Validate config
        cfg.validate().map_err(|e| anyhow::anyhow!(e))?;
        // Use the AT pixels from config - the config now has the correct defaults for template 0
        let at_pixels = cfg.at_pixels.clone();
        // Delegate to existing payload encoder
        Self::encode_generic_payload(image, cfg.template, &at_pixels)
    }

    /// Encodes a generic region (internal) payload using high-level config, returning the raw arithmetically coded data.
    pub fn encode_generic_region_cfg(
        &mut self,
        packed_data: &[u32],
        cfg: &crate::jbig2structs::GenericRegionConfig,
    ) -> Result<()> {
        // Validate config
        cfg.validate().map_err(|e| anyhow::anyhow!(e))?;
        // Prepare parameters
        let width = cfg.width as usize;
        let height = cfg.height as usize;
        let template = cfg.template;
        // Use the AT pixels from config - the config now has the correct defaults for template 0
        let at_pixels = cfg.at_pixels.clone();
        // Use only up to 4 AT pixels - for template 0, this will use the default values
        let mut at = [(0i8, 0i8); 4];
        for (i, &p) in at_pixels.iter().take(4).enumerate() {
            at[i] = p;
        }
        // Delegate to existing inner encoder
        self.encode_generic_region_inner(packed_data, width, height, template, &at)
    }
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
            bp: 0,
            data: Vec::new(),
            context: vec![Self::INITIAL_STATE; JBIG2_MAX_CTX],
            int_ctx: [[0; 512]; 13],
            iaid_ctx: Vec::new(),
            refinement_contexts: vec![0; NUM_REFINEMENT_CONTEXTS],
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
    pub fn finalize(&mut self, _data: &mut Vec<u8>) -> anyhow::Result<()> {
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

    /// Writes one (or two) bytes to the output buffer according to Annex E.9.
    #[inline]
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

    //// Finalizes the arithmetic coding stream per JBIG2 Annex A.3.6.
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

        if self.bp >= 0 && (with_marker || self.b != 0xFF) {
            self.data.push(self.b);
        }
        if with_marker {
            self.data.push(0xFF);
            self.data.push(0xAC);
        }
    }

    #[inline(always)]
    fn encode_bit_with_state_idx(&mut self, state_idx: usize, d: bool) -> usize {
        let state = FULL[state_idx];
        let qe = state.qe;
        let mps_val = state_idx >= 47;
        let mut next_state = state_idx;
        let mut renorm_needed = false;

        if d != mps_val {
            self.a = self.a.wrapping_sub(qe);
            if self.a < qe {
                self.c = self.c.wrapping_add(qe as u32);
            } else {
                self.a = qe;
            }
            next_state = state.nlps as usize;
            renorm_needed = true;
        } else {
            self.a = self.a.wrapping_sub(qe);
            if (self.a & 0x8000) == 0 {
                if self.a < qe {
                    self.a = qe;
                } else {
                    self.c = self.c.wrapping_add(qe as u32);
                }
                next_state = state.nmps as usize;
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

        next_state
    }

    #[inline(always)]
    fn update_prev(prev: u32, bit: bool) -> u32 {
        if (prev & 0x100) != 0 {
            (((prev << 1) | (bit as u32)) & 0x1ff) | 0x100
        } else {
            (prev << 1) | (bit as u32)
        }
    }

    /// Encodes a single bit `d` in the given context `ctx`.
    #[inline(always)]
    pub fn encode_bit(&mut self, ctx: usize, d: bool) {
        let next_state = self.encode_bit_with_state_idx(self.context[ctx], d);
        self.context[ctx] = next_state;
    }

    /// Encodes an integer `v` of `bits` width using a specific context `ctx`.
    #[inline]
    pub fn encode_int_with_ctx(&mut self, v: i32, bits: i32, ctx: IntProc) -> anyhow::Result<()> {
        if bits <= 0 || bits > 32 {
            return Err(anyhow!("encode_int_with_ctx: invalid bit width {}", bits));
        }
        let mut prev = 1u32;
        let proc_idx = ctx as usize;
        for i in (0..bits).rev() {
            let bit = ((v >> i) & 1) != 0;
            let idx = (prev & 0x1ff) as usize;
            let next_state = self.encode_bit_with_state_idx(self.int_ctx[proc_idx][idx], bit);
            self.int_ctx[proc_idx][idx] = next_state;
            prev = Self::update_prev(prev, bit);
        }
        Ok(())
    }

    /// Encodes an IAID-coded symbol identifier using the dedicated IAID context tree.
    pub fn encode_iaid(&mut self, value: u32, bits: u8) -> anyhow::Result<()> {
        if bits == 0 || bits > 24 {
            return Err(anyhow!("encode_iaid: invalid symbol id bit width {}", bits));
        }
        let needed = 1usize << bits;
        if self.iaid_ctx.len() < needed {
            self.iaid_ctx.resize(needed, 0);
        }
        let mut prev: u32 = 1;
        let mut shifted = value << (32 - bits);
        for _ in 0..bits {
            let bit = (shifted & 0x8000_0000) != 0;
            let idx = (prev as usize).min(self.iaid_ctx.len() - 1);
            let next_state = self.encode_bit_with_state_idx(self.iaid_ctx[idx], bit);
            self.iaid_ctx[idx] = next_state;
            prev = (prev << 1) | (bit as u32);
            shifted <<= 1;
        }
        Ok(())
    }

    /// Encodes an integer using the specified procedure.
    pub fn encode_integer(&mut self, proc: IntProc, value: i32) -> anyhow::Result<()> {
        if !(-2_000_000_000..=2_000_000_000).contains(&value) {
            return Err(anyhow!("Integer value out of encodable range"));
        }
        let range_info = INT_ENC_RANGE
            .iter()
            .find(|r| r.bot <= value && r.top >= value)
            .expect("Value out of range");
        let mut v = if value < 0 { -value } else { value } as u32;
        v = v.saturating_sub(range_info.delta);

        let context_idx = proc as usize;
        let mut prev_ctx = 1u32;

        // Prefix bits are emitted LSB-first from `data`, matching jbig2enc C implementation.
        let mut prefix = range_info.data;
        for _ in 0..range_info.bits {
            let bit = (prefix & 1) != 0;
            let idx = (prev_ctx & 0x1ff) as usize;
            let next_state = self.encode_bit_with_state_idx(self.int_ctx[context_idx][idx], bit);
            self.int_ctx[context_idx][idx] = next_state;
            prev_ctx = Self::update_prev(prev_ctx, bit);
            prefix >>= 1;
        }

        // Encode the range payload as MSB-first bits.
        if range_info.intbits > 0 {
            v <<= 32 - range_info.intbits;
            for _ in 0..range_info.intbits {
                let bit = (v & 0x8000_0000) != 0;
                let idx = (prev_ctx & 0x1ff) as usize;
                let next_state =
                    self.encode_bit_with_state_idx(self.int_ctx[context_idx][idx], bit);
                self.int_ctx[context_idx][idx] = next_state;
                prev_ctx = Self::update_prev(prev_ctx, bit);
                v <<= 1;
            }
        }
        Ok(())
    }

    /// Encodes an Out-of-Band (OOB) value for a given integer procedure.
    /// The OOB signal is the bit pattern '1000' as per Table A.1 in the JBIG2 specification.
    pub fn encode_oob(&mut self, proc: IntProc) -> anyhow::Result<()> {
        let context_idx = proc as usize;
        for (idx, bit) in [
            (1usize, true),
            (3usize, false),
            (6usize, false),
            (12usize, false),
        ] {
            let next_state = self.encode_bit_with_state_idx(self.int_ctx[context_idx][idx], bit);
            self.int_ctx[context_idx][idx] = next_state;
        }

        Ok(())
    }

    /// Encodes a generic region payload, returning the raw arithmetically coded data.
    /// This is the high-level entry point for generic region encoding.
    pub fn encode_generic_region(
        &mut self,
        packed_data: &[u32],
        width: usize,
        height: usize,
        template: u8,
        _at_pixels: &[(i8, i8)],
    ) -> Result<()> {
        #[cfg(debug_assertions)]
        {
            log::debug!(
                "encode_generic_region: width={}, height={}, template={}",
                width,
                height,
                template
            );

            if template != 0 {
                log::warn!(
                    "template {} is not fully tested, only template 0 is well-supported",
                    template
                );
            }

            if width == 0 || height == 0 {
                log::warn!(
                    "empty region ({}x{}) provided to encode_generic_region",
                    width,
                    height
                );
                return Ok(());
            }

            // Verify packed data size is sufficient
            let expected_words = ((width + 31) / 32) * height;
            if packed_data.len() < expected_words {
                log::warn!(
                    "packed_data size {} is less than expected {} for {}x{} bitmap",
                    packed_data.len(),
                    expected_words,
                    width,
                    height
                );
            }
        }

        // Use the provided AT pixels for template 0 (C encoder compatibility)
        let mut at = [(0i8, 0i8); 4];
        for (i, &p) in _at_pixels.iter().take(4).enumerate() {
            at[i] = p;
        }
        self.encode_generic_region_inner(packed_data, width, height, template, &at)
    }

    pub fn encode_generic_payload(
        image: &crate::jbig2sym::BitImage,
        template: u8,
        _at_pixels: &[(i8, i8)],
    ) -> Result<Vec<u8>> {
        #[cfg(debug_assertions)]
        {
            log::debug!(
                "encode_generic_payload: image={:?}x{:?}, template={}",
                image.width,
                image.height,
                template
            );
        }

        let packed_data = image.packed_words();

        // Verify bit-order correctness: first black pixel should match between image and packed data
        if let Some(expected_first_pixel) = crate::jbig2enc::first_black_pixel(image) {
            let actual_first_pixel = crate::jbig2sym::first_black_pixel_in_packed(
                packed_data,
                image.width,
                image.height,
            );
            assert_eq!(
                actual_first_pixel,
                Some(expected_first_pixel),
                "bit-order / row-order mismatch in packer! Expected first black pixel at {:?}, got {:?}",
                expected_first_pixel,
                actual_first_pixel
            );
        }

        let mut coder = Jbig2ArithCoder::new();

        // Use provided AT pixels for context formation (C encoder compatibility)
        let mut at = [(0i8, 0i8); 4];
        for (i, &p) in _at_pixels.iter().take(4).enumerate() {
            at[i] = p;
        }
        coder.encode_generic_region_inner(
            packed_data,
            image.width,
            image.height,
            template,
            &at, // Use the provided AT pixels
        )?;

        // Finalize the arithmetic coder state with the JBIG2 terminator
        coder.flush(true);
        // Get the result
        let result = coder.data;

        debug_assert!(
            !result.is_empty(),
            "empty arithmetic stream – context generation broken"
        );

        log::debug!(
            "generic-region payload {} bytes (template={})",
            result.len(),
            template
        );

        Ok(result)
    }

    /// Encode a generic region using the specified template, conforming to
    /// ITU-T T.88 §7.4 and Annex E.
    pub fn encode_generic_region_inner(
        &mut self,
        packed: &[u32],
        width: usize,
        height: usize,
        template: u8,
        _at: &[(i8, i8); 4],
    ) -> Result<()> {
        #[cfg(debug_assertions)]
        {
            log::debug!(
                "encode_generic_region_inner: {}x{}, template={}",
                width,
                height,
                template
            );
            log::debug!(
                "  packed data: {} words ({} bytes), expected {} words for {}x{} bitmap",
                packed.len(),
                packed.len() * 4,
                ((width + 31) / 32) * height,
                width,
                height
            );

            if let Some(idx) = packed.iter().position(|w| *w != 0) {
                let first_bit = (idx * 32) + (31 - packed[idx].leading_zeros() as usize);
                let row = first_bit / width;
                let col = first_bit % width;
                log::debug!(
                    "first black pixel according to Rust packer: ({}, {})",
                    col,
                    row
                );
            } else {
                log::debug!("packed data all zero - no black pixels");
            }

            match template {
                0 => log::debug!("Using Template 0 (Standard 10-pixel context)"),
                1..=3 => log::warn!("Template {} is not fully tested", template),
                _ => log::error!("Invalid template {}", template),
            }
        }

        let words_per_row = (width + 31) / 32;

        #[cfg(debug_assertions)]
        let mut context_distribution: HashMap<usize, usize> = HashMap::new();
        #[cfg(debug_assertions)]
        let mut last_reported_progress = -1;

        for y in 0..height {
            #[cfg(debug_assertions)]
            {
                let progress = (y as i32 * 10) / height as i32;
                if progress > last_reported_progress {
                    last_reported_progress = progress;
                    let unique_contexts = context_distribution.len();
                    let total_samples: usize = context_distribution.values().sum();
                    let avg_occurrences = if unique_contexts > 0 {
                        total_samples / unique_contexts
                    } else {
                        0
                    };

                    log::debug!(
                        "Progress: {}% - Line {}/{} - Contexts: {} unique (avg {:.1} uses/context)",
                        progress * 10,
                        y,
                        height,
                        unique_contexts,
                        avg_occurrences as f32
                    );
                    context_distribution.clear();
                }
            }

            let row = &packed[y * words_per_row..(y + 1) * words_per_row];
            let prev1 = if y > 0 {
                Some(&packed[(y - 1) * words_per_row..y * words_per_row])
            } else {
                None
            };
            let prev2 = if y > 1 {
                Some(&packed[(y - 2) * words_per_row..(y - 1) * words_per_row])
            } else {
                None
            };

            let mut c1 = (Self::sample_row(prev2, width, 0) << 2)
                | (Self::sample_row(prev2, width, 1) << 1)
                | Self::sample_row(prev2, width, 2);
            let mut c2 = (Self::sample_row(prev1, width, 0) << 3)
                | (Self::sample_row(prev1, width, 1) << 2)
                | (Self::sample_row(prev1, width, 2) << 1)
                | Self::sample_row(prev1, width, 3);
            let mut c3: u16 = 0;

            for x in 0..width {
                let tval = (c1 << 11) | (c2 << 4) | c3;
                let pixel = Self::sample_row(Some(row), width, x);

                self.encode_bit(tval as usize, pixel != 0);
                #[cfg(debug_assertions)]
                {
                    *context_distribution.entry(tval as usize).or_insert(0) += 1;
                }

                c1 = ((c1 << 1) | Self::sample_row(prev2, width, x + 3)) & 31;
                c2 = ((c2 << 1) | Self::sample_row(prev1, width, x + 4)) & 127;
                c3 = ((c3 << 1) | pixel) & 15;
            }

            #[cfg(debug_assertions)]
            {
                log::trace!("Finished encoding line {}/{}", y + 1, height);
            }
        }

        Ok(())
    }

    /// Helper function to sample a single bit from a packed bitmap.
    /// Returns 0 or 1, or 0 if out of bounds.
    ///
    /// Reads from packed data in row-major order with MSb-first bit orientation,
    /// matching the format produced by BitImage::to_packed_words().
    #[inline(always)]
    fn sample_row(row: Option<&[u32]>, width: usize, x: usize) -> u16 {
        if x >= width {
            return 0;
        }
        let Some(row) = row else {
            return 0;
        };
        let word = row[x >> 5];
        ((word >> (31 - (x & 31))) & 1) as u16
    }
    #[cfg(feature = "line_verify")]
    fn verify_line_contexts(
        width: usize,
        y: usize,
        static_buf: &[bool],  // pixels from all previous lines
        current_buf: &[bool], // pixels encoded so far on this line
        enc_cxs: &[usize],    // encoder contexts for this line
        at: &[(i8, i8); 4],
    ) -> Result<(), String> {
        const STATIC: [(i8, i8); 6] = [(-1, -2), (0, -2), (1, -2), (-2, -1), (-1, -1), (0, -1)];

        let mut idx = 0;
        for x in 0..width as i32 {
            // --- rebuild cx exactly as decoder will ---
            let mut cx = 0usize;
            let mut bp = 0;

            for &(dx, dy) in &STATIC {
                let xx = x + dx as i32;
                let yy = y as i32 + dy as i32;
                let bit = if yy < 0 || xx < 0 || xx >= width as i32 || yy >= y as i32 {
                    false
                } else {
                    static_buf[yy as usize * width + xx as usize]
                };
                cx |= (bit as usize) << bp;
                bp += 1;
            }

            // four AT pixels
            for (dx, dy) in at {
                let xx = x + *dx as i32;
                let yy = y as i32 + *dy as i32;
                let bit = if yy == y as i32 {
                    if xx < 0 || xx >= x {
                        false
                    } else {
                        current_buf[xx as usize]
                    }
                } else if yy == y as i32 - 1 {
                    if xx < 0 || xx >= width as i32 || yy < 0 {
                        false
                    } else {
                        static_buf[yy as usize * width + xx as usize]
                    }
                } else if yy < 0 || xx < 0 || xx >= width as i32 || yy >= y as i32 {
                    false
                } else {
                    static_buf[yy as usize * width + xx as usize]
                };
                cx |= (bit as usize) << bp;
                bp += 1;
            }

            // --- compare with encoder ---
            if cx != enc_cxs[idx] {
                return Err(format!(
                    "Context mismatch @ ({},{})  enc={:#05x}  dec={:#05x}",
                    x, y, enc_cxs[idx], cx
                ));
            }
            idx += 1;
        }
        Ok(())
    }

    /// Encodes a refinement region using the JBIG2 generic refinement procedure (§6.3).
    ///
    /// The target bitmap is encoded pixel-by-pixel using a 13-bit context formed
    /// from 3 already-coded target pixels + 9 reference pixels (3×3 neighbourhood)
    /// + 1 adaptive template (AT) pixel from the reference. This matches the
    /// GRTEMPLATE=0 context layout from T.88 Table 12, verified against jbig2dec.
    ///
    /// # Arguments
    /// * `target` - The bitmap to encode (the actual instance glyph)
    /// * `reference` - The prototype/dictionary symbol used as predictor
    /// * `grdx` - Horizontal offset: reference pos = target pos − (grdx, grdy)
    /// * `grdy` - Vertical offset
    /// * `template` - GRTEMPLATE (0 or 1). Only 0 is implemented.
    /// * `grat` - Adaptive template pixel offsets; for GRTEMPLATE=0 supply one (x,y) pair
    pub fn encode_refinement_region(
        &mut self,
        target: &crate::jbig2sym::BitImage,
        reference: &crate::jbig2sym::BitImage,
        grdx: i32,
        grdy: i32,
        template: u8,
        grat: &[(i8, i8)],
    ) -> Result<()> {
        // GRTEMPLATE=0: 13-bit context → 8192 possible contexts
        // Context bit layout (matching jbig2dec for interoperability):
        //   bit  0: ref(rx-1, ry-1)    upper-left in reference
        //   bit  1: ref(rx,   ry-1)    upper-center
        //   bit  2: ref(rx+1, ry-1)    upper-right
        //   bit  3: ref(rx-1, ry)      middle-left
        //   bit  4: ref(rx,   ry)      center (the prediction pixel)
        //   bit  5: ref(rx+1, ry)      middle-right
        //   bit  6: target(x-1, y)     left (already coded)
        //   bit  7: ref(rx-1, ry+1)    lower-left
        //   bit  8: ref(rx,   ry+1)    lower-center
        //   bit  9: ref(rx+1, ry+1)    lower-right
        //   bit 10: target(x+1, y-1)   above-right (already coded)
        //   bit 11: target(x, y-1)     above (already coded)
        //   bit 12: target AT pixel    adaptive template (GRAT1, nominal (-1,-1))
        //
        // T.88 §6.3.5.3 Figure 12: GRTEMPLATE=0 has TWO adaptive pixels — GRAT1 in
        // the already-coded (target/GRREG) bitmap and GRAT2 in the reference. With
        // nominal AT = (-1,-1), GRAT2 coincides with the fixed reference pixel at
        // (rx-1, ry-1) (bit 0), so the distinct 13th pixel is the *target* GRAT1 at
        // (x-1, y-1). Reading bit 12 from the reference instead duplicates a
        // reference pixel and omits the target AT pixel, which mismatches jbig2dec's
        // context (blank decode + arithmetic desync).

        let grat_x = grat.first().map_or(-1i8, |g| g.0);
        let grat_y = grat.first().map_or(-1i8, |g| g.1);

        for y in 0..target.height as i32 {
            for x in 0..target.width as i32 {
                // Reference center for this target pixel
                let rx = x - grdx;
                let ry = y - grdy;

                let mut cx: usize = 0;

                // Reference pixels (bits 0-5, 7-9)
                cx |= reference.get_pixel_safely(rx - 1, ry - 1) as usize;
                cx |= (reference.get_pixel_safely(rx, ry - 1) as usize) << 1;
                cx |= (reference.get_pixel_safely(rx + 1, ry - 1) as usize) << 2;
                cx |= (reference.get_pixel_safely(rx - 1, ry) as usize) << 3;
                cx |= (reference.get_pixel_safely(rx, ry) as usize) << 4;
                cx |= (reference.get_pixel_safely(rx + 1, ry) as usize) << 5;

                // Target pixel (bit 6) — left neighbor, already coded
                cx |= (target.get_pixel_safely(x - 1, y) as usize) << 6;

                // Reference lower row (bits 7-9)
                cx |= (reference.get_pixel_safely(rx - 1, ry + 1) as usize) << 7;
                cx |= (reference.get_pixel_safely(rx, ry + 1) as usize) << 8;
                cx |= (reference.get_pixel_safely(rx + 1, ry + 1) as usize) << 9;

                // Target pixels (bits 10-11) — above, already coded
                cx |= (target.get_pixel_safely(x + 1, y - 1) as usize) << 10;
                cx |= (target.get_pixel_safely(x, y - 1) as usize) << 11;

                // Adaptive template pixel GRAT1 from the target/GRREG bitmap (bit 12).
                if template == 0 {
                    cx |= (target.get_pixel_safely(x + grat_x as i32, y + grat_y as i32) as usize)
                        << 12;
                }

                let pixel = target.get_pixel_safely(x, y) != 0;

                // Encode using the dedicated refinement context array
                let next_state =
                    self.encode_bit_with_state_idx(self.refinement_contexts[cx], pixel);
                self.refinement_contexts[cx] = next_state;
            }
        }

        Ok(())
    }

    /// Resets only the refinement contexts, keeping other state intact.
    /// Call this between successive refinement regions within the same text region
    /// segment so that each symbol instance starts from a clean prediction state.
    pub fn reset_refinement_contexts(&mut self) {
        self.refinement_contexts.fill(0);
    }

    /// Returns the output buffer as a Vec<u8>.
    pub fn into_vec(mut self) -> Vec<u8> {
        self.flush(false); // Flush any pending data, do not add JBIG2 marker
        std::mem::take(&mut self.data) // Assuming data field; ensure it's correct
    }
}
// End of Jbig2ArithCoder implementation
