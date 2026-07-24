use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use crate::document::{CacheCategory, MemoryArbiter, MemoryLease, PageIndex};
use crate::geometry::RectF;

use super::TextSubstrate;

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub page: PageIndex,
    /// UTF-16 range in the native page text substrate.
    pub text_range: Range<usize>,
    /// Overlay rectangles in document space. The compact whole-document
    /// index does not retain geometry; visible-page geometry is attached by
    /// `SearchIndex::overlays_for_hit` when that page substrate is resident.
    pub overlays: Arc<[RectF]>,
}

#[derive(Debug, Clone)]
struct IndexedPage {
    utf16: Arc<[u16]>,
    _memory_lease: Option<MemoryLease>,
}

/// Compact whole-document index. It deliberately does not retain character
/// geometry or line boxes, because those are governed by the viewport text
/// cache. This preserves the blank-slate plan's single memory arbiter instead
/// of silently pinning every compiled page through search.
#[derive(Debug, Clone, Default)]
pub struct SearchIndex {
    pages: BTreeMap<PageIndex, IndexedPage>,
    memory: Option<MemoryArbiter>,
}

impl SearchIndex {
    pub fn with_memory(memory: MemoryArbiter) -> Self {
        Self {
            pages: BTreeMap::new(),
            memory: Some(memory),
        }
    }

    pub fn insert(&mut self, page: PageIndex, substrate: Arc<TextSubstrate>) {
        let utf16 = Arc::clone(&substrate.utf16);
        let lease = self.memory.as_ref().map(|memory| {
            memory.reserve(
                CacheCategory::Text,
                (utf16.len() * std::mem::size_of::<u16>()) as u64,
            )
        });
        self.pages.insert(
            page,
            IndexedPage {
                utf16,
                _memory_lease: lease,
            },
        );
    }

    pub fn remove(&mut self, page: PageIndex) {
        self.pages.remove(&page);
    }

    /// Incremental baseline search. It searches UTF-16 directly so hit
    /// offsets remain identical to the extraction API. Unicode case folding,
    /// token indexing, and disk-backed indexes belong behind this same API.
    pub fn search_exact(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        let needle: Vec<u16> = query.encode_utf16().collect();
        if needle.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for (&page, indexed) in &self.pages {
            for start in find_all(&indexed.utf16, &needle) {
                hits.push(SearchHit {
                    page,
                    text_range: start..start + needle.len(),
                    overlays: Arc::from([]),
                });
                if hits.len() == limit {
                    return hits;
                }
            }
        }
        hits
    }

    pub fn overlays_for_hit(
        hit: &SearchHit,
        substrate: &TextSubstrate,
    ) -> Arc<[RectF]> {
        substrate
            .characters
            .iter()
            .filter(|character| hit.text_range.contains(&character.char_index))
            .map(|character| character.bounds)
            .collect::<Vec<_>>()
            .into()
    }
}

fn find_all<'a>(haystack: &'a [u16], needle: &'a [u16]) -> impl Iterator<Item = usize> + 'a {
    haystack
        .windows(needle.len())
        .enumerate()
        .filter_map(move |(index, candidate)| (candidate == needle).then_some(index))
}
