//! Protobuf wire-format decoding for the ONNX subset Lege's runtime needs.
//!
//! The wire format is a sequence of `(field_number, wire_type)` tags, each
//! followed by a payload whose shape the wire type determines. That is all this
//! module implements — there is no schema, no reflection, and no code
//! generation. `proto/onnx.proto` remains in the tree as the reference
//! document; [`super`] implements the eleven messages of it that the graph
//! loader reads.
//!
//! Three properties matter for reading untrusted model files:
//!
//! * **Every length is checked against the remaining buffer.** A truncated or
//!   hostile length yields [`WireError`], never a panic or an over-read.
//! * **Varints are capped at ten bytes** and rejected on overflow, so a run of
//!   continuation bytes cannot spin or silently wrap.
//! * **Unknown fields are skipped, not rejected.** Real models carry
//!   `doc_string`, `metadata_props`, training info and exporter extensions that
//!   this runtime ignores; forward compatibility is what keeps a newer export
//!   loadable.

use std::ops::Range;

/// A failure to decode the protobuf wire format.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("truncated protobuf input at byte {offset} (needed {needed} more)")]
    Truncated { offset: usize, needed: usize },
    #[error("varint at byte {offset} exceeds 64 bits")]
    VarintOverflow { offset: usize },
    #[error("unsupported wire type {wire_type} for field {field} at byte {offset}")]
    WireType {
        field: u32,
        wire_type: u8,
        offset: usize,
    },
    #[error("field number 0 is not valid (byte {offset})")]
    ZeroField { offset: usize },
    #[error("packed {kind} payload at byte {offset} is not a multiple of {unit} bytes")]
    PackedLength {
        kind: &'static str,
        offset: usize,
        unit: usize,
    },
    #[error("string field at byte {offset} is not valid UTF-8")]
    Utf8 { offset: usize },
}

pub type Result<T> = std::result::Result<T, WireError>;

/// Wire type 0: varint.
pub(crate) const WIRE_VARINT: u8 = 0;
/// Wire type 1: 64-bit fixed.
pub(crate) const WIRE_FIXED64: u8 = 1;
/// Wire type 2: length-delimited.
pub(crate) const WIRE_LEN: u8 = 2;
/// Wire type 5: 32-bit fixed.
pub(crate) const WIRE_FIXED32: u8 = 5;

/// A cursor over one protobuf message body.
///
/// `base` is the offset of `buf[0]` within the whole model file, so a
/// length-delimited payload can report an absolute range. That is what lets
/// `raw_data` be recorded as a range into the single owned model buffer instead
/// of being copied out — the difference between one and two copies of a
/// multi-megabyte weight tensor.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    base: usize,
}

impl<'a> Reader<'a> {
    /// A reader over a whole model file.
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            base: 0,
        }
    }

    /// Whether the message body has been fully consumed.
    pub(crate) fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// Absolute offset of the cursor within the model file.
    pub(crate) fn offset(&self) -> usize {
        self.base + self.pos
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(len).ok_or(WireError::Truncated {
            offset: self.offset(),
            needed: len,
        })?;
        if end > self.buf.len() {
            return Err(WireError::Truncated {
                offset: self.offset(),
                needed: end - self.buf.len(),
            });
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Decode a base-128 varint. Capped at ten bytes; the tenth may only carry
    /// the single remaining bit of a 64-bit value.
    pub(crate) fn varint(&mut self) -> Result<u64> {
        let start = self.offset();
        let mut value = 0u64;
        for index in 0..10 {
            let byte = *self.buf.get(self.pos).ok_or(WireError::Truncated {
                offset: self.offset(),
                needed: 1,
            })?;
            self.pos += 1;
            let payload = u64::from(byte & 0x7F);
            if index == 9 {
                // Only bit 64 may remain; anything else overflows.
                if byte & 0x7F > 1 {
                    return Err(WireError::VarintOverflow { offset: start });
                }
            }
            value |= payload << (index * 7);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(WireError::VarintOverflow { offset: start })
    }

    /// Read the next tag, or `None` at the end of the message body.
    pub(crate) fn tag(&mut self) -> Result<Option<(u32, u8)>> {
        if self.is_empty() {
            return Ok(None);
        }
        let offset = self.offset();
        let key = self.varint()?;
        let field = u32::try_from(key >> 3).map_err(|_| WireError::ZeroField { offset })?;
        if field == 0 {
            return Err(WireError::ZeroField { offset });
        }
        Ok(Some((field, (key & 0x7) as u8)))
    }

    /// Read a length-delimited payload, borrowed from the input.
    pub(crate) fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.varint()?;
        let len = usize::try_from(len).map_err(|_| WireError::Truncated {
            offset: self.offset(),
            needed: usize::MAX,
        })?;
        self.take(len)
    }

    /// Read a length-delimited payload and report its absolute range in the
    /// model file alongside the borrowed bytes.
    pub(crate) fn bytes_range(&mut self) -> Result<(&'a [u8], Range<usize>)> {
        let len = self.varint()?;
        let len = usize::try_from(len).map_err(|_| WireError::Truncated {
            offset: self.offset(),
            needed: usize::MAX,
        })?;
        let start = self.offset();
        let slice = self.take(len)?;
        Ok((slice, start..start + len))
    }

    /// Read a length-delimited payload as a UTF-8 string.
    pub(crate) fn string(&mut self) -> Result<String> {
        let offset = self.offset();
        let bytes = self.bytes()?;
        String::from_utf8(bytes.to_vec()).map_err(|_| WireError::Utf8 { offset })
    }

    /// A reader over the next length-delimited payload — a nested message, or a
    /// packed repeated field.
    pub(crate) fn nested(&mut self) -> Result<Reader<'a>> {
        let (buf, range) = self.bytes_range()?;
        Ok(Reader {
            buf,
            pos: 0,
            base: range.start,
        })
    }

    pub(crate) fn fixed32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Skip a field this runtime does not read.
    pub(crate) fn skip(&mut self, field: u32, wire_type: u8) -> Result<()> {
        match wire_type {
            WIRE_VARINT => {
                self.varint()?;
            }
            WIRE_FIXED64 => {
                self.take(8)?;
            }
            WIRE_LEN => {
                self.bytes()?;
            }
            WIRE_FIXED32 => {
                self.take(4)?;
            }
            // Wire types 3 and 4 are the deprecated start/end-group pair; ONNX
            // has never used them, and a stream carrying one is not a model
            // this runtime can make sense of.
            _ => {
                return Err(WireError::WireType {
                    field,
                    wire_type,
                    offset: self.offset(),
                });
            }
        }
        Ok(())
    }
}

