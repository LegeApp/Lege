//! File- and stream-level parsing (jbig2decplan.md §9): the standalone
//! sequential file organisation (T.88 Annex D) and the bare embedded/PDF
//! segment sequence. Parsing is separated from execution — this module only
//! locates segment boundaries and slices payloads; nothing is decoded here.

use crate::decode::context::{DecodeStrictness, RecoveryEvent};
use crate::decode::error::{DecodeError, ParseError, UnsupportedFeature};
use crate::decode::segment::{SegmentHeader, ensure_known_length, parse_segment_header_limited};
use crate::shared::limits::DecodeLimits;
use crate::shared::reader::Reader;
use crate::shared::segment::SegmentType;

/// The 8-byte JBIG2 file identification string (T.88 §D.4.1).
pub const FILE_MAGIC: [u8; 8] = [0x97, 0x4A, 0x42, 0x32, 0x0D, 0x0A, 0x1A, 0x0A];

/// File organisation (T.88 §D.4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOrganization {
    /// Sequential: each segment header is immediately followed by its data.
    Sequential,
    /// Random access: all segment headers precede all segment data (T.88 §D.2).
    RandomAccess,
    /// Embedded/PDF: a bare segment sequence with no file header.
    Embedded,
}

/// One parsed segment: its header plus a borrow of its data bytes.
#[derive(Clone, Debug)]
pub struct ParsedSegment<'a> {
    pub header: SegmentHeader,
    pub data: &'a [u8],
}

/// A parsed document: the organisation plus every segment in stream order.
#[derive(Clone, Debug)]
pub struct ParsedDocument<'a> {
    pub organization: FileOrganization,
    pub segments: Vec<ParsedSegment<'a>>,
    /// Malformations tolerated in [`DecodeStrictness::Compatible`] mode.
    pub recovery: Vec<RecoveryEvent>,
}

/// Parse a standalone JBIG2 file (T.88 Annex D) in strict mode.
pub fn parse_file<'a>(
    data: &'a [u8],
    limits: &DecodeLimits,
) -> Result<ParsedDocument<'a>, DecodeError> {
    parse_file_with(data, limits, DecodeStrictness::Strict)
}

/// Parse a standalone JBIG2 file with an explicit strictness.
pub fn parse_file_with<'a>(
    data: &'a [u8],
    limits: &DecodeLimits,
    strictness: DecodeStrictness,
) -> Result<ParsedDocument<'a>, DecodeError> {
    let mut reader = Reader::new(data);

    let magic = reader.take(FILE_MAGIC.len())?;
    if magic != FILE_MAGIC {
        return Err(DecodeError::InvalidFileHeader);
    }

    // §D.4.2 file-header flags. Bit 0 set => sequential organisation (matches
    // this crate's encoder and jbig2dec 0.20); clear => random access. Bit 1 =>
    // number of pages unknown (the 4-byte page count is then omitted).
    let flags = reader.read_u8()?;
    let reserved = flags & 0xFC;
    let mut recovery = Vec::new();
    if reserved != 0 {
        if strictness == DecodeStrictness::Strict {
            return Err(DecodeError::Malformed {
                reason: "reserved standalone file-header flags are non-zero",
            });
        }
        recovery.push(RecoveryEvent::ReservedFileHeaderBits { bits: reserved });
    }
    let sequential = flags & 0x01 != 0;
    let unknown_pages = flags & 0x02 != 0;
    let declared_pages = if !unknown_pages {
        // 4-byte number of pages; parsed but unused here.
        Some(reader.read_u32_be()?)
    } else {
        None
    };

    let segments = if sequential {
        parse_segment_sequence(&mut reader, limits, strictness, &mut recovery)?
    } else {
        parse_random_access(&mut reader, limits)?
    };
    if let Some(declared) = declared_pages {
        let actual = segments
            .iter()
            .filter(|s| matches!(s.header.segment_type(), Some(SegmentType::PageInformation)))
            .count();
        let actual = u32::try_from(actual).map_err(|_| DecodeError::Overflow {
            operation: "decoded page count",
        })?;
        if declared != actual {
            if strictness == DecodeStrictness::Strict {
                return Err(DecodeError::Malformed {
                    reason: "declared page count does not match page-information segments",
                });
            }
            recovery.push(RecoveryEvent::DeclaredPageCountMismatch { declared, actual });
        }
    }
    Ok(ParsedDocument {
        organization: if sequential {
            FileOrganization::Sequential
        } else {
            FileOrganization::RandomAccess
        },
        segments,
        recovery,
    })
}

