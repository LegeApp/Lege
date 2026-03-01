// src/iff/bs_byte_stream.rs

//! This module implements the BZZ compression algorithm as required by the DjVu specification.
//! It is a port of the C++ BSByteStream implementation from DjVuLibre.

use crate::encode::zc::{BitContext, ZEncoder};
use crate::utils::error::{DjvuError, Result};
use std::io::Write;

const MIN_BLOCK_SIZE: usize = 10 * 1024;
const MAX_BLOCK_SIZE: usize = 4096 * 1024;
const OVERFLOW: usize = 32; // Extra bytes for encoding safety
const FREQMAX: usize = 4; // Max frequencies for MTF
const CTXIDS: usize = 3; // Context IDs for ZP encoding
const FREQS0: u32 = 100000; // Thresholds for estimation speed
const FREQS1: u32 = 1000000;

pub struct BsEncoder<W: Write> {
    zp_encoder: ZEncoder<W>,
    buffer: Vec<u8>,
    block_size: usize,
}

impl<W: Write> BsEncoder<W> {
    pub fn new(writer: W, block_size_k: usize) -> Result<Self> {
        let block_size = (block_size_k * 1024).clamp(MIN_BLOCK_SIZE, MAX_BLOCK_SIZE);
        let zp_encoder = ZEncoder::new(writer, true)?; // DjVu compatibility mode
        Ok(Self {
            zp_encoder,
            buffer: Vec::with_capacity(block_size + OVERFLOW),
            block_size,
        })
    }

    fn encode_block(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        // Record size BEFORE adding sentinel (critical for DjVu compatibility)
        let size = self.buffer.len() as u32; // length *without* sentinel
        self.buffer.push(0); // add sentinel

        // 1. Burrows-Wheeler Transform
        let (mut transformed_block, markerpos) = self.bwt(&self.buffer);
        self.buffer.clear();

        // 2. Encode the transformed block using MTF and ZP
        self.encode_transformed(&mut transformed_block, size, markerpos)?;

        Ok(())
    }

    /// Performs the Burrows-Wheeler Transform on the input data.
    fn bwt(&self, data: &[u8]) -> (Vec<u8>, usize) {
        let len = data.len();
        if len == 0 {
            return (Vec::new(), 0);
        }

        // BWT implementation - ensure this matches the C++ version exactly
        let mut rotations: Vec<usize> = (0..len).collect();
        rotations.sort_by(|&a, &b| {
            let a_rot = data[a..].iter().chain(data[..a].iter());
            let b_rot = data[b..].iter().chain(data[..b].iter());
            a_rot.cmp(b_rot)
        });

        let mut last_col = vec![0u8; len];
        let mut primary_index = 0;
        for (i, &start) in rotations.iter().enumerate() {
            if start == 0 {
                primary_index = i;
            }
            last_col[i] = data[(start + len - 1) % len];
        }

        println!(
            "DEBUG BWT: Input length={}, primary_index={}, first 10 bytes of output: {:?}",
            len,
            primary_index,
            &last_col[..10.min(len)]
        );

        (last_col, primary_index)
    }

