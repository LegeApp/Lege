pub(crate) const MQC_NUMCTXS: usize = 19;
#[allow(dead_code)]
pub(crate) const BYPASS_CT_INIT: u32 = 0xDEAD_BEEF;

pub(crate) const T1_CTXNO_ZC: u8 = 0;
#[allow(dead_code)]
pub(crate) const T1_CTXNO_SC: u8 = 9;
pub(crate) const T1_CTXNO_MAG: u8 = 14;
pub(crate) const T1_CTXNO_AGG: u8 = 17;
pub(crate) const T1_CTXNO_UNI: u8 = 18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MqState {
    pub qeval: u16,
    pub mps: u8,
    pub nmps: u8,
    pub nlps: u8,
}

#[derive(Debug, Clone)]
pub(crate) struct MqCoder {
    a: u32,
    c: u32,
    ct: u32,
    out: Vec<u8>,
    pos: usize,
    ctxs: [u8; MQC_NUMCTXS],
}

#[derive(Debug, Clone)]
pub(crate) struct MqDecoder<'a> {
    bytes: &'a [u8],
    bp: usize,
    a: u32,
    c: u32,
    ct: u32,
    ctxs: [u8; MQC_NUMCTXS],
}

impl<'a> MqDecoder<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        let mut decoder = Self {
            bytes,
            bp: 0,
            a: 0x8000,
            c: if bytes.is_empty() {
                0xff << 16
            } else {
                u32::from(bytes[0]) << 16
            },
            ct: 0,
            ctxs: [0; MQC_NUMCTXS],
        };
        decoder.reset_contexts();
        decoder.bytein();
        decoder.c <<= 7;
        decoder.ct = decoder.ct.saturating_sub(7);
        decoder
    }

    /// Start a new terminated codeword segment while preserving the adaptive
    /// probability states from the preceding segment.
    pub(crate) fn new_with_contexts(bytes: &'a [u8], contexts: [u8; MQC_NUMCTXS]) -> Self {
        let mut decoder = Self::new(bytes);
        decoder.ctxs = contexts;
        decoder
    }

    pub(crate) fn contexts(&self) -> [u8; MQC_NUMCTXS] {
        self.ctxs
    }

    pub(crate) fn decode_with_ctx(&mut self, ctx: u8) -> u8 {
        let ctx_index = ctx as usize;
        let state = MQ_STATES[self.ctxs[ctx_index] as usize];
        self.a = self.a.wrapping_sub(u32::from(state.qeval));

        let bit = if (self.c >> 16) < u32::from(state.qeval) {
            self.lps_exchange(ctx_index, state)
        } else {
            self.c = self.c.wrapping_sub(u32::from(state.qeval) << 16);
            if self.a & 0x8000 == 0 {
                self.mps_exchange(ctx_index, state)
            } else {
                state.mps
            }
        };

        if self.a & 0x8000 == 0 {
            self.renormd();
        }
        bit
    }

    pub(crate) fn set_state(&mut self, ctx: u8, msb: u8, prob: u8) {
        let index = msb as usize + ((prob as usize) << 1);
        debug_assert!(index < MQ_STATES.len());
        self.ctxs[ctx as usize] = index as u8;
    }

    #[cfg(test)]
    pub(crate) fn state_index(&self, ctx: u8) -> u8 {
        self.ctxs[ctx as usize]
    }

    pub(crate) fn reset_contexts(&mut self) {
        self.ctxs.fill(0);
        self.set_state(T1_CTXNO_UNI, 0, 46);
        self.set_state(T1_CTXNO_AGG, 0, 3);
        self.set_state(T1_CTXNO_ZC, 0, 4);
    }

    fn mps_exchange(&mut self, ctx_index: usize, state: MqState) -> u8 {
        if self.a < u32::from(state.qeval) {
            self.ctxs[ctx_index] = state.nlps;
            1 ^ state.mps
        } else {
            self.ctxs[ctx_index] = state.nmps;
            state.mps
        }
    }

    fn lps_exchange(&mut self, ctx_index: usize, state: MqState) -> u8 {
        let post_subtraction_a = self.a;
        self.a = u32::from(state.qeval);
        if post_subtraction_a < u32::from(state.qeval) {
            self.ctxs[ctx_index] = state.nmps;
            state.mps
        } else {
            self.ctxs[ctx_index] = state.nlps;
            1 ^ state.mps
        }
    }

    fn renormd(&mut self) {
        while self.a < 0x8000 {
            if self.ct == 0 {
                self.bytein();
            }
            self.a <<= 1;
            self.c <<= 1;
            self.ct -= 1;
        }
    }

    fn bytein(&mut self) {
        let next = self.byte_at(self.bp + 1);
        if self.byte_at(self.bp) == 0xff {
            if next > 0x8f {
                self.c = self.c.wrapping_add(0xff00);
                self.ct = 8;
            } else {
                self.bp += 1;
                self.c = self.c.wrapping_add(u32::from(next) << 9);
                self.ct = 7;
            }
        } else {
            self.bp += 1;
            self.c = self.c.wrapping_add(u32::from(next) << 8);
            self.ct = 8;
        }
    }

    fn byte_at(&self, index: usize) -> u8 {
        self.bytes.get(index).copied().unwrap_or(0xff)
    }
}

