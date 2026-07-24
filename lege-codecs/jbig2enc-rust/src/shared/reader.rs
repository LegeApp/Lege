//! A checked, borrowing slice reader (jbig2decplan.md §8, §23).
//!
//! The decoder consumes PDF stream bytes already held in memory, so a
//! borrowed slice reader is simpler and faster than a generic `Read`. Every
//! offset and length computation is checked (`checked_add`), and every read is
//! bounds-checked, so a truncated or malicious stream yields a typed
//! [`ParseError`] rather than a panic.

// `ParseError` lives in `decode::error` per the plan's file map. In Phase 0
// both halves are always compiled, so this shared module can borrow it; when
// encode/decode feature gating lands (Phase 1), ParseError moves to `shared`.
use crate::shared::error::ParseError;

/// A cursor over a byte slice with checked, big-endian reads.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    input: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    /// Create a reader positioned at the start of `input`.
    #[inline]
    pub fn new(input: &'a [u8]) -> Self {
        Self { input, offset: 0 }
    }

    /// The current byte offset from the start of the input.
    #[inline]
    pub fn position(&self) -> usize {
        self.offset
    }

    /// The number of unread bytes remaining.
    #[inline]
    pub fn remaining(&self) -> usize {
        // offset never exceeds input.len() (every advance is checked), so this
        // subtraction cannot underflow, but saturate defensively anyway.
        self.input.len().saturating_sub(self.offset)
    }

    /// Whether all input has been consumed.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Borrow the next `len` bytes and advance past them.
    pub fn take(&mut self, len: usize) -> Result<&'a [u8], ParseError> {
        let end = self.offset.checked_add(len).ok_or(ParseError::Overflow {
            operation: "reader take offset",
        })?;
        if end > self.input.len() {
            return Err(ParseError::UnexpectedEof {
                offset: self.offset,
                needed: len.saturating_sub(self.remaining()),
            });
        }
        let slice = &self.input[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    /// Peek at the next `len` bytes without advancing.
    pub fn peek(&self, len: usize) -> Result<&'a [u8], ParseError> {
        let end = self.offset.checked_add(len).ok_or(ParseError::Overflow {
            operation: "reader peek offset",
        })?;
        if end > self.input.len() {
            return Err(ParseError::UnexpectedEof {
                offset: self.offset,
                needed: len.saturating_sub(self.remaining()),
            });
        }
        Ok(&self.input[self.offset..end])
    }

    /// Take `len` bytes as an independent sub-reader (for a bounded segment
    /// payload) and advance past them in the parent.
    pub fn subreader(&mut self, len: usize) -> Result<Reader<'a>, ParseError> {
        Ok(Reader::new(self.take(len)?))
    }

    /// Read one unsigned byte.
    #[inline]
    pub fn read_u8(&mut self) -> Result<u8, ParseError> {
        Ok(self.take(1)?[0])
    }

    /// Read one signed byte.
    #[inline]
    pub fn read_i8(&mut self) -> Result<i8, ParseError> {
        Ok(self.read_u8()? as i8)
    }

    /// Read a big-endian `u16`.
    #[inline]
    pub fn read_u16_be(&mut self) -> Result<u16, ParseError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    /// Read a big-endian `u32`.
    #[inline]
    pub fn read_u32_be(&mut self) -> Result<u32, ParseError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a big-endian `i32`.
    #[inline]
    pub fn read_i32_be(&mut self) -> Result<i32, ParseError> {
        Ok(self.read_u32_be()? as i32)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn sequential_reads_advance_position() {
        let data = [0x12u8, 0x34, 0x56, 0x78, 0x9a];
        let mut r = Reader::new(&data);
        assert_eq!(r.position(), 0);
        assert_eq!(r.remaining(), 5);
        assert_eq!(r.read_u8().unwrap(), 0x12);
        assert_eq!(r.position(), 1);
        assert_eq!(r.read_u16_be().unwrap(), 0x3456);
        assert_eq!(r.position(), 3);
        assert_eq!(r.read_u8().unwrap(), 0x78);
        assert_eq!(r.remaining(), 1);
        assert!(!r.is_empty());
        assert_eq!(r.read_u8().unwrap(), 0x9a);
        assert!(r.is_empty());
    }

    #[test]
    fn big_endian_widths() {
        let data = [0x80u8, 0x00, 0x00, 0x01];
        let mut r = Reader::new(&data);
        assert_eq!(r.read_u32_be().unwrap(), 0x8000_0001);

        let mut r = Reader::new(&data);
        assert_eq!(r.read_i32_be().unwrap(), -0x7FFF_FFFF);

        let mut r = Reader::new(&[0xFFu8]);
        assert_eq!(r.read_i8().unwrap(), -1);
    }

    #[test]
    fn truncation_is_a_typed_error() {
        let data = [0x00u8, 0x01];
        let mut r = Reader::new(&data);
        assert_eq!(r.read_u8().unwrap(), 0x00);
        // Only one byte left; a u32 read must fail without panicking.
        match r.read_u32_be() {
            Err(ParseError::UnexpectedEof { offset, needed }) => {
                assert_eq!(offset, 1);
                assert_eq!(needed, 3);
            }
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
        // Position is unchanged after a failed read.
        assert_eq!(r.position(), 1);
    }

    #[test]
    fn take_zero_is_ok_at_end() {
        let data = [0x01u8];
        let mut r = Reader::new(&data);
        assert_eq!(r.read_u8().unwrap(), 1);
        assert_eq!(r.take(0).unwrap(), &[] as &[u8]);
    }

    #[test]
    fn subreader_is_bounded_and_independent() {
        let data = [1u8, 2, 3, 4, 5];
        let mut r = Reader::new(&data);
        let mut sub = r.subreader(3).unwrap();
        assert_eq!(r.position(), 3);
        assert_eq!(sub.read_u8().unwrap(), 1);
        assert_eq!(sub.read_u16_be().unwrap(), 0x0203);
        // The sub-reader cannot see past its window.
        assert!(sub.read_u8().is_err());
        // The parent continues after the window.
        assert_eq!(r.read_u16_be().unwrap(), 0x0405);
    }

    #[test]
    fn oversized_take_reports_eof_not_overflow() {
        let data = [0u8; 4];
        let mut r = Reader::new(&data);
        assert!(matches!(
            r.take(usize::MAX),
            Err(ParseError::Overflow { .. }) | Err(ParseError::UnexpectedEof { .. })
        ));
        // A merely-too-large (non-overflowing) length is a clean EOF.
        let mut r = Reader::new(&data);
        assert!(matches!(r.take(5), Err(ParseError::UnexpectedEof { .. })));
    }

    #[test]
    fn peek_does_not_advance() {
        let data = [9u8, 8, 7];
        let mut r = Reader::new(&data);
        assert_eq!(r.peek(2).unwrap(), &[9, 8]);
        assert_eq!(r.position(), 0);
        assert_eq!(r.read_u8().unwrap(), 9);
    }
}
