//! Full T.88 §7.2 segment-header parser (jbig2decplan.md §9).
//!
//! This parses the complete header syntax, not merely the inverse of the
//! encoder's writer: short *and* long referred-to-count forms, 1/2/4-byte
//! referred segment numbers with the correct `<= 256` / `<= 65536` boundaries
//! (T.88 §7.2.5 — note the writer's `< 256` / `< 65536` is a latent boundary
//! discrepancy, audited separately), 1/4-byte page association, and the
//! unknown-data-length sentinel (flagged `Unsupported` for now).

use crate::decode::error::{DecodeError, LimitError, ParseError, UnsupportedFeature};
use crate::shared::reader::Reader;
use crate::shared::segment::SegmentType;

/// The sentinel data-length value meaning "unknown length" (T.88 §7.2.7).
pub const UNKNOWN_DATA_LENGTH: u32 = 0xFFFF_FFFF;

/// A parsed segment header (T.88 §7.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentHeader {
    /// Segment number (§7.2.2).
    pub number: u32,
    /// Raw 6-bit segment type code (§7.2.3). Kept raw so unknown types round
    /// trip; use [`SegmentHeader::segment_type`] for the decoded enum.
    pub type_code: u8,
    /// Bit 7 of the header flags: the "deferred-non-retain" flag (§7.2.3).
    pub deferred_non_retain: bool,
    /// Bit 6 of the header flags: page association is 4 bytes when set (§7.2.3).
    pub page_association_size_4: bool,
    /// Referred-to segment numbers, in order (§7.2.5).
    pub referred_to: Vec<u32>,
    /// The retain-flag bits (§7.2.4), stored raw for validation/round-trip.
    /// Short form holds the low 5 bits; long form holds the expanded bytes.
    pub retain_flags: RetainFlags,
    /// Page association (§7.2.6).
    pub page_association: u32,
    /// Segment data length in bytes (§7.2.7), or [`UNKNOWN_DATA_LENGTH`].
    pub data_length: u32,
    /// Number of bytes this header occupied.
    pub header_length: usize,
}

/// Retain flags in either the short (§7.2.4 five-bit) or long (expanded) form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetainFlags {
    /// Short form: the low 5 bits of the count/retain byte.
    Short(u8),
    /// Long form: `ceil((count + 1) / 8)` expanded retain-flag bytes.
    Long(Vec<u8>),
}

impl SegmentHeader {
    /// The decoded [`SegmentType`], or `None` for an unassigned type code.
    #[inline]
    pub fn segment_type(&self) -> Option<SegmentType> {
        SegmentType::from_code(self.type_code)
    }

    /// Whether the data length is the "unknown" sentinel.
    #[inline]
    pub fn is_unknown_length(&self) -> bool {
        self.data_length == UNKNOWN_DATA_LENGTH
    }
}

/// The referred-to-segment-number size for a segment with the given number
/// (T.88 §7.2.5): 1 byte if `number <= 256`, 2 bytes if `<= 65536`, else 4.
#[inline]
fn referred_size(number: u32) -> usize {
    if number <= 256 {
        1
    } else if number <= 65_536 {
        2
    } else {
        4
    }
}

/// Parse one segment header from `reader` (T.88 §7.2). On success the reader is
/// positioned at the first byte of the segment *data*.
#[deprecated(
    note = "trusted-input helper; use parse_segment_header_with_limits for untrusted data"
)]
pub fn parse_segment_header(reader: &mut Reader<'_>) -> Result<SegmentHeader, ParseError> {
    parse_segment_header_impl(reader)
}

