//! The MQ arithmetic-coder probability state table (T.88 §E / ISO 14492
//! Table E.1), shared by the encoder and decoder (jbig2decplan.md §10, §23).
//!
//! The probability table is protocol data; the arithmetic *registers* are not.
//! This module owns only the table plus the small `MqContext` handle. The
//! encoder keeps its own register machine (`encode::arith`) and the decoder
//! will keep its own (`decode::arith`); both index into [`MQ_STATES`].
//!
//! Layout (per §10): the 94-entry table encodes the sign of the MPS in the
//! index — states `0..47` have MPS = 0, states `47..94` have MPS = 1 — and the
//! next-state fields already incorporate the conditional-exchange (`SWITCH`),
//! so consumers never need a separate switch flag.

/// One resolved MQ state: the `Qe` probability estimate and the next state
/// index after coding an MPS or an LPS. The MPS value is implied by the
/// index (`>= 47` means MPS = 1), so no switch flag is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MqEntry {
    /// `Qe` — the LPS probability estimate (16-bit fixed point).
    pub qe: u16,
    /// Next state index if the coded symbol was the MPS.
    pub next_mps: u8,
    /// Next state index if the coded symbol was the LPS.
    pub next_lps: u8,
}

/// A one-byte handle to a current MQ state (an index into [`MQ_STATES`]).
///
/// Contexts start at state 0 (MPS = 0, `Qe = 0x5601`). Being one byte and
/// `Copy`, dense arrays of `MqContext` are the decoder's per-region context
/// storage (jbig2decplan.md §10, §22).
#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct MqContext(pub u8);

impl MqContext {
    /// The state index as a `usize`, ready to index [`MQ_STATES`].
    #[inline(always)]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// The resolved table entry for this context's current state.
    #[inline(always)]
    pub const fn entry(self) -> MqEntry {
        MQ_STATES[self.0 as usize]
    }

    /// The current more-probable-symbol value (0 for states `0..47`, 1 for
    /// states `47..94`).
    #[inline(always)]
    pub const fn mps(self) -> bool {
        self.0 >= 47
    }
}

/// Canonical base rows for states 0..47 (MPS = 0 half): `(Qe, NMPS, NLPS,
/// SWITCH)` exactly as tabulated in ISO 14492 Table E.1. The full 94-entry
/// table is derived from these; keeping them in one place is the single
/// source of truth for both halves of the crate.
pub(crate) const BASE_ROWS: [(u16, u8, u8, bool); 47] = [
    (0x5601, 1, 1, true),
    (0x3401, 2, 6, false),
    (0x1801, 3, 9, false),
    (0x0AC1, 4, 12, false),
    (0x0521, 5, 29, false),
    (0x0221, 38, 33, false),
    (0x5601, 7, 6, true),
    (0x5401, 8, 14, false),
    (0x4801, 9, 14, false),
    (0x3801, 10, 14, false),
    (0x3001, 11, 17, false),
    (0x2401, 12, 18, false),
    (0x1C01, 13, 20, false),
    (0x1601, 29, 21, false),
    (0x5601, 15, 14, true),
    (0x5401, 16, 14, false),
    (0x5101, 17, 15, false),
    (0x4801, 18, 16, false),
    (0x3801, 19, 17, false),
    (0x3401, 20, 18, false),
    (0x3001, 21, 19, false),
    (0x2801, 22, 19, false),
    (0x2401, 23, 20, false),
    (0x2201, 24, 21, false),
    (0x1C01, 25, 22, false),
    (0x1801, 26, 23, false),
    (0x1601, 27, 24, false),
    (0x1401, 28, 25, false),
    (0x1201, 29, 26, false),
    (0x1101, 30, 27, false),
    (0x0AC1, 31, 28, false),
    (0x09C1, 32, 29, false),
    (0x08A1, 33, 30, false),
    (0x0521, 34, 31, false),
    (0x0441, 35, 32, false),
    (0x02A1, 36, 33, false),
    (0x0221, 37, 34, false),
    (0x0141, 38, 35, false),
    (0x0111, 39, 36, false),
    (0x0085, 40, 37, false),
    (0x0049, 41, 38, false),
    (0x0025, 42, 39, false),
    (0x0015, 43, 40, false),
    (0x0009, 44, 41, false),
    (0x0005, 45, 42, false),
    (0x0001, 45, 43, false),
    (0x5601, 46, 46, false), // dummy "all done" state
];

/// The full 94-entry MQ state table. States `0..47` are the MPS = 0 half and
/// `47..94` are the MPS = 1 half; the next-state fields already fold in the
/// `SWITCH` conditional exchange.
pub const MQ_STATES: [MqEntry; 94] = build_full();

const fn build_full() -> [MqEntry; 94] {
    let mut t = [MqEntry {
        qe: 0,
        next_mps: 0,
        next_lps: 0,
    }; 94];

    let mut i = 0;
    while i < 47 {
        let (qe, nmps, nlps, switch) = BASE_ROWS[i];

        // Lower half: MPS = 0. Staying MPS keeps us in the lower half; an LPS
        // that flips the MPS (SWITCH) moves us into the upper half.
        t[i] = MqEntry {
            qe,
            next_mps: nmps,
            next_lps: if switch { nlps + 47 } else { nlps },
        };

        // Upper half: MPS = 1. Symmetric: staying MPS keeps us in the upper
        // half; an LPS that flips the MPS returns us to the lower half.
        t[i + 47] = MqEntry {
            qe,
            next_mps: nmps + 47,
            next_lps: if switch { nlps } else { nlps + 47 },
        };

        i += 1;
    }

    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_defaults_to_state_zero() {
        assert_eq!(MqContext::default(), MqContext(0));
        assert_eq!(MqContext::default().entry().qe, 0x5601);
        assert!(!MqContext::default().mps());
        assert!(MqContext(47).mps());
    }

    #[test]
    fn table_has_no_out_of_range_transitions() {
        for (i, e) in MQ_STATES.iter().enumerate() {
            assert!(
                (e.next_mps as usize) < 94,
                "state {i} next_mps out of range"
            );
            assert!(
                (e.next_lps as usize) < 94,
                "state {i} next_lps out of range"
            );
        }
    }

    #[test]
    fn lower_and_upper_halves_share_qe() {
        for i in 0..47 {
            assert_eq!(MQ_STATES[i].qe, MQ_STATES[i + 47].qe);
        }
    }

    #[test]
    fn known_anchor_states() {
        // MPS=0 half, state 0.
        assert_eq!(
            MQ_STATES[0],
            MqEntry {
                qe: 0x5601,
                next_mps: 1,
                // state 0 switches, so LPS crosses into the upper half: 1 + 47.
                next_lps: 48,
            }
        );
        // MPS=1 half, state 0.
        assert_eq!(
            MQ_STATES[47],
            MqEntry {
                qe: 0x5601,
                next_mps: 48,
                next_lps: 1,
            }
        );
        // A non-switching row (index 1): LPS stays within its half.
        assert_eq!(MQ_STATES[1].next_lps, 6);
        assert_eq!(MQ_STATES[48].next_lps, 6 + 47);
    }
}