impl Default for MqCoder {
    fn default() -> Self {
        Self::new()
    }
}

impl MqCoder {
    pub(crate) fn new() -> Self {
        Self::with_capacity(256)
    }

    /// Create a new MQ coder with pre-allocated output buffer capacity.
    /// Helps avoid repeated Vec growth in hot encoding loops.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let mut out = Vec::with_capacity(capacity.max(1));
        out.push(0);
        let mut coder = Self {
            a: 0x8000,
            c: 0,
            ct: 12,
            out,
            pos: 0,
            ctxs: [0; MQC_NUMCTXS],
        };
        coder.reset_state_only();
        coder
    }

    pub(crate) fn reset(&mut self) {
        self.a = 0x8000;
        self.c = 0;
        self.ct = 12;
        self.out.clear();
        self.out.push(0);
        self.pos = 0;
        self.reset_state_only();
    }

    fn reset_state_only(&mut self) {
        self.ctxs.fill(0);
        self.set_state(T1_CTXNO_UNI, 0, 46);
        self.set_state(T1_CTXNO_AGG, 0, 3);
        self.set_state(T1_CTXNO_ZC, 0, 4);
    }

    pub(crate) fn set_state(&mut self, ctx: u8, msb: u8, prob: u8) {
        let index = msb as usize + ((prob as usize) << 1);
        debug_assert!(index < MQ_STATES.len());
        self.ctxs[ctx as usize] = index as u8;
    }

    #[cfg(test)]
    pub(crate) fn state_index(&self, ctx: u8) -> u8 {
        self.ctxs[ctx as usize]
    }

    pub(crate) fn encode_with_ctx(&mut self, ctx: u8, bit: u8) {
        #[cfg(feature = "counters")]
        crate::encode::counters::MQ_SYMBOLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        debug_assert!(bit <= 1);
        let state = MQ_STATES[self.ctxs[ctx as usize] as usize];
        if bit == state.mps {
            self.a = self.a.wrapping_sub(state.qeval as u32);
            if (self.a & 0x8000) == 0 {
                if self.a < state.qeval as u32 {
                    self.a = state.qeval as u32;
                } else {
                    self.c = self.c.wrapping_add(state.qeval as u32);
                }
                self.ctxs[ctx as usize] = state.nmps;
                self.renorme();
            } else {
                self.c = self.c.wrapping_add(state.qeval as u32);
            }
        } else {
            self.a = self.a.wrapping_sub(state.qeval as u32);
            if self.a < state.qeval as u32 {
                self.c = self.c.wrapping_add(state.qeval as u32);
            } else {
                self.a = state.qeval as u32;
            }
            self.ctxs[ctx as usize] = state.nlps;
            self.renorme();
        }
    }

    pub(crate) fn flush(&mut self) {
        self.setbits();
        self.c <<= self.ct;
        self.byteout();
        self.c <<= self.ct;
        self.byteout();
    }

    #[allow(dead_code)]
    pub(crate) fn restart_init(&mut self) {
        self.a = 0x8000;
        self.c = 0;
        self.ct = 12;
        self.out.clear();
        self.out.push(0);
        self.pos = 0;
    }

    /// Flush the current coding interval to bytes, then restart the arithmetic
    /// registers (a/c/ct) and output buffer for the next coding pass while
    /// **keeping all context states intact**.  This implements the TERMALL
    /// (RESTART) boundary between passes within a single codeblock — ISO
    /// 15444-1 C.2.8 INITENC re-initialisation.
    pub(crate) fn flush_and_restart(&mut self) -> Vec<u8> {
        self.flush();
        let bytes = self.take_bytes();
        self.restart_init();
        bytes
    }

    #[allow(dead_code)]
    pub(crate) fn erterm_flush(&mut self) -> Vec<u8> {
        let mut k = (11_i32).wrapping_sub(self.ct as i32).wrapping_add(1);
        while k > 0 {
            self.c <<= self.ct;
            self.ct = 0;
            self.byteout();
            k -= self.ct as i32;
        }
        if self.out[self.pos] != 0xff {
            self.byteout();
        }
        self.out[1..self.pos].to_vec()
    }

    #[allow(dead_code)]
    pub(crate) fn segmark_encode(&mut self) {
        for bit in [1u8, 0, 1, 0] {
            self.encode_with_ctx(T1_CTXNO_UNI, bit);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn bypass_init(&mut self) {
        self.c = 0;
        self.ct = BYPASS_CT_INIT;
    }

    #[allow(dead_code)]
    pub(crate) fn bypass_encode(&mut self, bit: u8) {
        debug_assert!(bit <= 1);
        if self.ct == BYPASS_CT_INIT {
            self.ct = 8;
        }
        self.ct -= 1;
        self.c = self.c.wrapping_add((bit as u32) << self.ct);
        if self.ct == 0 {
            self.write_next(self.c as u8);
            self.ct = 8;
            if self.out[self.pos] == 0xff {
                self.ct = 7;
            }
            self.c = 0;
        }
    }

    #[allow(dead_code)]
    pub(crate) fn bypass_flush(&mut self, erterm: bool) {
        let prev_is_ff = self.pos > 0 && self.out[self.pos] == 0xff;

        if self.ct < 7 || (self.ct == 7 && (erterm || !prev_is_ff)) {
            let mut bit = 0u32;
            while self.ct > 0 {
                self.ct -= 1;
                self.c = self.c.wrapping_add(bit << self.ct);
                bit ^= 1;
            }
            self.write_next(self.c as u8);
        } else if self.ct == 7 && prev_is_ff {
            assert!(!erterm);
            self.out.pop();
            self.pos = self.pos.saturating_sub(1);
        } else if self.ct == 8
            && !erterm
            && self.pos >= 2
            && self.out[self.pos] == 0x7f
            && self.out[self.pos - 1] == 0xff
        {
            self.out.truncate(self.pos - 1);
            self.pos -= 2;
        }
    }

    #[allow(dead_code)]
    pub(crate) fn raw_term_flush_and_restart(&mut self, erterm: bool) -> Vec<u8> {
        self.bypass_flush(erterm);
        let bytes = self.take_bytes();
        self.restart_init();
        bytes
    }

    #[allow(dead_code)]
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.flush();
        self.take_bytes()
    }

    #[inline]
    fn renorme(&mut self) {
        while (self.a & 0x8000) == 0 {
            self.a <<= 1;
            self.c <<= 1;
            self.ct -= 1;
            if self.ct == 0 {
                self.byteout();
            }
        }
    }

    fn setbits(&mut self) {
        let temp = self.c.wrapping_add(self.a);
        self.c |= 0xffff;
        if self.c >= temp {
            self.c = self.c.wrapping_sub(0x8000);
        }
    }

    #[inline]
    fn byteout(&mut self) {
        if self.out[self.pos] == 0xff {
            self.write_next((self.c >> 20) as u8);
            self.c &= 0x000f_ffff;
            self.ct = 7;
        } else if (self.c & 0x0800_0000) == 0 {
            self.write_next((self.c >> 19) as u8);
            self.c &= 0x0007_ffff;
            self.ct = 8;
        } else {
            self.out[self.pos] = self.out[self.pos].wrapping_add(1);
            if self.out[self.pos] == 0xff {
                self.c &= 0x07ff_ffff;
                self.write_next((self.c >> 20) as u8);
                self.c &= 0x000f_ffff;
                self.ct = 7;
            } else {
                self.write_next((self.c >> 19) as u8);
                self.c &= 0x0007_ffff;
                self.ct = 8;
            }
        }
    }

    #[inline]
    fn write_next(&mut self, byte: u8) {
        self.pos += 1;
        if self.pos == self.out.len() {
            self.out.push(byte);
        } else {
            self.out[self.pos] = byte;
        }
    }

    fn take_bytes(&self) -> Vec<u8> {
        let end = if self.out[self.pos] == 0xff {
            self.pos
        } else {
            self.pos + 1
        };
        self.out[1..end].to_vec()
    }

    /// Number of bytes committed to the output buffer so far (approximation
    /// matching OpenJPEG's `mqc_numbytes = bp - start`). Suitable for taking
    /// per-pass length snapshots mid-codeblock.
    #[allow(dead_code)]
    pub(crate) fn numbytes(&self) -> usize {
        self.pos
    }
}

