use super::table::{ZpTableEntry, DEFAULT_ZP_TABLE};
use super::ZpEncoderCursor;
use std::io::Cursor;
use std::io::Write;
use thiserror::Error;

/// A single byte representing the statistical context for encoding a bit.
pub type BitContext = u8;

/// Raw (non-adaptive) contexts according to the DjVu IW44 spec.
pub const RAW_CONTEXT_128: BitContext = 128;
pub const RAW_CONTEXT_129: BitContext = 129;

/// Errors that can occur during Z-Coder encoding.
#[derive(Error, Debug)]
pub enum ZCodecError {
    #[error("I/O error during write operation")]
    Io(#[from] std::io::Error),
    #[error("Attempted to encode after the stream was finished")]
    Finished,
}

impl From<ZCodecError> for std::io::Error {
    fn from(err: ZCodecError) -> Self {
        match err {
            ZCodecError::Io(e) => e,
            ZCodecError::Finished => {
                std::io::Error::new(std::io::ErrorKind::Other, err.to_string())
            }
        }
    }
}

/// An adaptive quasi-arithmetic encoder implementing the ZP-Coder algorithm.
pub struct ZEncoder<W: Write> {
    writer: Option<W>,
    // Core ZP-Coder registers (matching djvulibre exactly)
    a: u32,      // range register (unsigned!)
    subend: u32, // subinterval end
    buffer: u32, // 3-byte bit buffer (24-bit)
    nrun: u32,   // run of pending bits
    byte: u8,    // current output byte
    scount: i32, // bit counter in current byte
    delay: i32,  // delay counter
    finished: bool,
    table: [ZpTableEntry; 256], // mutable table for patching
}

impl<W: Write> ZEncoder<W> {
    /// Creates a new ZP-Coder encoder that writes to the given writer.
    pub fn new(writer: W, djvu_compat: bool) -> Result<Self, ZCodecError> {
        // Create a 256-entry table, starting with the default 251 entries
        let mut table = [ZpTableEntry {
            p: 0,
            m: 0,
            up: 0,
            dn: 0,
        }; 256];

        // Copy the default table entries
        for (i, &entry) in DEFAULT_ZP_TABLE.iter().enumerate() {
            table[i] = entry;
        }

        // Patch table when djvu_compat is false
        if !djvu_compat {
            for j in 0..256 {
                let mut a = 0x10000 - table[j].p as u32;
                while a >= 0x8000 {
                    a = (a << 1) & 0xffff;
                }
                if table[j].m > 0 && a + table[j].p as u32 >= 0x8000 && a >= table[j].m as u32 {
                    let x = DEFAULT_ZP_TABLE[j].dn;
                    let y = DEFAULT_ZP_TABLE[x as usize].dn;
                    table[j].dn = y;
                }
            }
        }

        Ok(ZEncoder {
            writer: Some(writer),
            a: 0,             // Initialize to 0 as per DjVuLibre
            subend: 0,        // Subinterval end starts at 0
            buffer: 0xffffff, // 3-byte buffer initialized to all 1s
            nrun: 0,          // Run counter starts at 0
            byte: 0,          // Current byte starts at 0
            scount: 0,        // Bit count starts at 0
            delay: 25,        // Delay starts at 25
            finished: false,
            table,
        })
    }

    /// Encodes a single bit using the provided statistical context.
    #[inline(always)]
    pub fn encode(&mut self, bit: bool, ctx: &mut BitContext) -> Result<(), ZCodecError> {
        if self.finished {
            return Err(ZCodecError::Finished);
        }

        // CRITICAL: z = a + p[ctx], not just p[ctx]!
        let z = self.a + self.table[*ctx as usize].p as u32;
        if bit != (*ctx & 1 != 0) {
            // LPS path
            self.encode_lps(ctx, z)?;
        } else if z >= 0x8000 {
            // MPS path (only if z >= 0x8000)
            self.encode_mps(ctx, z)?;
        } else {
            // Fast path: just update a
            self.a = z;
        }

        Ok(())
    }

