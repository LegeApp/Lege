use crate::doc::djvu_dir::{DjVmDir, File as DjVuFile, FileType};
use crate::doc::djvu_document::DjVuDocument;
use crate::encode::jb2::symbol_dict::SymDictBuilder;
use crate::iff::chunk_tree::{ChunkPayload, IffChunk, IffDocument};
use crate::iff::data_pool::DataPool;
use crate::iff::iff::{IffReaderExt, IffWriter};
use crate::utils::error::Result;
use crate::DjvuError;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};

/// Structure to represent file dependency relationships
pub struct RefMap {
    pub parents: HashMap<String, HashSet<String>>,
    pub children: HashMap<String, HashSet<String>>,
}

pub struct DjVuDocEditor {
    doc: DjVuDocument,
    chunk_tree: Option<IffDocument>,
}

impl DjVuDocEditor {
    pub fn build_chunk_tree_from_doc(&mut self) -> Result<()> {
        use crate::iff::chunk_tree::file_type_to_id;
        let mut root = IffChunk::new_composite(*b"FORM", *b"DJVM");
        let mut children = Vec::new();

        let mut dirm_stream = crate::iff::byte_stream::MemoryStream::new();
        self.doc
            .dir()
            .encode_explicit(&mut dirm_stream, true, true)?;
        let dirm_bytes = dirm_stream.into_vec();
        let dirm_chunk = IffChunk::new_raw(*b"DIRM", dirm_bytes);
        children.push(dirm_chunk);

        let files_list = self.doc.dir().get_files_list();
        for file in &files_list {
            let chunk_id = file_type_to_id(file.file_type);
            let payload = self
                .doc
                .data
                .get(&file.id)
                .ok_or_else(|| DjvuError::Stream(format!("No page at position {}", file.id)))?;
            let chunk = IffChunk::new_raw(chunk_id, payload.to_vec()?);
            children.push(chunk);
        }

        if let Some(nav) = self.doc.nav() {
            let mut nav_bytes = Vec::new();
            nav.encode(&mut Cursor::new(&mut nav_bytes))?;
            let navm_chunk = IffChunk::new_raw(*b"NAVM", nav_bytes);
            children.push(navm_chunk);
        }

        if let ChunkPayload::Composite {
            children: ref mut root_children,
            ..
        } = root.payload
        {
            *root_children = children;
        }
        self.chunk_tree = Some(IffDocument::new(root));

        debug_assert!(
            {
                let tree = self.chunk_tree.as_ref().unwrap();
                if let ChunkPayload::Composite { children, .. } = &tree.root.payload {
                    !children.is_empty() && children[0].id == *b"DIRM"
                } else {
                    false
                }
            },
            "Chunk tree root must have DIRM as first child"
        );
        Ok(())
    }

    pub fn chunk_tree_mut(&mut self) -> Result<&mut IffDocument> {
        if self.chunk_tree.is_none() {
            self.build_chunk_tree_from_doc()?;
        }
        Ok(self.chunk_tree.as_mut().unwrap())
    }

    pub fn insert_chunk_at_root(&mut self, chunk: IffChunk) -> Result<()> {
        let tree = self.chunk_tree_mut()?;
        if let ChunkPayload::Composite { children, .. } = &mut tree.root.payload {
            children.push(chunk);
        }
        Ok(())
    }

    pub fn remove_chunk_by_id(&mut self, id: [u8; 4]) -> Result<()> {
        let tree = self.chunk_tree_mut()?;
        if let ChunkPayload::Composite { children, .. } = &mut tree.root.payload {
            children.retain(|c| c.id != id);
        }
        Ok(())
    }

    pub fn rebuild_doc_from_chunk_tree(&mut self) -> Result<()> {
        // TODO: Implement if needed
        Ok(())
    }

