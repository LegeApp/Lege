use crate::iff::bs_byte_stream::bzz_compress;
use crate::iff::byte_stream::{ByteStream, MemoryStream};
use crate::utils::error::{DjvuError, Result};

use std::collections::HashMap;
use std::io::Write; // Added for write_all support

use std::sync::{Arc, Mutex};
pub type PageId = String;

// File types for DjVmDir
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Include = 0,
    Page = 1,
    Thumbnails = 2,
    SharedAnno = 3,
}

/// Represents a file record in a DjVmDir directory
#[derive(Debug, Clone)]
pub struct File {
    pub id: String,          // File identifier
    pub name: String,        // File name for saving
    pub title: String,       // User-friendly title
    pub offset: u32,         // Offset in bundled format
    pub size: u32,           // Size of the file
    pub file_type: FileType, // Type of the file
    pub has_name: bool,      // Indicates if name differs from id
    pub has_title: bool,     // Indicates if title differs from id
    pub page_num: i32,       // Page number if a page, -1 otherwise
    pub valid_name: bool,    // Whether the name is valid for native encoding
    oldname: String,         // Original name before modification
}

impl File {
    /// Creates a new File instance wrapped in an Arc
    pub fn new(id: &str, name: &str, title: &str, file_type: FileType) -> Arc<Self> {
        Arc::new(File {
            id: id.to_string(),
            name: name.to_string(),
            title: title.to_string(),
            offset: 0,
            size: 0,
            file_type,
            has_name: name != id,
            has_title: title != id,
            page_num: -1,
            valid_name: false,
            oldname: String::new(),
        })
    }

    /// Creates a new File instance with specified offset and size
    pub fn new_with_offset(
        id: &str,
        name: &str,
        title: &str,
        file_type: FileType,
        offset: u32,
        size: u32,
    ) -> Arc<Self> {
        Arc::new(File {
            id: id.to_string(),
            name: name.to_string(),
            title: title.to_string(),
            offset,
            size,
            file_type,
            has_name: name != id,
            has_title: title != id,
            page_num: -1,
            valid_name: false,
            oldname: String::new(),
        })
    }

    /// Checks and modifies the save name if invalid for native encoding
    pub fn check_save_name(&mut self, is_bundled: bool) -> String {
        if !is_bundled && !self.valid_name {
            let retval = if self.name.is_empty() {
                &self.id
            } else {
                &self.name
            }
            .to_string();
            // Simplified check for native encoding compatibility
            // In real implementation, check against filesystem encoding
            if retval.chars().any(|c| c.is_control() || c > '\x7F') {
                let mut buf = String::new();
                for c in retval.chars() {
                    if c.is_control() || c > '\x7F' {
                        buf.push_str(&format!("{:02X}", c as u8));
                    } else {
                        buf.push(c);
                    }
                }
                self.oldname = std::mem::replace(&mut self.name, buf);
                self.valid_name = true;
            }
            self.valid_name = true;
            self.name.clone()
        } else {
            self.get_save_name()
        }
    }

    /// Returns the save name (name if set, else id)
    pub fn get_save_name(&self) -> String {
        if self.name.is_empty() {
            self.id.clone()
        } else {
            self.name.clone()
        }
    }

    /// Returns the load name (id)
    pub fn get_load_name(&self) -> &str {
        &self.id
    }

    /// Sets the load name (id) based on a URL-like string
    pub fn set_load_name(&mut self, id: &str) {
        // Simplified: assumes id is the filename part of a URL
        self.id = id.to_string();
    }

    /// Sets the save name, resetting validity
    pub fn set_save_name(&mut self, name: &str) {
        self.valid_name = false;
        self.name = name.to_string();
        self.oldname = String::new();
    }

    /// Returns the title (title if set, else id)
    pub fn get_title(&self) -> String {
        if self.title.is_empty() {
            self.id.clone()
        } else {
            self.title.clone()
        }
    }

    /// Sets the title
    pub fn set_title(&mut self, title: &str) {
        self.title = title.to_string();
    }

