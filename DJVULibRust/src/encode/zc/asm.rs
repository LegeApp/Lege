use super::table::DEFAULT_ZP_TABLE;
use super::zcodec::{BitContext, ZCodecError};
use super::ZpEncoderCursor;
use std::ffi::c_void;
use std::io::{Cursor, Write};
use std::marker::PhantomData;
use std::mem;
use std::slice;

// IMPORTANT: Use natural C layout and alignment. The assembly expects 64-bit pointer alignment.
// With repr(C) on x86_64, the field offsets are:
//   byte:    0
//   scount:  1
//   delay:   2
//   encoding:3
//   a:       4 (u32)
//   subend:  8 (u32)
//   buffer: 12 (u32)
//   nrun:   16 (u32)
//   bs:     24 (pointer, 8-byte aligned)
//   p:      32
//   m:    1056 (32 + 256*4)
//   up:   2080 (1056 + 256*4)
//   dn:   2336 (2080 + 256)
#[repr(C)]
pub struct ZpAsmState {
    // Leading fields
    pub byte: u8,        // 0
    pub scount: u8,      // 1
    pub delay: u8,       // 2
    pub encoding: u8,    // 3
    pub a: u32,          // 4
    pub subend: u32,     // 8
    pub buffer: u32,     // 12
    pub nrun: u32,       // 16
    pub bs: *mut c_void, // 24 on x86_64
    // Probability/adaptation tables
    pub p: [u32; 256], // 32
    pub m: [u32; 256], // 1056
    pub up: [u8; 256], // 2080
    pub dn: [u8; 256], // 2336
}

// Debug hook called by ASM outbit path.
// Signature from ASM: void zp_debug_hook(int event, uint32 a, uint32 subend, uint32 buffer, uint32 nrun, int bit)
#[no_mangle]
pub extern "C" fn zp_debug_hook(event: i32, a: u32, subend: u32, buffer: u32, nrun: u32, bit: i32) {
    unsafe {
        if DBG_COUNT < DBG_MAX {
            eprintln!(
                "[ZPDBG] ev={} a={:04x} sub={:04x} buf={:06x} nrun={} bit={}",
                event,
                a & 0xffff,
                subend & 0xffff,
                buffer & 0x00ff_ffff,
                nrun,
                bit & 1
            );
            DBG_COUNT += 1;
            if DBG_COUNT == DBG_MAX {
                eprintln!("[ZPDBG] ... (truncated) ...");
            }
        }
    }
}

extern "C" {
    fn zpcodec_einit(state: *mut ZpAsmState);
    fn zpcodec_zemit(state: *mut ZpAsmState, b: i32);
    fn zpcodec_outbit(state: *mut ZpAsmState, bit: i32);
    fn zpcodec_encode_mps(state: *mut ZpAsmState, ctx: *mut u8, z: u32);
    fn zpcodec_encode_lps(state: *mut ZpAsmState, ctx: *mut u8, z: u32);
    fn zpcodec_encode_mps_simple(state: *mut ZpAsmState, z: u32);
    fn zpcodec_encode_lps_simple(state: *mut ZpAsmState, z: u32);
    fn zpcodec_eflush(state: *mut ZpAsmState);
}

// Track bytes written for debugging
static mut BYTES_WRITTEN: usize = 0;
static mut LAST_BYTES: [u8; 8] = [0; 8];
static mut DBG_COUNT: u32 = 0;
const DBG_MAX: u32 = 300;

#[no_mangle]
pub extern "C" fn bytestream_write(bs: *mut c_void, data: *const c_void, len: usize) -> usize {
    // We treat bs as &mut Cursor<Vec<u8>> exclusively
    // The assembly emits one byte at a time, but we support arbitrary len
    if bs.is_null() || data.is_null() || len == 0 {
        return 0;
    }
    unsafe {
        let writer: &mut Cursor<Vec<u8>> = &mut *(bs as *mut Cursor<Vec<u8>>);
        let buf = slice::from_raw_parts(data as *const u8, len);

        // Debug tracking
        BYTES_WRITTEN += len;
        if len > 0 {
            // Keep track of last few bytes
            for i in 0..len.min(8) {
                LAST_BYTES[(BYTES_WRITTEN - len + i) % 8] = buf[i];
            }
        }

        // Use write_all to ensure all bytes are written
        match writer.write_all(buf) {
            Ok(()) => len, // Return requested length on success
            Err(_) => 0,   // Return 0 on failure
        }
    }
}

pub struct ZEncoder<W: Write> {
    state: ZpAsmState,
    writer: Option<Box<Cursor<Vec<u8>>>>,
    _marker: PhantomData<W>,
}