    pub fn insert_page_from_components(
        &mut self,
        _page_id: String,
        components: crate::doc::page_encoder::PageComponents,
        params: crate::doc::page_encoder::PageEncodeParams,
    ) -> Result<()> {
        let page_num = (self.doc.dir().get_pages_num() + 1) as u32;
        let dpm = (params.dpi * 100 / 254) as u32; // Convert DPI to DPM (dots per meter)
        let (width, height) = components.dimensions();
        let rotation = if width >= height { 1 } else { 1 };
        let gamma = Some(2.2);
        let encoded = components.encode(&params, page_num as u32, dpm, rotation, gamma)?;
        let mut cursor = Cursor::new(encoded);
        let chunk = if let Some(chunk) = cursor.next_chunk()? {
            let data = cursor.get_chunk_data(&chunk)?;
            IffChunk::new_raw(chunk.id, data)
        } else {
            return Err(DjvuError::Stream("Page number out of bounds".to_string()));
        };
        let tree = self.chunk_tree_mut()?;
        if let ChunkPayload::Composite { children, .. } = &mut tree.root.payload {
            children.push(chunk);
        }
        Ok(())
    }

    pub fn write_bundled<W: Write + Seek>(&mut self, writer: W) -> Result<()> {
        let tree = self.chunk_tree_mut()?;
        tree.write(writer)
    }

    pub fn insert_dirm_chunk(&mut self, djvm_dir: &DjVmDir) -> Result<()> {
        let mut dirm_buf = Vec::new();
        djvm_dir.encode(&mut Cursor::new(&mut dirm_buf), false)?;
        let dirm_chunk = IffChunk::new_raw(*b"DIRM", dirm_buf);
        let tree = self.chunk_tree_mut()?;
        if let ChunkPayload::Composite { children, .. } = &mut tree.root.payload {
            children.retain(|c| c.id != *b"DIRM");
            children.insert(0, dirm_chunk);
        }
        Ok(())
    }

    pub fn new() -> Result<Self> {
        let doc = DjVuDocument::new();
        // Create a new symbol dictionary builder for shared symbols with default max error of 0
        let _symbol_dict_builder = SymDictBuilder::new(0);
        // The symbol dictionary will be used when encoding pages
        Ok(DjVuDocEditor {
            doc,
            chunk_tree: None,
        })
    }

    pub fn build(self) -> DjVuDocument {
        self.doc
    }

    pub fn insert_page(
        &mut self,
        page_id: &str,
        page_data: Vec<u8>,
        get_include_data: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
    ) -> Result<()> {
        let mut work_queue: VecDeque<(String, DataPool)> = VecDeque::new();
        work_queue.push_back((page_id.to_string(), DataPool::from_vec(page_data)));

        let mut processed_ids = HashSet::new();

        while let Some((current_id, current_data)) = work_queue.pop_front() {
            if self.doc.has_file_with_id(&current_id) {
                continue;
            }
            processed_ids.insert(current_id.to_string());

            let file = DjVuFile::new(&current_id, &current_id, "", FileType::Page);
            let included_ids = self.parse_included_ids(&current_data.to_vec()?)?;
            self.doc.insert_file(file, current_data)?;

            for id in included_ids.into_iter() {
                let id_str = id.to_string();
                if !self.doc.has_file_with_id(&id_str) && !processed_ids.contains(&id_str) {
                    let include_data = get_include_data(&id_str)?;
                    work_queue.push_back((id_str, DataPool::from_vec(include_data)));
                }
            }
        }
        Ok(())
    }

    pub fn build_ref_map(&mut self) -> Result<RefMap> {
        let mut parents: HashMap<String, HashSet<String>> = HashMap::new();
        let mut children: HashMap<String, HashSet<String>> = HashMap::new();

        // Process each file in the document
        for file_id in self.doc.dir().get_files_ids() {
            // Get the file's data
            if let Some(data) = self.doc.data.get(&file_id) {
                // Create a reader to parse the file
                let mut reader = data.clone();

                // Extract included file IDs
                let included_ids = self.parse_included_ids_from_reader(&mut reader)?;

                // Update the reference maps
                for included_id in included_ids {
                    // Add to parents map
                    parents
                        .entry(included_id.clone())
                        .or_insert_with(HashSet::new)
                        .insert(file_id.clone());

                    // Add to children map
                    children
                        .entry(file_id.clone())
                        .or_insert_with(HashSet::new)
                        .insert(included_id);
                }
            }
        }

        Ok(RefMap { parents, children })
    }