    /// Returns a string representation of the file type
    pub fn get_str_type(&self) -> String {
        match self.file_type {
            FileType::Include => "INCLUDE".to_string(),
            FileType::Page => "PAGE".to_string(),
            FileType::Thumbnails => "THUMBNAILS".to_string(),
            FileType::SharedAnno => "SHARED_ANNO".to_string(),
        }
    }

    /// Checks if the file is a page
    pub fn is_page(&self) -> bool {
        self.file_type == FileType::Page
    }

    /// Checks if the file is an include file
    pub fn is_include(&self) -> bool {
        self.file_type == FileType::Include
    }

    /// Checks if the file contains thumbnails
    pub fn is_thumbnails(&self) -> bool {
        self.file_type == FileType::Thumbnails
    }

    /// Checks if the file contains shared annotations
    pub fn is_shared_anno(&self) -> bool {
        self.file_type == FileType::SharedAnno
    }

    /// Returns the page number (-1 if not a page)
    pub fn get_page_num(&self) -> i32 {
        self.page_num
    }
}

/// Directory for a multipage DjVu document (DIRM chunk)
pub struct DjVmDir {
    data: Mutex<DjVmDirData>,
}

#[derive(Clone, Default)]
pub struct DjVmDirData {
    files_list: Vec<Arc<File>>,
    page2file: Vec<Arc<File>>,
    name2file: HashMap<String, Arc<File>>,
    id2file: HashMap<String, Arc<File>>,
}

impl Clone for DjVmDir {
    fn clone(&self) -> Self {
        DjVmDir {
            data: Mutex::new(self.data.lock().unwrap().clone()),
        }
    }
}

impl DjVmDir {
    const VERSION: u8 = 1;

    pub fn new() -> Arc<Self> {
        Arc::new(DjVmDir {
            data: Mutex::new(DjVmDirData::default()),
        })
    }

    pub fn get_files_list(&self) -> Vec<Arc<File>> {
        self.data.lock().unwrap().files_list.clone()
    }

    pub fn get_files_ids(&self) -> Vec<String> {
        self.data
            .lock()
            .unwrap()
            .files_list
            .iter()
            .map(|f| f.id.clone())
            .collect()
    }

    pub fn get_pages_num(&self) -> usize {
        self.data.lock().unwrap().page2file.len()
    }

    pub fn get_shared_anno_file(&self) -> Option<Arc<File>> {
        self.data
            .lock()
            .unwrap()
            .files_list
            .iter()
            .find(|f| f.is_shared_anno())
            .cloned()
    }

