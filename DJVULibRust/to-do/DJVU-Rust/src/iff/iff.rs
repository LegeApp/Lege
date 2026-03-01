// src/iff.rs

//! A module for reading and writing IFF (Interchange File Format) streams.
//!
//! This module provides:
//! - `IffReaderExt`: A trait for parsing IFF chunks from any source that implements `Read` and `Seek`.
//! - `IffWriter`: A struct for creating IFF files on any destination that implements `Write` and `Seek`.

use crate::utils::error::{DjvuError, Result};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Read, Seek, SeekFrom, Write};

/// Represents the header of an IFF chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// The 4-character primary identifier (e.g., "FORM", "PM44").
    pub id: [u8; 4],
    /// The 4-character secondary identifier for composite chunks (e.g., "DJVU" in "FORM:DJVU").
    /// For simple chunks, this is typically all spaces or nulls.
    pub secondary_id: [u8; 4],
    /// The size of the chunk's data payload in bytes.
    pub size: u32,
    /// Indicates if the chunk is a composite type like 'FORM' or 'LIST'.
    pub is_composite: bool,
}

impl Chunk {
    /// Returns the full chunk ID as a string, e.g., "FORM:DJVU".
    #[inline]
    pub fn full_id(&self) -> String {
        let primary = String::from_utf8_lossy(&self.id);
        if self.is_composite {
            let secondary = String::from_utf8_lossy(&self.secondary_id);
            format!("{}:{}", primary, secondary.trim_end())
        } else {
            primary.trim_end().to_string()
        }
    }
}

/// An extension trait for reading IFF-structured data from a seekable stream.
/// This provides a higher-level API for iterating through chunks.
pub trait IffReaderExt: Read + Seek {
    /// Reads the next chunk header from the stream.
    ///
    /// On success, returns `Ok(Some(Chunk))`.
    /// On end-of-stream, returns `Ok(None)`.
    /// On a parsing error, returns `Err(DjvuError)`.
    ///
    /// After calling this, the stream is positioned at the start of the chunk's
    /// data payload.
    fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        let mut id = [0u8; 4];
        match self.read_exact(&mut id) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }

        let size = self.read_u32::<BigEndian>()?;
        let is_composite = matches!(&id, b"FORM" | b"LIST" | b"PROP" | b"CAT ");

        let secondary_id = if is_composite {
            let mut sid = [0u8; 4];
            self.read_exact(&mut sid)?;
            sid
        } else {
            [b' '; 4]
        };

        Ok(Some(Chunk {
            id,
            secondary_id,
            size: if is_composite { size - 4 } else { size },
            is_composite,
        }))
    }

    /// Reads the data payload of a given chunk.
    ///
    /// This method reads `chunk.size` bytes from the current stream position
    /// and returns them in a `Vec<u8>`. It also handles the IFF padding byte
    /// by seeking past it if necessary.
    fn get_chunk_data(&mut self, chunk: &Chunk) -> Result<Vec<u8>> {
        let mut data = vec![0; chunk.size as usize];
        self.read_exact(&mut data)?;

        // IFF chunks are padded to an even number of bytes.
        if chunk.size % 2 != 0 {
            self.seek(SeekFrom::Current(1))?;
        }

        Ok(data)
    }
}

// Blanket implementation for any type that is Read + Seek.
impl<T: Read + Seek> IffReaderExt for T {}

/// A writer for creating IFF-structured data on a byte stream.
/// The underlying writer must also implement `Seek` to allow for patching chunk sizes.
pub trait WriteSeek: Write + Seek {}
impl<T: Write + Seek> WriteSeek for T {}

pub struct IffWriter<'a> {
    writer: Box<dyn WriteSeek + 'a>,
    // Stack of (size_field_position, payload_start_position, is_composite)
    chunk_stack: Vec<(u64, u64, bool)>,
}

impl<'a> IffWriter<'a> {
    /// Creates a new `IffWriter` that wraps an existing writer.
    #[inline]
    pub fn new(writer: impl Write + Seek + 'a) -> Self {
        IffWriter {
            writer: Box::new(writer),
            chunk_stack: Vec::new(),
        }
    }

    /// Writes the DjVu "AT&T" magic bytes to the start of the stream.
    /// This should only be called once at the very beginning of the file.
    #[inline]
    pub fn write_magic_bytes(&mut self) -> Result<()> {
        self.writer.write_all(&[0x41, 0x54, 0x26, 0x54])?;
        Ok(())
    }

    /// Begins a new chunk, writes its header with a placeholder size, and returns the position of the size field.
    /// The caller is responsible for calling `patch_chunk_size` later.
    pub fn write_chunk_header(&mut self, full_id: &str) -> Result<u64> {
        let (id, secondary_id) = Self::parse_full_id(full_id)?;
        self.writer.write_all(&id)?;
        let size_pos = self.writer.stream_position()?;
        self.writer.write_u32::<BigEndian>(0)?; // Placeholder size
        if let Some(sid) = secondary_id {
            self.writer.write_all(&sid)?;
        }
        Ok(size_pos)
    }

