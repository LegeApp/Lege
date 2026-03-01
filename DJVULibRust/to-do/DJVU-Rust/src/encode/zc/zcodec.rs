use super::table::{DEFAULT_ZP_TABLE, ZpTableEntry};
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

/// An adaptive quasi-arithmetic encoder implementing the Z-Coder algorithm.
pub struct ZEncoder<W: Write> {
    writer: Option<W>,
    a: u16,      // range register (16-bit)
    subend: u32, // subrange end (32-bit to match C++ unsigned int)
    buffer: u32, // 24-bit buffer for zemit
    nrun: u16,   // run length for zemit
    byte: u8,
    scount: u8,
    delay: u8,   // delay counter for initial bit suppression
    finished: bool,
    table: [ZpTableEntry; 256], // mutable table for patching
    debug_count: usize, // Debug counter for limiting debug output
}

impl<W: Write> ZEncoder<W> {
    /// Creates a new Z-Coder encoder that writes to the given writer.
    pub fn new(writer: W, _djvu_compat: bool) -> Result<Self, ZCodecError> {
        // Create a 256-entry table, starting with the default 251 entries
        let mut table = [ZpTableEntry { p: 0, m: 0, up: 0, dn: 0 }; 256];
        
        // Copy the default table entries
        for (i, &entry) in DEFAULT_ZP_TABLE.iter().enumerate() {
            table[i] = entry;
        }
        
        // Patch table when _djvu_compat is false
        if !_djvu_compat {
            for j in 0..251 {  // Only patch the valid entries
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
            a: 0, // C++ initializes to 0, not 0x8000
            subend: 0,
            buffer: 0x000000, // Try starting with empty buffer instead of 0xffffff
            nrun: 0,
            byte: 0,
            scount: 0,
            delay: 25, // Initial delay as per C++
            finished: false,
            table,
            debug_count: 0, // Initialize debug counter
        })
    }

    /// Encodes a single bit using the provided statistical context.
    #[inline(always)]
    pub fn encode(&mut self, bit: bool, ctx: &mut BitContext) -> Result<(), ZCodecError> {
        if self.finished {
            return Err(ZCodecError::Finished);
        }

        let ctx_before = *ctx;
        
        let z = self.a as u32 + self.table[*ctx as usize].p as u32;

        // Debug output for first few bits
        if self.debug_count < 10 {
            eprintln!("RUST DEBUG: bit={}, ctx_before={}, a=0x{:04x}, p={}, z=0x{:04x}, mps={}", 
                     bit, ctx_before, self.a, self.table[ctx_before as usize].p, z, (ctx_before & 1 != 0));
            self.debug_count += 1;
        }

        if bit != (*ctx & 1 != 0) {
            self.encode_lps(ctx, z as u16)?;
        } else if z >= 0x8000 {
            self.encode_mps(ctx, z as u16)?;
        } else {
            self.a = z as u16;
        }
        Ok(())
    }

    /// Internal MPS encoding logic.
    #[inline(always)]
    fn encode_mps(&mut self, ctx: &mut u8, z: u16) -> Result<(), ZCodecError> {
        let old_a = self.a;
        let mut z_adj = z;
        
        // Apply interval adjustment like C++ ZCODER 
        if z_adj >= 0x8000 {
            z_adj = 0x4000 + (z_adj >> 1);
        }
        
        // Adaptation
        if self.a >= self.table[*ctx as usize].m {
            *ctx = self.table[*ctx as usize].up;
        }
        
        // Code MPS
        self.a = z_adj;
        
        if self.debug_count < 10 {
            eprintln!("RUST MPS: old_a=0x{:04x}, new_a=0x{:04x}, m={}, ctx_updated={}", 
                     old_a, self.a, self.table[*ctx as usize].m, 
                     old_a >= self.table[*ctx as usize].m);
        }
        
        // Export bits
        if self.a >= 0x8000 {
            let subend_16 = (self.subend & 0xffff) as u16;
            self.zemit(1 - (subend_16 >> 15) as u32)?;
            self.subend = (self.subend << 1) & 0xffff;
            self.a = (self.a << 1) & 0xffff;
        }
        Ok(())
    }