    pub fn set_file_title(&self, id: &str, title: &str) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        if let Some(file) = data.id2file.get_mut(id) {
            Arc::get_mut(file).unwrap().set_title(title);
            Ok(())
        } else {
            Err(DjvuError::InvalidArg(format!("File not found: {}", id)))
        }
    }

    pub fn add_file(&self, file: Arc<File>) {
        let mut data = self.data.lock().unwrap();
        let file_id = file.id.clone();
        let file_name = file.name.clone();

        data.files_list.push(Arc::clone(&file));
        data.id2file.insert(file_id, Arc::clone(&file));
        data.name2file.insert(file_name, Arc::clone(&file));

        if file.is_page() {
            let page_num = data.page2file.len() as i32;
            // Safely get the last file and set its page number
            if let Some(last_file) = data.files_list.last_mut() {
                if let Some(file_mut) = Arc::get_mut(last_file) {
                    file_mut.page_num = page_num;
                }
            }
            data.page2file.push(file);
        }
    }

    pub fn remove_file(&self, id: &str) -> Option<Arc<File>> {
        let mut data = self.data.lock().unwrap();
        if let Some(file) = data.id2file.remove(id) {
            data.name2file.remove(&file.name);
            data.files_list.retain(|f| f.id != id);
            if file.is_page() {
                data.page2file.retain(|f| f.id != id);
                // Re-number pages
                for (i, page_file) in data.page2file.iter_mut().enumerate() {
                    Arc::get_mut(page_file).unwrap().page_num = i as i32;
                }
            }
            Some(file)
        } else {
            None
        }
    }

    pub fn move_file_to_page_pos(&self, id: &str, new_pos: usize) -> Result<()> {
        let mut data = self.data.lock().unwrap();

        let file_idx = data
            .files_list
            .iter()
            .position(|f| f.id == id)
            .ok_or_else(|| DjvuError::Stream(format!("File not found: {}", id)))?;
        let file = data.files_list.remove(file_idx);

        if !file.is_page() {
            data.files_list.insert(file_idx, file); // Put it back if not a page
            return Err(DjvuError::Stream(format!(
                "File with ID {} is not a page and cannot be moved in page list.",
                id
            )));
        }

        // Remove from page2file and re-insert at new_pos
        let old_page_pos = data
            .page2file
            .iter()
            .position(|f| Arc::ptr_eq(f, &file))
            .unwrap();
        data.page2file.remove(old_page_pos);

        let new_pos = new_pos.min(data.page2file.len());
        data.page2file.insert(new_pos, Arc::clone(&file));

        // Update page_num for all affected pages
        for i in 0..data.page2file.len() {
            Arc::get_mut(&mut data.page2file[i]).unwrap().page_num = i as i32;
        }

        // Re-insert into files_list at an appropriate position (e.g., after other pages)
        // This part might need more sophisticated logic depending on how files_list is used.
        // For now, let's just re-insert it at the end of the page section.
        let last_page_idx = data
            .files_list
            .iter()
            .rposition(|f| f.is_page())
            .map_or(0, |idx| idx + 1);
        data.files_list.insert(last_page_idx, file);

        Ok(())
    }

    pub fn encode_explicit(
        &self,
        stream: &mut dyn ByteStream,
        bundled: bool,
        _do_rename: bool,
    ) -> Result<()> {
        let data = self.data.lock().unwrap();

        // Write unencoded header
        stream.write_u8(Self::VERSION | if bundled { 0x80 } else { 0 })?;
        stream.write_u16(data.files_list.len() as u16)?;

        if data.files_list.is_empty() {
            return Ok(());
        }

        // Write offsets (unencoded, only for bundled documents)
        if bundled {
            for file in &data.files_list {
                stream.write_u32(file.offset)?;
            }
        }

        // Prepare BZZ-encoded data according to DjVu spec
        let mut bzz_buffer = MemoryStream::new();

        // 1. Write sizes (3 bytes each, as INT24)
        for file in &data.files_list {
            // Write size as 3-byte big-endian integer (INT24)
            let size = file.size;
            ByteStream::write_u8(&mut bzz_buffer, (size >> 16) as u8)?;
            ByteStream::write_u8(&mut bzz_buffer, (size >> 8) as u8)?;
            ByteStream::write_u8(&mut bzz_buffer, size as u8)?;
        }

        // 2. Write flags (1 byte each)
        for file in &data.files_list {
            let flags = match file.file_type {
                FileType::Page => 0x01,
                FileType::Include => 0x00,
                FileType::Thumbnails => 0x02,
                FileType::SharedAnno => 0x03,
            };
            ByteStream::write_u8(&mut bzz_buffer, flags)?;
        }

        // 3. Write zero-terminated IDs
        for file in &data.files_list {
            bzz_buffer.write_all(file.id.as_bytes())?;
            ByteStream::write_u8(&mut bzz_buffer, 0)?; // Null terminator
        }

        // Use proper BZZ compression for the DIRM data according to DjVu spec
        let compressed = bzz_compress(bzz_buffer.as_slice(), 50)?; // 50KB block size for small DIRM

        stream.write_all(&compressed)?;

        Ok(())
    }

    pub fn encode(&self, stream: &mut dyn ByteStream, do_rename: bool) -> Result<()> {
        let data = self.data.lock().unwrap();
        let bundled = data.files_list.iter().all(|f| f.offset > 0);
        if data.files_list.iter().any(|f| (f.offset > 0) != bundled) {
            return Err(DjvuError::Stream(
                "Mixed bundled and indirect records".into(),
            ));
        }
        self.encode_explicit(stream, bundled, do_rename)
    }

    pub fn page_to_id(&self, page_num: i32) -> Option<PageId> {
        if page_num < 0 {
            return None;
        }
        let data = self.data.lock().unwrap();
        if page_num as usize >= data.page2file.len() {
            return None;
        }
        Some(data.page2file[page_num as usize].id.clone())
    }

    pub fn page_to_file(&self, page_num: i32) -> Result<Arc<File>> {
        let page_id = self.page_to_id(page_num).ok_or_else(|| {
            DjvuError::InvalidOperation(format!("Page number {} not found", page_num))
        })?;

        let data = self.data.lock().unwrap();
        data.id2file.get(&page_id).cloned().ok_or_else(|| {
            DjvuError::InvalidOperation(format!("File for page {} not found", page_num))
        })
    }

    pub fn pos_to_file(&self, fileno: i32) -> Option<(Arc<File>, Option<i32>)> {
        let data = self.data.lock().unwrap();
        if fileno < 0 || fileno as usize >= data.files_list.len() {
            return None;
        }
        let mut pageno = 0;
        for (i, file) in data.files_list.iter().enumerate() {
            if i == fileno as usize {
                return Some((
                    Arc::clone(file),
                    if file.is_page() { Some(pageno) } else { None },
                ));
            }
            if file.is_page() {
                pageno += 1;
            }
        }
        None
    }

    /// Gets the position of a file in the files list
    pub fn get_file_pos(&self, file: &File) -> Option<usize> {
        let data = self.data.lock().unwrap();
        data.files_list
            .iter()
            .position(|f| Arc::ptr_eq(f, &Arc::new(file.clone())))
    }

    pub fn get_page_pos(&self, page_num: i32) -> Option<usize> {
        let file = self.page_to_file(page_num).ok()?;
        self.get_file_pos(&file)
    }
    /// Deletes a file by ID
    pub fn delete_file(&self, id: &str) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        if let Some(pos) = data.files_list.iter().position(|f| f.id == id) {
            let file = data.files_list.remove(pos);
            data.name2file.remove(&file.name);
            data.id2file.remove(&file.id);
            if file.is_page() {
                if let Some(page_pos) = data.page2file.iter().position(|f| Arc::ptr_eq(f, &file)) {
                    data.page2file.remove(page_pos);
                    for i in page_pos..data.page2file.len() {
                        Arc::get_mut(&mut data.page2file[i]).unwrap().page_num = i as i32;
                    }
                }
            }
            Ok(())
        } else {
            Err(DjvuError::Stream(format!("File not found: {}", id)))
        }
    }

    // Second implementation of move_file_to_page_pos removed to fix duplicate function error

    /// Resolves duplicate file names in the directory
    pub fn resolve_duplicates(&self, _save_names_only: bool) -> Vec<Arc<File>> {
        let data = self.data.lock().unwrap();
        let mut result = Vec::new();

        for file in &data.files_list {
            // Create a new File with the same properties
            let new_file = File {
                id: file.id.clone(),
                name: file.name.clone(),
                title: file.title.clone(),
                file_type: file.file_type.clone(),
                size: file.size,
                offset: file.offset,
                has_name: file.has_name,
                has_title: file.has_title,
                page_num: file.page_num,
                valid_name: file.valid_name,
                oldname: file.oldname.clone(),
            };

            // Create a new Arc with the new File
            let new_arc = Arc::new(new_file);

            // Now we can add the Arc to our result
            result.push(new_arc);
        }

        // Note: This implementation doesn't actually check for duplicates
        // You'll need to implement that logic separately
        result
    }

    /// Gets a file by its ID
    pub fn get_file_by_id(&self, id: &str) -> Option<Arc<File>> {
        let data = self.data.lock().unwrap();
        data.id2file.get(id).cloned()
    }

    /// Inserts a file at a specific position
    pub fn insert_file(&self, file: Arc<File>, pos: i32) -> Result<()> {
        let mut data = self.data.lock().unwrap();

        // Check if file already exists
        if data.id2file.contains_key(&file.id) {
            return Err(DjvuError::InvalidOperation(format!(
                "File with ID '{}' already exists",
                file.id
            )));
        }

        // Insert file in files_list at position or at the end if pos is -1
        let insert_pos = if pos < 0 {
            data.files_list.len()
        } else {
            pos.min(data.files_list.len() as i32) as usize
        };

        data.files_list.insert(insert_pos, Arc::clone(&file));
        data.id2file.insert(file.id.clone(), Arc::clone(&file));
        data.name2file.insert(file.name.clone(), Arc::clone(&file));

        // If it's a page, add it to page2file
        if file.is_page() {
            let page_num = data.page2file.len() as i32;
            // We need to update the page_num, but since we have multiple Arc references,
            // we need to make a mutable copy first
            let mut file_copy = (*file).clone();
            file_copy.page_num = page_num;
            let file_arc = Arc::new(file_copy);

            // Update all the references with the corrected page_num
            let insert_pos = data.files_list.len() - 1; // Last inserted position
            data.files_list[insert_pos] = Arc::clone(&file_arc);
            data.id2file.insert(file.id.clone(), Arc::clone(&file_arc));
            data.name2file
                .insert(file.name.clone(), Arc::clone(&file_arc));

            data.page2file.push(file_arc);
        }

        Ok(())
    }

    /// Clone the directory with new offsets for files
    pub fn clone_with_new_offsets(&self, file_offsets: &HashMap<String, u32>) -> Arc<Self> {
        // Create a new DjVmDir instance
        let new_dir = DjVmDir::new();

        // Get the current data
        let data = self.data.lock().unwrap();

        // Copy all files with updated offsets
        for file in &data.files_list {
            // Create a new File with the same properties but potentially updated offset
            let new_offset = file_offsets.get(&file.id).copied().unwrap_or(file.offset);

            let new_file = File {
                id: file.id.clone(),
                name: file.name.clone(),
                title: file.title.clone(),
                file_type: file.file_type.clone(),
                size: file.size,
                offset: new_offset,
                has_name: file.has_name,
                has_title: file.has_title,
                page_num: file.page_num,
                valid_name: file.valid_name,
                oldname: file.oldname.clone(),
            };

            // Add the new file to the new directory
            new_dir.add_file(Arc::new(new_file));
        }

        new_dir
    }
}