// ── Repeated-field helpers ───────────────────────────────────────────────────
//
// Every repeated numeric field below accepts *both* encodings. Proto3 packs
// scalars by default and proto2 does not, and ONNX models in the wild carry
// both: `dims` is unpacked in many exporters while `float_data` is declared
// `[packed = true]`. A reader that assumed one encoding would silently read an
// empty list from half the models it was given.

/// Append one repeated `int64`, packed or not.
pub(crate) fn read_repeated_i64(
    reader: &mut Reader<'_>,
    field: u32,
    wire_type: u8,
    out: &mut Vec<i64>,
) -> Result<()> {
    match wire_type {
        WIRE_VARINT => out.push(reader.varint()? as i64),
        WIRE_LEN => {
            let mut packed = reader.nested()?;
            while !packed.is_empty() {
                out.push(packed.varint()? as i64);
            }
        }
        _ => {
            return Err(WireError::WireType {
                field,
                wire_type,
                offset: reader.offset(),
            });
        }
    }
    Ok(())
}

/// Append one repeated `int32`, packed or not.
pub(crate) fn read_repeated_i32(
    reader: &mut Reader<'_>,
    field: u32,
    wire_type: u8,
    out: &mut Vec<i32>,
) -> Result<()> {
    match wire_type {
        WIRE_VARINT => out.push(reader.varint()? as i32),
        WIRE_LEN => {
            let mut packed = reader.nested()?;
            while !packed.is_empty() {
                out.push(packed.varint()? as i32);
            }
        }
        _ => {
            return Err(WireError::WireType {
                field,
                wire_type,
                offset: reader.offset(),
            });
        }
    }
    Ok(())
}

