//! Internal document encoder implementation (private)
//!
//! This module handles the low-level encoding and assembly of DjVu documents.
//! It is used internally by the public builder API and not exposed directly.

use crate::Result;
use crate::doc::djvu_dir::{DjVmDir, DjVmNav, File as DjVuFile, FileType};
use crate::encode::jb2::{encoder::JB2Encoder, symbol_dict::SharedDict};
use crate::iff::bs_byte_stream::bzz_compress;
use crate::iff::iff::IffWriter;
use std::io::Write;

/// Internal document encoder
///
/// Used by the public builder API to assemble pages into complete DjVu documents.
pub(crate) struct DocumentEncoder;

struct BundledComponent<'a> {
    id: String,
    file_type: FileType,
    bytes: &'a [u8],
}

impl DocumentEncoder {
    /// Assembles encoded pages into a complete DjVu document
    ///
    /// Returns the complete document as bytes (single-page DJVU or multi-page DJVM)
    pub fn assemble_pages(pages: &[Vec<u8>]) -> Result<Vec<u8>> {
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

    /// Assemble pages plus their JB2 dictionaries as a bundled DjVu document.
    /// Each dictionary becomes a FORM:DJVI component and pages refer to it by
    /// the matching `dictNNNN.iff` INCL identifier written by PageComponents.
    pub fn assemble_pages_with_shared_dictionaries(
        pages: &[Vec<u8>],
        dictionaries: &[std::sync::Arc<SharedDict>],
    ) -> Result<Vec<u8>> {
        Self::assemble_pages_with_shared_dictionaries_and_navigation(pages, dictionaries, None)
    }

    pub fn assemble_pages_with_shared_dictionaries_and_navigation(
        pages: &[Vec<u8>],
        dictionaries: &[std::sync::Arc<SharedDict>],
        navigation: Option<&DjVmNav>,
    ) -> Result<Vec<u8>> {
        if dictionaries.is_empty() && navigation.is_none() {
            return Self::assemble_pages(pages);
        }

        let mut dictionary_files = Vec::with_capacity(dictionaries.len());
        for dictionary in dictionaries {
            dictionary_files.push(Self::encode_shared_dictionary(dictionary)?);
        }

        let mut components = Vec::with_capacity(dictionary_files.len() + pages.len());
        for (index, data) in dictionary_files.iter().enumerate() {
            components.push(BundledComponent {
                id: format!("dict{:04}.iff", index + 1),
                file_type: FileType::Include,
                bytes: data,
            });
        }
        for (index, data) in pages.iter().enumerate() {
            components.push(BundledComponent {
                id: format!("p{:04}.djvu", index + 1),
                file_type: FileType::Page,
                bytes: data,
            });
        }

        let mut output = Vec::new();
        Self::assemble_djvm_components(&mut output, &components, navigation)?;
        Ok(output)
    }

    fn encode_shared_dictionary(dictionary: &SharedDict) -> Result<Vec<u8>> {
        let shapes = dictionary.shapes();
        let parents = vec![-1; shapes.len()];
        let mut encoder = JB2Encoder::new(Vec::new());
        let data = encoder
            .encode_dictionary(shapes, &parents, 0)
            .map_err(|error| crate::DjvuError::EncodingError(error.to_string()))?;

        let mut output = Vec::new();
        {
            let mut cursor = std::io::Cursor::new(&mut output);
            let mut writer = IffWriter::new(&mut cursor);
            writer.write_magic_bytes()?;
            writer.put_chunk("FORM:DJVI")?;
            writer.put_chunk("Djbz")?;
            writer.write_all(&data)?;
            writer.close_chunk()?;
            writer.close_chunk()?;
        }
        Ok(output)
    }

    /// Assembles a multi-page DJVM document
    fn assemble_djvm(writer: &mut Vec<u8>, pages: &[Vec<u8>]) -> Result<()> {
        let components: Vec<_> = pages
            .iter()
            .enumerate()
            .map(|(index, bytes)| BundledComponent {
                id: format!("p{:04}.djvu", index + 1),
                file_type: FileType::Page,
                bytes,
            })
            .collect();
        Self::assemble_djvm_components(writer, &components, None)
    }

    fn assemble_djvm_components(
        writer: &mut Vec<u8>,
        components: &[BundledComponent<'_>],
        navigation: Option<&DjVmNav>,
    ) -> Result<()> {
        // Build cheap slice references, stripping the AT&T prefix where present.
        // No cloning — just pointer + length.
        let component_chunks: Vec<&[u8]> = components
            .iter()
            .map(|component| {
                let p = component.bytes;
                if p.starts_with(b"AT&TFORM") {
                    &p[4..] // Slice — zero allocation
                } else {
                    p
                }
            })
            .collect();

        let nav_data = if let Some(navigation) = navigation {
            let mut raw = Vec::new();
            navigation.encode(&mut raw)?;
            if raw.is_empty() {
                Vec::new()
            } else {
                bzz_compress(&raw, 100).map_err(|error| {
                    crate::DjvuError::EncodingError(format!("BZZ compress NAVM failed: {error}"))
                })?
            }
        } else {
            Vec::new()
        };
        let nav_chunk_size = if nav_data.is_empty() {
            0
        } else {
            8 + nav_data.len() + (nav_data.len() % 2)
        };

        // Create directory and calculate offsets
        let dirm = DjVmDir::new();

        // Estimate DIRM size conservatively
        let estimated_dirm_size = 3 + (4 * component_chunks.len()) + 80;
        let dirm_chunk_size = 8 + estimated_dirm_size + (estimated_dirm_size % 2);

        // Calculate initial page offsets (after DIRM + NAVM chunks)
        // Offsets in DIRM are ABSOLUTE file positions (confirmed by analyzing working files).
        // The base is AT&T(4) + FORM(4) + size(4) + DJVM(4) = 16 bytes.
        let base_offset = 16u32;
        let mut current_offset = base_offset + dirm_chunk_size as u32 + nav_chunk_size as u32;
        let mut file_offsets = Vec::new();

        for (component, chunk) in components.iter().zip(&component_chunks) {
            if current_offset % 2 != 0 {
                current_offset += 1;
            }

            file_offsets.push(current_offset);
            current_offset += chunk.len() as u32;
            let file = DjVuFile::new_with_offset(
                &component.id,
                &component.id,
                "",
                component.file_type,
                *file_offsets.last().unwrap(),
                chunk.len() as u32,
            );
            dirm.insert_file(file, -1)?;
        }

        // Encode DIRM to get actual size
        let mut dirm_stream = crate::iff::MemoryStream::new();
        dirm.encode_explicit(&mut dirm_stream, true, true)?;
        let dirm_data = dirm_stream.into_vec();

        // Check if estimate matches actual — any mismatch corrupts page offsets
        let actual_dirm_chunk_size = 8 + dirm_data.len() + (dirm_data.len() % 2);
        let final_dirm_data;

        if actual_dirm_chunk_size != dirm_chunk_size {
            // Re-calculate with correct DIRM size
            let corrected_dirm = DjVmDir::new();
            current_offset = base_offset + actual_dirm_chunk_size as u32 + nav_chunk_size as u32;
            let mut corrected_offsets = Vec::new();

            for (component, chunk) in components.iter().zip(&component_chunks) {
                if current_offset % 2 != 0 {
                    current_offset += 1;
                }

                corrected_offsets.push(current_offset);
                current_offset += chunk.len() as u32;
                let file = DjVuFile::new_with_offset(
                    &component.id,
                    &component.id,
                    "",
                    component.file_type,
                    *corrected_offsets.last().unwrap(),
                    chunk.len() as u32,
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
        let pages_total_size: usize = component_chunks.iter().map(|p| p.len()).sum();

        // Calculate padding
        let mut padding_bytes = 0;
        let mut pos = base_offset as usize + total_dirm_chunk_size + nav_chunk_size;
        for page_chunk in &component_chunks {
            if pos % 2 != 0 {
                padding_bytes += 1;
                pos += 1;
            }
            pos += page_chunk.len();
        }

        let total_djvm_payload =
            total_dirm_chunk_size + nav_chunk_size + pages_total_size + padding_bytes;

        // Write DJVM header
        writer.write_all(b"AT&TFORM")?;
        writer.write_all(&((4 + total_djvm_payload) as u32).to_be_bytes())?;
        writer.write_all(b"DJVM")?;

        // Write DIRM chunk
        writer.write_all(b"DIRM")?;
        writer.write_all(&(final_dirm_data.len() as u32).to_be_bytes())?;
        writer.write_all(&final_dirm_data)?;
        if final_dirm_data.len() % 2 != 0 {
            writer.write_all(&[0])?; // padding
        }

        if !nav_data.is_empty() {
            writer.write_all(b"NAVM")?;
            writer.write_all(&(nav_data.len() as u32).to_be_bytes())?;
            writer.write_all(&nav_data)?;
            if nav_data.len() % 2 != 0 {
                writer.write_all(&[0])?;
            }
        }

        // Write page chunks with alignment
        let mut written_pos = base_offset as usize + total_dirm_chunk_size + nav_chunk_size;
        for page_data in &component_chunks {
            if written_pos % 2 != 0 {
                writer.write_all(&[0])?;
                written_pos += 1;
            }

            writer.write_all(page_data)?;
            written_pos += page_data.len();
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::page_encoder::{PageComponents, PageEncodeParams};
    use crate::encode::jb2::symbol_dict::BitImage;
    use std::process::Command;

    #[test]
    fn shared_jb2_dictionary_is_a_djvi_component_referenced_by_the_page() {
        let mut shape = BitImage::new(5, 7).unwrap();
        shape.set_usize(2, 3, true);
        let dictionary = std::sync::Arc::new(SharedDict::new(vec![shape]));

        // With no local shapes, this blit refers directly to shared shape 0.
        let page = PageComponents::new_with_dimensions(32, 32)
            .with_jb2_manual(Vec::new(), vec![(8, 8, 0)])
            .with_shared_dict(std::sync::Arc::clone(&dictionary));
        let page = page
            .encode_with_shared_dict_id(
                &PageEncodeParams::default(),
                1,
                118,
                1,
                Some(2.2),
                Some("dict0001.iff"),
            )
            .unwrap();

        let navigation = DjVmNav {
            bookmarks: vec![crate::doc::Bookmark {
                title: "Opening".to_string(),
                dest: "#p0001.djvu".to_string(),
                children: Vec::new(),
            }],
        };
        let document = DocumentEncoder::assemble_pages_with_shared_dictionaries_and_navigation(
            &[page],
            &[dictionary],
            Some(&navigation),
        )
        .unwrap();

        assert!(document.windows(4).any(|chunk| chunk == b"DJVI"));
        assert!(document.windows(4).any(|chunk| chunk == b"Djbz"));
        assert!(document.windows(4).any(|chunk| chunk == b"INCL"));
        assert!(document.windows(4).any(|chunk| chunk == b"NAVM"));
        assert!(
            document
                .windows(b"dict0001.iff".len())
                .any(|chunk| chunk == b"dict0001.iff")
        );

        // Verify the directory record, dictionary component, and INCL resolve
        // together in an independent DjVuLibre decoder when it is available.
        if Command::new("ddjvu").arg("--help").output().is_ok() {
            let base = std::env::temp_dir()
                .join(format!("djvulibrust-shared-dict-{}", std::process::id()));
            let input = base.with_extension("djvu");
            let output = base.with_extension("ppm");
            std::fs::write(&input, &document).unwrap();
            let result = Command::new("ddjvu")
                .args([
                    "-format=ppm",
                    input.to_str().unwrap(),
                    output.to_str().unwrap(),
                ])
                .output()
                .unwrap();
            if Command::new("djvused").arg("--help").output().is_ok() {
                let outline = Command::new("djvused")
                    .arg(&input)
                    .args(["-e", "print-outline"])
                    .output()
                    .unwrap();
                assert!(
                    outline.status.success()
                        && String::from_utf8_lossy(&outline.stdout).contains("Opening"),
                    "djvused could not decode NAVM: {}",
                    String::from_utf8_lossy(&outline.stderr)
                );
            }
            let _ = std::fs::remove_file(&input);
            let _ = std::fs::remove_file(&output);
            assert!(
                result.status.success(),
                "ddjvu could not resolve the shared dictionary: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
}
