use crate::doc::djvu_dir::{DjVmDir, File, FileType};
use crate::doc::page_encoder::PageComponents;
use crate::{PageEncodeParams, Result};
use byteorder::{BigEndian, WriteBytesExt};
use std::io::Write;

/// A high-level encoder for creating DjVu documents.
///
/// This struct provides a builder-like interface for assembling a document
/// from multiple pages. It automatically handles creating a single-page (DJVU)
/// or multi-page (DJVM) document based on the number of pages added.
#[derive(Default)]
pub struct DocumentEncoder {
    pages: Vec<Vec<u8>>,
    params: PageEncodeParams,
    dpi: u32,
    gamma: Option<f32>,
}

impl DocumentEncoder {
    /// Creates a new `DocumentEncoder` with default parameters.
    pub fn new() -> Self {
        Self {
            pages: Vec::new(),
            params: PageEncodeParams::default(),
            dpi: 300,
            gamma: Some(2.2),
        }
    }

    /// Sets the default encoding parameters for all subsequent pages.
    pub fn with_params(mut self, params: PageEncodeParams) -> Self {
        self.params = params;
        self
    }

    /// Sets the DPI for all subsequent pages.
    pub fn with_dpi(mut self, dpi: u32) -> Self {
        self.dpi = dpi;
        self
    }

    /// Sets the gamma correction value for all subsequent pages.
    pub fn with_gamma(mut self, gamma: Option<f32>) -> Self {
        self.gamma = gamma;
        self
    }

    /// Sets whether to encode in color (true) or grayscale (false).
    pub fn with_color(mut self, color: bool) -> Self {
        self.params.color = color;
        self
    }

    pub fn with_decibels(mut self, decibels: f32) -> Self {
        self.params.decibels = Some(decibels);
        self
    }

    /// Adds a new page to the document.
    ///
    /// The page is encoded using the parameters set on the `DocumentEncoder`
    /// and stored as a complete, self-contained DJVU page chunk.
    pub fn add_page(&mut self, page_components: PageComponents) -> Result<()> {
        let page_num = (self.pages.len() + 1) as u32;
        let dpm = (self.dpi * 100 / 254) as u32; // Dots per meter
        let rotation = 1; // Default rotation

        let encoded_page_bytes =
            page_components.encode(&self.params, page_num, dpm, rotation, self.gamma)?;

        self.pages.push(encoded_page_bytes);
        Ok(())
    }

