//! Arithmetic integer-decoding procedure identifiers (T.88 Annex A / §6.8),
//! shared by the encoder and decoder (jbig2decplan.md §11, §23).
//!
//! These are the `IADH`, `IADW`, `IAEX`, … integer-arithmetic procedures.
//! Each names one logical stream of integer values with its own context set;
//! the identifier is protocol data, so it lives in `shared`. The encoder and
//! decoder keep their own integer *procedures* (encode vs decode) but agree on
//! these identifiers and their ordering.

/// Integer-arithmetic procedure identifiers, corresponding to the JBIG2
/// `IA*` procedures. The discriminants are contiguous from 0 so the value
/// doubles as an index into a `[_; 13]` array of per-procedure context sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum IntProc {
    /// IAAI — number of symbol instances in an aggregation.
    Iaai = 0,
    /// IADH — height-class delta height.
    Iadh,
    /// IADS — symbol instance S-coordinate delta.
    Iads,
    /// IADT — strip T-coordinate delta.
    Iadt,
    /// IADW — symbol width delta.
    Iadw,
    /// IAEX — export flag run length.
    Iaex,
    /// IAFS — first symbol instance S-coordinate.
    Iafs,
    /// IAIT — symbol instance T-coordinate.
    Iait,
    /// IARDH — refinement delta height.
    Iardh,
    /// IARDW — refinement delta width.
    Iardw,
    /// IARDX — refinement delta X.
    Iardx,
    /// IARDY — refinement delta Y.
    Iardy,
    /// IARI — refinement flag.
    Iari,
}

/// The number of distinct integer-arithmetic procedures (`IAAI`..`IARI`).
pub const INT_PROC_COUNT: usize = 13;

impl IntProc {
    /// The procedure's index (0..[`INT_PROC_COUNT`]).
    #[inline(always)]
    pub const fn index(self) -> usize {
        self as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_are_contiguous() {
        let all = [
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
        ];
        assert_eq!(all.len(), INT_PROC_COUNT);
        for (i, p) in all.iter().enumerate() {
            assert_eq!(p.index(), i);
        }
    }
}