    /// Encodes the transformed block with MTF and ZP encoding.
    fn encode_transformed(&mut self, data: &mut [u8], size: u32, markerpos: usize) -> Result<()> {
        println!(
            "DEBUG BZZ: Encoding block size={}, markerpos={}",
            size, markerpos
        );

        // Header: encode block size
        self.encode_raw(24, size)?;

        // Determine and encode estimation speed
        let mut fshift_ctx: BitContext = 0; // Create real context instead of literal
        let fshift = if size < FREQS0 {
            println!("DEBUG BZZ: Using fshift=0 (size {} < {})", size, FREQS0);
            self.zp_encoder.encode(false, &mut fshift_ctx)?;
            0
        } else if size < FREQS1 {
            println!("DEBUG BZZ: Using fshift=1 (size {} < {})", size, FREQS1);
            self.zp_encoder.encode(true, &mut fshift_ctx)?;
            self.zp_encoder.encode(false, &mut fshift_ctx)?;
            1
        } else {
            println!("DEBUG BZZ: Using fshift=2 (size {} >= {})", size, FREQS1);
            self.zp_encoder.encode(true, &mut fshift_ctx)?;
            self.zp_encoder.encode(true, &mut fshift_ctx)?;
            2
        };

        // Initialize Move-to-Front (MTF) tables
        let mut mtf: Vec<u8> = (0..=255).collect();
        let mut rmtf = vec![0u8; 256];
        for (i, &val) in mtf.iter().enumerate() {
            rmtf[val as usize] = i as u8;
        }
        let mut freq = [0u32; FREQMAX];
        let fadd = 4u32;

        // Encode data with MTF and ZP
        let mut mtfno = 3; // This should be mutable and track current MTF state
        let mut contexts: Vec<BitContext> = vec![0; 300]; // Context array as in C++ code
        for (i, &c) in data.iter().enumerate() {
            let mut ctxid = (CTXIDS - 1) as u8;
            if ctxid as usize > mtfno {
                ctxid = mtfno as u8;
            }

            // Get MTF position for this character (or marker)
            let mtfno_current = if i == markerpos {
                256 // Special marker position
            } else {
                rmtf[c as usize] as usize
            };

            // Update mtfno for next iteration (C++ does this)
            mtfno = mtfno_current;

            let mut cx_idx = 0;
            let bit = mtfno_current == 0;
            println!(
                "DEBUG BZZ: Encoding bit={} for mtfno={} at position {}",
                bit, mtfno_current, i
            );
            self.zp_encoder
                .encode(bit, &mut contexts[cx_idx + ctxid as usize])?;
            if bit {
                self.rotate_mtf(&mut mtf, &mut rmtf, &mut freq, c, fadd, fshift as u8);
                continue;
            }

            cx_idx += CTXIDS;
            let bit = mtfno_current == 1;
            self.zp_encoder
                .encode(bit, &mut contexts[cx_idx + ctxid as usize])?;
            if bit {
                self.rotate_mtf(&mut mtf, &mut rmtf, &mut freq, c, fadd, fshift as u8);
                continue;
            }

            cx_idx += CTXIDS;
            let bit = mtfno_current < 4;
            self.zp_encoder.encode(bit, &mut contexts[cx_idx])?;
            if bit {
                self.encode_binary(&mut contexts[cx_idx + 1..], 1, mtfno_current - 2)?;
                self.rotate_mtf(&mut mtf, &mut rmtf, &mut freq, c, fadd, fshift as u8);
                continue;
            }

            cx_idx += 1 + 1;
            let bit = mtfno_current < 8;
            self.zp_encoder.encode(bit, &mut contexts[cx_idx])?;
            if bit {
                self.encode_binary(&mut contexts[cx_idx + 1..], 2, mtfno_current - 4)?;
                self.rotate_mtf(&mut mtf, &mut rmtf, &mut freq, c, fadd, fshift as u8);
                continue;
            }

            cx_idx += 1 + 3;
            let bit = mtfno_current < 16;
            self.zp_encoder.encode(bit, &mut contexts[cx_idx])?;
            if bit {
                self.encode_binary(&mut contexts[cx_idx + 1..], 3, mtfno_current - 8)?;
                self.rotate_mtf(&mut mtf, &mut rmtf, &mut freq, c, fadd, fshift as u8);
                continue;
            }

            cx_idx += 1 + 7;
            let bit = mtfno_current < 32;
            self.zp_encoder.encode(bit, &mut contexts[cx_idx])?;
            if bit {
                self.encode_binary(&mut contexts[cx_idx + 1..], 4, mtfno_current - 16)?;
                self.rotate_mtf(&mut mtf, &mut rmtf, &mut freq, c, fadd, fshift as u8);
                continue;
            }

            cx_idx += 1 + 15;
            let bit = mtfno_current < 64;
            self.zp_encoder.encode(bit, &mut contexts[cx_idx])?;
            if bit {
                self.encode_binary(&mut contexts[cx_idx + 1..], 5, mtfno_current - 32)?;
                self.rotate_mtf(&mut mtf, &mut rmtf, &mut freq, c, fadd, fshift as u8);
                continue;
            }

            cx_idx += 1 + 31;
            let bit = mtfno_current < 128;
            self.zp_encoder.encode(bit, &mut contexts[cx_idx])?;
            if bit {
                self.encode_binary(&mut contexts[cx_idx + 1..], 6, mtfno_current - 64)?;
                self.rotate_mtf(&mut mtf, &mut rmtf, &mut freq, c, fadd, fshift as u8);
                continue;
            }

            cx_idx += 1 + 63;
            let bit = mtfno_current < 256;
            self.zp_encoder.encode(bit, &mut contexts[cx_idx])?;
            if bit {
                self.encode_binary(&mut contexts[cx_idx + 1..], 7, mtfno_current - 128)?;
                self.rotate_mtf(&mut mtf, &mut rmtf, &mut freq, c, fadd, fshift as u8);
                continue;
            }

            // Marker position (mtfno == 256)
            self.rotate_mtf(&mut mtf, &mut rmtf, &mut freq, c, fadd, fshift as u8);
        }

        Ok(())
    }

    /// Encodes a raw integer value with the specified number of bits.
    fn encode_raw(&mut self, bits: u8, x: u32) -> Result<()> {
        println!("DEBUG BZZ: encode_raw bits={}, x={}", bits, x);
        // Write bits MSB-first, as required by DjVu spec
        for i in (0..bits).rev() {
            let b = ((x >> i) & 1) != 0;
            println!("DEBUG BZZ: encode_raw bit={}", b);
            self.zp_encoder.encode(b, &mut 0)?;
        }
        Ok(())
    }

