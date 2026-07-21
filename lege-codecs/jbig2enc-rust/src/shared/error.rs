//! Decoder error types (jbig2decplan.md §5, §23).
//!
//! Decoder library code returns structured, typed errors — never `anyhow`,
//! never a panic on malformed input. This is the Phase 0 skeleton: the variant
//! set grows as later phases add parsing and image decoding, but the top-level
//! shape (`DecodeError` wrapping lower-level `ParseError` / `LimitError`) is
//! fixed here.

/// A feature that appears in the stream but is not yet supported by this
/// decoder. Kept as a typed value so the caller can distinguish "malformed"
/// from "valid but unimplemented".
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UnsupportedFeature {
    #[error("unknown segment data length")]
    UnknownSegmentLength,
    #[error("symbol dictionary / text region segment")]
    SymbolCoding,
    #[error("segment type {0} not handled in this phase")]
    SegmentType(u8),
}

/// Which resource limit ([`DecodeLimits`]) was exceeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LimitError {
    #[error("bitmap dimension {value} exceeds limit {limit} ({dimension})")]
    Dimension {
        dimension: &'static str,
        value: u64,
        limit: u64,
    },
    #[error("pixel count {value} exceeds limit {limit} ({what})")]
    Pixels {
        what: &'static str,
        value: u64,
        limit: u64,
    },
    #[error("count {value} exceeds limit {limit} ({what})")]
    Count {
        what: &'static str,
        value: u64,
        limit: u64,
    },
}

/// Low-level structural parse errors from the checked reader and segment
/// parsers. These carry no image semantics — only "the bytes did not form the
/// structure we expected".
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("unexpected end of input at byte {offset} (needed {needed} more)")]
    UnexpectedEof { offset: usize, needed: usize },
    #[error("length {length} overflows the remaining input at byte {offset}")]
    LengthOverflow { offset: usize, length: usize },
    #[error("integer overflow while parsing {operation}")]
    Overflow { operation: &'static str },
    #[error("invalid file header")]
    InvalidFileHeader,
    #[error("invalid segment header at byte {offset}: {reason}")]
    InvalidSegmentHeader {
        offset: usize,
        reason: &'static str,
    },
}

/// The top-level decoder error (jbig2decplan.md §5).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("invalid JBIG2 file header")]
    InvalidFileHeader,

    #[error("parse error: {0}")]
    Parse(#[from] ParseError),

    #[error("error in segment {segment}: {source}")]
    Segment {
        segment: u32,
        #[source]
        source: ParseError,
    },

    #[error("decode limit exceeded: {0}")]
    Limit(#[from] LimitError),

    #[error("integer overflow while decoding {operation}")]
    Overflow { operation: &'static str },

    #[error("malformed stream: {reason}")]
    Malformed { reason: &'static str },

    #[error("duplicate segment number {number}")]
    DuplicateSegment { number: u32 },

    #[error("segment {segment} refers to missing segment {referred}")]
    MissingReferredSegment { segment: u32, referred: u32 },

    #[error("segment {segment} refers to segment {referred} of the wrong type")]
    WrongReferredSegmentType { segment: u32, referred: u32 },

    #[error("unsupported JBIG2 feature: {0}")]
    Unsupported(#[from] UnsupportedFeature),
}

/// Checked `u32` → `usize` conversion for decoder use (jbig2decplan.md §8).
///
/// Unlike the encoder's `u32_to_usize`, this is fallible: on a 16-bit target
/// (or any platform where `usize < u32`) an out-of-range dimension returns a
/// typed error instead of silently truncating.
#[inline]
pub fn usize_from_u32(value: u32, operation: &'static str) -> Result<usize, DecodeError> {
    usize::try_from(value).map_err(|_| DecodeError::Overflow { operation })
}

impl DecodeError {
    /// Convenience constructor threading a limit failure through the top-level
    /// error.
    pub fn limit(inner: LimitError) -> Self {
        DecodeError::Limit(inner)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn display_is_populated() {
        let e = DecodeError::Segment {
            segment: 7,
            source: ParseError::UnexpectedEof {
                offset: 3,
                needed: 4,
            },
        };
        let s = e.to_string();
        assert!(s.contains("segment 7"), "{s}");
        assert!(s.contains("byte 3"), "{s}");
    }

    #[test]
    fn from_conversions() {
        let e: DecodeError = ParseError::InvalidFileHeader.into();
        assert!(matches!(e, DecodeError::Parse(ParseError::InvalidFileHeader)));

        let e: DecodeError = UnsupportedFeature::SymbolCoding.into();
        assert!(matches!(
            e,
            DecodeError::Unsupported(UnsupportedFeature::SymbolCoding)
        ));
    }

    #[test]
    fn checked_conversion() {
        assert_eq!(usize_from_u32(5, "test").unwrap(), 5usize);
    }
}