impl ZEncoder<Cursor<Vec<u8>>> {
    pub fn new(writer: Cursor<Vec<u8>>, _djvu_compat: bool) -> Result<Self, ZCodecError> {
        // Reset debug counters for each new encoder
        unsafe {
            BYTES_WRITTEN = 0;
            LAST_BYTES = [0; 8];
        }

        // Allocate boxed writer to obtain a stable heap pointer for FFI
        let mut writer_box = Box::new(writer);

        // Initialize state (will be filled by zpcodec_einit)
        let mut state: ZpAsmState = unsafe { mem::zeroed() };
        unsafe { zpcodec_einit(&mut state as *mut ZpAsmState) };

        // Fill adaptation table from DEFAULT_ZP_TABLE
        for i in 0..256 {
            let e = if i < DEFAULT_ZP_TABLE.len() {
                DEFAULT_ZP_TABLE[i]
            } else {
                DEFAULT_ZP_TABLE[DEFAULT_ZP_TABLE.len() - 1]
            };
            state.p[i] = e.p as u32;
            state.m[i] = e.m as u32;
            state.up[i] = e.up as u8;
            state.dn[i] = e.dn as u8;
        }

        // Hook bytestream and enable emission
        state.encoding = 1;
        let bs_ptr: *mut Cursor<Vec<u8>> = &mut *writer_box;
        state.bs = bs_ptr as *mut c_void;

        Ok(Self {
            state,
            writer: Some(writer_box),
            _marker: PhantomData,
        })
    }

    #[inline(always)]
    pub fn encode(&mut self, bit: bool, ctx: &mut BitContext) -> Result<(), ZCodecError> {
        // Compute z = a + p[ctx]
        let z = self.state.a.wrapping_add(self.state.p[*ctx as usize]);
        let mps = (*ctx & 1) != 0;
        unsafe {
            if bit != mps {
                zpcodec_encode_lps(&mut self.state, ctx as *mut u8, z);
            } else if z >= 0x8000 {
                zpcodec_encode_mps(&mut self.state, ctx as *mut u8, z);
            } else {
                // a = z
                self.state.a = z & 0xffff;
            }
        }
        Ok(())
    }

    #[inline(always)]
    pub fn IWencoder(&mut self, bit: bool) -> Result<(), ZCodecError> {
        // Raw-bit path: use fixed probability threshold at 0x8000
        let z = 0x8000u32;
        unsafe {
            if bit {
                zpcodec_encode_lps_simple(&mut self.state, z);
            } else {
                zpcodec_encode_mps_simple(&mut self.state, z);
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<Cursor<Vec<u8>>, ZCodecError> {
        unsafe {
            zpcodec_eflush(&mut self.state);
        }

        let mut writer = self.writer.take().expect("writer present");

        // Ensure the stream ends with 0xFF for proper termination
        writer.write_all(&[0xFF]).expect("write termination byte");

        let buf = writer.into_inner();

        // Debug: Check what the ZP stream looks like
        if buf.len() > 20 {
            eprintln!("[DEBUG] First 20 bytes of ZP stream: {:02x?}", &buf[..20]);
        }

        // Return the full buffer without truncation to preserve the complete ZP stream
        Ok(Cursor::new(buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    #[test]
    fn test_struct_alignment() {
        // Verify that Rust struct layout matches assembly expectations
        assert_eq!(
            mem::size_of::<ZpAsmState>(),
            32 + 256 * 4 + 256 * 4 + 256 + 256
        );

        // Check critical field offsets
        let dummy = ZpAsmState {
            byte: 0,
            scount: 0,
            delay: 0,
            encoding: 0,
            a: 0,
            subend: 0,
            buffer: 0,
            nrun: 0,
            bs: std::ptr::null_mut(),
            p: [0; 256],
            m: [0; 256],
            up: [0; 256],
            dn: [0; 256],
        };

        unsafe {
            let base = &dummy as *const _ as usize;
            let byte_offset = &dummy.byte as *const _ as usize - base;
            let scount_offset = &dummy.scount as *const _ as usize - base;
            let delay_offset = &dummy.delay as *const _ as usize - base;
            let bs_offset = &dummy.bs as *const _ as usize - base;

            eprintln!("[TEST] ZpAsmState offsets:");
            eprintln!("  byte: {}", byte_offset);
            eprintln!("  scount: {}", scount_offset);
            eprintln!("  delay: {}", delay_offset);
            eprintln!("  bs: {}", bs_offset);

            // Assembly expects these offsets
            assert_eq!(byte_offset, 0, "byte should be at offset 0");
            assert_eq!(scount_offset, 1, "scount should be at offset 1");
            assert_eq!(delay_offset, 2, "delay should be at offset 2");
            assert_eq!(bs_offset, 24, "bs should be at offset 24 on x86_64");
        }
    }
}

impl ZpEncoderCursor for ZEncoder<Cursor<Vec<u8>>> {
    #[inline(always)]
    fn encode(&mut self, bit: bool, ctx: &mut BitContext) -> Result<(), ZCodecError> {
        ZEncoder::encode(self, bit, ctx)
    }
    #[inline(always)]
    fn IWencoder(&mut self, bit: bool) -> Result<(), ZCodecError> {
        ZEncoder::IWencoder(self, bit)
    }
    #[inline(always)]
    fn encode_raw_bit(&mut self, bit: bool) -> Result<(), ZCodecError> {
        ZEncoder::encode_raw(self, bit)
    }
    fn tell_bytes(&self) -> usize {
        self.writer
            .as_ref()
            .map(|w| w.as_ref().get_ref().len())
            .unwrap_or(0)
    }
    fn finish(self) -> Result<Cursor<Vec<u8>>, ZCodecError> {
        ZEncoder::finish(self)
    }
}