/// Directory for an older DjVu all-in-one-file format (DIR0 chunk)
pub struct DjVmDir0 {
    name2file: HashMap<String, Arc<FileRec>>,
    num2file: Vec<Arc<FileRec>>,
}

#[derive(Debug, Clone)]
pub struct FileRec {
    pub name: String,
    pub iff_file: bool,
    pub offset: u32,
    pub size: u32,
}

impl FileRec {
    pub fn new(name: &str, iff_file: bool, offset: u32, size: u32) -> Arc<Self> {
        Arc::new(FileRec {
            name: name.to_string(),
            iff_file,
            offset,
            size,
        })
    }
}

impl DjVmDir0 {
    /// Creates a new DjVmDir0 instance
    pub fn new() -> Arc<Self> {
        Arc::new(DjVmDir0 {
            name2file: HashMap::new(),
            num2file: Vec::new(),
        })
    }

    /// Calculates the encoded size of the directory
    pub fn get_size(&self) -> usize {
        2 + self
            .num2file
            .iter()
            .map(|f| f.name.len() + 1 + 1 + 4 + 4)
            .sum::<usize>()
    }

    /// Encodes the directory to a ByteStream
    pub fn encode(&self, stream: &mut dyn ByteStream) -> Result<()> {
        stream.write_u16(self.num2file.len() as u16)?;
        for file in &self.num2file {
            stream.write_all(file.name.as_bytes())?;
            stream.write_u8(0)?; // Null terminator
            stream.write_u8(if file.iff_file { 1 } else { 0 })?;
            stream.write_u32(file.offset)?;
            stream.write_u32(file.size)?;
        }
        Ok(())
    }