fn parse_segment_header_impl(reader: &mut Reader<'_>) -> Result<SegmentHeader, ParseError> {
    let start = reader.position();

    // §7.2.2 Segment number.
    let number = reader.read_u32_be()?;

    // §7.2.3 Segment header flags.
    let flags = reader.read_u8()?;
    let type_code = flags & 0x3F;
    let page_association_size_4 = flags & 0x40 != 0;
    let deferred_non_retain = flags & 0x80 != 0;

    // §7.2.4 Referred-to segment count and retention flags.
    let count_byte = reader.read_u8()?;
    let short_count = (count_byte >> 5) & 0x07;
    let (ref_count, retain_flags) = if short_count == 7 {
        // Long form: the count byte's low 5 bits are the top 5 bits of a 32-bit
        // count field; re-read the full 4 bytes (we already consumed the first).
        let b1 = count_byte as u32;
        let b2 = reader.read_u8()? as u32;
        let b3 = reader.read_u8()? as u32;
        let b4 = reader.read_u8()? as u32;
        let raw = (b1 << 24) | (b2 << 16) | (b3 << 8) | b4;
        let count = raw & 0x1FFF_FFFF; // clear the top 3 marker bits (0b111).
        let count_usize = usize::try_from(count).map_err(|_| ParseError::Overflow {
            operation: "referred-to segment count",
        })?;
        // ceil((count + 1) / 8) retain-flag bytes follow.
        let retain_bytes_len = count_usize
            .checked_add(1)
            .and_then(|v| v.checked_add(7))
            .map(|v| v / 8)
            .ok_or(ParseError::Overflow {
                operation: "retain-flag byte count",
            })?;
        let retain = reader.take(retain_bytes_len)?.to_vec();
        (count, RetainFlags::Long(retain))
    } else if short_count <= 4 {
        // Short form: low 5 bits are retention flags, count is 0..=4.
        (short_count as u32, RetainFlags::Short(count_byte & 0x1F))
    } else {
        return Err(ParseError::InvalidSegmentHeader {
            offset: reader.position().saturating_sub(1),
            reason: "reserved short-form referred-to count",
        });
    };

    // §7.2.5 Referred-to segment numbers.
    let ref_count_usize = usize::try_from(ref_count).map_err(|_| ParseError::Overflow {
        operation: "referred-to segment count to usize",
    })?;
    let size = referred_size(number);
    let mut referred_to = Vec::with_capacity(ref_count_usize.min(1024));
    for _ in 0..ref_count_usize {
        let value = match size {
            1 => reader.read_u8()? as u32,
            2 => reader.read_u16_be()? as u32,
            _ => reader.read_u32_be()?,
        };
        referred_to.push(value);
    }

    // §7.2.6 Segment page association.
    let page_association = if page_association_size_4 {
        reader.read_u32_be()?
    } else {
        reader.read_u8()? as u32
    };

    // §7.2.7 Segment data length.
    let data_length = reader.read_u32_be()?;

    let header_length = reader.position().saturating_sub(start);

    Ok(SegmentHeader {
        number,
        type_code,
        deferred_non_retain,
        page_association_size_4,
        referred_to,
        retain_flags,
        page_association,
        data_length,
        header_length,
    })
}

/// Parse one segment header while enforcing the caller's referred-segment
/// limit before allocating either the retention flags or referred-number
/// vector.
///
/// The low-level [`parse_segment_header`] remains available for tools that only
/// parse trusted headers. Document decoding must use this bounded entry point:
/// in the long form the encoded count can be as large as 2^29-1, so checking
/// only after parsing allows an attacker-controlled allocation first.
pub fn parse_segment_header_with_limits(
    reader: &mut Reader<'_>,
    limits: &crate::shared::limits::DecodeLimits,
) -> Result<SegmentHeader, DecodeError> {
    parse_segment_header_limited(reader, limits.max_referred_segments)
}

pub(crate) fn parse_segment_header_limited(
    reader: &mut Reader<'_>,
    max_referred_segments: usize,
) -> Result<SegmentHeader, DecodeError> {
    // Segment number (4), flags (1), then the count/retention byte.
    let prefix = reader.peek(6)?;
    let count_byte = prefix[5];
    let short_count = (count_byte >> 5) & 0x07;
    let count = if short_count == 7 {
        // The first byte of the four-byte long-form count is the byte already
        // at offset 5. Peek through the remaining three count bytes without
        // advancing or allocating.
        let long_prefix = reader.peek(9)?;
        let raw = u32::from_be_bytes([
            long_prefix[5],
            long_prefix[6],
            long_prefix[7],
            long_prefix[8],
        ]);
        (raw & 0x1FFF_FFFF) as u64
    } else {
        short_count as u64
    };
    if count > max_referred_segments as u64 {
        return Err(DecodeError::limit(LimitError::Count {
            what: "referred-to segments",
            value: count,
            limit: max_referred_segments as u64,
        }));
    }
    parse_segment_header_impl(reader).map_err(DecodeError::Parse)
}

/// Reject a header whose data length is unknown (T.88 §7.2.7). The self-decoder
/// (Phase 1) never emits unknown-length segments, so this is `Unsupported`.
pub fn ensure_known_length(header: &SegmentHeader) -> Result<u32, UnsupportedFeature> {
    if header.is_unknown_length() {
        Err(UnsupportedFeature::UnknownSegmentLength)
    } else {
        Ok(header.data_length)
    }
}

