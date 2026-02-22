// src/chunk_tree.rs

use crate::doc::djvu_dir::FileType as DirFileType;

/// Maps a DjVu file type to its canonical chunk ID.
pub fn file_type_to_id(file_type: DirFileType) -> [u8; 4] {
    match file_type {
        DirFileType::Page => *b"FORM", // FORM:DJVU (handled with secondary_id)
        DirFileType::Include => *b"INCL", // Included file
        DirFileType::Thumbnails => *b"THUM", // Thumbnails chunk
        DirFileType::SharedAnno => *b"ANTa", // Shared annotation chunk
    }
}

/// Pads the writer to an even offset if needed (IFF spec compliance).
pub fn align_even<W: Write + Seek>(w: &mut W) -> std::io::Result<()> {
    let pos = w.stream_position()?;
    if pos % 2 != 0 {
        w.write_all(&[0x00])?;
    }
    Ok(())
}

// An in-memory representation of an IFF file structure.
//
// This module replaces the C++ `GIFFManager` and `GIFFChunk` classes. It provides
// a tree-like data structure, `IffChunk`, that can be loaded from a stream,
// manipulated in memory, and saved back to a stream.
use crate::iff::iff::IffReaderExt;
use crate::iff::iff::IffWriter;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::utils::error::{DjvuError, Result};

/// Represents the data payload of an `IffChunk`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkPayload {
    /// A raw byte buffer for simple chunks (e.g., "BG44", "INFO").
    Raw(Vec<u8>),
    /// A list of child chunks for composite chunks (e.g., "FORM", "LIST").
    Composite {
        /// The 4-character secondary identifier (e.g., "DJVU" in "FORM:DJVU").
        secondary_id: [u8; 4],
        /// The vector of child chunks.
        children: Vec<IffChunk>,
    },
}

/// Represents a single chunk in an IFF file, which can be either a leaf or a node in a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IffChunk {
    /// The 4-character primary identifier (e.g., "FORM", "PM44").
    pub id: [u8; 4],
    /// The chunk's data, which can be raw bytes or a collection of sub-chunks.
    pub payload: ChunkPayload,
}

impl IffChunk {
    /// Creates a new raw data chunk.
    #[inline]
    pub fn new_raw(id: [u8; 4], data: Vec<u8>) -> Self {
        IffChunk {
            id,
            payload: ChunkPayload::Raw(data),
        }
    }

    /// Creates a new, empty composite chunk.
    #[inline]
    pub fn new_composite(id: [u8; 4], secondary_id: [u8; 4]) -> Self {
        IffChunk {
            id,
            payload: ChunkPayload::Composite {
                secondary_id,
                children: Vec::new(),
            },
        }
    }

    /// Returns `true` if this is a composite chunk.
    #[inline]
    pub fn is_composite(&self) -> bool {
        matches!(self.payload, ChunkPayload::Composite { .. })
    }

    /// Returns the chunk's primary ID as a string slice.
    #[inline]
    pub fn id_as_str(&self) -> &str {
        std::str::from_utf8(&self.id).unwrap_or("????")
    }

    /// Recursively writes this chunk and its children to the `IffWriter`.
    fn write(&self, writer: &mut IffWriter<'_>) -> Result<()> {
        match &self.payload {
            ChunkPayload::Raw(data) => {
                let id_str = std::str::from_utf8(&self.id).unwrap_or("????");
                writer.put_chunk(id_str)?;
                writer.write_all(data)?;
            }
            ChunkPayload::Composite {
                secondary_id,
                children,
            } => {
                let id_str = std::str::from_utf8(&self.id).unwrap_or("????");
                let secondary_str = std::str::from_utf8(secondary_id)
                    .unwrap_or("????")
                    .trim_end_matches('\0');
                let full_id = format!("{}:{}", id_str, secondary_str);
                writer.put_chunk(&full_id)?;
                for child in children {
                    child.write(writer)?;
                }
            }
        }
        writer.close_chunk()?;
        Ok(())
    }
}

/// Represents an entire IFF document as a tree of chunks.
/// This is the main entry point for creating, loading, and saving IFF files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IffDocument {
    /// The root chunk of the document, typically a "FORM" chunk.
    pub root: IffChunk,
}

impl IffDocument {
    /// Creates a new IFF document with a specified root chunk.
    #[inline]
    pub fn new(root_chunk: IffChunk) -> Self {
        IffDocument { root: root_chunk }
    }

    /// Parses an entire IFF stream from a reader into an `IffDocument`.
    pub fn from_reader<R: Read + Seek>(mut reader: R) -> Result<Self> {
        use crate::iff::iff::IffReaderExt;

        // Read the root chunk header
        let root_chunk_header = reader.next_chunk()?.ok_or_else(|| {
            DjvuError::Stream("Cannot create document from empty stream.".to_string())
        })?;

        if !root_chunk_header.is_composite {
            return Err(DjvuError::Stream(
                "Root chunk of a document must be a composite type (e.g., FORM).".to_string(),
            ));
        }

        // Get the data of the root chunk to parse its children
        let root_data = reader.get_chunk_data(&root_chunk_header)?;
        let mut root_data_reader = std::io::Cursor::new(root_data);

        // Read the children recursively from the root's data
        let children = Self::read_chunk_tree(&mut root_data_reader)?;

        let root = IffChunk {
            id: root_chunk_header.id,
            payload: ChunkPayload::Composite {
                secondary_id: root_chunk_header.secondary_id,
                children,
            },
        };

        Ok(IffDocument { root })
    }