    /// Internal LPS encoding logic.
    #[inline(always)]
    fn encode_lps(&mut self, ctx: &mut u8, z: u16) -> Result<(), ZCodecError> {
        let old_a = self.a;
        let old_subend = self.subend;
        let mut z_adj = z;
        
        // Apply interval adjustment like C++ ZCODER 
        if z_adj >= 0x8000 {
            z_adj = 0x4000 + (z_adj >> 1);
        }
        
        // Adaptation
        *ctx = self.table[*ctx as usize].dn;
        
        // Code LPS
        let z_lps = 0x10000u32 - z_adj as u32;
        self.subend = self.subend + z_lps;
        self.a = (self.a as u32 + z_lps) as u16;
        
        if self.debug_count < 10 {
            eprintln!("RUST LPS: old_a=0x{:04x}, new_a=0x{:04x}, old_subend=0x{:08x}, new_subend=0x{:08x}, z_adj=0x{:04x}, z_lps=0x{:04x}", 
                     old_a, self.a, old_subend, self.subend, z_adj, z_lps);
        }
        
        // Export bits
        while self.a >= 0x8000 {
            let subend_16 = (self.subend & 0xffff) as u16;
            self.zemit(1 - (subend_16 >> 15) as u32)?;
            self.subend = (self.subend << 1) & 0xffff;
            self.a = (self.a << 1) & 0xffff;
        }
        Ok(())
    }

    /// Emits a bit to the output buffer.
    #[inline(always)]
    fn zemit(&mut self, b: u32) -> Result<(), ZCodecError> {
        if self.debug_count < 30 {
            eprintln!("RUST ZEMIT: b={}, buffer_before=0x{:06x}, nrun={}", b, self.buffer, self.nrun);
        }
        
        self.buffer = (self.buffer << 1) | b;
        let out_byte = (self.buffer >> 24) as u8;
        self.buffer &= 0xffffff;

        if self.debug_count < 30 {
            eprintln!("RUST ZEMIT: out_byte=0x{:02x}, buffer_after=0x{:06x}", out_byte, self.buffer);
        }

        match out_byte {
            1 => {
                if self.debug_count < 30 {
                    eprintln!("RUST ZEMIT: emit 1, then {} zeros", self.nrun);
                }
                self.outbit(1)?;
                while self.nrun > 0 {
                    self.outbit(0)?;
                    self.nrun -= 1;
                }
            }
            0xff => {
                if self.debug_count < 30 {
                    eprintln!("RUST ZEMIT: emit 0, then {} ones", self.nrun);
                }
                self.outbit(0)?;
                while self.nrun > 0 {
                    self.outbit(1)?;
                    self.nrun -= 1;
                }
            }
            0 => {
                if self.debug_count < 30 {
                    eprintln!("RUST ZEMIT: increment nrun to {}", self.nrun + 1);
                }
                self.nrun += 1;
            }
            _ => unreachable!("zemit logic guarantees out_byte is 0, 1, or 0xff"),
        }
        Ok(())
    }

    /// Outputs a single bit to the writer with delay logic.
    #[inline(always)]
    fn outbit(&mut self, bit: u8) -> Result<(), ZCodecError> {
        if self.delay > 0 {
            if self.delay < 0xff {
                self.delay -= 1;
            }
            return Ok(());
        }

        if let Some(ref mut writer) = self.writer {
            self.byte = (self.byte << 1) | bit;
            self.scount += 1;

            if self.scount == 8 {
                writer.write_all(&[self.byte])?;
                self.scount = 0;
                self.byte = 0;
            }
        }
        Ok(())
    }