    /// Returns the number of pages currently added to the document.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Assembles the final DjVu document and writes it to the provided writer.
    ///
    /// If there is only one page, a single-page DJVU file is written.
    /// If there are multiple pages, a multi-page DJVM file is written.
    pub fn write_to<W: Write>(&self, mut writer: W) -> Result<()> {
        if self.pages.is_empty() {
            // Or return an error, an empty document is not very useful.
            return Ok(());
        }

        if self.pages.len() == 1 {
            // Single-page document, write the page data directly.
            // It should already be a valid AT&TFORM...DJVU file.
            writer.write_all(&self.pages[0])?;
            writer.flush()?;
            return Ok(());
        }

        // Multi-page document, construct a DJVM file.
        let page_chunks: Vec<Vec<u8>> = self
            .pages
            .iter()
            .map(|p| {
                if p.starts_with(b"AT&TFORM") {
                    // Strip the "AT&T" part for inclusion in a DJVM file.
                    // The page data should start with "FORM".
                    p[4..].to_vec()
                } else {
                    // Assume it's already a valid FORM chunk.
                    p.clone()
                }
            })
            .collect();

        // First, let's calculate the exact structure and offsets
        // The DJVM structure is:
        // AT&TFORM <size> DJVM DIRM <dirm_size> <dirm_data> [padding] <page1> [padding] <page2> [padding] ...

        // Create the DIRM with actual files and proper offsets
        let dirm = DjVmDir::new();

        // Calculate where each page will be positioned
        // We need to calculate the DIRM size first, but this creates a circular dependency
        // Let's estimate conservatively and then adjust

        // Estimate DIRM size: header(3) + offsets(4*num_pages) + compressed_data
        // For 2 pages, compressed data is roughly: sizes(6) + flags(2) + ids(~20) = ~28 bytes
        // After ZP compression, this could be 40-60 bytes, so let's estimate 80 bytes total
        let estimated_dirm_size = 3 + (4 * page_chunks.len()) + 80;
        let dirm_chunk_size = 8 + estimated_dirm_size + (estimated_dirm_size % 2); // Include chunk header + padding

        println!(
            "DEBUG: Page chunk sizes: {:?}",
            page_chunks.iter().map(|p| p.len()).collect::<Vec<_>>()
        );

        // Calculate page offsets (relative to start of DJVM payload, after "DJVM")
        let mut current_offset = dirm_chunk_size as u32;
        let mut file_offsets = Vec::new();

        for (i, page_chunk) in page_chunks.iter().enumerate() {
            // Ensure even alignment
            if current_offset % 2 != 0 {
                current_offset += 1;
            }

            file_offsets.push(current_offset);
            current_offset += page_chunk.len() as u32;

            // Add the files to DIRM with calculated offsets
            let page_id = format!("p{:04}", i + 1);
            let file = File::new_with_offset(
                &page_id,
                &page_id,
                "",
                FileType::Page,
                file_offsets[i],
                page_chunk.len() as u32,
            );
            println!(
                "DEBUG: Adding file {} with offset {} and size {}",
                page_id,
                file_offsets[i],
                page_chunk.len()
            );
            dirm.insert_file(file, -1)?;
        }

        // Now encode the DIRM to get its actual size
        let mut dirm_stream = crate::iff::byte_stream::MemoryStream::new();
        dirm.encode_explicit(&mut dirm_stream, true, true)?;
        let dirm_data = dirm_stream.into_vec();

        println!("DEBUG: DIRM data size: {} bytes", dirm_data.len());
        println!(
            "DEBUG: First 20 bytes of DIRM: {:02X?}",
            &dirm_data[..dirm_data.len().min(20)]
        );

        // Check if our estimate was close enough - if not, recalculate
        let actual_dirm_chunk_size = 8 + dirm_data.len() + (dirm_data.len() % 2);
        if (actual_dirm_chunk_size as i32 - dirm_chunk_size as i32).abs() > 16 {
            println!(
                "DEBUG: DIRM size estimate was off by {} bytes, recalculating...",
                actual_dirm_chunk_size as i32 - dirm_chunk_size as i32
            );

            // Recalculate with correct DIRM size
            let corrected_dirm_chunk_size = actual_dirm_chunk_size;
            current_offset = corrected_dirm_chunk_size as u32;
            file_offsets.clear();

            // Create a new DIRM with corrected offsets
            let corrected_dirm = DjVmDir::new();
            for (i, page_chunk) in page_chunks.iter().enumerate() {
                if current_offset % 2 != 0 {
                    current_offset += 1;
                }

                file_offsets.push(current_offset);
                current_offset += page_chunk.len() as u32;

                let page_id = format!("p{:04}", i + 1);
                let file = File::new_with_offset(
                    &page_id,
                    &page_id,
                    "",
                    FileType::Page,
                    file_offsets[i],
                    page_chunk.len() as u32,
                );
                corrected_dirm.insert_file(file, -1)?;
            }

            // Re-encode with corrected offsets
            let mut corrected_dirm_stream = crate::iff::byte_stream::MemoryStream::new();
            corrected_dirm.encode_explicit(&mut corrected_dirm_stream, true, true)?;
            let corrected_dirm_data = corrected_dirm_stream.into_vec();

            println!(
                "DEBUG: Corrected DIRM data size: {} bytes",
                corrected_dirm_data.len()
            );

            // Calculate total document size
            let total_dirm_chunk_size =
                8 + corrected_dirm_data.len() + (corrected_dirm_data.len() % 2);
            let pages_total_size: usize = page_chunks.iter().map(|p| p.len()).sum();

            // Calculate exact padding needed for alignment
            let mut padding_bytes = 0;
            let mut pos = total_dirm_chunk_size;
            for page_chunk in &page_chunks {
                if pos % 2 != 0 {
                    padding_bytes += 1;
                    pos += 1;
                }
                pos += page_chunk.len();
            }

            let total_djvm_payload = total_dirm_chunk_size + pages_total_size + padding_bytes;

            // Write the DJVM file
            writer.write_all(b"AT&TFORM")?;
            writer.write_u32::<BigEndian>((4 + total_djvm_payload) as u32)?; // 4 for "DJVM"
            writer.write_all(b"DJVM")?;

            // Write DIRM chunk
            writer.write_all(b"DIRM")?;
            writer.write_u32::<BigEndian>(corrected_dirm_data.len() as u32)?;
            writer.write_all(&corrected_dirm_data)?;
            if corrected_dirm_data.len() % 2 != 0 {
                writer.write_u8(0)?; // Padding
            }

            // Write each page chunk with proper alignment
            let mut written_pos = total_dirm_chunk_size; // Position after DJVM + DIRM
            for (i, page_data) in page_chunks.iter().enumerate() {
                // Add padding if needed for even alignment
                if written_pos % 2 != 0 {
                    writer.write_u8(0)?;
                    written_pos += 1;
                }

                println!(
                    "DEBUG: Writing page {} at offset {} (expected {})",
                    i + 1,
                    written_pos,
                    file_offsets[i]
                );

                writer.write_all(page_data)?;
                written_pos += page_data.len();
            }
        } else {
            // Original estimate was good enough, proceed
            let total_dirm_chunk_size = actual_dirm_chunk_size;
            let pages_total_size: usize = page_chunks.iter().map(|p| p.len()).sum();

            // Calculate exact padding needed for alignment
            let mut padding_bytes = 0;
            let mut pos = total_dirm_chunk_size;
            for page_chunk in &page_chunks {
                if pos % 2 != 0 {
                    padding_bytes += 1;
                    pos += 1;
                }
                pos += page_chunk.len();
            }

            let total_djvm_payload = total_dirm_chunk_size + pages_total_size + padding_bytes;

            // Write the DJVM file
            writer.write_all(b"AT&TFORM")?;
            writer.write_u32::<BigEndian>((4 + total_djvm_payload) as u32)?;
            writer.write_all(b"DJVM")?;

            // Write DIRM chunk
            writer.write_all(b"DIRM")?;
            writer.write_u32::<BigEndian>(dirm_data.len() as u32)?;
            writer.write_all(&dirm_data)?;
            if dirm_data.len() % 2 != 0 {
                writer.write_u8(0)?;
            }

            // Write each page chunk with proper alignment
            let mut written_pos = total_dirm_chunk_size;
            for (i, page_data) in page_chunks.iter().enumerate() {
                if written_pos % 2 != 0 {
                    writer.write_u8(0)?;
                    written_pos += 1;
                }

                println!(
                    "DEBUG: Writing page {} at offset {} (expected {})",
                    i + 1,
                    written_pos,
                    file_offsets[i]
                );

                writer.write_all(page_data)?;
                written_pos += page_data.len();
            }
        }

        writer.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::page_encoder::PageComponents;
    use image::RgbImage;
    use std::io::Cursor;

    #[test]
    fn test_single_page_document() -> Result<()> {
        let mut encoder = DocumentEncoder::new();
        let page1 = PageComponents::new().with_background(RgbImage::new(10, 10))?;
        encoder.add_page(page1)?;

        let mut buffer = Cursor::new(Vec::new());
        encoder.write_to(&mut buffer)?;

        let data = buffer.into_inner();
        assert!(data.len() > 20); // Sanity check
        assert_eq!(&data[0..8], b"AT&TFORM");
        assert_eq!(&data[12..16], b"DJVU"); // Should be a single DJVU file
        Ok(())
    }

    #[test]
    fn test_multi_page_document() -> Result<()> {
        let mut encoder = DocumentEncoder::new();
        let page1 = PageComponents::new().with_background(RgbImage::new(10, 10))?;
        encoder.add_page(page1)?;
        let page2 = PageComponents::new().with_background(RgbImage::new(20, 20))?;
        encoder.add_page(page2)?;

        let mut buffer = Cursor::new(Vec::new());
        encoder.write_to(&mut buffer)?;

        let data = buffer.into_inner();
        assert!(data.len() > 20); // Sanity check
        assert_eq!(&data[0..8], b"AT&TFORM");
        assert_eq!(&data[12..16], b"DJVM"); // Should be a DJVM file
                                            // A simple search for the DIRM and the nested FORM chunks
        assert!(data.windows(4).any(|w| w == b"DIRM"));
        assert!(data.windows(4).any(|w| w == b"FORM")); // The nested page form
        assert!(data.windows(4).any(|w| w == b"DJVU"));

        Ok(())
    }
}