/// Parse the random-access organisation (T.88 §D.2): all segment headers first
/// (the last must be an end-of-file segment), then every segment's data in the
/// same order.
fn parse_random_access<'a>(
    reader: &mut Reader<'a>,
    limits: &DecodeLimits,
) -> Result<Vec<ParsedSegment<'a>>, DecodeError> {
    // 1) Read segment headers up to and including the end-of-file header.
    let mut headers: Vec<SegmentHeader> = Vec::new();
    loop {
        if headers.len() >= limits.max_segments {
            return Err(DecodeError::limit(
                crate::decode::error::LimitError::Count {
                    what: "segments",
                    value: headers.len() as u64,
                    limit: limits.max_segments as u64,
                },
            ));
        }
        let header = parse_segment_header_limited(reader, limits.max_referred_segments)?;
        let is_eof = matches!(header.segment_type(), Some(SegmentType::EndOfFile));
        headers.push(header);
        if is_eof {
            break;
        }
        if reader.is_empty() {
            // No end-of-file header was found: the boundary between headers and
            // data cannot be determined (§D.2).
            return Err(DecodeError::Malformed {
                reason: "random-access file has no end-of-file segment",
            });
        }
    }

    // 2) The data section follows the last header, one block per header in order.
    let mut segments = Vec::with_capacity(headers.len());
    for header in headers {
        let len = ensure_known_length(&header).map_err(DecodeError::Unsupported)?;
        let len = usize::try_from(len).map_err(|_| DecodeError::Overflow {
            operation: "segment data length to usize",
        })?;
        let data = reader.take(len).map_err(|source| DecodeError::Segment {
            segment: header.number,
            source,
        })?;
        segments.push(ParsedSegment { header, data });
    }
    Ok(segments)
}

/// Parse a bare embedded/PDF segment sequence (no file header).
pub fn parse_embedded<'a>(
    data: &'a [u8],
    limits: &DecodeLimits,
) -> Result<ParsedDocument<'a>, DecodeError> {
    parse_embedded_with(data, limits, DecodeStrictness::Strict)
}

/// Parse a bare embedded segment sequence with an explicit strictness.
pub fn parse_embedded_with<'a>(
    data: &'a [u8],
    limits: &DecodeLimits,
    strictness: DecodeStrictness,
) -> Result<ParsedDocument<'a>, DecodeError> {
    let mut reader = Reader::new(data);
    let mut recovery = Vec::new();
    let segments = parse_segment_sequence(&mut reader, limits, strictness, &mut recovery)?;
    Ok(ParsedDocument {
        organization: FileOrganization::Embedded,
        segments,
        recovery,
    })
}

/// Parse consecutive `[header][data]` segments until the reader is exhausted.
/// In [`DecodeStrictness::Compatible`] mode, trailing bytes after the last
/// well-formed segment that fail to parse as a segment header are tolerated and
/// recorded as a [`RecoveryEvent::TrailingGarbage`].
fn parse_segment_sequence<'a>(
    reader: &mut Reader<'a>,
    limits: &DecodeLimits,
    strictness: DecodeStrictness,
    recovery: &mut Vec<RecoveryEvent>,
) -> Result<Vec<ParsedSegment<'a>>, DecodeError> {
    let mut segments = Vec::new();
    while !reader.is_empty() {
        if segments.len() >= limits.max_segments {
            return Err(DecodeError::limit(
                crate::decode::error::LimitError::Count {
                    what: "segments",
                    value: segments.len() as u64,
                    limit: limits.max_segments as u64,
                },
            ));
        }
        let seg_start = reader.position();
        let remaining_at_start = reader.remaining();
        let header = match parse_segment_header_limited(reader, limits.max_referred_segments) {
            Ok(h) => h,
            Err(DecodeError::Parse(e)) => {
                // A malformed header after at least one good segment, in
                // Compatible mode, is treated as trailing garbage.
                if strictness == DecodeStrictness::Compatible && !segments.is_empty() {
                    recovery.push(RecoveryEvent::TrailingGarbage {
                        offset: seg_start,
                        bytes: remaining_at_start,
                    });
                    break;
                }
                return Err(DecodeError::Parse(e));
            }
            Err(e) => return Err(e),
        };
        // §7.2.7 unknown data length: legal only for immediate generic regions,
        // whose length is recovered by scanning for the terminator sequence.
        let data = if header.is_unknown_length() {
            let ty = header.segment_type();
            if !matches!(ty, Some(SegmentType::ImmediateGenericRegion)) {
                return Err(DecodeError::Unsupported(
                    UnsupportedFeature::UnknownSegmentLength,
                ));
            }
            let remaining = reader.peek(reader.remaining()).unwrap_or(&[]);
            let len = unknown_generic_length(remaining)?;
            reader.take(len).map_err(|source| DecodeError::Segment {
                segment: header.number,
                source,
            })?
        } else {
            let len = usize::try_from(header.data_length).map_err(|_| DecodeError::Overflow {
                operation: "segment data length to usize",
            })?;
            reader.take(len).map_err(|source| DecodeError::Segment {
                segment: header.number,
                source,
            })?
        };
        segments.push(ParsedSegment { header, data });
    }
    Ok(segments)
}

