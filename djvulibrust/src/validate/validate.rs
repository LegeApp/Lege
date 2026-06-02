//! DjVu file format validation
//!
//! This module provides functionality to validate DjVu files against the
//! official DjVu specification.

use crate::{
    iff::{IffChunk, IffReader},
    utils::error::{DjvuError, Result},
};
use std::io::{Read, Seek, SeekFrom};

/// Validates that a DjVu file follows the specification
pub fn validate_djvu<R: Read + Seek>(reader: &mut R) -> Result<()> {
    // Check DjVu magic number (0x41 0x54 0x26 0x54 = "AT&T")
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic)?;
    if magic != [0x41, 0x54, 0x26, 0x54] {
        return Err(DjvuError::ValidationError(
            "Invalid DjVu magic number".to_string(),
        ));
    }

    // Parse IFF structure
    let mut iff_reader = IffReader::new(reader);
    let start_pos = iff_reader.reader().stream_position()?;

    // The root must be a FORM chunk
    let root_chunk = iff_reader.next_chunk()?.ok_or_else(|| {
        DjvuError::ValidationError("Empty file".to_string())
    })?;

    // Validate chunk alignment
    validate_chunk_alignment(start_pos, &root_chunk)?;

    if root_chunk.id != "FORM" {
        return Err(DjvuError::ValidationError(
            "Root chunk must be a FORM chunk".to_string(),
        ));
    }

    // Read the form type
    let mut form_type = [0u8; 4];
    iff_reader.reader().read_exact(&mut form_type)?;
    let form_type = std::str::from_utf8(&form_type).map_err(|_| {
        DjvuError::ValidationError("Invalid FORM type encoding".to_string())
    })?;

    match form_type {
        "DJVU" => validate_djvu_page(&mut iff_reader, &root_chunk),
        "DJVM" => validate_djvu_document(&mut iff_reader, &root_chunk),
        "DJVI" => validate_shared_dict(&mut iff_reader, &root_chunk),
        "THUM" => validate_thumbnail(&mut iff_reader, &root_chunk),
        _ => Err(DjvuError::ValidationError(
            format!("Unknown FORM type: {}", form_type),
        )),
    }
}

/// Validates a single-page DjVu document (FORM:DJVU)
fn validate_djvu_page<R: Read + Seek>(
    reader: &mut IffReader<R>,
    root_chunk: &IffChunk,
) -> Result<()> {
    let mut seen_info = false;
    let mut bg44_count = 0;
    let valid_chunks = [
        "INFO", "Sjbz", "FG44", "BG44", "ANTa", "ANTz", "TXTa", "TXTz", 
        "FGjp", "BGjp", "Smmr", "WMRM", "FGbz",
    ];

    while let Some(chunk) = reader.next_chunk()? {
        let offset = reader.reader().stream_position()? - 8 - chunk.length as u64;
        validate_chunk_alignment(offset, &chunk)?;

        if !valid_chunks.contains(&chunk.id.as_str()) {
            return Err(DjvuError::ValidationError(
                format!("Invalid chunk type {} in FORM:DJVU", chunk.id),
            ));
        }

        if chunk.id == "INFO" {
            if seen_info {
                return Err(DjvuError::ValidationError(
                    "Multiple INFO chunks found in FORM:DJVU".to_string(),
                ));
            }
            if offset != root_chunk.start_offset + 12 {
                return Err(DjvuError::ValidationError(
                    "INFO chunk must be first in FORM:DJVU".to_string(),
                ));
            }
            if chunk.length != 10 {
                return Err(DjvuError::ValidationError(
                    format!("Invalid INFO chunk size: {} (expected 10)", chunk.length),
                ));
            }
            seen_info = true;
        } else if chunk.id == "BG44" {
            bg44_count += 1;
            // Additional BG44 validation could be added (slice count, version, etc.)
        }
    }

    if !seen_info {
        return Err(DjvuError::ValidationError(
            "Missing required INFO chunk in FORM:DJVU".to_string(),
        ));
    }

    Ok(())
}