    /// Decodes the directory from a ByteStream
    pub fn decode(&mut self, stream: &mut dyn ByteStream) -> Result<()> {
        self.name2file.clear();
        self.num2file.clear();

        let count = stream.read_u16()?;
        for _ in 0..count {
            let mut name = String::new();
            let mut byte = stream.read_u8()?;
            while byte != 0 {
                name.push(byte as char);
                byte = stream.read_u8()?;
            }
            let iff_file = stream.read_u8()? != 0;
            let offset = stream.read_u32()?;
            let size = stream.read_u32()?;
            self.add_file(&name, iff_file, offset, size)?;
        }
        Ok(())
    }

    /// Retrieves a file by name
    pub fn get_file_by_name(&self, name: &str) -> Option<Arc<FileRec>> {
        self.name2file.get(name).cloned()
    }

    /// Retrieves a file by index
    pub fn get_file_by_num(&self, file_num: usize) -> Option<Arc<FileRec>> {
        self.num2file.get(file_num).cloned()
    }

    /// Adds a file to the directory
    pub fn add_file(&mut self, name: &str, iff_file: bool, offset: u32, size: u32) -> Result<()> {
        if name.contains('/') {
            return Err(DjvuError::Stream("File name cannot contain slashes".into()));
        }
        let file = FileRec::new(name, iff_file, offset, size);
        self.name2file.insert(name.to_string(), Arc::clone(&file));
        self.num2file.push(file);
        Ok(())
    }
}

