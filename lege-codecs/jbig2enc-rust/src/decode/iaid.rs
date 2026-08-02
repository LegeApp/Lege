//! IAID symbol-ID decoding (jbig2decplan.md §11, T.88 §6.5.8.2.3 / Annex A).
//!
//! The inverse of the encoder's IAID tree writer
//! ([`crate::encode::arith::Jbig2ArithCoder::encode_iaid`]): `SBSYMCODELEN` bits
//! are read most-significant-first, walking a binary context tree whose node
//! index is the value decoded so far with an implicit leading 1. The context
//! vector is grown (never shrunk) through the [`DecoderContext`] and reset when
//! the symbol-ID width changes.

use crate::decode::arith::ArithmeticDecoder;
use crate::decode::error::{DecodeError, UnsupportedFeature};
use crate::shared::mq_table::MqContext;

/// Reusable IAID context tree (jbig2decplan.md §11).
#[derive(Default)]
pub struct IaidContexts {
    states: Vec<MqContext>,
}

impl IaidContexts {
    /// Size and zero the context tree for a `bits`-wide symbol ID.
    ///
    /// A `bits`-wide tree needs `1 << bits` nodes. `bits == 0` (a single-symbol
    /// dictionary, where the ID is implicit) needs one node and reads nothing.
    pub fn reset_for_bits(&mut self, bits: u8) -> Result<(), DecodeError> {
        // The encoder caps IAID widths at 24 bits; anything wider is malformed
        // for the self-decoder and would demand a >16M-entry table.
        if bits > 24 {
            return Err(DecodeError::Unsupported(UnsupportedFeature::SymbolCoding));
        }
        let needed = 1usize << bits;
        if self.states.len() < needed {
            self.states.resize(needed, MqContext(0));
        }
        for c in self.states[..needed].iter_mut() {
            *c = MqContext(0);
        }
        Ok(())
    }

    /// Decode a `bits`-wide symbol ID from `dec` (T.88 Annex A / §6.5.8.2.3).
    ///
    /// [`reset_for_bits`](Self::reset_for_bits) must have been called with the
    /// same `bits` first. `bits == 0` returns `0` without consuming any bits.
    #[inline]
    pub fn decode(&mut self, dec: &mut ArithmeticDecoder<'_>, bits: u8) -> u32 {
        if bits == 0 {
            return 0;
        }
        let len = self.states.len();
        if len == 0 {
            return 0;
        }
        let mut prev: usize = 1;
        for _ in 0..bits {
            let idx = prev.min(len - 1);
            let b = dec.decode_bit(&mut self.states[idx]);
            prev = (prev << 1) | (b as usize);
        }
        // After `bits` steps `prev == (1 << bits) | value`.
        (prev - (1usize << bits)) as u32
    }
}

#[cfg(all(test, feature = "encode"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::encode::arith::Jbig2ArithCoder;

    fn roundtrip_ids(bits: u8, ids: &[u32]) {
        let mut enc = Jbig2ArithCoder::new();
        for &id in ids {
            enc.encode_iaid(id, bits).unwrap();
        }
        let data = enc.into_vec();
        let mut dec = ArithmeticDecoder::new(&data);
        let mut ctx = IaidContexts::default();
        ctx.reset_for_bits(bits).unwrap();
        for &id in ids {
            assert_eq!(ctx.decode(&mut dec, bits), id, "bits={bits} id={id}");
        }
    }

    #[test]
    fn roundtrip_all_bit_widths_and_boundaries() {
        for bits in 1u8..=16 {
            let max = (1u32 << bits) - 1;
            let mids = [0u32, 1, max / 2, max.saturating_sub(1), max];
            // Deduplicate for tiny widths.
            let mut ids: Vec<u32> = mids.to_vec();
            ids.dedup();
            roundtrip_ids(bits, &ids);
        }
    }

    #[test]
    fn interleaved_ids_single_stream() {
        // A realistic text-region pattern: many IDs of the same width in a row.
        let bits = 10u8;
        let mut ids = Vec::new();
        let mut s = 12345u32;
        for _ in 0..300 {
            s = s.wrapping_mul(1103515245).wrapping_add(12345);
            ids.push((s >> 8) % (1 << bits));
        }
        roundtrip_ids(bits, &ids);
    }

    #[test]
    fn zero_bits_reads_nothing() {
        let mut ctx = IaidContexts::default();
        ctx.reset_for_bits(0).unwrap();
        let data = [0u8; 4];
        let mut dec = ArithmeticDecoder::new(&data);
        assert_eq!(ctx.decode(&mut dec, 0), 0);
    }

    #[test]
    fn encoder_rejects_ids_that_do_not_fit_code_length() {
        let mut enc = Jbig2ArithCoder::new();
        assert!(enc.encode_iaid(8, 3).is_err());
        assert!(enc.encode_iaid(7, 3).is_ok());
    }
}