/// Validates a multi-page DjVu document (FORM:DJVM)
fn validate_djvu_document<R: Read + Seek>(
    reader: &mut IffReader<R>,
    root_chunk: &IffChunk,
) -> Result<()> {
    let mut seen_dirm = false;
    let mut seen_navm = false;
    let valid_chunks = ["DIRM", "NAVM", "FORM"];

    while let Some(chunk) = reader.next_chunk()? {
        let offset = reader.reader().stream_position()? - 8 - chunk.length as u64;
        validate_chunk_alignment(offset, &chunk)?;

        if !valid_chunks.contains(&chunk.id.as_str()) {
            return Err(DjvuError::ValidationError(
                format!("Invalid chunk type {} in FORM:DJVM", chunk.id),
            ));
        }

        if chunk.id == "DIRM" {
            if seen_dirm {
                return Err(DjvuError::ValidationError(
                    "Multiple DIRM chunks found in FORM:DJVM".to_string(),
                ));
            }
            if offset != root_chunk.start_offset + 12 {
                return Err(DjvuError::ValidationError(
                    "DIRM chunk must be first in FORM:DJVM".to_string(),
                ));
            }
            seen_dirm = true;
        } else if chunk.id == "NAVM" {
            if seen_navm {
                return Err(DjvuError::ValidationError(
                    "Multiple NAVM chunks found in FORM:DJVM".to_string(),
                ));
            }
            seen_navm = true;
        } else if chunk.id == "FORM" {
            // Validate nested FORM chunks (DJVU or DJVI)
            let mut form_type = [0u8; 4];
            reader.reader().read_exact(&mut form_type)?;
            let form_type = std::str::from_utf8(&form_type).map_err(|_| {
                DjvuError::ValidationError("Invalid nested FORM type encoding".to_string())
            })?;
            if form_type != "DJVU" && form_type != "DJVI" {
                return Err(DjvuError::ValidationError(
                    format!("Invalid nested FORM type: {} in FORM:DJVM", form_type),
                ));
            }
            // Seek back to start of FORM chunk
            reader
                .reader()
                .seek(SeekFrom::Current(-4))?;
            let nested_form = reader.next_chunk()?.unwrap();
            match form_type {
                "DJVU" => validate_djvu_page(reader, &nested_form)?,
                "DJVI" => validate_shared_dict(reader, &nested_form)?,
                _ => unreachable!(),
            }
        }
    }

    if !seen_dirm {
        return Err(DjvuError::ValidationError(
            "Missing required DIRM chunk in FORM:DJVM".to_string(),
        ));
    }

    Ok(())
}

/// Validates a shared dictionary (FORM:DJVI)
fn validate_shared_dict<R: Read + Seek>(
    reader: &mut IffReader<R>,
    _root_chunk: &IffChunk,
) -> Result<()> {
    let valid_chunks = ["Djbz", "FGbz", "ANTa", "ANTz"];

    while let Some(chunk) = reader.next_chunk()? {
        let offset = reader.reader().stream_position()? - 8 - chunk.length as u64;
        validate_chunk_alignment(offset, &chunk)?;

        if !valid_chunks.contains(&chunk.id.as_str()) {
            return Err(DjvuError::ValidationError(
                format!("Invalid chunk type {} in FORM:DJVI", chunk.id),
            ));
        }
    }

    Ok(())
}

/// Validates a thumbnail container (FORM:THUM)
fn validate_thumbnail<R: Read + Seek>(
    reader: &mut IffReader<R>,
    _root_chunk: &IffChunk,
) -> Result<()> {
    while let Some(chunk) = reader.next_chunk()? {
        let offset = reader.reader().stream_position()? - 8 - chunk.length as u64;
        validate_chunk_alignment(offset, &chunk)?;

        if chunk.id != "TH44" {
            return Err(DjvuError::ValidationError(
                format!("Invalid chunk type {} in FORM:THUM, expected TH44", chunk.id),
            ));
        }
    }

    Ok(())
}

/// Validates chunk alignment
fn validate_chunk_alignment(offset: u64, chunk: &IffChunk) -> Result<()> {
    // Chunks must start on even boundaries
    if offset % 2 != 0 {
        return Err(DjvuError::ValidationError(
            format!("Chunk {} at offset {} is not aligned to even boundary", chunk.id, offset),
        ));
    }

    // Check if padding is needed after this chunk
    if chunk.length % 2 != 0 {
        let next_chunk_offset = offset + 8 + chunk.length as u64;
        let mut padding = [0u8; 1];
        if let Ok(n) = chunk.reader.read(&mut padding) {
            if n == 0 || padding[0] != 0x00 {
                return Err(DjvuError::ValidationError(
                    format!("Missing or invalid padding byte after chunk {}", chunk.id),
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_validate_magic_number() {
        // Valid magic
        let data = vec![0x41, 0x54, 0x26, 0x54];
        let mut cursor = Cursor::new(data);
        assert!(validate_djvu(&mut cursor).is_ok());

        // Invalid magic
        let data = b"INVALID".to_vec();
        let mut cursor = Cursor::new(data);
        assert!(validate_djvu(&mut cursor).is_err());
    }

    // Additional tests could be added for:
    // - Valid FORM:DJVU with INFO chunk
    // - Invalid FORM:DJVU missing INFO
    // - Valid FORM:DJVM with DIRM
    // - Nested FORM validation
    // - Chunk alignment validation
}