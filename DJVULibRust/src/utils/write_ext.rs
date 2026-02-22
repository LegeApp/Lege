//! An extension trait for `std::io::Write` to add helpers for writing
//! custom integer types, such as 24-bit integers.

use std::io::{self, Write};

/// Extends `std::io::Write` with methods for writing 24-bit integers.
pub trait WriteBytesExtU24: Write {
    /// Writes a 24-bit unsigned integer to the underlying writer in big-endian format.
    fn write_u24<B: byteorder::ByteOrder>(&mut self, n: u32) -> io::Result<()>;
}

impl<W: Write> WriteBytesExtU24 for W {
    fn write_u24<B: byteorder::ByteOrder>(&mut self, n: u32) -> io::Result<()> {
        // Ensure the value fits within 24 bits.
        if n > 0xFFFFFF {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "value too large for u24",
            ));
        }
        let mut buf = [0; 3];
        B::write_u24(&mut buf, n);
        self.write_all(&buf)
    }
}