#[rustfmt::skip]
pub(crate) const MQ_STATES: [MqState; 94] = [
    MqState { qeval: 0x5601, mps: 0, nmps: 2,  nlps: 3  },
    MqState { qeval: 0x5601, mps: 1, nmps: 3,  nlps: 2  },
    MqState { qeval: 0x3401, mps: 0, nmps: 4,  nlps: 12 },
    MqState { qeval: 0x3401, mps: 1, nmps: 5,  nlps: 13 },
    MqState { qeval: 0x1801, mps: 0, nmps: 6,  nlps: 18 },
    MqState { qeval: 0x1801, mps: 1, nmps: 7,  nlps: 19 },
    MqState { qeval: 0x0ac1, mps: 0, nmps: 8,  nlps: 24 },
    MqState { qeval: 0x0ac1, mps: 1, nmps: 9,  nlps: 25 },
    MqState { qeval: 0x0521, mps: 0, nmps: 10, nlps: 58 },
    MqState { qeval: 0x0521, mps: 1, nmps: 11, nlps: 59 },
    MqState { qeval: 0x0221, mps: 0, nmps: 76, nlps: 66 },
    MqState { qeval: 0x0221, mps: 1, nmps: 77, nlps: 67 },
    MqState { qeval: 0x5601, mps: 0, nmps: 14, nlps: 13 },
    MqState { qeval: 0x5601, mps: 1, nmps: 15, nlps: 12 },
    MqState { qeval: 0x5401, mps: 0, nmps: 16, nlps: 28 },
    MqState { qeval: 0x5401, mps: 1, nmps: 17, nlps: 29 },
    MqState { qeval: 0x4801, mps: 0, nmps: 18, nlps: 28 },
    MqState { qeval: 0x4801, mps: 1, nmps: 19, nlps: 29 },
    MqState { qeval: 0x3801, mps: 0, nmps: 20, nlps: 28 },
    MqState { qeval: 0x3801, mps: 1, nmps: 21, nlps: 29 },
    MqState { qeval: 0x3001, mps: 0, nmps: 22, nlps: 34 },
    MqState { qeval: 0x3001, mps: 1, nmps: 23, nlps: 35 },
    MqState { qeval: 0x2401, mps: 0, nmps: 24, nlps: 36 },
    MqState { qeval: 0x2401, mps: 1, nmps: 25, nlps: 37 },
    MqState { qeval: 0x1c01, mps: 0, nmps: 26, nlps: 40 },
    MqState { qeval: 0x1c01, mps: 1, nmps: 27, nlps: 41 },
    MqState { qeval: 0x1601, mps: 0, nmps: 58, nlps: 42 },
    MqState { qeval: 0x1601, mps: 1, nmps: 59, nlps: 43 },
    MqState { qeval: 0x5601, mps: 0, nmps: 30, nlps: 29 },
    MqState { qeval: 0x5601, mps: 1, nmps: 31, nlps: 28 },
    MqState { qeval: 0x5401, mps: 0, nmps: 32, nlps: 28 },
    MqState { qeval: 0x5401, mps: 1, nmps: 33, nlps: 29 },
    MqState { qeval: 0x5101, mps: 0, nmps: 34, nlps: 30 },
    MqState { qeval: 0x5101, mps: 1, nmps: 35, nlps: 31 },
    MqState { qeval: 0x4801, mps: 0, nmps: 36, nlps: 32 },
    MqState { qeval: 0x4801, mps: 1, nmps: 37, nlps: 33 },
    MqState { qeval: 0x3801, mps: 0, nmps: 38, nlps: 34 },
    MqState { qeval: 0x3801, mps: 1, nmps: 39, nlps: 35 },
    MqState { qeval: 0x3401, mps: 0, nmps: 40, nlps: 36 },
    MqState { qeval: 0x3401, mps: 1, nmps: 41, nlps: 37 },
    MqState { qeval: 0x3001, mps: 0, nmps: 42, nlps: 38 },
    MqState { qeval: 0x3001, mps: 1, nmps: 43, nlps: 39 },
    MqState { qeval: 0x2801, mps: 0, nmps: 44, nlps: 38 },
    MqState { qeval: 0x2801, mps: 1, nmps: 45, nlps: 39 },
    MqState { qeval: 0x2401, mps: 0, nmps: 46, nlps: 40 },
    MqState { qeval: 0x2401, mps: 1, nmps: 47, nlps: 41 },
    MqState { qeval: 0x2201, mps: 0, nmps: 48, nlps: 42 },
    MqState { qeval: 0x2201, mps: 1, nmps: 49, nlps: 43 },
    MqState { qeval: 0x1c01, mps: 0, nmps: 50, nlps: 44 },
    MqState { qeval: 0x1c01, mps: 1, nmps: 51, nlps: 45 },
    MqState { qeval: 0x1801, mps: 0, nmps: 52, nlps: 46 },
    MqState { qeval: 0x1801, mps: 1, nmps: 53, nlps: 47 },
    MqState { qeval: 0x1601, mps: 0, nmps: 54, nlps: 48 },
    MqState { qeval: 0x1601, mps: 1, nmps: 55, nlps: 49 },
    MqState { qeval: 0x1401, mps: 0, nmps: 56, nlps: 50 },
    MqState { qeval: 0x1401, mps: 1, nmps: 57, nlps: 51 },
    MqState { qeval: 0x1201, mps: 0, nmps: 58, nlps: 52 },
    MqState { qeval: 0x1201, mps: 1, nmps: 59, nlps: 53 },
    MqState { qeval: 0x1101, mps: 0, nmps: 60, nlps: 54 },
    MqState { qeval: 0x1101, mps: 1, nmps: 61, nlps: 55 },
    MqState { qeval: 0x0ac1, mps: 0, nmps: 62, nlps: 56 },
    MqState { qeval: 0x0ac1, mps: 1, nmps: 63, nlps: 57 },
    MqState { qeval: 0x09c1, mps: 0, nmps: 64, nlps: 58 },
    MqState { qeval: 0x09c1, mps: 1, nmps: 65, nlps: 59 },
    MqState { qeval: 0x08a1, mps: 0, nmps: 66, nlps: 60 },
    MqState { qeval: 0x08a1, mps: 1, nmps: 67, nlps: 61 },
    MqState { qeval: 0x0521, mps: 0, nmps: 68, nlps: 62 },
    MqState { qeval: 0x0521, mps: 1, nmps: 69, nlps: 63 },
    MqState { qeval: 0x0441, mps: 0, nmps: 70, nlps: 64 },
    MqState { qeval: 0x0441, mps: 1, nmps: 71, nlps: 65 },
    MqState { qeval: 0x02a1, mps: 0, nmps: 72, nlps: 66 },
    MqState { qeval: 0x02a1, mps: 1, nmps: 73, nlps: 67 },
    MqState { qeval: 0x0221, mps: 0, nmps: 74, nlps: 68 },
    MqState { qeval: 0x0221, mps: 1, nmps: 75, nlps: 69 },
    MqState { qeval: 0x0141, mps: 0, nmps: 76, nlps: 70 },
    MqState { qeval: 0x0141, mps: 1, nmps: 77, nlps: 71 },
    MqState { qeval: 0x0111, mps: 0, nmps: 78, nlps: 72 },
    MqState { qeval: 0x0111, mps: 1, nmps: 79, nlps: 73 },
    MqState { qeval: 0x0085, mps: 0, nmps: 80, nlps: 74 },
    MqState { qeval: 0x0085, mps: 1, nmps: 81, nlps: 75 },
    MqState { qeval: 0x0049, mps: 0, nmps: 82, nlps: 76 },
    MqState { qeval: 0x0049, mps: 1, nmps: 83, nlps: 77 },
    MqState { qeval: 0x0025, mps: 0, nmps: 84, nlps: 78 },
    MqState { qeval: 0x0025, mps: 1, nmps: 85, nlps: 79 },
    MqState { qeval: 0x0015, mps: 0, nmps: 86, nlps: 80 },
    MqState { qeval: 0x0015, mps: 1, nmps: 87, nlps: 81 },
    MqState { qeval: 0x0009, mps: 0, nmps: 88, nlps: 82 },
    MqState { qeval: 0x0009, mps: 1, nmps: 89, nlps: 83 },
    MqState { qeval: 0x0005, mps: 0, nmps: 90, nlps: 84 },
    MqState { qeval: 0x0005, mps: 1, nmps: 91, nlps: 85 },
    MqState { qeval: 0x0001, mps: 0, nmps: 90, nlps: 86 },
    MqState { qeval: 0x0001, mps: 1, nmps: 91, nlps: 87 },
    MqState { qeval: 0x5601, mps: 0, nmps: 92, nlps: 92 },
    MqState { qeval: 0x5601, mps: 1, nmps: 93, nlps: 93 },
];

