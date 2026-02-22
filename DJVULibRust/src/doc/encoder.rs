//! Internal document encoder implementation (private)
//!
//! This module handles the low-level encoding and assembly of DjVu documents.
//! It is used internally by the public builder API and not exposed directly.

use crate::doc::djvu_dir::{DjVmDir, File as DjVuFile, FileType};
// NAVM-related imports disabled for now - keep for future use
// use crate::doc::djvu_dir::{Bookmark, DjVmNav};
// use crate::iff::bs_byte_stream::bzz_compress;
// use crate::iff::MemoryStream;
use crate::Result;
use byteorder::{BigEndian, WriteBytesExt};
use std::io::Write;
use std::sync::Arc;

/// Internal document encoder
///
/// Used by the public builder API to assemble pages into complete DjVu documents.
pub(crate) struct DocumentEncoder;

impl DocumentEncoder {
    /// Assembles encoded pages into a complete DjVu document
    ///
    /// Returns the complete document as bytes (single-page DJVU or multi-page DJVM)
    pub fn assemble_pages(pages: &[Arc<Vec<u8>>]) -> Result<Vec<u8>> {
        let mut output = Vec::new();

        if pages.is_empty() {
            return Ok(output);
        }

        if pages.len() == 1 {
            // Single-page document: write directly
            output.write_all(&pages[0])?;
            return Ok(output);
        }

        // Multi-page document: create DJVM
        Self::assemble_djvm(&mut output, pages)?;
        Ok(output)
    }

