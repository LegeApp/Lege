use crate::doc::djvu_dir::{DjVmDir, File as DjVuFile, FileType};
use crate::encode::jb2::symbol_dict::SymDictBuilder;
use crate::iff::data_pool::DataPool;
use crate::iff::iff::{IffReaderExt, IffWriter, IffWriterExt};
use crate::utils::error::Result;
use crate::DjvuError;
use byteorder::{BigEndian, WriteBytesExt};
use std::collections::HashMap;
use std::fs::File as StdFile;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use url::Url;

/// Navigation/bookmark data (simplified for encoding).
#[derive(Clone, Default)]
pub struct DjVmNav {
    bookmarks: Vec<String>,
}

impl DjVmNav {
    /// Encodes the navigation data to the provided writer
    ///
    /// This method serializes the navigation data in the DjVu format
    pub fn encode<W: Write>(&self, writer: &mut W) -> Result<()> {
        // Write the number of bookmarks
        writer.write_u32::<BigEndian>(self.bookmarks.len() as u32)?;

        // Write each bookmark
        for bookmark in &self.bookmarks {
            // Write the bookmark length
            writer.write_u32::<BigEndian>(bookmark.len() as u32)?;

            // Write the bookmark text
            writer.write_all(bookmark.as_bytes())?;
        }

        Ok(())
    }

    /// Adds a bookmark to the navigation data
    pub fn add_bookmark(&mut self, bookmark: String) {
        self.bookmarks.push(bookmark);
    }
}

/// Represents a multipage DjVu document for encoding purposes.
pub struct DjVuDocument {
    dir: Arc<DjVmDir>,
    pub data: HashMap<String, DataPool>,
    nav: Option<DjVmNav>,
}

impl DjVuDocument {
    /// Creates a new, empty DjVu document.
    pub fn new() -> Self {
        DjVuDocument {
            dir: DjVmDir::new(),
            data: HashMap::new(),
            nav: None,
        }
    }

    /// Returns a reference to the navigation data.
    pub fn nav(&self) -> &Option<DjVmNav> {
        &self.nav
    }

    /// Returns a mutable reference to the navigation data.
    pub fn nav_mut(&mut self) -> &mut Option<DjVmNav> {
        &mut self.nav
    }

    /// Creates a new multi-page document from a vector of page data with custom IDs.
    pub fn from_pages(
        pages: Vec<(String, crate::doc::page_encoder::PageComponents)>,
    ) -> Result<Self> {
        let mut doc = Self::new();
        // Create a new symbol dictionary for shared symbols with default max error of 0
        let _dict_builder = SymDictBuilder::new(0);
        let dict_data = DataPool::from_vec(Vec::new());
        doc.insert_file(
            DjVuFile::new(
                "shared_anno.iff",
                "shared_anno.iff",
                "shared_anno.iff",
                FileType::SharedAnno,
            ),
            dict_data,
        )?;

        if pages.len() > 10 {
            use rayon::prelude::*;
            let encoded_pages: Vec<_> = pages
                .into_par_iter()
                .enumerate()
                .map(|(i, (page_id, page_components))| {
                    let page_num = i + 1;
                    let page_filename = format!("p{:04}.djvu", page_num);
                    let (width, height) = page_components.dimensions();
                    let rotation = if width >= height { 1 } else { 1 };
                    let gamma = Some(2.2);
                    let params = crate::doc::page_encoder::PageEncodeParams::default();
                    let encoded_data =
                        page_components.encode(&params, page_num as u32, 300, rotation, gamma)?;
                    let page_file = DjVuFile::new(
                        &page_id,
                        &page_filename,
                        &format!("Page {}", page_num),
                        FileType::Page,
                    );
                    Ok((page_file, DataPool::from_vec(encoded_data)))
                })
                .collect::<Result<Vec<_>>>()?;
            for (page_file, encoded_data) in encoded_pages {
                doc.insert_file(page_file, encoded_data)?;
            }
        } else {
            for (i, (page_id, page_components)) in pages.into_iter().enumerate() {
                let page_num = i + 1;
                let page_filename = format!("p{:04}.djvu", page_num);
                let (width, height) = page_components.dimensions();
                let rotation = if width >= height { 1 } else { 1 };
                let gamma = Some(2.2);
                let params = crate::doc::page_encoder::PageEncodeParams::default();
                let encoded_data =
                    page_components.encode(&params, page_num as u32, 300, rotation, gamma)?;
                let page_file = DjVuFile::new(
                    &page_id,
                    &page_filename,
                    &format!("Page {}", page_num),
                    FileType::Page,
                );
                doc.insert_file(page_file, DataPool::from_vec(encoded_data))?;
            }
        }
        Ok(doc)
    }