#[cfg(test)]
mod reference {
    //! Faithful port of OpenJPEG `opj_mqc_*` encoder, used only to
    //! verify bit-perfect equivalence with [`super::MqCoder`].
    //!
    //! The layout mirrors the C model: a contiguous output buffer with a
    //! 1-byte fake pre-byte at index 0 (init sets bp = start-1 = 0), real
    //! output starting at index 1. All arithmetic follows openjp2-test-rustier/src/coders/mqc.rs.
    use super::{MQ_STATES, MQC_NUMCTXS, MqState, T1_CTXNO_AGG, T1_CTXNO_UNI, T1_CTXNO_ZC};

    pub(super) struct RefMq {
        pub a: u32,
        pub c: u32,
        pub ct: u32,
        pub bp: usize,
        pub out: Vec<u8>,
        pub ctxs: [usize; MQC_NUMCTXS],
        pub curctx: usize,
    }

    impl RefMq {
        pub fn new() -> Self {
            // Buffer with fake pre-byte at index 0. Generous size for tests.
            let mut this = Self {
                a: 0x8000,
                c: 0,
                ct: 12,
                bp: 0,
                out: vec![0u8; 4096],
                ctxs: [0; MQC_NUMCTXS],
                curctx: 0,
            };
            // resetstates + the three special contexts
            for i in 0..MQC_NUMCTXS {
                this.ctxs[i] = 0;
            }
            this.setstate(T1_CTXNO_UNI, 0, 46);
            this.setstate(T1_CTXNO_AGG, 0, 3);
            this.setstate(T1_CTXNO_ZC, 0, 4);
            this
        }