    /// Flushes internal buffers and terminates the stream.
    fn eflush(&mut self) -> Result<(), ZCodecError> {
        // Adjust subend per C++ rules - exactly match C++ logic
        if self.subend > 0x8000 {
            self.subend = 0x10000;
        } else if self.subend > 0 {
            self.subend = 0x8000;
        }

        // Emit final bits - ensure 16-bit wrap-around like C++
        while self.buffer != 0xffffff || self.subend != 0 {
            let subend_16 = (self.subend & 0xffff) as u16;
            self.zemit(1 - (subend_16 >> 15) as u32)?;
            self.subend = (self.subend << 1) & 0xffff;
        }

        // Emit pending run
        self.outbit(1)?;
        while self.nrun > 0 {
            self.outbit(0)?;
            self.nrun -= 1;
        }

        // Pad with 1s
        while self.scount > 0 {
            self.outbit(1)?;
        }

        // Prevent further emission
        self.delay = 0xff;
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

    // Extended encoding methods

    /// Encodes MPS without adaptation.
    #[inline(always)]
    pub fn encode_mps_simple(&mut self, z: u16) -> Result<(), ZCodecError> {
        self.a = z;
        if self.a >= 0x8000 {
            self.zemit(1 - (self.subend >> 15) as u32)?;
            self.subend = (self.subend << 1) & 0xffff;
            self.a = (self.a << 1) & 0xffff;
        }
        Ok(())
    }

    /// Encodes LPS without adaptation.
    #[inline(always)]
    pub fn encode_lps_simple(&mut self, z: u16) -> Result<(), ZCodecError> {
        let z_adjusted = 0x10000 - z as u32;
        self.subend = self.subend + z_adjusted;
        self.a = (self.a as u32 + z_adjusted) as u16;
        while self.a >= 0x8000 {
            self.zemit(1 - (self.subend >> 15) as u32)?;
            self.subend = (self.subend << 1) & 0xffff;
            self.a = (self.a << 1) & 0xffff;
        }
        Ok(())
    }

    /// Encodes MPS without learning, with interval reversion.
    #[inline(always)]
    pub fn encode_mps_nolearn(&mut self, mut z: u16) -> Result<(), ZCodecError> {
        if z >= 0x8000 {
            z = 0x4000 + (z >> 1);
        }
        self.a = z;
        if self.a >= 0x8000 {
            self.zemit(1 - (self.subend >> 15) as u32)?;
            self.subend = (self.subend << 1) & 0xffff;
            self.a = (self.a << 1) & 0xffff;
        }
        Ok(())
    }

    /// Encodes LPS without learning, with interval reversion.
    #[inline(always)]
    pub fn encode_lps_nolearn(&mut self, mut z: u16) -> Result<(), ZCodecError> {
        if z >= 0x8000 {
            z = 0x4000 + (z >> 1);
        }
        let z_adjusted = 0x10000 - z as u32;
        self.subend = self.subend + z_adjusted;
        self.a = (self.a as u32 + z_adjusted) as u16;
        while self.a >= 0x8000 {
            self.zemit(1 - (self.subend >> 15) as u32)?;
            self.subend = (self.subend << 1) & 0xffff;
            self.a = (self.a << 1) & 0xffff;
        }
        Ok(())
    }

    /// IWencoder for IW44 compatibility.
    #[inline(always)]
    pub fn IWencoder(&mut self, bit: bool) -> Result<(), ZCodecError> {
        let z = 0x8000 + ((self.a as u32 * 3) >> 3) as u16;
        if bit {
            self.encode_lps_simple(z)?;
        } else {
            self.encode_mps_simple(z)?;
        }
        Ok(())
    }

    /// Encodes a bit with context-based routing (adaptive vs fixed-probability).
    /// Raw contexts (128, 129) use IWencoder, others use normal adaptive encoding.
    #[inline(always)]
    pub fn encode_with_context_routing(&mut self, bit: bool, ctx: &mut BitContext) -> Result<(), ZCodecError> {
        match *ctx {
            RAW_CONTEXT_128 | RAW_CONTEXT_129 => {
                // Fixed-probability path – no context update
                self.IWencoder(bit)
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
            if let Err(e) = self.eflush() {
                panic!("ZEncoder failed to flush on drop: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_encode_simple_sequence() {
        let mut encoder = ZEncoder::new(Cursor::new(Vec::new()), true).unwrap();
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
        let mut encoder = ZEncoder::new(Cursor::new(Vec::new()), true).unwrap();
        let mut ctx = 0;

        for _ in 0..1000 {
            encoder.encode(false, &mut ctx).unwrap();
        }
        encoder.encode(true, &mut ctx).unwrap();

        let data = encoder.finish().unwrap().into_inner();
        assert!(data.len() < 20);
    }
}