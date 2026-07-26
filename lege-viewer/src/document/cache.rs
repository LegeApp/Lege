use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use arc_swap::ArcSwap;

use super::tile::{TileCoord, TileDemand, TileKey, TileSurface, TileTier, ZoomBucket};
use super::{DocumentId, PageIndex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheCategory {
    Compiled,
    Tiles,
    GpuTiles,
    Thumbnails,
    Text,
    Images,
}

#[derive(Debug)]
struct MemoryState {
    budget: u64,
    total: AtomicU64,
    compiled: AtomicU64,
    tiles: AtomicU64,
    gpu_tiles: AtomicU64,
    thumbnails: AtomicU64,
    text: AtomicU64,
    images: AtomicU64,
}

#[derive(Debug, Clone)]
pub struct MemoryArbiter {
    inner: Arc<MemoryState>,
}

impl MemoryArbiter {
    pub fn new(budget: u64) -> Self {
        Self {
            inner: Arc::new(MemoryState {
                budget,
                total: AtomicU64::new(0),
                compiled: AtomicU64::new(0),
                tiles: AtomicU64::new(0),
                gpu_tiles: AtomicU64::new(0),
                thumbnails: AtomicU64::new(0),
                text: AtomicU64::new(0),
                images: AtomicU64::new(0),
            }),
        }
    }

    pub fn reserve(&self, category: CacheCategory, bytes: u64) -> MemoryLease {
        self.inner.total.fetch_add(bytes, Ordering::AcqRel);
        counter(&self.inner, category).fetch_add(bytes, Ordering::AcqRel);
        MemoryLease {
            inner: Arc::new(MemoryLeaseInner {
                owner: Arc::downgrade(&self.inner),
                category,
                bytes,
            }),
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.inner.total.load(Ordering::Acquire)
    }

    pub fn budget_bytes(&self) -> u64 {
        self.inner.budget
    }

    pub fn over_budget(&self) -> u64 {
        self.total_bytes().saturating_sub(self.budget_bytes())
    }

    pub fn category_bytes(&self, category: CacheCategory) -> u64 {
        counter(&self.inner, category).load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone)]
pub struct MemoryLease {
    inner: Arc<MemoryLeaseInner>,
}

impl MemoryLease {
    pub fn bytes(&self) -> u64 {
        self.inner.bytes
    }
}

#[derive(Debug)]
struct MemoryLeaseInner {
    owner: Weak<MemoryState>,
    category: CacheCategory,
    bytes: u64,
}

impl Drop for MemoryLeaseInner {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.total.fetch_sub(self.bytes, Ordering::AcqRel);
            counter(&owner, self.category).fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

fn counter(state: &MemoryState, category: CacheCategory) -> &AtomicU64 {
    match category {
        CacheCategory::Compiled => &state.compiled,
        CacheCategory::Tiles => &state.tiles,
        CacheCategory::GpuTiles => &state.gpu_tiles,
        CacheCategory::Thumbnails => &state.thumbnails,
        CacheCategory::Text => &state.text,
        CacheCategory::Images => &state.images,
    }
}

#[derive(Debug)]
struct TileEntry {
    surface: Arc<TileSurface>,
    lease: MemoryLease,
    last_used: u64,
    reproduction_cost: f64,
    distance_from_viewport: f64,
}

#[derive(Debug)]
pub struct TileCache {
    document: DocumentId,
    arbiter: MemoryArbiter,
    entries: Mutex<HashMap<TileKey, TileEntry>>,
    page_snapshots: Mutex<HashMap<PageIndex, Arc<ArcSwap<PageTileSnapshot>>>>,
    clock: AtomicU64,
}

#[derive(Debug, Default)]
struct PageTileSnapshot {
    entries: HashMap<TileKey, Arc<TileSurface>>,
}

#[derive(Debug, Clone)]
pub struct TileFrameSnapshot {
    document: DocumentId,
    pages: HashMap<PageIndex, Arc<PageTileSnapshot>>,
}

impl TileFrameSnapshot {
    pub fn best_covering(
        &self,
        demand: TileDemand,
        requested_bucket: ZoomBucket,
    ) -> Vec<Arc<TileSurface>> {
        let mut surfaces = Vec::new();
        self.best_covering_into(demand, requested_bucket, &mut surfaces);
        surfaces
    }

    pub fn best_covering_into(
        &self,
        demand: TileDemand,
        requested_bucket: ZoomBucket,
        surfaces: &mut Vec<Arc<TileSurface>>,
    ) {
        surfaces.clear();
        let Some(entries) = self.pages.get(&demand.page).map(|page| &page.entries) else {
            return;
        };
        let exact = entries
            .iter()
            .filter(|(key, _)| {
                key.document == self.document
                    && key.page == demand.page
                    && key.bucket == requested_bucket
                    && key.coord == demand.coord
                    && key.variant == demand.variant
                    && key.tier != TileTier::Thumbnail
            })
            .max_by_key(|(key, _)| key.tier.rank())
            .map(|(_, surface)| surface.clone());
        if let Some(surface) = exact {
            surfaces.push(surface);
            return;
        }

        let fallback_bucket = entries
            .iter()
            .filter(|(key, surface)| {
                key.document == self.document
                    && key.page == demand.page
                    && key.variant == demand.variant
                    && key.tier != TileTier::Thumbnail
                    && surface
                        .page_document_rect
                        .intersects(demand.page_document_rect)
            })
            .map(|(key, _)| key.bucket)
            .min_by_key(|bucket| bucket.distance(requested_bucket));
        if let Some(bucket) = fallback_bucket {
            snapshot_best_intersecting_into(
                entries,
                self.document,
                demand,
                bucket,
                false,
                surfaces,
            );
            return;
        }

        let thumbnail_bucket = entries
            .iter()
            .filter(|(key, surface)| {
                key.document == self.document
                    && key.page == demand.page
                    && key.variant == demand.variant
                    && key.tier == TileTier::Thumbnail
                    && surface
                        .page_document_rect
                        .intersects(demand.page_document_rect)
            })
            .map(|(key, _)| key.bucket)
            .min_by_key(|bucket| bucket.distance(requested_bucket));
        if let Some(bucket) = thumbnail_bucket {
            snapshot_best_intersecting_into(entries, self.document, demand, bucket, true, surfaces);
        }
    }

    pub fn page_tiles_at_tier(&self, page: PageIndex, tier: TileTier) -> Vec<Arc<TileSurface>> {
        let Some(entries) = self.pages.get(&page).map(|snapshot| &snapshot.entries) else {
            return Vec::new();
        };
        let mut bucket_counts: HashMap<ZoomBucket, usize> = HashMap::new();
        for key in entries.keys() {
            if key.document == self.document && key.page == page && key.tier == tier {
                *bucket_counts.entry(key.bucket).or_default() += 1;
            }
        }
        let Some(bucket) = bucket_counts
            .into_iter()
            .max_by_key(|(bucket, count)| (*count, std::cmp::Reverse(bucket.0.abs())))
            .map(|(bucket, _)| bucket)
        else {
            return Vec::new();
        };
        let mut surfaces = entries
            .iter()
            .filter(|(key, _)| {
                key.document == self.document
                    && key.page == page
                    && key.bucket == bucket
                    && key.tier == tier
            })
            .map(|(_, surface)| surface.clone())
            .collect::<Vec<_>>();
        surfaces.sort_by_key(|surface| surface.key.coord);
        surfaces
    }
}

impl TileCache {
    pub fn new(document: DocumentId, arbiter: MemoryArbiter) -> Self {
        Self {
            document,
            arbiter,
            entries: Mutex::new(HashMap::new()),
            page_snapshots: Mutex::new(HashMap::new()),
            clock: AtomicU64::new(1),
        }
    }

    pub fn insert(&self, surface: Arc<TileSurface>, distance_from_viewport: f64) {
        let mut entries = self.lock_entries();
        if entries
            .get(&surface.key)
            .is_some_and(|existing| existing.surface.generation > surface.generation)
        {
            return;
        }
        if surface.key.tier != TileTier::Thumbnail {
            let higher_quality_exists = entries.keys().any(|candidate| {
                candidate.document == surface.key.document
                    && candidate.page == surface.key.page
                    && candidate.bucket == surface.key.bucket
                    && candidate.coord == surface.key.coord
                    && candidate.variant == surface.key.variant
                    && candidate.tier != TileTier::Thumbnail
                    && candidate.tier.rank() > surface.key.tier.rank()
            });
            if higher_quality_exists {
                return;
            }
            // A promoted tile supersedes lower tiers at the same exact
            // identity. Keeping both wastes memory and makes eviction retain
            // pixels the presenter can no longer select.
            entries.retain(|candidate, _| {
                candidate.document != surface.key.document
                    || candidate.page != surface.key.page
                    || candidate.bucket != surface.key.bucket
                    || candidate.coord != surface.key.coord
                    || candidate.variant != surface.key.variant
                    || candidate.tier == TileTier::Thumbnail
                    || candidate.tier.rank() >= surface.key.tier.rank()
            });
        }
        let category = if surface.key.tier == TileTier::Thumbnail {
            CacheCategory::Thumbnails
        } else {
            CacheCategory::Tiles
        };
        let bytes = surface.byte_len();
        let lease = self.arbiter.reserve(category, bytes);
        let reproduction_cost = match surface.key.tier {
            TileTier::Thumbnail => 0.2,
            TileTier::Draft => 0.4,
            TileTier::TextFirst => 0.8,
            TileTier::Final => 1.0,
        };
        let key = surface.key;
        let snapshot_surface = surface.clone();
        entries.insert(
            surface.key,
            TileEntry {
                surface,
                lease,
                last_used: self.clock.fetch_add(1, Ordering::Relaxed),
                reproduction_cost,
                distance_from_viewport,
            },
        );
        drop(entries);
        self.update_page_snapshot(key.page, |snapshot| {
            if key.tier != TileTier::Thumbnail {
                snapshot.retain(|candidate, _| {
                    candidate.document != key.document
                        || candidate.page != key.page
                        || candidate.bucket != key.bucket
                        || candidate.coord != key.coord
                        || candidate.variant != key.variant
                        || candidate.tier == TileTier::Thumbnail
                        || candidate.tier.rank() >= key.tier.rank()
                });
            }
            snapshot.insert(key, snapshot_surface);
        });
        self.evict_over_budget();
    }

    pub fn refresh_distances(&self, mut distance: impl FnMut(&TileSurface) -> f64) {
        let mut entries = self.lock_entries();
        for entry in entries.values_mut() {
            entry.distance_from_viewport = distance(&entry.surface);
        }
    }

    /// Resolve the fallback ladder for one requested page region. Exact-
    /// bucket tiles are selected by coordinate. Cross-bucket fallbacks are
    /// selected by page-space intersection, because the same tile coordinate
    /// covers a different region at every zoom bucket.
    pub fn best_covering(
        &self,
        demand: TileDemand,
        requested_bucket: ZoomBucket,
    ) -> Vec<Arc<TileSurface>> {
        let mut entries = self.lock_entries();
        let tick = self.clock.fetch_add(1, Ordering::Relaxed);

        let exact_key = entries
            .keys()
            .filter(|key| {
                key.document == self.document
                    && key.page == demand.page
                    && key.bucket == requested_bucket
                    && key.coord == demand.coord
                    && key.variant == demand.variant
                    && key.tier != TileTier::Thumbnail
            })
            .max_by_key(|key| key.tier.rank())
            .copied();
        if let Some(key) = exact_key
            && let Some(entry) = entries.get_mut(&key)
        {
            entry.last_used = tick;
            return vec![entry.surface.clone()];
        }

        let fallback_bucket = entries
            .keys()
            .filter(|key| {
                key.document == self.document
                    && key.page == demand.page
                    && key.variant == demand.variant
                    && key.tier != TileTier::Thumbnail
            })
            .filter_map(|key| {
                let entry = entries.get(key)?;
                entry
                    .surface
                    .page_document_rect
                    .intersects(demand.page_document_rect)
                    .then_some(key.bucket)
            })
            .min_by_key(|bucket| bucket.distance(requested_bucket));

        if let Some(bucket) = fallback_bucket {
            return best_intersecting_at_bucket(
                &mut entries,
                self.document,
                demand,
                bucket,
                false,
                tick,
            );
        }

        let thumbnail_bucket = entries
            .keys()
            .filter(|key| {
                key.document == self.document
                    && key.page == demand.page
                    && key.variant == demand.variant
                    && key.tier == TileTier::Thumbnail
            })
            .filter_map(|key| {
                let entry = entries.get(key)?;
                entry
                    .surface
                    .page_document_rect
                    .intersects(demand.page_document_rect)
                    .then_some(key.bucket)
            })
            .min_by_key(|bucket| bucket.distance(requested_bucket));
        thumbnail_bucket.map_or_else(Vec::new, |bucket| {
            best_intersecting_at_bucket(&mut entries, self.document, demand, bucket, true, tick)
        })
    }

    /// Return one coherent raster bucket for a page/tier, suitable for a
    /// thumbnail popup or other page-level preview. The page still remains
    /// tile-backed; this method never assembles a whole-page bitmap.
    pub fn page_tiles_at_tier(&self, page: PageIndex, tier: TileTier) -> Vec<Arc<TileSurface>> {
        let mut entries = self.lock_entries();
        let mut bucket_counts: HashMap<ZoomBucket, usize> = HashMap::new();
        for key in entries.keys() {
            if key.document == self.document && key.page == page && key.tier == tier {
                *bucket_counts.entry(key.bucket).or_default() += 1;
            }
        }
        let Some(bucket) = bucket_counts
            .into_iter()
            .max_by_key(|(bucket, count)| (*count, std::cmp::Reverse(bucket.0.abs())))
            .map(|(bucket, _)| bucket)
        else {
            return Vec::new();
        };

        let tick = self.clock.fetch_add(1, Ordering::Relaxed);
        let mut keys = entries
            .keys()
            .filter(|key| {
                key.document == self.document
                    && key.page == page
                    && key.bucket == bucket
                    && key.tier == tier
            })
            .copied()
            .collect::<Vec<_>>();
        keys.sort_by_key(|key| key.coord);
        keys.into_iter()
            .filter_map(|key| {
                let entry = entries.get_mut(&key)?;
                entry.last_used = tick;
                Some(entry.surface.clone())
            })
            .collect()
    }

    pub fn contains_at_or_above(&self, key: TileKey) -> bool {
        let entries = self.lock_entries();
        if key.tier == TileTier::Thumbnail {
            return entries.contains_key(&key);
        }
        entries.keys().any(|candidate| {
            candidate.document == key.document
                && candidate.page == key.page
                && candidate.bucket == key.bucket
                && candidate.coord == key.coord
                && candidate.variant == key.variant
                && candidate.tier != TileTier::Thumbnail
                && candidate.tier.rank() >= key.tier.rank()
        })
    }

    pub fn contains(&self, key: TileKey) -> bool {
        self.lock_entries().contains_key(&key)
    }

    pub fn len(&self) -> usize {
        self.lock_entries().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock_entries().is_empty()
    }

    pub fn frame_snapshot(&self) -> TileFrameSnapshot {
        let slots = self.lock_page_snapshots();
        TileFrameSnapshot {
            document: self.document,
            pages: slots
                .iter()
                .map(|(page, snapshot)| (*page, snapshot.load_full()))
                .collect(),
        }
    }

    pub fn frame_snapshot_for_pages(
        &self,
        pages: impl IntoIterator<Item = PageIndex>,
    ) -> TileFrameSnapshot {
        let slots = self.lock_page_snapshots();
        TileFrameSnapshot {
            document: self.document,
            pages: pages
                .into_iter()
                .filter_map(|page| {
                    slots
                        .get(&page)
                        .map(|snapshot| (page, snapshot.load_full()))
                })
                .collect(),
        }
    }

    fn evict_over_budget(&self) {
        let mut entries = self.lock_entries();
        while self.arbiter.over_budget() > 0 && entries.len() > 1 {
            let now = self.clock.load(Ordering::Relaxed);
            let candidate = entries
                .iter()
                .max_by(|(_, left), (_, right)| {
                    eviction_score(left, now)
                        .partial_cmp(&eviction_score(right, now))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(key, _)| *key);
            let Some(candidate) = candidate else {
                break;
            };
            entries.remove(&candidate);
            self.update_page_snapshot(candidate.page, |snapshot| {
                snapshot.remove(&candidate);
            });
        }
    }

    fn update_page_snapshot(
        &self,
        page: PageIndex,
        update: impl FnOnce(&mut HashMap<TileKey, Arc<TileSurface>>),
    ) {
        let slot = {
            let mut snapshots = self.lock_page_snapshots();
            snapshots
                .entry(page)
                .or_insert_with(|| Arc::new(ArcSwap::from_pointee(PageTileSnapshot::default())))
                .clone()
        };
        let current = slot.load_full();
        let mut entries = current.entries.clone();
        update(&mut entries);
        slot.store(Arc::new(PageTileSnapshot { entries }));
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<TileKey, TileEntry>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_page_snapshots(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<PageIndex, Arc<ArcSwap<PageTileSnapshot>>>> {
        self.page_snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn snapshot_best_intersecting_into(
    entries: &HashMap<TileKey, Arc<TileSurface>>,
    document: DocumentId,
    demand: TileDemand,
    bucket: ZoomBucket,
    thumbnails: bool,
    surfaces: &mut Vec<Arc<TileSurface>>,
) {
    for (&key, surface) in entries {
        if key.document != document
            || key.page != demand.page
            || key.bucket != bucket
            || key.variant != demand.variant
            || (key.tier == TileTier::Thumbnail) != thumbnails
            || !surface
                .page_document_rect
                .intersects(demand.page_document_rect)
        {
            continue;
        }
        if let Some(best) = surfaces
            .iter_mut()
            .find(|candidate| candidate.key.coord == key.coord)
        {
            if key.tier.rank() > best.key.tier.rank() {
                *best = surface.clone();
            }
        } else {
            surfaces.push(surface.clone());
        }
    }
    surfaces.sort_by_key(|surface| surface.key.coord);
}

fn best_intersecting_at_bucket(
    entries: &mut HashMap<TileKey, TileEntry>,
    document: DocumentId,
    demand: TileDemand,
    bucket: ZoomBucket,
    thumbnails: bool,
    tick: u64,
) -> Vec<Arc<TileSurface>> {
    let mut best_by_coord: HashMap<TileCoord, TileKey> = HashMap::new();
    for (&key, entry) in entries.iter() {
        if key.document != document
            || key.page != demand.page
            || key.bucket != bucket
            || key.variant != demand.variant
            || (key.tier == TileTier::Thumbnail) != thumbnails
            || !entry
                .surface
                .page_document_rect
                .intersects(demand.page_document_rect)
        {
            continue;
        }
        best_by_coord
            .entry(key.coord)
            .and_modify(|best| {
                if key.tier.rank() > best.tier.rank() {
                    *best = key;
                }
            })
            .or_insert(key);
    }
    let mut keys = best_by_coord.into_values().collect::<Vec<_>>();
    keys.sort_by_key(|key| key.coord);
    keys.into_iter()
        .filter_map(|key| {
            let entry = entries.get_mut(&key)?;
            entry.last_used = tick;
            Some(entry.surface.clone())
        })
        .collect()
}

fn eviction_score(entry: &TileEntry, now: u64) -> f64 {
    let age = now.saturating_sub(entry.last_used) as f64;
    let bytes = entry.lease.bytes().max(1) as f64;
    // Distant, cheap-to-reproduce, old, large entries leave first.
    (entry.distance_from_viewport + 1.0)
        * (1.2 - entry.reproduction_cost)
        * (age + 1.0)
        * bytes.sqrt()
}