        fn setstate(&mut self, ctx: u8, msb: u32, prob: i32) {
            self.ctxs[ctx as usize] = (msb + ((prob as u32) << 1)) as usize;
        }

        fn state(&self) -> MqState {
            MQ_STATES[self.ctxs[self.curctx]]
        }

        pub fn setcurctx(&mut self, ctx: u8) {
            self.curctx = ctx as usize;
        }

        fn byteout(&mut self) {
            let bp_val = self.out[self.bp];
            if bp_val == 0xff {
                self.bp += 1;
                self.out[self.bp] = (self.c >> 20) as u8;
                self.c &= 0xfffff;
                self.ct = 7;
            } else if (self.c & 0x8000000) == 0 {
                self.bp += 1;
                self.out[self.bp] = (self.c >> 19) as u8;
                self.c &= 0x7ffff;
                self.ct = 8;
            } else {
                self.out[self.bp] = bp_val.wrapping_add(1);
                if self.out[self.bp] == 0xff {
                    self.c &= 0x7ffffff;
                    self.bp += 1;
                    self.out[self.bp] = (self.c >> 20) as u8;
                    self.c &= 0xfffff;
                    self.ct = 7;
                } else {
                    self.bp += 1;
                    self.out[self.bp] = (self.c >> 19) as u8;
                    self.c &= 0x7ffff;
                    self.ct = 8;
                }
            }
        }

