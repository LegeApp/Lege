//! Arithmetic integer decoding (jbig2decplan.md §11, T.88 Annex A).
//!
//! This is the exact inverse of the encoder's integer coder
//! ([`crate::encode::arith::Jbig2ArithCoder::encode_integer`] / `encode_oob`).
//! The encoder's `INT_ENC_RANGE` table is a factored representation of the T.88
//! Annex A decode tree; decoding with the canonical tree (sign bit, unary range
//! selector, then the range's value bits, all threaded through the same `PREV`
//! context register) reproduces every value and out-of-band marker the encoder
//! emits.
//!
//! Each of the 13 [`IntProc`] procedures owns its own `[MqContext; 512]` bank,
//! all drawing bits from one shared [`ArithmeticDecoder`]. The banks are reused
//! across pages via [`DecoderContext`](crate::decode::context::DecoderContext)
//! and reset per segment (T.88 resets the coder per symbol dictionary / text
//! region — mirrored by the encoder creating a fresh coder each segment).

use crate::decode::arith::ArithmeticDecoder;
use crate::shared::int_proc::{INT_PROC_COUNT, IntProc};
use crate::shared::mq_table::MqContext;

/// Contexts per integer procedure (T.88 uses a 9-bit `PREV`, i.e. 512 states).
pub const INT_CTX_SIZE: usize = 512;

/// The result of decoding one arithmetic integer (jbig2decplan.md §11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodedInteger {
    /// A finite decoded value.
    Value(i32),
    /// The out-of-band marker (T.88 Table A.1 `OOB`).
    OutOfBand,
}

/// The 13 integer-procedure context banks (jbig2decplan.md §11).
pub struct IntegerContexts {
    procedures: [[MqContext; INT_CTX_SIZE]; INT_PROC_COUNT],
}

impl Default for IntegerContexts {
    fn default() -> Self {
        Self {
            procedures: [[MqContext(0); INT_CTX_SIZE]; INT_PROC_COUNT],
        }
    }
}

/// Clamp an `i64` into `i32` range without panicking. Legitimate streams never
/// hit the clamp; malformed ones are rejected downstream by range/limit checks.
#[inline]
fn clamp_i32(v: i64) -> i32 {
    if v > i32::MAX as i64 {
        i32::MAX
    } else if v < i32::MIN as i64 {
        i32::MIN
    } else {
        v as i32
    }
}

impl IntegerContexts {
    /// Zero every procedure's context bank (T.88 per-segment reset).
    pub fn reset(&mut self) {
        for bank in self.procedures.iter_mut() {
            for c in bank.iter_mut() {
                *c = MqContext(0);
            }
        }
    }

    /// Decode one integer for `proc` from `dec` (T.88 Annex A.3).
    ///
    /// Returns [`DecodedInteger::OutOfBand`] for the `OOB` marker (sign bit set,
    /// magnitude zero), else the finite value.
    #[inline]
    pub fn decode(&mut self, dec: &mut ArithmeticDecoder<'_>, proc: IntProc) -> DecodedInteger {
        let bank = &mut self.procedures[proc.index()];
        let mut prev: usize = 1;

        // Decode one bit under context `prev & 0x1ff`, then advance `PREV`
        // exactly as the encoder does (`update_prev`).
        macro_rules! bit {
            () => {{
                let b = dec.decode_bit(&mut bank[prev & 0x1ff]);
                prev = if prev < 0x100 {
                    (prev << 1) | (b as usize)
                } else {
                    (((prev << 1) | (b as usize)) & 0x1ff) | 0x100
                };
                b
            }};
        }

        let sign = bit!();
        let (n_bits, offset): (u32, i64) = if !bit!() {
            (2, 0)
        } else if !bit!() {
            (4, 4)
        } else if !bit!() {
            (6, 20)
        } else if !bit!() {
            (8, 84)
        } else if !bit!() {
            (12, 340)
        } else {
            (32, 4436)
        };

        let mut v: u32 = 0;
        for _ in 0..n_bits {
            v = (v << 1) | (bit!() as u32);
        }

        let magnitude = offset + v as i64;
        if sign {
            if magnitude == 0 {
                DecodedInteger::OutOfBand
            } else {
                DecodedInteger::Value(clamp_i32(-magnitude))
            }
        } else {
            DecodedInteger::Value(clamp_i32(magnitude))
        }
    }
}