    pub fn remove_page(&mut self, page_num: i32, remove_unreferenced: bool) -> Result<()> {
        let page_id = self
            .doc
            .dir()
            .page_to_id(page_num)
            .ok_or_else(|| DjvuError::InvalidArg(format!("Page {} not found", page_num)))?;
        let mut to_remove: VecDeque<String> = VecDeque::new();
        to_remove.push_back(page_id);

        if !remove_unreferenced {
            self.doc.remove_file(&to_remove[0]);
            return Ok(());
        }

        let ref_map = self.build_ref_map()?;
        let mut parents = ref_map.parents;
        let mut children = ref_map.children;

        while let Some(id_to_remove) = to_remove.pop_front() {
            if let Some(child_ids) = children.remove(&id_to_remove) {
                for child_id in child_ids {
                    if let Some(parent_set) = parents.get_mut(&child_id) {
                        parent_set.remove(&id_to_remove);
                        if parent_set.is_empty() {
                            to_remove.push_back(child_id.clone());
                        }
                    }
                }
            }
            self.doc.remove_file(&id_to_remove);
        }
        Ok(())
    }

    pub fn move_page(&mut self, from_page_num: i32, to_page_num: i32) -> Result<()> {
        let id = self.doc.dir().page_to_id(from_page_num).ok_or_else(|| {
            DjvuError::InvalidArg(format!("Source page {} not found", from_page_num))
        })?;
        self.move_page_by_id(&id, to_page_num)
    }

    pub fn set_page_title(&mut self, page_num: i32, title: &str) -> Result<()> {
        let page_id = self
            .doc
            .dir()
            .page_to_id(page_num)
            .ok_or_else(|| DjvuError::InvalidArg(format!("Page {} not found", page_num)))?;
        self.doc.dir().set_file_title(&page_id, title)
    }

    pub fn create_shared_anno_file(&mut self) -> Result<()> {
        if self.doc.dir().get_shared_anno_file().is_some() {
            return Err(DjvuError::InvalidOperation(
                "Shared annotation file already exists.".to_string(),
            ));
        }

        let mut buffer = Vec::new();
        {
            let mut iff_writer = IffWriter::new(Cursor::new(&mut buffer));
            iff_writer.put_chunk("FORM:DJVI")?;
            iff_writer.close_chunk()?;
        }

        let file = DjVuFile::new(
            "shared_anno.iff",
            "shared_anno.iff",
            "Shared Annotations",
            FileType::SharedAnno,
        );

        self.doc
            .insert_file(file.into(), DataPool::from_vec(buffer))
    }

    /// Move a page by its ID to a new position
    pub fn move_page_by_id(&mut self, id: &str, to_page_num: i32) -> Result<()> {
        self.doc
            .dir()
            .move_file_to_page_pos(id, to_page_num as usize)
    }

    /// Parse included file IDs from raw data
    fn parse_included_ids(&self, data: &[u8]) -> Result<Vec<String>> {
        let mut cursor = std::io::Cursor::new(data);
        self.parse_included_ids_from_reader(&mut cursor)
    }

    /// Parse included IDs from a reader
    fn parse_included_ids_from_reader<R: Read + Seek>(
        &self,
        reader: &mut R,
    ) -> Result<Vec<String>> {
        let mut included_ids = Vec::new();
        reader.seek(SeekFrom::Start(0))?;

        while let Some(chunk) = reader.next_chunk()? {
            if chunk.id == *b"INCL" {
                let data = reader.get_chunk_data(&chunk)?;
                let id = String::from_utf8(data).map_err(|e| {
                    DjvuError::ValidationError(format!("INCL chunk contains invalid UTF-8: {}", e))
                })?;
                included_ids.push(id.trim_end_matches('\0').to_string());
            }
        }

        Ok(included_ids)
    }
}