    /// Adds a page to the document.
    pub fn add_page(&mut self, page_id: String, page_data: Vec<u8>) -> Result<()> {
        let file = DjVuFile::new(&page_id, &page_id, "", FileType::Page);
        let data_pool = DataPool::from_vec(page_data);
        self.insert_file(file, data_pool)?;
        Ok(())
    }

    /// Inserts a file into the document with its data.
    pub fn insert_file(&mut self, file: Arc<DjVuFile>, data: DataPool) -> Result<()> {
        self.dir.add_file(file.clone());
        self.data.insert(file.id.clone(), data);
        Ok(())
    }

    pub fn dir_mut(&mut self) -> &mut DjVmDir {
        Arc::make_mut(&mut self.dir)
    }

    pub fn dir(&self) -> &DjVmDir {
        &self.dir
    }

    /// Inserts a file into the document with its data.
    pub fn insert_file_no_dir_update(&mut self, file: Arc<DjVuFile>, data: DataPool) -> Result<()> {
        // Check if file already exists using has_file_with_id method instead of direct field access
        if self.has_file_with_id(&file.id) {
            return Err(crate::utils::error::DjvuError::InvalidOperation(
                "File with this ID already exists".to_string(),
            ));
        }

        self.data.insert(file.id.clone(), data);
        Ok(())
    }

    pub fn insert_file_with_includes<F>(
        &mut self,
        file: Arc<DjVuFile>,
        data: Vec<u8>,
        get_data: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&str) -> Result<Box<dyn Read>>,
    {
        self.insert_file(file, DataPool::from_vec(data.clone()))?;

        let includes = self.get_included_ids(&data)?;

        for include_id in includes.into_iter() {
            let include_id_str = include_id.to_string();
            if !self.data.contains_key(&include_id_str) {
                let mut include_data = Vec::new();
                let mut reader = get_data(&include_id_str)?;
                reader.read_to_end(&mut include_data)?;

                let include_file = DjVuFile::new(
                    &include_id_str,
                    &include_id_str,
                    &include_id_str,
                    FileType::Include,
                );

                self.insert_file_with_includes(include_file, include_data, get_data)?;
            }
        }
        Ok(())
    }

    /// Sets the document's bookmarks.
    pub fn set_bookmarks(&mut self, bookmarks: Vec<String>) -> Result<()> {
        self.nav = Some(DjVmNav { bookmarks });
        Ok(())
    }