    /// Encodes a bit without compression (pass-thru encoder).
    ///
    /// Matches DjVuLibre `ZPCodec::encoder(int bit)`:
    /// Raw bit encoding for IWencoder. Matches C++ ZPCodec::IWencoder.
    /// ```cpp
    /// const int z = 0x8000 + ((a+a+a) >> 3);
    /// if (bit) encode_lps_simple(z);
    /// else     encode_mps_simple(z);
    /// ```
    #[inline(always)]
    pub fn encode_raw(&mut self, bit: bool) -> Result<(), ZCodecError> {
        if self.finished {
            return Err(ZCodecError::Finished);
        }

        // CRITICAL: Match C++ formula exactly: z = 0x8000 + ((a+a+a) >> 3)
        // This gives z = 0x8000 + 3*a/8, NOT 0x8000 + a/2
        let z = 0x8000u32 + ((self.a + self.a + self.a) >> 3);
        if bit {
            self.encode_lps_simple(z)
        } else {
            self.encode_mps_simple(z)
        }
    }

    #[inline(always)]
    fn encode_mps(&mut self, ctx: &mut BitContext, mut z: u32) -> Result<(), ZCodecError> {
        let d = 0x6000 + ((z + self.a) >> 2);
        if z > d {
            z = d;
        }
        if self.a >= self.table[*ctx as usize].m as u32 {
            *ctx = self.table[*ctx as usize].up;
        }
        self.a = z;
        if self.a >= 0x8000 {
            self.zemit(1 - ((self.subend >> 15) as i32))?;
            self.subend = (self.subend << 1) as u16 as u32;
            self.a = (self.a << 1) as u16 as u32;
        }
        Ok(())
    }

    #[inline(always)]
    fn encode_lps(&mut self, ctx: &mut BitContext, mut z: u32) -> Result<(), ZCodecError> {
        let d = 0x6000 + ((z + self.a) >> 2);
        if z > d {
            z = d;
        }
        *ctx = self.table[*ctx as usize].dn;
        z = 0x10000 - z;
        self.subend = self.subend.wrapping_add(z);
        self.a = self.a.wrapping_add(z);
        while self.a >= 0x8000 {
            self.zemit(1 - ((self.subend >> 15) as i32))?;
            self.subend = (self.subend << 1) as u16 as u32;
            self.a = (self.a << 1) as u16 as u32;
        }
        Ok(())
    }

    #[inline(always)]
    fn encode_mps_simple(&mut self, z: u32) -> Result<(), ZCodecError> {
        self.a = z;
        if self.a >= 0x8000 {
            self.zemit(1 - ((self.subend >> 15) as i32))?;
            self.subend = (self.subend << 1) as u16 as u32;
            self.a = (self.a << 1) as u16 as u32;
        }
        Ok(())
    }

    #[inline(always)]
    fn encode_lps_simple(&mut self, mut z: u32) -> Result<(), ZCodecError> {
        z = 0x10000 - z;
        self.subend = self.subend.wrapping_add(z);
        self.a = self.a.wrapping_add(z);
        while self.a >= 0x8000 {
            self.zemit(1 - ((self.subend >> 15) as i32))?;
            self.subend = (self.subend << 1) as u16 as u32;
            self.a = (self.a << 1) as u16 as u32;
        }
        Ok(())
    }

