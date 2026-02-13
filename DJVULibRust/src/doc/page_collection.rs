use crate::doc::djvu_dir::DjVmNav;
use crate::doc::page_encoder::{EncodedPage, PageComponents, PageEncodeParams};
use crate::{DjvuError, Result};
use std::collections::{hash_map::Entry, HashMap};
use std::sync::{Arc, RwLock};

#[derive(Clone)]
enum PageSlot {
    Pending,
    Ready(Arc<Vec<u8>>),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(test)]
    use image::RgbImage;

    #[test]
    fn document_builder_outputs_djvu_document() -> Result<()> {
        let builder = DocumentBuilder::new(1);
        let page = PageComponents::new()
            .with_background(RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0])))?;
        builder.encode_and_insert(0, page)?;

        let document = builder.build_document()?;
        assert!(document.has_file_with_id("p0001.djvu"));
        Ok(())
    }
}

pub struct PageCollection {
    slots: RwLock<Vec<PageSlot>>,
    metadata: RwLock<HashMap<usize, PageMetadata>>,
    total_pages: usize,
}

#[derive(Clone)]
pub struct PageMetadata {
    width: u32,
    height: u32,
    id: Option<String>,
}

impl PageCollection {
    pub fn new(total_pages: usize) -> Self {
        let slots = vec![PageSlot::Pending; total_pages];
        Self {
            slots: RwLock::new(slots),
            metadata: RwLock::new(HashMap::new()),
            total_pages,
        }
    }

    pub fn len(&self) -> usize {
        self.total_pages
    }

    pub fn is_empty(&self) -> bool {
        self.total_pages == 0
    }

    pub fn insert_page(&self, page_num: usize, page: EncodedPage) -> Result<()> {
        if page_num >= self.total_pages {
            return Err(DjvuError::InvalidOperation(format!(
                "Page number {} exceeds total pages {}",
                page_num, self.total_pages
            )));
        }

        {
            let mut slots = self.slots.write().unwrap();
            match &slots[page_num] {
                PageSlot::Ready(_) => {
                    return Err(DjvuError::InvalidOperation(format!(
                        "Page {} already exists",
                        page_num
                    )));
                }
                PageSlot::Pending => {
                    slots[page_num] = PageSlot::Ready(Arc::clone(&page.data));
                }
            }
        }

        {
            let mut metadata = self.metadata.write().unwrap();
            match metadata.entry(page_num) {
                Entry::Occupied(mut entry) => {
                    let meta = entry.get_mut();
                    meta.width = page.width;
                    meta.height = page.height;
                }
                Entry::Vacant(entry) => {
                    entry.insert(PageMetadata {
                        width: page.width,
                        height: page.height,
                        id: None,
                    });
                }
            }
        }

        Ok(())
    }

    pub fn is_page_ready(&self, page_num: usize) -> bool {
        if page_num >= self.total_pages {
            return false;
        }

        let slots = self.slots.read().unwrap();
        matches!(slots[page_num], PageSlot::Ready(_))
    }

    pub fn is_complete(&self) -> bool {
        let slots = self.slots.read().unwrap();
        slots.iter().all(|slot| matches!(slot, PageSlot::Ready(_)))
    }

    pub fn ready_count(&self) -> usize {
        let slots = self.slots.read().unwrap();
        slots
            .iter()
            .filter(|slot| matches!(slot, PageSlot::Ready(_)))
            .count()
    }

    pub fn get_page(&self, page_num: usize) -> Option<Arc<Vec<u8>>> {
        if page_num >= self.total_pages {
            return None;
        }

        let slots = self.slots.read().unwrap();
        match &slots[page_num] {
            PageSlot::Ready(data) => Some(Arc::clone(data)),
            PageSlot::Pending => None,
        }
    }

    pub fn collect_all(&self) -> Option<Vec<Arc<Vec<u8>>>> {
        let slots = self.slots.read().unwrap();

        let mut pages = Vec::with_capacity(self.total_pages);
        for slot in slots.iter() {
            match slot {
                PageSlot::Ready(data) => pages.push(Arc::clone(data)),
                PageSlot::Pending => return None,
            }
        }

        Some(pages)
    }

