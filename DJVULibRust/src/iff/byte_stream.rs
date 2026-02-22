// src/iff/byte_stream.rs

//! A byte stream abstraction for reading and writing DjVu data structures.
//! This provides big-endian byte order operations needed for DjVu format.

use crate::utils::error::{DjvuError, Result};
use bytemuck::{cast_slice, Pod, Zeroable};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Read, Write};

/// A trait for reading and writing structured data in DjVu format.
pub trait ByteStream: Read + Write {
    fn read_u8(&mut self) -> Result<u8> {
        Ok(ReadBytesExt::read_u8(self)?)
    }

    fn read_u16(&mut self) -> Result<u16> {
        Ok(ReadBytesExt::read_u16::<BigEndian>(self)?)
    }

    fn read_u24(&mut self) -> Result<u32> {
        let mut bytes = [0u8; 3];
        self.read_exact(&mut bytes)?;
        Ok(((bytes[0] as u32) << 16) | ((bytes[1] as u32) << 8) | (bytes[2] as u32))
    }

    fn read_u32(&mut self) -> Result<u32> {
        Ok(ReadBytesExt::read_u32::<BigEndian>(self)?)
    }

    fn write_u8(&mut self, value: u8) -> Result<()> {
        Ok(WriteBytesExt::write_u8(self, value)?)
    }

    fn write_u16(&mut self, value: u16) -> Result<()> {
        Ok(WriteBytesExt::write_u16::<BigEndian>(self, value)?)
    }

    fn write_u24(&mut self, value: u32) -> Result<()> {
        if value > 0xFFFFFF {
            eprintln!(
                "ERROR: Trying to write u24 value {} which is too large (max={})",
                value, 0xFFFFFF
            );
            return Err(DjvuError::InvalidArg("Value too large for u24".to_string()));
        }
        let bytes = [
            ((value >> 16) & 0xFF) as u8,
            ((value >> 8) & 0xFF) as u8,
            (value & 0xFF) as u8,
        ];
        self.write_all(&bytes)?;
        Ok(())
    }

    fn write_u32(&mut self, value: u32) -> Result<()> {
        Ok(WriteBytesExt::write_u32::<BigEndian>(self, value)?)
    }

    fn write_string(&mut self, s: &str) -> Result<()> {
        self.write_all(s.as_bytes())?;
        Ok(())
    }

    /// Efficiently write a slice of u16 values in big-endian format using bytemuck
    fn write_u16_slice(&mut self, values: &[u16]) -> Result<()> {
        let be_values: Vec<BeU16> = values.iter().map(|&v| v.into()).collect();
        let bytes: &[u8] = cast_slice(&be_values);
        self.write_all(bytes)?;
        Ok(())
    }

    /// Efficiently write a slice of u32 values in big-endian format using bytemuck
    fn write_u32_slice(&mut self, values: &[u32]) -> Result<()> {
        let be_values: Vec<BeU32> = values.iter().map(|&v| v.into()).collect();
        let bytes: &[u8] = cast_slice(&be_values);
        self.write_all(bytes)?;
        Ok(())
    }

    /// Efficiently write a slice of u24 values in big-endian format using bytemuck
    fn write_u24_slice(&mut self, values: &[u32]) -> Result<()> {
        for &value in values {
            if value > 0xFFFFFF {
                eprintln!(
                    "ERROR: Trying to write u24 slice value {} which is too large (max={})",
                    value, 0xFFFFFF
                );
                return Err(DjvuError::InvalidArg("Value too large for u24".to_string()));
            }
        }
        let be_values: Vec<BeU24> = values.iter().map(|&v| v.into()).collect();
        let bytes: &[u8] = cast_slice(&be_values);
        self.write_all(bytes)?;
        Ok(())
    }

    /// Efficiently read a slice of u16 values in big-endian format using bytemuck
    fn read_u16_slice(&mut self, count: usize) -> Result<Vec<u16>> {
        let mut buffer = vec![0u8; count * 2];
        self.read_exact(&mut buffer)?;
        let be_values: &[BeU16] = cast_slice(&buffer);
        Ok(be_values.iter().map(|&v| v.into()).collect())
    }

    /// Efficiently read a slice of u32 values in big-endian format using bytemuck
    fn read_u32_slice(&mut self, count: usize) -> Result<Vec<u32>> {
        let mut buffer = vec![0u8; count * 4];
        self.read_exact(&mut buffer)?;
        let be_values: &[BeU32] = cast_slice(&buffer);
        Ok(be_values.iter().map(|&v| v.into()).collect())
    }

    /// Efficiently read a slice of u24 values in big-endian format using bytemuck
    fn read_u24_slice(&mut self, count: usize) -> Result<Vec<u32>> {
        let mut buffer = vec![0u8; count * 3];
        self.read_exact(&mut buffer)?;
        let be_values: &[BeU24] = cast_slice(&buffer);
        Ok(be_values.iter().map(|&v| v.into()).collect())
    }
}

/// Implement ByteStream for any type that implements Read + Write
impl<T: Read + Write> ByteStream for T {}

/// A wrapper around Vec<u8> that implements ByteStream for in-memory operations
pub struct MemoryStream {
    buffer: Vec<u8>,
    position: usize,
}

impl MemoryStream {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            position: 0,
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(capacity),
            position: 0,
        }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buffer
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buffer
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.buffer
    }
}

impl Read for MemoryStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let available = self.buffer.len().saturating_sub(self.position);
        let to_read = buf.len().min(available);

        if to_read > 0 {
            buf[..to_read].copy_from_slice(&self.buffer[self.position..self.position + to_read]);
            self.position += to_read;
        }

        Ok(to_read)
    }
}

impl Write for MemoryStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // If we're writing at the end, just extend
        if self.position == self.buffer.len() {
            self.buffer.extend_from_slice(buf);
        } else {
            // Otherwise, we need to handle overwriting
            let end_pos = self.position + buf.len();
            if end_pos > self.buffer.len() {
                self.buffer.resize(end_pos, 0);
            }
            self.buffer[self.position..end_pos].copy_from_slice(buf);
        }

        self.position += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Big-endian u16 that can be safely cast to/from bytes
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct BeU16([u8; 2]);

/// Big-endian u32 that can be safely cast to/from bytes  
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct BeU32([u8; 4]);

/// Big-endian u24 (3-byte) value that can be safely cast to/from bytes
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct BeU24([u8; 3]);

impl From<u16> for BeU16 {
    fn from(value: u16) -> Self {
        BeU16(value.to_be_bytes())
    }
}

impl From<BeU16> for u16 {
    fn from(value: BeU16) -> Self {
        u16::from_be_bytes(value.0)
    }
}

impl From<u32> for BeU32 {
    fn from(value: u32) -> Self {
        BeU32(value.to_be_bytes())
    }
}

impl From<BeU32> for u32 {
    fn from(value: BeU32) -> Self {
        u32::from_be_bytes(value.0)
    }
}

impl From<u32> for BeU24 {
    fn from(value: u32) -> Self {
        if value > 0xFFFFFF {
            panic!("Value too large for u24");
        }
        BeU24([
            ((value >> 16) & 0xFF) as u8,
            ((value >> 8) & 0xFF) as u8,
            (value & 0xFF) as u8,
        ])
    }
}

impl From<BeU24> for u32 {
    fn from(value: BeU24) -> Self {
        ((value.0[0] as u32) << 16) | ((value.0[1] as u32) << 8) | (value.0[2] as u32)
    }
}