    /// Identical logic for the rare direct-patch helper
    /// Note: This method cannot distinguish between composite and simple chunks,
    /// so it may not work correctly for composite chunks. Use put_chunk/close_chunk instead.
    pub fn patch_chunk_size(&mut self, size_pos: u64) -> Result<()> {
        let end_pos = self.writer.stream_position()?;
        // The content size is everything from after the size field to the current position.
        let content_size = end_pos - (size_pos + 4);

        // Add padding if content size is odd. The padding byte is not part of the size.
        if (content_size & 1) != 0 {
            self.writer.write_all(&[0])?;
        }

        let final_pos = self.writer.stream_position()?;

        // Seek back, write the real size, and restore position.
        self.writer.seek(SeekFrom::Start(size_pos))?;
        self.writer.write_u32::<BigEndian>(content_size as u32)?;
        self.writer.seek(SeekFrom::Start(final_pos))?;

        Ok(())
    }

    /// Begins a new chunk with the given ID.
    ///
    /// For composite chunks, the ID should be in the format "FORM:DJVU".
    /// The writer is now positioned to write the chunk's payload.
    pub fn put_chunk(&mut self, full_id: &str) -> Result<()> {
        let (id, secondary_id) = Self::parse_full_id(full_id)?;
        let is_composite = secondary_id.is_some();

        self.writer.write_all(&id)?;

        // Store the position of the size field to be patched later.
        let size_pos = self.writer.stream_position()?;

        // Write a placeholder for the size.
        self.writer.write_u32::<BigEndian>(0)?;

        let payload_start_pos = if let Some(sid) = secondary_id {
            self.writer.write_all(&sid)?;
            self.writer.stream_position()?
        } else {
            self.writer.stream_position()?
        };

        self.chunk_stack.push((size_pos, payload_start_pos, is_composite));

        Ok(())
    }

    /// Finishes the most recently opened chunk.
    ///
    /// For composite chunks, the size includes the 4-byte secondary id
    /// to match the DjVu specification and standard IFF format.
    pub fn close_chunk(&mut self) -> Result<()> {
        let (size_pos, payload_start_pos, is_composite) = self.chunk_stack.pop()
            .ok_or_else(|| DjvuError::InvalidOperation("close_chunk: no open chunk".into()))?;

        let mut end_pos = self.writer.stream_position()?;
        
        // Calculate the size field value
        // For composite chunks: include the secondary ID (full payload from after size field)
        // For simple chunks: exclude nothing (payload from after size field)
        let size_calculation_start = if is_composite {
            size_pos + 4  // Start counting from after the size field (includes secondary ID)
        } else {
            payload_start_pos  // Start counting from after the size field (no secondary ID)
        };
        
        let chunk_size_field = end_pos - size_calculation_start;

        // IFF: pad to even overall size, but byte is **not** counted
        if (chunk_size_field & 1) != 0 {
            self.writer.write_all(&[0])?;
            end_pos += 1;
        }

        // Patch the size field and restore position
        self.writer.seek(SeekFrom::Start(size_pos))?;
        self.writer.write_u32::<BigEndian>(chunk_size_field as u32)?;
        self.writer.seek(SeekFrom::Start(end_pos))?;
        Ok(())
    }

    /// Returns the current nesting level (number of open chunks).
    pub fn nesting_level(&self) -> usize {
        self.chunk_stack.len()
    }

    /// Helper to parse a user-friendly ID string into IFF bytes.
    fn parse_full_id(full_id: &str) -> Result<([u8; 4], Option<[u8; 4]>)> {
        let parts: Vec<_> = full_id.split(':').collect();
        match parts.as_slice() {
            [primary] => {
                if primary.len() != 4 {
                    return Err(DjvuError::InvalidArg(format!(
                        "Chunk ID must be 4 characters: '{}'",
                        primary
                    )));
                }
                Ok((primary.as_bytes().try_into().unwrap(), None))
            }
            [primary, secondary] => {
                if primary.len() != 4 || secondary.len() > 4 {
                    return Err(DjvuError::InvalidArg(format!(
                        "Composite chunk IDs must be 4 chars: '{}:{}'",
                        primary, secondary
                    )));
                }
                let mut sid_buf = [0u8; 4]; // Pad with nulls, not spaces
                sid_buf[..secondary.len()].copy_from_slice(secondary.as_bytes());
                Ok((primary.as_bytes().try_into().unwrap(), Some(sid_buf)))
            }
            _ => Err(DjvuError::InvalidArg(format!(
                "Invalid chunk ID format: '{}'",
                full_id
            ))),
        }
    }
}

/// An extension trait to provide helper methods for `IffWriter`.
pub trait IffWriterExt {
    /// Writes a complete simple chunk (header, data, and padding) to the stream.
    fn write_chunk(&mut self, id: [u8; 4], data: &[u8]) -> Result<()>;
}

impl<'a> IffWriterExt for IffWriter<'a> {
    fn write_chunk(&mut self, id: [u8; 4], data: &[u8]) -> Result<()> {
        let id_str = std::str::from_utf8(&id)
            .map_err(|e| DjvuError::InvalidArg(format!("Invalid UTF-8 in chunk ID: {}", e)))?;
        self.put_chunk(id_str)?;
        self.write_all(data)?;
        self.close_chunk()
    }
}

// Implement Write for IffWriter to pass through writes to the underlying writer.
impl<'a> Write for IffWriter<'a> {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.write(buf)
    }

    #[inline]
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

// Implement Seek for IffWriter to pass through seeks to the underlying writer.
impl<'a> Seek for IffWriter<'a> {
    #[inline]
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.writer.seek(pos)
    }
}
