use crate::doc::djvu_dir::DjVmNav;
use crate::doc::page_encoder::{EncodedPage, PageComponents, PageEncodeParams};
use crate::{DjvuError, Result};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

enum PageSlot {
    Pending,
    Ready(Arc<Vec<u8>>),
}

/// Thread-safe, out-of-order page collection with per-slot locking.
///
/// Each page slot has its own `RwLock`, so concurrent insertions to different
/// pages never contend with each other.
pub struct PageCollection {
    slots: Vec<RwLock<PageSlot>>,
    metadata: Vec<RwLock<Option<PageMetadata>>>,
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
        let mut slots = Vec::with_capacity(total_pages);
        let mut metadata = Vec::with_capacity(total_pages);
        for _ in 0..total_pages {
            slots.push(RwLock::new(PageSlot::Pending));
            metadata.push(RwLock::new(None));
        }
        Self {
            slots,
            metadata,
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
            let mut slot = self.slots[page_num].write().unwrap();
            if matches!(*slot, PageSlot::Ready(_)) {
                return Err(DjvuError::InvalidOperation(format!(
                    "Page {} already exists",
                    page_num
                )));
            }
            *slot = PageSlot::Ready(Arc::clone(&page.data));
        }

        {
            let mut meta = self.metadata[page_num].write().unwrap();
            *meta = Some(PageMetadata {
                width: page.width,
                height: page.height,
                id: meta.as_ref().and_then(|m| m.id.clone()),
            });
        }

        Ok(())
    }

    pub fn is_page_ready(&self, page_num: usize) -> bool {
        if page_num >= self.total_pages {
            return false;
        }
        let slot = self.slots[page_num].read().unwrap();
        matches!(*slot, PageSlot::Ready(_))
    }

    pub fn is_complete(&self) -> bool {
        self.slots
            .iter()
            .all(|s| matches!(*s.read().unwrap(), PageSlot::Ready(_)))
    }

    pub fn ready_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| matches!(*s.read().unwrap(), PageSlot::Ready(_)))
            .count()
    }

    pub fn get_page(&self, page_num: usize) -> Option<Arc<Vec<u8>>> {
        if page_num >= self.total_pages {
            return None;
        }
        let slot = self.slots[page_num].read().unwrap();
        match &*slot {
            PageSlot::Ready(data) => Some(Arc::clone(data)),
            PageSlot::Pending => None,
        }
    }

    /// Collect all pages as `Arc` references (non-destructive).
    pub fn collect_all(&self) -> Option<Vec<Arc<Vec<u8>>>> {
        let mut pages = Vec::with_capacity(self.total_pages);
        for slot_lock in &self.slots {
            let slot = slot_lock.read().unwrap();
            match &*slot {
                PageSlot::Ready(data) => pages.push(Arc::clone(data)),
                PageSlot::Pending => return None,
            }
        }
        Some(pages)
    }

    /// Take all pages out of the collection, consuming the internal references.
    ///
    /// Each slot is swapped to `Pending`, dropping the collection's `Arc`
    /// reference. This guarantees `Arc::try_unwrap` succeeds on the returned
    /// values, avoiding deep clones during finalization.
    pub fn take_all(&self) -> Option<Vec<Vec<u8>>> {
        // Quick check: all slots must be Ready before we start swapping.
        for slot_lock in &self.slots {
            if !matches!(*slot_lock.read().unwrap(), PageSlot::Ready(_)) {
                return None;
            }
        }

        let mut pages = Vec::with_capacity(self.total_pages);
        for slot_lock in &self.slots {
            let mut slot = slot_lock.write().unwrap();
            if let PageSlot::Ready(data) = std::mem::replace(&mut *slot, PageSlot::Pending) {
                pages.push(Arc::try_unwrap(data).unwrap_or_else(|a| (*a).clone()));
            }
        }
        Some(pages)
    }

    pub fn get_metadata(&self, page_num: usize) -> Option<(u32, u32)> {
        if page_num >= self.total_pages {
            return None;
        }
        let meta = self.metadata[page_num].read().unwrap();
        meta.as_ref().map(|m| (m.width, m.height))
    }

    pub fn set_page_id(&self, page_num: usize, id: String) -> Result<()> {
        if page_num >= self.total_pages {
            return Err(DjvuError::InvalidOperation(format!(
                "Page number {} exceeds total pages {}",
                page_num, self.total_pages
            )));
        }

        let mut meta = self.metadata[page_num].write().unwrap();
        match meta.as_mut() {
            Some(m) => m.id = Some(id),
            None => {
                *meta = Some(PageMetadata {
                    width: 0,
                    height: 0,
                    id: Some(id),
                });
            }
        }
        Ok(())
    }

    pub fn metadata_for(&self, page_num: usize) -> Option<PageMetadata> {
        if page_num >= self.total_pages {
            return None;
        }
        let meta = self.metadata[page_num].read().unwrap();
        meta.clone()
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
        let encoded =
            EncodedPage::from_components(page_num, components, &self.params, self.dpi, self.gamma)?;
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

        let page_data = pages
            .take_all()
            .ok_or_else(|| DjvuError::InvalidOperation("Not all pages ready".to_string()))?;

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
