use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::document::{CacheCategory, MemoryArbiter, MemoryLease, PageIndex};
use crate::document::{SessionUpdate, UpdateQueue};
use crate::geometry::RectF;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

use super::TextSubstrate;

const DEFAULT_RESIDENT_LIMIT: u64 = 64 * 1024 * 1024;

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
    text: IndexedText,
    _memory_lease: Option<MemoryLease>,
}

#[derive(Debug, Clone)]
enum IndexedText {
    Resident(Arc<[u16]>),
    Spilled {
        file: Arc<SpillFile>,
        offset: u64,
        len: usize,
    },
}

impl IndexedText {
    fn load(&self) -> Option<Arc<[u16]>> {
        match self {
            Self::Resident(text) => Some(Arc::clone(text)),
            Self::Spilled { file, offset, len } => file.read(*offset, *len),
        }
    }

    fn resident_bytes(&self) -> u64 {
        match self {
            Self::Resident(text) => (text.len() * std::mem::size_of::<u16>()) as u64,
            Self::Spilled { .. } => 0,
        }
    }
}

#[derive(Debug)]
struct SpillFile {
    file: Mutex<File>,
}

impl SpillFile {
    fn create() -> std::io::Result<Self> {
        Ok(Self {
            file: Mutex::new(tempfile::tempfile()?),
        })
    }

    fn append(&self, text: &[u16]) -> std::io::Result<u64> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let offset = file.seek(SeekFrom::End(0))?;
        let mut bytes = Vec::with_capacity(std::mem::size_of_val(text));
        for unit in text {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        file.write_all(&bytes)?;
        Ok(offset)
    }

    fn read(&self, offset: u64, len: usize) -> Option<Arc<[u16]>> {
        let byte_len = len.checked_mul(std::mem::size_of::<u16>())?;
        let mut bytes = vec![0_u8; byte_len];
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.seek(SeekFrom::Start(offset)).ok()?;
        file.read_exact(&mut bytes).ok()?;
        Some(
            bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>()
                .into(),
        )
    }
}

/// Compact whole-document index. It deliberately does not retain character
/// geometry or line boxes, because those are governed by the viewport text
/// cache. This preserves the blank-slate plan's single memory arbiter instead
/// of silently pinning every compiled page through search.
#[derive(Debug, Clone)]
pub struct SearchIndex {
    pages: BTreeMap<PageIndex, IndexedPage>,
    memory: Option<MemoryArbiter>,
    spill: Option<Arc<SpillFile>>,
    resident_bytes: u64,
    resident_limit: u64,
}

#[derive(Debug)]
enum SearchRequest {
    Query {
        request: u64,
        index_revision: u64,
        index: SearchIndex,
        query: String,
    },
    Shutdown,
}

#[derive(Debug)]
pub struct SearchService {
    requests: Sender<SearchRequest>,
    pending: Receiver<SearchRequest>,
    latest_request: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl SearchService {
    pub fn spawn(updates: Arc<UpdateQueue>) -> std::io::Result<Self> {
        // A query owns a snapshot of the whole index. Only the latest query is
        // useful, so retaining an unbounded sequence while the worker is busy
        // can waste substantial memory during rapid typing.
        let (requests, receiver) = bounded(1);
        let pending = receiver.clone();
        let latest_request = Arc::new(AtomicU64::new(0));
        let worker_latest = Arc::clone(&latest_request);
        let thread = std::thread::Builder::new()
            .name("lege-viewer-search".to_owned())
            .spawn(move || search_worker(receiver, worker_latest, updates))?;
        Ok(Self {
            requests,
            pending,
            latest_request,
            thread: Some(thread),
        })
    }

    pub fn submit(&self, request: u64, index_revision: u64, index: SearchIndex, query: String) {
        self.latest_request.store(request, Ordering::Release);
        self.send_latest(SearchRequest::Query {
            request,
            index_revision,
            index,
            query,
        });
    }

    pub fn cancel(&self, request: u64) {
        self.latest_request.store(request, Ordering::Release);
    }

    fn send_latest(&self, mut request: SearchRequest) {
        loop {
            match self.requests.try_send(request) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => return,
                Err(TrySendError::Full(returned)) => {
                    request = returned;
                    // The worker may win this race and empty the slot first;
                    // retrying then sends directly into the newly free slot.
                    let _ = self.pending.try_recv();
                }
            }
        }
    }
}