    /// A recursive helper to read a tree of chunks from a seekable reader.
    fn read_chunk_tree<R: Read + Seek>(mut reader: R) -> Result<Vec<IffChunk>> {
        let mut children = Vec::new();

        while let Some(chunk_header) = reader.next_chunk()? {
            let chunk = if chunk_header.is_composite {
                // For composite chunks, read their data and recurse
                let chunk_data = reader.get_chunk_data(&chunk_header)?;
                let mut chunk_data_reader = std::io::Cursor::new(chunk_data);
                let sub_children = Self::read_chunk_tree(&mut chunk_data_reader)?;
                IffChunk {
                    id: chunk_header.id,
                    payload: ChunkPayload::Composite {
                        secondary_id: chunk_header.secondary_id,
                        children: sub_children,
                    },
                }
            } else {
                // For raw chunks, just read their data
                let data = reader.get_chunk_data(&chunk_header)?;
                IffChunk::new_raw(chunk_header.id, data)
            };
            children.push(chunk);
        }

        Ok(children)
    }

    /// Writes the entire IFF document to the given writer.
    pub fn write<W: Write + Seek>(&self, writer: W) -> Result<()> {
        let mut iff_writer = IffWriter::new(writer);
        iff_writer.write_magic_bytes()?;
        self.root.write(&mut iff_writer)?;
        Ok(())
    }

    /// Writes the IFF document to the writer, patching the DIRM chunk offsets in a single pass.
    ///
    /// - `writer`: Output stream (must support Write + Seek)
    /// - `dir_model`: Directory model (DjVmDir) containing file order and metadata
    /// - `data_map`: Map of file IDs to DataPool (file contents)
    ///
    /// This method:
    /// 1. Writes magic bytes and FORM:DJVM root
    /// 2. Reserves space for DIRM (directory) chunk, records its offset
    /// 3. Streams all file chunks (pages, includes, etc.), recording offsets/sizes
    /// 4. Patches the DIRM chunk in-place with actual offsets
    /// 5. Ensures even-byte alignment for all chunks
    pub fn write_with_dirm_patch<W: Write + Seek>(
        &self,
        mut writer: W,
        dir_model: &crate::doc::djvu_dir::DjVmDir,
        data_map: &std::collections::HashMap<String, crate::iff::data_pool::DataPool>,
    ) -> Result<()> {
        use std::collections::HashMap;

        // Write DjVu magic bytes
        let mut iff_writer = IffWriter::new(&mut writer);
        iff_writer.write_magic_bytes()?;

        // Write FORM:DJVM root chunk header (reserve size)
        let form_start = iff_writer.stream_position()?;
        iff_writer.put_chunk("FORM:DJVM")?;

        // --- DIRM chunk ---
        let dirm_offset = iff_writer.stream_position()?;
        // Write DIRM header and dummy payload (size to be patched)
        iff_writer.put_chunk("DIRM")?;
        let dirm_payload_offset = iff_writer.stream_position()?;
        // Encode directory with dummy offsets to reserve space
        let mut dummy_dir_stream = crate::iff::byte_stream::MemoryStream::new();
        dir_model.encode_explicit(&mut dummy_dir_stream, true, true)?;
        let dummy_dir_bytes = dummy_dir_stream.into_inner();
        iff_writer.write_all(&dummy_dir_bytes)?;
        let dirm_end = iff_writer.stream_position()?;
        iff_writer.close_chunk()?;

        // --- File chunks (pages, includes, etc.) ---
        let mut file_offsets: HashMap<String, (u32, u32)> = HashMap::new();
        let files_list = dir_model.get_files_list();
        for file in files_list {
            let file_id = &file.id;
            #[allow(unused_assignments)]
            let mut chunk_id_buf = [0u8; 4];
            let chunk_id_str = if file.file_type == DirFileType::Page {
                "FORM:DJVU"
            } else {
                chunk_id_buf = file_type_to_id(file.file_type);
                std::str::from_utf8(&chunk_id_buf).unwrap_or("????")
            };

            let payload = data_map.get(file_id).ok_or_else(|| {
                DjvuError::Stream(format!("Missing data for file_id: {}", file_id))
            })?;
            let chunk_start = iff_writer.stream_position()?;
            iff_writer.put_chunk(chunk_id_str)?;
            iff_writer.write_all(&payload.to_vec()?)?;
            iff_writer.close_chunk()?;
            let chunk_end = iff_writer.stream_position()?; // Position after padding
            let offset = (chunk_start - form_start) as u32;
            let size = (chunk_end - chunk_start) as u32;
            file_offsets.insert(file_id.clone(), (offset, size));
        }

        // --- Patch DIRM chunk with real offsets ---
        // Create a HashMap with just the offsets for clone_with_new_offsets
        let mut offset_map = HashMap::new();
        for (id, (offset, _)) in &file_offsets {
            offset_map.insert(id.clone(), *offset);
        }
        let patched_dir = dir_model.clone_with_new_offsets(&offset_map);
        let mut real_dir_stream = crate::iff::byte_stream::MemoryStream::new();
        patched_dir.encode_explicit(&mut real_dir_stream, true, true)?;
        let real_dir_bytes = real_dir_stream.into_inner();
        // Seek to DIRM payload and overwrite
        iff_writer.seek(SeekFrom::Start(dirm_payload_offset))?;
        iff_writer.write_all(&real_dir_bytes)?;
        align_even(&mut iff_writer)?;
        // Seek back to end
        iff_writer.seek(SeekFrom::Start(dirm_end))?;

        // --- Patch DIRM chunk size ---
        let dirm_size = (dirm_end - dirm_payload_offset) as u32;
        iff_writer.seek(SeekFrom::Start(dirm_offset + 4))?;
        iff_writer.write_all(&dirm_size.to_be_bytes())?;
        iff_writer.seek(SeekFrom::Start(dirm_end))?;

        // --- Close FORM:DJVM ---
        iff_writer.close_chunk()?;
        Ok(())
    }
}