    #[inline(always)]
    fn zemit(&mut self, bit: i32) -> Result<(), ZCodecError> {
        self.buffer = (self.buffer << 1).wrapping_add(bit as u32);
        let b = (self.buffer >> 24) as u8;
        self.buffer &= 0x00ff_ffff;

        match b {
            1 => {
                self.outbit(1)?;
                while self.nrun > 0 {
                    self.outbit(0)?;
                    self.nrun -= 1;
                }
                self.nrun = 0;
            }
            0xff => {
                self.outbit(0)?;
                while self.nrun > 0 {
                    self.outbit(1)?;
                    self.nrun -= 1;
                }
                self.nrun = 0;
            }
            0 => {
                self.nrun += 1;
            }
            _ => {
                return Err(ZCodecError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid zemit bit",
                )))
            }
        }
        Ok(())
    }

    #[inline(always)]
    fn outbit(&mut self, bit: u8) -> Result<(), ZCodecError> {
        if self.delay > 0 {
            if self.delay < 0xff {
                self.delay -= 1;
            }
        } else {
            self.byte = (self.byte << 1) | (bit & 1);
            self.scount += 1;
            if self.scount == 8 {
                if let Some(ref mut writer) = self.writer {
                    writer.write_all(&[self.byte])?;
                }
                self.scount = 0;
                self.byte = 0;
            }
        }
        Ok(())
    }

    fn eflush(&mut self) -> Result<(), ZCodecError> {
        if self.subend > 0x8000 {
            self.subend = 0x10000;
        } else if self.subend > 0 {
            self.subend = 0x8000;
        }
        while self.buffer != 0xffffff || self.subend != 0 {
            self.zemit(1 - ((self.subend >> 15) as i32))?;
            self.subend = (self.subend << 1) as u16 as u32;
        }
        self.outbit(1)?;
        while self.nrun > 0 {
            self.outbit(0)?;
            self.nrun -= 1;
        }
        self.nrun = 0;
        while self.scount > 0 {
            self.outbit(1)?;
        }
        self.delay = 0xff;
        Ok(())
    }

    /// MPS encoding logic matching DjVuLibre exactly.
    #[cfg(any())]
    #[inline(always)]
    fn zencoder_mps(&mut self, p: i32) -> Result<(), ZCodecError> {
        self.a -= p;
        if self.a <= 0 {
            if self.a < -p {
                // MPS_EXCHANGE
                self.a = p;
                self.zencoder_lps(p)?;
            } else {
                // CONDITIONAL_EXCHANGE
                self.a = p;
                self.zencoder_renorm()?;
            }
        } else {
            self.zencoder_renorm()?;
        }
        Ok(())
    }

    /// LPS encoding logic matching DjVuLibre exactly.
    #[cfg(any())]
    #[inline(always)]
    fn zencoder_lps(&mut self, z: i32) -> Result<(), ZCodecError> {
        self.a -= z;
        if self.a < 0 {
            self.a = z;
            self.zencoder_renorm()?;
        } else {
            self.c = self.c.wrapping_add(self.a as u32);
            self.a = z;
            self.zencoder_renorm()?;
        }
        Ok(())
    }

    /// Renormalization logic matching DjVuLibre exactly.
    #[cfg(any())]
    #[inline(always)]
    fn zencoder_renorm(&mut self) -> Result<(), ZCodecError> {
        while self.a < 0x8000 {
            self.a <<= 1;
            self.c <<= 1;
            self.c &= 0xffffffff;
            self.ct -= 1;
            if self.ct < 0 {
                self.encoder_shift()?;
            }
        }
        Ok(())
    }

    /// Encoder shift logic matching DjVuLibre exactly.
    #[cfg(any())]
    #[inline(always)]
    fn encoder_shift(&mut self) -> Result<(), ZCodecError> {
        let b = ((self.c >> 24) & 0xff) as i32;
        if b != 0xff {
            self.encoder_out(b)?;
        } else if self.fflag {
            self.encoder_out(b)?;
        } else if self.scount > 0 {
            self.buffer += 1;
            if self.buffer == 0xff {
                self.encoder_out(0xff)?;
                self.buffer = 0;
            }
            let mut remaining = self.scount;
            while remaining > 0 {
                self.encoder_out(self.buffer)?;
                remaining -= 1;
            }
            self.scount = 0;
            self.encoder_out(b)?
        } else {
            self.fflag = true;
            self.scount = 0;
            self.buffer = b;
        }
        self.ct = 8;
        Ok(())
    }

    /// Encoder output logic matching DjVuLibre exactly.
    #[cfg(any())]
    #[inline(always)]
    fn encoder_out(&mut self, b: i32) -> Result<(), ZCodecError> {
        if let Some(ref mut writer) = self.writer {
            writer.write_all(&[b as u8])?;
        }
        Ok(())
    }

    /// Encoder flush logic matching DjVuLibre exactly.
    #[cfg(any())]
    fn encoder_flush(&mut self) -> Result<(), ZCodecError> {
        self.zencoder_renorm()?;
        if self.ct > 0 {
            self.buffer += 1;
            if self.buffer == 0xff {
                self.encoder_out(0xff)?;
                self.buffer = 0;
            }
            let mut remaining = self.scount;
            while remaining > 0 {
                self.encoder_out(self.buffer)?;
                remaining -= 1;
            }
            self.scount = 0;
            self.c = (self.c & 0xffffff) | ((self.buffer as u32) << (self.ct as u32 + 24 - 8));
            for _ in 0..4 {
                self.encoder_out(((self.c >> 24) & 0xff) as i32)?;
                self.c = (self.c << 8) & 0xffffffff;
            }
        }
        self.a = 0;
        Ok(())
    }

    /// Finalizes encoding and returns the writer.
    pub fn finish(mut self) -> Result<W, ZCodecError> {
        if !self.finished {
            self.eflush()?;
            self.finished = true;
        }
        self.writer.take().ok_or(ZCodecError::Finished)
    }


    /// Iwencoder for IW44 compatibility - uses fixed-probability (non-adaptive) coding.
    #[inline(always)]
    pub fn iwencoder(&mut self, bit: bool) -> Result<(), ZCodecError> {
        self.encode_raw(bit)
    }

    /// Encodes a bit with context-based routing (adaptive vs fixed-probability).
    /// Raw contexts (128, 129) use IWencoder, others use normal adaptive encoding.
    #[inline(always)]
    pub fn encode_with_context_routing(
        &mut self,
        bit: bool,
        ctx: &mut BitContext,
    ) -> Result<(), ZCodecError> {
        match *ctx {
            RAW_CONTEXT_128 | RAW_CONTEXT_129 => {
                // Fixed-probability path – no context update
                self.iwencoder(bit)
            }
            _ => {
                // Normal adaptive arithmetic coding
                self.encode(bit, ctx)
            }
        }
    }
}