/// Determine the data-part length of an unknown-length immediate generic region
/// (T.88 §7.2.7). The data begins with the 17-byte region-segment information
/// field and a 1-byte generic-flags field (the "eighteenth byte"), whose bit 0
/// selects MMR. The data ends with a terminator — `0xFF 0xAC` (arithmetic) or
/// `0x00 0x00` (MMR) — that can occur anywhere after the eighteenth byte,
/// followed by a four-byte row count. Returns the total data length in bytes.
fn unknown_generic_length(data: &[u8]) -> Result<usize, DecodeError> {
    // Need at least the 18-byte prefix plus a 2-byte terminator and 4-byte count.
    const PREFIX: usize = 18;
    if data.len() < PREFIX + 6 {
        return Err(DecodeError::Malformed {
            reason: "unknown-length generic region too short",
        });
    }
    let mmr = data[17] & 0x01 != 0;
    let (a, b) = if mmr { (0x00, 0x00) } else { (0xFF, 0xAC) };
    // Search for the terminator starting at the eighteenth byte (index 17),
    // scanning forward. The row count is the four bytes immediately after it.
    let mut i = PREFIX;
    while i + 6 <= data.len() {
        if data[i] == a && data[i + 1] == b {
            return Ok(i + 2 + 4);
        }
        i += 1;
    }
    Err(DecodeError::Malformed {
        reason: "unknown-length generic region terminator not found",
    })
}

/// Detect whether `data` begins with the JBIG2 file magic.
#[inline]
pub fn has_file_magic(data: &[u8]) -> bool {
    data.len() >= FILE_MAGIC.len() && data[..FILE_MAGIC.len()] == FILE_MAGIC
}

/// Parse either a standalone file (if the magic is present) or an embedded
/// stream (otherwise), in strict mode.
pub fn parse_auto<'a>(
    data: &'a [u8],
    limits: &DecodeLimits,
) -> Result<ParsedDocument<'a>, DecodeError> {
    parse_auto_with(data, limits, DecodeStrictness::Strict)
}

/// Parse either organisation with an explicit strictness.
pub fn parse_auto_with<'a>(
    data: &'a [u8],
    limits: &DecodeLimits,
    strictness: DecodeStrictness,
) -> Result<ParsedDocument<'a>, DecodeError> {
    if has_file_magic(data) {
        parse_file_with(data, limits, strictness)
    } else {
        parse_embedded_with(data, limits, strictness)
    }
}

