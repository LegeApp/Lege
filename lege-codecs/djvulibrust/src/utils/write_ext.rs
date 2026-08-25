//! An extension trait for `std::io::Write` to add helpers for writing
//! custom integer types, such as 24-bit integers.

use std::io::{self, Write};

/// Extends `std::io::Write` with methods for writing 24-bit integers.
///
/// This used to be one method generic over `byteorder::ByteOrder`. DjVu writes
/// 24-bit fields big-endian everywhere, so the generic parameter only ever had
/// one useful instantiation; both orders are now spelled out concretely and the
/// `byteorder` dependency is gone.
pub trait WriteBytesExtU24: Write {
    /// Writes a 24-bit unsigned integer in big-endian order.
    fn write_u24_be(&mut self, n: u32) -> io::Result<()> {
        let [_, b1, b2, b3] = check_u24(n)?.to_be_bytes();
        self.write_all(&[b1, b2, b3])
    }

    /// Writes a 24-bit unsigned integer in little-endian order.
    fn write_u24_le(&mut self, n: u32) -> io::Result<()> {
        let [b0, b1, b2, _] = check_u24(n)?.to_le_bytes();
        self.write_all(&[b0, b1, b2])
    }
}

impl<W: Write> WriteBytesExtU24 for W {}

/// Ensure the value fits within 24 bits.
fn check_u24(n: u32) -> io::Result<u32> {
    if n > 0xFF_FFFF {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "value too large for u24",
        ));
    }
    Ok(n)
}