impl<W: Write> Drop for ZEncoder<W> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.eflush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_encode_simple_sequence() {
        let mut encoder = ZEncoder::new(Cursor::new(Vec::new()), false).unwrap();
        let mut ctx = 0;

        for i in 0..100 {
            encoder.encode(i % 2 == 0, &mut ctx).unwrap();
        }

        let writer = encoder.finish().unwrap();
        let data = writer.into_inner();
        assert!(!data.is_empty());
        assert!(data.len() > 0 && data.len() < 50);
        // Update expected output after verifying against C++ output
    }

    #[test]
    fn test_encode_highly_probable_sequence() {
        let mut encoder = ZEncoder::new(Cursor::new(Vec::new()), false).unwrap();
        let mut ctx = 0;

        for _ in 0..1000 {
            encoder.encode(false, &mut ctx).unwrap();
        }
        encoder.encode(true, &mut ctx).unwrap();

        let data = encoder.finish().unwrap().into_inner();
        assert!(data.len() < 20);
    }
}

// Implement ZpEncoderCursor trait for ZEncoder<Cursor<Vec<u8>>>
impl ZpEncoderCursor for ZEncoder<Cursor<Vec<u8>>> {
    fn encode(&mut self, bit: bool, ctx: &mut BitContext) -> Result<(), ZCodecError> {
        self.encode(bit, ctx)
    }

    fn iwencoder(&mut self, bit: bool) -> Result<(), ZCodecError> {
        self.iwencoder(bit)
    }

    fn encode_raw_bit(&mut self, bit: bool) -> Result<(), ZCodecError> {
        self.encode_raw(bit)
    }

    fn tell_bytes(&self) -> usize {
        if let Some(ref writer) = self.writer {
            writer.get_ref().len()
        } else {
            0
        }
    }

    fn finish(self) -> Result<Cursor<Vec<u8>>, ZCodecError> {
        self.finish()
    }
}