// Navigation/bookmark structures (previously in djvu_nav.rs)

/// Represents a single bookmark entry.
#[derive(Debug, Clone)]
pub struct Bookmark {
    pub title: String,
    /// Destination URL, typically a page ID like "#1".
    pub dest: String,
    /// Nested bookmarks.
    pub children: Vec<Bookmark>,
}

/// Represents the entire navigation/bookmark structure (`NAVM` chunk).
#[derive(Debug, Clone, Default)]
pub struct DjVmNav {
    pub bookmarks: Vec<Bookmark>,
}

impl DjVmNav {
    /// Creates a new, empty navigation structure.
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts total number of bookmarks in the tree (including nested)
    fn count_bookmarks(&self) -> u16 {
        fn count_recursive(bookmarks: &[Bookmark]) -> u16 {
            let mut count = bookmarks.len() as u16;
            for b in bookmarks {
                count += count_recursive(&b.children);
            }
            count
        }
        count_recursive(&self.bookmarks)
    }

    /// Writes a 24-bit big-endian integer
    fn write_int24<W: std::io::Write>(writer: &mut W, value: u32) -> std::io::Result<()> {
        // INT24 is 3 bytes big-endian
        writer.write_all(&[(value >> 16) as u8, (value >> 8) as u8, value as u8])
    }

    /// Encodes the navigation data into the binary format required for a `NAVM` chunk.
    /// Format: UINT16 count, then for each bookmark: BYTE nChildren, INT24 nDesc, UTF8 sDesc, INT24 nURL, UTF8 sURL
    pub fn encode<W: std::io::Write>(&self, writer: &mut W) -> Result<()> {
        use byteorder::{BigEndian, WriteBytesExt};

        if self.bookmarks.is_empty() {
            return Ok(());
        }

        // Write total bookmark count (UINT16 big-endian)
        let total = self.count_bookmarks();
        writer.write_u16::<BigEndian>(total)?;

        // Write each bookmark recursively
        for bookmark in &self.bookmarks {
            self.encode_bookmark_binary(bookmark, writer)?;
        }

        Ok(())
    }

    fn encode_bookmark_binary<W: std::io::Write>(
        &self,
        bookmark: &Bookmark,
        writer: &mut W,
    ) -> Result<()> {
        // nChildren: BYTE (number of immediate children)
        writer.write_all(&[bookmark.children.len() as u8])?;

        // nDesc: INT24 (size of description/title)
        let title_bytes = bookmark.title.as_bytes();
        Self::write_int24(writer, title_bytes.len() as u32)?;

        // sDesc: UTF8 string (the title)
        writer.write_all(title_bytes)?;

        // nURL: INT24 (size of URL)
        let url_bytes = bookmark.dest.as_bytes();
        Self::write_int24(writer, url_bytes.len() as u32)?;

        // sURL: UTF8 string (the URL/destination)
        writer.write_all(url_bytes)?;

        // Recursively encode children
        for child in &bookmark.children {
            self.encode_bookmark_binary(child, writer)?;
        }

        Ok(())
    }
}