/// Append one repeated `float`, packed or not.
///
/// The packed case decodes element-by-element from little-endian bytes rather
/// than casting the payload: a length-delimited payload starts wherever the
/// preceding varint left off, so it carries no alignment guarantee, and a
/// `[u8]`-to-`[f32]` cast of it would fail (or, done by hand, be unsound).
pub(crate) fn read_repeated_f32(
    reader: &mut Reader<'_>,
    field: u32,
    wire_type: u8,
    out: &mut Vec<f32>,
) -> Result<()> {
    match wire_type {
        WIRE_FIXED32 => out.push(f32::from_bits(reader.fixed32()?)),
        WIRE_LEN => {
            let offset = reader.offset();
            let bytes = reader.bytes()?;
            if bytes.len() % 4 != 0 {
                return Err(WireError::PackedLength {
                    kind: "float",
                    offset,
                    unit: 4,
                });
            }
            out.reserve(bytes.len() / 4);
            out.extend(
                bytes
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
            );
        }
        _ => {
            return Err(WireError::WireType {
                field,
                wire_type,
                offset: reader.offset(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trips_boundary_values() {
        for value in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut encoded = Vec::new();
            let mut v = value;
            loop {
                let byte = (v & 0x7F) as u8;
                v >>= 7;
                if v == 0 {
                    encoded.push(byte);
                    break;
                }
                encoded.push(byte | 0x80);
            }
            let mut reader = Reader::new(&encoded);
            assert_eq!(reader.varint().unwrap(), value, "value {value}");
        }
    }

    #[test]
    fn varint_rejects_overlong_and_overflowing_input() {
        // Eleven continuation bytes: capped before it can spin.
        let overlong = [0x80u8; 11];
        assert!(Reader::new(&overlong).varint().is_err());
        // Ten bytes whose final group carries more than the one remaining bit.
        let mut overflow = [0x80u8; 10];
        overflow[9] = 0x7F;
        assert!(Reader::new(&overflow).varint().is_err());
    }

    #[test]
    fn truncated_length_delimited_field_errors_rather_than_panicking() {
        // Length says 8 bytes, only 2 follow.
        let mut reader = Reader::new(&[0x08, 0xAA, 0xBB]);
        assert!(reader.bytes().is_err());
    }

    #[test]
    fn zero_field_number_is_rejected() {
        assert!(Reader::new(&[0x00]).tag().is_err());
    }

    #[test]
    fn unknown_fields_of_every_wire_type_are_skipped() {
        // field 1 varint, field 2 fixed64, field 3 len, field 4 fixed32.
        let payload = [
            0x08, 0x96, 0x01, // 1: varint 150
            0x11, 1, 2, 3, 4, 5, 6, 7, 8, // 2: fixed64
            0x1A, 0x02, 0xAA, 0xBB, // 3: bytes
            0x25, 1, 2, 3, 4, // 4: fixed32
        ];
        let mut reader = Reader::new(&payload);
        let mut seen = Vec::new();
        while let Some((field, wire)) = reader.tag().unwrap() {
            seen.push(field);
            reader.skip(field, wire).unwrap();
        }
        assert_eq!(seen, vec![1, 2, 3, 4]);
        assert!(reader.is_empty());
    }

    #[test]
    fn group_wire_types_are_rejected() {
        let mut reader = Reader::new(&[0x0B]); // field 1, wire type 3
        let (field, wire) = reader.tag().unwrap().unwrap();
        assert_eq!(wire, 3);
        assert!(reader.skip(field, wire).is_err());
    }

    #[test]
    fn repeated_i64_accepts_packed_and_unpacked() {
        // Unpacked: two separate varint fields.
        let mut out = Vec::new();
        let mut reader = Reader::new(&[0x03, 0x05]);
        read_repeated_i64(&mut reader, 1, WIRE_VARINT, &mut out).unwrap();
        read_repeated_i64(&mut reader, 1, WIRE_VARINT, &mut out).unwrap();
        assert_eq!(out, vec![3, 5]);

        // Packed: one length-delimited payload holding both.
        let mut out = Vec::new();
        let mut reader = Reader::new(&[0x02, 0x03, 0x05]);
        read_repeated_i64(&mut reader, 1, WIRE_LEN, &mut out).unwrap();
        assert_eq!(out, vec![3, 5]);
    }

    #[test]
    fn repeated_f32_accepts_packed_and_unpacked() {
        let one = 1.0f32.to_le_bytes();
        let two = 2.0f32.to_le_bytes();

        let mut out = Vec::new();
        let mut reader = Reader::new(&one);
        read_repeated_f32(&mut reader, 4, WIRE_FIXED32, &mut out).unwrap();
        assert_eq!(out, vec![1.0]);

        let mut packed = vec![8u8];
        packed.extend_from_slice(&one);
        packed.extend_from_slice(&two);
        let mut out = Vec::new();
        let mut reader = Reader::new(&packed);
        read_repeated_f32(&mut reader, 4, WIRE_LEN, &mut out).unwrap();
        assert_eq!(out, vec![1.0, 2.0]);
    }

    #[test]
    fn packed_float_payload_of_ragged_length_errors() {
        let mut out = Vec::new();
        let mut reader = Reader::new(&[0x03, 1, 2, 3]);
        assert!(read_repeated_f32(&mut reader, 4, WIRE_LEN, &mut out).is_err());
    }

    #[test]
    fn bytes_range_reports_absolute_offsets_through_nesting() {
        // Outer: field 1, len 4 -> inner message at offset 2.
        // Inner: field 1, len 2 -> payload at offset 4.
        let payload = [0x0A, 0x04, 0x0A, 0x02, 0xAA, 0xBB];
        let mut outer = Reader::new(&payload);
        let (_, wire) = outer.tag().unwrap().unwrap();
        assert_eq!(wire, WIRE_LEN);
        let mut inner = outer.nested().unwrap();
        let (_, wire) = inner.tag().unwrap().unwrap();
        assert_eq!(wire, WIRE_LEN);
        let (bytes, range) = inner.bytes_range().unwrap();
        assert_eq!(bytes, &[0xAA, 0xBB]);
        assert_eq!(range, 4..6, "range must be absolute, not inner-relative");
    }
}