#[cfg(all(test, feature = "encode"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::encode::arith::Jbig2ArithCoder;

    fn all_procs() -> [IntProc; INT_PROC_COUNT] {
        [
            IntProc::Iaai,
            IntProc::Iadh,
            IntProc::Iads,
            IntProc::Iadt,
            IntProc::Iadw,
            IntProc::Iaex,
            IntProc::Iafs,
            IntProc::Iait,
            IntProc::Iardh,
            IntProc::Iardw,
            IntProc::Iardx,
            IntProc::Iardy,
            IntProc::Iari,
        ]
    }

    /// Every boundary value of every range bucket must round-trip, plus OOB.
    #[test]
    fn roundtrip_boundary_values_every_proc() {
        let values: Vec<i32> = vec![
            0, 1, -1, 2, 3, -2, -3, 4, 19, -4, -19, 20, 83, -20, -83, 84, 339, -84, -339, 340,
            4435, -340, -4435, 4436, 100_000, 2_000_000_000, -4436, -100_000, -2_000_000_000,
        ];
        for proc in all_procs() {
            // Encode the whole value sequence (then an OOB) under one procedure,
            // decode it back with a fresh decoder, assert equality.
            let mut enc = Jbig2ArithCoder::new();
            for &v in &values {
                enc.encode_integer(proc, v).unwrap();
            }
            enc.encode_oob(proc).unwrap();
            let data = enc.into_vec();

            let mut dec = ArithmeticDecoder::new(&data);
            let mut ctx = IntegerContexts::default();
            for &v in &values {
                match ctx.decode(&mut dec, proc) {
                    DecodedInteger::Value(got) => assert_eq!(got, v, "proc {proc:?} value {v}"),
                    DecodedInteger::OutOfBand => panic!("unexpected OOB for {v}"),
                }
            }
            assert_eq!(ctx.decode(&mut dec, proc), DecodedInteger::OutOfBand);
        }
    }

    /// Interleaving several procedures in one stream must still decode exactly
    /// (each keeps its own context bank).
    #[test]
    fn interleaved_procedures() {
        let mut enc = Jbig2ArithCoder::new();
        let seq: &[(IntProc, i32)] = &[
            (IntProc::Iadh, 5),
            (IntProc::Iadw, 3),
            (IntProc::Iadt, -7),
            (IntProc::Iafs, 1000),
            (IntProc::Iads, -1),
            (IntProc::Iaex, 0),
            (IntProc::Iaex, 42),
        ];
        for &(p, v) in seq {
            enc.encode_integer(p, v).unwrap();
        }
        let data = enc.into_vec();
        let mut dec = ArithmeticDecoder::new(&data);
        let mut ctx = IntegerContexts::default();
        for &(p, v) in seq {
            assert_eq!(ctx.decode(&mut dec, p), DecodedInteger::Value(v));
        }
    }

    #[test]
    fn reset_clears_state() {
        let mut ctx = IntegerContexts::default();
        let mut enc = Jbig2ArithCoder::new();
        enc.encode_integer(IntProc::Iadh, 12345).unwrap();
        let data = enc.into_vec();
        let mut dec = ArithmeticDecoder::new(&data);
        let _ = ctx.decode(&mut dec, IntProc::Iadh);
        ctx.reset();
        // After reset the banks are all-zero again (compile/behavior smoke).
        for bank in ctx.procedures.iter() {
            for c in bank.iter() {
                assert_eq!(*c, MqContext(0));
            }
        }
    }
}