    /// Assembles a multi-page DJVM document
    fn assemble_djvm(writer: &mut Vec<u8>, pages: &[Arc<Vec<u8>>]) -> Result<()> {
        // Strip AT&T prefix from pages if present
        let page_chunks: Vec<Vec<u8>> = pages
            .iter()
            .map(|p| {
                if p.starts_with(b"AT&TFORM") {
                    p[4..].to_vec() // Strip "AT&T"
                } else {
                    p.to_vec()
                }
            })
            .collect();

        // NAVM feature disabled for now - keep code for future use
        // Create automatic navigation bookmarks for multi-page documents
        // let navigation = Self::create_default_navigation(pages.len())?;
        // let mut nav_stream = MemoryStream::new();
        // navigation.encode(&mut nav_stream)?;
        // let nav_raw = nav_stream.into_vec();
        // BZZ-compress the navigation data as required by DjVu spec
        // let nav_data = bzz_compress(&nav_raw, 100)
        //     .map_err(|e| crate::DjvuError::EncodingError(format!("BZZ compress NAVM failed: {e}")))?;
        // let nav_chunk_size = 8 + nav_data.len() + (nav_data.len() % 2);
        let nav_chunk_size = 0; // NAVM disabled

        // Create directory and calculate offsets
        let dirm = DjVmDir::new();

        // Estimate DIRM size conservatively
        let estimated_dirm_size = 3 + (4 * page_chunks.len()) + 80;
        let dirm_chunk_size = 8 + estimated_dirm_size + (estimated_dirm_size % 2);

    // Calculate initial page offsets (after DIRM + NAVM chunks)
    // Offsets in DIRM are ABSOLUTE file positions (confirmed by analyzing working files).
    // The base is AT&T(4) + FORM(4) + size(4) + DJVM(4) = 16 bytes.
    let base_offset = 16u32;
        let mut current_offset = base_offset
            + dirm_chunk_size as u32
            + nav_chunk_size as u32;
        let mut file_offsets = Vec::new();

        for (i, page_chunk) in page_chunks.iter().enumerate() {
            if current_offset % 2 != 0 {
                current_offset += 1;
            }

            file_offsets.push(current_offset);
            current_offset += page_chunk.len() as u32;

            let page_id = format!("p{:04}.djvu", i + 1);
            let file = DjVuFile::new_with_offset(
                &page_id,
                &page_id,
                "",
                FileType::Page,
                file_offsets[i],
                page_chunk.len() as u32,
            );
            dirm.insert_file(file, -1)?;
        }

        // Encode DIRM to get actual size
        let mut dirm_stream = crate::iff::MemoryStream::new();
        dirm.encode_explicit(&mut dirm_stream, true, true)?;
        let dirm_data = dirm_stream.into_vec();

        // Check if estimate was accurate enough
        let actual_dirm_chunk_size = 8 + dirm_data.len() + (dirm_data.len() % 2);
        let final_dirm_data;

        if (actual_dirm_chunk_size as i32 - dirm_chunk_size as i32).abs() > 16 {
            // Re-calculate with correct DIRM size
            let corrected_dirm = DjVmDir::new();
            current_offset = base_offset
                + actual_dirm_chunk_size as u32
                + nav_chunk_size as u32;
            let mut corrected_offsets = Vec::new();

            for (i, page_chunk) in page_chunks.iter().enumerate() {
                if current_offset % 2 != 0 {
                    current_offset += 1;
                }

                corrected_offsets.push(current_offset);
                current_offset += page_chunk.len() as u32;

                let page_id = format!("p{:04}.djvu", i + 1);
                let file = DjVuFile::new_with_offset(
                    &page_id,
                    &page_id,
                    "",
                    FileType::Page,
                    corrected_offsets[i],
                    page_chunk.len() as u32,
                );
                corrected_dirm.insert_file(file, -1)?;
            }

            // Re-encode with corrected offsets
            let mut corrected_stream = crate::iff::MemoryStream::new();
            corrected_dirm.encode_explicit(&mut corrected_stream, true, true)?;
            final_dirm_data = corrected_stream.into_vec();
        } else {
            final_dirm_data = dirm_data;
        }

        // Calculate total size
        let total_dirm_chunk_size = 8 + final_dirm_data.len() + (final_dirm_data.len() % 2);
        let pages_total_size: usize = page_chunks.iter().map(|p| p.len()).sum();

        // Calculate padding
    let mut padding_bytes = 0;
    let mut pos = base_offset as usize + total_dirm_chunk_size + nav_chunk_size;
        for page_chunk in &page_chunks {
            if pos % 2 != 0 {
                padding_bytes += 1;
                pos += 1;
            }
            pos += page_chunk.len();
        }

        let total_djvm_payload = total_dirm_chunk_size + nav_chunk_size + pages_total_size + padding_bytes;

        // Write DJVM header
        writer.write_all(b"AT&TFORM")?;
        writer.write_u32::<BigEndian>((4 + total_djvm_payload) as u32)?;
        writer.write_all(b"DJVM")?;

        // Write DIRM chunk
        writer.write_all(b"DIRM")?;
        writer.write_u32::<BigEndian>(final_dirm_data.len() as u32)?;
        writer.write_all(&final_dirm_data)?;
        if final_dirm_data.len() % 2 != 0 {
            writer.write_u8(0)?; // padding
        }

        // NAVM chunk disabled - keep code for future use
        // Write NAVM chunk (automatic navigation bookmarks)
        // if !nav_data.is_empty() {
        //     writer.write_all(b"NAVM")?;
        //     writer.write_u32::<BigEndian>(nav_data.len() as u32)?;
        //     writer.write_all(&nav_data)?;
        //     if nav_data.len() % 2 != 0 {
        //         writer.write_u8(0)?; // padding
        //     }
        // }

        // Write page chunks with alignment
    let mut written_pos = base_offset as usize + total_dirm_chunk_size + nav_chunk_size;
        for page_data in &page_chunks {
            if written_pos % 2 != 0 {
                writer.write_u8(0)?;
                written_pos += 1;
            }

            writer.write_all(page_data)?;
            written_pos += page_data.len();
        }

        Ok(())
    }

    // NAVM feature disabled - keep code for future use
    // /// Creates default navigation structure with simple page bookmarks
    // fn create_default_navigation(page_count: usize) -> Result<DjVmNav> {
    //     let mut nav = DjVmNav::new();
    //     
    //     for i in 0..page_count {
    //         let bookmark = Bookmark {
    //             title: format!("Page {}", i + 1),
    //             dest: format!("#p{:04}.djvu", i + 1),
    //             children: Vec::new(), // Leaf node (no children)
    //         };
    //         nav.bookmarks.push(bookmark);
    //     }
    //     
    //     Ok(nav)
    // }
}