/// Guard: a stray `ParseError` used only internally.
#[allow(dead_code)]
fn _assert_parse_error_is_used(_: ParseError) {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::shared::segment::SegmentType;

    fn limits() -> DecodeLimits {
        DecodeLimits::default()
    }

    /// A minimal sequential file: magic + flags + n_pages + one page-info
    /// segment (empty data) + one EOF segment.
    fn minimal_file() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&FILE_MAGIC);
        v.push(0x01); // sequential
        v.extend_from_slice(&1u32.to_be_bytes()); // n_pages
        // Segment 0: page info, 4 bytes data.
        v.extend_from_slice(&0u32.to_be_bytes());
        v.push(48); // type page info
        v.push(0); // no refs
        v.push(1); // page
        v.extend_from_slice(&4u32.to_be_bytes()); // data length
        v.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        // Segment 1: EOF, 0 bytes.
        v.extend_from_slice(&1u32.to_be_bytes());
        v.push(51);
        v.push(0);
        v.push(0);
        v.extend_from_slice(&0u32.to_be_bytes());
        v
    }

    #[test]
    fn parses_sequential_file() {
        let bytes = minimal_file();
        let doc = parse_file(&bytes, &limits()).unwrap();
        assert_eq!(doc.organization, FileOrganization::Sequential);
        assert_eq!(doc.segments.len(), 2);
        assert_eq!(
            doc.segments[0].header.segment_type(),
            Some(SegmentType::PageInformation)
        );
        assert_eq!(doc.segments[0].data, &[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(
            doc.segments[1].header.segment_type(),
            Some(SegmentType::EndOfFile)
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = minimal_file();
        bytes[0] = 0x00;
        assert!(matches!(
            parse_file(&bytes, &limits()),
            Err(DecodeError::InvalidFileHeader)
        ));
    }

    #[test]
    fn reserved_file_flags_respect_strictness() {
        let mut bytes = minimal_file();
        bytes[8] |= 0x80;
        assert!(matches!(
            parse_file_with(&bytes, &limits(), DecodeStrictness::Strict),
            Err(DecodeError::Malformed {
                reason: "reserved standalone file-header flags are non-zero"
            })
        ));

        let doc = parse_file_with(&bytes, &limits(), DecodeStrictness::Compatible).unwrap();
        assert_eq!(
            doc.recovery,
            vec![RecoveryEvent::ReservedFileHeaderBits { bits: 0x80 }]
        );
    }

    #[test]
    fn declared_page_count_respects_strictness() {
        let mut bytes = minimal_file();
        bytes[9..13].copy_from_slice(&2u32.to_be_bytes());
        assert!(parse_file(&bytes, &limits()).is_err());
        let doc = parse_file_with(&bytes, &limits(), DecodeStrictness::Compatible).unwrap();
        assert!(matches!(
            doc.recovery.as_slice(),
            [RecoveryEvent::DeclaredPageCountMismatch {
                declared: 2,
                actual: 1
            }]
        ));
    }

    /// A minimal random-access file (§D.2): magic + flags (random access) +
    /// n_pages + [page-info header][EOF header] + [page-info data].
    fn minimal_random_access_file() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&FILE_MAGIC);
        v.push(0x00); // random access, number of pages known
        v.extend_from_slice(&1u32.to_be_bytes()); // n_pages
        // Header for segment 0: page info, 4 bytes data.
        v.extend_from_slice(&0u32.to_be_bytes());
        v.push(48); // page info
        v.push(0); // no refs
        v.push(1); // page
        v.extend_from_slice(&4u32.to_be_bytes()); // data length
        // Header for segment 1: EOF, 0 bytes data.
        v.extend_from_slice(&1u32.to_be_bytes());
        v.push(51);
        v.push(0);
        v.push(0);
        v.extend_from_slice(&0u32.to_be_bytes());
        // Data section: segment 0's four bytes (segment 1 has none).
        v.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        v
    }

    #[test]
    fn random_access_parses_headers_then_data() {
        let bytes = minimal_random_access_file();
        let doc = parse_file(&bytes, &limits()).unwrap();
        assert_eq!(doc.organization, FileOrganization::RandomAccess);
        assert_eq!(doc.segments.len(), 2);
        assert_eq!(
            doc.segments[0].header.segment_type(),
            Some(SegmentType::PageInformation)
        );
        assert_eq!(doc.segments[0].data, &[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(
            doc.segments[1].header.segment_type(),
            Some(SegmentType::EndOfFile)
        );
        assert!(doc.segments[1].data.is_empty());
    }

    #[test]
    fn random_access_without_eof_is_malformed() {
        let mut bytes = minimal_random_access_file();
        // Turn the EOF segment (type 51) into a non-EOF type so no terminator
        // header is found and the header/data boundary is undeterminable.
        // The EOF header's flags byte is at: 8+1+4 (file hdr) + 11 (seg0 hdr) + 4
        // (seg1 number) = 28.
        bytes[28] = 48; // page info instead of EOF
        assert!(parse_file(&bytes, &limits()).is_err());
    }

    #[test]
    fn embedded_sequence_without_header() {
        // Just the two segments, no file magic.
        let full = minimal_file();
        let embedded = &full[13..]; // skip 8 magic + 1 flags + 4 n_pages
        let doc = parse_embedded(embedded, &limits()).unwrap();
        assert_eq!(doc.organization, FileOrganization::Embedded);
        assert_eq!(doc.segments.len(), 2);
    }

    #[test]
    fn auto_detects_organisation() {
        let full = minimal_file();
        assert_eq!(
            parse_auto(&full, &limits()).unwrap().organization,
            FileOrganization::Sequential
        );
        let embedded = &full[13..];
        assert_eq!(
            parse_auto(embedded, &limits()).unwrap().organization,
            FileOrganization::Embedded
        );
    }

    #[test]
    fn truncated_payload_is_error() {
        let mut bytes = minimal_file();
        bytes.truncate(bytes.len() - 6); // cut into the first segment payload/EOF
        assert!(parse_file(&bytes, &limits()).is_err());
    }

    #[test]
    fn referred_segment_limit_is_checked_before_long_form_allocation() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.push(48); // page information
        // Long form marker plus an encoded referred-to count of 65,537.
        bytes.extend_from_slice(&0xE001_0001u32.to_be_bytes());

        let mut constrained = limits();
        constrained.max_referred_segments = 4;
        let err = parse_embedded(&bytes, &constrained).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::Limit(crate::decode::error::LimitError::Count {
                what: "referred-to segments",
                value: 65_537,
                limit: 4,
            })
        ));
    }
}