    pub fn write_bundled<W: Write + Seek>(&self, writer: W) -> Result<()> {
        let mut iff_writer = IffWriter::new(writer);
        iff_writer.put_chunk("FORM:DJVM")?;

        // Create a mutable copy of the directory to update offsets
        let dir_to_write = self.dir.as_ref().clone();

        // --- Pre-pass to calculate offsets ---
        // First, calculate the size of the DIRM and NAVM chunks to find the starting offset for file data.
        let mut dirm_size_buf = Vec::new();
        dir_to_write.encode_explicit(&mut Cursor::new(&mut dirm_size_buf), false, true)?; // bundled=false for size calculation
        let dirm_chunk_size = (8 + dirm_size_buf.len() + (dirm_size_buf.len() % 2)) as u32;

        let navm_chunk_size = if let Some(nav) = &self.nav {
            let mut nav_buf = Vec::new();
            nav.encode(&mut nav_buf)?;
            (8 + nav_buf.len() + (nav_buf.len() % 2)) as u32
        } else {
            0
        };

        // The first file component will start after the FORM header (12 bytes), the DIRM chunk, and the NAVM chunk.
        let mut current_offset = 12 + dirm_chunk_size + navm_chunk_size;

        // Create a map of file IDs to offsets
        let mut offset_map = HashMap::new();

        // First pass: collect all file IDs and their new offsets
        for file in dir_to_write.get_files_list() {
            if let Some(data_pool) = self.data.get(&file.id) {
                offset_map.insert(file.id.clone(), current_offset);

                let data_len = data_pool.len();
                let chunk_total_size = if file.file_type == FileType::Page {
                    // Page data is written raw, but must be padded to an even length.
                    (data_len + (data_len % 2)) as u32
                } else {
                    // Other files are wrapped in a chunk (ID + size + data + optional pad).
                    (8 + data_len + (data_len % 2)) as u32
                };
                current_offset += chunk_total_size;
            }
        }

        // --- Writing Pass ---
        // Create a new directory with the updated offsets
        let dir_with_offsets = dir_to_write.clone_with_new_offsets(&offset_map);

        // Now encode the directory with correct offsets
        let mut final_dirm_buf = Vec::new();
        dir_with_offsets.encode_explicit(&mut Cursor::new(&mut final_dirm_buf), true, true)?;
        iff_writer.write_chunk(*b"DIRM", &final_dirm_buf)?;

        // Write NAVM
        if let Some(nav) = &self.nav {
            let mut nav_buf = Vec::new();
            nav.encode(&mut nav_buf)?;
            iff_writer.write_chunk(*b"NAVM", &nav_buf)?;
        }

        // Write file data
        for file_info in self.dir.get_files_list() {
            if let Some(data_pool) = self.data.get(&file_info.id) {
                let data_vec = data_pool.to_vec()?;
                if file_info.file_type == FileType::Page {
                    // Each page must be wrapped in a FORM:DJVU chunk
                    iff_writer.put_chunk("FORM:DJVU")?;
                    iff_writer.write_all(&data_vec)?;
                    iff_writer.close_chunk()?;
                } else {
                    let chunk_id = file_type_to_chunk_id(file_info.file_type);
                    iff_writer.write_chunk(chunk_id, &data_vec)?;
                }
            }
        }

        iff_writer.close_chunk()?;
        Ok(())
    }

    /// Writes the document in indirect format to the specified directory.
    pub fn write_indirect(&self, codebase: &Url, idx_name: &str) -> Result<()> {
        use std::fs::create_dir_all;

        let files = self.dir.resolve_duplicates(false);

        if let Ok(mut base_path) = codebase.to_file_path() {
            base_path.pop();
            create_dir_all(&base_path)?;

            for file in &files {
                let path = base_path.join(file.get_save_name());
                if let Some(parent) = path.parent() {
                    create_dir_all(parent)?;
                }
                let mut writer = StdFile::create(&path)?;
                if let Some(data_pool) = self.data.get(&file.id) {
                    let mut data_vec = Vec::new();
                    let mut pool = data_pool.clone();
                    pool.seek(SeekFrom::Start(0))?;
                    pool.read_to_end(&mut data_vec)?;
                    self.save_file_with_remap(&data_vec, &mut writer)?;
                }
            }

            if !idx_name.is_empty() {
                let idx_path = base_path.join(idx_name);
                let writer = StdFile::create(idx_path)?;

                let mut iff_writer = IffWriter::new(writer);
                iff_writer.put_chunk("FORM:DJVM")?;

                let mut dir_buf = Vec::new();
                self.dir
                    .encode_explicit(&mut Cursor::new(&mut dir_buf), false, false)?;
                iff_writer.write_chunk(*b"DIRM", &dir_buf)?;

                if let Some(nav) = &self.nav {
                    let mut nav_buf = Vec::new();
                    nav.encode(&mut nav_buf)?;
                    iff_writer.write_chunk(*b"NAVM", &nav_buf)?;
                }

                iff_writer.close_chunk()?;
            }
        }
        Ok(())
    }

