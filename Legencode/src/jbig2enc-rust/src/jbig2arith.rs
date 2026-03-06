// src/arithmetic_coder.rs

//! A pure Rust port of the context-adaptive arithmetic coder from jbig2enc.
//! This module provides low-level bit-level encoding for JBIG2, mirroring the
//! functionality of `jbig2arith.cc` and `jbig2arith.h`. It is designed to be
//! integrated into a larger JBIG2 encoding pipeline where binarization and
//! preprocessing are already handled.

use anyhow::anyhow;
use anyhow::Result;
use once_cell::sync::Lazy;
use std::collections::HashMap;

#[cfg(not(feature = "trace_arith"))]
#[macro_use]
mod trace_stubs {
    #[allow(unused_macros)]
    macro_rules! debug {
        ($($arg:tt)*) => { println!($($arg)*); };
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
});

/// Context-adaptive arithmetic encoder for JBIG2.
const NUM_REFINEMENT_CX_STATES: usize = 17; // For GRTEMPLATE=0, contexts 0-16
/// Initial index into CTBL for new contexts, typically 0 (MPS=0, Qe=0x5601).

pub struct Jbig2ArithCoder {
    a: u16,        // Range register A
    c: u32,        // Code register C
    b: u8,         // Current byte being built
    ct: i8,        // Countdown register CT
    bp: isize,     // Byte position in output
    data: Vec<u8>, // Output data
    context: Vec<usize>,
    int_ctx: [[usize; 256]; 13], // Contexts for integer encoding, storing CTBL indices
    iaid_ctx: [usize; 512],      // Dynamically sized context for IAID symbols, storing CTBL indices
    refinement_contexts: [u8; 16], // Contexts for GRTEMPLATE=0 (16 states)
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

