//! Segment-type protocol definitions shared by the encoder and decoder
//! (jbig2decplan.md §23 Phase 0, Gap A).
//!
//! `SegmentType` is pure protocol data — the numeric segment-type codes from
//! T.88 §7.3 — so it belongs in `shared`. The encoder re-exports it from
//! `encode::structs` for backward compatibility.

use std::io;

/// JBIG2 segment types as defined in the specification (T.88 §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SegmentType {
    #[default]
    SymbolDictionary = 0,
    IntermediateTextRegion = 4,
    ImmediateTextRegion = 6,
    ImmediateLosslessTextRegion = 7,
    PatternDictionary = 16,
    IntermediateHalftoneRegion = 20,
    ImmediateHalftoneRegion = 22,
    ImmediateLosslessHalftoneRegion = 23,
    IntermediateGenericRegion = 36,
    ImmediateGenericRegion = 38,
    ImmediateLosslessGenericRegion = 39,
    IntermediateGenericRefinementRegion = 40,
    ImmediateGenericRefinementRegion = 42,
    ImmediateLosslessGenericRefinementRegion = 43,
    PageInformation = 48,
    EndOfPage = 49,
    EndOfStripe = 50,
    EndOfFile = 51,
    Profiles = 52,
    Tables = 53,
    ColorPalette = 54,
    FileHeader = 56,
    Extension = 62,
}

impl SegmentType {
    /// Map a raw 6-bit segment-type code to a `SegmentType`, or `None` if the
    /// code is unassigned. This is the checked, decoder-facing form (no
    /// panics, no error allocation).
    pub const fn from_code(value: u8) -> Option<Self> {
        Some(match value {
            0 => SegmentType::SymbolDictionary,
            4 => SegmentType::IntermediateTextRegion,
            6 => SegmentType::ImmediateTextRegion,
            7 => SegmentType::ImmediateLosslessTextRegion,
            16 => SegmentType::PatternDictionary,
            20 => SegmentType::IntermediateHalftoneRegion,
            22 => SegmentType::ImmediateHalftoneRegion,
            23 => SegmentType::ImmediateLosslessHalftoneRegion,
            36 => SegmentType::IntermediateGenericRegion,
            38 => SegmentType::ImmediateGenericRegion,
            39 => SegmentType::ImmediateLosslessGenericRegion,
            40 => SegmentType::IntermediateGenericRefinementRegion,
            42 => SegmentType::ImmediateGenericRefinementRegion,
            43 => SegmentType::ImmediateLosslessGenericRefinementRegion,
            48 => SegmentType::PageInformation,
            49 => SegmentType::EndOfPage,
            50 => SegmentType::EndOfStripe,
            51 => SegmentType::EndOfFile,
            52 => SegmentType::Profiles,
            53 => SegmentType::Tables,
            54 => SegmentType::ColorPalette,
            56 => SegmentType::FileHeader,
            62 => SegmentType::Extension,
            _ => return None,
        })
    }
}

impl TryFrom<u8> for SegmentType {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        SegmentType::from_code(value).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Invalid segment type: {}", value),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codes_round_trip() {
        for code in [
            0u8, 4, 6, 7, 16, 20, 22, 23, 36, 38, 39, 40, 42, 43, 48, 49, 50, 51, 52, 53, 54, 56,
            62,
        ] {
            let ty = SegmentType::from_code(code).expect("known code");
            assert_eq!(ty as u8, code);
            assert_eq!(SegmentType::try_from(code).unwrap(), ty);
        }
    }

    #[test]
    fn unassigned_codes_rejected() {
        for code in [1u8, 2, 3, 5, 8, 15, 44, 55, 63, 200, 255] {
            assert!(SegmentType::from_code(code).is_none());
            assert!(SegmentType::try_from(code).is_err());
        }
    }
}