#[cfg(test)]
#[allow(deprecated, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Build a short-form header: number, flags, count/retain, referred-to (by
    /// `size`), page-assoc (1 byte), data length.
    fn short_header(
        number: u32,
        type_code: u8,
        referred: &[u32],
        page: u8,
        data_len: u32,
    ) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&number.to_be_bytes());
        v.push(type_code & 0x3F);
        v.push((referred.len() as u8) << 5);
        let size = referred_size(number);
        for &r in referred {
            match size {
                1 => v.push(r as u8),
                2 => v.extend_from_slice(&(r as u16).to_be_bytes()),
                _ => v.extend_from_slice(&r.to_be_bytes()),
            }
        }
        v.push(page);
        v.extend_from_slice(&data_len.to_be_bytes());
        v
    }

    #[test]
    fn zero_references() {
        let bytes = short_header(0, 48, &[], 1, 19);
        let mut r = Reader::new(&bytes);
        let h = parse_segment_header(&mut r).unwrap();
        assert_eq!(h.number, 0);
        assert_eq!(h.segment_type(), Some(SegmentType::PageInformation));
        assert!(h.referred_to.is_empty());
        assert_eq!(h.page_association, 1);
        assert_eq!(h.data_length, 19);
        assert_eq!(h.header_length, bytes.len());
    }

    #[test]
    fn four_references_short_form() {
        let bytes = short_header(10, 6, &[1, 2, 3, 4], 1, 100);
        let mut r = Reader::new(&bytes);
        let h = parse_segment_header(&mut r).unwrap();
        assert_eq!(h.referred_to, vec![1, 2, 3, 4]);
        assert_eq!(h.segment_type(), Some(SegmentType::ImmediateTextRegion));
    }

    #[test]
    fn five_references_long_form() {
        // Long form: count = 5, top 3 bits of first byte = 111.
        let number = 20u32;
        let mut v = Vec::new();
        v.extend_from_slice(&number.to_be_bytes());
        v.push(0); // flags: type 0
        // Long count field: 0b111 << 29 | 5.
        let raw: u32 = (0b111 << 29) | 5;
        v.extend_from_slice(&raw.to_be_bytes());
        // retain flags: ceil((5+1)/8) = 1 byte.
        v.push(0x00);
        // 5 referred-to numbers, 1 byte each (number 20 <= 256).
        for r in [1u8, 2, 3, 4, 5] {
            v.push(r);
        }
        v.push(1); // page
        v.extend_from_slice(&50u32.to_be_bytes());
        let mut rd = Reader::new(&v);
        let h = parse_segment_header(&mut rd).unwrap();
        assert_eq!(h.referred_to, vec![1, 2, 3, 4, 5]);
        assert!(matches!(h.retain_flags, RetainFlags::Long(ref b) if b.len() == 1));
    }

    #[test]
    fn referred_size_boundaries() {
        // §7.2.5 boundaries: 256 -> 1 byte, 257 -> 2 bytes, 65536 -> 2, 65537 -> 4.
        assert_eq!(referred_size(255), 1);
        assert_eq!(referred_size(256), 1);
        assert_eq!(referred_size(257), 2);
        assert_eq!(referred_size(65_535), 2);
        assert_eq!(referred_size(65_536), 2);
        assert_eq!(referred_size(65_537), 4);
    }

    #[test]
    fn reserved_short_form_counts_are_rejected() {
        for count in [5u8, 6] {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&1u32.to_be_bytes());
            bytes.push(48);
            bytes.push(count << 5);
            let mut r = Reader::new(&bytes);
            assert!(matches!(
                parse_segment_header(&mut r),
                Err(ParseError::InvalidSegmentHeader {
                    reason: "reserved short-form referred-to count",
                    ..
                })
            ));
        }
    }

    #[test]
    fn two_byte_references_at_257() {
        // Segment 257 refers to segment 300 (2-byte referred numbers).
        let bytes = short_header(257, 0, &[300], 1, 0);
        let mut r = Reader::new(&bytes);
        let h = parse_segment_header(&mut r).unwrap();
        assert_eq!(h.referred_to, vec![300]);
    }

    #[test]
    fn four_byte_references_at_65537() {
        let bytes = short_header(65_537, 0, &[70_000], 1, 0);
        let mut r = Reader::new(&bytes);
        let h = parse_segment_header(&mut r).unwrap();
        assert_eq!(h.referred_to, vec![70_000]);
    }

    #[test]
    fn four_byte_page_association() {
        let number = 5u32;
        let mut v = Vec::new();
        v.extend_from_slice(&number.to_be_bytes());
        v.push(0x40); // bit 6: 4-byte page association
        v.push(0); // no references
        v.extend_from_slice(&1000u32.to_be_bytes()); // page = 1000 (4 bytes)
        v.extend_from_slice(&12u32.to_be_bytes()); // data length
        let mut r = Reader::new(&v);
        let h = parse_segment_header(&mut r).unwrap();
        assert!(h.page_association_size_4);
        assert_eq!(h.page_association, 1000);
    }

    #[test]
    fn unknown_length_flagged() {
        let bytes = short_header(1, 38, &[], 1, UNKNOWN_DATA_LENGTH);
        let mut r = Reader::new(&bytes);
        let h = parse_segment_header(&mut r).unwrap();
        assert!(h.is_unknown_length());
        assert_eq!(
            ensure_known_length(&h),
            Err(UnsupportedFeature::UnknownSegmentLength)
        );
    }

    #[test]
    fn truncated_header_is_error() {
        let bytes = short_header(0, 48, &[], 1, 19);
        for cut in 0..bytes.len() {
            let mut r = Reader::new(&bytes[..cut]);
            assert!(
                parse_segment_header(&mut r).is_err(),
                "cut at {cut} should error"
            );
        }
    }

    #[test]
    fn deferred_non_retain_flag() {
        let mut bytes = short_header(1, 38, &[], 1, 0);
        bytes[4] |= 0x80; // set deferred-non-retain in flags byte
        let mut r = Reader::new(&bytes);
        let h = parse_segment_header(&mut r).unwrap();
        assert!(h.deferred_non_retain);
    }
}