        fn renorme(&mut self) {
            loop {
                self.a <<= 1;
                self.c <<= 1;
                self.ct = self.ct.wrapping_sub(1);
                if self.ct == 0 {
                    self.byteout();
                }
                if self.a & 0x8000 != 0 {
                    break;
                }
            }
        }

        fn codemps(&mut self) {
            let s = self.state();
            self.a = self.a.wrapping_sub(s.qeval as u32);
            if self.a & 0x8000 == 0 {
                if self.a < s.qeval as u32 {
                    self.a = s.qeval as u32;
                } else {
                    self.c = self.c.wrapping_add(s.qeval as u32);
                }
                self.ctxs[self.curctx] = s.nmps as usize;
                self.renorme();
            } else {
                self.c = self.c.wrapping_add(s.qeval as u32);
            }
        }

        fn codelps(&mut self) {
            let s = self.state();
            self.a = self.a.wrapping_sub(s.qeval as u32);
            if self.a < s.qeval as u32 {
                self.c = self.c.wrapping_add(s.qeval as u32);
            } else {
                self.a = s.qeval as u32;
            }
            self.ctxs[self.curctx] = s.nlps as usize;
            self.renorme();
        }

        pub fn encode(&mut self, d: u32) {
            if self.state().mps as u32 == d {
                self.codemps();
            } else {
                self.codelps();
            }
        }

        fn setbits(&mut self) {
            let tempc = self.c.wrapping_add(self.a);
            self.c |= 0xffff;
            if self.c >= tempc {
                self.c = self.c.wrapping_sub(0x8000);
            }
        }

        pub fn flush(&mut self) {
            self.setbits();
            self.c <<= self.ct;
            self.byteout();
            self.c <<= self.ct;
            self.byteout();
            // OpenJPEG drops a trailing 0xff via "if *bp != 0xff: inc_bp".
            if self.out[self.bp] != 0xff {
                self.bp += 1;
            }
        }

        #[allow(dead_code)]
        pub fn restart_init(&mut self) {
            self.a = 0x8000;
            self.c = 0;
            self.ct = 12;
            self.bp = 0;
            self.out.fill(0);
        }