    /// Encodes a binary value with the specified number of bits using contexts.
    fn encode_binary(&mut self, ctx: &mut [BitContext], bits: u8, x: usize) -> Result<()> {
        // Implementation matches C++ exactly: ctx = ctx - 1; ctx[n]
        let mut n = 1u32;
        let m = 1u32 << bits;
        let mut x = x as u32;

        // C++ does: ctx = ctx - 1, then uses ctx[n]
        // This means we need to offset by -1 from the slice start
        // But since we can't have negative indices, we adjust our indexing
        while n < m {
            x = (x & (m - 1)) << 1;
            let b = (x >> bits) != 0;

            // Use n-1 as the index since C++ pre-decrements ctx pointer
            let ctx_idx = (n - 1) as usize;
            if ctx_idx < ctx.len() {
                self.zp_encoder.encode(b, &mut ctx[ctx_idx])?;
            }
            n = (n << 1) | (b as u32);
        }
        Ok(())
    }

    /// Rotates the MTF table and updates frequencies.
    /// c: the actual character value (not MTF position)
    fn rotate_mtf(
        &mut self,
        mtf: &mut Vec<u8>,
        rmtf: &mut [u8],
        freq: &mut [u32; FREQMAX],
        c: u8,
        mut fadd: u32,
        fshift: u8,
    ) {
        let mtfno = rmtf[c as usize] as usize; // Get current MTF position of character

        // Adjust frequencies for overflow (matches C++ exactly)
        fadd = fadd + (fadd >> fshift);
        if fadd > 0x10000000 {
            fadd = fadd >> 24;
            for f in freq.iter_mut() {
                *f = *f >> 24;
            }
        }

        let mut fc = fadd;
        if mtfno < FREQMAX {
            fc += freq[mtfno];
        }

        // Relocate char according to new frequency (exact C++ logic)
        let mut k = mtfno;
        while k >= FREQMAX {
            mtf[k] = mtf[k - 1];
            rmtf[mtf[k] as usize] = k as u8;
            k -= 1;
        }
        while k > 0 && fc >= freq[k - 1] {
            mtf[k] = mtf[k - 1];
            freq[k] = freq[k - 1];
            rmtf[mtf[k] as usize] = k as u8;
            k -= 1;
        }
        mtf[k] = c;
        freq[k] = fc;
        rmtf[c as usize] = k as u8;
    }
}

impl<W: Write> Write for BsEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut bytes_written = 0;
        while bytes_written < buf.len() {
            let remaining_in_block = self.block_size - self.buffer.len();
            let to_write = (buf.len() - bytes_written).min(remaining_in_block);

            self.buffer
                .extend_from_slice(&buf[bytes_written..bytes_written + to_write]);
            bytes_written += to_write;

            if self.buffer.len() == self.block_size {
                self.encode_block()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            }
        }
        Ok(bytes_written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.encode_block()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        // Note: ZEncoder doesn't have a public flush method, finish() will be called in Drop
        Ok(())
    }
}

impl<W: Write> Drop for BsEncoder<W> {
    fn drop(&mut self) {
        println!("DEBUG BZZ: Drop called - flushing remaining data and writing EOF marker");
        let _ = self.flush();
        // Encode EOF marker (zero-length block) - matches C++ BSByteStream::Encode::~Encode()
        if let Ok(_) = self.encode_raw(24, 0) {
            println!("DEBUG BZZ: EOF marker written successfully");
        } else {
            println!("DEBUG BZZ: ERROR writing EOF marker!");
        }
        // Note: ZEncoder will be dropped naturally, which calls its Drop impl that flushes
        println!("DEBUG BZZ: BsEncoder drop complete, ZEncoder will be dropped next");
    }
}

/// Compresses data using the DjVu BZZ compression algorithm.
/// This is a convenience function that creates a BsEncoder, writes the data,
/// and returns the compressed result.
///
/// # Arguments
/// * `data` - The raw byte slice to compress
/// * `block_size_k` - Block size in kilobytes (clamped between 10KB and 4MB)
///
/// # Returns
/// A `Result` containing the compressed data as a `Vec<u8>`
pub fn bzz_compress(data: &[u8], block_size_k: usize) -> Result<Vec<u8>> {
    let mut compressed_data = Vec::new();
    {
        let mut encoder = BsEncoder::new(&mut compressed_data, block_size_k)?;
        encoder.write_all(data).map_err(|e| DjvuError::Io(e))?;
        encoder.flush().map_err(|e| DjvuError::Io(e))?;
    }
    Ok(compressed_data)
}