impl Drop for SearchService {
    fn drop(&mut self) {
        self.latest_request.store(u64::MAX, Ordering::Release);
        self.send_latest(SearchRequest::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl SearchIndex {
    pub fn with_memory(memory: MemoryArbiter) -> Self {
        Self {
            pages: BTreeMap::new(),
            memory: Some(memory),
            spill: None,
            resident_bytes: 0,
            resident_limit: DEFAULT_RESIDENT_LIMIT,
        }
    }

    pub fn insert(&mut self, page: PageIndex, substrate: Arc<TextSubstrate>) {
        self.insert_with_lease(page, substrate, None);
    }

    pub fn insert_with_lease(
        &mut self,
        page: PageIndex,
        substrate: Arc<TextSubstrate>,
        existing_lease: Option<MemoryLease>,
    ) {
        self.remove(page);
        let utf16 = Arc::clone(&substrate.utf16);
        let bytes = (utf16.len() * std::mem::size_of::<u16>()) as u64;
        if self.spill.is_none() && self.resident_bytes.saturating_add(bytes) > self.resident_limit {
            self.start_spilling();
        }
        if let Some(file) = &self.spill
            && let Ok(offset) = file.append(&utf16)
        {
            self.pages.insert(
                page,
                IndexedPage {
                    text: IndexedText::Spilled {
                        file: Arc::clone(file),
                        offset,
                        len: utf16.len(),
                    },
                    _memory_lease: None,
                },
            );
            return;
        }
        let lease = existing_lease.or_else(|| {
            self.memory
                .as_ref()
                .map(|memory| memory.reserve(CacheCategory::Text, bytes))
        });
        self.resident_bytes = self.resident_bytes.saturating_add(bytes);
        self.pages.insert(
            page,
            IndexedPage {
                text: IndexedText::Resident(utf16),
                _memory_lease: lease,
            },
        );
    }

    pub fn remove(&mut self, page: PageIndex) {
        if let Some(removed) = self.pages.remove(&page) {
            self.resident_bytes = self
                .resident_bytes
                .saturating_sub(removed.text.resident_bytes());
        }
    }

    pub fn indexed_page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn contains_page(&self, page: PageIndex) -> bool {
        self.pages.contains_key(&page)
    }

    pub fn page_text(&self, page: PageIndex) -> Option<Arc<[u16]>> {
        self.pages.get(&page)?.text.load()
    }

    /// Literal, Unicode-lowercase search. The folded buffer carries a mapping
    /// back to native UTF-16 offsets so overlays and copy always address the
    /// renderer-owned text substrate.
    pub fn search_case_insensitive(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        self.search_case_insensitive_cancellable(query, limit, || false)
            .unwrap_or_default()
    }

    fn search_case_insensitive_cancellable(
        &self,
        query: &str,
        limit: usize,
        cancelled: impl Fn() -> bool,
    ) -> Option<Vec<SearchHit>> {
        let needle = fold_string(query);
        if needle.is_empty() || limit == 0 {
            return Some(Vec::new());
        }
        let mut hits = Vec::new();
        for (&page, indexed) in &self.pages {
            if cancelled() {
                return None;
            }
            let Some(text) = indexed.text.load() else {
                continue;
            };
            append_folded_hits(page, &text, &needle, limit, &mut hits, &cancelled)?;
            if hits.len() == limit {
                return Some(hits);
            }
        }
        Some(hits)
    }

    /// Search one newly indexed page so progressive indexing does not rescan
    /// the already indexed prefix on every background completion.
    pub fn search_page_case_insensitive(
        &self,
        page: PageIndex,
        query: &str,
        limit: usize,
    ) -> Vec<SearchHit> {
        let needle = fold_string(query);
        if needle.is_empty() || limit == 0 {
            return Vec::new();
        }
        let Some(text) = self
            .pages
            .get(&page)
            .and_then(|indexed| indexed.text.load())
        else {
            return Vec::new();
        };
        let mut hits = Vec::new();
        let _ = append_folded_hits(page, &text, &needle, limit, &mut hits, &|| false);
        hits
    }

    /// Case-sensitive compatibility path used by focused tests and callers
    /// that need byte-for-byte PDF extraction semantics.
    pub fn search_exact(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        let needle: Vec<u16> = query.encode_utf16().collect();
        if needle.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for (&page, indexed) in &self.pages {
            let Some(text) = indexed.text.load() else {
                continue;
            };
            for start in find_all(&text, &needle) {
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

    pub fn overlays_for_hit(hit: &SearchHit, substrate: &TextSubstrate) -> Arc<[RectF]> {
        substrate
            .characters
            .iter()
            .filter(|character| hit.text_range.contains(&character.char_index))
            .map(|character| character.bounds)
            .collect::<Vec<_>>()
            .into()
    }

    fn start_spilling(&mut self) {
        let Ok(file) = SpillFile::create() else {
            return;
        };
        let file = Arc::new(file);
        let mut locations = BTreeMap::new();
        for (&page, indexed) in &self.pages {
            let IndexedText::Resident(text) = &indexed.text else {
                continue;
            };
            let Ok(offset) = file.append(text) else {
                return;
            };
            locations.insert(page, (offset, text.len()));
        }
        for (page, (offset, len)) in locations {
            let Some(indexed) = self.pages.get_mut(&page) else {
                continue;
            };
            indexed.text = IndexedText::Spilled {
                file: Arc::clone(&file),
                offset,
                len,
            };
            indexed._memory_lease = None;
        }
        self.resident_bytes = 0;
        self.spill = Some(file);
    }
}

fn search_worker(
    receiver: Receiver<SearchRequest>,
    latest_request: Arc<AtomicU64>,
    updates: Arc<UpdateQueue>,
) {
    while let Ok(mut message) = receiver.recv() {
        while let Ok(newer) = receiver.try_recv() {
            message = newer;
        }
        let SearchRequest::Query {
            request,
            index_revision,
            index,
            query,
        } = message
        else {
            break;
        };
        let Some(mut hits) = index.search_case_insensitive_cancellable(&query, 10_001, || {
            latest_request.load(Ordering::Acquire) != request
        }) else {
            continue;
        };
        if latest_request.load(Ordering::Acquire) != request {
            continue;
        }
        let capped = hits.len() > 10_000;
        hits.truncate(10_000);
        updates.push(SessionUpdate::SearchCompleted {
            request,
            index_revision,
            hits: hits.into(),
            capped,
        });
    }
}

impl Default for SearchIndex {
    fn default() -> Self {
        Self {
            pages: BTreeMap::new(),
            memory: None,
            spill: None,
            resident_bytes: 0,
            resident_limit: DEFAULT_RESIDENT_LIMIT,
        }
    }
}

fn fold_string(text: &str) -> Vec<u16> {
    text.chars()
        .flat_map(char::to_lowercase)
        .flat_map(|ch| {
            let mut buffer = [0_u16; 2];
            let len = ch.encode_utf16(&mut buffer).len();
            buffer.into_iter().take(len)
        })
        .collect()
}

fn fold_utf16_cancellable(
    text: &[u16],
    cancelled: &impl Fn() -> bool,
) -> Option<(Vec<u16>, Vec<usize>)> {
    let mut folded = Vec::with_capacity(text.len());
    let mut offsets = Vec::with_capacity(text.len() + 1);
    let mut source_offset = 0_usize;
    let mut next_cancel_check = 0_usize;
    for decoded in char::decode_utf16(text.iter().copied()) {
        if source_offset >= next_cancel_check {
            if cancelled() {
                return None;
            }
            next_cancel_check = source_offset.saturating_add(4_096);
        }
        let source = decoded.unwrap_or(char::REPLACEMENT_CHARACTER);
        for lower in source.to_lowercase() {
            let mut buffer = [0_u16; 2];
            for unit in lower.encode_utf16(&mut buffer).iter().copied() {
                folded.push(unit);
                offsets.push(source_offset);
            }
        }
        source_offset += source.len_utf16();
    }
    offsets.push(text.len());
    Some((folded, offsets))
}

fn append_folded_hits(
    page: PageIndex,
    text: &[u16],
    needle: &[u16],
    limit: usize,
    output: &mut Vec<SearchHit>,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    if output.len() >= limit {
        return Some(());
    }
    let (haystack, source_offsets) = fold_utf16_cancellable(text, cancelled)?;
    let Some(last_start) = haystack.len().checked_sub(needle.len()) else {
        return Some(());
    };
    for start in 0..=last_start {
        if start.is_multiple_of(4_096) && cancelled() {
            return None;
        }
        if haystack[start..start + needle.len()] != *needle {
            continue;
        }
        let folded_end = start + needle.len();
        let source_start = source_offsets.get(start).copied().unwrap_or(0);
        let source_end = source_offsets
            .get(folded_end)
            .copied()
            .unwrap_or(text.len());
        output.push(SearchHit {
            page,
            text_range: source_start..source_end.max(source_start + 1),
            overlays: Arc::from([]),
        });
        if output.len() == limit {
            return Some(());
        }
    }
    Some(())
}

fn find_all<'a>(haystack: &'a [u16], needle: &'a [u16]) -> impl Iterator<Item = usize> + 'a {
    haystack
        .windows(needle.len())
        .enumerate()
        .filter_map(move |(index, candidate)| (candidate == needle).then_some(index))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::document::WakeSink;
    use crate::text::{LineSource, PageLineSet};
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    #[derive(Debug)]
    struct NoopWake;

    impl WakeSink for NoopWake {
        fn wake(&self) {}
    }

    fn substrate(text: &str) -> Arc<TextSubstrate> {
        Arc::new(TextSubstrate {
            utf16: text.encode_utf16().collect::<Vec<_>>().into(),
            characters: Arc::from([]),
            lines: Arc::new(PageLineSet {
                page: PageIndex(0),
                lines: Arc::from([]),
                source: LineSource::ContentStream,
                median_height: None,
            }),
        })
    }

    #[test]
    fn case_insensitive_search_preserves_native_utf16_offsets() {
        let mut index = SearchIndex::default();
        index.insert(PageIndex(0), substrate("A Straße STRASSE"));
        let hits = index.search_case_insensitive("straße", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            String::from_utf16_lossy(
                &index.page_text(PageIndex(0)).expect("page")[hits[0].text_range.clone()]
            ),
            "Straße"
        );
    }

    #[test]
    fn result_limit_is_hard_and_deterministic() {
        let mut index = SearchIndex::default();
        index.insert(PageIndex(0), substrate("aaa"));
        assert_eq!(index.search_case_insensitive("a", 2).len(), 2);
    }

    #[test]
    fn cancellation_is_checked_while_scanning_a_single_large_page() {
        let mut index = SearchIndex::default();
        index.insert(PageIndex(0), substrate(&"a".repeat(12_000)));
        let checks = Cell::new(0_u32);
        let result = index.search_case_insensitive_cancellable("z", 10, || {
            let next = checks.get() + 1;
            checks.set(next);
            // One page-level check and three folding checks complete first;
            // cancellation then fires during the no-match scan.
            next >= 6
        });
        assert!(result.is_none());
        assert!(checks.get() >= 6);
    }

    #[test]
    fn large_index_spills_without_changing_search_or_copy_text() {
        let mut index = SearchIndex {
            resident_limit: 4,
            ..SearchIndex::default()
        };
        index.insert(PageIndex(0), substrate("alpha"));
        index.insert(PageIndex(1), substrate("Beta alpha"));
        assert!(index.spill.is_some());
        assert_eq!(index.resident_bytes, 0);
        assert_eq!(
            String::from_utf16_lossy(&index.page_text(PageIndex(1)).expect("spilled page")),
            "Beta alpha"
        );
        assert_eq!(index.search_case_insensitive("ALPHA", 10).len(), 2);
    }

    #[test]
    fn search_service_returns_results_through_the_session_queue() {
        let updates = UpdateQueue::new(16, Arc::new(NoopWake));
        let service = SearchService::spawn(Arc::clone(&updates)).expect("search worker");
        let mut index = SearchIndex::default();
        index.insert(PageIndex(0), substrate("background search"));
        service.submit(7, 3, index, "SEARCH".to_owned());

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(SessionUpdate::SearchCompleted {
                request,
                index_revision,
                hits,
                capped,
            }) = updates.drain().into_iter().next()
            {
                assert_eq!(request, 7);
                assert_eq!(index_revision, 3);
                assert_eq!(hits.len(), 1);
                assert!(!capped);
                break;
            }
            assert!(Instant::now() < deadline, "search worker timed out");
            std::thread::yield_now();
        }
    }

    #[test]
    fn search_mailbox_keeps_only_the_latest_pending_snapshot() {
        let (requests, pending) = bounded(1);
        let service = SearchService {
            requests,
            pending,
            latest_request: Arc::new(AtomicU64::new(0)),
            thread: None,
        };
        service.send_latest(SearchRequest::Query {
            request: 1,
            index_revision: 1,
            index: SearchIndex::default(),
            query: "old".to_owned(),
        });
        service.send_latest(SearchRequest::Query {
            request: 2,
            index_revision: 2,
            index: SearchIndex::default(),
            query: "new".to_owned(),
        });

        let message = service.pending.try_recv();
        assert!(matches!(
            &message,
            Ok(SearchRequest::Query {
                request: 2,
                query,
                ..
            }) if query == "new"
        ));
        if let Ok(SearchRequest::Query { request, query, .. }) = message {
            assert_eq!(request, 2);
            assert_eq!(query, "new");
        }
    }
}