    /// Encodes a single bit `d` in the given context `ctx`.
    #[inline(always)]
    pub fn encode_bit(&mut self, ctx: usize, d: bool) {
        let state_idx = self.context[ctx];
        let state = FULL[state_idx];
        let qe = state.qe;
        let mps_val = state_idx >= 47;

        let mut renorm_needed = false;

        if d != mps_val {
            // LPS path
            self.a = self.a.wrapping_sub(qe);
            if self.a < qe {
                self.c = self.c.wrapping_add(qe as u32);
            } else {
                self.a = qe;
            }

            self.context[ctx] = state.nlps as usize;
            renorm_needed = true;
        } else {
            // MPS path
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
    #[inline]
    pub fn encode_int_with_ctx(&mut self, v: i32, bits: i32, ctx: IntProc) -> anyhow::Result<()> {
        let mut prev = 1usize;
        for i in (0..bits).rev() {
            let bit = ((v >> i) & 1) != 0; // Explicitly make bit a bool
            let state_idx = self.int_ctx[ctx as usize][prev & 0xFF];
            self.encode_bit(state_idx, bit);
            prev = if bit {
                // Use bool comparison directly
                ((prev << 1) | 1) & 0x1ff | if prev & 0x100 != 0 { 0x100 } else { 0 }
            } else {
                (prev << 1) & 0x1ff | if prev & 0x100 != 0 { 0x100 } else { 0 }
            };
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
        Ok(())
    }

    /// Encodes an Out-of-Band (OOB) value for a given integer procedure.
    /// The OOB signal is the bit pattern '1000' as per Table A.1 in the JBIG2 specification.
    pub fn encode_oob(&mut self, proc: IntProc) -> anyhow::Result<()> {
        let context_idx = proc as usize;
        let mut prev_ctx = 0u32; // Initial PREV value for integer coding (start with 0, same as encode_integer)

        // Encode the OOB bit pattern '1000'
        // Bit 1 (first bit is '1')
        let c_usize = (prev_ctx & 0xFF) as usize;
        let state = &self.int_ctx[context_idx][c_usize];
        self.encode_bit(*state, true);
        prev_ctx = if prev_ctx & 0x100 != 0 {
            ((prev_ctx << 1) | 1) & 0x1ff | 0x100
        } else {
            (prev_ctx << 1) | 1
        };

        // Bit 0 (second bit is '0')
        let c_usize = (prev_ctx & 0xFF) as usize;
        let state = &self.int_ctx[context_idx][c_usize];
        self.encode_bit(*state, false);
        prev_ctx = if prev_ctx & 0x100 != 0 {
            ((prev_ctx << 1) | 0) & 0x1ff | 0x100
        } else {
            (prev_ctx << 1) | 0
        };

        // Bit 0 (third bit is '0')
        let c_usize = (prev_ctx & 0xFF) as usize;
        let state = &self.int_ctx[context_idx][c_usize];
        self.encode_bit(*state, false);
        prev_ctx = if prev_ctx & 0x100 != 0 {
            ((prev_ctx << 1) | 0) & 0x1ff | 0x100
        } else {
            (prev_ctx << 1) | 0
        };

        // Bit 0 (fourth bit is '0')
        let c_usize = (prev_ctx & 0xFF) as usize;
        let state = &self.int_ctx[context_idx][c_usize];
        self.encode_bit(*state, false);

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
                println!("first non-zero word @ index {}", idx);
                let first_bit = (idx * 32) + (31 - packed[idx].leading_zeros() as usize);
                let row = first_bit / width;
                let col = first_bit % width;
                println!(
                    "first black pixel according to Rust packer: ({}, {})",
                    col, row
                );
            } else {
                println!("packed data all zero - no black pixels");
            }

            match template {
                0 => log::debug!("Using Template 0 (Standard 10-pixel context)"),
                1..=3 => log::warn!("Template {} is not fully tested", template),
                _ => log::error!("Invalid template {}", template),
            }
        }

        // Helper function matching the original C code's image_get
        let image_get = |x: i32, y: i32| -> u8 {
            if y < 0 || x >= width as i32 || y >= height as i32 {
                return 0;
            }
            if x < 0 {
                return 0;
            }
            Self::sample(packed, width, height, x, y) as u8
        };

        let mut context_distribution: HashMap<usize, usize> = HashMap::new();
        let progress_interval = (height as f32 * 0.1).ceil() as i32;
        let mut last_reported_progress = -1;

        for y in 0..height as i32 {
            // Report progress at 10% intervals
            let progress = (y * 10) / height as i32;
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

                // Reset for next interval
                context_distribution.clear();
            }

            // Initialize context values for this row, matching C code exactly
            let x = 0;
            let mut c1 = (image_get(x, y - 2) as u16) << 2
                | (image_get(x + 1, y - 2) as u16) << 1
                | (image_get(x + 2, y - 2) as u16);
            let mut c2 = (image_get(x, y - 1) as u16) << 3
                | (image_get(x + 1, y - 1) as u16) << 2
                | (image_get(x + 2, y - 1) as u16) << 1
                | (image_get(x + 3, y - 1) as u16);
            let mut c3: u16 = 0;

            for x in 0..width as i32 {
                // Form the context exactly as in the C code
                let tval = (c1 << 11) | (c2 << 4) | c3;
                let pixel = image_get(x, y);

                self.encode_bit(tval as usize, pixel != 0);
                *context_distribution.entry(tval as usize).or_insert(0) += 1;

                // Update context values for next pixel, matching C code
                c1 <<= 1;
                c2 <<= 1;
                c3 <<= 1;
                c1 |= image_get(x + 3, y - 2) as u16;
                c2 |= image_get(x + 4, y - 1) as u16;
                c3 |= pixel as u16;
                c1 &= 31; // keep only 5 bits
                c2 &= 127; // keep only 7 bits
                c3 &= 15; // keep only 4 bits
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
    fn sample(packed: &[u32], width: usize, height: usize, x: i32, y: i32) -> u32 {
        let result = if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
            #[cfg(debug_assertions)]
            if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                log::trace!(
                    "sample: out of bounds access x={}, y={} (width={}, height={})",
                    x,
                    y,
                    width,
                    height
                );
            }
            0
        } else {
            let words_per_row = (width + 31) / 32;
            let row_offset = (y as usize) * words_per_row;
            let word_in_row = (x as usize) / 32;
            let idx = row_offset + word_in_row;
            if idx >= packed.len() {
                #[cfg(debug_assertions)]
                log::warn!(
                    "sample: index out of bounds: idx={}, packed.len()={}",
                    idx,
                    packed.len()
                );
                return 0;
            }
            // MSb-first bit ordering within each word
            let word = packed[idx];
            let bit_pos = 31 - (x as usize % 32); // Keep MSB-first
            (word >> bit_pos) & 1
        };

        #[cfg(debug_assertions)]
        log::trace!("sample: x={}, y={} -> {}", x, y, result);

        result
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

    /// Returns the output buffer as a Vec<u8>.
    pub fn into_vec(mut self) -> Vec<u8> {
        self.flush(false); // Flush any pending data, do not add JBIG2 marker
        std::mem::take(&mut self.data) // Assuming data field; ensure it's correct
    }
}
// End of Jbig2ArithCoder implementation