    pub fn get_metadata(&self, page_num: usize) -> Option<(u32, u32)> {
        let metadata = self.metadata.read().unwrap();
        metadata.get(&page_num).map(|m| (m.width, m.height))
    }

    pub fn set_page_id(&self, page_num: usize, id: String) -> Result<()> {
        if page_num >= self.total_pages {
            return Err(DjvuError::InvalidOperation(format!(
                "Page number {} exceeds total pages {}",
                page_num, self.total_pages
            )));
        }

        let mut metadata = self.metadata.write().unwrap();
        match metadata.entry(page_num) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().id = Some(id);
            }
            Entry::Vacant(entry) => {
                entry.insert(PageMetadata {
                    width: 0,
                    height: 0,
                    id: Some(id),
                });
            }
        }

        Ok(())
    }

    pub fn metadata_for(&self, page_num: usize) -> Option<PageMetadata> {
        let metadata = self.metadata.read().unwrap();
        metadata.get(&page_num).cloned()
    }
}

pub struct DocumentBuilder {
    pages: Arc<PageCollection>,
    params: PageEncodeParams,
    dpi: u32,
    gamma: Option<f32>,
    nav: Option<DjVmNav>,
    metadata: HashMap<String, String>,
}

impl DocumentBuilder {
    pub fn new(total_pages: usize) -> Self {
        let params = PageEncodeParams::default();
        Self {
            pages: Arc::new(PageCollection::new(total_pages)),
            params,
            dpi: 300,
            gamma: Some(2.2),
            nav: None,
            metadata: HashMap::new(),
        }
    }

    pub fn with_params(mut self, params: PageEncodeParams) -> Self {
        self.dpi = params.dpi;
        self.params = params;
        self
    }

    pub fn with_dpi(mut self, dpi: u32) -> Self {
        self.dpi = dpi;
        self.params.dpi = dpi;
        self
    }

    pub fn with_gamma(mut self, gamma: Option<f32>) -> Self {
        self.gamma = gamma;
        self
    }

    pub fn pages(&self) -> Arc<PageCollection> {
        Arc::clone(&self.pages)
    }

    pub fn encode_and_insert(&self, page_num: usize, components: PageComponents) -> Result<()> {
        let encoded = EncodedPage::from_components(page_num, components, &self.params, self.dpi, self.gamma)?;
        self.pages.insert_page(page_num, encoded)
    }

    pub fn set_page_id(&self, page_num: usize, id: impl Into<String>) -> Result<()> {
        self.pages.set_page_id(page_num, id.into())
    }

    pub fn set_navigation(&mut self, nav: DjVmNav) {
        self.nav = Some(nav);
    }

    pub fn add_metadata(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.metadata.insert(key.into(), value.into());
    }

    pub fn status(&self) -> DocumentStatus {
        DocumentStatus {
            total_pages: self.pages.len(),
            ready_pages: self.pages.ready_count(),
            is_complete: self.pages.is_complete(),
        }
    }

    pub fn build(self) -> Result<Vec<Vec<u8>>> {
        let (pages, _) = Self::collect_pages(self.pages)?;
        Ok(pages)
    }

    fn collect_pages(pages: Arc<PageCollection>) -> Result<(Vec<Vec<u8>>, Vec<Option<String>>)> {
        let total_pages = pages.len();
        let identifiers: Vec<Option<String>> = (0..total_pages)
            .map(|idx| pages.metadata_for(idx).and_then(|meta| meta.id.clone()))
            .collect();

        let encoded = pages
            .collect_all()
            .ok_or_else(|| DjvuError::InvalidOperation("Not all pages ready".to_string()))?;

        // Drop the page slots so we can unwrap the Arc-backed page buffers.
        drop(pages);

        let page_data = encoded
            .into_iter()
            .map(|arc| Arc::try_unwrap(arc).unwrap_or_else(|a| (*a).clone()))
            .collect();

        Ok((page_data, identifiers))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DocumentStatus {
    pub total_pages: usize,
    pub ready_pages: usize,
    pub is_complete: bool,
}

impl DocumentStatus {
    pub fn progress(&self) -> f32 {
        if self.total_pages == 0 {
            1.0
        } else {
            self.ready_pages as f32 / self.total_pages as f32
        }
    }
}