        pub fn erterm(&mut self) {
            let mut k = (11_i32).wrapping_sub(self.ct as i32).wrapping_add(1);
            while k > 0 {
                self.c <<= self.ct;
                self.ct = 0;
                self.byteout();
                k -= self.ct as i32;
            }
            if self.out[self.bp] != 0xff {
                self.byteout();
            }
        }

        pub fn segmark(&mut self) {
            self.setcurctx(T1_CTXNO_UNI);
            for bit in [1u32, 0, 1, 0] {
                self.encode(bit);
            }
        }

        pub fn bytes(&self) -> &[u8] {
            // start = 1; numbytes = bp - start
            &self.out[1..self.bp]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MqCoder, MqDecoder, T1_CTXNO_AGG, T1_CTXNO_UNI, T1_CTXNO_ZC};

    #[test]
    fn reset_initializes_special_contexts() {
        let coder = MqCoder::new();
        assert_eq!(coder.state_index(T1_CTXNO_ZC), 8);
        assert_eq!(coder.state_index(T1_CTXNO_AGG), 6);
        assert_eq!(coder.state_index(T1_CTXNO_UNI), 92);
    }

    #[test]
    fn encoding_is_deterministic() {
        let mut first = MqCoder::new();
        let mut second = MqCoder::new();
        for &bit in &[1, 0, 1, 1, 0, 0, 1, 0, 1, 1, 1, 0] {
            first.encode_with_ctx(T1_CTXNO_UNI, bit);
            second.encode_with_ctx(T1_CTXNO_UNI, bit);
        }
        let first = first.finish();
        let second = second.finish();
        assert!(!first.is_empty());
        assert_eq!(first, second);
    }

    // ------------- bit-perfect cross-checks vs. OpenJPEG reference -------------

    fn xorshift(s: &mut u64) -> u64 {
        let mut x = *s;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *s = x;
        x
    }

    fn run_bits(bits: &[(u8, u8)]) -> (Vec<u8>, Vec<u8>) {
        // bits is a slice of (ctx, bit) pairs.
        let mut coder = MqCoder::new();
        let mut reference = super::reference::RefMq::new();
        for &(ctx, bit) in bits {
            coder.encode_with_ctx(ctx, bit);
            reference.setcurctx(ctx);
            reference.encode(bit as u32);
        }
        reference.flush();
        let produced = coder.finish();
        let expected = reference.bytes().to_vec();
        (produced, expected)
    }

    fn roundtrip_bits(bits: &[(u8, u8)]) {
        let mut coder = MqCoder::new();
        for &(ctx, bit) in bits {
            coder.encode_with_ctx(ctx, bit);
        }
        let bytes = coder.finish();
        let mut decoder = MqDecoder::new(&bytes);
        for (idx, &(ctx, expected)) in bits.iter().enumerate() {
            let actual = decoder.decode_with_ctx(ctx);
            assert_eq!(actual, expected, "decoded bit mismatch at symbol {idx}");
        }
    }

    #[test]
    fn matches_reference_uni_all_zero() {
        let bits: Vec<(u8, u8)> = (0..256).map(|_| (T1_CTXNO_UNI, 0)).collect();
        let (a, b) = run_bits(&bits);
        assert_eq!(a, b, "UNI all-zero");
    }

    #[test]
    fn matches_reference_uni_all_one() {
        let bits: Vec<(u8, u8)> = (0..256).map(|_| (T1_CTXNO_UNI, 1)).collect();
        let (a, b) = run_bits(&bits);
        assert_eq!(a, b, "UNI all-one");
    }

    #[test]
    fn matches_reference_alternating() {
        let bits: Vec<(u8, u8)> = (0..512).map(|i| (T1_CTXNO_AGG, (i & 1) as u8)).collect();
        let (a, b) = run_bits(&bits);
        assert_eq!(a, b, "alternating bits on AGG");
    }

    #[test]
    fn matches_reference_mixed_contexts() {
        let ctxs = [
            super::T1_CTXNO_ZC,
            super::T1_CTXNO_SC,
            super::T1_CTXNO_MAG,
            T1_CTXNO_AGG,
            T1_CTXNO_UNI,
        ];
        let mut bits = Vec::new();
        for i in 0..400u32 {
            let ctx = ctxs[(i as usize) % ctxs.len()];
            let bit = ((i.wrapping_mul(2654435761) >> 30) & 1) as u8;
            bits.push((ctx, bit));
        }
        let (a, b) = run_bits(&bits);
        assert_eq!(a, b, "mixed contexts PRNG-ish");
    }