    /// Parses IFF structure to extract included file IDs from INCL chunks.
    fn get_included_ids(&self, data: &[u8]) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut cursor = Cursor::new(data);

        while let Some(chunk) = cursor.next_chunk()? {
            if chunk.id == *b"INCL" {
                let chunk_data = cursor.get_chunk_data(&chunk)?;
                let id = String::from_utf8(chunk_data).map_err(|e| {
                    DjvuError::ValidationError(format!("INCL chunk has invalid UTF-8: {}", e))
                })?;
                ids.push(id.trim_end_matches('\0').to_string());
            }
        }
        Ok(ids)
    }

    /// Saves a file with INCL chunks remapped according to the directory.
    fn save_file_with_remap<W: Write + Seek>(&self, data: &[u8], writer: &mut W) -> Result<()> {
        let mut cursor = Cursor::new(data);
        let mut iff_writer = IffWriter::new(writer);

        while let Some(chunk) = cursor.next_chunk()? {
            let chunk_data = cursor.get_chunk_data(&chunk)?;
            let mut remapped = false;

            if chunk.id == *b"INCL" {
                if let Ok(incl_id) = String::from_utf8(chunk_data.clone()) {
                    let incl_id = incl_id.trim_end_matches('\0').to_string();
                    if let Some(file) = self.dir.get_file_by_id(&incl_id) {
                        let new_incl_data = file.get_save_name().into_bytes();
                        iff_writer.write_chunk(*b"INCL", &new_incl_data)?;
                        remapped = true;
                    }
                }
            }

            if !remapped {
                iff_writer.write_chunk(chunk.id, &chunk_data)?;
            }
        }
        Ok(())
    }

    /// Checks if a file with the given ID exists.
    pub fn has_file_with_id(&self, id: &str) -> bool {
        self.data.contains_key(id)
    }

    /// Removes a file by ID.
    pub fn remove_file(&mut self, id: &str) {
        if let Ok(_) = self.dir.delete_file(id) {
            self.data.remove(id);
        }
    }
}

/// Maps a file type to its corresponding chunk ID for bundled documents.
fn file_type_to_chunk_id(file_type: FileType) -> [u8; 4] {
    match file_type {
        FileType::Page => *b"FORM", // Should not be used; pages are written directly.
        FileType::Include => *b"INCL",
        FileType::Thumbnails => *b"THUM",
        FileType::SharedAnno => *b"ANTa",
    }
}

/// Aligns the writer to an even byte boundary by writing a padding byte if needed.
fn _align_even<W: Write + Seek>(writer: &mut W) -> Result<()> {
    let pos = writer.stream_position()?;
    if pos % 2 != 0 {
        writer.write_all(&[0])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn test_create_and_save_djvu() -> Result<()> {
        let dir = tempdir()?;
        let file_path = dir.path().join("test.djvu");

        let mut doc = DjVuDocument::new();

        // Add a test page
        let page_data = vec![
            0x41, 0x54, 0x26, 0x54, 0x46, 0x4f, 0x52, 0x4d, 0x00, 0x00, 0x00, 0x0c, 0x44, 0x4a,
            0x56, 0x55, 0x46, 0x4d, 0x4d, 0x52, 0x00, 0x00, 0x00, 0x00,
        ];
        doc.add_page("page1".to_string(), page_data)?;

        // Write to a file
        let file = File::create(&file_path)?;
        doc.write_bundled(file)?;

        // Verify the file was created
        assert!(file_path.exists());

        Ok(())
    }
}