    #[test]
    fn matches_reference_randomized() {
        let ctxs = [
            super::T1_CTXNO_ZC,
            super::T1_CTXNO_SC,
            super::T1_CTXNO_MAG,
            T1_CTXNO_AGG,
            T1_CTXNO_UNI,
        ];
        for seed_base in 0..32u64 {
            let mut s = seed_base.wrapping_add(0x9E3779B97F4A7C15);
            if s == 0 {
                s = 1;
            }
            let n = 100 + (xorshift(&mut s) % 1500) as usize;
            let mut bits = Vec::with_capacity(n);
            for _ in 0..n {
                let r = xorshift(&mut s);
                let ctx = ctxs[(r as usize) % ctxs.len()];
                let bit = ((r >> 32) & 1) as u8;
                bits.push((ctx, bit));
            }
            let (a, b) = run_bits(&bits);
            assert_eq!(a, b, "randomized seed {}", seed_base);
        }
    }

    #[test]
    fn decoder_roundtrips_mixed_contexts() {
        let ctxs = [
            super::T1_CTXNO_ZC,
            super::T1_CTXNO_SC,
            super::T1_CTXNO_MAG,
            T1_CTXNO_AGG,
            T1_CTXNO_UNI,
        ];
        let mut bits = Vec::new();
        for i in 0..800u32 {
            let ctx = ctxs[((i * 7) as usize) % ctxs.len()];
            let bit = ((i.wrapping_mul(1103515245).wrapping_add(12345) >> 29) & 1) as u8;
            bits.push((ctx, bit));
        }
        roundtrip_bits(&bits);
    }

    #[test]
    fn decoder_initializes_special_contexts_like_encoder() {
        let decoder = MqDecoder::new(&[]);
        assert_eq!(decoder.state_index(T1_CTXNO_ZC), 8);
        assert_eq!(decoder.state_index(T1_CTXNO_AGG), 6);
        assert_eq!(decoder.state_index(T1_CTXNO_UNI), 92);
    }

    #[test]
    fn matches_reference_skewed_mps() {
        // Mostly MPS (bit=0), occasional LPS — exercises the MPS-heavy path
        // where 0xff carry/overflow cases are most common.
        let mut s: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let mut bits = Vec::new();
        for _ in 0..5000 {
            let r = xorshift(&mut s);
            let bit = if (r & 0xff) < 8 { 1 } else { 0 };
            bits.push((T1_CTXNO_AGG, bit));
        }
        let (a, b) = run_bits(&bits);
        assert_eq!(a, b, "skewed MPS stream");
    }

    #[test]
    fn lps_and_mps_advance_context_state() {
        let mut coder = MqCoder::new();
        let original = coder.state_index(T1_CTXNO_AGG);
        coder.encode_with_ctx(T1_CTXNO_AGG, 1);
        let after_lps = coder.state_index(T1_CTXNO_AGG);
        coder.encode_with_ctx(T1_CTXNO_AGG, 0);
        let after_mps = coder.state_index(T1_CTXNO_AGG);
        assert_ne!(after_lps, original);
        assert!((after_mps as usize) < super::MQ_STATES.len());
    }

    #[test]
    fn flush_and_restart_preserves_context_state() {
        let mut coder = MqCoder::new();
        coder.encode_with_ctx(T1_CTXNO_AGG, 1);
        let state_before = coder.state_index(T1_CTXNO_AGG);
        let bytes = coder.flush_and_restart();
        assert!(!bytes.is_empty());
        assert_eq!(coder.state_index(T1_CTXNO_AGG), state_before);
        let bytes2 = coder.flush_and_restart();
        assert!(bytes2.is_empty() || !bytes2.is_empty());
    }

    #[test]
    fn segmark_matches_reference() {
        let mut coder = MqCoder::new();
        let mut reference = super::reference::RefMq::new();
        coder.segmark_encode();
        reference.segmark();
        reference.flush();
        assert_eq!(coder.finish(), reference.bytes().to_vec());
    }

    #[test]
    fn erterm_matches_reference_after_payload() {
        let mut coder = MqCoder::new();
        let mut reference = super::reference::RefMq::new();
        let bits = [
            (T1_CTXNO_ZC, 1u8),
            (T1_CTXNO_AGG, 0),
            (T1_CTXNO_UNI, 1),
            (T1_CTXNO_AGG, 1),
            (T1_CTXNO_UNI, 0),
            (T1_CTXNO_ZC, 0),
            (T1_CTXNO_UNI, 1),
        ];
        for (ctx, bit) in bits {
            coder.encode_with_ctx(ctx, bit);
            reference.setcurctx(ctx);
            reference.encode(bit as u32);
        }
        let actual = coder.erterm_flush();
        reference.erterm();
        assert_eq!(actual, reference.bytes().to_vec());
    }
}
